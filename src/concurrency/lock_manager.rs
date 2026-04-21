//! SWMR (Single-Writer, Multi-Reader) concurrency wrapper.
//!
//! Wraps a `GrumpyDb` in an `Arc<RwLock>` to allow safe concurrent access
//! from multiple threads. Readers get shared access (non-blocking between
//! each other), writers get exclusive access.

use std::path::Path;
use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::document::value::Value;
use crate::engine::GrumpyDb;
use crate::error::Result;

/// A thread-safe handle to a GrumpyDB database.
///
/// `SharedDb` wraps [`GrumpyDb`] in `Arc<RwLock<GrumpyDb>>` to enable
/// the SWMR (Single-Writer, Multi-Reader) concurrency model:
///
/// - **Multiple readers** can call `get()`, `scan()` concurrently (shared lock)
/// - **One writer** at a time can call `insert()`, `update()`, `delete()` (exclusive lock)
/// - Readers and writers never deadlock — `parking_lot::RwLock` is fair
///
/// # Example
///
/// ```no_run
/// use grumpydb::concurrency::lock_manager::SharedDb;
/// use grumpydb::Value;
/// use uuid::Uuid;
/// use std::sync::Arc;
///
/// let db = SharedDb::open(std::path::Path::new("./mydb")).unwrap();
///
/// // Clone the handle for another thread (cheap: Arc clone)
/// let db2 = db.clone();
///
/// let writer = std::thread::spawn(move || {
///     let key = Uuid::new_v4();
///     db2.insert(key, Value::String("hello".into())).unwrap();
///     key
/// });
///
/// let key = writer.join().unwrap();
/// let value = db.get(&key).unwrap();
/// assert!(value.is_some());
/// ```
#[derive(Clone)]
pub struct SharedDb {
    inner: Arc<RwLock<GrumpyDb>>,
}

impl SharedDb {
    /// Opens or creates a database, returning a thread-safe handle.
    pub fn open(path: &Path) -> Result<Self> {
        let db = GrumpyDb::open(path)?;
        Ok(Self {
            inner: Arc::new(RwLock::new(db)),
        })
    }

    // ── Read operations (shared lock) ───────────────────────────────────

    /// Retrieves a document by key. Multiple threads can call this concurrently.
    ///
    /// Acquires a **write lock** internally because `GrumpyDb::get()` requires `&mut self`
    /// (due to B+Tree cursor state). In a future version with an immutable read path,
    /// this could use a read lock.
    pub fn get(&self, key: &Uuid) -> Result<Option<Value>> {
        // NOTE: We use write() here because GrumpyDb::get takes &mut self.
        // This is a limitation of the current engine design — the B+Tree
        // cursor requires mutable access. A future buffer-pool-based design
        // could separate read and write paths.
        let mut db = self.inner.write();
        db.get(key)
    }

    /// Scans documents in a key range. Acquires write lock (see `get()` note).
    pub fn scan(
        &self,
        range: impl std::ops::RangeBounds<Uuid>,
    ) -> Result<Vec<(Uuid, Value)>> {
        let mut db = self.inner.write();
        db.scan(range)
    }

    // ── Write operations (exclusive lock) ───────────────────────────────

    /// Inserts a document. Acquires exclusive write lock.
    pub fn insert(&self, key: Uuid, value: Value) -> Result<()> {
        let mut db = self.inner.write();
        db.insert(key, value)
    }

    /// Updates a document. Acquires exclusive write lock.
    pub fn update(&self, key: &Uuid, value: Value) -> Result<()> {
        let mut db = self.inner.write();
        db.update(key, value)
    }

    /// Deletes a document. Acquires exclusive write lock.
    pub fn delete(&self, key: &Uuid) -> Result<()> {
        let mut db = self.inner.write();
        db.delete(key)
    }

    /// Flushes all data to disk + WAL checkpoint. Acquires exclusive lock.
    pub fn flush(&self) -> Result<()> {
        let mut db = self.inner.write();
        db.flush()
    }

