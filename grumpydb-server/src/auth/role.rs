//! Role-based access control: roles, actions, resource scopes, permissions.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Predefined role names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleName {
    /// Full server access — cross-tenant.
    ServerAdmin,
    /// Manage databases and users within a tenant.
    TenantAdmin,
    /// Manage collections and indexes within a database.
    DbAdmin,
    /// Full CRUD within scope.
    ReadWrite,
    /// Read-only within scope.
    ReadOnly,
}

impl fmt::Display for RoleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RoleName::ServerAdmin => write!(f, "server_admin"),
            RoleName::TenantAdmin => write!(f, "tenant_admin"),
            RoleName::DbAdmin => write!(f, "db_admin"),
            RoleName::ReadWrite => write!(f, "read_write"),
            RoleName::ReadOnly => write!(f, "read_only"),
        }
    }
}

impl RoleName {
    /// Parse a role name from a string.
    pub fn from_str_name(s: &str) -> Option<RoleName> {
        match s {
            "server_admin" => Some(RoleName::ServerAdmin),
            "tenant_admin" => Some(RoleName::TenantAdmin),
            "db_admin" => Some(RoleName::DbAdmin),
            "read_write" => Some(RoleName::ReadWrite),
            "read_only" => Some(RoleName::ReadOnly),
            _ => None,
        }
    }

    /// Returns `true` if this role permits the given action type.
    pub fn permits_action(&self, action: &Action) -> bool {
        match self {
            RoleName::ServerAdmin => true,
            RoleName::TenantAdmin => !matches!(action, Action::ManageServer),
            RoleName::DbAdmin => matches!(action, Action::Read | Action::Write | Action::Admin),
            RoleName::ReadWrite => matches!(action, Action::Read | Action::Write),
            RoleName::ReadOnly => matches!(action, Action::Read),
        }
    }
}

/// The type of action being performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Read,
    Write,
    Admin,
    ManageUsers,
    ManageDatabases,
    ManageServer,
}

/// The scope of a role assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ResourceScope {
    /// Entire server (server_admin only).
    Server,
    /// All resources within the user's tenant.
    Tenant,
    /// All databases in the tenant.
    AllDatabases,
    /// A specific database.
    Database { name: String },
    /// All collections in a database.
    AllCollections { database: String },
    /// A specific collection.
    Collection {
        database: String,
        collection: String,
    },
}

impl ResourceScope {
    /// Returns `true` if this scope covers the given target resource.
    pub fn covers(&self, target: &ResourceScope) -> bool {
        match self {
            ResourceScope::Server => true,
            ResourceScope::Tenant => !matches!(target, ResourceScope::Server),
            ResourceScope::AllDatabases => matches!(
                target,
                ResourceScope::AllDatabases
                    | ResourceScope::Database { .. }
                    | ResourceScope::AllCollections { .. }
                    | ResourceScope::Collection { .. }
            ),
            ResourceScope::Database { name } => match target {
                ResourceScope::Database { name: t } => name == t,
                ResourceScope::AllCollections { database } => name == database,
                ResourceScope::Collection { database, .. } => name == database,
                _ => false,
            },
            ResourceScope::AllCollections { database } => match target {
                ResourceScope::AllCollections { database: t } => database == t,
                ResourceScope::Collection { database: td, .. } => database == td,
                _ => false,
            },
            ResourceScope::Collection {
                database,
                collection,
            } => match target {
                ResourceScope::Collection {
                    database: td,
                    collection: tc,
                } => database == td && collection == tc,
                _ => false,
            },
        }
    }
}

/// A role assigned to a user with a specific scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleAssignment {
    pub role: RoleName,
    pub scope: ResourceScope,
}

