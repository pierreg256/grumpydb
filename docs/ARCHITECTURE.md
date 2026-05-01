# GrumpyDB — Technical Architecture

## 1. Overview

GrumpyDB is an embedded storage engine (library crate) that persists schema-less documents on disk with:
- **Page-based storage** of 8 KiB in `data.db`
- **B+Tree index** in `primary.idx` for O(log n) access by UUID
- **Secondary indexes** on document fields via `BTree<Vec<u8>>` (`idx_*.idx`)
- **Collection** abstraction encapsulating data pages + primary index + secondary indexes
- **Database** layer managing multiple collections with shared WAL
- **Server** layer for multi-tenant isolation (Client & Server)
- **Write-Ahead Log** in `wal.log` for durability
- **LRU Buffer Pool** for in-memory caching
- **SWMR** (Single-Writer, Multi-Reader) concurrency
- **grumpy-repl** interactive REPL for exploring databases

## 2. Page Format (8 KiB)

### 2.1 Page Header (32 bytes)

```
Offset  Size    Field
──────  ──────  ─────
0       4       page_id: u32
4       1       page_type: u8 (Free=0, Data=1, BTreeInternal=2, BTreeLeaf=3, Overflow=4, FreeList=5)
5       1       flags: u8 (bit 0: dirty)
6       2       num_slots: u16 (for slotted pages)
8       2       free_space_start: u16 (start of free space)
10      2       free_space_end: u16 (end of free space)
12      4       next_page_id: u32 (overflow chain / next leaf)
16      4       prev_page_id: u32 (doubly-linked leaves)
20      8       lsn: u64 (Log Sequence Number for WAL)
28      4       checksum: u32 (CRC32 of content)
```

### 2.2 Slotted Page Layout (Data pages)

```
┌──────────────────────────────────────────────┐
│ Page Header (32 bytes)                        │
├──────────────────────────────────────────────┤
│ Slot Array [slot_0, slot_1, ..., slot_n]     │  ← grows downward
│ (each slot = 4 bytes: offset:u16 + len:u16)  │
├──────────────────────────────────────────────┤
│                                              │
│          Free space                          │
│                                              │
├──────────────────────────────────────────────┤
│ Tuple data (serialized objects)              │  ← grows upward
│ [tuple_n, ..., tuple_1, tuple_0]             │
└──────────────────────────────────────────────┘
```

- Usable space per page: `8192 - 32 = 8160 bytes`
- Each slot: `offset (2) + length (2) = 4 bytes`
- A slot with offset=0 indicates a deleted slot (tombstone)

### 2.3 Overflow Pages

For documents larger than the free space of a single page (~7-8 KiB):
- The main tuple contains a **9-byte overflow reference**:
  - `OVERFLOW_MARKER` (1 byte, 0xFF) + `first_page_id` (4 bytes u32 LE) + `total_data_len` (4 bytes u32 LE)
- Overflow pages are chained via `next_page_id` in the header
- Chunk length in each page is stored in the `num_slots` header field (repurposed)
- Each overflow page stores up to `8160 bytes` of payload
- Functions: `write_overflow()`, `read_overflow()`, `free_overflow()`
- Helpers: `encode_overflow_ref()`, `decode_overflow_ref()`, `is_overflow()`

### 2.4 Free List

- Page 0 (type `FreeList`): contains the free-list
- Format: `num_free: u32` (offset 32) + `[page_id: u32, ...]` (offset 36+)
- Max capacity: 2039 page IDs per page
- When a page is freed → push onto free-list (LIFO)
- When a page is allocated → pop from free-list, otherwise extend file
- Page 0 cannot be freed

## 3. B+Tree Index

### 3.1 Properties

| Property | Value |
|----------|-------|
| Key type | UUID (16 bytes, lexicographic comparison) |
| Value | `page_id(u32) + slot_id(u16)` = 6 bytes |
| Page size | 8192 bytes |
| Internal fan-out (INTERNAL_MAX_KEYS) | 407 (floor((8160 - 6) / 20)) |
| Entries per leaf (LEAF_MAX_ENTRIES) | 370 (floor((8160 - 10) / 22)) |
| Merge threshold (MIN_OCCUPANCY_PERCENT) | 40% |
| Metadata page | Page 1 (page 0 = PageManager free-list) |
| Initial root | Page 2 (empty leaf) |

### 3.2 Internal Node

```
┌─────────────────────────────────────────┐
│ Page Header (32 bytes, type=BTreeInternal)│
├─────────────────────────────────────────┤
│ num_keys: u16                            │
│ right_child: u32 (page_id of last child) │
├─────────────────────────────────────────┤
│ Entry[0]: key(16) + child_page_id(4)     │  = 20 bytes
│ Entry[1]: key(16) + child_page_id(4)     │
│ ...                                      │
│ Entry[n]: key(16) + child_page_id(4)     │
└─────────────────────────────────────────┘
```

Semantics: `entries[i].child_page_id` contains keys `< entries[i].key`, `right_child` contains keys `≥ entries[last].key`.
Descent via `find_child()`: linear scan, first entry whose key > search_key → return its child_page_id.

### 3.3 Leaf Node

```
┌─────────────────────────────────────────┐
│ Page Header (32 bytes, type=BTreeLeaf)   │
├─────────────────────────────────────────┤
│ num_entries: u16                         │
│ next_leaf: u32 (page_id)                 │
│ prev_leaf: u32 (page_id)                 │
├─────────────────────────────────────────┤
│ Entry[0]: key(16) + page_id(4) + slot(2) │  = 22 bytes
│ Entry[1]: key(16) + page_id(4) + slot(2) │
│ ...                                      │
└─────────────────────────────────────────┘
```

### 3.4 Operations

| Operation | Complexity | Description |
|-----------|------------|-------------|
| Search | O(log n) | Descent from root |
| Insert | O(log n) | Insertion + split if node full |
| Delete | O(log n) | Removal + merge if underfull |
| Range scan | O(log n + k) | Search + traverse linked leaves |

### 3.5 Split/Merge

- **Split**: when a node is full, divide into 2 halves, promote the median key
- **Merge**: when a node falls below 40% occupancy, merge with a sibling
- **Redistribution**: if the sibling has enough entries, redistribute instead of merging

### 3.6 Variable-Key B+Tree (`BTree<Vec<u8>>`)

**v5 (Phase 33)**: the fixed-key and variable-key B+Trees are now a
single generic implementation parameterised by a `Key` trait. The
variable-key flavour used by secondary indexes is `BTree<Vec<u8>>`; the
primary-key flavour is `BTree<Uuid>`. The on-disk format is unchanged
from v4 — existing databases continue to work.

```rust
pub trait Key: Ord + Clone {
    fn encoded_len(&self) -> u16;
    fn encode_to(&self, buf: &mut [u8]);
    fn decode_from(buf: &[u8]) -> Result<Self>;
    /// `Some(N)` for fixed-width keys (e.g. `Uuid` → `Some(16)`),
    /// `None` for variable-width keys (e.g. `Vec<u8>`).
    const FIXED_LEN: Option<u16>;
}
```

The pre-v5 type alias `VarBTree` has been removed. Downstream code should
use `BTree<Vec<u8>>` directly — `VarBTree` was never re-exported at the
crate root, so this is not a semver break for external consumers.

The variable-key flavour supports keys up to 256 bytes and is used by
secondary indexes.

#### Properties

| Property | Value |
|----------|-------|
| Key type | Variable-length `&[u8]` (max 256 bytes) |
| Value | `page_id(u32) + slot_id(u16)` = 6 bytes |
| Page size | 8192 bytes |
| Key storage | Fixed-stride: `key_len(u16) + key_data + padding` to `max_key_size` |
| Metadata page | Page 1 (page 0 = PageManager free-list) |
| Initial root | Page 2 (empty leaf) |
| Merge threshold | 40% occupancy |

Fan-out depends on `max_key_size` (configured at creation):
- Internal: `(8160 - 8) / (2 + max_key_size + 4)` keys
- Leaf: `(8160 - 12) / (2 + max_key_size + 6)` entries

#### VarBTree Internal Node

```
┌──────────────────────────────────────────────┐
│ Page Header (32 bytes, type=BTreeInternal)    │
├──────────────────────────────────────────────┤
│ num_keys: u16                                 │
│ right_child: u32                              │
│ max_key_size: u16                             │
├──────────────────────────────────────────────┤
│ Entry[0]: key_len(u16) + key + pad + child(4) │  = (2 + max_key_size + 4) bytes
│ Entry[1]: ...                                 │
│ ...                                           │
└──────────────────────────────────────────────┘
```

#### VarBTree Leaf Node

```
┌──────────────────────────────────────────────┐
│ Page Header (32 bytes, type=BTreeLeaf)        │
├──────────────────────────────────────────────┤
│ num_entries: u16                              │
│ next_leaf: u32                                │
│ prev_leaf: u32                                │
│ max_key_size: u16                             │
├──────────────────────────────────────────────┤
│ Entry[0]: key_len(u16) + key + pad + page(4) + slot(2) │  = (2 + max_key_size + 6) bytes
│ Entry[1]: ...                                 │
│ ...                                           │
└──────────────────────────────────────────────┘
```

