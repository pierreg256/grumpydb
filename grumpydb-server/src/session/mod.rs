//! Per-connection session context and RBAC enforcer.
//!
//! Each TCP connection has a [`SessionContext`] that holds the decoded JWT
//! claims and the currently selected database. The `authorize()` method
//! checks permissions before every command execution.

use grumpydb_protocol::Command;
use grumpydb_protocol::command::{Action, Resource};

use crate::auth::jwt::Claims;
use crate::auth::role::ResourceScope;
use crate::auth::user::AuthError;

/// Per-connection session state.
pub struct SessionContext {
    /// Decoded JWT claims (None before LOGIN).
    claims: Option<Claims>,
    /// Currently selected database (None before USE).
    current_db: Option<String>,
}

impl SessionContext {
    /// Create a new unauthenticated session.
    pub fn new() -> Self {
        Self {
            claims: None,
            current_db: None,
        }
    }

    /// Set the JWT claims after successful LOGIN or TOKEN.
    pub fn set_claims(&mut self, claims: Claims) {
        self.claims = Some(claims);
    }

    /// Set the current database after USE.
    pub fn set_database(&mut self, name: String) {
        self.current_db = Some(name);
    }

    /// Returns `true` if the session is authenticated.
    pub fn is_authenticated(&self) -> bool {
        self.claims.is_some()
    }

    /// Get the tenant name from the JWT claims.
    pub fn tenant(&self) -> Result<&str, AuthError> {
        self.claims
            .as_ref()
            .map(|c| c.tenant.as_str())
            .ok_or(AuthError::NotAuthenticated)
    }

    /// Get the username from the JWT claims.
    pub fn username(&self) -> Result<&str, AuthError> {
        self.claims
            .as_ref()
            .map(|c| c.sub.as_str())
            .ok_or(AuthError::NotAuthenticated)
    }

    /// Get the current database name.
    pub fn current_db(&self) -> Option<&str> {
        self.current_db.as_deref()
    }

    /// Get a reference to the claims.
    pub fn claims(&self) -> Option<&Claims> {
        self.claims.as_ref()
    }

    /// Check if the current session has permission to execute a command.
    ///
    /// Pre-auth commands (LOGIN, PING, QUIT) are always allowed.
    /// All other commands require authentication and RBAC authorization.
    pub fn authorize(&self, command: &Command) -> Result<(), AuthError> {
        // Pre-auth commands are always allowed
        if command.is_pre_auth() {
            return Ok(());
        }

        // Must be authenticated
        let claims = self.claims.as_ref().ok_or(AuthError::NotAuthenticated)?;

        let action = command.required_action();

        // Session commands (USE, WHOAMI, TOKEN, REFRESH) just need authentication
        if action == Action::Session {
            return Ok(());
        }

        // Map protocol Action to auth Action
        let auth_action = match action {
            Action::Read => crate::auth::role::Action::Read,
            Action::Write => crate::auth::role::Action::Write,
            Action::Admin => crate::auth::role::Action::Admin,
            Action::ManageUsers => crate::auth::role::Action::ManageUsers,
            Action::ManageDatabases => crate::auth::role::Action::ManageDatabases,
            Action::ManageServer => crate::auth::role::Action::ManageServer,
            Action::ManageCluster => crate::auth::role::Action::ManageServer,
            Action::Session => return Ok(()), // already handled above
        };

        // Map protocol Resource to auth ResourceScope
        let resource = command.target_resource(self.current_db.as_deref());
        let auth_resource = match resource {
            Resource::Server => ResourceScope::Server,
            Resource::Database(name) => ResourceScope::Database { name },
            Resource::Collection(db, coll) => ResourceScope::Collection {
                database: db,
                collection: coll,
            },
            Resource::None => return Ok(()),
        };

        // Check if any role assignment permits this action
        let permitted = claims
            .roles
            .iter()
            .any(|ra| ra.permits(&auth_action, &auth_resource));

        if permitted {
            Ok(())
        } else {
            Err(AuthError::AccessDenied(format!(
                "{:?} on {:?} denied for user '{}'",
                auth_action, auth_resource, claims.sub
            )))
        }
    }
}

