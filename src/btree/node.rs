//! B+Tree node types: InternalNode and LeafNode with binary serialization.
//!
//! Internal nodes store keys and child page pointers. Leaf nodes store keys
//! and data pointers (page_id + slot_id), linked as a doubly-linked list.

use crate::page::{PAGE_HEADER_SIZE, PAGE_SIZE, PageHeader, PageType};

/// Size of a UUID key in bytes.
pub const KEY_SIZE: usize = 16;

/// Size of an internal node entry: key(16) + child_page_id(4) = 20 bytes.
pub const INTERNAL_ENTRY_SIZE: usize = KEY_SIZE + 4;

/// Size of a leaf node entry: key(16) + page_id(4) + slot_id(2) = 22 bytes.
pub const LEAF_ENTRY_SIZE: usize = KEY_SIZE + 4 + 2;

/// Usable space in a page after the header.
const USABLE_SPACE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

/// Maximum keys in an internal node.
/// Layout after header: num_keys(2) + right_child(4) + entries(20 each).
pub const INTERNAL_MAX_KEYS: usize = (USABLE_SPACE - 6) / INTERNAL_ENTRY_SIZE;

/// Maximum entries in a leaf node.
/// Layout after header: num_entries(2) + next_leaf(4) + prev_leaf(4) + entries(22 each).
pub const LEAF_MAX_ENTRIES: usize = (USABLE_SPACE - 10) / LEAF_ENTRY_SIZE;

/// Minimum occupancy percentage before merge is considered.
pub const MIN_OCCUPANCY_PERCENT: usize = 40;

/// Minimum keys in an internal node (except root).
pub const INTERNAL_MIN_KEYS: usize = INTERNAL_MAX_KEYS * MIN_OCCUPANCY_PERCENT / 100;

/// Minimum entries in a leaf node (except root).
pub const LEAF_MIN_ENTRIES: usize = LEAF_MAX_ENTRIES * MIN_OCCUPANCY_PERCENT / 100;

/// An entry in an internal node: a separator key and a child pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InternalEntry {
    pub key: [u8; KEY_SIZE],
    pub child_page_id: u32,
}

/// An entry in a leaf node: a key and a pointer to the data (page + slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeafEntry {
    pub key: [u8; KEY_SIZE],
    pub page_id: u32,
    pub slot_id: u16,
}

/// A B+Tree internal node.
///
/// Binary layout after the 32-byte PageHeader:
/// ```text
/// num_keys: u16         (offset 32)
/// right_child: u32      (offset 34)
/// entries[0..num_keys]: [key(16) + child_page_id(4)]  (offset 38+)
/// ```
///
/// Semantics: `entries[i].child_page_id` contains keys `< entries[i].key`.
/// `right_child` contains keys `>= entries[last].key`.
#[derive(Debug, Clone)]
pub struct InternalNode {
    pub page_id: u32,
    pub num_keys: u16,
    pub right_child: u32,
    pub entries: Vec<InternalEntry>,
}

impl InternalNode {
    /// Creates a new empty internal node.
    pub fn new(page_id: u32) -> Self {
        Self {
            page_id,
            num_keys: 0,
            right_child: 0,
            entries: Vec::new(),
        }
    }

