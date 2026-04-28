//! Variable-key B+Tree: search, insert, delete with split/merge.
//!
//! This is a parallel implementation to `ops.rs` that supports variable-length
//! byte keys instead of fixed 16-byte UUIDs.

use crate::error::{GrumpyError, Result};
use crate::page::{PageHeader, PageType};

use super::var_node::{
    VarInternalEntry, VarInternalNode, VarLeafEntry, VarLeafNode, var_internal_min_keys,
    var_leaf_min_entries,
};
use super::var_tree::VarBTree;

/// Identifies the type of node loaded from a page.
enum VarNodeRef {
    Internal(VarInternalNode),
    Leaf(VarLeafNode),
}

impl VarBTree {
    // ── Helpers ─────────────────────────────────────────────────────────

    /// Loads a node from disk.
    fn load_node(&mut self, page_id: u32) -> Result<VarNodeRef> {
        let buf = self.pm.read_page(page_id)?;
        let header = PageHeader::read_from(&buf);
        match header.page_type {
            PageType::BTreeInternal => Ok(VarNodeRef::Internal(VarInternalNode::from_bytes(&buf))),
            PageType::BTreeLeaf => Ok(VarNodeRef::Leaf(VarLeafNode::from_bytes(&buf))),
            _ => Err(GrumpyError::PageNotFound(page_id)),
        }
    }

    fn save_internal(&mut self, node: &VarInternalNode) -> Result<()> {
        self.pm.write_page(node.page_id, &node.to_bytes())
    }

    fn save_leaf(&mut self, node: &VarLeafNode) -> Result<()> {
        self.pm.write_page(node.page_id, &node.to_bytes())
    }

    fn alloc_page(&mut self) -> Result<u32> {
        self.pm.allocate_page()
    }

    // ── Search ──────────────────────────────────────────────────────────

    /// Searches for a key. Returns `Some((page_id, slot_id))` if found.
    pub fn search(&mut self, key: &[u8]) -> Result<Option<(u32, u16)>> {
        let leaf = self.find_leaf(key)?;
        if let Some(idx) = leaf.search(key) {
            let entry = &leaf.entries[idx];
            Ok(Some((entry.page_id, entry.slot_id)))
        } else {
            Ok(None)
        }
    }

    /// Descends to the leaf that would contain `key`.
    pub(crate) fn find_leaf(&mut self, key: &[u8]) -> Result<VarLeafNode> {
        let mut current = self.meta.root_page_id;
        loop {
            match self.load_node(current)? {
                VarNodeRef::Leaf(leaf) => return Ok(leaf),
                VarNodeRef::Internal(internal) => {
                    current = internal.find_child(key);
                }
            }
        }
    }

    /// Descends to the leaf, recording the path.
    fn find_leaf_with_path(&mut self, key: &[u8]) -> Result<(VarLeafNode, Vec<(u32, usize)>)> {
        let mut path = Vec::new();
        let mut current = self.meta.root_page_id;
        loop {
            match self.load_node(current)? {
                VarNodeRef::Leaf(leaf) => return Ok((leaf, path)),
                VarNodeRef::Internal(internal) => {
                    let child_idx = var_find_child_index(&internal, key);
                    let child_page = var_get_child_at(&internal, child_idx);
                    path.push((current, child_idx));
                    current = child_page;
                }
            }
        }
    }

    // ── Insert ──────────────────────────────────────────────────────────

