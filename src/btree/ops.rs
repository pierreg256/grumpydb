//! B+Tree operations: search, insert (with split), delete (with merge/redistribute).

use uuid::Uuid;

use crate::error::{GrumpyError, Result};
use crate::page::{PageHeader, PageType};

use super::node::{
    InternalEntry, InternalNode, LeafEntry, LeafNode, KEY_SIZE,
};
use super::BTree;

/// Identifies the type of node loaded from a page.
enum NodeRef {
    Internal(InternalNode),
    Leaf(LeafNode),
}

impl BTree {
    // ── Helpers ─────────────────────────────────────────────────────────

    /// Loads a node from disk, detecting its type from the page header.
    fn load_node(&mut self, page_id: u32) -> Result<NodeRef> {
        let buf = self.pm.read_page(page_id)?;
        let header = PageHeader::read_from(&buf);
        match header.page_type {
            PageType::BTreeInternal => Ok(NodeRef::Internal(InternalNode::from_bytes(&buf))),
            PageType::BTreeLeaf => Ok(NodeRef::Leaf(LeafNode::from_bytes(&buf))),
            _ => Err(GrumpyError::PageNotFound(page_id)),
        }
    }

    /// Writes an internal node to disk.
    fn save_internal(&mut self, node: &InternalNode) -> Result<()> {
        self.pm.write_page(node.page_id, &node.to_bytes())
    }

    /// Writes a leaf node to disk.
    fn save_leaf(&mut self, node: &LeafNode) -> Result<()> {
        self.pm.write_page(node.page_id, &node.to_bytes())
    }

    /// Allocates a new page for a node.
    fn alloc_page(&mut self) -> Result<u32> {
        self.pm.allocate_page()
    }

    // ── Search ──────────────────────────────────────────────────────────

    /// Searches for a key in the B+Tree.
    ///
    /// Returns `Some((page_id, slot_id))` if found, `None` otherwise.
    pub fn search(&mut self, key: &Uuid) -> Result<Option<(u32, u16)>> {
        let key_bytes = uuid_to_key(key);
        let leaf = self.find_leaf(&key_bytes)?;

        if let Some(idx) = leaf.search(&key_bytes) {
            let entry = &leaf.entries[idx];
            Ok(Some((entry.page_id, entry.slot_id)))
        } else {
            Ok(None)
        }
    }

    /// Descends the tree to find the leaf node that would contain `key`.
    pub(crate) fn find_leaf(&mut self, key: &[u8; KEY_SIZE]) -> Result<LeafNode> {
        let mut current_page_id = self.meta.root_page_id;

        loop {
            match self.load_node(current_page_id)? {
                NodeRef::Leaf(leaf) => return Ok(leaf),
                NodeRef::Internal(internal) => {
                    current_page_id = internal.find_child(key);
                }
            }
        }
    }

    /// Descends to the leaf, recording the path of (internal_page_id, child_index_used).
    fn find_leaf_with_path(
        &mut self,
        key: &[u8; KEY_SIZE],
    ) -> Result<(LeafNode, Vec<(u32, usize)>)> {
        let mut path = Vec::new();
        let mut current_page_id = self.meta.root_page_id;

        loop {
            match self.load_node(current_page_id)? {
                NodeRef::Leaf(leaf) => return Ok((leaf, path)),
                NodeRef::Internal(internal) => {
                    // Determine which child we descend to and its index
                    let child_idx = find_child_index(&internal, key);
                    let child_page_id = get_child_at(&internal, child_idx);
                    path.push((current_page_id, child_idx));
                    current_page_id = child_page_id;
                }
            }
        }
    }

    // ── Insert ──────────────────────────────────────────────────────────

