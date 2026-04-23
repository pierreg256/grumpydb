# GrumpyDB v2 — Implementation Plan

## Vision

Transform GrumpyDB from a single key-value store into a **multi-tenant, multi-database document engine** with collections and secondary indexes.

### Target architecture

```
GrumpyServer
  └── Client ("alice")
        └── Database ("myapp")                 ← unit of transaction
              ├── Collection ("users")         ← unit of storage
              │     ├── primary index (UUID → location)
              │     ├── idx_email (email+UUID → ∅)
              │     └── idx_age (age+UUID → ∅)
              ├── Collection ("tasks")
              │     ├── primary index
              │     └── idx_done
              └── WAL (wal.log)                ← one per database
```

### On-disk layout

```
grumpydb_root/
  _clients.db                             ← client catalogue (B+Tree: name → metadata)

  alice/                                   ← client directory
    _databases.db                          ← database catalogue

    myapp/                                 ← database directory
      _meta.db                             ← collection + index catalogue
      wal.log                              ← WAL scoped to this database

      users/                               ← collection directory
        data.db                            ← slotted pages (documents)
        primary.idx                        ← B+Tree: UUID → (PageId, SlotId)
        idx_email.idx                      ← B+Tree: (encoded_email, UUID) → ()
        idx_age.idx                        ← B+Tree: (encoded_age, UUID) → ()

      tasks/
        data.db
        primary.idx
        idx_done.idx

  bob/
    _databases.db
    production/
      ...
```

### Key design decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| CRUD scope | Per-database | No cross-database queries needed |
| WAL scope | Per-database | Enables multi-collection transactions later |
| Buffer pool scope | Per-database (shared across collections) | Better cache utilization, less memory |
| SWMR lock scope | Per-database | Consistent with WAL scope |
| B+Tree key type | Variable-length `&[u8]` | Required for secondary index keys |
| Naming | `[a-z0-9_]{1,64}` validated | Prevents path injection, filesystem safe |
| Catalogue format | B+Tree files (`_clients.db`, `_databases.db`, `_meta.db`) | Transactional, consistent with engine |
| Backward compat | `GrumpyDb::open()` = 1 default client/db/collection | Existing code continues to work |

---

## Phase Overview

```
Phase 9:  Generic B+Tree       ████████████████████  Done    — variable-length keys
Phase 10: Collection            ████████████████████  Done    — extract from engine
Phase 11: Secondary Indexes     ████████████████████  Done    — sortable encoding
Phase 12: Database              ████████████████████  Done    — multi-collection + WAL
Phase 12b: GrumpyShell          ████████████████████  Done    — interactive JS-like REPL
Phase 13: Client & Server       ░░░░░░░░░░░░░░░░░░░░  Pending — multi-tenant
Phase 14: Concurrency v2        ░░░░░░░░░░░░░░░░░░░░  Pending — per-database SWMR
Phase 15: Polish & Migration    ░░░░░░░░░░░░░░░░░░░░  Pending — backward compat, docs
```

---

## Phase 9: Generic B+Tree

### Objective

Generalize the B+Tree to support **variable-length byte keys** instead of fixed 16-byte UUIDs. This is the foundation for both primary indexes (UUID keys) and secondary indexes (composite keys).

### Design

Instead of a generic `KeyFormat` trait (too invasive, would require refactoring all existing code), a **parallel VarBTree implementation** was created alongside the existing fixed-key BTree:

- **`BTree`** (existing, unchanged) = fixed 16-byte UUID keys, used for primary indexes
- **`VarBTree`** (new) = variable-length byte keys up to 256 bytes, used for secondary indexes

Same algorithms (search, insert with split, delete with merge/redistribute) but with length-prefixed keys and fixed-stride serialization (length prefix + padded to max_key_size).

```rust
// Key encoding utilities (src/btree/key.rs)
pub const VAR_KEY_MAX_SIZE: usize = 256;
pub fn encode_var_key(key: &[u8]) -> Vec<u8>;  // 2-byte length prefix + data
pub fn decode_var_key(buf: &[u8]) -> (&[u8], usize);

// VarBTree struct (src/btree/var_tree.rs)
pub struct VarBTree {
    pm: PageManager,
    meta: VarBTreeMeta,
    max_key_size: u16,
}

impl VarBTree {
    pub fn create(path, max_key_size: u16) -> Result<Self>;
    pub fn open(path) -> Result<Self>;
    pub fn search(&mut self, key: &[u8]) -> Result<Option<(u32, u16)>>;
    pub fn insert(&mut self, key: Vec<u8>, page_id: u32, slot_id: u16) -> Result<()>;
    pub fn delete(&mut self, key: &[u8]) -> Result<bool>;
    pub fn len(&self) -> u64;
    pub fn height(&self) -> u32;
}
```

