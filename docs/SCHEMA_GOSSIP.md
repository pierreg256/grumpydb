# Schema Gossip & Lazy Index Materialization

> **Status:** Design proposal — awaiting sign-off. Not yet implemented.
> **Phase:** v5 phase 44 (see `IMPLEMENTATION_PLAN_V5.md`, to be created on sign-off).
> **Inspiration:** Cassandra schema gossip (versioned schema, eventually
> consistent, lazy materialization on local shard).

## 1. Problem statement

In v5 today, `CREATE INDEX` and `DROP INDEX` are propagated by routing the
DDL through `Coordinator::replica_peer_nodes_for_key()` with a synthetic
routing key `"ddl:create-index:<coll>:<name>:<field>"`. With `N=3` on a
5-node cluster, this materializes the index on **only 3 of the 5 nodes** —
the same 3 that would own writes for that synthetic routing key. The other
2 nodes still receive INSERTs for collection rows that hash to them, but
they cannot index those rows (no `IndexDefinition` locally).

The visible symptom (from `scripts/demo_cluster.sh`):

```
-ERR verified query failed to fetch candidates from peer ...aaa4: index not found: by_name
```

The root cause is a **granularity mismatch**: data is sharded across the
whole ring (every node potentially holds rows of every collection), but
DDL is sharded by the same N-of-cluster_size policy.

## 2. Design goals

1. **Eventual consistency on schema** — every up node converges to the same
   set of `IndexDefinition`s, with bounded staleness measured in seconds.
2. **Synchronous local commit** — `CREATE INDEX` returns `+OK` as soon as
   the coordinator node has persisted the new schema version locally.
3. **No write blocking on schema convergence** — INSERTs proceed even if a
   peer hasn't yet seen the new schema; the missing index entries are
   backfilled lazily on that peer.
4. **No cluster-wide DDL broadcast** — schema changes piggyback on the
   existing gossip probe loop, with a single `schema_version: u64` field
   acting as the "is something new?" trigger.
5. **Resilience to node downtime** — a node that was down at `CREATE INDEX`
   time pulls the schema on rejoin via the same mechanism.
6. **Backwards compatible on disk** — pre-existing data dirs without a
   schema log get one bootstrapped from the implicit on-disk schema
   (existing `idx_*.idx` files) on first start with the new binary.

## 3. Non-goals (for this phase)

- Strong consistency on DDL (no Raft, no quorum-blocking commit).
- DDL transactions spanning multiple statements.
- Schema changes for `CREATE COLLECTION` / `DROP COLLECTION` —
  out of scope for phase 44, will be folded in once the foundation works.
  (CREATE COLLECTION already replicates implicitly through INSERT routing.)
- Per-tenant schema versioning. We use a **single per-cluster monotonic
  version**, same as Cassandra.

## 4. Data model

### 4.1 In-memory `SchemaState`

A single struct held inside `Coordinator` (or a sibling), behind an
`RwLock`:

```rust
pub struct SchemaState {
    /// Monotonically-increasing per-cluster schema version.
    /// Bumped on every applied DDL operation.
    pub version: u64,
    /// Logical schema entries, keyed by full path.
    /// (tenant, database, collection, index_name)
    pub indexes: BTreeMap<IndexKey, IndexEntry>,
}

pub struct IndexKey {
    pub tenant: String,
    pub database: String,
    pub collection: String,
    pub index_name: String,
}

pub struct IndexEntry {
    pub field_path: String,
    /// HLC timestamp of the originating DDL, used for LWW conflict resolution.
    pub created_hlc: u64,
    /// `false` once a DROP INDEX with newer HLC has been applied.
    /// Tombstones are kept to suppress late re-creation via gossip.
    pub tombstone: bool,
    /// HLC of the last operation (create or drop) on this entry.
    pub last_modified_hlc: u64,
}
```

### 4.2 On-disk `schema.log`

A JSONL append-only file at `<data_dir>/_cluster/schema.log` (alongside
the existing `_cluster/node.json` and `_cluster/hints/`), one record per
applied DDL operation:

```json
{"version": 1, "hlc": 17780000000000, "op": {"create_index": {"tenant": "_system", "database": "demo_db", "collection": "docs", "index_name": "by_name", "field_path": "name"}}}
{"version": 2, "hlc": 17780000010000, "op": {"create_index": {"tenant": "_system", "database": "demo_db", "collection": "docs", "index_name": "by_age", "field_path": "age"}}}
{"version": 3, "hlc": 17780000020000, "op": {"drop_index": {"tenant": "_system", "database": "demo_db", "collection": "docs", "index_name": "by_age"}}}
```

The log is the source of truth on startup; the in-memory `SchemaState` is
rebuilt by replaying it. Truncation/compaction is **out of scope** for
phase 44 (a fresh schema log grows by ~200 bytes per CREATE INDEX, so
even thousands of indexes is < 1 MB).