    /// Deserializes an internal node from a page buffer.
    pub fn from_bytes(buf: &[u8; PAGE_SIZE]) -> Self {
        let header = PageHeader::read_from(buf);
        let num_keys = u16::from_le_bytes([buf[32], buf[33]]);
        let right_child = u32::from_le_bytes([buf[34], buf[35], buf[36], buf[37]]);

        let mut entries = Vec::with_capacity(num_keys as usize);
        for i in 0..num_keys as usize {
            let base = 38 + i * INTERNAL_ENTRY_SIZE;
            let mut key = [0u8; KEY_SIZE];
            key.copy_from_slice(&buf[base..base + KEY_SIZE]);
            let p = base + KEY_SIZE;
            let child_page_id = u32::from_le_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]);
            entries.push(InternalEntry { key, child_page_id });
        }

        Self {
            page_id: header.page_id,
            num_keys,
            right_child,
            entries,
        }
    }

    /// Serializes the internal node into a page buffer.
    pub fn to_bytes(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(self.page_id, PageType::BTreeInternal);
        header.write_to(&mut buf);

        buf[32..34].copy_from_slice(&self.num_keys.to_le_bytes());
        buf[34..38].copy_from_slice(&self.right_child.to_le_bytes());

        for (i, entry) in self.entries.iter().enumerate() {
            let base = 38 + i * INTERNAL_ENTRY_SIZE;
            buf[base..base + KEY_SIZE].copy_from_slice(&entry.key);
            buf[base + KEY_SIZE..base + KEY_SIZE + 4]
                .copy_from_slice(&entry.child_page_id.to_le_bytes());
        }

        buf
    }

    /// Finds the child page ID for the given key using binary search.
    ///
    /// Convention: `entries[i].child_page_id` = left child of `entries[i].key`,
    /// i.e., subtree containing keys `< entries[i].key` (and `>= entries[i-1].key`).
    /// `right_child` = subtree containing keys `>= entries[last].key`.
    pub fn find_child(&self, key: &[u8; KEY_SIZE]) -> u32 {
        // We want the child pointer for the subtree that could contain `key`.
        // Scan entries to find the first key > search_key; go to its left child.
        // If no such key exists, go to right_child.
        for entry in &self.entries {
            if key < &entry.key {
                return entry.child_page_id;
            }
        }
        self.right_child
    }

    /// Inserts a promoted separator key with its new right child pointer.
    ///
    /// After a child split produces (left_child, promoted_key, right_child),
    /// `left_child` is already referenced by an existing pointer. This method
    /// inserts `promoted_key` such that `right_child_page_id` becomes the
    /// pointer for keys `>= promoted_key` (and `< next_key`).
    pub fn insert_entry(&mut self, key: [u8; KEY_SIZE], right_child_page_id: u32) {
        // Find insertion position
        let pos = self
            .entries
            .binary_search_by(|e| e.key.cmp(&key))
            .unwrap_or_else(|i| i);

        // Insert new entry at `pos`. The child_page_id of the new entry
        // should be the pointer that WAS at position `pos` (the left child
        // of the old key at `pos`), and `right_child_page_id` takes over
        // the slot that previously pointed to the unsplit child.
        //
        // Actually, simpler: the new entry's child_page_id IS the new right child.
        // The left child is already correctly pointed to by the existing pointer
        // at the position we descended through.
        //
        // When we descended through child_idx to reach the split child:
        //   - If child_idx < entries.len(): entries[child_idx].child_page_id pointed to it
        //   - If child_idx == entries.len(): right_child pointed to it
        //
        // After split, that pointer still points to the LEFT half.
        // We insert (promoted_key, new_right_child) AFTER that pointer.
        // The new_right_child needs to be reachable for keys >= promoted_key.
        //
        // Since entries[i].child_page_id is the LEFT child of entries[i].key,
        // the new entry at position `pos` should have child_page_id = new_right_child.
        // BUT that would mean keys < promoted_key go to new_right_child, which is wrong.
        //
        // The correct approach: we need to insert the promoted_key such that:
        //   - The pointer BEFORE promoted_key → left child (already in place)
        //   - The pointer AFTER promoted_key → right child (new)
        //
        // In our layout: entries[pos] has child_page_id = left pointer.
        // If we insert at pos, the new entry gets child_page_id = ?
        //
        // Let's think of it differently. Before insertion, the pointer at child_idx
        // pointed to the node that was split. After split:
        //   - That pointer now points to LEFT half
        //   - We need promoted_key inserted, with RIGHT half reachable
        //
        // We insert the entry AFTER the left pointer. If child_idx < entries.len(),
        // we insert at pos = child_idx + 1 (well, pos from binary search).
        // The new entry at pos: child_page_id should be... let's just swap:
        //   new_entry = (promoted_key, child_page_id=right_child_page_id)
        // Then reroute: if pos < entries.len(), the old entries[pos].child is unaffected.
        //
        // Wait, this is the standard approach: just insert (key, right_ptr) where
        // the entry's child_page_id represents the RIGHT subtree. But our convention
        // says entries[i].child_page_id is the LEFT child of entries[i].key.
        //
        // So the fix: insert the new entry, then swap the child_page_id.
        // New entry goes at `pos` with child_page_id = the OLD left pointer at pos,
        // and the old pointer at pos becomes right_child_page_id.

        if pos < self.entries.len() {
            // Inserting before an existing entry.
            // The pointer at entries[pos].child_page_id is the existing left child.
            // After insert: new entry at pos gets that old child (left of promoted_key),
            // and entries[pos+1] (the shifted old entry) gets right_child_page_id.
            let old_child = self.entries[pos].child_page_id;
            self.entries.insert(
                pos,
                InternalEntry {
                    key,
                    child_page_id: old_child,
                },
            );
            self.entries[pos + 1].child_page_id = right_child_page_id;
        } else {
            // Inserting at the end (promoted_key > all existing keys).
            // The current right_child is the left pointer of promoted_key.
            // The new right_child becomes right_child_page_id.
            self.entries.push(InternalEntry {
                key,
                child_page_id: self.right_child,
            });
            self.right_child = right_child_page_id;
        }
        self.num_keys += 1;
    }

    /// Returns true if the node is overfull and needs splitting.
    pub fn is_overfull(&self) -> bool {
        self.num_keys as usize > INTERNAL_MAX_KEYS
    }

    /// Returns true if the node is underfull (below minimum occupancy).
    pub fn is_underfull(&self) -> bool {
        (self.num_keys as usize) < INTERNAL_MIN_KEYS
    }
}

