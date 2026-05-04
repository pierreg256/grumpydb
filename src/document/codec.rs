//! Binary codec for Value serialization/deserialization.
//!
//! Each value is prefixed by a 1-byte type tag. Nested structures are
//! encoded recursively. All integers use little-endian byte order.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::document::value::{CrdtKind, Value};
use crate::error::{GrumpyError, Result};

// ── Type tags ───────────────────────────────────────────────────────────

const TAG_NULL: u8 = 0x00;
const TAG_BOOL: u8 = 0x01;
const TAG_INTEGER: u8 = 0x02;
const TAG_FLOAT: u8 = 0x03;
const TAG_STRING: u8 = 0x04;
const TAG_BYTES: u8 = 0x05;
const TAG_ARRAY: u8 = 0x06;
const TAG_OBJECT: u8 = 0x07;
const TAG_REF: u8 = 0x08;
/// Tombstone marker (Phase 40d). Format-locking — DO NOT REUSE in v6+.
const TAG_TOMBSTONE: u8 = 0x09;
/// CRDT payload marker (Phase 46).
const TAG_CRDT: u8 = 0x0a;

/// Maximum collection name length in a Ref (from naming rules).
const MAX_REF_NAME_LEN: u32 = 64;

/// Maximum encoded vector-clock length stored inside a tombstone payload.
/// Defensive cap (matches `MAX_VCLOCK_ENTRIES * 24 + 2` ≈ 100 KiB) so
/// malformed input cannot cause unbounded allocations.
const MAX_TOMBSTONE_VCLOCK_LEN: u32 = 128 * 1024;
/// Maximum encoded CRDT payload length.
const MAX_CRDT_PAYLOAD_LEN: u32 = 16 * 1024 * 1024;

// ── Safety limits ───────────────────────────────────────────────────────

/// Maximum nesting depth for decode (prevents stack overflow).
const MAX_NESTING_DEPTH: usize = 64;
/// Maximum string/bytes length (16 MiB).
const MAX_BLOB_LEN: u32 = 16 * 1024 * 1024;
/// Maximum array element count.
const MAX_ARRAY_LEN: u32 = 1_000_000;
/// Maximum object key count.
const MAX_OBJECT_KEYS: u32 = 100_000;

// ── Encode ──────────────────────────────────────────────────────────────

