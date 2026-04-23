//! Variable-length key B+Tree node types with binary serialization.
//!
//! Unlike the fixed-key nodes in `node.rs` (16-byte UUID keys), these nodes
//! store keys with a 2-byte length prefix, supporting keys up to 256 bytes.
//!
//! ## Internal node binary format
//!
//! ```text
//! Offset  Content
//! 0-31    PageHeader (page_type = BTreeInternal)
//! 32-33   num_keys: u16
//! 34-37   right_child: u32
//! 38-39   max_key_size: u16           ← NEW: for capacity validation
//! 40+     entries: [key_len(u16) + key_data[..] + child_page_id(u32)] × num_keys
//! ```
//!
//! ## Leaf node binary format
//!
//! ```text
//! Offset  Content
//! 0-31    PageHeader (page_type = BTreeLeaf)
//! 32-33   num_entries: u16
//! 34-37   next_leaf: u32
//! 38-41   prev_leaf: u32
//! 42-43   max_key_size: u16           ← NEW
//! 44+     entries: [key_len(u16) + key_data[..] + page_id(u32) + slot_id(u16)] × n
//! ```

use crate::page::{PAGE_HEADER_SIZE, PAGE_SIZE, PageHeader, PageType};

use super::key::VAR_KEY_LEN_PREFIX;

/// Fixed overhead per internal node (after page header):
/// num_keys(2) + right_child(4) + max_key_size(2) = 8 bytes.
const INTERNAL_OVERHEAD: usize = 8;

/// Fixed overhead per leaf node (after page header):
/// num_entries(2) + next_leaf(4) + prev_leaf(4) + max_key_size(2) = 12 bytes.
const LEAF_OVERHEAD: usize = 12;

/// Usable space in a page after the header.
const USABLE_SPACE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

/// Start offset for internal entries (after header + overhead).
const INTERNAL_ENTRIES_START: usize = PAGE_HEADER_SIZE + INTERNAL_OVERHEAD;

/// Start offset for leaf entries (after header + overhead).
const LEAF_ENTRIES_START: usize = PAGE_HEADER_SIZE + LEAF_OVERHEAD;

/// Size of a child pointer in internal entries.
const CHILD_PTR_SIZE: usize = 4;

/// Size of a data pointer in leaf entries (page_id + slot_id).
const DATA_PTR_SIZE: usize = 6;

/// Minimum occupancy percentage before merge is considered.
const MIN_OCCUPANCY_PERCENT: usize = 40;

/// Calculates maximum internal entries for a given max key size.
pub fn var_internal_max_keys(max_key_size: usize) -> usize {
    let entry_size = VAR_KEY_LEN_PREFIX + max_key_size + CHILD_PTR_SIZE;
    (USABLE_SPACE - INTERNAL_OVERHEAD) / entry_size
}

/// Calculates maximum leaf entries for a given max key size.
pub fn var_leaf_max_entries(max_key_size: usize) -> usize {
    let entry_size = VAR_KEY_LEN_PREFIX + max_key_size + DATA_PTR_SIZE;
    (USABLE_SPACE - LEAF_OVERHEAD) / entry_size
}

/// Minimum keys before an internal node is considered underfull.
pub fn var_internal_min_keys(max_key_size: usize) -> usize {
    var_internal_max_keys(max_key_size) * MIN_OCCUPANCY_PERCENT / 100
}

/// Minimum entries before a leaf node is considered underfull.
pub fn var_leaf_min_entries(max_key_size: usize) -> usize {
    var_leaf_max_entries(max_key_size) * MIN_OCCUPANCY_PERCENT / 100
}

/// An entry in a variable-key internal node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarInternalEntry {
    pub key: Vec<u8>,
    pub child_page_id: u32,
}

/// An entry in a variable-key leaf node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarLeafEntry {
    pub key: Vec<u8>,
    pub page_id: u32,
    pub slot_id: u16,
}

