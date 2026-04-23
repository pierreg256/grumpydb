# Contributing to GrumpyDB

## Prerequisites

- **Rust** (edition 2024) — install via [rustup](https://rustup.rs/)
- `cargo`, `clippy`, `rustfmt` (included with rustup)

## Commands

```bash
cargo build                     # Build
cargo test                      # All tests (unit + integration)
cargo test --lib                # Unit tests only
cargo test --test '*'           # Integration tests only
cargo clippy -- -D warnings     # Lint (strict, zero warnings)
cargo fmt --check               # Check formatting
cargo doc --no-deps --open      # Generate docs
```

## Project structure

```
src/
├── error.rs            # GrumpyError enum, Result<T> alias
├── naming.rs           # Name validation: [a-z0-9_]{1,64}
├── database/           # Database — multi-collection management
│   └── mod.rs          # Database struct, CRUD routing, shared WAL, index management
├── collection/         # Collection — unit of document storage
│   └── mod.rs          # Collection struct, raw CRUD, compact, PageWriteRecord, secondary indexes
├── index/              # Secondary indexes on document fields
│   ├── mod.rs          # SecondaryIndex struct, IndexDefinition, lookup, range_query
│   └── encoding.rs     # Sortable binary encoding (integers, floats, strings, refs), extract_field
├── page/               # Page storage (8 KiB pages)
│   ├── mod.rs          # Constants (PAGE_SIZE, etc.), PageHeader, PageType
│   ├── manager.rs      # PageManager — disk I/O, free-list
│   ├── slotted.rs      # SlottedPage — variable-length tuple storage
│   └── overflow.rs     # Overflow page chains for large documents
├── btree/              # B+Tree index (separate file: primary.idx)
│   ├── mod.rs          # BTree struct, metadata (page 1)
│   ├── node.rs         # InternalNode, LeafNode, binary serialization
│   ├── ops.rs          # search, insert (with split), delete (with merge)
│   ├── cursor.rs       # BTreeCursor, range scans
│   ├── key.rs          # Key encoding utilities (VAR_KEY_MAX_SIZE, encode/decode)
│   ├── var_node.rs     # VarInternalNode, VarLeafNode (variable-length keys)
│   ├── var_ops.rs      # VarBTree search/insert/delete with split/merge
│   ├── var_tree.rs     # VarBTree struct, metadata persistence
│   └── var_cursor.rs   # VarCursor, range scans for variable keys
├── document/           # Document model
│   ├── mod.rs          # Document struct (UUID + Value)
│   ├── value.rs        # Value enum — schema-less JSON-like type + Ref
│   └── codec.rs        # Binary codec — encode/decode/encoded_size
├── wal/                # Write-Ahead Log
│   ├── mod.rs          # WAL module
│   ├── record.rs       # WalRecord binary format with CRC32
│   ├── writer.rs       # WalWriter (append, commit, checkpoint, truncate)
│   └── recovery.rs     # Redo/undo recovery from WAL records
├── buffer/             # Buffer pool LRU cache
│   ├── mod.rs          # Buffer module
│   ├── frame.rs        # BufferFrame (pin/unpin, dirty tracking)
│   └── pool.rs         # BufferPool (LRU eviction, I/O counters)
├── concurrency/        # SWMR lock manager
│   ├── mod.rs          # Concurrency module
│   ├── lock_manager.rs # SharedDb (Arc<RwLock<GrumpyDb>>)
│   └── shared.rs       # SharedDatabase + SharedServer (per-database SWMR)
├── server/             # Multi-tenant server
│   ├── mod.rs          # GrumpyServer — top-level multi-tenant management
│   └── client.rs       # Client — manages multiple databases per tenant
├── engine.rs           # GrumpyDb — thin wrapper over Collection + WAL
└── lib.rs              # Public API, re-exports
```

## Code conventions

### Naming

- `snake_case` for functions and variables
- `CamelCase` for types (structs, enums, traits)
- `UPPER_SNAKE_CASE` for constants
- Test functions: `test_<module>_<expected_behavior>`

### Visibility

- `pub(crate)` by default
- `pub` only for the public API in `lib.rs`

### Error handling

- Use `thiserror` for error definitions
- All functions return `Result<T, GrumpyError>`
- No `unwrap()` or `panic!` outside of tests

### Serialization

- All binary formats use **little-endian** byte order
- No `unsafe` code unless documented and justified

### Tests — mandatory

Every `.rs` file must have a `#[cfg(test)] mod tests` block.

Each feature needs at minimum:
- Happy path
- Edge cases (page full, overflow, B+Tree split)
- Error cases (I/O failure, missing key, corruption)

Use `tempfile::TempDir` for any test involving disk I/O.

## Workflow

1. Read the relevant skill file in `.claude/skills/` before coding
2. Implement the feature
3. Write unit tests in the same file
4. Run: `cargo test && cargo clippy -- -D warnings`
5. Write integration tests in `tests/` if the feature spans multiple modules
6. Update documentation (the docs-agent handles this automatically)

## Dependencies

| Crate | Purpose |
|-------|---------|
| `uuid` | UUID v4 key generation |
| `thiserror` | Error type definitions |
| `crc32fast` | CRC32 checksums for pages and WAL |
| `parking_lot` | Fast RwLock/Mutex for SWMR concurrency |
| `tempfile` | Temporary directories for tests |
| `rand` | Random data generation for tests |
| `rustyline` | Line editing for GrumpyShell REPL (dev) |
| `serde_json` | JSON serialization for GrumpyShell (dev) |
