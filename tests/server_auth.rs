//! End-to-end auth/RBAC tests against the real `grumpydb-server` binary.

use grumpydb_client::GrumpyClient;
use grumpydb_protocol::Response;
use grumpydb_testing::TestServer;
use serde_json::json;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn test_e2e_expired_token_rejected() {
    // Spin up a server with a 1-second access token TTL via a config file.
    let cfg_dir = tempfile::TempDir::new().unwrap();
    let cfg_path = cfg_dir.path().join("grumpydb.toml");
    std::fs::write(
        &cfg_path,
        r#"
[auth]
access_token_ttl_secs = 1
refresh_token_ttl_secs = 60
"#,
    )
    .unwrap();
    let cfg_str = cfg_path.to_str().unwrap();
    let server = TestServer::spawn_with_extra_args(&["--config", cfg_str]).await;

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

    // Token still valid right now.
    client.whoami().await.expect("whoami while fresh");

    // Wait past the TTL.
    tokio::time::sleep(Duration::from_millis(2_500)).await;

    // The session should now reject any non-pre-auth command. WHOAMI is a
    // session-level command but the handler still requires authenticated
    // claims; verifying the token fails because it has expired. We use a new
    // connection + TOKEN command to probe expiration directly.
    let mut probe = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
        .await
        .expect("probe connect");

    // Get a fresh login (which yields a new pair of tokens), then sleep
    // well past the TTL boundary. Note: JWT `exp` has 1-second resolution,
    // so we sleep ≥ 2 × TTL to be sure that current time is strictly
    // greater than the encoded expiry second.
    let login_resp = probe
        .raw_execute(&format!(
            "LOGIN {} {} {}",
            server.admin_tenant, server.admin_user, server.admin_password
        ))
        .await
        .expect("login raw");
    let access = match login_resp {
        Response::Ok(msg) if msg.starts_with("TOKEN ") => {
            msg[6..].split(' ').next().unwrap_or("").to_string()
        }
        other => panic!("unexpected login: {other:?}"),
    };
    tokio::time::sleep(Duration::from_millis(2_500)).await;

    let mut second = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
        .await
        .expect("second connect");
    let resp = second
        .raw_execute(&format!("TOKEN {access}"))
        .await
        .expect("token");
    assert!(
        matches!(resp, Response::Error(_)),
        "expired TOKEN should error, got: {resp:?}"
    );
}

#[tokio::test]
async fn test_e2e_tampered_token_rejected() {
    let server = TestServer::spawn().await;
    let mut client = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
        .await
        .expect("connect");

    // Mangle a clearly-bogus token.
    let resp = client
        .raw_execute("TOKEN this.is.not-a-valid-jwt")
        .await
        .expect("token");
    assert!(
        matches!(resp, Response::Error(_)),
        "tampered TOKEN should error, got: {resp:?}"
    );

    // Also attempt with a real-looking but wrong-signature JWT.
    let resp = client
        .raw_execute("TOKEN eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJoYXgifQ.bogus_sig")
        .await
        .expect("token");
    assert!(
        matches!(resp, Response::Error(_)),
        "bogus-sig TOKEN should error, got: {resp:?}"
    );
}

#[tokio::test]
async fn test_e2e_role_enforcement() {
    let server = TestServer::spawn().await;

    // As admin: create user "alice" + grant read_only on database "demo".
    {
        let mut admin = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
            .await
            .expect("admin connect");
        admin
            .login(
                server.admin_tenant,
                server.admin_user,
                &server.admin_password,
            )
            .await
            .expect("admin login");

        admin.create_database("demo").await.expect("create demo");

        // Seed the collection on a SEPARATE connection so the admin's main
        // session retains no `USE` selection. This matters because the
        // resource grammar treats a bare `name` as a collection when a
        // database is currently selected, which would make a grant on
        // `demo` resolve to a collection-scoped grant instead of a
        // database-scoped one.
        {
            let mut seeder = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
                .await
                .expect("seeder connect");
            seeder
                .login(
                    server.admin_tenant,
                    server.admin_user,
                    &server.admin_password,
                )
                .await
                .expect("seeder login");
            let mut db = seeder.database("demo").await.expect("use demo");
            db.create_collection("docs").await.expect("create coll");
            db.insert("docs", Uuid::new_v4(), &json!({ "x": 1 }))
                .await
                .expect("seed");
        }

        // Create alice (in admin's tenant: _system).
        let resp = admin
            .raw_execute("CREATE USER alice supersecret123")
            .await
            .expect("create user");
        assert!(matches!(resp, Response::Ok(_)), "create user: {resp:?}");

        // Grant read_only on database "demo" (admin connection has no USE
        // active, so a bare name is parsed as a database scope).
        let resp = admin
            .raw_execute("GRANT read_only ON demo TO alice")
            .await
            .expect("grant");
        assert!(matches!(resp, Response::Ok(_)), "grant: {resp:?}");
    }

    // As alice: read works, write is denied.
    let mut alice = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
        .await
        .expect("alice connect");
    alice
        .login("_system", "alice", "supersecret123")
        .await
        .expect("alice login");
    let mut db = alice.database("demo").await.expect("alice use");

    // Reads must work.
    let count = db.count("docs").await.expect("count as alice");
    assert_eq!(count, 1);

    // Writes must be denied.
    let resp = alice
        .raw_execute(&format!("INSERT docs {} {}", Uuid::new_v4(), r#"{"y":2}"#))
        .await
        .expect("insert raw");
    match resp {
        Response::Error(msg) => {
            let lower = msg.to_lowercase();
            assert!(
                lower.contains("denied")
                    || lower.contains("permission")
                    || lower.contains("forbidden"),
                "expected access-denied error, got: {msg}"
            );
        }
        other => panic!("expected error response for read_only INSERT, got: {other:?}"),
    }
}
