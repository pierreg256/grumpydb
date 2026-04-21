# GrumpyDB — Implementation Plan

## Phase Overview

```
Phase 1: Foundations        ████████████████████  ✅ Done
Phase 2: B+Tree Index      ████████████████████  ✅ Done
Phase 3: Document Model    ████████████████████  ✅ Done
Phase 4: Storage Engine    ████████████████████  ✅ Done
Phase 4b: Demo App v1      ████████████████████  ✅ Done
Phase 5: WAL & Recovery    ████████████████████  ✅ Done
Phase 5b: Demo App v2      ████████████████████  ✅ Done
Phase 6: Buffer Pool       ░░░░░░░░░░░░░░░░░░░░  Pending
Phase 6b: Demo App v3      ░░░░░░░░░░░░░░░░░░░░  Pending — Add performance benchmarks
Phase 7: SWMR Concurrency  ░░░░░░░░░░░░░░░░░░░░  Pending
Phase 7b: Demo App v4      ░░░░░░░░░░░░░░░░░░░░  Pending — Add multi-threaded access
Phase 8: Polish & Hardening░░░░░░░░░░░░░░░░░░░░  Pending
Phase 8b: Demo App Final   ░░░░░░░░░░░░░░░░░░░░  Pending — Polished example + tutorial
```

---

## Phase 1: Foundations (Error types + Page system) ✅

### Objective
Lay the groundwork: error types, constants, page format, disk I/O.

### Tasks

#### 1.1 Error types (`src/error.rs`) ✅
- [x] Define `GrumpyError` with all variants (10 variants)
- [x] Define `type Result<T> = std::result::Result<T, GrumpyError>`
- [x] Tests: verify Display, From<io::Error> (3 tests)

#### 1.2 Constants and page types (`src/page/mod.rs`) ✅
- [x] `PAGE_SIZE = 8192`
- [x] `PAGE_HEADER_SIZE = 32`
- [x] `PageId(u32)`, `SlotId(u16)` newtypes
- [x] `PageType` enum (Free, Data, BTreeInternal, BTreeLeaf, Overflow, FreeList)
- [x] `PageHeader` struct with binary serialization (write_to / read_from)
- [x] Tests: header round-trip serialization (4 tests)
- [x] Additional constants: `PAGE_USABLE_SPACE`, `SLOT_SIZE`, `OVERFLOW_MARKER`, `INVALID_PAGE_ID`

#### 1.3 Page Manager (`src/page/manager.rs`) ✅
- [x] `PageManager::new(path)` → open/create `data.db`
- [x] `allocate_page()` → return PageId (free-list or append)
- [x] `read_page(page_id)` → read 8 KiB from disk
- [x] `write_page(page_id, &[u8; PAGE_SIZE])` → write to disk
- [x] `free_page(page_id)` → add to free-list
- [x] Free-list persisted in page 0 (capacity: 2039 IDs)
- [x] `sync()` → fsync to disk
- [x] Tests: alloc, read/write round-trip, free + realloc (12 tests)
- [x] Tests: empty file, existing file, reopen, free-list persistence

#### 1.4 Slotted Page (`src/page/slotted.rs`) ✅
- [x] `SlottedPage` wrapping `[u8; PAGE_SIZE]`
- [x] `new(page_id)` → initialize empty page with header
- [x] `from_bytes(data)` → wrap existing buffer
- [x] `insert(data: &[u8])` → return SlotId or PageFull
- [x] `get(slot_id)` → return `&[u8]` or error
- [x] `delete(slot_id)` → mark slot as tombstone
- [x] `update(slot_id, data: &[u8])` → in-place if fits, otherwise delete+insert
- [x] `free_space()` → remaining free space (saturating_sub)
- [x] `compact()` → defragment the page
- [x] `live_tuple_count()` → count non-deleted tuples
- [x] Tombstone slot reuse on insert
- [x] Tests: insert/get/delete, full page, compaction, update, tombstone reuse (15 tests)

