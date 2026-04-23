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

> **Note**: The full CRUD API (`insert`, `get`, `update`, `delete`, `scan`) is functional with WAL durability, LRU buffer pool caching, SWMR concurrency, page checksums, and compaction.

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
│     Engine (engine.rs) + WAL         │
├──────────────────────────────────────┤
│     Collection (collection/)         │
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
├── collection/         # Collection — unit of document storage
│   └── mod.rs          # Collection struct, raw CRUD, compact
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
│   ├── value.rs        # Value enum (JSON-like)
│   └── codec.rs        # Binary encode/decode
├── wal/                # Write-Ahead Log
├── buffer/             # Buffer pool LRU cache
└── concurrency/        # SWMR locks
```

## License

MIT
