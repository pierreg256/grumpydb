//! Thread-safe wrappers for Database and GrumpyServer.
//!
//! - [`SharedDatabase`] wraps a `Database` in `Arc<RwLock>` for per-database SWMR.
//! - [`SharedServer`] manages multiple `SharedDatabase` instances, allowing
//!   concurrent writes to **different databases** while enforcing SWMR within each.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::database::Database;
use crate::document::value::Value;
use crate::error::{GrumpyError, Result};
use crate::server::GrumpyServer;
use crate::wal::hlc::Hlc;

// ── SharedDatabase ──────────────────────────────────────────────────────

/// A thread-safe handle to a single [`Database`].
///
/// Multiple threads can read concurrently. Writes acquire an exclusive lock.
/// Clone is cheap (Arc clone).
#[derive(Clone)]
pub struct SharedDatabase {
    inner: Arc<RwLock<Database>>,
}

/// Snapshot-pinned read handle for [`SharedDatabase`].
///
/// Phase 41 (tranche 1): the handle keeps a stable `snapshot_hlc` and offers
/// read helpers. Reads currently reuse the existing engine path.
pub struct SharedReadTx {
    inner: Arc<RwLock<Database>>,
    snapshot_hlc: Hlc,
}

impl Clone for SharedReadTx {
    fn clone(&self) -> Self {
        self.inner
            .write()
            .register_reader_snapshot(self.snapshot_hlc);
        Self {
            inner: Arc::clone(&self.inner),
            snapshot_hlc: self.snapshot_hlc,
        }
    }
}

impl Drop for SharedReadTx {
    fn drop(&mut self) {
        self.inner
            .write()
            .unregister_reader_snapshot(self.snapshot_hlc);
    }
}

impl SharedReadTx {
    /// HLC snapshot associated with this read transaction.
    pub fn snapshot_hlc(&self) -> Hlc {
        self.snapshot_hlc
    }

    /// Reads one document at this transaction snapshot.
    pub fn get(&self, collection: &str, key: &Uuid) -> Result<Option<Value>> {
        self.inner
            .write()
            .snapshot_get(collection, key, self.snapshot_hlc)
    }

    /// Scans a range at this transaction snapshot.
    pub fn scan(
        &self,
        collection: &str,
        range: impl std::ops::RangeBounds<Uuid>,
    ) -> Result<Vec<(Uuid, Value)>> {
        self.inner
            .write()
            .snapshot_scan(collection, range, self.snapshot_hlc)
    }

    /// Queries an index at this transaction snapshot.
    pub fn query(
        &self,
        collection: &str,
        index_name: &str,
        value: &Value,
    ) -> Result<Vec<(Uuid, Value)>> {
        self.inner
            .write()
            .snapshot_query(collection, index_name, value, self.snapshot_hlc)
    }

    /// Range-query on an index at this transaction snapshot.
    pub fn query_range(
        &self,
        collection: &str,
        index_name: &str,
        start: &Value,
        end: &Value,
    ) -> Result<Vec<(Uuid, Value)>> {
        self.inner.write().snapshot_query_range(
            collection,
            index_name,
            start,
            end,
            self.snapshot_hlc,
        )
    }
}

impl SharedDatabase {
    /// Wraps an existing Database in a thread-safe handle.
    pub fn new(db: Database) -> Self {
        Self {
            inner: Arc::new(RwLock::new(db)),
        }
    }