### 4.3 Wire format additions

Extend `GossipPayload` (already exchanged via the handshake protocol on
each gossip probe) with two optional fields:

```rust
pub struct GossipPayload {
    // ...existing fields...
    /// Per-cluster schema version known by the sender.
    #[serde(default)]
    pub schema_version: u64,
    /// Optional inline schema diff: only sent in response to a pull
    /// request when the remote's `schema_version` is lower.
    #[serde(default)]
    pub schema_diff: Option<Vec<SchemaLogEntry>>,
}
```

Both default to zero / empty when absent, so old binaries keep working.

A new explicit RPC `pull_schema_since(version)` is added on the same
peer-RPC framing already used by `fetch_peer_value`, returning all log
entries with `version > since`. Nodes use it when they detect a peer
advertising a higher `schema_version`.

## 5. Operation flow

### 5.1 `CREATE INDEX docs by_name name`

1. Client sends to any node (say `nodeA`).
2. `nodeA` parses + RBAC + resolves `(tenant, db, coll)`.
3. `nodeA.SchemaState.write()`:
   - Allocate next `version = current_version + 1`.
   - Stamp `created_hlc` from the local HLC.
   - Insert `IndexEntry` into `indexes`.
4. `nodeA` appends one record to `schema.log` (`fsync` for durability).
5. `nodeA` calls `Database::create_index(coll, "by_name", "name")` locally
   — this builds the on-disk `idx_by_name.idx` from local rows (existing
   behavior, idempotent). Already happens today.
6. `nodeA` returns `+OK` to the client.
7. `nodeA`'s next gossip probe broadcasts `schema_version = N+1` in
   `GossipPayload`.

### 5.2 Schema gossip propagation (existing probe loop)

Inside `cluster::gossip::probe_one_peer`, after the existing handshake:

```text
if remote.schema_version > local.schema_version:
    diff = pull_schema_since(local.schema_version) over the same connection
    for entry in diff: apply_remote_schema_entry(entry)
elif remote.schema_version < local.schema_version:
    // The peer needs to catch up; it will pull from us on its next probe.
    // No action needed (gossip is symmetric).
```

`apply_remote_schema_entry` is the merge function:

- **CREATE** with new `(tenant, db, coll, name)`: insert.
- **CREATE** with conflicting field_path: LWW by `created_hlc` (we are
  using HLC, so total ordering is well-defined; ties break by node UUID).
- **DROP** with newer HLC: set `tombstone = true`, but **do not run local
  drop yet** — the lazy-materializer handles it.
- Bump local `version` to `max(local.version, remote_entry.version)`.
- Append the entry to local `schema.log`.

### 5.3 Lazy materialization

The `SchemaState` is consulted at **two trigger points**:

#### Trigger A — at `Database::open()` / collection open

After collection load, compare the schema log to materialized indexes:

```rust
for entry in schema_state.indexes_for(tenant, db, coll):
    if !entry.tombstone && !collection.has_index(entry.index_name):
        // schedule rebuild on the background materializer
        materializer.enqueue(MaterializeJob::Build { ... });
    if entry.tombstone && collection.has_index(entry.index_name):
        materializer.enqueue(MaterializeJob::Drop { ... });
```

#### Trigger B — on schema gossip apply

Whenever `apply_remote_schema_entry` adds/marks-tombstone on a key, it
enqueues the same `MaterializeJob` immediately. The collection may be
opened or not — the job handler is idempotent.

#### The materializer worker

A single tokio task per server (spawned at startup, alongside gossip and
hint-replay) that:

- Drains a `mpsc::UnboundedReceiver<MaterializeJob>`.
- For each job:
  - `Build`: open the collection, call `Database::create_index(...)` (the
    existing rebuild path that scans every local row and inserts into the
    new B+Tree). Result: `idx_<name>.idx` materialized.
  - `Drop`: call `Database::drop_index(...)`, removing the on-disk file.
- Logs `tracing::info!(version, coll, name, duration_ms, "schema converged")`
  on success. On error, logs and re-enqueues with backoff (up to a
  configurable max).

### 5.4 INSERT path — no change

The INSERT path is **unchanged**:

```rust
db.insert(coll, uuid, value)
  → collection.insert_doc(...)        // updates secondary_indexes
```

The `secondary_indexes` field already only contains indexes that have
been materialized locally. If the schema says "this collection has
`by_name`" but the materializer hasn't built it yet, the INSERT just
doesn't update `by_name` for that row — and that's **fine**, because the
materializer's `Build` job will scan the entire local shard (including
this newly inserted row) and produce a complete index.

