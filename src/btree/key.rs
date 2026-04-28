//! Key configuration for B+Tree variants.
//!
//! Defines shared types and utilities for both fixed-key (UUID) and
//! variable-length key B+Trees.

/// Maximum size of a variable-length key in bytes (excluding the 2-byte length prefix).
/// Composite keys for secondary indexes: type_tag(1) + value(≤128) + uuid(16) = 145 max.
pub const VAR_KEY_MAX_SIZE: usize = 256;

/// Size of the length prefix for variable-length keys.
pub const VAR_KEY_LEN_PREFIX: usize = 2;

/// Encodes a variable-length key with a 2-byte length prefix.
///
/// Layout: `key_len(u16 LE) + key_data[0..key_len]`
pub fn encode_var_key(key: &[u8]) -> Vec<u8> {
    assert!(
        key.len() <= VAR_KEY_MAX_SIZE,
        "key too large: {} > {}",
        key.len(),
        VAR_KEY_MAX_SIZE
    );
    let mut buf = Vec::with_capacity(VAR_KEY_LEN_PREFIX + key.len());
    buf.extend_from_slice(&(key.len() as u16).to_le_bytes());
    buf.extend_from_slice(key);
    buf
}

/// Decodes a variable-length key, returning `(key_data, total_bytes_consumed)`.
pub fn decode_var_key(buf: &[u8]) -> (&[u8], usize) {
    let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    (&buf[2..2 + len], 2 + len)
}

/// Returns the total on-disk size of a variable-length key (prefix + data).
pub fn var_key_disk_size(key: &[u8]) -> usize {
    VAR_KEY_LEN_PREFIX + key.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_var_key() {
        let key = b"hello world";
        let encoded = encode_var_key(key);
        assert_eq!(encoded.len(), 2 + 11);

        let (decoded, consumed) = decode_var_key(&encoded);
        assert_eq!(decoded, b"hello world");
        assert_eq!(consumed, 13);
    }

    #[test]
    fn test_encode_empty_key() {
        let encoded = encode_var_key(b"");
        assert_eq!(encoded.len(), 2);
        let (decoded, consumed) = decode_var_key(&encoded);
        assert_eq!(decoded, b"");
        assert_eq!(consumed, 2);
    }

    #[test]
    fn test_encode_max_size_key() {
        let key = vec![0xAB; VAR_KEY_MAX_SIZE];
        let encoded = encode_var_key(&key);
        assert_eq!(encoded.len(), 2 + VAR_KEY_MAX_SIZE);
        let (decoded, _) = decode_var_key(&encoded);
        assert_eq!(decoded, &key[..]);
    }

    #[test]
    #[should_panic(expected = "key too large")]
    fn test_encode_oversized_key_panics() {
        let key = vec![0; VAR_KEY_MAX_SIZE + 1];
        encode_var_key(&key);
    }

    #[test]
    fn test_var_key_ordering_preserved() {
        // Lexicographic byte ordering should be preserved
        let k1 = encode_var_key(b"aaa");
        let k2 = encode_var_key(b"bbb");
        let k3 = encode_var_key(b"aab");

        // Compare the raw key data (not the length prefix)
        let (d1, _) = decode_var_key(&k1);
        let (d2, _) = decode_var_key(&k2);
        let (d3, _) = decode_var_key(&k3);

        assert!(d1 < d2);
        assert!(d1 < d3);
        assert!(d3 < d2);
    }

    #[test]
    fn test_var_key_disk_size() {
        assert_eq!(var_key_disk_size(b"hello"), 7);
        assert_eq!(var_key_disk_size(b""), 2);
    }
}
