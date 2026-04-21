# Skill: B+Tree Index

## When to use this skill

When working on:
- `src/btree/mod.rs` — BTree structure, open/create
- `src/btree/node.rs` — InternalNode, LeafNode, serialization
- `src/btree/ops.rs` — search, insert, delete, split, merge
- `src/btree/cursor.rs` — iteration, range scan

## Core principles

### Why B+Tree and not B-Tree?

- Data (page_id+slot_id pointers) is **only in the leaves**
- Internal nodes contain only keys + child pointers
- Leaves are **chained** (doubly-linked list) → efficient sequential scans
- Higher fan-out in internal nodes → shallower tree

### Tree parameters

```rust
const PAGE_SIZE: usize = 8192;
const PAGE_HEADER_SIZE: usize = 32;
const USABLE_SPACE: usize = PAGE_SIZE - PAGE_HEADER_SIZE; // 8160

// UUID key = 16 bytes
const KEY_SIZE: usize = 16;

// Internal node: key(16) + child_page_id(4) = 20 bytes per entry + num_keys(2) + right_child(4)
const INTERNAL_ENTRY_SIZE: usize = 20;
const INTERNAL_MAX_KEYS: usize = (USABLE_SPACE - 6) / INTERNAL_ENTRY_SIZE; // 407
// -6 for: num_keys(2) + right_child(4)

// Leaf node: key(16) + page_id(4) + slot_id(2) = 22 bytes per entry
const LEAF_ENTRY_SIZE: usize = 22;
const LEAF_MAX_ENTRIES: usize = (USABLE_SPACE - 10) / LEAF_ENTRY_SIZE; // 370
// -10 for: num_entries(2) + next_leaf(4) + prev_leaf(4)

const MIN_OCCUPANCY: usize = 40; // merge if < 40%
```

### Internal node binary format

```
Offset  Content
0-31    PageHeader (page_type = BTreeInternal)
32-33   num_keys: u16
34-37   right_child: u32
38+     entries: [key: [u8;16] + child_page_id: u32] × num_keys
```

### Leaf node binary format

```
Offset  Content
0-31    PageHeader (page_type = BTreeLeaf)
32-33   num_entries: u16
34-37   next_leaf: u32 (0 = no next)
38-41   prev_leaf: u32 (0 = no previous)
42+     entries: [key: [u8;16] + page_id: u32 + slot_id: u16] × num_entries
```

### B+Tree metadata (page 1 of the index file)

Note: page 0 is reserved by the PageManager for the free-list.
B+Tree metadata is stored in page 1 (`BTree::META_PAGE_ID = 1`).
The initial root is allocated at page 2.

```
Offset  Content
0-31    PageHeader (type = BTreeInternal, repurposed)
32-35   root_page_id: u32
36-39   height: u32
40-47   num_entries: u64
```

## Key algorithms

### Search

```
fn search(key) -> Option<(PageId, SlotId)>:
    node = load(root_page_id)
    while node is Internal:
        idx = binary_search(node.keys, key)
        child_id = if idx found: node.children[idx] else: node.right_child
        node = load(child_id)
    // node is Leaf
    idx = binary_search(node.entries, key)
    if exact match: return Some(node.entries[idx].value)
    return None
```

### Insert

```
fn insert(key, page_id, slot_id):
    if search(key).is_some(): return Err(DuplicateKey)
    
    // Descend to leaf with parent stack
    path = []  // stack of (node_page_id, child_index)
    node = load(root)
    while node is Internal:
        idx = find_child_index(node, key)
        path.push((node.page_id, idx))
        node = load(node.children[idx])
    
    // Insert into the leaf
    leaf = node as Leaf
    insert_sorted(leaf, key, page_id, slot_id)
    
    if leaf.num_entries <= LEAF_MAX_ENTRIES:
        save(leaf)
        return Ok(())
    
    // Split needed
    (left, right, median_key) = split_leaf(leaf)
    save(left)
    save(right)
    
    // Propagate the split up into internal nodes
    promoted_key = median_key
    right_page_id = right.page_id
    while !path.is_empty():
        (parent_id, idx) = path.pop()
        parent = load(parent_id)
        insert_into_internal(parent, idx, promoted_key, right_page_id)
        if parent.num_keys <= INTERNAL_MAX_KEYS:
            save(parent)
            return Ok(())
        (left_int, right_int, new_promoted) = split_internal(parent)
        save(left_int)
        save(right_int)
        promoted_key = new_promoted
        right_page_id = right_int.page_id
    
    // Root split → new root
    new_root = new_internal_node()
    new_root.keys = [promoted_key]
    new_root.children = [old_root_id]
    new_root.right_child = right_page_id
    save(new_root)
    update_metadata(root = new_root.page_id, height += 1)
```

### Leaf split

```
fn split_leaf(full_leaf) -> (left, right, median_key):
    mid = num_entries / 2
    left.entries = entries[0..mid]
    right.entries = entries[mid..]
    
    // Update the linked list pointers
    right.next_leaf = left.next_leaf
    left.next_leaf = right.page_id
    right.prev_leaf = left.page_id
    if right.next_leaf != 0:
        next = load(right.next_leaf)
        next.prev_leaf = right.page_id
        save(next)
    
    median_key = right.entries[0].key  // first key of the right part
    return (left, right, median_key)
```

### Delete

```
fn delete(key):
    entry = search(key)
    if entry.is_none(): return Err(KeyNotFound)
    
    // Descend with path
    // Remove the entry from the leaf
    // If below threshold (40%):
    //   1. Try redistribution with a sibling
    //   2. Otherwise, merge with a sibling
    //   3. The merge may propagate an underflow to the parent
```

## Mandatory test patterns

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_insert_and_search_single() { /* 1 insert → search → found */ }

    #[test]
    fn test_insert_sequential_causes_splits() {
        // Insert 0..1000 sequentially
        // Verify each key is retrievable
        // Verify height > 1
    }

    #[test]
    fn test_insert_random_order() {
        // Insert 1000 random UUIDs
        // Verify each key
    }

    #[test]
    fn test_delete_and_verify() {
        // Insert 100 → delete 50 → verify the remaining 50
    }

    #[test]
    fn test_delete_causes_merge() {
        // Insert enough to cause splits → delete enough to trigger merge
        // Verify tree integrity
    }

    #[test]
    fn test_duplicate_key_error() {
        // Insert key → insert same key → DuplicateKey error
    }

    #[test]
    fn test_cursor_range_scan() {
        // Insert 1000 sorted → range scan [200..500] → verify 300 results in order
    }

    #[test]
    fn test_cursor_full_scan() {
        // Insert N random → full scan → verify all N returned in sorted order
    }

    #[test]
    fn test_empty_tree_operations() {
        // Search on empty → None
        // Delete on empty → KeyNotFound
        // Cursor on empty → empty iterator
    }
}
```

## Common mistakes to avoid

1. **Binary search**: beware of comparison direction for UUIDs (big-endian byte comparison)
2. **Split**: don't forget to update the `prev_leaf`/`next_leaf` of neighbors
3. **Merge**: put the parent key back into the merged node
4. **Root**: special case when the root is a leaf (height=1)
5. **Page 0**: reserved by PageManager (free-list). Metadata in page 1, root in page 2
6. **UUID comparison**: use lexicographic comparison on the 16 raw bytes (`Uuid::as_bytes()`)
7. **find_child**: linear scan (first entry with key > search_key), not binary_search, to avoid convention bugs
8. **insert_entry**: handles two cases (insertion before an existing entry, or at the end with right_child update)