#### VarBTree Metadata (Page 1)

```
Offset  Content
0-31    PageHeader (type = BTreeInternal)
32-35   root_page_id: u32
36-39   height: u32
40-47   num_entries: u64
48-49   max_key_size: u16
```

#### VarCursor

Range scans via `VarCursor` with `scan_all()`, `range(start, end)`, and `cursor_from(start_key)`.

## 4. Document Model

### 4.1 Value Type

```rust
enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
    Ref(String, Uuid),  // (collection_name, document_uuid)
}
```

### 4.2 Binary Codec

Compact format with 1-byte type tag:

```
Tag  Type       Encoding
───  ────       ────────
0x00 Null       (nothing)
0x01 Bool       1 byte (0/1)
0x02 Integer    8 bytes (i64 little-endian)
0x03 Float      8 bytes (f64 little-endian)
0x04 String     4 bytes len (u32 LE) + UTF-8 bytes
0x05 Bytes      4 bytes len (u32 LE) + raw bytes
0x06 Array      4 bytes count (u32 LE) + recursive elements
0x07 Object     4 bytes count (u32 LE) + recursive pairs (key_string + value)
0x08 Ref        4 bytes name len (u32 LE) + UTF-8 collection name + 16 bytes UUID
```

### 4.3 Document

A document is a tuple `(UUID, Value)` where `Value` is typically an `Object`.

## 5. Write-Ahead Log (WAL)

### 5.1 WAL Record Format

```
┌────────────────────────────────────────┐
│ record_len: u32 (total size)            │
│ lsn: u64 (Log Sequence Number)         │
│ tx_id: u64 (transaction identifier)     │
│ op_type: u8                             │
│   1 = PageWrite (before + after image)  │
│   2 = Commit                            │
│   3 = Rollback                          │
│   4 = Checkpoint                        │
│ page_id: u32 (for PageWrite)            │
│ data_len: u32                           │
│ data: [u8] (before/after image)         │
│ checksum: u32 (CRC32)                   │
└────────────────────────────────────────┘
```

### 5.2 WAL Protocol

1. **Before** any page modification → write WAL record with before-image
2. **After** modification → write record with after-image
3. **Commit** → write Commit record + `fsync` the WAL
4. Modified pages are only flushed after the WAL is persisted

### 5.3 Recovery

At startup:
1. Read the WAL from the last checkpoint
2. **Redo**: replay committed transactions (apply after-images)
3. **Undo**: rollback uncommitted transactions (apply before-images)
4. Write a new checkpoint
5. Truncate the WAL before the checkpoint

### 5.4 Checkpoints

- Flush all dirty pages from the buffer pool
- Write a Checkpoint record in the WAL with the current LSN
- The WAL can be truncated before the checkpoint LSN

## 6. Buffer Pool

### 6.1 Structure

```rust
pub struct BufferPool {
    frames: Vec<BufferFrame>,          // fixed-size page cache
    page_table: HashMap<u32, usize>,   // page_id → frame index
    pm: PageManager,                   // underlying disk I/O
    clock: u64,                        // monotonic LRU counter
    pub read_count: u64,               // disk reads (cache misses)
    pub write_count: u64,              // disk writes (dirty evictions + flushes)
}

pub struct BufferFrame {
    pub data: [u8; PAGE_SIZE],         // raw page content (8 KiB)
    pub page_id: Option<u32>,          // which page is loaded (None = free)
    pub pin_count: u32,                // active references (>0 = cannot evict)
    pub dirty: bool,                   // modified since load?
    pub last_accessed: u64,            // monotonic counter for LRU ordering
}
```

Default pool: 256 frames × 8 KiB = 2 MiB (`DEFAULT_POOL_CAPACITY`).
Overflow pages bypass the pool (sequential I/O, not revisited).

### 6.2 LRU Eviction Policy

1. When the pool is full and a frame is needed:
2. Scan all frames for a free frame (no page loaded)
3. If none free, find the unpinned frame with the lowest `last_accessed` counter
4. If dirty → flush to disk first
5. Remove from page table, reset the frame, load the new page
6. If all frames are pinned → return `BufferPoolExhausted` error

### 6.3 Pin/Unpin

- `fetch_page(page_id)`: load page if absent (or cache hit), pin, return frame index
- `new_page()`: allocate on disk, load into pool (pinned, dirty), return (page_id, frame_idx)
- `unpin(page_id, dirty)`: decrement `pin_count`, optionally mark dirty
- `flush_page(page_id)`: write dirty page to disk, clear dirty flag
- `flush_all()`: flush all dirty pages + sync
- A page with `pin_count > 0` CANNOT be evicted

## 7. SWMR Concurrency

### 7.1 Model

- **Single writer** at a time (Mutex on write operations)
- **Multiple concurrent readers** (RwLock per page)
- Writer acquires write-locks on modified pages
- Readers acquire read-locks (non-blocking between each other)

### 7.2 Lock Manager

```
LockManager {
    page_locks: HashMap<PageId, RwLock<()>>,
    write_mutex: Mutex<()>,  // ensures single writer
}
```

### 7.3 Protocol

1. **Read**: `read_lock(page_id)` → read → `read_unlock(page_id)`
2. **Write**: `write_mutex.lock()` → `write_lock(pages...)` → modify → WAL → `write_unlock` → `write_mutex.unlock()`

## 8. Collection

A `Collection` is the unit of document storage — it owns its data pages (via a `BufferPool`) and a primary B+Tree index. The engine (`GrumpyDb`) is a thin wrapper over a single `Collection` plus a `WalWriter`.

### On-disk layout

```
<collection_dir>/
  data.db       ← slotted pages (documents)
  primary.idx   ← B+Tree: UUID → (PageId, SlotId)
```

### Structure

```rust
pub struct Collection {
    name: String,
    path: PathBuf,
    data_pool: BufferPool,        // LRU cache wrapping data PageManager
    btree: BTree,                  // primary index
    current_data_page: u32,        // page being filled
}

pub struct PageWriteRecord {
    pub page_id: u32,
    pub before: [u8; PAGE_SIZE],   // page image before modification
    pub after: [u8; PAGE_SIZE],    // page image after modification
}
```

### API

```rust
impl Collection {
    pub fn open(path: &Path, name: &str, pool_capacity: usize) -> Result<Self>;
    pub fn open_default(path: &Path, name: &str) -> Result<Self>;

    // Raw CRUD — no WAL, caller handles logging via returned PageWriteRecords
    pub fn insert_raw(&mut self, key: Uuid, encoded: &[u8]) -> Result<((u32, u16), Vec<PageWriteRecord>)>;
    pub fn get_raw(&mut self, key: &Uuid) -> Result<Option<Vec<u8>>>;
    pub fn delete_raw(&mut self, key: &Uuid) -> Result<Vec<PageWriteRecord>>;
    pub fn scan_raw(&mut self, range: impl RangeBounds<Uuid>) -> Result<Vec<(Uuid, Vec<u8>)>>;

    // Maintenance
    pub fn compact(&mut self) -> Result<u64>;
    pub fn flush(&mut self) -> Result<()>;
    pub fn document_count(&self) -> u64;
    pub fn pool_stats(&self) -> (u64, u64, usize, usize);
    pub fn data_page_manager(&mut self) -> &mut PageManager;
    pub fn index_page_manager(&mut self) -> &mut PageManager;
}
```

### Design rationale

- **WAL-free**: the `Collection` does not know about WAL. It returns `PageWriteRecord`s (before/after images) so the caller (`GrumpyDb` or future `Database`) can log them.
- **Self-contained**: a `Collection` can be used standalone in tests without WAL infrastructure.
- **Future-proof**: in Phase 12, a `Database` will own multiple `Collection`s sharing a single WAL.

## 9. Public API

`GrumpyDb` is a thin wrapper over a single `Collection` + `WalWriter`.

> **Deprecated in v5 (Phase 34)**: `GrumpyDb` and its `SharedDb` SWMR
> wrapper are annotated
> `#[deprecated(since = "5.0.0", note = "use Database with the _default collection")]`
> and will be **removed in v6**. New code should use `Database` (with the
> `_default` collection if a single collection is enough). The type and
> its methods are documented here for the deprecation cycle.

