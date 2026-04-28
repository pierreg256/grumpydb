//! Persistent auth store: user CRUD, server secret, disk persistence.

use std::path::{Path, PathBuf};

use crate::auth::jwt::{self, Claims, JwtConfig};
use crate::auth::role::{ResourceScope, RoleAssignment, RoleName};
use crate::auth::user::{self, AuthError, User};

/// Manages users and the JWT secret on disk.
///
/// ## On-disk layout
///
/// ```text
/// _auth/
///   secret.key                      ← 32 raw bytes (HMAC secret)
///   users/
///     acme__alice.json              ← user records
/// ```
pub struct AuthStore {
    /// Path to the `_auth/` directory.
    auth_dir: PathBuf,
    /// JWT configuration (holds the secret).
    jwt_config: JwtConfig,
    /// In-memory user cache.
    users: Vec<User>,
}

impl AuthStore {
    /// Open or create the auth store at `<server_root>/_auth/`.
    ///
    /// Bootstrap behaviour:
    /// - If no users exist on disk and `bootstrap_password` is `None`, the call
    ///   returns [`AuthError::BootstrapRefused`] — the server MUST NOT silently
    ///   create an `admin/admin` account.
    /// - If `bootstrap_password` is provided and is short (< 8 chars), it is
    ///   accepted but a warning is logged.
    /// - Once at least one user exists, subsequent opens never re-bootstrap.
    pub fn open(
        auth_dir: &Path,
        access_ttl_secs: u64,
        refresh_ttl_secs: u64,
        bootstrap_password: Option<&str>,
    ) -> Result<Self, AuthError> {
        std::fs::create_dir_all(auth_dir)?;
        let users_dir = auth_dir.join("users");
        std::fs::create_dir_all(&users_dir)?;

        // Load or generate secret key
        let secret_path = auth_dir.join("secret.key");
        let secret = if secret_path.exists() {
            // Verify on-disk permissions are not world/group readable on Unix.
            check_secret_permissions(&secret_path)?;
            let data = std::fs::read(&secret_path)?;
            if data.len() != 32 {
                return Err(AuthError::Io(format!(
                    "secret.key has invalid length: {} (expected 32)",
                    data.len()
                )));
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&data);
            arr
        } else {
            use rand::Rng;
            let mut secret = [0u8; 32];
            rand::thread_rng().fill(&mut secret);
            std::fs::write(&secret_path, secret)?;
            // Tighten permissions to 0600 on Unix immediately after write.
            set_owner_only_permissions(&secret_path)?;
            secret
        };

        let jwt_config = JwtConfig::new(
            secret,
            std::time::Duration::from_secs(access_ttl_secs),
            std::time::Duration::from_secs(refresh_ttl_secs),
        );

        // Load existing users
        let mut users = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&users_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    let data = std::fs::read_to_string(&path)?;
                    match serde_json::from_str::<User>(&data) {
                        Ok(u) => users.push(u),
                        Err(e) => {
                            tracing::warn!(
                                file = %path.display(),
                                error = %e,
                                "failed to parse user file — skipped"
                            );
                        }
                    }
                }
            }
        }

        let mut store = Self {
            auth_dir: auth_dir.to_path_buf(),
            jwt_config,
            users,
        };

        // Bootstrap: create initial _system/admin only if explicitly allowed.
        if store.users.is_empty() {
            let pwd = bootstrap_password.ok_or_else(|| {
                AuthError::BootstrapRefused(
                    "no users on disk; pass --bootstrap-password <pw> (or set \
                     GRUMPYDB_BOOTSTRAP_PASSWORD) on first start to create \
                     the initial _system/admin account"
                        .to_string(),
                )
            })?;
            if pwd.len() < 8 {
                tracing::warn!(
                    "bootstrap password is shorter than 8 characters — strongly \
                     recommend at least 12 characters for the initial admin"
                );
            }
            store.create_user(
                "_system",
                "admin",
                pwd,
                vec![RoleAssignment {
                    role: RoleName::ServerAdmin,
                    scope: ResourceScope::Server,
                }],
            )?;
            tracing::info!(
                "bootstrapped initial _system/admin user — change the password \
                 immediately via PASSWD command"
            );
        }

        Ok(store)
    }

    /// Create a new user.
    pub fn create_user(
        &mut self,
        tenant: &str,
        username: &str,
        password: &str,
        roles: Vec<RoleAssignment>,
    ) -> Result<(), AuthError> {
        // Check for duplicates
        if self
            .users
            .iter()
            .any(|u| u.tenant == tenant && u.username == username)
        {
            return Err(AuthError::UserAlreadyExists(format!("{tenant}/{username}")));
        }

        let password_hash = user::hash_password(password)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| AuthError::ClockError(e.to_string()))?
            .as_secs();

        let new_user = User {
            username: username.to_string(),
            tenant: tenant.to_string(),
            password_hash,
            roles,
            created_at: now,
        };

        // Persist to disk
        self.save_user(&new_user)?;
        self.users.push(new_user);
        Ok(())
    }

    /// Delete a user.
    pub fn delete_user(&mut self, tenant: &str, username: &str) -> Result<(), AuthError> {
        let idx = self
            .users
            .iter()
            .position(|u| u.tenant == tenant && u.username == username)
            .ok_or_else(|| AuthError::UserNotFound(format!("{tenant}/{username}")))?;

        self.users.remove(idx);

        let file_path = self.user_file_path(tenant, username);
        if file_path.exists() {
            std::fs::remove_file(&file_path)?;
        }
        Ok(())
    }

    /// Get a user by tenant and username.
    pub fn get_user(&self, tenant: &str, username: &str) -> Option<&User> {
        self.users
            .iter()
            .find(|u| u.tenant == tenant && u.username == username)
    }

    /// List all users in a tenant.
    pub fn list_users(&self, tenant: &str) -> Vec<&User> {
        self.users.iter().filter(|u| u.tenant == tenant).collect()
    }

    /// Update roles for a user.
    pub fn update_roles(
        &mut self,
        tenant: &str,
        username: &str,
        roles: Vec<RoleAssignment>,
    ) -> Result<(), AuthError> {
        let u = self
            .users
            .iter_mut()
            .find(|u| u.tenant == tenant && u.username == username)
            .ok_or_else(|| AuthError::UserNotFound(format!("{tenant}/{username}")))?;

        u.roles = roles;
        let snapshot = u.clone();
        self.save_user(&snapshot)?;
        Ok(())
    }

    /// Authenticate a user and return (access_token, refresh_token).
    ///
    /// Returns a generic "invalid credentials" error for both wrong password
    /// and nonexistent user (prevents user enumeration).
    pub fn authenticate(
        &self,
        tenant: &str,
        username: &str,
        password: &str,
    ) -> Result<(String, String), AuthError> {
        let u = self
            .get_user(tenant, username)
            .ok_or(AuthError::InvalidCredentials)?;

        let valid = user::verify_password(password, &u.password_hash)?;
        if !valid {
            return Err(AuthError::InvalidCredentials);
        }

        let access = jwt::generate_access_token(u, &self.jwt_config)?;
        let refresh = jwt::generate_refresh_token(u, &self.jwt_config)?;
        Ok((access, refresh))
    }

    /// Verify a token and return the decoded claims.
    pub fn verify_token(&self, token: &str) -> Result<Claims, AuthError> {
        jwt::verify_token(token, &self.jwt_config)
    }

    /// Refresh an access token using a valid refresh token.
    pub fn refresh_access_token(&self, refresh_token: &str) -> Result<String, AuthError> {
        let claims = self.verify_token(refresh_token)?;
        if claims.token_type != "refresh" {
            return Err(AuthError::InvalidToken(
                "expected refresh token".to_string(),
            ));
        }

        // Look up current user (roles may have changed)
        let u = self
            .get_user(&claims.tenant, &claims.sub)
            .ok_or(AuthError::InvalidCredentials)?;

        jwt::generate_access_token(u, &self.jwt_config)
    }

    // ── Private helpers ─────────────────────────────────────────────

    fn user_file_path(&self, tenant: &str, username: &str) -> PathBuf {
        self.auth_dir
            .join("users")
            .join(format!("{tenant}__{username}.json"))
    }

    fn save_user(&self, user: &User) -> Result<(), AuthError> {
        let path = self.user_file_path(&user.tenant, &user.username);
        let json = serde_json::to_string_pretty(user).map_err(|e| AuthError::Io(e.to_string()))?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

// ── File permission helpers ──────────────────────────────────────────────────

/// Tighten file permissions to owner-only (mode 0600) on Unix. No-op elsewhere.
fn set_owner_only_permissions(path: &Path) -> Result<(), AuthError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    let _ = path; // suppress unused warning on non-unix
    Ok(())
}

