# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### v5 stream — Hardening (P0)

This unreleased section tracks Stream H of the v5 plan
([docs/IMPLEMENTATION_PLAN_V4.md](docs/IMPLEMENTATION_PLAN_V4.md)). No version
bump or crates.io publication yet.

#### Phase 24 — CI / Clippy / Hygiene
- Added `.github/workflows/ci.yml` with jobs `fmt`, `clippy`, `test`
  (matrix: stable + 1.85 MSRV), `docs`, `audit`.
- README badges added: CI status, crates.io version, docs.rs, MIT license.
- Fixed three clippy issues:
  - `grumpy-repl/src/json_parser.rs` — replaced PI approximation literal in a test.
  - `grumpydb-protocol/src/lib.rs` — converted constant assertions to `const { assert!(...) }` blocks.
  - `examples/taskman/store.rs` — fixed `drop with reference` warning by introducing a scope block.
- Workspace is now `cargo fmt`-clean and passes
  `cargo clippy --workspace --all-targets -- -D warnings`.

#### Phase 25 — Eliminate `unwrap()` in the engine
- New `GrumpyError` variants in `src/error.rs`:
  `Corruption(String)`, `InvalidPageOffset { page, offset }`, `InvalidVarKey(String)`.
- Refactored 73 production `.unwrap()` calls across `src/` to either explicit
  byte-array literals or `?` propagation with `Corruption` errors. Doc-comment
  examples and `#[cfg(test)]` modules were left intact.
- Added a crate-level lint at the top of `src/lib.rs`:
  `#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::panic, clippy::expect_used))]`.
- Added panic isolation in `grumpydb-server/src/tcp/handler.rs`: every
  `execute_command` call is wrapped in `AssertUnwindSafe(...).catch_unwind().await`.
  Panics are caught, logged via `tracing::error!`, and surfaced to the client
  as `Response::Error("internal error (corruption): …")` instead of tearing
  down the whole server.
- Added `futures = "0.3"` dependency to `grumpydb-server/Cargo.toml` for
  `FutureExt::catch_unwind`.
- 0 production `.unwrap()` in `src/` (verified with an awk script that strips
  test modules and doc-comments).

#### Phase 26 — Auth bootstrap & secret hardening
- **Breaking auth bootstrap policy**: the legacy silent `_system/admin/admin`
  bootstrap is gone. `AuthStore::open` now takes a 4th argument
  `bootstrap_password: Option<&str>`. If no users exist on disk and
  `bootstrap_password` is `None`, the call returns
  `Err(AuthError::BootstrapRefused(...))`.
- The bootstrap password is resolved in `grumpydb-server/src/main.rs` from the
  CLI flag `--bootstrap-password <pw>` or the environment variable
  `GRUMPYDB_BOOTSTRAP_PASSWORD`. Bootstrap passwords shorter than 8 characters
  emit a warning.
- New `AuthError` variants in `grumpydb-server/src/auth/user.rs`:
  `ClockError(String)`, `ReadOnly`, `PasswordChangeRequired`,
  `BootstrapRefused(String)`.
- `secret.key` is now created with mode `0600` on Unix; on existing files,
  group/world bits are detected and the file is re-tightened with a warning
  logged.
- Two `SystemTime::now().duration_since(UNIX_EPOCH).unwrap()` sites
  (in `auth/jwt.rs` and `auth/store.rs`) were replaced with `?`-propagated
  `AuthError::ClockError`.
- New tests: `test_store_refuses_silent_bootstrap`,
  `test_store_no_rebootstrap_after_users_exist`,
  `test_secret_key_has_owner_only_permissions` (Unix-only).

### Validation
- `cargo build --workspace` clean
- `cargo test --workspace` — 468 tests pass (was 460)
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo fmt --all -- --check` clean

### Migration notes (operators)
- Existing deployments with users already on disk are unaffected.
- **Brand-new deployments must now pass `--bootstrap-password "<pw>"` (or set
  `GRUMPYDB_BOOTSTRAP_PASSWORD`) on first start**, otherwise the server
  refuses to start with a clear `BootstrapRefused` error.

### v5 stream — Observability (P1)

This unreleased section tracks Stream O of the v5 plan
([docs/IMPLEMENTATION_PLAN_V4.md](docs/IMPLEMENTATION_PLAN_V4.md)). Phases
27–32 are landed in `master` but no version bump or crates.io publication yet.

#### Phase 27 — `tracing` instrumentation
- `grumpydb-server` now produces structured **JSON logs by default**; emits
  text format when stdout is a TTY or when `--log-format text` is passed.
- New CLI flag `--log-format json|text` on the server binary; `RUST_LOG` is
  honored (env-filter).
- `tracing-subscriber` features bumped to `["env-filter", "json"]`.
- TCP listener wraps every accept in `info_span!("connection", peer, tls)`.
- TCP handler wraps every command in `info_span!("command", cmd, user, tenant)`,
  emits `elapsed_us` on completion, and logs auth events (login success/failure,
  token refresh, token verify) with structured fields.
- New helper `command_name(&Command) -> &'static str` for stable, low-cardinality
  command labels.

