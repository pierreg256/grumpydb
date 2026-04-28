//! JWT token generation and verification (HS256).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::auth::role::RoleAssignment;
use crate::auth::user::{AuthError, User};

/// JWT configuration.
pub struct JwtConfig {
    /// HMAC-SHA256 secret key (32 bytes).
    pub secret: [u8; 32],
    /// Access token time-to-live.
    pub access_ttl: Duration,
    /// Refresh token time-to-live.
    pub refresh_ttl: Duration,
}

impl JwtConfig {
    /// Create a config with a random secret (for testing).
    pub fn new_random() -> Self {
        use rand::Rng;
        let mut secret = [0u8; 32];
        rand::thread_rng().fill(&mut secret);
        Self {
            secret,
            access_ttl: Duration::from_secs(3600),
            refresh_ttl: Duration::from_secs(7 * 24 * 3600),
        }
    }

    /// Create a config from an existing secret.
    pub fn new(secret: [u8; 32], access_ttl: Duration, refresh_ttl: Duration) -> Self {
        Self {
            secret,
            access_ttl,
            refresh_ttl,
        }
    }
}

/// JWT claims embedded in access and refresh tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject (username).
    pub sub: String,
    /// Tenant name.
    pub tenant: String,
    /// Role assignments.
    pub roles: Vec<RoleAssignment>,
    /// Issued at (Unix timestamp).
    pub iat: u64,
    /// Expiration (Unix timestamp).
    pub exp: u64,
    /// Token type: "access" or "refresh".
    #[serde(default = "default_token_type")]
    pub token_type: String,
}

fn default_token_type() -> String {
    "access".to_string()
}

/// Generate an access token for the given user.
pub fn generate_access_token(user: &User, config: &JwtConfig) -> Result<String, AuthError> {
    generate_token(user, config, &config.access_ttl, "access")
}

/// Generate a refresh token for the given user.
pub fn generate_refresh_token(user: &User, config: &JwtConfig) -> Result<String, AuthError> {
    generate_token(user, config, &config.refresh_ttl, "refresh")
}

fn generate_token(
    user: &User,
    config: &JwtConfig,
    ttl: &Duration,
    token_type: &str,
) -> Result<String, AuthError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let claims = Claims {
        sub: user.username.clone(),
        tenant: user.tenant.clone(),
        roles: user.roles.clone(),
        iat: now,
        exp: now + ttl.as_secs(),
        token_type: token_type.to_string(),
    };

    let key = EncodingKey::from_secret(&config.secret);
    encode(&Header::default(), &claims, &key).map_err(|e| AuthError::JwtError(e.to_string()))
}

/// Verify a token and return the decoded claims.
///
/// Checks the HMAC-SHA256 signature and expiration time.
pub fn verify_token(token: &str, config: &JwtConfig) -> Result<Claims, AuthError> {
    let key = DecodingKey::from_secret(&config.secret);
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    validation.leeway = 0; // No grace period for expiration
    // Required claims
    validation.required_spec_claims.clear();
    validation.required_spec_claims.insert("exp".to_string());
    validation.required_spec_claims.insert("sub".to_string());

    decode::<Claims>(token, &key, &validation)
        .map(|data| data.claims)
        .map_err(|e| match e.kind() {
            jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::TokenExpired,
            _ => AuthError::InvalidToken(e.to_string()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::role::{ResourceScope, RoleName};

    fn test_user() -> User {
        User {
            username: "alice".into(),
            tenant: "acme".into(),
            password_hash: "not-used".into(),
            roles: vec![RoleAssignment {
                role: RoleName::ReadWrite,
                scope: ResourceScope::Database {
                    name: "mydb".into(),
                },
            }],
            created_at: 1700000000,
        }
    }

    #[test]
    fn test_generate_and_verify_access_token() {
        let config = JwtConfig::new_random();
        let user = test_user();

        let token = generate_access_token(&user, &config).unwrap();
        assert!(!token.is_empty());

        let claims = verify_token(&token, &config).unwrap();
        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.tenant, "acme");
        assert_eq!(claims.token_type, "access");
        assert_eq!(claims.roles.len(), 1);
        assert_eq!(claims.roles[0].role, RoleName::ReadWrite);
    }

    #[test]
    fn test_generate_and_verify_refresh_token() {
        let config = JwtConfig::new_random();
        let user = test_user();

        let token = generate_refresh_token(&user, &config).unwrap();
        let claims = verify_token(&token, &config).unwrap();
        assert_eq!(claims.token_type, "refresh");
        assert!(claims.exp > claims.iat);
    }

    #[test]
    fn test_wrong_secret_rejected() {
        let config1 = JwtConfig::new_random();
        let config2 = JwtConfig::new_random();
        let user = test_user();

        let token = generate_access_token(&user, &config1).unwrap();
        let result = verify_token(&token, &config2);
        assert!(matches!(result, Err(AuthError::InvalidToken(_))));
    }

    #[test]
    fn test_expired_token_rejected() {
        let config = JwtConfig::new([42u8; 32], Duration::from_secs(0), Duration::from_secs(0));
        let user = test_user();

        let token = generate_access_token(&user, &config).unwrap();
        // Token has exp = now + 0, so it's already expired
        std::thread::sleep(Duration::from_millis(1100));
        let result = verify_token(&token, &config);
        assert!(matches!(result, Err(AuthError::TokenExpired)));
    }

    #[test]
    fn test_tampered_token_rejected() {
        let config = JwtConfig::new_random();
        let user = test_user();

        let token = generate_access_token(&user, &config).unwrap();
        // Tamper with the token payload
        let mut tampered = token.clone();
        let last_char = tampered.pop().unwrap();
        let replacement = if last_char == 'A' { 'B' } else { 'A' };
        tampered.push(replacement);

        let result = verify_token(&tampered, &config);
        assert!(result.is_err());
    }

    #[test]
    fn test_access_and_refresh_have_different_exp() {
        let config = JwtConfig::new_random();
        let user = test_user();

        let access = generate_access_token(&user, &config).unwrap();
        let refresh = generate_refresh_token(&user, &config).unwrap();

        let access_claims = verify_token(&access, &config).unwrap();
        let refresh_claims = verify_token(&refresh, &config).unwrap();

        // Refresh token has longer TTL
        assert!(refresh_claims.exp > access_claims.exp);
    }

    #[test]
    fn test_invalid_token_string() {
        let config = JwtConfig::new_random();
        assert!(matches!(
            verify_token("not.a.jwt", &config),
            Err(AuthError::InvalidToken(_))
        ));
        assert!(matches!(
            verify_token("", &config),
            Err(AuthError::InvalidToken(_))
        ));
    }

    #[test]
    fn test_claims_preserve_roles() {
        let config = JwtConfig::new_random();
        let user = User {
            username: "bob".into(),
            tenant: "acme".into(),
            password_hash: "x".into(),
            roles: vec![
                RoleAssignment {
                    role: RoleName::ReadWrite,
                    scope: ResourceScope::Database {
                        name: "db1".into(),
                    },
                },
                RoleAssignment {
                    role: RoleName::DbAdmin,
                    scope: ResourceScope::Database {
                        name: "db2".into(),
                    },
                },
            ],
            created_at: 0,
        };

        let token = generate_access_token(&user, &config).unwrap();
        let claims = verify_token(&token, &config).unwrap();
        assert_eq!(claims.roles.len(), 2);
        assert_eq!(claims.roles[0].role, RoleName::ReadWrite);
        assert_eq!(claims.roles[1].role, RoleName::DbAdmin);
    }
}