/// On Unix, refuse to start if `secret.key` is readable by group or world.
fn check_secret_permissions(path: &Path) -> Result<(), AuthError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                mode = format!("{:o}", mode),
                "secret.key has group/world permissions; tightening to 0600"
            );
            set_owner_only_permissions(path)?;
        }
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, AuthStore) {
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("_auth");
        let store = AuthStore::open(&auth_dir, 3600, 604800, Some("bootstrap-test-pw")).unwrap();
        (dir, store)
    }

    #[test]
    fn test_store_bootstrap_creates_admin() {
        let (_dir, store) = setup();
        let admin = store.get_user("_system", "admin");
        assert!(admin.is_some());
        let admin = admin.unwrap();
        assert_eq!(admin.roles.len(), 1);
        assert_eq!(admin.roles[0].role, RoleName::ServerAdmin);
    }

    #[test]
    fn test_store_refuses_silent_bootstrap() {
        // No bootstrap_password → must refuse instead of creating admin/admin.
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("_auth");
        let result = AuthStore::open(&auth_dir, 3600, 604800, None);
        assert!(matches!(result, Err(AuthError::BootstrapRefused(_))));
    }

    #[test]
    fn test_store_no_rebootstrap_after_users_exist() {
        // Once at least one user is on disk, opening without a bootstrap
        // password is fine.
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("_auth");
        let _ = AuthStore::open(&auth_dir, 3600, 604800, Some("first-pw")).unwrap();

        // Reopen without any password — must not error.
        let store = AuthStore::open(&auth_dir, 3600, 604800, None).unwrap();
        assert!(store.get_user("_system", "admin").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_secret_key_has_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, store) = setup();
        let secret_path = store.auth_dir.join("secret.key");
        let mode = std::fs::metadata(&secret_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        // Group / world bits must be clear.
        assert_eq!(
            mode & 0o077,
            0,
            "secret.key must not be group/world readable, got mode {mode:o}"
        );
    }

    #[test]
    fn test_store_create_and_get_user() {
        let (_dir, mut store) = setup();
        store
            .create_user(
                "acme",
                "alice",
                "s3cr3t",
                vec![RoleAssignment {
                    role: RoleName::ReadWrite,
                    scope: ResourceScope::Database {
                        name: "mydb".into(),
                    },
                }],
            )
            .unwrap();

        let u = store.get_user("acme", "alice").unwrap();
        assert_eq!(u.username, "alice");
        assert_eq!(u.tenant, "acme");
        assert_eq!(u.roles.len(), 1);
    }

    #[test]
    fn test_store_duplicate_user_rejected() {
        let (_dir, mut store) = setup();
        store.create_user("acme", "alice", "pass", vec![]).unwrap();
        let result = store.create_user("acme", "alice", "pass2", vec![]);
        assert!(matches!(result, Err(AuthError::UserAlreadyExists(_))));
    }

    #[test]
    fn test_store_delete_user() {
        let (_dir, mut store) = setup();
        store.create_user("acme", "bob", "pass", vec![]).unwrap();
        assert!(store.get_user("acme", "bob").is_some());

        store.delete_user("acme", "bob").unwrap();
        assert!(store.get_user("acme", "bob").is_none());
    }

    #[test]
    fn test_store_delete_nonexistent_user() {
        let (_dir, mut store) = setup();
        assert!(matches!(
            store.delete_user("acme", "nobody"),
            Err(AuthError::UserNotFound(_))
        ));
    }

    #[test]
    fn test_store_list_users_by_tenant() {
        let (_dir, mut store) = setup();
        store.create_user("acme", "alice", "pass", vec![]).unwrap();
        store.create_user("acme", "bob", "pass", vec![]).unwrap();
        store
            .create_user("globex", "charlie", "pass", vec![])
            .unwrap();

        let acme_users = store.list_users("acme");
        assert_eq!(acme_users.len(), 2);

        let globex_users = store.list_users("globex");
        assert_eq!(globex_users.len(), 1);
    }

    #[test]
    fn test_store_update_roles() {
        let (_dir, mut store) = setup();
        store.create_user("acme", "alice", "pass", vec![]).unwrap();
        assert!(store.get_user("acme", "alice").unwrap().roles.is_empty());

        store
            .update_roles(
                "acme",
                "alice",
                vec![RoleAssignment {
                    role: RoleName::DbAdmin,
                    scope: ResourceScope::Database {
                        name: "prod".into(),
                    },
                }],
            )
            .unwrap();

        let u = store.get_user("acme", "alice").unwrap();
        assert_eq!(u.roles.len(), 1);
        assert_eq!(u.roles[0].role, RoleName::DbAdmin);
    }

    #[test]
    fn test_store_authenticate_success() {
        let (_dir, mut store) = setup();
        store
            .create_user("acme", "alice", "s3cr3t", vec![])
            .unwrap();

        let (access, refresh) = store.authenticate("acme", "alice", "s3cr3t").unwrap();
        assert!(!access.is_empty());
        assert!(!refresh.is_empty());

        // Verify tokens
        let claims = store.verify_token(&access).unwrap();
        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.tenant, "acme");
    }

    #[test]
    fn test_store_authenticate_wrong_password() {
        let (_dir, mut store) = setup();
        store
            .create_user("acme", "alice", "s3cr3t", vec![])
            .unwrap();

        let result = store.authenticate("acme", "alice", "wrong");
        assert!(matches!(result, Err(AuthError::InvalidCredentials)));
    }

    #[test]
    fn test_store_authenticate_unknown_user() {
        let (_dir, store) = setup();
        let result = store.authenticate("acme", "nobody", "pass");
        // Same error as wrong password (prevent enumeration)
        assert!(matches!(result, Err(AuthError::InvalidCredentials)));
    }

    #[test]
    fn test_store_refresh_access_token() {
        let (_dir, mut store) = setup();
        store
            .create_user("acme", "alice", "s3cr3t", vec![])
            .unwrap();

        let (_, refresh) = store.authenticate("acme", "alice", "s3cr3t").unwrap();
        let new_access = store.refresh_access_token(&refresh).unwrap();
        let claims = store.verify_token(&new_access).unwrap();
        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.token_type, "access");
    }

    #[test]
    fn test_store_refresh_with_access_token_fails() {
        let (_dir, mut store) = setup();
        store
            .create_user("acme", "alice", "s3cr3t", vec![])
            .unwrap();

        let (access, _) = store.authenticate("acme", "alice", "s3cr3t").unwrap();
        let result = store.refresh_access_token(&access);
        assert!(matches!(result, Err(AuthError::InvalidToken(_))));
    }

    #[test]
    fn test_store_persistence_across_reopen() {
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("_auth");

        {
            let mut store =
                AuthStore::open(&auth_dir, 3600, 604800, Some("bootstrap-test-pw")).unwrap();
            store
                .create_user(
                    "acme",
                    "alice",
                    "s3cr3t",
                    vec![RoleAssignment {
                        role: RoleName::ReadWrite,
                        scope: ResourceScope::Database {
                            name: "mydb".into(),
                        },
                    }],
                )
                .unwrap();
        }

        // Reopen
        let store = AuthStore::open(&auth_dir, 3600, 604800, Some("bootstrap-test-pw")).unwrap();
        let u = store.get_user("acme", "alice").unwrap();
        assert_eq!(u.username, "alice");
        assert_eq!(u.roles.len(), 1);

        // Can authenticate with same password
        let (access, _) = store.authenticate("acme", "alice", "s3cr3t").unwrap();
        let claims = store.verify_token(&access).unwrap();
        assert_eq!(claims.sub, "alice");
    }

    #[test]
    fn test_store_secret_persists() {
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("_auth");

        let token;
        {
            let mut store =
                AuthStore::open(&auth_dir, 3600, 604800, Some("bootstrap-test-pw")).unwrap();
            store.create_user("acme", "alice", "pass", vec![]).unwrap();
            let (t, _) = store.authenticate("acme", "alice", "pass").unwrap();
            token = t;
        }

        // Reopen — same secret → same token verifies
        let store = AuthStore::open(&auth_dir, 3600, 604800, Some("bootstrap-test-pw")).unwrap();
        let claims = store.verify_token(&token).unwrap();
        assert_eq!(claims.sub, "alice");
    }
}
