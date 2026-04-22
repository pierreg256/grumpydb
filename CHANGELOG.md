# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
