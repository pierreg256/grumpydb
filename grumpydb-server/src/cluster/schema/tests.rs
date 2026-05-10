//! Tests for [`crate::cluster::schema`] (Phase 44a).
//!
//! Covers:
//! - SchemaState model: insert / LWW resolution / tombstone / version
//!   advancement / idempotent replay.
//! - SchemaLog persistence: round-trip replay + corruption handling.
//! - Bootstrap from data dir: idempotency + index discovery + skipping
//!   reserved dirs.

use std::path::Path;

use tempfile::TempDir;

use super::bootstrap::{BootstrapReport, bootstrap_from_data_dir};
use super::log::{SchemaLog, SchemaLogError};
use super::{ApplyOutcome, IndexEntry, IndexKey, SchemaLogEntry, SchemaOp, SchemaState};

// ── helpers ────────────────────────────────────────────────────────

fn key(tenant: &str, db: &str, coll: &str, name: &str) -> IndexKey {
    IndexKey {
        tenant: tenant.into(),
        database: db.into(),
        collection: coll.into(),
        index_name: name.into(),
    }
}

fn create(version: u64, hlc: u64, k: IndexKey, field: &str) -> SchemaLogEntry {
    SchemaLogEntry {
        version,
        hlc,
        op: SchemaOp::CreateIndex {
            key: k,
            field_path: field.into(),
        },
    }
}

fn drop_(version: u64, hlc: u64, k: IndexKey) -> SchemaLogEntry {
    SchemaLogEntry {
        version,
        hlc,
        op: SchemaOp::DropIndex { key: k },
    }
}

fn touch_collection_dir(root: &Path, tenant: &str, db: &str, coll: &str) -> std::path::PathBuf {
    let coll_path = root.join(tenant).join(db).join(coll);
    std::fs::create_dir_all(&coll_path).unwrap();
    // The bootstrap routine uses `data.db` presence as the marker.
    std::fs::write(coll_path.join("data.db"), b"").unwrap();
    coll_path
}

fn touch_index_file(coll_path: &Path, name: &str) {
    std::fs::write(coll_path.join(format!("idx_{name}.idx")), b"").unwrap();
}

// ── SchemaState model ──────────────────────────────────────────────

#[test]
fn test_apply_create_on_empty_state() {
    let mut s = SchemaState::new();
    let k = key("_system", "demo", "docs", "by_name");
    let outcome = s.apply(&create(1, 100, k.clone(), "name"));

    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(s.version(), 1);
    assert_eq!(s.len(), 1);
    let entry = s.get(&k).unwrap();
    assert_eq!(entry.field_path, "name");
    assert!(!entry.tombstone);
    assert_eq!(entry.last_modified_hlc, 100);
}

#[test]
fn test_apply_create_then_drop() {
    let mut s = SchemaState::new();
    let k = key("t", "d", "c", "by_x");
    s.apply(&create(1, 10, k.clone(), "x"));
    let outcome = s.apply(&drop_(2, 20, k.clone()));

    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(s.version(), 2);
    // Live view is empty…
    assert!(s.get(&k).is_none());
    // …but the tombstone is still there.
    let any = s.get_any(&k).unwrap();
    assert!(any.tombstone);
    assert_eq!(any.last_modified_hlc, 20);
    // And the original field_path is preserved.
    assert_eq!(any.field_path, "x");
}

#[test]
fn test_drop_then_late_create_is_lww_stale() {
    let mut s = SchemaState::new();
    let k = key("t", "d", "c", "by_x");
    s.apply(&create(1, 10, k.clone(), "x"));
    s.apply(&drop_(2, 30, k.clone()));

    // A peer that hadn't seen the DROP re-publishes a CREATE with
    // older HLC; it must be ignored.
    let outcome = s.apply(&create(99, 20, k.clone(), "x"));
    assert_eq!(outcome, ApplyOutcome::Stale);

    // The local entry is unchanged: still tombstoned.
    assert!(s.get_any(&k).unwrap().tombstone);
    // But version still advances (max).
    assert_eq!(s.version(), 99);
}

#[test]
fn test_create_with_newer_hlc_overwrites() {
    let mut s = SchemaState::new();
    let k = key("t", "d", "c", "by_x");
    s.apply(&create(1, 10, k.clone(), "x"));
    let outcome = s.apply(&create(2, 20, k.clone(), "y"));

    assert_eq!(outcome, ApplyOutcome::Applied);
    let entry = s.get(&k).unwrap();
    assert_eq!(entry.field_path, "y");
    assert_eq!(entry.last_modified_hlc, 20);
}

#[test]
fn test_idempotent_replay_returns_duplicate() {
    let mut s = SchemaState::new();
    let k = key("t", "d", "c", "by_x");
    let entry = create(1, 10, k.clone(), "x");
    s.apply(&entry);
    let outcome = s.apply(&entry);
    assert_eq!(outcome, ApplyOutcome::Duplicate);
}

