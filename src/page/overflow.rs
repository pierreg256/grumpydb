//! Overflow pages: chained pages for documents larger than a single page.
//!
//! When a tuple is too large to fit in a slotted page, it is split across
//! a chain of overflow pages linked via `next_page_id` in the page header.

use crate::error::Result;
use crate::page::manager::PageManager;
use crate::page::{PageHeader, PageType, OVERFLOW_MARKER, PAGE_HEADER_SIZE, PAGE_SIZE, PAGE_USABLE_SPACE};

/// Size of the overflow reference stored in the main slotted page.
/// `OVERFLOW_MARKER (1) + first_overflow_page_id (4) + total_data_len (4) = 9 bytes`
pub const OVERFLOW_REF_SIZE: usize = 9;

/// Encodes an overflow reference to be stored in a slotted page slot.
///
/// The reference contains a marker byte, the first overflow page ID,
/// and the total data length.
pub fn encode_overflow_ref(first_page_id: u32, total_len: u32) -> [u8; OVERFLOW_REF_SIZE] {
    let mut buf = [0u8; OVERFLOW_REF_SIZE];
    buf[0] = OVERFLOW_MARKER;
    buf[1..5].copy_from_slice(&first_page_id.to_le_bytes());
    buf[5..9].copy_from_slice(&total_len.to_le_bytes());
    buf
}

/// Decodes an overflow reference from a slotted page slot.
///
/// Returns `(first_page_id, total_data_len)` or `None` if not an overflow ref.
pub fn decode_overflow_ref(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < OVERFLOW_REF_SIZE || data[0] != OVERFLOW_MARKER {
        return None;
    }
    let page_id = u32::from_le_bytes(data[1..5].try_into().unwrap());
    let total_len = u32::from_le_bytes(data[5..9].try_into().unwrap());
    Some((page_id, total_len))
}

/// Returns `true` if the given slot data is an overflow reference.
pub fn is_overflow(data: &[u8]) -> bool {
    !data.is_empty() && data[0] == OVERFLOW_MARKER && data.len() == OVERFLOW_REF_SIZE
}

/// Writes data across a chain of overflow pages.
///
/// Returns the page ID of the first overflow page in the chain.
pub fn write_overflow(pm: &mut PageManager, data: &[u8]) -> Result<u32> {
    let chunks = data.chunks(PAGE_USABLE_SPACE);
    let chunk_count = chunks.len();
    let chunks: Vec<&[u8]> = data.chunks(PAGE_USABLE_SPACE).collect();

    // Allocate all pages first
    let mut page_ids = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
        page_ids.push(pm.allocate_page()?);
    }

    // Write each page with its chunk and link to the next
    for (i, (&pid, chunk)) in page_ids.iter().zip(chunks.iter()).enumerate() {
        let mut buf = [0u8; PAGE_SIZE];
        let mut header = PageHeader::new(pid, PageType::Overflow);
        // Link to the next page in the chain (0 if last)
        header.next_page_id = if i + 1 < page_ids.len() {
            page_ids[i + 1]
        } else {
            0
        };
        // Store the chunk length in num_slots (repurposed field)
        header.num_slots = chunk.len() as u16;
        header.write_to(&mut buf);

        buf[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + chunk.len()].copy_from_slice(chunk);
        pm.write_page(pid, &buf)?;
    }

    Ok(page_ids[0])
}

/// Reads data from a chain of overflow pages.
///
/// Starting from `first_page_id`, follows the chain and reconstructs the data.
pub fn read_overflow(pm: &mut PageManager, first_page_id: u32) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut current_id = first_page_id;

    while current_id != 0 {
        let buf = pm.read_page(current_id)?;
        let header = PageHeader::read_from(&buf);
        let chunk_len = header.num_slots as usize;
        result.extend_from_slice(&buf[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + chunk_len]);
        current_id = header.next_page_id;
    }

    Ok(result)
}

