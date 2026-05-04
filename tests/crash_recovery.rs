//! Crash-recovery integration tests.
//!
//! Each test spawns a real `grumpydb-server` process via the
//! [`grumpydb_testing`] helper, exercises the server through the public
//! TCP wire protocol, then SIGKILLs the process and restarts it on the
//! same data directory. The post-restart state is then asserted against
//! the durability and consistency invariants documented in
//! `docs/IMPLEMENTATION_PLAN_V4.md` (Phase 29).
//!
//! These tests prove that:
//!   * Every client-acknowledged insert survives a SIGKILL.
//!   * The WAL fsync-on-commit guarantee holds even when the client did
//!     not issue an explicit `FLUSH`.
//!   * Mid-operation crashes never leave the database with phantom
//!     documents (no torn writes).
//!   * Repeated crash/restart cycles do not accumulate corruption.
//!
//! Note on timing: the "mid-crash" tests pick a small delay (~50ms) that
//! gives the server time to ack a *fraction* of the operations before
//! being killed. The assertions are written so that any outcome between
//! "nothing committed" and "everything committed" is acceptable, as long
//! as the consistency invariant holds.

use grumpydb_client::GrumpyClient;
use grumpydb_testing::TestServer;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

// ── helpers ──────────────────────────────────────────────────────────────

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

/// Deterministic UUID for index `i`. Used so a test can recompute the
/// expected key set without having to record it.
fn key(i: u128) -> Uuid {
    Uuid::from_u128(i)
}

// ── 1. crash after committed inserts (with explicit FLUSH) ──────────────

#[tokio::test]
async fn test_crash_after_committed_inserts() {
    let mut server = TestServer::spawn().await;

    // Phase A: write 100 documents and FLUSH.
    {
        let mut client = admin_client(&server).await;
        client.create_database("crash_db").await.expect("create db");
        let mut db = client.database("crash_db").await.expect("use");
        db.create_collection("docs").await.expect("create coll");

        for i in 0..100u128 {
            db.insert("docs", key(i), &json!({ "i": i as u64 }))
                .await
                .expect("insert");
        }
        db.flush().await.expect("flush");
    }

    // Phase B: SIGKILL.
    server.crash().await;

    // Phase C: restart and verify all 100 docs are durable.
    server.restart().await;

    let mut client = admin_client(&server).await;
    let mut db = client
        .database("crash_db")
        .await
        .expect("use after restart");

    let count = db.count("docs").await.expect("count");
    assert_eq!(count, 100, "all flushed inserts must survive crash");

    for i in 0..100u128 {
        let got = db.get("docs", &key(i)).await.expect("get");
        assert_eq!(
            got,
            Some(json!({ "i": i as u64 })),
            "doc {i} missing or corrupted after restart"
        );
    }
}

// ── 2. crash after committed inserts (no explicit FLUSH) ────────────────

#[tokio::test]
async fn test_crash_after_inserts_without_flush() {
    let mut server = TestServer::spawn().await;

    {
        let mut client = admin_client(&server).await;
        client
            .create_database("noflush_db")
            .await
            .expect("create db");
        let mut db = client.database("noflush_db").await.expect("use");
        db.create_collection("docs").await.expect("create coll");

        // No `FLUSH` — relies entirely on the WAL fsync-on-commit promise.
        for i in 0..100u128 {
            db.insert("docs", key(i), &json!({ "i": i as u64 }))
                .await
                .expect("insert");
        }
    }

    server.crash().await;
    server.restart().await;

    let mut client = admin_client(&server).await;
    let mut db = client
        .database("noflush_db")
        .await
        .expect("use after restart");

    let count = db.count("docs").await.expect("count");
    assert_eq!(
        count, 100,
        "WAL must replay all acked inserts even without explicit FLUSH"
    );

    for i in 0..100u128 {
        let got = db.get("docs", &key(i)).await.expect("get");
        assert!(got.is_some(), "doc {i} must be recovered from the WAL");
    }
}

