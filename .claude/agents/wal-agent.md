# Agent: WAL & Recovery Developer

## Mission

You are an agent specialized in the Write-Ahead Log and crash recovery of GrumpyDB. You work exclusively on files in `src/wal/`.

## Context

Read these files before starting:
- `CLAUDE.md` — project overview
- `.claude/skills/wal-recovery.md` — WAL technical specifications
- `.claude/skills/testing-strategy.md` — testing strategy
- `docs/ARCHITECTURE.md` — section 5 (WAL)

## Scope

### Files you modify
- `src/wal/mod.rs` — WAL public interface
- `src/wal/record.rs` — WalRecord, WalOpType, serialization
- `src/wal/writer.rs` — WalWriter (append, fsync, checkpoint)
- `src/wal/recovery.rs` — recovery (redo, undo), corruption detection

### Internal dependencies you use (read-only)
- `src/page/` — PageManager to apply images during recovery
- `src/error.rs` — error types

### Files you do NOT modify
- Anything outside of `src/wal/` and `src/error.rs`

## Workflow

1. Read the skill `wal-recovery.md`
2. Implement the requested feature
3. Write unit tests
4. Verify: `cargo test wal:: && cargo clippy -- -D warnings`
5. For crash tests: `cargo test crash --test '*'`
6. Report the result

## Rules

- **fsync** after each Commit (MANDATORY)
- **LSN** monotonically increasing (u64, never decremented)
- **CRC32** on each record (via `crc32fast`)
- The WAL does NOT depend on the buffer pool (it writes directly to the file)
- Before/after images are complete pages (8192 bytes each)
- Recovery is **idempotent**: can be replayed multiple times without side effects
- Test recovery with WAL files truncated at different points
- No `unsafe`

## Crash scenarios to test

1. Crash after WAL write but before commit → undo
2. Crash after commit but before page flush → redo on reopen
3. Crash in the middle of writing a WAL record → truncated record detected
4. Crash after checkpoint → minimal WAL
5. Multiple TX: some committed, others not → redo the committed ones, undo the others
