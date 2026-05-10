//! Phase 44e: integration tests for the schema-gossip + materializer chain.
//!
//! These tests stand up **two `SharedServer` + `Coordinator` pairs in
//! the same process** and exercise the convergence path manually
//! (skipping the gossip timer for determinism). Each test asserts a
//! property that the demo cluster could only validate visually before
//! 44a–d landed.
//!
//! The unit tests in `grumpydb-server/src/cluster/schema/` cover each
//! moving part in isolation; this file glues them together to show
//! the **whole flow**:
//!
//! ```text
//!  Node A:                   Node B:
//!  CREATE INDEX docs by_name name
//!     ↓
//!  apply_local_ddl
//!     ↓ (bumps schema_version,
//!        appends schema.log,
//!        enqueues materialize job — no-op locally
//!        because the engine call already happened)
//!     ↓
//!  schema.log contains entry v1
//!     ↓ (gossip would tick here)
//!     ↓
//!                            apply_remote_schema_entries(diff)
//!                              ↓
//!                              schema.log contains entry v1
//!                              materializer.enqueue Build
//!                              ↓
//!                              idx_by_name.idx exists on disk
//!                              QUERY by_name now succeeds
//! ```

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use grumpydb::SharedServer;
use grumpydb::document::value::Value;
use grumpydb_server::cluster::NodeIdentity;
use grumpydb_server::cluster::schema::log::SchemaLog;
use grumpydb_server::cluster::schema::{IndexKey, SchemaOp, materializer};
use grumpydb_server::config::{ClusterSection, PeerEntry};
use grumpydb_server::coordinator::Coordinator;
use tempfile::TempDir;
use uuid::Uuid;

// ── helpers ────────────────────────────────────────────────────────

fn doc(name: &str, age: i64) -> Value {
    let mut m = BTreeMap::new();
    m.insert("name".into(), Value::String(name.into()));
    m.insert("age".into(), Value::Integer(age));
    Value::Object(m)
}

fn cluster_id() -> Uuid {
    "11111111-1111-1111-1111-111111111111".parse().unwrap()
}

fn identity_with(node_id: Uuid) -> NodeIdentity {
    NodeIdentity {
        node_id,
        cluster_id: cluster_id(),
        created_at_unix: 0,
        identity_version: 1,
    }
}

/// Build a coordinator that knows about exactly one peer.
fn make_coord(
    identity: &NodeIdentity,
    peer_node_id: Uuid,
    local_addr: &str,
    peer_addr: &str,
) -> Coordinator {
    let cluster = ClusterSection {
        peers: vec![PeerEntry {
            node_id: peer_node_id.to_string(),
            addr: peer_addr.to_string(),
            status: None,
            last_seen_at_unix: None,
            vnode_assignments: vec![],
        }],
        ..ClusterSection::default()
    };
    Coordinator::from_config(identity, &cluster, local_addr)
}

fn open_node(
    data_dir: &Path,
    identity: &NodeIdentity,
    peer_node_id: Uuid,
    local_addr: &str,
    peer_addr: &str,
) -> (SharedServer, Coordinator) {
    let server = SharedServer::open(data_dir).expect("open SharedServer");
    server.create_client("_system").expect("client");
    server.create_database("_system", "demo").expect("db");
    let db = server.database("_system", "demo").expect("open db");
    db.create_collection("docs").expect("collection");

    let (log, state) = SchemaLog::open(data_dir).expect("open schema log");
    let mut coord = make_coord(identity, peer_node_id, local_addr, peer_addr);
    coord.attach_schema(state, log);
    coord.attach_materializer(materializer::spawn(server.clone()));
    (server, coord)
}

