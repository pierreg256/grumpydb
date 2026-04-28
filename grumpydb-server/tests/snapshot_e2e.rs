//! End-to-end snapshot/restore test against the real `grumpydb-server` binary.
//!
//! Strategy:
//! 1. Spawn a server, populate a database with a few documents.
//! 2. Stop the server cleanly (kill — server flushes are explicit so we
//!    issue a flush via the compact API beforehand to be safe).
//! 3. Call `snapshot::snapshot()` directly on the data dir → archive.
//! 4. Call `snapshot::restore()` to extract into a fresh empty dir.
//! 5. Spawn a NEW server pointed at the restored dir using the SAME admin
//!    password (which is preserved in `_auth/`). Verify all docs still read
//!    back correctly.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use grumpydb_client::GrumpyClient;
use grumpydb_server::snapshot::{self, Location, SnapshotOptions};
use grumpydb_testing::TestServer;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn test_e2e_snapshot_then_restore() {
    let server = TestServer::spawn().await;

    // ── Populate ────────────────────────────────────────────────
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
    client.create_database("snap_db").await.expect("create db");
    let mut db = client.database("snap_db").await.expect("use db");
    db.create_collection("docs").await.expect("create coll");

    let mut keys: Vec<(Uuid, serde_json::Value)> = Vec::new();
    for i in 0..50_i64 {
        let key = Uuid::new_v4();
        let doc = json!({"i": i, "name": format!("doc-{i}")});
        db.insert("docs", key, &doc).await.expect("insert");
        keys.push((key, doc));
    }
    // Flush so the data.db is fully written to disk; the snapshot then
    // captures a self-contained, recoverable state.
    db.flush().await.expect("flush");
    drop(db);
    drop(client);

    // ── Quiesce: stop the server so file copies are clean ──────
    let mut server = server;
    let admin_password = server.admin_password.clone();
    let original_data_dir = server.data_dir.clone();
    server.crash().await;
    // Tiny grace window so that any lingering FD is released.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ── Snapshot ────────────────────────────────────────────────
    let archive_dir = tempfile::TempDir::new().expect("tempdir");
    let archive_path = archive_dir.path().join("snap.tar.gz");
    snapshot::snapshot(
        &SnapshotOptions {
            data_dir: original_data_dir.clone(),
            force: false,
        },
        &Location::Local(archive_path.clone()),
    )
    .await
    .expect("snapshot");
    assert!(archive_path.exists());

    // ── Restore into a fresh empty dir ─────────────────────────
    let restored = tempfile::TempDir::new().expect("tempdir");
    snapshot::restore(
        &SnapshotOptions {
            data_dir: restored.path().to_path_buf(),
            force: false,
        },
        &Location::Local(archive_path),
    )
    .await
    .expect("restore");

    // ── Spawn a new server on the restored dir ──────────────────
    let new_server = spawn_server_on(restored.path().to_path_buf(), admin_password.clone()).await;

    // ── Verify every doc is intact ─────────────────────────────
    let mut new_client = GrumpyClient::connect("127.0.0.1", new_server.addr.port(), false)
        .await
        .expect("connect new server");
    new_client
        .login("_system", "admin", &admin_password)
        .await
        .expect("login new server");
    let mut db = new_client.database("snap_db").await.expect("use db");
    for (key, expected) in &keys {
        let got = db.get("docs", key).await.expect("get");
        assert_eq!(got.as_ref(), Some(expected), "doc mismatch for {key}");
    }

    drop(db);
    drop(new_client);
    drop(new_server); // kills the server
}

/// Spawn the server binary on the given data directory, reusing the
/// existing admin password (so no `--bootstrap-password` is needed).
async fn spawn_server_on(data_dir: PathBuf, _admin_password: String) -> RestoredServer {
    let bin = locate_server_binary();
    let port = pick_free_port();
    let http_port = pick_free_port();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let http_addr: std::net::SocketAddr = format!("127.0.0.1:{http_port}").parse().unwrap();

    let mut cmd = Command::new(&bin);
    cmd.arg("--data")
        .arg(&data_dir)
        .arg("--no-tls")
        .arg("--bind")
        .arg(addr.to_string())
        .arg("--http-bind")
        .arg(http_addr.to_string())
        .arg("--log-format")
        .arg("text")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut process = cmd.spawn().expect("spawn restored server");

    // Wait for the listener.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return RestoredServer {
                addr,
                _http_addr: http_addr,
                process,
            };
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Reap the orphan before panicking so the test environment stays clean.
    let _ = process.kill();
    let _ = process.wait();
    panic!("restored server did not become ready");
}

struct RestoredServer {
    addr: std::net::SocketAddr,
    _http_addr: std::net::SocketAddr,
    process: std::process::Child,
}

impl Drop for RestoredServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn pick_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn locate_server_binary() -> PathBuf {
    if let Ok(p) = std::env::var("GRUMPYDB_SERVER_BIN") {
        let path = PathBuf::from(p);
        if path.exists() {
            return path;
        }
    }
    let bin_name = if cfg!(windows) {
        "grumpydb-server.exe"
    } else {
        "grumpydb-server"
    };
    let mut current: &std::path::Path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    loop {
        let target = current.join("target");
        if target.is_dir() {
            for profile in ["debug", "release"] {
                let candidate = target.join(profile).join(bin_name);
                if candidate.exists() {
                    return candidate;
                }
            }
        }
        current = current.parent().expect("walked above filesystem root");
    }
}
