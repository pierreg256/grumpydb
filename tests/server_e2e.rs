//! End-to-end tests against the real `grumpydb-server` binary over TCP.
//!
//! Each test spawns its own `TestServer` so they may run in parallel.

use grumpydb_client::GrumpyClient;
use grumpydb_testing::TestServer;
use serde_json::json;
use uuid::Uuid;

async fn admin_client(server: &TestServer) -> GrumpyClient {
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
    client
}

#[tokio::test]
async fn test_e2e_login_and_whoami() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    let info = client.whoami().await.expect("whoami");
    assert!(info.contains("admin"), "whoami missing user: {info}");
    assert!(info.contains("_system"), "whoami missing tenant: {info}");
}

#[tokio::test]
async fn test_e2e_create_database_and_collection() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client.create_database("test_db").await.expect("create db");

    let dbs = client.list_databases().await.expect("list");
    assert!(dbs.iter().any(|d| d == "test_db"), "missing db: {dbs:?}");

    let mut db = client.database("test_db").await.expect("use");
    db.create_collection("users").await.expect("create coll");

    let cols = db.list_collections().await.expect("list cols");
    assert!(cols.iter().any(|c| c == "users"), "missing coll: {cols:?}");
}

#[tokio::test]
async fn test_e2e_crud_full_cycle() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client.create_database("crud_db").await.expect("create db");
    let mut db = client.database("crud_db").await.expect("use");
    db.create_collection("docs").await.expect("create coll");

    let key = Uuid::new_v4();
    let v1 = json!({ "name": "alice", "age": 30 });
    db.insert("docs", key, &v1).await.expect("insert");

    let got = db.get("docs", &key).await.expect("get");
    assert_eq!(got, Some(v1.clone()));

    let v2 = json!({ "name": "alice", "age": 31 });
    db.update("docs", &key, &v2).await.expect("update");
    let got = db.get("docs", &key).await.expect("get after update");
    assert_eq!(got, Some(v2));

    db.delete("docs", &key).await.expect("delete");
    let got = db.get("docs", &key).await.expect("get after delete");
    assert_eq!(got, None);
}

#[tokio::test]
async fn test_e2e_index_query() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client.create_database("idx_db").await.expect("create db");
    let mut db = client.database("idx_db").await.expect("use");
    db.create_collection("people").await.expect("create coll");
    db.create_index("people", "age_idx", "age")
        .await
        .expect("create idx");

    for age in [25_i64, 30, 35, 40, 45] {
        let key = Uuid::new_v4();
        let doc = json!({ "name": format!("p{age}"), "age": age });
        db.insert("people", key, &doc).await.expect("insert");
    }

    let exact = db
        .query("people", "age_idx", &json!(35))
        .await
        .expect("query");
    assert_eq!(exact.len(), 1, "exact age=35: {exact:?}");
    let (_k, v) = &exact[0];
    assert_eq!(v.get("age"), Some(&json!(35)));

    let indexes = db.list_indexes("people").await.expect("list indexes");
    assert!(
        indexes.iter().any(|i| i == "age_idx"),
        "indexes missing age_idx: {indexes:?}"
    );
}

#[tokio::test]
async fn test_e2e_count() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client.create_database("count_db").await.expect("create db");
    let mut db = client.database("count_db").await.expect("use");
    db.create_collection("items").await.expect("create coll");

    let n: i64 = 25;
    for i in 0..n {
        let key = Uuid::new_v4();
        db.insert("items", key, &json!({ "i": i }))
            .await
            .expect("insert");
    }

    let count = db.count("items").await.expect("count");
    assert_eq!(count, n);
}

#[tokio::test]
async fn test_e2e_token_refresh() {
    let server = TestServer::spawn().await;

    // Bypass the high-level `login()` so we can read the raw
    // "TOKEN <access> <refresh>" payload from the LOGIN response.
    let mut client = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
        .await
        .expect("connect");
    let resp = client
        .raw_execute(&format!(
            "LOGIN {} {} {}",
            server.admin_tenant, server.admin_user, server.admin_password
        ))
        .await
        .expect("login raw");
    let (_access, refresh) = match resp {
        grumpydb_protocol::Response::Ok(msg) if msg.starts_with("TOKEN ") => {
            let rest = &msg[6..];
            let mut it = rest.splitn(2, ' ');
            let a = it.next().unwrap_or("").to_string();
            let r = it.next().unwrap_or("").to_string();
            (a, r)
        }
        other => panic!("unexpected login response: {other:?}"),
    };
    assert!(!refresh.is_empty(), "refresh token must be present");

    let resp = client
        .raw_execute(&format!("REFRESH {refresh}"))
        .await
        .expect("refresh");
    let new_access = match resp {
        grumpydb_protocol::Response::Ok(msg) if msg.starts_with("TOKEN ") => msg[6..].to_string(),
        other => panic!("unexpected refresh response: {other:?}"),
    };
    assert!(!new_access.is_empty());

    // Use the new token on a brand new connection.
    let mut fresh = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
        .await
        .expect("connect 2");
    let resp = fresh
        .raw_execute(&format!("TOKEN {new_access}"))
        .await
        .expect("token");
    assert!(
        matches!(resp, grumpydb_protocol::Response::Ok(_)),
        "TOKEN response was: {resp:?}"
    );
    let info = match fresh.raw_execute("WHOAMI").await.expect("whoami") {
        grumpydb_protocol::Response::Ok(s) => s,
        other => panic!("unexpected WHOAMI: {other:?}"),
    };
    assert!(info.contains("admin"));
}

