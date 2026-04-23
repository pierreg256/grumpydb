//! Server: multi-tenant management of clients and databases.
//!
//! A [`GrumpyServer`] is the top-level entry point for multi-tenant use.
//! Each server manages multiple [`Client`]s, each client owns multiple
//! [`Database`](crate::database::Database)s.
//!
//! ## On-disk layout
//!
//! ```text
//! <server_root>/
//!   <client_name>/                     ← one directory per client
//!     <database_name>/                 ← one directory per database
//!       wal.log
//!       <collection_name>/
//!         data.db
//!         primary.idx
//!         idx_*.idx
//! ```

pub mod client;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{GrumpyError, Result};
use crate::naming::validate_name;

pub use client::Client;

/// A multi-tenant GrumpyDB server managing clients and their databases.
///
/// # Example
///
/// ```no_run
/// use grumpydb::server::GrumpyServer;
/// use grumpydb::Value;
/// use uuid::Uuid;
///
/// let mut server = GrumpyServer::open(std::path::Path::new("./data")).unwrap();
/// server.create_client("alice").unwrap();
///
/// let client = server.client("alice").unwrap();
/// client.create_database("myapp").unwrap();
///
/// let db = client.database("myapp").unwrap();
/// db.create_collection("users").unwrap();
/// db.insert("users", Uuid::new_v4(), Value::String("hello".into())).unwrap();
/// ```
pub struct GrumpyServer {
    /// Root directory.
    path: PathBuf,
    /// Named clients (lazily loaded).
    clients: HashMap<String, Client>,
}

