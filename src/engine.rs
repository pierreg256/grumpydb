//! Storage engine: orchestrates all subsystems to provide CRUD operations.
//!
//! `GrumpyDb` is a thin wrapper over a single [`Collection`] with WAL logging.
//! All data page access goes through the collection's [`BufferPool`].

use std::path::Path;
use uuid::Uuid;

use crate::collection::Collection;
use crate::document::Document;
use crate::document::value::Value;
use crate::error::{GrumpyError, Result};
use crate::page::manager::PageManager;
use crate::wal::writer::WalWriter;

/// Default number of frames in the buffer pool (256 frames × 8 KiB = 2 MiB).
const DEFAULT_POOL_CAPACITY: usize = 256;

/// The main GrumpyDB storage engine.
///
/// Provides CRUD operations on schema-less documents identified by UUID keys.
/// Documents are stored in page-based files with B+Tree indexing.
/// Data pages are cached in a buffer pool for reduced disk I/O.
///
/// # Example
///
/// ```no_run
/// use grumpydb::{GrumpyDb, Value};
/// use uuid::Uuid;
///
/// let mut db = GrumpyDb::open(std::path::Path::new("./mydb")).unwrap();
/// let key = Uuid::new_v4();
/// db.insert(key, Value::String("hello".into())).unwrap();
/// assert_eq!(db.get(&key).unwrap(), Some(Value::String("hello".into())));
/// db.close().unwrap();
/// ```
pub struct GrumpyDb {
    /// The underlying collection (data pages + primary index).
    collection: Collection,
    /// Write-Ahead Log for durability.
    wal: WalWriter,
    /// Write counter for periodic checkpointing.
    writes_since_checkpoint: u32,
}

/// Number of writes between automatic checkpoints.
const CHECKPOINT_INTERVAL: u32 = 100;

/// Result of a compaction operation.
#[derive(Debug)]
pub struct CompactResult {
    /// Number of documents preserved.
    pub documents: u64,
}