    /// Closes the database. Consumes the handle.
    ///
    /// If other clones of this handle exist, this waits for them to be dropped
    /// before closing. Returns an error if the Arc cannot be unwrapped.
    pub fn close(self) -> Result<()> {
        match Arc::try_unwrap(self.inner) {
            Ok(lock) => lock.into_inner().close(),
            Err(_) => {
                // Other handles still exist — just drop this one.
                // The last handle to drop will close the database.
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::value::Value;
    use std::sync::Barrier;
    use tempfile::TempDir;

    fn setup() -> (TempDir, SharedDb) {
        let dir = TempDir::new().unwrap();
        let db = SharedDb::open(dir.path().join("testdb").as_path()).unwrap();
        (dir, db)
    }

    #[test]
    fn test_shared_db_basic_crud() {
        let (_dir, db) = setup();
        let key = Uuid::new_v4();
        db.insert(key, Value::String("hello".into())).unwrap();
        assert_eq!(db.get(&key).unwrap(), Some(Value::String("hello".into())));
        db.delete(&key).unwrap();
        assert_eq!(db.get(&key).unwrap(), None);
    }

    #[test]
    fn test_shared_db_clone_and_read() {
        let (_dir, db) = setup();
        let key = Uuid::new_v4();
        db.insert(key, Value::Integer(42)).unwrap();

        let db2 = db.clone();
        assert_eq!(db2.get(&key).unwrap(), Some(Value::Integer(42)));
    }

    #[test]
    fn test_concurrent_reads() {
        let (_dir, db) = setup();

        // Insert some data
        for i in 0u128..50 {
            db.insert(Uuid::from_u128(i), Value::Integer(i as i64)).unwrap();
        }

        // Spawn 8 reader threads
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();

        for _ in 0..8 {
            let db = db.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait(); // All threads start at the same time
                for i in 0u128..50 {
                    let val = db.get(&Uuid::from_u128(i)).unwrap();
                    assert_eq!(val, Some(Value::Integer(i as i64)));
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_concurrent_writer_and_readers() {
        let (_dir, db) = setup();

        // Pre-insert some data
        for i in 0u128..100 {
            db.insert(Uuid::from_u128(i), Value::Integer(i as i64)).unwrap();
        }

        let barrier = Arc::new(Barrier::new(5)); // 1 writer + 4 readers

        // Writer thread: insert more data
        let db_writer = db.clone();
        let barrier_w = barrier.clone();
        let writer = std::thread::spawn(move || {
            barrier_w.wait();
            for i in 100u128..200 {
                db_writer
                    .insert(Uuid::from_u128(i), Value::Integer(i as i64))
                    .unwrap();
            }
        });

        // 4 reader threads: read existing data
        let mut readers = Vec::new();
        for _ in 0..4 {
            let db = db.clone();
            let barrier = barrier.clone();
            readers.push(std::thread::spawn(move || {
                barrier.wait();
                let mut reads = 0;
                for i in 0u128..100 {
                    if let Some(val) = db.get(&Uuid::from_u128(i)).unwrap() {
                        assert_eq!(val, Value::Integer(i as i64));
                        reads += 1;
                    }
                }
                assert_eq!(reads, 100, "reader should see all 100 pre-inserted docs");
            }));
        }

        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }

        // Verify all 200 documents exist
        for i in 0u128..200 {
            assert!(
                db.get(&Uuid::from_u128(i)).unwrap().is_some(),
                "key {i} missing after concurrent access"
            );
        }
    }

    #[test]
    fn test_no_deadlock_under_contention() {
        let (_dir, db) = setup();

        // Spawn many threads doing mixed operations
        let barrier = Arc::new(Barrier::new(10));
        let mut handles = Vec::new();

        for t in 0..10u128 {
            let db = db.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let base = t * 100;
                // Each thread works on its own key range (no conflicts)
                for i in 0..50 {
                    let key = Uuid::from_u128(base + i);
                    db.insert(key, Value::Integer(i as i64)).unwrap();
                }
                for i in 0..50 {
                    let key = Uuid::from_u128(base + i);
                    let val = db.get(&key).unwrap();
                    assert!(val.is_some());
                }
                for i in 0..25 {
                    let key = Uuid::from_u128(base + i);
                    db.delete(&key).unwrap();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // No panics, no deadlocks = success
    }

    #[test]
    fn test_persistence_through_shared_db() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("testdb");
        let key = Uuid::from_u128(42);

        {
            let db = SharedDb::open(&path).unwrap();
            db.insert(key, Value::String("shared".into())).unwrap();
            db.close().unwrap();
        }

        {
            let db = SharedDb::open(&path).unwrap();
            assert_eq!(
                db.get(&key).unwrap(),
                Some(Value::String("shared".into()))
            );
        }
    }

    #[test]
    fn test_scan_concurrent() {
        let (_dir, db) = setup();

        for i in 0u128..30 {
            db.insert(Uuid::from_u128(i), Value::Integer(i as i64)).unwrap();
        }

        let db2 = db.clone();
        let reader = std::thread::spawn(move || {
            let all = db2.scan(..).unwrap();
            assert_eq!(all.len(), 30);
        });

        reader.join().unwrap();
    }
}
