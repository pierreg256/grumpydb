# Consistent Hash Ring with Virtual Nodes (Phase 40c)

> Crate: `grumpydb-ring` (workspace member, `publish = false` in v5).

This document describes the data structure that determines **which physical
node owns a given key** in the GrumpyDB cluster. In v5 the cluster is a
single node, so the ring is degenerate (one node, 256 vnodes, all keys
land locally). In v6 the same data structure powers actual N-node
distribution without changing any call site.

---

## 1. Why a hash ring?

Naïve hash partitioning (`node = hash(key) % N`) has a fatal flaw:
adding or removing one node remaps **most** of the keys. With
consistent hashing only a `1/N` slice of the keyspace moves when
membership changes — a property we need for live cluster scaling.

We picked the Cassandra/Dynamo dialect:

- 64-bit hash space (`[0, u64::MAX]`).
- **Virtual nodes** (vnodes): each physical node owns ~256 random
  positions, not just one. This smooths the per-node load and shrinks
  the slice of keys that need to move when a node joins or leaves.

---

## 2. Hash function

```text
hash(key) = murmur3_x64_128(canonical_bytes(key)).low_64_bits()
```

`canonical_bytes` is built as:

```text
database || 0x00 || collection || 0x00 || key_bytes
```

The `0x00` separators disambiguate `("ab", "cd", k)` from
`("a", "bcd", k)` — note that database and collection names are
restricted to `[a-z0-9_]` upstream so a literal NUL byte cannot occur.

We chose Murmur3 because:

- **Fast** (sub-µs per key in the bench, see §6).
- **Well-distributed**: the `test_distribution_uniformity_chi_squared`
  test holds 10 nodes within ±12% of the mean over 1M random keys.
- **Stable across Rust versions and OS**: it's a pure arithmetic
  function over bytes; the `murmur3` crate (`0.5.x`) is feature-frozen.
- **Non-cryptographic**: that's fine — we're partitioning, not
  signing. Spending CPU on SHA-2 would buy nothing.

We keep the **low 64 bits** of the 128-bit Murmur3 output. 64 bits
gives 1 collision per ~4B keys — three orders of magnitude better than
the entropy of the typical key set, and the ring's `Vec<(u64, NodeId)>`
backing storage stays compact.

---

## 3. Vnode placement

For each physical node `n` and `i` in `0..vnodes_per_node`:

```text
position = murmur3_hash("{n}#{i}".as_bytes())
```

The combined `(position, owner)` list is kept sorted by `position` in
the `Ring`. With 256 vnodes per node and 10 nodes, that's 2560 entries
— tiny.

### Why 256?

- **Cassandra default.** Years of production workloads in the wild.
- **Smoothness.** Even with skewed key distributions, no single node
  ends up with > +30% of mean load (we test this).
- **Memory.** 256 vnodes/node × 10 nodes × 24 B per `(u64, NodeIdOpaque)`
  ≈ 60 KB. Negligible.
- **Lookup cost.** Binary-searching 256k entries (1000 nodes) is still
  ~18 comparisons. The bench shows < 200 ns even at 50 nodes.

This is **configurable** via `RingConfig::vnodes_per_node` so v6 ops can
tune per cluster.

---

## 4. `preference_list(key, n)` — the routing algorithm

```text
1. position = murmur3_hash(canonical_bytes(key))
2. start = first vnode whose position >= position (binary search)
3. walk clockwise, collecting distinct physical owners until we have N
4. return that list
```

**Invariants:**

- The list is **distinct** by physical node id, not by vnode (replicas
  must land on different machines).
- The list size is **clamped** to `min(n, num_nodes)` — empty ring
  returns empty, single-node ring returns `[that_node]` regardless of
  `n`. v6 strict mode (`RingError::NotEnoughNodes`) is reserved for
  the coordinator's startup sanity check.