#### Phase 28 — TCP end-to-end integration tests
- New private workspace member crate **`grumpydb-testing/`** (NOT published,
  `publish = false`) with a `TestServer` struct that spawns the actual
  `grumpydb-server` binary on a random port with a tempdir, kills it on `Drop`.
- New integration tests at workspace root:
  - `tests/server_e2e.rs` — login/whoami, create db/coll, full CRUD cycle,
    index query, count, token refresh, invalid creds, unauthorized command
    (8 tests).
  - `tests/server_concurrency.rs` — 50 concurrent clients × 100 ops each.
  - `tests/server_auth.rs` — expired token, tampered token, role enforcement
    (3 tests).
- **Two real bugs surfaced and fixed during this phase**:
  - `Command::Token(_)` and `Command::Refresh(_)` were missing from
    `Command::is_pre_auth()` — meant `TOKEN`/`REFRESH` commands required prior
    authentication, a chicken-and-egg situation. Fixed in
    `grumpydb-protocol/src/command.rs`.
  - `Command::ListIndexes` was returning an empty `[]` placeholder. Now it
    properly returns the collection's index names.
- **Public API addition**: new
  `SharedDatabase::list_indexes(&str) -> Result<Vec<String>>` method
  (minor-version-worthy on its own).

#### Phase 29 — Crash recovery integration tests
- `TestServer` extended with `crash()` (SIGKILL) and `restart()` (respawn on
  the same data dir + port).
- New `tests/crash_recovery.rs` with 6 scenarios: post-FLUSH crash, no-flush
  crash, mid-insert partial crash, crash during index creation, crash during
  compaction, repeated crash recovery loop.
- All 6 pass green and stable across multiple runs (~6 s wall total).

#### Phase 30 — Criterion benchmarks
- Added `criterion = { version = "0.5", features = ["html_reports"] }` to
  root `[dev-dependencies]`.
- New **`benches/engine.rs`** (8 benchmarks): insert (small/medium/4 KB
  overflow), get (cached/cold), scan, index exact + range queries.
- New **`benches/protocol.rs`** (3 benchmarks): parse simple commands,
  parse 1 KB INSERT, serialize 100-bulk array.
- README has a new "Performance" section with measured numbers from a
  MacBook Pro Apple Silicon run.
- New `bench-smoke` job in `.github/workflows/ci.yml` runs benches in
  `--quick` mode on every CI build (compile + minimal run; not regression
  detection).
- Notable insight from the bench (documented in the README): insert
  throughput is ~230 ops/s steady-state because every CRUD opens a fresh
  WAL transaction with fsync.

#### Phase 31 — Fuzzing with `cargo-fuzz`
- New **`fuzz/`** directory (excluded from the workspace via
  `exclude = ["fuzz"]` in root `[workspace]`) with 4 fuzz targets:
  - `parse_command` — protocol parser.
  - `value_codec_roundtrip` — document binary codec encode/decode stability.
  - `wal_record_decode` — WAL record decoder.
  - `response_serialize` — protocol response serializer.
- Each smoke-fuzzed locally for 20 s — millions of iterations, no panics.
- One real fuzzer-found issue (NaN inequality in a test assertion) was fixed
  in the fuzz target itself (not in the codec).
- New **`.github/workflows/fuzz.yml`** — weekly schedule + manual dispatch,
  runs each target for 5 minutes by default.

#### Phase 32 — Workspace version alignment
- New **`[workspace.package]`** table in root `Cargo.toml` with shared
  `version`, `edition`, `rust-version`, `license`, `repository`, `homepage`.
- Member crates inherit shared fields via `field.workspace = true`.
- `grumpydb` (root) and `grumpy-repl` use `version.workspace = true`.
- Sibling crates (`grumpydb-protocol`, `grumpydb-client`, `grumpydb-server`)
  keep an explicit `version = "1.0.0"` for now — they will be aligned to v5
  at the v5 release commit (Phase 43).

### Validation (P1 stream)
- `cargo build --workspace` clean
- `cargo test --workspace` — **486 tests pass** (was 468 after P0;
  +6 crash recovery, +12 e2e/concurrency/auth)
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- All 4 fuzz targets build and run cleanly (no crashes after 20 s of each).

### v5 stream — Architecture (P2)

This unreleased subsection tracks Stream A of the v5 plan
([docs/IMPLEMENTATION_PLAN_V4.md](docs/IMPLEMENTATION_PLAN_V4.md)). Phases
33–38 are landed in `master` but no version bump or crates.io publication yet.

#### Phase 33 — Unify B+Tree on a generic `Key` trait
- New `btree::Key` trait in `src/btree/key.rs` with `encoded_len`,
  `encode_to`, `decode_from`, and the associated constant
  `FIXED_LEN: Option<u16>`. Implementations for `Uuid`
  (`FIXED_LEN = Some(16)`) and `Vec<u8>` (`FIXED_LEN = None`).
- Single generic `BTreeNode<K>`, `BTree<K>`, `BTreeCursor<K>` replace
  the previous duplicated pairs (`node.rs`+`var_node.rs`,
  `ops.rs`+`var_ops.rs`, `cursor.rs`+`var_cursor.rs`).
- Files **deleted**: `src/btree/var_node.rs`, `src/btree/var_ops.rs`,
  `src/btree/var_cursor.rs`, `src/btree/var_tree.rs`.