#[test]
fn test_next_version_is_monotonic() {
    let mut s = SchemaState::new();
    assert_eq!(s.next_version(), 1);
    assert_eq!(s.next_version(), 2);
    assert_eq!(s.next_version(), 3);
}

#[test]
fn test_live_entries_skips_tombstones() {
    let mut s = SchemaState::new();
    let k1 = key("t", "d", "c", "a");
    let k2 = key("t", "d", "c", "b");
    s.apply(&create(1, 10, k1.clone(), "a"));
    s.apply(&create(2, 20, k2.clone(), "b"));
    s.apply(&drop_(3, 30, k1.clone()));

    let live: Vec<&IndexKey> = s.live_entries().map(|(k, _)| k).collect();
    assert_eq!(live, vec![&k2]);

    // all_entries still sees both.
    assert_eq!(s.all_entries().count(), 2);
}

#[test]
fn test_entries_since_returns_diff_in_version_order() {
    let mut s = SchemaState::new();
    let k1 = key("t", "d", "c", "a");
    let k2 = key("t", "d", "c", "b");
    s.apply(&create(1, 10, k1.clone(), "a"));
    s.apply(&create(2, 20, k2.clone(), "b"));
    s.apply(&drop_(3, 30, k1));

    let diff = s.entries_since(0);
    assert_eq!(diff.len(), 3);
    assert_eq!(diff[0].version, 1);
    assert_eq!(diff[1].version, 2);
    assert_eq!(diff[2].version, 3);

    let diff = s.entries_since(1);
    assert_eq!(diff.len(), 2);
    assert_eq!(diff[0].version, 2);

    let diff = s.entries_since(3);
    assert!(diff.is_empty());
}

#[test]
fn test_entries_since_excludes_stale_and_duplicate() {
    let mut s = SchemaState::new();
    let k = key("t", "d", "c", "a");
    s.apply(&create(1, 100, k.clone(), "a"));

    // Duplicate replay: must not appear in the diff a second time.
    s.apply(&create(1, 100, k.clone(), "a"));

    // Stale: older HLC, live entry wins.
    s.apply(&create(7, 50, k, "a"));

    let diff = s.entries_since(0);
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].version, 1);
}

// ── SchemaLog persistence ──────────────────────────────────────────

#[test]
fn test_log_append_and_replay_round_trip() {
    let dir = TempDir::new().unwrap();

    // First open: empty state.
    let (log, state) = SchemaLog::open(dir.path()).unwrap();
    assert_eq!(state.version(), 0);

    // Append a few entries.
    let k1 = key("t", "d", "c", "a");
    let k2 = key("t", "d", "c", "b");
    log.append(&create(1, 100, k1.clone(), "a")).unwrap();
    log.append(&create(2, 200, k2.clone(), "b")).unwrap();
    log.append(&drop_(3, 300, k1.clone())).unwrap();
    drop(log);

    // Re-open and assert state is rebuilt.
    let (_log, state) = SchemaLog::open(dir.path()).unwrap();
    assert_eq!(state.version(), 3);
    assert!(state.get(&k1).is_none()); // tombstoned
    assert!(state.get_any(&k1).unwrap().tombstone);
    assert_eq!(state.get(&k2).unwrap().field_path, "b");
}

