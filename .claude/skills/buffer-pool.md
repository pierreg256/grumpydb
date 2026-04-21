# Skill: Buffer Pool

## When to use this skill

When working on:
- `src/buffer/mod.rs` — buffer pool interface
- `src/buffer/pool.rs` — LRU implementation, eviction
- `src/buffer/frame.rs` — BufferFrame, pin/unpin, dirty tracking

## Core principles

### Role of the Buffer Pool

The buffer pool is an **in-memory page cache** between the engine and disk:
- Avoids redundant I/O (a page read once stays in cache)
- Centralizes writes (dirty pages flushed in batch)
- Manages memory pressure via LRU eviction

### Architecture

```
Engine ──→ BufferPool ──→ PageManager (disk)
              │
              ├── page_table: HashMap<PageId, FrameId>
              ├── frames: Vec<BufferFrame>
              ├── free_list: VecDeque<FrameId>
              └── clock/lru for eviction
```

### BufferFrame

```rust
pub struct BufferFrame {
    pub data: [u8; PAGE_SIZE],    // page content
    pub page_id: Option<PageId>,  // None if frame is free
    pub pin_count: u32,           // number of active references
    pub dirty: bool,              // modified since load?
    pub last_accessed: u64,       // counter for LRU (not Instant, for testability)
}
```

### Critical invariants

1. **A page has at most ONE frame** in the pool (page_table bijection)
2. **pin_count > 0** → the frame CANNOT be evicted
3. **dirty** → the frame MUST be flushed before eviction
4. **All page access** goes through the buffer pool (never direct PageManager access)

### BufferPool API

```rust
impl BufferPool {
    /// Creates a pool with `capacity` frames
    pub fn new(capacity: usize, page_manager: PageManager) -> Self;

    /// Fetches a page. Loads it from disk if not present.
    /// Increments pin_count. The caller MUST call unpin afterwards.
    pub fn fetch_page(&mut self, page_id: PageId) -> Result<&mut BufferFrame>;

    /// Allocates a new page via the PageManager and loads it into the pool.
    pub fn new_page(&mut self) -> Result<(PageId, &mut BufferFrame)>;

    /// Decrements pin_count. Marks dirty if `dirty=true`.
    pub fn unpin(&mut self, page_id: PageId, dirty: bool) -> Result<()>;

    /// Flushes a dirty page to disk.
    pub fn flush_page(&mut self, page_id: PageId) -> Result<()>;

    /// Flushes all dirty pages.
    pub fn flush_all(&mut self) -> Result<()>;
}
```

### fetch_page algorithm

```
fn fetch_page(page_id):
    if page_id in page_table:
        frame_id = page_table[page_id]
        frames[frame_id].pin_count += 1
        frames[frame_id].last_accessed = now()
        return &mut frames[frame_id]
    
    // Page not in cache → find a free frame
    frame_id = find_free_frame()?  // free_list or eviction
    
    // Load the page from disk
    data = page_manager.read_page(page_id)?
    
    frames[frame_id].data = data
    frames[frame_id].page_id = Some(page_id)
    frames[frame_id].pin_count = 1
    frames[frame_id].dirty = false
    frames[frame_id].last_accessed = now()
    
    page_table.insert(page_id, frame_id)
    return &mut frames[frame_id]
```

### LRU eviction algorithm

```
fn find_free_frame() -> Result<FrameId>:
    // 1. Look in the free_list
    if let Some(frame_id) = free_list.pop_front():
        return Ok(frame_id)
    
    // 2. Eviction: find the unpinned frame with the oldest last_accessed
    candidate = frames.iter()
        .filter(|f| f.pin_count == 0)
        .min_by_key(|f| f.last_accessed)
    
    if candidate.is_none():
        return Err(BufferPoolExhausted)
    
    let frame = candidate.unwrap()
    
    // 3. If dirty, flush before eviction
    if frame.dirty:
        page_manager.write_page(frame.page_id.unwrap(), &frame.data)?
        frame.dirty = false
    
    // 4. Remove from page_table
    page_table.remove(&frame.page_id.unwrap())
    frame.page_id = None
    
    return Ok(frame.id)
```

## Interaction with the WAL

**Critical rule**: before flushing a dirty page, verify that the WAL is persisted up to the page's LSN.

```
fn flush_page(page_id):
    frame = frames[page_table[page_id]]
    if frame.dirty:
        // Ensure the WAL is persisted at least up to this page's LSN
        wal.ensure_flushed_to(frame.lsn)?
        page_manager.write_page(page_id, &frame.data)?
        frame.dirty = false
```

## Mandatory test patterns

```rust
#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    fn setup(capacity: usize) -> (TempDir, BufferPool) {
        let dir = TempDir::new().unwrap();
        let pm = PageManager::new(dir.path().join("data.db")).unwrap();
        let pool = BufferPool::new(capacity, pm);
        (dir, pool)
    }

    #[test]
    fn test_fetch_and_unpin() {
        // new_page → write data → unpin(dirty) → fetch_page → verify data
    }

    #[test]
    fn test_cache_hit() {
        // fetch_page → unpin → fetch_page → verify pin_count is correct
        // Verify there was only one disk read (I/O counter)
    }

    #[test]
    fn test_eviction_lru() {
        // Pool with capacity 3
        // Load pages A, B, C → unpin all
        // Load page D → A should be evicted (oldest)
        // fetch A → should reload from disk
    }

    #[test]
    fn test_eviction_dirty_flush() {
        // Load page → modify → unpin(dirty=true)
        // Force eviction → verify the page was written to disk
    }

    #[test]
    fn test_pinned_page_not_evicted() {
        // Pool with capacity 2
        // Load A (pinned), load B (pinned)
        // Load C → BufferPoolExhausted error
    }

    #[test]
    fn test_flush_all() {
        // Modify 3 pages → flush_all → verify all written to disk
    }

    #[test]
    fn test_new_page() {
        // new_page → verify valid page_id, frame is pinned
    }
}
```

## Common mistakes to avoid

1. **Forgetting unpin**: each `fetch_page` MUST have a corresponding `unpin`. Use an RAII guard if possible.
2. **Double-fetch without unpin**: if you fetch the same page 2 times, pin_count = 2, you need 2 unpins
3. **Evicting a pinned page**: NEVER. Check pin_count == 0 before eviction
4. **Race condition**: in SWMR mode, the buffer pool must be protected by a lock (or use internal locks)
5. **Flush before WAL**: NEVER flush a dirty page if the WAL is not persisted to the corresponding LSN
6. **LRU counter**: use a monotonic counter, not `Instant::now()`, for test reproducibility
