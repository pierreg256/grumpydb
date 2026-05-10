# GrumpyDB v5 — Phase 44 (Schema Gossip)

> Phase 44 implements **Cassandra-style schema gossip with lazy index
> materialization**. It replaces the broken DDL fan-out of v5
> (Phases 40–43) where `CREATE INDEX` only reached `N` of the `N+k`
> nodes in the cluster, causing `QUERY` to fail intermittently on
> any node missing the index file.
>
> The full design rationale lives in
> [`SCHEMA_GOSSIP.md`](./SCHEMA_GOSSIP.md). This file is the
> implementation log split into shippable tranches.

## Status

All five tranches (44a–44e) shipped in May 2026; 44f added immediately
after as a follow-up to a sub-bug surfaced by the demo cluster.

| Tranche | Theme | Status |
|---|---|---|
| 44a | `SchemaState` model + `_cluster/schema.log` + bootstrap | ✅ |
| 44b | `GossipPayload.schema_version` + `pull_schema_since` RPC | ✅ |
| 44c | Background materializer + `record_local_index_ddl` | ✅ |
| 44d | `SCHEMA VERSION` / `SCHEMA STATUS` + Prometheus + refined `IndexNotFound` | ✅ |
| 44e | Integration tests + docs + demo script + legacy cleanup | ✅ |
| 44f | QUERY fan-out skip-with-warning + bounded retry on convergence lag | ✅ |

## What changed for users

### Wire protocol additions (44d)

```text
SCHEMA VERSION
→ :7

SCHEMA STATUS
→ ${json: {node_id, schema_version, indexes:[...]}}
```

`Action::Session` for both — no RBAC scope, mirrors `TOPOLOGY` /
`SNAPSHOT_HLC`.

### `QUERY` error refinement (44d)

When a `QUERY` lands on a node where the index file does not exist
yet, the error message now distinguishes between:

- *Schema knows the index, materializer hasn't caught up*:
  `index 'by_name' is in cluster schema (version 7) but not yet
  materialized on this node; retry shortly`
- *Schema does not know the index*:
  `index not found: by_name`

Smart drivers should treat the first as a **transient** error worth
retrying, the second as a **client error**.

## What changed on disk

A new file `<data_dir>/_cluster/schema.log` tracks every applied
schema operation as JSONL. ~200 bytes per CREATE/DROP, no
compaction in 44 (revisit if it becomes an issue).

```json
{"version":1,"hlc":1700000000000,"op":{"kind":"create_index","key":{"tenant":"_system","database":"demo","collection":"docs","index_name":"by_name"},"field_path":"name"}}
{"version":2,"hlc":1700000010000,"op":{"kind":"create_index","key":{...},"field_path":"age"}}
```

The file is non-secret and `chmod 0644`.

## What changed on the wire

`GossipPayload`, `ClusterHello`, `ClusterHelloResponse` all gained
an optional `schema_version: u64` field (`#[serde(default)]` for
backwards compatibility with pre-44b binaries).

`PeerDataOp` gained one new variant:
```rust
PullSchemaSince { since_version: u64 }
```
and `PeerDataOpResponse` gained:
```rust
SchemaDiff { entries: Vec<SchemaLogEntry> }
```

The pull RPC reuses the existing handshake-then-op framing, so no
new ports or sockets are opened.

## Migration from v4.x to v5 (Phase 44)

Zero-touch. On first start with a v5 binary:

1. `bootstrap_from_data_dir(&data_dir)` walks every
   `<tenant>/<database>/<collection>/idx_*.idx` file and synthesizes
   one `SchemaOp::CreateIndex` per file (HLC = 0, version 1..N).
2. The synthesized log is written to `_cluster/schema.log`.
3. The cluster converges through gossip; any node with newer real
   HLCs wins via LWW.

Operators do **not** need to touch the on-disk data.

## Metrics

Four new Prometheus series surfaced via `/metrics`:

| Name | Type | Labels |
|---|---|---|
| `grumpydb_schema_version` | gauge | `node_id` |
| `grumpydb_schema_pulls_total` | counter | `result=applied` |
| `grumpydb_schema_materialize_jobs_total` | counter | `kind=build\|drop`, `result=ok\|error` |
| `grumpydb_schema_materialize_duration_seconds` | histogram | `kind` |

## Tests

- **Unit (lib)**: 200+ tests in `grumpydb-server` covering
  `SchemaState`, `SchemaLog`, bootstrap, materializer, coordinator
  apply.
- **Integration (`grumpydb-server/tests/schema_gossip.rs`)**: 4
  end-to-end tests:
  - `test_create_index_propagates_to_peer_and_materializes`
  - `test_repeated_gossip_rounds_are_idempotent`
  - `test_drop_then_late_create_does_not_resurrect_on_peer`
  - `test_bootstrap_from_existing_idx_files_seeds_schema_state`
- **Manual**: `scripts/demo_cluster.sh` exercises a 5-node Docker
  cluster and validates schema convergence + cross-node QUERY.

