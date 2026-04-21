//! Storage engine: orchestrates all subsystems to provide CRUD operations.

use std::path::Path;
use uuid::Uuid;

use crate::document::value::Value;
use crate::error::Result;

/// The main GrumpyDB storage engine.
///
/// Provides CRUD operations on schema-less documents identified by UUID keys.
/// Documents are stored in page-based files with B+Tree indexing and WAL durability.
pub struct GrumpyDb {
    // TODO: Add fields (Phase 4)
    // - page_manager: PageManager (data.db)
    // - btree: BTree (index.db)
    // - wal: WalWriter (wal.log)
    // - buffer_pool: BufferPool
    // - lock_manager: LockManager
    _path: std::path::PathBuf,
}

impl GrumpyDb {
    /// Opens or creates a database at the given directory path.
    ///
    /// If the directory contains existing database files, they are opened and
    /// WAL recovery is performed if needed. Otherwise, new files are created.
    pub fn open(path: &Path) -> Result<Self> {
        // TODO: Implement (Phase 4)
        std::fs::create_dir_all(path)?;
        Ok(Self {
            _path: path.to_path_buf(),
        })
    }

    /// Inserts a document with the given UUID key.
    ///
    /// Returns an error if the key already exists.
    pub fn insert(&self, _key: Uuid, _value: Value) -> Result<()> {
        // TODO: Implement (Phase 4)
        todo!("insert not yet implemented")
    }

    /// Retrieves a document by its UUID key.
    ///
    /// Returns `None` if the key does not exist.
    pub fn get(&self, _key: &Uuid) -> Result<Option<Value>> {
        // TODO: Implement (Phase 4)
        todo!("get not yet implemented")
    }

    /// Updates an existing document.
    ///
    /// Returns an error if the key does not exist.
    pub fn update(&self, _key: &Uuid, _value: Value) -> Result<()> {
        // TODO: Implement (Phase 4)
        todo!("update not yet implemented")
    }

    /// Deletes a document by its UUID key.
    ///
    /// Returns an error if the key does not exist.
    pub fn delete(&self, _key: &Uuid) -> Result<()> {
        // TODO: Implement (Phase 4)
        todo!("delete not yet implemented")
    }

    /// Scans documents in a UUID key range.
    ///
    /// Returns all documents whose keys fall within the given range, sorted by key.
    pub fn scan(&self, _range: impl std::ops::RangeBounds<Uuid>) -> Result<Vec<(Uuid, Value)>> {
        // TODO: Implement (Phase 4)
        todo!("scan not yet implemented")
    }

    /// Flushes all dirty pages and writes a WAL checkpoint.
    pub fn flush(&self) -> Result<()> {
        // TODO: Implement (Phase 5)
        todo!("flush not yet implemented")
    }

    /// Closes the database, flushing all pending data.
    pub fn close(self) -> Result<()> {
        // TODO: Implement (Phase 4)
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_open_creates_directory() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        let db = GrumpyDb::open(&db_path).unwrap();
        assert!(db_path.exists());
        db.close().unwrap();
    }

    #[test]
    fn test_open_existing_directory() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("testdb");
        std::fs::create_dir_all(&db_path).unwrap();
        let db = GrumpyDb::open(&db_path).unwrap();
        db.close().unwrap();
    }
}
