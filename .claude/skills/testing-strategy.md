# Skill: Testing Strategy

## When to use this skill

When writing any test in GrumpyDB — unit or integration.

## Core principles

### Test structure

```
src/
  page/
    mod.rs          ← #[cfg(test)] mod tests { ... }
    manager.rs      ← #[cfg(test)] mod tests { ... }
    slotted.rs      ← #[cfg(test)] mod tests { ... }
  btree/
    node.rs         ← #[cfg(test)] mod tests { ... }
    ops.rs          ← #[cfg(test)] mod tests { ... }
  ...
tests/
  crud_test.rs      ← cross-module integration tests
  btree_stress.rs   ← B+Tree stress tests
  crash_recovery.rs ← crash + recovery tests
  concurrency.rs    ← multi-threaded tests
```

### Naming conventions

```rust
#[test]
fn test_<module>_<expected_behavior>() { ... }

// Examples:
fn test_slotted_page_insert_returns_valid_slot_id() { }
fn test_btree_search_returns_none_for_missing_key() { }
fn test_wal_recovery_replays_committed_transactions() { }
fn test_buffer_pool_evicts_lru_when_full() { }
```

### Setup with TempDir

**ALWAYS** use `tempfile::TempDir` for tests with disk I/O:

```rust
use tempfile::TempDir;

fn setup_page_manager() -> (TempDir, PageManager) {
    let dir = TempDir::new().unwrap();
    let pm = PageManager::new(dir.path().join("data.db")).unwrap();
    (dir, pm)  // ⚠️ dir MUST be returned to outlive the test
}

#[test]
fn test_something() {
    let (_dir, mut pm) = setup_page_manager();
    // ... test using pm
    // dir is dropped here → automatic cleanup
}
```

⚠️ **Common pitfall**: if `TempDir` is dropped before the end of the test, the directory is deleted and I/O operations fail.

### Test categories by module

#### Page Manager
| Test | Verifies |
|------|----------|
| `test_allocate_page_returns_unique_ids` | Each alloc returns a different PageId |
| `test_read_write_round_trip` | Write a page → read back → identical data |
| `test_free_and_realloc` | Free a page → alloc → same PageId reused |
| `test_free_list_persists` | Free pages → close → reopen → free list intact |
| `test_read_nonexistent_page` | Read an invalid PageId → error |

#### Slotted Page
| Test | Verifies |
|------|----------|
| `test_insert_single_tuple` | Basic insertion + read |
| `test_insert_fills_page` | Fill the page → PageFull |
| `test_delete_creates_tombstone` | Delete → slot inaccessible |
| `test_compact_recovers_space` | Delete + compact → space recovered |
| `test_update_in_place` | Update same size → no reallocation |
| `test_update_larger_reinserts` | Larger update → delete + insert |

#### B+Tree
| Test | Verifies |
|------|----------|
| `test_insert_search_1000_random` | Bulk insert + verify each |
| `test_leaf_split` | Insert enough to trigger leaf split |
| `test_internal_split` | Insert enough to trigger internal split |
| `test_root_split` | Insert enough to create a new root |
| `test_delete_and_merge` | Delete enough to trigger merge |
| `test_range_scan_ordered` | Scan returns results in UUID order |
| `test_persist_and_reopen` | Close + reopen → all keys present |

#### WAL
| Test | Verifies |
|------|----------|
| `test_write_and_read_records` | Round-trip |
| `test_committed_tx_survives` | Recovery applies committed TXs |
| `test_uncommitted_tx_rolled_back` | Recovery reverts uncommitted TXs |
| `test_checkpoint_truncates_wal` | Post-checkpoint, WAL is smaller |
| `test_corrupted_record_handled` | Truncated record → detected, WAL truncated |

#### Buffer Pool
| Test | Verifies |
|------|----------|
| `test_cache_hit` | Second fetch → no disk read |
| `test_lru_eviction` | Pool full → oldest page evicted |
| `test_dirty_flush_on_eviction` | Dirty page → written before eviction |
| `test_pinned_not_evicted` | Pinned page → error if pool full |
| `test_flush_all` | All dirty pages written |

