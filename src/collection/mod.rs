//! Collection: a named unit of document storage.
//!
//! A collection manages its own data pages (via a [`BufferPool`]) and
//! primary index (via a [`BTree`]). It provides raw CRUD operations
//! without WAL — the caller (Database or GrumpyDb) handles WAL logging.
//!
//! ## On-disk layout
//!
//! ```text
//! <collection_dir>/
//!   data.db       ← slotted pages (documents)
//!   primary.idx   ← B+Tree: UUID → (PageId, SlotId)
//! ```

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::btree::BTree;
use crate::buffer::pool::BufferPool;
use crate::document::Document;
use crate::document::value::Value;
use crate::error::{GrumpyError, Result};
use crate::index::{IndexDefinition, SecondaryIndex};
use crate::page::manager::PageManager;
use crate::page::overflow;
use crate::page::slotted::SlottedPage;
use crate::page::{PAGE_SIZE, PAGE_USABLE_SPACE, PageHeader, PageType, SLOT_SIZE};

/// Maximum document size that fits in a single slotted page (without overflow).
const INLINE_MAX: usize = PAGE_USABLE_SPACE - SLOT_SIZE;

/// Default number of frames in the buffer pool.
const DEFAULT_POOL_CAPACITY: usize = 256;

/// Before/after page images returned for WAL logging.
pub struct PageWriteRecord {
    pub page_id: u32,
    pub before: [u8; PAGE_SIZE],
    pub after: [u8; PAGE_SIZE],
}

/// A named collection of documents with a primary index.
///
/// A collection is the unit of storage — it owns its data pages and B+Tree.
/// WAL logging is NOT handled here; the caller must log the returned
/// [`PageWriteRecord`]s.
pub struct Collection {
    /// Collection name.
    name: String,
    /// Path to the collection directory.
    path: PathBuf,
    /// Buffer pool wrapping the data page manager (LRU cache).
    pub(crate) data_pool: BufferPool,
    /// Primary B+Tree index: UUID → (PageId, SlotId).
    pub(crate) btree: BTree<Uuid>,
    /// Page ID of the current data page being filled.
    current_data_page: u32,
    /// Secondary indexes.
    secondary_indexes: Vec<SecondaryIndex>,
    /// Index definitions (persisted separately by the database layer).
    index_defs: Vec<IndexDefinition>,
}

impl Collection {
    /// Opens or creates a collection at the given directory.
    pub fn open(path: &Path, name: &str, pool_capacity: usize) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        let data_path = path.join("data.db");
        let index_path = path.join("primary.idx");

        let data_exists = data_path.exists() && data_path.metadata()?.len() > 0;
        let index_exists = index_path.exists() && index_path.metadata()?.len() > 0;

        let mut data_pm = PageManager::new(&data_path)?;

        let btree = if index_exists {
            BTree::<Uuid>::open(&index_path)?
        } else {
            BTree::<Uuid>::create(&index_path)?
        };

        let current_data_page = if data_exists {
            Self::find_or_alloc_data_page(&mut data_pm)?
        } else {
            let page_id = data_pm.allocate_page()?;
            let page = SlottedPage::new(page_id);
            data_pm.write_page(page_id, &page.data)?;
            page_id
        };

        let data_pool = BufferPool::new(pool_capacity, data_pm);

