# GrumpyDB v5 — Hardening, Observability & Distribution Plan

## Vision

Transform GrumpyDB from a *credible single-node engine* (v4.1) into a **production-grade, observable, distributable database** suitable for being embedded as a building block of a larger distributed system.

This plan is split into four streams executed roughly in parallel but committed in priority order:

1. **Stream H — Hardening** (P0): zero `unwrap` in the engine, hardened auth bootstrap, CI, clippy clean.
2. **Stream O — Observability** (P1): tracing, metrics, integration tests, benchmarks, fuzzing.
3. **Stream A — Architecture** (P2): unify B+Tree, kill `GrumpyDb`, rate-limiting, Docker, snapshot/backup tooling.
4. **Stream D — Distribution** (P3, **mandatory** for the downstream distributed project): RS256 JWT with JWKS, WAL-shipping replication, MVCC foundations.

### Target architecture (end of v5)

```
                    ┌────────────────────────────────────────┐
                    │  Control Plane                         │
                    │  - JWKS endpoint (RS256 public keys)   │
                    │  - /metrics (Prometheus)               │
                    │  - /healthz, /readyz                   │
                    └────────────────────────────────────────┘
                                   │
   ┌───────────────────────────────┼───────────────────────────────┐
   ▼                               ▼                               ▼
┌──────────────┐              ┌──────────────┐              ┌──────────────┐
│  Primary     │  WAL ship    │  Replica 1   │   WAL ship   │  Replica 2   │
│  (read+write)│ ────────────►│ (read-only)  │ ────────────►│ (read-only)  │
│              │              │              │              │              │
│  RS256 sign  │              │  RS256 verify│              │  RS256 verify│
└──────────────┘              └──────────────┘              └──────────────┘
   ▲                               ▲                               ▲
   │            ┌──────────────────┴──────────────────┐            │
   │            │  Clients (with token from any node) │            │
   │            └─────────────────────────────────────┘            │
   │                                                               │
   └─── Snapshot/Backup ──► S3 / disk ◄── Snapshot/Backup ─────────┘
```

### Key design decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| JWT algorithm | **RS256** (asymmetric) | Required for distributed verification across replicas |
| Key publishing | **JWKS endpoint** (HTTP) | Standard, reused by clients and replicas |
| Replication | **WAL shipping** (async, primary→replicas) | The WAL already exists — minimal new design |
| Replication topology v1 | **Single primary, N async replicas** | Simple, correct, covers the immediate use case |
| Replica lag bound | **Bounded by config** (max 5 s default) | Failover decisions deferred to v6 |
| Snapshot format | **tar.gz of {checkpoint LSN, data files}** | Simple, restorable, S3-friendly |
| Observability | **`tracing` JSON + Prometheus** | Industry standard |
| MVCC | **Read snapshots, write serialized** (foundation only) | Full MVCC in v6; v5 unblocks reader/writer concurrency |

---

## Phase Overview

```
Phase 24: CI / Clippy / Hygiene             ████████████████████  P0 ✅ Done
Phase 25: Eliminate unwrap() in engine      ████████████████████  P0 ✅ Done
Phase 26: Auth bootstrap & secret hardening ████████████████████  P0 ✅ Done
Phase 27: tracing instrumentation           ██████░░░░░░░░░░░░░░  P1
Phase 28: Integration tests (TCP E2E)       ████████░░░░░░░░░░░░  P1
Phase 29: Crash recovery integration tests  ██████░░░░░░░░░░░░░░  P1
Phase 30: Criterion benchmarks              ████████░░░░░░░░░░░░  P1
Phase 31: Fuzz protocol & json parsers      ████░░░░░░░░░░░░░░░░  P1
Phase 32: Workspace version alignment       ██░░░░░░░░░░░░░░░░░░  P1
Phase 33: Unify B+Tree (generic over Key)   ████████████░░░░░░░░  P2
Phase 34: Retire GrumpyDb wrapper           ████░░░░░░░░░░░░░░░░  P2
Phase 35: Rate limiting & connection caps   ██████░░░░░░░░░░░░░░  P2
Phase 36: Health, readiness, metrics HTTP   ██████░░░░░░░░░░░░░░  P2
Phase 37: Docker + docker-compose           ████░░░░░░░░░░░░░░░░  P2
Phase 38: Snapshot & restore tooling        ████████░░░░░░░░░░░░  P2
Phase 39: RS256 JWT + JWKS                  ████████████░░░░░░░░  P3 ★
Phase 40: WAL shipping replication          ████████████████████  P3 ★
Phase 41: MVCC foundations (read snapshots) ████████████████░░░░  P3 ★
Phase 42: TypeScript driver hardening       ██████░░░░░░░░░░░░░░  P3
Phase 43: v5.0.0 release                    ██░░░░░░░░░░░░░░░░░░  P3
```

