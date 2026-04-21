# Skill: Document Model

## When to use this skill

When working on:
- `src/document/mod.rs` — Document type, re-exports
- `src/document/value.rs` — enum Value (JSON-like)
- `src/document/codec.rs` — binary encoding/decoding

## Core principles

### Value type

```rust
use std::collections::BTreeMap;

/// Represents a schema-less JSON-like value.
/// BTreeMap for objects guarantees deterministic key ordering.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}
```

### Why BTreeMap and not HashMap?

- **Deterministic ordering**: same document → same serialization → same checksum
- **Ordered iteration** for debugging and tests
- Acceptable performance for typical document sizes

### Idiomatic accessors

```rust
impl Value {
    pub fn is_null(&self) -> bool;
    pub fn as_bool(&self) -> Option<bool>;
    pub fn as_i64(&self) -> Option<i64>;
    pub fn as_f64(&self) -> Option<f64>;
    pub fn as_str(&self) -> Option<&str>;
    pub fn as_bytes(&self) -> Option<&[u8]>;
    pub fn as_array(&self) -> Option<&[Value]>;
    pub fn as_object(&self) -> Option<&BTreeMap<String, Value>>;
}
```

### Binary codec — Format

Each value is prefixed by a one-byte **type tag**:

| Tag | Type | Payload |
|-----|------|---------|
| `0x00` | Null | none |
| `0x01` | Bool | 1 byte: `0x00` (false) or `0x01` (true) |
| `0x02` | Integer | 8 bytes i64 little-endian |
| `0x03` | Float | 8 bytes f64 little-endian |
| `0x04` | String | 4 bytes len (u32 LE) + UTF-8 bytes |
| `0x05` | Bytes | 4 bytes len (u32 LE) + raw bytes |
| `0x06` | Array | 4 bytes count (u32 LE) + recursively encoded elements |
| `0x07` | Object | 4 bytes count (u32 LE) + pairs (String key + encoded value) |

### Encoding

```rust
pub fn encode(value: &Value, buf: &mut Vec<u8>) {
    match value {
        Value::Null => buf.push(0x00),
        Value::Bool(b) => {
            buf.push(0x01);
            buf.push(if *b { 1 } else { 0 });
        }
        Value::Integer(n) => {
            buf.push(0x02);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::Float(f) => {
            buf.push(0x03);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::String(s) => {
            buf.push(0x04);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            buf.push(0x05);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Array(arr) => {
            buf.push(0x06);
            buf.extend_from_slice(&(arr.len() as u32).to_le_bytes());
            for item in arr {
                encode(item, buf);
            }
        }
        Value::Object(map) => {
            buf.push(0x07);
            buf.extend_from_slice(&(map.len() as u32).to_le_bytes());
            for (key, val) in map {
                // key encoded as a String (without tag, we know it's always a String)
                buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
                buf.extend_from_slice(key.as_bytes());
                encode(val, buf);
            }
        }
    }
}
```

### Decoding

```rust
pub fn decode(cursor: &mut &[u8]) -> Result<Value> {
    let tag = read_u8(cursor)?;
    match tag {
        0x00 => Ok(Value::Null),
        0x01 => {
            let b = read_u8(cursor)?;
            Ok(Value::Bool(b != 0))
        }
        0x02 => {
            let n = read_i64_le(cursor)?;
            Ok(Value::Integer(n))
        }
        // ... similar pattern for other types
        0x07 => {
            let count = read_u32_le(cursor)? as usize;
            let mut map = BTreeMap::new();
            for _ in 0..count {
                let key_len = read_u32_le(cursor)? as usize;
                let key = read_string(cursor, key_len)?;
                let val = decode(cursor)?;
                map.insert(key, val);
            }
            Ok(Value::Object(map))
        }
        _ => Err(GrumpyError::Codec(format!("unknown type tag: 0x{:02x}", tag)))
    }
}
```

### Encoded size (without allocation)

