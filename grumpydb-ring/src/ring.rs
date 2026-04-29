//! [`Ring`]: a consistent-hash ring with virtual nodes.
//!
//! Algorithm
//! ---------
//! For every physical node we hash `format!("{node}#{i}")` for
//! `i in 0..vnodes_per_node` to obtain its vnode positions on a 64-bit
//! circle. The combined list of `(position, owner)` pairs is kept sorted
//! by position. Routing a key is a binary search on the position vector
//! followed by a linear walk to skip duplicate physical owners.

use std::collections::BTreeSet;
use std::fmt;
use std::hash::Hash;

use serde::{Deserialize, Serialize};

use crate::hash::{RoutingKey, murmur3_hash};

/// Configuration that determines how vnodes are placed on the ring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingConfig {
    /// Number of vnodes per physical node. Cassandra default is 256.
    /// Higher = smoother distribution but more memory and slightly
    /// slower lookups; lower = uneven hot spots when nodes leave.
    pub vnodes_per_node: u32,
}

impl Default for RingConfig {
    fn default() -> Self {
        Self {
            vnodes_per_node: 256,
        }
    }
}

/// A consistent-hash ring carrying `NodeId` ownership of vnodes.
///
/// Generic over `NodeId: Clone + Eq + Ord + Hash + Display + Debug`.
/// In tests this is `&'static str`; in production it's the cluster
/// identity `Uuid` (or any newtype around it).
#[derive(Debug, Clone)]
pub struct Ring<NodeId> {
    cfg: RingConfig,
    /// All vnode positions on the ring, sorted by hash.
    /// Each entry is `(hash_position, owner)`.
    vnodes: Vec<(u64, NodeId)>,
    /// Distinct physical nodes currently in the ring (sorted).
    physical_nodes: Vec<NodeId>,
}

/// A contiguous range of hash positions on the ring.
///
/// Used to describe the keys that change ownership when the ring
/// topology mutates (Phase 49 v6 rebalancing). The range is half-open:
/// `[start_inclusive, end_exclusive)`. A range may wrap around `u64::MAX`
/// — when it does, it's emitted as two separate ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRange {
    /// First hash position included in the range.
    pub start_inclusive: u64,
    /// First hash position **after** the range.
    pub end_exclusive: u64,
    /// Previous owner of the range, or `None` if the range was unowned
    /// (e.g. when the very first node is added to an empty ring).
    pub from: Option<NodeIdOpaque>,
    /// New owner of the range.
    pub to: NodeIdOpaque,
}

/// Type-erased `NodeId` carried in [`KeyRange`].
///
/// Stored as the canonical [`Display`](fmt::Display) string so the
/// public `KeyRange` type doesn't need to be triple-generic. Callers
/// that need a strongly-typed `NodeId` back can parse it themselves
/// (e.g. `Uuid::parse_str`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeIdOpaque(pub String);

impl NodeIdOpaque {
    fn from_node<N: fmt::Display>(node: &N) -> Self {
        Self(node.to_string())
    }
}

/// Errors returned by the ring.
///
/// In v5 the public `Ring` API never returns `RingError` —
/// `preference_list` clamps the requested replica count silently.
/// The error type exists for v6 strict mode where insufficient
/// nodes will be a hard error at coordinator boot.
#[derive(thiserror::Error, Debug)]
pub enum RingError {
    /// `preference_list(_, n)` was called with `n` larger than the
    /// number of physical nodes currently in the ring.
    #[error("preference_list called with n={n}, ring has {nodes} nodes")]
    NotEnoughNodes {
        /// Replica count requested by the caller.
        n: usize,
        /// Number of distinct physical nodes currently in the ring.
        nodes: usize,
    },
}

