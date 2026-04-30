use std::collections::HashMap;

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use serde_json::Value;

use crate::ClientError;

#[derive(Debug, Clone, Deserialize)]
struct JwksDocument {
    keys: Vec<Jwk>,
}

#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    kty: String,
    alg: Option<String>,
    #[serde(rename = "use")]
    use_: Option<String>,
    kid: String,
    n: String,
    e: String,
}

#[derive(Default)]
pub(crate) struct JwksCache {
    url: Option<String>,
    by_kid: HashMap<String, DecodingKey>,
}

impl JwksCache {
    pub(crate) fn set_url(&mut self, url: impl Into<String>) {
        self.url = Some(url.into());
        self.by_kid.clear();
    }

    pub(crate) fn configured(&self) -> bool {
        self.url.is_some()
    }

    pub(crate) async fn verify_access_token(&mut self, token: &str) -> Result<(), ClientError> {
        let header = decode_header(token)
            .map_err(|e| ClientError::Jwt(format!("invalid token header: {e}")))?;

        if header.alg != Algorithm::RS256 {
            return Err(ClientError::Jwt(format!(
                "unsupported JWT alg {:?}, expected RS256",
                header.alg
            )));
        }

        let kid = header
            .kid
            .ok_or_else(|| ClientError::Jwt("token header missing kid".into()))?;

        if !self.by_kid.contains_key(&kid) {
            self.refresh().await?;
        }

        if !self.by_kid.contains_key(&kid) {
            self.refresh().await?;
        }

        let key = self
            .by_kid
            .get(&kid)
            .ok_or_else(|| ClientError::Jwt(format!("kid '{kid}' not found in JWKS")))?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.required_spec_claims.insert("exp".to_string());

        decode::<Value>(token, key, &validation)
            .map_err(|e| ClientError::Jwt(format!("token verification failed: {e}")))?;
        Ok(())
    }

    async fn refresh(&mut self) -> Result<(), ClientError> {
        let url = self
            .url
            .clone()
            .ok_or_else(|| ClientError::Jwt("JWKS URL not configured".into()))?;

        let doc: JwksDocument = reqwest::get(&url)
            .await
            .map_err(|e| ClientError::Jwt(format!("JWKS fetch failed: {e}")))?
            .error_for_status()
            .map_err(|e| ClientError::Jwt(format!("JWKS HTTP error: {e}")))?
            .json()
            .await
            .map_err(|e| ClientError::Jwt(format!("invalid JWKS payload: {e}")))?;

        self.by_kid.clear();
        for key in doc.keys {
            if key.kty != "RSA" {
                continue;
            }
            if let Some(alg) = &key.alg
                && alg != "RS256"
            {
                continue;
            }
            if let Some(use_) = &key.use_
                && use_ != "sig"
            {
                continue;
            }

            let decoding = DecodingKey::from_rsa_components(&key.n, &key.e)
                .map_err(|e| ClientError::Jwt(format!("invalid RSA key in JWKS: {e}")))?;
            self.by_kid.insert(key.kid, decoding);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jwks_cache_not_configured_by_default() {
        let cache = JwksCache::default();
        assert!(!cache.configured());
    }

    #[test]
    fn test_jwks_cache_configured_after_url_set() {
        let mut cache = JwksCache::default();
        cache.set_url("http://127.0.0.1:8080/.well-known/jwks.json");
        assert!(cache.configured());
    }
}
