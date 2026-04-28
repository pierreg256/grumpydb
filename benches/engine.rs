//! Criterion benchmarks for the GrumpyDB storage engine.
//!
//! Run with `cargo bench --bench engine` (full mode) or
//! `cargo bench --bench engine -- --quick` (smoke run).

use std::collections::BTreeMap;
use std::path::PathBuf;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use grumpydb::{Database, Value};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use tempfile::TempDir;
use uuid::Uuid;

const COLLECTION: &str = "bench";

// ── Value builders ──────────────────────────────────────────────────────

fn small_value(i: i64) -> Value {
    let mut map = BTreeMap::new();
    map.insert("name".into(), Value::String(format!("user_{i}")));
    map.insert("age".into(), Value::Integer(i % 1000));
    map.insert("active".into(), Value::Bool(i % 2 == 0));
    Value::Object(map)
}

fn medium_value(i: i64) -> Value {
    let mut map = BTreeMap::new();
    map.insert("id".into(), Value::Integer(i));
    map.insert("name".into(), Value::String(format!("user_{i}")));
    map.insert(
        "email".into(),
        Value::String(format!("user{i}@example.com")),
    );
    map.insert("age".into(), Value::Integer(i % 1000));
    map.insert("active".into(), Value::Bool(i % 2 == 0));
    map.insert("score".into(), Value::Float((i as f64) * 1.5));
    map.insert("country".into(), Value::String("FR".to_string()));
    map.insert("city".into(), Value::String("Paris".to_string()));
    map.insert(
        "tags".into(),
        Value::Array(vec![
            Value::String("alpha".into()),
            Value::String("beta".into()),
            Value::String("gamma".into()),
        ]),
    );
    map.insert("bio".into(), Value::String("a".repeat(300)));
    Value::Object(map)
}

fn large_value(i: i64) -> Value {
    let mut map = BTreeMap::new();
    map.insert("id".into(), Value::Integer(i));
    // 4 KB string forces overflow page allocation.
    map.insert("payload".into(), Value::String("x".repeat(4096)));
    Value::Object(map)
}

// ── Setup helpers ───────────────────────────────────────────────────────

/// Create a fresh database in a tempdir, with one empty collection ready to use.
fn fresh_db() -> (TempDir, Database) {
    let dir = TempDir::new().expect("create tempdir");
    let path: PathBuf = dir.path().join("db");
    let mut db = Database::open(&path).expect("open db");
    db.create_collection(COLLECTION).expect("create collection");
    db.flush().expect("flush");
    (dir, db)
}

/// Create a fresh database, populate it with `n` documents using `make_value`,
/// and return the keys (in insertion order) plus the database.
fn populated_db(
    n: usize,
    mut make_value: impl FnMut(i64) -> Value,
) -> (TempDir, Database, Vec<Uuid>) {
    let (dir, mut db) = fresh_db();
    let mut keys = Vec::with_capacity(n);
    for i in 0..n {
        let key = Uuid::new_v4();
        db.insert(COLLECTION, key, make_value(i as i64))
            .expect("insert");
        keys.push(key);
    }
    db.flush().expect("flush");
    (dir, db, keys)
}

// ── Insert benches ──────────────────────────────────────────────────────