/// Frees all pages in an overflow chain.
///
/// Starting from `first_page_id`, follows the chain and frees each page.
pub fn free_overflow(pm: &mut PageManager, first_page_id: u32) -> Result<()> {
    let mut current_id = first_page_id;

    while current_id != 0 {
        let buf = pm.read_page(current_id)?;
        let header = PageHeader::read_from(&buf);
        let next = header.next_page_id;
        pm.free_page(current_id)?;
        current_id = next;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, PageManager) {
        let dir = TempDir::new().unwrap();
        let pm = PageManager::new(dir.path().join("data.db")).unwrap();
        (dir, pm)
    }

    #[test]
    fn test_overflow_ref_encode_decode() {
        let encoded = encode_overflow_ref(42, 99999);
        assert!(is_overflow(&encoded));
        let (pid, len) = decode_overflow_ref(&encoded).unwrap();
        assert_eq!(pid, 42);
        assert_eq!(len, 99999);
    }

    #[test]
    fn test_is_overflow_false_for_normal_data() {
        assert!(!is_overflow(b"hello"));
        assert!(!is_overflow(&[]));
        assert!(!is_overflow(&[0x00; 9]));
    }

    #[test]
    fn test_overflow_single_page_round_trip() {
        let (_dir, mut pm) = setup();
        let data = vec![0xAB; 1000]; // fits in one overflow page

        let first_id = write_overflow(&mut pm, &data).unwrap();
        let read_back = read_overflow(&mut pm, first_id).unwrap();
        assert_eq!(data, read_back);
    }

    #[test]
    fn test_overflow_multi_page_round_trip() {
        let (_dir, mut pm) = setup();
        // Create data that spans 3 overflow pages
        let data: Vec<u8> = (0..PAGE_USABLE_SPACE * 3 - 100)
            .map(|i| (i % 256) as u8)
            .collect();

        let first_id = write_overflow(&mut pm, &data).unwrap();
        let read_back = read_overflow(&mut pm, first_id).unwrap();
        assert_eq!(data, read_back);
    }

    #[test]
    fn test_overflow_exact_page_boundary() {
        let (_dir, mut pm) = setup();
        // Data that fills exactly 2 overflow pages
        let data = vec![0x42; PAGE_USABLE_SPACE * 2];

        let first_id = write_overflow(&mut pm, &data).unwrap();
        let read_back = read_overflow(&mut pm, first_id).unwrap();
        assert_eq!(data, read_back);
    }

    #[test]
    fn test_overflow_free_chain() {
        let (_dir, mut pm) = setup();
        let data = vec![0xCD; PAGE_USABLE_SPACE * 3];

        let pages_before = pm.num_pages();
        let first_id = write_overflow(&mut pm, &data).unwrap();
        let pages_after_write = pm.num_pages();
        assert_eq!(pages_after_write - pages_before, 3);

        // Free the chain
        free_overflow(&mut pm, first_id).unwrap();

        // Allocate 3 pages — should reuse the freed ones
        let r1 = pm.allocate_page().unwrap();
        let r2 = pm.allocate_page().unwrap();
        let r3 = pm.allocate_page().unwrap();
        // No new pages should have been added to the file
        assert_eq!(pm.num_pages(), pages_after_write);
        // Reused page IDs should be from the freed chain
        let mut reused = [r1, r2, r3];
        reused.sort();
        assert!(reused.iter().all(|&id| (first_id..first_id + 3).contains(&id)));
    }

    #[test]
    fn test_overflow_large_data() {
        let (_dir, mut pm) = setup();
        // ~50 KiB of data → should span ~7 overflow pages
        let data: Vec<u8> = (0..50_000).map(|i| (i % 251) as u8).collect();

        let first_id = write_overflow(&mut pm, &data).unwrap();
        let read_back = read_overflow(&mut pm, first_id).unwrap();
        assert_eq!(data.len(), read_back.len());
        assert_eq!(data, read_back);
    }

    #[test]
    fn test_overflow_small_data() {
        let (_dir, mut pm) = setup();
        let data = b"tiny";

        let first_id = write_overflow(&mut pm, data).unwrap();
        let read_back = read_overflow(&mut pm, first_id).unwrap();
        assert_eq!(data.as_slice(), read_back.as_slice());
    }
}
