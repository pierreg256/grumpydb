//! User records and password hashing with argon2.

use serde::{Deserialize, Serialize};

use crate::auth::role::RoleAssignment;

/// A user record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// Username (unique within a tenant).
    pub username: String,
    /// Tenant this user belongs to.
    pub tenant: String,
    /// Argon2 password hash (PHC string format).
    pub password_hash: String,
    /// Assigned roles with scopes.
    pub roles: Vec<RoleAssignment>,
    /// Creation timestamp (Unix epoch seconds).
    pub created_at: u64,
}

/// Hash a password with argon2id and a random salt.
///
/// Returns the hash in PHC string format:
/// `$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    use argon2::password_hash::{SaltString, rand_core::OsRng};
    use argon2::{Argon2, PasswordHasher};

    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::HashError(e.to_string()))?;
    Ok(hash.to_string())
}

/// Verify a password against an argon2 hash.
pub fn verify_password(password: &str, hash: &str) -> Result<bool, AuthError> {
    use argon2::{Argon2, PasswordVerifier};

    let parsed_hash =
        argon2::PasswordHash::new(hash).map_err(|e| AuthError::HashError(e.to_string()))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}

/// Auth-specific errors.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AuthError {
    #[error("password hashing error: {0}")]
    HashError(String),
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("user not found: {0}")]
    UserNotFound(String),
    #[error("user already exists: {0}")]
    UserAlreadyExists(String),
    #[error("JWT error: {0}")]
    JwtError(String),
    #[error("token expired")]
    TokenExpired,
    #[error("invalid token: {0}")]
    InvalidToken(String),
    #[error("access denied: {0}")]
    AccessDenied(String),
    #[error("not authenticated")]
    NotAuthenticated,
    #[error("I/O error: {0}")]
    Io(String),
    #[error("system clock error: {0}")]
    ClockError(String),
    #[error("server is configured for read-only access")]
    ReadOnly,
    #[error("password change required before further operations")]
    PasswordChangeRequired,
    #[error("server refuses to bootstrap: {0}")]
    BootstrapRefused(String),
}

impl From<std::io::Error> for AuthError {
    fn from(e: std::io::Error) -> Self {
        AuthError::Io(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_and_verify_password() {
        let hash = hash_password("s3cr3t").unwrap();
        assert!(hash.starts_with("$argon2"));
        assert!(verify_password("s3cr3t", &hash).unwrap());
        assert!(!verify_password("wrong", &hash).unwrap());
    }

    #[test]
    fn test_different_passwords_different_hashes() {
        let h1 = hash_password("password1").unwrap();
        let h2 = hash_password("password1").unwrap();
        // Different salts → different hashes
        assert_ne!(h1, h2);
        // But both verify
        assert!(verify_password("password1", &h1).unwrap());
        assert!(verify_password("password1", &h2).unwrap());
    }

    #[test]
    fn test_empty_password() {
        let hash = hash_password("").unwrap();
        assert!(verify_password("", &hash).unwrap());
        assert!(!verify_password("x", &hash).unwrap());
    }

    #[test]
    fn test_verify_invalid_hash() {
        assert!(verify_password("test", "not-a-valid-hash").is_err());
    }

    #[test]
    fn test_user_serde_round_trip() {
        use crate::auth::role::{ResourceScope, RoleName};

        let user = User {
            username: "alice".into(),
            tenant: "acme".into(),
            password_hash: "$argon2id$v=19$m=19456,t=2,p=1$salt$hash".into(),
            roles: vec![RoleAssignment {
                role: RoleName::ReadWrite,
                scope: ResourceScope::Database {
                    name: "mydb".into(),
                },
            }],
            created_at: 1700000000,
        };
        let json = serde_json::to_string_pretty(&user).unwrap();
        let parsed: User = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.username, "alice");
        assert_eq!(parsed.tenant, "acme");
        assert_eq!(parsed.roles.len(), 1);
    }
}
