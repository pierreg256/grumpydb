//! Generic B+Tree node types: `InternalNode<K>` and `LeafNode<K>`.
//!
//! The same node code path serves both UUID-keyed primary indexes and
//! variable-key secondary indexes. The `Key` trait abstracts over how a key
//! is encoded inside a slot, and how any per-tree configuration is persisted
//! on the node header.
//!
//! # Internal node layout
//!
//! ```text
//! Offset                                 Content
//! 0 .. PAGE_HEADER_SIZE                  PageHeader (page_type = BTreeInternal)
//! +0 .. +2                               num_keys: u16
//! +2 .. +6                               right_child: u32
//! +6 .. +6 + K::NODE_META_BYTES          per-node config (e.g. max_key_size)
//! +entries_start ..                      entries: [key + child_page_id(u32)] × num_keys
//! ```
//!
//! # Leaf node layout
//!
//! ```text
//! Offset                                 Content
//! 0 .. PAGE_HEADER_SIZE                  PageHeader (page_type = BTreeLeaf)
//! +0 .. +2                               num_entries: u16
//! +2 .. +6                               next_leaf: u32
//! +6 .. +10                              prev_leaf: u32
//! +10 .. +10 + K::NODE_META_BYTES        per-node config (e.g. max_key_size)
//! +entries_start ..                      entries: [key + page_id(u32) + slot_id(u16)] × n
//! ```

use crate::page::{PAGE_HEADER_SIZE, PAGE_SIZE, PageHeader, PageType};

use super::key::Key;

/// Bytes occupied by the internal node's standard header fields after the
/// page header (`num_keys` + `right_child`).
const INTERNAL_FIXED_HEADER: usize = 6;

/// Bytes occupied by the leaf node's standard header fields after the page
/// header (`num_entries` + `next_leaf` + `prev_leaf`).
const LEAF_FIXED_HEADER: usize = 10;

/// Size of an internal-node child pointer.
const CHILD_PTR_SIZE: usize = 4;

/// Size of a leaf-node data pointer (page_id + slot_id).
const DATA_PTR_SIZE: usize = 6;

/// Usable space inside a page (after the page header).
const USABLE_SPACE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

/// Minimum occupancy percentage before a node is considered underfull.
const MIN_OCCUPANCY_PERCENT: usize = 40;

/// Returns the byte offset where internal entries start in a node page.
#[inline]
fn internal_entries_start<K: Key>() -> usize {
    PAGE_HEADER_SIZE + INTERNAL_FIXED_HEADER + K::NODE_META_BYTES as usize
}

/// Returns the byte offset where leaf entries start in a node page.
#[inline]
fn leaf_entries_start<K: Key>() -> usize {
    PAGE_HEADER_SIZE + LEAF_FIXED_HEADER + K::NODE_META_BYTES as usize
}

/// Returns the maximum number of keys an internal node can hold.
pub fn internal_max_keys<K: Key>(cfg: K::Config) -> usize {
    let entry_size = K::slot_key_size(cfg) + CHILD_PTR_SIZE;
    let header_overhead = INTERNAL_FIXED_HEADER + K::NODE_META_BYTES as usize;
    (USABLE_SPACE - header_overhead) / entry_size
}

/// Returns the maximum number of entries a leaf can hold.
pub fn leaf_max_entries<K: Key>(cfg: K::Config) -> usize {
    let entry_size = K::slot_key_size(cfg) + DATA_PTR_SIZE;
    let header_overhead = LEAF_FIXED_HEADER + K::NODE_META_BYTES as usize;
    (USABLE_SPACE - header_overhead) / entry_size
}

/// Returns the minimum number of keys an internal node should keep before
/// being considered underfull (and thus eligible for merge/redistribute).
pub fn internal_min_keys<K: Key>(cfg: K::Config) -> usize {
    internal_max_keys::<K>(cfg) * MIN_OCCUPANCY_PERCENT / 100
}

/// Returns the minimum number of entries a leaf should keep before being
/// considered underfull.
pub fn leaf_min_entries<K: Key>(cfg: K::Config) -> usize {
    leaf_max_entries::<K>(cfg) * MIN_OCCUPANCY_PERCENT / 100
}

/// An entry inside an internal node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalEntry<K: Key> {
    pub key: K,
    pub child_page_id: u32,
}

