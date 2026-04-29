//! Vector clock: per-node monotonic counter, supporting the standard
//! comparison `Equal | LessThan | GreaterThan | Concurrent`.
//!
//! Vector clocks are stamped on every WAL record alongside the HLC.
//! In v5 we always emit a singleton `{ self: hlc.0 }`; v6 will start
//! merging clocks during replication.

use std::collections::BTreeMap;

/// Maximum number of entries allowed in a vector clock. Defensive cap
/// against malformed input from the network or disk. In practice
/// real-world clusters have far fewer than 100 nodes.
pub const MAX_VCLOCK_ENTRIES: u16 = 4096;

/// Size of one entry on disk: u128 node_id (16) + u64 counter (8).
pub const VCLOCK_ENTRY_SIZE: usize = 16 + 8;

/// A pointwise per-node counter. Sorted by `node_id` for stable
/// serialisation across runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VectorClock {
    entries: BTreeMap<u128, u64>,
}

/// The four possible orderings of two vector clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VClockOrdering {
    /// Every entry of `self` equals the corresponding entry of `other`.
    Equal,
    /// Every entry of `self` is `<=` corresponding, and at least one is `<`.
    LessThan,
    /// Symmetric of `LessThan`.
    GreaterThan,
    /// Neither dominates the other.
    Concurrent,
}

/// Errors raised when decoding a vector clock from bytes.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum VClockError {
    /// The input buffer is shorter than what the length-prefix claims.
    #[error("truncated vector clock: need {needed} bytes, have {have}")]
    Truncated {
        /// Number of bytes the encoded clock claims to occupy.
        needed: usize,
        /// Number of bytes actually available in the buffer.
        have: usize,
    },
    /// The length-prefix exceeds [`MAX_VCLOCK_ENTRIES`].
    #[error("vector clock has too many entries: {0} (max 4096)")]
    TooManyEntries(u16),
}

impl VectorClock {
    /// Constructs an empty vector clock.
    pub fn new() -> Self {
        Self::default()
    }

    /// Constructs a vector clock with a single `(node_id, counter)` entry.
    pub fn singleton(node_id: u128, counter: u64) -> Self {
        let mut entries = BTreeMap::new();
        entries.insert(node_id, counter);
        Self { entries }
    }

    /// Returns the counter for `node_id`, or `0` if absent.
    pub fn get(&self, node_id: u128) -> u64 {
        self.entries.get(&node_id).copied().unwrap_or(0)
    }

    /// Sets (or overwrites) the counter for `node_id`.
    pub fn set(&mut self, node_id: u128, counter: u64) {
        self.entries.insert(node_id, counter);
    }

    /// Increments the counter for `node_id`. Returns the new value.
    pub fn bump(&mut self, node_id: u128) -> u64 {
        let entry = self.entries.entry(node_id).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }

    /// Pointwise max merge: `self[k] = max(self[k], other[k])` for every
    /// node id present in either clock.
    pub fn merge(&mut self, other: &VectorClock) {
        for (&node, &counter) in &other.entries {
            let cur = self.entries.entry(node).or_insert(0);
            if counter > *cur {
                *cur = counter;
            }
        }
    }

    /// Returns the standard vector clock ordering between `self` and `other`.
    pub fn compare(&self, other: &VectorClock) -> VClockOrdering {
        let mut le = true;
        let mut ge = true;

        // Walk the union of both keysets.
        let all_keys: std::collections::BTreeSet<u128> = self
            .entries
            .keys()
            .chain(other.entries.keys())
            .copied()
            .collect();
        for k in all_keys {
            let a = self.get(k);
            let b = other.get(k);
            if a > b {
                le = false;
            }
            if a < b {
                ge = false;
            }
        }

        match (le, ge) {
            (true, true) => VClockOrdering::Equal,
            (true, false) => VClockOrdering::LessThan,
            (false, true) => VClockOrdering::GreaterThan,
            (false, false) => VClockOrdering::Concurrent,
        }
    }