impl<NodeId> Ring<NodeId>
where
    NodeId: Clone + Eq + Ord + Hash + fmt::Display + fmt::Debug,
{
    /// Build a new empty ring with the given configuration.
    pub fn new(cfg: RingConfig) -> Self {
        Self {
            cfg,
            vnodes: Vec::new(),
            physical_nodes: Vec::new(),
        }
    }

    /// Total number of vnodes currently placed
    /// (= physical nodes × `vnodes_per_node`).
    pub fn vnode_count(&self) -> usize {
        self.vnodes.len()
    }

    /// All distinct physical nodes currently in the ring (sorted).
    pub fn nodes(&self) -> &[NodeId] {
        &self.physical_nodes
    }

    /// Current ring configuration.
    pub fn config(&self) -> &RingConfig {
        &self.cfg
    }

    /// Add a physical node. Idempotent — adding an already-present
    /// node is a no-op and returns an empty `Vec`.
    ///
    /// Returns the [`KeyRange`]s that have moved owners as a result of
    /// the addition (for v6 rebalancing). On the very first insertion
    /// the entire `[0, u64::MAX)` circle is reported as moving from
    /// `None` to the new node.
    pub fn add_node(&mut self, node: NodeId) -> Vec<KeyRange> {
        if self.physical_nodes.contains(&node) {
            return Vec::new();
        }

        let before = self.snapshot_owners();
        let new_vnodes = self.compute_vnodes_for(&node);
        for vn in new_vnodes {
            // Insert keeping the vector sorted by position.
            // Ties on position (extremely rare with Murmur3 64-bit)
            // are broken by physical node id ordering.
            let pos = self
                .vnodes
                .binary_search_by(|probe| probe.0.cmp(&vn.0).then_with(|| probe.1.cmp(&vn.1)))
                .unwrap_or_else(|e| e);
            self.vnodes.insert(pos, vn);
        }

        // Maintain physical_nodes sorted for deterministic .nodes().
        let pos = self
            .physical_nodes
            .binary_search(&node)
            .unwrap_or_else(|e| e);
        self.physical_nodes.insert(pos, node);

        let after = self.snapshot_owners();
        diff_owners(&before, &after)
    }

    /// Remove a physical node. Idempotent — removing an absent node is
    /// a no-op and returns an empty `Vec`.
    ///
    /// Returns the [`KeyRange`]s that need to be taken over by the
    /// surviving owners.
    pub fn remove_node(&mut self, node: &NodeId) -> Vec<KeyRange> {
        if !self.physical_nodes.contains(node) {
            return Vec::new();
        }

        let before = self.snapshot_owners();
        self.vnodes.retain(|(_, owner)| owner != node);
        self.physical_nodes.retain(|n| n != node);
        let after = self.snapshot_owners();
        diff_owners(&before, &after)
    }

    /// First `n` distinct physical nodes encountered when walking the
    /// ring clockwise starting at the position of `key`.
    ///
    /// `n` is clamped to `self.nodes().len()` so:
    /// - empty ring returns an empty `Vec`,
    /// - single-node ring returns `[that_node]` regardless of `n`,
    /// - calling with `n = 0` returns an empty `Vec`.
    pub fn preference_list(&self, key: &RoutingKey<'_>, n: usize) -> Vec<NodeId> {
        if self.physical_nodes.is_empty() || n == 0 {
            return Vec::new();
        }
        let n = n.min(self.physical_nodes.len());

        let pos = murmur3_hash(&key.canonical_bytes());
        // Bisect for the first vnode whose position is >= pos.
        let start = self
            .vnodes
            .binary_search_by_key(&pos, |&(p, _)| p)
            .unwrap_or_else(|e| e);

        let mut seen: BTreeSet<&NodeId> = BTreeSet::new();
        let mut out: Vec<NodeId> = Vec::with_capacity(n);

        // Walk forward, then wrap. Worst case: 2 * vnodes (one full lap
        // when the ring has many vnodes for the same owner clustered).
        let total = self.vnodes.len();
        for step in 0..total {
            let idx = (start + step) % total;
            let owner = &self.vnodes[idx].1;
            if seen.insert(owner) {
                out.push(owner.clone());
                if out.len() == n {
                    break;
                }
            }
        }

        out
    }

    /// Returns true iff `node` appears in `preference_list(key, n)`.
    pub fn owns(&self, node: &NodeId, key: &RoutingKey<'_>, n: usize) -> bool {
        self.preference_list(key, n).iter().any(|o| o == node)
    }

    // -------- internal helpers --------

    fn compute_vnodes_for(&self, node: &NodeId) -> Vec<(u64, NodeId)> {
        let mut out = Vec::with_capacity(self.cfg.vnodes_per_node as usize);
        for i in 0..self.cfg.vnodes_per_node {
            // Tag = "{node}#{i}". The `#` and decimal `i` keep the
            // tagged string injective for any sane Display impl.
            let tag = format!("{node}#{i}");
            let pos = murmur3_hash(tag.as_bytes());
            out.push((pos, node.clone()));
        }
        out
    }

    /// Snapshot of "owner of the segment ending at this vnode position",
    /// for diffing topology changes.
    fn snapshot_owners(&self) -> Vec<(u64, NodeId)> {
        self.vnodes.clone()
    }
}

