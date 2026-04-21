# Skill: WAL & Crash Recovery

## When to use this skill

When working on:
- `src/wal/mod.rs` — WAL interface
- `src/wal/record.rs` — WAL record format
- `src/wal/writer.rs` — WAL writing
- `src/wal/recovery.rs` — crash recovery (redo/undo)
- WAL integration in `src/engine.rs`

## Core principles

### WAL Rule #1: Write-Ahead

**NEVER** write a modified page to disk BEFORE the corresponding WAL record is persisted (fsync).

Correct sequence:
1. Write the WAL record (before-image + after-image)
2. `fsync` the WAL file
3. Modify the page in memory (buffer pool)
4. The page will be flushed later (lazy)

### WAL Rule #2: Commit = Durability

A commit is only confirmed to the client AFTER the Commit record is `fsync`'d in the WAL.

### WAL record format

```rust
pub struct WalRecord {
    pub record_len: u32,    // total record size (to allow skipping)
    pub lsn: u64,           // Log Sequence Number, monotonically increasing
    pub tx_id: u64,         // transaction identifier
    pub op_type: WalOpType, // operation type
    pub page_id: u32,       // affected page (0 for Commit/Checkpoint)
    pub data: Vec<u8>,      // payload (before/after image, or empty)
    pub checksum: u32,      // CRC32 of the entire record except checksum
}

pub enum WalOpType {
    PageWrite = 1,  // page modification: data = before_image ++ after_image
    Commit = 2,     // transaction committed
    Rollback = 3,   // transaction rolled back
    Checkpoint = 4, // recovery point
}
```

### Binary layout on disk

```
For each record:
┌─────────────────────────────┐
│ record_len: u32 (LE)        │  4 bytes
│ lsn: u64 (LE)               │  8 bytes
│ tx_id: u64 (LE)             │  8 bytes
│ op_type: u8                  │  1 byte
│ page_id: u32 (LE)           │  4 bytes
│ data_len: u32 (LE)          │  4 bytes
│ data: [u8; data_len]        │  variable
│ checksum: u32 (LE)          │  4 bytes
└─────────────────────────────┘
record_len = 4 + 8 + 8 + 1 + 4 + 4 + data_len + 4 = 33 + data_len
```

### PageWrite data format

```
For a PageWrite:
data = before_image (PAGE_SIZE bytes) ++ after_image (PAGE_SIZE bytes)
data_len = PAGE_SIZE * 2 = 16384
```

Note: storing full images is simple but large. Future optimization possible with diffs.

## Recovery algorithm

### At DB startup

```
fn recover(wal_path, data_file, index_file):
    records = read_all_valid_records(wal_path)
    
    // Find the last checkpoint
    last_checkpoint_lsn = find_last_checkpoint(records)
    
    // Filter: keep only records after the last checkpoint
    active_records = records.filter(|r| r.lsn > last_checkpoint_lsn)
    
    // Identify committed transactions
    committed_txs = active_records
        .filter(|r| r.op_type == Commit)
        .map(|r| r.tx_id)
        .collect::<HashSet<_>>()
    
    // REDO phase: replay writes from committed TXs (in order)
    for record in active_records.iter().filter(|r| r.op_type == PageWrite):
        if committed_txs.contains(&record.tx_id):
            apply_after_image(record.page_id, &record.data[PAGE_SIZE..])
    
    // UNDO phase: revert writes from uncommitted TXs (in reverse order)
    for record in active_records.iter().rev().filter(|r| r.op_type == PageWrite):
        if !committed_txs.contains(&record.tx_id):
            apply_before_image(record.page_id, &record.data[..PAGE_SIZE])
    
    // Write a new checkpoint
    write_checkpoint()
    truncate_wal_before(new_checkpoint_lsn)
```

### Handling corrupted records

- Read records sequentially
- If a checksum doesn't match → truncate the WAL at that point
- Records after a corruption are considered lost
- Treat as a crash in the middle of a WAL write

## Integration with the Buffer Pool

```
// Write sequence
fn write_page_with_wal(tx_id, page_id, modification):
    frame = buffer_pool.fetch_page(page_id)
    before_image = frame.data.clone()
    
    // Apply the modification
    apply(frame.data, modification)
    
    after_image = frame.data.clone()
    
    // Write to the WAL BEFORE marking dirty
    wal.log_page_write(tx_id, page_id, &before_image, &after_image)
    
    // Mark the page as dirty in the buffer pool
    buffer_pool.unpin(page_id, dirty=true)
    
    // The page will be flushed at the next checkpoint or eviction

fn commit(tx_id):
    wal.log_commit(tx_id)  // includes fsync
    // Dirty pages remain in memory, will be flushed at checkpoint
```

## Checkpoint

```
fn checkpoint():
    // 1. Flush all dirty pages from the buffer pool
    buffer_pool.flush_all()
    
    // 2. Write the Checkpoint record
    lsn = wal.log_checkpoint()
    
    // 3. Fsync the data and index files
    data_file.sync_all()
    index_file.sync_all()
    
    // 4. Truncate the WAL (optional, to limit size)
    wal.truncate_before(lsn)
```

## Mandatory test patterns

```rust
#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    #[test]
    fn test_wal_record_round_trip() {
        // Write a record → read back → compare
    }

    #[test]
    fn test_wal_multiple_records() {
        // Write N records → read all → verify order and content
    }

    #[test]
    fn test_recovery_committed_tx() {
        // Write PageWrite + Commit → recovery → verify page = after_image
    }

    #[test]
    fn test_recovery_uncommitted_tx() {
        // Write PageWrite without Commit → recovery → verify page = before_image
    }

    #[test]
    fn test_recovery_mixed_txs() {
        // TX1: PageWrite + Commit
        // TX2: PageWrite (no commit)
        // Recovery → TX1 applied, TX2 undone
    }

    #[test]
    fn test_corrupted_record_truncation() {
        // Write 3 valid records + 1 corrupted
        // Recovery only reads the 3 valid ones
    }

    #[test]
    fn test_checkpoint_and_truncation() {
        // Write 100 records → checkpoint → verify WAL truncated
    }

    #[test]
    fn test_recovery_after_checkpoint() {
        // Records before checkpoint are ignored
        // Records after checkpoint are processed
    }
}
```

## Common mistakes to avoid

1. **Forgotten fsync**: the WAL MUST be fsync'd after each Commit. Without fsync, the kernel can buffer the writes
2. **REDO order**: records must be replayed in LSN ORDER
3. **UNDO order**: records must be reverted in REVERSE LSN order
4. **Checksum**: compute CRC32 on the record WITHOUT the checksum field (set to 0)
5. **Truncate at the right position**: truncate the WAL at the exact byte, not at the next record
6. **before_image**: capture the image BEFORE the modification, not after
7. **Page 0**: the metadata/free-list page must also be protected by the WAL
