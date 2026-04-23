//! Database: manages multiple named collections with a shared WAL.
//!
//! A database is the unit of transaction — all CRUD operations are scoped
//! to a single database. Each database has its own WAL and collections.
//!
//! ## On-disk layout
//!
//! ```text
//! <database_dir>/
//!   wal.log            ← Write-Ahead Log (shared across collections)
//!   <collection_name>/
//!     data.db
//!     primary.idx
//!     idx_*.idx        ← secondary indexes
//! ```

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::collection::Collection;
use crate::document::Document;
use crate::document::value::Value;
use crate::error::{GrumpyError, Result};
use crate::naming::validate_name;
use crate::wal::writer::WalWriter;

/// Default buffer pool capacity per collection.
const DEFAULT_POOL_CAPACITY: usize = 256;

/// Number of writes between automatic checkpoints.
const CHECKPOINT_INTERVAL: u32 = 100;

/// A database containing multiple named collections.
pub struct Database {
    /// Path to the database directory.
    path: PathBuf,
    /// Named collections.
    collections: HashMap<String, Collection>,
    /// Shared Write-Ahead Log.
    wal: WalWriter,
    /// Write counter for periodic checkpointing.
    writes_since_checkpoint: u32,
}

impl Database {
    /// Opens or creates a database at the given directory.
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        let wal_path = path.join("wal.log");
        let wal = WalWriter::new(&wal_path)?;