    /// Inserts a key-value mapping into the B+Tree.
    ///
    /// Returns `DuplicateKey` if the key already exists.
    pub fn insert(&mut self, key: Uuid, page_id: u32, slot_id: u16) -> Result<()> {
        let key_bytes = uuid_to_key(&key);

        // Find leaf with path for split propagation
        let (mut leaf, path) = self.find_leaf_with_path(&key_bytes)?;

        // Check for duplicate
        if leaf.search(&key_bytes).is_some() {
            return Err(GrumpyError::DuplicateKey(key));
        }

        // Insert into leaf
        leaf.insert_entry(LeafEntry {
            key: key_bytes,
            page_id,
            slot_id,
        });

        if !leaf.is_overfull() {
            self.save_leaf(&leaf)?;
            self.meta.num_entries += 1;
            self.flush_meta()?;
            return Ok(());
        }

        // Leaf is overfull → split
        let (left, right, promoted_key) = self.split_leaf(leaf)?;
        self.save_leaf(&left)?;
        self.save_leaf(&right)?;

        // Update linked-list neighbor if needed
        if right.next_leaf != 0 {
            let buf = self.pm.read_page(right.next_leaf)?;
            let mut next = LeafNode::from_bytes(&buf);
            next.prev_leaf = right.page_id;
            self.save_leaf(&next)?;
        }

        // Propagate the split up through internal nodes
        self.propagate_split(path, promoted_key, left.page_id, right.page_id)?;

        self.meta.num_entries += 1;
        self.flush_meta()?;
        Ok(())
    }

    /// Splits a leaf node into two halves.
    ///
    /// The left node keeps the original page_id. A new page is allocated for the right.
    /// Returns (left, right, promoted_key) where promoted_key = first key of right.
    fn split_leaf(&mut self, mut full_leaf: LeafNode) -> Result<(LeafNode, LeafNode, [u8; KEY_SIZE])> {
        let mid = full_leaf.entries.len() / 2;
        let right_entries: Vec<LeafEntry> = full_leaf.entries.drain(mid..).collect();
        full_leaf.num_entries = full_leaf.entries.len() as u16;

        let right_page_id = self.alloc_page()?;
        let mut right = LeafNode::new(right_page_id);
        right.entries = right_entries;
        right.num_entries = right.entries.len() as u16;

        // Update linked list
        right.next_leaf = full_leaf.next_leaf;
        full_leaf.next_leaf = right_page_id;
        right.prev_leaf = full_leaf.page_id;

        let promoted_key = right.entries[0].key;
        Ok((full_leaf, right, promoted_key))
    }

    /// Splits an internal node into two halves.
    ///
    /// The median key is promoted (not kept in either child).
    /// Convention: entries[i].child_page_id = left child of entries[i].key.
    fn split_internal(
        &mut self,
        mut full_node: InternalNode,
    ) -> Result<(InternalNode, InternalNode, [u8; KEY_SIZE])> {
        let mid = full_node.entries.len() / 2;

        // The promoted key is entries[mid].key
        let promoted_key = full_node.entries[mid].key;

        // Everything after mid goes to the right node
        let right_entries: Vec<InternalEntry> = full_node.entries.drain(mid + 1..).collect();

        // Remove the mid entry from left — its child becomes left's new right_child
        // entries[mid].child_page_id was the left child of promoted_key,
        // which means it contains keys < promoted_key and >= entries[mid-1].key.
        // This should become left's right_child.
        let mid_entry = full_node.entries.pop().unwrap();
        full_node.num_keys = full_node.entries.len() as u16;

        // Right node setup
        let right_page_id = self.alloc_page()?;
        let mut right = InternalNode::new(right_page_id);
        right.entries = right_entries;
        right.num_keys = right.entries.len() as u16;

        // left's right_child = mid_entry.child_page_id (left child of promoted key)
        // right's right_child = original right_child (rightmost pointer)
        right.right_child = full_node.right_child;
        full_node.right_child = mid_entry.child_page_id;

        Ok((full_node, right, promoted_key))
    }