    /// Inserts a key → (page_id, slot_id) mapping.
    pub fn insert(&mut self, key: Vec<u8>, page_id: u32, slot_id: u16) -> Result<()> {
        let (mut leaf, path) = self.find_leaf_with_path(&key)?;

        if leaf.search(&key).is_some() {
            return Err(GrumpyError::Codec(format!(
                "duplicate key in VarBTree: {} bytes",
                key.len()
            )));
        }

        leaf.insert_entry(VarLeafEntry {
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

        // Split
        let (left, right, promoted_key) = self.split_leaf(leaf)?;
        self.save_leaf(&left)?;
        self.save_leaf(&right)?;

        if right.next_leaf != 0 {
            let buf = self.pm.read_page(right.next_leaf)?;
            let mut next = VarLeafNode::from_bytes(&buf);
            next.prev_leaf = right.page_id;
            self.save_leaf(&next)?;
        }

        self.propagate_split(path, promoted_key, left.page_id, right.page_id)?;
        self.meta.num_entries += 1;
        self.flush_meta()?;
        Ok(())
    }

    fn split_leaf(
        &mut self,
        mut full_leaf: VarLeafNode,
    ) -> Result<(VarLeafNode, VarLeafNode, Vec<u8>)> {
        let mid = full_leaf.entries.len() / 2;
        let right_entries: Vec<VarLeafEntry> = full_leaf.entries.drain(mid..).collect();
        full_leaf.num_entries = full_leaf.entries.len() as u16;

        let right_page_id = self.alloc_page()?;
        let mut right = VarLeafNode::new(right_page_id, full_leaf.max_key_size);
        right.entries = right_entries;
        right.num_entries = right.entries.len() as u16;

        right.next_leaf = full_leaf.next_leaf;
        full_leaf.next_leaf = right_page_id;
        right.prev_leaf = full_leaf.page_id;

        let promoted_key = right.entries[0].key.clone();
        Ok((full_leaf, right, promoted_key))
    }

    fn split_internal(
        &mut self,
        mut full_node: VarInternalNode,
    ) -> Result<(VarInternalNode, VarInternalNode, Vec<u8>)> {
        let mid = full_node.entries.len() / 2;
        let promoted_key = full_node.entries[mid].key.clone();

        let right_entries: Vec<VarInternalEntry> = full_node.entries.drain(mid + 1..).collect();
        let mid_entry = full_node
            .entries
            .pop()
            .ok_or_else(|| GrumpyError::Corruption("split_internal: empty entries".into()))?;
        full_node.num_keys = full_node.entries.len() as u16;

        let right_page_id = self.alloc_page()?;
        let mut right = VarInternalNode::new(right_page_id, full_node.max_key_size);
        right.entries = right_entries;
        right.num_keys = right.entries.len() as u16;

        right.right_child = full_node.right_child;
        full_node.right_child = mid_entry.child_page_id;

        Ok((full_node, right, promoted_key))
    }

    fn propagate_split(
        &mut self,
        mut path: Vec<(u32, usize)>,
        mut promoted_key: Vec<u8>,
        _left_page_id: u32,
        mut right_page_id: u32,
    ) -> Result<()> {
        while let Some((parent_page_id, _child_idx)) = path.pop() {
            let buf = self.pm.read_page(parent_page_id)?;
            let mut parent = VarInternalNode::from_bytes(&buf);

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

        // New root needed
        let new_root_page_id = self.alloc_page()?;
        let mut new_root = VarInternalNode::new(new_root_page_id, self.max_key_size);
        new_root.entries.push(VarInternalEntry {
            key: promoted_key,
            child_page_id: self.meta.root_page_id,
        });
        new_root.num_keys = 1;
        new_root.right_child = right_page_id;
        self.save_internal(&new_root)?;

        self.meta.root_page_id = new_root_page_id;
        self.meta.height += 1;
        Ok(())
    }

    // ── Delete ──────────────────────────────────────────────────────────

    /// Deletes a key from the tree.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        let (mut leaf, path) = self.find_leaf_with_path(key)?;

        if leaf.remove_entry(key).is_none() {
            return Err(GrumpyError::Codec(format!(
                "key not found in VarBTree: {} bytes",
                key.len()
            )));
        }

        self.save_leaf(&leaf)?;
        self.meta.num_entries -= 1;

        if path.is_empty() || !leaf.is_underfull() {
            self.flush_meta()?;
            return Ok(());
        }

        self.rebalance_leaf(leaf, path)?;
        self.flush_meta()?;
        Ok(())
    }

    fn rebalance_leaf(&mut self, leaf: VarLeafNode, mut path: Vec<(u32, usize)>) -> Result<()> {
        let (parent_page_id, child_idx) = path
            .pop()
            .ok_or_else(|| GrumpyError::Corruption("rebalance_leaf: empty path".into()))?;
        let buf = self.pm.read_page(parent_page_id)?;
        let mut parent = VarInternalNode::from_bytes(&buf);
        let num_children = parent.num_keys as usize + 1;
        let min = var_leaf_min_entries(self.max_key_size as usize);

        if child_idx > 0 {
            let left_sib_id = var_get_child_at(&parent, child_idx - 1);
            let buf = self.pm.read_page(left_sib_id)?;
            let mut left_sib = VarLeafNode::from_bytes(&buf);

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

        if child_idx + 1 < num_children {
            let right_sib_id = var_get_child_at(&parent, child_idx + 1);
            let buf = self.pm.read_page(right_sib_id)?;
            let mut right_sib = VarLeafNode::from_bytes(&buf);

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
        left: &mut VarLeafNode,
        mut leaf: VarLeafNode,
        parent: &mut VarInternalNode,
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
        mut leaf: VarLeafNode,
        right: &mut VarLeafNode,
        parent: &mut VarInternalNode,
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
        mut left: VarLeafNode,
        right: VarLeafNode,
        parent: &mut VarInternalNode,
        right_child_idx: usize,
        path: Vec<(u32, usize)>,
    ) -> Result<()> {
        for entry in &right.entries {
            left.insert_entry(entry.clone());
        }

        left.next_leaf = right.next_leaf;
        if right.next_leaf != 0 {
            let buf = self.pm.read_page(right.next_leaf)?;
            let mut next = VarLeafNode::from_bytes(&buf);
            next.prev_leaf = left.page_id;
            self.save_leaf(&next)?;
        }

        self.pm.free_page(right.page_id)?;
        self.save_leaf(&left)?;

        self.remove_from_internal(parent, right_child_idx, path)
    }

    fn remove_from_internal(
        &mut self,
        parent: &mut VarInternalNode,
        merged_child_idx: usize,
        path: Vec<(u32, usize)>,
    ) -> Result<()> {
        let mut children: Vec<u32> = parent.entries.iter().map(|e| e.child_page_id).collect();
        children.push(parent.right_child);
        let mut keys: Vec<Vec<u8>> = parent.entries.iter().map(|e| e.key.clone()).collect();

        children.remove(merged_child_idx);
        let sep_idx = if merged_child_idx > 0 {
            merged_child_idx - 1
        } else {
            0
        };
        if !keys.is_empty() {
            keys.remove(sep_idx);
        }

        parent.entries.clear();
        for (i, key) in keys.iter().enumerate() {
            parent.entries.push(VarInternalEntry {
                key: key.clone(),
                child_page_id: children[i],
            });
        }
        parent.right_child = *children.last().unwrap_or(&0);
        parent.num_keys = parent.entries.len() as u16;

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

        if !path.is_empty() && parent.is_underfull() {
            self.rebalance_internal(parent.clone(), path)?;
        }

        Ok(())
    }

    fn rebalance_internal(
        &mut self,
        node: VarInternalNode,
        mut path: Vec<(u32, usize)>,
    ) -> Result<()> {
        let (gp_page_id, child_idx) = path
            .pop()
            .ok_or_else(|| GrumpyError::Corruption("rebalance_internal: empty path".into()))?;
        let buf = self.pm.read_page(gp_page_id)?;
        let mut gp = VarInternalNode::from_bytes(&buf);
        let num_children = gp.num_keys as usize + 1;
        let min = var_internal_min_keys(self.max_key_size as usize);

        if child_idx > 0 {
            let left_id = var_get_child_at(&gp, child_idx - 1);
            let buf = self.pm.read_page(left_id)?;
            let left = VarInternalNode::from_bytes(&buf);

            if left.entries.len() > min {
                return self.redistribute_internal_from_left(left, node, &mut gp, child_idx);
            }
            return self.merge_internal(left, node, &mut gp, child_idx, path);
        }

        if child_idx + 1 < num_children {
            let right_id = var_get_child_at(&gp, child_idx + 1);
            let buf = self.pm.read_page(right_id)?;
            let right = VarInternalNode::from_bytes(&buf);

            if right.entries.len() > min {
                return self.redistribute_internal_from_right(node, right, &mut gp, child_idx);
            }
            return self.merge_internal(node, right, &mut gp, child_idx + 1, path);
        }

        Ok(())
    }

    fn redistribute_internal_from_left(
        &mut self,
        mut left: VarInternalNode,
        mut node: VarInternalNode,
        gp: &mut VarInternalNode,
        child_idx: usize,
    ) -> Result<()> {
        let sep_idx = child_idx - 1;
        let separator_key = gp.entries[sep_idx].key.clone();

        node.entries.insert(
            0,
            VarInternalEntry {
                key: separator_key,
                child_page_id: left.right_child,
            },
        );
        node.num_keys += 1;

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
        mut node: VarInternalNode,
        mut right: VarInternalNode,
        gp: &mut VarInternalNode,
        child_idx: usize,
    ) -> Result<()> {
        let sep_idx = child_idx;
        if sep_idx >= gp.entries.len() {
            return Ok(());
        }
        let separator_key = gp.entries[sep_idx].key.clone();

        node.entries.push(VarInternalEntry {
            key: separator_key,
            child_page_id: node.right_child,
        });
        node.num_keys += 1;

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
        mut left: VarInternalNode,
        right: VarInternalNode,
        gp: &mut VarInternalNode,
        right_child_idx: usize,
        path: Vec<(u32, usize)>,
    ) -> Result<()> {
        let sep_idx = if right_child_idx > 0 {
            right_child_idx - 1
        } else {
            0
        };
        let separator_key = gp.entries[sep_idx].key.clone();

        left.entries.push(VarInternalEntry {
            key: separator_key,
            child_page_id: left.right_child,
        });
        left.num_keys += 1;

        for entry in &right.entries {
            left.entries.push(entry.clone());
            left.num_keys += 1;
        }
        left.right_child = right.right_child;

        self.pm.free_page(right.page_id)?;
        self.save_internal(&left)?;

        self.remove_from_internal(gp, right_child_idx, path)
    }
}

// ── Utilities ───────────────────────────────────────────────────────────

fn var_find_child_index(node: &VarInternalNode, key: &[u8]) -> usize {
    for (i, entry) in node.entries.iter().enumerate() {
        if key < entry.key.as_slice() {
            return i;
        }
    }
    node.entries.len()
}

fn var_get_child_at(node: &VarInternalNode, child_idx: usize) -> u32 {
    if child_idx < node.entries.len() {
        node.entries[child_idx].child_page_id
    } else {
        node.right_child
    }
}
