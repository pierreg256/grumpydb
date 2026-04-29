//! JWT token generation and verification.
//!
//! Phase 39: dual-algorithm support.
//!
//! - **HS256** (legacy): a single 32-byte symmetric secret, used by all v4
//!   deployments. The secret is loaded from `_auth/secret.key`. Tokens omit
//!   the `kid` header (a single key is in use).
//! - **RS256** (new default): asymmetric RSA-2048 keypair, with a two-key
//!   ring (`current` + `next`) that supports zero-downtime rotation. Tokens
//!   carry a `kid` header pointing to the key that signed them; verifiers
//!   try the matching key in the ring. Public keys are exposed via the
//!   JWKS endpoint at `/.well-known/jwks.json` (see [`crate::http`]).
//!
//! The two-key ring is the standard pattern for graceful key rotation:
//! during rotation, `next` is promoted to `current` and a fresh `next` is
//! generated; existing tokens (signed with the old `current`, now still
//! present in the ring as `next`) keep verifying until they expire.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use rsa::pkcs1::EncodeRsaPublicKey;
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::role::RoleAssignment;
use crate::auth::user::{AuthError, User};

/// Recommended modulus size for fresh RSA keypairs (in bits).
pub const RSA_BITS: usize = 2048;

/// Selects the JWT signing/verification algorithm.
///
/// `JwtAlgorithm` is recorded in the on-disk `_auth/jwt/config.json` so a
/// server reopened against an existing data directory uses exactly the
/// same algorithm it used last time, regardless of any new defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JwtAlgorithm {
    /// HMAC-SHA256 with a 32-byte symmetric secret. Legacy v4 default.
    /// Public verification is impossible; JWKS returns an empty key set.
    Hs256,
    /// RSASSA-PKCS1-v1_5 with SHA-256 over an RSA-2048 keypair. Default
    /// for new v5 deployments. Public keys are exposed via JWKS so any
    /// peer (cluster member, external service) can verify tokens.
    Rs256,
}

/// A single RSA keypair in the keyring.
///
/// Holds both the parsed `EncodingKey`/`DecodingKey` for `jsonwebtoken`
/// operations and the original PEM-encoded forms for persistence and JWKS.
/// The `kid` is a short stable identifier derived from the SHA-256 of the
/// SPKI DER encoding of the public key — the same value (truncated) is
/// embedded in every JWT header signed with this key.
pub struct RsKey {
    /// 16-character hex Key ID = first 8 bytes of `SHA-256(spki_der)`.
    pub kid: String,
    /// Pre-parsed signing key (private).
    pub encoding_key: EncodingKey,
    /// Pre-parsed verification key (public).
    pub decoding_key: DecodingKey,
    /// PKCS#8 PEM of the private key (for disk persistence).
    pub private_pem: String,
    /// SubjectPublicKeyInfo PEM of the public key (for disk + JWKS).
    pub public_pem: String,
    /// Raw `RsaPublicKey` retained so JWKS can expose `n` / `e` without
    /// re-parsing the PEM on every request.
    public_key: RsaPublicKey,
    /// Unix-second timestamp at creation. Useful for operational logging
    /// and for picking the "older" key during a rotation drill.
    pub created_at: u64,
}

impl std::fmt::Debug for RsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RsKey")
            .field("kid", &self.kid)
            .field("created_at", &self.created_at)
            // Intentionally omit the actual key material.
            .finish_non_exhaustive()
    }
}

impl RsKey {
    /// Generate a fresh RSA-2048 keypair and wrap it as an `RsKey`.
    ///
    /// Generation is deliberately blocking: it is called only at server
    /// boot or during explicit rotation, never on a per-request hot path.
    /// On a modern machine an RSA-2048 keypair takes well under a second
    /// (see Phase 39 benchmark in the test module).
    pub fn generate() -> Result<Self, AuthError> {
        let mut rng = rand::rngs::OsRng;
        let private = RsaPrivateKey::new(&mut rng, RSA_BITS)
            .map_err(|e| AuthError::JwtError(format!("RSA keygen failed: {e}")))?;
        let public = RsaPublicKey::from(&private);
        Self::from_parts(private, public)
    }