- LoC reduction: `src/btree/` went from ~3 500 to 2 581 lines (**−26 %**).
- **On-disk format unchanged** — existing databases keep working
  (verified by the crash-recovery integration tests).
- Public API change: `VarBTree` no longer exists; its replacement is
  `BTree<Vec<u8>>`. It was never re-exported at the crate root, so no
  semver impact for downstream users.

#### Phase 34 — Deprecate `GrumpyDb` wrapper
- `GrumpyDb` and `SharedDb` are now annotated
  `#[deprecated(since = "5.0.0", note = "use Database with the _default collection")]`
  — kept for one major-version cycle, scheduled for removal in v6.
- Internal usage sites (the impls themselves, the `pub use` in
  `src/lib.rs`, `tests/crud_test.rs`, the engine's own concurrency
  wrapper) are silenced via `#[allow(deprecated)]` so we don't spam our
  own builds. **Downstream consumers still see the deprecation warning**
  when they import the type.
- README "Single-collection (simple key-value)" example was rewritten to
  use `Database` instead of `GrumpyDb`. A note documents the deprecation
  and the v6 removal.
- Doc-comment example in `src/lib.rs` switched to `Database`.

#### Phase 35 — Rate limiting & connection caps
- New `grumpydb-server/src/limits.rs` module with `Limits` and
  `LimitsConfig`. Uses `governor 0.6` + `nonzero_ext 0.3`.
- New `[limits]` TOML section in the server config with serde defaults
  for: `commands_per_sec_per_ip` (100), `commands_burst_per_ip` (200),
  `failed_logins_per_min_per_ip` (5), `max_conns_per_ip` (100),
  `max_conns_global` (10 000), and **`bypass_loopback` (default `true`)**.
- Per-IP token bucket for commands; per-IP exponential back-off for
  failed logins (1 s, 2 s, 4 s, 8 s, 16 s, 32 s, capped at 60 s); per-IP
  and global connection caps enforced at accept time.
- **Loopback bypass is on by default** — production deployments that
  expose loopback to untrusted callers should set
  `bypass_loopback = false`.
- Wired into `tcp/listener.rs` (connection accept) and
  `tcp/handler.rs` (command rate limit + login back-off).
- New integration test `test_e2e_login_rate_limited` in
  `tests/server_auth.rs` (uses `bypass_loopback = false`).

#### Phase 36 — HTTP endpoints (`/healthz`, `/readyz`, `/metrics`)
- New `grumpydb-server/src/http.rs` module — a tiny `hyper 1.x` server
  on a separate port (default `0.0.0.0:6381`).
- Endpoints:
  - `GET /healthz` → `200` (process alive).
  - `GET /readyz` → `200` once the TCP listener has bound, else `503`.
  - `GET /metrics` → Prometheus exposition format
    (`text/plain; version=0.0.4`).
  - Anything else → `404`.
- Metrics catalog (initial set, all DESCRIBED in `init_metrics`):
  - `grumpydb_connections_active` (gauge) — wired in listener
    accept/release.
  - `grumpydb_commands_total{cmd,result}` (counter) — wired in handler
    around `execute_command`.
  - `grumpydb_command_duration_seconds{cmd}` (histogram) — same site.
  - `grumpydb_buffer_pool_pages{state}` (gauge) — DESCRIBED, value
    stays at `0` until a future engine-side hook lands.
  - `grumpydb_wal_records_total` (counter) — same status.
  - `grumpydb_login_failures_total{reason}` (counter) — wired.
  - `grumpydb_rate_limit_hits_total{kind}` (counter) — wired.
- New `[http]` section in server config with `bind` field — empty string
  disables the HTTP server entirely.
- `grumpydb-testing/src/server.rs` `TestServer` extended with
  `http_addr: SocketAddr`.
- New integration test file `tests/server_http.rs`
  (`test_e2e_health_endpoints` and friends).
- **No authentication on the HTTP endpoints in v5 by design** (so
  Prometheus and k8s probes can scrape without bootstrap). TODO logged
  for v6 to consider basic-auth or IP allowlisting.

#### Phase 37 — Docker + docker-compose
- New files at the repo root: `Dockerfile.server` (multi-stage with
  `rust:1.88-bookworm` builder + distroless `cc-debian12:nonroot`
  runtime, ~30 MB), `Dockerfile.repl`, `Dockerfile.publish-ci` (CI bash
  image used to publish to crates.io).
- New `docker-compose.yml` with services `server` (healthcheck on
  `/healthz`, now functional thanks to Phase 36),
  `prometheus` (`prom/prometheus:v3.1.0`), `grafana`
  (`grafana/grafana:11.4.0`), and `repl` (profile-gated via
  `--profile repl`).
- `docker/prometheus.yml` (scrape config for `server:6381`),
  `docker/grafana/provisioning/datasources/prometheus.yml`.
- `.env.example` with `GRUMPYDB_BOOTSTRAP_PASSWORD`. `docker-compose`
  refuses to start without it via `${VAR:?msg}` interpolation.
- `.dockerignore` excluding test fixtures, docs, examples, etc.
- README "Running with Docker" section near the Server section.
- All images pinned to explicit tags — no `:latest` anywhere.
- Multi-arch build instructions in `Dockerfile.server` header.

