//! Generic B+Tree operations: search, insert (with split), delete (with
//! merge/redistribute).
//!
//! All operations are written once and parameterised over `K: Key`.

use crate::error::{GrumpyError, Result};
use crate::page::{PageHeader, PageType};

use super::BTree;
use super::key::Key;
use super::node::{
    InternalEntry, InternalNode, LeafEntry, LeafNode, internal_min_keys, leaf_min_entries,
};

/// Path of `(internal_page_id, child_index_used)` pairs from the root down
/// to a leaf, recorded during a descending search so splits and merges can
/// be propagated back up the tree.
type DescentPath = Vec<(u32, usize)>;

/// Lazily-typed result of loading a page that may be either node kind.
enum NodeRef<K: Key> {
    Internal(InternalNode<K>),
    Leaf(LeafNode<K>),
}

impl<K: Key> BTree<K> {
    // ── Helpers ─────────────────────────────────────────────────────────

    /// Loads a node from disk, detecting its kind from the page header.
    fn load_node(&mut self, page_id: u32) -> Result<NodeRef<K>> {
        let buf = self.pm.read_page(page_id)?;
        let header = PageHeader::read_from(&buf);
        match header.page_type {
            PageType::BTreeInternal => Ok(NodeRef::Internal(InternalNode::from_bytes(&buf))),
            PageType::BTreeLeaf => Ok(NodeRef::Leaf(LeafNode::from_bytes(&buf))),
            _ => Err(GrumpyError::PageNotFound(page_id)),
        }
    }

    fn save_internal(&mut self, node: &InternalNode<K>) -> Result<()> {
        self.pm.write_page(node.page_id, &node.to_bytes())
    }

    fn save_leaf(&mut self, node: &LeafNode<K>) -> Result<()> {
        self.pm.write_page(node.page_id, &node.to_bytes())
    }

    fn alloc_page(&mut self) -> Result<u32> {
        self.pm.allocate_page()
    }

    // ── Search ──────────────────────────────────────────────────────────

    /// Searches for `key` and returns its `(page_id, slot_id)` if present.
    pub fn search(&mut self, key: &K) -> Result<Option<(u32, u16)>> {
        let leaf = self.find_leaf(key)?;
        if let Some(idx) = leaf.search(key) {
            let entry = &leaf.entries[idx];
            Ok(Some((entry.page_id, entry.slot_id)))
        } else {
            Ok(None)
        }
    }

    /// Descends the tree to find the leaf node that would contain `key`.
    pub(crate) fn find_leaf(&mut self, key: &K) -> Result<LeafNode<K>> {
        let mut current = self.meta.root_page_id;
        loop {
            match self.load_node(current)? {
                NodeRef::Leaf(leaf) => return Ok(leaf),
                NodeRef::Internal(internal) => {
                    current = internal.find_child(key);
                }
            }
        }
    }

    /// Descends to the leaf and records the path of `(internal_page_id,
    /// child_index_used)` taken at each level. The path is needed to
    /// propagate splits and merges back up.
    fn find_leaf_with_path(&mut self, key: &K) -> Result<(LeafNode<K>, DescentPath)> {
        let mut path = Vec::new();
        let mut current = self.meta.root_page_id;
        loop {
            match self.load_node(current)? {
                NodeRef::Leaf(leaf) => return Ok((leaf, path)),
                NodeRef::Internal(internal) => {
                    let child_idx = find_child_index(&internal, key);
                    let child_page = get_child_at(&internal, child_idx);
                    path.push((current, child_idx));
                    current = child_page;
                }
            }
        }
    }

    // ── Insert ──────────────────────────────────────────────────────────

