//! `Key` trait used by the generic B+Tree.
//!
//! The same B+Tree code path serves both:
//! - Primary indexes keyed by `Uuid` (fixed 16-byte keys).
//! - Secondary indexes keyed by `Vec<u8>` (variable-length keys, up to a
//!   per-tree `max_key_size`).
//!
//! Each `Key` impl describes its own on-disk encoding so that databases
//! created by either of the previous two implementations remain readable
//! byte-for-byte.

use std::fmt::Debug;

use uuid::Uuid;

use crate::error::GrumpyError;
use crate::page::PAGE_SIZE;

/// Maximum allowed size (in bytes) of a variable-length key payload, excluding
/// the 2-byte length prefix. Composite secondary index keys
/// (`type_tag(1) + value(≤128) + uuid(16) = 145`) fit comfortably below this.
pub const VAR_KEY_MAX_SIZE: usize = 256;

/// Length of the variable-length-key length prefix.
pub(super) const VAR_LEN_PREFIX: usize = 2;

/// A key type that can be stored in a B+Tree.
///
/// Each implementor decides how it is encoded inside a node's slot, and how
/// any per-tree configuration (e.g. `max_key_size` for variable keys) is
/// persisted on the meta page and on each node page.
///
/// Two impls are provided in this module:
/// - `Uuid`: fixed 16-byte keys. `Config = ()`. No node/tree meta bytes.
/// - `Vec<u8>`: variable-length keys, fixed-stride layout with a 2-byte
///   length prefix and zero-padding to `max_key_size`. `Config = u16`
///   (the `max_key_size`).
pub trait Key: Sized + Clone + Ord + Debug + 'static {
    /// Per-tree configuration describing how keys are laid out on disk.
    type Config: Copy + Eq + Debug;

    /// Bytes that the per-tree configuration occupies in the meta page,
    /// starting at offset 48 (after `root_page_id`, `height`, `num_entries`).
    const TREE_META_BYTES: u16;

    /// Bytes that the per-node configuration occupies in each node page,
    /// immediately after the standard header fields (`num_keys`/`right_child`
    /// for internal, `num_entries`/`next_leaf`/`prev_leaf` for leaves).
    const NODE_META_BYTES: u16;

    /// Reads the per-tree configuration from the meta page.
    fn read_tree_config(buf: &[u8; PAGE_SIZE]) -> Self::Config;

    /// Writes the per-tree configuration into the meta page.
    fn write_tree_config(cfg: Self::Config, buf: &mut [u8; PAGE_SIZE]);

    /// Reads the per-node configuration starting at the given offset.
    fn read_node_config(buf: &[u8; PAGE_SIZE], offset: usize) -> Self::Config;

    /// Writes the per-node configuration starting at the given offset.
    fn write_node_config(cfg: Self::Config, buf: &mut [u8; PAGE_SIZE], offset: usize);

    /// Number of bytes a single key occupies in a slot, *excluding* the
    /// trailing pointer. For fixed-stride layouts this is constant; for
    /// variable-length keys this is `2 + max_key_size`.
    fn slot_key_size(cfg: Self::Config) -> usize;

    /// Decodes a key from a slot of `slot_key_size(cfg)` bytes.
    fn read_key(buf: &[u8], cfg: Self::Config) -> Self;

    /// Encodes a key into a slot of `slot_key_size(cfg)` bytes (zero-padding
    /// the unused tail when applicable).
    fn write_key(&self, buf: &mut [u8], cfg: Self::Config);

    /// Builds the "duplicate key" error this trait surface should return when
    /// `insert()` finds an existing key.
    fn duplicate_key_error(self) -> GrumpyError;

    /// Builds the "key not found" error this trait surface should return when
    /// `delete()` cannot locate a key.
    fn key_not_found_error(&self) -> GrumpyError;
}

// ─────────────────────────────── Uuid impl ───────────────────────────────

/// Size, in bytes, of a UUID key.
const UUID_KEY_SIZE: usize = 16;

impl Key for Uuid {
    type Config = ();

    const TREE_META_BYTES: u16 = 0;
    const NODE_META_BYTES: u16 = 0;

    fn read_tree_config(_buf: &[u8; PAGE_SIZE]) -> Self::Config {}

    fn write_tree_config(_cfg: Self::Config, _buf: &mut [u8; PAGE_SIZE]) {}

    fn read_node_config(_buf: &[u8; PAGE_SIZE], _offset: usize) -> Self::Config {}

    fn write_node_config(_cfg: Self::Config, _buf: &mut [u8; PAGE_SIZE], _offset: usize) {}

    fn slot_key_size(_cfg: Self::Config) -> usize {
        UUID_KEY_SIZE
    }

    fn read_key(buf: &[u8], _cfg: Self::Config) -> Self {
        let mut bytes = [0u8; UUID_KEY_SIZE];
        bytes.copy_from_slice(&buf[..UUID_KEY_SIZE]);
        Uuid::from_bytes(bytes)
    }

    fn write_key(&self, buf: &mut [u8], _cfg: Self::Config) {
        buf[..UUID_KEY_SIZE].copy_from_slice(self.as_bytes());
    }

    fn duplicate_key_error(self) -> GrumpyError {
        GrumpyError::DuplicateKey(self)
    }

    fn key_not_found_error(&self) -> GrumpyError {
        GrumpyError::KeyNotFound(*self)
    }
}

// ───────────────────────────── Vec<u8> impl ─────────────────────────────

impl Key for Vec<u8> {
    type Config = u16;

    const TREE_META_BYTES: u16 = 2;
    const NODE_META_BYTES: u16 = 2;

