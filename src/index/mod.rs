//! Secondary index: maps field values to document UUIDs via a `BTree<Vec<u8>>`.
//!
//! A secondary index stores composite keys `(encoded_field_value, uuid)` in a
//! variable-key B+Tree. This enables fast exact-match lookups and range queries
//! on document fields.

pub mod encoding;

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::btree::BTree;
use crate::document::value::Value;
use crate::error::Result;

use self::encoding::{encode_composite_key, encode_sortable_value, extract_field};

/// Maximum key size for secondary index B+Trees.
/// tag(1) + max_value(128) + uuid(16) = 145, rounded up.
const INDEX_MAX_KEY_SIZE: u16 = 160;

/// Definition of a secondary index.
#[derive(Debug, Clone)]
pub struct IndexDefinition {
    /// Index name (e.g., "by_email").
    pub name: String,
    /// Dot-separated field path (e.g., "email" or "address.city").
    pub field_path: String,
}

/// A secondary index backed by a `BTree<Vec<u8>>`.
pub struct SecondaryIndex {
    /// Index definition.
    pub def: IndexDefinition,
    /// The underlying variable-key B+Tree.
    btree: BTree<Vec<u8>>,
    /// Path to the .idx file.
    path: PathBuf,
}

impl SecondaryIndex {
    /// Creates a new secondary index.
    pub fn create(dir: &Path, def: IndexDefinition) -> Result<Self> {
        let idx_path = dir.join(format!("idx_{}.idx", def.name));
        let btree = BTree::<Vec<u8>>::create_with(&idx_path, INDEX_MAX_KEY_SIZE)?;
        Ok(Self {
            def,
            btree,
            path: idx_path,
        })
    }

    /// Opens an existing secondary index.
    pub fn open(dir: &Path, def: IndexDefinition) -> Result<Self> {
        let idx_path = dir.join(format!("idx_{}.idx", def.name));
        let btree = BTree::<Vec<u8>>::open(&idx_path)?;
        Ok(Self {
            def,
            btree,
            path: idx_path,
        })
    }

    /// Indexes a document: extracts the field and inserts the composite key.
    pub fn index_document(&mut self, uuid: &Uuid, doc: &Value) -> Result<()> {
        if let Some(field_val) = extract_field(doc, &self.def.field_path) {
            let key = encode_composite_key(field_val, uuid)?;
            // slot_id=0 — secondary indexes don't point to slots, just existence
            self.btree.insert(key, 0, 0)?;
        }
        // Missing field → not indexed (not an error)
        Ok(())
    }

    /// Removes a document from the index.
    pub fn unindex_document(&mut self, uuid: &Uuid, doc: &Value) -> Result<()> {
        if let Some(field_val) = extract_field(doc, &self.def.field_path) {
            let key = encode_composite_key(field_val, uuid)?;
            // Ignore errors (key might not exist if field was missing when inserted)
            let _ = self.btree.delete(&key);
        }
        Ok(())
    }

    /// Looks up all document UUIDs with an exact field value match.
    pub fn lookup(&mut self, value: &Value) -> Result<Vec<Uuid>> {
        let prefix = encode_sortable_value(value)?;
        // Range scan: [prefix + UUID::MIN, prefix + UUID::MAX)
        let mut start_key = prefix.clone();
        start_key.extend_from_slice(Uuid::nil().as_bytes());

        // End key: prefix + 1 (next value boundary)
        let mut end_key = prefix;
        // Increment the last byte to form the exclusive upper bound
        increment_bytes(&mut end_key);

        let entries = self.btree.range(Some(&start_key), Some(&end_key))?;
        let uuids = entries
            .iter()
            .filter_map(|e| extract_uuid_from_composite(&e.key))
            .collect();
        Ok(uuids)
    }

    /// Range query: returns UUIDs where field value is in [start, end).
    pub fn range_query(&mut self, start: &Value, end: &Value) -> Result<Vec<Uuid>> {
        let mut start_key = encode_sortable_value(start)?;
        start_key.extend_from_slice(Uuid::nil().as_bytes());

        let mut end_key = encode_sortable_value(end)?;
        end_key.extend_from_slice(Uuid::nil().as_bytes());

        let entries = self.btree.range(Some(&start_key), Some(&end_key))?;
        let uuids = entries
            .iter()
            .filter_map(|e| extract_uuid_from_composite(&e.key))
            .collect();
        Ok(uuids)
    }

    /// Returns the number of indexed entries.
    pub fn count(&self) -> u64 {
        self.btree.len()
    }

    /// Syncs the index to disk.
    pub fn sync(&self) -> Result<()> {
        self.btree.sync()
    }

    /// Rebuilds the index from a set of documents.
    pub fn rebuild(&mut self, docs: &[(Uuid, Value)]) -> Result<()> {
        // Drop and recreate the btree
        let def = self.def.clone();
        // Remove old file
        let _ = std::fs::remove_file(&self.path);
        self.btree = BTree::<Vec<u8>>::create_with(&self.path, INDEX_MAX_KEY_SIZE)?;
        self.def = def;

        for (uuid, doc) in docs {
            self.index_document(uuid, doc)?;
        }
        self.btree.sync()?;
        Ok(())
    }

