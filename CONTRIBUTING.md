# Contributing to GrumpyDB

## Prerequisites

- **Rust** (edition 2024) — install via [rustup](https://rustup.rs/)
- `cargo`, `clippy`, `rustfmt` (included with rustup)

## Commands

```bash
cargo build --workspace             # Build all crates
cargo test --workspace              # All tests (~515 across all workspace crates)
cargo test --lib                    # Unit tests only (current crate)
cargo test --test '*'               # Integration tests only
# If a slow CI runner needs more startup budget for server e2e tests:
GRUMPYDB_TEST_SERVER_STARTUP_TIMEOUT_SECS=120 cargo test --test server_e2e
cargo clippy --workspace --all-targets -- -D warnings  # Lint (strict, zero warnings)
cargo fmt --all -- --check          # Check formatting
cargo doc --workspace --no-deps     # Generate docs

# Optional cloud SDK feature flags (used by snapshot/restore — see below).
cargo build --workspace --features grumpydb-server/cloud-aws
cargo build --workspace --features grumpydb-server/cloud-azure
cargo build --workspace --features grumpydb-server/cloud-aws,grumpydb-server/cloud-azure

# Benchmarks (criterion). HTML reports land under target/criterion/report/.
cargo bench                         # All benches: engine + protocol
cargo bench --bench engine          # Engine-only benches
cargo bench --bench protocol        # Protocol-only benches
cargo bench -- --quick              # Quick smoke run (used by CI's bench-smoke)

# Fuzzing (requires `cargo install cargo-fuzz` and a nightly toolchain).
# The fuzz/ crate is intentionally excluded from the workspace.
cd fuzz && cargo +nightly fuzz run parse_command
cd fuzz && cargo +nightly fuzz run value_codec_roundtrip
cd fuzz && cargo +nightly fuzz run wal_record_decode
cd fuzz && cargo +nightly fuzz run response_serialize

# Run a specific binary
# Note: the server requires --bootstrap-password on the FIRST start
# (or the env variable GRUMPYDB_BOOTSTRAP_PASSWORD). Subsequent starts on
# the same data directory do not need it.
cargo run -p grumpydb-server -- --no-tls --data ./data \
    --bootstrap-password "dev-only-password"
# Optional server flags: --log-format json|text (default: json, or text on a TTY).
# RUST_LOG is honored, e.g. RUST_LOG=grumpydb_server=debug.
cargo run -p grumpy-repl
cargo run --example taskman -- help

# Snapshot / restore subcommands (Phase 38). Local destination always
# available; s3:// requires the cloud-aws feature, az:// requires
# cloud-azure. The server must be stopped (snapshot/restore operate on
# files directly in v5).
cargo run -p grumpydb-server -- snapshot --data ./data ./backup.tar.gz
cargo run -p grumpydb-server -- restore  --data ./data ./backup.tar.gz
cargo run -p grumpydb-server --features cloud-aws -- \
    snapshot --data ./data s3://my-bucket/grumpydb/2026-04-28.tar.gz
cargo run -p grumpydb-server --features cloud-azure -- \
    restore  --data ./data az://my-container/grumpydb/2026-04-28.tar.gz --force

# Docker stack (Phase 37): server + Prometheus + Grafana.
# Requires GRUMPYDB_BOOTSTRAP_PASSWORD set in .env (copy from .env.example).
docker compose up -d                           # full stack
docker compose up -d server                    # server only
docker compose --profile repl run --rm repl \
    --host server --tenant _system --user admin \
    --password "$(grep ^GRUMPYDB_BOOTSTRAP_PASSWORD .env | cut -d= -f2-)"

# 3-node cluster smoke test (v5 demo compose).
# Defaults to GRUMPYDB_BOOTSTRAP_PASSWORD=admin if unset.
scripts/smoke_cluster.sh
GRUMPYDB_BOOTSTRAP_PASSWORD=monsecret scripts/smoke_cluster.sh --keep-up

# TypeScript driver (drivers/typescript/)
cd drivers/typescript && npm ci && npm run lint && npm test && npm run build
```

## Project structure

```
grumpydb/                       # workspace root
├── Cargo.toml                  # workspace members
│
├── .github/workflows/          # CI: fmt, clippy, test (stable + 1.85 MSRV), docs, audit, typescript-driver
│
├── src/                        # grumpydb crate (storage engine library)
│   ├── error.rs                # GrumpyError enum, Result<T> alias
│   ├── naming.rs               # Name validation: [a-z0-9_]{1,64}, reserved: _default, _system
│   ├── lib.rs                  # Public API, re-exports
│   ├── engine.rs               # GrumpyDb — thin wrapper over Collection + WAL
│   ├── database/               # Database — multi-collection management with shared WAL
│   ├── collection/             # Collection — unit of document storage, raw CRUD, compact
│   ├── index/                  # Secondary indexes: encoding, SecondaryIndex, IndexDefinition
│   ├── page/                   # Page storage (8 KiB), slotted layout, overflow, free-list
│   ├── btree/                  # B+Tree (fixed UUID + variable-length keys)
│   ├── document/               # Value enum + binary codec
│   ├── wal/                    # Write-Ahead Log: record, writer, recovery
│   ├── buffer/                 # LRU buffer pool with dirty tracking
│   ├── concurrency/            # SharedDb / SharedDatabase / SharedServer (SWMR)
│   └── server/                 # GrumpyServer + Client (multi-tenant)
│
├── grumpydb-protocol/          # RESP-like wire protocol crate
│   └── src/{lib,command,response,parser}.rs
│
├── grumpydb-server/            # TCP/TLS server binary + library
│   └── src/
│       ├── lib.rs main.rs config.rs
│       ├── limits.rs                       # governor token bucket + login backoff + conn caps
│       ├── http.rs                         # /healthz /readyz /metrics (hyper 1.x, port 6381)
│       ├── snapshot.rs                     # tar.gz snapshot/restore (local + s3:// + az://)
│       ├── auth/{user,role,jwt,store}.rs   # argon2, JWT HS256, AuthStore
│       ├── session/mod.rs                  # SessionContext + RBAC enforcer
│       └── tcp/{listener,handler}.rs       # tokio + tokio-rustls
│
├── grumpydb-client/            # Async Rust client driver
│   └── src/{lib,connection,error}.rs
│
├── grumpy-repl/                # Interactive REPL binary (dual-mode: embedded + TCP)
│   └── src/{main,repl,parser,filter,json_parser,tcp_backend}.rs
│
├── grumpydb-testing/           # Internal test harness crate (publish = false).
│   └── src/{lib,server}.rs     # TestServer: spawns the real server binary,
│                               #   random port + tempdir + auto-kill on Drop;
│                               #   exposes crash() (SIGKILL) and restart()
│                               #   for crash-recovery tests; startup retries
│                               #   (3 attempts), configurable startup timeout
│                               #   via GRUMPYDB_TEST_SERVER_STARTUP_TIMEOUT_SECS
│                               #   (default 60s), and early-exit stderr capture.
│
├── drivers/typescript/         # @grumpydb/client npm package (Node ≥ 18)
│   └── src/{index,client,database,connection,protocol,auth,types,errors}.ts
│
├── examples/
│   └── taskman/                # Demo task manager (embedded engine)
│
├── benches/                    # Criterion benchmarks (cargo bench)
│   ├── engine.rs               # 8 benches: insert (small/medium/4 KB), get,
│   │                           #   scan, index exact/range
│   └── protocol.rs             # 3 benches: parse simple cmd, parse 1 KB INSERT,
│                               #   serialize 100-bulk array
│
├── fuzz/                       # cargo-fuzz targets (excluded from workspace)
│   ├── Cargo.toml
│   ├── corpus/<target>/        # Seed corpora
│   └── fuzz_targets/
│       ├── parse_command.rs        # Protocol parser
│       ├── value_codec_roundtrip.rs # Document codec encode/decode stability
│       ├── wal_record_decode.rs    # WAL record decoder
│       └── response_serialize.rs   # Protocol response serializer
│
├── tests/                      # Integration tests
│   ├── crud_test.rs            # Engine CRUD
│   ├── stress_test.rs          # Concurrency stress
│   ├── server_e2e.rs           # TCP end-to-end (8 tests, uses TestServer)
│   ├── server_concurrency.rs   # 50 concurrent clients × 100 ops
│   ├── server_auth.rs          # Expired/tampered tokens, role enforcement, login rate limit
│   ├── server_http.rs          # /healthz /readyz /metrics e2e (Phase 36)
│   ├── snapshot_e2e.rs         # snapshot → restore round-trip via TestServer (Phase 38)
│   └── crash_recovery.rs       # 6 crash-and-restart scenarios
│
├── Dockerfile.server           # Multi-stage: rust:1.88 builder → distroless runtime (~30 MB)
├── Dockerfile.repl             # Same builder, ships grumpy-repl
├── Dockerfile.publish-ci       # CI bash image used for crates.io publication
├── docker-compose.yml          # server + prometheus + grafana + repl (profile-gated)
├── docker/
│   ├── prometheus.yml                              # scrape config for server:6381
│   └── grafana/provisioning/datasources/prometheus.yml
├── .env.example                # GRUMPYDB_BOOTSTRAP_PASSWORD template
├── .dockerignore               # Excludes heavy/non-runtime paths (target/, docs/, tests/, examples/, ...); keeps benches/ for workspace Docker builds
│
└── docs/
    ├── ARCHITECTURE.md
    ├── IMPLEMENTATION_PLAN.md     # phases 1–8: storage engine
    ├── IMPLEMENTATION_PLAN_V2.md  # phases 9–15: server + concurrency + shell
    ├── IMPLEMENTATION_PLAN_V3.md  # phases 16–23: protocol + auth + TCP + drivers
    └── IMPLEMENTATION_PLAN_V4.md  # phases 24–43: hardening, observability, RS256/JWKS, replication, MVCC
```

## Code conventions

### Naming

- `snake_case` for functions and variables
- `CamelCase` for types (structs, enums, traits)
- `UPPER_SNAKE_CASE` for constants
- Test functions: `test_<module>_<expected_behavior>`

### Visibility

- `pub(crate)` by default
- `pub` only for the public API in `lib.rs`

### Error handling

- Use `thiserror` for error definitions
- All functions return `Result<T, GrumpyError>`
- **No `unwrap()`, `expect()`, or `panic!` in `src/`** — enforced by the
  crate-level lint
  `#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::panic, clippy::expect_used))]`
  in `src/lib.rs`. Use `?` propagation; for "shouldn't happen" cases return
  `GrumpyError::Corruption(...)`.
- The lint is allowed inside `#[cfg(test)]` modules.

### Serialization

- All binary formats use **little-endian** byte order
- No `unsafe` code unless documented and justified

### Tests — mandatory

Every `.rs` file must have a `#[cfg(test)] mod tests` block.

Each feature needs at minimum:
- Happy path
- Edge cases (page full, overflow, B+Tree split)
- Error cases (I/O failure, missing key, corruption)

Use `tempfile::TempDir` for any test involving disk I/O.

## Workflow

1. Read the relevant skill file in `.claude/skills/` before coding
2. Implement the feature
3. Write unit tests in the same file
4. Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
5. Write integration tests in `tests/` if the feature spans multiple modules
6. Update documentation (the docs-agent handles this automatically)

Every push to `master` and every PR is validated by GitHub Actions
(`.github/workflows/ci.yml`): `fmt`, `clippy --all-targets -D warnings`,
`test` (matrix: stable + 1.85 MSRV), `docs`, `audit`. A red CI blocks merge.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `uuid` | UUID v4 key generation |
| `thiserror` | Error type definitions |
| `crc32fast` | CRC32 checksums for pages and WAL |
| `parking_lot` | Fast RwLock/Mutex for SWMR concurrency |
| `tempfile` | Temporary directories for tests |
| `rand` | Random data generation for tests |
| `criterion` | Benchmark harness (`benches/engine.rs`, `benches/protocol.rs`) |
| `rustyline` | Line editing for grumpy-repl (binary crate) |
| `serde_json` | JSON serialization for grumpy-repl (binary crate) |
| `futures` | `FutureExt::catch_unwind` for panic isolation in `grumpydb-server` |
| `tracing` | Structured logging spans + events in `grumpydb-server` |
| `tracing-subscriber` (`env-filter`, `json`) | JSON log subscriber, `RUST_LOG` env-filter |
| `governor` | Token-bucket rate limiter (per-IP) in `grumpydb-server::limits` |
| `nonzero_ext` | `nonzero!` macro feeding `governor`'s `NonZeroU32` quotas |
| `hyper` (1.x) | Tiny HTTP server for `/healthz`, `/readyz`, `/metrics` (port 6381) |
| `hyper-util` | `TokioExecutor` glue for `hyper 1.x` server runtime |
| `metrics` | Counter / gauge / histogram facade used throughout `grumpydb-server` |
| `metrics-exporter-prometheus` | Prometheus exposition format renderer for `/metrics` |
| `tar` | Tar archive read/write for snapshot/restore tooling |
| `flate2` | Gzip codec wrapping the tar streams |
| `sha2` | SHA-256 per-file checksums in `snapshot.json` |
| `hex` | Hex encoding of the SHA-256 digests in the manifest |
| `aws-sdk-s3` *(optional, feature `cloud-aws`)* | S3 backend for snapshot/restore |
| `aws-config` *(optional, feature `cloud-aws`)* | Standard AWS credential chain |
| `azure_storage` *(optional, feature `cloud-azure`)* | Azure Blob storage core types |
| `azure_storage_blobs` *(optional, feature `cloud-azure`)* | Azure Blob backend for snapshot/restore |
| `azure_identity` *(optional, feature `cloud-azure`)* | `DefaultAzureCredential` chain |

### Workspace crates

| Crate | Purpose |
|-------|--------|
| `grumpydb` | Core storage engine (library) |
| `grumpydb-protocol` | RESP-like wire protocol (commands, responses, parser) |
| `grumpydb-server` | TCP+TLS server binary (tokio, JWT auth, RBAC, structured tracing) |
| `grumpydb-client` | Async Rust client driver |
| `grumpy-repl` | Interactive REPL shell binary (embedded + TCP) |
| `grumpydb-testing` | Internal test harness (`TestServer`); `publish = false`, never released |

## Contribution licensing

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in GrumpyDB by you shall be dual-licensed as
`MIT OR Apache-2.0` (as defined in `LICENSE`, `LICENSE-MIT`, and
`LICENSE-APACHE`), without additional terms or conditions.