/// Diff two ordered vnode-owner snapshots and emit the
/// [`KeyRange`]s whose owners changed.
///
/// The ring covers the full `[0, u64::MAX]` circle with each segment
/// owned by the next vnode clockwise (so segment `(prev_pos, vnode_pos]`
/// is owned by the vnode at `vnode_pos`). We model this as half-open
/// `[prev_pos+1, vnode_pos+1)` and special-case the wrap from the last
/// vnode back to the first.
///
/// For diffing we generate the pairwise (prev_pos, this_pos, owner) for
/// each of `before` and `after`, then walk both sorted streams and emit
/// `KeyRange`s wherever the owner of an overlapping segment differs.
///
/// Adjacent same-owner ranges in the output are merged so we don't
/// emit 256 tiny ranges per node addition.
fn diff_owners<NodeId>(before: &[(u64, NodeId)], after: &[(u64, NodeId)]) -> Vec<KeyRange>
where
    NodeId: Clone + Eq + fmt::Display,
{
    if after.is_empty() && before.is_empty() {
        return Vec::new();
    }

    // Empty -> N : the new ring owns everything.
    if before.is_empty() {
        // Use a single range covering the whole circle; the owner is
        // whichever vnode owns position 0 (== first vnode in the
        // sorted "after" list, by our wrap-around convention). To keep
        // the API honest we just emit one range owned by the new node.
        // (When multiple nodes are added at once we'd lose detail; v5
        // only adds one node at a time.)
        let owner = &after[0].1;
        return vec![KeyRange {
            start_inclusive: 0,
            end_exclusive: u64::MAX,
            from: None,
            to: NodeIdOpaque::from_node(owner),
        }];
    }

    // N -> empty : everything is unowned afterwards. We emit one range
    // per previous owner — the type system forces a `to`, so we instead
    // collapse to a single "from = first owner, to = first owner" no-op
    // synthesised range here. In practice v5 never empties the ring.
    if after.is_empty() {
        return Vec::new();
    }

    // Build segment views: each entry is
    // (segment_end_exclusive, owner).
    // The segment owned by vnode at position p covers (prev_p, p],
    // which in half-open form is [prev_p+1, p+1).
    // For position 0 we wrap: segment is [last_p+1, 0+1) split into
    // [last_p+1, u64::MAX] and [0, 1). We avoid the awkward split by
    // sampling owners at a finite set of "boundary" positions: every
    // distinct vnode position in either snapshot.
    let mut boundaries: BTreeSet<u64> = BTreeSet::new();
    for (p, _) in before.iter().chain(after.iter()) {
        boundaries.insert(*p);
        // Also add p+1 wrap if needed — but the boundary set is enough
        // because we always sample segment owners *clockwise*.
    }
    boundaries.insert(0);
    boundaries.insert(u64::MAX);

    let owners_at = |snap: &[(u64, NodeId)], pos: u64| -> Option<NodeId> {
        if snap.is_empty() {
            return None;
        }
        // Owner of `pos` = first vnode with position >= pos.
        // If `pos > max_pos` we wrap to the first vnode.
        match snap.binary_search_by_key(&pos, |&(p, _)| p) {
            Ok(idx) => Some(snap[idx].1.clone()),
            Err(idx) => {
                if idx == snap.len() {
                    Some(snap[0].1.clone())
                } else {
                    Some(snap[idx].1.clone())
                }
            }
        }
    };

    // Sweep across boundary-to-boundary segments. For each segment,
    // sample the owner at its start position in both snapshots; if
    // they differ, emit a KeyRange.
    let mut out: Vec<KeyRange> = Vec::new();
    let bounds: Vec<u64> = boundaries.into_iter().collect();
    for w in bounds.windows(2) {
        let start = w[0];
        let end = w[1];
        if start == end {
            continue;
        }
        let before_owner = owners_at(before, start);
        let after_owner = owners_at(after, start);
        if before_owner != after_owner
            && let Some(to) = after_owner
        {
            let new_range = KeyRange {
                start_inclusive: start,
                end_exclusive: end,
                from: before_owner.as_ref().map(NodeIdOpaque::from_node),
                to: NodeIdOpaque::from_node(&to),
            };
            // Merge with the previous emitted range when adjacent and
            // same (from, to). Cuts the count from ~256 ranges per
            // add_node down to a handful.
            if let Some(last) = out.last_mut()
                && last.end_exclusive == new_range.start_inclusive
                && last.from == new_range.from
                && last.to == new_range.to
            {
                last.end_exclusive = new_range.end_exclusive;
            } else {
                out.push(new_range);
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    fn ring_with(nodes: &[&'static str]) -> Ring<&'static str> {
        let mut r = Ring::new(RingConfig::default());
        for n in nodes {
            r.add_node(*n);
        }
        r
    }

    fn key(s: &str) -> RoutingKey<'_> {
        RoutingKey {
            database: "db",
            collection: "coll",
            key_bytes: s.as_bytes(),
        }
    }

    // -------- stability --------

    #[test]
    fn test_empty_ring_preference_list_is_empty() {
        let r: Ring<&'static str> = Ring::new(RingConfig::default());
        assert!(r.preference_list(&key("anything"), 3).is_empty());
        assert_eq!(r.vnode_count(), 0);
        assert!(r.nodes().is_empty());
    }

    #[test]
    fn test_single_node_ring_preference_list_returns_that_node() {
        let r = ring_with(&["only"]);
        let pl = r.preference_list(&key("k1"), 5);
        assert_eq!(pl, vec!["only"]);
    }

    #[test]
    fn test_add_node_idempotent() {
        let mut r = ring_with(&["A", "B"]);
        let before_vnodes = r.vnode_count();
        let ranges = r.add_node("A");
        assert!(ranges.is_empty());
        assert_eq!(r.vnode_count(), before_vnodes);
        assert_eq!(r.nodes(), &["A", "B"]);
    }

    #[test]
    fn test_remove_unknown_node_no_op() {
        let mut r = ring_with(&["A", "B"]);
        let before_vnodes = r.vnode_count();
        let ranges = r.remove_node(&"Z");
        assert!(ranges.is_empty());
        assert_eq!(r.vnode_count(), before_vnodes);
    }

    #[test]
    fn test_zero_n_returns_empty() {
        let r = ring_with(&["A", "B", "C"]);
        assert!(r.preference_list(&key("foo"), 0).is_empty());
    }

    #[test]
    fn test_n_clamped_to_node_count() {
        let r = ring_with(&["A", "B"]);
        let pl = r.preference_list(&key("foo"), 99);
        assert_eq!(pl.len(), 2);
    }

    // -------- distribution --------

    #[test]
    fn test_distribution_uniformity_chi_squared() {
        // 10 nodes, 1M random keys. With 256 vnodes/node Murmur3
        // should keep every node within +/-30% of the mean.
        let nodes: Vec<&'static str> =
            vec!["n1", "n2", "n3", "n4", "n5", "n6", "n7", "n8", "n9", "n10"];
        let r = ring_with(&nodes);

        const N: u64 = 1_000_000;
        let mut counts: HashMap<&'static str, u64> = HashMap::new();
        for i in 0..N {
            // Build a varied key cheaply without pulling rand.
            let s = format!("key-{i}");
            let pl = r.preference_list(&key(&s), 1);
            *counts.entry(pl[0]).or_insert(0) += 1;
        }

        let mean = N as f64 / nodes.len() as f64;
        for n in &nodes {
            let c = *counts.get(n).unwrap_or(&0) as f64;
            let dev = (c - mean).abs() / mean;
            assert!(
                dev < 0.30,
                "node {n} holds {c} keys (mean {mean}, dev {dev})"
            );
        }
    }

    #[test]
    fn test_three_node_preference_list_returns_three_distinct_nodes() {
        let r = ring_with(&["A", "B", "C"]);
        for i in 0..200 {
            let s = format!("k-{i}");
            let pl = r.preference_list(&key(&s), 3);
            assert_eq!(pl.len(), 3, "key {s} -> {pl:?}");
            let unique: BTreeSet<_> = pl.iter().collect();
            assert_eq!(unique.len(), 3, "key {s} -> {pl:?}");
        }
    }

    // -------- membership change --------

    #[test]
    fn test_add_node_moves_one_n_th_of_keys() {
        let r3 = ring_with(&["A", "B", "C"]);
        let mut r4 = r3.clone();
        r4.add_node("D");

        const N: u64 = 50_000;
        let mut moved = 0u64;
        for i in 0..N {
            let s = format!("k-{i}");
            let k = key(&s);
            let a = r3.preference_list(&k, 1)[0];
            let b = r4.preference_list(&k, 1)[0];
            if a != b {
                moved += 1;
            }
        }
        let frac = moved as f64 / N as f64;
        // Expected ~0.25 (1/4 of keys reassign when going 3->4 nodes).
        // Loose bounds: 0.18 .. 0.32.
        assert!(
            (0.18..0.32).contains(&frac),
            "moved fraction = {frac} (expected ~0.25)"
        );
    }

    #[test]
    fn test_remove_node_redistributes() {
        let r3 = ring_with(&["A", "B", "C"]);
        let mut r2 = r3.clone();
        r2.remove_node(&"C");

        const N: u64 = 50_000;
        let mut moved = 0u64;
        for i in 0..N {
            let s = format!("k-{i}");
            let k = key(&s);
            let a = r3.preference_list(&k, 1)[0];
            let b = r2.preference_list(&k, 1)[0];
            if a != b {
                moved += 1;
            }
        }
        let frac = moved as f64 / N as f64;
        // Expected ~0.33 (the keys formerly owned by C are reassigned).
        assert!(
            (0.25..0.42).contains(&frac),
            "moved fraction = {frac} (expected ~0.33)"
        );
    }

    // -------- key range --------

    #[test]
    fn test_keyrange_partition_covers_full_circle() {
        // Add three nodes one by one, accumulating the union of the
        // emitted KeyRanges. After all three, the union must cover
        // the full circle.
        let mut r: Ring<&'static str> = Ring::new(RingConfig::default());
        let mut union: Vec<(u64, u64)> = Vec::new();
        for n in ["A", "B", "C"] {
            for kr in r.add_node(n) {
                union.push((kr.start_inclusive, kr.end_exclusive));
            }
        }
        union.sort();

        // Merge overlapping/contiguous segments.
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (s, e) in union {
            match merged.last_mut() {
                Some(last) if last.1 >= s => {
                    last.1 = last.1.max(e);
                }
                _ => merged.push((s, e)),
            }
        }

        // We want full coverage: [0, u64::MAX]. The first add_node emits
        // a single [0, u64::MAX) range, so further adds only narrow
        // ownership inside that — coverage stays complete.
        assert!(!merged.is_empty());
        assert_eq!(merged.first().unwrap().0, 0);
        assert_eq!(merged.last().unwrap().1, u64::MAX);
    }

    #[test]
    fn test_add_node_emits_some_ranges() {
        let mut r: Ring<&'static str> = Ring::new(RingConfig::default());
        let r1 = r.add_node("A");
        assert_eq!(r1.len(), 1, "first add covers the whole circle");
        assert_eq!(r1[0].from, None);
        assert_eq!(r1[0].to, NodeIdOpaque("A".into()));

        let r2 = r.add_node("B");
        // B steals roughly half the segments — lots of small ranges
        // (post-merge typically 100+ but bounded by 256).
        assert!(!r2.is_empty(), "adding a second node must move ranges");
        for kr in &r2 {
            assert_eq!(kr.from, Some(NodeIdOpaque("A".into())));
            assert_eq!(kr.to, NodeIdOpaque("B".into()));
        }
    }

    #[test]
    fn test_remove_node_emits_ranges_to_survivors() {
        let mut r = ring_with(&["A", "B", "C"]);
        let ranges = r.remove_node(&"C");
        assert!(!ranges.is_empty());
        for kr in &ranges {
            assert_eq!(kr.from, Some(NodeIdOpaque("C".into())));
            assert!(matches!(
                kr.to,
                NodeIdOpaque(ref s) if s == "A" || s == "B"
            ));
        }
    }

    // -------- determinism --------

    #[test]
    fn test_ring_is_deterministic() {
        let r1 = ring_with(&["A", "B", "C"]);
        let r2 = ring_with(&["A", "B", "C"]);
        assert_eq!(r1.vnodes, r2.vnodes);
        assert_eq!(r1.physical_nodes, r2.physical_nodes);
    }

    #[test]
    fn test_ring_insertion_order_independent() {
        let r1 = ring_with(&["A", "B", "C"]);
        let r2 = ring_with(&["C", "A", "B"]);
        // Same vnode set, possibly different insertion order — but the
        // sorted vector should be identical.
        assert_eq!(r1.vnodes, r2.vnodes);
    }

    // -------- proptest --------

    fn prop_ring(node_count: usize) -> Ring<String> {
        let mut r = Ring::new(RingConfig::default());
        for i in 0..node_count {
            r.add_node(format!("node-{i}"));
        }
        r
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn proptest_preference_list_size(
            node_count in 1usize..=10,
            n in 0usize..=15,
            k in any::<Vec<u8>>(),
        ) {
            let ring = prop_ring(node_count);
            let routing = RoutingKey { database: "d", collection: "c", key_bytes: &k };
            let pl = ring.preference_list(&routing, n);
            let expected = n.min(node_count);
            prop_assert_eq!(pl.len(), expected);
        }

        #[test]
        fn proptest_preference_list_distinct(
            node_count in 1usize..=10,
            n in 0usize..=15,
            k in any::<Vec<u8>>(),
        ) {
            let ring = prop_ring(node_count);
            let routing = RoutingKey { database: "d", collection: "c", key_bytes: &k };
            let pl = ring.preference_list(&routing, n);
            let unique: BTreeSet<_> = pl.iter().collect();
            prop_assert_eq!(unique.len(), pl.len());
        }

        #[test]
        fn proptest_owns_consistent_with_preference_list(
            node_count in 1usize..=10,
            n in 0usize..=15,
            k in any::<Vec<u8>>(),
        ) {
            let ring = prop_ring(node_count);
            let routing = RoutingKey { database: "d", collection: "c", key_bytes: &k };
            let pl = ring.preference_list(&routing, n);
            for i in 0..node_count {
                let nid = format!("node-{i}");
                let in_pl = pl.contains(&nid);
                prop_assert_eq!(ring.owns(&nid, &routing, n), in_pl);
            }
        }
    }
}
