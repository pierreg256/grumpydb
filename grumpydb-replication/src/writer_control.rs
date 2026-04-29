//! Single-writer enforcement for collections — slice 40e.6.
//!
//! This module checks whether a node is allowed to write to a specific
//! database and collection according to the cluster's static writer
//! assignment (from config `[cluster] writers`). In v5, failover is
//! manual; in v6+ it will be coordinated via RAFT or similar.
//!
//! A [`WriterAssignment`] maps `(database, collection)` → `node_id`.
//! The `*` wildcard collection means "database-level default".
//! Collections not in the map inherit from the database default.

use std::collections::HashMap;

/// Error returned when a node is not the writer for a resource.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not the writer for {database}/{collection}; current writer is {current_writer}")]
pub struct WriterNotAllowed {
    /// Database name.
    pub database: String,
    /// Collection name.
    pub collection: String,
    /// The current writer node ID.
    pub current_writer: String,
}

/// Persistent assignment of (database, collection) → node_id (writer).
///
/// Loaded from config `[cluster] writers`, where each entry is:
/// ```toml
/// [[cluster.writers]]
/// collection = "*"          # or a specific collection name
/// node_id = "node-1"        # UUID string
///
/// [[cluster.writers]]
/// collection = "users"
/// node_id = "node-2"
/// ```
///
/// Lookup rules:
/// 1. Exact match: `(db, coll)` → `node_id`
/// 2. Fallback: `(db, "*")` → `node_id` (database default)
/// 3. No entry: `None` (no write restriction, any node can write)
#[derive(Debug, Clone)]
pub struct WriterAssignment {
    /// Maps `"<db>/<coll>"` → `node_id` for exact matches.
    exact: HashMap<String, String>,
    /// Maps `"<db>/*"` → `node_id` for database defaults.
    defaults: HashMap<String, String>,
}

impl WriterAssignment {
    /// Create an empty assignment (no write restrictions).
    pub fn empty() -> Self {
        Self {
            exact: HashMap::new(),
            defaults: HashMap::new(),
        }
    }

    /// Insert or update an exact collection assignment.
    pub fn set_collection(&mut self, db: &str, coll: &str, node_id: String) {
        self.exact.insert(format!("{db}/{coll}"), node_id);
    }

    /// Insert or update a database-level default assignment.
    pub fn set_database_default(&mut self, db: &str, node_id: String) {
        self.defaults.insert(db.to_string(), node_id);
    }

    /// Lookup the writer node_id for a (database, collection) pair.
    /// Returns `None` if no assignment exists (no restriction).
    pub fn lookup(&self, db: &str, coll: &str) -> Option<String> {
        // Try exact match first
        if let Some(writer) = self.exact.get(&format!("{db}/{coll}")) {
            return Some(writer.clone());
        }
        // Fallback to database default
        if let Some(writer) = self.defaults.get(db) {
            return Some(writer.clone());
        }
        None
    }

    /// Check whether `node_id` is allowed to write to `(db, coll)`.
    /// If no assignment exists, returns `Ok(())` (no restriction).
    /// If an assignment exists but doesn't match, returns `Err(WriterNotAllowed)`.
    pub fn check_writer(
        &self,
        node_id: &str,
        db: &str,
        coll: &str,
    ) -> Result<(), WriterNotAllowed> {
        match self.lookup(db, coll) {
            None => Ok(()),
            Some(expected) if node_id == expected => Ok(()),
            Some(expected) => Err(WriterNotAllowed {
                database: db.to_string(),
                collection: coll.to_string(),
                current_writer: expected,
            }),
        }
    }

    /// Perform an election: assign a new writer for `(database, collection)`.
    /// If `collection` is `None`, assign the database-level default.
    pub fn elect(&mut self, node_id: String, database: &str, collection: Option<&str>) {
        match collection {
            None => self.set_database_default(database, node_id),
            Some(coll) => self.set_collection(database, coll, node_id),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_assignment_allows_any_writer() {
        let assign = WriterAssignment::empty();
        assert_eq!(assign.lookup("db1", "coll1"), None);
        assert!(assign.check_writer("any-node", "db1", "coll1").is_ok());
    }

    #[test]
    fn test_exact_collection_assignment() {
        let mut assign = WriterAssignment::empty();
        assign.set_collection("db1", "users", "node-1".to_string());

        assert_eq!(assign.lookup("db1", "users"), Some("node-1".to_string()));
        assert!(assign.check_writer("node-1", "db1", "users").is_ok());

        let err = assign.check_writer("node-2", "db1", "users").unwrap_err();
        assert_eq!(err.current_writer, "node-1");
    }

    #[test]
    fn test_database_default_assignment() {
        let mut assign = WriterAssignment::empty();
        assign.set_database_default("db1", "node-1".to_string());

        // Query a collection not explicitly assigned → inherits database default
        assert_eq!(assign.lookup("db1", "any_coll"), Some("node-1".to_string()));
        assert!(assign.check_writer("node-1", "db1", "any_coll").is_ok());

        let err = assign
            .check_writer("node-2", "db1", "any_coll")
            .unwrap_err();
        assert_eq!(err.current_writer, "node-1");
    }

    #[test]
    fn test_exact_overrides_default() {
        let mut assign = WriterAssignment::empty();
        assign.set_database_default("db1", "node-1".to_string());
        assign.set_collection("db1", "special", "node-2".to_string());

        // Exact match takes precedence
        assert_eq!(assign.lookup("db1", "special"), Some("node-2".to_string()));
        // Other collections inherit the default
        assert_eq!(assign.lookup("db1", "other"), Some("node-1".to_string()));
    }

    #[test]
    fn test_elect_database_level() {
        let mut assign = WriterAssignment::empty();
        assign.elect("node-1".to_string(), "db1", None);

        assert_eq!(assign.lookup("db1", "any_coll"), Some("node-1".to_string()));
    }

    #[test]
    fn test_elect_collection_level() {
        let mut assign = WriterAssignment::empty();
        assign.elect("node-2".to_string(), "db1", Some("users"));

        assert_eq!(assign.lookup("db1", "users"), Some("node-2".to_string()));
    }

    #[test]
    fn test_failover_updates_existing_assignment() {
        let mut assign = WriterAssignment::empty();
        assign.set_collection("db1", "users", "node-1".to_string());

        // Failover: elect node-2 as the new writer
        assign.elect("node-2".to_string(), "db1", Some("users"));

        assert_eq!(assign.lookup("db1", "users"), Some("node-2".to_string()));
        assert!(assign.check_writer("node-2", "db1", "users").is_ok());
        assert!(assign.check_writer("node-1", "db1", "users").is_err());
    }
}
