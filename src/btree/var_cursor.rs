//! Cursor for variable-key B+Tree: range scans and iteration.

use crate::error::Result;
use crate::page::{PageHeader, PageType};

use super::var_node::VarLeafNode;
use super::var_tree::VarBTree;

/// A positioned cursor over VarBTree leaf entries.
pub struct VarCursor {
    entries: Vec<VarCursorEntry>,
    pos: usize,
    next_leaf_id: u32,
}

/// A key-value pair returned by the VarBTree cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarCursorEntry {
    pub key: Vec<u8>,
    pub page_id: u32,
    pub slot_id: u16,
}

/// Item returned by `next_entry()`.
pub struct VarCursorItem {
    pub key: Vec<u8>,
    pub page_id: u32,
    pub slot_id: u16,
}

impl VarBTree {
    /// Creates a cursor at the first entry (smallest key).
    pub fn cursor(&mut self) -> Result<VarCursor> {
        let first_leaf = self.find_first_leaf()?;
        Ok(VarCursor {
            entries: leaf_to_cursor_entries(&first_leaf),
            pos: 0,
            next_leaf_id: first_leaf.next_leaf,
        })
    }

    /// Creates a cursor positioned at the first entry >= `start_key`.
    pub fn cursor_from(&mut self, start_key: &[u8]) -> Result<VarCursor> {
        let leaf = self.find_leaf(start_key)?;
        let entries = leaf_to_cursor_entries(&leaf);
        let pos = entries
            .iter()
            .position(|e| e.key.as_slice() >= start_key)
            .unwrap_or(entries.len());
        Ok(VarCursor {
            entries,
            pos,
            next_leaf_id: leaf.next_leaf,
        })
    }

    /// Collects all entries in the range `[start, end)`.
    ///
    /// If `start` is None, starts from the beginning.
    /// If `end` is None, scans to the end.
    pub fn range(
        &mut self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<Vec<VarCursorEntry>> {
        let mut cursor = if let Some(start_key) = start {
            self.cursor_from(start_key)?
        } else {
            self.cursor()?
        };

        let mut results = Vec::new();
        while let Some(item) = cursor.next_entry(&mut self.pm)? {
            if let Some(end_key) = end {
                if item.key.as_slice() >= end_key {
                    break;
                }
            }
            results.push(VarCursorEntry {
                key: item.key,
                page_id: item.page_id,
                slot_id: item.slot_id,
            });
        }
        Ok(results)
    }

    /// Returns all entries in sorted order.
    pub fn scan_all(&mut self) -> Result<Vec<VarCursorEntry>> {
        self.range(None, None)
    }

    /// Finds the leftmost leaf.
    fn find_first_leaf(&mut self) -> Result<VarLeafNode> {
        let mut current = self.meta.root_page_id;
        loop {
            let buf = self.pm.read_page(current)?;
            let header = PageHeader::read_from(&buf);
            match header.page_type {
                PageType::BTreeLeaf => return Ok(VarLeafNode::from_bytes(&buf)),
                PageType::BTreeInternal => {
                    let node = super::var_node::VarInternalNode::from_bytes(&buf);
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

impl VarCursor {
    /// Advances the cursor to the next entry.
    pub fn next_entry(
        &mut self,
        pm: &mut crate::page::manager::PageManager,
    ) -> Result<Option<VarCursorItem>> {
        if self.pos < self.entries.len() {
            let entry = &self.entries[self.pos];
            self.pos += 1;
            return Ok(Some(VarCursorItem {
                key: entry.key.clone(),
                page_id: entry.page_id,
                slot_id: entry.slot_id,
            }));
        }

        // Load next leaf
        if self.next_leaf_id == 0 {
            return Ok(None);
        }

        let buf = pm.read_page(self.next_leaf_id)?;
        let leaf = VarLeafNode::from_bytes(&buf);
        self.entries = leaf_to_cursor_entries(&leaf);
        self.pos = 0;
        self.next_leaf_id = leaf.next_leaf;

        if self.entries.is_empty() {
            return Ok(None);
        }

        let entry = &self.entries[0];
        self.pos = 1;
        Ok(Some(VarCursorItem {
            key: entry.key.clone(),
            page_id: entry.page_id,
            slot_id: entry.slot_id,
        }))
    }
}

fn leaf_to_cursor_entries(leaf: &VarLeafNode) -> Vec<VarCursorEntry> {
    leaf.entries
        .iter()
        .map(|e| VarCursorEntry {
            key: e.key.clone(),
            page_id: e.page_id,
            slot_id: e.slot_id,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup(max_key_size: u16) -> (TempDir, VarBTree) {
        let dir = TempDir::new().unwrap();
        let tree = VarBTree::create(dir.path().join("cursor_test.db"), max_key_size).unwrap();
        (dir, tree)
    }

    #[test]
    fn test_var_cursor_empty_tree() {
        let (_dir, mut tree) = setup(32);
        let results = tree.scan_all().unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_var_cursor_full_scan() {
        let (_dir, mut tree) = setup(32);

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
    fn test_var_cursor_range_scan() {
        let (_dir, mut tree) = setup(32);

        for i in 0u32..20 {
            tree.insert(format!("k_{i:04}").into_bytes(), i, 0).unwrap();
        }

        let results = tree.range(Some(b"k_0005"), Some(b"k_0010")).unwrap();
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].key, b"k_0005");
        assert_eq!(results[4].key, b"k_0009");
    }

    #[test]
    fn test_var_cursor_unbounded_start() {
        let (_dir, mut tree) = setup(32);

        for i in 0u32..10 {
            tree.insert(format!("x_{i:02}").into_bytes(), i, 0).unwrap();
        }

        let results = tree.range(None, Some(b"x_05")).unwrap();
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].key, b"x_00");
    }

    #[test]
    fn test_var_cursor_unbounded_end() {
        let (_dir, mut tree) = setup(32);

        for i in 0u32..10 {
            tree.insert(format!("y_{i:02}").into_bytes(), i, 0).unwrap();
        }

        let results = tree.range(Some(b"y_07"), None).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].key, b"y_07");
        assert_eq!(results[2].key, b"y_09");
    }

    #[test]
    fn test_var_cursor_across_leaf_splits() {
        let (_dir, mut tree) = setup(32);

        for i in 0u32..500 {
            tree.insert(format!("split_{i:06}").into_bytes(), i, 0)
                .unwrap();
        }

        let results = tree.scan_all().unwrap();
        assert_eq!(results.len(), 500);

        // Verify sorted order
        for i in 1..results.len() {
            assert!(
                results[i - 1].key < results[i].key,
                "not sorted at position {i}"
            );
        }
    }

    #[test]
    fn test_var_cursor_empty_range() {
        let (_dir, mut tree) = setup(32);

        tree.insert(b"aaa".to_vec(), 1, 0).unwrap();
        tree.insert(b"zzz".to_vec(), 2, 0).unwrap();

        let results = tree.range(Some(b"mmm"), Some(b"mmm")).unwrap();
        assert!(results.is_empty());
    }
}