#### 1.5 Overflow Pages (`src/page/overflow.rs`) ✅
- [x] `write_overflow(page_manager, data: &[u8])` → overflow page chain
- [x] `read_overflow(page_manager, first_page_id)` → reconstruct data
- [x] `free_overflow(page_manager, first_page_id)` → free the chain
- [x] `encode_overflow_ref` / `decode_overflow_ref` / `is_overflow` → reference codec (9 bytes)
- [x] Chunk length stored in `num_slots` header field (repurposed)
- [x] Tests: single/multi-page, boundary, free chain, large data (8 tests)

### Validation criteria Phase 1 ✅
- [x] `cargo test --lib` passes 100% (53 tests)
- [x] Every public struct/fn has a doc-comment
- [x] `cargo clippy -- -D warnings` passes

---

## Phase 2: B+Tree Index ✅

### Objective
Implement a complete B+Tree in a separate file (`index.db`).

### Tasks

#### 2.1 Node types (`src/btree/node.rs`) ✅
- [x] `InternalNode`: serialization/deserialization from `[u8; PAGE_SIZE]` (from_bytes/to_bytes)
- [x] `LeafNode`: serialization/deserialization (from_bytes/to_bytes)
- [x] Max fan-out calculation: INTERNAL_MAX_KEYS=407, LEAF_MAX_ENTRIES=370
- [x] `InternalEntry`, `LeafEntry` structs, `find_child`, `insert_entry`, `remove_entry`
- [x] Min thresholds: INTERNAL_MIN_KEYS=162, LEAF_MIN_ENTRIES=148 (40%)
- [x] Tests: round-trip serialization, max capacity, find_child, search, insert/remove (9 tests)

#### 2.2 B+Tree structure (`src/btree/mod.rs`) ✅
- [x] `BTree::create(path)` → create index file (page 0=free-list, page 1=metadata, page 2=empty root)
- [x] `BTree::open(path)` → open existing index, read metadata from page 1
- [x] Metadata: root_page_id, height, num_entries (stored in page 1, not page 0)
- [x] `len()`, `is_empty()`, `height()`, `sync()`, `flush_meta()`
- [x] Tests: create + open round-trip, meta persistence, empty root leaf (3 tests)

#### 2.3 Search (`src/btree/ops.rs`) ✅
- [x] `search(key: &Uuid)` → `Option<(u32, u16)>`
- [x] Iterative descent from root via `find_leaf()`
- [x] Linear scan in internal nodes (`find_child`), binary search in leaves
- [x] Tests: key present, key absent, empty tree (3 tests)

#### 2.4 Insert (`src/btree/ops.rs`) ✅
- [x] `insert(key: Uuid, page_id: u32, slot_id: u16)` → `Result<()>`
- [x] Insert into leaf using `find_leaf_with_path()`
- [x] **Leaf split** if full → promote median key (`split_leaf`)
- [x] **Internal node split** if full → recursive split (`split_internal`)
- [x] **Root split** → new root via `propagate_split`
- [x] `DuplicateKey` handling
- [x] Tests: single, 100 sequential, leaf split (370+), root split (5000), random, duplicate (7 tests)

#### 2.5 Delete (`src/btree/ops.rs`) ✅
- [x] `delete(key: &Uuid)` → `Result<()>`
- [x] Remove from leaf via `remove_entry`
- [x] **Merge** if underfull (< 40%) via `merge_leaves` / `merge_internal_nodes`
- [x] **Redistribution** with sibling via `redistribute_leaf_from_left/right`
- [x] Special case: empty root → reduce height in `remove_from_internal`
- [x] Tests: delete single, nonexistent, half (250/500), all (200), reinsert, stress 2000 (6 tests)

#### 2.6 Cursor (`src/btree/cursor.rs`) ✅
- [x] `BTreeCursor`: leaf iterator with `next_entry()`
- [x] `cursor_from(key)` → position at first entry >= key
- [x] `cursor()` → cursor at beginning (smallest key)
- [x] `range(start, end)` → iterate over range [start, end)
- [x] `scan_all()` → all entries sorted
- [x] `CursorEntry` and `CursorItem` structs
- [x] Tests: full scan, range scan, unbounded, across splits, empty range, cursor_from (8 tests)

