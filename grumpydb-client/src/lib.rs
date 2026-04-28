//! GrumpyDB Rust Client Driver.
//!
//! Async client for connecting to a GrumpyDB server over TCP+TLS.
//!
//! # Example
//!
//! ```no_run
//! use grumpydb_client::GrumpyClient;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), grumpydb_client::ClientError> {
//!     let mut client = GrumpyClient::connect("localhost", 6380, false).await?;
//!     client.login("acme", "alice", "s3cr3t").await?;
//!     let db = client.database("myapp").await?;
//!     Ok(())
//! }
//! ```

mod connection;
mod error;

pub use error::ClientError;

use connection::Connection;
use grumpydb_protocol::Response;
use uuid::Uuid;

/// A GrumpyDB client connected to a server.
pub struct GrumpyClient {
    conn: Connection,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

impl GrumpyClient {
    /// Connect to a GrumpyDB server.
    pub async fn connect(host: &str, port: u16, tls: bool) -> Result<Self, ClientError> {
        let conn = Connection::connect(host, port, tls).await?;
        Ok(Self {
            conn,
            access_token: None,
            refresh_token: None,
        })
    }

    /// Authenticate with the server.
    pub async fn login(
        &mut self,
        tenant: &str,
        username: &str,
        password: &str,
    ) -> Result<(), ClientError> {
        let resp = self
            .conn
            .execute(&format!("LOGIN {tenant} {username} {password}"))
            .await?;
        match resp {
            Response::Ok(msg) if msg.starts_with("TOKEN ") => {
                let parts: Vec<&str> = msg[6..].splitn(2, ' ').collect();
                self.access_token = Some(parts[0].to_string());
                if parts.len() > 1 {
                    self.refresh_token = Some(parts[1].to_string());
                }
                // Set session token
                if let Some(token) = &self.access_token {
                    self.conn.execute(&format!("TOKEN {token}")).await?;
                }
                Ok(())
            }
            Response::Error(msg) => Err(ClientError::Auth(msg)),
            _ => Err(ClientError::Protocol("unexpected LOGIN response".into())),
        }
    }

    /// Select a database, returning a scoped handle.
    pub async fn database(&mut self, name: &str) -> Result<DatabaseHandle<'_>, ClientError> {
        let resp = self.conn.execute(&format!("USE {name}")).await?;
        match resp {
            Response::Ok(_) => Ok(DatabaseHandle {
                conn: &mut self.conn,
                _db: name.to_string(),
            }),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Protocol("unexpected USE response".into())),
        }
    }

    /// Create a database.
    pub async fn create_database(&mut self, name: &str) -> Result<(), ClientError> {
        expect_ok(
            self.conn
                .execute(&format!("CREATE DATABASE {name}"))
                .await?,
        )
    }

    /// Drop a database.
    pub async fn drop_database(&mut self, name: &str) -> Result<(), ClientError> {
        expect_ok(self.conn.execute(&format!("DROP DATABASE {name}")).await?)
    }

    /// List all databases.
    pub async fn list_databases(&mut self) -> Result<Vec<String>, ClientError> {
        expect_string_array(self.conn.execute("LIST DATABASES").await?)
    }

    /// Get session info.
    pub async fn whoami(&mut self) -> Result<String, ClientError> {
        match self.conn.execute("WHOAMI").await? {
            Response::Ok(msg) => Ok(msg),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Protocol("unexpected WHOAMI response".into())),
        }
    }

    /// Close the connection.
    pub async fn close(&mut self) -> Result<(), ClientError> {
        let _ = self.conn.execute("QUIT").await;
        Ok(())
    }

    /// Ping the server.
    pub async fn ping(&mut self) -> Result<(), ClientError> {
        expect_ok(self.conn.execute("PING").await?)
    }

    /// Execute a raw protocol command and return the response.
    /// Used by the `grumpy-repl` TCP backend for direct command forwarding.
    pub async fn raw_execute(&mut self, cmd: &str) -> Result<Response, ClientError> {
        self.conn.execute(cmd).await
    }
}

/// A handle scoped to a specific database.
pub struct DatabaseHandle<'a> {
    conn: &'a mut Connection,
    _db: String,
}