/// Wait for a file to appear, panicking after `timeout`.
async fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return;
        }
        if Instant::now() > deadline {
            panic!("expected file did not appear within {timeout:?}: {path:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Drive one round of "gossip" by hand: read A's schema_version, pull
/// the diff from A's local view, apply it to B. Returns the number of
/// entries B applied.
fn simulate_gossip_round(from: &Coordinator, to: &Coordinator) -> usize {
    let entries = from.local_schema_diff_since(to.schema_version());
    to.apply_remote_schema_entries(&entries)
}

// ── tests ──────────────────────────────────────────────────────────

/// CREATE INDEX on A → simulated gossip → B observes the schema and
/// materializes the index locally (B already has the collection +
/// some documents).
#[tokio::test]
async fn test_create_index_propagates_to_peer_and_materializes() {
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    let id_a = identity_with(Uuid::new_v4());
    let id_b = identity_with(Uuid::new_v4());

    let (server_a, coord_a) = open_node(
        dir_a.path(),
        &id_a,
        id_b.node_id,
        "127.0.0.1:7000",
        "127.0.0.1:7001",
    );
    let (server_b, coord_b) = open_node(
        dir_b.path(),
        &id_b,
        id_a.node_id,
        "127.0.0.1:7001",
        "127.0.0.1:7000",
    );

    // Both nodes hold a few documents.
    let db_a = server_a.database("_system", "demo").unwrap();
    db_a.insert("docs", Uuid::new_v4(), doc("alice", 30))
        .unwrap();
    db_a.insert("docs", Uuid::new_v4(), doc("bob", 25)).unwrap();
    let db_b = server_b.database("_system", "demo").unwrap();
    db_b.insert("docs", Uuid::new_v4(), doc("carol", 42))
        .unwrap();

    // Issue the DDL locally on A. The handler call sequence in
    // production also calls `db.create_index(...)` synchronously; we
    // mirror that here so A's on-disk state matches a real run.
    db_a.create_index("docs", "by_name", "name").unwrap();
    let entry = coord_a
        .apply_local_create_index("_system", "demo", "docs", "by_name", "name", 1_000)
        .expect("local apply");
    assert_eq!(entry.version, 1);
    assert_eq!(coord_a.schema_version(), 1);
    assert_eq!(coord_b.schema_version(), 0);

    // Simulate one gossip round A → B.
    let applied = simulate_gossip_round(&coord_a, &coord_b);
    assert_eq!(applied, 1);
    assert_eq!(coord_b.schema_version(), 1);

    // The materializer should now produce the on-disk index file on B.
    let idx_b = dir_b
        .path()
        .join("_system")
        .join("demo")
        .join("docs")
        .join("idx_by_name.idx");
    wait_for_file(&idx_b, Duration::from_secs(2)).await;

    // And QUERY now works on B.
    let results = db_b
        .query("docs", "by_name", &Value::String("carol".into()))
        .expect("query");
    assert_eq!(results.len(), 1);
}

/// Idempotency: running gossip a second time must not re-create or
/// disturb the on-disk index.
#[tokio::test]
async fn test_repeated_gossip_rounds_are_idempotent() {
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    let id_a = identity_with(Uuid::new_v4());
    let id_b = identity_with(Uuid::new_v4());

    let (server_a, coord_a) = open_node(
        dir_a.path(),
        &id_a,
        id_b.node_id,
        "127.0.0.1:7010",
        "127.0.0.1:7011",
    );
    let (server_b, coord_b) = open_node(
        dir_b.path(),
        &id_b,
        id_a.node_id,
        "127.0.0.1:7011",
        "127.0.0.1:7010",
    );

    let db_a = server_a.database("_system", "demo").unwrap();
    db_a.insert("docs", Uuid::new_v4(), doc("x", 1)).unwrap();
    let db_b = server_b.database("_system", "demo").unwrap();
    db_b.insert("docs", Uuid::new_v4(), doc("y", 2)).unwrap();

    db_a.create_index("docs", "by_name", "name").unwrap();
    coord_a
        .apply_local_create_index("_system", "demo", "docs", "by_name", "name", 1_000)
        .unwrap();

    for _ in 0..3 {
        let applied = simulate_gossip_round(&coord_a, &coord_b);
        // Only the first round produces an Applied; subsequent rounds
        // see no new entries above coord_b.schema_version().
        if coord_b.schema_version() == 1 {
            assert!(applied <= 1);
        }
    }

    let idx_b = dir_b
        .path()
        .join("_system")
        .join("demo")
        .join("docs")
        .join("idx_by_name.idx");
    wait_for_file(&idx_b, Duration::from_secs(2)).await;
    assert_eq!(coord_b.schema_version(), 1);
}

/// LWW: a CREATE arriving with an older HLC after a DROP must NOT
/// resurrect the index on the receiving node.
#[tokio::test]
async fn test_drop_then_late_create_does_not_resurrect_on_peer() {
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    let id_a = identity_with(Uuid::new_v4());
    let id_b = identity_with(Uuid::new_v4());

    let (_server_a, coord_a) = open_node(
        dir_a.path(),
        &id_a,
        id_b.node_id,
        "127.0.0.1:7020",
        "127.0.0.1:7021",
    );
    let (_server_b, coord_b) = open_node(
        dir_b.path(),
        &id_b,
        id_a.node_id,
        "127.0.0.1:7021",
        "127.0.0.1:7020",
    );

    // Step 1 on A: CREATE then DROP, both with high HLCs.
    coord_a
        .apply_local_create_index("_system", "demo", "docs", "by_name", "name", 5_000)
        .unwrap();
    coord_a
        .apply_local_drop_index("_system", "demo", "docs", "by_name", 6_000)
        .unwrap();

    // Step 2: B gossips and converges to "tombstoned".
    let applied = simulate_gossip_round(&coord_a, &coord_b);
    assert_eq!(applied, 2);
    assert!(!coord_b.schema_has_index("_system", "demo", "docs", "by_name"));

    // Step 3: a late peer with an older HLC tries to apply a CREATE
    // directly to B (e.g. a partition rejoin). It must be ignored.
    let late_create = grumpydb_server::cluster::schema::SchemaLogEntry {
        version: 999,
        hlc: 1_000, // older than the DROP at 6000
        op: SchemaOp::CreateIndex {
            key: IndexKey {
                tenant: "_system".into(),
                database: "demo".into(),
                collection: "docs".into(),
                index_name: "by_name".into(),
            },
            field_path: "name".into(),
        },
    };
    let _ = coord_b.apply_remote_schema_entries(&[late_create]);

    assert!(!coord_b.schema_has_index("_system", "demo", "docs", "by_name"));
    // Version still moves forward (max), but the live status is unchanged.
    assert_eq!(coord_b.schema_version(), 999);
}

/// Bootstrap: a node that already had `idx_*.idx` files on disk
/// (e.g. from a pre-44a binary) ends up with a populated SchemaState
/// after the first start.
#[tokio::test]
async fn test_bootstrap_from_existing_idx_files_seeds_schema_state() {
    let dir = TempDir::new().unwrap();
    let server = SharedServer::open(dir.path()).unwrap();
    server.create_client("_system").unwrap();
    server.create_database("_system", "demo").unwrap();
    let db = server.database("_system", "demo").unwrap();
    db.create_collection("docs").unwrap();
    db.insert("docs", Uuid::new_v4(), doc("alice", 30)).unwrap();
    db.create_index("docs", "by_name", "name").unwrap();
    db.create_index("docs", "by_age", "age").unwrap();

    // Now simulate a fresh server start: bootstrap_from_data_dir +
    // SchemaLog::open should rebuild a non-empty SchemaState.
    grumpydb_server::cluster::schema::bootstrap::bootstrap_from_data_dir(dir.path()).unwrap();
    let (_log, state) = SchemaLog::open(dir.path()).unwrap();
    assert_eq!(state.version(), 2);
    let live_count = state.live_entries().count();
    assert_eq!(live_count, 2);
}
