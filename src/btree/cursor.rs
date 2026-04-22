//! B+Tree cursor: iterator over leaf entries for range scans.
//!
//! The cursor navigates the linked list of leaf nodes to provide
//! sequential access to entries in sorted key order.

use uuid::Uuid;

use crate::error::Result;
use crate::page::{PageHeader, PageType};

use super::BTree;
use super::node::{LeafEntry, LeafNode};

/// A positioned cursor over B+Tree leaf entries.
///
/// Supports forward iteration through the doubly-linked list of leaf nodes.
/// Created via [`BTree::cursor`] or [`BTree::cursor_from`].
pub struct BTreeCursor {
    /// Current leaf's entries (cached in memory).
    entries: Vec<LeafEntry>,
    /// Current position within `entries`.
    pos: usize,
    /// Page ID of the next leaf node (0 = no more).
    next_leaf_id: u32,
}

/// A key-value pair returned by the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorEntry {
    pub key: Uuid,
    pub page_id: u32,
    pub slot_id: u16,
}

impl BTree {
    /// Creates a cursor positioned at the first entry (smallest key).
    pub fn cursor(&mut self) -> Result<BTreeCursor> {
        let first_leaf = self.find_first_leaf()?;
        Ok(BTreeCursor {
            entries: first_leaf.entries,
            pos: 0,
            next_leaf_id: first_leaf.next_leaf,
        })
    }

    /// Creates a cursor positioned at the first entry >= `start_key`.
    pub fn cursor_from(&mut self, start_key: &Uuid) -> Result<BTreeCursor> {
        let key_bytes = *start_key.as_bytes();
        let leaf = self.find_leaf(&key_bytes)?;

        // Find the position of the first entry >= start_key
        let pos = leaf
            .entries
            .binary_search_by(|e| e.key.cmp(&key_bytes))
            .unwrap_or_else(|i| i);

        Ok(BTreeCursor {
            entries: leaf.entries,
            pos,
            next_leaf_id: leaf.next_leaf,
        })
    }

    /// Collects all entries in a key range into a Vec.
    ///
    /// Both `start` and `end` are inclusive-exclusive: `[start, end)`.
    /// Pass `None` for unbounded start/end.
    pub fn range(&mut self, start: Option<&Uuid>, end: Option<&Uuid>) -> Result<Vec<CursorEntry>> {
        let mut cursor = if let Some(s) = start {
            self.cursor_from(s)?
        } else {
            self.cursor()?
        };

        let end_bytes = end.map(|e| *e.as_bytes());

        let mut results = Vec::new();
        while let Some(entry) = cursor.next_entry(self)? {
            if let Some(ref end_key) = end_bytes
                && entry.key.as_bytes() >= end_key
            {
                break;
            }
            results.push(CursorEntry {
                key: entry.key,
                page_id: entry.page_id,
                slot_id: entry.slot_id,
            });
        }
        Ok(results)
    }

    /// Returns all entries in the tree in sorted order.
    pub fn scan_all(&mut self) -> Result<Vec<CursorEntry>> {
        self.range(None, None)
    }

    /// Finds the leftmost leaf by descending through the first children.
    fn find_first_leaf(&mut self) -> Result<LeafNode> {
        let mut current_page_id = self.meta.root_page_id;

        loop {
            let buf = self.pm.read_page(current_page_id)?;
            let header = PageHeader::read_from(&buf);
            match header.page_type {
                PageType::BTreeLeaf => return Ok(LeafNode::from_bytes(&buf)),
                PageType::BTreeInternal => {
                    let node = super::node::InternalNode::from_bytes(&buf);
                    // Always go to the leftmost child (first entry's child)
                    current_page_id = if node.entries.is_empty() {
                        node.right_child
                    } else {
                        node.entries[0].child_page_id
                    };
                }
                _ => return Err(crate::error::GrumpyError::PageNotFound(current_page_id)),
            }
        }
    }
}

/// A single entry returned by cursor iteration, with UUID key.
#[derive(Debug)]
pub struct CursorItem {
    pub key: Uuid,
    pub page_id: u32,
    pub slot_id: u16,
}