impl DatabaseHandle<'_> {
    // ── CRUD ────────────────────────────────────────────────────

    /// Insert a document.
    pub async fn insert(
        &mut self,
        collection: &str,
        key: Uuid,
        value: &serde_json::Value,
    ) -> Result<(), ClientError> {
        let json =
            serde_json::to_string(value).map_err(|e| ClientError::Protocol(e.to_string()))?;
        expect_ok(
            self.conn
                .execute(&format!("INSERT {collection} {key} {json}"))
                .await?,
        )
    }

    /// Get a document by key.
    pub async fn get(
        &mut self,
        collection: &str,
        key: &Uuid,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        match self
            .conn
            .execute(&format!("GET {collection} {key}"))
            .await?
        {
            Response::Bulk(Some(data)) => {
                let val: serde_json::Value = serde_json::from_str(&data)
                    .map_err(|e| ClientError::Protocol(e.to_string()))?;
                Ok(Some(val))
            }
            Response::Bulk(None) => Ok(None),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Protocol("unexpected GET response".into())),
        }
    }

    /// Update a document.
    pub async fn update(
        &mut self,
        collection: &str,
        key: &Uuid,
        value: &serde_json::Value,
    ) -> Result<(), ClientError> {
        let json =
            serde_json::to_string(value).map_err(|e| ClientError::Protocol(e.to_string()))?;
        expect_ok(
            self.conn
                .execute(&format!("UPDATE {collection} {key} {json}"))
                .await?,
        )
    }

    /// Delete a document.
    pub async fn delete(&mut self, collection: &str, key: &Uuid) -> Result<(), ClientError> {
        expect_ok(
            self.conn
                .execute(&format!("DELETE {collection} {key}"))
                .await?,
        )
    }

    /// Scan documents in a collection.
    pub async fn scan(
        &mut self,
        collection: &str,
        start: Option<&Uuid>,
        end: Option<&Uuid>,
    ) -> Result<Vec<(String, serde_json::Value)>, ClientError> {
        let cmd = if let (Some(s), Some(e)) = (start, end) {
            format!("SCAN {collection} {s} {e}")
        } else {
            format!("SCAN {collection}")
        };
        parse_kv_array(self.conn.execute(&cmd).await?)
    }

    /// Count documents.
    pub async fn count(&mut self, collection: &str) -> Result<i64, ClientError> {
        match self.conn.execute(&format!("COUNT {collection}")).await? {
            Response::Integer(n) => Ok(n),
            Response::Error(msg) => Err(ClientError::Server(msg)),
            _ => Err(ClientError::Protocol("unexpected COUNT response".into())),
        }
    }

    // ── Collection management ───────────────────────────────────

    /// Create a collection.
    pub async fn create_collection(&mut self, name: &str) -> Result<(), ClientError> {
        expect_ok(
            self.conn
                .execute(&format!("CREATE COLLECTION {name}"))
                .await?,
        )
    }

    /// Drop a collection.
    pub async fn drop_collection(&mut self, name: &str) -> Result<(), ClientError> {
        expect_ok(
            self.conn
                .execute(&format!("DROP COLLECTION {name}"))
                .await?,
        )
    }

    /// List collections.
    pub async fn list_collections(&mut self) -> Result<Vec<String>, ClientError> {
        expect_string_array(self.conn.execute("LIST COLLECTIONS").await?)
    }

    // ── Index management ────────────────────────────────────────

    /// Create a secondary index.
    pub async fn create_index(
        &mut self,
        collection: &str,
        index_name: &str,
        field_path: &str,
    ) -> Result<(), ClientError> {
        expect_ok(
            self.conn
                .execute(&format!(
                    "CREATE INDEX {collection} {index_name} {field_path}"
                ))
                .await?,
        )
    }

    /// Drop a secondary index.
    pub async fn drop_index(
        &mut self,
        collection: &str,
        index_name: &str,
    ) -> Result<(), ClientError> {
        expect_ok(
            self.conn
                .execute(&format!("DROP INDEX {collection} {index_name}"))
                .await?,
        )
    }

    /// List indexes.
    pub async fn list_indexes(&mut self, collection: &str) -> Result<Vec<String>, ClientError> {
        expect_string_array(
            self.conn
                .execute(&format!("LIST INDEXES {collection}"))
                .await?,
        )
    }

    /// Query index by exact value.
    pub async fn query(
        &mut self,
        collection: &str,
        index_name: &str,
        value: &serde_json::Value,
    ) -> Result<Vec<(String, serde_json::Value)>, ClientError> {
        let json =
            serde_json::to_string(value).map_err(|e| ClientError::Protocol(e.to_string()))?;
        parse_kv_array(
            self.conn
                .execute(&format!("QUERY {collection} {index_name} {json}"))
                .await?,
        )
    }

    // ── Maintenance ─────────────────────────────────────────────

    /// Compact a collection.
    pub async fn compact(&mut self, collection: &str) -> Result<(), ClientError> {
        expect_ok(self.conn.execute(&format!("COMPACT {collection}")).await?)
    }

    /// Flush all data to disk.
    pub async fn flush(&mut self) -> Result<(), ClientError> {
        expect_ok(self.conn.execute("FLUSH").await?)
    }
}

// ── Response helpers ────────────────────────────────────────────────────

fn expect_ok(resp: Response) -> Result<(), ClientError> {
    match resp {
        Response::Ok(_) => Ok(()),
        Response::Error(msg) => Err(ClientError::Server(msg)),
        _ => Err(ClientError::Protocol("expected OK response".into())),
    }
}

fn expect_string_array(resp: Response) -> Result<Vec<String>, ClientError> {
    match resp {
        Response::Array(items) => {
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                if let Response::Bulk(Some(s)) = item {
                    result.push(s);
                }
            }
            Ok(result)
        }
        Response::Error(msg) => Err(ClientError::Server(msg)),
        _ => Err(ClientError::Protocol("expected array response".into())),
    }
}

fn parse_kv_array(resp: Response) -> Result<Vec<(String, serde_json::Value)>, ClientError> {
    match resp {
        Response::Array(items) => {
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                if let Response::Bulk(Some(data)) = item
                    && let Some((key, json_str)) = data.split_once(' ')
                    && let Ok(val) = serde_json::from_str(json_str)
                {
                    result.push((key.to_string(), val));
                }
            }
            Ok(result)
        }
        Response::Error(msg) => Err(ClientError::Server(msg)),
        _ => Err(ClientError::Protocol("expected array response".into())),
    }
}
