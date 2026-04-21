# GrumpyDB

A disk-based object storage engine written in Rust. GrumpyDB stores schema-less documents (JSON-like) with B+Tree indexing, page-based storage, WAL for durability, and SWMR concurrency.

## Features

| Feature | Status |
|---------|--------|
| Page-based storage (8 KiB pages, slotted layout, overflow) | ✅ Implemented |
| B+Tree index (search, insert, delete, range scan) | ✅ Implemented |
| Document model (JSON-like Value type, binary codec) | ✅ Implemented |
| Storage engine (CRUD API) | 🔲 Phase 4 |
| Write-Ahead Log (crash recovery) | 🔲 Phase 5 |
| Buffer pool (LRU cache) | 🔲 Phase 6 |
| SWMR concurrency | 🔲 Phase 7 |

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

let db = GrumpyDb::open(std::path::Path::new("./my_database")).unwrap();

let key = Uuid::new_v4();
let value = Value::Object(BTreeMap::from([
    ("name".into(), Value::String("GrumpyDB".into())),
    ("version".into(), Value::Integer(1)),
]));

db.insert(key, value).unwrap();

let doc = db.get(&key).unwrap();
assert!(doc.is_some());
```

> **Note** : L'API CRUD complète (`insert`, `get`, `update`, `delete`, `scan`) n'est pas encore câblée dans l'engine (Phase 4). L'exemple ci-dessus montre l'API cible.

## Architecture

```
┌──────────────────────────────────────┐
│         API publique (lib.rs)        │
├──────────────────────────────────────┤
│         Engine (engine.rs)           │
├────────────┬─────────────┬───────────┤
│  Document  │  Concurrency│  Buffer   │
│  Model     │  (SWMR)     │  Pool     │
├────────────┼─────────────┼───────────┤
│  B+Tree    │     WAL     │  Page     │
│  Index     │             │  Manager  │
│ (index.db) │  (wal.log)  │ (data.db) │
└────────────┴─────────────┴───────────┘
```

Voir [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) pour les détails techniques.

## Project structure

```
src/
├── lib.rs              # Public API, re-exports
├── error.rs            # GrumpyError, Result type
├── engine.rs           # GrumpyDb — CRUD orchestrator
├── page/               # 8 KiB page management
│   ├── mod.rs          # Constants, PageHeader, PageType
│   ├── manager.rs      # PageManager (I/O, free-list)
│   ├── slotted.rs      # SlottedPage (variable-length tuples)
│   └── overflow.rs     # Overflow page chains
├── btree/              # B+Tree index
│   ├── mod.rs          # BTree struct, metadata
│   ├── node.rs         # InternalNode, LeafNode
│   ├── ops.rs          # search, insert, delete
│   └── cursor.rs       # BTreeCursor, range scans
├── document/           # Document model
│   ├── mod.rs          # Document struct
│   ├── value.rs        # Value enum (JSON-like)
│   └── codec.rs        # Binary encode/decode
├── wal/                # Write-Ahead Log (Phase 5)
├── buffer/             # Buffer pool LRU (Phase 6)
└── concurrency/        # SWMR locks (Phase 7)
```

## License

MIT
