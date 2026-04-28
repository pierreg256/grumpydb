//! Generic B+Tree index.
//!
//! Maps keys of type `K: Key` to `(PageId, SlotId)` locations inside a data
//! file. The same generic implementation backs both:
//!
//! - **Primary indexes** keyed by [`Uuid`] (fixed 16-byte keys, no per-tree
//!   configuration). Open with [`BTree::create`] / [`BTree::open`].
//! - **Secondary indexes** keyed by `Vec<u8>` (variable-length keys, up to a
//!   per-tree `max_key_size`). Open with [`BTree::create_with`] /
//!   [`BTree::open`].
//!
//! The B+Tree is stored in its own file with its own page management.
//!
//! [`Uuid`]: uuid::Uuid

pub mod cursor;
pub mod key;
pub mod node;
pub mod ops;

use std::path::Path;

use uuid::Uuid;

use crate::error::Result;
use crate::page::manager::PageManager;
use crate::page::{PAGE_SIZE, PageHeader, PageType};

use self::key::Key;
use self::node::LeafNode;

/// Metadata stored on the B+Tree's metadata page (page 1 of the index file).
///
/// On-disk layout:
/// ```text
/// 0  ..32   PageHeader (type = BTreeInternal, repurposed for the meta page)
/// 32 ..36   root_page_id: u32
/// 36 ..40   height: u32
/// 40 ..48   num_entries: u64
/// 48 ..(48 + K::TREE_META_BYTES)   per-tree config (e.g. max_key_size)
/// ```
#[derive(Debug, Clone, Copy)]
pub struct BTreeMeta<K: Key> {
    pub root_page_id: u32,
    pub height: u32,
    pub num_entries: u64,
    pub config: K::Config,
}

/// A B+Tree index stored in a separate file.
///
/// O(log n) `search`, `insert`, `delete`, plus ordered range scans through
/// [`BTree::cursor`] / [`BTree::scan_all`] / [`BTree::range`].
pub struct BTree<K: Key> {
    /// Page id where the metadata lives (always [`Self::META_PAGE_ID`]).
    meta_page_id: u32,
    pub(crate) pm: PageManager,
    pub(crate) meta: BTreeMeta<K>,
}

impl<K: Key> BTree<K> {
    /// The page id reserved for B+Tree metadata. Page 0 is the page-manager
    /// free-list, page 1 is the meta page, page 2 is the initial root.
    pub(crate) const META_PAGE_ID: u32 = 1;

    /// Creates a new B+Tree at `path` with the given per-tree configuration.
    ///
    /// For `BTree<Uuid>`, `cfg` is `()`; the convenience [`BTree::create`]
    /// passes it for you. For `BTree<Vec<u8>>`, `cfg` is the `max_key_size`.
    pub fn create_with(path: impl AsRef<Path>, cfg: K::Config) -> Result<Self> {
        let mut pm = PageManager::new(path)?;

        // page 1 = meta, page 2 = root leaf (page 0 = free-list, allocated by PM)
        let meta_page_id = pm.allocate_page()?;
        let root_page_id = pm.allocate_page()?;

        let root: LeafNode<K> = LeafNode::new(root_page_id, cfg);
        pm.write_page(root_page_id, &root.to_bytes())?;

        let meta = BTreeMeta {
            root_page_id,
            height: 1,
            num_entries: 0,
            config: cfg,
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
        })
    }

    /// Opens an existing B+Tree at `path`.
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

    /// Persists the in-memory metadata to its page.
    pub(crate) fn flush_meta(&mut self) -> Result<()> {
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(self.meta_page_id, PageType::BTreeInternal);
        header.write_to(&mut buf);
        Self::write_meta_to_buf(&mut buf, &self.meta);
        self.pm.write_page(self.meta_page_id, &buf)
    }

    /// Flushes the underlying file to disk.
    pub fn sync(&self) -> Result<()> {
        self.pm.sync()
    }

    /// Returns the number of entries currently in the tree.
    pub fn len(&self) -> u64 {
        self.meta.num_entries
    }

    /// True if the tree contains no entries.
    pub fn is_empty(&self) -> bool {
        self.meta.num_entries == 0
    }

    /// Returns the height of the tree (1 = single root leaf).
    pub fn height(&self) -> u32 {
        self.meta.height
    }

    /// Returns the per-tree configuration.
    pub fn config(&self) -> K::Config {
        self.meta.config
    }

    fn write_meta_to_buf(buf: &mut [u8; PAGE_SIZE], meta: &BTreeMeta<K>) {
        buf[32..36].copy_from_slice(&meta.root_page_id.to_le_bytes());
        buf[36..40].copy_from_slice(&meta.height.to_le_bytes());
        buf[40..48].copy_from_slice(&meta.num_entries.to_le_bytes());
        K::write_tree_config(meta.config, buf);
    }

    fn read_meta_from_buf(buf: &[u8; PAGE_SIZE]) -> BTreeMeta<K> {
        BTreeMeta {
            root_page_id: u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]),
            height: u32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]),
            num_entries: u64::from_le_bytes([
                buf[40], buf[41], buf[42], buf[43], buf[44], buf[45], buf[46], buf[47],
            ]),
            config: K::read_tree_config(buf),
        }
    }
}