### Validation criteria Phase 2 ✅
- [x] Insert 5,000 keys + verify (test_insert_causes_root_split)
- [x] Delete 50% of keys + verify (test_delete_half, test_large_insert_and_delete_stress)
- [x] Range scan verifies sorted order (test_cursor_across_leaf_splits)
- [x] Persistence: insert → close → reopen → verify (test_persist_and_reopen)
- [x] 87 total tests, 0 clippy warnings

---

## Phase 3: Document Model ✅

### Objective
JSON-like data model with compact binary codec.

### Tasks

#### 3.1 Value type (`src/document/value.rs`) ✅
- [x] `Value` enum with all variants (Null, Bool, Integer, Float, String, Bytes, Array, Object)
- [x] `impl PartialEq, Debug, Clone` for Value
- [x] Accessor methods: `is_null()`, `as_bool()`, `as_i64()`, `as_f64()`, `as_str()`, `as_bytes()`, `as_array()`, `as_object()`
- [x] Tests: construction, equality, accessors, clone (11 tests)

#### 3.2 Binary codec (`src/document/codec.rs`) ✅
- [x] `encode(value: &Value, buf: &mut Vec<u8>)` + `encode_to_vec()`
- [x] `decode(bytes: &[u8]) → Result<Value>` + `decode_from_cursor()`
- [x] `encoded_size(value: &Value) → usize` (no allocation)
- [x] Safety limits: MAX_NESTING_DEPTH=64, MAX_BLOB_LEN=16MiB, MAX_ARRAY_LEN=1M, MAX_OBJECT_KEYS=100K
- [x] Type tags: 0x00=Null, 0x01=Bool, 0x02=Integer, 0x03=Float, 0x04=String, 0x05=Bytes, 0x06=Array, 0x07=Object
- [x] Tests: round-trip each type, nested complex, encoded_size, unknown tag, truncated, invalid UTF-8, nesting depth, NaN, empty containers (19 tests)

#### 3.3 Document (`src/document/mod.rs`) ✅
- [x] `Document { key: Uuid, value: Value }` with `new()`, `encode()`, `decode()`, `encoded_size()`
- [x] Encode: 16 bytes UUID + encoded Value
- [x] Tests: round-trip simple/complex/null, encoded_size, too short, UUID preservation (6 tests)

### Validation criteria Phase 3 ✅
- [x] 112 total tests, 0 clippy warnings
- [x] Safety limits tested (nesting depth, unknown tag, truncated, invalid UTF-8)
- [x] Float NaN handled correctly

---

## Phase 4: Storage Engine (assembly) ✅

### Objective
Connect Pages + B+Tree + Documents for a functional CRUD (without WAL or cache).

### Tasks

#### 4.1 Engine (`src/engine.rs`) ✅
- [x] `GrumpyDb::open(path)` → open/create data.db (PageManager) + index.db (BTree)
- [x] `insert(key, value)` → encode document → store in slotted page → index in B+Tree
- [x] `get(key)` → search B+Tree → read page + slot → decode document
- [x] `update(key, value)` → delete old + insert new
- [x] `delete(key)` → search B+Tree → delete from slotted page + free overflow → remove from B+Tree
- [x] `scan(range)` → B+Tree range query → read each document
- [x] Overflow page handling for large documents (store_overflow / read_tuple)
- [x] `flush()` → sync data + index to disk
- [x] `close()` → flush + drop
- [x] Current data page tracking + auto-allocation on full
- [x] Tests: 16 unit tests (CRUD lifecycle, overflow, persistence, scan, errors)

#### 4.2 Public API (`src/lib.rs`) ✅
- [x] `GrumpyDb` with `&mut self` methods
- [x] Re-export `Value`, `Uuid` (via uuid), `GrumpyError`, `Result`
- [x] Doc-comments with examples (doctest passes)