    /// Returns the file path of this index.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Extracts the UUID suffix from a composite key.
fn extract_uuid_from_composite(key: &[u8]) -> Option<Uuid> {
    if key.len() < 16 {
        return None;
    }
    let uuid_bytes = &key[key.len() - 16..];
    Some(Uuid::from_bytes(uuid_bytes.try_into().ok()?))
}

/// Increments a byte vector as a big-endian integer (for range upper bounds).
fn increment_bytes(bytes: &mut Vec<u8>) {
    for i in (0..bytes.len()).rev() {
        if bytes[i] < 0xFF {
            bytes[i] += 1;
            return;
        }
        bytes[i] = 0;
    }
    // All bytes were 0xFF — push a new byte
    bytes.push(0x00);
    bytes.insert(0, 0x01);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn make_user(name: &str, age: i64) -> Value {
        Value::Object(BTreeMap::from([
            ("name".into(), Value::String(name.into())),
            ("age".into(), Value::Integer(age)),
        ]))
    }

    #[test]
    fn test_secondary_index_create_and_open() {
        let dir = TempDir::new().unwrap();
        let def = IndexDefinition {
            name: "by_age".into(),
            field_path: "age".into(),
        };

        {
            let idx = SecondaryIndex::create(dir.path(), def.clone()).unwrap();
            assert_eq!(idx.count(), 0);
        }

        {
            let idx = SecondaryIndex::open(dir.path(), def).unwrap();
            assert_eq!(idx.count(), 0);
        }
    }

    #[test]
    fn test_secondary_index_insert_and_lookup() {
        let dir = TempDir::new().unwrap();
        let def = IndexDefinition {
            name: "by_age".into(),
            field_path: "age".into(),
        };
        let mut idx = SecondaryIndex::create(dir.path(), def).unwrap();

        let u1 = Uuid::from_u128(1);
        let u2 = Uuid::from_u128(2);
        let u3 = Uuid::from_u128(3);

        idx.index_document(&u1, &make_user("Alice", 30)).unwrap();
        idx.index_document(&u2, &make_user("Bob", 25)).unwrap();
        idx.index_document(&u3, &make_user("Charlie", 30)).unwrap();

        let results = idx.lookup(&Value::Integer(30)).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.contains(&u1));
        assert!(results.contains(&u3));

        let results = idx.lookup(&Value::Integer(25)).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results.contains(&u2));

        let results = idx.lookup(&Value::Integer(99)).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_secondary_index_range_query() {
        let dir = TempDir::new().unwrap();
        let def = IndexDefinition {
            name: "by_age".into(),
            field_path: "age".into(),
        };
        let mut idx = SecondaryIndex::create(dir.path(), def).unwrap();

        for i in 0u128..10 {
            let uuid = Uuid::from_u128(i);
            idx.index_document(&uuid, &make_user(&format!("user{i}"), i as i64 * 10))
                .unwrap();
        }

        // Range [20, 50) → ages 20, 30, 40
        let results = idx
            .range_query(&Value::Integer(20), &Value::Integer(50))
            .unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_secondary_index_delete() {
        let dir = TempDir::new().unwrap();
        let def = IndexDefinition {
            name: "by_age".into(),
            field_path: "age".into(),
        };
        let mut idx = SecondaryIndex::create(dir.path(), def).unwrap();

        let uuid = Uuid::from_u128(1);
        let doc = make_user("Alice", 30);

        idx.index_document(&uuid, &doc).unwrap();
        assert_eq!(idx.lookup(&Value::Integer(30)).unwrap().len(), 1);

        idx.unindex_document(&uuid, &doc).unwrap();
        assert!(idx.lookup(&Value::Integer(30)).unwrap().is_empty());
    }

    #[test]
    fn test_secondary_index_rebuild() {
        let dir = TempDir::new().unwrap();
        let def = IndexDefinition {
            name: "by_age".into(),
            field_path: "age".into(),
        };
        let mut idx = SecondaryIndex::create(dir.path(), def).unwrap();

        let docs: Vec<(Uuid, Value)> = (0..5)
            .map(|i| (Uuid::from_u128(i), make_user(&format!("u{i}"), i as i64)))
            .collect();

        idx.rebuild(&docs).unwrap();
        assert_eq!(idx.count(), 5);

        for i in 0..5 {
            let results = idx.lookup(&Value::Integer(i)).unwrap();
            assert_eq!(results.len(), 1);
        }
    }

    #[test]
    fn test_secondary_index_missing_field() {
        let dir = TempDir::new().unwrap();
        let def = IndexDefinition {
            name: "by_email".into(),
            field_path: "email".into(),
        };
        let mut idx = SecondaryIndex::create(dir.path(), def).unwrap();

        let uuid = Uuid::from_u128(1);
        // Document without "email" field — should not be indexed
        let doc = make_user("Alice", 30);
        idx.index_document(&uuid, &doc).unwrap();
        assert_eq!(idx.count(), 0);
    }

    #[test]
    fn test_secondary_index_string_field() {
        let dir = TempDir::new().unwrap();
        let def = IndexDefinition {
            name: "by_name".into(),
            field_path: "name".into(),
        };
        let mut idx = SecondaryIndex::create(dir.path(), def).unwrap();

        let u1 = Uuid::from_u128(1);
        let u2 = Uuid::from_u128(2);

        idx.index_document(&u1, &make_user("Alice", 30)).unwrap();
        idx.index_document(&u2, &make_user("Bob", 25)).unwrap();

        let results = idx.lookup(&Value::String("Alice".into())).unwrap();
        assert_eq!(results, vec![u1]);
    }
}