    /// Inserts a key → `(page_id, slot_id)` mapping.
    ///
    /// Returns `K::duplicate_key_error()` if the key already exists.
    pub fn insert(&mut self, key: K, page_id: u32, slot_id: u16) -> Result<()> {
        let (mut leaf, path) = self.find_leaf_with_path(&key)?;

        if leaf.search(&key).is_some() {
            return Err(key.duplicate_key_error());
        }

        leaf.insert_entry(LeafEntry {
            key: key.clone(),
            page_id,
            slot_id,
        });

        if !leaf.is_overfull() {
            self.save_leaf(&leaf)?;
            self.meta.num_entries += 1;
            self.flush_meta()?;
            return Ok(());
        }

        // Leaf overfull → split.
        let (left, right, promoted_key) = self.split_leaf(leaf)?;
        self.save_leaf(&left)?;
        self.save_leaf(&right)?;

        // Update doubly-linked list neighbour, if any.
        if right.next_leaf != 0 {
            let buf = self.pm.read_page(right.next_leaf)?;
            let mut next: LeafNode<K> = LeafNode::from_bytes(&buf);
            next.prev_leaf = right.page_id;
            self.save_leaf(&next)?;
        }

        self.propagate_split(path, promoted_key, left.page_id, right.page_id)?;
        self.meta.num_entries += 1;
        self.flush_meta()?;
        Ok(())
    }

    /// Splits a full leaf in two. The original page id stays with the left
    /// half; a fresh page is allocated for the right. Returns the median key
    /// (== first key of the right half) for promotion.
    fn split_leaf(&mut self, mut full_leaf: LeafNode<K>) -> Result<(LeafNode<K>, LeafNode<K>, K)> {
        let mid = full_leaf.entries.len() / 2;
        let right_entries: Vec<LeafEntry<K>> = full_leaf.entries.drain(mid..).collect();
        full_leaf.num_entries = full_leaf.entries.len() as u16;

        let right_page_id = self.alloc_page()?;
        let mut right: LeafNode<K> = LeafNode::new(right_page_id, full_leaf.config);
        right.entries = right_entries;
        right.num_entries = right.entries.len() as u16;

        // Linked-list pointers
        right.next_leaf = full_leaf.next_leaf;
        full_leaf.next_leaf = right_page_id;
        right.prev_leaf = full_leaf.page_id;

        let promoted_key = right.entries[0].key.clone();
        Ok((full_leaf, right, promoted_key))
    }

    /// Splits a full internal node in two. The median key is *promoted* (it
    /// remains in neither child).
    fn split_internal(
        &mut self,
        mut full_node: InternalNode<K>,
    ) -> Result<(InternalNode<K>, InternalNode<K>, K)> {
        let mid = full_node.entries.len() / 2;
        let promoted_key = full_node.entries[mid].key.clone();

        // Everything strictly after `mid` goes to the right node.
        let right_entries: Vec<InternalEntry<K>> = full_node.entries.drain(mid + 1..).collect();
        // Pop the mid entry (it's the new last entry of the trimmed left vec):
        // its child pointer becomes left's new `right_child` (left of promoted
        // key).
        let mid_entry = full_node
            .entries
            .pop()
            .ok_or_else(|| GrumpyError::Corruption("split_internal: empty entries".into()))?;
        full_node.num_keys = full_node.entries.len() as u16;

        let right_page_id = self.alloc_page()?;
        let mut right: InternalNode<K> = InternalNode::new(right_page_id, full_node.config);
        right.entries = right_entries;
        right.num_keys = right.entries.len() as u16;

        right.right_child = full_node.right_child;
        full_node.right_child = mid_entry.child_page_id;

        Ok((full_node, right, promoted_key))
    }

    /// Walks back up the descent path, inserting the promoted separator at
    /// each level and splitting further if necessary.
    fn propagate_split(
        &mut self,
        mut path: Vec<(u32, usize)>,
        mut promoted_key: K,
        _left_page_id: u32,
        mut right_page_id: u32,
    ) -> Result<()> {
        while let Some((parent_page_id, _child_idx)) = path.pop() {
            let buf = self.pm.read_page(parent_page_id)?;
            let mut parent: InternalNode<K> = InternalNode::from_bytes(&buf);

            parent.insert_entry(promoted_key.clone(), right_page_id);

            if !parent.is_overfull() {
                self.save_internal(&parent)?;
                return Ok(());
            }

            let (left_int, right_int, new_promoted) = self.split_internal(parent)?;
            self.save_internal(&left_int)?;
            self.save_internal(&right_int)?;

            promoted_key = new_promoted;
            right_page_id = right_int.page_id;
        }

        // Path exhausted → grow the tree by one level.
        let new_root_page_id = self.alloc_page()?;
        let mut new_root: InternalNode<K> = InternalNode::new(new_root_page_id, self.meta.config);
        new_root.entries.push(InternalEntry {
            key: promoted_key,
            child_page_id: self.meta.root_page_id, // old root is the left child
        });
        new_root.num_keys = 1;
        new_root.right_child = right_page_id;
        self.save_internal(&new_root)?;

        self.meta.root_page_id = new_root_page_id;
        self.meta.height += 1;
        Ok(())
    }