#[tokio::test]
async fn test_e2e_invalid_credentials() {
    let server = TestServer::spawn().await;
    let mut client = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
        .await
        .expect("connect");
    let err = client
        .login(server.admin_tenant, server.admin_user, "definitely-wrong")
        .await
        .expect_err("login should fail");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("invalid"),
        "expected invalid credentials error, got: {msg}"
    );
}

#[tokio::test]
async fn test_e2e_unauthorized_command() {
    let server = TestServer::spawn().await;
    let mut client = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
        .await
        .expect("connect");
    let key = Uuid::new_v4();
    let resp = client
        .raw_execute(&format!("INSERT users {key} {{}}"))
        .await
        .expect("send insert");
    assert!(
        matches!(resp, grumpydb_protocol::Response::Error(_)),
        "unauthenticated INSERT should error, got: {resp:?}"
    );
}

#[tokio::test]
async fn test_e2e_topology_returns_json_snapshot() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;

    let resp = client.raw_execute("TOPOLOGY").await.expect("topology");
    let json = match resp {
        grumpydb_protocol::Response::Bulk(Some(s)) => s,
        other => panic!("unexpected TOPOLOGY response: {other:?}"),
    };
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid topology JSON");
    assert!(v.get("cluster_id").is_some());
    assert!(v.get("local_node_id").is_some());
    assert_eq!(v.get("n"), Some(&serde_json::json!(1)));
}

#[tokio::test]
async fn test_e2e_v5_rejects_non_default_concerns() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client.create_database("wc_db").await.expect("create db");

    let use_resp = client.raw_execute("USE wc_db").await.expect("use");
    assert!(
        matches!(use_resp, grumpydb_protocol::Response::Ok(_)),
        "USE failed: {use_resp:?}"
    );
    let create_coll = client
        .raw_execute("CREATE COLLECTION users")
        .await
        .expect("create collection");
    assert!(
        matches!(create_coll, grumpydb_protocol::Response::Ok(_)),
        "CREATE COLLECTION failed: {create_coll:?}"
    );

    let key = Uuid::new_v4();
    let resp = client
        .raw_execute(&format!(
            "WRITE_CONCERN W=2 INSERT users {key} {{\"name\":\"alice\"}}"
        ))
        .await
        .expect("write concern insert");

    match resp {
        grumpydb_protocol::Response::Error(msg) => {
            assert!(
                msg.contains("v5 only supports R=1, W=1"),
                "unexpected error: {msg}"
            );
        }
        other => panic!("expected error for W=2, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_e2e_snapshot_hlc_exposed_to_clients() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client.create_database("snap_db").await.expect("create db");

    let use_resp = client.raw_execute("USE snap_db").await.expect("use");
    assert!(
        matches!(use_resp, grumpydb_protocol::Response::Ok(_)),
        "USE failed: {use_resp:?}"
    );

    let first = match client
        .raw_execute("SNAPSHOT_HLC")
        .await
        .expect("snapshot 1")
    {
        grumpydb_protocol::Response::Integer(n) => n,
        other => panic!("unexpected SNAPSHOT_HLC response: {other:?}"),
    };
    assert!(first > 0, "SNAPSHOT_HLC must be positive, got {first}");

    let second = match client
        .raw_execute("SNAPSHOT_HLC")
        .await
        .expect("snapshot 2")
    {
        grumpydb_protocol::Response::Integer(n) => n,
        other => panic!("unexpected SNAPSHOT_HLC response: {other:?}"),
    };
    assert!(
        second >= first,
        "SNAPSHOT_HLC must be monotonic: {first} -> {second}"
    );
}

#[tokio::test]
async fn test_e2e_rust_client_connect_cluster_seed_fallback() {
    let server = TestServer::spawn().await;
    let seed_ok = format!("127.0.0.1:{}", server.addr.port());

    let mut client = GrumpyClient::connect_cluster(&["127.0.0.1:1", &seed_ok], false)
        .await
        .expect("connect cluster fallback");
    client
        .login(
            server.admin_tenant,
            server.admin_user,
            &server.admin_password,
        )
        .await
        .expect("login");

    let info = client.whoami().await.expect("whoami");
    assert!(info.contains("admin"), "whoami missing user: {info}");
}

#[tokio::test]
async fn test_e2e_rust_client_topology_cache_after_login() {
    let server = TestServer::spawn().await;
    let seed = format!("127.0.0.1:{}", server.addr.port());

    let mut client = GrumpyClient::connect_cluster(&[&seed], false)
        .await
        .expect("connect cluster");
    client
        .login(
            server.admin_tenant,
            server.admin_user,
            &server.admin_password,
        )
        .await
        .expect("login");

    let cached = client
        .cached_topology()
        .expect("topology should be cached after login");
    assert_eq!(cached.n, 1);
    assert!(!cached.cluster_id.is_empty());

    let topo = client.topology().await.expect("topology");
    assert_eq!(topo.n, 1);
    assert!(!topo.peers.is_empty());
}
