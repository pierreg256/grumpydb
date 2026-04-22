//! Buffer pool: LRU page cache with pin/unpin and dirty tracking.
//!
//! The buffer pool sits between the engine and the disk, caching frequently
//! accessed pages in memory to reduce I/O operations.

use std::collections::HashMap;

use crate::error::{GrumpyError, Result};
use crate::page::PAGE_SIZE;
use crate::page::manager::PageManager;

use super::frame::BufferFrame;

/// An LRU buffer pool that caches pages in memory.
///
/// Pages are loaded into fixed-size frames. When the pool is full,
/// the least recently accessed unpinned frame is evicted to make room.
pub struct BufferPool {
    /// All frames in the pool.
    frames: Vec<BufferFrame>,
    /// Mapping from page_id → frame index.
    page_table: HashMap<u32, usize>,
    /// The underlying page manager for disk I/O.
    pm: PageManager,
    /// Monotonic counter for LRU ordering.
    clock: u64,
    /// Number of disk reads (for performance monitoring).
    pub read_count: u64,
    /// Number of disk writes (for performance monitoring).
    pub write_count: u64,
}

impl BufferPool {
    /// Creates a new buffer pool with the given capacity (number of frames).
    ///
    /// # Arguments
    ///
    /// * `capacity` — Maximum number of pages cached in memory.
    /// * `pm` — The page manager for disk I/O.
    pub fn new(capacity: usize, pm: PageManager) -> Self {
        let frames = (0..capacity).map(|_| BufferFrame::new()).collect();
        Self {
            frames,
            page_table: HashMap::new(),
            pm,
            clock: 0,
            read_count: 0,
            write_count: 0,
        }
    }

    /// Fetches a page into the pool. If already cached, returns the cached frame.
    /// Otherwise, loads it from disk (evicting an LRU frame if needed).
    ///
    /// The frame is **pinned** — the caller MUST call `unpin()` when done.
    ///
    /// Returns the frame index.
    pub fn fetch_page(&mut self, page_id: u32) -> Result<usize> {
        // Cache hit: page is already in the pool
        if let Some(&frame_idx) = self.page_table.get(&page_id) {
            self.frames[frame_idx].pin();
            self.clock += 1;
            self.frames[frame_idx].last_accessed = self.clock;
            return Ok(frame_idx);
        }

        // Cache miss: load from disk
        let frame_idx = self.find_free_frame()?;
        let data = self.pm.read_page(page_id)?;
        self.read_count += 1;

        let frame = &mut self.frames[frame_idx];
        frame.data = data;
        frame.page_id = Some(page_id);
        frame.pin_count = 1;
        frame.dirty = false;
        self.clock += 1;
        frame.last_accessed = self.clock;

        self.page_table.insert(page_id, frame_idx);
        Ok(frame_idx)
    }

    /// Allocates a new page on disk and loads it into the pool (pinned).
    ///
    /// Returns `(page_id, frame_index)`.
    pub fn new_page(&mut self) -> Result<(u32, usize)> {
        let page_id = self.pm.allocate_page()?;
        let frame_idx = self.find_free_frame()?;

        let frame = &mut self.frames[frame_idx];
        frame.data = [0u8; PAGE_SIZE];
        frame.page_id = Some(page_id);
        frame.pin_count = 1;
        frame.dirty = true; // New page needs to be written
        self.clock += 1;
        frame.last_accessed = self.clock;

        self.page_table.insert(page_id, frame_idx);
        Ok((page_id, frame_idx))
    }

    /// Unpins a frame, optionally marking it dirty.
    pub fn unpin(&mut self, page_id: u32, dirty: bool) -> Result<()> {
        let &frame_idx = self
            .page_table
            .get(&page_id)
            .ok_or(GrumpyError::PageNotFound(page_id))?;
        self.frames[frame_idx].unpin(dirty);
        Ok(())
    }

    /// Flushes a specific dirty page to disk.
    pub fn flush_page(&mut self, page_id: u32) -> Result<()> {
        let &frame_idx = self
            .page_table
            .get(&page_id)
            .ok_or(GrumpyError::PageNotFound(page_id))?;
        let frame = &mut self.frames[frame_idx];
        if frame.dirty {
            self.pm.write_page(page_id, &frame.data)?;
            self.write_count += 1;
            frame.dirty = false;
        }
        Ok(())
    }