    // ── Delete ──────────────────────────────────────────────────────────

    /// Deletes `key` from the tree.
    ///
    /// Returns `K::key_not_found_error()` if the key does not exist.
    pub fn delete(&mut self, key: &K) -> Result<()> {
        let (mut leaf, path) = self.find_leaf_with_path(key)?;

        if leaf.remove_entry(key).is_none() {
            return Err(key.key_not_found_error());
        }

        self.save_leaf(&leaf)?;
        self.meta.num_entries -= 1;

        // Root leaf is allowed to be empty; otherwise rebalance if underfull.
        if path.is_empty() || !leaf.is_underfull() {
            self.flush_meta()?;
            return Ok(());
        }

        self.rebalance_leaf(leaf, path)?;
        self.flush_meta()?;
        Ok(())
    }

    fn rebalance_leaf(&mut self, leaf: LeafNode<K>, mut path: Vec<(u32, usize)>) -> Result<()> {
        let (parent_page_id, child_idx) = path
            .pop()
            .ok_or_else(|| GrumpyError::Corruption("rebalance_leaf: empty path".into()))?;
        let buf = self.pm.read_page(parent_page_id)?;
        let mut parent: InternalNode<K> = InternalNode::from_bytes(&buf);
        let num_children = parent.num_keys as usize + 1;
        let min = leaf_min_entries::<K>(self.meta.config);

        // Try the left sibling first.
        if child_idx > 0 {
            let left_id = get_child_at(&parent, child_idx - 1);
            let buf = self.pm.read_page(left_id)?;
            let mut left_sib: LeafNode<K> = LeafNode::from_bytes(&buf);

            if left_sib.entries.len() > min {
                return self.redistribute_leaf_from_left(
                    &mut left_sib,
                    leaf,
                    &mut parent,
                    child_idx,
                );
            }
            return self.merge_leaves(left_sib, leaf, &mut parent, child_idx, path);
        }

        // Otherwise the right sibling.
        if child_idx + 1 < num_children {
            let right_id = get_child_at(&parent, child_idx + 1);
            let buf = self.pm.read_page(right_id)?;
            let mut right_sib: LeafNode<K> = LeafNode::from_bytes(&buf);

            if right_sib.entries.len() > min {
                return self.redistribute_leaf_from_right(
                    leaf,
                    &mut right_sib,
                    &mut parent,
                    child_idx,
                );
            }
            return self.merge_leaves(leaf, right_sib, &mut parent, child_idx + 1, path);
        }

        Ok(())
    }

    fn redistribute_leaf_from_left(
        &mut self,
        left: &mut LeafNode<K>,
        mut leaf: LeafNode<K>,
        parent: &mut InternalNode<K>,
        child_idx: usize,
    ) -> Result<()> {
        let moved = left.entries.pop().ok_or_else(|| {
            GrumpyError::Corruption("redistribute_leaf_from_left: empty left sibling".into())
        })?;
        left.num_entries -= 1;
        leaf.insert_entry(moved);

        let sep_idx = child_idx - 1;
        parent.entries[sep_idx].key = leaf.entries[0].key.clone();

        self.save_leaf(left)?;
        self.save_leaf(&leaf)?;
        self.save_internal(parent)?;
        Ok(())
    }

    fn redistribute_leaf_from_right(
        &mut self,
        mut leaf: LeafNode<K>,
        right: &mut LeafNode<K>,
        parent: &mut InternalNode<K>,
        child_idx: usize,
    ) -> Result<()> {
        let moved = right.entries.remove(0);
        right.num_entries -= 1;
        leaf.insert_entry(moved);

        if child_idx < parent.entries.len() {
            parent.entries[child_idx].key = right.entries[0].key.clone();
        }

        self.save_leaf(&leaf)?;
        self.save_leaf(right)?;
        self.save_internal(parent)?;
        Ok(())
    }