/// An entry inside a leaf node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafEntry<K: Key> {
    pub key: K,
    pub page_id: u32,
    pub slot_id: u16,
}

/// A B+Tree internal node.
///
/// `entries[i].child_page_id` is the *left* child of `entries[i].key`, i.e.
/// the subtree containing keys `< entries[i].key` (and `>= entries[i-1].key`).
/// `right_child` is the subtree containing keys `>= entries[last].key`.
#[derive(Debug, Clone)]
pub struct InternalNode<K: Key> {
    pub page_id: u32,
    pub num_keys: u16,
    pub right_child: u32,
    pub config: K::Config,
    pub entries: Vec<InternalEntry<K>>,
}

impl<K: Key> InternalNode<K> {
    /// Creates a new empty internal node.
    pub fn new(page_id: u32, config: K::Config) -> Self {
        Self {
            page_id,
            num_keys: 0,
            right_child: 0,
            config,
            entries: Vec::new(),
        }
    }

    /// Deserialises an internal node from a page buffer.
    pub fn from_bytes(buf: &[u8; PAGE_SIZE]) -> Self {
        let header = PageHeader::read_from(buf);
        let num_keys = u16::from_le_bytes([buf[32], buf[33]]);
        let right_child = u32::from_le_bytes([buf[34], buf[35], buf[36], buf[37]]);
        let config = K::read_node_config(buf, PAGE_HEADER_SIZE + INTERNAL_FIXED_HEADER);

        let slot_key = K::slot_key_size(config);
        let slot_size = slot_key + CHILD_PTR_SIZE;
        let entries_start = internal_entries_start::<K>();

        let mut entries = Vec::with_capacity(num_keys as usize);
        for i in 0..num_keys as usize {
            let base = entries_start + i * slot_size;
            let key = K::read_key(&buf[base..base + slot_key], config);
            let p = base + slot_key;
            let child_page_id = u32::from_le_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]);
            entries.push(InternalEntry { key, child_page_id });
        }

        Self {
            page_id: header.page_id,
            num_keys,
            right_child,
            config,
            entries,
        }
    }

    /// Serialises the internal node into a page buffer.
    pub fn to_bytes(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(self.page_id, PageType::BTreeInternal);
        header.write_to(&mut buf);

        buf[32..34].copy_from_slice(&self.num_keys.to_le_bytes());
        buf[34..38].copy_from_slice(&self.right_child.to_le_bytes());
        K::write_node_config(
            self.config,
            &mut buf,
            PAGE_HEADER_SIZE + INTERNAL_FIXED_HEADER,
        );

        let slot_key = K::slot_key_size(self.config);
        let slot_size = slot_key + CHILD_PTR_SIZE;
        let entries_start = internal_entries_start::<K>();

        for (i, entry) in self.entries.iter().enumerate() {
            let base = entries_start + i * slot_size;
            entry
                .key
                .write_key(&mut buf[base..base + slot_key], self.config);
            let p = base + slot_key;
            buf[p..p + 4].copy_from_slice(&entry.child_page_id.to_le_bytes());
        }

        buf
    }

    /// Returns the child page id for the subtree that could contain `key`.
    pub fn find_child(&self, key: &K) -> u32 {
        // Find the first entry whose key is strictly greater than `key`.
        // `partition_point` returns the first index where the predicate
        // becomes false. Predicate `e.key <= key` becomes false at the first
        // entry where `e.key > key`.
        let pos = self.entries.partition_point(|e| e.key <= *key);
        if pos < self.entries.len() {
            self.entries[pos].child_page_id
        } else {
            self.right_child
        }
    }

    /// Inserts a separator key together with the page id of the new right
    /// child produced by a child split.
    pub fn insert_entry(&mut self, key: K, right_child_page_id: u32) {
        let pos = self
            .entries
            .binary_search_by(|e| e.key.cmp(&key))
            .unwrap_or_else(|i| i);

        if pos < self.entries.len() {
            // Inserting before an existing entry: the existing left pointer at
            // `pos` becomes the left pointer of the inserted (promoted) key,
            // and the shifted entry takes the new right pointer.
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
            // Inserting at the end: the current `right_child` becomes the
            // left pointer of the promoted key and the new pointer takes its
            // place.
            self.entries.push(InternalEntry {
                key,
                child_page_id: self.right_child,
            });
            self.right_child = right_child_page_id;
        }
        self.num_keys += 1;
    }

    /// True if the node is overfull and must be split.
    pub fn is_overfull(&self) -> bool {
        self.num_keys as usize > internal_max_keys::<K>(self.config)
    }

    /// True if the node is below the minimum occupancy threshold.
    pub fn is_underfull(&self) -> bool {
        (self.num_keys as usize) < internal_min_keys::<K>(self.config)
    }
}

