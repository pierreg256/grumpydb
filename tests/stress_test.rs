//! Stress test: multi-tenant server with concurrent access.
//!
//! Tests 3 clients × 3 databases × 3 collections × 1,000 documents = 27,000 docs.

use grumpydb::{GrumpyServer, SharedServer, Value};
use std::sync::{Arc, Barrier};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn test_stress_multi_tenant() {
    let dir = TempDir::new().unwrap();
    let mut server = GrumpyServer::open(dir.path().join("root").as_path()).unwrap();

    // Create 3 clients × 3 databases × 3 collections
    for c in 0..3 {
        let client_name = format!("client{c}");
        server.create_client(&client_name).unwrap();
        let client = server.client(&client_name).unwrap();
        for d in 0..3 {
            let db_name = format!("db{d}");
            client.create_database(&db_name).unwrap();
            let db = client.database(&db_name).unwrap();
            for col in 0..3 {
                db.create_collection(&format!("coll{col}")).unwrap();
            }
        }
    }

    // Insert 1,000 docs into each of the 27 collections
    for c in 0..3 {
        let client_name = format!("client{c}");
        let client = server.client(&client_name).unwrap();
        for d in 0..3 {
            let db_name = format!("db{d}");
            let db = client.database(&db_name).unwrap();
            for col in 0..3 {
                let coll_name = format!("coll{col}");
                for i in 0u128..1_000 {
                    let key = Uuid::from_u128(
                        c as u128 * 1_000_000 + d as u128 * 100_000 + col as u128 * 10_000 + i,
                    );
                    db.insert(
                        &coll_name,
                        key,
                        Value::Object(std::collections::BTreeMap::from([
                            ("client".into(), Value::String(client_name.clone())),
                            ("db".into(), Value::String(db_name.clone())),
                            ("coll".into(), Value::String(coll_name.clone())),
                            ("idx".into(), Value::Integer(i as i64)),
                        ])),
                    )
                    .unwrap();
                }
            }
        }
    }

    // Verify counts: each collection should have 1,000 docs
    for c in 0..3 {
        let client = server.client(&format!("client{c}")).unwrap();
        for d in 0..3 {
            let db = client.database(&format!("db{d}")).unwrap();
            for col in 0..3 {
                let count = db.document_count(&format!("coll{col}")).unwrap();
                assert_eq!(
                    count, 1_000,
                    "client{c}/db{d}/coll{col} should have 1000 docs"
                );
            }
        }
    }

    // Verify isolation: spot-check a few documents
    let client0 = server.client("client0").unwrap();
    let db0 = client0.database("db0").unwrap();
    let val = db0.get("coll0", &Uuid::from_u128(42)).unwrap().unwrap();
    assert_eq!(
        val.as_object().unwrap().get("client"),
        Some(&Value::String("client0".into()))
    );

    server.close().unwrap();
}

#[test]
fn test_stress_concurrent_multi_database() {
    let dir = TempDir::new().unwrap();
    let server = SharedServer::open(dir.path().join("root").as_path()).unwrap();
    server.create_client("test").unwrap();

    // Create 4 databases, each with a "data" collection
    for i in 0..4 {
        server.create_database("test", &format!("db{i}")).unwrap();
        let db = server.database("test", &format!("db{i}")).unwrap();
        db.create_collection("data").unwrap();
    }

    // 8 threads, 2 per database, each inserting 500 docs
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();

    for t in 0..8u128 {
        let server = server.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            let db_idx = t % 4;
            let db = server.database("test", &format!("db{db_idx}")).unwrap();
            barrier.wait();
            for i in 0..500 {
                let key = Uuid::from_u128(t * 10_000 + i);
                db.insert("data", key, Value::Integer((t * 10_000 + i) as i64))
                    .unwrap();
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // Each database should have 1,000 docs (2 threads × 500)
    for i in 0..4 {
        let db = server.database("test", &format!("db{i}")).unwrap();
        assert_eq!(db.document_count("data").unwrap(), 1_000);
    }
}