#### Phase 38 — Snapshot & restore tooling
- New `grumpydb-server/src/snapshot.rs` module exposing `snapshot()`
  and `restore()` plus a `Location` enum.
- New CLI subcommands parsed manually before the server-mode dispatch
  in `main.rs`:
  - `grumpydb-server snapshot --data <dir> <DEST>`
  - `grumpydb-server restore --data <dir> <SRC> [--force]`
- Destinations / sources:
  - **Local** filesystem path — always available.
  - **`s3://bucket/key`** — AWS S3 via `aws-sdk-s3 1.x`, behind feature
    `cloud-aws`. Uses the standard AWS credential chain (env, profile,
    instance role).
  - **`az://container/blob`** — Azure Blob Storage via
    `azure_storage_blobs 0.21`, behind feature `cloud-azure`. Uses
    `DefaultAzureCredential` with fallback to
    `AZURE_STORAGE_CONNECTION_STRING`.
- **Tar.gz archive** containing a `snapshot.json` manifest with version
  (`MANIFEST_VERSION = 1`), timestamp, GrumpyDB version, and a per-file
  SHA-256 checksum. Restore verifies every checksum and aborts on
  mismatch.
- Restore refuses to write into a non-empty data dir without `--force`.
- **Online snapshot semantics**: holds the `SharedDatabase` write lock
  for the duration of the file copy (blocks writers, reads continue).
  v6 with MVCC will offer point-in-time consistency.
- New deps (root): `tar`, `flate2`, `sha2`, `hex`. Optional cloud SDKs
  are gated by features (`aws-sdk-s3` + `aws-config` for `cloud-aws`;
  `azure_storage` + `azure_storage_blobs` + `azure_identity` for
  `cloud-azure`).
- New tests: 9 unit tests in `snapshot.rs`, integration test
  `tests/snapshot_e2e.rs` (round-trip via `TestServer`), plus
  `tests/snapshot_aws.rs` and `tests/snapshot_azure.rs` (`#[ignore]`d,
  require live cloud credentials).

### Validation (P2 stream)
- `cargo build --workspace` clean (default features)
- `cargo build --workspace --features grumpydb-server/cloud-aws` clean
- `cargo build --workspace --features grumpydb-server/cloud-azure` clean
- `cargo build --workspace --features grumpydb-server/cloud-aws,grumpydb-server/cloud-azure`
  clean
- `cargo test --workspace` — **515 tests pass** (was 497 at the end of
  P1; net +18: −1 from B+Tree merge, +1 rate-limit e2e, +4 HTTP unit,
  +2 HTTP e2e, +9 snapshot unit, +1 snapshot e2e, +others)
- `cargo clippy --workspace --all-targets -- -D warnings` clean
  (default + all cloud features)
- `cargo fmt --all -- --check` clean
- `RUSTDOCFLAGS="-Dwarnings" cargo doc --workspace --no-deps` clean
- `RUSTDOCFLAGS="-Dwarnings" cargo doc --workspace --no-deps --features grumpydb-server/cloud-aws,grumpydb-server/cloud-azure`
  clean

## [4.1.0] - 2026-04-28

Minor release: the interactive REPL is promoted from an example to a first-class workspace crate published on crates.io as `grumpy-repl 4.1.0`. No engine API changes. Only `grumpydb` (4.0.0 → 4.1.0) and the new `grumpy-repl` crate are published in this release; `grumpydb-protocol`, `grumpydb-server`, and `grumpydb-client` are unchanged.

### Changed
- **REPL promoted to first-class workspace crate**: the interactive shell formerly known as `GrumpyShell` (under `examples/grumpysh/`) has been moved to a dedicated workspace member crate `grumpy-repl/` (binary `grumpy-repl`, version 4.1.0). Sources `main.rs`, `repl.rs`, `parser.rs`, `filter.rs`, `json_parser.rs`, `tcp_backend.rs` were moved with `git mv` from `examples/grumpysh/` to `grumpy-repl/src/`.
- Workspace `Cargo.toml` `members` now lists `grumpy-repl`. Dev-dependencies `rustyline`, `serde_json`, `grumpydb-client`, `grumpydb-protocol`, and `tokio` were removed from the root crate (only used by the old example).
- CLI display strings, default data directory, and rustyline history file were renamed:
  - `GrumpyShell` → `grumpy-repl` (printed banners, help text, doc-comments)
  - `.grumpysh_data` → `.grumpy_repl_data` (default data dir for embedded mode)
  - `~/.grumpysh_history` → `~/.grumpy_repl_history` (rustyline history)
  - Usage examples switched from `cargo run --example grumpysh` to `cargo run -p grumpy-repl`
- `.gitignore` now ignores `.grumpy_repl_data/` (kept `.grumpysh_data/` for backward compatibility).
- Documentation (`README.md`, `CONTRIBUTING.md`, `docs/ARCHITECTURE.md`, `docs/IMPLEMENTATION_PLAN_V2.md`, `docs/IMPLEMENTATION_PLAN_V3.md`, `grumpydb-client/src/lib.rs`) updated to reflect the new crate name and binary invocation.
- `grumpydb` bumped 4.0.0 → **4.1.0** (workspace re-shuffle, no engine API change).
- `grumpy-repl` first publication at **4.1.0** (kept aligned with the root crate version).