★ = required by the downstream distributed project.

---

# Stream H — Hardening (P0)

## Phase 24: CI / Clippy / Hygiene

**Status: ✅ Done**

### Goal
Zero clippy warnings on `--workspace --all-targets`, every push validated by GitHub Actions.

### Delivered
1. **`.github/workflows/ci.yml`** — jobs `fmt`, `clippy`, `test`
   (matrix: stable + 1.85 MSRV), `docs`, `audit`.
2. **Fixed clippy issues**:
   - `grumpy-repl/src/json_parser.rs` — replaced PI approximation literal in a test.
   - `grumpydb-protocol/src/lib.rs` — converted constant assertions to
     `const { assert!(...) }` blocks.
   - `examples/taskman/store.rs` — fixed `drop with reference` warning by
     introducing a scope block.
3. **README badges**: CI status, crates.io version, docs.rs, MIT license.

### Deferred (tracked for a follow-up)
- `.github/workflows/release.yml` (manual dispatch publish workflow).
- `.cargo/audit.toml` for false-positive ignores (added on demand).

### Acceptance — met
- `cargo clippy --workspace --all-targets -- -D warnings` exits 0.
- `cargo fmt --all -- --check` exits 0.

---

## Phase 25: Eliminate `unwrap()` in the Engine

**Status: ✅ Done**

### Goal
Zero `unwrap()` / `panic!()` in `src/` (the engine). The server may panic on
truly impossible states but must surface them as errors to clients.

### Delivered
1. **New `GrumpyError` variants** in `src/error.rs`:
   - `Corruption(String)` — replaces every "shouldn't happen" `unwrap`.
   - `InvalidPageOffset { page: u32, offset: u16 }`.
   - `InvalidVarKey(String)`.
2. **Refactored 73 production `.unwrap()` sites** across `src/` to either
   explicit byte-array literals or `?` propagation with `Corruption` errors.
   Doc-comment examples and `#[cfg(test)]` modules were left intact.
3. **Lint enforcement** at the top of `src/lib.rs`:
   ```rust
   #![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::panic, clippy::expect_used))]
   ```
4. **Server-side panic isolation** in `grumpydb-server/src/tcp/handler.rs`:
   each `execute_command` call is wrapped in
   `AssertUnwindSafe(...).catch_unwind().await`. Panics are caught, logged via
   `tracing::error!`, and surfaced to the client as
   `Response::Error("internal error (corruption): …")` instead of tearing
   down the whole server. Added `futures = "0.3"` dependency for
   `FutureExt::catch_unwind`.

### Deferred (tracked for a follow-up)
- Dedicated `tests/corruption_test.rs` injecting a malformed page on disk and
  asserting the server stays up. The catch_unwind wrapper is in place but a
  malformed-page integration test is still pending.

### Acceptance — met
- 0 production `.unwrap()` in `src/` (verified with an awk script that
  strips test modules and doc-comments).
- `cargo clippy --workspace --all-targets -- -D warnings` clean with the new lint.

---

## Phase 26: Auth Bootstrap & Secret Hardening

**Status: ✅ Done**

### Goal
Eliminate the `admin/admin` footgun. Protect `secret.key` at rest.

### Delivered
1. **Bootstrap policy** (`grumpydb-server/src/auth/store.rs`):
   - `AuthStore::open` now takes a 4th argument
     `bootstrap_password: Option<&str>`.
   - If no users exist on disk and `bootstrap_password` is `None`, the call
     returns `Err(AuthError::BootstrapRefused(...))` with a clear message.
   - The legacy silent `_system/admin/admin` bootstrap is gone.
   - `--bootstrap-password <pw>` (CLI) or `GRUMPYDB_BOOTSTRAP_PASSWORD`
     (env) creates `_system/admin` with the provided password. Passwords
     shorter than 8 characters emit a warning.
