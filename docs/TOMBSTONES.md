# GrumpyDB — Tombstones

This document describes the tombstone format introduced by Phase 40b/40d
of the v5 plan, and the operational model around it. Tombstones make
`delete` safe under multi-node replication: a deleted key cannot
"resurrect" when an out-of-date replica reconnects.

> **Status (v5):** the tombstone format (slot bit, codec tag, value
> variant) is **format-locked**. The semantic wiring — `delete` writes a
> tombstone instead of physically removing the slot — is **scheduled
> for v6 Phase 46**, alongside conflict-resolution (LWW + CRDT) where
> tombstones must compete with concurrent writes via vector clocks.
> v5 single-writer regime cannot generate the resurrection scenario,
> so the existing `delete` path (which physically removes the slot)
> remains correct for v5 deployments.

## Why tombstones

The "resurrection" problem in eventually-consistent replication:

1. Node A has key `K = v` at HLC `t0`.
2. Node A deletes `K` at HLC `t1`. Slot is reclaimed.
3. Node B was offline during steps 1–2. At step 3, B comes back online
   carrying its older copy `K = v` at HLC `t0`.
4. Anti-entropy / read repair / WAL stream sees that A is missing `K`
   and B has it → B "wins" by virtue of having data.
5. **K resurrects** with value `v` despite the explicit delete.

A tombstone is a marker that says "K was explicitly deleted at HLC t1
by node N, with vector clock VC". Replays of `K = v` at `t0` are dropped
because their vector clock is dominated by the tombstone's. The
tombstone itself is GC'd only after a `gc_grace_seconds` window long
enough to guarantee every peer has seen it.

## Format (locked in v5)

### Slot bit (`src/page/slotted.rs`)

The high bit of the per-slot `length` u16 (bit 15) is reserved as
`SLOT_TOMBSTONE_BIT`:

```
struct Slot {
    offset: u16,                  // bytes 0..2 LE
    length_and_flag: u16,         // bytes 2..4 LE
                                  //   bit 15:    SLOT_TOMBSTONE_BIT
                                  //   bits 0..14 SLOT_LENGTH_MASK (effective max 32 KiB - 1)
}
```

Effective slot length is capped at 32 KiB-1 = 32 767 bytes. Pages are
8 KiB so single-page tuples are well under the cap; overflow refs are
under 32 bytes.

API:

```rust
SlottedPage::is_slot_tombstone(slot_index) -> bool
```

### Codec tag (`src/document/codec.rs`)

`TAG_TOMBSTONE = 0x0A`. Encoding:

```
[1] 0x0A
[8] deleted_at_hlc:  u64 LE   (packed Hlc)
[N] vector_clock:    length-prefixed VectorClock encoding (see docs/WAL.md)
```

### Value variant (`src/document/value.rs`)

```rust
pub enum Value {
    // ... existing variants ...
    Tombstone {
        deleted_at_hlc: u64,
        vector_clock: Vec<u8>,    // opaque encoded VectorClock bytes
    },
}
```

The vector clock is stored as opaque bytes in `Value` to keep
`document/value.rs` independent of `wal/`. The `wal/vclock.rs`
`VectorClock::encode_to` / `decode` round-trip via these bytes.

## v5 semantics (today)

`Database::delete(coll, key)`:

1. Look up the slot via the B+Tree.
2. Free overflow pages if any.
3. Physically remove the slot (`SlottedPage::delete`).
4. Remove the B+Tree primary index entry.

Slot bit and codec tag exist on disk format but are NEVER set in v5.
This is intentional: with single-writer-per-collection enforcement,
the resurrection scenario cannot arise, so paying the cost of carrying
tombstones in steady state is unjustified.

## v6 semantics (Phase 46)

When multi-writer + LWW resolution lands, `delete` becomes a
tombstone write:

1. Build `Value::Tombstone { deleted_at_hlc: clock.now(), vector_clock: VC }`.
2. Encode via the codec.
3. Write INTO the slot, setting `SLOT_TOMBSTONE_BIT`.
4. WAL-log as a normal v2 PageWrite (HLC + VC already there).
5. Keep the B+Tree entry. `document_count` includes tombstones.

`get` / `scan` / `find` / `query` filter via `is_slot_tombstone`. A new
admin-only `scan_with_tombstones` returns ALL slots for v6 conflict
resolution + future REPL `--include-tombstones` flag.

GC happens during `compact_with(gc_grace_seconds)`. A tombstone with
`deleted_at_hlc = h` is eligible iff:

```
Hlc::from_packed(h).physical_ms() + gc_grace_seconds * 1000
    < clock.now().physical_ms()
```

AND no peer has been unreachable longer than `gc_grace_seconds`. In v5
the second clause is vacuous (single writer); in v6 it gates the
compactor.

## Operational guidance (v6+)

- **Default `gc_grace_seconds = 864_000`** (10 days) — comfortably longer
  than any realistic peer downtime + repair window.
- Lower it ONLY if you can guarantee shorter operational windows. A
  too-short value risks resurrection bugs the protocol is designed to
  prevent.
- In environments where partition tolerance is weak (e.g. a network
  with frequent week-long splits), raise it. The cost is disk space
  used by tombstones until GC.

## Cross-references

- [`docs/WAL.md`](WAL.md) — WAL v2 format and vector clock encoding.
- [`docs/CLUSTER.md`](CLUSTER.md) — node identity, gossip-reserved
  fields.
- [`docs/IMPLEMENTATION_PLAN_V4.md`](IMPLEMENTATION_PLAN_V4.md) — Phase
  40b/40d delivery scope, Phase 46 v6 follow-up.
