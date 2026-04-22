//! Storage engine: orchestrates all subsystems to provide CRUD operations.
//!
//! All data page access goes through the [`BufferPool`] for LRU caching.
//! Overflow pages bypass the pool (they are sequential, not revisited).

use std::path::Path;
use uuid::Uuid;

use crate::btree::BTree;
use crate::buffer::pool::BufferPool;
use crate::document::value::Value;
use crate::document::Document;
use crate::error::{GrumpyError, Result};
use crate::page::manager::PageManager;
use crate::page::overflow;
use crate::page::slotted::SlottedPage;
use crate::page::{PageHeader, PageType, PAGE_SIZE, PAGE_USABLE_SPACE, SLOT_SIZE};
use crate::wal::writer::WalWriter;

/// Maximum document size that fits in a single slotted page (without overflow).
const INLINE_MAX: usize = PAGE_USABLE_SPACE - SLOT_SIZE;

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
    /// Buffer pool wrapping the data page manager (LRU cache).
    data_pool: BufferPool,
    btree: BTree,
    wal: WalWriter,
    /// Page ID of the current data page being filled.
    current_data_page: u32,
    /// Write counter for periodic checkpointing.
    writes_since_checkpoint: u32,
}

/// Number of writes between automatic checkpoints.
const CHECKPOINT_INTERVAL: u32 = 100;