    /// Returns the number of entries currently held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the clock has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates entries in ascending `node_id` order.
    pub fn iter(&self) -> impl Iterator<Item = (u128, u64)> + '_ {
        self.entries.iter().map(|(&k, &v)| (k, v))
    }

    /// Returns the size in bytes of the on-disk encoding.
    pub fn encoded_len(&self) -> usize {
        2 + self.entries.len() * VCLOCK_ENTRY_SIZE
    }

    /// Appends the on-disk encoding to `buf`. Layout:
    ///   `u16 num_entries (LE)` then `num_entries × (u128 node_id LE + u64 counter LE)`.
    pub fn encode_to(&self, buf: &mut Vec<u8>) {
        let n = self.entries.len() as u16;
        buf.extend_from_slice(&n.to_le_bytes());
        for (&node, &counter) in &self.entries {
            buf.extend_from_slice(&node.to_le_bytes());
            buf.extend_from_slice(&counter.to_le_bytes());
        }
    }

    /// Decodes a vector clock from `buf`. Returns the clock and the
    /// number of bytes consumed.
    pub fn decode(buf: &[u8]) -> Result<(Self, usize), VClockError> {
        if buf.len() < 2 {
            return Err(VClockError::Truncated {
                needed: 2,
                have: buf.len(),
            });
        }
        let n = u16::from_le_bytes([buf[0], buf[1]]);
        if n > MAX_VCLOCK_ENTRIES {
            return Err(VClockError::TooManyEntries(n));
        }
        let total = 2 + (n as usize) * VCLOCK_ENTRY_SIZE;
        if buf.len() < total {
            return Err(VClockError::Truncated {
                needed: total,
                have: buf.len(),
            });
        }
        let mut entries = BTreeMap::new();
        let mut cur = 2usize;
        for _ in 0..n {
            let mut nid = [0u8; 16];
            nid.copy_from_slice(&buf[cur..cur + 16]);
            let node = u128::from_le_bytes(nid);
            cur += 16;
            let mut cb = [0u8; 8];
            cb.copy_from_slice(&buf[cur..cur + 8]);
            let counter = u64::from_le_bytes(cb);
            cur += 8;
            entries.insert(node, counter);
        }
        Ok((Self { entries }, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng;

    #[test]
    fn test_vclock_compare_equal() {
        let a = VectorClock::singleton(1, 5);
        let b = VectorClock::singleton(1, 5);
        assert_eq!(a.compare(&b), VClockOrdering::Equal);
    }

    #[test]
    fn test_vclock_compare_less_than() {
        let a = VectorClock::singleton(1, 4);
        let b = VectorClock::singleton(1, 5);
        assert_eq!(a.compare(&b), VClockOrdering::LessThan);
        // With multiple entries.
        let mut a2 = VectorClock::new();
        a2.set(1, 4);
        a2.set(2, 7);
        let mut b2 = VectorClock::new();
        b2.set(1, 4);
        b2.set(2, 8);
        assert_eq!(a2.compare(&b2), VClockOrdering::LessThan);
    }

    #[test]
    fn test_vclock_compare_greater_than() {
        let a = VectorClock::singleton(1, 5);
        let b = VectorClock::singleton(1, 4);
        assert_eq!(a.compare(&b), VClockOrdering::GreaterThan);
    }

    #[test]
    fn test_vclock_compare_concurrent() {
        let a = VectorClock::singleton(1, 1);
        let b = VectorClock::singleton(2, 1);
        assert_eq!(a.compare(&b), VClockOrdering::Concurrent);

        let mut a2 = VectorClock::new();
        a2.set(1, 2);
        a2.set(2, 1);
        let mut b2 = VectorClock::new();
        b2.set(1, 1);
        b2.set(2, 2);
        assert_eq!(a2.compare(&b2), VClockOrdering::Concurrent);
    }

    #[test]
    fn test_vclock_merge_pointwise_max() {
        let mut a = VectorClock::new();
        a.set(1, 3);
        a.set(2, 5);
        let mut b = VectorClock::new();
        b.set(1, 7);
        b.set(3, 9);
        a.merge(&b);
        assert_eq!(a.get(1), 7);
        assert_eq!(a.get(2), 5);
        assert_eq!(a.get(3), 9);
    }

    #[test]
    fn test_vclock_bump_increments() {
        let mut a = VectorClock::new();
        assert_eq!(a.bump(42), 1);
        assert_eq!(a.bump(42), 2);
        assert_eq!(a.bump(7), 1);
        assert_eq!(a.get(42), 2);
        assert_eq!(a.get(7), 1);
    }

    #[test]
    fn test_vclock_encode_decode_round_trip() {
        let mut a = VectorClock::new();
        a.set(0xdeadbeef, 42);
        a.set(0xbadc0ffee, 7);
        a.set(123456789, 0);
        let mut buf = Vec::new();
        a.encode_to(&mut buf);
        assert_eq!(buf.len(), a.encoded_len());
        let (b, n) = VectorClock::decode(&buf).unwrap();
        assert_eq!(n, buf.len());
        assert_eq!(a, b);
    }

    #[test]
    fn test_vclock_empty_round_trip() {
        let a = VectorClock::new();
        let mut buf = Vec::new();
        a.encode_to(&mut buf);
        assert_eq!(buf, &[0u8, 0u8]);
        let (b, n) = VectorClock::decode(&buf).unwrap();
        assert_eq!(n, 2);
        assert!(b.is_empty());
    }

    #[test]
    fn test_vclock_decode_truncated_returns_error() {
        // num_entries=3 but only 1 entry of payload provided.
        let mut buf = Vec::new();
        buf.extend_from_slice(&3u16.to_le_bytes());
        buf.extend_from_slice(&1u128.to_le_bytes());
        buf.extend_from_slice(&7u64.to_le_bytes());
        let err = VectorClock::decode(&buf).unwrap_err();
        assert!(matches!(err, VClockError::Truncated { .. }));
    }

    #[test]
    fn test_vclock_decode_too_many_entries() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_VCLOCK_ENTRIES + 1).to_le_bytes());
        let err = VectorClock::decode(&buf).unwrap_err();
        assert!(matches!(err, VClockError::TooManyEntries(_)));
    }

    /// Randomised round-trip property check (acts as a lightweight
    /// proptest — kept in-tree to avoid pulling in the proptest crate).
    #[test]
    fn vclock_encode_decode_proptest() {
        let mut rng = rand::thread_rng();
        for _ in 0..200 {
            let n: u16 = rng.gen_range(0..64);
            let mut a = VectorClock::new();
            for _ in 0..n {
                let node: u128 = rng.r#gen();
                let counter: u64 = rng.r#gen();
                a.set(node, counter);
            }
            let mut buf = Vec::new();
            a.encode_to(&mut buf);
            assert_eq!(buf.len(), a.encoded_len());
            let (b, consumed) = VectorClock::decode(&buf).unwrap();
            assert_eq!(consumed, buf.len());
            assert_eq!(a, b, "round-trip mismatch on n={n}");
            // Re-encoding b yields exactly the same bytes (stable).
            let mut buf2 = Vec::new();
            b.encode_to(&mut buf2);
            assert_eq!(buf, buf2, "encoding is not byte-stable");
        }
    }
}
