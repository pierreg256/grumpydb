//! Variable-key B+Tree: a B+Tree that supports variable-length byte keys.
//!
//! This is used for secondary indexes where keys are composite
//! (encoded field value + UUID). The fixed-key `BTree` in `mod.rs`
//! remains the primary index implementation for UUID keys.

use std::path::Path;

use crate::error::Result;
use crate::page::manager::PageManager;
use crate::page::{PAGE_SIZE, PageHeader, PageType};

use super::var_node::VarLeafNode;

/// Metadata stored in page 1 of the index file.
#[derive(Debug, Clone, Copy)]
pub struct VarBTreeMeta {
    pub root_page_id: u32,
    pub height: u32,
    pub num_entries: u64,
    pub max_key_size: u16,
}

/// A B+Tree index with variable-length byte keys.
///
/// Keys are arbitrary byte slices up to `max_key_size` bytes.
/// Used for secondary indexes where keys are composite
/// (e.g., `encoded_field_value + uuid`).
pub struct VarBTree {
    meta_page_id: u32,
    pub(crate) pm: PageManager,
    pub(crate) meta: VarBTreeMeta,
    pub(crate) max_key_size: u16,
}

impl VarBTree {
    const META_PAGE_ID: u32 = 1;

    /// Creates a new variable-key B+Tree index file.
    ///
    /// `max_key_size` determines the maximum key length and affects fan-out.
    pub fn create(path: impl AsRef<Path>, max_key_size: u16) -> Result<Self> {
        let mut pm = PageManager::new(path)?;

        let meta_page_id = pm.allocate_page()?; // page 1
        let root_page_id = pm.allocate_page()?; // page 2

        // Write empty root leaf
        let root = VarLeafNode::new(root_page_id, max_key_size);
        pm.write_page(root_page_id, &root.to_bytes())?;

        let meta = VarBTreeMeta {
            root_page_id,
            height: 1,
            num_entries: 0,
            max_key_size,
        };

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
            max_key_size,
        })
    }

    /// Opens an existing variable-key B+Tree index file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut pm = PageManager::new(path)?;
        let meta_buf = pm.read_page(Self::META_PAGE_ID)?;
        let meta = Self::read_meta_from_buf(&meta_buf);
        Ok(Self {
            meta_page_id: Self::META_PAGE_ID,
            pm,
            max_key_size: meta.max_key_size,
            meta,
        })
    }

    /// Persists metadata to disk.
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

    /// Returns the number of entries.
    pub fn len(&self) -> u64 {
        self.meta.num_entries
    }

    /// Returns true if empty.
    pub fn is_empty(&self) -> bool {
        self.meta.num_entries == 0
    }

    /// Returns the tree height.
    pub fn height(&self) -> u32 {
        self.meta.height
    }

    /// Returns the maximum key size.
    pub fn max_key_size(&self) -> u16 {
        self.max_key_size
    }

    fn write_meta_to_buf(buf: &mut [u8; PAGE_SIZE], meta: &VarBTreeMeta) {
        buf[32..36].copy_from_slice(&meta.root_page_id.to_le_bytes());
        buf[36..40].copy_from_slice(&meta.height.to_le_bytes());
        buf[40..48].copy_from_slice(&meta.num_entries.to_le_bytes());
        buf[48..50].copy_from_slice(&meta.max_key_size.to_le_bytes());
    }

    fn read_meta_from_buf(buf: &[u8; PAGE_SIZE]) -> VarBTreeMeta {
        VarBTreeMeta {
            root_page_id: u32::from_le_bytes(buf[32..36].try_into().unwrap()),
            height: u32::from_le_bytes(buf[36..40].try_into().unwrap()),
            num_entries: u64::from_le_bytes(buf[40..48].try_into().unwrap()),
            max_key_size: u16::from_le_bytes(buf[48..50].try_into().unwrap()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup(max_key_size: u16) -> (TempDir, VarBTree) {
        let dir = TempDir::new().unwrap();
        let tree = VarBTree::create(dir.path().join("var_index.db"), max_key_size).unwrap();
        (dir, tree)
    }

    #[test]
    fn test_var_btree_create_and_open() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("var_index.db");

        {
            let tree = VarBTree::create(&path, 64).unwrap();
            assert_eq!(tree.len(), 0);
            assert!(tree.is_empty());
            assert_eq!(tree.height(), 1);
            assert_eq!(tree.max_key_size(), 64);
        }

        {
            let tree = VarBTree::open(&path).unwrap();
            assert_eq!(tree.len(), 0);
            assert_eq!(tree.height(), 1);
            assert_eq!(tree.max_key_size(), 64);
        }
    }

    #[test]
    fn test_var_btree_insert_and_search() {
        let (_dir, mut tree) = setup(32);

        tree.insert(b"hello".to_vec(), 10, 0).unwrap();
        tree.insert(b"world".to_vec(), 20, 1).unwrap();
        tree.insert(b"foo".to_vec(), 30, 2).unwrap();

        assert_eq!(tree.search(b"hello").unwrap(), Some((10, 0)));
        assert_eq!(tree.search(b"world").unwrap(), Some((20, 1)));
        assert_eq!(tree.search(b"foo").unwrap(), Some((30, 2)));
        assert_eq!(tree.search(b"bar").unwrap(), None);
        assert_eq!(tree.len(), 3);
    }

    #[test]
    fn test_var_btree_insert_many_causes_splits() {
        let (_dir, mut tree) = setup(32);

        for i in 0u32..1000 {
            let key = format!("key_{i:06}");
            tree.insert(key.into_bytes(), i, 0).unwrap();
        }

        assert_eq!(tree.len(), 1000);
        assert!(tree.height() > 1, "should have split at least once");

        // Verify all keys are retrievable
        for i in 0u32..1000 {
            let key = format!("key_{i:06}");
            let result = tree.search(key.as_bytes()).unwrap();
            assert_eq!(result, Some((i, 0)), "key_{i:06} not found");
        }
    }

    #[test]
    fn test_var_btree_delete() {
        let (_dir, mut tree) = setup(32);

        tree.insert(b"alpha".to_vec(), 1, 0).unwrap();
        tree.insert(b"beta".to_vec(), 2, 0).unwrap();
        tree.insert(b"gamma".to_vec(), 3, 0).unwrap();

        tree.delete(b"beta").unwrap();
        assert_eq!(tree.search(b"beta").unwrap(), None);
        assert_eq!(tree.search(b"alpha").unwrap(), Some((1, 0)));
        assert_eq!(tree.search(b"gamma").unwrap(), Some((3, 0)));
        assert_eq!(tree.len(), 2);
    }

    #[test]
    fn test_var_btree_delete_many() {
        let (_dir, mut tree) = setup(32);

        for i in 0u32..500 {
            tree.insert(format!("key_{i:06}").into_bytes(), i, 0)
                .unwrap();
        }

        // Delete first 250
        for i in 0u32..250 {
            tree.delete(format!("key_{i:06}").as_bytes()).unwrap();
        }

        assert_eq!(tree.len(), 250);

        // Verify deleted are gone
        for i in 0u32..250 {
            assert_eq!(tree.search(format!("key_{i:06}").as_bytes()).unwrap(), None);
        }

        // Verify remaining exist
        for i in 250u32..500 {
            assert!(
                tree.search(format!("key_{i:06}").as_bytes())
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[test]
    fn test_var_btree_persistence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("persist.db");

        {
            let mut tree = VarBTree::create(&path, 32).unwrap();
            for i in 0u32..100 {
                tree.insert(format!("k{i:04}").into_bytes(), i, 0).unwrap();
            }
            tree.sync().unwrap();
        }

        {
            let mut tree = VarBTree::open(&path).unwrap();
            assert_eq!(tree.len(), 100);
            for i in 0u32..100 {
                assert!(
                    tree.search(format!("k{i:04}").as_bytes())
                        .unwrap()
                        .is_some()
                );
            }
        }
    }

    #[test]
    fn test_var_btree_duplicate_key() {
        let (_dir, mut tree) = setup(32);
        tree.insert(b"dup".to_vec(), 1, 0).unwrap();
        let result = tree.insert(b"dup".to_vec(), 2, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_var_btree_delete_nonexistent() {
        let (_dir, mut tree) = setup(32);
        let result = tree.delete(b"ghost");
        assert!(result.is_err());
    }

    #[test]
    fn test_var_btree_large_stress() {
        let (_dir, mut tree) = setup(64);

        // Insert 3000 keys
        for i in 0u32..3000 {
            tree.insert(
                format!("stress_key_{i:08}").into_bytes(),
                i,
                (i % 100) as u16,
            )
            .unwrap();
        }
        assert_eq!(tree.len(), 3000);

        // Delete 1500
        for i in 0u32..1500 {
            tree.delete(format!("stress_key_{i:08}").as_bytes())
                .unwrap();
        }
        assert_eq!(tree.len(), 1500);

        // Verify remaining
        for i in 1500u32..3000 {
            let result = tree
                .search(format!("stress_key_{i:08}").as_bytes())
                .unwrap();
            assert_eq!(result, Some((i, (i % 100) as u16)));
        }
    }
}