/// A B+Tree leaf node.
///
/// Leaves form a doubly-linked list (via `next_leaf` / `prev_leaf`) for fast
/// sequential range scans.
#[derive(Debug, Clone)]
pub struct LeafNode<K: Key> {
    pub page_id: u32,
    pub num_entries: u16,
    pub next_leaf: u32,
    pub prev_leaf: u32,
    pub config: K::Config,
    pub entries: Vec<LeafEntry<K>>,
}

impl<K: Key> LeafNode<K> {
    /// Creates a new empty leaf.
    pub fn new(page_id: u32, config: K::Config) -> Self {
        Self {
            page_id,
            num_entries: 0,
            next_leaf: 0,
            prev_leaf: 0,
            config,
            entries: Vec::new(),
        }
    }

    /// Deserialises a leaf from a page buffer.
    pub fn from_bytes(buf: &[u8; PAGE_SIZE]) -> Self {
        let header = PageHeader::read_from(buf);
        let num_entries = u16::from_le_bytes([buf[32], buf[33]]);
        let next_leaf = u32::from_le_bytes([buf[34], buf[35], buf[36], buf[37]]);
        let prev_leaf = u32::from_le_bytes([buf[38], buf[39], buf[40], buf[41]]);
        let config = K::read_node_config(buf, PAGE_HEADER_SIZE + LEAF_FIXED_HEADER);

        let slot_key = K::slot_key_size(config);
        let slot_size = slot_key + DATA_PTR_SIZE;
        let entries_start = leaf_entries_start::<K>();

        let mut entries = Vec::with_capacity(num_entries as usize);
        for i in 0..num_entries as usize {
            let base = entries_start + i * slot_size;
            let key = K::read_key(&buf[base..base + slot_key], config);
            let p = base + slot_key;
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
            config,
            entries,
        }
    }

    /// Serialises the leaf into a page buffer.
    pub fn to_bytes(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        let header = PageHeader::new(self.page_id, PageType::BTreeLeaf);
        header.write_to(&mut buf);

        buf[32..34].copy_from_slice(&self.num_entries.to_le_bytes());
        buf[34..38].copy_from_slice(&self.next_leaf.to_le_bytes());
        buf[38..42].copy_from_slice(&self.prev_leaf.to_le_bytes());
        K::write_node_config(self.config, &mut buf, PAGE_HEADER_SIZE + LEAF_FIXED_HEADER);

        let slot_key = K::slot_key_size(self.config);
        let slot_size = slot_key + DATA_PTR_SIZE;
        let entries_start = leaf_entries_start::<K>();

        for (i, entry) in self.entries.iter().enumerate() {
            let base = entries_start + i * slot_size;
            entry
                .key
                .write_key(&mut buf[base..base + slot_key], self.config);
            let p = base + slot_key;
            buf[p..p + 4].copy_from_slice(&entry.page_id.to_le_bytes());
            buf[p + 4..p + 6].copy_from_slice(&entry.slot_id.to_le_bytes());
        }

        buf
    }

    /// Searches for `key` and returns its position if found.
    pub fn search(&self, key: &K) -> Option<usize> {
        self.entries.binary_search_by(|e| e.key.cmp(key)).ok()
    }

    /// Inserts an entry in sorted order. Does *not* check for overflow.
    pub fn insert_entry(&mut self, entry: LeafEntry<K>) {
        let pos = self
            .entries
            .binary_search_by(|e| e.key.cmp(&entry.key))
            .unwrap_or_else(|i| i);
        self.entries.insert(pos, entry);
        self.num_entries += 1;
    }

