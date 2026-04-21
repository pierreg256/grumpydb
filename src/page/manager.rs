//! Page Manager: handles page I/O and free-list management.
//!
//! The PageManager is responsible for allocating, reading, writing, and freeing
//! pages in the data file (`data.db`). Page 0 is reserved for the free-list.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{GrumpyError, Result};
use crate::page::{PageHeader, PageType, PAGE_HEADER_SIZE, PAGE_SIZE};

/// Manages page-level I/O for a single database file.
///
/// Page 0 is reserved for the free-list, which tracks deallocated pages
/// available for reuse. User pages start at page ID 1.
pub struct PageManager {
    file: File,
    /// Total number of pages in the file (including page 0).
    num_pages: u32,
}

impl PageManager {
    /// Opens or creates a page-managed file at the given path.
    ///
    /// If the file does not exist, it is created with page 0 initialized
    /// as the free-list page. If it exists, the page count is computed
    /// from the file size.
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let exists = path.exists();

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        if !exists || file.metadata()?.len() == 0 {
            // Initialize page 0 as the free-list page
            let mut buf = [0u8; PAGE_SIZE];
            let header = PageHeader::new(0, PageType::FreeList);
            header.write_to(&mut buf);
            // num_free = 0 at offset PAGE_HEADER_SIZE
            buf[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + 4]
                .copy_from_slice(&0u32.to_le_bytes());
            file.write_all(&buf)?;
            file.sync_all()?;
            Ok(Self {
                file,
                num_pages: 1,
            })
        } else {
            let file_len = file.metadata()?.len();
            let num_pages = (file_len / PAGE_SIZE as u64) as u32;
            Ok(Self { file, num_pages })
        }
    }

    /// Allocates a new page. Reuses a freed page if available, otherwise
    /// extends the file.
    ///
    /// Returns the page ID of the newly allocated page (always >= 1).
    pub fn allocate_page(&mut self) -> Result<u32> {
        // Try to pop from free-list first
        if let Some(page_id) = self.pop_free_list()? {
            return Ok(page_id);
        }

        // Extend the file with a new blank page
        let page_id = self.num_pages;
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(page_id, PageType::Free);
        header.write_to(&mut buf);

        self.file
            .seek(SeekFrom::Start(page_id as u64 * PAGE_SIZE as u64))?;
        self.file.write_all(&buf)?;
        self.num_pages += 1;
        Ok(page_id)
    }

    /// Reads a full page from disk.
    ///
    /// Returns the raw page bytes. Returns `PageNotFound` if the page ID
    /// is out of range.
    pub fn read_page(&mut self, page_id: u32) -> Result<[u8; PAGE_SIZE]> {
        if page_id >= self.num_pages {
            return Err(GrumpyError::PageNotFound(page_id));
        }
        let mut buf = [0u8; PAGE_SIZE];
        self.file
            .seek(SeekFrom::Start(page_id as u64 * PAGE_SIZE as u64))?;
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Writes a full page to disk.
    ///
    /// Returns `PageNotFound` if the page ID is out of range.
    pub fn write_page(&mut self, page_id: u32, data: &[u8; PAGE_SIZE]) -> Result<()> {
        if page_id >= self.num_pages {
            return Err(GrumpyError::PageNotFound(page_id));
        }
        self.file
            .seek(SeekFrom::Start(page_id as u64 * PAGE_SIZE as u64))?;
        self.file.write_all(data)?;
        Ok(())
    }

    /// Frees a page, adding it to the free-list for later reuse.
    ///
    /// The page content is zeroed and its type set to `Free`.
    pub fn free_page(&mut self, page_id: u32) -> Result<()> {
        if page_id == 0 {
            return Err(GrumpyError::PageNotFound(0)); // Cannot free page 0
        }
        if page_id >= self.num_pages {
            return Err(GrumpyError::PageNotFound(page_id));
        }

        // Zero out the freed page
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(page_id, PageType::Free);
        header.write_to(&mut buf);
        self.write_page(page_id, &buf)?;

        // Add to the free-list
        self.push_free_list(page_id)?;
        Ok(())
    }

    /// Returns the total number of pages in the file.
    pub fn num_pages(&self) -> u32 {
        self.num_pages
    }

    /// Syncs all pending writes to disk.
    pub fn sync(&self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    // ── Free-list helpers ──────────────────────────────────────────────

    /// Reads the free-list from page 0. Returns the list of free page IDs.
    fn read_free_list(&mut self) -> Result<Vec<u32>> {
        let buf = self.read_page(0)?;
        let num_free =
            u32::from_le_bytes(buf[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + 4].try_into().unwrap());
        let mut free_pages = Vec::with_capacity(num_free as usize);
        let start = PAGE_HEADER_SIZE + 4;
        for i in 0..num_free as usize {
            let offset = start + i * 4;
            if offset + 4 > PAGE_SIZE {
                break; // Safety: don't read past page boundary
            }
            let pid = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap());
            free_pages.push(pid);
        }
        Ok(free_pages)
    }

    /// Writes the free-list back to page 0.
    fn write_free_list(&mut self, free_pages: &[u32]) -> Result<()> {
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(0, PageType::FreeList);
        header.write_to(&mut buf);

        let num_free = free_pages.len() as u32;
        buf[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + 4]
            .copy_from_slice(&num_free.to_le_bytes());

        let start = PAGE_HEADER_SIZE + 4;
        for (i, &pid) in free_pages.iter().enumerate() {
            let offset = start + i * 4;
            if offset + 4 > PAGE_SIZE {
                break; // Safety: max capacity reached
            }
            buf[offset..offset + 4].copy_from_slice(&pid.to_le_bytes());
        }

        self.write_page(0, &buf)
    }

    /// Pops one page ID from the free-list. Returns `None` if empty.
    fn pop_free_list(&mut self) -> Result<Option<u32>> {
        let mut free_pages = self.read_free_list()?;
        if let Some(page_id) = free_pages.pop() {
            self.write_free_list(&free_pages)?;
            Ok(Some(page_id))
        } else {
            Ok(None)
        }
    }

    /// Pushes a page ID onto the free-list.
    fn push_free_list(&mut self, page_id: u32) -> Result<()> {
        let mut free_pages = self.read_free_list()?;
        free_pages.push(page_id);
        self.write_free_list(&free_pages)
    }

    /// Maximum number of page IDs that can be stored in the free-list page.
    #[allow(dead_code)]
    fn free_list_capacity() -> usize {
        // Usable bytes = PAGE_SIZE - PAGE_HEADER_SIZE - 4 (num_free field)
        (PAGE_SIZE - PAGE_HEADER_SIZE - 4) / 4
    }
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
    fn test_page_manager_new_creates_file() {
        let (dir, pm) = setup();
        assert!(dir.path().join("data.db").exists());
        assert_eq!(pm.num_pages(), 1); // page 0 (free-list)
    }

    #[test]
    fn test_page_manager_allocate_returns_sequential_ids() {
        let (_dir, mut pm) = setup();
        let p1 = pm.allocate_page().unwrap();
        let p2 = pm.allocate_page().unwrap();
        let p3 = pm.allocate_page().unwrap();
        assert_eq!(p1, 1);
        assert_eq!(p2, 2);
        assert_eq!(p3, 3);
        assert_eq!(pm.num_pages(), 4);
    }

    #[test]
    fn test_page_manager_read_write_round_trip() {
        let (_dir, mut pm) = setup();
        let page_id = pm.allocate_page().unwrap();

        let mut data = [0u8; PAGE_SIZE];
        data[100] = 0xAB;
        data[200] = 0xCD;
        data[PAGE_SIZE - 1] = 0xEF;
        pm.write_page(page_id, &data).unwrap();

        let read_back = pm.read_page(page_id).unwrap();
        assert_eq!(data, read_back);
    }

    #[test]
    fn test_page_manager_read_nonexistent() {
        let (_dir, mut pm) = setup();
        let result = pm.read_page(999);
        assert!(matches!(result, Err(GrumpyError::PageNotFound(999))));
    }

    #[test]
    fn test_page_manager_write_nonexistent() {
        let (_dir, mut pm) = setup();
        let data = [0u8; PAGE_SIZE];
        let result = pm.write_page(999, &data);
        assert!(matches!(result, Err(GrumpyError::PageNotFound(999))));
    }

    #[test]
    fn test_page_manager_free_and_realloc() {
        let (_dir, mut pm) = setup();
        let p1 = pm.allocate_page().unwrap();
        let p2 = pm.allocate_page().unwrap();
        assert_eq!(p1, 1);
        assert_eq!(p2, 2);

        // Free page 1
        pm.free_page(p1).unwrap();

        // Next allocation should reuse page 1
        let p3 = pm.allocate_page().unwrap();
        assert_eq!(p3, 1); // reused!

        // Next allocation should extend
        let p4 = pm.allocate_page().unwrap();
        assert_eq!(p4, 3);
    }

    #[test]
    fn test_page_manager_free_page_zero_fails() {
        let (_dir, mut pm) = setup();
        let result = pm.free_page(0);
        assert!(result.is_err());
    }

    #[test]
    fn test_page_manager_free_clears_page() {
        let (_dir, mut pm) = setup();
        let page_id = pm.allocate_page().unwrap();

        // Write some data
        let mut data = [0xAA; PAGE_SIZE];
        let header = PageHeader::new(page_id, PageType::Data);
        header.write_to(&mut data);
        pm.write_page(page_id, &data).unwrap();

        // Free the page
        pm.free_page(page_id).unwrap();

        // Read it back — should be zeroed (with Free header)
        let read_back = pm.read_page(page_id).unwrap();
        let hdr = PageHeader::read_from(&read_back);
        assert_eq!(hdr.page_type, PageType::Free);
        // Data area should be zero
        assert!(read_back[PAGE_HEADER_SIZE..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_page_manager_reopen_preserves_pages() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("data.db");

        // Write some pages
        {
            let mut pm = PageManager::new(&path).unwrap();
            let p1 = pm.allocate_page().unwrap();
            let mut data = [0u8; PAGE_SIZE];
            data[42] = 0x42;
            pm.write_page(p1, &data).unwrap();
            pm.sync().unwrap();
        }

        // Reopen and verify
        {
            let mut pm = PageManager::new(&path).unwrap();
            assert_eq!(pm.num_pages(), 2); // page 0 + page 1
            let data = pm.read_page(1).unwrap();
            assert_eq!(data[42], 0x42);
        }
    }

    #[test]
    fn test_page_manager_free_list_persists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("data.db");

        {
            let mut pm = PageManager::new(&path).unwrap();
            let p1 = pm.allocate_page().unwrap();
            let _p2 = pm.allocate_page().unwrap();
            pm.free_page(p1).unwrap();
            pm.sync().unwrap();
        }

        // Reopen — free-list should still contain page 1
        {
            let mut pm = PageManager::new(&path).unwrap();
            let reused = pm.allocate_page().unwrap();
            assert_eq!(reused, 1);
        }
    }

    #[test]
    fn test_page_manager_multiple_free_realloc() {
        let (_dir, mut pm) = setup();
        let p1 = pm.allocate_page().unwrap();
        let p2 = pm.allocate_page().unwrap();
        let p3 = pm.allocate_page().unwrap();

        pm.free_page(p1).unwrap();
        pm.free_page(p3).unwrap();

        // Should reuse in LIFO order (stack)
        let r1 = pm.allocate_page().unwrap();
        let r2 = pm.allocate_page().unwrap();
        assert_eq!(r1, 3); // last freed = first reused
        assert_eq!(r2, 1);
        // p2 was never freed
        let _ = p2;
    }

    #[test]
    fn test_page_manager_free_list_capacity() {
        let cap = PageManager::free_list_capacity();
        // (8192 - 32 - 4) / 4 = 2039
        assert_eq!(cap, 2039);
    }
}
