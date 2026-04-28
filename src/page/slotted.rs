//! Slotted page: variable-length tuple storage within a fixed-size page.
//!
//! Uses a slot array growing from the header and tuple data growing from the
//! end of the page, with free space in between.

use crate::error::{GrumpyError, Result};
use crate::page::{PAGE_HEADER_SIZE, PAGE_SIZE, PageHeader, PageType, SLOT_SIZE};

/// A slotted page that stores variable-length tuples within a fixed 8 KiB buffer.
///
/// Layout:
/// ```text
/// ┌─────────────────────────────────────┐
/// │ PageHeader (32 bytes)               │
/// ├─────────────────────────────────────┤
/// │ Slot array [slot_0, ..., slot_n]    │  ← grows downward
/// ├─────────────────────────────────────┤
/// │         Free space                  │
/// ├─────────────────────────────────────┤
/// │ Tuple data [tuple_n, ..., tuple_0]  │  ← grows upward (from end)
/// └─────────────────────────────────────┘
/// ```
pub struct SlottedPage {
    pub data: [u8; PAGE_SIZE],
}

impl SlottedPage {
    /// Creates a new empty slotted page with the given page ID.
    pub fn new(page_id: u32) -> Self {
        let mut data = [0u8; PAGE_SIZE];
        let header = PageHeader::new(page_id, PageType::Data);
        header.write_to(&mut data);
        Self { data }
    }

    /// Wraps an existing page buffer as a slotted page.
    pub fn from_bytes(data: [u8; PAGE_SIZE]) -> Self {
        Self { data }
    }

    /// Returns the page header.
    pub fn header(&self) -> PageHeader {
        PageHeader::read_from(&self.data)
    }

    /// Returns the number of slots (including tombstones).
    pub fn num_slots(&self) -> u16 {
        u16::from_le_bytes([self.data[6], self.data[7]])
    }

    /// Returns the current free space start offset.
    fn free_space_start(&self) -> u16 {
        u16::from_le_bytes([self.data[8], self.data[9]])
    }

    /// Returns the current free space end offset.
    fn free_space_end(&self) -> u16 {
        u16::from_le_bytes([self.data[10], self.data[11]])
    }

    /// Sets the number of slots in the header.
    fn set_num_slots(&mut self, n: u16) {
        self.data[6..8].copy_from_slice(&n.to_le_bytes());
    }

    /// Sets the free space start offset.
    fn set_free_space_start(&mut self, offset: u16) {
        self.data[8..10].copy_from_slice(&offset.to_le_bytes());
    }

    /// Sets the free space end offset.
    fn set_free_space_end(&mut self, offset: u16) {
        self.data[10..12].copy_from_slice(&offset.to_le_bytes());
    }

    /// Returns the usable free space in bytes (for data + new slot entry).
    pub fn free_space(&self) -> usize {
        let start = self.free_space_start() as usize;
        let end = self.free_space_end() as usize;
        end.saturating_sub(start)
    }

    /// Returns the offset of a slot entry in the slot array.
    fn slot_offset(slot_index: u16) -> usize {
        PAGE_HEADER_SIZE + (slot_index as usize) * SLOT_SIZE
    }

    /// Reads a slot entry (offset, length) from the slot array.
    fn read_slot(&self, slot_index: u16) -> (u16, u16) {
        let base = Self::slot_offset(slot_index);
        let offset = u16::from_le_bytes([self.data[base], self.data[base + 1]]);
        let length = u16::from_le_bytes([self.data[base + 2], self.data[base + 3]]);
        (offset, length)
    }

    /// Writes a slot entry (offset, length) into the slot array.
    fn write_slot(&mut self, slot_index: u16, offset: u16, length: u16) {
        let base = Self::slot_offset(slot_index);
        self.data[base..base + 2].copy_from_slice(&offset.to_le_bytes());
        self.data[base + 2..base + 4].copy_from_slice(&length.to_le_bytes());
    }

