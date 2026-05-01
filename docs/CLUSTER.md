# GrumpyDB — Clustering & Inter-Node Protocols

This document describes the clustering primitives introduced in v5 (Phase
40a) and the path from there to v6 (gossip + multi-writer) and v7
(anti-entropy + multi-region).

Status note (v6 Stream E):
- Phase 44 is complete. GrumpyDB now runs background peer probes,
  gossips runtime membership over handshake payloads, and converges peer
  liveness/vnode metadata in `TOPOLOGY` beyond the initial static seed list.
- Phase 45 is complete. Coordinator routing defaults to
  `N = min(3, cluster_size)`, write admission is allowed on any local write
  replica in the key's preference list, and bounded `WRITE_CONCERN W` in
  `[1, N]` is validated (with `R` still pinned to `1`). Keyed writes now fan
  out ack waits to replica peers over the handshake transport and fail when
  quorum cannot be met before `write_ack_timeout_ms`.

## Mental model

A GrumpyDB cluster is a set of nodes that share a common `cluster_id`. Each
node has a stable `node_id` that survives restarts. Nodes find each other
through static configuration in v5 (the `[cluster]` TOML section). In v6
Phase 44, nodes use that static list as bootstrap seeds and converge to a
runtime membership view via periodic gossip-style probes.

The data plane (TCP+TLS, port 6380) is unchanged — clients connect and
issue protocol commands as before. The new **cluster plane** (TCP+TLS,
default port 6390, configurable via `[cluster].listen_peer`) is reserved
for inter-node traffic: handshake, WAL streaming (Phase 40e), and gossip
(Phase 44).

For a ready-to-run v5 demo topology, use:

- `docker-compose.cluster.yml` (3-node demo stack)
- `scripts/smoke_cluster.sh` (health/JWKS/log smoke test; `--keep-up` supported)
- `docker/cluster/node1.toml`, `docker/cluster/node2.toml`, `docker/cluster/node3.toml`
- pre-seeded identities under `docker/cluster/data/node*/_cluster/node.json`

## Node identity

Every node persists its identity at `<data_dir>/_cluster/node.json`:

```json
{
  "node_id": "5e3f9c1a-...",
  "cluster_id": "a1b2c3d4-...",
  "created_at_unix": 1714368000,
  "identity_version": 1
}
```

- **`node_id`**: random UUID v4. Stable for the lifetime of the data
  directory. Used in tracing, metrics, WAL records (Phase 40b), JWT
  audience claims (Phase 39 follow-up), and as the routing target on the
  consistent-hash ring (Phase 40c).
- **`cluster_id`**: random UUID v4. Identical across all nodes of the
  same cluster. The handshake refuses any peer presenting a different
  `cluster_id`.
- **`identity_version`**: schema version for forward compatibility. v5
  writes `1`; the loader rejects unknown future versions to prevent a
  silent downgrade.
- The file is `chmod 0644` — it contains no secret.

### Bootstrap workflows

**First node of a new cluster** (auto-bootstrap on first start):

```bash
grumpydb-server --data ./data --bootstrap-password 'admin-pw'
# On first start, _cluster/node.json is created with a fresh node_id and
# a fresh cluster_id. The cluster_id is logged so an operator can copy
# it into the configs of additional nodes.
```

**Joining an existing cluster** (explicit init before first start):

```bash
grumpydb-server cluster init --data ./data2 --cluster-id <UUID-from-node1>
grumpydb-server --data ./data2 --bootstrap-password 'admin-pw' \
                --bind 0.0.0.0:6380 --http-bind 0.0.0.0:6381
```

`cluster init` refuses to overwrite an existing `_cluster/node.json` — the
operator must explicitly remove it to start over.

## Configuration

```toml
[cluster]
# Required if any peer is configured. MUST match every peer's cluster_id.
cluster_id = "a1b2c3d4-1234-5678-9abc-def012345678"

# Inter-node TCP+TLS port. Empty string disables clustering (single-node).
listen_peer = "0.0.0.0:6390"

# Static peer list. Each entry MUST include the peer's stable node_id
# so spoofing is detectable at the handshake stage.
peers = [
  { node_id = "...", addr = "node-2.internal:6390" },
  { node_id = "...", addr = "node-3.internal:6390" },
]

# Number of virtual nodes per physical node on the consistent-hash ring
# (Phase 40c). 256 = Cassandra default.
vnodes_per_node = 256

# Tombstone GC grace period in seconds (Phase 40d). 10 days default.
gc_grace_seconds = 864000

# Replication lag threshold (Phase 40e). Config is available; end-to-end
# lag-gate wiring to /readyz is still in progress.
max_lag_seconds = 5

# Per-collection writer assignment (still config-driven in v6).
# Gossip convergence updates runtime peer liveness/membership metadata,
# but does not rewrite this static writer map.
# `*` = the database default writer.
writers = [
  { collection = "*", node_id = "..." },
]

# v6 Phase 44: background probe cadence and dead-peer threshold.
gossip_probe_interval_ms = 1000
gossip_peer_dead_after_secs = 5
```

