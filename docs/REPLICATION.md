# Replication (Phase 40e)

This document describes the current WAL-stream replication design and implementation status in GrumpyDB v5.

## Scope and Status

Phase 40e implements WAL-stream replication primitives with a single-writer regime.

Implemented slices:
- 40e.1: replication wire frames and codec
- 40e.2: WAL tailer
- 40e.3: peer session handshake (cluster identity + peer auth)
- 40e.4: leader and follower streaming tasks
- 40e.5: idempotent apply primitives and high-water helpers
- 40e.6: single-writer assignment and manual election model
- 40e.7: lag tracking primitive (`LagTracker`)
- 40e.8: 3-node in-process integration test with failover flow

Remaining in 40e line:
- 40e.9: documentation sync checkpoint (this pass)

## Module Map

Main implementation crate:
- `grumpydb-replication`

Key modules:
- `grumpydb-replication/src/frame.rs`: replication frame types + binary codec
- `grumpydb-replication/src/session.rs`: initiator/responder handshake and framed peer session
- `grumpydb-replication/src/tailer.rs`: WAL tailing over `wal.log`
- `grumpydb-replication/src/tasks.rs`: `LeaderTask` and `FollowerTask`
- `grumpydb-replication/src/idempotent.rs`: replay-safe apply helpers
- `grumpydb-replication/src/writer_control.rs`: static writer assignment + election primitive
- `grumpydb-replication/src/lag_tracker.rs`: per-peer lag accounting primitive

Server integration points:
- `grumpydb-server/src/tcp/handler.rs`: cluster management command handling (`ElectWriter`)
- `grumpydb-server/src/config.rs`: cluster static config (`peers`, `writers`, `max_lag_seconds`)
- `grumpydb-server/src/http.rs`: `/healthz`, `/readyz`, `/metrics` base endpoints

## Replication Wire Protocol

Frames currently defined:
- `Hello`
- `HelloAck`
- `Subscribe`
- `WalRecord`
- `Ack`
- `Heartbeat`
- `Bye`

Transport contract:
- length-prefixed frame payload
- CRC32 guard on `frame_type || payload`
- strict frame type validation

Follower flow:
1. Open peer session (initiator side).
2. Send `Subscribe { start_node_id, start_hlc }`.
3. Receive `WalRecord`, apply via `ReplicationApplier`.
4. Emit periodic `Ack` based on `AckPolicy`.

Leader flow:
1. Accept peer session (responder side).
2. Read `Subscribe`.
3. Tail WAL from requested watermark.
4. Ship `WalRecord` + periodic `Heartbeat`.

## Single-Writer Model (v5)

Writer assignment model:
- `WriterAssignment` maps `(database, collection)` to `node_id`
- exact collection assignment has priority
- database-level default (`*`) is fallback
- no assignment means no restriction

Election primitive:
- `WriterAssignment::elect(node_id, database, collection)` rewrites assignment
- intended as manual failover in v5

Protocol and handler support:
- protocol command variant `ElectWriter { node_id, database, collection }`
- server handler accepts the command and returns success text

Important v5 limitation:
- election handling in server command path is currently accepted and logged as v5 in-memory behavior, with persistent/distributed coordination deferred to v6

## Lag and Readiness

Implemented primitive:
- `LagTracker` tracks leader HLC and per-peer acknowledged HLC
- provides `peer_lag`, `max_lag`, and lag snapshots

Config fields available:
- `cluster.max_lag_seconds`

Current HTTP readiness behavior:
- `/readyz` reflects process readiness (TCP listener ready state)
- lag-gate wiring to `/readyz` is not yet active end-to-end

## Security Model for Peer Traffic

Peer authentication is based on cluster identity and peer token verification in the replication handshake.

Highlights:
- peer must match cluster identity
- peer token is verified through the authenticator abstraction
- spoofed node identity is rejected

See also:
- `docs/AUTH.md`
- `docs/CLUSTER.md`

## Test Coverage

Core replication tests are in:
- `grumpydb-replication/src/tasks.rs`
- `grumpydb-replication/src/session.rs`
- `grumpydb-replication/src/tailer.rs`
- `grumpydb-replication/src/idempotent.rs`
- `grumpydb-replication/src/writer_control.rs`
- `grumpydb-replication/src/lag_tracker.rs`

Phase 40e.8 integration milestone:
- `test_three_node_replication_with_failover` validates:
  - writer node-1 replicates to node-2 and node-3
  - manual election promotes node-2
  - node-3 replicates from new writer node-2

## Operational Notes (v5)

What you can rely on now:
- WAL stream transport primitives
- replay-safe follower apply helpers
- static writer assignment/election primitive
- in-process integration scenario proving failover flow

What is intentionally deferred:
- automatic writer election/consensus
- fully coordinated persistent failover state across all nodes
- full lag-gated readiness and replication metrics publication pipeline

## Roadmap Linkage

Next distributed phases after 40e:
- 40f: coordinator and tunable consistency protocol
- 41: MVCC read snapshots indexed by HLC
- 42: smart drivers
- 43: v5 release checkpoint

Authoritative phase plan:
- `docs/IMPLEMENTATION_PLAN_V4.md`