    /// Inserts a tuple into the page.
    ///
    /// Returns the slot index of the inserted tuple.
    /// Returns `PageFull` if there is not enough space.
    pub fn insert(&mut self, tuple_data: &[u8]) -> Result<u16> {
        let data_len = tuple_data.len();
        let needed = data_len + SLOT_SIZE;

        if self.free_space() < needed {
            let header = self.header();
            return Err(GrumpyError::PageFull(header.page_id));
        }

        // Check if there's a tombstone slot we can reuse
        let slot_index = self.find_tombstone_slot().unwrap_or_else(|| {
            let idx = self.num_slots();
            self.set_num_slots(idx + 1);
            self.set_free_space_start(Self::slot_offset(idx + 1) as u16);
            idx
        });

        // Allocate space from the end of the page
        let new_end = self.free_space_end() - data_len as u16;
        self.set_free_space_end(new_end);

        // Copy tuple data
        let offset = new_end as usize;
        self.data[offset..offset + data_len].copy_from_slice(tuple_data);

        // Write the slot entry
        self.write_slot(slot_index, new_end, data_len as u16);

        Ok(slot_index)
    }

    /// Finds the first tombstone slot (offset == 0, but slot exists).
    fn find_tombstone_slot(&self) -> Option<u16> {
        let num = self.num_slots();
        for i in 0..num {
            let (offset, _) = self.read_slot(i);
            if offset == 0 {
                return Some(i);
            }
        }
        None
    }

    /// Retrieves the tuple data at the given slot index.
    ///
    /// Returns an error if the slot is out of range or has been deleted.
    pub fn get(&self, slot_index: u16) -> Result<&[u8]> {
        if slot_index >= self.num_slots() {
            let header = self.header();
            return Err(GrumpyError::PageNotFound(header.page_id));
        }

        let (offset, length) = self.read_slot(slot_index);
        if offset == 0 {
            // Tombstone — slot has been deleted
            let header = self.header();
            return Err(GrumpyError::PageNotFound(header.page_id));
        }

        let start = offset as usize;
        let end = start + length as usize;
        Ok(&self.data[start..end])
    }

    /// Deletes the tuple at the given slot index by marking it as a tombstone.
    ///
    /// The space is not immediately reclaimed; call [`SlottedPage::compact`] to defragment.
    pub fn delete(&mut self, slot_index: u16) -> Result<()> {
        if slot_index >= self.num_slots() {
            let header = self.header();
            return Err(GrumpyError::PageNotFound(header.page_id));
        }

        let (offset, _) = self.read_slot(slot_index);
        if offset == 0 {
            let header = self.header();
            return Err(GrumpyError::PageNotFound(header.page_id));
        }

        // Mark as tombstone
        self.write_slot(slot_index, 0, 0);
        Ok(())
    }

    /// Updates the tuple at the given slot index.
    ///
    /// If the new data fits in the existing space, it is updated in-place.
    /// Otherwise, the old slot is deleted and a new tuple is inserted.
    pub fn update(&mut self, slot_index: u16, new_data: &[u8]) -> Result<u16> {
        if slot_index >= self.num_slots() {
            let header = self.header();
            return Err(GrumpyError::PageNotFound(header.page_id));
        }

        let (offset, length) = self.read_slot(slot_index);
        if offset == 0 {
            let header = self.header();
            return Err(GrumpyError::PageNotFound(header.page_id));
        }

        if new_data.len() <= length as usize {
            // In-place update: write new data at the same offset
            let start = offset as usize;
            self.data[start..start + new_data.len()].copy_from_slice(new_data);
            // Update length (might be shorter)
            self.write_slot(slot_index, offset, new_data.len() as u16);
            Ok(slot_index)
        } else {
            // Delete + re-insert
            self.delete(slot_index)?;
            self.insert(new_data)
        }
    }