    /// Propagates a split upward through the internal nodes.
    fn propagate_split(
        &mut self,
        mut path: Vec<(u32, usize)>,
        mut promoted_key: [u8; KEY_SIZE],
        _left_page_id: u32,
        mut right_page_id: u32,
    ) -> Result<()> {
        while let Some((parent_page_id, _child_idx)) = path.pop() {
            let buf = self.pm.read_page(parent_page_id)?;
            let mut parent = InternalNode::from_bytes(&buf);

            parent.insert_entry(promoted_key, right_page_id);

            if !parent.is_overfull() {
                self.save_internal(&parent)?;
                return Ok(());
            }

            // Internal node also overfull → split it
            let (left_int, right_int, new_promoted) = self.split_internal(parent)?;
            self.save_internal(&left_int)?;
            self.save_internal(&right_int)?;

            promoted_key = new_promoted;
            right_page_id = right_int.page_id;
        }

        // We've exhausted the path → need a new root
        let new_root_page_id = self.alloc_page()?;
        let mut new_root = InternalNode::new(new_root_page_id);
        new_root.entries.push(InternalEntry {
            key: promoted_key,
            child_page_id: self.meta.root_page_id, // old root = left child
        });
        new_root.num_keys = 1;
        new_root.right_child = right_page_id;
        self.save_internal(&new_root)?;

        self.meta.root_page_id = new_root_page_id;
        self.meta.height += 1;
        Ok(())
    }

    // ── Delete ──────────────────────────────────────────────────────────

    /// Deletes a key from the B+Tree.
    ///
    /// Returns `KeyNotFound` if the key does not exist.
    pub fn delete(&mut self, key: &Uuid) -> Result<()> {
        let key_bytes = uuid_to_key(key);

        let (mut leaf, path) = self.find_leaf_with_path(&key_bytes)?;

        // Remove from leaf
        if leaf.remove_entry(&key_bytes).is_none() {
            return Err(GrumpyError::KeyNotFound(*key));
        }

        self.save_leaf(&leaf)?;
        self.meta.num_entries -= 1;

        // Check if leaf needs rebalancing
        // Root leaf can be empty, no need to rebalance
        if path.is_empty() || !leaf.is_underfull() {
            self.flush_meta()?;
            return Ok(());
        }

        self.rebalance_leaf(leaf, path)?;
        self.flush_meta()?;
        Ok(())
    }

    /// Rebalances an underfull leaf by redistributing or merging with a sibling.
    fn rebalance_leaf(&mut self, leaf: LeafNode, mut path: Vec<(u32, usize)>) -> Result<()> {
        let (parent_page_id, child_idx) = path.pop().unwrap();
        let buf = self.pm.read_page(parent_page_id)?;
        let mut parent = InternalNode::from_bytes(&buf);

        let num_children = parent.num_keys as usize + 1;

        // Try left sibling first
        if child_idx > 0 {
            let left_sibling_id = get_child_at(&parent, child_idx - 1);
            let buf = self.pm.read_page(left_sibling_id)?;
            let mut left_sib = LeafNode::from_bytes(&buf);

            if left_sib.entries.len() > super::node::LEAF_MIN_ENTRIES {
                // Redistribute: move last entry from left to leaf
                return self.redistribute_leaf_from_left(
                    &mut left_sib,
                    leaf,
                    &mut parent,
                    child_idx,
                );
            }

            // Merge: merge leaf into left sibling
            return self.merge_leaves(left_sib, leaf, &mut parent, child_idx, path);
        }

        // Try right sibling
        if child_idx + 1 < num_children {
            let right_sibling_id = get_child_at(&parent, child_idx + 1);
            let buf = self.pm.read_page(right_sibling_id)?;
            let mut right_sib = LeafNode::from_bytes(&buf);

            if right_sib.entries.len() > super::node::LEAF_MIN_ENTRIES {
                // Redistribute: move first entry from right to leaf
                return self.redistribute_leaf_from_right(
                    leaf,
                    &mut right_sib,
                    &mut parent,
                    child_idx,
                );
            }

            // Merge: merge right into leaf
            return self.merge_leaves(leaf, right_sib, &mut parent, child_idx + 1, path);
        }

        // No siblings available (shouldn't happen if parent has >= 2 children)
        Ok(())
    }

