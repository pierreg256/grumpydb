//! GrumpyDB — A disk-based object storage engine.
//!
//! GrumpyDB stores schema-less documents (JSON-like) on disk with B+Tree indexing,
//! page-based storage, WAL for durability, and SWMR concurrency.
//!
//! # Example
//!
//! ```no_run
//! use grumpydb::{Database, Value};
//! use uuid::Uuid;
//! use std::collections::BTreeMap;
//!
//! let mut db = Database::open(std::path::Path::new("./my_database")).unwrap();
//! db.create_collection("docs").unwrap();
//!
//! let key = Uuid::new_v4();
//! let value = Value::Object(BTreeMap::from([
//!     ("name".into(), Value::String("GrumpyDB".into())),
//!     ("version".into(), Value::Integer(1)),
//! ]));
//!
//! db.insert("docs", key, value).unwrap();
//!
//! let doc = db.get("docs", &key).unwrap();
//! assert!(doc.is_some());
//! db.close().unwrap();
//! ```

// Forbid panics in production engine code. Doc-comment examples and `#[cfg(test)]`
// modules are exempt; new code should propagate errors via `Result<T, GrumpyError>`.
#![cfg_attr(
    not(test),
    warn(clippy::unwrap_used, clippy::panic, clippy::expect_used)
)]

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

// Concurrency primitives. `SharedDb` is the SWMR wrapper for the deprecated
// `GrumpyDb`; prefer `SharedDatabase` (multi-collection) for new code.
#[allow(deprecated)]
pub use concurrency::lock_manager::SharedDb;
pub use concurrency::shared::{SharedDatabase, SharedServer};
pub use database::Database;
pub use document::value::Value;
pub use engine::CompactResult;
// `GrumpyDb` is deprecated in v5 and removed in v6 — kept here for the
// deprecation cycle. New code should use `Database`.
#[allow(deprecated)]
pub use engine::GrumpyDb;
pub use error::{GrumpyError, Result};
pub use index::IndexDefinition;
pub use server::GrumpyServer;
pub use server::client::Client;