## [4.0.0] - 2026-04-28

Major release: networked multi-tenant server with authentication and RBAC. Closes phases 16–23 of the v3 plan (client interface).

### Added
- **RESP-like Protocol Crate** (Phase 16): new `grumpydb-protocol` crate (v1.0.0) with Command enum, Response serialization/parsing, RESP-like single-line parser, Action/Resource enums for RBAC metadata (70 tests)
- **Authentication & RBAC** (Phases 17–18): new `grumpydb-server` crate (v1.0.0) auth module — argon2 password hashing, JWT (HS256) access & refresh tokens, 5-role RBAC model (`admin`, `dba`, `read_write`, `read_only`, `auditor`), per-connection `SessionContext`, RBAC enforcer with `authorize()` (56 tests)
- **TCP/TLS Server** (Phase 19): async TCP+TLS server built on `tokio` + `tokio-rustls`, auto-generated self-signed certs via `rcgen`, TOML configuration, full command executor with RBAC enforcement, graceful shutdown (60 tests)
- **Rust Client Driver** (Phase 20): new `grumpydb-client` crate (v1.0.0) with async TCP+TLS connection, LOGIN/TOKEN/REFRESH auth, `DatabaseHandle` CRUD+index+admin API, `raw_execute()` for direct protocol commands, `NoCertVerifier` for dev TLS
- **TypeScript Client Driver** (Phase 21): new `@grumpydb/client` npm package under `drivers/typescript/` — zero runtime dependencies, `node:net`/`node:tls` transport, full CRUD+auth+admin API
- **GrumpyShell v2** (Phase 22): dual-mode shell — connected (TCP client) and embedded (direct disk)
  - CLI flags: `--host`, `--port`, `--tenant`, `--user`, `--password`, `--tls`/`--no-tls`, `--embedded`
  - `examples/grumpysh/tcp_backend.rs`: `TcpBackend` wrapping `grumpydb-client` with synchronous `block_on()`
  - Notation `user@tenant` and `[collection:][db][@tenant]` for resource paths
  - E2E tested over TCP: LOGIN, USE, CREATE COLLECTION, INSERT, GET, DELETE, COUNT, SCAN
- `src/naming.rs`: `_system` is now an allowed reserved name (alongside `_default`)
- `grumpydb-server/src/tcp/handler.rs`: LOGIN auto-creates tenant, USE auto-creates database
- `grumpydb-client/src/lib.rs`: `raw_execute()` for forwarding raw protocol commands
- ~445 total tests across the workspace, 0 clippy warnings

### Changed
- `grumpydb` bumped 3.1.0 → **4.0.0** (new networking layer, new public surface via sibling crates)
- Workspace now contains 4 crates: `grumpydb`, `grumpydb-protocol`, `grumpydb-server`, `grumpydb-client`
- `README.md` rewritten for the v3 networked architecture
- `docs/ARCHITECTURE.md` §19.4 updated with the multi-tenant server topology
- `docs/IMPLEMENTATION_PLAN_V3.md` marked phases 16–22 complete; phase 23 partially complete (final polish, formal integration tests, Docker image, and CI deferred)

### Notes
- Phase 23 deferred items: Dockerfile, GitHub Actions CI matrix, formal end-to-end integration test suite, additional polish (will land in a future patch / minor release)


## [3.1.0] - 2026-04-24

### Added
- `GrumpyDb::migrate_to_database()` — migration tool to move v1 single-collection data into a v2 Database collection
- TaskMan v5: store.rs rewritten to use `Database` API with `create_collection("tasks")` and secondary index on `done` field
- TaskMan concurrent: updated to use `SharedDatabase` instead of `SharedDb`
- Stress test: 3 clients × 3 databases × 3 collections × 1,000 docs + concurrent multi-database test

### Changed
- Applied `cargo fmt` across the entire codebase
- 314 total tests (296 unit + 14 integration + 4 doctests), 0 clippy warnings, 0 fmt diffs

## [3.0.0] - 2026-04-24

### Added
- **Multi-Tenant Server** (Phase 13): full client/server hierarchy for multi-tenant isolation
  - `src/server/mod.rs`: `GrumpyServer` struct — multi-tenant server managing isolated clients
  - `src/server/client.rs`: `Client` struct — per-tenant client with independent databases
  - Full hierarchy: Server → Client → Database → Collection
  - `GrumpyError::ClientNotFound` and `GrumpyError::DatabaseNotFound` error variants
  - `GrumpyServer` and `Client` exported from `lib.rs`
  - 19 new tests (9 client + 10 server)
- **Concurrency v2** (Phase 14): thread-safe wrappers for Database and Server
  - `SharedDatabase` — thread-safe Database wrapper with per-database `Arc<RwLock>`
  - `SharedServer` — multi-tenant server with independent per-database locking
  - Concurrent writes to different databases without contention
  - `SharedDatabase` and `SharedServer` exported from `lib.rs`
  - 9 new concurrency tests (4 SharedDatabase + 5 SharedServer)