impl GrumpyServer {
    /// Opens or creates a server at the given root directory.
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        // Discover existing clients by scanning subdirectories
        let mut clients = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let ft = entry.file_type()?;
                if ft.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    // Skip hidden dirs
                    if name.starts_with('.') {
                        continue;
                    }
                    let client_path = entry.path();
                    let client = Client::open(&client_path, &name)?;
                    clients.insert(name, client);
                }
            }
        }

        Ok(Self {
            path: path.to_path_buf(),
            clients,
        })
    }

    /// Creates a new client.
    pub fn create_client(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        if self.clients.contains_key(name) {
            return Err(GrumpyError::ClientNotFound(format!(
                "client '{name}' already exists"
            )));
        }
        let client_path = self.path.join(name);
        let client = Client::open(&client_path, name)?;
        self.clients.insert(name.to_string(), client);
        Ok(())
    }

    /// Drops a client, closing all databases and deleting all files.
    pub fn drop_client(&mut self, name: &str) -> Result<()> {
        let client = self
            .clients
            .remove(name)
            .ok_or_else(|| GrumpyError::ClientNotFound(name.into()))?;
        let client_path = client.path().to_path_buf();
        drop(client);
        std::fs::remove_dir_all(&client_path)?;
        Ok(())
    }

    /// Returns a mutable reference to a client (lazy open if needed).
    pub fn client(&mut self, name: &str) -> Result<&mut Client> {
        if !self.clients.contains_key(name) {
            let client_path = self.path.join(name);
            if client_path.exists() {
                let client = Client::open(&client_path, name)?;
                self.clients.insert(name.to_string(), client);
            } else {
                return Err(GrumpyError::ClientNotFound(name.into()));
            }
        }
        self.clients
            .get_mut(name)
            .ok_or_else(|| GrumpyError::ClientNotFound(name.into()))
    }

    /// Lists all client names.
    pub fn list_clients(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.clients.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// Returns the server root directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Closes the server, flushing all clients and databases.
    pub fn close(mut self) -> Result<()> {
        for (_, client) in self.clients.drain() {
            client.close()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, GrumpyServer) {
        let dir = TempDir::new().unwrap();
        let server = GrumpyServer::open(dir.path().join("root").as_path()).unwrap();
        (dir, server)
    }

    #[test]
    fn test_server_open_creates_dir() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("newroot");
        let _server = GrumpyServer::open(&root).unwrap();
        assert!(root.exists());
    }

    #[test]
    fn test_create_and_list_clients() {
        let (_dir, mut server) = setup();
        assert!(server.list_clients().is_empty());

        server.create_client("alice").unwrap();
        server.create_client("bob").unwrap();

        let clients = server.list_clients();
        assert_eq!(clients, vec!["alice", "bob"]);
    }

    #[test]
    fn test_drop_client() {
        let (_dir, mut server) = setup();
        server.create_client("temp").unwrap();

        // Add some data inside
        let client = server.client("temp").unwrap();
        client.create_database("mydb").unwrap();

        server.drop_client("temp").unwrap();
        assert!(server.list_clients().is_empty());
        assert!(server.client("temp").is_err());
    }

    #[test]
    fn test_drop_nonexistent_client() {
        let (_dir, mut server) = setup();
        assert!(server.drop_client("nope").is_err());
    }

    #[test]
    fn test_client_isolation() {
        let (_dir, mut server) = setup();
        server.create_client("alice").unwrap();
        server.create_client("bob").unwrap();

        // Alice creates a database
        {
            let alice = server.client("alice").unwrap();
            alice.create_database("myapp").unwrap();
            let db = alice.database("myapp").unwrap();
            db.create_collection("users").unwrap();
            db.insert(
                "users",
                uuid::Uuid::from_u128(1),
                crate::Value::String("Alice's data".into()),
            )
            .unwrap();
        }

        // Bob creates a different database
        {
            let bob = server.client("bob").unwrap();
            bob.create_database("production").unwrap();
            let db = bob.database("production").unwrap();
            db.create_collection("tasks").unwrap();
            db.insert(
                "tasks",
                uuid::Uuid::from_u128(2),
                crate::Value::String("Bob's task".into()),
            )
            .unwrap();
        }

        // Verify isolation
        {
            let alice = server.client("alice").unwrap();
            assert_eq!(alice.list_databases(), vec!["myapp"]);
            let db = alice.database("myapp").unwrap();
            assert_eq!(db.list_collections(), vec!["users"]);
        }
        {
            let bob = server.client("bob").unwrap();
            assert_eq!(bob.list_databases(), vec!["production"]);
            let db = bob.database("production").unwrap();
            assert_eq!(db.list_collections(), vec!["tasks"]);
        }
    }

    #[test]
    fn test_full_hierarchy() {
        let (_dir, mut server) = setup();
        server.create_client("alice").unwrap();

        let client = server.client("alice").unwrap();
        client.create_database("db1").unwrap();
        client.create_database("db2").unwrap();

        {
            let db1 = client.database("db1").unwrap();
            db1.create_collection("coll_a").unwrap();
            db1.create_collection("coll_b").unwrap();
        }
        {
            let db2 = client.database("db2").unwrap();
            db2.create_collection("coll_c").unwrap();
            db2.create_collection("coll_d").unwrap();
        }

        assert_eq!(
            client.database("db1").unwrap().list_collections(),
            vec!["coll_a", "coll_b"]
        );
        assert_eq!(
            client.database("db2").unwrap().list_collections(),
            vec!["coll_c", "coll_d"]
        );
    }

    #[test]
    fn test_persistence_across_reopen() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root");

        {
            let mut server = GrumpyServer::open(&root).unwrap();
            server.create_client("alice").unwrap();
            let client = server.client("alice").unwrap();
            client.create_database("mydb").unwrap();
            let db = client.database("mydb").unwrap();
            db.create_collection("items").unwrap();
            db.insert(
                "items",
                uuid::Uuid::from_u128(42),
                crate::Value::Integer(99),
            )
            .unwrap();
            server.close().unwrap();
        }

        {
            let mut server = GrumpyServer::open(&root).unwrap();
            assert_eq!(server.list_clients(), vec!["alice"]);
            let client = server.client("alice").unwrap();
            assert_eq!(client.list_databases(), vec!["mydb"]);
            let db = client.database("mydb").unwrap();
            assert_eq!(db.list_collections(), vec!["items"]);
            let val = db.get("items", &uuid::Uuid::from_u128(42)).unwrap();
            assert_eq!(val, Some(crate::Value::Integer(99)));
        }
    }

    #[test]
    fn test_invalid_client_name() {
        let (_dir, mut server) = setup();
        assert!(server.create_client("Bad-Name").is_err());
        assert!(server.create_client("").is_err());
    }

    #[test]
    fn test_two_clients_two_databases_each() {
        let (_dir, mut server) = setup();
        server.create_client("c1").unwrap();
        server.create_client("c2").unwrap();

        for client_name in &["c1", "c2"] {
            let client = server.client(client_name).unwrap();
            client.create_database("d1").unwrap();
            client.create_database("d2").unwrap();
            for db_name in &["d1", "d2"] {
                let db = client.database(db_name).unwrap();
                db.create_collection("items").unwrap();
                db.insert(
                    "items",
                    uuid::Uuid::new_v4(),
                    crate::Value::String(format!("{client_name}/{db_name}")),
                )
                .unwrap();
            }
        }

        // Verify each client has 2 databases
        for client_name in &["c1", "c2"] {
            let client = server.client(client_name).unwrap();
            assert_eq!(client.list_databases(), vec!["d1", "d2"]);
            for db_name in &["d1", "d2"] {
                let db = client.database(db_name).unwrap();
                assert_eq!(db.document_count("items").unwrap(), 1);
            }
        }
    }
}
