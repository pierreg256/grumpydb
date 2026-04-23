//! Client: manages multiple named databases for a single tenant.
//!
//! A client is the unit of isolation — each client has their own databases
//! and cannot access databases belonging to other clients.
//!
//! ## On-disk layout
//!
//! ```text
//! <client_dir>/
//!   <database_name>/
//!     wal.log
//!     <collection_name>/
//!       data.db
//!       primary.idx
//!       idx_*.idx
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::database::Database;
use crate::error::{GrumpyError, Result};
use crate::naming::validate_name;

/// A client containing multiple named databases.
pub struct Client {
    /// Client name.
    name: String,
    /// Path to the client directory.
    path: PathBuf,
    /// Open databases (lazily loaded).
    databases: HashMap<String, Database>,
}

impl Client {
    /// Opens or creates a client at the given directory.
    pub fn open(path: &Path, name: &str) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        // Discover existing databases by scanning subdirectories
        let mut databases = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let ft = entry.file_type()?;
                if ft.is_dir() {
                    let db_name = entry.file_name().to_string_lossy().to_string();
                    // Skip hidden dirs
                    if db_name.starts_with('.') {
                        continue;
                    }
                    let db_path = entry.path();
                    // Only open if it looks like a database (has wal.log or subdirs with data.db)
                    if db_path.join("wal.log").exists() || has_collection_subdirs(&db_path) {
                        let db = Database::open(&db_path)?;
                        databases.insert(db_name, db);
                    }
                }
            }
        }

        Ok(Self {
            name: name.to_string(),
            path: path.to_path_buf(),
            databases,
        })
    }

    /// Creates a new database.
    pub fn create_database(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        if self.databases.contains_key(name) {
            return Err(GrumpyError::DatabaseNotFound(format!(
                "database '{name}' already exists"
            )));
        }
        let db_path = self.path.join(name);
        let db = Database::open(&db_path)?;
        self.databases.insert(name.to_string(), db);
        Ok(())
    }

    /// Drops a database, closing it and deleting all its files.
    pub fn drop_database(&mut self, name: &str) -> Result<()> {
        let db = self
            .databases
            .remove(name)
            .ok_or_else(|| GrumpyError::DatabaseNotFound(name.into()))?;
        let db_path = db.path().to_path_buf();
        drop(db);
        std::fs::remove_dir_all(&db_path)?;
        Ok(())
    }

    /// Returns a mutable reference to a database (lazy open if needed).
    pub fn database(&mut self, name: &str) -> Result<&mut Database> {
        // If not yet open, try to open from disk
        if !self.databases.contains_key(name) {
            let db_path = self.path.join(name);
            if db_path.exists() {
                let db = Database::open(&db_path)?;
                self.databases.insert(name.to_string(), db);
            } else {
                return Err(GrumpyError::DatabaseNotFound(name.into()));
            }
        }
        self.databases
            .get_mut(name)
            .ok_or_else(|| GrumpyError::DatabaseNotFound(name.into()))
    }

    /// Lists all database names.
    pub fn list_databases(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.databases.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// Returns the client name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the client directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Closes the client, flushing all databases.
    pub fn close(mut self) -> Result<()> {
        for (_, db) in self.databases.drain() {
            db.close()?;
        }
        Ok(())
    }
}

/// Check if a directory has any subdirectories that look like collections.
fn has_collection_subdirs(path: &Path) -> bool {
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|ft| ft.is_dir())
                && entry.path().join("data.db").exists()
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Client) {
        let dir = TempDir::new().unwrap();
        let client = Client::open(dir.path().join("alice").as_path(), "alice").unwrap();
        (dir, client)
    }

    #[test]
    fn test_client_open_creates_dir() {
        let dir = TempDir::new().unwrap();
        let client_path = dir.path().join("newclient");
        let _client = Client::open(&client_path, "newclient").unwrap();
        assert!(client_path.exists());
    }

    #[test]
    fn test_client_name() {
        let (_dir, client) = setup();
        assert_eq!(client.name(), "alice");
    }

    #[test]
    fn test_create_and_list_databases() {
        let (_dir, mut client) = setup();
        assert!(client.list_databases().is_empty());

        client.create_database("myapp").unwrap();
        client.create_database("staging").unwrap();

        let dbs = client.list_databases();
        assert_eq!(dbs, vec!["myapp", "staging"]);
    }

    #[test]
    fn test_drop_database() {
        let (_dir, mut client) = setup();
        client.create_database("temp").unwrap();

        // Insert some data to make sure it's a real database
        let db = client.database("temp").unwrap();
        db.create_collection("items").unwrap();

        client.drop_database("temp").unwrap();
        assert!(client.list_databases().is_empty());
        assert!(client.database("temp").is_err());
    }

    #[test]
    fn test_drop_nonexistent_database() {
        let (_dir, mut client) = setup();
        assert!(client.drop_database("nope").is_err());
    }

    #[test]
    fn test_database_access() {
        let (_dir, mut client) = setup();
        client.create_database("mydb").unwrap();

        let db = client.database("mydb").unwrap();
        db.create_collection("users").unwrap();
        assert_eq!(db.list_collections(), vec!["users"]);
    }

    #[test]
    fn test_database_isolation() {
        let (_dir, mut client) = setup();
        client.create_database("db1").unwrap();
        client.create_database("db2").unwrap();

        {
            let db1 = client.database("db1").unwrap();
            db1.create_collection("users").unwrap();
        }
        {
            let db2 = client.database("db2").unwrap();
            db2.create_collection("tasks").unwrap();
        }

        assert_eq!(
            client.database("db1").unwrap().list_collections(),
            vec!["users"]
        );
        assert_eq!(
            client.database("db2").unwrap().list_collections(),
            vec!["tasks"]
        );
    }

    #[test]
    fn test_persistence_across_reopen() {
        let dir = TempDir::new().unwrap();
        let client_path = dir.path().join("persistent");

        {
            let mut client = Client::open(&client_path, "persistent").unwrap();
            client.create_database("mydb").unwrap();
            let db = client.database("mydb").unwrap();
            db.create_collection("items").unwrap();
            db.insert("items", uuid::Uuid::from_u128(1), crate::Value::Integer(42))
                .unwrap();
            client.close().unwrap();
        }

        {
            let mut client = Client::open(&client_path, "persistent").unwrap();
            assert_eq!(client.list_databases(), vec!["mydb"]);
            let db = client.database("mydb").unwrap();
            assert_eq!(db.list_collections(), vec!["items"]);
            let val = db.get("items", &uuid::Uuid::from_u128(1)).unwrap();
            assert_eq!(val, Some(crate::Value::Integer(42)));
        }
    }

    #[test]
    fn test_lazy_open() {
        let dir = TempDir::new().unwrap();
        let client_path = dir.path().join("lazy");

        // Create a database on disk manually
        {
            let mut client = Client::open(&client_path, "lazy").unwrap();
            client.create_database("existing").unwrap();
            client.close().unwrap();
        }

        // Reopen — databases should be auto-discovered
        let mut client = Client::open(&client_path, "lazy").unwrap();
        assert!(client.database("existing").is_ok());
    }

    #[test]
    fn test_invalid_database_name() {
        let (_dir, mut client) = setup();
        assert!(client.create_database("Bad-Name").is_err());
        assert!(client.create_database("").is_err());
    }
}