impl GrumpyDb {
    /// Opens or creates a database at the given directory path.
    ///
    /// Creates `data.db` for document storage and `primary.idx` for the B+Tree index.
    /// Data pages are cached in a buffer pool (256 frames = 2 MiB by default).
    /// If the files already exist, they are opened and the engine resumes.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_pool_capacity(path, DEFAULT_POOL_CAPACITY)
    }

    /// Opens a database with a custom buffer pool capacity (number of frames).
    pub fn open_with_pool_capacity(path: &Path, pool_capacity: usize) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        let data_path = path.join("data.db");
        let index_path = path.join("primary.idx");
        let wal_path = path.join("wal.log");

        // WAL recovery happens BEFORE creating the Collection,
        // because recovery needs two &mut PageManager references.
        let mut wal = WalWriter::new(&wal_path)?;
        let records = wal.read_all_records()?;
        if !records.is_empty() {
            let mut data_pm = PageManager::new(&data_path)?;
            let mut index_pm = PageManager::new(&index_path)?;
            crate::wal::recovery::recover(&records, &mut data_pm, &mut index_pm)?;
            data_pm.sync()?;
            index_pm.sync()?;
            wal.log_checkpoint()?;
            wal.truncate()?;
        }

        // Now open the Collection (wraps data.db + primary.idx in BufferPool + BTree)
        let collection = Collection::open(path, "_default", pool_capacity)?;

        Ok(Self {
            collection,
            wal,
            writes_since_checkpoint: 0,
        })
    }

    /// Inserts a document with the given UUID key.
    ///
    /// Returns `DuplicateKey` if the key already exists.
    pub fn insert(&mut self, key: Uuid, value: Value) -> Result<()> {
        let doc = Document::new(key, value);
        let encoded = doc.encode();

        let tx_id = self.wal.begin_tx();

        let ((_page_id, _slot_id), records) = self.collection.insert_raw(key, &encoded)?;

        // Log all page writes to WAL
        for rec in &records {
            self.wal
                .log_page_write(tx_id, rec.page_id, &rec.before, &rec.after)?;
        }

        self.wal.log_commit(tx_id)?;
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Retrieves a document by its UUID key.
    ///
    /// Returns `None` if the key does not exist.
    /// Uses the buffer pool — repeated reads of the same page hit the cache.
    pub fn get(&mut self, key: &Uuid) -> Result<Option<Value>> {
        let Some(raw) = self.collection.get_raw(key)? else {
            return Ok(None);
        };
        let doc = Document::decode(&raw)?;
        Ok(Some(doc.value))
    }

    /// Updates an existing document.
    ///
    /// Returns `KeyNotFound` if the key does not exist.
    pub fn update(&mut self, key: &Uuid, value: Value) -> Result<()> {
        if self.collection.get_raw(key)?.is_none() {
            return Err(GrumpyError::KeyNotFound(*key));
        }
        self.delete(key)?;
        self.insert(*key, value)?;
        Ok(())
    }

    /// Deletes a document by its UUID key.
    ///
    /// Returns `KeyNotFound` if the key does not exist.
    pub fn delete(&mut self, key: &Uuid) -> Result<()> {
        let tx_id = self.wal.begin_tx();

        let records = self.collection.delete_raw(key)?;

        for rec in &records {
            self.wal
                .log_page_write(tx_id, rec.page_id, &rec.before, &rec.after)?;
        }

        self.wal.log_commit(tx_id)?;
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Scans documents in a UUID key range.
    ///
    /// Returns all documents whose keys fall within the given range, sorted by key.
    pub fn scan(&mut self, range: impl std::ops::RangeBounds<Uuid>) -> Result<Vec<(Uuid, Value)>> {
        let raw_results = self.collection.scan_raw(range)?;
        let mut results = Vec::with_capacity(raw_results.len());
        for (key, raw) in raw_results {
            let doc = Document::decode(&raw)?;
            results.push((key, doc.value));
        }
        Ok(results)
    }

    /// Flushes all data to disk and writes a WAL checkpoint.
    pub fn flush(&mut self) -> Result<()> {
        self.collection.flush()?;
        self.wal.log_checkpoint()?;
        self.wal.truncate()?;
        self.writes_since_checkpoint = 0;
        Ok(())
    }

    /// Closes the database, flushing all pending data.
    pub fn close(mut self) -> Result<()> {
        self.flush()
    }

    /// Returns the number of documents in the database.
    pub fn document_count(&self) -> u64 {
        self.collection.document_count()
    }

    /// Compacts the database: defragments data pages and rebuilds the B+Tree index.
    pub fn compact(&mut self) -> Result<CompactResult> {
        let docs = self.collection.compact()?;

        self.wal.log_checkpoint()?;
        self.wal.truncate()?;
        self.writes_since_checkpoint = 0;

        Ok(CompactResult { documents: docs })
    }

    /// Returns buffer pool statistics: `(read_count, write_count, cached_count, capacity)`.
    pub fn pool_stats(&self) -> (u64, u64, usize, usize) {
        self.collection.pool_stats()
    }

    /// Periodic checkpoint: flush + truncate WAL every N writes.
    fn maybe_checkpoint(&mut self) -> Result<()> {
        self.writes_since_checkpoint += 1;
        if self.writes_since_checkpoint >= CHECKPOINT_INTERVAL {
            self.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn setup() -> (TempDir, GrumpyDb) {
        let dir = TempDir::new().unwrap();
        let db = GrumpyDb::open(dir.path().join("testdb").as_path()).unwrap();
        (dir, db)
    }

    #[test]
    fn test_open_creates_files() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let db = GrumpyDb::open(&db_path).unwrap();
        assert!(db_path.join("data.db").exists());
        assert!(db_path.join("primary.idx").exists());
        db.close().unwrap();
    }

    #[test]
    fn test_insert_and_get() {
        let (_dir, mut db) = setup();
        let key = Uuid::new_v4();
        db.insert(key, Value::String("hello".into())).unwrap();
        let val = db.get(&key).unwrap();
        assert_eq!(val, Some(Value::String("hello".into())));
    }

    #[test]
    fn test_get_nonexistent() {
        let (_dir, mut db) = setup();
        let val = db.get(&Uuid::new_v4()).unwrap();
        assert_eq!(val, None);
    }

    #[test]
    fn test_insert_duplicate_key() {
        let (_dir, mut db) = setup();
        let key = Uuid::new_v4();
        db.insert(key, Value::Integer(1)).unwrap();
        let result = db.insert(key, Value::Integer(2));
        assert!(matches!(result, Err(GrumpyError::DuplicateKey(_))));
    }

    #[test]
    fn test_delete() {
        let (_dir, mut db) = setup();
        let key = Uuid::new_v4();
        db.insert(key, Value::Integer(42)).unwrap();
        db.delete(&key).unwrap();
        assert_eq!(db.get(&key).unwrap(), None);
    }

    #[test]
    fn test_delete_nonexistent() {
        let (_dir, mut db) = setup();
        let result = db.delete(&Uuid::new_v4());
        assert!(matches!(result, Err(GrumpyError::KeyNotFound(_))));
    }

    #[test]
    fn test_update() {
        let (_dir, mut db) = setup();
        let key = Uuid::new_v4();
        db.insert(key, Value::Integer(1)).unwrap();
        db.update(&key, Value::Integer(2)).unwrap();
        assert_eq!(db.get(&key).unwrap(), Some(Value::Integer(2)));
    }

    #[test]
    fn test_update_nonexistent() {
        let (_dir, mut db) = setup();
        let result = db.update(&Uuid::new_v4(), Value::Integer(1));
        assert!(matches!(result, Err(GrumpyError::KeyNotFound(_))));
    }

    #[test]
    fn test_insert_complex_document() {
        let (_dir, mut db) = setup();
        let key = Uuid::new_v4();
        let value = Value::Object(BTreeMap::from([
            ("name".into(), Value::String("GrumpyDB".into())),
            ("version".into(), Value::Integer(1)),
            (
                "tags".into(),
                Value::Array(vec![
                    Value::String("db".into()),
                    Value::String("rust".into()),
                ]),
            ),
        ]));
        db.insert(key, value.clone()).unwrap();
        assert_eq!(db.get(&key).unwrap(), Some(value));
    }

    #[test]
    fn test_crud_lifecycle() {
        let (_dir, mut db) = setup();
        let key = Uuid::new_v4();

        // Create
        db.insert(key, Value::String("v1".into())).unwrap();
        assert_eq!(db.get(&key).unwrap(), Some(Value::String("v1".into())));

        // Update
        db.update(&key, Value::String("v2".into())).unwrap();
        assert_eq!(db.get(&key).unwrap(), Some(Value::String("v2".into())));

        // Delete
        db.delete(&key).unwrap();
        assert_eq!(db.get(&key).unwrap(), None);
    }

    #[test]
    fn test_multiple_inserts() {
        let (_dir, mut db) = setup();
        let mut keys = Vec::new();
        for i in 0..100 {
            let key = Uuid::from_u128(i);
            db.insert(key, Value::Integer(i as i64)).unwrap();
            keys.push(key);
        }
        for (i, key) in keys.iter().enumerate() {
            assert_eq!(db.get(key).unwrap(), Some(Value::Integer(i as i64)));
        }
    }

    #[test]
    fn test_persistence_across_reopen() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let key = Uuid::from_u128(42);

        {
            let mut db = GrumpyDb::open(&db_path).unwrap();
            db.insert(key, Value::String("persistent".into())).unwrap();
            db.close().unwrap();
        }

        {
            let mut db = GrumpyDb::open(&db_path).unwrap();
            let val = db.get(&key).unwrap();
            assert_eq!(val, Some(Value::String("persistent".into())));
        }
    }

    #[test]
    fn test_scan_range() {
        let (_dir, mut db) = setup();
        for i in 0u128..20 {
            db.insert(Uuid::from_u128(i), Value::Integer(i as i64))
                .unwrap();
        }

        let start = Uuid::from_u128(5);
        let end = Uuid::from_u128(10);
        let results = db.scan(start..end).unwrap();

        assert_eq!(results.len(), 5);
        for (key, val) in &results {
            let i = key.as_u128();
            assert!((5..10).contains(&i));
            assert_eq!(*val, Value::Integer(i as i64));
        }
    }

    #[test]
    fn test_scan_all() {
        let (_dir, mut db) = setup();
        for i in 0u128..10 {
            db.insert(Uuid::from_u128(i), Value::Integer(i as i64))
                .unwrap();
        }

        let results = db.scan(..).unwrap();
        assert_eq!(results.len(), 10);

        // Verify sorted order
        for i in 1..results.len() {
            assert!(results[i - 1].0 < results[i].0);
        }
    }

    #[test]
    fn test_overflow_document() {
        let (_dir, mut db) = setup();
        let key = Uuid::new_v4();
        // Create a large document that will require overflow pages
        let large_string = "x".repeat(10_000);
        let value = Value::String(large_string.clone());
        db.insert(key, value).unwrap();

        let retrieved = db.get(&key).unwrap().unwrap();
        assert_eq!(retrieved, Value::String(large_string));
    }

    #[test]
    fn test_delete_overflow_document() {
        let (_dir, mut db) = setup();
        let key = Uuid::new_v4();
        let value = Value::String("x".repeat(10_000));
        db.insert(key, value).unwrap();
        db.delete(&key).unwrap();
        assert_eq!(db.get(&key).unwrap(), None);
    }

    #[test]
    fn test_buffer_pool_cache_hits() {
        let dir = TempDir::new().unwrap();
        // Small pool (4 frames) to exercise caching
        let mut db =
            GrumpyDb::open_with_pool_capacity(dir.path().join("testdb").as_path(), 4).unwrap();

        // Insert 10 documents — they'll share the current data page (cache hit)
        let mut keys = Vec::new();
        for i in 0u128..10 {
            let key = Uuid::from_u128(i);
            db.insert(key, Value::Integer(i as i64)).unwrap();
            keys.push(key);
        }

        let (reads_before, _, _, _) = db.pool_stats();

        // Re-read all 10 — the data page should be cached (0 or minimal reads)
        for key in &keys {
            assert!(db.get(key).unwrap().is_some());
        }

        let (reads_after, _, cached, capacity) = db.pool_stats();
        // With a pool, most reads should come from cache
        assert!(cached <= capacity);
        // There should be far fewer disk reads than total get() calls
        assert!(
            reads_after - reads_before <= 2,
            "expected mostly cache hits, got {} disk reads",
            reads_after - reads_before
        );
    }

    #[test]
    fn test_buffer_pool_flush_persists() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let key = Uuid::from_u128(99);

        {
            let mut db = GrumpyDb::open_with_pool_capacity(&db_path, 8).unwrap();
            db.insert(key, Value::String("cached".into())).unwrap();
            db.close().unwrap();
        }

        {
            let mut db = GrumpyDb::open_with_pool_capacity(&db_path, 8).unwrap();
            let val = db.get(&key).unwrap();
            assert_eq!(val, Some(Value::String("cached".into())));
        }
    }

    #[test]
    fn test_pool_stats() {
        let (_dir, db) = setup();
        let (reads, writes, cached, capacity) = db.pool_stats();
        assert_eq!(reads, 0);
        assert_eq!(writes, 0);
        assert!(cached <= capacity);
        assert_eq!(capacity, DEFAULT_POOL_CAPACITY);
    }

    #[test]
    fn test_compact_after_deletes() {
        let (_dir, mut db) = setup();

        // Insert 200 documents
        let mut keys = Vec::new();
        for i in 0u128..200 {
            let key = Uuid::from_u128(i);
            db.insert(key, Value::Integer(i as i64)).unwrap();
            keys.push(key);
        }
        assert_eq!(db.document_count(), 200);

        // Delete 100 of them
        for key in &keys[..100] {
            db.delete(key).unwrap();
        }
        assert_eq!(db.document_count(), 100);

        // Compact
        let result = db.compact().unwrap();
        assert_eq!(result.documents, 100);
        assert_eq!(db.document_count(), 100);

        // Verify surviving documents
        for key in &keys[100..] {
            let val = db.get(key).unwrap();
            assert!(val.is_some(), "key should survive compaction");
        }

        // Verify deleted documents stay deleted
        for key in &keys[..100] {
            assert_eq!(db.get(key).unwrap(), None);
        }
    }

    #[test]
    fn test_compact_with_overflow() {
        let (_dir, mut db) = setup();

        let key1 = Uuid::from_u128(1);
        let key2 = Uuid::from_u128(2);

        db.insert(key1, Value::String("x".repeat(10_000))).unwrap();
        db.insert(key2, Value::Integer(42)).unwrap();
        db.delete(&key2).unwrap();

        let result = db.compact().unwrap();
        assert_eq!(result.documents, 1);

        let val = db.get(&key1).unwrap().unwrap();
        assert_eq!(val, Value::String("x".repeat(10_000)));
    }

    #[test]
    fn test_compact_empty_db() {
        let (_dir, mut db) = setup();
        let result = db.compact().unwrap();
        assert_eq!(result.documents, 0);
    }

    #[test]
    fn test_document_count() {
        let (_dir, mut db) = setup();
        assert_eq!(db.document_count(), 0);
        let key = Uuid::new_v4();
        db.insert(key, Value::Integer(1)).unwrap();
        assert_eq!(db.document_count(), 1);
        db.delete(&key).unwrap();
        assert_eq!(db.document_count(), 0);
    }
}