### Tasks

#### 9.1 Key encoding utilities (`src/btree/key.rs`) — NEW FILE

- [x] `VAR_KEY_MAX_SIZE = 256` constant
- [x] `VAR_KEY_LEN_PREFIX = 2` constant
- [x] `encode_var_key(key: &[u8]) → Vec<u8>` (2-byte LE length prefix + data)
- [x] `decode_var_key(buf: &[u8]) → (&[u8], usize)` (returns key slice + bytes consumed)
- [x] `var_key_disk_size(key: &[u8]) → usize`
- [x] Tests: encode/decode round-trip, empty key, max size key, oversized key panic, ordering preservation, disk size

#### 9.2 Variable-key node types (`src/btree/var_node.rs`) — NEW FILE

- [x] `VarInternalNode` with `from_bytes()` / `to_bytes()`, `find_child()`, `insert_entry()`, `remove_entry()`
- [x] `VarLeafNode` with `from_bytes()` / `to_bytes()`, `search()`, `insert()`, `remove()`
- [x] Fixed-stride serialization: length prefix + padded to max_key_size per entry
- [x] `var_internal_max_keys(max_key_size)` / `var_leaf_max_entries(max_key_size)` capacity functions
- [x] `var_internal_min_keys()` / `var_leaf_min_entries()` — 40% occupancy threshold
- [x] Tests: node serialization round-trip, capacity calculations, find_child, insert/search/remove

#### 9.3 Variable-key B+Tree operations (`src/btree/var_ops.rs`) — NEW FILE

- [x] `VarBTree::search(key: &[u8])` — O(log n) descent
- [x] `VarBTree::insert(key: Vec<u8>, page_id: u32, slot_id: u16)` — with leaf/internal split
- [x] `VarBTree::delete(key: &[u8])` — with merge/redistribute
- [x] `find_leaf()` / `find_leaf_with_path()` for descent
- [x] Tests: insert 3,000 keys with splits, delete half, verify integrity

#### 9.4 Variable-key cursor (`src/btree/var_cursor.rs`) — NEW FILE

- [x] `VarCursor` struct with `next_entry()` and leaf-to-leaf traversal
- [x] `VarCursorEntry` / `VarCursorItem` types
- [x] `VarBTree::cursor()` — positioned at first entry
- [x] `VarBTree::cursor_from(start_key)` — positioned at first entry >= start_key
- [x] `VarBTree::range(start, end)` — collect entries in `[start, end)` range
- [x] `VarBTree::scan_all()` — collect all entries via cursor
- [x] Tests: scan_all ordering, range scan, cursor_from positioning

#### 9.5 VarBTree struct (`src/btree/var_tree.rs`) — NEW FILE

- [x] `VarBTree` struct with `PageManager` and `VarBTreeMeta`
- [x] `VarBTree::create(path, max_key_size)` — initialize index file
- [x] `VarBTree::open(path)` — open existing index with persisted max_key_size
- [x] `len()`, `is_empty()`, `height()`, `max_key_size()`, `sync()`
- [x] Metadata persistence: root_page_id, height, num_entries, max_key_size
- [x] Tests: create/open, insert/search, splits, persistence, stress 3,000 keys

#### 9.6 Module integration (`src/btree/mod.rs`) — UPDATED

- [x] Added module declarations for `key`, `var_node`, `var_ops`, `var_tree`, `var_cursor`
- [x] Updated module documentation to describe both BTree variants
- [x] Zero changes to existing BTree code (no regression risk)

### Validation criteria Phase 9

- [x] All 190 existing tests pass unchanged (zero regression — no existing code modified)
- [x] VarKey B+Tree: insert 3,000 variable-length keys, search, delete, scan
- [x] VarKey range scan preserves sort order
- [x] `cargo clippy -- -D warnings` passes (0 warnings)
- [x] UUID B+Tree performance: no regression (existing code completely untouched)
- [x] 220 total tests (205 unit + 12 integration + 3 doctests), 30 new tests for VarBTree

---

## Phase 10: Collection

### Objective

Extract the per-collection storage logic from `engine.rs` into a standalone `Collection` struct. A collection = data pages + primary index + buffer pool.

### Design