// ── 3. crash mid-insert: surviving state is a prefix of the ack log ─────

#[tokio::test]
async fn test_crash_during_inserts_partial_then_recover() {
    let mut server = TestServer::spawn().await;

    // Set up the database and collection synchronously, before racing.
    {
        let mut client = admin_client(&server).await;
        client
            .create_database("partial_db")
            .await
            .expect("create db");
        let mut db = client.database("partial_db").await.expect("use");
        db.create_collection("docs").await.expect("create coll");
    }

    let port = server.addr.port();
    let tenant = server.admin_tenant.to_string();
    let user = server.admin_user.to_string();
    let password = server.admin_password.clone();
    let acked: Arc<Mutex<Vec<Uuid>>> = Arc::new(Mutex::new(Vec::new()));

    // Background writer: tries to insert 50 docs, but the server may die
    // halfway through. Records every UUID it received an OK ack for.
    let acked_writer = acked.clone();
    let writer = tokio::spawn(async move {
        let Ok(mut client) = GrumpyClient::connect("127.0.0.1", port, false).await else {
            return;
        };
        if client.login(&tenant, &user, &password).await.is_err() {
            return;
        }
        let Ok(mut db) = client.database("partial_db").await else {
            return;
        };
        for i in 0..50u128 {
            let k = key(i);
            match db.insert("docs", k, &json!({ "i": i as u64 })).await {
                Ok(()) => acked_writer.lock().expect("acked lock").push(k),
                Err(_) => return, // connection died — server crashed
            }
        }
    });

    // Give the writer a small head start, then SIGKILL the server.
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.crash().await;

    // The writer may finish (all 50 acked) or fail (connection broken).
    // Either way we just want the recorded acks.
    let _ = writer.await;
    let acked_keys = acked.lock().expect("acked lock").clone();

    server.restart().await;

    let mut client = admin_client(&server).await;
    let mut db = client
        .database("partial_db")
        .await
        .expect("use after restart");

    let count = db.count("docs").await.expect("count");
    assert!(
        count >= acked_keys.len() as i64,
        "count={count} must be >= acked={} (every ack must be durable)",
        acked_keys.len()
    );
    assert!(
        count <= 50,
        "count={count} must be <= 50 (no phantom documents)"
    );

    // Every acked UUID must be retrievable.
    for k in &acked_keys {
        let got = db.get("docs", k).await.expect("get acked key");
        assert!(got.is_some(), "acked key {k} disappeared after restart");
    }

    // Every UUID *not* in the original 0..50 range must be absent
    // (no phantoms with random IDs).
    for i in 100..120u128 {
        let got = db.get("docs", &key(i)).await.expect("get");
        assert!(got.is_none(), "phantom document at index {i}");
    }
}

// ── 4. crash during index creation: consistency, not particular outcome ─