    /// Moves the last entry from the left sibling to the leaf.
    fn redistribute_leaf_from_left(
        &mut self,
        left: &mut LeafNode,
        mut leaf: LeafNode,
        parent: &mut InternalNode,
        child_idx: usize,
    ) -> Result<()> {
        let moved = left.entries.pop().unwrap();
        left.num_entries -= 1;
        leaf.insert_entry(moved);

        // Update the separator key in the parent
        // The separator between left and leaf is at parent.entries[child_idx - 1]
        let sep_idx = child_idx - 1;
        parent.entries[sep_idx].key = leaf.entries[0].key;

        self.save_leaf(left)?;
        self.save_leaf(&leaf)?;
        self.save_internal(parent)?;
        Ok(())
    }

    /// Moves the first entry from the right sibling to the leaf.
    fn redistribute_leaf_from_right(
        &mut self,
        mut leaf: LeafNode,
        right: &mut LeafNode,
        parent: &mut InternalNode,
        child_idx: usize,
    ) -> Result<()> {
        let moved = right.entries.remove(0);
        right.num_entries -= 1;
        leaf.insert_entry(moved);

        // Update separator: the key between leaf and right in parent
        // That separator is at parent.entries[child_idx]
        // (unless child_idx is the last child, which uses right_child)
        if child_idx < parent.entries.len() {
            parent.entries[child_idx].key = right.entries[0].key;
        }

        self.save_leaf(&leaf)?;
        self.save_leaf(right)?;
        self.save_internal(parent)?;
        Ok(())
    }

    /// Merges right_leaf into left_leaf and removes the separator from parent.
    fn merge_leaves(
        &mut self,
        mut left: LeafNode,
        right: LeafNode,
        parent: &mut InternalNode,
        right_child_idx: usize,
        path: Vec<(u32, usize)>,
    ) -> Result<()> {
        // Move all entries from right into left
        for entry in &right.entries {
            left.insert_entry(*entry);
        }

        // Update linked list
        left.next_leaf = right.next_leaf;
        if right.next_leaf != 0 {
            let buf = self.pm.read_page(right.next_leaf)?;
            let mut next = LeafNode::from_bytes(&buf);
            next.prev_leaf = left.page_id;
            self.save_leaf(&next)?;
        }

        // Free the right page
        self.pm.free_page(right.page_id)?;

        self.save_leaf(&left)?;

        // Remove the separator key from parent
        // The separator for right_child_idx is at entries[right_child_idx - 1]
        // unless right_child_idx == num_children - 1 (using right_child)
        self.remove_from_internal(parent, right_child_idx, path)
    }

