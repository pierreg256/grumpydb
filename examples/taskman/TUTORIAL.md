# GrumpyDB Tutorial

Learn how to build a complete application with GrumpyDB by studying the TaskMan example.
Each chapter maps to specific code in this directory.

---

## Chapter 1: Getting Started

### Opening a database

```rust
use grumpydb::GrumpyDb;
use std::path::Path;

// GrumpyDb::open() creates the directory and files if they don't exist.
// Files created: data.db (documents), index.db (B+Tree), wal.log (Write-Ahead Log)
let mut db = GrumpyDb::open(Path::new("./my_database")).unwrap();
```

### Inserting your first document

```rust
use grumpydb::Value;
use uuid::Uuid;

let key = Uuid::new_v4();
let value = Value::String("Hello, GrumpyDB!".into());

db.insert(key, value).unwrap();
```

### Closing

```rust
db.close().unwrap(); // flushes all data to disk
```

**See:** [`store.rs → TaskStore::open()`](store.rs) for the full pattern.

---

## Chapter 2: Data Modeling

GrumpyDB stores `Value` types — a JSON-like enum:

```rust
use grumpydb::Value;
use std::collections::BTreeMap;

// Primitives
let null = Value::Null;
let boolean = Value::Bool(true);
let integer = Value::Integer(42);
let float = Value::Float(3.14);
let string = Value::String("hello".into());

// Collections
let array = Value::Array(vec![Value::Integer(1), Value::Integer(2)]);
let object = Value::Object(BTreeMap::from([
    ("name".into(), Value::String("Alice".into())),
    ("age".into(), Value::Integer(30)),
]));
```

### Converting Rust structs to/from Value

The recommended pattern is to implement `to_value()` and `from_value()` methods:

```rust
struct Task {
    title: String,
    done: bool,
}

impl Task {
    fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            ("title".into(), Value::String(self.title.clone())),
            ("done".into(), Value::Bool(self.done)),
        ]))
    }

    fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        Some(Task {
            title: obj.get("title")?.as_str()?.to_string(),
            done: *obj.get("done")?.as_bool()?,
        })
    }
}
```

**See:** [`task.rs → Task::to_value() / Task::from_value()`](task.rs) for the full implementation.

---

## Chapter 3: Querying

### Get by key (O(log n))

```rust
let value = db.get(&key).unwrap(); // Returns Option<Value>
match value {
    Some(v) => println!("Found: {:?}", v),
    None => println!("Not found"),
}
```

### Scan all documents

```rust
let all = db.scan(..).unwrap(); // Vec<(Uuid, Value)>
for (key, value) in &all {
    println!("{}: {:?}", key, value);
}
```

### Range scan

```rust
let start = Uuid::from_u128(100);
let end = Uuid::from_u128(200);
let range = db.scan(start..end).unwrap(); // keys in [100, 200)
```

### Filtering (application-level)

GrumpyDB is a key-value store — there's no WHERE clause. Filter in your code:

```rust
let tasks = db.scan(..).unwrap();
let done_tasks: Vec<_> = tasks
    .iter()
    .filter(|(_, v)| v.as_object()
        .and_then(|o| o.get("done"))
        .and_then(|v| v.as_bool())
        == Some(&true))
    .collect();
```

**See:** [`store.rs → list_all_tasks() / list_by_status()`](store.rs)

---

## Chapter 4: Updates & Deletes

### Full replacement update

```rust
// GrumpyDB update replaces the entire document
db.update(&key, Value::String("updated value".into())).unwrap();
```

### Read-modify-write pattern

For partial updates, read → modify → write back:

```rust
// 1. Read current value
let mut task = db.get(&key).unwrap().unwrap();

// 2. Modify (assuming it's an Object)
if let Value::Object(ref mut obj) = task {
    obj.insert("done".into(), Value::Bool(true));
}

// 3. Write back
db.update(&key, task).unwrap();
```

### Delete

```rust
db.delete(&key).unwrap(); // Removes from data pages + B+Tree index
```

**See:** [`store.rs → set_task_done()`](store.rs) for the read-modify-write pattern.

---

## Chapter 5: Durability

### How the WAL works

Every write follows this protocol:
1. **Log** the page changes to `wal.log` (before + after images)
2. **Fsync** the WAL to disk (commit record)
3. **Write** the actual data pages (may be buffered)

If the process crashes:
- On next `open()`, the WAL is replayed automatically
- Committed transactions are redone (after-images applied)
- Uncommitted transactions are undone (before-images restored)

### Flushing

```rust
db.flush().unwrap();
// This:
// 1. Flushes all dirty pages from the buffer pool
// 2. Syncs data.db and index.db to disk
// 3. Writes a WAL checkpoint
// 4. Truncates the WAL (no longer needed)
```

### Auto-checkpoint

GrumpyDB automatically checkpoints every 100 writes. You don't need to call
`flush()` unless you want immediate durability.

**See:** [`examples/taskman/test_crash.sh`](test_crash.sh) for a crash simulation test.

---

## Chapter 6: Performance

### Buffer pool

GrumpyDB caches data pages in an LRU buffer pool (256 frames = 2 MiB by default):

```rust
// Default pool size
let db = GrumpyDb::open(path).unwrap();

// Custom pool size for large datasets
let db = GrumpyDb::open_with_pool_capacity(path, 1024).unwrap(); // 8 MiB
```

### Monitoring

```rust
let (reads, writes, cached, capacity) = db.pool_stats();
println!("Cache: {cached}/{capacity} pages, {reads} disk reads, {writes} disk writes");
```

### Compaction

After many deletes, compact to reclaim space:

```rust
let result = db.compact().unwrap();
println!("Preserved {} documents", result.documents);
```

### Complexity

| Operation | Time complexity |
|-----------|----------------|
| `get()` | O(log n) via B+Tree |
| `insert()` | O(log n) |
| `delete()` | O(log n) |
| `scan(range)` | O(log n + k) where k = results |
| `scan(..)` | O(n) full scan |
| `document_count()` | O(1) from metadata |

**See:** [`PERFORMANCE.md`](PERFORMANCE.md) for buffer pool details.

---

## Chapter 7: Concurrency

### SWMR model (Single-Writer, Multi-Reader)

```rust
use grumpydb::SharedDb;

let db = SharedDb::open(path).unwrap();

// Clone for another thread (cheap: Arc clone)
let db2 = db.clone();

// Writer thread (exclusive lock)
std::thread::spawn(move || {
    db2.insert(key, value).unwrap();
});

// Reader thread (shared lock — but currently uses write lock due to &mut self)
let value = db.get(&key).unwrap();
```

### Thread sharing pattern

```rust
use std::sync::Arc;
use std::sync::Barrier;

let db = SharedDb::open(path).unwrap();
let barrier = Arc::new(Barrier::new(num_threads));

let mut handles = Vec::new();
for _ in 0..num_threads {
    let db = db.clone();       // cheap Arc clone
    let barrier = barrier.clone();
    handles.push(std::thread::spawn(move || {
        barrier.wait();        // synchronize start
        // ... use db ...
    }));
}

for h in handles {
    h.join().unwrap();
}
```

**See:** [`concurrent.rs → run_bench() / run_server()`](concurrent.rs) for real-world examples.