/// A variable-key B+Tree internal node.
#[derive(Debug, Clone)]
pub struct VarInternalNode {
    pub page_id: u32,
    pub num_keys: u16,
    pub right_child: u32,
    pub max_key_size: u16,
    pub entries: Vec<VarInternalEntry>,
}

impl VarInternalNode {
    /// Creates a new empty internal node.
    pub fn new(page_id: u32, max_key_size: u16) -> Self {
        Self {
            page_id,
            num_keys: 0,
            right_child: 0,
            max_key_size,
            entries: Vec::new(),
        }
    }

    /// Deserializes from a page buffer.
    pub fn from_bytes(buf: &[u8; PAGE_SIZE]) -> Self {
        let header = PageHeader::read_from(buf);
        let num_keys = u16::from_le_bytes(buf[32..34].try_into().unwrap());
        let right_child = u32::from_le_bytes(buf[34..38].try_into().unwrap());
        let max_key_size = u16::from_le_bytes(buf[38..40].try_into().unwrap());

        let mut entries = Vec::with_capacity(num_keys as usize);
        let mut offset = INTERNAL_ENTRIES_START;

        for _ in 0..num_keys {
            let key_len = u16::from_le_bytes(buf[offset..offset + 2].try_into().unwrap()) as usize;
            offset += 2;
            let key = buf[offset..offset + key_len].to_vec();
            offset += key_len;
            // Pad to max_key_size for fixed-stride layout
            offset += max_key_size as usize - key_len;
            let child_page_id = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap());
            offset += 4;
            entries.push(VarInternalEntry { key, child_page_id });
        }

        Self {
            page_id: header.page_id,
            num_keys,
            right_child,
            max_key_size,
            entries,
        }
    }

    /// Serializes into a page buffer.
    pub fn to_bytes(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(self.page_id, PageType::BTreeInternal);
        header.write_to(&mut buf);

        buf[32..34].copy_from_slice(&self.num_keys.to_le_bytes());
        buf[34..38].copy_from_slice(&self.right_child.to_le_bytes());
        buf[38..40].copy_from_slice(&self.max_key_size.to_le_bytes());

        let stride = VAR_KEY_LEN_PREFIX + self.max_key_size as usize + CHILD_PTR_SIZE;
        for (i, entry) in self.entries.iter().enumerate() {
            let base = INTERNAL_ENTRIES_START + i * stride;
            buf[base..base + 2].copy_from_slice(&(entry.key.len() as u16).to_le_bytes());
            buf[base + 2..base + 2 + entry.key.len()].copy_from_slice(&entry.key);
            // Remaining bytes up to max_key_size are zero-padded (already zero)
            let ptr_offset = base + VAR_KEY_LEN_PREFIX + self.max_key_size as usize;
            buf[ptr_offset..ptr_offset + 4].copy_from_slice(&entry.child_page_id.to_le_bytes());
        }

        buf
    }

    /// Finds the child page ID for the given key.
    pub fn find_child(&self, key: &[u8]) -> u32 {
        for entry in &self.entries {
            if key < entry.key.as_slice() {
                return entry.child_page_id;
            }
        }
        self.right_child
    }

    /// Inserts a promoted separator key with its right child pointer.
    pub fn insert_entry(&mut self, key: Vec<u8>, right_child_page_id: u32) {
        let pos = self
            .entries
            .binary_search_by(|e| e.key.as_slice().cmp(key.as_slice()))
            .unwrap_or_else(|i| i);

        if pos < self.entries.len() {
            let old_child = self.entries[pos].child_page_id;
            self.entries.insert(
                pos,
                VarInternalEntry {
                    key,
                    child_page_id: old_child,
                },
            );
            self.entries[pos + 1].child_page_id = right_child_page_id;
        } else {
            self.entries.push(VarInternalEntry {
                key,
                child_page_id: self.right_child,
            });
            self.right_child = right_child_page_id;
        }
        self.num_keys += 1;
    }

    /// Returns true if the node is overfull.
    pub fn is_overfull(&self) -> bool {
        self.num_keys as usize > var_internal_max_keys(self.max_key_size as usize)
    }

    /// Returns true if the node is underfull.
    pub fn is_underfull(&self) -> bool {
        (self.num_keys as usize) < var_internal_min_keys(self.max_key_size as usize)
    }
}