fn bench_insert_small(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_small");
    group.throughput(Throughput::Elements(1_000));
    group.sample_size(10);
    group.bench_function("1000_docs", |b| {
        b.iter_batched(
            fresh_db,
            |(dir, mut db)| {
                for i in 0..1_000 {
                    let key = Uuid::new_v4();
                    db.insert(COLLECTION, key, small_value(i)).expect("insert");
                }
                drop(db);
                drop(dir);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_insert_medium(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_medium");
    group.throughput(Throughput::Elements(1_000));
    group.sample_size(10);
    group.bench_function("1000_docs", |b| {
        b.iter_batched(
            fresh_db,
            |(dir, mut db)| {
                for i in 0..1_000 {
                    let key = Uuid::new_v4();
                    db.insert(COLLECTION, key, medium_value(i)).expect("insert");
                }
                drop(db);
                drop(dir);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_insert_large(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_large");
    group.throughput(Throughput::Elements(100));
    group.sample_size(10);
    group.bench_function("100_docs_4kb", |b| {
        b.iter_batched(
            fresh_db,
            |(dir, mut db)| {
                for i in 0..100 {
                    let key = Uuid::new_v4();
                    db.insert(COLLECTION, key, large_value(i)).expect("insert");
                }
                drop(db);
                drop(dir);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

// ── Get benches ─────────────────────────────────────────────────────────

fn bench_get_by_uuid_cached(c: &mut Criterion) {
    let (_dir, mut db, keys) = populated_db(10_000, small_value);
    let mut rng = StdRng::seed_from_u64(0xC0FFEE);
    let sample: Vec<Uuid> = (0..1_000)
        .map(|_| keys[rng.gen_range(0..keys.len())])
        .collect();

    let mut group = c.benchmark_group("get_by_uuid_cached");
    group.throughput(Throughput::Elements(1_000));
    group.sample_size(20);
    group.bench_function("1000_lookups", |b| {
        b.iter(|| {
            for k in &sample {
                let v = db.get(COLLECTION, k).expect("get");
                debug_assert!(v.is_some());
            }
        });
    });
    group.finish();
}

fn bench_get_by_uuid_cold(c: &mut Criterion) {
    // Build the corpus once, but reopen the DB before every iteration to flush
    // the buffer pool. We seed RNG so the access pattern is deterministic.
    let (dir, db, keys) = populated_db(10_000, small_value);
    drop(db); // ensure the on-disk state is closed before reopen
    let db_path = dir.path().join("db");

    let mut rng = StdRng::seed_from_u64(0xCAFEBABE);
    let sample: Vec<Uuid> = (0..1_000)
        .map(|_| keys[rng.gen_range(0..keys.len())])
        .collect();

    let mut group = c.benchmark_group("get_by_uuid_cold");
    group.throughput(Throughput::Elements(1_000));
    group.sample_size(10);
    group.bench_function("1000_lookups", |b| {
        b.iter_batched(
            || Database::open(&db_path).expect("reopen"),
            |mut db| {
                for k in &sample {
                    let v = db.get(COLLECTION, k).expect("get");
                    debug_assert!(v.is_some());
                }
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();

    // Keep the tempdir alive until the bench is done.
    drop(dir);
}

// ── Scan bench ──────────────────────────────────────────────────────────

fn bench_scan_full_collection(c: &mut Criterion) {
    let (_dir, mut db, _keys) = populated_db(10_000, small_value);

    let mut group = c.benchmark_group("scan_full_collection");
    group.throughput(Throughput::Elements(10_000));
    group.sample_size(10);
    group.bench_function("10k_docs", |b| {
        b.iter(|| {
            let docs = db.scan(COLLECTION, ..).expect("scan");
            debug_assert_eq!(docs.len(), 10_000);
        });
    });
    group.finish();
}

// ── Index benches ───────────────────────────────────────────────────────

/// Build a populated DB with 10 000 docs and a secondary index on `age`.
/// Ages are distributed uniformly in 0..1000.
fn populated_db_with_index() -> (TempDir, Database) {
    let (dir, mut db) = fresh_db();
    for i in 0..10_000_i64 {
        let mut map = BTreeMap::new();
        map.insert("name".into(), Value::String(format!("user_{i}")));
        map.insert("age".into(), Value::Integer(i % 1000));
        map.insert("active".into(), Value::Bool(i % 2 == 0));
        db.insert(COLLECTION, Uuid::new_v4(), Value::Object(map))
            .expect("insert");
    }
    db.create_index(COLLECTION, "age_idx", "age")
        .expect("create index");
    db.flush().expect("flush");
    (dir, db)
}

fn bench_index_query_exact(c: &mut Criterion) {
    let (_dir, mut db) = populated_db_with_index();
    let mut rng = StdRng::seed_from_u64(0xBADF00D);
    let lookups: Vec<i64> = (0..1_000).map(|_| rng.gen_range(0..1000)).collect();

    let mut group = c.benchmark_group("index_query_exact");
    group.throughput(Throughput::Elements(1_000));
    group.sample_size(10);
    group.bench_function("1000_lookups", |b| {
        b.iter(|| {
            for v in &lookups {
                let res = db
                    .query(COLLECTION, "age_idx", &Value::Integer(*v))
                    .expect("query");
                debug_assert!(!res.is_empty());
            }
        });
    });
    group.finish();
}

fn bench_index_query_range(c: &mut Criterion) {
    let (_dir, mut db) = populated_db_with_index();
    let mut rng = StdRng::seed_from_u64(0xFEEDFACE);

    // Pre-generate 100 random ranges, each of width up to 50 so each query
    // returns a meaningful number of matches without becoming a near-full scan.
    let mut ranges: Vec<(i64, i64)> = (0..100)
        .map(|_| {
            let a = rng.gen_range(0..950);
            let width = rng.gen_range(1..=50);
            (a, a + width)
        })
        .collect();
    ranges.shuffle(&mut rng);

    let mut group = c.benchmark_group("index_query_range");
    group.throughput(Throughput::Elements(100));
    group.sample_size(10);
    group.bench_function("100_ranges", |b| {
        b.iter(|| {
            for (a, b_end) in &ranges {
                let res = db
                    .query_range(
                        COLLECTION,
                        "age_idx",
                        &Value::Integer(*a),
                        &Value::Integer(*b_end),
                    )
                    .expect("query_range");
                debug_assert!(!res.is_empty());
            }
        });
    });
    group.finish();
}

criterion_group!(
    engine_benches,
    bench_insert_small,
    bench_insert_medium,
    bench_insert_large,
    bench_get_by_uuid_cached,
    bench_get_by_uuid_cold,
    bench_scan_full_collection,
    bench_index_query_exact,
    bench_index_query_range,
);
criterion_main!(engine_benches);