The key insight: `Database::create_index` already does a full
`btree.scan_all()` rebuild ([src/database/mod.rs:450-480](../src/database/mod.rs#L450-L480)
→ [src/collection/mod.rs:370-405](../src/collection/mod.rs#L370-L405)).
We are not introducing new indexing logic; we're just **deferring** when
it runs on each peer.

### 5.5 QUERY path — small change

Today, `QUERY by_name "x"` on a node that doesn't have the index returns
`-ERR index not found: by_name`. With this design, that error now means
**either** "index never existed" **or** "index exists in schema but not
yet materialized here". To distinguish:

#### Local execution (44d)

The local QUERY handler (`Command::Query` / `Command::QueryRange`)
intercepts `IndexNotFound` from the engine and asks
`coordinator.schema_has_index()` whether the cluster schema knows the
index. The error is rewritten when applicable:

- *Schema knows it*: `-ERR index 'by_name' is in cluster schema
  (version 7) but not yet materialized on this node; retry shortly`
- *Schema does not know it*: `-ERR index not found: by_name`

#### Verified-query fan-out (44f)

When `R≥2`, the coordinator fans out candidate fetches to peers via
`fanout_query_peer_candidates_*`. Two layers of refinement apply:

1. **Acceptor side** (peer answering the RPC,
   `handle_peer_data_op_with_coord`): if the index lookup fails with
   `database not found` / `collection not found` / `index not found`
   AND the local `SchemaState` knows the index, the response is
   rewritten as `index_not_yet_materialized:<index_name>`. This is a
   stable, machine-readable prefix.
2. **Coordinator side** (handler, `collect_peer_candidates`): peer
   responses with that prefix cause the peer to be **skipped** rather
   than the QUERY to fail. The peer's `node_id` is collected.

If at least one peer was skipped, the handler sleeps for
`2 × gossip_probe_interval_ms` (capped at 5 s) and re-issues the
fan-out **once**. Most schema diffs converge within one probe period.

If peers remain skipped after the retry, the handler appends a
sentinel entry to the trailing `Response::Array`:

```text
_warning convergence: 2 peer(s) not yet materialized: [nodeB,nodeE]
```

The leading `_` makes the entry trivially distinguishable from a real
`<uuid> {json}` row (UUIDs never start with `_`). Drivers that don't
understand the warning see what looks like a noop bulk string;
schema-aware drivers can surface convergence lag to the application
and trigger a retry.

## 6. Bootstrap / migration

On startup, if `_cluster/schema.log` is **absent** but the data
directory contains existing `idx_*.idx` files:

1. Walk every collection on disk.
2. For each `idx_<name>.idx`, read its `IndexDefinition` (already loaded
   by `Collection::open`).
3. Synthesize one `CreateIndex` log entry per index, all with `version =
   1, 2, 3, ...` and `created_hlc = 0` (so any incoming gossip with a
   real HLC wins).
4. Write the synthesized log.

This makes the upgrade from 4.x to 5.x (this phase) zero-touch.

## 7. Failure modes & invariants

| Scenario | Behavior |
|---|---|
| Node down at CREATE INDEX | On rejoin, gossip detects mismatch, pulls schema diff, materializer builds locally. |
| Two CREATE INDEX with same name, different field, concurrent | Both nodes commit locally (different versions). Gossip merge picks the one with higher HLC; loser's `idx_*.idx` is dropped + rebuilt by materializer. **Operationally rare**; we don't try to be clever. |
| CREATE INDEX then immediately QUERY on a peer that hasn't gossiped yet | QUERY returns "not yet materialized"; client retries. With 1 s gossip interval, convergence is sub-second to a few seconds. |
| schema.log corrupted | Server refuses to start with `IdentityError::Malformed`-style error. Operator action: remove the file + rebuild from on-disk indexes (manual). |
| Cluster split-brain | Both halves keep advancing schema versions independently. On heal, gossip merges by HLC, version becomes `max(left, right)` of advances. (Same eventual-consistency story as Cassandra.) |

## 8. Observability

New Prometheus metrics:

- `grumpydb_schema_version{node_id}` — current local `schema_version`.
- `grumpydb_schema_pulls_total{result}` — counter of pulls (success/fail).
- `grumpydb_schema_materialize_jobs_total{kind, result}` — counter (build/drop, ok/error).
- `grumpydb_schema_materialize_duration_seconds` — histogram per job.

New protocol commands (admin-only):

- `SCHEMA VERSION` → `:N` (the local version).
- `SCHEMA STATUS` → JSON describing which `(tenant, db, coll, name)`
  defined indexes exist locally and which are pending materialization.

## 9. Testing strategy

- **Unit**: `SchemaState::apply` round-trip; LWW conflict resolution;
  bootstrap synthesis from an existing data dir.
- **Integration**:
  - `tests/schema_gossip.rs`: 3-node cluster, CREATE INDEX on node1,
    INSERT some docs, assert that within 5 s, QUERY on node2 and node3
    returns the same candidate set.
  - `tests/schema_recovery.rs`: stop node3, CREATE INDEX on node1, write
    docs, restart node3, assert convergence.
  - `tests/schema_concurrent_create.rs`: two parallel CREATE INDEX with
    same name + different field on different nodes; assert convergence to
    one definition.
- **Manual**: `scripts/demo_cluster.sh` should now show every node in
  the per-node SCAN+QUERY section returning consistent index hits.

## 10. Module touch-list

### New files

- `grumpydb-server/src/cluster/schema/mod.rs` — `SchemaState`, `IndexEntry`, `apply()`, `bootstrap_from_data_dir()`.
- `grumpydb-server/src/cluster/schema/log.rs` — append-only `schema.log` reader/writer.
- `grumpydb-server/src/cluster/schema/materializer.rs` — background tokio task + `MaterializeJob` enum + `mpsc` channel.
- `tests/schema_gossip.rs`, `tests/schema_recovery.rs`, `tests/schema_concurrent_create.rs` — integration tests.

### Modified files

- `grumpydb-server/src/cluster/handshake.rs` — extend `GossipPayload` with `schema_version` + `schema_diff`; add `pull_schema_since` peer RPC.
- `grumpydb-server/src/cluster/gossip.rs` — call schema pull when remote version > local.
- `grumpydb-server/src/coordinator.rs` — hold `Arc<RwLock<SchemaState>>`; expose `schema_version()`, `apply_local_ddl()`, `merge_remote_schema()`.
- `grumpydb-server/src/tcp/handler.rs` — replace `replicate_index_ddl` with a thin call into `coordinator.apply_local_ddl()`. Add `Command::SchemaVersion` and `Command::SchemaStatus` handlers. Update QUERY to distinguish "not materialized yet".
- `grumpydb-server/src/main.rs` — spawn the materializer task at startup; wire bootstrap-from-disk on first run.
- `grumpydb-protocol/src/command.rs` + `parser.rs` — add `Command::SchemaVersion`, `Command::SchemaStatus`.
- `docs/ARCHITECTURE.md` — document the new `_cluster/schema.log`.
- `docs/CLUSTER.md` — schema convergence story.
- `docs/IMPLEMENTATION_PLAN_V5.md` — new file with phase 44 broken into tranches.

### Unchanged (deliberately)

- `src/collection/mod.rs`, `src/database/mod.rs`, `src/index/mod.rs` — no changes to the engine. We're orchestrating *when* to call existing engine APIs, not changing them.

## 11. Roll-out plan (tranches inside phase 44)

| Tranche | Deliverable |
|---|---|
| 44a | `SchemaState` model + `schema.log` persistence + bootstrap-from-disk + unit tests. **Wire into Coordinator but don't yet act on remote.** Existing replication code stays. |
| 44b | Extend `GossipPayload` + `pull_schema_since` RPC + apply-on-pull. Schema converges in gossip but materialization still uses the old path. |
| 44c | Spawn the materializer task; replace `replicate_index_ddl` with `apply_local_ddl + bump version`. **Old code path removed.** |
| 44d | `SCHEMA VERSION` / `SCHEMA STATUS` commands + Prometheus metrics + QUERY "not yet materialized" handling. |
| 44e | Integration tests, documentation, demo script update. |

Each tranche is independently shippable and behind no feature flag (the
new code path is purely additive in 44a–b, and 44c is the cutover).

## 12. What we explicitly choose NOT to do

- **No Raft for schema.** Cassandra-style gossip is enough; total ordering
  is provided by HLC + LWW.
- **No schema log compaction in this phase.** ~200 bytes/op; thousands of
  ops fit in well under 1 MB.
- **No quorum on CREATE INDEX.** The `+OK` returned by `CREATE INDEX`
  means "I've persisted the intent locally"; convergence is observable
  via `SCHEMA STATUS` and metrics.
- **No CREATE COLLECTION refactor.** Today CREATE COLLECTION is implicitly
  replicated when the first INSERT routes there. We keep this for now and
  fold it into the schema log in a future phase.

---

## Sign-off checklist

- [ ] Granularity: per-cluster `schema_version` ✅ (confirmed)
- [ ] CREATE INDEX semantics: synchronous local + async gossip ✅ (confirmed)
- [ ] On-disk format: JSONL `schema.log`, no compaction in this phase
- [ ] Wire format: additive `schema_version` + optional `schema_diff` in `GossipPayload`
- [ ] Bootstrap: synthesize log from existing `idx_*.idx` on first start
- [ ] QUERY error semantics: distinguish "index not in schema" vs "not materialized yet"
- [ ] Tranche split (44a–e) acceptable

If any of these need adjustment, push back here before any code is written.