        Ok(Self {
            name: name.to_string(),
            path: path.to_path_buf(),
            data_pool,
            btree,
            current_data_page,
            secondary_indexes: Vec::new(),
            index_defs: Vec::new(),
        })
    }

    /// Opens a collection with default pool capacity.
    pub fn open_default(path: &Path, name: &str) -> Result<Self> {
        Self::open(path, name, DEFAULT_POOL_CAPACITY)
    }

    /// Returns the collection name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the collection directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // ── CRUD (no WAL) ───────────────────────────────────────────────────

    /// Inserts an encoded document. Returns (page_id, slot_id) and WAL records.
    ///
    /// The caller is responsible for WAL logging the returned records.
    /// Does NOT update secondary indexes — use `insert_doc` for that.
    pub fn insert_raw(
        &mut self,
        key: Uuid,
        encoded: &[u8],
    ) -> Result<((u32, u16), Vec<PageWriteRecord>)> {
        if self.btree.search(&key)?.is_some() {
            return Err(GrumpyError::DuplicateKey(key));
        }

        let (location, records) = if encoded.len() > INLINE_MAX {
            self.store_overflow(encoded)?
        } else {
            self.store_inline(encoded)?
        };

        self.btree.insert(key, location.0, location.1)?;
        Ok((location, records))
    }

    /// Retrieves raw encoded bytes by UUID key.
    pub fn get_raw(&mut self, key: &Uuid) -> Result<Option<Vec<u8>>> {
        let Some((page_id, slot_id)) = self.btree.search(key)? else {
            return Ok(None);
        };
        self.read_tuple(page_id, slot_id).map(Some)
    }

    /// Deletes a document. Returns WAL records for the page modification.
    pub fn delete_raw(&mut self, key: &Uuid) -> Result<Vec<PageWriteRecord>> {
        let Some((page_id, slot_id)) = self.btree.search(key)? else {
            return Err(GrumpyError::KeyNotFound(*key));
        };

        let frame_idx = self.data_pool.fetch_page(page_id)?;
        let slot_data = {
            let page = SlottedPage::from_bytes(self.data_pool.get_frame(frame_idx).data);
            page.get(slot_id)?.to_vec()
        };

        if overflow::is_overflow(&slot_data) {
            let (overflow_page_id, _) = overflow::decode_overflow_ref(&slot_data)
                .ok_or_else(|| GrumpyError::Corruption("malformed overflow ref".into()))?;
            overflow::free_overflow(self.data_pool.page_manager(), overflow_page_id)?;
        }

        let before = self.data_pool.get_frame(frame_idx).data;
        let mut page = SlottedPage::from_bytes(before);
        page.delete(slot_id)?;
        let after = page.data;

        self.data_pool.get_frame_mut(frame_idx).data = after;
        self.data_pool.unpin(page_id, true)?;

        self.btree.delete(key)?;

        Ok(vec![PageWriteRecord {
            page_id,
            before,
            after,
        }])
    }

    /// Scans documents in a UUID key range. Returns raw encoded bytes.
    pub fn scan_raw(
        &mut self,
        range: impl std::ops::RangeBounds<Uuid>,
    ) -> Result<Vec<(Uuid, Vec<u8>)>> {
        use std::ops::Bound;

        let start = match range.start_bound() {
            Bound::Included(k) => Some(*k),
            Bound::Excluded(k) => Some(*k),
            Bound::Unbounded => None,
        };

        let entries = self.btree.range(start.as_ref(), None)?;

        let mut results = Vec::new();
        for entry in &entries {
            match range.end_bound() {
                Bound::Included(end) => {
                    if entry.key > *end {
                        break;
                    }
                }
                Bound::Excluded(end) => {
                    if entry.key >= *end {
                        break;
                    }
                }
                Bound::Unbounded => {}
            }

            if let Bound::Excluded(start_key) = range.start_bound()
                && entry.key == *start_key
            {
                continue;
            }

            let raw = self.read_tuple(entry.page_id, entry.slot_id)?;
            results.push((entry.key, raw));
        }

        Ok(results)
    }

    /// Inserts a document and updates all secondary indexes.
    pub fn insert_doc(
        &mut self,
        key: Uuid,
        value: &Value,
        encoded: &[u8],
    ) -> Result<((u32, u16), Vec<PageWriteRecord>)> {
        let result = self.insert_raw(key, encoded)?;
        self.index_doc_in_secondaries(&key, value);
        Ok(result)
    }

    /// Deletes a document and removes it from all secondary indexes.
    pub fn delete_doc(&mut self, key: &Uuid, value: &Value) -> Result<Vec<PageWriteRecord>> {
        self.unindex_doc_from_secondaries(key, value);
        self.delete_raw(key)
    }

    // ── Maintenance ─────────────────────────────────────────────────────

    /// Returns the number of documents (O(1) from B+Tree metadata).
    pub fn document_count(&self) -> u64 {
        self.btree.len()
    }

    /// Returns buffer pool stats: (reads, writes, cached, capacity).
    pub fn pool_stats(&self) -> (u64, u64, usize, usize) {
        (
            self.data_pool.read_count,
            self.data_pool.write_count,
            self.data_pool.cached_count(),
            self.data_pool.capacity(),
        )
    }

    /// Flushes all dirty pages to disk and syncs the index.
    pub fn flush(&mut self) -> Result<()> {
        self.data_pool.flush_all()?;
        self.btree.sync()?;
        Ok(())
    }

    /// Compacts: defragments data pages and rebuilds the primary index.
    pub fn compact(&mut self) -> Result<u64> {
        self.data_pool.flush_all()?;
        self.btree.sync()?;

        let entries = self.btree.scan_all()?;
        let mut docs: Vec<(Uuid, Vec<u8>)> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let raw = self.read_tuple(entry.page_id, entry.slot_id)?;
            docs.push((entry.key, raw));
        }
        let docs_count = docs.len();

        let data_path = self.data_pool.page_manager().path().to_path_buf();
        let index_path = self.btree.pm.path().to_path_buf();
        let data_tmp = data_path.with_extension("db.compact");
        let index_tmp = index_path.with_extension("idx.compact");

        {
            let mut new_data_pm = PageManager::new(&data_tmp)?;
            let mut new_btree = BTree::create(&index_tmp)?;
            let mut current_page_id = new_data_pm.allocate_page()?;
            let mut current_page = SlottedPage::new(current_page_id);

            for (key, encoded) in &docs {
                let insert_data = if encoded.len() > INLINE_MAX {
                    let overflow_page_id = overflow::write_overflow(&mut new_data_pm, encoded)?;
                    overflow::encode_overflow_ref(overflow_page_id, encoded.len() as u32).to_vec()
                } else {
                    encoded.clone()
                };

                match current_page.insert(&insert_data) {
                    Ok(slot_id) => {
                        new_btree.insert(*key, current_page_id, slot_id)?;
                    }
                    Err(GrumpyError::PageFull(_)) => {
                        new_data_pm.write_page(current_page_id, &current_page.data)?;
                        current_page_id = new_data_pm.allocate_page()?;
                        current_page = SlottedPage::new(current_page_id);
                        let slot_id = current_page.insert(&insert_data)?;
                        new_btree.insert(*key, current_page_id, slot_id)?;
                    }
                    Err(e) => return Err(e),
                }
            }

            new_data_pm.write_page(current_page_id, &current_page.data)?;
            new_data_pm.sync()?;
            new_btree.flush_meta()?;
            new_btree.sync()?;
        }

        std::fs::rename(&data_tmp, &data_path)?;
        std::fs::rename(&index_tmp, &index_path)?;

        let new_data_pm = PageManager::new(&data_path)?;
        let new_btree = BTree::open(&index_path)?;
        let pool_capacity = self.data_pool.capacity();
        self.data_pool = BufferPool::new(pool_capacity, new_data_pm);
        self.btree = new_btree;
        self.current_data_page = Self::find_or_alloc_data_page(self.data_pool.page_manager())?;

        // Rebuild secondary indexes from the compacted data
        if !self.secondary_indexes.is_empty() {
            let decoded_docs: Vec<(Uuid, Value)> = docs
                .iter()
                .filter_map(|(key, raw)| Document::decode(raw).ok().map(|doc| (*key, doc.value)))
                .collect();

            for idx in &mut self.secondary_indexes {
                idx.rebuild(&decoded_docs)?;
            }
        }

        Ok(docs_count as u64)
    }

    /// Provides access to the data PageManager (for WAL recovery).
    pub fn data_page_manager(&mut self) -> &mut PageManager {
        self.data_pool.page_manager()
    }

    /// Provides access to the index PageManager (for WAL recovery).
    pub fn index_page_manager(&mut self) -> &mut PageManager {
        &mut self.btree.pm
    }

    // ── Secondary Indexes ───────────────────────────────────────────────

    /// Creates a secondary index on a field path and rebuilds it from existing docs.
    pub fn create_index(&mut self, name: &str, field_path: &str) -> Result<()> {
        // Check for duplicate
        if self.index_defs.iter().any(|d| d.name == name) {
            return Err(GrumpyError::IndexAlreadyExists(name.into()));
        }

        let def = IndexDefinition {
            name: name.to_string(),
            field_path: field_path.to_string(),
        };

        let mut idx = SecondaryIndex::create(&self.path, def.clone())?;

        // Rebuild from existing documents
        let entries = self.btree.scan_all()?;
        for entry in &entries {
            let raw = self.read_tuple(entry.page_id, entry.slot_id)?;
            let doc = Document::decode(&raw)?;
            idx.index_document(&entry.key, &doc.value)?;
        }
        idx.sync()?;

        self.secondary_indexes.push(idx);
        self.index_defs.push(def);
        Ok(())
    }

    /// Drops a secondary index by name.
    pub fn drop_index(&mut self, name: &str) -> Result<()> {
        let pos = self
            .index_defs
            .iter()
            .position(|d| d.name == name)
            .ok_or_else(|| GrumpyError::IndexNotFound(name.into()))?;

        let idx = self.secondary_indexes.remove(pos);
        self.index_defs.remove(pos);
        let _ = std::fs::remove_file(idx.path());
        Ok(())
    }

    /// Returns the list of index definitions.
    pub fn list_indexes(&self) -> &[IndexDefinition] {
        &self.index_defs
    }

    /// Queries a secondary index by exact value match.
    pub fn query_index(&mut self, index_name: &str, value: &Value) -> Result<Vec<(Uuid, Value)>> {
        let idx = self
            .secondary_indexes
            .iter_mut()
            .find(|i| i.def.name == index_name)
            .ok_or_else(|| GrumpyError::IndexNotFound(index_name.into()))?;

        let uuids = idx.lookup(value)?;
        let mut results = Vec::with_capacity(uuids.len());
        for uuid in uuids {
            if let Some(raw) = self.get_raw(&uuid)? {
                let doc = Document::decode(&raw)?;
                results.push((uuid, doc.value));
            }
        }
        Ok(results)
    }

    /// Queries a secondary index by range [start, end).
    pub fn query_index_range(
        &mut self,
        index_name: &str,
        start: &Value,
        end: &Value,
    ) -> Result<Vec<(Uuid, Value)>> {
        let idx = self
            .secondary_indexes
            .iter_mut()
            .find(|i| i.def.name == index_name)
            .ok_or_else(|| GrumpyError::IndexNotFound(index_name.into()))?;

        let uuids = idx.range_query(start, end)?;
        let mut results = Vec::with_capacity(uuids.len());
        for uuid in uuids {
            if let Some(raw) = self.get_raw(&uuid)? {
                let doc = Document::decode(&raw)?;
                results.push((uuid, doc.value));
            }
        }
        Ok(results)
    }

    /// Updates all secondary indexes after an insert.
    fn index_doc_in_secondaries(&mut self, key: &Uuid, value: &Value) {
        for idx in &mut self.secondary_indexes {
            let _ = idx.index_document(key, value);
        }
    }

    /// Removes a document from all secondary indexes.
    fn unindex_doc_from_secondaries(&mut self, key: &Uuid, value: &Value) {
        for idx in &mut self.secondary_indexes {
            let _ = idx.unindex_document(key, value);
        }
    }

    // ── Internal helpers ────────────────────────────────────────────────

    fn store_inline(&mut self, encoded: &[u8]) -> Result<((u32, u16), Vec<PageWriteRecord>)> {
        let frame_idx = self.data_pool.fetch_page(self.current_data_page)?;
        let before = self.data_pool.get_frame(frame_idx).data;
        let mut page = SlottedPage::from_bytes(before);

        match page.insert(encoded) {
            Ok(slot_id) => {
                let after = page.data;
                self.data_pool.get_frame_mut(frame_idx).data = after;
                self.data_pool.unpin(self.current_data_page, true)?;
                Ok((
                    (self.current_data_page, slot_id),
                    vec![PageWriteRecord {
                        page_id: self.current_data_page,
                        before,
                        after,
                    }],
                ))
            }
            Err(GrumpyError::PageFull(_)) => {
                self.data_pool.unpin(self.current_data_page, false)?;

                let (new_page_id, new_fidx) = self.data_pool.new_page()?;
                let before_new = [0u8; PAGE_SIZE];
                let mut new_page = SlottedPage::new(new_page_id);
                let slot_id = new_page.insert(encoded)?;
                let after_new = new_page.data;
                self.data_pool.get_frame_mut(new_fidx).data = after_new;
                self.data_pool.unpin(new_page_id, true)?;
                self.current_data_page = new_page_id;
                Ok((
                    (new_page_id, slot_id),
                    vec![PageWriteRecord {
                        page_id: new_page_id,
                        before: before_new,
                        after: after_new,
                    }],
                ))
            }
            Err(e) => {
                self.data_pool.unpin(self.current_data_page, false)?;
                Err(e)
            }
        }
    }

    fn store_overflow(&mut self, encoded: &[u8]) -> Result<((u32, u16), Vec<PageWriteRecord>)> {
        let overflow_page_id = overflow::write_overflow(self.data_pool.page_manager(), encoded)?;
        let ref_data = overflow::encode_overflow_ref(overflow_page_id, encoded.len() as u32);
        self.store_inline(&ref_data)
    }

    fn read_tuple(&mut self, page_id: u32, slot_id: u16) -> Result<Vec<u8>> {
        let frame_idx = self.data_pool.fetch_page(page_id)?;
        let slot_data = {
            let page = SlottedPage::from_bytes(self.data_pool.get_frame(frame_idx).data);
            page.get(slot_id)?.to_vec()
        };
        self.data_pool.unpin(page_id, false)?;

        if overflow::is_overflow(&slot_data) {
            let (overflow_page_id, _) = overflow::decode_overflow_ref(&slot_data)
                .ok_or_else(|| GrumpyError::Corruption("malformed overflow ref".into()))?;
            overflow::read_overflow(self.data_pool.page_manager(), overflow_page_id)
        } else {
            Ok(slot_data)
        }
    }

    fn find_or_alloc_data_page(pm: &mut PageManager) -> Result<u32> {
        let num_pages = pm.num_pages();
        for pid in (1..num_pages).rev() {
            let buf = pm.read_page(pid)?;
            let header = PageHeader::read_from(&buf);
            if header.page_type == PageType::Data {
                return Ok(pid);
            }
        }
        let page_id = pm.allocate_page()?;
        let page = SlottedPage::new(page_id);
        pm.write_page(page_id, &page.data)?;
        Ok(page_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use crate::document::value::Value;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Collection) {
        let dir = TempDir::new().unwrap();
        let coll = Collection::open(dir.path().join("test_coll").as_path(), "test", 16).unwrap();
        (dir, coll)
    }

    #[test]
    fn test_collection_open_creates_files() {
        let dir = TempDir::new().unwrap();
        let coll_path = dir.path().join("my_coll");
        let _coll = Collection::open(&coll_path, "my_coll", 16).unwrap();
        assert!(coll_path.join("data.db").exists());
        assert!(coll_path.join("primary.idx").exists());
    }

    #[test]
    fn test_collection_insert_and_get() {
        let (_dir, mut coll) = setup();
        let key = Uuid::new_v4();
        let doc = Document::new(key, Value::String("hello".into()));
        let encoded = doc.encode();

        let ((pid, sid), records) = coll.insert_raw(key, &encoded).unwrap();
        assert!(pid > 0 || sid == 0); // just check it returned something
        assert!(!records.is_empty());

        let raw = coll.get_raw(&key).unwrap().unwrap();
        let decoded = Document::decode(&raw).unwrap();
        assert_eq!(decoded.value, Value::String("hello".into()));
    }

    #[test]
    fn test_collection_delete() {
        let (_dir, mut coll) = setup();
        let key = Uuid::new_v4();
        let doc = Document::new(key, Value::Integer(42));
        coll.insert_raw(key, &doc.encode()).unwrap();

        let records = coll.delete_raw(&key).unwrap();
        assert!(!records.is_empty());
        assert!(coll.get_raw(&key).unwrap().is_none());
    }

    #[test]
    fn test_collection_scan() {
        let (_dir, mut coll) = setup();
        for i in 0u128..20 {
            let key = Uuid::from_u128(i);
            let doc = Document::new(key, Value::Integer(i as i64));
            coll.insert_raw(key, &doc.encode()).unwrap();
        }

        let results = coll.scan_raw(..).unwrap();
        assert_eq!(results.len(), 20);
    }

    #[test]
    fn test_collection_document_count() {
        let (_dir, mut coll) = setup();
        assert_eq!(coll.document_count(), 0);

        let key = Uuid::new_v4();
        let doc = Document::new(key, Value::Null);
        coll.insert_raw(key, &doc.encode()).unwrap();
        assert_eq!(coll.document_count(), 1);
    }

    #[test]
    fn test_collection_compact() {
        let (_dir, mut coll) = setup();

        for i in 0u128..100 {
            let key = Uuid::from_u128(i);
            let doc = Document::new(key, Value::Integer(i as i64));
            coll.insert_raw(key, &doc.encode()).unwrap();
        }

        for i in 0u128..50 {
            coll.delete_raw(&Uuid::from_u128(i)).unwrap();
        }

        let count = coll.compact().unwrap();
        assert_eq!(count, 50);
        assert_eq!(coll.document_count(), 50);

        for i in 50u128..100 {
            assert!(coll.get_raw(&Uuid::from_u128(i)).unwrap().is_some());
        }
    }

    #[test]
    fn test_collection_overflow() {
        let (_dir, mut coll) = setup();
        let key = Uuid::new_v4();
        let large = Value::String("x".repeat(10_000));
        let doc = Document::new(key, large.clone());
        coll.insert_raw(key, &doc.encode()).unwrap();

        let raw = coll.get_raw(&key).unwrap().unwrap();
        let decoded = Document::decode(&raw).unwrap();
        assert_eq!(decoded.value, large);
    }

    #[test]
    fn test_collection_persistence() {
        let dir = TempDir::new().unwrap();
        let coll_path = dir.path().join("persist_coll");
        let key = Uuid::from_u128(42);

        {
            let mut coll = Collection::open(&coll_path, "persist", 16).unwrap();
            let doc = Document::new(key, Value::String("persistent".into()));
            coll.insert_raw(key, &doc.encode()).unwrap();
            coll.flush().unwrap();
        }

        {
            let mut coll = Collection::open(&coll_path, "persist", 16).unwrap();
            let raw = coll.get_raw(&key).unwrap().unwrap();
            let decoded = Document::decode(&raw).unwrap();
            assert_eq!(decoded.value, Value::String("persistent".into()));
        }
    }

    #[test]
    fn test_collection_duplicate_key() {
        let (_dir, mut coll) = setup();
        let key = Uuid::new_v4();
        let doc = Document::new(key, Value::Null);
        coll.insert_raw(key, &doc.encode()).unwrap();
        let result = coll.insert_raw(key, &doc.encode());
        assert!(matches!(result, Err(GrumpyError::DuplicateKey(_))));
    }

    #[test]
    fn test_collection_pool_stats() {
        let (_dir, coll) = setup();
        let (reads, writes, cached, capacity) = coll.pool_stats();
        assert_eq!(reads, 0);
        assert_eq!(writes, 0);
        assert!(cached <= capacity);
    }
}