The reserved fields on each `PeerEntry` (`status`, `last_seen_at_unix`,
`vnode_assignments`) are part of the v6 schema. In Phase 44, gossip updates
these fields in memory and converges membership runtime state from handshake
payload exchanges.

## Handshake protocol (v5)

Every TCP+TLS connection on the cluster port begins with a single
JSON-over-length-prefix exchange:

```
initiator → acceptor:
  4-byte BE length + ClusterHello {
    cluster_id:     UUID,
    node_id:        UUID,
    server_version: string,
    capabilities:   [string],
    status:         string?,
    last_seen_at_unix: u64?,
    vnode_assignments: [u32],
    membership: [
      { node_id, addr, status, last_seen_at_unix?, vnode_assignments }
    ],
  }

acceptor → initiator:
  4-byte BE length + ClusterHelloResponse {
    cluster_id:     UUID,
    node_id:        UUID,
    server_version: string,
    accepted:       bool,
    error:          string?,    // "cluster_id_mismatch", "unknown_peer",
                                // or "protocol_error"
  }
```

Handshake rules:

1. **`cluster_id` must match**, otherwise `accepted = false`,
   `error = "cluster_id_mismatch"`, connection closes.
2. **`node_id` must appear in the acceptor's static `peers` list**, unless
  the initiator advertises gossip capability (`gossip-membership-v1`) for
  dynamic membership convergence. Otherwise `accepted = false`,
  `error = "unknown_peer"`.
3. Maximum frame size: 64 KiB. Larger frames are rejected with
   `protocol_error`.
4. After a successful handshake in v5, the acceptor immediately closes
   the connection. **Phase 40e** keeps it open and starts streaming WAL
   records.
5. JSON encoding is intentional for the handshake (small, debuggable,
   schema-evolvable). Phase 40e introduces a binary frame format on the
   *post-handshake* stream where throughput matters.

## Authentication on the cluster plane

The handshake itself does NOT carry authentication tokens. The transport
relies on **TLS mutual auth** (each node carries a certificate signed by
the cluster's internal CA) — though for v5 the server-side TLS code path
is the same as for client-facing port 6380 (auto-generated self-signed
cert; production deployments should provide a CA-signed chain).

Phase 40e (WAL streaming) layers a `cluster_peer` JWT on top: the
initiator sends an RS256-signed token (issued by the local node and
verifiable through the JWKS endpoint of any peer in the cluster) once the
handshake succeeds. The token carries the `Action::ReplicationPeer` role
and is denied access to any user-data operation by `RoleName::ClusterPeer`
in `auth/role.rs`.

## Tracing & observability

Every span and Prometheus metric carries a `node_id` field/label so
multi-node logs cross-reference cleanly:

```json
{"timestamp":"...","level":"INFO","fields":{
  "message":"login success",
  "node_id":"5e3f9c1a-...",
  "tenant":"acme",
  "user":"alice"
}}
```

A static gauge `grumpydb_node_info{node_id, cluster_id, version}` is
registered at startup and set to `1.0`, allowing PromQL joins:

```promql
sum(rate(grumpydb_commands_total[1m])) by (cmd)
  * on (node_id) group_left (cluster_id, version)
    grumpydb_node_info
```

## Forward compatibility

The `identity_version` field, the reserved `PeerEntry` gossip fields, and
the `capabilities` list in `ClusterHello` are the three forward-compat
escape valves. v5/v6/v7 all read the same `node.json` and the same
`[cluster]` config schema; v6 simply adds new behaviour:

- **v6 (gossip)**: peers populate `status` / `last_seen_at_unix` /
  `vnode_assignments` fields in the topology schema. In Phase 44, status
  transitions (`up`, `suspect`, `down`) are driven by periodic probes and
  membership converges by exchanging runtime gossip payloads over handshake.
- **v6 (multi-writer, Phase 45 complete)**: write-path admission accepts local
  writes when the local node is part of the ring preference list
  (`N=min(3, cluster_size)`), static validation accepts `W ∈ [1, N]`
  (`R` remains `1`), keyed writes validate `W` against currently live replicas,
  and keyed write execution performs ack fanout/wait. If quorum is not reached
  before `write_ack_timeout_ms`, the write fails with quorum-wait failure.
- **v7 (multi-region)**: a `region` field is reserved on `PeerEntry`
  (added in this doc when Phase 51 lands) and the ring becomes a
  per-region affair.

## See also

- [`docs/AUTH.md`](AUTH.md) — JWT signing & RBAC, including the
  `cluster_peer` role.
- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — global server architecture.
- [`docs/IMPLEMENTATION_PLAN_V4.md`](IMPLEMENTATION_PLAN_V4.md) —
  Phase 40a delivery details and Phase 40b–f follow-ups.