#### 4.3 Integration tests (`tests/crud_test.rs`) ✅
- [x] Basic CRUD: insert → get → update → get → delete → get(None)
- [x] Bulk insert 1,000 documents → verify each
- [x] Bulk delete → verify
- [x] Range scan → verify order
- [x] Scan all → verify count
- [x] Duplicate key → error
- [x] Get/update/delete on non-existent key → error
- [x] Reopen database → verify persistence
- [x] Complex documents (nested objects)
- [x] Overflow document CRUD (20 KiB strings)

### Validation criteria Phase 4 ✅
- [x] All 138 tests pass (126 unit + 10 integration + 2 doctests)
- [x] Database survives close + reopen
- [x] 0 clippy warnings

---

## Phase 4b: Demo App v1 — Task Manager CLI (basic CRUD) ✅

### Objective
Build a simple task management CLI app (`examples/taskman/`) that uses GrumpyDB as its storage engine.
This serves as a **living usage example** and **documentation** — every line of code must be thoroughly
commented to explain how to use the GrumpyDB API.

### Principles
- **Documentation-first**: every function, struct, and block has explanatory comments
- **Progressive complexity**: starts simple (CRUD), grows with each engine phase
- **Standalone**: the example is a separate binary in `examples/`, not a workspace member
- **Real-world patterns**: shows idiomatic Rust usage of the GrumpyDB API

### Tasks

#### 4b.1 Data model (`examples/taskman/task.rs`) ✅
- [x] `Task` struct: id (UUID), title, description (Option), done, created_at (i64), tags (Vec<String>)
- [x] `Task::to_value()` → serialize Task as `Value::Object` with BTreeMap
- [x] `Task::from_value(id, &Value)` → deserialize from `Value::Object`
- [x] `Task::new()` with auto UUID + timestamp
- [x] `Display` impl for pretty CLI output with status indicators (✓/○)
- [x] Thorough doc-comments explaining each conversion step and data flow

#### 4b.2 Storage layer (`examples/taskman/store.rs`) ✅
- [x] `TaskStore` wrapping `GrumpyDb` — documented constructor pattern
- [x] `add_task(task) → Uuid` — demonstrates `insert()` with doc-comments
- [x] `get_task(id) → Option<Task>` — demonstrates `get()` + Value→Task
- [x] `update_task(task)` — demonstrates `update()` (full replacement)
- [x] `set_task_done(id, done)` — demonstrates read-modify-write pattern
- [x] `delete_task(id)` — demonstrates `delete()`
- [x] `list_all_tasks() → Vec<Task>` — demonstrates `scan(..)`
- [x] `list_by_status(done) → Vec<Task>` — demonstrates scan + filter
- [x] `stats() → (total, done, pending)` — demonstrates scan + aggregation
- [x] Error handling: GrumpyError mapped to user-friendly String messages

#### 4b.3 CLI interface (`examples/taskman/main.rs`) ✅
- [x] Subcommands: `add`, `list`, `done`, `undone`, `show`, `delete`, `stats`, `help`
- [x] Args parsed with `std::env::args()` (no external crate dependency)
- [x] `--desc` and `--tags` flags for `add` command
- [x] `--done` / `--pending` filters for `list` command
- [x] Short UUID prefix matching for task IDs (8-char prefix scan)
- [x] Pretty-print with status indicators and tag display
- [x] Full help message with usage examples

#### 4b.4 Documentation quality ✅
- [x] Module-level `//!` docs in all 3 files explaining purpose, data flow, architecture
- [x] Every public function has `///` doc with argument descriptions
- [x] Inline comments explaining every GrumpyDB API call
- [x] Code architecture diagram in comments (Task ↔ Value ↔ Disk)
- [x] Pattern explanations: typed wrapper, read-modify-write, scan+filter

### Validation criteria Phase 4b ✅
- [x] `cargo run --example taskman -- add "Task"` works end-to-end
- [x] `cargo run --example taskman -- list` shows tasks
- [x] Persistence: tasks survive process restart
- [x] `done`/`undone` toggle works
- [x] 138 total tests, 0 clippy warnings (all-targets)
- [x] Every GrumpyDB API call has an inline comment

---

## Phase 5: WAL & Crash Recovery ✅

### Objective
Add durability with a Write-Ahead Log.

### Tasks