/// Encodes a `Value` into a byte buffer.
pub fn encode(value: &Value, buf: &mut Vec<u8>) {
    match value {
        Value::Null => buf.push(TAG_NULL),
        Value::Bool(b) => {
            buf.push(TAG_BOOL);
            buf.push(u8::from(*b));
        }
        Value::Integer(n) => {
            buf.push(TAG_INTEGER);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::Float(f) => {
            buf.push(TAG_FLOAT);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::String(s) => {
            buf.push(TAG_STRING);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            buf.push(TAG_BYTES);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Array(arr) => {
            buf.push(TAG_ARRAY);
            buf.extend_from_slice(&(arr.len() as u32).to_le_bytes());
            for item in arr {
                encode(item, buf);
            }
        }
        Value::Object(map) => {
            buf.push(TAG_OBJECT);
            buf.extend_from_slice(&(map.len() as u32).to_le_bytes());
            for (key, val) in map {
                buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
                buf.extend_from_slice(key.as_bytes());
                encode(val, buf);
            }
        }
        Value::Ref(collection, uuid) => {
            buf.push(TAG_REF);
            buf.extend_from_slice(&(collection.len() as u32).to_le_bytes());
            buf.extend_from_slice(collection.as_bytes());
            buf.extend_from_slice(uuid.as_bytes());
        }
        Value::Tombstone {
            deleted_at_hlc,
            vector_clock,
        } => {
            buf.push(TAG_TOMBSTONE);
            buf.extend_from_slice(&deleted_at_hlc.to_le_bytes());
            buf.extend_from_slice(&(vector_clock.len() as u32).to_le_bytes());
            buf.extend_from_slice(vector_clock);
        }
        Value::Crdt { kind, payload } => {
            buf.push(TAG_CRDT);
            buf.push(kind.to_tag());
            buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            buf.extend_from_slice(payload);
        }
    }
}

/// Convenience: encode a value into a new Vec.
pub fn encode_to_vec(value: &Value) -> Vec<u8> {
    let mut buf = Vec::with_capacity(encoded_size(value));
    encode(value, &mut buf);
    buf
}

// ── Encoded size ────────────────────────────────────────────────────────

/// Computes the encoded byte size of a value without allocating.
pub fn encoded_size(value: &Value) -> usize {
    match value {
        Value::Null => 1,
        Value::Bool(_) => 2,
        Value::Integer(_) | Value::Float(_) => 9,
        Value::String(s) => 1 + 4 + s.len(),
        Value::Bytes(b) => 1 + 4 + b.len(),
        Value::Array(arr) => 1 + 4 + arr.iter().map(encoded_size).sum::<usize>(),
        Value::Object(map) => {
            1 + 4
                + map
                    .iter()
                    .map(|(k, v)| 4 + k.len() + encoded_size(v))
                    .sum::<usize>()
        }
        Value::Ref(collection, _) => 1 + 4 + collection.len() + 16,
        Value::Tombstone { vector_clock, .. } => 1 + 8 + 4 + vector_clock.len(),
        Value::Crdt { payload, .. } => 1 + 1 + 4 + payload.len(),
    }
}

// ── Decode ──────────────────────────────────────────────────────────────

/// Decodes a `Value` from a byte slice.
pub fn decode(data: &[u8]) -> Result<Value> {
    let mut cursor = data;
    let value = decode_recursive(&mut cursor, 0)?;
    Ok(value)
}

/// Decodes from a cursor, returning the value and advancing the cursor.
/// Also used by Document::decode which needs to track position.
pub fn decode_from_cursor(cursor: &mut &[u8]) -> Result<Value> {
    decode_recursive(cursor, 0)
}

fn decode_recursive(cursor: &mut &[u8], depth: usize) -> Result<Value> {
    if depth > MAX_NESTING_DEPTH {
        return Err(GrumpyError::Codec(format!(
            "nesting depth exceeds maximum ({MAX_NESTING_DEPTH})"
        )));
    }

    let tag = read_u8(cursor)?;
    match tag {
        TAG_NULL => Ok(Value::Null),
        TAG_BOOL => {
            let b = read_u8(cursor)?;
            Ok(Value::Bool(b != 0))
        }
        TAG_INTEGER => {
            let n = read_i64_le(cursor)?;
            Ok(Value::Integer(n))
        }
        TAG_FLOAT => {
            let f = read_f64_le(cursor)?;
            Ok(Value::Float(f))
        }
        TAG_STRING => {
            let len = read_u32_le(cursor)?;
            if len > MAX_BLOB_LEN {
                return Err(GrumpyError::Codec(format!(
                    "string length {len} exceeds maximum ({MAX_BLOB_LEN})"
                )));
            }
            let s = read_string(cursor, len as usize)?;
            Ok(Value::String(s))
        }
        TAG_BYTES => {
            let len = read_u32_le(cursor)?;
            if len > MAX_BLOB_LEN {
                return Err(GrumpyError::Codec(format!(
                    "bytes length {len} exceeds maximum ({MAX_BLOB_LEN})"
                )));
            }
            let b = read_bytes(cursor, len as usize)?;
            Ok(Value::Bytes(b))
        }
        TAG_ARRAY => {
            let count = read_u32_le(cursor)?;
            if count > MAX_ARRAY_LEN {
                return Err(GrumpyError::Codec(format!(
                    "array length {count} exceeds maximum ({MAX_ARRAY_LEN})"
                )));
            }
            let mut arr = Vec::with_capacity(count as usize);
            for _ in 0..count {
                arr.push(decode_recursive(cursor, depth + 1)?);
            }
            Ok(Value::Array(arr))
        }
        TAG_OBJECT => {
            let count = read_u32_le(cursor)?;
            if count > MAX_OBJECT_KEYS {
                return Err(GrumpyError::Codec(format!(
                    "object key count {count} exceeds maximum ({MAX_OBJECT_KEYS})"
                )));
            }
            let mut map = BTreeMap::new();
            for _ in 0..count {
                let key_len = read_u32_le(cursor)?;
                if key_len > MAX_BLOB_LEN {
                    return Err(GrumpyError::Codec(format!(
                        "object key length {key_len} exceeds maximum ({MAX_BLOB_LEN})"
                    )));
                }
                let key = read_string(cursor, key_len as usize)?;
                let val = decode_recursive(cursor, depth + 1)?;
                map.insert(key, val);
            }
            Ok(Value::Object(map))
        }
        TAG_REF => {
            let name_len = read_u32_le(cursor)?;
            if name_len > MAX_REF_NAME_LEN {
                return Err(GrumpyError::Codec(format!(
                    "ref collection name length {name_len} exceeds maximum ({MAX_REF_NAME_LEN})"
                )));
            }
            let collection = read_string(cursor, name_len as usize)?;
            let uuid_bytes = read_bytes(cursor, 16)?;
            let uuid = {
                let mut arr = [0u8; 16];
                arr.copy_from_slice(&uuid_bytes);
                Uuid::from_bytes(arr)
            };
            Ok(Value::Ref(collection, uuid))
        }
        TAG_TOMBSTONE => {
            // 8 bytes packed HLC + length-prefixed encoded vector clock.
            let mut hlc_bytes = [0u8; 8];
            if cursor.len() < 8 {
                return Err(GrumpyError::Codec(
                    "unexpected end of data reading tombstone HLC".into(),
                ));
            }
            hlc_bytes.copy_from_slice(&cursor[..8]);
            let deleted_at_hlc = u64::from_le_bytes(hlc_bytes);
            *cursor = &cursor[8..];

            let vc_len = read_u32_le(cursor)?;
            if vc_len > MAX_TOMBSTONE_VCLOCK_LEN {
                return Err(GrumpyError::Codec(format!(
                    "tombstone vector clock length {vc_len} exceeds maximum \
                     ({MAX_TOMBSTONE_VCLOCK_LEN})"
                )));
            }
            let vector_clock = read_bytes(cursor, vc_len as usize)?;
            Ok(Value::Tombstone {
                deleted_at_hlc,
                vector_clock,
            })
        }
        TAG_CRDT => {
            let kind_tag = read_u8(cursor)?;
            let kind = CrdtKind::from_tag(kind_tag)
                .ok_or_else(|| GrumpyError::Codec(format!("unknown CRDT kind tag: {kind_tag}")))?;
            let payload_len = read_u32_le(cursor)?;
            if payload_len > MAX_CRDT_PAYLOAD_LEN {
                return Err(GrumpyError::Codec(format!(
                    "CRDT payload length {payload_len} exceeds maximum ({MAX_CRDT_PAYLOAD_LEN})"
                )));
            }
            let payload = read_bytes(cursor, payload_len as usize)?;
            Ok(Value::Crdt { kind, payload })
        }
        _ => Err(GrumpyError::Codec(format!("unknown type tag: 0x{tag:02x}"))),
    }
}

// ── Cursor helpers ──────────────────────────────────────────────────────

fn read_u8(cursor: &mut &[u8]) -> Result<u8> {
    if cursor.is_empty() {
        return Err(GrumpyError::Codec("unexpected end of data".into()));
    }
    let val = cursor[0];
    *cursor = &cursor[1..];
    Ok(val)
}

fn read_u32_le(cursor: &mut &[u8]) -> Result<u32> {
    if cursor.len() < 4 {
        return Err(GrumpyError::Codec(
            "unexpected end of data reading u32".into(),
        ));
    }
    let val = u32::from_le_bytes([cursor[0], cursor[1], cursor[2], cursor[3]]);
    *cursor = &cursor[4..];
    Ok(val)
}

fn read_i64_le(cursor: &mut &[u8]) -> Result<i64> {
    if cursor.len() < 8 {
        return Err(GrumpyError::Codec(
            "unexpected end of data reading i64".into(),
        ));
    }
    let val = i64::from_le_bytes([
        cursor[0], cursor[1], cursor[2], cursor[3], cursor[4], cursor[5], cursor[6], cursor[7],
    ]);
    *cursor = &cursor[8..];
    Ok(val)
}

fn read_f64_le(cursor: &mut &[u8]) -> Result<f64> {
    if cursor.len() < 8 {
        return Err(GrumpyError::Codec(
            "unexpected end of data reading f64".into(),
        ));
    }
    let val = f64::from_le_bytes([
        cursor[0], cursor[1], cursor[2], cursor[3], cursor[4], cursor[5], cursor[6], cursor[7],
    ]);
    *cursor = &cursor[8..];
    Ok(val)
}

fn read_bytes(cursor: &mut &[u8], len: usize) -> Result<Vec<u8>> {
    if cursor.len() < len {
        return Err(GrumpyError::Codec(format!(
            "unexpected end of data: need {len} bytes, have {}",
            cursor.len()
        )));
    }
    let val = cursor[..len].to_vec();
    *cursor = &cursor[len..];
    Ok(val)
}

fn read_string(cursor: &mut &[u8], len: usize) -> Result<String> {
    let bytes = read_bytes(cursor, len)?;
    String::from_utf8(bytes).map_err(|e| GrumpyError::Codec(format!("invalid UTF-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(value: &Value) {
        let encoded = encode_to_vec(value);
        assert_eq!(
            encoded.len(),
            encoded_size(value),
            "encoded_size mismatch for {value:?}"
        );
        let decoded = decode(&encoded).unwrap();
        assert_eq!(*value, decoded);
    }

    #[test]
    fn test_null_round_trip() {
        round_trip(&Value::Null);
    }

    #[test]
    fn test_bool_round_trip() {
        round_trip(&Value::Bool(true));
        round_trip(&Value::Bool(false));
    }

    #[test]
    fn test_integer_round_trip() {
        round_trip(&Value::Integer(0));
        round_trip(&Value::Integer(42));
        round_trip(&Value::Integer(-1));
        round_trip(&Value::Integer(i64::MAX));
        round_trip(&Value::Integer(i64::MIN));
    }

    #[test]
    fn test_float_round_trip() {
        round_trip(&Value::Float(0.0));
        round_trip(&Value::Float(std::f64::consts::PI));
        round_trip(&Value::Float(-1.0e100));
        round_trip(&Value::Float(f64::INFINITY));
        round_trip(&Value::Float(f64::NEG_INFINITY));
    }

    #[test]
    fn test_string_round_trip() {
        round_trip(&Value::String(String::new()));
        round_trip(&Value::String("hello".into()));
        round_trip(&Value::String("émoji: 🦀".into()));
        round_trip(&Value::String("a".repeat(10_000)));
    }

    #[test]
    fn test_bytes_round_trip() {
        round_trip(&Value::Bytes(vec![]));
        round_trip(&Value::Bytes(vec![0, 1, 2, 255]));
        round_trip(&Value::Bytes(vec![0xAB; 5000]));
    }

    #[test]
    fn test_array_round_trip() {
        round_trip(&Value::Array(vec![]));
        round_trip(&Value::Array(vec![
            Value::Integer(1),
            Value::String("two".into()),
            Value::Null,
        ]));
    }

    #[test]
    fn test_object_round_trip() {
        round_trip(&Value::Object(BTreeMap::new()));
        round_trip(&Value::Object(BTreeMap::from([
            ("name".into(), Value::String("grumpy".into())),
            ("version".into(), Value::Integer(1)),
        ])));
    }

    #[test]
    fn test_nested_complex_document() {
        let value = Value::Object(BTreeMap::from([
            ("name".into(), Value::String("GrumpyDB".into())),
            ("version".into(), Value::Integer(1)),
            ("active".into(), Value::Bool(true)),
            ("score".into(), Value::Float(99.5)),
            ("data".into(), Value::Bytes(vec![0xDE, 0xAD])),
            (
                "tags".into(),
                Value::Array(vec![
                    Value::String("db".into()),
                    Value::String("rust".into()),
                    Value::Null,
                ]),
            ),
            (
                "metadata".into(),
                Value::Object(BTreeMap::from([
                    ("created".into(), Value::Integer(1234567890)),
                    (
                        "nested".into(),
                        Value::Object(BTreeMap::from([("deep".into(), Value::Bool(true))])),
                    ),
                ])),
            ),
        ]));
        round_trip(&value);
    }

    #[test]
    fn test_encoded_size_matches() {
        let values = vec![
            Value::Null,
            Value::Bool(true),
            Value::Integer(42),
            Value::Float(std::f64::consts::PI),
            Value::String("test".into()),
            Value::Bytes(vec![1, 2, 3]),
            Value::Array(vec![Value::Integer(1), Value::Integer(2)]),
            Value::Object(BTreeMap::from([("k".into(), Value::Null)])),
        ];
        for v in &values {
            let encoded = encode_to_vec(v);
            assert_eq!(encoded.len(), encoded_size(v), "mismatch for {v:?}");
        }
    }

    #[test]
    fn test_decode_unknown_tag() {
        let data = [0xFF];
        let result = decode(&data);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unknown type tag"));
    }

    #[test]
    fn test_decode_truncated_string() {
        // Encode a string, then truncate
        let encoded = encode_to_vec(&Value::String("hello".into()));
        let truncated = &encoded[..3]; // tag + partial len
        assert!(decode(truncated).is_err());
    }

    #[test]
    fn test_decode_truncated_integer() {
        let encoded = encode_to_vec(&Value::Integer(42));
        let truncated = &encoded[..5]; // tag + 4 bytes instead of 8
        assert!(decode(truncated).is_err());
    }

    #[test]
    fn test_decode_empty_data() {
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn test_decode_invalid_utf8() {
        // Manually craft a String tag with invalid UTF-8
        let mut data = vec![TAG_STRING];
        data.extend_from_slice(&3u32.to_le_bytes());
        data.extend_from_slice(&[0xFF, 0xFE, 0xFD]); // invalid UTF-8
        let result = decode(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("UTF-8"));
    }

    #[test]
    fn test_nesting_depth_limit() {
        // Build a deeply nested array: [[[[...]]]]
        let mut value = Value::Null;
        for _ in 0..MAX_NESTING_DEPTH + 5 {
            value = Value::Array(vec![value]);
        }
        let encoded = encode_to_vec(&value);
        let result = decode(&encoded);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nesting depth"));
    }

    #[test]
    fn test_nesting_at_max_depth_ok() {
        // Exactly at MAX_NESTING_DEPTH should work
        let mut value = Value::Null;
        for _ in 0..MAX_NESTING_DEPTH {
            value = Value::Array(vec![value]);
        }
        let encoded = encode_to_vec(&value);
        assert!(decode(&encoded).is_ok());
    }

    #[test]
    fn test_empty_containers() {
        round_trip(&Value::String(String::new()));
        round_trip(&Value::Bytes(vec![]));
        round_trip(&Value::Array(vec![]));
        round_trip(&Value::Object(BTreeMap::new()));
    }

    #[test]
    fn test_float_nan() {
        // NaN encodes/decodes but NaN != NaN, so we check manually
        let encoded = encode_to_vec(&Value::Float(f64::NAN));
        let decoded = decode(&encoded).unwrap();
        match decoded {
            Value::Float(f) => assert!(f.is_nan()),
            _ => panic!("expected Float"),
        }
    }

    #[test]
    fn test_ref_round_trip() {
        let uuid = Uuid::from_u128(12345);
        round_trip(&Value::Ref("users".into(), uuid));
    }

    #[test]
    fn test_ref_in_object_round_trip() {
        let uuid = Uuid::from_u128(42);
        let value = Value::Object(BTreeMap::from([
            ("name".into(), Value::String("Order #1".into())),
            ("owner".into(), Value::Ref("users".into(), uuid)),
        ]));
        round_trip(&value);
    }

    #[test]
    fn test_ref_in_array_round_trip() {
        let value = Value::Array(vec![
            Value::Ref("a".into(), Uuid::from_u128(1)),
            Value::Ref("b".into(), Uuid::from_u128(2)),
        ]);
        round_trip(&value);
    }

    #[test]
    fn test_ref_encoded_size() {
        let v = Value::Ref("users".into(), Uuid::from_u128(1));
        let encoded = encode_to_vec(&v);
        assert_eq!(encoded.len(), encoded_size(&v));
        // tag(1) + name_len(4) + "users"(5) + uuid(16) = 26
        assert_eq!(encoded.len(), 26);
    }

    #[test]
    fn test_tombstone_codec_round_trip() {
        // Empty vclock.
        let v = Value::Tombstone {
            deleted_at_hlc: 0,
            vector_clock: vec![],
        };
        round_trip(&v);
        // Realistic vclock: u16 num_entries=1 + (u128 + u64) = 26 bytes.
        let mut vc_bytes = Vec::new();
        vc_bytes.extend_from_slice(&1u16.to_le_bytes());
        vc_bytes.extend_from_slice(&12345u128.to_le_bytes());
        vc_bytes.extend_from_slice(&7u64.to_le_bytes());
        let v2 = Value::Tombstone {
            deleted_at_hlc: 0xdeadbeef_cafebabe,
            vector_clock: vc_bytes.clone(),
        };
        round_trip(&v2);

        // Sanity: tag(1) + hlc(8) + vc_len(4) + vc(26) = 39 bytes.
        let encoded = encode_to_vec(&v2);
        assert_eq!(encoded.len(), 1 + 8 + 4 + vc_bytes.len());
        assert_eq!(encoded[0], TAG_TOMBSTONE);
    }

    #[test]
    fn test_tombstone_decode_truncated_hlc() {
        // Tag + only 4 bytes of the 8-byte HLC.
        let mut data = vec![TAG_TOMBSTONE];
        data.extend_from_slice(&[0u8; 4]);
        let err = decode(&data).unwrap_err();
        assert!(err.to_string().contains("tombstone HLC"));
    }

    #[test]
    fn test_tombstone_decode_oversize_vclock_rejected() {
        let mut data = vec![TAG_TOMBSTONE];
        data.extend_from_slice(&0u64.to_le_bytes());
        // Claim a vclock far above the cap.
        data.extend_from_slice(&(MAX_TOMBSTONE_VCLOCK_LEN + 1).to_le_bytes());
        let err = decode(&data).unwrap_err();
        assert!(err.to_string().contains("vector clock length"));
    }

    #[test]
    fn test_crdt_codec_round_trip() {
        let v = Value::Crdt {
            kind: CrdtKind::PNCounter,
            payload: vec![0xaa, 0xbb, 0xcc],
        };
        round_trip(&v);
        let enc = encode_to_vec(&v);
        assert_eq!(enc[0], TAG_CRDT);
    }

    #[test]
    fn test_crdt_codec_rejects_unknown_kind_tag() {
        let bytes = [TAG_CRDT, 0xff, 0, 0, 0, 0];
        let err = decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("unknown CRDT kind tag"));
    }
}