2. **Secret file permissions**:
   - On Unix, `secret.key` is created with mode `0600` at write time.
   - On startup, group/world bits on an existing `secret.key` are detected and
     the file is re-tightened with a warning logged.
3. **JWT generation**: replaced two
   `SystemTime::now().duration_since(UNIX_EPOCH).unwrap()` sites
   (in `auth/jwt.rs` and `auth/store.rs`) with `?`-propagated
   `AuthError::ClockError`.
4. **New `AuthError` variants**: `ClockError(String)`, `ReadOnly`,
   `PasswordChangeRequired`, `BootstrapRefused(String)`.
5. **New tests**: `test_store_refuses_silent_bootstrap`,
   `test_store_no_rebootstrap_after_users_exist`,
   `test_secret_key_has_owner_only_permissions` (Unix-only).

### Deferred (tracked for a follow-up)
- Forced password change on first login for the bootstrap user
  (`MUSTCHANGEPASSWORD` response code, client driver handling). The
  `AuthError::PasswordChangeRequired` variant is in place but the wire-protocol
  flow is not yet implemented.
- `--strict-perms` flag to *refuse* (rather than warn + tighten) on overly
  permissive `secret.key`.

### Acceptance — met
- Server refuses to start with a clean data directory unless
  `--bootstrap-password` (or `GRUMPYDB_BOOTSTRAP_PASSWORD`) is provided.
- After bootstrap, `ls -l secret.key` shows `-rw-------`.
- 468 tests pass, including the 3 new bootstrap/permission tests.

---

# Stream O — Observability (P1)

## Phase 27: `tracing` Instrumentation

### Goal
Every connection, command, and error produces a structured log event. Spans cover the full lifecycle.

### Deliverables
1. **Subscriber setup** in `grumpydb-server/src/main.rs`:
   - JSON output by default (`tracing-subscriber` with `json()` + `with_current_span(true)`).
   - `RUST_LOG` honored, default `grumpydb=info,grumpydb_server=info,tokio=warn`.
   - `--log-format text` for dev.
2. **Span hierarchy**:
   - `connection` span (per TCP/TLS accept) with peer addr.
   - `command` span (per request) with command name + tx_id.
   - `engine` events (page read/write, WAL record, btree split) at `debug` / `trace`.
3. **Auth events** at `info`: login success/failure, token refresh, user creation, role change.
4. **Error events** at `warn`/`error`: every `GrumpyError` returned to a client.
5. **Trace ID propagation** in protocol responses (optional `X-Trace-Id` field) so external observability stacks can correlate.

### Acceptance
- `RUST_LOG=info ./target/debug/grumpydb-server | jq` produces valid JSON, one event per request.
- Manual review: logging in / executing 5 commands / quitting yields a coherent trace.

---

## Phase 28: TCP End-to-End Integration Tests

### Goal
Cover the full client → server → engine → response loop without mocks.

### Deliverables
1. **`tests/server_e2e.rs`** (workspace-level integration test):
   - Spawn `grumpydb-server` on `127.0.0.1:0`.
   - Use `grumpydb-client` to: bootstrap, login, create database, create collection, CRUD, query, index, refresh token.
2. **`tests/server_concurrency.rs`**: 50 concurrent clients, each running 100 ops. Verify no errors, correct counts.
3. **`tests/server_auth.rs`**: forbidden actions return correct error codes; expired tokens rejected; tampered tokens rejected.
4. **Test harness** in a new internal crate `grumpydb-testing` (path-only, never published) with helpers for `spawn_server()`, `bootstrap_admin()`, `temp_data_dir()`.

### Acceptance
- `cargo test --test server_e2e` passes locally and in CI.
- Total wall time under 60 s.

---

## Phase 29: Crash Recovery Integration Tests

### Goal
Prove the WAL promise: kill the process at any point, restart, verify integrity.

### Deliverables
1. **`tests/crash_recovery.rs`** with scenarios:
   - Kill mid-insert (between WAL write and page flush).
   - Kill during compaction.
   - Kill during checkpoint.
   - Kill with multiple in-flight transactions.
2. Each scenario: spawn server, run workload, send `SIGKILL`, restart, validate via client.
3. **Property-based test** with `proptest`: random sequence of ops + random kill points → invariants always hold (no orphan pages, no corrupt index, all committed docs readable).

### Acceptance
- All scenarios green.
- proptest finds no counter-examples in 1 000 iterations.

---