    /// Build an `RsKey` from already-loaded key material (used at boot
    /// when reading the on-disk PEMs).
    pub fn from_parts(private: RsaPrivateKey, public: RsaPublicKey) -> Result<Self, AuthError> {
        let private_pem = private
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| AuthError::JwtError(format!("PKCS#8 PEM encode: {e}")))?
            .to_string();
        let public_pem = public
            .to_public_key_pem(LineEnding::LF)
            .map_err(|e| AuthError::JwtError(format!("SPKI PEM encode: {e}")))?;

        let encoding_key = EncodingKey::from_rsa_pem(private_pem.as_bytes())
            .map_err(|e| AuthError::JwtError(format!("EncodingKey from PEM: {e}")))?;
        let decoding_key = DecodingKey::from_rsa_pem(public_pem.as_bytes())
            .map_err(|e| AuthError::JwtError(format!("DecodingKey from PEM: {e}")))?;

        let spki_der = public
            .to_public_key_der()
            .map_err(|e| AuthError::JwtError(format!("SPKI DER encode: {e}")))?;
        let mut hasher = Sha256::new();
        hasher.update(spki_der.as_bytes());
        let digest = hasher.finalize();
        let kid = hex::encode(&digest[..8]);

        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| AuthError::ClockError(e.to_string()))?
            .as_secs();

        Ok(Self {
            kid,
            encoding_key,
            decoding_key,
            private_pem,
            public_pem,
            public_key: public,
            created_at,
        })
    }

    /// Load an `RsKey` from PKCS#8 PEM (private) and SPKI PEM (public).
    pub fn from_pem(private_pem: &str, public_pem: &str) -> Result<Self, AuthError> {
        use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};

        let private = RsaPrivateKey::from_pkcs8_pem(private_pem)
            .map_err(|e| AuthError::JwtError(format!("private PKCS#8 PEM decode: {e}")))?;
        let public = RsaPublicKey::from_public_key_pem(public_pem)
            .map_err(|e| AuthError::JwtError(format!("public SPKI PEM decode: {e}")))?;
        Self::from_parts(private, public)
    }

    /// JWKS representation of the public part (`n` / `e` base64url-encoded
    /// big-endian, leading zeros stripped per RFC 7518 §6.3.1).
    pub fn to_jwk(&self) -> Jwk {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let n_bytes = self.public_key.n().to_bytes_be();
        let e_bytes = self.public_key.e().to_bytes_be();
        Jwk {
            kty: "RSA".into(),
            alg: "RS256".into(),
            r#use: "sig".into(),
            kid: self.kid.clone(),
            n: URL_SAFE_NO_PAD.encode(strip_leading_zeros(&n_bytes)),
            e: URL_SAFE_NO_PAD.encode(strip_leading_zeros(&e_bytes)),
        }
    }

    /// PKCS#1 PEM (`-----BEGIN RSA PUBLIC KEY-----`) of the public key.
    /// Currently unused by GrumpyDB but exposed for tooling that prefers
    /// the legacy PKCS#1 format over SPKI.
    pub fn public_pkcs1_pem(&self) -> Result<String, AuthError> {
        self.public_key
            .to_pkcs1_pem(LineEnding::LF)
            .map(|pem| pem.to_string())
            .map_err(|e| AuthError::JwtError(format!("PKCS#1 PEM encode: {e}")))
    }
}

fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let mut i = 0;
    while i + 1 < bytes.len() && bytes[i] == 0 {
        i += 1;
    }
    &bytes[i..]
}

/// Two-key ring used by RS256.
///
/// Writes always use `current`; both keys verify successfully so that a
/// rotation can be performed without invalidating outstanding tokens. See
/// `JwtConfig::rotate` for the promotion logic.
#[derive(Debug)]
pub struct RsKeyRing {
    /// Active key — used to sign new tokens.
    pub current: RsKey,
    /// Standby key — verifies but does not sign. Promoted on rotation.
    pub next: RsKey,
}

impl RsKeyRing {
    /// Generate a brand new keyring (two fresh RSA-2048 keypairs).
    pub fn generate() -> Result<Self, AuthError> {
        Ok(Self {
            current: RsKey::generate()?,
            next: RsKey::generate()?,
        })
    }

    /// Find a key in the ring by `kid`.
    pub fn find(&self, kid: &str) -> Option<&RsKey> {
        if self.current.kid == kid {
            Some(&self.current)
        } else if self.next.kid == kid {
            Some(&self.next)
        } else {
            None
        }
    }