```rust
pub struct GrumpyDb {
    collection: Collection,
    wal: WalWriter,
    writes_since_checkpoint: u32,
}

impl GrumpyDb {
    /// Opens or creates a database in the specified directory.
    /// Data pages are cached in a 256-frame buffer pool (2 MiB) by default.
    pub fn open(path: &Path) -> Result<Self>;

    /// Opens a database with a custom buffer pool capacity (number of frames).
    pub fn open_with_pool_capacity(path: &Path, pool_capacity: usize) -> Result<Self>;

    /// Inserts a document with a UUID key, returns error if key exists
    pub fn insert(&mut self, key: Uuid, value: Value) -> Result<()>;

    /// Retrieves a document by its key
    pub fn get(&mut self, key: &Uuid) -> Result<Option<Value>>;

    /// Updates an existing document, returns error if key does not exist
    pub fn update(&mut self, key: &Uuid, value: Value) -> Result<()>;

    /// Deletes a document by its key
    pub fn delete(&mut self, key: &Uuid) -> Result<()>;

    /// Iterates over a key range
    pub fn scan(&mut self, range: impl RangeBounds<Uuid>) -> Result<Vec<(Uuid, Value)>>;

    /// Forces flush of all dirty pages + WAL checkpoint
    pub fn flush(&mut self) -> Result<()>;

    /// Returns buffer pool stats: (read_count, write_count, cached_count, capacity)
    pub fn pool_stats(&self) -> (u64, u64, usize, usize);

    /// Returns the number of documents in the database (O(1) via B+Tree metadata).
    pub fn document_count(&self) -> u64;

    /// Compacts the database: defragments data pages and rebuilds the B+Tree index.
    /// Returns a `CompactResult` with the number of preserved documents.
    pub fn compact(&mut self) -> Result<CompactResult>;

    /// Migrates all documents from this v1 GrumpyDb into a v2 Database collection.
    /// Creates the target collection if it doesn't exist.
    /// Returns the number of documents migrated.
    pub fn migrate_to_database(&mut self, target: &mut Database, collection: &str) -> Result<u64>;

    /// Closes the database cleanly (flush + close files)
    pub fn close(self) -> Result<()>;
}
```

## 10. Page Checksums

Every page written to disk is stamped with a CRC32 checksum covering all bytes
except the 4-byte checksum field itself (bytes 28–31).

### Functions

| Function | Description |
|----------|-------------|
| `compute_checksum(buf)` | CRC32 over bytes 0–27 + bytes 32–8191 |
| `stamp_checksum(buf)` | Compute and write the checksum into bytes 28–31 |
| `verify_checksum(buf, page_id)` | Verify on read; returns `ChecksumMismatch` on mismatch |

### Backwards compatibility

Pages with a stored checksum of `0` (never stamped, e.g. legacy data) skip
verification. This allows databases created before checksums were introduced
to remain readable.

### Integration

- `PageManager::write_page()` calls `stamp_checksum()` before writing.
- `PageManager::read_page()` calls `verify_checksum()` after reading.
- The B+Tree page manager follows the same protocol.

## 11. Compaction

The `compact()` method defragments the database by rewriting all live documents
into fresh, tightly-packed data pages and rebuilding the B+Tree index from scratch.

### Algorithm

1. **Flush** all dirty pages from the buffer pool and sync the B+Tree.
2. **Scan** all live entries from the B+Tree (sorted by key).
3. **Read** each document's raw bytes (inline or overflow).
4. **Create** temporary files (`data.db.compact`, `primary.idx.compact`).
5. **Reinsert** all documents into the fresh files, packing pages tightly.
6. **Swap** the compacted files over the originals (`rename`).
7. **Reopen** the engine with fresh file handles and buffer pool.

### Characteristics

- Requires `&mut self` — the database is unavailable during compaction.
- Space from deleted documents and fragmented pages is reclaimed.
- Overflow documents are preserved (rewritten into new overflow chains).
- The WAL is not used during compaction (atomic file swap).

### `CompactResult`

```rust
pub struct CompactResult {
    pub documents: u64,  // number of documents preserved
}
```
```

## 12. Error Handling

```rust
#[derive(Debug, thiserror::Error)]
pub enum GrumpyError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("page {0} not found")]
    PageNotFound(u32),

    #[error("page {0} is full")]
    PageFull(u32),

    #[error("key {0} already exists")]
    DuplicateKey(Uuid),

    #[error("key {0} not found")]
    KeyNotFound(Uuid),

    #[error("checksum mismatch on page {page_id}: expected {expected:#010x}, got {actual:#010x}")]
    ChecksumMismatch { page_id: u32, expected: u32, actual: u32 },

    #[error("WAL corrupted at LSN {0}")]
    WalCorrupted(u64),

    #[error("buffer pool exhausted: all frames are pinned")]
    BufferPoolExhausted,

    #[error("document too large: {size} bytes (max: {max})")]
    DocumentTooLarge { size: usize, max: usize },

    #[error("codec error: {0}")]
    Codec(String),

    #[error("value type cannot be indexed")]
    NotIndexable,

    #[error("index not found: {0}")]
    IndexNotFound(String),

    #[error("index already exists: {0}")]
    IndexAlreadyExists(String),

    #[error("collection not found: {0}")]
    CollectionNotFound(String),

    #[error("invalid name: {0}")]
    InvalidName(String),

    #[error("cyclic reference detected")]
    CyclicReference,

    #[error("client not found: {0}")]
    ClientNotFound(String),

    #[error("database not found: {0}")]
    DatabaseNotFound(String),

    #[error("data corruption detected: {0}")]
    Corruption(String),

    #[error("invalid page offset: page {page}, offset {offset}")]
    InvalidPageOffset { page: u32, offset: u16 },

    #[error("invalid variable-length key: {0}")]
    InvalidVarKey(String),
}
```

### 12.1 No-`unwrap` policy in the engine

The engine crate (`src/`) enforces a "no panic in production" policy via a
crate-level lint at the top of `src/lib.rs`:

```rust
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::panic, clippy::expect_used))]
```

The lint is allowed inside `#[cfg(test)]` modules and doc-comment examples but
fires on every production `unwrap()`, `expect()`, and `panic!` call.
"Shouldn't happen" cases (malformed bytes on disk, invalid offsets, etc.)
return one of the three new variants:

- `Corruption(String)` for generic invariant violations.
- `InvalidPageOffset { page, offset }` for out-of-range slot lookups.
- `InvalidVarKey(String)` for malformed variable-length B+Tree keys.

These bubble up through `?` and surface to the client as
`Response::Error("data corruption detected: …")` over the wire.

## 13. Secondary Indexes

Secondary indexes enable fast exact-match and range queries on document fields.

### 13.1 Sortable Encoding (`src/index/encoding.rs`)

Field values are encoded into byte sequences that preserve natural ordering under lexicographic byte comparison, enabling B+Tree range scans.

```
Type tag (1 byte) + encoded value:

  0x00  Null            → (empty)
  0x01  Bool(false)     → 0x00; Bool(true) → 0x01
  0x02  Integer(i64)    → XOR with 0x8000000000000000 (flip sign bit for sort)
  0x03  Float(f64)      → IEEE 754 sortable encoding
  0x04  String          → UTF-8 bytes (truncated to 128 bytes)
  0x05  Bytes           → raw bytes (truncated to 128 bytes)
  0x06  Ref             → collection name bytes (truncated to 128) + 16 bytes UUID
```

Ordering: `Null < Bool < Integer < Float < String < Bytes < Ref`. Arrays and Objects return `NotIndexable`.

Composite key = `encode_sortable_value(field) + uuid_bytes` (ensures uniqueness, max ~145 bytes).

### 13.2 SecondaryIndex struct (`src/index/mod.rs`)

```rust
pub struct IndexDefinition {
    pub name: String,        // e.g., "by_email"
    pub field_path: String,  // e.g., "email" or "address.city"
}

pub struct SecondaryIndex {
    pub def: IndexDefinition,
    btree: BTree<Vec<u8>>,   // variable-key B+Tree (max key size: 160 bytes)
    path: PathBuf,
}
```

On-disk file: `idx_<name>.idx` in the collection directory.

### 13.3 API

```rust
impl SecondaryIndex {
    pub fn create(dir: &Path, def: IndexDefinition) -> Result<Self>;
    pub fn open(dir: &Path, def: IndexDefinition) -> Result<Self>;
    pub fn index_document(&mut self, uuid: &Uuid, doc: &Value) -> Result<()>;
    pub fn unindex_document(&mut self, uuid: &Uuid, doc: &Value) -> Result<()>;
    pub fn lookup(&mut self, value: &Value) -> Result<Vec<Uuid>>;
    pub fn range_query(&mut self, start: &Value, end: &Value) -> Result<Vec<Uuid>>;
    pub fn count(&self) -> u64;
    pub fn rebuild(&mut self, docs: &[(Uuid, Value)]) -> Result<()>;
}
```

### 13.4 Collection Integration

`Collection` manages secondary indexes alongside the primary index:
- `create_index(name, field_path)` — creates `.idx` file + rebuilds from existing docs
- `drop_index(name)` — removes `.idx` file
- `insert_doc()` / `delete_doc()` — updates all secondary indexes automatically
- `query_index()` / `query_index_range()` — lookup + fetch full documents
- `compact()` — rebuilds secondary indexes after defragmentation

### 13.5 Field Extraction

`extract_field(value, "address.city")` navigates a `Value::Object` using dot-separated paths. Missing fields are silently skipped (document not indexed).

## 14. Database

A `Database` manages multiple named collections with a shared WAL.

### 14.1 On-disk layout

```
<database_dir>/
  wal.log             ← Write-Ahead Log (shared across collections)
  <collection_name>/
    data.db
    primary.idx
    idx_*.idx         ← secondary indexes
```

### 14.2 Structure

```rust
pub struct Database {
    path: PathBuf,
    collections: HashMap<String, Collection>,
    wal: WalWriter,
    writes_since_checkpoint: u32,
}
```

### 14.3 API

