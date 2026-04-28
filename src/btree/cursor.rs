//! Generic B+Tree cursor: forward iteration over leaf entries.
//!
//! The cursor walks the doubly-linked list of leaf nodes to provide
//! sequential, sorted access to all entries in a tree.

use crate::error::Result;
use crate::page::manager::PageManager;
use crate::page::{PageHeader, PageType};

use super::BTree;
use super::key::Key;
use super::node::{InternalNode, LeafEntry, LeafNode};

/// A positioned cursor over B+Tree leaf entries.
pub struct BTreeCursor<K: Key> {
    /// Cached entries of the current leaf.
    entries: Vec<LeafEntry<K>>,
    /// Position within `entries`.
    pos: usize,
    /// Page id of the next leaf (`0` = end of list).
    next_leaf_id: u32,
}

/// A key/value pair returned by `range`/`scan_all`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorEntry<K: Key> {
    pub key: K,
    pub page_id: u32,
    pub slot_id: u16,
}

/// A single item produced by [`BTreeCursor::next_entry`].
#[derive(Debug)]
pub struct CursorItem<K: Key> {
    pub key: K,
    pub page_id: u32,
    pub slot_id: u16,
}

impl<K: Key> BTree<K> {
    /// Returns a cursor positioned at the smallest key in the tree.
    pub fn cursor(&mut self) -> Result<BTreeCursor<K>> {
        let first_leaf = self.find_first_leaf()?;
        Ok(BTreeCursor {
            entries: first_leaf.entries,
            pos: 0,
            next_leaf_id: first_leaf.next_leaf,
        })
    }

    /// Returns a cursor positioned at the first entry with key `>= start_key`.
    pub fn cursor_from(&mut self, start_key: &K) -> Result<BTreeCursor<K>> {
        let leaf = self.find_leaf(start_key)?;
        let pos = leaf
            .entries
            .binary_search_by(|e| e.key.cmp(start_key))
            .unwrap_or_else(|i| i);
        Ok(BTreeCursor {
            entries: leaf.entries,
            pos,
            next_leaf_id: leaf.next_leaf,
        })
    }

    /// Collects all entries with `start <= key < end` into a `Vec`.
    ///
    /// Pass `None` for unbounded start/end.
    pub fn range(&mut self, start: Option<&K>, end: Option<&K>) -> Result<Vec<CursorEntry<K>>> {
        let mut cursor = if let Some(s) = start {
            self.cursor_from(s)?
        } else {
            self.cursor()?
        };

        let mut results = Vec::new();
        while let Some(item) = cursor.next_entry(&mut self.pm)? {
            if let Some(end_key) = end
                && &item.key >= end_key
            {
                break;
            }
            results.push(CursorEntry {
                key: item.key,
                page_id: item.page_id,
                slot_id: item.slot_id,
            });
        }
        Ok(results)
    }

    /// Returns every entry in the tree in sorted order.
    pub fn scan_all(&mut self) -> Result<Vec<CursorEntry<K>>> {
        self.range(None, None)
    }

    /// Walks down the leftmost spine to the smallest leaf.
    fn find_first_leaf(&mut self) -> Result<LeafNode<K>> {
        let mut current = self.meta.root_page_id;
        loop {
            let buf = self.pm.read_page(current)?;
            let header = PageHeader::read_from(&buf);
            match header.page_type {
                PageType::BTreeLeaf => return Ok(LeafNode::from_bytes(&buf)),
                PageType::BTreeInternal => {
                    let node: InternalNode<K> = InternalNode::from_bytes(&buf);
                    current = if node.entries.is_empty() {
                        node.right_child
                    } else {
                        node.entries[0].child_page_id
                    };
                }
                _ => return Err(crate::error::GrumpyError::PageNotFound(current)),
            }
        }
    }
}

