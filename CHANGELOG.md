# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