```rust
impl Database {
    pub fn open(path: &Path) -> Result<Self>;

    // Collection management
    pub fn create_collection(&mut self, name: &str) -> Result<()>;
    pub fn drop_collection(&mut self, name: &str) -> Result<()>;
    pub fn list_collections(&self) -> Vec<&str>;

    // CRUD (routed to named collection)
    pub fn insert(&mut self, collection: &str, key: Uuid, value: Value) -> Result<()>;
    pub fn get(&mut self, collection: &str, key: &Uuid) -> Result<Option<Value>>;
    pub fn update(&mut self, collection: &str, key: &Uuid, value: Value) -> Result<()>;
    pub fn delete(&mut self, collection: &str, key: &Uuid) -> Result<()>;
    pub fn scan(&mut self, collection: &str, range: impl RangeBounds<Uuid>) -> Result<Vec<(Uuid, Value)>>;

    // Index management
    pub fn create_index(&mut self, collection: &str, name: &str, field_path: &str) -> Result<()>;
    pub fn drop_index(&mut self, collection: &str, name: &str) -> Result<()>;
    pub fn query(&mut self, collection: &str, index: &str, value: &Value) -> Result<Vec<(Uuid, Value)>>;
    pub fn query_range(&mut self, collection: &str, index: &str, start: &Value, end: &Value) -> Result<Vec<(Uuid, Value)>>;

    // Reference resolution
    pub fn resolve_ref(&mut self, collection: &str, id: &Uuid) -> Result<Option<Value>>;
    pub fn resolve_deep(&mut self, value: &Value, max_depth: usize) -> Result<Value>;

    // Maintenance
    pub fn flush(&mut self) -> Result<()>;
    pub fn compact(&mut self, collection: &str) -> Result<u64>;
    pub fn document_count(&mut self, collection: &str) -> Result<u64>;
    pub fn close(self) -> Result<()>;
}
```

### 14.4 Name Validation (`src/naming.rs`)

All names (collections, indexes) are validated: `[a-z0-9_]{1,64}`, no path separators, no dots. Names starting with `_` are reserved (exceptions: `_default`, `_system`).

### 14.5 Auto-discovery

On `Database::open()`, existing collections are discovered by scanning subdirectories for `data.db` files. No separate catalogue file is needed.

## 15. grumpy-repl — Interactive REPL

The `grumpy-repl` workspace crate (binary `grumpy-repl`) provides a JavaScript-like shell for exploring GrumpyDB interactively.

### 15.1 Usage

```bash
# Embedded mode (direct disk access, no server needed)
cargo run -p grumpy-repl                           # launch REPL
cargo run -p grumpy-repl -- --data ./mydata        # custom data dir
cargo run -p grumpy-repl -- --eval "use test; db.users.count()"  # one-shot

# Connected mode (TCP client to a running GrumpyDB server)
cargo run -p grumpy-repl -- --host localhost --port 6380 --tenant acme --user alice
cargo run -p grumpy-repl -- --host localhost --no-tls --tenant acme --user admin --password "<bootstrap-password>"
```

### 15.2 Commands

```js
use mydb                                    // open/create database
db.createCollection("users")               // collection management
db.collections()
db.users.insert({ name: "Alice", age: 30 }) // insert JSON document
db.users.find()                            // list all documents
db.users.find({ age: 30 })                 // filter (client-side)
db.users.get("uuid-prefix")               // get by ID prefix
db.users.update("uuid", { ... })           // update document
db.users.delete("uuid")                    // delete document
db.users.createIndex("by_age", "age")      // secondary index
db.users.query("by_age", 30)              // exact-match query
db.users.queryRange("by_age", 20, 30)     // range query
db.users.count()                           // document count
db.users.compact()                         // compaction
db.orders.insert({ owner: $ref("users", "uuid") })  // document reference
db.orders.resolve("uuid")                 // resolve one level of refs
db.orders.resolveDeep("uuid")             // recursive resolve (max 16)
db.orders.resolveDeep("uuid", 5)          // recursive resolve (max 5)
help                                       // command reference
```

### 15.3 Architecture

| File | Role |
|------|------|
| `main.rs` | CLI entry: `--data`, `--eval`, `--host`, `--port`, `--tenant`, `--user`, `--password`, `--tls`/`--no-tls`, `--embedded` flags |
| `repl.rs` | Read-eval-print loop, session state, command execution (routes to embedded or TCP backend) |
| `parser.rs` | Command parser: `Command` enum, tokenizer |
| `json_parser.rs` | Relaxed JSON parser (unquoted keys, single quotes, trailing commas, `$ref()`) |
| `filter.rs` | Client-side document matching for `find({ field: value })` |
| `tcp_backend.rs` | TCP backend: wraps `grumpydb-client` with `tokio::Runtime::block_on()` for synchronous shell |

Relaxed JSON: unquoted keys, single/double quotes, trailing commas. Uses `rustyline` for line editing and persistent history (`~/.grumpy_repl_history`).

## 16. Server & Client (Multi-Tenant)

### 16.1 On-disk layout

```
<server_root>/
  <client_name>/                     ← one directory per client
    <database_name>/                 ← one directory per database
      wal.log
      <collection_name>/
        data.db
        primary.idx
        idx_*.idx
```

### 16.2 GrumpyServer

```rust
pub struct GrumpyServer {
    path: PathBuf,
    clients: HashMap<String, Client>,
}

impl GrumpyServer {
    pub fn open(path: &Path) -> Result<Self>;       // create dir, auto-discover clients
    pub fn create_client(&mut self, name: &str) -> Result<()>;
    pub fn drop_client(&mut self, name: &str) -> Result<()>;
    pub fn client(&mut self, name: &str) -> Result<&mut Client>;
    pub fn list_clients(&self) -> Vec<&str>;
    pub fn close(self) -> Result<()>;
}
```

### 16.3 Client

```rust
pub struct Client {
    name: String,
    path: PathBuf,
    databases: HashMap<String, Database>,
}

impl Client {
    pub fn open(path: &Path, name: &str) -> Result<Self>;  // create dir, auto-discover databases
    pub fn create_database(&mut self, name: &str) -> Result<()>;
    pub fn drop_database(&mut self, name: &str) -> Result<()>;
    pub fn database(&mut self, name: &str) -> Result<&mut Database>;
    pub fn list_databases(&self) -> Vec<&str>;
    pub fn close(self) -> Result<()>;
}
```

### 16.4 Auto-discovery

Both `GrumpyServer` and `Client` auto-discover existing children by scanning subdirectories:
- Server scans for client directories (skipping hidden dirs)
- Client scans for database directories (identified by `wal.log` or collection subdirectories with `data.db`)

## 17. Concurrency v2 (Per-Database SWMR)

### 17.1 SharedDatabase

Wraps a `Database` in `Arc<RwLock>` for thread-safe per-database access.

```rust
#[derive(Clone)]
pub struct SharedDatabase {
    inner: Arc<RwLock<Database>>,
}

impl SharedDatabase {
    pub fn new(db: Database) -> Self;
    pub fn open(path: &Path) -> Result<Self>;

    // Collection management
    pub fn create_collection(&self, name: &str) -> Result<()>;
    pub fn drop_collection(&self, name: &str) -> Result<()>;
    pub fn list_collections(&self) -> Vec<String>;

    // CRUD (acquires write lock)
    pub fn insert(&self, collection: &str, key: Uuid, value: Value) -> Result<()>;
    pub fn get(&self, collection: &str, key: &Uuid) -> Result<Option<Value>>;
    pub fn update(&self, collection: &str, key: &Uuid, value: Value) -> Result<()>;
    pub fn delete(&self, collection: &str, key: &Uuid) -> Result<()>;
    pub fn scan(&self, collection: &str, range: impl RangeBounds<Uuid>) -> Result<Vec<(Uuid, Value)>>;

    // Index introspection (added in v5 P1, used by `LIST INDEXES` over the wire).
    pub fn list_indexes(&self, collection: &str) -> Result<Vec<String>>;

    // Index, resolve, maintenance...
    pub fn close(self) -> Result<()>;
}
```

Multiple threads can read concurrently. Writes acquire an exclusive lock.
Clone is cheap (Arc clone).

### 17.2 SharedServer

Wraps a `GrumpyServer` with per-database independent locking.

```rust
#[derive(Clone)]
pub struct SharedServer {
    server: Arc<RwLock<GrumpyServer>>,
    databases: Arc<RwLock<HashMap<String, SharedDatabase>>>,
}

impl SharedServer {
    pub fn open(path: &Path) -> Result<Self>;
    pub fn create_client(&self, name: &str) -> Result<()>;
    pub fn drop_client(&self, name: &str) -> Result<()>;
    pub fn list_clients(&self) -> Vec<String>;
    pub fn create_database(&self, client: &str, db_name: &str) -> Result<()>;
    pub fn drop_database(&self, client: &str, db_name: &str) -> Result<()>;
    pub fn list_databases(&self, client: &str) -> Result<Vec<String>>;
    pub fn database(&self, client: &str, db_name: &str) -> Result<SharedDatabase>;
    pub fn close(self) -> Result<()>;
}
```