- **Polish & Migration** (Phase 15): migration tool, stress tests, TaskMan v5
  - `GrumpyDb::migrate_to_database()` — migrates all docs from v1 GrumpyDb to v2 Database collection
  - TaskMan v5: rewrote store.rs to use `Database` API with `create_collection("tasks")` and secondary index on `done` field
  - TaskMan concurrent: updated to use `SharedDatabase` instead of `SharedDb`
  - Stress test: 3 clients × 3 databases × 3 collections × 1,000 docs + concurrent multi-database test
  - All formatting fixed with `cargo fmt`
- 314 total tests (296 unit + 14 integration + 4 doctests), 0 clippy warnings, 0 fmt diffs

## [2.1.0] - 2026-04-23

### Added
- **Document References** (Phase 12c): cross-collection document linking with cycle detection
  - `Value::Ref(String, Uuid)` — reference type pointing to a document in another collection
  - Binary codec `TAG_REF = 0x08` for serialization/deserialization of Ref values
  - Sortable index encoding for Ref (`TAG_REF = 0x06`) in `src/index/encoding.rs`
  - `GrumpyError::CyclicReference` error variant for detecting circular reference chains
  - `Database::resolve_ref()` — resolve a single Ref to its target document
  - `Database::resolve_deep()` — recursively resolve all Ref fields with cycle detection
  - GrumpyShell: `$ref("collection", "uuid")` syntax for creating references in documents
  - GrumpyShell: `resolve()` and `resolveDeep()` commands for reference resolution
- 268 total tests (253 unit + 12 integration + 3 doctests), 0 clippy warnings

## [2.0.0] - 2026-04-23

### Added
- **Secondary Indexes** (Phase 11): fast exact-match and range queries on document fields
  - `src/index/encoding.rs`: sortable binary encoding — `encode_sortable_value()`, `encode_composite_key()`, `extract_field()`. Integer XOR sign-bit encoding, IEEE 754 float sort, string truncation to 128 bytes. 13 tests.
  - `src/index/mod.rs`: `SecondaryIndex` struct backed by VarBTree — `IndexDefinition`, `lookup()`, `range_query()`, `rebuild()`, `index_document()`, `unindex_document()`. 7 tests.
  - Collection integration: `create_index()`, `drop_index()`, `list_indexes()`, `query_index()`, `query_index_range()`, `insert_doc()`, `delete_doc()`. Compact rebuilds secondary indexes.
  - 5 new error variants: `NotIndexable`, `IndexNotFound`, `IndexAlreadyExists`, `CollectionNotFound`, `InvalidName`
  - `IndexDefinition` exported from `lib.rs`
- **Database** (Phase 12): multi-collection management with shared WAL
  - `src/database/mod.rs`: `Database` struct — `create_collection()`, `drop_collection()`, `list_collections()`. Full CRUD routed by collection name. Index management. Auto-discovery of existing collections on open. 12 tests.
  - `src/naming.rs`: `validate_name()` with `[a-z0-9_]{1,64}` validation. 5 tests.
  - `Database` exported from `lib.rs`
- **GrumpyShell** (Phase 12b): interactive JavaScript-like REPL for exploring GrumpyDB
  - `examples/grumpysh/main.rs`: CLI entry with `--data`, `--eval`, `--help`. Rustyline integration with history.
  - `examples/grumpysh/repl.rs`: read-eval-print loop with database state management
  - `examples/grumpysh/parser.rs`: command parser — `use`, `db.method()`, `db.coll.method()`, `Command` enum
  - `examples/grumpysh/json_parser.rs`: relaxed JSON parser (unquoted keys, single quotes, trailing commas). 11 tests.
  - `examples/grumpysh/filter.rs`: client-side document matching for `find({ field: value })`. 6 tests.
  - `rustyline` and `serde_json` added to dev-dependencies
- 268 total tests (253 unit + 12 integration + 3 doctests), 48 new tests, 0 clippy warnings

## [1.2.0] - 2026-04-23

### Added
- **Collection abstraction** (Phase 10): extracted per-collection storage from engine
  - `src/collection/mod.rs`: `Collection` struct — self-contained data pages + primary index
  - `Collection::open(path, name, pool_capacity)` — opens/creates a collection directory with `data.db` + `primary.idx`
  - Raw CRUD: `insert_raw()`, `get_raw()`, `delete_raw()`, `scan_raw()` — no WAL, caller handles logging
  - `PageWriteRecord` struct: before/after page images for WAL logging
  - `compact()`, `flush()`, `document_count()`, `pool_stats()`
  - `data_page_manager()`, `index_page_manager()` — for WAL recovery access
  - 10 new Collection unit tests (create, CRUD, scan, compact, overflow, persistence, duplicate key, pool stats)
  - 230 total tests (215 unit + 12 integration + 3 doctests), 0 clippy warnings

### Changed
- **Engine refactored**: `GrumpyDb` is now a thin wrapper over `Collection` + `WalWriter`
  - All internal page management code removed from engine (delegated to Collection)
  - WAL logging remains at engine level using `PageWriteRecord` from Collection
  - WAL recovery done on raw `PageManager`s before creating Collection (avoids double-borrow)
  - Index file renamed: `index.db` → `primary.idx` (matching Collection naming)

## [1.1.0] - 2026-04-23

