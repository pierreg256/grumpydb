# GrumpyDB — Claude Instructions

## Project

GrumpyDB is a disk-based object storage engine written in Rust. It provides persistent storage of schema-less documents (JSON-like) with B+Tree indexing, page-based storage, WAL for durability, and SWMR concurrency.

## Architecture

```
┌──────────────────────────────────────┐
│         Public API (lib.rs)          │  ← CRUD interface for external apps
├──────────────────────────────────────┤
│  Database (database/) + Engine (engine.rs) │  ← Multi-collection + single-collection wrappers
├──────────────────────────────────────┤
│     Collection (collection/) + Indexes    │  ← Unit of storage: data + primary + secondary
├────────────┬─────────────┬────────────┤
│  Document  │  Concurrency │  Buffer   │
│  Model     │  (SWMR)      │  Pool     │
├────────────┼─────────────┼────────────┤
│  B+Tree    │     WAL     │  Page     │
│  Index     │             │  Manager  │
│(primary.idx)│  (wal.log)  │ (data.db) │
└────────────┴─────────────┴────────────┘
```

### On-disk files

| File          | Role                                      |
|---------------|-------------------------------------------|
| `data.db`     | Page-based document storage                |
| `primary.idx` | B+Tree index (UUID → PageId + SlotId)      |
| `idx_*.idx`   | Secondary indexes (field value + UUID)     |
| `wal.log`     | Write-Ahead Log for crash recovery         |

### Modules

| Module         | Responsibility                                          |
|----------------|---------------------------------------------------------|
| `page`         | 8 KiB pages, slotted layout, overflow, free-list        |
| `btree`        | B+Tree index (fixed UUID keys + variable-length keys), search/insert/delete/split/merge, cursor |
| `wal`          | WAL records, writer, checkpoint, recovery                |
| `buffer`       | Buffer pool LRU, dirty tracking, pin/unpin               |
| `document`     | Value type (JSON-like + Ref), binary codec                |
| `collection`   | Unit of storage: data pages + primary index + secondary indexes, raw CRUD |
| `index`        | Secondary indexes: sortable encoding, SecondaryIndex, IndexDefinition |
| `database`     | Multi-collection management with shared WAL, CRUD routing, reference resolution |
| `naming`       | Name validation: `[a-z0-9_]{1,64}`                       |
| `concurrency`  | SWMR lock manager, page-level locks                     |
| `engine`       | Thin wrapper over Collection + WAL, exposes public CRUD  |
| `error`        | Centralized error types (16 variants)                    |

## Code conventions

### Rust

- **Edition**: 2024
- **Errors**: `thiserror` for definitions, `Result<T, GrumpyError>` everywhere
- **Unsafe**: forbidden unless documented justification (mmap only if decided)
- **Naming**: snake_case for functions/variables, CamelCase for types, UPPER_SNAKE for constants
- **Visibility**: `pub(crate)` by default, `pub` only for the public API in `lib.rs`
- **Documentation**: doc-comments (`///`) on all public API and key internal types
- **Constants**: all magic numbers in `src/page/mod.rs` (PAGE_SIZE, HEADER_SIZE, etc.)

### Tests — MANDATORY

Every `.rs` source file must have a `#[cfg(test)] mod tests` block with unit tests.

- **Unit tests**: in each module, test isolated logic
- **Integration tests**: in `tests/`, test cross-module interactions
- **Minimum coverage** per feature:
  - Happy path
  - Edge cases (full page, overflow, B+Tree node split)
  - Error cases (I/O failure, missing key, corruption)
- **Fixtures**: use `tempfile::TempDir` for tests with disk I/O
- **Naming**: `test_<module>_<expected_behavior>`
- Run tests: `cargo test`
- Run specific test: `cargo test test_name`
- Tests with output: `cargo test -- --nocapture`

### Development workflow

1. **Before coding**: read the relevant skill in `.claude/skills/`
2. **Implement** the feature
3. **Write tests** (unit tests in the same file)
4. **Verify**: `cargo test && cargo clippy -- -D warnings`
5. **Integration tests** if the feature touches multiple modules

### Useful commands

```bash
cargo test                          # All tests
cargo test --lib                    # Unit tests only
cargo test --test '*'               # Integration tests only
cargo clippy -- -D warnings         # Strict lint
cargo fmt --check                   # Check formatting
cargo doc --no-deps --open          # Generate docs
```

## Module dependencies (build order)

```
error (no internal dependencies)
  → page (depends on error)
    → document (depends on error, page for page serialization)
      → btree (depends on error, page, document)
        → wal (depends on error, page)
          → buffer (depends on error, page)
            → index (depends on error, btree, document)
              → collection (depends on error, page, btree, buffer, index)
                → naming (depends on error)
                  → concurrency (depends on error, page, buffer)
                    → database (depends on error, collection, wal, naming)
                      → engine (depends on collection, wal, concurrency)
                        → lib.rs (exposes engine, database, index)
```

## Implementation plan

See `docs/IMPLEMENTATION_PLAN.md` for the full phased plan.
See `docs/IMPLEMENTATION_PLAN_V2.md` for the v2 multi-tenant plan.
See `docs/ARCHITECTURE.md` for in-depth technical details.

## Available skills

| Skill | File | When to use |
|-------|------|-------------|
| Page Storage | `.claude/skills/page-storage.md` | Work on page manager, slotted pages, overflow |
| B+Tree Index | `.claude/skills/btree-index.md` | Work on B+Tree index |
| WAL & Recovery | `.claude/skills/wal-recovery.md` | Work on WAL, checkpoint, crash recovery |
| Buffer Pool | `.claude/skills/buffer-pool.md` | Work on LRU cache, dirty tracking |
| Document Model | `.claude/skills/document-model.md` | Work on document model, binary codec |
| Testing Strategy | `.claude/skills/testing-strategy.md` | Writing tests, test strategy |

## Available agents

| Agent | File | Mission |
|-------|------|---------|
| Page Agent | `.claude/agents/page-agent.md` | Develop the page system |
| B+Tree Agent | `.claude/agents/btree-agent.md` | Develop the B+Tree index |
| WAL Agent | `.claude/agents/wal-agent.md` | Develop WAL and recovery |
| Integration Agent | `.claude/agents/integration-agent.md` | Assemble modules and integration testing |
| Docs Agent | `.claude/agents/docs-agent.md` | Verify and update all documentation after each agent |
| Release Agent | `.claude/agents/release-agent.md` | Version bump, git commit/tag, crates.io package after each phase |

### Inter-agent workflow

After each execution of a development agent (page, btree, wal, integration), **always run the Docs Agent** to synchronize documentation with the code, then **run the Release Agent** at each phase completion to bump version and commit.