Each database gets its own `SharedDatabase` with independent locking.
Concurrent writes to **different databases** proceed without contention.
Within a single database, SWMR rules apply (1 writer OR N readers).

### 17.3 Concurrency Wrappers Summary

| Wrapper | Wraps | Scope | Use case |
|---------|-------|-------|----------|
| `SharedDb` | `GrumpyDb` | Single collection | Backward compat, simple use |
| `SharedDatabase` | `Database` | Multi-collection | Per-database concurrent access |
| `SharedServer` | `GrumpyServer` | Multi-tenant | Server-wide concurrent access |

## 18. v1 → v2 Migration

`GrumpyDb::migrate_to_database()` provides a one-shot migration from the v1
single-collection engine to a v2 `Database` collection.

### Algorithm

1. `scan(..)` all documents from the v1 `GrumpyDb`.
2. Ensure the target collection exists in the `Database` (create if absent).
3. `insert()` each document into the target collection.
4. Return the number of migrated documents.

### Usage

```rust
let mut v1 = GrumpyDb::open(Path::new("./old_db")).unwrap();
let mut v2 = Database::open(Path::new("./new_db")).unwrap();
let count = v1.migrate_to_database(&mut v2, "my_collection").unwrap();
println!("Migrated {count} documents");
```

The original v1 data is **not modified** — this is a copy, not a move.

## 19. Network Layer (v3)

> **Status**: Implemented (phases 16–22 complete, phase 23 partial — formal e2e/CI/Docker deferred). See `docs/IMPLEMENTATION_PLAN_V3.md` for the phased plan.

GrumpyDB is both an embedded engine and a **networked, secured, multi-tenant database server**. The existing engine crate (`src/`) remains unchanged and usable as an embedded library; the network layer is layered on top through three additional workspace crates plus a TypeScript driver.

### 19.1 Workspace topology

| Crate / package | Kind | Purpose |
|-----------------|------|---------|
| `grumpydb` | library | Storage engine (unchanged from v2) |
| `grumpydb-protocol` | library | RESP-like wire protocol — `Command`, `Response`, parser |
| `grumpydb-server` | binary + lib | TCP/TLS server, JWT auth, RBAC enforcer |
| `grumpydb-client` | library | Async Rust client driver |
| `@grumpydb/client` | npm package | TypeScript/Node.js client driver (zero runtime deps) |

### 19.2 Wire protocol (`grumpydb-protocol`)

RESP-like single-line text protocol terminated by `\r\n`.

**Constants** (`grumpydb-protocol/src/lib.rs`):

| Constant | Value | Purpose |
|----------|-------|---------|
| `DEFAULT_PORT` | `6380` | Default TCP port |
| `PROTOCOL_VERSION` | `"4.0.0"` | Banner string |
| `MAX_LINE_LENGTH` | `1_048_576` (1 MiB) | DoS protection |
| `MAX_BULK_LENGTH` | `16_777_216` (16 MiB) | Max document size on the wire |

**Response framing**:

```
+<message>\r\n              Simple string (success)
-ERR <message>\r\n          Error
:<integer>\r\n              Integer
$<length>\r\n<data>\r\n     Bulk string (binary-safe payload)
$-1\r\n                     Null bulk
*<count>\r\n...             Array (followed by count framed responses)
```

**Connection lifecycle**:

```
Client                                            Server
  │── TCP connect ───────────────────────────────→│
  │←── TLS handshake (rustls 1.2/1.3) ────────────│
  │←── +GRUMPYDB 4.0.0\r\n ───────────────────────│  banner
  │── LOGIN <tenant> <user> <pwd>\r\n ───────────→│
  │←── +TOKEN <access> <refresh>\r\n ─────────────│
  │── TOKEN <access>\r\n ────────────────────────→│
  │←── +OK\r\n ───────────────────────────────────│
  │── USE <db>\r\n ──────────────────────────────→│
  │←── +OK\r\n ───────────────────────────────────│
  │── INSERT <coll> <uuid> <json>\r\n ───────────→│   (RBAC checked)
  │←── +OK\r\n ───────────────────────────────────│
  │── QUIT\r\n ──────────────────────────────────→│
  │←── +BYE\r\n ──────────────────────────────────│
```

**Command groups** (`grumpydb-protocol::Command` enum):

| Group | Commands |
|-------|----------|
| Auth | `LOGIN`, `TOKEN`, `REFRESH`, `WHOAMI` |
| Session | `USE`, `PING`, `QUIT`, `TOPOLOGY`, `SNAPSHOT_HLC` |
| Database | `CREATE DATABASE`, `DROP DATABASE`, `LIST DATABASES` |
| Collection | `CREATE COLLECTION`, `DROP COLLECTION`, `LIST COLLECTIONS` |
| CRUD | `INSERT`, `GET`, `UPDATE`, `DELETE`, `PUT_WITH_VC`, `SCAN` |
| Index | `CREATE INDEX`, `DROP INDEX`, `LIST INDEXES`, `QUERY`, `QUERYRANGE` |
| Maintenance | `COMPACT`, `FLUSH`, `COUNT` |
| User mgmt | `CREATE USER`, `DROP USER`, `LIST USERS [@tenant]`, `GRANT`, `REVOKE` |
| Tenant mgmt | `CREATE TENANT`, `DROP TENANT`, `LIST TENANTS` |
| Cluster mgmt | `ELECT-WRITER` |

Consistency prefixes (Phase 40f) are parsed as a wrapper around the base
command: `READ_CONCERN R=<n>` and/or `WRITE_CONCERN W=<n>`. In v5, concerns
are accepted only for data commands (`INSERT`, `GET`, `UPDATE`, `DELETE`,
`PUT_WITH_VC`, `SCAN`, `QUERY`, `QUERYRANGE`, `COUNT`) and only with
`R=1, W=1`; otherwise the server returns
`-ERR v5 only supports R=1, W=1`.

Each `Command` carries RBAC metadata via `Command::required_action() -> Action` and `Command::target_resource() -> Resource`, used by the session enforcer.

**Naming conventions on the wire**:

| Syntax | Meaning |
|--------|---------|
| `alice` | User in current tenant (LOGIN, CREATE USER) |
| `alice@acme` | User in explicit tenant |
| `mydb@acme` | Database in explicit tenant |
| `users:mydb` | Collection in database |
| `users:mydb@acme` | Fully-qualified collection (used by GRANT/REVOKE) |
| `@acme` | Tenant-only scope |

### 19.3 Authentication & RBAC (`grumpydb-server::auth`)

**Modules**:

| Module | Responsibility |
|--------|----------------|
| `auth::user` | `User` struct, argon2 password hash/verify, `AuthError` |
| `auth::role` | `RoleName`, `Action`, `ResourceScope`, `RoleAssignment::permits()` |
| `auth::jwt` | `JwtConfig`, `Claims`, HS256 generate/verify (access + refresh) |
| `auth::store` | `AuthStore` — user CRUD, `secret.key`, on-disk persistence |

**Predefined roles** (`auth::role::RoleName`):

| Role | Permissions |
|------|-------------|
| `server_admin` | Cross-tenant: create/drop tenants, manage all users, full CRUD |
| `tenant_admin` | Within tenant: create/drop databases, manage users, full CRUD |
| `db_admin` | Within database: create/drop collections + indexes, compact, full CRUD |
| `read_write` | INSERT, GET, UPDATE, DELETE, SCAN, QUERY |
| `read_only` | GET, SCAN, QUERY only |

**JWT (HS256)** — payload:

```json
{
  "sub": "alice",
  "tenant": "acme",
  "roles": [{ "role": "read_write", "scope": { "Database": "myapp" } }],
  "iat": 1745740800,
  "exp": 1745744400
}
```

Default lifetimes: access 1 h, refresh 7 d (configurable). Secret key (32 random bytes) is generated on first boot and persisted in `_auth/secret.key`.

**On-disk auth layout**:

```
<data_dir>/
  _auth/
    secret.key                       ← 32-byte HMAC key
    users/
      <tenant>__<username>.json      ← User record (argon2 hash + roles)
```

A default `admin` user is bootstrapped in tenant `_system` with role
`server_admin` only when:

1. The auth directory `_auth/users/` is empty on startup, **and**
2. A bootstrap password is supplied either via the CLI flag
   `--bootstrap-password <pw>` or the environment variable
   `GRUMPYDB_BOOTSTRAP_PASSWORD`.

`AuthStore::open` takes that password as its 4th argument
(`bootstrap_password: Option<&str>`). If users already exist on disk, the
parameter is ignored. If no users exist on disk and no password is supplied,
`AuthStore::open` returns `Err(AuthError::BootstrapRefused(...))` and the
server refuses to start. The legacy silent `_system/admin/admin` default is
gone.

Bootstrap passwords shorter than 8 characters emit a warning. The
`secret.key` file is created with mode `0600` on Unix; existing files with
group/world bits are detected and re-tightened with a warning logged.

`AuthError` variants: `HashError`, `InvalidCredentials`, `UserNotFound`,
`UserAlreadyExists`, `JwtError`, `TokenExpired`, `InvalidToken`,
`AccessDenied`, `NotAuthenticated`, `Io`, `ClockError` (for
`SystemTime::now()` failures replacing former `unwrap()` sites), `ReadOnly`,
`PasswordChangeRequired`, `BootstrapRefused`.