### Added
- **Variable-Key B+Tree** (Phase 9): parallel `VarBTree` for variable-length byte keys
  - `src/btree/key.rs`: key encoding utilities — `encode_var_key()`, `decode_var_key()`, `var_key_disk_size()`, `VAR_KEY_MAX_SIZE=256`
  - `src/btree/var_node.rs`: `VarInternalNode`, `VarLeafNode` with fixed-stride serialization (length prefix + padded to max_key_size)
  - `src/btree/var_ops.rs`: search, insert (with split), delete (with merge/redistribute) for VarBTree
  - `src/btree/var_tree.rs`: `VarBTree` struct — `create(path, max_key_size)`, `open(path)`, `search()`, `insert()`, `delete()`, metadata persistence
  - `src/btree/var_cursor.rs`: `VarCursor` with `scan_all()`, `range()`, `cursor_from()`
  - Capacity functions: `var_internal_max_keys()`, `var_leaf_max_entries()`
  - 30 new tests (key encoding, node serialization, CRUD, splits, deletes, cursor, stress 3,000 keys)
  - 220 total tests (205 unit + 12 integration + 3 doctests), 0 clippy warnings
- Zero changes to existing BTree code (parallel implementation, no regression risk)

## [1.0.0] - 2026-04-22

### Added
- **Compaction** (Phase 8.1): defragment data pages and rebuild B+Tree index
  - `GrumpyDb::compact()` → rewrite all live documents into tightly-packed pages
  - `CompactResult` struct with preserved document count
  - `GrumpyDb::document_count()` → O(1) count via B+Tree metadata
  - `SharedDb::compact()`, `SharedDb::document_count()`, `SharedDb::pool_stats()`
  - `CompactResult` exported from `lib.rs`
  - 4 engine tests: compact after deletes, compact with overflow, compact empty, document count
- **Page checksums** (Phase 8.2): CRC32 integrity check on every page read/write
  - `page::compute_checksum()`, `page::stamp_checksum()`, `page::verify_checksum()`
  - Legacy pages (checksum==0) skip verification for backwards compatibility
  - `ChecksumMismatch` error variant on corruption detection
  - `PageManager::path()` accessor (needed for compaction)
  - 3 new checksum tests in `page/mod.rs`
- **Stress test** (Phase 8.2): `test_stress_random_operations` — 10,000 random operations
- **Compact integration test**: `test_compact_integration` — compact + reopen + verify
- **TaskMan Final** (Phase 8b): polished demo app with tutorial and cookbook
  - `compact` and `count` CLI commands
  - `TaskStore::compact()` and `TaskStore::document_count()` methods
  - `examples/taskman/TUTORIAL.md` — 7-chapter tutorial covering all GrumpyDB features
  - `examples/taskman/COOKBOOK.md` — 7 self-contained recipes for common tasks
- 190 total tests (175 unit + 12 integration + 3 doctests), 0 clippy warnings, 0 doc warnings

### Changed
- `PageManager::write_page()` now stamps CRC32 checksum before writing
- `PageManager::read_page()` now verifies CRC32 checksum after reading

## [0.5.0] - 2026-04-22

### Added
- **Buffer Pool** (`src/buffer/`): LRU page cache for reduced disk I/O (Phase 6)
  - `BufferFrame`: page caching with pin/unpin and dirty tracking
  - `BufferPool`: LRU eviction, `fetch_page()`, `new_page()`, `flush_all()`, I/O counters
  - Engine integration: data page access goes through the pool (256 frames = 2 MiB default)
  - `GrumpyDb::open_with_pool_capacity()` for custom pool sizing
  - `GrumpyDb::pool_stats()` for read/write/cache monitoring
  - Overflow pages bypass the pool (sequential, not revisited)
  - 11 buffer pool unit tests + 3 engine integration tests
- **TaskMan v3** (Phase 6b): performance benchmarks
  - `generate --count N` command: bulk-insert synthetic tasks with pool stats output
  - `search --tag TAG` command: scan + filter with pool stats output
  - `store.rs`: `pool_stats()` method
  - `PERFORMANCE.md`: buffer pool guide (architecture, impact table, capacity tuning)
- 181 total tests, 0 clippy warnings

### Changed
- `GrumpyDb` engine now uses `BufferPool` for all data page access instead of direct `PageManager`
- `flush()` now flushes buffer pool dirty pages before WAL checkpoint

## [0.4.0] - 2026-04-21

### Added
- **SWMR concurrency** (`src/concurrency/`): thread-safe database access (Phase 7)
  - `SharedDb`: `Arc<RwLock<GrumpyDb>>` wrapper with `Clone` for thread sharing
  - Concurrent reads and exclusive writes via `parking_lot::RwLock`
  - 7 concurrency tests: multi-reader, writer+readers, contention, persistence
- **TaskMan v4** (Phase 7b): multi-threaded demo
  - `concurrent.rs`: `run_bench()` multi-thread benchmark, `run_server()` TCP server
  - `bench` command: configurable writers/readers/count
  - `serve` command: line-protocol TCP server with per-client threads
  - Full concurrency documentation in comments
- `SharedDb` re-exported from `lib.rs`
- 165 total tests, 0 clippy warnings