        // Discover existing collections by scanning subdirectories
        let mut collections = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let ft = entry.file_type()?;
                if ft.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    // Skip hidden dirs
                    if name.starts_with('.') {
                        continue;
                    }
                    let coll_path = entry.path();
                    // Only open if it looks like a collection (has data.db)
                    if coll_path.join("data.db").exists() {
                        let coll = Collection::open(&coll_path, &name, DEFAULT_POOL_CAPACITY)?;
                        collections.insert(name, coll);
                    }
                }
            }
        }

        Ok(Self {
            path: path.to_path_buf(),
            collections,
            wal,
            writes_since_checkpoint: 0,
        })
    }

    // ── Collection management ───────────────────────────────────────────

    /// Creates a new collection.
    pub fn create_collection(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        if self.collections.contains_key(name) {
            return Err(GrumpyError::CollectionNotFound(format!(
                "collection '{name}' already exists"
            )));
        }
        let coll_path = self.path.join(name);
        let coll = Collection::open(&coll_path, name, DEFAULT_POOL_CAPACITY)?;
        self.collections.insert(name.to_string(), coll);
        Ok(())
    }

    /// Drops a collection, deleting all its files.
    pub fn drop_collection(&mut self, name: &str) -> Result<()> {
        let coll = self
            .collections
            .remove(name)
            .ok_or_else(|| GrumpyError::CollectionNotFound(name.into()))?;
        let coll_path = coll.path().to_path_buf();
        drop(coll);
        std::fs::remove_dir_all(&coll_path)?;
        Ok(())
    }

    /// Lists all collection names.
    pub fn list_collections(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.collections.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// Returns a mutable reference to a collection.
    pub fn collection(&mut self, name: &str) -> Result<&mut Collection> {
        self.collections
            .get_mut(name)
            .ok_or_else(|| GrumpyError::CollectionNotFound(name.into()))
    }

    // ── CRUD ────────────────────────────────────────────────────────────

    /// Inserts a document into a collection.
    pub fn insert(&mut self, collection: &str, key: Uuid, value: Value) -> Result<()> {
        let doc = Document::new(key, value.clone());
        let encoded = doc.encode();

        let tx_id = self.wal.begin_tx();

        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let (_, records) = coll.insert_doc(key, &value, &encoded)?;

        for rec in &records {
            self.wal
                .log_page_write(tx_id, rec.page_id, &rec.before, &rec.after)?;
        }

        self.wal.log_commit(tx_id)?;
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Retrieves a document from a collection.
    pub fn get(&mut self, collection: &str, key: &Uuid) -> Result<Option<Value>> {
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let Some(raw) = coll.get_raw(key)? else {
            return Ok(None);
        };
        let doc = Document::decode(&raw)?;
        Ok(Some(doc.value))
    }

    /// Updates a document in a collection.
    pub fn update(&mut self, collection: &str, key: &Uuid, value: Value) -> Result<()> {
        // Get old value for unindexing
        let old_value = self
            .get(collection, key)?
            .ok_or(GrumpyError::KeyNotFound(*key))?;

        // Delete old (with unindexing)
        let tx_id = self.wal.begin_tx();
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let del_records = coll.delete_doc(key, &old_value)?;
        for rec in &del_records {
            self.wal
                .log_page_write(tx_id, rec.page_id, &rec.before, &rec.after)?;
        }

        // Insert new (with indexing)
        let doc = Document::new(*key, value.clone());
        let encoded = doc.encode();

        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let (_, ins_records) = coll.insert_doc(*key, &value, &encoded)?;
        for rec in &ins_records {
            self.wal
                .log_page_write(tx_id, rec.page_id, &rec.before, &rec.after)?;
        }

        self.wal.log_commit(tx_id)?;
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Deletes a document from a collection.
    pub fn delete(&mut self, collection: &str, key: &Uuid) -> Result<()> {
        // Get value for unindexing
        let value = self
            .get(collection, key)?
            .ok_or(GrumpyError::KeyNotFound(*key))?;

        let tx_id = self.wal.begin_tx();
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let records = coll.delete_doc(key, &value)?;
        for rec in &records {
            self.wal
                .log_page_write(tx_id, rec.page_id, &rec.before, &rec.after)?;
        }

        self.wal.log_commit(tx_id)?;
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Scans documents in a collection.
    pub fn scan(
        &mut self,
        collection: &str,
        range: impl std::ops::RangeBounds<Uuid>,
    ) -> Result<Vec<(Uuid, Value)>> {
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let raw_results = coll.scan_raw(range)?;
        let mut results = Vec::with_capacity(raw_results.len());
        for (key, raw) in raw_results {
            let doc = Document::decode(&raw)?;
            results.push((key, doc.value));
        }
        Ok(results)
    }

    // ── Index management ────────────────────────────────────────────────

    /// Creates a secondary index on a collection field.
    pub fn create_index(
        &mut self,
        collection: &str,
        index_name: &str,
        field_path: &str,
    ) -> Result<()> {
        validate_name(index_name)?;
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
        coll.create_index(index_name, field_path)
    }

    /// Drops a secondary index.
    pub fn drop_index(&mut self, collection: &str, index_name: &str) -> Result<()> {
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
        coll.drop_index(index_name)
    }

    /// Queries a secondary index by exact value.
    pub fn query(
        &mut self,
        collection: &str,
        index_name: &str,
        value: &Value,
    ) -> Result<Vec<(Uuid, Value)>> {
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
        coll.query_index(index_name, value)
    }

    /// Queries a secondary index by range [start, end).
    pub fn query_range(
        &mut self,
        collection: &str,
        index_name: &str,
        start: &Value,
        end: &Value,
    ) -> Result<Vec<(Uuid, Value)>> {
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
        coll.query_index_range(index_name, start, end)
    }

    // ── References ─────────────────────────────────────────────────────

    /// Resolves a single reference to its target document value.
    pub fn resolve_ref(&mut self, collection: &str, id: &Uuid) -> Result<Option<Value>> {
        self.get(collection, id)
    }

    /// Recursively resolves all `Ref` values in a value tree.
    ///
    /// Each `Value::Ref(collection, uuid)` is replaced by the target document's
    /// value. Cycles are detected and return `GrumpyError::CyclicReference`.
    pub fn resolve_deep(&mut self, value: &Value, max_depth: usize) -> Result<Value> {
        let mut visited = HashSet::new();
        self.resolve_recursive(value, max_depth, 0, &mut visited)
    }

    fn resolve_recursive(
        &mut self,
        value: &Value,
        max_depth: usize,
        depth: usize,
        visited: &mut HashSet<(String, Uuid)>,
    ) -> Result<Value> {
        if depth > max_depth {
            return Ok(value.clone());
        }

        match value {
            Value::Ref(collection, uuid) => {
                let key = (collection.clone(), *uuid);
                if !visited.insert(key) {
                    return Err(GrumpyError::CyclicReference);
                }
                match self.get(collection, uuid)? {
                    Some(target) => self.resolve_recursive(&target, max_depth, depth + 1, visited),
                    None => Ok(value.clone()), // target not found — keep ref as-is
                }
            }
            Value::Object(map) => {
                let mut resolved = std::collections::BTreeMap::new();
                for (k, v) in map {
                    resolved.insert(k.clone(), self.resolve_recursive(v, max_depth, depth, visited)?);
                }
                Ok(Value::Object(resolved))
            }
            Value::Array(arr) => {
                let resolved: Result<Vec<Value>> = arr
                    .iter()
                    .map(|v| self.resolve_recursive(v, max_depth, depth, visited))
                    .collect();
                Ok(Value::Array(resolved?))
            }
            _ => Ok(value.clone()),
        }
    }

    // ── Maintenance ─────────────────────────────────────────────────────

    /// Returns the document count for a collection.
    pub fn document_count(&mut self, collection: &str) -> Result<u64> {
        let coll = self
            .collections
            .get(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
        Ok(coll.document_count())
    }

    /// Flushes all collections and writes a WAL checkpoint.
    pub fn flush(&mut self) -> Result<()> {
        for coll in self.collections.values_mut() {
            coll.flush()?;
        }
        self.wal.log_checkpoint()?;
        self.wal.truncate()?;
        self.writes_since_checkpoint = 0;
        Ok(())
    }

    /// Compacts a specific collection.
    pub fn compact(&mut self, collection: &str) -> Result<u64> {
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
        let count = coll.compact()?;
        self.wal.log_checkpoint()?;
        self.wal.truncate()?;
        self.writes_since_checkpoint = 0;
        Ok(count)
    }

    /// Closes the database, flushing all data.
    pub fn close(mut self) -> Result<()> {
        self.flush()
    }

    /// Returns the database directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

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

    fn setup() -> (TempDir, Database) {
        let dir = TempDir::new().unwrap();
        let db = Database::open(dir.path().join("testdb").as_path()).unwrap();
        (dir, db)
    }

    #[test]
    fn test_database_open_creates_dir() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("newdb");
        let _db = Database::open(&db_path).unwrap();
        assert!(db_path.exists());
    }

    #[test]
    fn test_create_and_list_collections() {
        let (_dir, mut db) = setup();
        assert!(db.list_collections().is_empty());

        db.create_collection("users").unwrap();
        db.create_collection("tasks").unwrap();

        let colls = db.list_collections();
        assert_eq!(colls, vec!["tasks", "users"]);
    }

    #[test]
    fn test_drop_collection() {
        let (_dir, mut db) = setup();
        db.create_collection("temp").unwrap();
        db.insert("temp", Uuid::new_v4(), Value::Null).unwrap();

        db.drop_collection("temp").unwrap();
        assert!(db.list_collections().is_empty());
        assert!(db.get("temp", &Uuid::new_v4()).is_err());
    }

    #[test]
    fn test_crud_across_collections() {
        let (_dir, mut db) = setup();
        db.create_collection("users").unwrap();
        db.create_collection("tasks").unwrap();

        let user_key = Uuid::from_u128(1);
        let task_key = Uuid::from_u128(2);

        db.insert(
            "users",
            user_key,
            Value::Object(BTreeMap::from([(
                "name".into(),
                Value::String("Alice".into()),
            )])),
        )
        .unwrap();

        db.insert("tasks", task_key, Value::String("Buy milk".into()))
            .unwrap();

        // Verify isolation
        assert!(db.get("users", &user_key).unwrap().is_some());
        assert!(db.get("tasks", &task_key).unwrap().is_some());
        assert!(db.get("users", &task_key).unwrap().is_none());
        assert!(db.get("tasks", &user_key).unwrap().is_none());
    }

    #[test]
    fn test_update_and_delete() {
        let (_dir, mut db) = setup();
        db.create_collection("items").unwrap();

        let key = Uuid::from_u128(42);
        db.insert("items", key, Value::Integer(1)).unwrap();

        db.update("items", &key, Value::Integer(2)).unwrap();
        assert_eq!(db.get("items", &key).unwrap(), Some(Value::Integer(2)));

        db.delete("items", &key).unwrap();
        assert_eq!(db.get("items", &key).unwrap(), None);
    }

    #[test]
    fn test_scan() {
        let (_dir, mut db) = setup();
        db.create_collection("nums").unwrap();

        for i in 0u128..10 {
            db.insert("nums", Uuid::from_u128(i), Value::Integer(i as i64))
                .unwrap();
        }

        let all = db.scan("nums", ..).unwrap();
        assert_eq!(all.len(), 10);
    }

    #[test]
    fn test_document_count() {
        let (_dir, mut db) = setup();
        db.create_collection("c").unwrap();
        assert_eq!(db.document_count("c").unwrap(), 0);

        db.insert("c", Uuid::new_v4(), Value::Null).unwrap();
        assert_eq!(db.document_count("c").unwrap(), 1);
    }

    #[test]
    fn test_secondary_index_via_database() {
        let (_dir, mut db) = setup();
        db.create_collection("users").unwrap();

        let u1 = Uuid::from_u128(1);
        let u2 = Uuid::from_u128(2);

        db.insert(
            "users",
            u1,
            Value::Object(BTreeMap::from([
                ("name".into(), Value::String("Alice".into())),
                ("age".into(), Value::Integer(30)),
            ])),
        )
        .unwrap();

        db.insert(
            "users",
            u2,
            Value::Object(BTreeMap::from([
                ("name".into(), Value::String("Bob".into())),
                ("age".into(), Value::Integer(25)),
            ])),
        )
        .unwrap();

        db.create_index("users", "by_age", "age").unwrap();

        let results = db.query("users", "by_age", &Value::Integer(30)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, u1);

        let range = db
            .query_range("users", "by_age", &Value::Integer(20), &Value::Integer(31))
            .unwrap();
        assert_eq!(range.len(), 2);
    }

    #[test]
    fn test_persistence_across_reopen() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("persist");
        let key = Uuid::from_u128(99);

        {
            let mut db = Database::open(&db_path).unwrap();
            db.create_collection("data").unwrap();
            db.insert("data", key, Value::String("hello".into()))
                .unwrap();
            db.close().unwrap();
        }

        {
            let mut db = Database::open(&db_path).unwrap();
            assert_eq!(db.list_collections(), vec!["data"]);
            let val = db.get("data", &key).unwrap();
            assert_eq!(val, Some(Value::String("hello".into())));
        }
    }

    #[test]
    fn test_compact_collection() {
        let (_dir, mut db) = setup();
        db.create_collection("c").unwrap();

        for i in 0u128..50 {
            db.insert("c", Uuid::from_u128(i), Value::Integer(i as i64))
                .unwrap();
        }
        for i in 0u128..25 {
            db.delete("c", &Uuid::from_u128(i)).unwrap();
        }

        let count = db.compact("c").unwrap();
        assert_eq!(count, 25);
        assert_eq!(db.document_count("c").unwrap(), 25);
    }

    #[test]
    fn test_collection_not_found() {
        let (_dir, mut db) = setup();
        assert!(db.get("nope", &Uuid::new_v4()).is_err());
        assert!(db.insert("nope", Uuid::new_v4(), Value::Null).is_err());
    }

    #[test]
    fn test_invalid_collection_name() {
        let (_dir, mut db) = setup();
        assert!(db.create_collection("Bad-Name").is_err());
        assert!(db.create_collection("").is_err());
    }

    #[test]
    fn test_resolve_ref_simple() {
        let (_dir, mut db) = setup();
        db.create_collection("users").unwrap();
        db.create_collection("orders").unwrap();

        let user_key = Uuid::from_u128(1);
        db.insert(
            "users",
            user_key,
            Value::Object(BTreeMap::from([(
                "name".into(),
                Value::String("Alice".into()),
            )])),
        )
        .unwrap();

        // Insert an order referencing the user
        let order_key = Uuid::from_u128(2);
        db.insert(
            "orders",
            order_key,
            Value::Object(BTreeMap::from([
                ("product".into(), Value::String("Widget".into())),
                ("owner".into(), Value::Ref("users".into(), user_key)),
            ])),
        )
        .unwrap();

        // Resolve the ref
        let resolved = db.resolve_ref("users", &user_key).unwrap();
        assert!(resolved.is_some());
        let val = resolved.unwrap();
        assert_eq!(
            val.as_object().unwrap().get("name"),
            Some(&Value::String("Alice".into()))
        );
    }

    #[test]
    fn test_resolve_deep_nested_refs() {
        let (_dir, mut db) = setup();
        db.create_collection("a").unwrap();
        db.create_collection("b").unwrap();

        let key_a = Uuid::from_u128(10);
        let key_b = Uuid::from_u128(20);

        db.insert("a", key_a, Value::String("target_a".into()))
            .unwrap();
        db.insert(
            "b",
            key_b,
            Value::Object(BTreeMap::from([(
                "link".into(),
                Value::Ref("a".into(), key_a),
            )])),
        )
        .unwrap();

        // A document with a ref to b, which itself refs a
        let doc = Value::Object(BTreeMap::from([(
            "nested".into(),
            Value::Ref("b".into(), key_b),
        )]));

        let resolved = db.resolve_deep(&doc, 16).unwrap();
        // nested -> b's doc -> { link: "target_a" }
        let nested = resolved.as_object().unwrap().get("nested").unwrap();
        let link = nested.as_object().unwrap().get("link").unwrap();
        assert_eq!(link, &Value::String("target_a".into()));
    }

    #[test]
    fn test_resolve_deep_cycle_detection() {
        let (_dir, mut db) = setup();
        db.create_collection("items").unwrap();

        let key1 = Uuid::from_u128(100);
        let key2 = Uuid::from_u128(200);

        // key1 -> refs key2, key2 -> refs key1 (cycle)
        db.insert("items", key1, Value::Ref("items".into(), key2))
            .unwrap();
        db.insert("items", key2, Value::Ref("items".into(), key1))
            .unwrap();

        let doc = Value::Ref("items".into(), key1);
        let result = db.resolve_deep(&doc, 16);
        assert!(matches!(result, Err(GrumpyError::CyclicReference)));
    }

    #[test]
    fn test_resolve_ref_missing_target() {
        let (_dir, mut db) = setup();
        db.create_collection("items").unwrap();

        let missing = Uuid::from_u128(999);
        let result = db.resolve_ref("items", &missing).unwrap();
        assert!(result.is_none());
    }
}