## Phase 30: Criterion Benchmarks

### Goal
Publishable performance numbers in the README. Detect regressions.

### Deliverables
1. **`benches/engine.rs`** (criterion):
   - `insert_single_thread` (small/medium/large doc sizes).
   - `get_by_uuid` (cache hit / cache miss).
   - `scan_full_collection` (1k / 100k docs).
   - `index_query_exact` and `index_query_range`.
2. **`benches/server.rs`**: TCP throughput over loopback, single client and 64 concurrent clients.
3. **`benches/protocol.rs`**: parse 1 M commands/sec target.
4. **README section** "Performance" with a table of headline numbers (laptop reference + reproducer command).
5. **CI bench job** (nightly): runs criterion in `--quick` mode, fails if regression > 20% via `critcmp`.

### Acceptance
- `cargo bench` runs without error.
- README has a populated performance table.

---

## Phase 31: Fuzz the Protocol and JSON Parsers

### Goal
Make the server impossible to crash with malformed input.

### Deliverables
1. **`fuzz/`** directory using `cargo-fuzz`.
2. **Targets**:
   - `parse_command` — fuzz the RESP-like protocol parser.
   - `parse_json` — fuzz the relaxed JSON parser in `grumpy-repl`.
   - `value_codec_roundtrip` — fuzz the document binary codec (encode then decode == identity).
   - `wal_record_decode` — fuzz the WAL record parser.
3. **CI job** (weekly): runs each fuzzer for 5 minutes.
4. **Corpus**: include real captured payloads from integration tests as seed corpus.

### Acceptance
- Each fuzzer runs ≥ 60 seconds locally without crash.
- Found-and-fixed bugs documented in `CHANGELOG.md`.

---

## Phase 32: Workspace Version Alignment

### Goal
Ship one consistent version number for all crates released together.

### Deliverables
1. Bump `grumpydb-protocol`, `grumpydb-client`, `grumpydb-server` to **5.0.0** at the v5 release (Phase 43).
2. Add to `Cargo.toml` under `[workspace.package]`:
   ```toml
   [workspace.package]
   version = "5.0.0"
   edition = "2024"
   rust-version = "1.85"
   license = "MIT"
   repository = "https://github.com/pierreg256/grumpydb"
   ```
   And reference it from each member with `version.workspace = true`.
3. **Compatibility matrix** in README: which client version pairs with which server version.

### Acceptance
- `cargo metadata | jq '.packages[].version'` shows the same version for all 5 crates.
- Documented matrix.

---

# Stream A — Architecture (P2)

## Phase 33: Unify B+Tree on a Generic `Key` Trait

### Goal
Eliminate ~1 500 lines of duplication between fixed-key and variable-key B+Trees.

### Deliverables
1. **New trait** `btree::Key`:
   ```rust
   pub trait Key: Ord + Clone {
       fn encoded_len(&self) -> u16;
       fn encode_to(&self, buf: &mut [u8]);
       fn decode_from(buf: &[u8]) -> Result<Self>;
       const FIXED_LEN: Option<u16>;  // Some(16) for Uuid, None for Vec<u8>
   }
   ```
2. **Generic node format**: a single `BTreeNode<K: Key>`, fixed-key variant uses `FIXED_LEN` to skip the offset table.
3. **Migration path**: keep both implementations behind feature flags during the transition; remove `var_*` files at the end.
4. **Tests**: every existing test re-runs on both `Uuid` and `Vec<u8>` keys via parameterized helper.

### Acceptance
- `src/btree/var_*.rs` files removed.
- Total btree LoC reduced by ≥ 30%.
- All existing tests pass.

---

## Phase 34: Retire `GrumpyDb` Wrapper

### Goal
One way to do it. `Database` is the public API for embedded use.

### Deliverables
1. Mark `GrumpyDb` as `#[deprecated(since = "5.0.0", note = "use Database with the _default collection")]`.
2. Update README, examples, and `taskman` to use `Database`.
3. Remove `engine.rs` in v6 (deprecation cycle).

### Acceptance
- All examples use `Database`.
- Deprecation warning visible at compile time when `GrumpyDb` is used.

---

## Phase 35: Rate Limiting & Connection Caps

### Goal
Make brute-force impractical without breaking legitimate clients.

### Deliverables
1. **Per-IP token bucket** (in-memory, `governor` crate):
   - Default 100 commands/sec, burst 200.
   - Configurable via `[limits]` section in server config.
