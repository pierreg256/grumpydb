//! Page management: constants, types, and page I/O.
//!
//! Pages are the fundamental unit of storage in GrumpyDB.
//! Each page is [`PAGE_SIZE`] bytes (8 KiB) and has a fixed [`PageHeader`].

pub mod manager;
pub mod overflow;
pub mod slotted;

/// Page size in bytes (8 KiB).
pub const PAGE_SIZE: usize = 8192;

/// Size of the page header in bytes.
pub const PAGE_HEADER_SIZE: usize = 32;

/// Usable space per page after the header.
pub const PAGE_USABLE_SPACE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

/// Size of a slot entry in the slot array (offset: u16 + length: u16).
pub const SLOT_SIZE: usize = 4;

/// Marker byte indicating an overflow tuple.
pub const OVERFLOW_MARKER: u8 = 0xFF;

/// Invalid page ID sentinel (no page).
pub const INVALID_PAGE_ID: u32 = 0;

/// Unique identifier for a page on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId(pub u32);

/// Slot index within a slotted page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId(pub u16);

/// Type of a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    /// Free / uninitialized page.
    Free = 0,
    /// Data page with slotted layout.
    Data = 1,
    /// B+Tree internal node.
    BTreeInternal = 2,
    /// B+Tree leaf node.
    BTreeLeaf = 3,
    /// Overflow page for large tuples.
    Overflow = 4,
    /// Free-list metadata page.
    FreeList = 5,
}

impl PageType {
    /// Convert a raw byte to a PageType.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Free),
            1 => Some(Self::Data),
            2 => Some(Self::BTreeInternal),
            3 => Some(Self::BTreeLeaf),
            4 => Some(Self::Overflow),
            5 => Some(Self::FreeList),
            _ => None,
        }
    }
}

/// Header at the beginning of every page (32 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    pub page_id: u32,
    pub page_type: PageType,
    pub flags: u8,
    pub num_slots: u16,
    pub free_space_start: u16,
    pub free_space_end: u16,
    pub next_page_id: u32,
    pub prev_page_id: u32,
    pub lsn: u64,
    pub checksum: u32,
}

impl PageHeader {
    /// Create a new header with default values.
    pub fn new(page_id: u32, page_type: PageType) -> Self {
        Self {
            page_id,
            page_type,
            flags: 0,
            num_slots: 0,
            free_space_start: PAGE_HEADER_SIZE as u16,
            free_space_end: PAGE_SIZE as u16,
            next_page_id: 0,
            prev_page_id: 0,
            lsn: 0,
            checksum: 0,
        }
    }

    /// Serialize the header into the first 32 bytes of a page buffer.
    pub fn write_to(&self, buf: &mut [u8; PAGE_SIZE]) {
        buf[0..4].copy_from_slice(&self.page_id.to_le_bytes());
        buf[4] = self.page_type as u8;
        buf[5] = self.flags;
        buf[6..8].copy_from_slice(&self.num_slots.to_le_bytes());
        buf[8..10].copy_from_slice(&self.free_space_start.to_le_bytes());
        buf[10..12].copy_from_slice(&self.free_space_end.to_le_bytes());
        buf[12..16].copy_from_slice(&self.next_page_id.to_le_bytes());
        buf[16..20].copy_from_slice(&self.prev_page_id.to_le_bytes());
        buf[20..28].copy_from_slice(&self.lsn.to_le_bytes());
        buf[28..32].copy_from_slice(&self.checksum.to_le_bytes());
    }

    /// Deserialize a header from the first 32 bytes of a page buffer.
    pub fn read_from(buf: &[u8; PAGE_SIZE]) -> Self {
        Self {
            page_id: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            page_type: PageType::from_u8(buf[4]).unwrap_or(PageType::Free),
            flags: buf[5],
            num_slots: u16::from_le_bytes(buf[6..8].try_into().unwrap()),
            free_space_start: u16::from_le_bytes(buf[8..10].try_into().unwrap()),
            free_space_end: u16::from_le_bytes(buf[10..12].try_into().unwrap()),
            next_page_id: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            prev_page_id: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
            lsn: u64::from_le_bytes(buf[20..28].try_into().unwrap()),
            checksum: u32::from_le_bytes(buf[28..32].try_into().unwrap()),
        }
    }
}

