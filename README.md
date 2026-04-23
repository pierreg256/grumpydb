<p align="center">
  <img src="docs/grumpy-logo.png" alt="GrumpyDB logo" width="200">
</p>

# GrumpyDB

A disk-based object storage engine written in Rust. GrumpyDB stores schema-less documents (JSON-like) with B+Tree indexing, page-based storage, WAL for durability, and SWMR concurrency.

## Features

| Feature | Status |
|---------|--------|
| Page-based storage (8 KiB pages, slotted layout, overflow) | ✅ Implemented |
| B+Tree index (search, insert, delete, range scan) | ✅ Implemented |
| Document model (JSON-like Value type, binary codec) | ✅ Implemented |
| Storage engine (CRUD API) | ✅ Implemented |
| Write-Ahead Log (crash recovery) | ✅ Implemented |
| Buffer pool (LRU cache) | ✅ Implemented |
| SWMR concurrency | ✅ Implemented |
| Page checksums (CRC32 integrity) | ✅ Implemented |
| Compaction (defrag + index rebuild) | ✅ Implemented |
| Variable-key B+Tree (secondary indexes) | ✅ Implemented |
| Collection abstraction (unit of storage) | ✅ Implemented |
| Secondary indexes (field-level queries) | ✅ Implemented |
| Multi-collection database | ✅ Implemented |
| Document references (cross-collection Ref, resolve, cycle detection) | ✅ Implemented |
| GrumpyShell interactive REPL | ✅ Implemented |

## Getting started

### Prerequisites

- Rust (edition 2024)

### Build

```bash
cargo build
```

### Test

```bash
cargo test
```

### Lint

```bash
cargo clippy -- -D warnings
```

## Usage

```rust
use grumpydb::{GrumpyDb, Value};
use uuid::Uuid;
use std::collections::BTreeMap;

let mut db = GrumpyDb::open(std::path::Path::new("./my_database")).unwrap();

let key = Uuid::new_v4();
let value = Value::Object(BTreeMap::from([
    ("name".into(), Value::String("GrumpyDB".into())),
    ("version".into(), Value::Integer(1)),
]));

db.insert(key, value).unwrap();

let doc = db.get(&key).unwrap();
assert!(doc.is_some());

db.close().unwrap();
```

> **Note**: The full CRUD API (`insert`, `get`, `update`, `delete`, `scan`) is functional with WAL durability, LRU buffer pool caching, SWMR concurrency, page checksums, and compaction. The `Database` API provides multi-collection support with secondary indexes.

## GrumpyShell — Interactive REPL

GrumpyShell provides a JavaScript-like interactive shell for exploring GrumpyDB:

```bash
cargo run --example grumpysh                           # launch REPL
cargo run --example grumpysh -- --data ./mydata        # custom data dir
cargo run --example grumpysh -- --eval "use test; db.users.count()"  # one-shot
```

```js
grumpy> use demo
grumpy [demo]> db.createCollection("users")
grumpy [demo]> db.users.insert({ name: "Alice", age: 30 })
Inserted: 3df9dde6-...
grumpy [demo]> db.users.createIndex("by_age", "age")
Index "by_age" created on field "age"
grumpy [demo]> db.users.query("by_age", 30)
[{ "_id": "...", "age": 30, "name": "Alice" }]
grumpy [demo]> db.users.find({ age: 30 })
[{ "_id": "...", "age": 30, "name": "Alice" }]
grumpy [demo]> db.orders.insert({ product: "widget", owner: $ref("users", "3df9dde6-...") })
Inserted: a1b2c3d4-...
grumpy [demo]> db.orders.resolve("a1b2c3d4")
{ ... resolved document ... }
grumpy [demo]> db.orders.resolveDeep("a1b2c3d4")
{ ... deeply resolved document ... }
```

Features: relaxed JSON (unquoted keys, single quotes), secondary index queries, client-side filtering, document references (`$ref()`), reference resolution (`resolve`, `resolveDeep`), line editing with history.

## Demo App & Tutorial

The `examples/taskman/` directory contains a fully documented task manager CLI that demonstrates every GrumpyDB feature:

- **[Tutorial](examples/taskman/TUTORIAL.md)** — 7-chapter guide: getting started, data modeling, querying, updates, durability, performance, concurrency
- **[Cookbook](examples/taskman/COOKBOOK.md)** — 7 self-contained recipes for common tasks (struct storage, iteration, filtering, bulk import, threading, compaction)
- **[Performance Guide](examples/taskman/PERFORMANCE.md)** — buffer pool architecture, tuning, and benchmarking

```bash
cargo run --example taskman -- help
```

## Architecture

```
┌──────────────────────────────────────┐
│         Public API (lib.rs)          │
├──────────────────────────────────────┤
│  Database (database/) + Engine (engine.rs) │
├──────────────────────────────────────┤
│     Collection (collection/) + Indexes    │
├────────────┬─────────────┬────────────┤
│  Document  │  Concurrency │  Buffer   │
│  Model     │  (SWMR)      │  Pool     │
├────────────┼─────────────┼────────────┤
│  B+Tree    │     WAL     │  Page     │
│  Index     │             │  Manager  │
│(primary.idx)│  (wal.log)  │ (data.db) │
└────────────┴─────────────┴────────────┘
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for technical details.

## Project structure

```
src/
├── lib.rs              # Public API, re-exports
├── error.rs            # GrumpyError, Result type
├── engine.rs           # GrumpyDb — thin wrapper over Collection + WAL
├── naming.rs           # Name validation: [a-z0-9_]{1,64}
├── database/           # Database — multi-collection management
│   └── mod.rs          # Database struct, CRUD routing, shared WAL
├── collection/         # Collection — unit of document storage
│   └── mod.rs          # Collection struct, raw CRUD, compact, secondary indexes
├── index/              # Secondary indexes on document fields
│   ├── mod.rs          # SecondaryIndex struct, IndexDefinition, lookup, range_query
│   └── encoding.rs     # Sortable binary encoding for B+Tree keys
├── page/               # 8 KiB page management
│   ├── mod.rs          # Constants, PageHeader, PageType
│   ├── manager.rs      # PageManager (I/O, free-list)
│   ├── slotted.rs      # SlottedPage (variable-length tuples)
│   └── overflow.rs     # Overflow page chains
├── btree/              # B+Tree index
│   ├── mod.rs          # BTree struct, metadata
│   ├── node.rs         # InternalNode, LeafNode (fixed UUID keys)
│   ├── ops.rs          # search, insert, delete
│   ├── cursor.rs       # BTreeCursor, range scans
│   ├── key.rs          # Key encoding utilities
│   ├── var_node.rs     # VarInternalNode, VarLeafNode (variable keys)
│   ├── var_ops.rs      # VarBTree search/insert/delete
│   ├── var_tree.rs     # VarBTree struct
│   └── var_cursor.rs   # VarCursor, range scans
├── document/           # Document model
│   ├── mod.rs          # Document struct
│   ├── value.rs        # Value enum (JSON-like + Ref)
│   └── codec.rs        # Binary encode/decode
├── wal/                # Write-Ahead Log
├── buffer/             # Buffer pool LRU cache
└── concurrency/        # SWMR locks
```

## License

MIT
