//! Command types for the GrumpyDB wire protocol.
//!
//! Each [`Command`] variant represents a single client request. Commands carry
//! metadata about the required [`Action`] and target [`Resource`] for RBAC
//! enforcement.

/// A parsed command from the client.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    // ── Authentication ──────────────────────────────────────────────
    /// Authenticate with tenant, username, and password.
    Login {
        tenant: String,
        username: String,
        password: String,
    },
    /// Set a JWT token for the current session.
    Token(String),
    /// Refresh an expired access token.
    Refresh(String),
    /// Display current session info.
    WhoAmI,

    // ── Session ─────────────────────────────────────────────────────
    /// Select the active database.
    Use(String),
    /// Health check.
    Ping,
    /// Close the connection.
    Quit,

    // ── Database management ─────────────────────────────────────────
    /// Create a new database.
    CreateDatabase(String),
    /// Drop an existing database.
    DropDatabase(String),
    /// List all databases.
    ListDatabases,

    // ── Collection management ───────────────────────────────────────
    /// Create a new collection.
    CreateCollection(String),
    /// Drop an existing collection.
    DropCollection(String),
    /// List all collections in the current database.
    ListCollections,

    // ── CRUD ────────────────────────────────────────────────────────
    /// Insert a document.
    Insert {
        collection: String,
        key: String,
        value: String,
    },
    /// Retrieve a document by key.
    Get {
        collection: String,
        key: String,
    },
    /// Update an existing document.
    Update {
        collection: String,
        key: String,
        value: String,
    },
    /// Delete a document by key.
    Delete {
        collection: String,
        key: String,
    },
    /// Scan documents in a key range.
    Scan {
        collection: String,
        start: Option<String>,
        end: Option<String>,
    },

    // ── Index management ────────────────────────────────────────────
    /// Create a secondary index.
    CreateIndex {
        collection: String,
        index_name: String,
        field_path: String,
    },
    /// Drop a secondary index.
    DropIndex {
        collection: String,
        index_name: String,
    },
    /// List indexes on a collection.
    ListIndexes(String),
    /// Query an index by exact value.
    Query {
        collection: String,
        index_name: String,
        value: String,
    },
    /// Query an index by value range.
    QueryRange {
        collection: String,
        index_name: String,
        start: String,
        end: String,
    },

    // ── Maintenance ─────────────────────────────────────────────────
    /// Compact a collection.
    Compact(String),
    /// Flush all data to disk.
    Flush,
    /// Count documents in a collection.
    Count(String),

    // ── User management ─────────────────────────────────────────────
    /// Create a new user in the current tenant.
    CreateUser {
        username: String,
        password: String,
    },
    /// Drop a user.
    DropUser(String),
    /// List all users in the current tenant (or specified tenant).
    ListUsers(Option<String>),
    /// Grant a role to a user.
    Grant {
        role: String,
        resource: String,
        username: String,
    },
    /// Revoke a role from a user.
    Revoke {
        role: String,
        resource: String,
        username: String,
    },

    // ── Tenant management ───────────────────────────────────────────
    /// Create a new tenant.
    CreateTenant(String),
    /// Drop a tenant.
    DropTenant(String),
    /// List all tenants.
    ListTenants,
}

/// The type of action a command performs (for RBAC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Read data: GET, SCAN, QUERY, QUERYRANGE, COUNT.
    Read,
    /// Write data: INSERT, UPDATE, DELETE.
    Write,
    /// Administer collections/indexes: CREATE/DROP COLLECTION, CREATE/DROP INDEX, COMPACT, FLUSH.
    Admin,
    /// Manage users: CREATE/DROP USER, GRANT, REVOKE.
    ManageUsers,
    /// Manage databases: CREATE/DROP DATABASE.
    ManageDatabases,
    /// Manage the server: CREATE/DROP TENANT.
    ManageServer,
    /// Session commands that bypass RBAC: LOGIN, TOKEN, REFRESH, PING, QUIT, USE, WHOAMI.
    Session,
}

/// The target resource of a command (for RBAC scope checking).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// The entire server.
    Server,
    /// A specific database.
    Database(String),
    /// A specific collection within a database.
    Collection(String, String),
    /// No specific resource (session commands).
    None,
}

impl Command {
    /// Returns the action type required by this command.
    pub fn required_action(&self) -> Action {
        match self {
            // Read
            Command::Get { .. }
            | Command::Scan { .. }
            | Command::Query { .. }
            | Command::QueryRange { .. }
            | Command::Count(_) => Action::Read,

            // Write
            Command::Insert { .. }
            | Command::Update { .. }
            | Command::Delete { .. } => Action::Write,

            // Admin
            Command::CreateCollection(_)
            | Command::DropCollection(_)
            | Command::CreateIndex { .. }
            | Command::DropIndex { .. }
            | Command::ListCollections
            | Command::ListIndexes(_)
            | Command::Compact(_)
            | Command::Flush => Action::Admin,

            // Database management
            Command::CreateDatabase(_)
            | Command::DropDatabase(_)
            | Command::ListDatabases => Action::ManageDatabases,

            // User management
            Command::CreateUser { .. }
            | Command::DropUser(_)
            | Command::ListUsers(_)
            | Command::Grant { .. }
            | Command::Revoke { .. } => Action::ManageUsers,

            // Server management
            Command::CreateTenant(_)
            | Command::DropTenant(_)
            | Command::ListTenants => Action::ManageServer,

            // Session (bypass RBAC)
            Command::Login { .. }
            | Command::Token(_)
            | Command::Refresh(_)
            | Command::WhoAmI
            | Command::Use(_)
            | Command::Ping
            | Command::Quit => Action::Session,
        }
    }

