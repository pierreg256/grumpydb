# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [3.0.0] - 2026-04-23

### Added
- **Multi-Tenant Server** (Phase 13): full client/server hierarchy for multi-tenant isolation
  - `src/server/mod.rs`: `GrumpyServer` struct — multi-tenant server managing isolated clients
  - `src/server/client.rs`: `Client` struct — per-tenant client with independent databases
  - Full hierarchy: Server → Client → Database → Collection
  - `GrumpyError::ClientNotFound` and `GrumpyError::DatabaseNotFound` error variants
  - `GrumpyServer` and `Client` exported from `lib.rs`
  - 19 new tests (9 client + 10 server)
- **Concurrency v2** (Phase 14): thread-safe wrappers for Database and Server
  - `SharedDatabase` — thread-safe Database wrapper with per-database `Arc<RwLock>`
  - `SharedServer` — multi-tenant server with independent per-database locking
  - Concurrent writes to different databases without contention
  - `SharedDatabase` and `SharedServer` exported from `lib.rs`
  - 9 new concurrency tests (4 SharedDatabase + 5 SharedServer)
- 295 total tests (unit + integration + doctests), 0 clippy warnings

## [2.1.0] - 2026-04-23

### Added
- **Document References** (Phase 12c): cross-collection document linking with cycle detection
  - `Value::Ref(String, Uuid)` — reference type pointing to a document in another collection
  - Binary codec `TAG_REF = 0x08` for serialization/deserialization of Ref values
  - Sortable index encoding for Ref (`TAG_REF = 0x06`) in `src/index/encoding.rs`
  - `GrumpyError::CyclicReference` error variant for detecting circular reference chains
  - `Database::resolve_ref()` — resolve a single Ref to its target document
  - `Database::resolve_deep()` — recursively resolve all Ref fields with cycle detection
  - GrumpyShell: `$ref("collection", "uuid")` syntax for creating references in documents
  - GrumpyShell: `resolve()` and `resolveDeep()` commands for reference resolution
- 268 total tests (253 unit + 12 integration + 3 doctests), 0 clippy warnings

## [2.0.0] - 2026-04-23

### Added
- **Secondary Indexes** (Phase 11): fast exact-match and range queries on document fields
  - `src/index/encoding.rs`: sortable binary encoding — `encode_sortable_value()`, `encode_composite_key()`, `extract_field()`. Integer XOR sign-bit encoding, IEEE 754 float sort, string truncation to 128 bytes. 13 tests.
  - `src/index/mod.rs`: `SecondaryIndex` struct backed by VarBTree — `IndexDefinition`, `lookup()`, `range_query()`, `rebuild()`, `index_document()`, `unindex_document()`. 7 tests.
  - Collection integration: `create_index()`, `drop_index()`, `list_indexes()`, `query_index()`, `query_index_range()`, `insert_doc()`, `delete_doc()`. Compact rebuilds secondary indexes.
  - 5 new error variants: `NotIndexable`, `IndexNotFound`, `IndexAlreadyExists`, `CollectionNotFound`, `InvalidName`
  - `IndexDefinition` exported from `lib.rs`
- **Database** (Phase 12): multi-collection management with shared WAL
  - `src/database/mod.rs`: `Database` struct — `create_collection()`, `drop_collection()`, `list_collections()`. Full CRUD routed by collection name. Index management. Auto-discovery of existing collections on open. 12 tests.
  - `src/naming.rs`: `validate_name()` with `[a-z0-9_]{1,64}` validation. 5 tests.
  - `Database` exported from `lib.rs`
- **GrumpyShell** (Phase 12b): interactive JavaScript-like REPL for exploring GrumpyDB
  - `examples/grumpysh/main.rs`: CLI entry with `--data`, `--eval`, `--help`. Rustyline integration with history.
  - `examples/grumpysh/repl.rs`: read-eval-print loop with database state management
  - `examples/grumpysh/parser.rs`: command parser — `use`, `db.method()`, `db.coll.method()`, `Command` enum
  - `examples/grumpysh/json_parser.rs`: relaxed JSON parser (unquoted keys, single quotes, trailing commas). 11 tests.
  - `examples/grumpysh/filter.rs`: client-side document matching for `find({ field: value })`. 6 tests.
  - `rustyline` and `serde_json` added to dev-dependencies