    /// Opens or creates a database, returning a thread-safe handle.
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::open(path)?;
        Ok(Self::new(db))
    }

    /// Begins a snapshot-pinned read transaction.
    pub fn begin_read(&self) -> SharedReadTx {
        let mut db = self.inner.write();
        let snapshot_hlc = db.current_hlc();
        db.register_reader_snapshot(snapshot_hlc);
        drop(db);
        SharedReadTx {
            inner: Arc::clone(&self.inner),
            snapshot_hlc,
        }
    }

    // ── Collection management ───────────────────────────────────────

    /// Creates a new collection.
    pub fn create_collection(&self, name: &str) -> Result<()> {
        self.inner.write().create_collection(name)
    }

    /// Drops a collection.
    pub fn drop_collection(&self, name: &str) -> Result<()> {
        self.inner.write().drop_collection(name)
    }

    /// Lists all collection names.
    pub fn list_collections(&self) -> Vec<String> {
        let db = self.inner.read();
        db.list_collections()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    // ── CRUD ────────────────────────────────────────────────────────

    /// Inserts a document.
    pub fn insert(&self, collection: &str, key: Uuid, value: Value) -> Result<()> {
        self.inner.write().insert(collection, key, value)
    }

    /// Retrieves a document.
    pub fn get(&self, collection: &str, key: &Uuid) -> Result<Option<Value>> {
        self.inner.write().get(collection, key)
    }

    /// Updates a document.
    pub fn update(&self, collection: &str, key: &Uuid, value: Value) -> Result<()> {
        self.inner.write().update(collection, key, value)
    }

    /// Deletes a document.
    pub fn delete(&self, collection: &str, key: &Uuid) -> Result<()> {
        self.inner.write().delete(collection, key)
    }

    /// Scans documents in a range.
    pub fn scan(
        &self,
        collection: &str,
        range: impl std::ops::RangeBounds<Uuid>,
    ) -> Result<Vec<(Uuid, Value)>> {
        self.inner.write().scan(collection, range)
    }

    // ── Index management ────────────────────────────────────────────

    /// Creates a secondary index.
    pub fn create_index(&self, collection: &str, index_name: &str, field_path: &str) -> Result<()> {
        self.inner
            .write()
            .create_index(collection, index_name, field_path)
    }

    /// Drops a secondary index.
    pub fn drop_index(&self, collection: &str, index_name: &str) -> Result<()> {
        self.inner.write().drop_index(collection, index_name)
    }

    /// Lists all secondary indexes on a collection (returns owned names).
    pub fn list_indexes(&self, collection: &str) -> Result<Vec<String>> {
        let mut guard = self.inner.write();
        let coll = guard.collection(collection)?;
        Ok(coll.list_indexes().iter().map(|d| d.name.clone()).collect())
    }

    /// Queries a secondary index by exact value.
    pub fn query(
        &self,
        collection: &str,
        index_name: &str,
        value: &Value,
    ) -> Result<Vec<(Uuid, Value)>> {
        self.inner.write().query(collection, index_name, value)
    }

    /// Queries a secondary index by range.
    pub fn query_range(
        &self,
        collection: &str,
        index_name: &str,
        start: &Value,
        end: &Value,
    ) -> Result<Vec<(Uuid, Value)>> {
        self.inner
            .write()
            .query_range(collection, index_name, start, end)
    }

    // ── References ──────────────────────────────────────────────────

    /// Resolves a single reference.
    pub fn resolve_ref(&self, collection: &str, id: &Uuid) -> Result<Option<Value>> {
        self.inner.write().resolve_ref(collection, id)
    }

    /// Recursively resolves references.
    pub fn resolve_deep(&self, value: &Value, max_depth: usize) -> Result<Value> {
        self.inner.write().resolve_deep(value, max_depth)
    }

    // ── Maintenance ─────────────────────────────────────────────────

    /// Returns the document count for a collection.
    pub fn document_count(&self, collection: &str) -> Result<u64> {
        self.inner.write().document_count(collection)
    }

    /// Flushes all data to disk.
    pub fn flush(&self) -> Result<()> {
        self.inner.write().flush()
    }

    /// Compacts a collection.
    pub fn compact(&self, collection: &str) -> Result<u64> {
        self.inner.write().compact(collection)
    }

    /// Returns database-level consistency defaults.
    pub fn consistency_defaults(&self) -> (Option<u16>, Option<u16>) {
        self.inner.read().consistency_defaults()
    }

    /// Sets database-level consistency defaults.
    pub fn set_consistency_defaults(
        &self,
        read_concern: Option<u16>,
        write_concern: Option<u16>,
    ) -> Result<()> {
        self.inner
            .write()
            .set_consistency_defaults(read_concern, write_concern)
    }

    /// Resets database-level consistency defaults.
    pub fn reset_consistency_defaults(&self) -> Result<()> {
        self.inner.write().reset_consistency_defaults()
    }

    /// Closes the database. Consumes the handle.
    pub fn close(self) -> Result<()> {
        match Arc::try_unwrap(self.inner) {
            Ok(lock) => lock.into_inner().close(),
            Err(_) => Ok(()),
        }
    }
}