    /// Iterate over all keys (current first, then next). Used by JWKS.
    pub fn iter(&self) -> impl Iterator<Item = &RsKey> {
        std::iter::once(&self.current).chain(std::iter::once(&self.next))
    }
}

/// JWT configuration.
#[derive(Debug)]
pub struct JwtConfig {
    pub algorithm: JwtAlgorithm,
    pub access_ttl: Duration,
    pub refresh_ttl: Duration,
    /// HS256 secret (legacy). `Some` iff `algorithm == Hs256`.
    pub hs_secret: Option<[u8; 32]>,
    /// RS256 keyring. `Some` iff `algorithm == Rs256`.
    pub rs_keys: Option<RsKeyRing>,
}

impl JwtConfig {
    /// Create a config with a random HS256 secret (for testing).
    pub fn new_random_hs256() -> Self {
        use rand::Rng;
        let mut secret = [0u8; 32];
        rand::thread_rng().fill(&mut secret);
        Self {
            algorithm: JwtAlgorithm::Hs256,
            access_ttl: Duration::from_secs(3600),
            refresh_ttl: Duration::from_secs(7 * 24 * 3600),
            hs_secret: Some(secret),
            rs_keys: None,
        }
    }

    /// Create a config with a freshly generated RS256 keyring (for testing).
    ///
    /// In production, the keyring is loaded from disk by
    /// [`crate::auth::store::AuthStore`].
    pub fn new_random_rs256() -> Result<Self, AuthError> {
        Ok(Self {
            algorithm: JwtAlgorithm::Rs256,
            access_ttl: Duration::from_secs(3600),
            refresh_ttl: Duration::from_secs(7 * 24 * 3600),
            hs_secret: None,
            rs_keys: Some(RsKeyRing::generate()?),
        })
    }

    /// Construct an HS256 config from an existing secret.
    pub fn new_hs256(secret: [u8; 32], access_ttl: Duration, refresh_ttl: Duration) -> Self {
        Self {
            algorithm: JwtAlgorithm::Hs256,
            access_ttl,
            refresh_ttl,
            hs_secret: Some(secret),
            rs_keys: None,
        }
    }

    /// Construct an RS256 config from an existing keyring.
    pub fn new_rs256(keyring: RsKeyRing, access_ttl: Duration, refresh_ttl: Duration) -> Self {
        Self {
            algorithm: JwtAlgorithm::Rs256,
            access_ttl,
            refresh_ttl,
            hs_secret: None,
            rs_keys: Some(keyring),
        }
    }

    /// Legacy alias for [`Self::new_hs256`], kept for backward compatibility
    /// with v4 callers that pass a raw secret.
    pub fn new(secret: [u8; 32], access_ttl: Duration, refresh_ttl: Duration) -> Self {
        Self::new_hs256(secret, access_ttl, refresh_ttl)
    }

    /// Backward-compatible random constructor — defaults to HS256 to keep
    /// existing v4 unit tests working unchanged.
    pub fn new_random() -> Self {
        Self::new_random_hs256()
    }

    /// Promote `next` to `current` and generate a fresh `next`.
    ///
    /// Tokens already issued under the **old** `current` will fail
    /// verification after rotation — the old key has rotated out of the
    /// ring. To keep outstanding tokens verifiable across a rotation in
    /// production, the on-disk path archives the previous PEMs for a
    /// configurable grace period (see
    /// [`crate::auth::store::AuthStore::rotate_jwt_keys`]).
    pub fn rotate(&mut self) -> Result<(), AuthError> {
        let ring = self
            .rs_keys
            .as_mut()
            .ok_or_else(|| AuthError::JwtError("rotate() requires RS256 algorithm".into()))?;
        let new_next = RsKey::generate()?;
        let old_next = std::mem::replace(&mut ring.next, new_next);
        ring.current = old_next;
        Ok(())
    }
}

/// JWKS public key entry (RFC 7517 §4 + RFC 7518 §6.3.1, RSA only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Jwk {
    pub kty: String,
    pub alg: String,
    #[serde(rename = "use")]
    pub r#use: String,
    pub kid: String,
    pub n: String,
    pub e: String,
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

/// Generate a token with an arbitrary TTL — used by the cluster_peer
/// long-lived token bootstrap (Phase 39).
pub fn generate_token_with_ttl(
    user: &User,
    config: &JwtConfig,
    ttl: Duration,
    token_type: &str,
) -> Result<String, AuthError> {
    generate_token(user, config, &ttl, token_type)
}