    /// Returns the target resource for RBAC scope checking.
    ///
    /// `current_db` is the database selected with `USE`, needed for commands
    /// that operate within the current database context.
    pub fn target_resource(&self, current_db: Option<&str>) -> Resource {
        match self {
            // Server-level
            Command::CreateTenant(_)
            | Command::DropTenant(_)
            | Command::ListTenants => Resource::Server,

            // Database-level
            Command::CreateDatabase(name) | Command::DropDatabase(name) => {
                Resource::Database(name.clone())
            }
            // ListDatabases is scoped to the session tenant, not server-level
            Command::ListDatabases => Resource::None,

            // Collection-level (requires current_db)
            Command::Insert { collection, .. }
            | Command::Get { collection, .. }
            | Command::Update { collection, .. }
            | Command::Delete { collection, .. }
            | Command::Scan { collection, .. }
            | Command::Query { collection, .. }
            | Command::QueryRange { collection, .. }
            | Command::Count(collection)
            | Command::Compact(collection)
            | Command::CreateIndex { collection, .. }
            | Command::DropIndex { collection, .. }
            | Command::ListIndexes(collection) => {
                let db = current_db.unwrap_or("").to_string();
                Resource::Collection(db, collection.clone())
            }

            Command::CreateCollection(_)
            | Command::DropCollection(_)
            | Command::ListCollections
            | Command::Flush => {
                let db = current_db.unwrap_or("").to_string();
                Resource::Database(db)
            }

            // User management scoped to tenant
            Command::CreateUser { .. }
            | Command::DropUser(_)
            | Command::ListUsers(_)
            | Command::Grant { .. }
            | Command::Revoke { .. } => Resource::Server,

            // Session — no resource
            Command::Login { .. }
            | Command::Token(_)
            | Command::Refresh(_)
            | Command::WhoAmI
            | Command::Use(_)
            | Command::Ping
            | Command::Quit => Resource::None,
        }
    }

    /// Returns `true` if this command is allowed before authentication.
    pub fn is_pre_auth(&self) -> bool {
        matches!(
            self,
            Command::Login { .. } | Command::Ping | Command::Quit
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_crud_actions() {
        let insert = Command::Insert {
            collection: "users".into(),
            key: "abc".into(),
            value: "{}".into(),
        };
        assert_eq!(insert.required_action(), Action::Write);

        let get = Command::Get {
            collection: "users".into(),
            key: "abc".into(),
        };
        assert_eq!(get.required_action(), Action::Read);

        let delete = Command::Delete {
            collection: "users".into(),
            key: "abc".into(),
        };
        assert_eq!(delete.required_action(), Action::Write);
    }

    #[test]
    fn test_command_admin_actions() {
        assert_eq!(
            Command::CreateCollection("x".into()).required_action(),
            Action::Admin
        );
        assert_eq!(
            Command::CreateDatabase("x".into()).required_action(),
            Action::ManageDatabases
        );
        assert_eq!(
            Command::CreateTenant("x".into()).required_action(),
            Action::ManageServer
        );
        assert_eq!(
            Command::CreateUser {
                username: "x".into(),
                password: "y".into()
            }
            .required_action(),
            Action::ManageUsers
        );
    }

    #[test]
    fn test_command_session_actions() {
        assert_eq!(Command::Ping.required_action(), Action::Session);
        assert_eq!(Command::Quit.required_action(), Action::Session);
        assert_eq!(Command::Use("db".into()).required_action(), Action::Session);
        assert_eq!(
            Command::Login {
                tenant: "t".into(),
                username: "u".into(),
                password: "p".into()
            }
            .required_action(),
            Action::Session
        );
    }

    #[test]
    fn test_command_target_resource() {
        let insert = Command::Insert {
            collection: "users".into(),
            key: "k".into(),
            value: "v".into(),
        };
        assert_eq!(
            insert.target_resource(Some("mydb")),
            Resource::Collection("mydb".into(), "users".into())
        );

        let create_db = Command::CreateDatabase("staging".into());
        assert_eq!(
            create_db.target_resource(None),
            Resource::Database("staging".into())
        );

        let create_tenant = Command::CreateTenant("acme".into());
        assert_eq!(create_tenant.target_resource(None), Resource::Server);
    }

    #[test]
    fn test_command_is_pre_auth() {
        assert!(Command::Ping.is_pre_auth());
        assert!(Command::Quit.is_pre_auth());
        assert!(Command::Login {
            tenant: "t".into(),
            username: "u".into(),
            password: "p".into()
        }
        .is_pre_auth());

        assert!(!Command::ListDatabases.is_pre_auth());
        assert!(!Command::WhoAmI.is_pre_auth());
    }
}