    /// Flushes all dirty pages to disk.
    pub fn flush_all(&mut self) -> Result<()> {
        let dirty_pages: Vec<u32> = self
            .page_table
            .iter()
            .filter(|(_, fidx)| self.frames[**fidx].dirty)
            .map(|(pid, _)| *pid)
            .collect();

        for pid in dirty_pages {
            self.flush_page(pid)?;
        }
        self.pm.sync()?;
        Ok(())
    }

    /// Returns a reference to a frame's data.
    pub fn get_frame(&self, frame_idx: usize) -> &BufferFrame {
        &self.frames[frame_idx]
    }

    /// Returns a mutable reference to a frame's data.
    pub fn get_frame_mut(&mut self, frame_idx: usize) -> &mut BufferFrame {
        &mut self.frames[frame_idx]
    }

    /// Provides direct access to the underlying PageManager.
    /// Used for operations that bypass the buffer pool (e.g., overflow pages).
    pub fn page_manager(&mut self) -> &mut PageManager {
        &mut self.pm
    }

    /// Syncs the underlying page manager to disk.
    pub fn sync(&self) -> Result<()> {
        self.pm.sync()
    }

    /// Returns the pool capacity.
    pub fn capacity(&self) -> usize {
        self.frames.len()
    }

    /// Returns the number of pages currently cached.
    pub fn cached_count(&self) -> usize {
        self.page_table.len()
    }

    // ── Internal ────────────────────────────────────────────────────────

    /// Finds a free frame: first checks for empty frames, then evicts LRU.
    fn find_free_frame(&mut self) -> Result<usize> {
        // 1. Look for a free frame
        for (i, frame) in self.frames.iter().enumerate() {
            if frame.is_free() {
                return Ok(i);
            }
        }

        // 2. Evict the LRU unpinned frame
        let victim = self
            .frames
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.is_pinned() && !f.is_free())
            .min_by_key(|(_, f)| f.last_accessed)
            .map(|(i, _)| i);

        let victim_idx = victim.ok_or(GrumpyError::BufferPoolExhausted)?;

        // Flush if dirty
        let frame = &self.frames[victim_idx];
        if frame.dirty {
            let pid = frame.page_id.unwrap();
            let data = frame.data;
            self.pm.write_page(pid, &data)?;
            self.write_count += 1;
        }

        // Remove from page table
        if let Some(pid) = self.frames[victim_idx].page_id {
            self.page_table.remove(&pid);
        }
        self.frames[victim_idx].reset();

