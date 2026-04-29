//! Sortable binary encoding for document field values.
//!
//! Encodes `Value` types into byte sequences that preserve natural ordering
//! under lexicographic byte comparison. This is critical for B+Tree range scans.

use uuid::Uuid;

use crate::document::value::Value;
use crate::error::{GrumpyError, Result};

/// Type tags for sortable encoding (ordering: Null < Bool < Integer < Float < String < Bytes < Ref).
const TAG_NULL: u8 = 0x00;
const TAG_BOOL: u8 = 0x01;
const TAG_INTEGER: u8 = 0x02;
const TAG_FLOAT: u8 = 0x03;
const TAG_STRING: u8 = 0x04;
const TAG_BYTES: u8 = 0x05;
const TAG_REF: u8 = 0x06;

/// Maximum length of string/bytes values in an index key (truncated if longer).
const MAX_INDEXED_LEN: usize = 128;

/// Encodes a `Value` into a sortable byte sequence.
///
/// The encoding preserves natural ordering:
/// - Null < Bool(false) < Bool(true) < Integer(-N) < Integer(0) < Integer(N)
///   < Float(-N) < Float(0) < Float(N) < String("a") < String("b") < Bytes
/// - Integers: sign bit flipped via XOR 0x8000000000000000
/// - Floats: IEEE 754 sortable transformation
/// - Strings/Bytes: truncated to 128 bytes
///
/// Arrays and Objects are not indexable and return `NotIndexable`.
pub fn encode_sortable_value(value: &Value) -> Result<Vec<u8>> {
    match value {
        Value::Null => Ok(vec![TAG_NULL]),
        Value::Bool(b) => Ok(vec![TAG_BOOL, if *b { 0x01 } else { 0x00 }]),
        Value::Integer(i) => {
            let mut buf = vec![TAG_INTEGER];
            // XOR with sign bit to make negative < positive in byte order
            let sortable = (*i as u64) ^ 0x8000_0000_0000_0000;
            buf.extend_from_slice(&sortable.to_be_bytes());
            Ok(buf)
        }
        Value::Float(f) => {
            let mut buf = vec![TAG_FLOAT];
            let bits = f.to_bits();
            // IEEE 754 sortable: if sign bit is set, flip all bits; else flip sign bit only
            let sortable = if bits & 0x8000_0000_0000_0000 != 0 {
                !bits // negative: flip all bits
            } else {
                bits ^ 0x8000_0000_0000_0000 // positive: flip sign bit
            };
            buf.extend_from_slice(&sortable.to_be_bytes());
            Ok(buf)
        }
        Value::String(s) => {
            let mut buf = vec![TAG_STRING];
            let bytes = s.as_bytes();
            let len = bytes.len().min(MAX_INDEXED_LEN);
            buf.extend_from_slice(&bytes[..len]);
            Ok(buf)
        }
        Value::Bytes(b) => {
            let mut buf = vec![TAG_BYTES];
            let len = b.len().min(MAX_INDEXED_LEN);
            buf.extend_from_slice(&b[..len]);
            Ok(buf)
        }
        Value::Ref(collection, uuid) => {
            let mut buf = vec![TAG_REF];
            let name_bytes = collection.as_bytes();
            let len = name_bytes.len().min(MAX_INDEXED_LEN);
            buf.extend_from_slice(&name_bytes[..len]);
            buf.extend_from_slice(uuid.as_bytes());
            Ok(buf)
        }
        Value::Array(_) | Value::Object(_) => Err(GrumpyError::NotIndexable),
        Value::Tombstone { .. } => Err(GrumpyError::NotIndexable),
    }
}

/// Encodes a composite key: sortable field value + UUID suffix.
///
/// This ensures uniqueness even when multiple documents have the same field value.
pub fn encode_composite_key(value: &Value, uuid: &Uuid) -> Result<Vec<u8>> {
    let mut key = encode_sortable_value(value)?;
    key.extend_from_slice(uuid.as_bytes());
    Ok(key)
}