```rust
/// A single named collection within a database.
pub(crate) struct Collection {
    name: String,
    data_pool: BufferPool,               // pages for this collection
    primary_index: BTree,                 // UUID → (PageId, SlotId) — uses UuidKeyFormat
    current_data_page: u32,
}

impl Collection {
    fn open(path: &Path, pool_capacity: usize) -> Result<Self>;
    fn insert(&mut self, key: Uuid, encoded: &[u8]) -> Result<(u32, u16)>;
    fn get(&mut self, key: &Uuid) -> Result<Option<Vec<u8>>>;
    fn delete(&mut self, key: &Uuid) -> Result<()>;
    fn scan(&mut self, range: impl RangeBounds<Uuid>) -> Result<Vec<(Uuid, Vec<u8>)>>;
    fn compact(&mut self) -> Result<u64>;
    fn flush(&mut self) -> Result<()>;
    fn document_count(&self) -> u64;
}
```

### Tasks

#### 10.1 Collection struct (`src/collection/mod.rs`) — NEW FILE

- [x] `Collection` struct: name, data_pool, primary_index (BTree), current_data_page
- [x] `Collection::open(path, name, pool_capacity)` → open/create data.db + primary.idx
- [x] `Collection::open_default(path, name)` → open with default pool capacity (256)
- [x] Move `store_inline`, `store_overflow`, `read_tuple` from engine.rs
- [x] Move `find_or_alloc_data_page` from engine.rs
- [x] `PageWriteRecord` struct: page_id, before/after images for WAL logging
- [x] Tests: CRUD lifecycle, overflow, persistence

#### 10.2 Collection CRUD (`src/collection/mod.rs`)

- [x] `insert_raw(key, encoded) → ((page_id, slot_id), Vec<PageWriteRecord>)` — no WAL (caller handles)
- [x] `get_raw(key) → Option<Vec<u8>>` — returns raw encoded bytes
- [x] `delete_raw(key) → Vec<PageWriteRecord>` — returns page images for WAL
- [x] `scan_raw(range) → Vec<(Uuid, Vec<u8>)>` — raw scan
- [x] `compact() → u64` — defrag + rebuild primary index, return doc count
- [x] `flush()` — flush buffer pool + sync index
- [x] `document_count()` — from primary index metadata
- [x] `pool_stats()` — delegate to buffer pool
- [x] `data_page_manager()`, `index_page_manager()` — for WAL recovery access
- [x] Tests: 10 tests covering CRUD, scan, compact, overflow, persistence, duplicate key, pool stats

#### 10.3 Engine refactor (`src/engine.rs`) — REFACTOR

- [x] `GrumpyDb` now wraps a single `Collection` + `WalWriter` (for backward compat)
- [x] All existing CRUD methods delegate to `self.collection.*_raw()` methods
- [x] WAL operations remain in `GrumpyDb` (not in Collection)
- [x] Collection returns `Vec<PageWriteRecord>` for WAL logging
- [x] WAL recovery done on raw PageManagers BEFORE creating Collection (avoids double-borrow)
- [x] File names changed: `index.db` → `primary.idx` (matching Collection naming)
- [x] Tests: all existing engine tests pass unchanged

### Validation criteria Phase 10

- [x] All existing tests pass (engine is a thin wrapper over Collection)
- [x] Collection can be used standalone (without WAL) in tests
- [x] `cargo clippy -- -D warnings` passes
- [x] 230 total tests (215 unit + 12 integration + 3 doctests), 10 new Collection tests

---

## Phase 11: Secondary Indexes

### Objective

Allow creating secondary indexes on document fields for fast lookups by value.

### Design

```rust
/// A secondary index over a document field.
pub(crate) struct SecondaryIndex {
    name: String,
    btree: GenericBTree<VarKeyFormat>,   // composite key: (encoded_value, uuid) → ()
}

/// Encodes a field value for sortable byte comparison.
/// Preserves natural ordering: integers sort numerically, strings lexicographically.
fn encode_sortable_value(value: &Value) -> Vec<u8>;
```

#### Sortable encoding scheme

```
Type tag (1 byte) + encoded value:

  0x00  Null            → (empty)
  0x01  Bool(false)     → 0x00
  0x01  Bool(true)      → 0x01
  0x02  Integer(i64)    → XOR with 0x8000000000000000 (flip sign bit for sort order)
  0x03  Float(f64)      → IEEE 754 sortable encoding
  0x04  String          → UTF-8 bytes (truncated to 128 bytes)
  0x05  Bytes           → raw bytes (truncated to 128 bytes)
  0x06  Array           → not indexable (error)
  0x07  Object          → not indexable (error)
```

Composite key = `encode_sortable_value(field) + uuid_bytes` (max ~145 bytes).

### Tasks

#### 11.1 Sortable encoding (`src/index/encoding.rs`) — NEW FILE