// ─────────────── Convenience constructors for the Uuid case ───────────────

impl BTree<Uuid> {
    /// Creates a new UUID-keyed B+Tree (primary index).
    ///
    /// Equivalent to [`BTree::create_with`] with config `()`.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        Self::create_with(path, ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_uuid_btree_create_and_open() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index.db");

        {
            let btree = BTree::<Uuid>::create(&path).unwrap();
            assert_eq!(btree.len(), 0);
            assert!(btree.is_empty());
            assert_eq!(btree.height(), 1);
            assert_eq!(btree.meta.root_page_id, 2);
        }

        {
            let btree = BTree::<Uuid>::open(&path).unwrap();
            assert_eq!(btree.len(), 0);
            assert_eq!(btree.height(), 1);
            assert_eq!(btree.meta.root_page_id, 2);
        }
    }

    #[test]
    fn test_uuid_btree_meta_persistence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index.db");

        {
            let mut btree = BTree::<Uuid>::create(&path).unwrap();
            btree.meta.num_entries = 42;
            btree.meta.height = 3;
            btree.meta.root_page_id = 7;
            btree.flush_meta().unwrap();
            btree.sync().unwrap();
        }

        {
            let btree = BTree::<Uuid>::open(&path).unwrap();
            assert_eq!(btree.len(), 42);
            assert_eq!(btree.height(), 3);
            assert_eq!(btree.meta.root_page_id, 7);
        }
    }

    #[test]
    fn test_uuid_btree_initial_root_is_empty_leaf() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index.db");
        let mut btree = BTree::<Uuid>::create(&path).unwrap();

        let buf = btree.pm.read_page(2).unwrap();
        let leaf: LeafNode<Uuid> = LeafNode::from_bytes(&buf);
        assert_eq!(leaf.num_entries, 0);
        assert_eq!(leaf.next_leaf, 0);
        assert_eq!(leaf.prev_leaf, 0);
    }

    #[test]
    fn test_vec_btree_create_and_open() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("var_index.db");

        {
            let tree = BTree::<Vec<u8>>::create_with(&path, 64).unwrap();
            assert_eq!(tree.len(), 0);
            assert!(tree.is_empty());
            assert_eq!(tree.height(), 1);
            assert_eq!(tree.config(), 64);
        }

        {
            let tree = BTree::<Vec<u8>>::open(&path).unwrap();
            assert_eq!(tree.len(), 0);
            assert_eq!(tree.height(), 1);
            assert_eq!(tree.config(), 64);
        }
    }
}