    fn merge_leaves(
        &mut self,
        mut left: LeafNode<K>,
        right: LeafNode<K>,
        parent: &mut InternalNode<K>,
        right_child_idx: usize,
        path: Vec<(u32, usize)>,
    ) -> Result<()> {
        for entry in right.entries.iter() {
            left.insert_entry(entry.clone());
        }

        // Linked-list maintenance.
        left.next_leaf = right.next_leaf;
        if right.next_leaf != 0 {
            let buf = self.pm.read_page(right.next_leaf)?;
            let mut next: LeafNode<K> = LeafNode::from_bytes(&buf);
            next.prev_leaf = left.page_id;
            self.save_leaf(&next)?;
        }

        self.pm.free_page(right.page_id)?;
        self.save_leaf(&left)?;

        self.remove_from_internal(parent, right_child_idx, path)
    }

    /// Removes a child reference from an internal node after a merge.
    fn remove_from_internal(
        &mut self,
        parent: &mut InternalNode<K>,
        merged_child_idx: usize,
        path: Vec<(u32, usize)>,
    ) -> Result<()> {
        // Build flat children/keys arrays for cleaner manipulation.
        let mut children: Vec<u32> = parent.entries.iter().map(|e| e.child_page_id).collect();
        children.push(parent.right_child);
        let mut keys: Vec<K> = parent.entries.iter().map(|e| e.key.clone()).collect();

        children.remove(merged_child_idx);
        let sep_idx = if merged_child_idx > 0 {
            merged_child_idx - 1
        } else {
            0
        };
        if !keys.is_empty() {
            keys.remove(sep_idx);
        }

        // Rebuild entries from the flat arrays.
        parent.entries.clear();
        for (i, key) in keys.into_iter().enumerate() {
            parent.entries.push(InternalEntry {
                key,
                child_page_id: children[i],
            });
        }
        parent.right_child = *children.last().unwrap_or(&0);
        parent.num_keys = parent.entries.len() as u16;

        // Root special case: parent has no separators left → its single
        // remaining child becomes the new root.
        if path.is_empty() && parent.num_keys == 0 {
            let new_root_id = if !parent.entries.is_empty() {
                parent.entries[0].child_page_id
            } else {
                parent.right_child
            };
            self.pm.free_page(parent.page_id)?;
            self.meta.root_page_id = new_root_id;
            if self.meta.height > 1 {
                self.meta.height -= 1;
            }
            return Ok(());
        }

        self.save_internal(parent)?;

        // Recurse upward if the parent itself is now underfull.
        if !path.is_empty() && parent.is_underfull() {
            self.rebalance_internal(parent.clone(), path)?;
        }

        Ok(())
    }

    fn rebalance_internal(
        &mut self,
        node: InternalNode<K>,
        mut path: Vec<(u32, usize)>,
    ) -> Result<()> {
        let (gp_page_id, child_idx) = path
            .pop()
            .ok_or_else(|| GrumpyError::Corruption("rebalance_internal: empty path".into()))?;
        let buf = self.pm.read_page(gp_page_id)?;
        let mut gp: InternalNode<K> = InternalNode::from_bytes(&buf);
        let num_children = gp.num_keys as usize + 1;
        let min = internal_min_keys::<K>(self.meta.config);

        if child_idx > 0 {
            let left_id = get_child_at(&gp, child_idx - 1);
            let buf = self.pm.read_page(left_id)?;
            let left: InternalNode<K> = InternalNode::from_bytes(&buf);

            if left.entries.len() > min {
                return self.redistribute_internal_from_left(left, node, &mut gp, child_idx);
            }
            return self.merge_internal(left, node, &mut gp, child_idx, path);
        }

        if child_idx + 1 < num_children {
            let right_id = get_child_at(&gp, child_idx + 1);
            let buf = self.pm.read_page(right_id)?;
            let right: InternalNode<K> = InternalNode::from_bytes(&buf);

            if right.entries.len() > min {
                return self.redistribute_internal_from_right(node, right, &mut gp, child_idx);
            }
            return self.merge_internal(node, right, &mut gp, child_idx + 1, path);
        }

        Ok(())
    }

