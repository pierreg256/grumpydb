//! Persistent auth store: user CRUD, JWT key material, disk persistence.
//!
//! Phase 39 introduced RS256 (asymmetric) JWTs alongside the legacy HS256
//! (symmetric) algorithm. The on-disk layout is now:
//!
//! ```text
//! _auth/
//!   secret.key                      (legacy HS256, 32 raw bytes)
//!   users/                          (one JSON file per user)
//!     <tenant>__<username>.json
//!   jwt/                            (Phase 39 — RS256 only)
//!     config.json                   (algorithm + kid pointers + TTLs)
//!     keys/
//!       <kid_current>.pem           (PKCS#8 PEM, chmod 600)
//!       <kid_current>.pub.pem       (SPKI PEM,    chmod 644)
//!       <kid_next>.pem
//!       <kid_next>.pub.pem
//!       <archived_kid>.pem.archived (kept for grace period after rotation)
//!     cluster_peer.token            (1y TTL JWT for inter-node auth)
//! ```
//!
//! Open behaviour:
//! - **Fresh install** (`_auth/jwt/config.json` missing AND `secret.key`
//!   missing): generate a brand new RS256 keyring, persist both keys, write
//!   `config.json`, bootstrap the admin and `cluster_peer` users.
//! - **Existing v4 install** (`secret.key` present, no `jwt/` dir): keep
//!   HS256. No silent migration. Use `auth migrate --to rs256` to upgrade.
//! - **Existing v5 install** (`jwt/config.json` present): load whichever
//!   algorithm it specifies.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::auth::jwt::{self, Claims, Jwk, JwtAlgorithm, JwtConfig, RsKey, RsKeyRing};
use crate::auth::role::{ResourceScope, RoleAssignment, RoleName};
use crate::auth::user::{self, AuthError, User};

/// Default grace period during which the previously-current key is kept
/// on disk (as `<kid>.pem.archived`) so already-issued tokens still
/// verify until they expire. Defaults to 7 days.
const DEFAULT_ARCHIVE_GRACE_SECS: u64 = 7 * 24 * 3600;

/// Default cluster_peer token TTL (1 year). Long-lived because rotating
/// it requires a coordinated multi-node restart; short-lived RS256 keys
/// are the recommended rotation path instead.
const CLUSTER_PEER_TTL_SECS: u64 = 365 * 24 * 3600;

/// Bootstrap username for the inter-node `cluster_peer` user. Phase 40a
/// will replace `local-bootstrap` with the real `node_id`.
const CLUSTER_PEER_USERNAME: &str = "_cluster/local-bootstrap";

/// Reserved tenant for internal users (admins, cluster peers).
const SYSTEM_TENANT: &str = "_system";

/// Serialized form of `_auth/jwt/config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JwtDiskConfig {
    algorithm: JwtAlgorithm,
    /// Kid of the active signing key (RS256 only).
    #[serde(skip_serializing_if = "Option::is_none")]
    current_kid: Option<String>,
    /// Kid of the standby key (RS256 only).
    #[serde(skip_serializing_if = "Option::is_none")]
    next_kid: Option<String>,
    /// Access-token TTL (seconds).
    access_ttl_secs: u64,
    /// Refresh-token TTL (seconds).
    refresh_ttl_secs: u64,
}

/// Outcome of a JWT key rotation.
#[derive(Debug, Clone)]
pub struct RotationOutcome {
    /// Kid of the key that was previously `current` (now archived).
    pub previous_current_kid: String,
    /// Kid that became the new `current` (was previously `next`).
    pub promoted_kid: String,
    /// Kid of the freshly generated `next` key.
    pub new_next_kid: String,
}

/// Manages users and JWT key material on disk.
pub struct AuthStore {
    auth_dir: PathBuf,
    jwt_config: JwtConfig,
    users: Vec<User>,
}

