//! Value type: JSON-like schema-less value representation.

use std::collections::BTreeMap;

use uuid::Uuid;

/// Supported CRDT payload kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrdtKind {
    /// Grow-only counter.
    GCounter,
    /// Positive/negative counter.
    PNCounter,
    /// Last-write-wins set.
    LwwSet,
    /// Observed-remove set.
    OrSet,
    /// Multi-value register.
    Mvr,
}

impl CrdtKind {
    /// Stable on-wire discriminator.
    pub fn to_tag(self) -> u8 {
        match self {
            Self::GCounter => 0,
            Self::PNCounter => 1,
            Self::LwwSet => 2,
            Self::OrSet => 3,
            Self::Mvr => 4,
        }
    }

    /// Parse the stable on-wire discriminator.
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::GCounter),
            1 => Some(Self::PNCounter),
            2 => Some(Self::LwwSet),
            3 => Some(Self::OrSet),
            4 => Some(Self::Mvr),
            _ => None,
        }
    }

    /// Human-readable canonical name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GCounter => "GCounter",
            Self::PNCounter => "PNCounter",
            Self::LwwSet => "LwwSet",
            Self::OrSet => "OrSet",
            Self::Mvr => "Mvr",
        }
    }

    /// Parse canonical names used by API payloads.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "GCounter" => Some(Self::GCounter),
            "PNCounter" => Some(Self::PNCounter),
            "LwwSet" => Some(Self::LwwSet),
            "OrSet" => Some(Self::OrSet),
            "Mvr" => Some(Self::Mvr),
            _ => None,
        }
    }
}

/// A schema-less value, similar to JSON but with additional types (Bytes, Ref).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// JSON null.
    Null,
    /// Boolean value.
    Bool(bool),
    /// 64-bit signed integer.
    Integer(i64),
    /// 64-bit floating point.
    Float(f64),
    /// UTF-8 string.
    String(String),
    /// Raw byte array.
    Bytes(Vec<u8>),
    /// Ordered list of values.
    Array(Vec<Value>),
    /// Key-value map with deterministic ordering.
    Object(BTreeMap<String, Value>),
    /// Reference to a document in another (or the same) collection.
    Ref(std::string::String, Uuid),
    /// Tombstone marker. Only ever produced by [`crate::Database::delete`]
    /// (and the legacy [`crate::GrumpyDb::delete`]).
    ///
    /// Carries the HLC at deletion time and the encoded vector clock so
    /// that concurrent writes (Phase 46+) can be reconciled. The vector
    /// clock is stored as opaque bytes — already produced by
    /// `VectorClock::encode_to` — to keep `value.rs` independent of the
    /// `wal` module.
    Tombstone {
        /// Packed [`crate::wal::hlc::Hlc`] at the moment of deletion.
        deleted_at_hlc: u64,
        /// Serialised vector clock (produced by `VectorClock::encode_to`).
        vector_clock: Vec<u8>,
    },
    /// Opaque CRDT payload. v6+ activates runtime merge semantics.
    Crdt { kind: CrdtKind, payload: Vec<u8> },
}