- 268 total tests (253 unit + 12 integration + 3 doctests), 48 new tests, 0 clippy warnings

## [1.2.0] - 2026-04-23

### Added
- **Collection abstraction** (Phase 10): extracted per-collection storage from engine
  - `src/collection/mod.rs`: `Collection` struct — self-contained data pages + primary index
  - `Collection::open(path, name, pool_capacity)` — opens/creates a collection directory with `data.db` + `primary.idx`
  - Raw CRUD: `insert_raw()`, `get_raw()`, `delete_raw()`, `scan_raw()` — no WAL, caller handles logging
  - `PageWriteRecord` struct: before/after page images for WAL logging
  - `compact()`, `flush()`, `document_count()`, `pool_stats()`
  - `data_page_manager()`, `index_page_manager()` — for WAL recovery access
  - 10 new Collection unit tests (create, CRUD, scan, compact, overflow, persistence, duplicate key, pool stats)
  - 230 total tests (215 unit + 12 integration + 3 doctests), 0 clippy warnings

### Changed
- **Engine refactored**: `GrumpyDb` is now a thin wrapper over `Collection` + `WalWriter`
  - All internal page management code removed from engine (delegated to Collection)
  - WAL logging remains at engine level using `PageWriteRecord` from Collection
  - WAL recovery done on raw `PageManager`s before creating Collection (avoids double-borrow)
  - Index file renamed: `index.db` → `primary.idx` (matching Collection naming)

## [1.1.0] - 2026-04-23

### Added
- **Variable-Key B+Tree** (Phase 9): parallel `VarBTree` for variable-length byte keys
  - `src/btree/key.rs`: key encoding utilities — `encode_var_key()`, `decode_var_key()`, `var_key_disk_size()`, `VAR_KEY_MAX_SIZE=256`
  - `src/btree/var_node.rs`: `VarInternalNode`, `VarLeafNode` with fixed-stride serialization (length prefix + padded to max_key_size)
  - `src/btree/var_ops.rs`: search, insert (with split), delete (with merge/redistribute) for VarBTree
  - `src/btree/var_tree.rs`: `VarBTree` struct — `create(path, max_key_size)`, `open(path)`, `search()`, `insert()`, `delete()`, metadata persistence
  - `src/btree/var_cursor.rs`: `VarCursor` with `scan_all()`, `range()`, `cursor_from()`
  - Capacity functions: `var_internal_max_keys()`, `var_leaf_max_entries()`
  - 30 new tests (key encoding, node serialization, CRUD, splits, deletes, cursor, stress 3,000 keys)
  - 220 total tests (205 unit + 12 integration + 3 doctests), 0 clippy warnings
- Zero changes to existing BTree code (parallel implementation, no regression risk)

## [1.0.0] - 2026-04-22

### Added
- **Compaction** (Phase 8.1): defragment data pages and rebuild B+Tree index
  - `GrumpyDb::compact()` → rewrite all live documents into tightly-packed pages
  - `CompactResult` struct with preserved document count
  - `GrumpyDb::document_count()` → O(1) count via B+Tree metadata
  - `SharedDb::compact()`, `SharedDb::document_count()`, `SharedDb::pool_stats()`
  - `CompactResult` exported from `lib.rs`
  - 4 engine tests: compact after deletes, compact with overflow, compact empty, document count
- **Page checksums** (Phase 8.2): CRC32 integrity check on every page read/write
  - `page::compute_checksum()`, `page::stamp_checksum()`, `page::verify_checksum()`
  - Legacy pages (checksum==0) skip verification for backwards compatibility
  - `ChecksumMismatch` error variant on corruption detection
  - `PageManager::path()` accessor (needed for compaction)
  - 3 new checksum tests in `page/mod.rs`
