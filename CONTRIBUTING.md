# Contributing to GrumpyDB

## Prerequisites

- **Rust** (edition 2024) — install via [rustup](https://rustup.rs/)
- `cargo`, `clippy`, `rustfmt` (included with rustup)

## Commands

```bash
cargo build --workspace             # Build all crates
cargo test --workspace              # All tests (~445 across all workspace crates)
cargo test --lib                    # Unit tests only (current crate)
cargo test --test '*'               # Integration tests only
cargo clippy --workspace -- -D warnings  # Lint (strict, zero warnings)
cargo fmt --check                   # Check formatting
cargo doc --workspace --no-deps     # Generate docs

# Run a specific binary
cargo run -p grumpydb-server -- --no-tls --data ./data
cargo run --example grumpysh
cargo run --example taskman -- help

# TypeScript driver (drivers/typescript/)
cd drivers/typescript && npm install && npm test && npm run build
```

## Project structure

```
grumpydb/                       # workspace root
├── Cargo.toml                  # workspace members
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
│       ├── auth/{user,role,jwt,store}.rs   # argon2, JWT HS256, AuthStore
│       ├── session/mod.rs                  # SessionContext + RBAC enforcer
│       └── tcp/{listener,handler}.rs       # tokio + tokio-rustls
│
├── grumpydb-client/            # Async Rust client driver
│   └── src/{lib,connection,error}.rs
│
├── drivers/typescript/         # @grumpydb/client npm package (Node ≥ 18)
│   └── src/{index,client,database,connection,protocol,auth,types,errors}.ts
│
├── examples/
│   ├── grumpysh/               # Dual-mode REPL (embedded + TCP)
│   └── taskman/                # Demo task manager (embedded engine)
│
├── tests/                      # Integration tests (engine + concurrency)
│   ├── crud_test.rs
│   └── stress_test.rs
│
└── docs/
    ├── ARCHITECTURE.md
    ├── IMPLEMENTATION_PLAN.md     # phases 1–8: storage engine
    ├── IMPLEMENTATION_PLAN_V2.md  # phases 9–15: server + concurrency + shell
    └── IMPLEMENTATION_PLAN_V3.md  # phases 16–23: protocol + auth + TCP + drivers
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
- No `unwrap()` or `panic!` outside of tests

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
4. Run: `cargo test && cargo clippy -- -D warnings`
5. Write integration tests in `tests/` if the feature spans multiple modules
6. Update documentation (the docs-agent handles this automatically)

## Dependencies

| Crate | Purpose |
|-------|---------|
| `uuid` | UUID v4 key generation |
| `thiserror` | Error type definitions |
| `crc32fast` | CRC32 checksums for pages and WAL |
| `parking_lot` | Fast RwLock/Mutex for SWMR concurrency |
| `tempfile` | Temporary directories for tests |
| `rand` | Random data generation for tests |
| `rustyline` | Line editing for GrumpyShell REPL (dev) |
| `serde_json` | JSON serialization for GrumpyShell (dev) |

### Workspace crates

| Crate | Purpose |
|-------|--------|
| `grumpydb` | Core storage engine (library) |
| `grumpydb-protocol` | RESP-like wire protocol (commands, responses, parser) |
| `grumpydb-server` | TCP+TLS server binary (tokio, JWT auth, RBAC) |
| `grumpydb-client` | Async Rust client driver |
