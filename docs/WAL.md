# GrumpyDB — Write-Ahead Log

This document describes the on-disk Write-Ahead Log (WAL) format and the
auxiliary primitives introduced by Phase 40b: Hybrid Logical Clocks (HLC),
vector clocks, and the per-origin "applied set" used for idempotent replay.

The format is **format-locking** for v6/v7 — every byte described here is
the byte v7 will read.

## File layout

A WAL log file is a sequence of fixed-size 8 KiB pages.

### v1 (legacy, read-only after Phase 40b)

```
[record 0][record 1][record 2]...
```

No header. The very first byte is `record_len`, the start of the first
record. Used by all v4.x deployments. v5+ reads v1 transparently and
upgrades to v2 atomically on the first write.

### v2 (current)

```
Page 0  (8 KiB):  WAL header
[8] magic = "GRUMPWAL"  (ASCII)
[2] version = 2          (u16 LE)
[8] reserved (zero in v2)
[N] zero padding to PAGE_SIZE

Page 1.. : sequence of v2 record frames
[record 0][record 1][record 2]...
```

The `WAL_MAGIC` constant lets the writer detect whether a file is v1
(no magic at offset 0, the bytes are the start of a record) or v2 (magic
present). The reserved 8 bytes at offset 10 are for future use; v2 writes
them as zero.

## Record format v2

```
[0..4]    record_len   u32 LE   total length of this frame minus the leading 4 bytes
[4..12]   lsn          u64 LE   log sequence number, monotonic per file
[12..20]  tx_id        u64 LE   transaction identifier
[20]      op_type      u8       Begin=0 / Commit=1 / PageWrite=2 / Checkpoint=3
[21..37]  origin_node  u128 LE  node_id that produced this record (NIL = legacy v1)
[37..45]  hlc          u64 LE   Hybrid Logical Clock at write time
[45..47]  vclock_len   u16 LE   number of vector-clock entries that follow
[47..47+N×24]  vector clock entries: each (u128 node_id LE, u64 counter LE)
... op-specific payload ...
[..-4..end]    checksum    u32 LE   CRC32 over [4..end-4]
```

Op-specific payload (only for `PageWrite`):
```
[0..4]    page_id      u32 LE
[4..8]    data_len     u32 LE
[8..8+data_len]   after image
[8+data_len..8+2*data_len]   before image
```

For `Begin`, `Commit`, `Checkpoint`: no op-specific payload; the frame ends
right after the vector clock with the trailing checksum.

### Record format v1 (read-only)

```
[0..4]    record_len   u32 LE
[4..12]   lsn          u64 LE
[12..20]  tx_id        u64 LE
[20]      op_type      u8
[21..25]  page_id      u32 LE      (zero for non-PageWrite)
[25..29]  data_len     u32 LE      (zero for non-PageWrite)
[29..29+data_len]      after image
[29+data_len..+data_len] before image
[..-4..end]    checksum    u32 LE
```

When the recovery code reads a v1 record, it materialises an in-memory v2
record with synthetic identity:
- `origin_node_id = 0` (the NIL UUID).
- `hlc = Hlc::from_packed(record.lsn)` — preserves total ordering with
  pre-existing v1 LSNs.
- `vector_clock = singleton(NIL_UUID, lsn)`.

This means: a v1 record and any v2 record produced by the same node compare
correctly under HLC ordering after the auto-migration.

## Auto-migration v1 → v2

The first WRITE to an existing v1 file triggers an atomic rewrite:

1. Read all v1 records into memory.
2. Open `<wal>.tmp` for writing. Write the v2 header (page 0). Write each
   record re-encoded in v2 format.
3. `fsync` the tmp file. `rename(tmp, wal)`. `fsync` the parent directory.
4. The writer re-opens the renamed file.

Recovery before the first write still uses the legacy reader; once the
first write completes, the file is unambiguously v2.

The migration is **idempotent**: if the writer crashes mid-rewrite, the
pre-existing v1 file is intact, so recovery on next start re-reads v1 and
the next write retries the migration.

## Hybrid Logical Clock (HLC)

```
struct Hlc(pub u64);
   bits 63..16: physical milliseconds since UNIX epoch (good for ~8900 years)
   bits 15..0:  logical counter (up to 65535 events per ms)
```

Operations:

- **`now()`** — local event. If wall-clock has advanced past the previous
  HLC's physical time, returns `Hlc::pack(wall_now, 0)`. Otherwise reuses
  the previous physical and increments the logical counter. Saturates at
  logical=65535 → `HlcError::LogicalOverflow` (in practice unreachable).
- **`update(remote)`** — receive event. Returns an HLC strictly greater
  than both `last` and `remote`. Refuses if `remote.physical >
  wall_now + max_skew_ms` (default 1 hour).

Properties:

- Total order on a single node.
- Causal order across nodes (when nodes communicate via `update()`).
- Cheap (8 bytes, single mutex bump per call).

## Vector clock

```
struct VectorClock { entries: BTreeMap<u128, u64> }
```

Entries are sorted by `node_id` (u128 LE Uuid bytes) so the on-disk
encoding is deterministic. Encoding:

```
[0..2]      vclock_len   u16 LE
[2..2+N×24] entries: (u128 node_id LE, u64 counter LE) × N
```

Capped at 4096 entries to bound deserialisation cost. In practice clusters
have well under 100 nodes.

Comparison semantics (standard):

| Result | Definition |
|---|---|
| `Equal` | Every entry equal pairwise. |
| `LessThan` | Every entry of self ≤ other, AND at least one strictly less. |
| `GreaterThan` | Symmetric of LessThan. |
| `Concurrent` | Neither dominates: each side has at least one entry the other lacks or has a smaller value for. |

In v5 single-writer regime, every record carries `singleton(node_id, hlc)`,
so vector clocks are always trivially `LessThan` / `GreaterThan` (never
`Concurrent`). Phase 46 (v6) activates real concurrent-detection logic on
this same encoding.

## Idempotent replay (`AppliedSet`)

Persisted at `<data_dir>/_replication/state.json`:

```json
{
  "schema_version": 1,
  "last_applied": {
    "5e3f9c1a-...": 1714368000123456,
    "d69be729-...": 1714368000123478
  }
}
```

Maps each `node_id` (UUID hyphenated string) to the highest `Hlc.0`
already applied from that origin. Recovery consults the set; records with
`(origin, hlc)` already at-or-below the recorded high-water-mark are
skipped (logged at debug level).

In v5 single-writer the set has at most one entry (self) and replay never
hits the duplicate path. Phase 40e turns the set into a hot path.

## Engine integration

`Database::open` accepts `Arc<HlcClock>` and a `node_id`:

```rust
// New, ring-aware:
let clock = Arc::new(HlcClock::new());
let node_id: u128 = /* server identity */;
let mut db = Database::open_with(path, node_id, clock.clone())?;

// Embedded back-compat: auto-creates a fresh node_id at
// <data_dir>/_database/node.json and a fresh HlcClock.
let mut db = Database::open(path)?;
```

Every WAL write site stamps `clock.now()?` and a singleton vector clock.

New (no-op in v5) hooks for v6 replication:

```rust
db.current_hlc()                  // last issued HLC
db.record_remote_hlc(remote_hlc)  // calls clock.update(remote)
```

## See also

- [`docs/CLUSTER.md`](CLUSTER.md) — node identity (the `node_id` stamped
  on every record).
- [`docs/IMPLEMENTATION_PLAN_V4.md`](IMPLEMENTATION_PLAN_V4.md) — Phase
  40b delivery details and Phase 40e replication that consumes this format.
- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — broader engine context.