    /// Removes the entry whose key matches `key`. Returns the removed entry,
    /// if any.
    pub fn remove_entry(&mut self, key: &K) -> Option<LeafEntry<K>> {
        if let Ok(idx) = self.entries.binary_search_by(|e| e.key.cmp(key)) {
            self.num_entries -= 1;
            Some(self.entries.remove(idx))
        } else {
            None
        }
    }

    /// True if the node is overfull and must be split.
    pub fn is_overfull(&self) -> bool {
        self.num_entries as usize > leaf_max_entries::<K>(self.config)
    }

    /// True if the node is below the minimum occupancy threshold.
    pub fn is_underfull(&self) -> bool {
        (self.num_entries as usize) < leaf_min_entries::<K>(self.config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn uuid_from(val: u8) -> Uuid {
        let mut k = [0u8; 16];
        k[15] = val;
        Uuid::from_bytes(k)
    }

    // ─── Uuid variant ──────────────────────────────────────────────────

    #[test]
    fn test_internal_node_uuid_round_trip() {
        let mut node = InternalNode::<Uuid>::new(5, ());
        node.right_child = 99;
        node.entries.push(InternalEntry {
            key: uuid_from(10),
            child_page_id: 2,
        });
        node.entries.push(InternalEntry {
            key: uuid_from(20),
            child_page_id: 3,
        });
        node.num_keys = 2;

        let buf = node.to_bytes();
        let restored: InternalNode<Uuid> = InternalNode::from_bytes(&buf);

        assert_eq!(restored.page_id, 5);
        assert_eq!(restored.num_keys, 2);
        assert_eq!(restored.right_child, 99);
        assert_eq!(restored.entries[0].key, uuid_from(10));
        assert_eq!(restored.entries[0].child_page_id, 2);
        assert_eq!(restored.entries[1].key, uuid_from(20));
        assert_eq!(restored.entries[1].child_page_id, 3);
    }

    #[test]
    fn test_leaf_node_uuid_round_trip() {
        let mut node = LeafNode::<Uuid>::new(7, ());
        node.next_leaf = 8;
        node.prev_leaf = 6;
        node.entries.push(LeafEntry {
            key: uuid_from(5),
            page_id: 100,
            slot_id: 0,
        });
        node.entries.push(LeafEntry {
            key: uuid_from(15),
            page_id: 101,
            slot_id: 3,
        });
        node.num_entries = 2;

        let buf = node.to_bytes();
        let restored: LeafNode<Uuid> = LeafNode::from_bytes(&buf);

        assert_eq!(restored.page_id, 7);
        assert_eq!(restored.num_entries, 2);
        assert_eq!(restored.next_leaf, 8);
        assert_eq!(restored.prev_leaf, 6);
        assert_eq!(restored.entries[0].key, uuid_from(5));
        assert_eq!(restored.entries[0].page_id, 100);
        assert_eq!(restored.entries[1].slot_id, 3);
    }

    #[test]
    fn test_internal_uuid_max_capacity() {
        let max_keys = internal_max_keys::<Uuid>(());
        let mut node = InternalNode::<Uuid>::new(1, ());
        node.right_child = 999;
        for i in 0..max_keys {
            let mut k = [0u8; 16];
            k[14..16].copy_from_slice(&(i as u16).to_le_bytes());
            node.entries.push(InternalEntry {
                key: Uuid::from_bytes(k),
                child_page_id: i as u32 + 1,
            });
        }
        node.num_keys = max_keys as u16;

        let buf = node.to_bytes();
        let restored: InternalNode<Uuid> = InternalNode::from_bytes(&buf);
        assert_eq!(restored.num_keys as usize, max_keys);
        assert_eq!(restored.entries.len(), max_keys);
    }

    #[test]
    fn test_leaf_uuid_max_capacity() {
        let max_entries = leaf_max_entries::<Uuid>(());
        let mut node = LeafNode::<Uuid>::new(1, ());
        for i in 0..max_entries {
            let mut k = [0u8; 16];
            k[14..16].copy_from_slice(&(i as u16).to_le_bytes());
            node.entries.push(LeafEntry {
                key: Uuid::from_bytes(k),
                page_id: i as u32,
                slot_id: 0,
            });
        }
        node.num_entries = max_entries as u16;

        let buf = node.to_bytes();
        let restored: LeafNode<Uuid> = LeafNode::from_bytes(&buf);
        assert_eq!(restored.num_entries as usize, max_entries);
        assert_eq!(restored.entries.len(), max_entries);
    }

    #[test]
    fn test_internal_uuid_find_child() {
        let mut node = InternalNode::<Uuid>::new(1, ());
        node.entries = vec![
            InternalEntry {
                key: uuid_from(10),
                child_page_id: 100,
            },
            InternalEntry {
                key: uuid_from(20),
                child_page_id: 101,
            },
            InternalEntry {
                key: uuid_from(30),
                child_page_id: 102,
            },
        ];
        node.num_keys = 3;
        node.right_child = 103;

        assert_eq!(node.find_child(&uuid_from(5)), 100); // < 10 → c0
        assert_eq!(node.find_child(&uuid_from(10)), 101); // >= 10, < 20 → c1
        assert_eq!(node.find_child(&uuid_from(15)), 101);
        assert_eq!(node.find_child(&uuid_from(20)), 102);
        assert_eq!(node.find_child(&uuid_from(25)), 102);
        assert_eq!(node.find_child(&uuid_from(30)), 103); // >= 30 → right_child
        assert_eq!(node.find_child(&uuid_from(99)), 103);
    }

    #[test]
    fn test_leaf_uuid_search_and_insert_sorted() {
        let mut node = LeafNode::<Uuid>::new(1, ());
        node.insert_entry(LeafEntry {
            key: uuid_from(30),
            page_id: 1,
            slot_id: 0,
        });
        node.insert_entry(LeafEntry {
            key: uuid_from(10),
            page_id: 2,
            slot_id: 0,
        });
        node.insert_entry(LeafEntry {
            key: uuid_from(20),
            page_id: 3,
            slot_id: 0,
        });
        assert_eq!(node.entries[0].key, uuid_from(10));
        assert_eq!(node.entries[1].key, uuid_from(20));
        assert_eq!(node.entries[2].key, uuid_from(30));
        assert_eq!(node.search(&uuid_from(20)), Some(1));
        assert_eq!(node.search(&uuid_from(5)), None);
    }

    #[test]
    fn test_leaf_uuid_remove_entry() {
        let mut node = LeafNode::<Uuid>::new(1, ());
        for v in [10, 20, 30] {
            node.insert_entry(LeafEntry {
                key: uuid_from(v),
                page_id: v as u32,
                slot_id: 0,
            });
        }
        let removed = node.remove_entry(&uuid_from(20));
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().page_id, 20);
        assert_eq!(node.num_entries, 2);
        assert!(node.search(&uuid_from(20)).is_none());
        assert!(node.remove_entry(&uuid_from(99)).is_none());
    }

    #[test]
    fn test_uuid_capacity_constants_sane() {
        let i = internal_max_keys::<Uuid>(());
        let l = leaf_max_entries::<Uuid>(());
        assert!(i > 100, "fan-out too small: {i}");
        assert!(l > 100, "leaf cap too small: {l}");
    }

    // ─── Vec<u8> variant ───────────────────────────────────────────────

    #[test]
    fn test_internal_vec_round_trip() {
        let mut node = InternalNode::<Vec<u8>>::new(5, 32);
        node.right_child = 99;
        node.entries.push(InternalEntry {
            key: b"alpha".to_vec(),
            child_page_id: 2,
        });
        node.entries.push(InternalEntry {
            key: b"beta".to_vec(),
            child_page_id: 3,
        });
        node.num_keys = 2;

        let buf = node.to_bytes();
        let restored: InternalNode<Vec<u8>> = InternalNode::from_bytes(&buf);

        assert_eq!(restored.page_id, 5);
        assert_eq!(restored.num_keys, 2);
        assert_eq!(restored.right_child, 99);
        assert_eq!(restored.config, 32);
        assert_eq!(restored.entries[0].key, b"alpha");
        assert_eq!(restored.entries[0].child_page_id, 2);
        assert_eq!(restored.entries[1].key, b"beta");
        assert_eq!(restored.entries[1].child_page_id, 3);
    }

    #[test]
    fn test_leaf_vec_round_trip() {
        let mut node = LeafNode::<Vec<u8>>::new(7, 32);
        node.next_leaf = 8;
        node.prev_leaf = 6;
        node.entries.push(LeafEntry {
            key: b"apple".to_vec(),
            page_id: 100,
            slot_id: 0,
        });
        node.entries.push(LeafEntry {
            key: b"banana".to_vec(),
            page_id: 101,
            slot_id: 3,
        });
        node.num_entries = 2;

        let buf = node.to_bytes();
        let restored: LeafNode<Vec<u8>> = LeafNode::from_bytes(&buf);

        assert_eq!(restored.page_id, 7);
        assert_eq!(restored.num_entries, 2);
        assert_eq!(restored.next_leaf, 8);
        assert_eq!(restored.prev_leaf, 6);
        assert_eq!(restored.config, 32);
        assert_eq!(restored.entries[0].key, b"apple");
        assert_eq!(restored.entries[0].page_id, 100);
        assert_eq!(restored.entries[1].key, b"banana");
        assert_eq!(restored.entries[1].slot_id, 3);
    }

    #[test]
    fn test_internal_vec_max_capacity() {
        let max_keys = internal_max_keys::<Vec<u8>>(32);
        assert!(max_keys > 10);
        let mut node = InternalNode::<Vec<u8>>::new(1, 32);
        node.right_child = 999;
        for i in 0..max_keys {
            node.entries.push(InternalEntry {
                key: format!("key_{i:06}").into_bytes(),
                child_page_id: i as u32 + 1,
            });
        }
        node.num_keys = max_keys as u16;

        let buf = node.to_bytes();
        let restored: InternalNode<Vec<u8>> = InternalNode::from_bytes(&buf);
        assert_eq!(restored.num_keys as usize, max_keys);
        assert_eq!(restored.entries.len(), max_keys);
    }

    #[test]
    fn test_leaf_vec_max_capacity() {
        let max_entries = leaf_max_entries::<Vec<u8>>(32);
        assert!(max_entries > 10);
        let mut node = LeafNode::<Vec<u8>>::new(1, 32);
        for i in 0..max_entries {
            node.entries.push(LeafEntry {
                key: format!("key_{i:06}").into_bytes(),
                page_id: i as u32,
                slot_id: 0,
            });
        }
        node.num_entries = max_entries as u16;

        let buf = node.to_bytes();
        let restored: LeafNode<Vec<u8>> = LeafNode::from_bytes(&buf);
        assert_eq!(restored.num_entries as usize, max_entries);
    }

    #[test]
    fn test_internal_vec_find_child() {
        let mut node = InternalNode::<Vec<u8>>::new(1, 32);
        node.entries = vec![
            InternalEntry {
                key: b"delta".to_vec(),
                child_page_id: 100,
            },
            InternalEntry {
                key: b"hotel".to_vec(),
                child_page_id: 101,
            },
            InternalEntry {
                key: b"mike".to_vec(),
                child_page_id: 102,
            },
        ];
        node.num_keys = 3;
        node.right_child = 103;

        assert_eq!(node.find_child(&b"alpha".to_vec()), 100);
        assert_eq!(node.find_child(&b"delta".to_vec()), 101);
        assert_eq!(node.find_child(&b"foxtrot".to_vec()), 101);
        assert_eq!(node.find_child(&b"hotel".to_vec()), 102);
        assert_eq!(node.find_child(&b"zulu".to_vec()), 103);
    }

    #[test]
    fn test_vec_capacity_calculations_match_legacy() {
        // The previous fixed-Uuid path had INTERNAL_MAX_KEYS=407 and
        // LEAF_MAX_ENTRIES=370. The Vec<u8>(max=16) layout pays an extra
        // 2-byte length prefix and an extra 2-byte node meta, so capacity is
        // a touch lower but still in the same ballpark.
        let internal16 = internal_max_keys::<Vec<u8>>(16);
        let leaf16 = leaf_max_entries::<Vec<u8>>(16);
        assert!(internal16 > 300, "internal16 = {internal16}");
        assert!(leaf16 > 250, "leaf16 = {leaf16}");

        // With max=256 capacity drops but stays usable.
        let internal_big = internal_max_keys::<Vec<u8>>(256);
        let leaf_big = leaf_max_entries::<Vec<u8>>(256);
        assert!(internal_big > 10);
        assert!(leaf_big > 10);
    }
}