    fn read_tree_config(buf: &[u8; PAGE_SIZE]) -> Self::Config {
        u16::from_le_bytes([buf[48], buf[49]])
    }

    fn write_tree_config(cfg: Self::Config, buf: &mut [u8; PAGE_SIZE]) {
        buf[48..50].copy_from_slice(&cfg.to_le_bytes());
    }

    fn read_node_config(buf: &[u8; PAGE_SIZE], offset: usize) -> Self::Config {
        u16::from_le_bytes([buf[offset], buf[offset + 1]])
    }

    fn write_node_config(cfg: Self::Config, buf: &mut [u8; PAGE_SIZE], offset: usize) {
        buf[offset..offset + 2].copy_from_slice(&cfg.to_le_bytes());
    }

    fn slot_key_size(cfg: Self::Config) -> usize {
        VAR_LEN_PREFIX + cfg as usize
    }

    fn read_key(buf: &[u8], _cfg: Self::Config) -> Self {
        let key_len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        buf[VAR_LEN_PREFIX..VAR_LEN_PREFIX + key_len].to_vec()
    }

    fn write_key(&self, buf: &mut [u8], cfg: Self::Config) {
        let key_len = self.len();
        debug_assert!(
            key_len <= cfg as usize,
            "key too large: {} > {}",
            key_len,
            cfg
        );
        buf[..2].copy_from_slice(&(key_len as u16).to_le_bytes());
        buf[VAR_LEN_PREFIX..VAR_LEN_PREFIX + key_len].copy_from_slice(self);
        // Bytes after `VAR_LEN_PREFIX + key_len` remain zero (page buffer was
        // zero-initialised by the caller).
    }

    fn duplicate_key_error(self) -> GrumpyError {
        GrumpyError::Codec(format!("duplicate key in VarBTree: {} bytes", self.len()))
    }

    fn key_not_found_error(&self) -> GrumpyError {
        GrumpyError::Codec(format!("key not found in VarBTree: {} bytes", self.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uuid_round_trip_through_slot() {
        let mut buf = [0u8; PAGE_SIZE];
        let original = Uuid::from_u128(0x1234_5678_9ABC_DEF0_FEDC_BA98_7654_3210);
        original.write_key(&mut buf[100..], ());
        let restored = Uuid::read_key(&buf[100..], ());
        assert_eq!(original, restored);
    }

    #[test]
    fn test_vec_round_trip_through_slot() {
        let mut buf = [0u8; PAGE_SIZE];
        let original = b"hello world".to_vec();
        let cfg: u16 = 32;
        let slot_size = <Vec<u8> as Key>::slot_key_size(cfg);
        original.write_key(&mut buf[100..100 + slot_size], cfg);
        let restored = <Vec<u8> as Key>::read_key(&buf[100..100 + slot_size], cfg);
        assert_eq!(original, restored);
    }

    #[test]
    fn test_vec_short_key_pads_with_zero() {
        // Writing a short key into a large slot should leave trailing bytes zero.
        let mut buf = [0u8; PAGE_SIZE];
        let cfg: u16 = 64;
        let slot_size = <Vec<u8> as Key>::slot_key_size(cfg);
        b"abc".to_vec().write_key(&mut buf[..slot_size], cfg);
        // Length prefix
        assert_eq!(u16::from_le_bytes([buf[0], buf[1]]), 3);
        assert_eq!(&buf[2..5], b"abc");
        // Tail is zero
        assert!(buf[5..slot_size].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_vec_max_size_key_round_trips() {
        let mut buf = [0u8; PAGE_SIZE];
        let cfg: u16 = VAR_KEY_MAX_SIZE as u16;
        let slot_size = <Vec<u8> as Key>::slot_key_size(cfg);
        let original = vec![0xABu8; VAR_KEY_MAX_SIZE];
        original.write_key(&mut buf[..slot_size], cfg);
        let restored = <Vec<u8> as Key>::read_key(&buf[..slot_size], cfg);
        assert_eq!(original, restored);
    }

    #[test]
    fn test_uuid_node_meta_is_zero_bytes() {
        // Uuid carries no per-node config: writing/reading at offset must be a no-op.
        let mut buf = [0u8; PAGE_SIZE];
        // Fill region with sentinel
        buf[40..50].copy_from_slice(&[0xFF; 10]);
        Uuid::write_node_config((), &mut buf, 40);
        // Region untouched
        assert!(buf[40..50].iter().all(|&b| b == 0xFF));
        // Reading from a Uuid-keyed node returns `()`, the unit type. We
        // intentionally invoke it to make sure the call doesn't trip an
        // assertion or panic.
        Uuid::read_node_config(&buf, 40);
    }

    #[test]
    fn test_vec_node_meta_round_trip() {
        let mut buf = [0u8; PAGE_SIZE];
        <Vec<u8>>::write_node_config(123u16, &mut buf, 40);
        assert_eq!(<Vec<u8>>::read_node_config(&buf, 40), 123u16);
    }

    #[test]
    fn test_uuid_ord_matches_byte_ord() {
        // Critical invariant: Uuid::cmp must match byte-lexicographic order
        // because internal-node descent uses Ord on K.
        let a = Uuid::from_bytes([0; 16]);
        let mut b_bytes = [0u8; 16];
        b_bytes[15] = 1;
        let b = Uuid::from_bytes(b_bytes);
        let mut c_bytes = [0u8; 16];
        c_bytes[0] = 1;
        let c = Uuid::from_bytes(c_bytes);
        assert!(a < b);
        assert!(b < c);
        assert!(a.as_bytes() < b.as_bytes());
        assert!(b.as_bytes() < c.as_bytes());
    }
}