- The walk wraps around the ring exactly once in the worst case, so
  the algorithm is `O(vnodes)` in the absolute worst case but
  `O(num_nodes)` in practice (we exit as soon as we've seen N distinct
  physical owners).

The first node in the preference list is the **primary** owner of the
key. The remaining `N-1` are the replicas (used by Phase 40e
WAL-stream replication and Phase 40f coordinator quorum).

---

## 5. Rebalancing — `add_node` / `remove_node`

Both functions return `Vec<KeyRange>` describing what moved:

```rust
pub struct KeyRange {
    pub start_inclusive: u64,
    pub end_exclusive: u64,
    pub from: Option<NodeIdOpaque>,  // None when previously unowned
    pub to: NodeIdOpaque,
}
```

This is the **shape** of the data Phase 49 (v6 ring rebalancing) will
hand to the replication layer:

> "Stream the keys whose hash falls in `[start, end)` from `from` to
> `to`. Then, once the receiver acks, the new ring becomes
> authoritative."

Adjacent same-`(from, to)` ranges are merged in the output, so adding a
node typically emits a few dozen ranges, not 256.

In v5 the ring is built once at server boot from the static `[cluster]
peers` list and never mutated. The `KeyRange` machinery is plumbed and
tested but the consumer (the replication peer) won't be wired until v6.

`NodeIdOpaque` is a deliberately type-erased wrapper around the node's
`Display` string. It lets `KeyRange` stay a non-generic public type
without forcing every consumer to spell out the full
`Ring<NodeId>::KeyRange`. Callers that need a strongly-typed
`NodeId` back can parse the inner string themselves
(e.g. `Uuid::parse_str(&kr.to.0)`).

---

## 6. Performance

Bench: `cargo bench -p grumpydb-ring`.

| Ring size | `n` | `preference_list` |
|-----------|-----|-------------------|
| 3 nodes   | 1   | ~107 ns           |
| 3 nodes   | 3   | ~170 ns           |
| 10 nodes  | 1   | ~106 ns           |
| 10 nodes  | 3   | ~166 ns           |
| 50 nodes  | 1   | ~112 ns           |
| 50 nodes  | 3   | ~175 ns           |

Target was **< 1 µs** — we're an order of magnitude under, and the
cost is essentially the Murmur3 hash plus a binary search. The
clockwise walk to find N distinct owners is the only part that scales
with cluster size, and even at 50 nodes it adds < 70 ns.

Distribution example: `cargo run --release -p grumpydb-ring --example
distribution` over 10 nodes × 1M random keys yields per-node loads
within ±12% of the mean.

---

## 7. Future work (out of v5 scope)

- **Phase 49 — Ring rebalancing** consumes the `KeyRange` deltas to
  drive the actual key transfer between nodes when membership changes.
- **Token-aware routing in the smart drivers** (Phase 42, partial in
  v5; full in v6 once the gossip protocol exposes the live ring to
  clients).
- **Heterogeneous nodes**: today every node gets the same number of
  vnodes. v7 may let an op declare a node's "weight" so a beefier box
  takes a proportionally larger slice. The `vnodes_per_node`
  configuration is already per-node-aware in spirit (it's a `u32`
  field on `RingConfig` we could promote to per-node) but the API
  hasn't been generalised yet.

---

## 8. Tests

The crate has **23 unit tests + 1 doc test** covering:

- API stability (empty/single-node/idempotent add/remove).
- Distribution uniformity (1M-key chi-squared style check).
- Membership change deltas (~1/N of keys move on `add_node`,
  ~1/N of keys move on `remove_node`).
- Full ring coverage (`KeyRange`s collectively cover `[0, u64::MAX]`).
- Determinism (build twice → same vnode positions; insertion order
  doesn't matter).
- Property tests (`proptest`) on `preference_list` size, distinctness,
  and `owns` consistency.

See `grumpydb-ring/src/{hash,ring}.rs` `#[cfg(test)] mod tests` blocks.