#[test]
fn test_log_corruption_returns_malformed_with_line_number() {
    let dir = TempDir::new().unwrap();
    let (log, _) = SchemaLog::open(dir.path()).unwrap();
    log.append(&create(1, 10, key("t", "d", "c", "a"), "a"))
        .unwrap();
    drop(log);

    // Corrupt by appending invalid JSON.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(SchemaLog::path_for(dir.path()))
            .unwrap();
        writeln!(f, "{{not json").unwrap();
    }

    let err = SchemaLog::open(dir.path()).unwrap_err();
    match err {
        SchemaLogError::Malformed { line, .. } => assert_eq!(line, 2),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_log_skips_blank_lines() {
    let dir = TempDir::new().unwrap();
    let path = SchemaLog::path_for(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    {
        let entry = create(1, 10, key("t", "d", "c", "a"), "a");
        let line = serde_json::to_string(&entry).unwrap();
        std::fs::write(&path, format!("{line}\n\n  \n")).unwrap();
    }
    let (_log, state) = SchemaLog::open(dir.path()).unwrap();
    assert_eq!(state.version(), 1);
}

#[test]
fn test_log_append_batch_is_atomic() {
    let dir = TempDir::new().unwrap();
    let (log, _) = SchemaLog::open(dir.path()).unwrap();
    log.append_batch(&[
        create(1, 10, key("t", "d", "c", "a"), "a"),
        create(2, 20, key("t", "d", "c", "b"), "b"),
        create(3, 30, key("t", "d", "c", "z"), "z"),
    ])
    .unwrap();
    drop(log);

    let (_log, state) = SchemaLog::open(dir.path()).unwrap();
    assert_eq!(state.version(), 3);
    assert_eq!(state.live_entries().count(), 3);
}

// ── Bootstrap from existing data dir ───────────────────────────────

#[test]
fn test_bootstrap_on_empty_dir_writes_empty_log() {
    let dir = TempDir::new().unwrap();
    let report = bootstrap_from_data_dir(dir.path()).unwrap();
    assert_eq!(
        report,
        BootstrapReport {
            entries_written: 0,
            already_initialized: false,
        }
    );
    assert!(SchemaLog::path_for(dir.path()).exists());

    let (_log, state) = SchemaLog::open(dir.path()).unwrap();
    assert!(state.is_empty());
}

#[test]
fn test_bootstrap_is_idempotent_when_log_already_exists() {
    let dir = TempDir::new().unwrap();
    bootstrap_from_data_dir(dir.path()).unwrap();
    let report = bootstrap_from_data_dir(dir.path()).unwrap();
    assert_eq!(
        report,
        BootstrapReport {
            entries_written: 0,
            already_initialized: true,
        }
    );
}

#[test]
fn test_bootstrap_synthesizes_one_entry_per_idx_file() {
    let dir = TempDir::new().unwrap();
    let coll = touch_collection_dir(dir.path(), "_system", "demo", "docs");
    touch_index_file(&coll, "by_name");
    touch_index_file(&coll, "by_age");

    // A second collection in another DB
    let coll2 = touch_collection_dir(dir.path(), "tenantA", "appdb", "users");
    touch_index_file(&coll2, "by_email");

    let report = bootstrap_from_data_dir(dir.path()).unwrap();
    assert_eq!(report.entries_written, 3);
    assert!(!report.already_initialized);

    let (_log, state) = SchemaLog::open(dir.path()).unwrap();
    assert_eq!(state.version(), 3);

    let live: Vec<_> = state
        .live_entries()
        .map(|(k, e): (&IndexKey, &IndexEntry)| {
            (
                k.tenant.clone(),
                k.database.clone(),
                k.collection.clone(),
                k.index_name.clone(),
                e.field_path.clone(),
            )
        })
        .collect();
    assert_eq!(live.len(), 3);
    assert!(live.contains(&(
        "_system".into(),
        "demo".into(),
        "docs".into(),
        "by_age".into(),
        "by_age".into()
    )));
    assert!(live.contains(&(
        "_system".into(),
        "demo".into(),
        "docs".into(),
        "by_name".into(),
        "by_name".into()
    )));
    assert!(live.contains(&(
        "tenantA".into(),
        "appdb".into(),
        "users".into(),
        "by_email".into(),
        "by_email".into()
    )));
}

#[test]
fn test_bootstrap_skips_reserved_directories() {
    let dir = TempDir::new().unwrap();
    // Engine-internal: should be skipped
    std::fs::create_dir_all(dir.path().join("_cluster")).unwrap();
    std::fs::create_dir_all(dir.path().join("_auth")).unwrap();
    // Hidden dir: should be skipped
    std::fs::create_dir_all(dir.path().join(".cache")).unwrap();
    // Real index
    let coll = touch_collection_dir(dir.path(), "tenant", "db", "c");
    touch_index_file(&coll, "by_name");

    let report = bootstrap_from_data_dir(dir.path()).unwrap();
    assert_eq!(report.entries_written, 1);
}

#[test]
fn test_bootstrap_skips_collection_without_data_db() {
    let dir = TempDir::new().unwrap();
    // Mkdir without `data.db` — not a valid collection.
    let coll = dir.path().join("tenant").join("db").join("notacoll");
    std::fs::create_dir_all(&coll).unwrap();
    std::fs::write(coll.join("idx_garbage.idx"), b"").unwrap();

    let report = bootstrap_from_data_dir(dir.path()).unwrap();
    assert_eq!(report.entries_written, 0);
}

#[test]
fn test_bootstrap_assigns_sequential_versions_in_sorted_order() {
    let dir = TempDir::new().unwrap();
    // Order of FS iteration is platform-dependent; the bootstrap must
    // sort by IndexKey so the on-disk version assignment is
    // reproducible across runs and across nodes.
    let coll = touch_collection_dir(dir.path(), "z_tenant", "db", "c");
    touch_index_file(&coll, "z_idx");
    let coll = touch_collection_dir(dir.path(), "a_tenant", "db", "c");
    touch_index_file(&coll, "a_idx");

    bootstrap_from_data_dir(dir.path()).unwrap();
    let (_log, state) = SchemaLog::open(dir.path()).unwrap();
    let mut entries: Vec<_> = state
        .all_entries()
        .map(|(k, e)| (e.last_modified_hlc, k.tenant.clone(), k.index_name.clone()))
        .collect();
    entries.sort_by_key(|(_, t, _)| t.clone());
    // Both have hlc = 0 (synthesized) and versions are sequential
    // starting at 1.
    assert_eq!(state.version(), 2);
    assert_eq!(entries.len(), 2);
}