#### 5.1 WAL Records (`src/wal/record.rs`) ✅
- [x] `WalRecord` struct with binary serialization (to_bytes / from_bytes)
- [x] Types: PageWrite, Commit, Rollback, Checkpoint (`WalOpType` enum)
- [x] CRC32 checksum per record (`crc32fast`)
- [x] Tests: round-trip serialization, corruption detection, sequential records (8 tests)

#### 5.2 WAL Writer (`src/wal/writer.rs`) ✅
- [x] `WalWriter::new(path)` → open/create `wal.log`, resume LSN on reopen
- [x] `log_page_write(tx_id, page_id, before, after)` → write record
- [x] `log_commit(tx_id)` → write record + fsync
- [x] `log_checkpoint()` → write checkpoint record + fsync
- [x] Auto-incrementing LSN, `begin_tx()` for TX ID generation
- [x] `truncate()` → clear WAL after checkpoint
- [x] `read_all_records()` → scan with corruption tolerance
- [x] Tests: write/read, LSN increment, checkpoint, truncate, reopen, multi-TX (7 tests)

#### 5.3 Recovery (`src/wal/recovery.rs`) ✅
- [x] `recover(records, data_pm, index_pm)` → replay WAL
- [x] Redo phase: apply after-images of committed TXs (in LSN order)
- [x] Undo phase: apply before-images of uncommitted TXs (reverse LSN order)
- [x] Checkpoint-aware: only process records after last checkpoint
- [x] `RecoveryResult` struct with redo/undo counts
- [x] Page ID convention: bit 31 = index file flag (`INDEX_PAGE_FLAG`)
- [x] Tests: empty, committed redo, uncommitted undo, mixed TXs, checkpoint (5 tests)

#### 5.4 WAL integration in Engine ✅
- [x] `GrumpyDb::open()` runs WAL recovery automatically
- [x] `insert()` / `delete()` log page writes with before/after images
- [x] Commit after each operation (fsync WAL)
- [x] `flush()` writes checkpoint + truncates WAL
- [x] Auto-checkpoint every 100 writes (`CHECKPOINT_INTERVAL`)
- [x] All existing 10 integration tests still pass

### Validation criteria Phase 5 ✅
- [x] WAL records: round-trip with CRC32 validation
- [x] Recovery: redo committed, undo uncommitted, respect checkpoints
- [x] WAL truncation after checkpoint works
- [x] Corrupted records detected and reading stops
- [x] 157 total tests, 0 clippy warnings

---

## Phase 5b: Demo App v2 — Crash Safety Demo ✅

### Objective
Update the task manager to demonstrate WAL durability. Show users how GrumpyDB
protects their data against crashes.

### Tasks

#### 5b.1 Crash safety documentation ✅
- [x] `examples/taskman/README.md`: full docs with "Data Safety" section
- [x] WAL protocol explained in user-friendly terms (write → fsync → commit → recovery)
- [x] `flush()` usage documented: when to call it, what it guarantees
- [x] API patterns table: all GrumpyDB calls mapped to TaskMan functions
- [x] On-disk files documented (data.db, index.db, wal.log)

#### 5b.2 Batch operations ✅
- [x] `export_tasks()` → export all tasks to pipe-delimited file (demonstrates `scan(..)`)
- [x] `import_tasks(data)` → bulk import with duplicate detection (demonstrates batch `insert()`)
- [x] `flush` CLI command → explicit WAL checkpoint
- [x] Documented crash semantics: "if import crashes mid-way, committed tasks are safe"

#### 5b.3 Reliability test script ✅
- [x] `examples/taskman/test_crash.sh` — 6-step test:
  1. Insert 20 tasks → verify count
  2. Export to file → verify line count
  3. Simulate restart (reopen) → verify all tasks survive
  4. Flush (WAL checkpoint) → verify
  5. Re-import → verify 0 duplicates added
  6. Final count → verify 20
- [x] Inline comments explaining what GrumpyDB guarantees at each step

### Validation criteria Phase 5b ✅
- [x] Import/export round-trip works
- [x] Crash test script passes (all 6 steps green)
- [x] README explains WAL in accessible terms

