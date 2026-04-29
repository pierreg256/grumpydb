//! End-to-end tests for the JWKS endpoint (Phase 39 — RS256 JWT + JWKS).
//!
//! These spin up the real `grumpydb-server` binary and verify that a token
//! issued by the server is verifiable using only the public key advertised
//! at `/.well-known/jwks.json`.

use grumpydb_client::GrumpyClient;
use grumpydb_protocol::Response;
use grumpydb_testing::TestServer;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Minimal JWKS shape (only the fields we care about).
#[derive(Debug, Deserialize)]
struct JwksDocument {
    keys: Vec<JwkEntry>,
}

#[derive(Debug, Deserialize)]
struct JwkEntry {
    kty: String,
    alg: String,
    #[serde(rename = "use")]
    use_field: String,
    kid: String,
    n: String,
    e: String,
}

async fn fetch_jwks(addr: std::net::SocketAddr) -> JwksDocument {
    let mut stream = TcpStream::connect(addr).await.expect("connect http");
    stream
        .write_all(
            b"GET /.well-known/jwks.json HTTP/1.1\r\n\
              Host: localhost\r\n\
              Connection: close\r\n\
              \r\n",
        )
        .await
        .expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .expect("response body");
    serde_json::from_str(body).expect("parse jwks json")
}

#[tokio::test]
async fn test_e2e_jwks_endpoint_serves_keys() {
    // Default config = RS256, so JWKS must contain at least one RSA key.
    let server = TestServer::spawn().await;
    let jwks = fetch_jwks(server.http_addr).await;
    assert!(
        !jwks.keys.is_empty(),
        "default server should expose at least one RSA key in JWKS"
    );
    for k in &jwks.keys {
        assert_eq!(k.kty, "RSA");
        assert_eq!(k.alg, "RS256");
        assert_eq!(k.use_field, "sig");
        assert!(!k.kid.is_empty());
        assert!(!k.n.is_empty());
        assert!(!k.e.is_empty());
    }
}

#[tokio::test]
async fn test_e2e_token_verifiable_with_jwks_key() {
    // Login → obtain access token → fetch JWKS → verify token with the JWKS
    // public key alone (no shared secret needed).
    let server = TestServer::spawn().await;

    let mut client = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
        .await
        .expect("connect tcp");
    let resp = client
        .raw_execute(&format!(
            "LOGIN {} {} {}",
            server.admin_tenant, server.admin_user, server.admin_password
        ))
        .await
        .expect("login");
    let access = match resp {
        Response::Ok(s) if s.starts_with("TOKEN ") => {
            s[6..].split(' ').next().unwrap_or("").to_string()
        }
        other => panic!("unexpected login response: {other:?}"),
    };
    assert!(!access.is_empty());

    // Pull JWKS and find the kid embedded in the token header.
    let jwks = fetch_jwks(server.http_addr).await;
    assert!(!jwks.keys.is_empty(), "RS256 JWKS must be non-empty");

    let header = jsonwebtoken::decode_header(&access).expect("decode header");
    let kid = header.kid.expect("RS256 token must carry a kid");
    let jwk = jwks
        .keys
        .iter()
        .find(|k| k.kid == kid)
        .expect("token kid present in JWKS");

    // Build a DecodingKey from the JWK n / e components and verify.
    let decoding =
        DecodingKey::from_rsa_components(&jwk.n, &jwk.e).expect("DecodingKey::from_rsa_components");
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_exp = true;
    // Drop the default `aud` requirement — GrumpyDB tokens don't set it.
    validation.required_spec_claims.clear();
    validation.required_spec_claims.insert("exp".into());
    validation.required_spec_claims.insert("sub".into());

    let _claims = jsonwebtoken::decode::<serde_json::Value>(&access, &decoding, &validation)
        .expect("verify token via JWKS");
}

#[tokio::test]
async fn test_e2e_jwks_unauthenticated() {
    // No login required: the JWKS endpoint is the *public* keyset.
    let server = TestServer::spawn().await;
    // First request without ever connecting on the TCP port.
    let _ = fetch_jwks(server.http_addr).await;
    // Sleep a touch to make sure no other concurrent activity is required.
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Second request still works.
    let jwks = fetch_jwks(server.http_addr).await;
    assert!(!jwks.keys.is_empty());
}