impl Default for SessionContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::role::{ResourceScope, RoleAssignment, RoleName};

    fn make_claims(username: &str, tenant: &str, roles: Vec<RoleAssignment>) -> Claims {
        Claims {
            sub: username.into(),
            tenant: tenant.into(),
            roles,
            iat: 0,
            exp: u64::MAX,
            token_type: "access".into(),
        }
    }

    #[test]
    fn test_session_new_is_unauthenticated() {
        let session = SessionContext::new();
        assert!(!session.is_authenticated());
        assert!(session.tenant().is_err());
        assert!(session.current_db().is_none());
    }

    #[test]
    fn test_session_set_claims() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims("alice", "acme", vec![]));
        assert!(session.is_authenticated());
        assert_eq!(session.tenant().unwrap(), "acme");
        assert_eq!(session.username().unwrap(), "alice");
    }

    #[test]
    fn test_session_set_database() {
        let mut session = SessionContext::new();
        assert!(session.current_db().is_none());
        session.set_database("mydb".into());
        assert_eq!(session.current_db(), Some("mydb"));
    }

    #[test]
    fn test_authorize_pre_auth_always_allowed() {
        let session = SessionContext::new();
        // Not authenticated, but pre-auth commands work
        assert!(session.authorize(&Command::Ping).is_ok());
        assert!(session.authorize(&Command::Quit).is_ok());
        assert!(
            session
                .authorize(&Command::Login {
                    tenant: "t".into(),
                    username: "u".into(),
                    password: "p".into(),
                })
                .is_ok()
        );
    }

    #[test]
    fn test_authorize_requires_authentication() {
        let session = SessionContext::new();
        let cmd = Command::ListDatabases;
        assert!(matches!(
            session.authorize(&cmd),
            Err(AuthError::NotAuthenticated)
        ));
    }

    #[test]
    fn test_authorize_read_only_can_read() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims(
            "alice",
            "acme",
            vec![RoleAssignment {
                role: RoleName::ReadOnly,
                scope: ResourceScope::Database {
                    name: "mydb".into(),
                },
            }],
        ));
        session.set_database("mydb".into());

        let get = Command::Get {
            collection: "users".into(),
            key: "abc".into(),
        };
        assert!(session.authorize(&get).is_ok());
    }

    #[test]
    fn test_authorize_read_only_cannot_write() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims(
            "alice",
            "acme",
            vec![RoleAssignment {
                role: RoleName::ReadOnly,
                scope: ResourceScope::Database {
                    name: "mydb".into(),
                },
            }],
        ));
        session.set_database("mydb".into());

        let insert = Command::Insert {
            collection: "users".into(),
            key: "abc".into(),
            value: "{}".into(),
        };
        assert!(matches!(
            session.authorize(&insert),
            Err(AuthError::AccessDenied(_))
        ));
    }

    #[test]
    fn test_authorize_read_write_can_crud() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims(
            "alice",
            "acme",
            vec![RoleAssignment {
                role: RoleName::ReadWrite,
                scope: ResourceScope::Database {
                    name: "mydb".into(),
                },
            }],
        ));
        session.set_database("mydb".into());

        assert!(
            session
                .authorize(&Command::Get {
                    collection: "u".into(),
                    key: "k".into()
                })
                .is_ok()
        );
        assert!(
            session
                .authorize(&Command::Insert {
                    collection: "u".into(),
                    key: "k".into(),
                    value: "v".into()
                })
                .is_ok()
        );
        assert!(
            session
                .authorize(&Command::Delete {
                    collection: "u".into(),
                    key: "k".into()
                })
                .is_ok()
        );
    }

    #[test]
    fn test_authorize_read_write_cannot_admin() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims(
            "alice",
            "acme",
            vec![RoleAssignment {
                role: RoleName::ReadWrite,
                scope: ResourceScope::Database {
                    name: "mydb".into(),
                },
            }],
        ));
        session.set_database("mydb".into());

        assert!(matches!(
            session.authorize(&Command::CreateCollection("x".into())),
            Err(AuthError::AccessDenied(_))
        ));
    }

    #[test]
    fn test_authorize_db_admin_can_manage_collections() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims(
            "alice",
            "acme",
            vec![RoleAssignment {
                role: RoleName::DbAdmin,
                scope: ResourceScope::Database {
                    name: "mydb".into(),
                },
            }],
        ));
        session.set_database("mydb".into());

        assert!(
            session
                .authorize(&Command::CreateCollection("users".into()))
                .is_ok()
        );
        assert!(
            session
                .authorize(&Command::DropCollection("users".into()))
                .is_ok()
        );
        assert!(
            session
                .authorize(&Command::CreateIndex {
                    collection: "u".into(),
                    index_name: "idx".into(),
                    field_path: "f".into()
                })
                .is_ok()
        );
    }

    #[test]
    fn test_authorize_db_admin_cannot_manage_databases() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims(
            "alice",
            "acme",
            vec![RoleAssignment {
                role: RoleName::DbAdmin,
                scope: ResourceScope::Database {
                    name: "mydb".into(),
                },
            }],
        ));

        assert!(matches!(
            session.authorize(&Command::CreateDatabase("newdb".into())),
            Err(AuthError::AccessDenied(_))
        ));
    }

    #[test]
    fn test_authorize_tenant_admin_can_manage_databases() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims(
            "alice",
            "acme",
            vec![RoleAssignment {
                role: RoleName::TenantAdmin,
                scope: ResourceScope::Tenant,
            }],
        ));

        assert!(
            session
                .authorize(&Command::CreateDatabase("newdb".into()))
                .is_ok()
        );
        assert!(
            session
                .authorize(&Command::DropDatabase("old".into()))
                .is_ok()
        );
        assert!(session.authorize(&Command::ListDatabases).is_ok());
    }

    #[test]
    fn test_authorize_tenant_admin_cannot_manage_server() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims(
            "alice",
            "acme",
            vec![RoleAssignment {
                role: RoleName::TenantAdmin,
                scope: ResourceScope::Tenant,
            }],
        ));

        assert!(matches!(
            session.authorize(&Command::CreateTenant("x".into())),
            Err(AuthError::AccessDenied(_))
        ));
    }

    #[test]
    fn test_authorize_server_admin_can_do_everything() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims(
            "admin",
            "_system",
            vec![RoleAssignment {
                role: RoleName::ServerAdmin,
                scope: ResourceScope::Server,
            }],
        ));
        session.set_database("anydb".into());

        assert!(
            session
                .authorize(&Command::CreateTenant("x".into()))
                .is_ok()
        );
        assert!(session.authorize(&Command::DropTenant("x".into())).is_ok());
        assert!(session.authorize(&Command::ListTenants).is_ok());
        assert!(
            session
                .authorize(&Command::CreateDatabase("x".into()))
                .is_ok()
        );
        assert!(
            session
                .authorize(&Command::Insert {
                    collection: "c".into(),
                    key: "k".into(),
                    value: "v".into()
                })
                .is_ok()
        );
    }

    #[test]
    fn test_authorize_scope_isolation() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims(
            "alice",
            "acme",
            vec![RoleAssignment {
                role: RoleName::ReadWrite,
                scope: ResourceScope::Database {
                    name: "allowed_db".into(),
                },
            }],
        ));

        // Allowed database
        session.set_database("allowed_db".into());
        assert!(
            session
                .authorize(&Command::Get {
                    collection: "u".into(),
                    key: "k".into()
                })
                .is_ok()
        );

        // Different database → denied
        session.set_database("other_db".into());
        assert!(matches!(
            session.authorize(&Command::Get {
                collection: "u".into(),
                key: "k".into()
            }),
            Err(AuthError::AccessDenied(_))
        ));
    }

    #[test]
    fn test_authorize_session_commands_need_auth_only() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims("alice", "acme", vec![]));
        // No roles at all, but session commands should work
        assert!(session.authorize(&Command::Use("mydb".into())).is_ok());
        assert!(session.authorize(&Command::WhoAmI).is_ok());
    }

    #[test]
    fn test_authorize_multiple_roles() {
        let mut session = SessionContext::new();
        session.set_claims(make_claims(
            "alice",
            "acme",
            vec![
                RoleAssignment {
                    role: RoleName::ReadOnly,
                    scope: ResourceScope::Database { name: "db1".into() },
                },
                RoleAssignment {
                    role: RoleName::ReadWrite,
                    scope: ResourceScope::Database { name: "db2".into() },
                },
            ],
        ));

        // Can read from db1
        session.set_database("db1".into());
        assert!(
            session
                .authorize(&Command::Get {
                    collection: "u".into(),
                    key: "k".into()
                })
                .is_ok()
        );

        // Cannot write to db1
        assert!(
            session
                .authorize(&Command::Insert {
                    collection: "u".into(),
                    key: "k".into(),
                    value: "v".into()
                })
                .is_err()
        );

        // Can write to db2
        session.set_database("db2".into());
        assert!(
            session
                .authorize(&Command::Insert {
                    collection: "u".into(),
                    key: "k".into(),
                    value: "v".into()
                })
                .is_ok()
        );
    }
}
