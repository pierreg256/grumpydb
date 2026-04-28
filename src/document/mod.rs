//! Document model: schema-less JSON-like values with binary codec.
//!
//! A document is a `(UUID, Value)` pair. The [`Value`] type supports
//! null, bool, integer, float, string, bytes, arrays, and objects.

pub mod codec;
pub mod value;

use uuid::Uuid;

use crate::error::{GrumpyError, Result};

use self::codec::{decode_from_cursor, encode, encoded_size};
use self::value::Value;

/// A document stored in GrumpyDB: a UUID key paired with a schema-less value.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub key: Uuid,
    pub value: Value,
}

impl Document {
    /// Creates a new document.
    pub fn new(key: Uuid, value: Value) -> Self {
        Self { key, value }
    }

    /// Encodes the document into bytes: 16-byte UUID + encoded value.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16 + encoded_size(&self.value));
        buf.extend_from_slice(self.key.as_bytes());
        encode(&self.value, &mut buf);
        buf
    }

    /// Decodes a document from bytes.
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < 16 {
            return Err(GrumpyError::Codec("document too short for UUID".into()));
        }
        let key = {
            let mut arr = [0u8; 16];
            arr.copy_from_slice(&data[..16]);
            Uuid::from_bytes(arr)
        };
        let mut cursor = &data[16..];
        let value = decode_from_cursor(&mut cursor)?;
        Ok(Self { key, value })
    }

    /// Returns the encoded byte size of this document (16 + value size).
    pub fn encoded_size(&self) -> usize {
        16 + encoded_size(&self.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn test_document_round_trip_simple() {
        let doc = Document::new(Uuid::new_v4(), Value::Integer(42));
        let encoded = doc.encode();
        let decoded = Document::decode(&encoded).unwrap();
        assert_eq!(doc, decoded);
    }

    #[test]
    fn test_document_round_trip_complex() {
        let doc = Document::new(
            Uuid::new_v4(),
            Value::Object(BTreeMap::from([
                ("name".into(), Value::String("test".into())),
                (
                    "tags".into(),
                    Value::Array(vec![Value::Integer(1), Value::Null]),
                ),
            ])),
        );
        let encoded = doc.encode();
        let decoded = Document::decode(&encoded).unwrap();
        assert_eq!(doc, decoded);
    }

    #[test]
    fn test_document_round_trip_null() {
        let doc = Document::new(Uuid::new_v4(), Value::Null);
        let encoded = doc.encode();
        let decoded = Document::decode(&encoded).unwrap();
        assert_eq!(doc, decoded);
    }

    #[test]
    fn test_document_encoded_size() {
        let doc = Document::new(Uuid::new_v4(), Value::String("hello".into()));
        assert_eq!(doc.encode().len(), doc.encoded_size());
    }

    #[test]
    fn test_document_decode_too_short() {
        let data = [0u8; 10]; // less than 16 bytes
        assert!(Document::decode(&data).is_err());
    }

    #[test]
    fn test_document_preserves_uuid() {
        let key = Uuid::from_u128(0xDEADBEEF);
        let doc = Document::new(key, Value::Bool(true));
        let decoded = Document::decode(&doc.encode()).unwrap();
        assert_eq!(decoded.key, key);
    }
}