    fn redistribute_internal_from_left(
        &mut self,
        mut left: InternalNode<K>,
        mut node: InternalNode<K>,
        gp: &mut InternalNode<K>,
        child_idx: usize,
    ) -> Result<()> {
        let sep_idx = child_idx - 1;
        let separator_key = gp.entries[sep_idx].key.clone();

        // Pull the separator down as the first entry of `node`. Its child
        // pointer is the rightmost child of `left` (which currently sits
        // between sibling and `node`).
        node.entries.insert(
            0,
            InternalEntry {
                key: separator_key,
                child_page_id: left.right_child,
            },
        );
        node.num_keys += 1;

        // Promote `left`'s last entry up to the grandparent.
        let moved_up = left.entries.pop().ok_or_else(|| {
            GrumpyError::Corruption("redistribute_internal_from_left: empty left sibling".into())
        })?;
        left.num_keys -= 1;
        left.right_child = moved_up.child_page_id;
        gp.entries[sep_idx].key = moved_up.key;

        self.save_internal(&left)?;
        self.save_internal(&node)?;
        self.save_internal(gp)?;
        Ok(())
    }

    fn redistribute_internal_from_right(
        &mut self,
        mut node: InternalNode<K>,
        mut right: InternalNode<K>,
        gp: &mut InternalNode<K>,
        child_idx: usize,
    ) -> Result<()> {
        let sep_idx = child_idx; // separator between `node` and `right`
        if sep_idx >= gp.entries.len() {
            // Nothing to rebalance through the right sibling at this level.
            return Ok(());
        }
        let separator_key = gp.entries[sep_idx].key.clone();

        // Pull separator down to be the new last entry of `node`.
        node.entries.push(InternalEntry {
            key: separator_key,
            child_page_id: node.right_child,
        });
        node.num_keys += 1;

        // First entry of `right` migrates upward as the new separator.
        let moved_up = right.entries.remove(0);
        right.num_keys -= 1;
        node.right_child = moved_up.child_page_id;
        gp.entries[sep_idx].key = moved_up.key;

        self.save_internal(&node)?;
        self.save_internal(&right)?;
        self.save_internal(gp)?;
        Ok(())
    }

    fn merge_internal(
        &mut self,
        mut left: InternalNode<K>,
        right: InternalNode<K>,
        gp: &mut InternalNode<K>,
        right_child_idx: usize,
        path: Vec<(u32, usize)>,
    ) -> Result<()> {
        let sep_idx = if right_child_idx > 0 {
            right_child_idx - 1
        } else {
            0
        };
        let separator_key = gp.entries[sep_idx].key.clone();

        // Pull the separator down between the two children.
        left.entries.push(InternalEntry {
            key: separator_key,
            child_page_id: left.right_child,
        });
        left.num_keys += 1;

        // Append all of `right`'s entries.
        for entry in right.entries.iter() {
            left.entries.push(entry.clone());
            left.num_keys += 1;
        }
        left.right_child = right.right_child;

        self.pm.free_page(right.page_id)?;
        self.save_internal(&left)?;

        self.remove_from_internal(gp, right_child_idx, path)
    }
}

// ── Free helpers ────────────────────────────────────────────────────────

/// Returns the child index for the subtree that could contain `key`.
///
/// Convention: `entries[i].child_page_id` is the *left* child of
/// `entries[i].key`. Index `i` means we descend through
/// `entries[i].child_page_id` (for keys `< entries[i].key`); index
/// `entries.len()` means we descend through `right_child`.
fn find_child_index<K: Key>(node: &InternalNode<K>, key: &K) -> usize {
    node.entries.partition_point(|e| e.key <= *key)
}