- **Stress test** (Phase 8.2): `test_stress_random_operations` — 10,000 random operations
- **Compact integration test**: `test_compact_integration` — compact + reopen + verify
- **TaskMan Final** (Phase 8b): polished demo app with tutorial and cookbook
  - `compact` and `count` CLI commands
  - `TaskStore::compact()` and `TaskStore::document_count()` methods
  - `examples/taskman/TUTORIAL.md` — 7-chapter tutorial covering all GrumpyDB features
  - `examples/taskman/COOKBOOK.md` — 7 self-contained recipes for common tasks
- 190 total tests (175 unit + 12 integration + 3 doctests), 0 clippy warnings, 0 doc warnings

### Changed
- `PageManager::write_page()` now stamps CRC32 checksum before writing
- `PageManager::read_page()` now verifies CRC32 checksum after reading

## [0.5.0] - 2026-04-22

### Added
- **Buffer Pool** (`src/buffer/`): LRU page cache for reduced disk I/O (Phase 6)
  - `BufferFrame`: page caching with pin/unpin and dirty tracking
  - `BufferPool`: LRU eviction, `fetch_page()`, `new_page()`, `flush_all()`, I/O counters
  - Engine integration: data page access goes through the pool (256 frames = 2 MiB default)
  - `GrumpyDb::open_with_pool_capacity()` for custom pool sizing
  - `GrumpyDb::pool_stats()` for read/write/cache monitoring
  - Overflow pages bypass the pool (sequential, not revisited)
  - 11 buffer pool unit tests + 3 engine integration tests
- **TaskMan v3** (Phase 6b): performance benchmarks
  - `generate --count N` command: bulk-insert synthetic tasks with pool stats output
  - `search --tag TAG` command: scan + filter with pool stats output
  - `store.rs`: `pool_stats()` method
  - `PERFORMANCE.md`: buffer pool guide (architecture, impact table, capacity tuning)
- 181 total tests, 0 clippy warnings

### Changed
- `GrumpyDb` engine now uses `BufferPool` for all data page access instead of direct `PageManager`
- `flush()` now flushes buffer pool dirty pages before WAL checkpoint

## [0.4.0] - 2026-04-21

### Added
- **SWMR concurrency** (`src/concurrency/`): thread-safe database access (Phase 7)
  - `SharedDb`: `Arc<RwLock<GrumpyDb>>` wrapper with `Clone` for thread sharing
  - Concurrent reads and exclusive writes via `parking_lot::RwLock`
  - 7 concurrency tests: multi-reader, writer+readers, contention, persistence
- **TaskMan v4** (Phase 7b): multi-threaded demo
  - `concurrent.rs`: `run_bench()` multi-thread benchmark, `run_server()` TCP server
  - `bench` command: configurable writers/readers/count
  - `serve` command: line-protocol TCP server with per-client threads
  - Full concurrency documentation in comments
- `SharedDb` re-exported from `lib.rs`
- 165 total tests, 0 clippy warnings

### Note
- Phase 6 (Buffer Pool) skipped for now — will be implemented later
- `SharedDb::get()` currently uses write lock (B+Tree cursor needs &mut self)

## [0.3.1] - 2026-04-21

### Added
- **TaskMan README** (`examples/taskman/README.md`): full docs with data safety section, WAL explanation, API patterns table
- **Crash test script** (`examples/taskman/test_crash.sh`): 6-step automated test (insert, export, restart, flush, re-import, verify)

### Fixed
- Phase 5 and 5b tasks now fully checked in implementation plan
- All documentation updated to reflect completed WAL + demo app work

## [0.3.0] - 2026-04-21

### Added
- **Write-Ahead Log** (`src/wal/`): crash recovery and durability (Phase 5)
  - `WalRecord`: binary serialization with CRC32 checksums
  - `WalWriter`: append-only writer with fsync on commit, LSN tracking
  - Recovery: redo committed TXs, undo uncommitted TXs, checkpoint support
  - Engine integration: all page writes logged, auto-checkpoint every 100 writes