### Note
- Phase 6 (Buffer Pool) skipped for now — will be implemented later
- `SharedDb::get()` currently uses write lock (B+Tree cursor needs &mut self)

## [0.3.1] - 2026-04-21

### Added
- **TaskMan README** (`examples/taskman/README.md`): full docs with data safety section, WAL explanation, API patterns table
- **Crash test script** (`examples/taskman/test_crash.sh`): 6-step automated test (insert, export, restart, flush, re-import, verify)

### Fixed
- Phase 5 and 5b tasks now fully checked in implementation plan
- All documentation updated to reflect completed WAL + demo app work

## [0.3.0] - 2026-04-21

### Added
- **Write-Ahead Log** (`src/wal/`): crash recovery and durability (Phase 5)
  - `WalRecord`: binary serialization with CRC32 checksums
  - `WalWriter`: append-only writer with fsync on commit, LSN tracking
  - Recovery: redo committed TXs, undo uncommitted TXs, checkpoint support
  - Engine integration: all page writes logged, auto-checkpoint every 100 writes
- **TaskMan v2** (Phase 5b): crash safety demo
  - `export` command: dump all tasks to pipe-delimited file
  - `import` command: bulk import with duplicate detection
  - `flush` command: explicit WAL checkpoint
  - Help updated with crash safety documentation
- 19 new WAL unit tests (record, writer, recovery)
- 157 total tests, 0 clippy warnings

### Changed
- `GrumpyDb::flush()` now writes WAL checkpoint and truncates WAL
- `GrumpyDb::open()` runs WAL recovery automatically

## [0.2.1] - 2026-04-21

### Added
- **TaskMan example app** (`examples/taskman/`): fully documented task manager CLI (Phase 4b)
  - `task.rs`: Task struct with `to_value()`/`from_value()` conversions, Display impl
  - `store.rs`: TaskStore wrapper around GrumpyDb (add, get, update, delete, list, stats)
  - `main.rs`: CLI with subcommands (add, list, done, undone, show, delete, stats, help)
  - Every GrumpyDB API call has inline documentation comments
  - Demonstrates: CRUD, scan+filter, read-modify-write pattern, error handling
- **Release agent** (`.claude/agents/release-agent.md`): automated versioning workflow
- Demo app phases (4b-8b) added to implementation plan

### Fixed
- Clippy warnings fixed across all targets (useless-vec, Range::contains, approx PI, constant assertions)

## [0.2.0] - 2026-04-21

### Added
- **Storage engine** (`src/engine.rs`): full CRUD wiring connecting pages + B+Tree + documents (Phase 4)
  - `GrumpyDb::open()`: creates/opens `data.db` + `index.db` in a directory
  - `insert(key, value)`: encode document → slotted page (or overflow) → B+Tree index
  - `get(key)`: B+Tree search → read page/slot → decode document
  - `update(key, value)`: delete + re-insert
  - `delete(key)`: remove from slotted page + free overflow + remove from B+Tree
  - `scan(range)`: B+Tree range cursor → read each document
  - `flush()` / `close()`: sync all data to disk
  - Overflow page support for large documents (>8 KiB)
  - Auto-allocation of new data pages when current is full
- **Integration tests** (`tests/crud_test.rs`): 10 cross-module tests
- **Release agent** (`.claude/agents/release-agent.md`): automated versioning workflow
- 138 total tests (126 unit + 10 integration + 2 doctests)

### Changed
- `GrumpyDb` methods now take `&mut self` (was `&self` stubs)
- Public API re-exports updated in `lib.rs`

## [0.1.0] - 2026-04-21

### Added
- **Page storage** (`src/page/`): 8 KiB page management with slotted layout, overflow chains, and free-list (Phase 1)
  - `PageManager`: disk I/O, page allocation/free with persistent free-list
  - `SlottedPage`: variable-length tuple storage with insert/get/delete/update/compact
  - Overflow pages: chained pages for documents larger than a single page
  - Constants: `PAGE_SIZE=8192`, `PAGE_HEADER_SIZE=32`, `SLOT_SIZE=4`
- **B+Tree index** (`src/btree/`): complete B+Tree with search, insert (split), delete (merge/redistribute), and cursor (Phase 2)
  - `InternalNode` / `LeafNode` with binary serialization
  - Fan-out: 407 internal keys, 370 leaf entries per node
  - `BTreeCursor` for range scans over doubly-linked leaf list
  - Metadata stored in page 1, root in page 2
- **Document model** (`src/document/`): schema-less JSON-like values with binary codec (Phase 3)
  - `Value` enum: Null, Bool, Integer, Float, String, Bytes, Array, Object
  - Binary codec with type tags, safety limits (nesting depth, blob size)
  - `Document` struct: UUID key + Value with encode/decode
- **Error handling** (`src/error.rs`): centralized `GrumpyError` enum with 10 variants
- **Engine stub** (`src/engine.rs`): `GrumpyDb` struct with open/close (CRUD not yet wired)
- 112 unit tests, 0 clippy warnings

### Not yet implemented
- Storage engine CRUD wiring (Phase 4)
- Write-Ahead Log (Phase 5)
- Buffer pool LRU cache (Phase 6)
- SWMR concurrency (Phase 7)
