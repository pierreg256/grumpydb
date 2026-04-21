# Skill: Page Storage

## When to use this skill

When working on:
- `src/page/mod.rs` — constants, types, PageHeader
- `src/page/manager.rs` — PageManager, disk I/O, free-list
- `src/page/slotted.rs` — SlottedPage, tuple insertion/deletion
- `src/page/overflow.rs` — overflow page chains

## Core principles

### Page layout (8192 bytes)

```
Bytes 0-31   : PageHeader (fixed, 32 bytes)
Bytes 32+    : Payload (depends on page_type)
```

### PageHeader — exact format

```rust
pub struct PageHeader {
    pub page_id: u32,           // offset 0,  4 bytes
    pub page_type: PageType,    // offset 4,  1 byte
    pub flags: u8,              // offset 5,  1 byte
    pub num_slots: u16,         // offset 6,  2 bytes
    pub free_space_start: u16,  // offset 8,  2 bytes
    pub free_space_end: u16,    // offset 10, 2 bytes
    pub next_page_id: u32,      // offset 12, 4 bytes
    pub prev_page_id: u32,      // offset 16, 4 bytes
    pub lsn: u64,               // offset 20, 8 bytes
    pub checksum: u32,          // offset 28, 4 bytes
}
```

Always use little-endian for serialization. Use `u32::from_le_bytes` / `u32::to_le_bytes`.

### Slotted Page — critical rules

1. **Slot array**: starts at offset 32 (after the header), grows downward
2. **Tuple data**: starts at the end of the page, grows upward
3. **Free space**: between `free_space_start` and `free_space_end`
4. **Slot format**: `offset: u16 + length: u16 = 4 bytes`
5. **Tombstone**: a slot with `offset = 0` is deleted
6. **Compaction**: reorganizes tuples to reclaim fragmented space

```
free_space_start = PAGE_HEADER_SIZE + (num_slots * SLOT_SIZE)
free_space_end   = offset of the last inserted tuple
free_space       = free_space_end - free_space_start
```

### Insertion conditions

```rust
fn can_insert(&self, data_len: usize) -> bool {
    let needed = data_len + SLOT_SIZE; // 4 bytes for the slot
    self.free_space() >= needed
}
```

### Overflow — when to use it

- If `encoded_size > PAGE_SIZE - PAGE_HEADER_SIZE - SLOT_SIZE` → overflow
- The main tuple contains: `OVERFLOW_MARKER (1 byte) + overflow_page_id (4 bytes) + total_len (4 bytes)` = 9 bytes (`OVERFLOW_REF_SIZE`)
- Each overflow page: header + payload (up to 8160 bytes)
- Chained via `next_page_id` in the header
- The chunk length is stored in `num_slots` of the header (repurposed field)
- Implemented functions: `write_overflow()`, `read_overflow()`, `free_overflow()`
- Helpers: `encode_overflow_ref()`, `decode_overflow_ref()`, `is_overflow()`

### Free-list

- Page 0 is reserved for the free-list (type `FreeList`)
- Format: `num_free: u32` (offset 32) + `[page_id: u32, ...]` (offset 36+)
- Max capacity: 2039 page IDs per page
- On allocation: pop from the free-list (LIFO), otherwise extend the file
- On deallocation: push onto the free-list
- Page 0 cannot be freed

## Mandatory test patterns

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Always use TempDir for tests with I/O
    fn setup() -> (TempDir, PageManager) {
        let dir = TempDir::new().unwrap();
        let pm = PageManager::new(dir.path().join("data.db")).unwrap();
        (dir, pm)  // dir must outlive the test
    }

    #[test]
    fn test_page_header_round_trip() { /* serialize → deserialize → compare */ }

    #[test]
    fn test_slotted_insert_and_get() { /* insert → get → compare data */ }

    #[test]
    fn test_slotted_page_full() { /* fill the page → verify PageFull error */ }

    #[test]
    fn test_slotted_delete_and_compact() { /* insert 3 → delete middle → compact → verify */ }

    #[test]
    fn test_overflow_round_trip() { /* data > PAGE_SIZE → write → read → compare */ }

    #[test]
    fn test_free_list_reuse() { /* alloc → free → alloc → verify same page_id */ }
}
```

## Common mistakes to avoid

1. **Off-by-one** in slot offset calculations: always verify that `free_space_start + SLOT_SIZE ≤ free_space_end - data_len`
2. **Forgetting to update num_slots** after insertion
3. **Not recalculating free_space_start** after compaction
4. **Checksum**: compute AFTER writing all content, BEFORE serializing the checksum itself (set checksum to 0, compute CRC32, then write the checksum)
5. **Endianness**: ALWAYS little-endian, never use direct casts