- **TaskMan v2** (Phase 5b): crash safety demo
  - `export` command: dump all tasks to pipe-delimited file
  - `import` command: bulk import with duplicate detection
  - `flush` command: explicit WAL checkpoint
  - Help updated with crash safety documentation
- 19 new WAL unit tests (record, writer, recovery)
- 157 total tests, 0 clippy warnings

### Changed
- `GrumpyDb::flush()` now writes WAL checkpoint and truncates WAL
- `GrumpyDb::open()` runs WAL recovery automatically

## [0.2.1] - 2026-04-21

### Added
- **TaskMan example app** (`examples/taskman/`): fully documented task manager CLI (Phase 4b)
  - `task.rs`: Task struct with `to_value()`/`from_value()` conversions, Display impl
  - `store.rs`: TaskStore wrapper around GrumpyDb (add, get, update, delete, list, stats)
  - `main.rs`: CLI with subcommands (add, list, done, undone, show, delete, stats, help)
  - Every GrumpyDB API call has inline documentation comments
  - Demonstrates: CRUD, scan+filter, read-modify-write pattern, error handling
- **Release agent** (`.claude/agents/release-agent.md`): automated versioning workflow
- Demo app phases (4b-8b) added to implementation plan

### Fixed
- Clippy warnings fixed across all targets (useless-vec, Range::contains, approx PI, constant assertions)

## [0.2.0] - 2026-04-21

### Added
- **Storage engine** (`src/engine.rs`): full CRUD wiring connecting pages + B+Tree + documents (Phase 4)
  - `GrumpyDb::open()`: creates/opens `data.db` + `index.db` in a directory
  - `insert(key, value)`: encode document → slotted page (or overflow) → B+Tree index
  - `get(key)`: B+Tree search → read page/slot → decode document
  - `update(key, value)`: delete + re-insert
  - `delete(key)`: remove from slotted page + free overflow + remove from B+Tree
  - `scan(range)`: B+Tree range cursor → read each document
  - `flush()` / `close()`: sync all data to disk
  - Overflow page support for large documents (>8 KiB)
  - Auto-allocation of new data pages when current is full
- **Integration tests** (`tests/crud_test.rs`): 10 cross-module tests
- **Release agent** (`.claude/agents/release-agent.md`): automated versioning workflow
- 138 total tests (126 unit + 10 integration + 2 doctests)

### Changed
- `GrumpyDb` methods now take `&mut self` (was `&self` stubs)
- Public API re-exports updated in `lib.rs`

## [0.1.0] - 2026-04-21

### Added
- **Page storage** (`src/page/`): 8 KiB page management with slotted layout, overflow chains, and free-list (Phase 1)
  - `PageManager`: disk I/O, page allocation/free with persistent free-list
  - `SlottedPage`: variable-length tuple storage with insert/get/delete/update/compact
  - Overflow pages: chained pages for documents larger than a single page
  - Constants: `PAGE_SIZE=8192`, `PAGE_HEADER_SIZE=32`, `SLOT_SIZE=4`
- **B+Tree index** (`src/btree/`): complete B+Tree with search, insert (split), delete (merge/redistribute), and cursor (Phase 2)
  - `InternalNode` / `LeafNode` with binary serialization
  - Fan-out: 407 internal keys, 370 leaf entries per node
  - `BTreeCursor` for range scans over doubly-linked leaf list
  - Metadata stored in page 1, root in page 2
- **Document model** (`src/document/`): schema-less JSON-like values with binary codec (Phase 3)
  - `Value` enum: Null, Bool, Integer, Float, String, Bytes, Array, Object
  - Binary codec with type tags, safety limits (nesting depth, blob size)
  - `Document` struct: UUID key + Value with encode/decode
- **Error handling** (`src/error.rs`): centralized `GrumpyError` enum with 10 variants
- **Engine stub** (`src/engine.rs`): `GrumpyDb` struct with open/close (CRUD not yet wired)
- 112 unit tests, 0 clippy warnings

### Not yet implemented
- Storage engine CRUD wiring (Phase 4)
- Write-Ahead Log (Phase 5)
- Buffer pool LRU cache (Phase 6)
- SWMR concurrency (Phase 7)