impl AuthStore {
    /// Open or create the auth store at `<server_root>/_auth/`.
    ///
    /// See the module docs for fresh-install / migration semantics.
    /// Open the auth store at `auth_dir`. If no JWT config exists on
    /// disk, bootstrap with `bootstrap_algorithm` (typically RS256 for
    /// production, HS256 for tests that spawn many short-lived servers).
    pub fn open(
        auth_dir: &Path,
        access_ttl_secs: u64,
        refresh_ttl_secs: u64,
        bootstrap_password: Option<&str>,
        bootstrap_algorithm: JwtAlgorithm,
    ) -> Result<Self, AuthError> {
        std::fs::create_dir_all(auth_dir)?;
        let users_dir = auth_dir.join("users");
        std::fs::create_dir_all(&users_dir)?;

        let jwt_config = load_or_create_jwt_config(
            auth_dir,
            access_ttl_secs,
            refresh_ttl_secs,
            bootstrap_algorithm,
        )?;

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
        if !store.has_admin() {
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
                SYSTEM_TENANT,
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

        // Bootstrap the cluster_peer user + long-lived token. Skipped if
        // the user already exists (idempotent).
        store.bootstrap_cluster_peer()?;

        Ok(store)
    }

    /// Returns whether at least one server_admin exists on disk.
    fn has_admin(&self) -> bool {
        self.users
            .iter()
            .any(|u| u.roles.iter().any(|r| r.role == RoleName::ServerAdmin))
    }

    /// Create the `_cluster/local-bootstrap` user (idempotent) and write
    /// a long-lived `cluster_peer.token` to disk.
    ///
    /// The token is regenerated every time the server boots so a
    /// just-rotated key is reflected immediately and rotated-out tokens
    /// don't linger on disk.
    fn bootstrap_cluster_peer(&mut self) -> Result<(), AuthError> {
        let cluster_peer_role = vec![RoleAssignment {
            role: RoleName::ClusterPeer,
            scope: ResourceScope::Server,
        }];

        if self
            .get_user(SYSTEM_TENANT, CLUSTER_PEER_USERNAME)
            .is_none()
        {
            // Random unguessable password — password login on this account
            // is not used (token-only); the password is purely a placeholder
            // for the user record.
            use rand::Rng;
            let mut pw_bytes = [0u8; 32];
            rand::thread_rng().fill(&mut pw_bytes);
            let pw = hex::encode(pw_bytes);
            self.create_user(
                SYSTEM_TENANT,
                CLUSTER_PEER_USERNAME,
                &pw,
                cluster_peer_role.clone(),
            )?;
            tracing::info!(
                user = %format!("{SYSTEM_TENANT}/{CLUSTER_PEER_USERNAME}"),
                "bootstrapped cluster_peer user"
            );
        }

        // Re-issue the long-lived cluster_peer token on every boot.
        if let Some(user) = self.get_user(SYSTEM_TENANT, CLUSTER_PEER_USERNAME) {
            let token = jwt::generate_token_with_ttl(
                user,
                &self.jwt_config,
                Duration::from_secs(CLUSTER_PEER_TTL_SECS),
                "access",
            )?;
            let jwt_dir = self.auth_dir.join("jwt");
            std::fs::create_dir_all(&jwt_dir)?;
            let token_path = jwt_dir.join("cluster_peer.token");
            std::fs::write(&token_path, &token)?;
            set_owner_only_permissions(&token_path)?;
            tracing::debug!(
                path = %token_path.display(),
                "(re)issued cluster_peer token"
            );
        }
        Ok(())
    }

    /// Create a new user.
    pub fn create_user(
        &mut self,
        tenant: &str,
        username: &str,
        password: &str,
        roles: Vec<RoleAssignment>,
    ) -> Result<(), AuthError> {
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

    /// Authenticate and return `(access_token, refresh_token)`.
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
        let u = self
            .get_user(&claims.tenant, &claims.sub)
            .ok_or(AuthError::InvalidCredentials)?;
        jwt::generate_access_token(u, &self.jwt_config)
    }

    /// Active JWT algorithm.
    pub fn algorithm(&self) -> JwtAlgorithm {
        self.jwt_config.algorithm
    }

    /// Render the JWKS array.
    ///
    /// Returns an empty array under HS256 — symmetric secrets must never
    /// be exposed publicly.
    pub fn jwks(&self) -> Vec<Jwk> {
        match (self.jwt_config.algorithm, &self.jwt_config.rs_keys) {
            (JwtAlgorithm::Rs256, Some(ring)) => ring.iter().map(|k| k.to_jwk()).collect(),
            _ => Vec::new(),
        }
    }

    /// Promote `next` to `current`, generate a new `next`, and persist.
    ///
    /// The previous `current.pem` and `current.pub.pem` are renamed to
    /// `<kid>.pem.archived` / `<kid>.pub.pem.archived` so external tooling
    /// can still validate tokens issued before rotation. They are kept on
    /// disk for at least `DEFAULT_ARCHIVE_GRACE_SECS` and then garbage
    /// collected by future calls. (Actual reloading of archived keys is a
    /// Phase 40 follow-up.)
    pub fn rotate_jwt_keys(&mut self) -> Result<RotationOutcome, AuthError> {
        if self.jwt_config.algorithm != JwtAlgorithm::Rs256 {
            return Err(AuthError::JwtError(
                "rotate_jwt_keys requires RS256 algorithm".into(),
            ));
        }

        let (previous_current_kid, previous_next_kid) = {
            let ring = self.jwt_config.rs_keys.as_ref().expect("rs_keys for RS256");
            (ring.current.kid.clone(), ring.next.kid.clone())
        };

        // In-memory rotation.
        self.jwt_config.rotate()?;

        let (new_current_kid, new_next_kid) = {
            let ring = self.jwt_config.rs_keys.as_ref().expect("rs_keys for RS256");
            (ring.current.kid.clone(), ring.next.kid.clone())
        };

        let keys_dir = self.auth_dir.join("jwt").join("keys");

        // 1. Archive the old current key (it has rotated out of the ring
        //    but already-issued tokens may still want to verify it during
        //    the grace period).
        let old_current_priv = keys_dir.join(format!("{previous_current_kid}.pem"));
        let old_current_pub = keys_dir.join(format!("{previous_current_kid}.pub.pem"));
        if old_current_priv.exists() {
            let _ = std::fs::rename(
                &old_current_priv,
                keys_dir.join(format!("{previous_current_kid}.pem.archived")),
            );
        }
        if old_current_pub.exists() {
            let _ = std::fs::rename(
                &old_current_pub,
                keys_dir.join(format!("{previous_current_kid}.pub.pem.archived")),
            );
        }

        // 2. The previous `next` is now `current` — its files keep their
        //    name, no disk action needed (`previous_next_kid == new_current_kid`).
        debug_assert_eq!(previous_next_kid, new_current_kid);

        // 3. Persist the freshly generated `next` key.
        let ring = self.jwt_config.rs_keys.as_ref().expect("rs_keys for RS256");
        write_key_pair(&keys_dir, &ring.next)?;

        // 4. Update config.json with the new pointers.
        write_jwt_config(
            &self.auth_dir,
            JwtAlgorithm::Rs256,
            Some(&new_current_kid),
            Some(&new_next_kid),
            self.jwt_config.access_ttl.as_secs(),
            self.jwt_config.refresh_ttl.as_secs(),
        )?;

        // 5. Garbage-collect any archived files older than the grace period.
        gc_archived_keys(&keys_dir, DEFAULT_ARCHIVE_GRACE_SECS);

        // 6. Re-issue the cluster_peer token with the new current key so
        //    inter-node auth keeps working.
        self.bootstrap_cluster_peer()?;

        Ok(RotationOutcome {
            previous_current_kid,
            promoted_kid: new_current_kid,
            new_next_kid,
        })
    }

    /// Migrate this store from HS256 to RS256.
    ///
    /// Generates a fresh keyring, persists it, switches `config.json` to
    /// RS256, and re-issues the cluster_peer token under the new keyring.
    /// The legacy `secret.key` file is **kept on disk** (and remains
    /// usable by anything that still reads it directly) but no new tokens
    /// are issued under HS256 from this point. Operators can `rm secret.key`
    /// after the longest-lived outstanding HS256 token has expired.
    pub fn migrate_to_rs256(&mut self) -> Result<(), AuthError> {
        if self.jwt_config.algorithm == JwtAlgorithm::Rs256 {
            return Err(AuthError::JwtError(
                "already running RS256 — nothing to migrate".into(),
            ));
        }

        let ring = RsKeyRing::generate()?;
        let access_ttl = self.jwt_config.access_ttl.as_secs();
        let refresh_ttl = self.jwt_config.refresh_ttl.as_secs();

        // Persist key material first so a crash mid-write leaves us with
        // a consistent on-disk state (config.json still points at HS256).
        let keys_dir = self.auth_dir.join("jwt").join("keys");
        std::fs::create_dir_all(&keys_dir)?;
        write_key_pair(&keys_dir, &ring.current)?;
        write_key_pair(&keys_dir, &ring.next)?;

        let current_kid = ring.current.kid.clone();
        let next_kid = ring.next.kid.clone();

        // Swap in-memory config.
        self.jwt_config = JwtConfig::new_rs256(
            ring,
            Duration::from_secs(access_ttl),
            Duration::from_secs(refresh_ttl),
        );

        // Now flip config.json atomically (last step).
        write_jwt_config(
            &self.auth_dir,
            JwtAlgorithm::Rs256,
            Some(&current_kid),
            Some(&next_kid),
            access_ttl,
            refresh_ttl,
        )?;

        // Re-issue the cluster_peer token under the new algorithm.
        self.bootstrap_cluster_peer()?;

        tracing::info!(
            current_kid = %current_kid,
            next_kid = %next_kid,
            "migrated JWT signing algorithm to RS256"
        );
        Ok(())
    }

    // ── Private helpers ───────────────────────────

    fn user_file_path(&self, tenant: &str, username: &str) -> PathBuf {
        // Usernames may contain slashes (e.g. `_cluster/local-bootstrap`)
        // — sanitise for the file system by replacing path separators
        // with double underscores. The on-disk name is purely a hint;
        // identity is verified by re-reading the JSON contents.
        let safe_user = sanitise_for_filename(username);
        self.auth_dir
            .join("users")
            .join(format!("{tenant}__{safe_user}.json"))
    }

    fn save_user(&self, user: &User) -> Result<(), AuthError> {
        let path = self.user_file_path(&user.tenant, &user.username);
        let json = serde_json::to_string_pretty(user).map_err(|e| AuthError::Io(e.to_string()))?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

fn sanitise_for_filename(s: &str) -> String {
    s.replace(['/', '\\'], "__")
}

// ── JWT config + keyring loader ────────────────────────────────────

fn load_or_create_jwt_config(
    auth_dir: &Path,
    access_ttl_secs: u64,
    refresh_ttl_secs: u64,
    bootstrap_algorithm: JwtAlgorithm,
) -> Result<JwtConfig, AuthError> {
    let jwt_dir = auth_dir.join("jwt");
    let config_path = jwt_dir.join("config.json");
    let secret_path = auth_dir.join("secret.key");

    if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path)?;
        let disk: JwtDiskConfig =
            serde_json::from_str(&raw).map_err(|e| AuthError::Io(e.to_string()))?;
        return load_from_disk_config(auth_dir, &disk);
    }

    if secret_path.exists() {
        // Legacy v4 deployment — stay on HS256 until an explicit migration.
        check_secret_permissions(&secret_path)?;
        let secret = read_hs_secret(&secret_path)?;
        // Don't overwrite anything; migration writes config.json.
        return Ok(JwtConfig::new_hs256(
            secret,
            Duration::from_secs(access_ttl_secs),
            Duration::from_secs(refresh_ttl_secs),
        ));
    }

    // Fresh install — bootstrap according to the requested algorithm.
    match bootstrap_algorithm {
        JwtAlgorithm::Hs256 => {
            tracing::info!(
                "no JWT config on disk; bootstrapping HS256 (cheap keygen — \
                 typical of test harnesses spawning short-lived servers)"
            );
            std::fs::create_dir_all(&jwt_dir)?;
            use rand::Rng;
            let mut secret = [0u8; 32];
            rand::thread_rng().fill(&mut secret);
            std::fs::write(&secret_path, secret)?;
            set_owner_only_permissions(&secret_path)?;
            write_jwt_config(
                auth_dir,
                JwtAlgorithm::Hs256,
                None,
                None,
                access_ttl_secs,
                refresh_ttl_secs,
            )?;
            Ok(JwtConfig::new_hs256(
                secret,
                Duration::from_secs(access_ttl_secs),
                Duration::from_secs(refresh_ttl_secs),
            ))
        }
        JwtAlgorithm::Rs256 => {
            tracing::info!(
                "no JWT config on disk; generating fresh RS256 keyring (default for v5)"
            );
            std::fs::create_dir_all(jwt_dir.join("keys"))?;
            let ring = RsKeyRing::generate()?;
            let keys_dir = jwt_dir.join("keys");
            write_key_pair(&keys_dir, &ring.current)?;
            write_key_pair(&keys_dir, &ring.next)?;
            let current_kid = ring.current.kid.clone();
            let next_kid = ring.next.kid.clone();
            write_jwt_config(
                auth_dir,
                JwtAlgorithm::Rs256,
                Some(&current_kid),
                Some(&next_kid),
                access_ttl_secs,
                refresh_ttl_secs,
            )?;
            Ok(JwtConfig::new_rs256(
                ring,
                Duration::from_secs(access_ttl_secs),
                Duration::from_secs(refresh_ttl_secs),
            ))
        }
    }
}

fn load_from_disk_config(auth_dir: &Path, disk: &JwtDiskConfig) -> Result<JwtConfig, AuthError> {
    match disk.algorithm {
        JwtAlgorithm::Hs256 => {
            let secret_path = auth_dir.join("secret.key");
            check_secret_permissions(&secret_path)?;
            let secret = read_hs_secret(&secret_path)?;
            Ok(JwtConfig::new_hs256(
                secret,
                Duration::from_secs(disk.access_ttl_secs),
                Duration::from_secs(disk.refresh_ttl_secs),
            ))
        }
        JwtAlgorithm::Rs256 => {
            let current_kid = disk
                .current_kid
                .as_deref()
                .ok_or_else(|| AuthError::Io("RS256 config.json missing current_kid".into()))?;
            let next_kid = disk
                .next_kid
                .as_deref()
                .ok_or_else(|| AuthError::Io("RS256 config.json missing next_kid".into()))?;
            let keys_dir = auth_dir.join("jwt").join("keys");
            let current = read_rs_key(&keys_dir, current_kid)?;
            let next = read_rs_key(&keys_dir, next_kid)?;
            let ring = RsKeyRing { current, next };
            Ok(JwtConfig::new_rs256(
                ring,
                Duration::from_secs(disk.access_ttl_secs),
                Duration::from_secs(disk.refresh_ttl_secs),
            ))
        }
    }
}

fn read_hs_secret(path: &Path) -> Result<[u8; 32], AuthError> {
    if path.exists() {
        let data = std::fs::read(path)?;
        if data.len() != 32 {
            return Err(AuthError::Io(format!(
                "secret.key has invalid length: {} (expected 32)",
                data.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&data);
        Ok(arr)
    } else {
        use rand::Rng;
        let mut secret = [0u8; 32];
        rand::thread_rng().fill(&mut secret);
        std::fs::write(path, secret)?;
        set_owner_only_permissions(path)?;
        Ok(secret)
    }
}

fn read_rs_key(keys_dir: &Path, kid: &str) -> Result<RsKey, AuthError> {
    let priv_path = keys_dir.join(format!("{kid}.pem"));
    let pub_path = keys_dir.join(format!("{kid}.pub.pem"));
    let priv_pem = std::fs::read_to_string(&priv_path)
        .map_err(|e| AuthError::Io(format!("read {}: {}", priv_path.display(), e)))?;
    let pub_pem = std::fs::read_to_string(&pub_path)
        .map_err(|e| AuthError::Io(format!("read {}: {}", pub_path.display(), e)))?;
    RsKey::from_pem(&priv_pem, &pub_pem)
}

fn write_key_pair(keys_dir: &Path, key: &RsKey) -> Result<(), AuthError> {
    std::fs::create_dir_all(keys_dir)?;
    let priv_path = keys_dir.join(format!("{}.pem", key.kid));
    let pub_path = keys_dir.join(format!("{}.pub.pem", key.kid));
    std::fs::write(&priv_path, key.private_pem.as_bytes())?;
    set_owner_only_permissions(&priv_path)?;
    std::fs::write(&pub_path, key.public_pem.as_bytes())?;
    set_world_readable_permissions(&pub_path)?;
    Ok(())
}

fn write_jwt_config(
    auth_dir: &Path,
    algorithm: JwtAlgorithm,
    current_kid: Option<&str>,
    next_kid: Option<&str>,
    access_ttl_secs: u64,
    refresh_ttl_secs: u64,
) -> Result<(), AuthError> {
    let jwt_dir = auth_dir.join("jwt");
    std::fs::create_dir_all(&jwt_dir)?;
    let disk = JwtDiskConfig {
        algorithm,
        current_kid: current_kid.map(String::from),
        next_kid: next_kid.map(String::from),
        access_ttl_secs,
        refresh_ttl_secs,
    };
    let path = jwt_dir.join("config.json");
    let json = serde_json::to_string_pretty(&disk).map_err(|e| AuthError::Io(e.to_string()))?;
    // Atomic write via rename.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn gc_archived_keys(keys_dir: &Path, grace_secs: u64) {
    let Ok(entries) = std::fs::read_dir(keys_dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".archived") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if let Ok(elapsed) = now.duration_since(modified)
            && elapsed.as_secs() > grace_secs
        {
            let _ = std::fs::remove_file(&path);
            tracing::debug!(path = %path.display(), "garbage-collected archived key");
        }
    }
}

// ── File permission helpers ───────────────────────────────────────

fn set_owner_only_permissions(path: &Path) -> Result<(), AuthError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    let _ = path;
    Ok(())
}

fn set_world_readable_permissions(path: &Path) -> Result<(), AuthError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(path, perms)?;
    }
    let _ = path;
    Ok(())
}

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
        // Default test bootstrap is HS256 — RSA-2048 keygen in debug
        // builds is too slow for the volume of TestServer instances some
        // CI runs spawn. RS256-specific tests use `setup_rs256()`.
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("_auth");
        let store = AuthStore::open(
            &auth_dir,
            3600,
            604800,
            Some("bootstrap-test-pw"),
            JwtAlgorithm::Hs256,
        )
        .unwrap();
        (dir, store)
    }

    fn setup_rs256() -> (TempDir, AuthStore) {
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("_auth");
        let store = AuthStore::open(
            &auth_dir,
            3600,
            604800,
            Some("bootstrap-test-pw"),
            JwtAlgorithm::Rs256,
        )
        .unwrap();
        (dir, store)
    }

    fn setup_hs256() -> (TempDir, AuthStore) {
        // Simulate a v4 install: pre-create `secret.key` so the open path
        // selects HS256 backwards-compat instead of generating RS256.
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("_auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        let secret = [7u8; 32];
        std::fs::write(auth_dir.join("secret.key"), secret).unwrap();
        let store = AuthStore::open(
            &auth_dir,
            3600,
            604800,
            Some("bootstrap-test-pw"),
            JwtAlgorithm::Hs256,
        )
        .unwrap();
        (dir, store)
    }

    #[test]
    fn test_store_bootstrap_creates_admin() {
        let (_dir, store) = setup();
        let admin = store.get_user("_system", "admin").unwrap();
        assert_eq!(admin.roles[0].role, RoleName::ServerAdmin);
    }

    #[test]
    fn test_store_bootstrap_creates_cluster_peer() {
        let (_dir, store) = setup();
        let cp = store
            .get_user("_system", "_cluster/local-bootstrap")
            .expect("cluster_peer user must be auto-created");
        assert_eq!(cp.roles.len(), 1);
        assert_eq!(cp.roles[0].role, RoleName::ClusterPeer);
        // Token file exists.
        let tok_path = store.auth_dir.join("jwt").join("cluster_peer.token");
        assert!(tok_path.exists(), "cluster_peer.token should be written");
        // Token verifies.
        let token = std::fs::read_to_string(&tok_path).unwrap();
        let claims = store.verify_token(token.trim()).unwrap();
        assert_eq!(claims.sub, "_cluster/local-bootstrap");
    }

    #[test]
    fn test_fresh_install_defaults_to_rs256() {
        let (_dir, store) = setup_rs256();
        assert_eq!(store.algorithm(), JwtAlgorithm::Rs256);
        let jwks = store.jwks();
        assert_eq!(jwks.len(), 2, "JWKS should contain both keys in the ring");
        assert!(jwks.iter().all(|k| k.alg == "RS256"));
    }

    #[test]
    fn test_legacy_hs256_still_works() {
        let (_dir, store) = setup_hs256();
        assert_eq!(store.algorithm(), JwtAlgorithm::Hs256);
        // JWKS must be empty under HS256.
        assert!(store.jwks().is_empty());
    }

    #[test]
    fn test_store_refuses_silent_bootstrap() {
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("_auth");
        let result = AuthStore::open(&auth_dir, 3600, 604800, None, JwtAlgorithm::Hs256);
        assert!(matches!(result, Err(AuthError::BootstrapRefused(_))));
    }

    #[test]
    fn test_store_no_rebootstrap_after_users_exist() {
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("_auth");
        let _ = AuthStore::open(
            &auth_dir,
            3600,
            604800,
            Some("first-pw"),
            JwtAlgorithm::Hs256,
        )
        .unwrap();
        // Reopen without a bootstrap password — fine.
        let store = AuthStore::open(&auth_dir, 3600, 604800, None, JwtAlgorithm::Hs256).unwrap();
        assert!(store.get_user("_system", "admin").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_rs256_private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, store) = setup_rs256();
        // Pull the kid from the JWKS so we don't rely on internals.
        let jwks = store.jwks();
        let kid = &jwks[0].kid;
        let path = store
            .auth_dir
            .join("jwt")
            .join("keys")
            .join(format!("{kid}.pem"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o077, 0, "private key has lax permissions: {mode:o}");
        // Public part should be world-readable so JWKS verifiers (or
        // operators) can grab it.
        let pub_path = store
            .auth_dir
            .join("jwt")
            .join("keys")
            .join(format!("{kid}.pub.pem"));
        let pub_mode = std::fs::metadata(&pub_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(pub_mode & 0o077, 0o044, "public key mode: {pub_mode:o}");
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
        assert_eq!(store.list_users("acme").len(), 2);
        assert_eq!(store.list_users("globex").len(), 1);
    }

    #[test]
    fn test_store_update_roles() {
        let (_dir, mut store) = setup();
        store.create_user("acme", "alice", "pass", vec![]).unwrap();
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
        assert_eq!(
            store.get_user("acme", "alice").unwrap().roles[0].role,
            RoleName::DbAdmin
        );
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
        let claims = store.verify_token(&access).unwrap();
        assert_eq!(claims.sub, "alice");
    }

    #[test]
    fn test_store_authenticate_wrong_password() {
        let (_dir, mut store) = setup();
        store
            .create_user("acme", "alice", "s3cr3t", vec![])
            .unwrap();
        assert!(matches!(
            store.authenticate("acme", "alice", "wrong"),
            Err(AuthError::InvalidCredentials)
        ));
    }

    #[test]
    fn test_store_authenticate_unknown_user() {
        let (_dir, store) = setup();
        assert!(matches!(
            store.authenticate("acme", "nobody", "pass"),
            Err(AuthError::InvalidCredentials)
        ));
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
        assert!(matches!(
            store.refresh_access_token(&access),
            Err(AuthError::InvalidToken(_))
        ));
    }

    #[test]
    fn test_store_persistence_across_reopen_rs256() {
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("_auth");
        let kid_first;
        {
            let mut store = AuthStore::open(
                &auth_dir,
                3600,
                604800,
                Some("bootstrap-test-pw"),
                JwtAlgorithm::Rs256,
            )
            .unwrap();
            store
                .create_user("acme", "alice", "s3cr3t", vec![])
                .unwrap();
            kid_first = store.jwks()[0].kid.clone();
        }
        // Reopen — same kid (key material persisted), token still verifies.
        // The bootstrap_algorithm passed below is ignored because the
        // on-disk config wins.
        let store = AuthStore::open(
            &auth_dir,
            3600,
            604800,
            Some("bootstrap-test-pw"),
            JwtAlgorithm::Hs256,
        )
        .unwrap();
        assert_eq!(store.algorithm(), JwtAlgorithm::Rs256);
        assert_eq!(store.jwks()[0].kid, kid_first);
        let (access, _) = store.authenticate("acme", "alice", "s3cr3t").unwrap();
        let claims = store.verify_token(&access).unwrap();
        assert_eq!(claims.sub, "alice");
    }

    #[test]
    fn test_store_persistence_across_reopen_hs256() {
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("_auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        std::fs::write(auth_dir.join("secret.key"), [9u8; 32]).unwrap();

        let token;
        {
            let mut store = AuthStore::open(
                &auth_dir,
                3600,
                604800,
                Some("bootstrap-test-pw"),
                JwtAlgorithm::Hs256,
            )
            .unwrap();
            assert_eq!(store.algorithm(), JwtAlgorithm::Hs256);
            store.create_user("acme", "alice", "pass", vec![]).unwrap();
            let (t, _) = store.authenticate("acme", "alice", "pass").unwrap();
            token = t;
        }
        let store = AuthStore::open(
            &auth_dir,
            3600,
            604800,
            Some("bootstrap-test-pw"),
            JwtAlgorithm::Hs256,
        )
        .unwrap();
        assert_eq!(store.algorithm(), JwtAlgorithm::Hs256);
        let claims = store.verify_token(&token).unwrap();
        assert_eq!(claims.sub, "alice");
    }

    #[test]
    fn test_rotate_promotes_next_and_archives_old_current() {
        let (_dir, mut store) = setup_rs256();
        let kids_before: Vec<String> = store.jwks().iter().map(|j| j.kid.clone()).collect();
        let outcome = store.rotate_jwt_keys().unwrap();
        let kids_after: Vec<String> = store.jwks().iter().map(|j| j.kid.clone()).collect();

        assert_eq!(outcome.previous_current_kid, kids_before[0]);
        assert_eq!(outcome.promoted_kid, kids_before[1]);
        assert_eq!(kids_after[0], outcome.promoted_kid);
        assert_eq!(kids_after[1], outcome.new_next_kid);
        assert_ne!(outcome.new_next_kid, outcome.previous_current_kid);

        // Old current key must now be archived (not in JWKS).
        let archived_priv = store
            .auth_dir
            .join("jwt")
            .join("keys")
            .join(format!("{}.pem.archived", outcome.previous_current_kid));
        assert!(
            archived_priv.exists(),
            "old current.pem should be archived: {}",
            archived_priv.display()
        );
    }

    #[test]
    fn test_rotate_on_hs256_errors() {
        let (_dir, mut store) = setup_hs256();
        let result = store.rotate_jwt_keys();
        assert!(matches!(result, Err(AuthError::JwtError(_))));
    }

    #[test]
    fn test_migrate_to_rs256_from_hs256() {
        let (dir, mut store) = setup_hs256();
        // Sanity: HS256 to start.
        assert_eq!(store.algorithm(), JwtAlgorithm::Hs256);
        assert!(store.jwks().is_empty());

        store.migrate_to_rs256().unwrap();
        assert_eq!(store.algorithm(), JwtAlgorithm::Rs256);
        assert_eq!(store.jwks().len(), 2);

        // Reopen — RS256 sticks.
        let auth_dir = dir.path().join("_auth");
        let store2 = AuthStore::open(&auth_dir, 3600, 604800, None, JwtAlgorithm::Hs256).unwrap();
        assert_eq!(store2.algorithm(), JwtAlgorithm::Rs256);
        // Old secret.key is intentionally kept on disk for the operator to
        // archive once the longest outstanding HS256 token has expired.
        assert!(auth_dir.join("secret.key").exists());
    }

    #[test]
    fn test_migrate_already_rs256_errors() {
        let (_dir, mut store) = setup_rs256();
        let result = store.migrate_to_rs256();
        assert!(matches!(result, Err(AuthError::JwtError(_))));
    }

    #[test]
    fn test_jwks_returns_unique_kids_for_rs256() {
        let (_dir, store) = setup_rs256();
        let jwks = store.jwks();
        assert_eq!(jwks.len(), 2);
        assert_ne!(jwks[0].kid, jwks[1].kid);
        for jwk in &jwks {
            assert_eq!(jwk.kty, "RSA");
            assert_eq!(jwk.r#use, "sig");
            assert_eq!(jwk.e, "AQAB");
            assert!(!jwk.n.is_empty());
        }
    }

    #[test]
    fn test_cluster_peer_token_reissued_on_rotate() {
        let (_dir, mut store) = setup_rs256();
        let tok_path = store.auth_dir.join("jwt").join("cluster_peer.token");
        let token_before = std::fs::read_to_string(&tok_path).unwrap();
        store.rotate_jwt_keys().unwrap();
        let token_after = std::fs::read_to_string(&tok_path).unwrap();
        // The token bytes change because the kid in the header is now the
        // newly-promoted key.
        assert_ne!(token_before, token_after);
        // And the new token verifies against the rotated keyring.
        let claims = store.verify_token(token_after.trim()).unwrap();
        assert_eq!(claims.sub, "_cluster/local-bootstrap");
    }

    #[test]
    fn test_sanitise_for_filename_replaces_path_separators() {
        assert_eq!(sanitise_for_filename("simple"), "simple");
        assert_eq!(sanitise_for_filename("a/b"), "a__b");
        assert_eq!(sanitise_for_filename("_cluster/x"), "_cluster__x");
    }
}