2. **Auth-attempt limiter**: 5 failed logins per IP per minute → exponential backoff.
3. **Max concurrent connections** per IP and global (default: 100/IP, 10 000 global).
4. **Response code** `RATE_LIMITED` in protocol; client driver handles it with optional retry-after.

### Acceptance
- Integration test: 200 logins with wrong password from same IP → first 5 fail with `Unauthorized`, the rest with `RateLimited`.
- Healthy client at 50 cmd/s never sees a limit.

---

## Phase 36: Health, Readiness, Metrics HTTP

### Goal
Standard endpoints for orchestrators (Kubernetes, docker-compose).

### Deliverables
1. **HTTP server on a separate port** (default 6381, configurable):
   - `GET /healthz` → `200 OK` if process alive.
   - `GET /readyz` → `200 OK` only when WAL recovery is done and TCP listener is accepting.
   - `GET /metrics` → Prometheus text format.
2. **Metrics catalog** (initial set):
   - `grumpydb_connections_active` (gauge)
   - `grumpydb_commands_total{command,result}` (counter)
   - `grumpydb_command_duration_seconds{command}` (histogram)
   - `grumpydb_buffer_pool_pages{state}` (gauge: clean/dirty/pinned)
   - `grumpydb_wal_size_bytes` (gauge)
   - `grumpydb_wal_records_total` (counter)
   - `grumpydb_database_size_bytes{database}` (gauge)
3. **Use `metrics` + `metrics-exporter-prometheus`** crates.

### Acceptance
- `curl http://localhost:6381/metrics` returns valid Prometheus exposition format.
- A docker-compose with Prometheus + Grafana scrapes successfully.

---

## Phase 37: Docker + docker-compose

### Goal
Ten-second demo: clone → `docker-compose up` → working server with REPL.

### Deliverables
1. **`Dockerfile.server`**: multi-stage, distroless final image, ~30 MB.
2. **`Dockerfile.repl`**: same base, ships the `grumpy-repl` binary.
3. **`docker-compose.yml`**:
   - `server` service with persistent volume `grumpydb-data`.
   - `prometheus` + `grafana` (scrapes `/metrics`).
   - `repl` service (interactive on demand: `docker compose run repl`).
4. **`Dockerfile.publish-ci`**: image used to publish to crates.io from a clean environment.
5. **Multi-arch build** (amd64 + arm64) via `docker buildx`.

### Acceptance
- `docker compose up -d` brings the stack up.
- Grafana dashboard imported and showing live metrics.

---

## Phase 38: Snapshot & Restore Tooling

### Goal
Backup-able, restorable database. Foundation for replication seeding (Phase 40).

### Deliverables
1. **New CLI subcommand** `grumpydb-server snapshot`:
   - Issues a checkpoint → flushes WAL → tar.gz of `data/` + `_auth/` + manifest.
   - Output to local path or `s3://` URL (via `aws-sdk-s3`, behind feature flag).
2. **`grumpydb-server restore`**: reverse operation, refuses if data dir non-empty without `--force`.
3. **Manifest** (`snapshot.json`): version, timestamp, last LSN, file list with SHA-256.
4. **Online snapshots**: clients keep working during snapshot (read-locks on collections).

### Acceptance
- Round-trip test: snapshot → wipe → restore → all data identical.
- Documented in `docs/OPERATIONS.md`.

---

# Stream D — Distribution (P3, mandatory)

## Phase 39: RS256 JWT + JWKS

### Goal
Asymmetric tokens. Any node (or external service) can verify a token using only the public key.

### Deliverables
1. **Key management** in `AuthStore`:
   - Generate RSA-2048 keypair on first start (or import existing PEM via config).
   - Store private key in `_auth/jwt_private.pem` (chmod 600), public in `_auth/jwt_public.pem`.
   - Key ID (`kid`) in JWT header.
2. **JWKS endpoint** on the HTTP port (Phase 36): `GET /.well-known/jwks.json`.
3. **Algorithm switch**: `JwtConfig` becomes an enum `Algorithm::{Hs256, Rs256}`, configurable. Default for new deployments: `Rs256`. Existing HS256 deployments continue to work (backward compatibility).
4. **Key rotation**:
   - Two keys can coexist (`current` + `next`); new tokens signed with `current`.
   - Rotation operation: `next` becomes `current`, fresh `next` generated. Old tokens still verify until expiry.