---

## Phase 6: Buffer Pool

### Objective
LRU cache to avoid redundant disk I/O.

### Tasks

#### 6.1 Buffer Frame (`src/buffer/frame.rs`)
- [ ] `BufferFrame` struct
- [ ] Pin/unpin with atomic counter
- [ ] Dirty tracking
- [ ] Tests: pin/unpin, dirty flag

#### 6.2 Buffer Pool (`src/buffer/pool.rs`)
- [ ] `BufferPool::new(capacity, page_manager)`
- [ ] `fetch_page(page_id)` → return pinned frame (load if absent)
- [ ] `new_page()` → allocate + return pinned frame
- [ ] `unpin(page_id, dirty)`
- [ ] `flush_page(page_id)` → write if dirty
- [ ] `flush_all()` → flush all dirty pages
- [ ] LRU eviction when pool is full
- [ ] Tests: fetch/unpin, LRU eviction, flush, full pool with all pinned → error

#### 6.3 Engine integration
- [ ] Replace direct PageManager access with BufferPool
- [ ] All existing tests must still pass
- [ ] Performance test: measure improvement with cache

### Validation criteria Phase 6
- Buffer pool unit tests
- All existing integration tests pass (regression)
- Disk I/O count decreases (measurable via counter)

---

## Phase 6b: Demo App v3 — Performance Benchmarks

### Objective
Add performance-oriented features to the task manager and benchmark GrumpyDB
with and without the buffer pool. Demonstrate caching benefits to users.

### Tasks

#### 6b.1 Benchmark subcommand
- [ ] `taskman bench --count 10000` — insert N tasks, measure time, report ops/sec
- [ ] `taskman bench --read 10000` — random reads, measure latency
- [ ] Document buffer pool impact with inline comments comparing before/after
- [ ] Show how page cache hits reduce disk I/O (add metrics reporting)

#### 6b.2 Large dataset demo
- [ ] `taskman generate --count 50000` — generate synthetic tasks for testing
- [ ] `taskman search --tag "urgent"` — scan + filter, show scan performance
- [ ] Document: "How GrumpyDB handles 50K+ documents efficiently"

#### 6b.3 Performance documentation
- [ ] `examples/taskman/PERFORMANCE.md` — benchmark results, graphs explanation
- [ ] Inline comments explaining: page cache hit ratio, LRU eviction, buffer pool sizing
- [ ] Code comments: "Why this operation is O(log n) and not O(n)"

### Validation criteria Phase 6b
- Benchmark subcommand runs without error
- Performance numbers documented
- Buffer pool impact clearly explained in comments

---

## Phase 7: SWMR Concurrency

### Objective
Allow concurrent reads with an exclusive writer.

### Tasks

#### 7.1 Lock Manager (`src/concurrency/lock_manager.rs`)
- [ ] `LockManager` with page-level `RwLock` (via `parking_lot`)
- [ ] `read_lock(page_id)` / `read_unlock(page_id)`
- [ ] `write_lock(page_id)` / `write_unlock(page_id)`
- [ ] Global write mutex
- [ ] Tests: lock/unlock, concurrent reads, write blocks reads

#### 7.2 Engine integration
- [ ] Wrap GrumpyDb in Arc for thread sharing
- [ ] Read operations → read locks
- [ ] Write operations → write mutex + write locks
- [ ] Tests: concurrent reads from N threads
- [ ] Tests: writer + simultaneous readers
- [ ] Tests: verify no deadlocks

### Validation criteria Phase 7
- Test with 8 reader threads + 1 writer thread for 5 seconds
- No deadlocks, no corruption
- All existing tests still pass

---

## Phase 7b: Demo App v4 — Multi-threaded Access

### Objective
Demonstrate concurrent access to GrumpyDB from multiple threads. Show the SWMR
model in action with a real application.

### Tasks

#### 7b.1 Concurrent task operations
- [ ] `taskman serve --port 8080` — simple HTTP server (using std TcpListener, no external crate)
- [ ] Multiple clients can read tasks concurrently
- [ ] Single writer at a time (SWMR model demonstrated)
- [ ] Document: Arc<GrumpyDb> usage pattern with inline comments
- [ ] Document: why reads don't block each other

