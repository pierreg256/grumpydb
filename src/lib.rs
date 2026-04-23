//! GrumpyDB — A disk-based object storage engine.
//!
//! GrumpyDB stores schema-less documents (JSON-like) on disk with B+Tree indexing,
//! page-based storage, WAL for durability, and SWMR concurrency.
//!
//! # Example
//!
//! ```no_run
//! use grumpydb::{GrumpyDb, Value};
//! use uuid::Uuid;
//! use std::collections::BTreeMap;
//!
//! let mut db = GrumpyDb::open(std::path::Path::new("./my_database")).unwrap();
//!
//! let key = Uuid::new_v4();
//! let value = Value::Object(BTreeMap::from([
//!     ("name".into(), Value::String("GrumpyDB".into())),
//!     ("version".into(), Value::Integer(1)),
//! ]));
//!
//! db.insert(key, value).unwrap();
//!
//! let doc = db.get(&key).unwrap();
//! assert!(doc.is_some());
//! db.close().unwrap();
//! ```

pub mod btree;
pub mod buffer;
pub mod collection;
pub mod concurrency;
pub mod database;
pub mod document;
pub mod engine;
pub mod error;
pub mod index;
pub mod naming;
pub mod page;
pub mod server;
pub mod wal;

pub use concurrency::lock_manager::SharedDb;
pub use concurrency::shared::{SharedDatabase, SharedServer};
pub use database::Database;
pub use document::value::Value;
pub use engine::{CompactResult, GrumpyDb};
pub use error::{GrumpyError, Result};
pub use index::IndexDefinition;
pub use server::client::Client;
pub use server::GrumpyServer;