```rust
pub fn encoded_size(value: &Value) -> usize {
    match value {
        Value::Null => 1,
        Value::Bool(_) => 2,
        Value::Integer(_) => 9,   // 1 tag + 8 data
        Value::Float(_) => 9,
        Value::String(s) => 1 + 4 + s.len(),
        Value::Bytes(b) => 1 + 4 + b.len(),
        Value::Array(arr) => 1 + 4 + arr.iter().map(encoded_size).sum::<usize>(),
        Value::Object(map) => {
            1 + 4 + map.iter()
                .map(|(k, v)| 4 + k.len() + encoded_size(v))
                .sum::<usize>()
        }
    }
}
```

### Security limits

```rust
const MAX_NESTING_DEPTH: usize = 64;    // max recursion depth
const MAX_STRING_LEN: u32 = 16 * 1024 * 1024;  // 16 MiB per string
const MAX_ARRAY_LEN: u32 = 1_000_000;   // 1M elements per array
const MAX_OBJECT_KEYS: u32 = 100_000;   // 100K keys per object
```

Check these limits in `decode()` to prevent DoS via malicious documents.

### Document

```rust
pub struct Document {
    pub key: Uuid,
    pub value: Value,
}

impl Document {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(self.key.as_bytes()); // 16 bytes
        codec::encode(&self.value, &mut buf);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < 16 {
            return Err(GrumpyError::Codec("document too short".into()));
        }
        let key = Uuid::from_bytes(data[..16].try_into().unwrap());
        let mut cursor = &data[16..];
        let value = codec::decode(&mut cursor)?;
        Ok(Document { key, value })
    }
}
```

## Mandatory test patterns

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_null_round_trip() {
        let v = Value::Null;
        let encoded = encode_to_vec(&v);
        assert_eq!(decode_from_slice(&encoded).unwrap(), v);
    }

    // One test per Value type...

    #[test]
    fn test_nested_object_round_trip() {
        let v = Value::Object(BTreeMap::from([
            ("name".into(), Value::String("grumpy".into())),
            ("version".into(), Value::Integer(1)),
            ("tags".into(), Value::Array(vec![
                Value::String("db".into()),
                Value::String("rust".into()),
            ])),
            ("metadata".into(), Value::Object(BTreeMap::from([
                ("created".into(), Value::Integer(1234567890)),
                ("active".into(), Value::Bool(true)),
            ]))),
        ]));
        let encoded = encode_to_vec(&v);
        assert_eq!(decode_from_slice(&encoded).unwrap(), v);
    }

    #[test]
    fn test_encoded_size_matches_actual() {
        // For each type, verify that encoded_size() == encode().len()
    }

    #[test]
    fn test_decode_invalid_tag() {
        let data = [0xFF];
        assert!(decode_from_slice(&data).is_err());
    }

    #[test]
    fn test_decode_truncated_data() {
        // Encode a String, truncate the data → clean error
    }

    #[test]
    fn test_nesting_depth_limit() {
        // Create an object nested to MAX_DEPTH+1 → error
    }

    #[test]
    fn test_empty_containers() {
        // Empty Array, empty Object, empty String
    }

    #[test]
    fn test_document_round_trip() {
        let doc = Document { key: Uuid::new_v4(), value: Value::Integer(42) };
        let encoded = doc.encode();
        let decoded = Document::decode(&encoded).unwrap();
        assert_eq!(doc.key, decoded.key);
        assert_eq!(doc.value, decoded.value);
    }
}
```

## Common mistakes to avoid

1. **Float NaN**: `NaN != NaN` in IEEE 754. Decide whether to accept them or not. If yes, PartialEq must be adjusted.
2. **String UTF-8**: always validate that decoded bytes are valid UTF-8 (`String::from_utf8`)
3. **u32 overflow**: a string longer than 4 GiB doesn't fit in the format. Check before encoding.
4. **Recursion depth**: pass a `depth` counter in `decode()` to prevent stack overflow
5. **Determinism**: BTreeMap guarantees ordering, but be careful if switching to HashMap