5. **Client driver updates**:
   - Both Rust and TS drivers cache the JWKS, refresh on `kid` miss, retry once.

### Acceptance
- A token issued by node A is verifiable by node B given only B knows A's JWKS URL.
- Unit tests for rotation, expiry, kid mismatch.
- Documented in `docs/AUTH.md`.

---

## Phase 40: WAL-Shipping Replication

### Goal
A primary streams its WAL to N async replicas. Replicas are read-only and lag-bounded.

### Architecture

```
┌──────────────┐   wal record stream   ┌──────────────┐
│   Primary    │ ────────────────────► │   Replica    │
│              │                       │              │
│  WAL writer  │                       │  WAL applier │
│  + ship task │                       │  + apply loop│
└──────────────┘                       └──────────────┘
       ▲                                      │
       │                                      │
       │      writes rejected on replica      │
       │  ◄─────────────────────────────────  │
                  (returned to client)
```

### Deliverables
1. **New crate `grumpydb-replication`** (workspace member):
   - `ShipServer`: TCP server on the primary, streams WAL records to replicas.
   - `ShipClient`: replica side, requests records from a given LSN, applies them locally.
2. **WAL changes** (minor):
   - `WalRecord` already has LSN + tx_id; add a `replicated_at` optional timestamp on the receive side.
   - Allow tailing the WAL log file (long-lived read).
3. **Bootstrap**: a fresh replica syncs via Phase 38 snapshot (catch-up via WAL after the snapshot LSN).
4. **Consistency model**:
   - Async replication, primary acks writes immediately.
   - `READ_CONCERN=primary|local|majority` token in protocol; v5 supports `primary` (always primary) and `local` (any replica). `majority` deferred.
5. **Failover**: **manual only** in v5. Operator promotes a replica via CLI (`grumpydb-server promote`). Automatic election (Raft) is v6.
6. **Monitoring**:
   - Metrics `grumpydb_replication_lag_seconds{replica}`, `grumpydb_replication_records_shipped_total`.
   - `/readyz` on replica returns 503 if lag > threshold.
7. **Server config**:
   ```toml
   [replication]
   role = "primary"           # or "replica"
   listen_ship = "0.0.0.0:6390"
   primary_addr = "primary:6390"  # replicas only
   max_lag_seconds = 5
   ```
8. **Authentication between nodes**: RS256 JWT (Phase 39) with role `replication_peer`. Node-to-node TLS mandatory.
9. **Read on replica**:
   - All reads work transparently.
   - Writes return `ReadOnlyReplica` error with primary address hint.

### Tests
- Integration: 1 primary + 2 replicas, 1 000 inserts, verify lag < 1s, verify all data on replicas.
- Network partition: replica reconnects from last LSN, catches up.
- Snapshot bootstrap: replica joins after primary already has 100k docs.

### Acceptance
- `grumpydb-replication` crate compiles, tests pass.
- 3-node integration test green in CI.
- Documented in `docs/REPLICATION.md`.

---

## Phase 41: MVCC Foundations (Read Snapshots)

### Goal
Unblock reader/writer concurrency. Full multi-writer is v6; v5 ships single-writer + snapshot reads.

### Deliverables
1. **Page-level versioning**: each page write produces a new version with the writer's `tx_id`.
2. **Read transaction** with a snapshot LSN: a reader sees the state as-of its snapshot, regardless of concurrent writes.
3. **Garbage collection**: old page versions pruned once no active reader needs them (tracked LSN watermark).
4. **API additions**:
   - `Database::begin_read()` → `ReadTx`.
   - `ReadTx::get`, `scan`, `query` … all without taking the writer lock.
5. **Existing single-writer path unchanged** — full MVCC writes deferred.
6. **Replica wins big here**: replicas get long-running snapshot reads "for free".

### Acceptance
- Benchmark: 1 writer + 64 concurrent readers shows readers do not block on writer.
- Property test: snapshot reads always see a consistent point in time.
- Documented in `docs/MVCC.md`.

---

## Phase 42: TypeScript Driver Hardening

### Goal
First-class TS driver: published, typed, tested, documented.

### Deliverables
1. Update `drivers/typescript/`:
   - Support RS256 (JWKS fetch).
   - Replication-aware: connection pool of {primary, replicas}, route writes/reads correctly.
   - Comprehensive types for all commands/responses.