        Ok(victim_idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup(capacity: usize) -> (TempDir, BufferPool) {
        let dir = TempDir::new().unwrap();
        let pm = PageManager::new(dir.path().join("data.db")).unwrap();
        let pool = BufferPool::new(capacity, pm);
        (dir, pool)
    }

    #[test]
    fn test_new_page_and_fetch() {
        let (_dir, mut pool) = setup(10);

        // Allocate a new page
        let (page_id, fidx) = pool.new_page().unwrap();
        assert!(page_id >= 1);

        // Write data to the frame
        pool.get_frame_mut(fidx).data[100] = 0xAB;
        pool.unpin(page_id, true).unwrap();

        // Fetch it back — should be a cache hit
        let fidx2 = pool.fetch_page(page_id).unwrap();
        assert_eq!(pool.get_frame(fidx2).data[100], 0xAB);
        assert_eq!(pool.read_count, 0); // no disk read — cache hit
        pool.unpin(page_id, false).unwrap();
    }

    #[test]
    fn test_cache_hit_no_disk_read() {
        let (_dir, mut pool) = setup(10);
        let (pid, _) = pool.new_page().unwrap();
        pool.unpin(pid, true).unwrap();

        let reads_before = pool.read_count;
        let _fidx = pool.fetch_page(pid).unwrap();
        assert_eq!(pool.read_count, reads_before); // cache hit
        pool.unpin(pid, false).unwrap();
    }

    #[test]
    fn test_eviction_lru() {
        // Pool with capacity 3
        let (_dir, mut pool) = setup(3);

        let (p1, _) = pool.new_page().unwrap();
        pool.unpin(p1, true).unwrap();
        let (p2, _) = pool.new_page().unwrap();
        pool.unpin(p2, true).unwrap();
        let (p3, _) = pool.new_page().unwrap();
        pool.unpin(p3, true).unwrap();

        assert_eq!(pool.cached_count(), 3);

        // Loading a 4th page should evict p1 (oldest)
        let (p4, _) = pool.new_page().unwrap();
        pool.unpin(p4, true).unwrap();

        assert_eq!(pool.cached_count(), 3); // still 3
        assert!(!pool.page_table.contains_key(&p1)); // p1 evicted
        assert!(pool.page_table.contains_key(&p4)); // p4 cached
    }

    #[test]
    fn test_eviction_dirty_flush() {
        let (_dir, mut pool) = setup(2);

        let (p1, fidx1) = pool.new_page().unwrap();
        pool.get_frame_mut(fidx1).data[50] = 0xFF;
        pool.unpin(p1, true).unwrap(); // dirty

        let (p2, _) = pool.new_page().unwrap();
        pool.unpin(p2, false).unwrap();

        // Evict p1 by loading p3 — p1 is dirty so it must be flushed
        let writes_before = pool.write_count;
        let (p3, _) = pool.new_page().unwrap();
        pool.unpin(p3, false).unwrap();

        assert!(
            pool.write_count > writes_before,
            "dirty page should have been flushed"
        );

        // Reload p1 from disk — should have the written data
        let fidx = pool.fetch_page(p1).unwrap();
        assert_eq!(pool.get_frame(fidx).data[50], 0xFF);
        pool.unpin(p1, false).unwrap();
    }

    #[test]
    fn test_pinned_not_evicted() {
        let (_dir, mut pool) = setup(2);

        let (p1, _) = pool.new_page().unwrap(); // pinned
        let (p2, _) = pool.new_page().unwrap(); // pinned

        // Both frames are pinned — no eviction possible
        let result = pool.new_page();
        assert!(
            matches!(result, Err(GrumpyError::BufferPoolExhausted)),
            "should fail when all frames are pinned"
        );

        // Unpin one — now eviction should work
        pool.unpin(p1, false).unwrap();
        let (p3, _) = pool.new_page().unwrap();
        pool.unpin(p3, false).unwrap();
        pool.unpin(p2, false).unwrap();
    }

    #[test]
    fn test_flush_all() {
        let (_dir, mut pool) = setup(10);

        let (p1, fidx1) = pool.new_page().unwrap();
        pool.get_frame_mut(fidx1).data[0] = 1;
        pool.unpin(p1, true).unwrap();

        let (p2, fidx2) = pool.new_page().unwrap();
        pool.get_frame_mut(fidx2).data[0] = 2;
        pool.unpin(p2, true).unwrap();

        pool.flush_all().unwrap();

        // Verify both pages are no longer dirty
        let f1 = pool.page_table[&p1];
        let f2 = pool.page_table[&p2];
        assert!(!pool.frames[f1].dirty);
        assert!(!pool.frames[f2].dirty);
    }

    #[test]
    fn test_flush_page_single() {
        let (_dir, mut pool) = setup(10);

        let (pid, fidx) = pool.new_page().unwrap();
        pool.get_frame_mut(fidx).data[42] = 0xCC;
        pool.unpin(pid, true).unwrap();

        assert!(pool.frames[pool.page_table[&pid]].dirty);
        pool.flush_page(pid).unwrap();
        assert!(!pool.frames[pool.page_table[&pid]].dirty);
    }

    #[test]
    fn test_multiple_pins() {
        let (_dir, mut pool) = setup(10);

        let (pid, _) = pool.new_page().unwrap(); // pin_count = 1
        let _fidx2 = pool.fetch_page(pid).unwrap(); // pin_count = 2

        let fidx = pool.page_table[&pid];
        assert_eq!(pool.frames[fidx].pin_count, 2);

        pool.unpin(pid, false).unwrap(); // 1
        pool.unpin(pid, false).unwrap(); // 0
        assert!(!pool.frames[fidx].is_pinned());
    }

    #[test]
    fn test_io_counters() {
        let (_dir, mut pool) = setup(2);

        assert_eq!(pool.read_count, 0);
        assert_eq!(pool.write_count, 0);

        let (p1, _) = pool.new_page().unwrap();
        pool.unpin(p1, true).unwrap();

        // Flush writes 1 page
        pool.flush_page(p1).unwrap();
        assert_eq!(pool.write_count, 1);

        // Evict p1 by filling the pool, then fetch p1 again — triggers a disk read
        let (p2, _) = pool.new_page().unwrap();
        pool.unpin(p2, false).unwrap();
        let (p3, _) = pool.new_page().unwrap();
        pool.unpin(p3, false).unwrap();

        // p1 was evicted (clean), now fetch it from disk
        let _fidx = pool.fetch_page(p1).unwrap();
        assert!(pool.read_count >= 1);
        pool.unpin(p1, false).unwrap();
    }
}