/// Returns the child page id at the given index (handles `right_child`).
fn get_child_at<K: Key>(node: &InternalNode<K>, child_idx: usize) -> u32 {
    if child_idx < node.entries.len() {
        node.entries[child_idx].child_page_id
    } else {
        node.right_child
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::node::{internal_max_keys, leaf_max_entries};
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
    fn test_uuid_search_empty_tree() {
        let (_dir, mut btree) = uuid_setup();
        assert!(btree.search(&make_uuid(1)).unwrap().is_none());
    }

    #[test]
    fn test_uuid_insert_and_search_single() {
        let (_dir, mut btree) = uuid_setup();
        let key = make_uuid(42);
        btree.insert(key, 10, 5).unwrap();
        assert_eq!(btree.search(&key).unwrap(), Some((10, 5)));
        assert_eq!(btree.len(), 1);
    }

    #[test]
    fn test_uuid_insert_duplicate_key() {
        let (_dir, mut btree) = uuid_setup();
        let key = make_uuid(42);
        btree.insert(key, 10, 5).unwrap();
        let result = btree.insert(key, 20, 0);
        assert!(matches!(result, Err(GrumpyError::DuplicateKey(_))));
    }

    #[test]
    fn test_uuid_insert_multiple_and_search() {
        let (_dir, mut btree) = uuid_setup();
        for i in 0..100u128 {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }
        assert_eq!(btree.len(), 100);
        for i in 0..100u128 {
            assert_eq!(
                btree.search(&make_uuid(i)).unwrap(),
                Some((i as u32, 0)),
                "key {i} not found"
            );
        }
        assert!(btree.search(&make_uuid(999)).unwrap().is_none());
    }

    #[test]
    fn test_uuid_insert_causes_leaf_split() {
        let (_dir, mut btree) = uuid_setup();
        let count = (leaf_max_entries::<Uuid>(()) + 10) as u128;
        for i in 0..count {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }
        assert_eq!(btree.len(), count as u64);
        assert!(btree.height() >= 2);
        for i in 0..count {
            assert_eq!(
                btree.search(&make_uuid(i)).unwrap(),
                Some((i as u32, 0)),
                "key {i} missing after split"
            );
        }
    }

    #[test]
    fn test_uuid_insert_causes_root_split() {
        let (_dir, mut btree) = uuid_setup();
        let lf = leaf_max_entries::<Uuid>(());
        let inn = internal_max_keys::<Uuid>(());
        let count = ((lf * (inn + 2)) as u128).min(5000);
        for i in 0..count {
            btree
                .insert(make_uuid(i), i as u32, (i % 100) as u16)
                .unwrap();
        }
        assert!(btree.height() >= 2);
        assert_eq!(btree.len(), count as u64);
        for i in (0..count).step_by(50) {
            assert!(btree.search(&make_uuid(i)).unwrap().is_some());
        }
    }

    #[test]
    fn test_uuid_insert_random_order() {
        let (_dir, mut btree) = uuid_setup();
        let count = 1000u128;
        let keys: Vec<u128> = (0..count).map(|i| i.wrapping_mul(7919) % 100_000).collect();
        for (i, &k) in keys.iter().enumerate() {
            if btree.search(&make_uuid(k)).unwrap().is_some() {
                continue;
            }
            btree.insert(make_uuid(k), i as u32, 0).unwrap();
        }
        for &k in &keys {
            assert!(btree.search(&make_uuid(k)).unwrap().is_some());
        }
    }

    #[test]
    fn test_uuid_delete_single() {
        let (_dir, mut btree) = uuid_setup();
        let key = make_uuid(42);
        btree.insert(key, 10, 5).unwrap();
        btree.delete(&key).unwrap();
        assert!(btree.search(&key).unwrap().is_none());
        assert_eq!(btree.len(), 0);
    }

    #[test]
    fn test_uuid_delete_nonexistent() {
        let (_dir, mut btree) = uuid_setup();
        let result = btree.delete(&make_uuid(999));
        assert!(matches!(result, Err(GrumpyError::KeyNotFound(_))));
    }

    #[test]
    fn test_uuid_delete_half() {
        let (_dir, mut btree) = uuid_setup();
        let count = 500u128;
        for i in 0..count {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }
        for i in (0..count).step_by(2) {
            btree.delete(&make_uuid(i)).unwrap();
        }
        assert_eq!(btree.len(), count as u64 / 2);
        for i in (1..count).step_by(2) {
            assert!(btree.search(&make_uuid(i)).unwrap().is_some());
        }
        for i in (0..count).step_by(2) {
            assert!(btree.search(&make_uuid(i)).unwrap().is_none());
        }
    }

    #[test]
    fn test_uuid_delete_all() {
        let (_dir, mut btree) = uuid_setup();
        let count = 200u128;
        for i in 0..count {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }
        for i in 0..count {
            btree.delete(&make_uuid(i)).unwrap();
        }
        assert_eq!(btree.len(), 0);
        assert!(btree.is_empty());
    }

    #[test]
    fn test_uuid_insert_delete_reinsert() {
        let (_dir, mut btree) = uuid_setup();
        let key = make_uuid(42);
        btree.insert(key, 10, 0).unwrap();
        btree.delete(&key).unwrap();
        btree.insert(key, 20, 1).unwrap();
        assert_eq!(btree.search(&key).unwrap(), Some((20, 1)));
    }

    #[test]
    fn test_uuid_persist_and_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index.db");
        {
            let mut btree = BTree::<Uuid>::create(&path).unwrap();
            for i in 0..100u128 {
                btree.insert(make_uuid(i), i as u32, 0).unwrap();
            }
            btree.sync().unwrap();
        }
        {
            let mut btree = BTree::<Uuid>::open(&path).unwrap();
            assert_eq!(btree.len(), 100);
            for i in 0..100u128 {
                assert!(btree.search(&make_uuid(i)).unwrap().is_some());
            }
        }
    }

    #[test]
    fn test_uuid_large_insert_and_delete_stress() {
        let (_dir, mut btree) = uuid_setup();
        let count = 2000u128;
        for i in 0..count {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }
        assert_eq!(btree.len(), count as u64);
        for i in 0..count / 2 {
            btree.delete(&make_uuid(i)).unwrap();
        }
        assert_eq!(btree.len(), count as u64 / 2);
        for i in count / 2..count {
            assert!(btree.search(&make_uuid(i)).unwrap().is_some());
        }
    }

    // ─── Vec<u8> path ─────────────────────────────────────────────────

    fn vec_setup(max_key_size: u16) -> (TempDir, BTree<Vec<u8>>) {
        let dir = TempDir::new().unwrap();
        let tree = BTree::<Vec<u8>>::create_with(dir.path().join("var.idx"), max_key_size).unwrap();
        (dir, tree)
    }

    #[test]
    fn test_vec_insert_and_search() {
        let (_dir, mut tree) = vec_setup(32);
        tree.insert(b"hello".to_vec(), 10, 0).unwrap();
        tree.insert(b"world".to_vec(), 20, 1).unwrap();
        tree.insert(b"foo".to_vec(), 30, 2).unwrap();
        assert_eq!(tree.search(&b"hello".to_vec()).unwrap(), Some((10, 0)));
        assert_eq!(tree.search(&b"world".to_vec()).unwrap(), Some((20, 1)));
        assert_eq!(tree.search(&b"foo".to_vec()).unwrap(), Some((30, 2)));
        assert!(tree.search(&b"bar".to_vec()).unwrap().is_none());
        assert_eq!(tree.len(), 3);
    }

    #[test]
    fn test_vec_insert_many_causes_splits() {
        let (_dir, mut tree) = vec_setup(32);
        for i in 0u32..1000 {
            tree.insert(format!("key_{i:06}").into_bytes(), i, 0)
                .unwrap();
        }
        assert_eq!(tree.len(), 1000);
        assert!(tree.height() > 1);
        for i in 0u32..1000 {
            assert_eq!(
                tree.search(&format!("key_{i:06}").into_bytes()).unwrap(),
                Some((i, 0))
            );
        }
    }

    #[test]
    fn test_vec_delete() {
        let (_dir, mut tree) = vec_setup(32);
        tree.insert(b"alpha".to_vec(), 1, 0).unwrap();
        tree.insert(b"beta".to_vec(), 2, 0).unwrap();
        tree.insert(b"gamma".to_vec(), 3, 0).unwrap();
        tree.delete(&b"beta".to_vec()).unwrap();
        assert!(tree.search(&b"beta".to_vec()).unwrap().is_none());
        assert_eq!(tree.search(&b"alpha".to_vec()).unwrap(), Some((1, 0)));
        assert_eq!(tree.search(&b"gamma".to_vec()).unwrap(), Some((3, 0)));
        assert_eq!(tree.len(), 2);
    }

    #[test]
    fn test_vec_delete_many() {
        let (_dir, mut tree) = vec_setup(32);
        for i in 0u32..500 {
            tree.insert(format!("key_{i:06}").into_bytes(), i, 0)
                .unwrap();
        }
        for i in 0u32..250 {
            tree.delete(&format!("key_{i:06}").into_bytes()).unwrap();
        }
        assert_eq!(tree.len(), 250);
        for i in 0u32..250 {
            assert!(
                tree.search(&format!("key_{i:06}").into_bytes())
                    .unwrap()
                    .is_none()
            );
        }
        for i in 250u32..500 {
            assert!(
                tree.search(&format!("key_{i:06}").into_bytes())
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[test]
    fn test_vec_persistence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("persist.db");
        {
            let mut tree = BTree::<Vec<u8>>::create_with(&path, 32).unwrap();
            for i in 0u32..100 {
                tree.insert(format!("k{i:04}").into_bytes(), i, 0).unwrap();
            }
            tree.sync().unwrap();
        }
        {
            let mut tree = BTree::<Vec<u8>>::open(&path).unwrap();
            assert_eq!(tree.len(), 100);
            for i in 0u32..100 {
                assert!(
                    tree.search(&format!("k{i:04}").into_bytes())
                        .unwrap()
                        .is_some()
                );
            }
        }
    }

    #[test]
    fn test_vec_duplicate_key() {
        let (_dir, mut tree) = vec_setup(32);
        tree.insert(b"dup".to_vec(), 1, 0).unwrap();
        let result = tree.insert(b"dup".to_vec(), 2, 0);
        assert!(matches!(result, Err(GrumpyError::Codec(_))));
    }

    #[test]
    fn test_vec_delete_nonexistent() {
        let (_dir, mut tree) = vec_setup(32);
        let result = tree.delete(&b"ghost".to_vec());
        assert!(matches!(result, Err(GrumpyError::Codec(_))));
    }

    #[test]
    fn test_vec_large_stress() {
        let (_dir, mut tree) = vec_setup(64);
        for i in 0u32..3000 {
            tree.insert(
                format!("stress_key_{i:08}").into_bytes(),
                i,
                (i % 100) as u16,
            )
            .unwrap();
        }
        assert_eq!(tree.len(), 3000);
        for i in 0u32..1500 {
            tree.delete(&format!("stress_key_{i:08}").into_bytes())
                .unwrap();
        }
        assert_eq!(tree.len(), 1500);
        for i in 1500u32..3000 {
            assert_eq!(
                tree.search(&format!("stress_key_{i:08}").into_bytes())
                    .unwrap(),
                Some((i, (i % 100) as u16))
            );
        }
    }

    // ─── Cross-impl property: equivalent behaviour ────────────────────

    /// Property check: a small mix of inserts/deletes/searches behaves the
    /// same way against a `BTree<Uuid>` and a `BTree<Vec<u8>>` whose keys are
    /// the canonical 16-byte serialisation of those Uuids.
    #[test]
    fn test_property_uuid_and_vec_agree() {
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();
        let mut uu = BTree::<Uuid>::create(dir1.path().join("u.idx")).unwrap();
        let mut vv = BTree::<Vec<u8>>::create_with(dir2.path().join("v.idx"), 16).unwrap();

        // Pseudo-random sequence of operations.
        let mut state: Vec<Uuid> = Vec::new();
        for i in 0u128..256 {
            let id = Uuid::from_u128(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            uu.insert(id, i as u32, (i % 7) as u16).unwrap();
            vv.insert(id.as_bytes().to_vec(), i as u32, (i % 7) as u16)
                .unwrap();
            state.push(id);
        }

        // Random spot checks
        for &id in state.iter().step_by(13) {
            let u = uu.search(&id).unwrap();
            let v = vv.search(&id.as_bytes().to_vec()).unwrap();
            assert_eq!(u, v);
        }

        // Delete every other key.
        for &id in state.iter().step_by(2) {
            uu.delete(&id).unwrap();
            vv.delete(&id.as_bytes().to_vec()).unwrap();
        }
        assert_eq!(uu.len(), vv.len());

        for (i, &id) in state.iter().enumerate() {
            let u = uu.search(&id).unwrap();
            let v = vv.search(&id.as_bytes().to_vec()).unwrap();
            assert_eq!(u, v, "mismatch at index {i}");
            if i % 2 == 0 {
                assert!(u.is_none());
            } else {
                assert!(u.is_some());
            }
        }
    }
}