2. **CI** (separate job): `npm ci && npm run lint && npm test && npm run build`.
3. **Publish to npm** as `@grumpydb/client@5.0.0`.
4. **Examples** in `drivers/typescript/examples/`: basic CRUD, replicated read, JWKS rotation.
5. **README at the top level** prominently links to npm.

### Acceptance
- `npm install @grumpydb/client` works.
- All TS tests green in CI.

---

## Phase 43: v5.0.0 Release

### Goal
Cut a versioned release that delivers the full plan.

### Deliverables
1. **CHANGELOG**: structured entry summarizing each phase.
2. **Version bump**: all Rust crates → 5.0.0, TS driver → 5.0.0.
3. **Migration guide** `docs/MIGRATING_4_to_5.md`:
   - Bootstrap behavior change.
   - HS256 → RS256 (with opt-in HS256 for legacy).
   - `GrumpyDb` deprecation.
   - Configuration file new sections.
4. **Release artifacts**:
   - GitHub release with binary builds (amd64 + arm64) for the server.
   - Docker images on ghcr.io.
   - All Rust crates on crates.io.
   - npm package.
5. **Blog post / release notes** highlighting replication as the headline.

### Acceptance
- `cargo install grumpydb-server` works on a clean machine and starts.
- Docker image runs.
- Replication demo (3 nodes via docker-compose) showcased.

---

## Module dependency graph (end of v5)

```
grumpydb (engine, MVCC reads)
   ▲
   │
   ├─── grumpydb-protocol (RS256 token schema)
   │
grumpydb-server  ────► grumpydb-protocol
   ▲   ▲
   │   │
   │   └─── grumpydb-replication (WAL shipping)
   │
grumpydb-client ────► grumpydb-protocol
   ▲
   │
grumpy-repl ─────────► grumpydb-client + grumpydb (embedded mode)

drivers/typescript ──► grumpydb-protocol (over-the-wire spec only)
```

---

## Phase ordering & parallelization

```
P0 (must finish first, blocks everything else)
  Phase 24 ──┬──► Phase 25 ──► Phase 26
             │
P1 (parallelizable once P0 done)
             ├──► Phase 27 (tracing)
             ├──► Phase 28 (E2E)
             ├──► Phase 29 (recovery tests)
             ├──► Phase 30 (benchmarks)
             ├──► Phase 31 (fuzzing)
             └──► Phase 32 (versioning)

P2 (parallelizable, low coupling)
             ├──► Phase 33 (unify btree)
             ├──► Phase 34 (retire GrumpyDb)
             ├──► Phase 35 (rate limit)
             ├──► Phase 36 (health/metrics)
             ├──► Phase 37 (docker)
             └──► Phase 38 (snapshot)

P3 (sequential dependencies)
             Phase 39 (RS256/JWKS) ──► Phase 40 (replication)
                                              │
                                              └──► Phase 41 (MVCC reads)
                                              │
             Phase 42 (TS driver) ────────────┤
                                              ▼
                                        Phase 43 (release)
```

### Suggested commit cadence
- One phase per feature branch, one commit per logical sub-step inside the phase.
- After each phase: docs-agent then release-agent (skip the actual publish for intra-stream phases; only publish at Phase 43).

---

## Out of scope for v5 (deferred to v6+)

- **Automatic failover** (Raft / consensus protocol).
- **Multi-writer MVCC** (full snapshot isolation with concurrent writers).
- **Sharding / horizontal partitioning**.
- **Synchronous replication** (`READ_CONCERN=majority`).
- **Cross-region replication** with conflict resolution.
- **Schema validation** (JSON-Schema per collection).
- **Full-text search**.
- **Time-series optimizations**.

---

## Success criteria for v5

| Criterion | Target |
|---|---|
| `cargo clippy --workspace --all-targets -- -D warnings` | passes |
| `unwrap` in `src/` | 0 |
| Test count | ≥ 700 (vs 460 today) |
| Code coverage (tarpaulin) | ≥ 80% on `src/` |
| CI runtime (full pipeline) | ≤ 10 min |
| Documented benchmarks | yes (README section) |
| Replication tested in CI | yes (3-node compose) |
| RS256 JWT default | yes |
| Docker images on ghcr.io | yes (amd64 + arm64) |
| TS driver on npm | yes |
| Migration guide v4 → v5 | yes |