## Out of scope (future phases)

- Schema log compaction / GC.
- Schema operations beyond CREATE/DROP INDEX (e.g. ALTER COLLECTION,
  schema validation rules). The log format is forward-compatible:
  `SchemaOp` is a `serde(tag = "kind")` enum.
- Replacing the placeholder `now_unix_millis()` HLC source in
  `record_local_index_ddl` with the engine's real `HlcClock`. The
  current implementation is monotonic per-node, which is enough for
  per-node version allocation — concurrent CREATEs across nodes get
  tie-broken by HLC + (eventually) node-id ordering.

## Tranche 44f — QUERY fan-out skip-with-warning

### Problem

After 44e shipped, `scripts/demo_cluster.sh` still occasionally
returned a hard error of the form

```text
-ERR verified query failed to fetch candidates from peer ...aaa5: index not found: by_name
```

when a `QUERY` with `R≥2` arrived on a coordinator node before every
peer in the preference list had finished materializing the index
locally. The handler treated any peer failure as fatal, even when the
schema gossip clearly knew the index existed cluster-wide.

### Fix

1. **Acceptor side** (`cluster::handshake::handle_peer_data_op_with_coord`):
   when `QueryIndexExact` / `QueryIndexRange` triggers a
   `database not found` / `collection not found` / `index not found`
   reply **and** the local `SchemaState` knows the target index, the
   error message is rewritten as
   `index_not_yet_materialized:<index_name>`. This is a stable
   machine-readable prefix.
2. **Coordinator handler** (`tcp::handler::collect_peer_candidates`):
   peer errors with that prefix are logged at `debug` level and the
   peer's `node_id` is collected into `skipped_peers`; the fan-out
   continues with the remaining peers instead of aborting.
3. **Bounded retry** (Phase 44f extended scope): if at least one peer
   was skipped, the handler sleeps `2 × gossip_probe_interval_ms`
   (capped at 5 s) and re-issues the fan-out **once**. Most schema
   diffs converge within one probe period; the retry keeps results
   complete in the common case while bounding latency.
4. **Result framing**: when one or more peers remain skipped after
   the retry, a sentinel `_warning convergence: N peer(s) not yet
   materialized: [nodeA,nodeB]` entry is appended to the trailing
   `Response::Array`. The leading `_` makes it trivially distinguishable
   from a normal `<uuid> {json}` row (UUIDs never start with `_`).
   Drivers that don't know about it see what looks like a noop bulk
   string; aware drivers can surface convergence lag to the caller
   and trigger an application-level retry.

### Why "skip" rather than "fail" or "block"

- **Fail** (pre-44f): unusable in practice — every `CREATE INDEX`
  followed quickly by a `QUERY` ran a real risk of intermittent
  errors during the gossip convergence window.
- **Block (synchronous quorum on DDL)**: would mean making
  `CREATE INDEX` block until every replica has materialized, killing
  the asynchronous, partition-tolerant model that motivated 44a–c.
- **Skip + warning** (chosen): clients with `R≥2` get a useful
  partial answer plus an explicit signal that the result may be
  incomplete. The bounded retry inside the handler hides the
  short-window common case.

### Module touch-list

- `grumpydb-server/src/coordinator.rs` — new field
  `gossip_probe_interval_ms: u64` (cached from `[cluster]` config) +
  `pub fn gossip_probe_interval_ms()` accessor.
- `grumpydb-server/src/cluster/handshake.rs` — new
  `pub(crate) const INDEX_NOT_YET_MATERIALIZED_PREFIX`;
  `handle_peer_data_op_with_coord` now post-processes index-query
  responses to rewrite convergence-lag errors. Two new unit tests.
- `grumpydb-server/src/tcp/handler.rs` — new `PeerCandidateFanout`
  struct + `collect_peer_candidates` helper + `format_skip_warning`
  helper; both `Command::Query` and `Command::QueryRange` branches
  refactored to use them and to perform the bounded retry. Four new
  unit tests.

### What the demo cluster shows

After 44f, `scripts/demo_cluster.sh` no longer prints the
`verified query failed to fetch candidates ... index not found`
banner. Either:

- the gossip has fully converged before the QUERY is issued (most
  common — sub-second convergence), or
- the handler's bounded retry resolves it transparently, or
- a `_warning convergence: ...` entry is appended to the trailing
  array (visible but non-fatal).

### Out of scope for 44f

- Recomputing the **R-quorum** in light of skipped peers (Option C
  of the design discussion). For now `R` is treated against the
  **liveness** of replicas, not their schema-readiness; a future
  44g/45 may tighten this.
- Driver-side retry helpers / typed warnings in `grumpydb-client` /
  TS driver. Drivers can opportunistically filter `_warning` entries
  today.
- Peer-side caching of "convergence in flight" so we don't post-process
  on every QUERY when a node knows it's catching up. Probably not
  worth the bookkeeping in v5.