#### 7b.2 Shared state demo
- [ ] `taskman watch` — poll for changes from another thread (demonstrates concurrent reads)
- [ ] `taskman worker` — background task processor (demonstrates writer pattern)
- [ ] Document thread-safety guarantees with comments at each critical section

#### 7b.3 Concurrency documentation
- [ ] Inline comments: "Why we use RwLock, not Mutex, for readers"
- [ ] Inline comments: "How SWMR prevents data corruption"
- [ ] Code tour: "From HTTP request to disk write — the full lock sequence"

### Validation criteria Phase 7b
- Two concurrent readers get consistent results
- Writer + readers work without deadlocks
- Every lock acquisition has an explanatory comment

---

## Phase 8: Polish & Hardening

### Objective
Finalize, harden, document.

### Tasks

#### 8.1 Compaction
- [ ] `compact()` → defragment data file
- [ ] Rebuild B+Tree index
- [ ] Tests: compact after many deletes, verify integrity

#### 8.2 Robustness
- [ ] Checksum validation on every page read
- [ ] Size limits documented and enforced
- [ ] Graceful degradation on I/O error
- [ ] Stress tests: 100,000 random operations

#### 8.3 Documentation
- [ ] Clean `cargo doc` for the full public API
- [ ] README.md with usage examples
- [ ] Benchmarks with `criterion`

#### 8.4 CI-ready
- [ ] `cargo fmt --check && cargo clippy -- -D warnings && cargo test`
- [ ] Non-regression tests tagged

### Validation criteria Phase 8
- `cargo doc` with no warnings
- Stress test passes
- README with working examples

---

## Phase 8b: Demo App Final — Polished Example & Tutorial

### Objective
Finalize the task manager as a **production-quality example** and **tutorial**.
It should serve as the primary onboarding resource for new GrumpyDB users.

### Tasks

#### 8b.1 Code polish
- [ ] Refactor all example code for clarity and idiomaticness
- [ ] Every file starts with a `//!` module doc explaining its role in the example
- [ ] Every public function has a `/// # Examples` block with runnable code
- [ ] Error handling uses custom error type wrapping `GrumpyError` (demonstrates integration)
- [ ] No `unwrap()` in non-test code — all errors handled with `?` or user messages

#### 8b.2 Tutorial documentation (`examples/taskman/TUTORIAL.md`)
- [ ] **Chapter 1: Getting Started** — opening a database, inserting your first document
- [ ] **Chapter 2: Data Modeling** — converting Rust structs to/from `Value`
- [ ] **Chapter 3: Querying** — get, scan, range queries
- [ ] **Chapter 4: Updates & Deletes** — mutation patterns
- [ ] **Chapter 5: Durability** — flush, WAL, crash recovery
- [ ] **Chapter 6: Performance** — buffer pool, page cache, benchmarking
- [ ] **Chapter 7: Concurrency** — multi-threaded access, SWMR model
- [ ] Each chapter references specific functions in the example code

#### 8b.3 API cookbook (`examples/taskman/COOKBOOK.md`)
- [ ] Recipe: "Store a Rust struct in GrumpyDB"
- [ ] Recipe: "Iterate over all documents"
- [ ] Recipe: "Filter documents by field value"
- [ ] Recipe: "Handle a missing key gracefully"
- [ ] Recipe: "Bulk import from JSON"
- [ ] Recipe: "Use GrumpyDB from multiple threads"
- [ ] Each recipe is a self-contained code snippet with inline comments

#### 8b.4 Integration with main docs
- [ ] Link tutorial from main `README.md`
- [ ] Link cookbook from `cargo doc` landing page
- [ ] `examples/` section in `CONTRIBUTING.md`
- [ ] Verify all code snippets compile: `cargo test --doc`

### Validation criteria Phase 8b
- Tutorial covers all 7 chapters with working code
- Cookbook has 6+ recipes
- All code snippets in docs compile
- A new user can build a CRUD app by following the tutorial alone