impl<K: Key> BTreeCursor<K> {
    /// Advances the cursor and returns the next entry, or `None` when the
    /// scan is exhausted. The page manager is borrowed mutably to load the
    /// next leaf page when needed.
    pub fn next_entry(&mut self, pm: &mut PageManager) -> Result<Option<CursorItem<K>>> {
        loop {
            if self.pos < self.entries.len() {
                let entry = &self.entries[self.pos];
                self.pos += 1;
                return Ok(Some(CursorItem {
                    key: entry.key.clone(),
                    page_id: entry.page_id,
                    slot_id: entry.slot_id,
                }));
            }

            if self.next_leaf_id == 0 {
                return Ok(None);
            }

            let buf = pm.read_page(self.next_leaf_id)?;
            let leaf: LeafNode<K> = LeafNode::from_bytes(&buf);
            self.entries = leaf.entries;
            self.pos = 0;
            self.next_leaf_id = leaf.next_leaf;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use uuid::Uuid;

    // ─── Uuid path ────────────────────────────────────────────────────

    fn uuid_setup() -> (TempDir, BTree<Uuid>) {
        let dir = TempDir::new().unwrap();
        let btree = BTree::<Uuid>::create(dir.path().join("index.db")).unwrap();
        (dir, btree)
    }

    fn make_uuid(val: u128) -> Uuid {
        Uuid::from_u128(val)
    }

    #[test]
    fn test_uuid_cursor_empty_tree() {
        let (_dir, mut btree) = uuid_setup();
        let mut cursor = btree.cursor().unwrap();
        assert!(cursor.next_entry(&mut btree.pm).unwrap().is_none());
    }

    #[test]
    fn test_uuid_cursor_full_scan() {
        let (_dir, mut btree) = uuid_setup();
        for i in 0..100u128 {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }
        let results = btree.scan_all().unwrap();
        assert_eq!(results.len(), 100);
        for i in 1..results.len() {
            assert!(results[i - 1].key < results[i].key);
        }
    }

    #[test]
    fn test_uuid_cursor_range_scan() {
        let (_dir, mut btree) = uuid_setup();
        for i in 0..200u128 {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }
        let start = make_uuid(50);
        let end = make_uuid(100);
        let results = btree.range(Some(&start), Some(&end)).unwrap();
        assert_eq!(results.len(), 50);
        for entry in &results {
            assert!(entry.key >= start);
            assert!(entry.key < end);
        }
    }

    #[test]
    fn test_uuid_cursor_unbounded_start() {
        let (_dir, mut btree) = uuid_setup();
        for i in 0..50u128 {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }
        let end = make_uuid(25);
        let results = btree.range(None, Some(&end)).unwrap();
        assert_eq!(results.len(), 25);
    }

    #[test]
    fn test_uuid_cursor_unbounded_end() {
        let (_dir, mut btree) = uuid_setup();
        for i in 0..50u128 {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }
        let start = make_uuid(25);
        let results = btree.range(Some(&start), None).unwrap();
        assert_eq!(results.len(), 25);
    }

    #[test]
    fn test_uuid_cursor_across_leaf_splits() {
        let (_dir, mut btree) = uuid_setup();
        let count = 1000u128;
        for i in 0..count {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }
        let results = btree.scan_all().unwrap();
        assert_eq!(results.len(), count as usize);
        for i in 1..results.len() {
            assert!(results[i - 1].key < results[i].key);
        }
    }

    #[test]
    fn test_uuid_cursor_from_specific_key() {
        let (_dir, mut btree) = uuid_setup();
        for i in 0..100u128 {
            btree.insert(make_uuid(i * 10), i as u32, 0).unwrap();
        }
        let start = make_uuid(250);
        let mut cursor = btree.cursor_from(&start).unwrap();
        if let Some(first) = cursor.next_entry(&mut btree.pm).unwrap() {
            assert!(first.key >= start);
        }
    }

    #[test]
    fn test_uuid_cursor_empty_range() {
        let (_dir, mut btree) = uuid_setup();
        for i in 0..50u128 {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }
        let start = make_uuid(100);
        let end = make_uuid(200);
        let results = btree.range(Some(&start), Some(&end)).unwrap();
        assert!(results.is_empty());
    }

    // ─── Vec<u8> path ─────────────────────────────────────────────────

    fn vec_setup(max_key_size: u16) -> (TempDir, BTree<Vec<u8>>) {
        let dir = TempDir::new().unwrap();
        let tree = BTree::<Vec<u8>>::create_with(dir.path().join("c.idx"), max_key_size).unwrap();
        (dir, tree)
    }

    #[test]
    fn test_vec_cursor_empty_tree() {
        let (_dir, mut tree) = vec_setup(32);
        let results = tree.scan_all().unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_vec_cursor_full_scan() {
        let (_dir, mut tree) = vec_setup(32);
        tree.insert(b"charlie".to_vec(), 3, 0).unwrap();
        tree.insert(b"alpha".to_vec(), 1, 0).unwrap();
        tree.insert(b"bravo".to_vec(), 2, 0).unwrap();
        let results = tree.scan_all().unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].key, b"alpha");
        assert_eq!(results[1].key, b"bravo");
        assert_eq!(results[2].key, b"charlie");
    }

    #[test]
    fn test_vec_cursor_range_scan() {
        let (_dir, mut tree) = vec_setup(32);
        for i in 0u32..20 {
            tree.insert(format!("k_{i:04}").into_bytes(), i, 0).unwrap();
        }
        let results = tree
            .range(Some(&b"k_0005".to_vec()), Some(&b"k_0010".to_vec()))
            .unwrap();
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].key, b"k_0005");
        assert_eq!(results[4].key, b"k_0009");
    }

    #[test]
    fn test_vec_cursor_unbounded_start() {
        let (_dir, mut tree) = vec_setup(32);
        for i in 0u32..10 {
            tree.insert(format!("x_{i:02}").into_bytes(), i, 0).unwrap();
        }
        let results = tree.range(None, Some(&b"x_05".to_vec())).unwrap();
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].key, b"x_00");
    }

    #[test]
    fn test_vec_cursor_unbounded_end() {
        let (_dir, mut tree) = vec_setup(32);
        for i in 0u32..10 {
            tree.insert(format!("y_{i:02}").into_bytes(), i, 0).unwrap();
        }
        let results = tree.range(Some(&b"y_07".to_vec()), None).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].key, b"y_07");
        assert_eq!(results[2].key, b"y_09");
    }

    #[test]
    fn test_vec_cursor_across_leaf_splits() {
        let (_dir, mut tree) = vec_setup(32);
        for i in 0u32..500 {
            tree.insert(format!("split_{i:06}").into_bytes(), i, 0)
                .unwrap();
        }
        let results = tree.scan_all().unwrap();
        assert_eq!(results.len(), 500);
        for i in 1..results.len() {
            assert!(results[i - 1].key < results[i].key);
        }
    }

    #[test]
    fn test_vec_cursor_empty_range() {
        let (_dir, mut tree) = vec_setup(32);
        tree.insert(b"aaa".to_vec(), 1, 0).unwrap();
        tree.insert(b"zzz".to_vec(), 2, 0).unwrap();
        let results = tree
            .range(Some(&b"mmm".to_vec()), Some(&b"mmm".to_vec()))
            .unwrap();
        assert!(results.is_empty());
    }
}