- [x] `encode_sortable_value(value: &Value) → Result<Vec<u8>>`
- [x] Integer encoding: XOR sign bit for correct sort order
- [x] Float encoding: IEEE 754 sortable transformation
- [x] String/Bytes truncation to 128 bytes
- [x] Reject Array/Object with `GrumpyError::NotIndexable`
- [x] `encode_composite_key(value: &Value, uuid: &Uuid) → Vec<u8>`
- [x] `extract_field(value, field_path) → Option<&Value>` (dot-notation)
- [x] Tests: sort order preservation for integers (negative, zero, positive), strings, mixed types, null ordering, float ordering, truncation, cross-type ordering, composite key (13 tests)

#### 11.2 SecondaryIndex struct (`src/index/mod.rs`) — NEW FILE

- [x] `SecondaryIndex` struct: name, btree (VarBTree), path
- [x] `SecondaryIndex::create(dir, def)` → create .idx file
- [x] `SecondaryIndex::open(dir, def)` → open existing .idx file
- [x] `index_document(uuid, value)` → extract field, insert composite key
- [x] `unindex_document(uuid, value)` → extract field, delete composite key
- [x] `lookup(value: &Value) → Vec<Uuid>` — exact match query via prefix range scan
- [x] `range_query(start: &Value, end: &Value) → Vec<Uuid>` — range scan
- [x] `count() → u64` — number of indexed entries
- [x] `rebuild(docs: &[(Uuid, Value)]) → Result<()>` — full rebuild (drop + recreate)
- [x] Tests: create/open, insert + lookup, range query, delete + re-query, rebuild, duplicate values (7 tests)

#### 11.3 IndexDefinition (`src/index/mod.rs`)

- [x] `IndexDefinition` struct: name, field_path (e.g., `"email"` or `"address.city"`)
- [x] `extract_field(value: &Value, field_path: &str) → Option<&Value>` — dot-notation path (in encoding.rs)
- [x] Support nested paths: `"profile.name"` → obj["profile"]["name"]
- [x] Missing field returns None (document not indexed, not an error)

#### 11.4 Collection + SecondaryIndex integration

- [x] `Collection` gets `secondary_indexes: Vec<SecondaryIndex>` and `index_defs: Vec<IndexDefinition>`
- [x] `create_index(name, field_path) → Result<()>` — creates .idx file + full rebuild from existing docs
- [x] `drop_index(name) → Result<()>` — deletes .idx file
- [x] `insert_doc()` → updates all secondary indexes after primary insert
- [x] `delete_doc()` → removes from all secondary indexes before primary delete
- [x] `query_index(index_name, value) → Vec<(Uuid, Value)>` — lookup + fetch docs
- [x] `query_index_range(index_name, start, end) → Vec<(Uuid, Value)>` — range + fetch
- [x] `compact()` → rebuilds secondary indexes too
- [x] `list_indexes()` → returns index definitions

#### 11.5 New error variants

- [x] `GrumpyError::NotIndexable` — Array/Object values cannot be indexed
- [x] `GrumpyError::IndexNotFound(String)` — unknown index name
- [x] `GrumpyError::IndexAlreadyExists(String)` — duplicate index name
- [x] `GrumpyError::CollectionNotFound(String)` — unknown collection name
- [x] `GrumpyError::InvalidName(String)` — invalid name format

### Validation criteria Phase 11

- [x] Create index on 1,000 docs → lookup returns correct results
- [x] Range query on integer field returns sorted results
- [x] Delete doc → index updated, query returns nothing
- [x] Update doc field → old value gone, new value indexed
- [x] Compact rebuilds secondary indexes correctly
- [x] All existing tests pass (regression)
- [x] 20 new tests (13 encoding + 7 index), 268 total tests

---

## Phase 12: Database

### Objective

A `Database` manages multiple collections with a shared WAL. It is the unit of transaction and the scope of CRUD operations.

### Design

```rust
/// A database containing multiple named collections.
pub struct Database {
    path: PathBuf,
    collections: HashMap<String, Collection>,
    wal: WalWriter,                         // one WAL for all collections
    meta: MetaCatalogue,                     // _meta.db: collection + index definitions
    writes_since_checkpoint: u32,
    pool_capacity_per_collection: usize,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self>;
    pub fn create_collection(&mut self, name: &str) -> Result<()>;
    pub fn drop_collection(&mut self, name: &str) -> Result<()>;
    pub fn list_collections(&self) -> Vec<&str>;

    pub fn insert(&mut self, collection: &str, key: Uuid, value: Value) -> Result<()>;
    pub fn get(&mut self, collection: &str, key: &Uuid) -> Result<Option<Value>>;
    pub fn update(&mut self, collection: &str, key: &Uuid, value: Value) -> Result<()>;
    pub fn delete(&mut self, collection: &str, key: &Uuid) -> Result<()>;
    pub fn scan(&mut self, collection: &str, range: impl RangeBounds<Uuid>) -> Result<Vec<(Uuid, Value)>>;

    pub fn create_index(&mut self, collection: &str, name: &str, field_path: &str) -> Result<()>;
    pub fn drop_index(&mut self, collection: &str, name: &str) -> Result<()>;
    pub fn query(&mut self, collection: &str, index: &str, value: &Value) -> Result<Vec<(Uuid, Value)>>;
    pub fn query_range(&mut self, collection: &str, index: &str, start: &Value, end: &Value) -> Result<Vec<(Uuid, Value)>>;

    pub fn flush(&mut self) -> Result<()>;
    pub fn compact(&mut self, collection: &str) -> Result<CompactResult>;
    pub fn document_count(&self, collection: &str) -> Result<u64>;
    pub fn close(self) -> Result<()>;
}
```