/// Extracts a field value from a document using dot-notation path.
///
/// E.g., `extract_field(doc, "address.city")` returns `doc["address"]["city"]`.
pub fn extract_field<'a>(value: &'a Value, field_path: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in field_path.split('.') {
        match current {
            Value::Object(obj) => {
                current = obj.get(part)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn test_encode_null() {
        let encoded = encode_sortable_value(&Value::Null).unwrap();
        assert_eq!(encoded, vec![TAG_NULL]);
    }

    #[test]
    fn test_encode_bool_ordering() {
        let f = encode_sortable_value(&Value::Bool(false)).unwrap();
        let t = encode_sortable_value(&Value::Bool(true)).unwrap();
        assert!(f < t);
    }

    #[test]
    fn test_encode_integer_ordering() {
        let neg = encode_sortable_value(&Value::Integer(-100)).unwrap();
        let zero = encode_sortable_value(&Value::Integer(0)).unwrap();
        let pos = encode_sortable_value(&Value::Integer(100)).unwrap();
        let max = encode_sortable_value(&Value::Integer(i64::MAX)).unwrap();
        let min = encode_sortable_value(&Value::Integer(i64::MIN)).unwrap();

        assert!(min < neg);
        assert!(neg < zero);
        assert!(zero < pos);
        assert!(pos < max);
    }

    #[test]
    fn test_encode_float_ordering() {
        let neg = encode_sortable_value(&Value::Float(-1.5)).unwrap();
        let zero = encode_sortable_value(&Value::Float(0.0)).unwrap();
        let pos = encode_sortable_value(&Value::Float(1.5)).unwrap();

        assert!(neg < zero);
        assert!(zero < pos);
    }

    #[test]
    fn test_encode_string_ordering() {
        let a = encode_sortable_value(&Value::String("alpha".into())).unwrap();
        let b = encode_sortable_value(&Value::String("beta".into())).unwrap();
        let z = encode_sortable_value(&Value::String("zulu".into())).unwrap();

        assert!(a < b);
        assert!(b < z);
    }

    #[test]
    fn test_encode_cross_type_ordering() {
        let null = encode_sortable_value(&Value::Null).unwrap();
        let bool_v = encode_sortable_value(&Value::Bool(false)).unwrap();
        let int_v = encode_sortable_value(&Value::Integer(0)).unwrap();
        let float_v = encode_sortable_value(&Value::Float(0.0)).unwrap();
        let str_v = encode_sortable_value(&Value::String("a".into())).unwrap();

        assert!(null < bool_v);
        assert!(bool_v < int_v);
        assert!(int_v < float_v);
        assert!(float_v < str_v);
    }

    #[test]
    fn test_encode_string_truncation() {
        let long = "x".repeat(200);
        let encoded = encode_sortable_value(&Value::String(long)).unwrap();
        // tag(1) + 128 bytes max
        assert_eq!(encoded.len(), 1 + MAX_INDEXED_LEN);
    }

    #[test]
    fn test_encode_array_not_indexable() {
        let result = encode_sortable_value(&Value::Array(vec![]));
        assert!(matches!(result, Err(GrumpyError::NotIndexable)));
    }

    #[test]
    fn test_encode_object_not_indexable() {
        let result = encode_sortable_value(&Value::Object(BTreeMap::new()));
        assert!(matches!(result, Err(GrumpyError::NotIndexable)));
    }

    #[test]
    fn test_encode_composite_key() {
        let uuid = Uuid::from_u128(42);
        let key = encode_composite_key(&Value::Integer(100), &uuid).unwrap();
        // tag(1) + i64(8) + uuid(16) = 25 bytes
        assert_eq!(key.len(), 25);
    }

    #[test]
    fn test_composite_key_ordering_same_value() {
        let v = Value::Integer(42);
        let u1 = Uuid::from_u128(1);
        let u2 = Uuid::from_u128(2);
        let k1 = encode_composite_key(&v, &u1).unwrap();
        let k2 = encode_composite_key(&v, &u2).unwrap();
        // Same value, different UUIDs — should still have a deterministic order
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_extract_field_flat() {
        let val = Value::Object(BTreeMap::from([
            ("name".into(), Value::String("Alice".into())),
            ("age".into(), Value::Integer(30)),
        ]));
        assert_eq!(
            extract_field(&val, "name"),
            Some(&Value::String("Alice".into()))
        );
        assert_eq!(extract_field(&val, "age"), Some(&Value::Integer(30)));
        assert_eq!(extract_field(&val, "missing"), None);
    }

    #[test]
    fn test_extract_field_nested() {
        let val = Value::Object(BTreeMap::from([(
            "address".into(),
            Value::Object(BTreeMap::from([(
                "city".into(),
                Value::String("Paris".into()),
            )])),
        )]));
        assert_eq!(
            extract_field(&val, "address.city"),
            Some(&Value::String("Paris".into()))
        );
        assert_eq!(extract_field(&val, "address.zip"), None);
    }

    #[test]
    fn test_extract_field_not_object() {
        let val = Value::Integer(42);
        assert_eq!(extract_field(&val, "field"), None);
    }

    #[test]
    fn test_encode_ref() {
        let uuid = Uuid::from_u128(42);
        let encoded = encode_sortable_value(&Value::Ref("users".into(), uuid)).unwrap();
        // tag(1) + "users"(5) + uuid(16) = 22
        assert_eq!(encoded.len(), 22);
        assert_eq!(encoded[0], TAG_REF);
    }

    #[test]
    fn test_encode_ref_ordering() {
        // Ref should sort after Bytes
        let bytes_v = encode_sortable_value(&Value::Bytes(vec![0])).unwrap();
        let ref_v = encode_sortable_value(&Value::Ref("a".into(), Uuid::from_u128(1))).unwrap();
        assert!(bytes_v < ref_v);
    }

    #[test]
    fn test_encode_ref_composite_key() {
        let doc_uuid = Uuid::from_u128(99);
        let ref_val = Value::Ref("orders".into(), Uuid::from_u128(1));
        let key = encode_composite_key(&ref_val, &doc_uuid).unwrap();
        // tag(1) + "orders"(6) + ref_uuid(16) + doc_uuid(16) = 39
        assert_eq!(key.len(), 39);
    }
}