impl RoleAssignment {
    /// Returns `true` if this assignment permits the given action on the target.
    pub fn permits(&self, action: &Action, target: &ResourceScope) -> bool {
        self.role.permits_action(action) && self.scope.covers(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_name_display_and_parse() {
        let roles = [
            RoleName::ServerAdmin,
            RoleName::TenantAdmin,
            RoleName::DbAdmin,
            RoleName::ReadWrite,
            RoleName::ReadOnly,
        ];
        for role in &roles {
            let s = role.to_string();
            assert_eq!(RoleName::from_str_name(&s), Some(*role));
        }
        assert_eq!(RoleName::from_str_name("unknown"), None);
    }

    #[test]
    fn test_server_admin_permits_everything() {
        let sa = RoleName::ServerAdmin;
        assert!(sa.permits_action(&Action::Read));
        assert!(sa.permits_action(&Action::Write));
        assert!(sa.permits_action(&Action::Admin));
        assert!(sa.permits_action(&Action::ManageUsers));
        assert!(sa.permits_action(&Action::ManageDatabases));
        assert!(sa.permits_action(&Action::ManageServer));
    }

    #[test]
    fn test_tenant_admin_cannot_manage_server() {
        let ta = RoleName::TenantAdmin;
        assert!(ta.permits_action(&Action::Read));
        assert!(ta.permits_action(&Action::Write));
        assert!(ta.permits_action(&Action::Admin));
        assert!(ta.permits_action(&Action::ManageUsers));
        assert!(ta.permits_action(&Action::ManageDatabases));
        assert!(!ta.permits_action(&Action::ManageServer));
    }

    #[test]
    fn test_db_admin_permissions() {
        let da = RoleName::DbAdmin;
        assert!(da.permits_action(&Action::Read));
        assert!(da.permits_action(&Action::Write));
        assert!(da.permits_action(&Action::Admin));
        assert!(!da.permits_action(&Action::ManageUsers));
        assert!(!da.permits_action(&Action::ManageDatabases));
    }

    #[test]
    fn test_read_write_permissions() {
        let rw = RoleName::ReadWrite;
        assert!(rw.permits_action(&Action::Read));
        assert!(rw.permits_action(&Action::Write));
        assert!(!rw.permits_action(&Action::Admin));
    }

    #[test]
    fn test_read_only_permissions() {
        let ro = RoleName::ReadOnly;
        assert!(ro.permits_action(&Action::Read));
        assert!(!ro.permits_action(&Action::Write));
        assert!(!ro.permits_action(&Action::Admin));
    }

    #[test]
    fn test_scope_server_covers_all() {
        let server = ResourceScope::Server;
        assert!(server.covers(&ResourceScope::Server));
        assert!(server.covers(&ResourceScope::Tenant));
        assert!(server.covers(&ResourceScope::Database { name: "x".into() }));
        assert!(server.covers(&ResourceScope::Collection {
            database: "x".into(),
            collection: "y".into()
        }));
    }

    #[test]
    fn test_scope_tenant_covers_databases() {
        let tenant = ResourceScope::Tenant;
        assert!(!tenant.covers(&ResourceScope::Server));
        assert!(tenant.covers(&ResourceScope::Tenant));
        assert!(tenant.covers(&ResourceScope::Database { name: "x".into() }));
        assert!(tenant.covers(&ResourceScope::Collection {
            database: "x".into(),
            collection: "y".into()
        }));
    }

    #[test]
    fn test_scope_database_covers_its_collections() {
        let db = ResourceScope::Database {
            name: "mydb".into(),
        };
        assert!(db.covers(&ResourceScope::Database {
            name: "mydb".into()
        }));
        assert!(db.covers(&ResourceScope::Collection {
            database: "mydb".into(),
            collection: "users".into()
        }));
        // Different database → not covered
        assert!(!db.covers(&ResourceScope::Database {
            name: "other".into()
        }));
        assert!(!db.covers(&ResourceScope::Collection {
            database: "other".into(),
            collection: "users".into()
        }));
    }

    #[test]
    fn test_scope_collection_covers_only_itself() {
        let coll = ResourceScope::Collection {
            database: "mydb".into(),
            collection: "users".into(),
        };
        assert!(coll.covers(&ResourceScope::Collection {
            database: "mydb".into(),
            collection: "users".into()
        }));
        assert!(!coll.covers(&ResourceScope::Collection {
            database: "mydb".into(),
            collection: "tasks".into()
        }));
        assert!(!coll.covers(&ResourceScope::Database {
            name: "mydb".into()
        }));
    }

    #[test]
    fn test_role_assignment_permits() {
        let ra = RoleAssignment {
            role: RoleName::ReadWrite,
            scope: ResourceScope::Database {
                name: "mydb".into(),
            },
        };

        // Can read in mydb.users
        assert!(ra.permits(
            &Action::Read,
            &ResourceScope::Collection {
                database: "mydb".into(),
                collection: "users".into()
            }
        ));

        // Can write in mydb.users
        assert!(ra.permits(
            &Action::Write,
            &ResourceScope::Collection {
                database: "mydb".into(),
                collection: "users".into()
            }
        ));

        // Cannot admin in mydb
        assert!(!ra.permits(
            &Action::Admin,
            &ResourceScope::Database {
                name: "mydb".into()
            }
        ));

        // Cannot read in other_db
        assert!(!ra.permits(
            &Action::Read,
            &ResourceScope::Collection {
                database: "other".into(),
                collection: "users".into()
            }
        ));
    }

    #[test]
    fn test_role_assignment_serde_round_trip() {
        let ra = RoleAssignment {
            role: RoleName::DbAdmin,
            scope: ResourceScope::Database {
                name: "staging".into(),
            },
        };
        let json = serde_json::to_string(&ra).unwrap();
        let parsed: RoleAssignment = serde_json::from_str(&json).unwrap();
        assert_eq!(ra, parsed);
    }
}