### Tasks

#### 12.1 Name validation (`src/naming.rs`) — NEW FILE

- [x] `validate_name(name: &str) → Result<()>` — `[a-z0-9_]{1,64}`
- [x] Used for client, database, collection, and index names
- [x] `GrumpyError::InvalidName(String)` — error variant (added in Phase 11)
- [x] Reserved `_` prefix (exception: `_default`)
- [x] Tests: valid names, empty, too long, special chars, reserved underscore (5 tests)

#### 12.2 MetaCatalogue — SIMPLIFIED

Not implemented as a separate file. Collection discovery is done by scanning subdirectories
for `data.db` files. Index metadata is managed at the Collection level.

- [x] Auto-discovery: scan database directory for collection subdirectories
- [x] Collection presence = subdirectory with `data.db`
- [x] Index definitions stored in-memory per Collection (rebuilt on open)

#### 12.3 Database struct (`src/database/mod.rs`) — NEW FILE

- [x] `Database::open(path)` → create directory, init WAL, auto-discover existing collections
- [x] Collection management: `create_collection()`, `drop_collection()`, `list_collections()`
- [x] Drop collection: remove from HashMap, delete directory recursively
- [x] `collection(name) → &mut Collection` — direct access
- [x] Tests: open creates dir, create/list/drop collections (12 tests)

#### 12.4 Database CRUD (`src/database/mod.rs`)

- [x] `insert(collection, key, value)` — route to collection with WAL logging + secondary indexes
- [x] `get(collection, key)` — route to collection
- [x] `update(collection, key, value)` — unindex old, delete, insert new, reindex
- [x] `delete(collection, key)` — unindex, delete from collection, WAL
- [x] `scan(collection, range)` — route to collection
- [x] Auto-checkpoint every 100 writes
- [x] Tests: CRUD across collections, verify isolation, update/delete, scan, document count

#### 12.5 Database WAL — SIMPLIFIED

- [x] Shared WAL per database (one `wal.log` file)
- [x] WAL records logged per CRUD operation with begin/commit protocol
- [x] Checkpoint + truncate on flush and after compaction
- Note: cross-collection WAL recovery routing not yet implemented (future work)

#### 12.6 Database query + index management

- [x] `create_index(collection, name, field_path)` → validate name + delegate to collection
- [x] `drop_index(collection, name)` → delegate to collection
- [x] `query(collection, index, value)` → delegate to collection `query_index()`
- [x] `query_range(collection, index, start, end)` → delegate to collection `query_index_range()`
- [x] Tests: secondary index creation/query via database API

#### 12.7 Engine backward compatibility (`src/engine.rs`) — PRESERVED

- [x] `GrumpyDb` remains a thin wrapper over single `Collection` + `WalWriter`
- [x] All existing engine methods unchanged (backward compatible)
- [x] `Database` is a separate higher-level API for multi-collection use
- [x] Both `GrumpyDb` and `Database` exported from `lib.rs`
- [x] All existing 230+ tests pass unchanged

### Validation criteria Phase 12

- [x] CRUD in multiple collections within 1 database
- [x] Drop collection removes files and HashMap entry
- [x] Backward compat: all existing tests pass
- [x] No cross-collection data leakage (isolation verified in tests)
- [x] Secondary index management via Database API
- [x] 17 new tests (12 database + 5 naming), 268 total tests

---

## Phase 12b: GrumpyShell — Interactive REPL

### Objective

Build an interactive shell (`examples/grumpysh/`) with JavaScript-like syntax
and JSON documents. This is the primary tool for **exploring, debugging, and
demonstrating** GrumpyDB features interactively.

### Design

#### Syntax overview