#### Concurrency
| Test | Verifies |
|------|----------|
| `test_concurrent_reads` | N threads read in parallel without error |
| `test_writer_blocks_readers` | Active writer → readers wait |
| `test_readers_dont_block_each_other` | Multiple simultaneous readers OK |
| `test_no_data_corruption` | Writer + readers → consistent data |

### Integration tests (in `tests/`)

```rust
// tests/crud_test.rs
use grumpydb::{GrumpyDb, Value};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn test_full_crud_lifecycle() {
    let dir = TempDir::new().unwrap();
    let db = GrumpyDb::open(dir.path()).unwrap();

    let key = Uuid::new_v4();
    let value = Value::Object(/* ... */);

    // Create
    db.insert(key, value.clone()).unwrap();

    // Read
    let got = db.get(&key).unwrap().unwrap();
    assert_eq!(got, value);

    // Update
    let new_value = Value::Integer(42);
    db.update(&key, new_value.clone()).unwrap();
    let got = db.get(&key).unwrap().unwrap();
    assert_eq!(got, new_value);

    // Delete
    db.delete(&key).unwrap();
    assert!(db.get(&key).unwrap().is_none());
}

#[test]
fn test_persistence_across_close_reopen() {
    let dir = TempDir::new().unwrap();
    let key = Uuid::new_v4();
    let value = Value::String("persistent".into());

    {
        let db = GrumpyDb::open(dir.path()).unwrap();
        db.insert(key, value.clone()).unwrap();
        db.close().unwrap();
    }

    {
        let db = GrumpyDb::open(dir.path()).unwrap();
        let got = db.get(&key).unwrap().unwrap();
        assert_eq!(got, value);
    }
}

#[test]
fn test_bulk_insert_and_scan() {
    let dir = TempDir::new().unwrap();
    let db = GrumpyDb::open(dir.path()).unwrap();

    let mut keys: Vec<Uuid> = (0..10_000).map(|_| Uuid::new_v4()).collect();

    for (i, key) in keys.iter().enumerate() {
        db.insert(*key, Value::Integer(i as i64)).unwrap();
    }

    // Verify all
    for (i, key) in keys.iter().enumerate() {
        let val = db.get(key).unwrap().unwrap();
        assert_eq!(val, Value::Integer(i as i64));
    }

    // Scan should return sorted
    keys.sort();
    let scanned = db.scan(..).unwrap();
    let scanned_keys: Vec<Uuid> = scanned.iter().map(|(k, _)| *k).collect();
    assert_eq!(scanned_keys, keys);
}
```

### Crash simulation tests

```rust
// tests/crash_recovery.rs

#[test]
fn test_crash_recovery_preserves_committed_data() {
    let dir = TempDir::new().unwrap();

    // Phase 1: write data normally
    {
        let db = GrumpyDb::open(dir.path()).unwrap();
        for i in 0..100 {
            db.insert(Uuid::new_v4(), Value::Integer(i)).unwrap();
        }
        db.flush().unwrap();
        // Simulate crash: drop without close
    }

    // Phase 2: reopen → automatic recovery
    {
        let db = GrumpyDb::open(dir.path()).unwrap();
        let all = db.scan(..).unwrap();
        assert_eq!(all.len(), 100);
    }
}
```

## Test commands

```bash
# All tests
cargo test

# Unit tests only
cargo test --lib

# Integration tests only
cargo test --test '*'

# A specific test
cargo test test_slotted_page_insert

# With output
cargo test -- --nocapture

# Tests for a module
cargo test page::

# Tests in parallel (default) or sequential
cargo test -- --test-threads=1
```

## Checklist before submitting code

- [ ] Every public function has at least one test
- [ ] Edge cases are tested (empty, full, overflow)
- [ ] Error cases are tested (I/O failure, missing key)
- [ ] `cargo test` passes at 100%
- [ ] `cargo clippy -- -D warnings` passes
- [ ] No `unwrap()` outside of tests