    /// Compacts the page by removing gaps left by deleted tuples.
    ///
    /// After compaction, all live tuples are packed at the end of the page
    /// and the free space is contiguous.
    pub fn compact(&mut self) {
        let num_slots = self.num_slots();

        // Collect live tuples: (slot_index, data)
        let mut live_tuples: Vec<(u16, Vec<u8>)> = Vec::new();
        for i in 0..num_slots {
            let (offset, length) = self.read_slot(i);
            if offset != 0 {
                let start = offset as usize;
                let end = start + length as usize;
                live_tuples.push((i, self.data[start..end].to_vec()));
            }
        }

        // Clear the tuple data area
        let header_and_slots_end = Self::slot_offset(num_slots);
        self.data[header_and_slots_end..PAGE_SIZE].fill(0);

        // Re-pack tuples from the end of the page
        let mut write_end = PAGE_SIZE as u16;
        for (slot_index, tuple_data) in &live_tuples {
            write_end -= tuple_data.len() as u16;
            let start = write_end as usize;
            self.data[start..start + tuple_data.len()].copy_from_slice(tuple_data);
            self.write_slot(*slot_index, write_end, tuple_data.len() as u16);
        }

        self.set_free_space_end(write_end);
    }

    /// Returns the number of live (non-tombstone) tuples.
    pub fn live_tuple_count(&self) -> usize {
        let num = self.num_slots();
        let mut count = 0;
        for i in 0..num {
            let (offset, _) = self.read_slot(i);
            if offset != 0 {
                count += 1;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slotted_page_new() {
        let page = SlottedPage::new(1);
        let hdr = page.header();
        assert_eq!(hdr.page_id, 1);
        assert_eq!(hdr.page_type, PageType::Data);
        assert_eq!(page.num_slots(), 0);
        assert_eq!(page.free_space(), PAGE_SIZE - PAGE_HEADER_SIZE);
    }

    #[test]
    fn test_slotted_page_insert_and_get() {
        let mut page = SlottedPage::new(1);
        let data = b"hello, grumpydb!";
        let slot = page.insert(data).unwrap();
        assert_eq!(slot, 0);

        let retrieved = page.get(0).unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn test_slotted_page_insert_multiple() {
        let mut page = SlottedPage::new(1);
        let d0 = b"first";
        let d1 = b"second";
        let d2 = b"third";

        let s0 = page.insert(d0).unwrap();
        let s1 = page.insert(d1).unwrap();
        let s2 = page.insert(d2).unwrap();

        assert_eq!(s0, 0);
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(page.num_slots(), 3);

        assert_eq!(page.get(0).unwrap(), d0.as_slice());
        assert_eq!(page.get(1).unwrap(), d1.as_slice());
        assert_eq!(page.get(2).unwrap(), d2.as_slice());
    }

    #[test]
    fn test_slotted_page_full() {
        let mut page = SlottedPage::new(1);
        // Fill the page with large tuples until full
        let big_data = vec![0xAB; 1000];
        let mut count = 0;
        loop {
            match page.insert(&big_data) {
                Ok(_) => count += 1,
                Err(GrumpyError::PageFull(_)) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(count > 0);
        assert!(count < 10); // sanity: can't fit more than ~8 at 1000 bytes each
    }

    #[test]
    fn test_slotted_page_delete() {
        let mut page = SlottedPage::new(1);
        page.insert(b"keep").unwrap();
        page.insert(b"delete_me").unwrap();
        page.insert(b"also_keep").unwrap();

        page.delete(1).unwrap();

        assert_eq!(page.get(0).unwrap(), b"keep");
        assert!(page.get(1).is_err()); // deleted
        assert_eq!(page.get(2).unwrap(), b"also_keep");
        assert_eq!(page.live_tuple_count(), 2);
    }

    #[test]
    fn test_slotted_page_delete_nonexistent() {
        let mut page = SlottedPage::new(1);
        assert!(page.delete(0).is_err());
    }

    #[test]
    fn test_slotted_page_double_delete() {
        let mut page = SlottedPage::new(1);
        page.insert(b"data").unwrap();
        page.delete(0).unwrap();
        assert!(page.delete(0).is_err()); // already deleted
    }

    #[test]
    fn test_slotted_page_compact() {
        let mut page = SlottedPage::new(1);
        page.insert(b"aaa").unwrap();
        page.insert(b"bbb").unwrap();
        page.insert(b"ccc").unwrap();

        let free_before = page.free_space();
        page.delete(1).unwrap(); // delete "bbb"

        page.compact();

        // After compaction, free space should increase (recovered "bbb" data space)
        let free_after = page.free_space();
        assert!(free_after > free_before);

        // Live tuples should still be accessible
        assert_eq!(page.get(0).unwrap(), b"aaa");
        assert!(page.get(1).is_err()); // still a tombstone
        assert_eq!(page.get(2).unwrap(), b"ccc");
        assert_eq!(page.live_tuple_count(), 2);
    }

    #[test]
    fn test_slotted_page_update_in_place() {
        let mut page = SlottedPage::new(1);
        page.insert(b"hello world!!").unwrap();

        // Update with shorter data → in-place
        let new_slot = page.update(0, b"hi").unwrap();
        assert_eq!(new_slot, 0); // same slot
        assert_eq!(page.get(0).unwrap(), b"hi");
    }

    #[test]
    fn test_slotted_page_update_larger() {
        let mut page = SlottedPage::new(1);
        page.insert(b"hi").unwrap();

        // Update with larger data → delete + re-insert
        let new_slot = page.update(0, b"hello world, this is much longer").unwrap();
        // Original slot 0 is now a tombstone, new data is in a new slot
        // (or reuses slot 0 tombstone)
        let retrieved = page.get(new_slot).unwrap();
        assert_eq!(retrieved, b"hello world, this is much longer");
    }

    #[test]
    fn test_slotted_page_tombstone_reuse() {
        let mut page = SlottedPage::new(1);
        page.insert(b"first").unwrap(); // slot 0
        page.insert(b"second").unwrap(); // slot 1

        page.delete(0).unwrap(); // slot 0 → tombstone

        // Next insert should reuse slot 0
        let slot = page.insert(b"third").unwrap();
        assert_eq!(slot, 0);
        assert_eq!(page.get(0).unwrap(), b"third");
        assert_eq!(page.num_slots(), 2); // no new slot added
    }

    #[test]
    fn test_slotted_page_get_out_of_range() {
        let page = SlottedPage::new(1);
        assert!(page.get(0).is_err());
        assert!(page.get(100).is_err());
    }

    #[test]
    fn test_slotted_page_from_bytes_round_trip() {
        let mut page = SlottedPage::new(5);
        page.insert(b"test data").unwrap();

        let bytes = page.data;
        let restored = SlottedPage::from_bytes(bytes);
        assert_eq!(restored.get(0).unwrap(), b"test data");
        assert_eq!(restored.header().page_id, 5);
    }

    #[test]
    fn test_slotted_page_free_space_decreases() {
        let mut page = SlottedPage::new(1);
        let initial = page.free_space();

        page.insert(b"some data").unwrap();
        let after = page.free_space();

        // Should decrease by data_len + SLOT_SIZE
        assert_eq!(initial - after, 9 + SLOT_SIZE);
    }

    #[test]
    fn test_slotted_page_many_small_tuples() {
        let mut page = SlottedPage::new(1);
        let data = b"x";
        let mut count = 0;
        while page.insert(data).is_ok() {
            count += 1;
        }
        // Each tuple needs 1 (data) + 4 (slot) = 5 bytes
        // Available: 8160 bytes → ~1632 tuples
        assert!(count > 1000);
        assert!(count <= 1632);

        // Verify all accessible
        for i in 0..count {
            assert_eq!(page.get(i as u16).unwrap(), b"x");
        }
    }
}
