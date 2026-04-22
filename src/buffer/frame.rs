//! Buffer frame: a single cached page with pin counting and dirty tracking.
//!
//! Each frame holds one page's data in memory along with metadata for
//! the LRU eviction policy (pin count, dirty flag, access counter).

use crate::page::PAGE_SIZE;

/// A single frame in the buffer pool, holding one cached page.
///
/// Invariants:
/// - `pin_count > 0` → frame cannot be evicted
/// - `dirty == true` → frame must be flushed before eviction
/// - `page_id == None` → frame is free (available for loading a new page)
#[derive(Debug)]
pub struct BufferFrame {
    /// Raw page data (8 KiB).
    pub data: [u8; PAGE_SIZE],
    /// Which page is loaded in this frame (`None` = free frame).
    pub page_id: Option<u32>,
    /// Number of active references. Must reach 0 before eviction.
    pub pin_count: u32,
    /// Whether the page has been modified since it was loaded.
    pub dirty: bool,
    /// Monotonic counter for LRU ordering (higher = more recently accessed).
    pub last_accessed: u64,
}

impl BufferFrame {
    /// Creates a new empty (free) frame.
    pub fn new() -> Self {
        Self {
            data: [0u8; PAGE_SIZE],
            page_id: None,
            pin_count: 0,
            dirty: false,
            last_accessed: 0,
        }
    }

    /// Pins the frame (increments reference count).
    pub fn pin(&mut self) {
        self.pin_count += 1;
    }

    /// Unpins the frame (decrements reference count). Optionally marks dirty.
    pub fn unpin(&mut self, dirty: bool) {
        debug_assert!(self.pin_count > 0, "unpin called on unpinned frame");
        self.pin_count -= 1;
        if dirty {
            self.dirty = true;
        }
    }

    /// Returns true if the frame is pinned (cannot be evicted).
    pub fn is_pinned(&self) -> bool {
        self.pin_count > 0
    }

    /// Returns true if the frame is free (no page loaded).
    pub fn is_free(&self) -> bool {
        self.page_id.is_none()
    }

    /// Resets the frame to a free state.
    pub fn reset(&mut self) {
        self.page_id = None;
        self.pin_count = 0;
        self.dirty = false;
        self.last_accessed = 0;
    }
}

impl Default for BufferFrame {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_new_is_free() {
        let frame = BufferFrame::new();
        assert!(frame.is_free());
        assert!(!frame.is_pinned());
        assert!(!frame.dirty);
    }

    #[test]
    fn test_frame_pin_unpin() {
        let mut frame = BufferFrame::new();
        frame.pin();
        assert!(frame.is_pinned());
        assert_eq!(frame.pin_count, 1);

        frame.pin();
        assert_eq!(frame.pin_count, 2);

        frame.unpin(false);
        assert_eq!(frame.pin_count, 1);
        assert!(frame.is_pinned());

        frame.unpin(false);
        assert!(!frame.is_pinned());
    }

    #[test]
    fn test_frame_dirty_tracking() {
        let mut frame = BufferFrame::new();
        frame.pin();
        assert!(!frame.dirty);

        frame.unpin(true);
        assert!(frame.dirty);

        // Dirty flag persists even after re-pin
        frame.pin();
        frame.unpin(false);
        assert!(frame.dirty); // still dirty
    }

    #[test]
    fn test_frame_reset() {
        let mut frame = BufferFrame::new();
        frame.page_id = Some(42);
        frame.pin();
        frame.dirty = true;
        frame.last_accessed = 100;

        frame.reset();
        assert!(frame.is_free());
        assert!(!frame.is_pinned());
        assert!(!frame.dirty);
        assert_eq!(frame.last_accessed, 0);
    }
}