impl GrumpyDb {
    /// Opens or creates a database at the given directory path.
    ///
    /// Creates `data.db` for document storage and `index.db` for the B+Tree index.
    /// Data pages are cached in a buffer pool (256 frames = 2 MiB by default).
    /// If the files already exist, they are opened and the engine resumes.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_pool_capacity(path, DEFAULT_POOL_CAPACITY)
    }

    /// Opens a database with a custom buffer pool capacity (number of frames).
    pub fn open_with_pool_capacity(path: &Path, pool_capacity: usize) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        let data_path = path.join("data.db");
        let index_path = path.join("index.db");
        let wal_path = path.join("wal.log");

        let data_exists = data_path.exists() && data_path.metadata()?.len() > 0;
        let index_exists = index_path.exists() && index_path.metadata()?.len() > 0;

        let mut data_pm = PageManager::new(&data_path)?;

        let mut btree = if index_exists {
            BTree::open(&index_path)?
        } else {
            BTree::create(&index_path)?
        };

        // WAL recovery: replay committed transactions, undo uncommitted ones
        let mut wal = WalWriter::new(&wal_path)?;
        let records = wal.read_all_records()?;
        if !records.is_empty() {
            crate::wal::recovery::recover(&records, &mut data_pm, &mut btree.pm)?;
            // Checkpoint after recovery to clean the WAL
            data_pm.sync()?;
            btree.sync()?;
            wal.log_checkpoint()?;
            wal.truncate()?;
        }

        // Find or allocate the current data page
        let current_data_page = if data_exists {
            Self::find_or_alloc_data_page(&mut data_pm)?
        } else {
            let page_id = data_pm.allocate_page()?;
            let page = SlottedPage::new(page_id);
            data_pm.write_page(page_id, &page.data)?;
            page_id
        };

        // Wrap the PageManager in a BufferPool for LRU caching
        let data_pool = BufferPool::new(pool_capacity, data_pm);

        Ok(Self {
            data_pool,
            btree,
            wal,
            current_data_page,
            writes_since_checkpoint: 0,
        })
    }

    /// Inserts a document with the given UUID key.
    ///
    /// Returns `DuplicateKey` if the key already exists.
    pub fn insert(&mut self, key: Uuid, value: Value) -> Result<()> {
        // Check for duplicate via B+Tree
        if self.btree.search(&key)?.is_some() {
            return Err(GrumpyError::DuplicateKey(key));
        }

        let doc = Document::new(key, value);
        let encoded = doc.encode();

        // Begin WAL transaction
        let tx_id = self.wal.begin_tx();

        let (page_id, slot_id) = if encoded.len() > INLINE_MAX {
            self.store_overflow_wal(tx_id, &encoded)?
        } else {
            self.store_inline_wal(tx_id, &encoded)?
        };

        // Index in B+Tree
        self.btree.insert(key, page_id, slot_id)?;

        // Commit the transaction (fsync WAL)
        self.wal.log_commit(tx_id)?;
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Retrieves a document by its UUID key.
    ///
    /// Returns `None` if the key does not exist.
    /// Uses the buffer pool — repeated reads of the same page hit the cache.
    pub fn get(&mut self, key: &Uuid) -> Result<Option<Value>> {
        let Some((page_id, slot_id)) = self.btree.search(key)? else {
            return Ok(None);
        };

        let raw = self.read_tuple(page_id, slot_id)?;
        let doc = Document::decode(&raw)?;
        Ok(Some(doc.value))
    }

    /// Updates an existing document.
    ///
    /// Returns `KeyNotFound` if the key does not exist.
    pub fn update(&mut self, key: &Uuid, value: Value) -> Result<()> {
        // Verify the key exists
        if self.btree.search(key)?.is_none() {
            return Err(GrumpyError::KeyNotFound(*key));
        }

        // Delete old + insert new (simple strategy)
        self.delete(key)?;
        self.insert(*key, value)?;
        Ok(())
    }

    /// Deletes a document by its UUID key.
    ///
    /// Returns `KeyNotFound` if the key does not exist.
    pub fn delete(&mut self, key: &Uuid) -> Result<()> {
        let Some((page_id, slot_id)) = self.btree.search(key)? else {
            return Err(GrumpyError::KeyNotFound(*key));
        };

        let tx_id = self.wal.begin_tx();

        // Read the slot via buffer pool to check for overflow
        let frame_idx = self.data_pool.fetch_page(page_id)?;
        let slot_data = {
            let page = SlottedPage::from_bytes(self.data_pool.get_frame(frame_idx).data);
            page.get(slot_id)?.to_vec()
        };

        if overflow::is_overflow(&slot_data) {
            let (overflow_page_id, _) = overflow::decode_overflow_ref(&slot_data).unwrap();
            // Overflow bypasses the buffer pool (sequential I/O, not revisited)
            overflow::free_overflow(self.data_pool.page_manager(), overflow_page_id)?;
        }

        // WAL: log before deleting from slotted page
        let before = self.data_pool.get_frame(frame_idx).data;
        let mut page = SlottedPage::from_bytes(before);
        page.delete(slot_id)?;
        self.wal.log_page_write(tx_id, page_id, &before, &page.data)?;

        // Write the modified data into the frame and unpin as dirty
        self.data_pool.get_frame_mut(frame_idx).data = page.data;
        self.data_pool.unpin(page_id, true)?;

        // Remove from B+Tree
        self.btree.delete(key)?;

        self.wal.log_commit(tx_id)?;
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Scans documents in a UUID key range.
    ///
    /// Returns all documents whose keys fall within the given range, sorted by key.
    pub fn scan(
        &mut self,
        range: impl std::ops::RangeBounds<Uuid>,
    ) -> Result<Vec<(Uuid, Value)>> {
        use std::ops::Bound;

        let start = match range.start_bound() {
            Bound::Included(k) => Some(*k),
            Bound::Excluded(k) => {
                // For UUID, "excluded" is tricky. We'll start from k and skip it.
                Some(*k)
            }
            Bound::Unbounded => None,
        };

        let entries = self.btree.range(
            start.as_ref(),
            None, // We'll filter the end in post
        )?;

        let mut results = Vec::new();
        for entry in &entries {
            // Check end bound
            match range.end_bound() {
                Bound::Included(end) => {
                    if entry.key > *end {
                        break;
                    }
                }
                Bound::Excluded(end) => {
                    if entry.key >= *end {
                        break;
                    }
                }
                Bound::Unbounded => {}
            }

            // Check start bound (for Excluded)
            if let Bound::Excluded(start_key) = range.start_bound() {
                if entry.key == *start_key {
                    continue;
                }
            }

            let raw = self.read_tuple(entry.page_id, entry.slot_id)?;
            let doc = Document::decode(&raw)?;
            results.push((doc.key, doc.value));
        }

        Ok(results)
    }

    /// Flushes all data to disk and writes a WAL checkpoint.
    ///
    /// Flushes all dirty pages from the buffer pool, syncs the B+Tree,
    /// writes a WAL checkpoint, and truncates the WAL.
    pub fn flush(&mut self) -> Result<()> {
        self.data_pool.flush_all()?;
        self.btree.sync()?;
        self.wal.log_checkpoint()?;
        self.wal.truncate()?;
        self.writes_since_checkpoint = 0;
        Ok(())
    }

    /// Closes the database, flushing all pending data.
    pub fn close(mut self) -> Result<()> {
        self.flush()
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Stores inline with WAL logging, using the buffer pool.
    fn store_inline_wal(&mut self, tx_id: u64, encoded: &[u8]) -> Result<(u32, u16)> {
        let frame_idx = self.data_pool.fetch_page(self.current_data_page)?;
        let before = self.data_pool.get_frame(frame_idx).data;
        let mut page = SlottedPage::from_bytes(before);

        match page.insert(encoded) {
            Ok(slot_id) => {
                self.wal.log_page_write(tx_id, self.current_data_page, &before, &page.data)?;
                self.data_pool.get_frame_mut(frame_idx).data = page.data;
                self.data_pool.unpin(self.current_data_page, true)?;
                Ok((self.current_data_page, slot_id))
            }
            Err(GrumpyError::PageFull(_)) => {
                // Current page is full — unpin it (not dirty) and allocate a new one
                self.data_pool.unpin(self.current_data_page, false)?;

                let (new_page_id, new_fidx) = self.data_pool.new_page()?;
                let before_new = [0u8; PAGE_SIZE];
                let mut new_page = SlottedPage::new(new_page_id);
                let slot_id = new_page.insert(encoded)?;
                self.wal.log_page_write(tx_id, new_page_id, &before_new, &new_page.data)?;
                self.data_pool.get_frame_mut(new_fidx).data = new_page.data;
                self.data_pool.unpin(new_page_id, true)?;
                self.current_data_page = new_page_id;
                Ok((new_page_id, slot_id))
            }
            Err(e) => {
                self.data_pool.unpin(self.current_data_page, false)?;
                Err(e)
            }
        }
    }

    /// Stores overflow with WAL logging (for the reference slot only).
    /// Overflow page chains bypass the buffer pool (sequential writes, not revisited).
    fn store_overflow_wal(&mut self, tx_id: u64, encoded: &[u8]) -> Result<(u32, u16)> {
        let overflow_page_id = overflow::write_overflow(self.data_pool.page_manager(), encoded)?;
        let ref_data = overflow::encode_overflow_ref(overflow_page_id, encoded.len() as u32);
        self.store_inline_wal(tx_id, &ref_data)
    }

    /// Periodic checkpoint: flush + truncate WAL every N writes.
    fn maybe_checkpoint(&mut self) -> Result<()> {
        self.writes_since_checkpoint += 1;
        if self.writes_since_checkpoint >= CHECKPOINT_INTERVAL {
            self.flush()?;
        }
        Ok(())
    }

    /// Returns buffer pool statistics: `(read_count, write_count, cached_count, capacity)`.
    pub fn pool_stats(&self) -> (u64, u64, usize, usize) {
        (
            self.data_pool.read_count,
            self.data_pool.write_count,
            self.data_pool.cached_count(),
            self.data_pool.capacity(),
        )
    }

    /// Reads a tuple from a slotted page via the buffer pool, following overflow chains if needed.
    fn read_tuple(&mut self, page_id: u32, slot_id: u16) -> Result<Vec<u8>> {
        let frame_idx = self.data_pool.fetch_page(page_id)?;
        let slot_data = {
            let page = SlottedPage::from_bytes(self.data_pool.get_frame(frame_idx).data);
            page.get(slot_id)?.to_vec()
        };
        self.data_pool.unpin(page_id, false)?;

        if overflow::is_overflow(&slot_data) {
            let (overflow_page_id, _) = overflow::decode_overflow_ref(&slot_data).unwrap();
            overflow::read_overflow(self.data_pool.page_manager(), overflow_page_id)
        } else {
            Ok(slot_data)
        }
    }

    /// Finds a usable data page or allocates a new one.
    fn find_or_alloc_data_page(pm: &mut PageManager) -> Result<u32> {
        // Scan from the last page backwards to find a Data page with space
        let num_pages = pm.num_pages();
        for pid in (1..num_pages).rev() {
            let buf = pm.read_page(pid)?;
            let header = PageHeader::read_from(&buf);
            if header.page_type == PageType::Data {
                return Ok(pid);
            }
        }
        // No data page found → allocate one
        let page_id = pm.allocate_page()?;
        let page = SlottedPage::new(page_id);
        pm.write_page(page_id, &page.data)?;
        Ok(page_id)
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
        assert!(db_path.join("index.db").exists());
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
            ("tags".into(), Value::Array(vec![
                Value::String("db".into()),
                Value::String("rust".into()),
            ])),
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
            db.insert(Uuid::from_u128(i), Value::Integer(i as i64)).unwrap();
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
            db.insert(Uuid::from_u128(i), Value::Integer(i as i64)).unwrap();
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
        let mut db = GrumpyDb::open_with_pool_capacity(dir.path().join("testdb").as_path(), 4).unwrap();

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
        assert!(reads_after - reads_before <= 2, "expected mostly cache hits, got {} disk reads", reads_after - reads_before);
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
}