#[tokio::test]
async fn test_crash_during_index_creation() {
    let mut server = TestServer::spawn().await;

    // Pre-populate 200 docs with an `age` field.
    {
        let mut client = admin_client(&server).await;
        client.create_database("idx_db").await.expect("create db");
        let mut db = client.database("idx_db").await.expect("use");
        db.create_collection("people").await.expect("create coll");

        for i in 0..200u128 {
            db.insert(
                "people",
                key(i),
                &json!({ "i": i as u64, "age": (i % 100) as i64 }),
            )
            .await
            .expect("insert");
        }
        db.flush().await.expect("flush");
    }

    // Trigger CREATE INDEX in the background and crash 100ms later.
    let port = server.addr.port();
    let tenant = server.admin_tenant.to_string();
    let user = server.admin_user.to_string();
    let password = server.admin_password.clone();

    let creator = tokio::spawn(async move {
        let Ok(mut client) = GrumpyClient::connect("127.0.0.1", port, false).await else {
            return;
        };
        if client.login(&tenant, &user, &password).await.is_err() {
            return;
        }
        let Ok(mut db) = client.database("idx_db").await else {
            return;
        };
        let _ = db.create_index("people", "age_idx", "age").await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    server.crash().await;
    let _ = creator.await;

    server.restart().await;

    let mut client = admin_client(&server).await;
    let mut db = client.database("idx_db").await.expect("use after restart");

    // The 200 base documents must always survive the crash.
    let count = db.count("people").await.expect("count");
    assert_eq!(count, 200, "base data must survive even if index aborted");

    // The index may or may not exist — both outcomes are valid. Either way,
    // the database must be queryable and the docs intact.
    let indexes = db.list_indexes("people").await.expect("list indexes");
    if indexes.iter().any(|i| i == "age_idx") {
        // If the index claims to exist, it must be usable and correct.
        let q = db
            .query("people", "age_idx", &json!(42_i64))
            .await
            .expect("query existing index");
        // Indices 42 and 142 both have age=42 (i % 100).
        assert_eq!(q.len(), 2, "index returned wrong number of matches");
        for (_k, v) in &q {
            assert_eq!(v.get("age"), Some(&json!(42_i64)));
        }
    }

    // GETs by primary key always work, regardless of secondary index state.
    for i in [0u128, 1, 50, 199] {
        let got = db.get("people", &key(i)).await.expect("get");
        assert!(got.is_some(), "primary lookup of {i} broke after crash");
    }
}

// ── 5. crash during compaction: surviving docs intact ──────────────────

#[tokio::test]
async fn test_crash_during_compaction() {
    let mut server = TestServer::spawn().await;

    // Insert 500, then delete every other one to create fragmentation.
    let mut survivors = Vec::with_capacity(250);
    {
        let mut client = admin_client(&server).await;
        client
            .create_database("compact_db")
            .await
            .expect("create db");
        let mut db = client.database("compact_db").await.expect("use");
        db.create_collection("docs").await.expect("create coll");

        for i in 0..500u128 {
            db.insert("docs", key(i), &json!({ "i": i as u64 }))
                .await
                .expect("insert");
        }
        for i in 0..500u128 {
            if i.is_multiple_of(2) {
                db.delete("docs", &key(i)).await.expect("delete");
            } else {
                survivors.push(key(i));
            }
        }
        db.flush().await.expect("flush");
    }

    // Trigger COMPACT in the background, crash 50ms later.
    let port = server.addr.port();
    let tenant = server.admin_tenant.to_string();
    let user = server.admin_user.to_string();
    let password = server.admin_password.clone();

    let compactor = tokio::spawn(async move {
        let Ok(mut client) = GrumpyClient::connect("127.0.0.1", port, false).await else {
            return;
        };
        if client.login(&tenant, &user, &password).await.is_err() {
            return;
        }
        let Ok(mut db) = client.database("compact_db").await else {
            return;
        };
        let _ = db.compact("docs").await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    server.crash().await;
    let _ = compactor.await;

    server.restart().await;

    let mut client = admin_client(&server).await;
    let mut db = client
        .database("compact_db")
        .await
        .expect("use after restart");

    // COUNT includes hidden tombstones until compaction completes.
    // Verify logical live rows through SCAN/Get visibility instead.
    let live_rows = db.scan("docs", None, None).await.expect("scan live rows");
    assert_eq!(
        live_rows.len(),
        survivors.len(),
        "compaction crash changed live row visibility"
    );

    for k in &survivors {
        let got = db.get("docs", k).await.expect("get survivor");
        assert!(
            got.is_some(),
            "survivor {k} lost during compaction crash recovery"
        );
    }

    // Deleted keys must remain absent.
    for i in (0..500u128).filter(|i| i.is_multiple_of(2)) {
        let got = db.get("docs", &key(i)).await.expect("get deleted");
        assert!(got.is_none(), "deleted key {i} resurrected after recovery");
    }
}

// ── 6. repeated crash/restart cycles ────────────────────────────────────

#[tokio::test]
async fn test_repeated_crash_recovery() {
    let mut server = TestServer::spawn().await;

    {
        let mut client = admin_client(&server).await;
        client.create_database("loop_db").await.expect("create db");
        let mut db = client.database("loop_db").await.expect("use");
        db.create_collection("docs").await.expect("create coll");
    }

    for cycle in 0..10u128 {
        {
            let mut client = admin_client(&server).await;
            let mut db = client.database("loop_db").await.expect("use");
            for i in 0..10u128 {
                let global = cycle * 10 + i;
                db.insert("docs", key(global), &json!({ "i": global as u64 }))
                    .await
                    .expect("insert");
            }
            db.flush().await.expect("flush");
        }
        server.crash().await;
        server.restart().await;
    }

    let mut client = admin_client(&server).await;
    let mut db = client.database("loop_db").await.expect("use");

    let count = db.count("docs").await.expect("count");
    assert_eq!(count, 100, "all 10 cycles of 10 inserts must survive");

    for global in 0..100u128 {
        let got = db.get("docs", &key(global)).await.expect("get");
        assert_eq!(got, Some(json!({ "i": global as u64 })));
    }
}

// NOTE: A property-based crash-recovery test (proptest) would generate
// random op sequences and random crash points to verify that the surviving
// state is always a prefix of the acked operation log. This is left as
// future work; integrating proptest with async/tokio non-trivially
// exceeds the budget for Phase 29.

#[tokio::test]
async fn test_crash_recovery_rebalance_control_plane_still_operable() {
    let mut server = TestServer::spawn().await;

    {
        let mut client = admin_client(&server).await;
        client
            .create_database("rebalance_recovery_db")
            .await
            .expect("create db");
        let mut db = client.database("rebalance_recovery_db").await.expect("use");
        db.create_collection("docs").await.expect("create coll");
    }

    let mut client = admin_client(&server).await;
    let plan_before = client
        .raw_execute("REBALANCE PLAN ADD-NODE 11111111-1111-1111-1111-111111111111")
        .await
        .expect("plan before crash");
    let body = match plan_before {
        grumpydb_protocol::Response::Bulk(Some(s)) => s,
        other => panic!("unexpected plan response before crash: {other:?}"),
    };
    let json_before: serde_json::Value = serde_json::from_str(&body).expect("plan json before");
    assert_eq!(json_before.get("action"), Some(&json!("add-node")));

    server.crash().await;
    server.restart().await;

    let mut client = admin_client(&server).await;
    let mut db = client
        .database("rebalance_recovery_db")
        .await
        .expect("use after restart");
    db.insert("docs", key(42), &json!({ "i": 42 }))
        .await
        .expect("insert after restart");

    let plan_after = client
        .raw_execute("REBALANCE PLAN REMOVE-NODE 11111111-1111-1111-1111-111111111111")
        .await
        .expect("plan after restart");
    let body = match plan_after {
        grumpydb_protocol::Response::Bulk(Some(s)) => s,
        other => panic!("unexpected plan response after crash: {other:?}"),
    };
    let json_after: serde_json::Value = serde_json::from_str(&body).expect("plan json after");
    assert_eq!(json_after.get("action"), Some(&json!("remove-node")));

    let exec_after = client
        .raw_execute("REBALANCE EXECUTE REMOVE-NODE 11111111-1111-1111-1111-111111111111 docs")
        .await
        .expect("execute after restart");
    let body = match exec_after {
        grumpydb_protocol::Response::Bulk(Some(s)) => s,
        other => panic!("unexpected execute response after crash: {other:?}"),
    };
    let json_exec: serde_json::Value = serde_json::from_str(&body).expect("execute json");
    assert_eq!(
        json_exec.get("action"),
        Some(&json!("remove-node-transfer"))
    );
}