// ── SharedServer ────────────────────────────────────────────────────────

/// A thread-safe multi-tenant server.
///
/// Each database gets its own `SharedDatabase` with independent locking.
/// Concurrent writes to **different databases** proceed without contention.
pub struct SharedServer {
    /// The underlying server (for client/database management).
    server: Arc<RwLock<GrumpyServer>>,
    /// Per-database shared handles (client_name/db_name → SharedDatabase).
    databases: Arc<RwLock<HashMap<String, SharedDatabase>>>,
}

impl Clone for SharedServer {
    fn clone(&self) -> Self {
        Self {
            server: Arc::clone(&self.server),
            databases: Arc::clone(&self.databases),
        }
    }
}

impl SharedServer {
    /// Opens or creates a server, returning a thread-safe handle.
    pub fn open(path: &Path) -> Result<Self> {
        let server = GrumpyServer::open(path)?;
        Ok(Self {
            server: Arc::new(RwLock::new(server)),
            databases: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    // ── Client management ───────────────────────────────────────────

    /// Creates a new client.
    pub fn create_client(&self, name: &str) -> Result<()> {
        self.server.write().create_client(name)
    }

    /// Drops a client, removing all associated database handles.
    pub fn drop_client(&self, name: &str) -> Result<()> {
        // Remove cached database handles for this client
        {
            let mut dbs = self.databases.write();
            let prefix = format!("{name}/");
            dbs.retain(|k, _| !k.starts_with(&prefix));
        }
        self.server.write().drop_client(name)
    }

    /// Lists all client names.
    pub fn list_clients(&self) -> Vec<String> {
        let server = self.server.read();
        server
            .list_clients()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    // ── Database management ─────────────────────────────────────────

    /// Creates a database for a client.
    pub fn create_database(&self, client: &str, db_name: &str) -> Result<()> {
        self.server.write().client(client)?.create_database(db_name)
    }

    /// Drops a database.
    pub fn drop_database(&self, client: &str, db_name: &str) -> Result<()> {
        let key = format!("{client}/{db_name}");
        self.databases.write().remove(&key);
        self.server.write().client(client)?.drop_database(db_name)
    }

    /// Lists databases for a client.
    pub fn list_databases(&self, client: &str) -> Result<Vec<String>> {
        let mut server = self.server.write();
        let c = server.client(client)?;
        Ok(c.list_databases()
            .into_iter()
            .map(|s| s.to_string())
            .collect())
    }

    /// Returns a `SharedDatabase` handle for independent per-database locking.
    ///
    /// The database is extracted from the server on first access and wrapped
    /// in its own `Arc<RwLock>`. Subsequent calls return the cached handle.
    pub fn database(&self, client: &str, db_name: &str) -> Result<SharedDatabase> {
        let key = format!("{client}/{db_name}");

        // Check cache first
        {
            let dbs = self.databases.read();
            if let Some(db) = dbs.get(&key) {
                return Ok(db.clone());
            }
        }

        // Open the database from the server and cache it
        let db_path = {
            let server = self.server.read();
            server.path().join(client).join(db_name)
        };

        if !db_path.exists() {
            return Err(GrumpyError::DatabaseNotFound(db_name.into()));
        }

        let shared_db = SharedDatabase::open(&db_path)?;
        self.databases.write().insert(key, shared_db.clone());
        Ok(shared_db)
    }

    /// Returns database-level consistency defaults.
    pub fn database_consistency_defaults(
        &self,
        client: &str,
        db_name: &str,
    ) -> Result<(Option<u16>, Option<u16>)> {
        self.database(client, db_name)
            .map(|db| db.consistency_defaults())
    }

    /// Sets database-level consistency defaults.
    pub fn set_database_consistency_defaults(
        &self,
        client: &str,
        db_name: &str,
        read_concern: Option<u16>,
        write_concern: Option<u16>,
    ) -> Result<()> {
        self.database(client, db_name)?
            .set_consistency_defaults(read_concern, write_concern)
    }

    /// Resets database-level consistency defaults.
    pub fn reset_database_consistency_defaults(&self, client: &str, db_name: &str) -> Result<()> {
        self.database(client, db_name)?.reset_consistency_defaults()
    }

    /// Closes the server.
    pub fn close(self) -> Result<()> {
        // Drop all cached database handles
        drop(self.databases);
        match Arc::try_unwrap(self.server) {
            Ok(lock) => lock.into_inner().close(),
            Err(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use tempfile::TempDir;

    // ── SharedDatabase tests ────────────────────────────────────────

    fn setup_shared_db() -> (TempDir, SharedDatabase) {
        let dir = TempDir::new().unwrap();
        let db = SharedDatabase::open(dir.path().join("testdb").as_path()).unwrap();
        (dir, db)
    }

    #[test]
    fn test_shared_database_crud() {
        let (_dir, db) = setup_shared_db();
        db.create_collection("items").unwrap();
        let key = Uuid::from_u128(1);
        db.insert("items", key, Value::Integer(42)).unwrap();
        assert_eq!(db.get("items", &key).unwrap(), Some(Value::Integer(42)));
        db.update("items", &key, Value::Integer(99)).unwrap();
        assert_eq!(db.get("items", &key).unwrap(), Some(Value::Integer(99)));
        db.delete("items", &key).unwrap();
        assert_eq!(db.get("items", &key).unwrap(), None);
    }

    #[test]
    fn test_shared_database_concurrent_reads() {
        let (_dir, db) = setup_shared_db();
        db.create_collection("nums").unwrap();

        for i in 0u128..50 {
            db.insert("nums", Uuid::from_u128(i), Value::Integer(i as i64))
                .unwrap();
        }

        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();

        for _ in 0..8 {
            let db = db.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0u128..50 {
                    let val = db.get("nums", &Uuid::from_u128(i)).unwrap();
                    assert_eq!(val, Some(Value::Integer(i as i64)));
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_shared_database_begin_read_snapshot() {
        let (_dir, db) = setup_shared_db();
        db.create_collection("c").unwrap();
        let key = Uuid::from_u128(1);
        db.insert("c", key, Value::Integer(7)).unwrap();

        let tx = db.begin_read();
        assert_eq!(tx.get("c", &key).unwrap(), Some(Value::Integer(7)));
        assert!(tx.snapshot_hlc().0 > 0);
    }

    #[test]
    fn test_shared_read_tx_clone_tracks_watermark() {
        let (_dir, db) = setup_shared_db();
        db.create_collection("c").unwrap();
        db.insert("c", Uuid::from_u128(1), Value::Integer(1))
            .unwrap();

        let tx1 = db.begin_read();
        let snapshot = tx1.snapshot_hlc();
        assert_eq!(db.inner.read().reader_watermark(), Some(snapshot));

        let tx2 = tx1.clone();
        assert_eq!(db.inner.read().reader_watermark(), Some(snapshot));

        drop(tx1);
        assert_eq!(db.inner.read().reader_watermark(), Some(snapshot));

        drop(tx2);
        assert_eq!(db.inner.read().reader_watermark(), None);
    }

    #[test]
    fn test_shared_read_tx_drop_triggers_version_gc() {
        let (_dir, db) = setup_shared_db();
        db.create_collection("c").unwrap();

        let key = Uuid::from_u128(11);
        db.insert("c", key, Value::Integer(1)).unwrap();

        let tx1 = db.begin_read();
        let snapshot = tx1.snapshot_hlc();
        let tx2 = tx1.clone();

        db.update("c", &key, Value::Integer(2)).unwrap();
        db.update("c", &key, Value::Integer(3)).unwrap();

        assert_eq!(tx1.get("c", &key).unwrap(), Some(Value::Integer(1)));
        assert_eq!(db.inner.read().reader_watermark(), Some(snapshot));
        assert!(db.inner.read().debug_version_len("c", &key) >= 3);

        drop(tx1);
        assert_eq!(db.inner.read().reader_watermark(), Some(snapshot));

        drop(tx2);
        assert_eq!(db.inner.read().reader_watermark(), None);
        assert_eq!(db.inner.read().debug_version_len("c", &key), 1);
        assert_eq!(db.get("c", &key).unwrap(), Some(Value::Integer(3)));
    }

    #[test]
    fn test_shared_database_writer_and_readers() {
        let (_dir, db) = setup_shared_db();
        db.create_collection("data").unwrap();

        for i in 0u128..100 {
            db.insert("data", Uuid::from_u128(i), Value::Integer(i as i64))
                .unwrap();
        }

        let barrier = Arc::new(Barrier::new(5));

        // Writer
        let db_w = db.clone();
        let b_w = barrier.clone();
        let writer = std::thread::spawn(move || {
            b_w.wait();
            for i in 100u128..200 {
                db_w.insert("data", Uuid::from_u128(i), Value::Integer(i as i64))
                    .unwrap();
            }
        });

        // 4 readers
        let mut readers = Vec::new();
        for _ in 0..4 {
            let db = db.clone();
            let b = barrier.clone();
            readers.push(std::thread::spawn(move || {
                b.wait();
                for i in 0u128..100 {
                    let val = db.get("data", &Uuid::from_u128(i)).unwrap();
                    assert_eq!(val, Some(Value::Integer(i as i64)));
                }
            }));
        }

        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }

        assert_eq!(db.document_count("data").unwrap(), 200);
    }

    #[test]
    fn test_shared_database_collections_and_indexes() {
        let (_dir, db) = setup_shared_db();
        db.create_collection("users").unwrap();
        db.create_index("users", "by_age", "age").unwrap();

        let key = Uuid::from_u128(1);
        let val = Value::Object(std::collections::BTreeMap::from([
            ("name".into(), Value::String("Alice".into())),
            ("age".into(), Value::Integer(30)),
        ]));
        db.insert("users", key, val).unwrap();

        let results = db.query("users", "by_age", &Value::Integer(30)).unwrap();
        assert_eq!(results.len(), 1);
    }

    // ── SharedServer tests ──────────────────────────────────────────

    fn setup_shared_server() -> (TempDir, SharedServer) {
        let dir = TempDir::new().unwrap();
        let server = SharedServer::open(dir.path().join("root").as_path()).unwrap();
        (dir, server)
    }

    #[test]
    fn test_shared_server_client_management() {
        let (_dir, server) = setup_shared_server();
        server.create_client("alice").unwrap();
        server.create_client("bob").unwrap();

        let clients = server.list_clients();
        assert_eq!(clients, vec!["alice", "bob"]);

        server.drop_client("bob").unwrap();
        assert_eq!(server.list_clients(), vec!["alice"]);
    }

    #[test]
    fn test_shared_server_database_access() {
        let (_dir, server) = setup_shared_server();
        server.create_client("alice").unwrap();
        server.create_database("alice", "mydb").unwrap();

        let db = server.database("alice", "mydb").unwrap();
        db.create_collection("items").unwrap();
        db.insert("items", Uuid::from_u128(1), Value::Integer(42))
            .unwrap();
        assert_eq!(
            db.get("items", &Uuid::from_u128(1)).unwrap(),
            Some(Value::Integer(42))
        );
    }

    #[test]
    fn test_shared_server_concurrent_different_databases() {
        let (_dir, server) = setup_shared_server();
        server.create_client("alice").unwrap();

        // Create 4 databases
        for i in 0..4 {
            server.create_database("alice", &format!("db{i}")).unwrap();
            let db = server.database("alice", &format!("db{i}")).unwrap();
            db.create_collection("items").unwrap();
        }

        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();

        // 4 threads, each writing to a different database
        for t in 0..4u128 {
            let server = server.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let db = server.database("alice", &format!("db{t}")).unwrap();
                barrier.wait();
                for i in 0..50 {
                    db.insert(
                        "items",
                        Uuid::from_u128(t * 1000 + i),
                        Value::Integer(i as i64),
                    )
                    .unwrap();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Verify each database has 50 docs
        for i in 0..4 {
            let db = server.database("alice", &format!("db{i}")).unwrap();
            assert_eq!(db.document_count("items").unwrap(), 50);
        }
    }

    #[test]
    fn test_shared_server_8_threads_4_databases() {
        let (_dir, server) = setup_shared_server();
        server.create_client("test").unwrap();

        for i in 0..4 {
            server.create_database("test", &format!("db{i}")).unwrap();
            let db = server.database("test", &format!("db{i}")).unwrap();
            db.create_collection("data").unwrap();
        }

        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();

        // 8 threads, 2 per database
        for t in 0..8u128 {
            let server = server.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let db_idx = t % 4;
                let db = server.database("test", &format!("db{db_idx}")).unwrap();
                barrier.wait();
                for i in 0..25 {
                    let key = Uuid::from_u128(t * 1000 + i);
                    db.insert("data", key, Value::Integer((t * 1000 + i) as i64))
                        .unwrap();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Each database should have 50 docs (2 threads × 25 each)
        for i in 0..4 {
            let db = server.database("test", &format!("db{i}")).unwrap();
            assert_eq!(db.document_count("data").unwrap(), 50);
        }
    }

    #[test]
    fn test_shared_server_writer_and_readers_per_db() {
        let (_dir, server) = setup_shared_server();
        server.create_client("c").unwrap();
        server.create_database("c", "mydb").unwrap();

        let db = server.database("c", "mydb").unwrap();
        db.create_collection("nums").unwrap();

        // Pre-insert
        for i in 0u128..100 {
            db.insert("nums", Uuid::from_u128(i), Value::Integer(i as i64))
                .unwrap();
        }

        let barrier = Arc::new(Barrier::new(5));

        // 1 writer
        let db_w = db.clone();
        let b_w = barrier.clone();
        let writer = std::thread::spawn(move || {
            b_w.wait();
            for i in 100u128..200 {
                db_w.insert("nums", Uuid::from_u128(i), Value::Integer(i as i64))
                    .unwrap();
            }
        });

        // 4 readers
        let mut readers = Vec::new();
        for _ in 0..4 {
            let db = db.clone();
            let b = barrier.clone();
            readers.push(std::thread::spawn(move || {
                b.wait();
                for i in 0u128..100 {
                    let val = db.get("nums", &Uuid::from_u128(i)).unwrap();
                    assert_eq!(val, Some(Value::Integer(i as i64)));
                }
            }));
        }

        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }

        assert_eq!(db.document_count("nums").unwrap(), 200);
    }

    #[test]
    fn test_shared_server_cross_database_independence() {
        let (_dir, server) = setup_shared_server();
        server.create_client("c").unwrap();
        server.create_database("c", "fast").unwrap();
        server.create_database("c", "slow").unwrap();

        let db_fast = server.database("c", "fast").unwrap();
        let db_slow = server.database("c", "slow").unwrap();
        db_fast.create_collection("items").unwrap();
        db_slow.create_collection("items").unwrap();

        // Write to fast while slow is also being written to — no contention
        let barrier = Arc::new(Barrier::new(2));
        let b1 = barrier.clone();
        let b2 = barrier.clone();

        let h1 = std::thread::spawn(move || {
            b1.wait();
            for i in 0u128..100 {
                db_fast
                    .insert("items", Uuid::from_u128(i), Value::Integer(i as i64))
                    .unwrap();
            }
        });

        let h2 = std::thread::spawn(move || {
            b2.wait();
            for i in 0u128..100 {
                db_slow
                    .insert("items", Uuid::from_u128(1000 + i), Value::Integer(i as i64))
                    .unwrap();
            }
        });

        h1.join().unwrap();
        h2.join().unwrap();
    }

    #[test]
    fn test_shared_server_database_consistency_defaults_roundtrip() {
        let (_dir, server) = setup_shared_server();
        server.create_client("alice").unwrap();
        server.create_database("alice", "mydb").unwrap();

        server
            .set_database_consistency_defaults("alice", "mydb", Some(2), Some(3))
            .unwrap();
        assert_eq!(
            server
                .database_consistency_defaults("alice", "mydb")
                .unwrap(),
            (Some(2), Some(3))
        );

        server
            .reset_database_consistency_defaults("alice", "mydb")
            .unwrap();
        assert_eq!(
            server
                .database_consistency_defaults("alice", "mydb")
                .unwrap(),
            (None, None)
        );
    }
}