impl BTreeCursor {
    /// Advances the cursor and returns the next entry, or `None` if exhausted.
    ///
    /// Requires a mutable reference to the BTree to load the next leaf page
    /// when the current leaf is exhausted.
    pub fn next_entry(&mut self, btree: &mut BTree) -> Result<Option<CursorItem>> {
        loop {
            if self.pos < self.entries.len() {
                let entry = &self.entries[self.pos];
                self.pos += 1;
                return Ok(Some(CursorItem {
                    key: Uuid::from_bytes(entry.key),
                    page_id: entry.page_id,
                    slot_id: entry.slot_id,
                }));
            }

            // Current leaf exhausted — move to next leaf
            if self.next_leaf_id == 0 {
                return Ok(None); // No more leaves
            }

            let buf = btree.pm.read_page(self.next_leaf_id)?;
            let leaf = LeafNode::from_bytes(&buf);
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

    fn setup() -> (TempDir, BTree) {
        let dir = TempDir::new().unwrap();
        let btree = BTree::create(dir.path().join("index.db")).unwrap();
        (dir, btree)
    }

    fn make_uuid(val: u128) -> Uuid {
        Uuid::from_u128(val)
    }

    #[test]
    fn test_cursor_empty_tree() {
        let (_dir, mut btree) = setup();
        let mut cursor = btree.cursor().unwrap();
        assert!(cursor.next_entry(&mut btree).unwrap().is_none());
    }

    #[test]
    fn test_cursor_full_scan() {
        let (_dir, mut btree) = setup();

        for i in 0..100u128 {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }

        let results = btree.scan_all().unwrap();
        assert_eq!(results.len(), 100);

        // Verify sorted order
        for i in 1..results.len() {
            assert!(
                results[i - 1].key < results[i].key,
                "entries should be in sorted order"
            );
        }
    }

    #[test]
    fn test_cursor_range_scan() {
        let (_dir, mut btree) = setup();

        for i in 0..200u128 {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }

        let start = make_uuid(50);
        let end = make_uuid(100);
        let results = btree.range(Some(&start), Some(&end)).unwrap();

        assert_eq!(results.len(), 50, "range [50, 100) should have 50 entries");

        // Verify all keys are in range
        for entry in &results {
            assert!(entry.key >= start);
            assert!(entry.key < end);
        }
    }

    #[test]
    fn test_cursor_range_unbounded_start() {
        let (_dir, mut btree) = setup();

        for i in 0..50u128 {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }

        let end = make_uuid(25);
        let results = btree.range(None, Some(&end)).unwrap();
        assert_eq!(results.len(), 25);
    }

    #[test]
    fn test_cursor_range_unbounded_end() {
        let (_dir, mut btree) = setup();

        for i in 0..50u128 {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }

        let start = make_uuid(25);
        let results = btree.range(Some(&start), None).unwrap();
        assert_eq!(results.len(), 25);
    }

    #[test]
    fn test_cursor_across_leaf_splits() {
        let (_dir, mut btree) = setup();

        let count = 1000u128;
        for i in 0..count {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }

        let results = btree.scan_all().unwrap();
        assert_eq!(results.len(), count as usize);

        // Verify strict sorted order across leaf boundaries
        for i in 1..results.len() {
            assert!(results[i - 1].key < results[i].key);
        }
    }

    #[test]
    fn test_cursor_from_specific_key() {
        let (_dir, mut btree) = setup();

        for i in 0..100u128 {
            btree.insert(make_uuid(i * 10), i as u32, 0).unwrap();
        }

        // Start from key 250 (which doesn't exist, should start from 250)
        let start = make_uuid(250);
        let mut cursor = btree.cursor_from(&start).unwrap();

        if let Some(first) = cursor.next_entry(&mut btree).unwrap() {
            assert!(first.key >= start, "first entry should be >= start_key");
        }
    }

    #[test]
    fn test_cursor_empty_range() {
        let (_dir, mut btree) = setup();

        for i in 0..50u128 {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }

        // Range with no matching entries
        let start = make_uuid(100);
        let end = make_uuid(200);
        let results = btree.range(Some(&start), Some(&end)).unwrap();
        assert!(results.is_empty());
    }
}
