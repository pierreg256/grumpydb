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
async fn test_e2e_v6_phase45_still_rejects_w_gt_1_until_ack_pipeline() {
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
                msg.contains("invalid consistency concerns")
                    || msg.contains("not enough live replicas"),
                "unexpected error: {msg}"
            );
        }
        other => panic!("expected error for W=2, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_e2e_rejects_r_gt_1_when_single_node_has_insufficient_replicas() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client.create_database("rc_db").await.expect("create db");

    let use_resp = client.raw_execute("USE rc_db").await.expect("use");
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
    let insert = client
        .raw_execute(&format!("INSERT users {key} {{\"name\":\"alice\"}}"))
        .await
        .expect("insert");
    assert!(
        matches!(insert, grumpydb_protocol::Response::Ok(_)),
        "INSERT failed: {insert:?}"
    );

    let resp = client
        .raw_execute(&format!("READ_CONCERN R=2 GET users {key}"))
        .await
        .expect("read concern get");
    match resp {
        grumpydb_protocol::Response::Error(msg) => {
            assert!(
                msg.contains("invalid consistency concerns")
                    || msg.contains("not enough live replicas")
                    || msg.contains("read quorum"),
                "unexpected error: {msg}"
            );
        }
        other => panic!("expected error for R=2 in single-node mode, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_e2e_database_level_consistency_defaults_and_override_precedence() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client
        .create_database("consistency_db")
        .await
        .expect("create db");

    let use_resp = client.raw_execute("USE consistency_db").await.expect("use");
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

    let set_resp = client
        .raw_execute(
            "ALTER DATABASE consistency_db SET CONSISTENCY READ_CONCERN R=2 WRITE_CONCERN W=2",
        )
        .await
        .expect("set consistency");
    assert!(
        matches!(set_resp, grumpydb_protocol::Response::Ok(_)),
        "ALTER DATABASE SET failed: {set_resp:?}"
    );

    let show_resp = client
        .raw_execute("SHOW DATABASE consistency_db CONSISTENCY")
        .await
        .expect("show consistency");
    let show_json = match show_resp {
        grumpydb_protocol::Response::Bulk(Some(s)) => s,
        other => panic!("unexpected SHOW response: {other:?}"),
    };
    let show_value: serde_json::Value = serde_json::from_str(&show_json).expect("valid show json");
    assert_eq!(show_value.get("database"), Some(&json!("consistency_db")));
    assert_eq!(show_value.get("read_concern"), Some(&json!(2)));
    assert_eq!(show_value.get("write_concern"), Some(&json!(2)));

    // Default W=2 applies to plain writes and fails on a single-node topology.
    let failing_key = Uuid::new_v4();
    let write_with_db_default = client
        .raw_execute(&format!(
            "INSERT users {failing_key} {{\"name\":\"blocked_by_default\"}}"
        ))
        .await
        .expect("insert with db default");
    assert!(
        matches!(write_with_db_default, grumpydb_protocol::Response::Error(_)),
        "expected write to fail due to default W=2, got: {write_with_db_default:?}"
    );

    // Per-request override must take precedence over DB default.
    let ok_key = Uuid::new_v4();
    let write_override = client
        .raw_execute(&format!(
            "WRITE_CONCERN W=1 INSERT users {ok_key} {{\"name\":\"override_ok\"}}"
        ))
        .await
        .expect("insert with override");
    assert!(
        matches!(write_override, grumpydb_protocol::Response::Ok(_)),
        "override write should succeed, got: {write_override:?}"
    );

    let read_with_db_default = client
        .raw_execute(&format!("GET users {ok_key}"))
        .await
        .expect("get with db default");
    assert!(
        matches!(read_with_db_default, grumpydb_protocol::Response::Error(_)),
        "expected read to fail due to default R=2, got: {read_with_db_default:?}"
    );

    let read_override = client
        .raw_execute(&format!("READ_CONCERN R=1 GET users {ok_key}"))
        .await
        .expect("get with override");
    assert!(
        matches!(read_override, grumpydb_protocol::Response::Bulk(Some(_))),
        "override read should succeed, got: {read_override:?}"
    );

    let reset_resp = client
        .raw_execute("ALTER DATABASE consistency_db RESET CONSISTENCY")
        .await
        .expect("reset consistency");
    assert!(
        matches!(reset_resp, grumpydb_protocol::Response::Ok(_)),
        "ALTER DATABASE RESET failed: {reset_resp:?}"
    );

    let show_after_reset = client
        .raw_execute("SHOW DATABASE consistency_db CONSISTENCY")
        .await
        .expect("show after reset");
    let reset_json = match show_after_reset {
        grumpydb_protocol::Response::Bulk(Some(s)) => s,
        other => panic!("unexpected SHOW response after reset: {other:?}"),
    };
    let reset_value: serde_json::Value =
        serde_json::from_str(&reset_json).expect("valid reset show json");
    assert_eq!(
        reset_value.get("read_concern"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(
        reset_value.get("write_concern"),
        Some(&serde_json::Value::Null)
    );
}

#[tokio::test]
async fn test_e2e_index_query_respects_db_read_concern_default_and_override() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client
        .create_database("idx_consistency_db")
        .await
        .expect("create db");

    let use_resp = client
        .raw_execute("USE idx_consistency_db")
        .await
        .expect("use");
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

    let create_idx = client
        .raw_execute("CREATE INDEX users name_idx name")
        .await
        .expect("create index");
    assert!(
        matches!(create_idx, grumpydb_protocol::Response::Ok(_)),
        "CREATE INDEX failed: {create_idx:?}"
    );

    let key = Uuid::new_v4();
    let insert = client
        .raw_execute(&format!(
            "INSERT users {key} {{\"name\":\"alice\",\"age\":30}}"
        ))
        .await
        .expect("insert");
    assert!(
        matches!(insert, grumpydb_protocol::Response::Ok(_)),
        "INSERT failed: {insert:?}"
    );

    let set_resp = client
        .raw_execute("ALTER DATABASE idx_consistency_db SET CONSISTENCY READ_CONCERN R=2")
        .await
        .expect("set consistency");
    assert!(
        matches!(set_resp, grumpydb_protocol::Response::Ok(_)),
        "ALTER DATABASE SET failed: {set_resp:?}"
    );

    // R=2 from database default should reject on single-node topology.
    let query_default = client
        .raw_execute("QUERY users name_idx \"alice\"")
        .await
        .expect("query default concern");
    assert!(
        matches!(query_default, grumpydb_protocol::Response::Error(_)),
        "expected QUERY to fail with default R=2, got: {query_default:?}"
    );

    // Per-request override must bypass the DB default.
    let query_override = client
        .raw_execute("READ_CONCERN R=1 QUERY users name_idx \"alice\"")
        .await
        .expect("query override concern");
    match query_override {
        grumpydb_protocol::Response::Array(items) => {
            assert_eq!(items.len(), 1, "expected one row from QUERY override")
        }
        other => panic!("unexpected QUERY override response: {other:?}"),
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

#[tokio::test]
async fn test_e2e_put_with_vc_merges_crdt_gcounter() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client.create_database("vc_db").await.expect("create db");
    {
        let mut db = client.database("vc_db").await.expect("use");
        db.create_collection("docs").await.expect("create coll");
    }

    let key = Uuid::new_v4();
    let v1 = json!({
        "$crdt": {
            "kind": "GCounter",
            "payload_b64": "BQAAAAAAAAA="
        }
    });
    let v2 = json!({
        "$crdt": {
            "kind": "GCounter",
            "payload_b64": "CQAAAAAAAAA="
        }
    });
    let vc1 = json!({"n1": 1});
    let vc2 = json!({"n2": 1});

    let resp1 = client
        .raw_execute(&format!("PUT_WITH_VC docs {key} {} {}", v1, vc1))
        .await
        .expect("put_with_vc 1");
    assert!(
        matches!(resp1, grumpydb_protocol::Response::Ok(_)),
        "first PUT_WITH_VC failed: {resp1:?}"
    );

    let resp2 = client
        .raw_execute(&format!("PUT_WITH_VC docs {key} {} {}", v2, vc2))
        .await
        .expect("put_with_vc 2");
    assert!(
        matches!(resp2, grumpydb_protocol::Response::Ok(_)),
        "second PUT_WITH_VC failed: {resp2:?}"
    );

    let mut db = client.database("vc_db").await.expect("use");
    let got = db
        .get("docs", &key)
        .await
        .expect("get merged")
        .expect("doc");
    assert_eq!(got["$crdt"]["kind"], json!("GCounter"));
    assert_eq!(got["$crdt"]["payload_b64"], json!("CQAAAAAAAAA="));
}

#[tokio::test]
async fn test_e2e_put_with_vc_round_trips_crdt_pncounter_envelope() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client.create_database("vc_pn_db").await.expect("create db");
    {
        let mut db = client.database("vc_pn_db").await.expect("use");
        db.create_collection("docs").await.expect("create coll");
    }

    let key = Uuid::new_v4();
    let left = json!({
        "$crdt": {
            "kind": "PNCounter",
            "payload_b64": "AgAAAAAAAAAHAAAAAAAAAA=="
        }
    });
    let right = json!({
        "$crdt": {
            "kind": "PNCounter",
            "payload_b64": "CQAAAAAAAAADAAAAAAAAAA=="
        }
    });
    let vc1 = json!({"n1": 1});
    let vc2 = json!({"n2": 1});

    let first = client
        .raw_execute(&format!("PUT_WITH_VC docs {key} {} {}", left, vc1))
        .await
        .expect("first put_with_vc");
    assert!(
        matches!(first, grumpydb_protocol::Response::Ok(_)),
        "first PUT_WITH_VC failed: {first:?}"
    );

    let second = client
        .raw_execute(&format!("PUT_WITH_VC docs {key} {} {}", right, vc2))
        .await
        .expect("second put_with_vc");
    assert!(
        matches!(second, grumpydb_protocol::Response::Ok(_)),
        "second PUT_WITH_VC failed: {second:?}"
    );

    let mut db = client.database("vc_pn_db").await.expect("use");
    let got = db
        .get("docs", &key)
        .await
        .expect("get merged")
        .expect("doc");
    assert_eq!(got["$crdt"]["kind"], json!("PNCounter"));
    // PNCounter merge keeps component-wise maxima between concurrent envelopes:
    // left=(2,7), right=(9,3) => merged=(9,7).
    assert_eq!(
        got["$crdt"]["payload_b64"],
        json!("CQAAAAAAAAAHAAAAAAAAAA==")
    );
}

#[tokio::test]
async fn test_e2e_put_with_vc_after_delete_restores_document() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client
        .create_database("vc_tombstone_db")
        .await
        .expect("create db");
    let key = Uuid::new_v4();
    {
        let mut db = client.database("vc_tombstone_db").await.expect("use");
        db.create_collection("docs").await.expect("create coll");
        db.insert("docs", key, &json!({"name":"before"}))
            .await
            .expect("insert");
        db.delete("docs", &key).await.expect("delete");
        assert_eq!(db.get("docs", &key).await.expect("get after delete"), None);
    }

    let reconciled = json!({"name":"after","source":"put_with_vc"});
    let vc = json!({"n1": 2});
    let resp = client
        .raw_execute(&format!("PUT_WITH_VC docs {key} {} {}", reconciled, vc))
        .await
        .expect("put_with_vc after delete");
    assert!(
        matches!(resp, grumpydb_protocol::Response::Ok(_)),
        "PUT_WITH_VC after delete failed: {resp:?}"
    );

    let mut db = client.database("vc_tombstone_db").await.expect("use");
    let got = db
        .get("docs", &key)
        .await
        .expect("get restored")
        .expect("doc");
    assert_eq!(got, json!({"name":"after","source":"put_with_vc"}));
}

#[tokio::test]
async fn test_e2e_rebalance_plan_commands_return_json() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;

    let resp = client
        .raw_execute("REBALANCE PLAN ADD-NODE 11111111-1111-1111-1111-111111111111")
        .await
        .expect("rebalance plan add");
    let body = match resp {
        grumpydb_protocol::Response::Bulk(Some(s)) => s,
        other => panic!("unexpected REBALANCE PLAN ADD response: {other:?}"),
    };
    let json: serde_json::Value = serde_json::from_str(&body).expect("plan add json");
    assert_eq!(json.get("action"), Some(&json!("add-node")));

    let resp = client
        .raw_execute("REBALANCE PLAN REMOVE-NODE 11111111-1111-1111-1111-111111111111")
        .await
        .expect("rebalance plan remove");
    let body = match resp {
        grumpydb_protocol::Response::Bulk(Some(s)) => s,
        other => panic!("unexpected REBALANCE PLAN REMOVE response: {other:?}"),
    };
    let json: serde_json::Value = serde_json::from_str(&body).expect("plan remove json");
    assert_eq!(json.get("action"), Some(&json!("remove-node")));
}

#[tokio::test]
async fn test_e2e_rebalance_execute_requires_selected_database() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;

    let resp = client
        .raw_execute("REBALANCE EXECUTE ADD-NODE 11111111-1111-1111-1111-111111111111 users")
        .await
        .expect("rebalance execute add");
    match resp {
        grumpydb_protocol::Response::Error(msg) => {
            assert!(
                msg.contains("no database selected"),
                "unexpected error: {msg}"
            );
        }
        other => panic!("expected error without USE, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_e2e_rebalance_execute_commands_return_json_with_use() {
    let server = TestServer::spawn().await;
    let mut client = admin_client(&server).await;
    client
        .create_database("rebalance_db")
        .await
        .expect("create db");
    let mut db = client.database("rebalance_db").await.expect("use db");
    db.create_collection("users").await.expect("create users");

    let resp = client
        .raw_execute("REBALANCE EXECUTE ADD-NODE 11111111-1111-1111-1111-111111111111 users")
        .await
        .expect("rebalance execute add");
    let body = match resp {
        grumpydb_protocol::Response::Bulk(Some(s)) => s,
        other => panic!("unexpected REBALANCE EXECUTE ADD response: {other:?}"),
    };
    let json: serde_json::Value = serde_json::from_str(&body).expect("execute add json");
    assert_eq!(json.get("action"), Some(&json!("add-node-transfer")));

    let resp = client
        .raw_execute("REBALANCE EXECUTE REMOVE-NODE 11111111-1111-1111-1111-111111111111 users")
        .await
        .expect("rebalance execute remove");
    let body = match resp {
        grumpydb_protocol::Response::Bulk(Some(s)) => s,
        other => panic!("unexpected REBALANCE EXECUTE REMOVE response: {other:?}"),
    };
    let json: serde_json::Value = serde_json::from_str(&body).expect("execute remove json");
    assert_eq!(json.get("action"), Some(&json!("remove-node-transfer")));
}