    /// Removes a child reference from an internal node after a merge.
    ///
    /// After merging child[merged_child_idx] into its left sibling,
    /// removes the separator key and the dangling child pointer.
    fn remove_from_internal(
        &mut self,
        parent: &mut InternalNode,
        merged_child_idx: usize,
        path: Vec<(u32, usize)>,
    ) -> Result<()> {
        // Build flat children/keys arrays for clean manipulation.
        // Children: [e0.child, e1.child, ..., e_{n-1}.child, right_child]
        // Keys:     [e0.key,   e1.key,   ..., e_{n-1}.key              ]
        let mut children: Vec<u32> = parent.entries.iter().map(|e| e.child_page_id).collect();
        children.push(parent.right_child);
        let mut keys: Vec<[u8; KEY_SIZE]> = parent.entries.iter().map(|e| e.key).collect();

        // Remove the merged child
        children.remove(merged_child_idx);
        // Remove the separator key (between left sibling and merged child)
        let sep_idx = if merged_child_idx > 0 {
            merged_child_idx - 1
        } else {
            0
        };
        if !keys.is_empty() {
            keys.remove(sep_idx);
        }

        // Rebuild entries from flat arrays
        parent.entries.clear();
        for (i, &key) in keys.iter().enumerate() {
            parent.entries.push(InternalEntry {
                key,
                child_page_id: children[i],
            });
        }
        parent.right_child = *children.last().unwrap_or(&0);
        parent.num_keys = parent.entries.len() as u16;

        // Check if parent is now the root and has no more keys
        if path.is_empty() && parent.num_keys == 0 {
            // The single remaining child becomes the new root
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

        // Recurse upward if parent is also underfull
        if !path.is_empty() && parent.is_underfull() {
            self.rebalance_internal(parent.clone(), path)?;
        }

        Ok(())
    }

    /// Rebalances an underfull internal node.
    fn rebalance_internal(
        &mut self,
        node: InternalNode,
        mut path: Vec<(u32, usize)>,
    ) -> Result<()> {
        let (grandparent_page_id, child_idx) = path.pop().unwrap();
        let buf = self.pm.read_page(grandparent_page_id)?;
        let mut grandparent = InternalNode::from_bytes(&buf);
        let num_children = grandparent.num_keys as usize + 1;

        // Try left sibling
        if child_idx > 0 {
            let left_sibling_id = get_child_at(&grandparent, child_idx - 1);
            let buf = self.pm.read_page(left_sibling_id)?;
            let left_sib = InternalNode::from_bytes(&buf);

            if left_sib.entries.len() > super::node::INTERNAL_MIN_KEYS {
                return self.redistribute_internal_from_left(
                    left_sib,
                    node,
                    &mut grandparent,
                    child_idx,
                    path,
                );
            }

            return self.merge_internal_nodes(
                left_sib,
                node,
                &mut grandparent,
                child_idx,
                path,
            );
        }

        // Try right sibling
        if child_idx + 1 < num_children {
            let right_sibling_id = get_child_at(&grandparent, child_idx + 1);
            let buf = self.pm.read_page(right_sibling_id)?;
            let right_sib = InternalNode::from_bytes(&buf);

            if right_sib.entries.len() > super::node::INTERNAL_MIN_KEYS {
                return self.redistribute_internal_from_right(
                    node,
                    right_sib,
                    &mut grandparent,
                    child_idx,
                    path,
                );
            }

            return self.merge_internal_nodes(
                node,
                right_sib,
                &mut grandparent,
                child_idx + 1,
                path,
            );
        }

        Ok(())
    }

    fn redistribute_internal_from_left(
        &mut self,
        mut left: InternalNode,
        mut node: InternalNode,
        grandparent: &mut InternalNode,
        child_idx: usize,
        _path: Vec<(u32, usize)>,
    ) -> Result<()> {
        let sep_idx = child_idx - 1;
        let separator_key = grandparent.entries[sep_idx].key;

        // Move separator down to node (as first entry, pointing to node's implicit left)
        // The right_child of left becomes the child pointer of this new entry
        node.entries.insert(
            0,
            InternalEntry {
                key: separator_key,
                child_page_id: left.right_child,
            },
        );
        node.num_keys += 1;

        // Move last entry from left up to grandparent as new separator
        let moved_up = left.entries.pop().unwrap();
        left.num_keys -= 1;
        left.right_child = moved_up.child_page_id;
        grandparent.entries[sep_idx].key = moved_up.key;

        self.save_internal(&left)?;
        self.save_internal(&node)?;
        self.save_internal(grandparent)?;
        Ok(())
    }

    fn redistribute_internal_from_right(
        &mut self,
        mut node: InternalNode,
        mut right: InternalNode,
        grandparent: &mut InternalNode,
        child_idx: usize,
        _path: Vec<(u32, usize)>,
    ) -> Result<()> {
        let sep_idx = child_idx; // separator between node and right
        let separator_key = if sep_idx < grandparent.entries.len() {
            grandparent.entries[sep_idx].key
        } else {
            // This shouldn't happen in normal cases
            return Ok(());
        };

        // Move separator down to node (as last entry)
        node.entries.push(InternalEntry {
            key: separator_key,
            child_page_id: node.right_child,
        });
        node.num_keys += 1;

        // First entry from right moves up as new separator
        let moved_up = right.entries.remove(0);
        right.num_keys -= 1;
        node.right_child = moved_up.child_page_id;
        grandparent.entries[sep_idx].key = moved_up.key;

        self.save_internal(&node)?;
        self.save_internal(&right)?;
        self.save_internal(grandparent)?;
        Ok(())
    }

    fn merge_internal_nodes(
        &mut self,
        mut left: InternalNode,
        right: InternalNode,
        grandparent: &mut InternalNode,
        right_child_idx: usize,
        path: Vec<(u32, usize)>,
    ) -> Result<()> {
        // Pull down the separator key from grandparent
        let sep_idx = if right_child_idx > 0 {
            right_child_idx - 1
        } else {
            0
        };

        let separator_key = grandparent.entries[sep_idx].key;

        // Add separator to left, with child = left's right_child
        left.entries.push(InternalEntry {
            key: separator_key,
            child_page_id: left.right_child,
        });
        left.num_keys += 1;

        // Add all of right's entries
        for entry in &right.entries {
            left.entries.push(*entry);
            left.num_keys += 1;
        }
        left.right_child = right.right_child;

        // Free right node
        self.pm.free_page(right.page_id)?;
        self.save_internal(&left)?;

        // Remove separator from grandparent
        self.remove_from_internal(grandparent, right_child_idx, path)
    }
}

// ── Utility functions ───────────────────────────────────────────────────

/// Converts a UUID to a KEY_SIZE byte array for comparison.
fn uuid_to_key(uuid: &Uuid) -> [u8; KEY_SIZE] {
    *uuid.as_bytes()
}

/// Returns the child index for the given key in an internal node.
///
/// Convention: entries[i].child_page_id = left child of entries[i].key.
/// Index i means we descend through entries[i].child_page_id (for keys < entries[i].key).
/// Index == entries.len() means we descend through right_child.
fn find_child_index(node: &InternalNode, key: &[u8; KEY_SIZE]) -> usize {
    for (i, entry) in node.entries.iter().enumerate() {
        if key < &entry.key {
            return i;
        }
    }
    node.entries.len() // right_child
}

/// Returns the child page_id at the given index.
///
/// Index 0..n-1 → entries[i].child_page_id, index n → right_child.
fn get_child_at(node: &InternalNode, child_idx: usize) -> u32 {
    if child_idx < node.entries.len() {
        node.entries[child_idx].child_page_id
    } else {
        node.right_child
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::node::{INTERNAL_MAX_KEYS, LEAF_MAX_ENTRIES};
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
    fn test_search_empty_tree() {
        let (_dir, mut btree) = setup();
        let result = btree.search(&make_uuid(1)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_insert_and_search_single() {
        let (_dir, mut btree) = setup();
        let key = make_uuid(42);
        btree.insert(key, 10, 5).unwrap();

        let result = btree.search(&key).unwrap();
        assert_eq!(result, Some((10, 5)));
        assert_eq!(btree.len(), 1);
    }

    #[test]
    fn test_insert_duplicate_key() {
        let (_dir, mut btree) = setup();
        let key = make_uuid(42);
        btree.insert(key, 10, 5).unwrap();
        let result = btree.insert(key, 20, 0);
        assert!(matches!(result, Err(GrumpyError::DuplicateKey(_))));
    }

    #[test]
    fn test_insert_multiple_and_search() {
        let (_dir, mut btree) = setup();

        for i in 0..100u128 {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }

        assert_eq!(btree.len(), 100);

        for i in 0..100u128 {
            let result = btree.search(&make_uuid(i)).unwrap();
            assert_eq!(result, Some((i as u32, 0)), "key {i} not found");
        }

        // Non-existent key
        assert!(btree.search(&make_uuid(999)).unwrap().is_none());
    }

    #[test]
    fn test_insert_causes_leaf_split() {
        let (_dir, mut btree) = setup();

        // Insert enough entries to cause at least one split
        let count = (LEAF_MAX_ENTRIES + 10) as u128;
        for i in 0..count {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }

        assert_eq!(btree.len(), count as u64);
        assert!(btree.height() >= 2, "height should increase after split");

        // Verify all keys are still findable
        for i in 0..count {
            let result = btree.search(&make_uuid(i)).unwrap();
            assert_eq!(result, Some((i as u32, 0)), "key {i} not found after split");
        }
    }

    #[test]
    fn test_insert_causes_root_split() {
        let (_dir, mut btree) = setup();

        // Insert enough to cause internal node splits too
        let count = (LEAF_MAX_ENTRIES * (INTERNAL_MAX_KEYS + 2)) as u128;
        let count = count.min(5000); // cap for test speed

        for i in 0..count {
            btree.insert(make_uuid(i), i as u32, (i % 100) as u16).unwrap();
        }

        assert!(btree.height() >= 2);
        assert_eq!(btree.len(), count as u64);

        // Spot check
        for i in (0..count).step_by(50) {
            let result = btree.search(&make_uuid(i)).unwrap();
            assert!(result.is_some(), "key {i} not found");
        }
    }

    #[test]
    fn test_insert_random_order() {
        let (_dir, mut btree) = setup();

        // Insert UUIDs in a "random-ish" order using a simple scramble
        let count = 1000u128;
        let keys: Vec<u128> = (0..count).map(|i| i.wrapping_mul(7919) % 100_000).collect();

        for (i, &k) in keys.iter().enumerate() {
            // Skip duplicates from our scramble
            if btree.search(&make_uuid(k)).unwrap().is_some() {
                continue;
            }
            btree.insert(make_uuid(k), i as u32, 0).unwrap();
        }

        // Verify all inserted keys
        for &k in &keys {
            let result = btree.search(&make_uuid(k)).unwrap();
            assert!(result.is_some(), "key {k} not found");
        }
    }

    #[test]
    fn test_delete_single() {
        let (_dir, mut btree) = setup();
        let key = make_uuid(42);
        btree.insert(key, 10, 5).unwrap();
        btree.delete(&key).unwrap();

        assert!(btree.search(&key).unwrap().is_none());
        assert_eq!(btree.len(), 0);
    }

    #[test]
    fn test_delete_nonexistent() {
        let (_dir, mut btree) = setup();
        let result = btree.delete(&make_uuid(999));
        assert!(matches!(result, Err(GrumpyError::KeyNotFound(_))));
    }

    #[test]
    fn test_delete_half() {
        let (_dir, mut btree) = setup();
        let count = 500u128;

        for i in 0..count {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }

        // Delete even keys
        for i in (0..count).step_by(2) {
            btree.delete(&make_uuid(i)).unwrap();
        }

        assert_eq!(btree.len(), count as u64 / 2);

        // Verify odd keys still present
        for i in (1..count).step_by(2) {
            let result = btree.search(&make_uuid(i)).unwrap();
            assert!(result.is_some(), "key {i} should still exist");
        }

        // Verify even keys gone
        for i in (0..count).step_by(2) {
            assert!(
                btree.search(&make_uuid(i)).unwrap().is_none(),
                "key {i} should be deleted"
            );
        }
    }

    #[test]
    fn test_delete_all() {
        let (_dir, mut btree) = setup();
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
    fn test_insert_delete_reinsert() {
        let (_dir, mut btree) = setup();
        let key = make_uuid(42);

        btree.insert(key, 10, 0).unwrap();
        btree.delete(&key).unwrap();
        btree.insert(key, 20, 1).unwrap();

        let result = btree.search(&key).unwrap();
        assert_eq!(result, Some((20, 1)));
    }

    #[test]
    fn test_persist_and_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index.db");

        {
            let mut btree = BTree::create(&path).unwrap();
            for i in 0..100u128 {
                btree.insert(make_uuid(i), i as u32, 0).unwrap();
            }
            btree.sync().unwrap();
        }

        {
            let mut btree = BTree::open(&path).unwrap();
            assert_eq!(btree.len(), 100);
            for i in 0..100u128 {
                assert!(
                    btree.search(&make_uuid(i)).unwrap().is_some(),
                    "key {i} not found after reopen"
                );
            }
        }
    }

    #[test]
    fn test_large_insert_and_delete_stress() {
        let (_dir, mut btree) = setup();
        let count = 2000u128;

        // Insert all
        for i in 0..count {
            btree.insert(make_uuid(i), i as u32, 0).unwrap();
        }
        assert_eq!(btree.len(), count as u64);

        // Delete first half
        for i in 0..count / 2 {
            btree.delete(&make_uuid(i)).unwrap();
        }
        assert_eq!(btree.len(), count as u64 / 2);

        // Verify second half
        for i in count / 2..count {
            assert!(
                btree.search(&make_uuid(i)).unwrap().is_some(),
                "key {i} should exist"
            );
        }
    }
}