```js
// ── Connection ───────────────────────────────────────
grumpy> use mydb                          // open/create database "mydb"
Switched to database "mydb"

// ── Collections ──────────────────────────────────────
grumpy> db.createCollection("users")
Collection "users" created

grumpy> db.collections()
["users"]

grumpy> db.dropCollection("users")
Collection "users" dropped

// ── Insert ───────────────────────────────────────────
grumpy> db.users.insert({ name: "Alice", age: 30, email: "alice@example.com" })
Inserted: a3b4c5d6-...

grumpy> db.users.insert({ name: "Bob", age: 25, tags: ["dev", "rust"] })
Inserted: e7f8a9b0-...

// ── Query ────────────────────────────────────────────
grumpy> db.users.get("a3b4c5d6")          // by short ID prefix
{ "name": "Alice", "age": 30, "email": "alice@example.com" }

grumpy> db.users.find()                   // all documents
[
  { "_id": "a3b4c5d6-...", "name": "Alice", "age": 30 },
  { "_id": "e7f8a9b0-...", "name": "Bob", "age": 25 }
]

grumpy> db.users.find({ age: 30 })        // filter (client-side scan + match)
[
  { "_id": "a3b4c5d6-...", "name": "Alice", "age": 30 }
]

grumpy> db.users.count()
2

// ── Update / Delete ──────────────────────────────────
grumpy> db.users.update("a3b4c5d6", { name: "Alice", age: 31 })
Updated: a3b4c5d6-...

grumpy> db.users.delete("a3b4c5d6")
Deleted: a3b4c5d6-...

// ── Secondary Indexes ────────────────────────────────
grumpy> db.users.createIndex("by_age", "age")
Index "by_age" created on field "age"

grumpy> db.users.query("by_age", 25)      // exact lookup via index
[{ "_id": "e7f8a9b0-...", "name": "Bob", "age": 25 }]

grumpy> db.users.queryRange("by_age", 20, 30)  // range via index
[{ "_id": "e7f8a9b0-...", "name": "Bob", "age": 25 }]

grumpy> db.users.indexes()
["by_age"]

grumpy> db.users.dropIndex("by_age")
Index "by_age" dropped

// ── Maintenance ──────────────────────────────────────
grumpy> db.users.compact()
Compacted: 42 documents preserved

grumpy> db.users.stats()
{ "documents": 42, "pool": { "reads": 120, "writes": 35, "cached": 12, "capacity": 256 } }

grumpy> db.flush()
Flushed

// ── Utilities ────────────────────────────────────────
grumpy> help                              // command reference
grumpy> help insert                       // detailed help for insert
grumpy> clear                             // clear screen
grumpy> exit                              // quit
```

#### Grammar (simplified)

```
statement     := use_stmt | db_stmt | help_stmt | exit_stmt
use_stmt      := "use" IDENTIFIER
db_stmt       := "db" "." db_method | "db" "." IDENTIFIER "." coll_method
db_method     := "createCollection" "(" STRING ")"
               | "dropCollection" "(" STRING ")"
               | "collections" "()"
               | "flush" "()"
coll_method   := "insert" "(" json_object ")"
               | "get" "(" STRING ")"
               | "find" "(" json_object? ")"
               | "count" "()"
               | "update" "(" STRING "," json_object ")"
               | "delete" "(" STRING ")"
               | "createIndex" "(" STRING "," STRING ")"
               | "dropIndex" "(" STRING ")"
               | "query" "(" STRING "," json_value ")"
               | "queryRange" "(" STRING "," json_value "," json_value ")"
               | "indexes" "()"
               | "compact" "()"
               | "stats" "()"
json_object   := // relaxed JSON: unquoted keys, single/double quotes, trailing commas
json_value    := json_object | json_array | STRING | NUMBER | BOOL | NULL
```

### Tasks

#### 12b.1 JSON parser (`examples/grumpysh/json_parser.rs`)

- [x] Parse relaxed JSON (unquoted keys, single quotes, trailing commas)
- [x] Convert to `grumpydb::Value`
- [x] Pretty-print `Value` as JSON with indentation via `to_json_string()`
- [x] Tests: parse objects, arrays, nested, numbers, strings, booleans, null, edge cases (11 tests)

#### 12b.2 Command parser (`examples/grumpysh/parser.rs`)

- [x] Tokenizer: identifiers, strings, numbers, punctuation
- [x] Parse `use`, `db.method()`, `db.collection.method()`, `help`, `exit`, `clear`
- [x] `Command` enum representing all possible operations
- [x] Error messages for invalid syntax

#### 12b.3 REPL engine (`examples/grumpysh/repl.rs`)

- [x] Read-eval-print loop with `rustyline` (line editing, history)
- [x] State: current database path, open `Database` handle
- [x] `use <db>` → open/create database at `<data_dir>/<db>/`
- [x] Execute `Command` → call appropriate `Database` / `Collection` method
- [x] Format results as pretty JSON
- [x] Error handling: display errors, don't crash
- [x] History file: `~/.grumpysh_history`