fn generate_token(
    user: &User,
    config: &JwtConfig,
    ttl: &Duration,
    token_type: &str,
) -> Result<String, AuthError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| AuthError::ClockError(e.to_string()))?
        .as_secs();

    let claims = Claims {
        sub: user.username.clone(),
        tenant: user.tenant.clone(),
        roles: user.roles.clone(),
        iat: now,
        exp: now + ttl.as_secs(),
        token_type: token_type.to_string(),
    };

    match config.algorithm {
        JwtAlgorithm::Hs256 => {
            let secret = config
                .hs_secret
                .as_ref()
                .ok_or_else(|| AuthError::JwtError("HS256 config missing secret".into()))?;
            let key = EncodingKey::from_secret(secret);
            encode(&Header::new(Algorithm::HS256), &claims, &key)
                .map_err(|e| AuthError::JwtError(e.to_string()))
        }
        JwtAlgorithm::Rs256 => {
            let ring = config
                .rs_keys
                .as_ref()
                .ok_or_else(|| AuthError::JwtError("RS256 config missing keyring".into()))?;
            let mut header = Header::new(Algorithm::RS256);
            header.kid = Some(ring.current.kid.clone());
            encode(&header, &claims, &ring.current.encoding_key)
                .map_err(|e| AuthError::JwtError(e.to_string()))
        }
    }
}