/// A B+Tree leaf node.
///
/// Binary layout after the 32-byte PageHeader:
/// ```text
/// num_entries: u16      (offset 32)
/// next_leaf: u32        (offset 34, 0 = no next)
/// prev_leaf: u32        (offset 38, 0 = no prev)
/// entries[0..n]: [key(16) + page_id(4) + slot_id(2)]  (offset 42+)
/// ```
///
/// Leaf nodes form a doubly-linked list for efficient sequential scans.
#[derive(Debug, Clone)]
pub struct LeafNode {
    pub page_id: u32,
    pub num_entries: u16,
    pub next_leaf: u32,
    pub prev_leaf: u32,
    pub entries: Vec<LeafEntry>,
}

impl LeafNode {
    /// Creates a new empty leaf node.
    pub fn new(page_id: u32) -> Self {
        Self {
            page_id,
            num_entries: 0,
            next_leaf: 0,
            prev_leaf: 0,
            entries: Vec::new(),
        }
    }

    /// Deserializes a leaf node from a page buffer.
    pub fn from_bytes(buf: &[u8; PAGE_SIZE]) -> Self {
        let header = PageHeader::read_from(buf);
        let num_entries = u16::from_le_bytes([buf[32], buf[33]]);
        let next_leaf = u32::from_le_bytes([buf[34], buf[35], buf[36], buf[37]]);
        let prev_leaf = u32::from_le_bytes([buf[38], buf[39], buf[40], buf[41]]);

        let mut entries = Vec::with_capacity(num_entries as usize);
        for i in 0..num_entries as usize {
            let base = 42 + i * LEAF_ENTRY_SIZE;
            let mut key = [0u8; KEY_SIZE];
            key.copy_from_slice(&buf[base..base + KEY_SIZE]);
            let p = base + KEY_SIZE;
            let page_id = u32::from_le_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]);
            let slot_id = u16::from_le_bytes([buf[p + 4], buf[p + 5]]);
            entries.push(LeafEntry {
                key,
                page_id,
                slot_id,
            });
        }

        Self {
            page_id: header.page_id,
            num_entries,
            next_leaf,
            prev_leaf,
            entries,
        }
    }

    /// Serializes the leaf node into a page buffer.
    pub fn to_bytes(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(self.page_id, PageType::BTreeLeaf);
        header.write_to(&mut buf);

        buf[32..34].copy_from_slice(&self.num_entries.to_le_bytes());
        buf[34..38].copy_from_slice(&self.next_leaf.to_le_bytes());
        buf[38..42].copy_from_slice(&self.prev_leaf.to_le_bytes());

        for (i, entry) in self.entries.iter().enumerate() {
            let base = 42 + i * LEAF_ENTRY_SIZE;
            buf[base..base + KEY_SIZE].copy_from_slice(&entry.key);
            buf[base + KEY_SIZE..base + KEY_SIZE + 4].copy_from_slice(&entry.page_id.to_le_bytes());
            buf[base + KEY_SIZE + 4..base + KEY_SIZE + 6]
                .copy_from_slice(&entry.slot_id.to_le_bytes());
        }

        buf
    }

    /// Searches for a key in the leaf. Returns the entry index if found.
    pub fn search(&self, key: &[u8; KEY_SIZE]) -> Option<usize> {
        self.entries.binary_search_by(|e| e.key.cmp(key)).ok()
    }

    /// Inserts an entry in sorted order. Does NOT check for overflow.
    pub fn insert_entry(&mut self, entry: LeafEntry) {
        let pos = self
            .entries
            .binary_search_by(|e| e.key.cmp(&entry.key))
            .unwrap_or_else(|i| i);
        self.entries.insert(pos, entry);
        self.num_entries += 1;
    }

    /// Removes the entry at the given key. Returns the removed entry if found.
    pub fn remove_entry(&mut self, key: &[u8; KEY_SIZE]) -> Option<LeafEntry> {
        if let Ok(idx) = self.entries.binary_search_by(|e| e.key.cmp(key)) {
            self.num_entries -= 1;
            Some(self.entries.remove(idx))
        } else {
            None
        }
    }

    /// Returns true if the node is overfull and needs splitting.
    pub fn is_overfull(&self) -> bool {
        self.num_entries as usize > LEAF_MAX_ENTRIES
    }

    /// Returns true if the node is underfull (below minimum occupancy).
    pub fn is_underfull(&self) -> bool {
        (self.num_entries as usize) < LEAF_MIN_ENTRIES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(val: u8) -> [u8; KEY_SIZE] {
        let mut k = [0u8; KEY_SIZE];
        k[15] = val;
        k
    }

    #[test]
    fn test_internal_node_round_trip() {
        let mut node = InternalNode::new(5);
        node.right_child = 99;
        node.entries.push(InternalEntry {
            key: make_key(10),
            child_page_id: 2,
        });
        node.entries.push(InternalEntry {
            key: make_key(20),
            child_page_id: 3,
        });
        node.num_keys = 2;

        let buf = node.to_bytes();
        let restored = InternalNode::from_bytes(&buf);

        assert_eq!(restored.page_id, 5);
        assert_eq!(restored.num_keys, 2);
        assert_eq!(restored.right_child, 99);
        assert_eq!(restored.entries.len(), 2);
        assert_eq!(restored.entries[0].key, make_key(10));
        assert_eq!(restored.entries[0].child_page_id, 2);
        assert_eq!(restored.entries[1].key, make_key(20));
        assert_eq!(restored.entries[1].child_page_id, 3);
    }

    #[test]
    fn test_leaf_node_round_trip() {
        let mut node = LeafNode::new(7);
        node.next_leaf = 8;
        node.prev_leaf = 6;
        node.entries.push(LeafEntry {
            key: make_key(5),
            page_id: 100,
            slot_id: 0,
        });
        node.entries.push(LeafEntry {
            key: make_key(15),
            page_id: 101,
            slot_id: 3,
        });
        node.num_entries = 2;

        let buf = node.to_bytes();
        let restored = LeafNode::from_bytes(&buf);

        assert_eq!(restored.page_id, 7);
        assert_eq!(restored.num_entries, 2);
        assert_eq!(restored.next_leaf, 8);
        assert_eq!(restored.prev_leaf, 6);
        assert_eq!(restored.entries[0].key, make_key(5));
        assert_eq!(restored.entries[0].page_id, 100);
        assert_eq!(restored.entries[1].slot_id, 3);
    }

    #[test]
    fn test_internal_max_capacity() {
        // Verify we can serialize a full internal node
        let mut node = InternalNode::new(1);
        node.right_child = 999;
        for i in 0..INTERNAL_MAX_KEYS {
            let mut key = [0u8; KEY_SIZE];
            key[14..16].copy_from_slice(&(i as u16).to_le_bytes());
            node.entries.push(InternalEntry {
                key,
                child_page_id: i as u32 + 1,
            });
        }
        node.num_keys = INTERNAL_MAX_KEYS as u16;

        let buf = node.to_bytes();
        let restored = InternalNode::from_bytes(&buf);
        assert_eq!(restored.num_keys as usize, INTERNAL_MAX_KEYS);
        assert_eq!(restored.entries.len(), INTERNAL_MAX_KEYS);
    }

    #[test]
    fn test_leaf_max_capacity() {
        let mut node = LeafNode::new(1);
        for i in 0..LEAF_MAX_ENTRIES {
            let mut key = [0u8; KEY_SIZE];
            key[14..16].copy_from_slice(&(i as u16).to_le_bytes());
            node.entries.push(LeafEntry {
                key,
                page_id: i as u32,
                slot_id: 0,
            });
        }
        node.num_entries = LEAF_MAX_ENTRIES as u16;

        let buf = node.to_bytes();
        let restored = LeafNode::from_bytes(&buf);
        assert_eq!(restored.num_entries as usize, LEAF_MAX_ENTRIES);
        assert_eq!(restored.entries.len(), LEAF_MAX_ENTRIES);
    }

    #[test]
    fn test_internal_find_child() {
        // keys: [10, 20, 30]
        // children layout: c0 | k=10 | c1 | k=20 | c2 | k=30 | right_child
        // c0 = keys < 10, c1 = keys in [10,20), c2 = keys in [20,30), right = keys >= 30
        let mut node = InternalNode::new(1);
        node.entries = vec![
            InternalEntry {
                key: make_key(10),
                child_page_id: 100,
            },
            InternalEntry {
                key: make_key(20),
                child_page_id: 101,
            },
            InternalEntry {
                key: make_key(30),
                child_page_id: 102,
            },
        ];
        node.num_keys = 3;
        node.right_child = 103;

        assert_eq!(node.find_child(&make_key(5)), 100); // < 10 → c0
        assert_eq!(node.find_child(&make_key(10)), 101); // >= 10, < 20 → c1
        assert_eq!(node.find_child(&make_key(15)), 101); // >= 10, < 20 → c1
        assert_eq!(node.find_child(&make_key(20)), 102); // >= 20, < 30 → c2
        assert_eq!(node.find_child(&make_key(25)), 102); // >= 20, < 30 → c2
        assert_eq!(node.find_child(&make_key(30)), 103); // >= 30 → right_child
        assert_eq!(node.find_child(&make_key(99)), 103); // >= 30 → right_child
    }

    #[test]
    fn test_leaf_search() {
        let mut node = LeafNode::new(1);
        node.insert_entry(LeafEntry {
            key: make_key(10),
            page_id: 1,
            slot_id: 0,
        });
        node.insert_entry(LeafEntry {
            key: make_key(20),
            page_id: 2,
            slot_id: 1,
        });
        node.insert_entry(LeafEntry {
            key: make_key(30),
            page_id: 3,
            slot_id: 2,
        });

        assert_eq!(node.search(&make_key(20)), Some(1));
        assert_eq!(node.search(&make_key(5)), None);
        assert_eq!(node.search(&make_key(15)), None);
    }

    #[test]
    fn test_leaf_insert_sorted() {
        let mut node = LeafNode::new(1);
        node.insert_entry(LeafEntry {
            key: make_key(30),
            page_id: 1,
            slot_id: 0,
        });
        node.insert_entry(LeafEntry {
            key: make_key(10),
            page_id: 2,
            slot_id: 0,
        });
        node.insert_entry(LeafEntry {
            key: make_key(20),
            page_id: 3,
            slot_id: 0,
        });

        assert_eq!(node.entries[0].key, make_key(10));
        assert_eq!(node.entries[1].key, make_key(20));
        assert_eq!(node.entries[2].key, make_key(30));
        assert_eq!(node.num_entries, 3);
    }

    #[test]
    fn test_leaf_remove_entry() {
        let mut node = LeafNode::new(1);
        node.insert_entry(LeafEntry {
            key: make_key(10),
            page_id: 1,
            slot_id: 0,
        });
        node.insert_entry(LeafEntry {
            key: make_key(20),
            page_id: 2,
            slot_id: 0,
        });
        node.insert_entry(LeafEntry {
            key: make_key(30),
            page_id: 3,
            slot_id: 0,
        });

        let removed = node.remove_entry(&make_key(20));
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().page_id, 2);
        assert_eq!(node.num_entries, 2);
        assert!(node.search(&make_key(20)).is_none());

        assert!(node.remove_entry(&make_key(99)).is_none());
    }

    #[test]
    fn test_constants() {
        // These values are compile-time constants, but we verify them
        // as a sanity check for the page layout calculations.
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(
                INTERNAL_MAX_KEYS > 100,
                "fan-out should be large: {INTERNAL_MAX_KEYS}"
            );
            assert!(
                LEAF_MAX_ENTRIES > 100,
                "leaf capacity should be large: {LEAF_MAX_ENTRIES}"
            );
            assert!(
                INTERNAL_MIN_KEYS > 0,
                "internal min keys: {INTERNAL_MIN_KEYS}"
            );
            assert!(LEAF_MIN_ENTRIES > 0, "leaf min entries: {LEAF_MIN_ENTRIES}");
        }
    }
}