#### 12b.4 CLI entry point (`examples/grumpysh/main.rs`)

- [x] `cargo run --example grumpysh` → launch REPL
- [x] `cargo run --example grumpysh -- --data ./mydata` → custom data directory
- [x] `cargo run --example grumpysh -- --eval "use test; db.users.count()"` → one-shot execution
- [x] `--help` flag for usage information
- [x] Module docs with usage examples

#### 12b.5 Filter matching (`examples/grumpysh/filter.rs`)

- [x] `matches_filter(doc: &Value, filter: &Value) → bool` — client-side document matching
- [x] Equality match: `{ age: 30 }` → doc.age == 30
- [x] Nested field matching
- [x] Used by `db.collection.find({ ... })` to filter scan results
- [x] Tests: match flat, nested, type mismatch → false, empty filter → true (6 tests)

#### 12b.6 Documentation

- [x] Inline `//!` module docs in all files
- [x] Help text built into the REPL

### Validation criteria Phase 12b

- [x] `cargo run --example grumpysh` launches a working REPL
- [x] Can create collections, insert/query/delete JSON documents
- [x] `find({ field: value })` filtering works
- [x] Secondary index create/query/drop works
- [x] `--eval` mode executes commands non-interactively
- [x] Pretty JSON output
- [x] History persists across sessions via rustyline
- [x] 17 new tests (11 json_parser + 6 filter), 268 total tests

---

## Phase 13: Client & Server

### Objective

Multi-tenant isolation: each client has their own databases.

### Tasks

#### 13.1 Client struct (`src/server/client.rs`) — NEW FILE

- [ ] `Client` struct: name, path, databases catalogue
- [ ] `Client::open(path)` → read `_databases.db`, list available databases
- [ ] `create_database(name) → Result<()>` — create subdirectory + Database
- [ ] `drop_database(name) → Result<()>` — close + delete directory
- [ ] `database(name) → Result<&mut Database>` — open (lazy) or return cached
- [ ] `list_databases() → Vec<String>`
- [ ] Tests: create/open client, create/drop databases, lazy open

#### 13.2 Server struct (`src/server/mod.rs`) — NEW FILE

- [ ] `GrumpyServer` struct: root_path, clients catalogue
- [ ] `GrumpyServer::open(path)` → create root dir, read `_clients.db`
- [ ] `create_client(name) → Result<()>` — create subdirectory
- [ ] `drop_client(name) → Result<()>` — close all databases + delete directory
- [ ] `client(name) → Result<&mut Client>` — open (lazy) or return cached
- [ ] `list_clients() → Vec<String>`
- [ ] Tests: create/open server, create/drop clients, isolation between clients

#### 13.3 Public API (`src/lib.rs`) — UPDATE

- [ ] Export `GrumpyServer`, `Client`, `Database`, `Collection` (public)
- [ ] Export `IndexDefinition`, `CompactResult`
- [ ] Keep existing `GrumpyDb` export for backward compat
- [ ] Tests: doctest with full Server → Client → Database → Collection flow

### Validation criteria Phase 13

- [ ] 2 clients, 2 databases each, 2 collections each — isolated
- [ ] Drop client removes everything
- [ ] Drop database removes everything under it
- [ ] Server survives close + reopen
- [ ] All existing tests pass

---

## Phase 14: Concurrency v2

### Objective

Thread-safe access with SWMR locks **per database** (not global).

### Design

```rust
/// Thread-safe server handle.
pub struct SharedServer {
    inner: Arc<RwLock<GrumpyServer>>,
}

/// Thread-safe database handle (obtained from SharedServer).
/// Each database has its own RwLock for independent concurrency.
pub struct SharedDatabase {
    inner: Arc<RwLock<Database>>,
}
```

Multiple threads can write to **different databases** concurrently.
Within one database, SWMR rules apply (1 writer OR N readers).

### Tasks

#### 14.1 SharedDatabase (`src/concurrency/mod.rs`) — REFACTOR

- [ ] `SharedDatabase` wrapping `Arc<RwLock<Database>>`
- [ ] All Database methods wrapped with appropriate locks
- [ ] `Clone` for thread sharing
- [ ] Tests: concurrent reads on same database, concurrent writes on different databases

#### 14.2 SharedServer (`src/concurrency/mod.rs`)

- [ ] `SharedServer` wrapping `Arc<RwLock<GrumpyServer>>`
- [ ] `client(name)` → returns client (short lock)
- [ ] `database(client, db)` → returns `SharedDatabase`
- [ ] Tests: 4 threads writing to 4 different databases concurrently, no contention

#### 14.3 Backward compat (`src/concurrency/lock_manager.rs`)