/// Computes a CRC32 checksum over a page buffer (excluding the checksum field at bytes 28-31).
pub fn compute_checksum(buf: &[u8; PAGE_SIZE]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&buf[0..28]); // header up to checksum
    hasher.update(&buf[32..]); // everything after header
    hasher.finalize()
}

/// Writes the CRC32 checksum into the page buffer's checksum field (bytes 28-31).
pub fn stamp_checksum(buf: &mut [u8; PAGE_SIZE]) {
    let csum = compute_checksum(buf);
    buf[28..32].copy_from_slice(&csum.to_le_bytes());
}

/// Verifies the CRC32 checksum of a page buffer.
///
/// Returns `Ok(())` if valid, or `ChecksumMismatch` if corrupted.
/// Pages with a zero checksum (never stamped) are considered valid.
pub fn verify_checksum(buf: &[u8; PAGE_SIZE], page_id: u32) -> crate::error::Result<()> {
    let stored = u32::from_le_bytes(buf[28..32].try_into().unwrap());
    if stored == 0 {
        // Legacy page (never stamped) — skip verification
        return Ok(());
    }
    let computed = compute_checksum(buf);
    if stored != computed {
        return Err(crate::error::GrumpyError::ChecksumMismatch {
            page_id,
            expected: stored,
            actual: computed,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_header_round_trip() {
        let header = PageHeader {
            page_id: 42,
            page_type: PageType::Data,
            flags: 0x01,
            num_slots: 5,
            free_space_start: 52,
            free_space_end: 7000,
            next_page_id: 43,
            prev_page_id: 41,
            lsn: 123456789,
            checksum: 0xDEADBEEF,
        };

        let mut buf = [0u8; PAGE_SIZE];
        header.write_to(&mut buf);
        let restored = PageHeader::read_from(&buf);

        assert_eq!(header, restored);
    }

    #[test]
    fn test_page_header_defaults() {
        let header = PageHeader::new(1, PageType::Data);
        assert_eq!(header.page_id, 1);
        assert_eq!(header.page_type, PageType::Data);
        assert_eq!(header.num_slots, 0);
        assert_eq!(header.free_space_start, PAGE_HEADER_SIZE as u16);
        assert_eq!(header.free_space_end, PAGE_SIZE as u16);
    }

    #[test]
    fn test_page_type_from_u8() {
        assert_eq!(PageType::from_u8(0), Some(PageType::Free));
        assert_eq!(PageType::from_u8(1), Some(PageType::Data));
        assert_eq!(PageType::from_u8(4), Some(PageType::Overflow));
        assert_eq!(PageType::from_u8(99), None);
    }

    #[test]
    fn test_constants() {
        assert_eq!(PAGE_SIZE, 8192);
        assert_eq!(PAGE_HEADER_SIZE, 32);
        assert_eq!(PAGE_USABLE_SPACE, 8160);
        assert_eq!(SLOT_SIZE, 4);
    }

    #[test]
    fn test_checksum_round_trip() {
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(1, PageType::Data);
        header.write_to(&mut buf);
        buf[100] = 0xAB; // some data

        stamp_checksum(&mut buf);
        let stored = u32::from_le_bytes(buf[28..32].try_into().unwrap());
        assert_ne!(stored, 0);

        // Verify should pass
        assert!(verify_checksum(&buf, 1).is_ok());
    }

    #[test]
    fn test_checksum_detects_corruption() {
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(1, PageType::Data);
        header.write_to(&mut buf);
        stamp_checksum(&mut buf);

        // Corrupt a byte
        buf[100] ^= 0xFF;

        let result = verify_checksum(&buf, 1);
        assert!(matches!(
            result,
            Err(crate::error::GrumpyError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn test_checksum_zero_skips_verification() {
        // A page with checksum == 0 (legacy/never stamped) should pass
        let mut buf = [0u8; PAGE_SIZE];
        buf[28..32].copy_from_slice(&0u32.to_le_bytes());
        assert!(verify_checksum(&buf, 0).is_ok());
    }
}
