# GrumpyDB — Technical Architecture

## 1. Overview

GrumpyDB is an embedded storage engine (library crate) that persists schema-less documents on disk with:
- **Page-based storage** of 8 KiB in `data.db`
- **B+Tree index** in `primary.idx` for O(log n) access by UUID
- **Collection** abstraction encapsulating data pages + primary index
- **Write-Ahead Log** in `wal.log` for durability
- **LRU Buffer Pool** for in-memory caching
- **SWMR** (Single-Writer, Multi-Reader) concurrency

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

## 4. Document Model

### 4.0 Variable-Key B+Tree (VarBTree)

A parallel B+Tree implementation supporting variable-length byte keys (up to 256 bytes), used for secondary indexes.

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
}
```
