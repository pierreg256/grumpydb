//! B+Tree index: maps UUID keys to (PageId, SlotId) locations in the data file.
//!
//! The B+Tree is stored in a separate file (`index.db`) with its own page management.
//! Page 0 holds metadata (root, height, entry count).

pub mod cursor;
pub mod node;
pub mod ops;

use std::path::Path;

use crate::error::Result;
use crate::page::manager::PageManager;
use crate::page::{PageHeader, PageType, PAGE_SIZE};

use self::node::LeafNode;

/// Metadata stored in page 0 of the index file.
///
/// Layout:
/// ```text
/// 0-31    PageHeader (type = BTreeInternal, repurposed)
/// 32-35   root_page_id: u32
/// 36-39   height: u32
/// 40-47   num_entries: u64
/// ```
#[derive(Debug, Clone, Copy)]
pub struct BTreeMeta {
    pub root_page_id: u32,
    pub height: u32,
    pub num_entries: u64,
}

/// A B+Tree index stored in a separate file.
///
/// Provides O(log n) search, insert, and delete by UUID key.
/// Values are `(page_id, slot_id)` pointers into the data file.
pub struct BTree {
    /// Page ID where B+Tree metadata is stored (always 1).
    meta_page_id: u32,
    pub(crate) pm: PageManager,
    pub(crate) meta: BTreeMeta,
}

impl BTree {
    /// The page ID used for B+Tree metadata (page 0 is the free-list).
    const META_PAGE_ID: u32 = 1;

    /// Creates a new B+Tree index file at the given path.
    ///
    /// Initializes page 0 as PageManager's free-list, page 1 for B+Tree metadata,
    /// and page 2 as the empty root leaf.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let mut pm = PageManager::new(path)?;

        // Allocate page 1 for metadata and page 2 for root leaf
        let meta_page_id = pm.allocate_page()?; // page 1
        let root_page_id = pm.allocate_page()?; // page 2

        // Write the empty root leaf
        let root = LeafNode::new(root_page_id);
        pm.write_page(root_page_id, &root.to_bytes())?;

        let meta = BTreeMeta {
            root_page_id,
            height: 1,
            num_entries: 0,
        };

        // Write metadata to page 1
        let mut meta_buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(meta_page_id, PageType::BTreeInternal);
        header.write_to(&mut meta_buf);
        Self::write_meta_to_buf(&mut meta_buf, &meta);
        pm.write_page(meta_page_id, &meta_buf)?;
        pm.sync()?;

        Ok(Self {
            meta_page_id,
            pm,
            meta,
        })
    }

    /// Opens an existing B+Tree index file.
    ///
    /// Reads metadata from page 1.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut pm = PageManager::new(path)?;
        let meta_buf = pm.read_page(Self::META_PAGE_ID)?;
        let meta = Self::read_meta_from_buf(&meta_buf);
        Ok(Self {
            meta_page_id: Self::META_PAGE_ID,
            pm,
            meta,
        })
    }

    /// Persists the current metadata to the metadata page.
    pub(crate) fn flush_meta(&mut self) -> Result<()> {
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(self.meta_page_id, PageType::BTreeInternal);
        header.write_to(&mut buf);
        Self::write_meta_to_buf(&mut buf, &self.meta);
        self.pm.write_page(self.meta_page_id, &buf)
    }

    /// Syncs all data to disk.
    pub fn sync(&self) -> Result<()> {
        self.pm.sync()
    }

    /// Returns the number of entries in the index.
    pub fn len(&self) -> u64 {
        self.meta.num_entries
    }

    /// Returns true if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.meta.num_entries == 0
    }

    /// Returns the height of the tree.
    pub fn height(&self) -> u32 {
        self.meta.height
    }

    fn write_meta_to_buf(buf: &mut [u8; PAGE_SIZE], meta: &BTreeMeta) {
        buf[32..36].copy_from_slice(&meta.root_page_id.to_le_bytes());
        buf[36..40].copy_from_slice(&meta.height.to_le_bytes());
        buf[40..48].copy_from_slice(&meta.num_entries.to_le_bytes());
    }

    fn read_meta_from_buf(buf: &[u8; PAGE_SIZE]) -> BTreeMeta {
        BTreeMeta {
            root_page_id: u32::from_le_bytes(buf[32..36].try_into().unwrap()),
            height: u32::from_le_bytes(buf[36..40].try_into().unwrap()),
            num_entries: u64::from_le_bytes(buf[40..48].try_into().unwrap()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_btree_create_and_open() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index.db");

        {
            let btree = BTree::create(&path).unwrap();
            assert_eq!(btree.len(), 0);
            assert!(btree.is_empty());
            assert_eq!(btree.height(), 1);
            assert_eq!(btree.meta.root_page_id, 2); // page 0 = free-list, page 1 = meta
        }

        {
            let btree = BTree::open(&path).unwrap();
            assert_eq!(btree.len(), 0);
            assert_eq!(btree.height(), 1);
            assert_eq!(btree.meta.root_page_id, 2);
        }
    }

    #[test]
    fn test_btree_meta_persistence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index.db");

        {
            let mut btree = BTree::create(&path).unwrap();
            btree.meta.num_entries = 42;
            btree.meta.height = 3;
            btree.meta.root_page_id = 7;
            btree.flush_meta().unwrap();
            btree.sync().unwrap();
        }

        {
            let btree = BTree::open(&path).unwrap();
            assert_eq!(btree.len(), 42);
            assert_eq!(btree.height(), 3);
            assert_eq!(btree.meta.root_page_id, 7);
        }
    }

    #[test]
    fn test_btree_initial_root_is_empty_leaf() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index.db");
        let mut btree = BTree::create(&path).unwrap();

        let buf = btree.pm.read_page(2).unwrap();
        let leaf = LeafNode::from_bytes(&buf);
        assert_eq!(leaf.num_entries, 0);
        assert_eq!(leaf.next_leaf, 0);
        assert_eq!(leaf.prev_leaf, 0);
    }
}