- [ ] `SharedDb` now wraps a `SharedDatabase` with default collection
- [ ] All existing `SharedDb` tests pass
- [ ] Tests: SharedDb regression

### Validation criteria Phase 14

- [ ] 8 threads × 4 databases — concurrent writes, no deadlocks
- [ ] 1 writer + 4 readers per database — no corruption
- [ ] Cross-database operations are independent (no lock contention)
- [ ] Backward compat: existing SharedDb tests pass

---

## Phase 15: Polish & Migration

### Objective

Documentation, migration tools, demo app update, CI.

### Tasks

#### 15.1 Migration tool

- [ ] `GrumpyDb::migrate_to_v2(old_path, server, client, database, collection)` — one-shot migration
- [ ] Reads all docs from v1 format, inserts into v2 collection
- [ ] Tests: migrate 1,000 docs, verify integrity

#### 15.2 Demo app update (TaskMan v5)

- [ ] TaskMan uses `GrumpyServer` → `Client` → `Database` → `Collection`
- [ ] Add `--client` and `--database` CLI flags
- [ ] Create secondary index on `done` field for fast filtering
- [ ] `taskman search --done` uses index instead of full scan
- [ ] Update TUTORIAL.md with v2 API
- [ ] Update COOKBOOK.md with collection/index recipes

#### 15.3 Documentation

- [ ] Update `docs/ARCHITECTURE.md` with v2 architecture
- [ ] Update `README.md` with v2 features
- [ ] `cargo doc` with 0 warnings
- [ ] Update `CLAUDE.md` with new modules

#### 15.4 CI & testing

- [ ] `cargo fmt --check && cargo clippy -- -D warnings && cargo test`
- [ ] Stress test: 3 clients × 3 databases × 3 collections × 1,000 docs
- [ ] All tests pass, 0 warnings

### Validation criteria Phase 15

- [ ] Migration from v1 to v2 works
- [ ] Demo app uses full v2 API
- [ ] All documentation up to date
- [ ] 300+ tests, 0 warnings

---

## Module dependency graph (v2)

```
error (no deps)
  → page (error)
    → document (error, page)
      → btree/key (error)                        ← NEW: KeyFormat trait
        → btree (error, page, btree/key)          ← REFACTORED: generic
          → wal (error, page)
            → buffer (error, page)
              → index/encoding (error, document)  ← NEW: sortable encoding
                → index (error, btree, document)  ← NEW: SecondaryIndex
                  → collection (error, page, document, btree, buffer, index)  ← NEW
                    → database (error, collection, wal, naming)               ← NEW
                      → server/client (error, database, naming)               ← NEW
                        → server (error, server/client, naming)               ← NEW
                          → concurrency (server, database)                    ← REFACTORED
                            → engine (database)                               ← REFACTORED (compat)
                              → lib.rs (exports all)

examples/grumpysh/                                                             ← NEW: REPL
  main.rs → repl.rs → parser.rs + json_parser.rs + filter.rs
  depends on: grumpydb (Database, Collection, Value)
  extra dep: rustyline (line editing + history)
```

---

## Versioning plan

| Phase | Version | Milestone |
|-------|---------|-----------|
| 9 | 1.1.0 | Generic B+Tree |
| 10 | 1.2.0 | Collection extracted |
| 11 | 1.3.0 | Secondary indexes |
| 12 | 2.0.0 | Database (multi-collection) — **breaking** || 12b | 2.0.1 | GrumpyShell REPL || 13 | 2.1.0 | Client & Server |
| 14 | 2.2.0 | Concurrency v2 |
| 15 | 2.3.0 | Polish & Migration |

Phase 12 is the first **breaking change** (new API structure) → major version bump.
Phases 9–11 are backward compatible → minor versions.

---

## Risk assessment

| Risk | Impact | Mitigation |
|------|--------|------------|
| Generic B+Tree breaks existing tests | High | Type aliases preserve exact same types |
| Variable-length keys reduce fan-out | Medium | VarKeyFormat capped at 256 bytes → acceptable fan-out |
| WAL encoding for multi-collection | Medium | Use collection index in high bits of page_id |
| File handle exhaustion (many collections) | Low | Lazy open, close unused databases |
| Index consistency on crash | High | Secondary index updates inside WAL transaction |
| Sort encoding edge cases (NaN, empty string) | Medium | Comprehensive test suite for encoding |

---

## Estimated test counts

| Phase | New tests | Total |
|-------|-----------|-------|
| 9 | ~25 | ~215 |
| 10 | ~15 | ~230 |
| 11 | ~30 | ~260 |
| 12 | ~25 | ~285 |
| 12b | ~10 | ~295 |
| 13 | ~15 | ~310 |
| 14 | ~15 | ~325 |
| 15 | ~15 | ~340 |