**RBAC enforcer** (`grumpydb-server::session::SessionContext`):

```rust
pub struct SessionContext {
    pub claims: Option<Claims>,       // None until LOGIN/TOKEN
    pub current_db: Option<String>,   // None until USE
}

impl SessionContext {
    pub fn is_authenticated(&self) -> bool;
    pub fn tenant(&self) -> Result<&str>;
    pub fn authorize(&self, command: &Command) -> Result<(), AuthError>;
}
```

`authorize()` rejects any command other than `LOGIN`, `TOKEN`, `REFRESH`, `PING`, `QUIT` before authentication. After authentication it checks every `RoleAssignment` in the JWT claims against `command.required_action()` and `command.target_resource()`.

### 19.4 TCP server (`grumpydb-server::tcp`)

**Layout**:

| File | Responsibility |
|------|----------------|
| `tcp/listener.rs` | Bind + accept loop, TLS handshake, graceful shutdown (SIGINT/SIGTERM) |
| `tcp/handler.rs` | Per-connection: read → parse → authorize → execute → respond |
| `config.rs` | `ServerConfig` — bind, data dir, TLS, auth TTLs (TOML + CLI) |
| `main.rs` | Binary entry point — argument parsing + listener startup |

**Three modes**:

| Mode | Config | Usage |
|------|--------|-------|
| Plaintext | `tls.enabled = false` or `--no-tls` | Development |
| TLS | `tls.enabled = true` (default) | Production — auto-generates self-signed cert via `rcgen` if files absent |
| mTLS | TLS + client CA | Reserved for future use |

**Default config** (`grumpydb.toml`):

```toml
[server]
bind = "0.0.0.0:6380"
max_connections = 1024
data_dir = "./data"

[tls]
enabled = true
# cert_file / key_file auto-generated if absent

[auth]
access_token_ttl_secs  = 3600     # 1 h
refresh_token_ttl_secs = 604800   # 7 d
```

**Connection handler** (`tcp/handler.rs`):

```rust
pub async fn handle_connection<S>(
    stream: S,
    peer: SocketAddr,
    auth_store: Arc<RwLock<AuthStore>>,
    shared_server: SharedServer,
    limits: Arc<Limits>,
    coordinator: Arc<Coordinator>,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: AsyncRead + AsyncWrite + Unpin;
```

Each connection is spawned in its own tokio task. The handler:

1. Reads a line (enforcing `MAX_LINE_LENGTH`).
2. Parses via `grumpydb_protocol::parse_command`.
3. Validates optional consistency prefixes (`READ_CONCERN` / `WRITE_CONCERN`) via
  the coordinator. In v6 Phase 45 tranche 2, validation accepts bounded
  `WRITE_CONCERN W` in `[1, N]` while `READ_CONCERN R` remains pinned to `1`.
  Read/non-write commands carrying `WRITE_CONCERN` return a clear validation
  error (`-ERR consistency concerns are only supported for data commands`).
4. Calls `session.authorize(&cmd)` (returns `-ERR access denied` on failure).
5. For key-based write commands, applies runtime write-concern validation after
  auth and before execution. In v6 Phase 45 tranche 2, requested `W` must be
  `<=` currently live replicas in the key preference list. Liveness uses peer
  status (`down` is unavailable; `unknown`/`suspect` are treated as
  potentially available).
6. Dispatches to `execute_command` which maps to `SharedServer` calls.
7. For key-based data commands, applies split routing checks:
   - Read paths (`GET`, `GET_WITH_VC`) enforce primary-owner placement through
     `Coordinator::enforce_local_owner`; when local node is not owner, returns
     `-ERR forward to <node>@<addr>; not the owner`.
   - Write paths (`INSERT`, `UPDATE`, `DELETE`, `PUT_WITH_VC`) enforce local
     write-replica admission through `Coordinator::enforce_local_write_replica`.
     In v6 Phase 45 tranche 1, writes are accepted on any local replica in the
     preference list (`N = min(3, cluster_size)`); otherwise the handler returns
     `-ERR forward to <node>@<addr>; local node is outside write replica set`.
  In Phase 42 tranche 2, both drivers parse this hint and perform a single
  automatic one-hop retry to the forwarded target.
8. `TOPOLOGY` returns a JSON topology view (`cluster_id`, `local_node_id`,
  `n`, `vnodes_per_node`, `peers`, `writers`) from
  `Coordinator::topology_json()`. In v6 Phase 44 tranche 1, each peer entry
  includes live `status` and `last_seen_at_unix` liveness fields plus
  `vnode_assignments` metadata.
9. `SNAPSHOT_HLC` returns the current selected-database snapshot HLC as an
  integer response (via `db.begin_read().snapshot_hlc()`).
10. Writes the `Response` back to the stream.

Listener startup (`tcp/listener.rs`) also spawns a background gossip probe
task in v6 Phase 44 tranche 1. The task periodically handshakes configured
peers and refreshes coordinator liveness used by `TOPOLOGY`.

**Panic isolation**: every `execute_command` invocation is wrapped in
`AssertUnwindSafe(...).catch_unwind().await` (via the `futures` crate's
`FutureExt`). If the engine panics on a corrupt page or document, the panic
is caught, logged via `tracing::error!`, and the client receives
`Response::Error("internal error (corruption): …")` instead of having its
connection (and the whole server task) torn down. A panic on one
connection cannot affect any other connection.

**Tenant auto-provisioning**: a successful `LOGIN` for an existing user creates the matching tenant client directory if it does not yet exist. `USE <db>` similarly creates the database on demand (subject to RBAC).

**Listing semantics**:

- `LIST TENANTS` filters out the internal `_auth` directory (where authentication metadata lives) so it never appears as a tenant.
- `LIST USERS` accepts an optional `@<tenant>` suffix (`LIST USERS @acme`) to filter to a specific tenant; `LIST USERS` without suffix lists users in the caller's current tenant.
- `CREATE USER` and `LOGIN` both accept the `username@tenant` notation. When the suffix is omitted, the caller's tenant (or the tenant from the LOGIN parameters) is used.
- `GRANT` / `REVOKE` accept resource notation `[collection:][db][@tenant]` — e.g. `mydb`, `mydb@acme`, `users:mydb`, `users:mydb@acme`, or just `@acme` for tenant-scoped grants.

### 19.5 Rust driver (`grumpydb-client`)

```rust
use grumpydb_client::GrumpyClient;

let mut client = GrumpyClient::connect("localhost", 6380, false).await?;
client.login("acme", "alice", "s3cr3t").await?;

let db = client.database("myapp").await?;
let key = uuid::Uuid::new_v4();
db.insert("users", key, &serde_json::json!({"name": "Bob"})).await?;
let doc = db.get("users", &key).await?;
db.delete("users", &key).await?;

client.raw_execute("PING").await?;
client.close().await?;
```

| Module | Role |
|--------|------|
| `lib.rs` | `GrumpyClient` (connect, login, database, raw_execute) + `DatabaseHandle` (CRUD, index, admin) |
| `connection.rs` | TCP / `tokio_rustls::TlsStream`, line-based I/O, `NoCertVerifier` for dev TLS |
| `error.rs` | `ClientError` — `Connection`, `Auth`, `Protocol`, `Server`, `Timeout` |

The driver depends on `grumpydb-protocol` for parsing responses, so wire format changes are caught at compile time on both sides.

Phase 42 (Smart Client Drivers) is closed for v5 scope. Delivered:
`connect_cluster(seeds, tls)`, topology fetch/cache APIs
(`refresh_topology`, `topology`, `cached_topology`), seed-fallback/topology-cache E2E tests,
automatic one-hop forward fallback with session replay (`TOKEN`, `USE`), and
`DatabaseHandle::get_with_siblings`, plus optional JWKS URL configuration
with RS256 access-token verification on `login`.
Current v5 limitation: `get_with_siblings` returns a singleton sibling with
placeholder vector clock `"{}"`.
Remaining beyond v5 scope for the Rust driver: ring-aware routing beyond one
hop and sibling reconciliation semantics.

### 19.6 TypeScript driver (`@grumpydb/client`)

```typescript
import { GrumpyClient } from '@grumpydb/client';

const client = await GrumpyClient.connect({
  host: 'localhost', port: 6380, tls: false,
  tenant: 'acme', username: 'alice', password: 's3cr3t',
});

const db = client.database('myapp');
await db.insert('users', crypto.randomUUID(), { name: 'Bob' });
const doc = await db.get('users', '<uuid>');
await client.close();
```

| File | Role |
|------|------|
| `src/index.ts` | Public exports |
| `src/connection.ts` | `node:net` + `node:tls`, line-based I/O, exponential backoff reconnect |
| `src/protocol.ts` | RESP-like encode/decode (mirrors `grumpydb-protocol`) |
| `src/auth.ts` | LOGIN, JWT storage, auto-refresh on `token expired` |
| `src/client.ts` | `GrumpyClient` class |
| `src/database.ts` | `DatabaseHandle` with CRUD, index, admin |
| `src/types.ts` | `ConnectOptions`, `UserInfo`, value types |
| `src/errors.ts` | `GrumpyError`, `ConnectionError`, `AuthError`, `ProtocolError`, `ServerError` |