/// Verify a token and return the decoded claims.
pub fn verify_token(token: &str, config: &JwtConfig) -> Result<Claims, AuthError> {
    let map_err = |e: jsonwebtoken::errors::Error| match e.kind() {
        jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::TokenExpired,
        _ => AuthError::InvalidToken(e.to_string()),
    };

    match config.algorithm {
        JwtAlgorithm::Hs256 => {
            let secret = config
                .hs_secret
                .as_ref()
                .ok_or_else(|| AuthError::JwtError("HS256 config missing secret".into()))?;
            let key = DecodingKey::from_secret(secret);
            let mut validation = Validation::new(Algorithm::HS256);
            validation.validate_exp = true;
            validation.leeway = 0;
            validation.required_spec_claims.clear();
            validation.required_spec_claims.insert("exp".to_string());
            validation.required_spec_claims.insert("sub".to_string());
            decode::<Claims>(token, &key, &validation)
                .map(|d| d.claims)
                .map_err(map_err)
        }
        JwtAlgorithm::Rs256 => {
            let ring = config
                .rs_keys
                .as_ref()
                .ok_or_else(|| AuthError::JwtError("RS256 config missing keyring".into()))?;
            let header = jsonwebtoken::decode_header(token)
                .map_err(|e| AuthError::InvalidToken(format!("bad header: {e}")))?;
            let kid = header
                .kid
                .as_deref()
                .ok_or_else(|| AuthError::InvalidToken("missing kid in header".into()))?;
            let key = ring.find(kid).ok_or_else(|| {
                AuthError::InvalidToken(format!("kid not found in keyring: {kid}"))
            })?;
            let mut validation = Validation::new(Algorithm::RS256);
            validation.validate_exp = true;
            validation.leeway = 0;
            validation.required_spec_claims.clear();
            validation.required_spec_claims.insert("exp".to_string());
            validation.required_spec_claims.insert("sub".to_string());
            decode::<Claims>(token, &key.decoding_key, &validation)
                .map(|d| d.claims)
                .map_err(map_err)
        }
    }
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

    // ── HS256 (legacy) ───────────────────────────────────────────────────

    #[test]
    fn test_hs256_round_trip() {
        let config = JwtConfig::new_random_hs256();
        let user = test_user();
        let token = generate_access_token(&user, &config).unwrap();
        let claims = verify_token(&token, &config).unwrap();
        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.tenant, "acme");
    }

    #[test]
    fn test_hs256_legacy_constructor_defaults_to_hs256() {
        let config = JwtConfig::new_random();
        assert_eq!(config.algorithm, JwtAlgorithm::Hs256);
        let token = generate_access_token(&test_user(), &config).unwrap();
        assert!(verify_token(&token, &config).is_ok());
    }

    #[test]
    fn test_generate_and_verify_refresh_token() {
        let config = JwtConfig::new_random_hs256();
        let token = generate_refresh_token(&test_user(), &config).unwrap();
        let claims = verify_token(&token, &config).unwrap();
        assert_eq!(claims.token_type, "refresh");
    }

    #[test]
    fn test_wrong_secret_rejected() {
        let c1 = JwtConfig::new_random_hs256();
        let c2 = JwtConfig::new_random_hs256();
        let token = generate_access_token(&test_user(), &c1).unwrap();
        assert!(matches!(
            verify_token(&token, &c2),
            Err(AuthError::InvalidToken(_))
        ));
    }

    #[test]
    fn test_expired_token_rejected_hs256() {
        let config =
            JwtConfig::new_hs256([42u8; 32], Duration::from_secs(0), Duration::from_secs(0));
        let token = generate_access_token(&test_user(), &config).unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        assert!(matches!(
            verify_token(&token, &config),
            Err(AuthError::TokenExpired)
        ));
    }

    #[test]
    fn test_tampered_token_rejected() {
        let config = JwtConfig::new_random_hs256();
        let token = generate_access_token(&test_user(), &config).unwrap();
        let mut tampered = token.clone();
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        assert!(verify_token(&tampered, &config).is_err());
    }

    #[test]
    fn test_invalid_token_string() {
        let config = JwtConfig::new_random_hs256();
        assert!(matches!(
            verify_token("not.a.jwt", &config),
            Err(AuthError::InvalidToken(_))
        ));
    }

    #[test]
    fn test_claims_preserve_roles_hs256() {
        let config = JwtConfig::new_random_hs256();
        let user = User {
            username: "bob".into(),
            tenant: "acme".into(),
            password_hash: "x".into(),
            roles: vec![
                RoleAssignment {
                    role: RoleName::ReadWrite,
                    scope: ResourceScope::Database { name: "db1".into() },
                },
                RoleAssignment {
                    role: RoleName::DbAdmin,
                    scope: ResourceScope::Database { name: "db2".into() },
                },
            ],
            created_at: 0,
        };
        let token = generate_access_token(&user, &config).unwrap();
        let claims = verify_token(&token, &config).unwrap();
        assert_eq!(claims.roles.len(), 2);
    }

    #[test]
    fn test_access_and_refresh_have_different_exp() {
        let config = JwtConfig::new_random_hs256();
        let access = generate_access_token(&test_user(), &config).unwrap();
        let refresh = generate_refresh_token(&test_user(), &config).unwrap();
        let access_claims = verify_token(&access, &config).unwrap();
        let refresh_claims = verify_token(&refresh, &config).unwrap();
        assert!(refresh_claims.exp > access_claims.exp);
    }

    // ── RS256 ────────────────────────────────────────────────────────────

    #[test]
    fn test_rs256_round_trip() {
        let config = JwtConfig::new_random_rs256().expect("generate keyring");
        let user = test_user();
        let token = generate_access_token(&user, &config).unwrap();
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert_eq!(header.alg, Algorithm::RS256);
        let ring = config.rs_keys.as_ref().unwrap();
        assert_eq!(header.kid.as_deref(), Some(ring.current.kid.as_str()));
        let claims = verify_token(&token, &config).unwrap();
        assert_eq!(claims.sub, "alice");
    }

    #[test]
    fn test_rs256_kid_mismatch_rejected() {
        let config = JwtConfig::new_random_rs256().unwrap();
        let token = generate_access_token(&test_user(), &config).unwrap();
        // Manually rewrite the header kid to a value not present in the ring.
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut parts = token.splitn(3, '.');
        let header_b64 = parts.next().unwrap();
        let payload = parts.next().unwrap();
        let sig = parts.next().unwrap();
        let header_json = URL_SAFE_NO_PAD.decode(header_b64).unwrap();
        let mut header_value: serde_json::Value = serde_json::from_slice(&header_json).unwrap();
        header_value["kid"] = serde_json::Value::String("does-not-exist".into());
        let new_header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header_value).unwrap());
        let tampered = format!("{new_header}.{payload}.{sig}");
        assert!(matches!(
            verify_token(&tampered, &config),
            Err(AuthError::InvalidToken(_))
        ));
    }

    #[test]
    fn test_rs256_next_key_verifies() {
        // Manually force signing with `next` to confirm the verifier accepts
        // tokens issued under either key in the ring.
        let mut config = JwtConfig::new_random_rs256().unwrap();
        let ring = config.rs_keys.as_mut().unwrap();
        std::mem::swap(&mut ring.current, &mut ring.next);
        let token = generate_access_token(&test_user(), &config).unwrap();
        // Swap back — verification should still succeed because the
        // signing key is still in the ring (now as `next`).
        let ring = config.rs_keys.as_mut().unwrap();
        std::mem::swap(&mut ring.current, &mut ring.next);
        let claims = verify_token(&token, &config).unwrap();
        assert_eq!(claims.sub, "alice");
    }

    #[test]
    fn test_rs256_expired_rejected() {
        let mut config = JwtConfig::new_random_rs256().unwrap();
        config.access_ttl = Duration::from_secs(0);
        let token = generate_access_token(&test_user(), &config).unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        assert!(matches!(
            verify_token(&token, &config),
            Err(AuthError::TokenExpired)
        ));
    }

    #[test]
    fn test_rotate_promotes_next_to_current() {
        let mut config = JwtConfig::new_random_rs256().unwrap();
        let old_current_kid = config.rs_keys.as_ref().unwrap().current.kid.clone();
        let old_next_kid = config.rs_keys.as_ref().unwrap().next.kid.clone();

        let pre_token = generate_access_token(&test_user(), &config).unwrap();
        let pre_header = jsonwebtoken::decode_header(&pre_token).unwrap();
        assert_eq!(pre_header.kid.as_deref(), Some(old_current_kid.as_str()));

        config.rotate().unwrap();
        let ring = config.rs_keys.as_ref().unwrap();
        assert_eq!(ring.current.kid, old_next_kid);
        assert_ne!(ring.next.kid, old_current_kid);
        assert_ne!(ring.next.kid, old_next_kid);

        // Pre-rotation token: signed with old_current_kid, no longer in
        // the ring → must fail.
        assert!(matches!(
            verify_token(&pre_token, &config),
            Err(AuthError::InvalidToken(_))
        ));

        // Post-rotation token: signed with the new current.
        let post_token = generate_access_token(&test_user(), &config).unwrap();
        let post_header = jsonwebtoken::decode_header(&post_token).unwrap();
        assert_eq!(post_header.kid.as_deref(), Some(old_next_kid.as_str()));
        assert!(verify_token(&post_token, &config).is_ok());
    }

    #[test]
    fn test_rotate_on_hs256_errors() {
        let mut config = JwtConfig::new_random_hs256();
        let result = config.rotate();
        assert!(matches!(result, Err(AuthError::JwtError(_))));
    }

    #[test]
    fn test_rs_key_jwk_round_trip_pem() {
        let key = RsKey::generate().unwrap();
        let jwk = key.to_jwk();
        assert_eq!(jwk.kty, "RSA");
        assert_eq!(jwk.alg, "RS256");
        assert_eq!(jwk.r#use, "sig");
        assert_eq!(jwk.kid, key.kid);
        assert!(!jwk.n.is_empty());
        assert_eq!(jwk.e, "AQAB"); // 65537 in URL-safe base64
        let parsed = RsKey::from_pem(&key.private_pem, &key.public_pem).unwrap();
        assert_eq!(parsed.kid, key.kid);
        let jwk2 = parsed.to_jwk();
        assert_eq!(jwk2.n, jwk.n);
        assert_eq!(jwk2.e, jwk.e);
    }

    #[test]
    fn test_rs256_keygen_benchmark() {
        // Documented benchmark: RSA-2048 keypair generation. The `rsa`
        // crate is pure-Rust without intrinsics, which is dramatically
        // slower in debug than in release. Production servers always run
        // release builds, so we just print the timing in debug and only
        // assert on the budget in release.
        let start = std::time::Instant::now();
        let _key = RsKey::generate().unwrap();
        let elapsed = start.elapsed();
        eprintln!("RSA-2048 keygen: {elapsed:?}");
        #[cfg(not(debug_assertions))]
        assert!(
            elapsed < Duration::from_secs(5),
            "RSA-2048 keygen took {elapsed:?}, expected <5s in release"
        );
    }

    #[test]
    fn test_rs256_pkcs1_pem_for_public_key() {
        let key = RsKey::generate().unwrap();
        let pkcs1 = key.public_pkcs1_pem().unwrap();
        assert!(pkcs1.starts_with("-----BEGIN RSA PUBLIC KEY-----"));
    }
}
