//! Command types for the GrumpyDB wire protocol.
//!
//! Each [`Command`] variant represents a single client request. Commands carry
//! metadata about the required [`Action`] and target [`Resource`] for RBAC
//! enforcement.

/// A parsed command from the client.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    // ── Consistency wrappers (Phase 40f) ───────────────────────────
    /// Wraps another command with optional read/write concerns.
    ///
    /// In v5 servers, only `R=1` and `W=1` are accepted. The wrapper is
    /// preserved at the protocol layer so v6 can honor higher values without
    /// changing wire grammar.
    WithConsistency {
        read_concern: Option<u16>,
        write_concern: Option<u16>,
        command: Box<Command>,
    },

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
    /// Return cluster topology snapshot as JSON.
    Topology,
    /// Return the current database snapshot HLC so clients can pin reads.
    SnapshotHlc,

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
    /// Set default consistency concerns for a database.
    SetDatabaseConsistency {
        database: String,
        read_concern: Option<u16>,
        write_concern: Option<u16>,
    },
    /// Reset database-level consistency defaults to engine fallbacks.
    ResetDatabaseConsistency { database: String },
    /// Show effective consistency defaults configured for a database.
    ShowDatabaseConsistency { database: String },

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
    Get { collection: String, key: String },
    /// Update an existing document.
    Update {
        collection: String,
        key: String,
        value: String,
    },
    /// Delete a document by key.
    Delete { collection: String, key: String },
    /// Insert/update a reconciled value using an explicit vector clock.
    ///
    /// v5 stores the value through the regular write path and validates the
    /// vector-clock payload syntactically at the protocol boundary.
    PutWithVc {
        collection: String,
        key: String,
        value: String,
        vector_clock: String,
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
    CreateUser { username: String, password: String },
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

    // ── Cluster administration ──────────────────────────────────────
    /// Failover: elect a new writer for a database or collection.
    /// `collection` is optional; if absent, applies to the entire database.
    ElectWriter {
        node_id: String,
        database: String,
        collection: Option<String>,
    },
    /// Preview ownership deltas for adding a node.
    PlanRebalanceAddNode { node_id: String },
    /// Preview ownership deltas for removing a node.
    PlanRebalanceRemoveNode { node_id: String },
    /// Execute add-node transfer for one collection in the selected database.
    ExecuteRebalanceAddNode { node_id: String, collection: String },
    /// Execute remove-node transfer for one collection in the selected database.
    ExecuteRebalanceRemoveNode { node_id: String, collection: String },
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
    /// Manage the cluster: ELECT-WRITER, REBALANCE.
    ManageCluster,
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
            Command::WithConsistency { command, .. } => command.required_action(),

            // Read
            Command::Get { .. }
            | Command::Scan { .. }
            | Command::Query { .. }
            | Command::QueryRange { .. }
            | Command::Count(_) => Action::Read,

            // Write
            Command::Insert { .. }
            | Command::Update { .. }
            | Command::Delete { .. }
            | Command::PutWithVc { .. } => Action::Write,

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
            | Command::ListDatabases
            | Command::SetDatabaseConsistency { .. }
            | Command::ResetDatabaseConsistency { .. }
            | Command::ShowDatabaseConsistency { .. } => Action::ManageDatabases,

            // User management
            Command::CreateUser { .. }
            | Command::DropUser(_)
            | Command::ListUsers(_)
            | Command::Grant { .. }
            | Command::Revoke { .. } => Action::ManageUsers,

            // Server management
            Command::CreateTenant(_) | Command::DropTenant(_) | Command::ListTenants => {
                Action::ManageServer
            }

            // Cluster management
            Command::ElectWriter { .. }
            | Command::PlanRebalanceAddNode { .. }
            | Command::PlanRebalanceRemoveNode { .. }
            | Command::ExecuteRebalanceAddNode { .. }
            | Command::ExecuteRebalanceRemoveNode { .. } => Action::ManageCluster,

            // Session (bypass RBAC)
            Command::Login { .. }
            | Command::Token(_)
            | Command::Refresh(_)
            | Command::WhoAmI
            | Command::Topology
            | Command::SnapshotHlc
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
            Command::WithConsistency { command, .. } => command.target_resource(current_db),

            // Server-level
            Command::CreateTenant(_) | Command::DropTenant(_) | Command::ListTenants => {
                Resource::Server
            }

            // Database-level
            Command::CreateDatabase(name) | Command::DropDatabase(name) => {
                Resource::Database(name.clone())
            }
            Command::SetDatabaseConsistency { database, .. }
            | Command::ResetDatabaseConsistency { database }
            | Command::ShowDatabaseConsistency { database } => Resource::Database(database.clone()),
            // ListDatabases is scoped to the session tenant, not server-level
            Command::ListDatabases => Resource::None,

            // Collection-level (requires current_db)
            Command::Insert { collection, .. }
            | Command::Get { collection, .. }
            | Command::Update { collection, .. }
            | Command::Delete { collection, .. }
            | Command::PutWithVc { collection, .. }
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
            | Command::SnapshotHlc
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

            // Cluster management → server scope
            Command::ElectWriter { .. }
            | Command::PlanRebalanceAddNode { .. }
            | Command::PlanRebalanceRemoveNode { .. }
            | Command::ExecuteRebalanceAddNode { .. }
            | Command::ExecuteRebalanceRemoveNode { .. } => Resource::Server,

            // Session — no resource
            Command::Login { .. }
            | Command::Token(_)
            | Command::Refresh(_)
            | Command::WhoAmI
            | Command::Topology
            | Command::Use(_)
            | Command::Ping
            | Command::Quit => Resource::None,
        }
    }

    /// Returns `true` if this command is allowed before authentication.
    ///
    /// `Token` and `Refresh` belong here because they are themselves the means
    /// to authenticate a session: a freshly opened connection must be able to
    /// resume a session by submitting a previously issued access token, and
    /// must be able to swap an expired access token for a new one without
    /// going through full `LOGIN` again.
    pub fn is_pre_auth(&self) -> bool {
        matches!(
            self,
            Command::WithConsistency { command, .. } if command.is_pre_auth()
        ) || matches!(
            self,
            Command::Login { .. }
                | Command::Ping
                | Command::Quit
                | Command::Token(_)
                | Command::Refresh(_)
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

        let put_with_vc = Command::PutWithVc {
            collection: "users".into(),
            key: "abc".into(),
            value: "{}".into(),
            vector_clock: "{}".into(),
        };
        assert_eq!(put_with_vc.required_action(), Action::Write);
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
            Command::SetDatabaseConsistency {
                database: "x".into(),
                read_concern: Some(2),
                write_concern: Some(2),
            }
            .required_action(),
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
        assert_eq!(Command::Topology.required_action(), Action::Session);
        assert_eq!(Command::SnapshotHlc.required_action(), Action::Session);
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

        let set_db_consistency = Command::SetDatabaseConsistency {
            database: "staging".into(),
            read_concern: Some(2),
            write_concern: Some(3),
        };
        assert_eq!(
            set_db_consistency.target_resource(None),
            Resource::Database("staging".into())
        );

        let create_tenant = Command::CreateTenant("acme".into());
        assert_eq!(create_tenant.target_resource(None), Resource::Server);

        let with_consistency = Command::WithConsistency {
            read_concern: Some(1),
            write_concern: None,
            command: Box::new(Command::Get {
                collection: "users".into(),
                key: "k".into(),
            }),
        };
        assert_eq!(
            with_consistency.target_resource(Some("mydb")),
            Resource::Collection("mydb".into(), "users".into())
        );
    }

    #[test]
    fn test_command_cluster_actions() {
        let elect = Command::ElectWriter {
            node_id: "node-1".into(),
            database: "mydb".into(),
            collection: Some("mycoll".into()),
        };
        assert_eq!(elect.required_action(), Action::ManageCluster);
        assert_eq!(elect.target_resource(None), Resource::Server);

        let plan_add = Command::PlanRebalanceAddNode {
            node_id: "node-2".into(),
        };
        assert_eq!(plan_add.required_action(), Action::ManageCluster);
        assert_eq!(plan_add.target_resource(None), Resource::Server);

        let exec_remove = Command::ExecuteRebalanceRemoveNode {
            node_id: "node-2".into(),
            collection: "users".into(),
        };
        assert_eq!(exec_remove.required_action(), Action::ManageCluster);
        assert_eq!(exec_remove.target_resource(Some("db")), Resource::Server);
    }

    #[test]
    fn test_command_is_pre_auth() {
        assert!(Command::Ping.is_pre_auth());
        assert!(Command::Quit.is_pre_auth());
        assert!(
            Command::Login {
                tenant: "t".into(),
                username: "u".into(),
                password: "p".into()
            }
            .is_pre_auth()
        );

        assert!(!Command::ListDatabases.is_pre_auth());
        assert!(!Command::WhoAmI.is_pre_auth());

        let pre_auth_with_concern = Command::WithConsistency {
            read_concern: Some(1),
            write_concern: None,
            command: Box::new(Command::Ping),
        };
        assert!(pre_auth_with_concern.is_pre_auth());
    }
}