Zero runtime dependencies — only Node built-ins (`node:net`, `node:tls`, `node:crypto`). Test runner: `vitest` (dev only). Requires Node ≥ 18.

Phase 42 (Smart Client Drivers) is closed for v5 scope. Delivered:
`connectCluster({ seeds, ... })`, topology fetch/cache APIs
(`refreshTopology`, `topology`), exported cluster/topology types
(`ClusterConnectOptions`, `ClusterTopology`), automatic one-hop forward fallback
with session replay (`TOKEN`, `USE`), and `DatabaseHandle.getWithSiblings`.
The driver also supports `jwksUrl` with JWKS cache + RS256 verification
on login, and has CI coverage via the TypeScript lane
(`npm ci`, `npm run lint`, `npm test`, `npm run build`).
Current v5 limitation: `getWithSiblings` returns a singleton sibling with
placeholder vector clock `"{}"`.
Remaining Phase 42 work for the TypeScript driver: ring-aware routing beyond
one hop, sibling reconciliation semantics, and publish workflow integration.

### 19.7 grumpy-repl v2 (`grumpy-repl/`)

Dual-mode REPL workspace crate — connected (TCP via `grumpydb-client`) when `--host` is given, embedded (direct disk access) otherwise (or with `--embedded`).

```bash
# Embedded
cargo run -p grumpy-repl
cargo run -p grumpy-repl -- --embedded --data ./mydata

# Connected (TCP)
cargo run -p grumpy-repl -- --host localhost --port 6380 \
    --tenant acme --user alice --password s3cr3t
cargo run -p grumpy-repl -- --host localhost --no-tls \
    --tenant _system --user admin --password "<bootstrap-password>"
```

`tcp_backend.rs` wraps `GrumpyClient` and uses `tokio::runtime::Runtime::block_on()` so the REPL itself stays synchronous. Parsed shell commands are translated to protocol command strings and dispatched via `client.raw_execute()`. Responses are pretty-printed (JSON formatting, arrays).

### 19.8 Module dependency graph (v3)

```
grumpydb (engine)
   ▲
   │
grumpydb-server  ──────► grumpydb-protocol ◄────── grumpydb-client
   │                                                    │
   ▼                                                    ▼
[binary: tcp listener,                          [embedded in grumpy-repl
 auth store, RBAC,                               connected mode and
 SharedServer]                                   user applications]

drivers/typescript (@grumpydb/client)
   └── reimplements protocol in TypeScript, no Rust dep
```


## 20. Observability (v5 P1)

The `grumpydb-server` binary emits **structured logs via [`tracing`]** so
that operators can pipe events into log aggregators (jq, Loki, Datadog,
Splunk…) without parsing free-form text.

### 20.1 Output formats
- **JSON by default** (one event per line, suitable for `| jq`).
- **Text** (human-readable) when stdout is detected as a TTY, or when forced
  via the new CLI flag `--log-format text`.
- The format is selectable explicitly: `--log-format json|text`.
- The standard `RUST_LOG` environment variable is honored via
  `tracing-subscriber`'s `EnvFilter`, e.g.
  `RUST_LOG=grumpydb_server=debug,tokio=warn`.

### 20.2 Span hierarchy
Every emitted event is enclosed in nested spans:

```
connection                                  ← per TCP/TLS accept
  ├─ peer  = "127.0.0.1:54321"
  └─ tls   = true | false

  └─► command                               ← per request
        ├─ cmd     = "INSERT" | "GET" | …    (stable label, low cardinality)
        ├─ user    = "<authenticated user>" | absent for pre-auth commands
        ├─ tenant  = "<client name>"        | absent for pre-auth commands
        └─ elapsed_us = <integer>           ← emitted on completion
```

The stable `cmd` label comes from a small helper
`command_name(&Command) -> &'static str` so that downstream metrics
backends see a fixed cardinality.

### 20.3 Notable events
- **Auth events** at `info`: `login` (success/failure), `token_refresh`,
  `token_verify`. All carry structured fields (no PII in the payload).
- **Error events** at `warn`/`error`: every `GrumpyError` returned to a
  client is logged inside its `command` span, so the surrounding
  `connection` and `command` context is preserved.
- **Panic isolation events** (Phase 25): if a command handler panics,
  `tracing::error!` records the payload before the connection is closed
  cleanly with `Response::Error("internal error (corruption): …")`.

### 20.4 Dependencies and configuration
- `tracing-subscriber` is configured with the features
  `["env-filter", "json"]`.
- The default subscriber filter applies if `RUST_LOG` is unset.
- Trace-ID propagation in protocol responses (an optional `X-Trace-Id`
  field) is **not yet implemented**; tracked as future work for v6.

[`tracing`]: https://docs.rs/tracing


## 21. Testing

GrumpyDB combines four layers of tests, all run on every CI build:

### 21.1 Unit tests
Co-located in each `.rs` file under `#[cfg(test)] mod tests`. They cover
isolated logic: page slot insertion, B+Tree node splits, WAL record
encoding, JSON parsing, RBAC predicates, etc.

```bash
cargo test --lib            # current crate
cargo test --workspace --lib
```

### 21.2 Integration tests (workspace `tests/`)
| File | Purpose |
|------|---------|
| `tests/crud_test.rs` | End-to-end engine CRUD against the `grumpydb` library. |
| `tests/stress_test.rs` | Concurrency stress against the engine (SWMR). |
| `tests/server_e2e.rs` | Full client → server → engine → response loop, 8 tests. Uses `TestServer` to spawn the real `grumpydb-server` binary on a random port. |
| `tests/server_concurrency.rs` | 50 concurrent clients × 100 ops each. |
| `tests/server_auth.rs` | Expired token, tampered token, role enforcement (3 tests). |
| `tests/crash_recovery.rs` | 6 crash-and-restart scenarios using `TestServer::crash()` (SIGKILL) + `TestServer::restart()`: post-FLUSH crash, no-flush crash, mid-insert partial crash, crash during index creation, crash during compaction, repeated crash recovery loop. |

The server-spawning helper lives in the internal crate
**`grumpydb-testing/`** (`publish = false`, never released). It exposes a
`TestServer` struct that spawns the actual server binary on a random port
with a tempdir, kills it on `Drop`, and provides `crash()` (SIGKILL) and
`restart()` (respawn on the same data dir + port) for crash-recovery
tests. Startup readiness uses up to 3 retry attempts and a configurable
timeout via `GRUMPYDB_TEST_SERVER_STARTUP_TIMEOUT_SECS` (default: 60s).
If the server exits before readiness, the harness includes captured stderr
in the failure diagnostics.

### 21.3 Benchmarks (`benches/`)
Criterion-based, with HTML reports under `target/criterion/report/`.
- **`benches/engine.rs`** — 8 benchmarks: insert (small / medium / 4 KB
  overflow), get (warm / cold reopen), scan, index exact + range queries.
- **`benches/protocol.rs`** — 3 benchmarks: parse simple command, parse
  1 KB `INSERT`, serialize 100-bulk array.

```bash
cargo bench                # all benches
cargo bench -- --quick     # smoke run (used by CI)
```

Headline numbers are reproduced in the README's *Performance* section.

### 21.4 Fuzzing (`fuzz/`, excluded from workspace)
`cargo-fuzz` targets focused on the network-attackable surface. The
`fuzz/` crate is intentionally excluded from the workspace so it does not
pollute normal builds.

| Target | Surface |
|--------|---------|
| `parse_command` | RESP-like protocol parser. |
| `value_codec_roundtrip` | Document binary codec encode→decode stability. |
| `wal_record_decode` | WAL record decoder. |
| `response_serialize` | Protocol response serializer. |

```bash
cd fuzz && cargo +nightly fuzz run parse_command
```

### 21.5 CI workflows
- **`.github/workflows/ci.yml`** — jobs `fmt`, `clippy`, `test` (matrix:
  stable + 1.85 MSRV), `docs`, `audit`, **`bench-smoke`** (compile +
  `--quick` run of all benches; not a regression gate).
- **`.github/workflows/fuzz.yml`** — weekly schedule + manual dispatch,
  runs each fuzz target for 5 minutes by default.


## 22. Rate Limiting (v5 P2)

The `grumpydb-server` enforces three layers of rate limiting at the
network edge to make brute-force impractical without breaking
legitimate clients. The implementation lives in
`grumpydb-server/src/limits.rs` and is configured by the new `[limits]`
section of the server TOML config (`LimitsSection` → `LimitsConfig`).

### 22.1 Per-IP token bucket (commands)
Built on `governor 0.6` + `nonzero_ext 0.3`. Each remote IP has its
own `RateLimiter` instance:
- `commands_per_sec_per_ip` (default **100**) — sustained rate.
- `commands_burst_per_ip` (default **200**) — burst capacity.

A blocked command is responded to with a `RateLimited` error and
counted under `grumpydb_rate_limit_hits_total{kind="command"}`.

