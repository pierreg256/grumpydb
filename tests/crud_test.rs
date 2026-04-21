//! Integration tests for GrumpyDB CRUD operations.

use grumpydb::{GrumpyDb, GrumpyError, Value};
use std::collections::BTreeMap;
use tempfile::TempDir;
use uuid::Uuid;

fn setup() -> (TempDir, GrumpyDb) {
    let dir = TempDir::new().unwrap();
    let db = GrumpyDb::open(dir.path().join("testdb").as_path()).unwrap();
    (dir, db)
}

#[test]
fn test_crud_full_lifecycle() {
    let (_dir, mut db) = setup();
    let key = Uuid::new_v4();

    // Insert
    db.insert(key, Value::String("v1".into())).unwrap();
    assert_eq!(db.get(&key).unwrap(), Some(Value::String("v1".into())));

    // Update
    db.update(&key, Value::String("v2".into())).unwrap();
    assert_eq!(db.get(&key).unwrap(), Some(Value::String("v2".into())));

    // Delete
    db.delete(&key).unwrap();
    assert_eq!(db.get(&key).unwrap(), None);

    // Delete again → error
    assert!(matches!(db.delete(&key), Err(GrumpyError::KeyNotFound(_))));
}

#[test]
fn test_bulk_insert_and_verify() {
    let (_dir, mut db) = setup();
    let count = 1_000;
    let mut keys = Vec::with_capacity(count);

    for i in 0..count {
        let key = Uuid::from_u128(i as u128);
        db.insert(key, Value::Integer(i as i64)).unwrap();
        keys.push(key);
    }

    for (i, key) in keys.iter().enumerate() {
        let val = db.get(key).unwrap();
        assert_eq!(val, Some(Value::Integer(i as i64)), "key {i} mismatch");
    }
}

#[test]
fn test_bulk_delete() {
    let (_dir, mut db) = setup();
    let count = 500u128;

    for i in 0..count {
        db.insert(Uuid::from_u128(i), Value::Integer(i as i64)).unwrap();
    }

    // Delete first half
    for i in 0..count / 2 {
        db.delete(&Uuid::from_u128(i)).unwrap();
    }

    // Verify first half gone
    for i in 0..count / 2 {
        assert_eq!(db.get(&Uuid::from_u128(i)).unwrap(), None);
    }

    // Verify second half still present
    for i in count / 2..count {
        assert!(db.get(&Uuid::from_u128(i)).unwrap().is_some());
    }
}

#[test]
fn test_scan_range_ordered() {
    let (_dir, mut db) = setup();

    for i in 0u128..50 {
        db.insert(Uuid::from_u128(i), Value::Integer(i as i64)).unwrap();
    }

    let start = Uuid::from_u128(10);
    let end = Uuid::from_u128(20);
    let results = db.scan(start..end).unwrap();

    assert_eq!(results.len(), 10);

    // Verify sorted
    for i in 1..results.len() {
        assert!(results[i - 1].0 < results[i].0, "scan not sorted");
    }

    // Verify range
    for (key, _) in &results {
        let v = key.as_u128();
        assert!(v >= 10 && v < 20);
    }
}

#[test]
fn test_scan_full() {
    let (_dir, mut db) = setup();

    for i in 0u128..30 {
        db.insert(Uuid::from_u128(i), Value::Integer(i as i64)).unwrap();
    }

    let all = db.scan(..).unwrap();
    assert_eq!(all.len(), 30);
}

#[test]
fn test_duplicate_key_error() {
    let (_dir, mut db) = setup();
    let key = Uuid::new_v4();
    db.insert(key, Value::Null).unwrap();
    let result = db.insert(key, Value::Null);
    assert!(matches!(result, Err(GrumpyError::DuplicateKey(_))));
}

#[test]
fn test_get_update_delete_nonexistent() {
    let (_dir, mut db) = setup();
    let missing = Uuid::new_v4();

    assert_eq!(db.get(&missing).unwrap(), None);
    assert!(matches!(db.update(&missing, Value::Null), Err(GrumpyError::KeyNotFound(_))));
    assert!(matches!(db.delete(&missing), Err(GrumpyError::KeyNotFound(_))));
}

#[test]
fn test_persistence_across_reopen() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("persist_test");

    let mut keys = Vec::new();
    {
        let mut db = GrumpyDb::open(&db_path).unwrap();
        for i in 0u128..50 {
            let key = Uuid::from_u128(i);
            db.insert(key, Value::Integer(i as i64)).unwrap();
            keys.push(key);
        }
        db.close().unwrap();
    }

    {
        let mut db = GrumpyDb::open(&db_path).unwrap();
        for (i, key) in keys.iter().enumerate() {
            let val = db.get(key).unwrap();
            assert_eq!(val, Some(Value::Integer(i as i64)), "key {i} not found after reopen");
        }
    }
}

#[test]
fn test_complex_documents() {
    let (_dir, mut db) = setup();

    for i in 0..20 {
        let key = Uuid::from_u128(i);
        let value = Value::Object(BTreeMap::from([
            ("id".into(), Value::Integer(i as i64)),
            ("name".into(), Value::String(format!("item_{i}"))),
            ("active".into(), Value::Bool(i % 2 == 0)),
            ("tags".into(), Value::Array(vec![
                Value::String("tag1".into()),
                Value::String("tag2".into()),
            ])),
        ]));
        db.insert(key, value).unwrap();
    }

    for i in 0u128..20 {
        let val = db.get(&Uuid::from_u128(i)).unwrap().unwrap();
        let obj = val.as_object().unwrap();
        assert_eq!(obj.get("id"), Some(&Value::Integer(i as i64)));
    }
}

#[test]
fn test_overflow_document_crud() {
    let (_dir, mut db) = setup();
    let key = Uuid::new_v4();
    let large = Value::String("X".repeat(20_000));

    db.insert(key, large.clone()).unwrap();
    assert_eq!(db.get(&key).unwrap(), Some(large));

    db.delete(&key).unwrap();
    assert_eq!(db.get(&key).unwrap(), None);
}