impl Value {
    /// Returns `true` if the value is `Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Returns the boolean value if this is a `Bool`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Returns the integer value if this is an `Integer`.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(n) => Some(*n),
            _ => None,
        }
    }

    /// Returns the float value if this is a `Float`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Returns a string slice if this is a `String`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// Returns a byte slice if this is `Bytes`.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Returns a slice of values if this is an `Array`.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Returns a reference to the map if this is an `Object`.
    pub fn as_object(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    /// Returns the collection name and UUID if this is a `Ref`.
    pub fn as_ref(&self) -> Option<(&str, &Uuid)> {
        match self {
            Value::Ref(coll, id) => Some((coll, id)),
            _ => None,
        }
    }

    /// Returns `true` if this value is a [`Value::Tombstone`].
    pub fn is_tombstone(&self) -> bool {
        matches!(self, Value::Tombstone { .. })
    }

    /// If this value is a tombstone, returns `(deleted_at_hlc, &vector_clock_bytes)`.
    pub fn as_tombstone(&self) -> Option<(u64, &[u8])> {
        match self {
            Value::Tombstone {
                deleted_at_hlc,
                vector_clock,
            } => Some((*deleted_at_hlc, vector_clock)),
            _ => None,
        }
    }

    /// Returns `true` if this value is a [`Value::Crdt`].
    pub fn is_crdt(&self) -> bool {
        matches!(self, Value::Crdt { .. })
    }

    /// If this value is a CRDT, returns `(kind, payload)`.
    pub fn as_crdt(&self) -> Option<(CrdtKind, &[u8])> {
        match self {
            Value::Crdt { kind, payload } => Some((*kind, payload)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_null() {
        let v = Value::Null;
        assert!(v.is_null());
        assert!(v.as_bool().is_none());
    }

    #[test]
    fn test_value_bool() {
        let v = Value::Bool(true);
        assert!(!v.is_null());
        assert_eq!(v.as_bool(), Some(true));
    }

    #[test]
    fn test_value_integer() {
        let v = Value::Integer(42);
        assert_eq!(v.as_i64(), Some(42));
        assert!(v.as_str().is_none());
    }

    #[test]
    fn test_value_float() {
        let v = Value::Float(std::f64::consts::PI);
        assert_eq!(v.as_f64(), Some(std::f64::consts::PI));
    }

    #[test]
    fn test_value_string() {
        let v = Value::String("hello".into());
        assert_eq!(v.as_str(), Some("hello"));
    }

    #[test]
    fn test_value_bytes() {
        let v = Value::Bytes(vec![1, 2, 3]);
        assert_eq!(v.as_bytes(), Some(&[1, 2, 3][..]));
    }

    #[test]
    fn test_value_array() {
        let v = Value::Array(vec![Value::Integer(1), Value::Integer(2)]);
        assert_eq!(v.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_value_object() {
        let v = Value::Object(BTreeMap::from([(
            "key".into(),
            Value::String("value".into()),
        )]));
        let obj = v.as_object().unwrap();
        assert_eq!(obj.get("key"), Some(&Value::String("value".into())));
    }

    #[test]
    fn test_value_clone_and_eq() {
        let v = Value::Object(BTreeMap::from([(
            "nested".into(),
            Value::Array(vec![Value::Null, Value::Bool(false)]),
        )]));
        let cloned = v.clone();
        assert_eq!(v, cloned);
    }

    #[test]
    fn test_value_ref() {
        let id = Uuid::from_u128(42);
        let v = Value::Ref("users".into(), id);
        assert_eq!(v.as_ref(), Some(("users", &id)));
        assert!(v.as_str().is_none());
        assert!(!v.is_null());
    }

    #[test]
    fn test_value_ref_clone_and_eq() {
        let v = Value::Ref("tasks".into(), Uuid::from_u128(99));
        let cloned = v.clone();
        assert_eq!(v, cloned);
    }

    #[test]
    fn test_value_tombstone_helpers() {
        let v = Value::Tombstone {
            deleted_at_hlc: 0xdeadbeef,
            vector_clock: vec![0u8, 0u8],
        };
        assert!(v.is_tombstone());
        assert!(!Value::Null.is_tombstone());
        let (hlc, vc) = v.as_tombstone().unwrap();
        assert_eq!(hlc, 0xdeadbeef);
        assert_eq!(vc, &[0u8, 0u8]);
        assert!(Value::Null.as_tombstone().is_none());
        assert!(!v.is_null());
    }

    #[test]
    fn test_value_tombstone_clone_and_eq() {
        let v = Value::Tombstone {
            deleted_at_hlc: 42,
            vector_clock: vec![1, 2, 3, 4],
        };
        let cloned = v.clone();
        assert_eq!(v, cloned);
    }

    #[test]
    fn test_value_crdt_helpers() {
        let v = Value::Crdt {
            kind: CrdtKind::GCounter,
            payload: vec![1, 2, 3],
        };
        assert!(v.is_crdt());
        assert!(!Value::Null.is_crdt());
        let (kind, payload) = v.as_crdt().unwrap();
        assert_eq!(kind, CrdtKind::GCounter);
        assert_eq!(payload, &[1, 2, 3]);
        assert!(Value::Null.as_crdt().is_none());
    }

    #[test]
    fn test_crdt_kind_tag_and_name_round_trip() {
        let all = [
            CrdtKind::GCounter,
            CrdtKind::PNCounter,
            CrdtKind::LwwSet,
            CrdtKind::OrSet,
            CrdtKind::Mvr,
        ];

        for kind in all {
            let tag = kind.to_tag();
            assert_eq!(CrdtKind::from_tag(tag), Some(kind));
            assert_eq!(CrdtKind::from_name(kind.as_str()), Some(kind));
        }

        assert!(CrdtKind::from_tag(42).is_none());
        assert!(CrdtKind::from_name("unknown").is_none());
    }
}