### 22.2 Per-IP failed-login back-off
After `failed_logins_per_min_per_ip` (default **5**) bad logins from
the same IP within a one-minute window, subsequent attempts from that
IP are delayed with exponential back-off: **1 s, 2 s, 4 s, 8 s, 16 s,
32 s, capped at 60 s**. The state is held in a `parking_lot::Mutex`
keyed by IP. Hits are counted under
`grumpydb_login_failures_total{reason}` and
`grumpydb_rate_limit_hits_total{kind="login"}`.

### 22.3 Connection caps
Enforced inside `tcp/listener.rs` at accept time:
- `max_conns_per_ip` (default **100**) — per-IP simultaneous accepts.
- `max_conns_global` (default **10 000**) — across the entire process.

Refused connections are dropped immediately and counted; healthy peers
keep their existing connections.

### 22.4 Loopback bypass
The `bypass_loopback` flag (default **`true`**) exempts loopback
addresses (`127.0.0.0/8`, `::1`) from all three limiters. This keeps
the development experience and the in-process integration tests fast.

> **Production note**: deployments that expose loopback to untrusted
> callers (for example shared-host/PaaS scenarios) must set
> `bypass_loopback = false`. The integration test
> `test_e2e_login_rate_limited` in `tests/server_auth.rs` exercises the
> non-bypassed path end-to-end.

### 22.5 Defaults summary

```toml
[limits]
commands_per_sec_per_ip       = 100
commands_burst_per_ip         = 200
failed_logins_per_min_per_ip  = 5
max_conns_per_ip              = 100
max_conns_global              = 10_000
bypass_loopback               = true
```


## 23. Observability HTTP Endpoints (v5 P2)

The `grumpydb-server` runs a tiny `hyper 1.x` HTTP server on a separate
port (default `0.0.0.0:6381`, configurable via the `[http]` section).
This server is dedicated to operator/monitoring traffic — it is **not**
the database protocol, which keeps running on its TCP/TLS port.

### 23.1 Endpoints

| Method + path | Status | Body |
|---------------|--------|------|
| `GET /healthz` | `200 OK` | Process is alive (the HTTP server itself is up). Used as a Docker `HEALTHCHECK`. |
| `GET /readyz`  | `200 OK` once the TCP listener has bound, else `503 Service Unavailable`. | Suitable for k8s readiness probes. |
| `GET /metrics` | `200 OK` with `Content-Type: text/plain; version=0.0.4` | Prometheus exposition format. |
| anything else  | `404 Not Found` | — |

`HttpState::ready` is an `AtomicBool` flipped (`Release`) by the TCP
listener once `TcpListener::bind` succeeds. `/readyz` reads it with
`Acquire`.

### 23.2 Metric catalog (initial set)

Every series is **DESCRIBED** at process start in
`http::init_metrics()` so a fresh `/metrics` call lists every series
even before the first event:

| Metric | Type | Wired in | Notes |
|--------|------|----------|-------|
| `grumpydb_connections_active` | gauge | `tcp/listener.rs` (accept/release) | — |
| `grumpydb_commands_total{cmd,result}` | counter | `tcp/handler.rs` around `execute_command` | — |
| `grumpydb_command_duration_seconds{cmd}` | histogram | same site | seconds |
| `grumpydb_buffer_pool_pages{state}` | gauge | **described only** in v5 | TODO: hook into the engine's buffer pool |
| `grumpydb_wal_records_total` | counter | **described only** in v5 | TODO: hook into the WAL writer |
| `grumpydb_login_failures_total{reason}` | counter | login back-off path | — |
| `grumpydb_rate_limit_hits_total{kind}` | counter | command + login limiters | `kind` ∈ `{"command","login","conn_per_ip","conn_global"}` |

The two engine-side metrics still emit zero — they will start moving
once the engine grows the corresponding hooks. They are listed
up-front so dashboards can be built today and stop showing "no data".

### 23.3 Configuration

```toml
[http]
bind = "0.0.0.0:6381"   # empty string disables the HTTP server entirely
```

### 23.4 No authentication, by design

The HTTP endpoints have **no authentication** in v5. The reasoning:
- `/healthz` and `/readyz` must be reachable by orchestrators
  (Kubernetes probes, Docker healthcheck) before any user exists.
- `/metrics` is consumed by Prometheus, which has no shared secret with
  the server.

This matches the standard practice for sidecar observability ports.
TODO logged for **v6**: consider opt-in basic-auth or IP allowlisting
for `/metrics`.

### 23.5 Test harness integration

`grumpydb-testing::TestServer` exposes the HTTP listener address as
`http_addr: SocketAddr`. End-to-end coverage lives in
`tests/server_http.rs` (`test_e2e_health_endpoints` and friends).


## 24. Backup & Restore (v5 P2)

The `grumpydb-server` binary ships two new subcommands for taking and
restoring full database snapshots. The implementation lives in
`grumpydb-server/src/snapshot.rs` and is reachable via the URL-scheme
dispatcher `Location::parse(&str)`.

### 24.1 CLI

```bash
grumpydb-server snapshot --data <dir> <DEST>
grumpydb-server restore  --data <dir> <SRC> [--force]
```

`<DEST>` and `<SRC>` are URLs:

| Scheme | Backend | Cargo feature |
|--------|---------|---------------|
| `<path>` (no scheme) | Local filesystem | always available |
| `s3://bucket/key` | AWS S3 (`aws-sdk-s3 1.x`) | `cloud-aws` |
| `az://container/blob` | Azure Blob (`azure_storage_blobs 0.21`) | `cloud-azure` |

Cloud backends are gated by their feature flag — building without them
keeps the binary lean and free of AWS/Azure SDK transitive deps.

### 24.2 Cloud authentication

Credentials never appear on the command line — only the URL does:

- **AWS**: standard credential chain (env, shared profile, EC2/ECS
  instance role) via `aws-config`.
- **Azure**: `DefaultAzureCredential` chain (env, managed identity, CLI
  login) via `azure_identity`. Falls back to a connection string read
  from `AZURE_STORAGE_CONNECTION_STRING` when explicitly set.

### 24.3 Archive format

A snapshot is a **gzipped tar archive** containing every relevant file
under the data directory, plus a manifest at the archive root:

```
backup.tar.gz
├── snapshot.json               (= MANIFEST_FILENAME)
├── _auth/secret.key
├── _auth/users.dat
├── <tenant>/<database>/wal.log
└── <tenant>/<database>/<collection>/{data.db, primary.idx, idx_*.idx}
```

`snapshot.json` (`MANIFEST_VERSION = 1`):

```json
{
  "version": 1,
  "created_at": "<RFC3339 UTC>",
  "grumpydb_version": "<crate version at snapshot time>",
  "files": [
    { "path": "<relative path>", "size": <bytes>, "sha256": "<hex>" },
    ...
  ]
}
```

Restore verifies every file's SHA-256 against the manifest and aborts
with a `ChecksumMismatch` error on the first discrepancy.

### 24.4 Online snapshot semantics (v5)

`snapshot::snapshot()` holds the **`SharedDatabase` write lock** for
the entire archive copy. Writers block for the duration; readers
continue normally. This is the simplest correct semantics on top of the
current SWMR model.

> Phase 41 tranche 1 (v5) introduced the snapshot read API
> (`ReadTx` / `SharedReadTx`) with `snapshot_hlc` capture.
> Phase 41 tranche 2 now applies HLC-based snapshot visibility in
> `src/database/mod.rs` via per-key in-memory version history and routes
> `SharedReadTx` reads through snapshot-aware methods.
> Phase 41 tranche 3 adds snapshot reader watermark tracking,
> in-memory version GC (preserve versions needed by active readers;
> collapse to latest when no readers remain), and `SharedReadTx`
> clone/drop integration with reader accounting.
> Phase 41 tranche 4 exposes snapshot HLC through the wire protocol
> (`SNAPSHOT_HLC`). Current limits: version history is still in-memory
> (not persisted), and lock-free immutable read path remains deferred.
> `snapshot()` still uses the `SharedDatabase` write lock in v5.

### 24.5 Restore safety

`restore` refuses to write into a non-empty data directory unless
`--force` is passed. This is a guard against accidentally clobbering a
live deployment.

### 24.6 Build matrix

The four feature combinations are exercised in CI and locally:

```bash
cargo build --workspace
cargo build --workspace --features grumpydb-server/cloud-aws
cargo build --workspace --features grumpydb-server/cloud-azure
cargo build --workspace --features grumpydb-server/cloud-aws,grumpydb-server/cloud-azure
```

All four also pass `cargo clippy --workspace --all-targets -- -D warnings`.

### 24.7 Testing

- **9 unit tests** in `grumpydb-server/src/snapshot.rs` — manifest
  round-trip, checksum mismatch detection, URL parsing, etc.
- **`tests/snapshot_e2e.rs`** — full snapshot → wipe → restore
  round-trip via `TestServer`, asserts that all data is identical.
- **`tests/snapshot_aws.rs`** and **`tests/snapshot_azure.rs`** —
  cloud round-trips, marked `#[ignore]` (require live cloud
  credentials; opt-in with `cargo test -- --ignored`).
