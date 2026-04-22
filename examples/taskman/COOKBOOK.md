# GrumpyDB Cookbook

Self-contained recipes for common GrumpyDB tasks. Each recipe is copy-pasteable.

---

## Recipe 1: Store a Rust struct in GrumpyDB

```rust
use grumpydb::{GrumpyDb, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

struct User {
    name: String,
    email: String,
    age: i64,
}

impl User {
    fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            ("name".into(), Value::String(self.name.clone())),
            ("email".into(), Value::String(self.email.clone())),
            ("age".into(), Value::Integer(self.age)),
        ]))
    }

    fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        Some(User {
            name: obj.get("name")?.as_str()?.to_string(),
            email: obj.get("email")?.as_str()?.to_string(),
            age: *obj.get("age")?.as_i64()?,
        })
    }
}

// Usage:
let mut db = GrumpyDb::open(std::path::Path::new("./users")).unwrap();
let key = Uuid::new_v4();
let user = User { name: "Alice".into(), email: "alice@example.com".into(), age: 30 };
db.insert(key, user.to_value()).unwrap();

// Read back:
let value = db.get(&key).unwrap().unwrap();
let user = User::from_value(&value).unwrap();
assert_eq!(user.name, "Alice");
```

---

## Recipe 2: Iterate over all documents

```rust
// scan(..) with unbounded range returns ALL documents sorted by key
let all_docs = db.scan(..).unwrap();
for (key, value) in &all_docs {
    println!("{}: {:?}", key, value);
}
println!("Total: {} documents", all_docs.len());

// Or use document_count() for O(1) count (no scan):
let count = db.document_count();
```

---

## Recipe 3: Filter documents by field value

```rust
// GrumpyDB has no query language — filter in application code after scan
let all = db.scan(..).unwrap();

let active_users: Vec<_> = all
    .iter()
    .filter(|(_, v)| {
        v.as_object()
            .and_then(|obj| obj.get("active"))
            .and_then(|v| v.as_bool())
            == Some(&true)
    })
    .collect();

println!("Active users: {}", active_users.len());
```

---

## Recipe 4: Handle a missing key gracefully

```rust
use grumpydb::GrumpyError;

// get() returns None for missing keys (not an error)
match db.get(&key).unwrap() {
    Some(value) => println!("Found: {:?}", value),
    None => println!("Key not found — this is normal"),
}

// update() and delete() return KeyNotFound for missing keys
match db.update(&key, Value::Null) {
    Ok(()) => println!("Updated"),
    Err(GrumpyError::KeyNotFound(_)) => println!("Nothing to update"),
    Err(e) => eprintln!("Unexpected error: {e}"),
}

// insert() returns DuplicateKey if the key already exists
match db.insert(key, Value::Null) {
    Ok(()) => println!("Inserted"),
    Err(GrumpyError::DuplicateKey(_)) => println!("Key already exists"),
    Err(e) => eprintln!("Unexpected error: {e}"),
}
```

---

## Recipe 5: Bulk import data

```rust
// Insert many documents in a loop. Each insert is individually WAL-committed.
// GrumpyDB auto-checkpoints every 100 writes for efficiency.
let items = vec![("item1", 10), ("item2", 20), ("item3", 30)];

for (name, price) in items {
    let key = Uuid::new_v4();
    let value = Value::Object(BTreeMap::from([
        ("name".into(), Value::String(name.into())),
        ("price".into(), Value::Integer(price)),
    ]));
    db.insert(key, value).unwrap();
}

// Optional: flush to ensure everything is on disk immediately
db.flush().unwrap();
```

---

## Recipe 6: Use GrumpyDB from multiple threads

```rust
use grumpydb::SharedDb;
use grumpydb::Value;
use uuid::Uuid;

// SharedDb wraps GrumpyDb in Arc<RwLock> for thread-safe access
let db = SharedDb::open(std::path::Path::new("./shared_db")).unwrap();

// Spawn a writer thread
let db_writer = db.clone();
let writer = std::thread::spawn(move || {
    for i in 0..100 {
        let key = Uuid::from_u128(i);
        db_writer.insert(key, Value::Integer(i as i64)).unwrap();
    }
});

// Spawn reader threads
let mut readers = Vec::new();
for _ in 0..4 {
    let db_reader = db.clone();
    readers.push(std::thread::spawn(move || {
        // Readers can run concurrently (shared lock)
        let count = db_reader.document_count();
        println!("Reader sees {count} documents");
    }));
}

writer.join().unwrap();
for r in readers { r.join().unwrap(); }

// Clean up
db.flush().unwrap();
db.close().unwrap();
```

---

## Recipe 7: Compact after bulk deletes

```rust
// After deleting many documents, data pages have gaps (tombstones).
// Compaction rewrites everything into fresh, packed pages.

for i in 0u128..1000 {
    db.insert(Uuid::from_u128(i), Value::Integer(i as i64)).unwrap();
}

// Delete half
for i in 0u128..500 {
    db.delete(&Uuid::from_u128(i)).unwrap();
}

// Compact: defragments data + rebuilds index
let result = db.compact().unwrap();
println!("Compaction preserved {} documents", result.documents);

// All surviving documents are still accessible
assert!(db.get(&Uuid::from_u128(500)).unwrap().is_some());
```