/// A variable-key B+Tree leaf node.
#[derive(Debug, Clone)]
pub struct VarLeafNode {
    pub page_id: u32,
    pub num_entries: u16,
    pub next_leaf: u32,
    pub prev_leaf: u32,
    pub max_key_size: u16,
    pub entries: Vec<VarLeafEntry>,
}

impl VarLeafNode {
    /// Creates a new empty leaf node.
    pub fn new(page_id: u32, max_key_size: u16) -> Self {
        Self {
            page_id,
            num_entries: 0,
            next_leaf: 0,
            prev_leaf: 0,
            max_key_size,
            entries: Vec::new(),
        }
    }

    /// Deserializes from a page buffer.
    pub fn from_bytes(buf: &[u8; PAGE_SIZE]) -> Self {
        let header = PageHeader::read_from(buf);
        let num_entries = u16::from_le_bytes(buf[32..34].try_into().unwrap());
        let next_leaf = u32::from_le_bytes(buf[34..38].try_into().unwrap());
        let prev_leaf = u32::from_le_bytes(buf[38..42].try_into().unwrap());
        let max_key_size = u16::from_le_bytes(buf[42..44].try_into().unwrap());

        let stride = VAR_KEY_LEN_PREFIX + max_key_size as usize + DATA_PTR_SIZE;
        let mut entries = Vec::with_capacity(num_entries as usize);

        for i in 0..num_entries as usize {
            let base = LEAF_ENTRIES_START + i * stride;
            let key_len = u16::from_le_bytes(buf[base..base + 2].try_into().unwrap()) as usize;
            let key = buf[base + 2..base + 2 + key_len].to_vec();
            let ptr_offset = base + VAR_KEY_LEN_PREFIX + max_key_size as usize;
            let page_id = u32::from_le_bytes(buf[ptr_offset..ptr_offset + 4].try_into().unwrap());
            let slot_id =
                u16::from_le_bytes(buf[ptr_offset + 4..ptr_offset + 6].try_into().unwrap());
            entries.push(VarLeafEntry {
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
            max_key_size,
            entries,
        }
    }

    /// Serializes into a page buffer.
    pub fn to_bytes(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(self.page_id, PageType::BTreeLeaf);
        header.write_to(&mut buf);

        buf[32..34].copy_from_slice(&self.num_entries.to_le_bytes());
        buf[34..38].copy_from_slice(&self.next_leaf.to_le_bytes());
        buf[38..42].copy_from_slice(&self.prev_leaf.to_le_bytes());
        buf[42..44].copy_from_slice(&self.max_key_size.to_le_bytes());

        let stride = VAR_KEY_LEN_PREFIX + self.max_key_size as usize + DATA_PTR_SIZE;
        for (i, entry) in self.entries.iter().enumerate() {
            let base = LEAF_ENTRIES_START + i * stride;
            buf[base..base + 2].copy_from_slice(&(entry.key.len() as u16).to_le_bytes());
            buf[base + 2..base + 2 + entry.key.len()].copy_from_slice(&entry.key);
            let ptr_offset = base + VAR_KEY_LEN_PREFIX + self.max_key_size as usize;
            buf[ptr_offset..ptr_offset + 4].copy_from_slice(&entry.page_id.to_le_bytes());
            buf[ptr_offset + 4..ptr_offset + 6].copy_from_slice(&entry.slot_id.to_le_bytes());
        }

        buf
    }

    /// Searches for a key. Returns the entry index if found.
    pub fn search(&self, key: &[u8]) -> Option<usize> {
        self.entries
            .binary_search_by(|e| e.key.as_slice().cmp(key))
            .ok()
    }

    /// Inserts an entry in sorted order.
    pub fn insert_entry(&mut self, entry: VarLeafEntry) {
        let pos = self
            .entries
            .binary_search_by(|e| e.key.as_slice().cmp(entry.key.as_slice()))
            .unwrap_or_else(|i| i);
        self.entries.insert(pos, entry);
        self.num_entries += 1;
    }

    /// Removes the entry with the given key. Returns the removed entry if found.
    pub fn remove_entry(&mut self, key: &[u8]) -> Option<VarLeafEntry> {
        if let Ok(idx) = self.entries.binary_search_by(|e| e.key.as_slice().cmp(key)) {
            self.num_entries -= 1;
            Some(self.entries.remove(idx))
        } else {
            None
        }
    }

    /// Returns true if the node is overfull.
    pub fn is_overfull(&self) -> bool {
        self.num_entries as usize > var_leaf_max_entries(self.max_key_size as usize)
    }

    /// Returns true if the node is underfull.
    pub fn is_underfull(&self) -> bool {
        (self.num_entries as usize) < var_leaf_min_entries(self.max_key_size as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_var_internal_node_round_trip() {
        let mut node = VarInternalNode::new(5, 32);
        node.right_child = 99;
        node.entries.push(VarInternalEntry {
            key: b"alpha".to_vec(),
            child_page_id: 2,
        });
        node.entries.push(VarInternalEntry {
            key: b"beta".to_vec(),
            child_page_id: 3,
        });
        node.num_keys = 2;

        let buf = node.to_bytes();
        let restored = VarInternalNode::from_bytes(&buf);

        assert_eq!(restored.page_id, 5);
        assert_eq!(restored.num_keys, 2);
        assert_eq!(restored.right_child, 99);
        assert_eq!(restored.max_key_size, 32);
        assert_eq!(restored.entries[0].key, b"alpha");
        assert_eq!(restored.entries[0].child_page_id, 2);
        assert_eq!(restored.entries[1].key, b"beta");
        assert_eq!(restored.entries[1].child_page_id, 3);
    }

    #[test]
    fn test_var_leaf_node_round_trip() {
        let mut node = VarLeafNode::new(7, 32);
        node.next_leaf = 8;
        node.prev_leaf = 6;
        node.entries.push(VarLeafEntry {
            key: b"apple".to_vec(),
            page_id: 100,
            slot_id: 0,
        });
        node.entries.push(VarLeafEntry {
            key: b"banana".to_vec(),
            page_id: 101,
            slot_id: 3,
        });
        node.num_entries = 2;

        let buf = node.to_bytes();
        let restored = VarLeafNode::from_bytes(&buf);

        assert_eq!(restored.page_id, 7);
        assert_eq!(restored.num_entries, 2);
        assert_eq!(restored.next_leaf, 8);
        assert_eq!(restored.prev_leaf, 6);
        assert_eq!(restored.max_key_size, 32);
        assert_eq!(restored.entries[0].key, b"apple");
        assert_eq!(restored.entries[0].page_id, 100);
        assert_eq!(restored.entries[1].key, b"banana");
        assert_eq!(restored.entries[1].slot_id, 3);
    }

    #[test]
    fn test_var_internal_max_capacity() {
        let max_keys = var_internal_max_keys(32);
        assert!(max_keys > 10, "should fit many keys, got {max_keys}");

        let mut node = VarInternalNode::new(1, 32);
        node.right_child = 999;
        for i in 0..max_keys {
            node.entries.push(VarInternalEntry {
                key: format!("key_{i:06}").into_bytes(),
                child_page_id: i as u32 + 1,
            });
        }
        node.num_keys = max_keys as u16;

        let buf = node.to_bytes();
        let restored = VarInternalNode::from_bytes(&buf);
        assert_eq!(restored.num_keys as usize, max_keys);
        assert_eq!(restored.entries.len(), max_keys);
    }

    #[test]
    fn test_var_leaf_max_capacity() {
        let max_entries = var_leaf_max_entries(32);
        assert!(
            max_entries > 10,
            "should fit many entries, got {max_entries}"
        );

        let mut node = VarLeafNode::new(1, 32);
        for i in 0..max_entries {
            node.entries.push(VarLeafEntry {
                key: format!("key_{i:06}").into_bytes(),
                page_id: i as u32,
                slot_id: 0,
            });
        }
        node.num_entries = max_entries as u16;

        let buf = node.to_bytes();
        let restored = VarLeafNode::from_bytes(&buf);
        assert_eq!(restored.num_entries as usize, max_entries);
    }

    #[test]
    fn test_var_internal_find_child() {
        let mut node = VarInternalNode::new(1, 32);
        node.entries = vec![
            VarInternalEntry {
                key: b"delta".to_vec(),
                child_page_id: 100,
            },
            VarInternalEntry {
                key: b"hotel".to_vec(),
                child_page_id: 101,
            },
            VarInternalEntry {
                key: b"mike".to_vec(),
                child_page_id: 102,
            },
        ];
        node.num_keys = 3;
        node.right_child = 103;

        assert_eq!(node.find_child(b"alpha"), 100); // < "delta"
        assert_eq!(node.find_child(b"delta"), 101); // >= "delta", < "hotel"
        assert_eq!(node.find_child(b"foxtrot"), 101);
        assert_eq!(node.find_child(b"hotel"), 102);
        assert_eq!(node.find_child(b"zulu"), 103); // >= "mike" → right
    }

    #[test]
    fn test_var_leaf_search_and_insert() {
        let mut node = VarLeafNode::new(1, 32);
        node.insert_entry(VarLeafEntry {
            key: b"cherry".to_vec(),
            page_id: 1,
            slot_id: 0,
        });
        node.insert_entry(VarLeafEntry {
            key: b"apple".to_vec(),
            page_id: 2,
            slot_id: 1,
        });
        node.insert_entry(VarLeafEntry {
            key: b"banana".to_vec(),
            page_id: 3,
            slot_id: 2,
        });

        assert_eq!(node.entries[0].key, b"apple");
        assert_eq!(node.entries[1].key, b"banana");
        assert_eq!(node.entries[2].key, b"cherry");

        assert_eq!(node.search(b"banana"), Some(1));
        assert_eq!(node.search(b"date"), None);
    }

    #[test]
    fn test_var_leaf_remove_entry() {
        let mut node = VarLeafNode::new(1, 32);
        node.insert_entry(VarLeafEntry {
            key: b"a".to_vec(),
            page_id: 1,
            slot_id: 0,
        });
        node.insert_entry(VarLeafEntry {
            key: b"b".to_vec(),
            page_id: 2,
            slot_id: 0,
        });
        node.insert_entry(VarLeafEntry {
            key: b"c".to_vec(),
            page_id: 3,
            slot_id: 0,
        });

        let removed = node.remove_entry(b"b");
        assert!(removed.is_some());
        assert_eq!(node.num_entries, 2);
        assert_eq!(node.search(b"b"), None);
        assert_eq!(node.search(b"a"), Some(0));
        assert_eq!(node.search(b"c"), Some(1));
    }

    #[test]
    fn test_var_capacity_calculations() {
        // With max_key_size=16 (UUID-like), should have similar capacity to fixed-key BTree
        let internal = var_internal_max_keys(16);
        let leaf = var_leaf_max_entries(16);
        // Original: INTERNAL_MAX_KEYS=407, LEAF_MAX_ENTRIES=370
        // With 2-byte length prefix overhead, we expect slightly less
        assert!(internal > 300, "internal capacity {internal} too low");
        assert!(leaf > 250, "leaf capacity {leaf} too low");

        // With max_key_size=256, capacity drops significantly but is still usable
        let internal_256 = var_internal_max_keys(256);
        let leaf_256 = var_leaf_max_entries(256);
        assert!(
            internal_256 > 10,
            "internal_256 capacity {internal_256} too low"
        );
        assert!(leaf_256 > 10, "leaf_256 capacity {leaf_256} too low");
    }
}
