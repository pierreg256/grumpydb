//! End-to-end tests for the observability HTTP server (`/healthz`,
//! `/readyz`, `/metrics`).

use grumpydb_client::GrumpyClient;
use grumpydb_testing::TestServer;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Minimal raw HTTP/1.1 GET. Returns `(status, body)`.
async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("tcp connect");
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[tokio::test]
async fn test_e2e_health_endpoints() {
    let server = TestServer::spawn().await;

    // /healthz: always 200 + "ok".
    let (status, body) = http_get(server.http_addr, "/healthz").await;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body, "ok");

    // /readyz: 200 once the TCP listener is bound (TestServer::spawn waits
    // for that), so this should be ready immediately.
    let (status, _) = http_get(server.http_addr, "/readyz").await;
    assert_eq!(status, 200);

    // /metrics: 200 with Prometheus exposition body containing at least
    // one of our described metrics.
    let (status, body) = http_get(server.http_addr, "/metrics").await;
    assert_eq!(status, 200);
    assert!(
        body.contains("grumpydb_connections_active"),
        "metrics body missing expected gauge: {body}"
    );

    // Unknown paths return 404.
    let (status, _) = http_get(server.http_addr, "/no-such-route").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn test_e2e_metrics_record_login_command() {
    let server = TestServer::spawn().await;
    let mut client = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
        .await
        .expect("connect");
    client
        .login(
            server.admin_tenant,
            server.admin_user,
            &server.admin_password,
        )
        .await
        .expect("login");

    // Issue a couple of commands so the metric gets a non-zero sample.
    client
        .create_database("metrics_db")
        .await
        .expect("create db");
    let mut db = client.database("metrics_db").await.expect("use");
    db.create_collection("c").await.expect("create coll");

    // Give the metrics recorder a brief moment in case histogram observations
    // are sampled asynchronously by the exporter.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (status, body) = http_get(server.http_addr, "/metrics").await;
    assert_eq!(status, 200);

    // The exposition format includes `name{labels} value`. Match on the
    // LOGIN counter rather than relying on label ordering.
    let login_ok_present = body.lines().filter(|l| !l.starts_with('#')).any(|l| {
        l.starts_with("grumpydb_commands_total")
            && l.contains("cmd=\"LOGIN\"")
            && l.contains("result=\"ok\"")
    });
    assert!(
        login_ok_present,
        "missing LOGIN ok counter sample in metrics body:\n{body}"
    );
}
