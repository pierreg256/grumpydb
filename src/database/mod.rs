//! Database: manages multiple named collections with a shared WAL.
//!
//! A database is the unit of transaction — all CRUD operations are scoped
//! to a single database. Each database has its own WAL and collections.
//!
//! ## On-disk layout
//!
//! ```text
//! <database_dir>/
//!   wal.log            ← Write-Ahead Log (shared across collections)
//!   <collection_name>/
//!     data.db
//!     primary.idx
//!     idx_*.idx        ← secondary indexes
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;

use crate::collection::Collection;
use crate::document::Document;
use crate::document::value::Value;
use crate::error::{GrumpyError, Result};
use crate::index::encoding::{encode_sortable_value, extract_field};
use crate::naming::validate_name;
use crate::wal::applied_set::AppliedSet;
use crate::wal::hlc::{Hlc, HlcClock, HlcError};
use crate::wal::vclock::VectorClock;
use crate::wal::writer::WalWriter;

/// Default buffer pool capacity per collection.
const DEFAULT_POOL_CAPACITY: usize = 256;

/// Number of writes between automatic checkpoints.
const CHECKPOINT_INTERVAL: u32 = 100;

/// Subdirectory (under the database dir) where the engine identity is
/// persisted.
const IDENTITY_DIR: &str = "_database";
/// File name for the persisted engine identity.
const IDENTITY_FILE: &str = "node.json";
/// File name for persisted database-level consistency defaults.
const CONSISTENCY_FILE: &str = "consistency.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedIdentity {
    /// Hyphenated UUID string of the engine's node identifier.
    node_id: String,
    /// Schema version of this file (currently 1).
    schema_version: u32,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
struct PersistedConsistencyDefaults {
    /// Schema version of this file (currently 1).
    schema_version: u32,
    /// Optional default read concern for this database.
    read_concern: Option<u16>,
    /// Optional default write concern for this database.
    write_concern: Option<u16>,
}

/// A database containing multiple named collections.
pub struct Database {
    /// Path to the database directory.
    path: PathBuf,
    /// Named collections.
    collections: HashMap<String, Collection>,
    /// Shared Write-Ahead Log.
    wal: WalWriter,
    /// Write counter for periodic checkpointing.
    writes_since_checkpoint: u32,
    /// Engine-side node identifier (Phase 40b).
    node_id: u128,
    /// Shared HLC clock used to stamp every WAL record (Phase 40b).
    clock: Arc<HlcClock>,
    /// Per-origin "highest applied HLC" tracker. Allocated and
    /// persisted from Phase 40b onward; not consulted on the write path
    /// in v5 (Phase 40e will start using it).
    applied_set: AppliedSet,
    /// Per-collection key history used by snapshot reads (Phase 41
    /// tranche 2). Each key stores append-only committed versions.
    versions: HashMap<String, HashMap<Uuid, Vec<VersionedValue>>>,
    /// Active readers grouped by their snapshot HLC.
    active_readers: BTreeMap<Hlc, usize>,
    /// Optional database-level defaults used by the server protocol layer.
    read_concern_default: Option<u16>,
    /// Optional database-level defaults used by the server protocol layer.
    write_concern_default: Option<u16>,
}

#[derive(Debug, Clone)]
struct VersionedValue {
    hlc: Hlc,
    value: Option<Value>,
}

/// Read-only transaction pinned to a point-in-time HLC snapshot.
///
/// Phase 41 (tranche 1): the snapshot timestamp is captured and exposed via
/// [`ReadTx::snapshot_hlc`]. Reads are routed through the current engine read
/// path while preserving a stable API for future MVCC page-version selection.
pub struct ReadTx<'a> {
    db: &'a mut Database,
    snapshot_hlc: Hlc,
}

impl Drop for ReadTx<'_> {
    fn drop(&mut self) {
        self.db.unregister_reader_snapshot(self.snapshot_hlc);
    }
}

impl ReadTx<'_> {
    /// HLC value representing this read transaction's snapshot point.
    pub fn snapshot_hlc(&self) -> Hlc {
        self.snapshot_hlc
    }

    /// Reads one document at the transaction snapshot.
    pub fn get(&mut self, collection: &str, key: &Uuid) -> Result<Option<Value>> {
        self.db.snapshot_get(collection, key, self.snapshot_hlc)
    }

    /// Scans a key range at the transaction snapshot.
    pub fn scan(
        &mut self,
        collection: &str,
        range: impl std::ops::RangeBounds<Uuid>,
    ) -> Result<Vec<(Uuid, Value)>> {
        self.db.snapshot_scan(collection, range, self.snapshot_hlc)
    }

    /// Queries a secondary index at the transaction snapshot.
    pub fn query(
        &mut self,
        collection: &str,
        index_name: &str,
        value: &Value,
    ) -> Result<Vec<(Uuid, Value)>> {
        self.db
            .snapshot_query(collection, index_name, value, self.snapshot_hlc)
    }

    /// Range-query on a secondary index at the transaction snapshot.
    pub fn query_range(
        &mut self,
        collection: &str,
        index_name: &str,
        start: &Value,
        end: &Value,
    ) -> Result<Vec<(Uuid, Value)>> {
        self.db
            .snapshot_query_range(collection, index_name, start, end, self.snapshot_hlc)
    }
}

impl Database {
    /// Opens or creates a database at the given directory.
    ///
    /// On first open, an engine identity (random UUID) is generated and
    /// persisted at `<path>/_database/node.json`. A fresh
    /// [`HlcClock`] is created and used to stamp every WAL record.
    /// Subsequent opens reuse the persisted identity.
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        let node_id = load_or_create_identity(path)?;
        let clock = Arc::new(HlcClock::new());
        Self::open_with(path, node_id, clock)
    }

    /// Opens or creates a database with an explicit node identity and
    /// HLC clock. Used by the server (which carries its own cluster
    /// identity from `_cluster/node.json`).
    pub fn open_with(path: &Path, node_id: u128, clock: Arc<HlcClock>) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        let wal_path = path.join("wal.log");
        let wal = WalWriter::new_with_identity(&wal_path, node_id, Arc::clone(&clock))?;
        let applied_set = AppliedSet::load(path)?;
        let (read_concern_default, write_concern_default) = load_consistency_defaults(path)?;

        // Discover existing collections by scanning subdirectories
        let mut collections = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let ft = entry.file_type()?;
                if ft.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    // Skip hidden dirs and engine-internal dirs.
                    if name.starts_with('.') || name == IDENTITY_DIR || name == "_replication" {
                        continue;
                    }
                    let coll_path = entry.path();
                    if coll_path.join("data.db").exists() {
                        let coll = Collection::open(&coll_path, &name, DEFAULT_POOL_CAPACITY)?;
                        collections.insert(name, coll);
                    }
                }
            }
        }

        Ok(Self {
            path: path.to_path_buf(),
            collections,
            wal,
            writes_since_checkpoint: 0,
            node_id,
            clock,
            applied_set,
            versions: HashMap::new(),
            active_readers: BTreeMap::new(),
            read_concern_default,
            write_concern_default,
        })
    }

    // ── Read snapshots (Phase 41) ───────────────────────────────────

    /// Begins a read transaction pinned to the current HLC snapshot.
    ///
    /// The returned [`ReadTx`] exposes read methods and carries the snapshot
    /// timestamp through [`ReadTx::snapshot_hlc`] so callers can correlate
    /// reads with replication/consistency workflows.
    pub fn begin_read(&mut self) -> ReadTx<'_> {
        let snapshot_hlc = self.current_hlc();
        self.register_reader_snapshot(snapshot_hlc);
        ReadTx {
            db: self,
            snapshot_hlc,
        }
    }

    // ── Collection management ───────────────────────────────────────────

    /// Creates a new collection.
    pub fn create_collection(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        if self.collections.contains_key(name) {
            return Err(GrumpyError::CollectionNotFound(format!(
                "collection '{name}' already exists"
            )));
        }
        let coll_path = self.path.join(name);
        let coll = Collection::open(&coll_path, name, DEFAULT_POOL_CAPACITY)?;
        self.collections.insert(name.to_string(), coll);
        Ok(())
    }

    /// Drops a collection, deleting all its files.
    pub fn drop_collection(&mut self, name: &str) -> Result<()> {
        let coll = self
            .collections
            .remove(name)
            .ok_or_else(|| GrumpyError::CollectionNotFound(name.into()))?;
        self.versions.remove(name);
        let coll_path = coll.path().to_path_buf();
        drop(coll);
        std::fs::remove_dir_all(&coll_path)?;
        Ok(())
    }

    /// Lists all collection names.
    pub fn list_collections(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.collections.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// Returns a mutable reference to a collection.
    pub fn collection(&mut self, name: &str) -> Result<&mut Collection> {
        self.collections
            .get_mut(name)
            .ok_or_else(|| GrumpyError::CollectionNotFound(name.into()))
    }

    // ── CRUD ────────────────────────────────────────────────────────────

    /// Inserts a document into a collection.
    pub fn insert(&mut self, collection: &str, key: Uuid, value: Value) -> Result<()> {
        let doc = Document::new(key, value.clone());
        let encoded = doc.encode();

        let tx_id = self.wal.begin_tx();

        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let (_, records) = coll.insert_doc(key, &value, &encoded)?;

        for rec in &records {
            self.wal
                .log_page_write(tx_id, rec.page_id, &rec.before, &rec.after)?;
        }

        let (_, commit_hlc) = self.wal.log_commit(tx_id)?;
        self.append_version(collection, key, commit_hlc, Some(value));
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Retrieves a document from a collection.
    pub fn get(&mut self, collection: &str, key: &Uuid) -> Result<Option<Value>> {
        let Some(value) = self.get_including_tombstone(collection, key)? else {
            return Ok(None);
        };
        if value.is_tombstone() {
            return Ok(None);
        }
        Ok(Some(value))
    }

    /// Retrieves a document without applying tombstone visibility rules.
    fn get_including_tombstone(&mut self, collection: &str, key: &Uuid) -> Result<Option<Value>> {
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let Some(raw) = coll.get_raw(key)? else {
            return Ok(None);
        };
        let doc = Document::decode(&raw)?;
        Ok(Some(doc.value))
    }

    /// Updates a document in a collection.
    pub fn update(&mut self, collection: &str, key: &Uuid, value: Value) -> Result<()> {
        // Get old value for unindexing
        let old_value = self
            .get_including_tombstone(collection, key)?
            .ok_or(GrumpyError::KeyNotFound(*key))?;
        self.ensure_baseline_version(collection, *key, old_value.clone());

        // Delete old (with unindexing)
        let tx_id = self.wal.begin_tx();
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let del_records = coll.delete_doc(key, &old_value)?;
        for rec in &del_records {
            self.wal
                .log_page_write(tx_id, rec.page_id, &rec.before, &rec.after)?;
        }

        // Insert new (with indexing)
        let doc = Document::new(*key, value.clone());
        let encoded = doc.encode();

        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let (_, ins_records) = coll.insert_doc(*key, &value, &encoded)?;
        for rec in &ins_records {
            self.wal
                .log_page_write(tx_id, rec.page_id, &rec.before, &rec.after)?;
        }

        let (_, commit_hlc) = self.wal.log_commit(tx_id)?;
        self.append_version(collection, *key, commit_hlc, Some(value));
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Deletes a document from a collection.
    pub fn delete(&mut self, collection: &str, key: &Uuid) -> Result<()> {
        // Get value for unindexing
        let value = self
            .get(collection, key)?
            .ok_or(GrumpyError::KeyNotFound(*key))?;
        self.ensure_baseline_version(collection, *key, value.clone());

        let deleted_at_hlc = self
            .clock
            .now()
            .map_err(|e| GrumpyError::Hlc(e.to_string()))?;
        let mut vector_clock = Vec::new();
        VectorClock::singleton(self.node_id, deleted_at_hlc.0).encode_to(&mut vector_clock);
        let tombstone = Value::Tombstone {
            deleted_at_hlc: deleted_at_hlc.0,
            vector_clock,
        };

        let tx_id = self.wal.begin_tx();
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let del_records = coll.delete_doc(key, &value)?;
        for rec in &del_records {
            self.wal
                .log_page_write(tx_id, rec.page_id, &rec.before, &rec.after)?;
        }

        let doc = Document::new(*key, tombstone.clone());
        let encoded = doc.encode();
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let (_, ins_records) = coll.insert_doc(*key, &tombstone, &encoded)?;
        for rec in &ins_records {
            self.wal
                .log_page_write(tx_id, rec.page_id, &rec.before, &rec.after)?;
        }

        let (_, commit_hlc) = self.wal.log_commit(tx_id)?;
        self.append_version(collection, *key, commit_hlc, None);
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Scans documents in a collection.
    pub fn scan(
        &mut self,
        collection: &str,
        range: impl std::ops::RangeBounds<Uuid>,
    ) -> Result<Vec<(Uuid, Value)>> {
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;

        let raw_results = coll.scan_raw(range)?;
        let mut results = Vec::with_capacity(raw_results.len());
        for (key, raw) in raw_results {
            let doc = Document::decode(&raw)?;
            if doc.value.is_tombstone() {
                continue;
            }
            results.push((key, doc.value));
        }
        Ok(results)
    }

    // ── Index management ────────────────────────────────────────────────

    /// Creates a secondary index on a collection field.
    pub fn create_index(
        &mut self,
        collection: &str,
        index_name: &str,
        field_path: &str,
    ) -> Result<()> {
        validate_name(index_name)?;
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
        coll.create_index(index_name, field_path)
    }

    /// Drops a secondary index.
    pub fn drop_index(&mut self, collection: &str, index_name: &str) -> Result<()> {
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
        coll.drop_index(index_name)
    }

    /// Queries a secondary index by exact value.
    pub fn query(
        &mut self,
        collection: &str,
        index_name: &str,
        value: &Value,
    ) -> Result<Vec<(Uuid, Value)>> {
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
        let rows = coll.query_index(index_name, value)?;
        Ok(rows
            .into_iter()
            .filter(|(_, v)| !v.is_tombstone())
            .collect())
    }

    /// Queries a secondary index by range [start, end).
    pub fn query_range(
        &mut self,
        collection: &str,
        index_name: &str,
        start: &Value,
        end: &Value,
    ) -> Result<Vec<(Uuid, Value)>> {
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
        let rows = coll.query_index_range(index_name, start, end)?;
        Ok(rows
            .into_iter()
            .filter(|(_, v)| !v.is_tombstone())
            .collect())
    }

    // ── References ─────────────────────────────────────────────────────

    /// Resolves a single reference to its target document value.
    pub fn resolve_ref(&mut self, collection: &str, id: &Uuid) -> Result<Option<Value>> {
        self.get(collection, id)
    }

    /// Recursively resolves all `Ref` values in a value tree.
    ///
    /// Each `Value::Ref(collection, uuid)` is replaced by the target document's
    /// value. Cycles are detected and return `GrumpyError::CyclicReference`.
    pub fn resolve_deep(&mut self, value: &Value, max_depth: usize) -> Result<Value> {
        let mut visited = HashSet::new();
        self.resolve_recursive(value, max_depth, 0, &mut visited)
    }

    fn resolve_recursive(
        &mut self,
        value: &Value,
        max_depth: usize,
        depth: usize,
        visited: &mut HashSet<(String, Uuid)>,
    ) -> Result<Value> {
        if depth > max_depth {
            return Ok(value.clone());
        }

        match value {
            Value::Ref(collection, uuid) => {
                let key = (collection.clone(), *uuid);
                if !visited.insert(key) {
                    return Err(GrumpyError::CyclicReference);
                }
                match self.get(collection, uuid)? {
                    Some(target) => self.resolve_recursive(&target, max_depth, depth + 1, visited),
                    None => Ok(value.clone()), // target not found — keep ref as-is
                }
            }
            Value::Object(map) => {
                let mut resolved = std::collections::BTreeMap::new();
                for (k, v) in map {
                    resolved.insert(
                        k.clone(),
                        self.resolve_recursive(v, max_depth, depth, visited)?,
                    );
                }
                Ok(Value::Object(resolved))
            }
            Value::Array(arr) => {
                let resolved: Result<Vec<Value>> = arr
                    .iter()
                    .map(|v| self.resolve_recursive(v, max_depth, depth, visited))
                    .collect();
                Ok(Value::Array(resolved?))
            }
            _ => Ok(value.clone()),
        }
    }

    // ── Maintenance ─────────────────────────────────────────────────────

    /// Returns the document count for a collection.
    pub fn document_count(&mut self, collection: &str) -> Result<u64> {
        let coll = self
            .collections
            .get(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
        Ok(coll.document_count())
    }

    /// Flushes all collections and writes a WAL checkpoint.
    pub fn flush(&mut self) -> Result<()> {
        for coll in self.collections.values_mut() {
            coll.flush()?;
        }
        self.wal.log_checkpoint()?;
        self.wal.truncate()?;
        self.writes_since_checkpoint = 0;
        Ok(())
    }

    /// Compacts a specific collection.
    pub fn compact(&mut self, collection: &str) -> Result<u64> {
        let coll = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
        let count = coll.compact()?;
        self.wal.log_checkpoint()?;
        self.wal.truncate()?;
        self.writes_since_checkpoint = 0;
        Ok(count)
    }

    /// Returns database-level consistency defaults.
    pub fn consistency_defaults(&self) -> (Option<u16>, Option<u16>) {
        (self.read_concern_default, self.write_concern_default)
    }

    /// Sets database-level consistency defaults.
    pub fn set_consistency_defaults(
        &mut self,
        read_concern: Option<u16>,
        write_concern: Option<u16>,
    ) -> Result<()> {
        if read_concern == Some(0) {
            return Err(GrumpyError::InvalidArgument(
                "read_concern must be >= 1".into(),
            ));
        }
        if write_concern == Some(0) {
            return Err(GrumpyError::InvalidArgument(
                "write_concern must be >= 1".into(),
            ));
        }
        self.read_concern_default = read_concern;
        self.write_concern_default = write_concern;
        save_consistency_defaults(&self.path, read_concern, write_concern)
    }

    /// Resets database-level consistency defaults to engine fallbacks.
    pub fn reset_consistency_defaults(&mut self) -> Result<()> {
        self.set_consistency_defaults(None, None)
    }

    /// Closes the database, flushing all data.
    pub fn close(mut self) -> Result<()> {
        // Persist the AppliedSet (no-op in v5 single-writer; the file
        // is created the first time observe() ever advances anything).
        let _ = self.applied_set.save(&self.path);
        self.flush()
    }

    /// Returns the database directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the engine-side node identifier (Phase 40b).
    pub fn node_id(&self) -> u128 {
        self.node_id
    }

    /// Returns the last issued HLC value (Phase 40b).
    pub fn current_hlc(&self) -> Hlc {
        self.clock.read()
    }

    /// Blends a remote HLC into the local clock state. Used by Phase
    /// 40e replication apply when ingesting records produced by another
    /// node. Returns the new local HLC after the merge.
    pub fn record_remote_hlc(&self, hlc: Hlc) -> Result<Hlc> {
        self.clock
            .update(hlc)
            .map_err(|e: HlcError| GrumpyError::Hlc(e.to_string()))
    }

    /// Returns a clone of the shared HLC clock (cheap — `Arc::clone`).
    pub fn clock(&self) -> Arc<HlcClock> {
        Arc::clone(&self.clock)
    }

    fn maybe_checkpoint(&mut self) -> Result<()> {
        self.writes_since_checkpoint += 1;
        if self.writes_since_checkpoint >= CHECKPOINT_INTERVAL {
            self.flush()?;
        }
        Ok(())
    }

    fn append_version(&mut self, collection: &str, key: Uuid, hlc: Hlc, value: Option<Value>) {
        let versions = self
            .versions
            .entry(collection.to_string())
            .or_default()
            .entry(key)
            .or_default();
        versions.push(VersionedValue { hlc, value });
        self.gc_versions();
    }

    fn ensure_baseline_version(&mut self, collection: &str, key: Uuid, value: Value) {
        let versions = self
            .versions
            .entry(collection.to_string())
            .or_default()
            .entry(key)
            .or_default();
        if versions.is_empty() {
            versions.push(VersionedValue {
                hlc: Hlc::ZERO,
                value: Some(value),
            });
        }
    }

    fn lookup_snapshot_version(
        &self,
        collection: &str,
        key: &Uuid,
        snapshot: Hlc,
    ) -> Option<Option<Value>> {
        let key_versions = self.versions.get(collection)?.get(key)?;
        let mut selected = None;
        let mut found = false;
        for version in key_versions {
            if version.hlc <= snapshot {
                selected = version.value.clone();
                found = true;
            } else {
                break;
            }
        }
        if found {
            Some(selected)
        } else {
            // History exists, but key was created after snapshot.
            Some(None)
        }
    }

    pub(crate) fn register_reader_snapshot(&mut self, snapshot: Hlc) {
        *self.active_readers.entry(snapshot).or_insert(0) += 1;
    }

    pub(crate) fn unregister_reader_snapshot(&mut self, snapshot: Hlc) {
        let mut remove = false;
        if let Some(count) = self.active_readers.get_mut(&snapshot) {
            if *count > 1 {
                *count -= 1;
            } else {
                remove = true;
            }
        }
        if remove {
            self.active_readers.remove(&snapshot);
        }
        self.gc_versions();
    }

    pub(crate) fn reader_watermark(&self) -> Option<Hlc> {
        self.active_readers.first_key_value().map(|(hlc, _)| *hlc)
    }

    fn gc_versions(&mut self) {
        let watermark = self.reader_watermark();
        for by_key in self.versions.values_mut() {
            for versions in by_key.values_mut() {
                if versions.len() <= 1 {
                    continue;
                }

                match watermark {
                    Some(wm) => {
                        let mut anchor_idx = None;
                        for (idx, version) in versions.iter().enumerate() {
                            if version.hlc <= wm {
                                anchor_idx = Some(idx);
                            } else {
                                break;
                            }
                        }

                        let mut retained = Vec::with_capacity(versions.len());
                        if let Some(idx) = anchor_idx {
                            retained.push(versions[idx].clone());
                            for version in versions.iter().skip(idx + 1) {
                                retained.push(version.clone());
                            }
                        } else {
                            for version in versions.iter() {
                                retained.push(version.clone());
                            }
                        }
                        *versions = retained;
                    }
                    None => {
                        if let Some(last) = versions.last().cloned() {
                            versions.clear();
                            versions.push(last);
                        }
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn debug_version_len(&self, collection: &str, key: &Uuid) -> usize {
        self.versions
            .get(collection)
            .and_then(|by_key| by_key.get(key))
            .map(Vec::len)
            .unwrap_or(0)
    }

    pub(crate) fn snapshot_get(
        &mut self,
        collection: &str,
        key: &Uuid,
        snapshot: Hlc,
    ) -> Result<Option<Value>> {
        if let Some(value) = self.lookup_snapshot_version(collection, key, snapshot) {
            return Ok(value);
        }
        self.get(collection, key)
    }

    pub(crate) fn snapshot_scan(
        &mut self,
        collection: &str,
        range: impl std::ops::RangeBounds<Uuid>,
        snapshot: Hlc,
    ) -> Result<Vec<(Uuid, Value)>> {
        let mut visible = BTreeMap::new();
        for (key, value) in self.scan(collection, ..)? {
            if range.contains(&key) {
                visible.insert(key, value);
            }
        }

        if let Some(by_key) = self.versions.get(collection) {
            for key in by_key.keys() {
                if !range.contains(key) {
                    continue;
                }
                if let Some(maybe_value) = self.lookup_snapshot_version(collection, key, snapshot) {
                    match maybe_value {
                        Some(value) => {
                            visible.insert(*key, value);
                        }
                        None => {
                            visible.remove(key);
                        }
                    }
                }
            }
        }

        Ok(visible.into_iter().collect())
    }

    pub(crate) fn snapshot_query(
        &mut self,
        collection: &str,
        index_name: &str,
        value: &Value,
        snapshot: Hlc,
    ) -> Result<Vec<(Uuid, Value)>> {
        let field_path = {
            let coll = self
                .collections
                .get(collection)
                .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
            coll.list_indexes()
                .iter()
                .find(|def| def.name == index_name)
                .ok_or_else(|| GrumpyError::IndexNotFound(index_name.into()))?
                .field_path
                .clone()
        };

        let rows = self.snapshot_scan(collection, .., snapshot)?;
        Ok(rows
            .into_iter()
            .filter(|(_, doc)| extract_field(doc, &field_path) == Some(value))
            .collect())
    }

    pub(crate) fn snapshot_query_range(
        &mut self,
        collection: &str,
        index_name: &str,
        start: &Value,
        end: &Value,
        snapshot: Hlc,
    ) -> Result<Vec<(Uuid, Value)>> {
        let field_path = {
            let coll = self
                .collections
                .get(collection)
                .ok_or_else(|| GrumpyError::CollectionNotFound(collection.into()))?;
            coll.list_indexes()
                .iter()
                .find(|def| def.name == index_name)
                .ok_or_else(|| GrumpyError::IndexNotFound(index_name.into()))?
                .field_path
                .clone()
        };

        let start_key = encode_sortable_value(start)?;
        let end_key = encode_sortable_value(end)?;

        let rows = self.snapshot_scan(collection, .., snapshot)?;
        let mut out = Vec::new();
        for (key, doc) in rows {
            let Some(field) = extract_field(&doc, &field_path) else {
                continue;
            };
            let Ok(encoded) = encode_sortable_value(field) else {
                continue;
            };
            if encoded >= start_key && encoded < end_key {
                out.push((key, doc));
            }
        }
        Ok(out)
    }
}

/// Loads the engine identity from `<path>/_database/node.json`. Creates
/// a fresh random one (and persists it) if the file is missing.
fn load_or_create_identity(path: &Path) -> Result<u128> {
    let dir = path.join(IDENTITY_DIR);
    let file = dir.join(IDENTITY_FILE);
    if file.exists() {
        let bytes = std::fs::read(&file)?;
        let parsed: PersistedIdentity = serde_json::from_slice(&bytes)
            .map_err(|e| GrumpyError::Corruption(format!("invalid {IDENTITY_FILE}: {e}")))?;
        let uuid = Uuid::parse_str(&parsed.node_id)
            .map_err(|e| GrumpyError::Corruption(format!("invalid node_id UUID: {e}")))?;
        return Ok(uuid.as_u128());
    }
    std::fs::create_dir_all(&dir)?;
    let new_id = Uuid::new_v4();
    let body = serde_json::to_vec_pretty(&PersistedIdentity {
        node_id: new_id.hyphenated().to_string(),
        schema_version: 1,
    })
    .map_err(|e| GrumpyError::Corruption(format!("serialize identity: {e}")))?;
    let tmp = dir.join(format!("{IDENTITY_FILE}.tmp"));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &file)?;
    Ok(new_id.as_u128())
}

fn load_consistency_defaults(path: &Path) -> Result<(Option<u16>, Option<u16>)> {
    let file = path.join(IDENTITY_DIR).join(CONSISTENCY_FILE);
    if !file.exists() {
        return Ok((None, None));
    }

    let bytes = std::fs::read(&file)?;
    let parsed: PersistedConsistencyDefaults = serde_json::from_slice(&bytes)
        .map_err(|e| GrumpyError::Corruption(format!("invalid {CONSISTENCY_FILE}: {e}")))?;

    if parsed.read_concern == Some(0) {
        return Err(GrumpyError::Corruption(
            "invalid consistency default: read_concern must be >= 1".into(),
        ));
    }
    if parsed.write_concern == Some(0) {
        return Err(GrumpyError::Corruption(
            "invalid consistency default: write_concern must be >= 1".into(),
        ));
    }

    Ok((parsed.read_concern, parsed.write_concern))
}

fn save_consistency_defaults(
    path: &Path,
    read_concern: Option<u16>,
    write_concern: Option<u16>,
) -> Result<()> {
    let dir = path.join(IDENTITY_DIR);
    let file = dir.join(CONSISTENCY_FILE);

    if read_concern.is_none() && write_concern.is_none() {
        if file.exists() {
            std::fs::remove_file(file)?;
        }
        return Ok(());
    }

    std::fs::create_dir_all(&dir)?;
    let body = serde_json::to_vec_pretty(&PersistedConsistencyDefaults {
        schema_version: 1,
        read_concern,
        write_concern,
    })
    .map_err(|e| GrumpyError::Corruption(format!("serialize consistency defaults: {e}")))?;
    let tmp = dir.join(format!("{CONSISTENCY_FILE}.tmp"));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Database) {
        let dir = TempDir::new().unwrap();
        let db = Database::open(dir.path().join("testdb").as_path()).unwrap();
        (dir, db)
    }

    #[test]
    fn test_database_open_creates_dir() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("newdb");
        let _db = Database::open(&db_path).unwrap();
        assert!(db_path.exists());
    }

    #[test]
    fn test_create_and_list_collections() {
        let (_dir, mut db) = setup();
        assert!(db.list_collections().is_empty());

        db.create_collection("users").unwrap();
        db.create_collection("tasks").unwrap();

        let colls = db.list_collections();
        assert_eq!(colls, vec!["tasks", "users"]);
    }

    #[test]
    fn test_drop_collection() {
        let (_dir, mut db) = setup();
        db.create_collection("temp").unwrap();
        db.insert("temp", Uuid::new_v4(), Value::Null).unwrap();

        db.drop_collection("temp").unwrap();
        assert!(db.list_collections().is_empty());
        assert!(db.get("temp", &Uuid::new_v4()).is_err());
    }

    #[test]
    fn test_crud_across_collections() {
        let (_dir, mut db) = setup();
        db.create_collection("users").unwrap();
        db.create_collection("tasks").unwrap();

        let user_key = Uuid::from_u128(1);
        let task_key = Uuid::from_u128(2);

        db.insert(
            "users",
            user_key,
            Value::Object(BTreeMap::from([(
                "name".into(),
                Value::String("Alice".into()),
            )])),
        )
        .unwrap();

        db.insert("tasks", task_key, Value::String("Buy milk".into()))
            .unwrap();

        // Verify isolation
        assert!(db.get("users", &user_key).unwrap().is_some());
        assert!(db.get("tasks", &task_key).unwrap().is_some());
        assert!(db.get("users", &task_key).unwrap().is_none());
        assert!(db.get("tasks", &user_key).unwrap().is_none());
    }

    #[test]
    fn test_update_and_delete() {
        let (_dir, mut db) = setup();
        db.create_collection("items").unwrap();

        let key = Uuid::from_u128(42);
        db.insert("items", key, Value::Integer(1)).unwrap();

        db.update("items", &key, Value::Integer(2)).unwrap();
        assert_eq!(db.get("items", &key).unwrap(), Some(Value::Integer(2)));

        db.delete("items", &key).unwrap();
        assert_eq!(db.get("items", &key).unwrap(), None);
    }

    #[test]
    fn test_database_consistency_defaults_persist_across_reopen() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("settingsdb");

        {
            let mut db = Database::open(&db_path).unwrap();
            db.set_consistency_defaults(Some(2), Some(3)).unwrap();
            assert_eq!(db.consistency_defaults(), (Some(2), Some(3)));
            db.close().unwrap();
        }

        {
            let db = Database::open(&db_path).unwrap();
            assert_eq!(db.consistency_defaults(), (Some(2), Some(3)));
        }
    }

    #[test]
    fn test_database_consistency_defaults_reset_removes_file() {
        let (_dir, mut db) = setup();
        db.set_consistency_defaults(Some(2), Some(2)).unwrap();
        db.reset_consistency_defaults().unwrap();
        assert_eq!(db.consistency_defaults(), (None, None));
        assert!(!db.path().join(IDENTITY_DIR).join(CONSISTENCY_FILE).exists());
    }

    #[test]
    fn test_database_consistency_defaults_reject_zero() {
        let (_dir, mut db) = setup();
        let err = db.set_consistency_defaults(Some(0), Some(1)).unwrap_err();
        assert!(matches!(err, GrumpyError::InvalidArgument(_)));
        let err = db.set_consistency_defaults(Some(1), Some(0)).unwrap_err();
        assert!(matches!(err, GrumpyError::InvalidArgument(_)));
    }

    #[test]
    fn test_scan() {
        let (_dir, mut db) = setup();
        db.create_collection("nums").unwrap();

        for i in 0u128..10 {
            db.insert("nums", Uuid::from_u128(i), Value::Integer(i as i64))
                .unwrap();
        }

        let all = db.scan("nums", ..).unwrap();
        assert_eq!(all.len(), 10);
    }

    #[test]
    fn test_document_count() {
        let (_dir, mut db) = setup();
        db.create_collection("c").unwrap();
        assert_eq!(db.document_count("c").unwrap(), 0);

        db.insert("c", Uuid::new_v4(), Value::Null).unwrap();
        assert_eq!(db.document_count("c").unwrap(), 1);
    }

    #[test]
    fn test_secondary_index_via_database() {
        let (_dir, mut db) = setup();
        db.create_collection("users").unwrap();

        let u1 = Uuid::from_u128(1);
        let u2 = Uuid::from_u128(2);

        db.insert(
            "users",
            u1,
            Value::Object(BTreeMap::from([
                ("name".into(), Value::String("Alice".into())),
                ("age".into(), Value::Integer(30)),
            ])),
        )
        .unwrap();

        db.insert(
            "users",
            u2,
            Value::Object(BTreeMap::from([
                ("name".into(), Value::String("Bob".into())),
                ("age".into(), Value::Integer(25)),
            ])),
        )
        .unwrap();

        db.create_index("users", "by_age", "age").unwrap();

        let results = db.query("users", "by_age", &Value::Integer(30)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, u1);

        let range = db
            .query_range("users", "by_age", &Value::Integer(20), &Value::Integer(31))
            .unwrap();
        assert_eq!(range.len(), 2);
    }

    #[test]
    fn test_persistence_across_reopen() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("persist");
        let key = Uuid::from_u128(99);

        {
            let mut db = Database::open(&db_path).unwrap();
            db.create_collection("data").unwrap();
            db.insert("data", key, Value::String("hello".into()))
                .unwrap();
            db.close().unwrap();
        }

        {
            let mut db = Database::open(&db_path).unwrap();
            assert_eq!(db.list_collections(), vec!["data"]);
            let val = db.get("data", &key).unwrap();
            assert_eq!(val, Some(Value::String("hello".into())));
        }
    }

    #[test]
    fn test_compact_collection() {
        let (_dir, mut db) = setup();
        db.create_collection("c").unwrap();

        for i in 0u128..50 {
            db.insert("c", Uuid::from_u128(i), Value::Integer(i as i64))
                .unwrap();
        }
        for i in 0u128..25 {
            db.delete("c", &Uuid::from_u128(i)).unwrap();
        }

        let count = db.compact("c").unwrap();
        assert_eq!(count, 25);
        assert_eq!(db.document_count("c").unwrap(), 25);
    }

    #[test]
    fn test_collection_not_found() {
        let (_dir, mut db) = setup();
        assert!(db.get("nope", &Uuid::new_v4()).is_err());
        assert!(db.insert("nope", Uuid::new_v4(), Value::Null).is_err());
    }

    #[test]
    fn test_invalid_collection_name() {
        let (_dir, mut db) = setup();
        assert!(db.create_collection("Bad-Name").is_err());
        assert!(db.create_collection("").is_err());
    }

    #[test]
    fn test_begin_read_exposes_snapshot_hlc() {
        let (_dir, mut db) = setup();
        db.create_collection("c").unwrap();
        db.insert("c", Uuid::new_v4(), Value::Integer(1)).unwrap();

        let before = db.current_hlc();
        let tx = db.begin_read();
        assert!(tx.snapshot_hlc() >= before);
    }

    #[test]
    fn test_read_tx_get_and_scan() {
        let (_dir, mut db) = setup();
        db.create_collection("c").unwrap();

        let k1 = Uuid::from_u128(1);
        let k2 = Uuid::from_u128(2);
        db.insert("c", k1, Value::Integer(10)).unwrap();
        db.insert("c", k2, Value::Integer(20)).unwrap();

        let mut tx = db.begin_read();
        assert_eq!(tx.get("c", &k1).unwrap(), Some(Value::Integer(10)));
        let rows = tx.scan("c", ..).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_read_tx_snapshot_hides_future_update() {
        let (_dir, mut db) = setup();
        db.create_collection("c").unwrap();

        let key = Uuid::from_u128(7);
        db.insert("c", key, Value::Integer(1)).unwrap();
        let snapshot = db.current_hlc();
        db.register_reader_snapshot(snapshot);

        db.update("c", &key, Value::Integer(2)).unwrap();

        assert_eq!(
            db.snapshot_get("c", &key, snapshot).unwrap(),
            Some(Value::Integer(1))
        );
        db.unregister_reader_snapshot(snapshot);
        assert_eq!(db.get("c", &key).unwrap(), Some(Value::Integer(2)));
    }

    #[test]
    fn test_snapshot_delete_preserves_pre_snapshot_visibility() {
        let (_dir, mut db) = setup();
        db.create_collection("c").unwrap();

        let key = Uuid::from_u128(8);
        db.insert("c", key, Value::Integer(10)).unwrap();

        // Simulate pre-Phase 41 data without history and ensure delete seeds
        // a baseline version for older snapshots.
        db.versions.clear();
        let snapshot = db.current_hlc();
        db.register_reader_snapshot(snapshot);
        db.delete("c", &key).unwrap();

        assert_eq!(
            db.snapshot_get("c", &key, snapshot).unwrap(),
            Some(Value::Integer(10))
        );
        db.unregister_reader_snapshot(snapshot);
        assert_eq!(db.get("c", &key).unwrap(), None);
    }

    #[test]
    fn test_delete_writes_hidden_tombstone_until_compact() {
        let (_dir, mut db) = setup();
        db.create_collection("c").unwrap();

        let key = Uuid::from_u128(1001);
        db.insert("c", key, Value::Integer(7)).unwrap();
        db.delete("c", &key).unwrap();

        // Tombstones are hidden from reads/scans.
        assert_eq!(db.get("c", &key).unwrap(), None);
        assert!(db.scan("c", ..).unwrap().is_empty());

        // Tombstone still occupies the key until compaction.
        assert_eq!(db.document_count("c").unwrap(), 1);

        db.compact("c").unwrap();
        assert_eq!(db.document_count("c").unwrap(), 0);
    }

    #[test]
    fn test_read_tx_drop_clears_reader_watermark() {
        let (_dir, mut db) = setup();
        db.create_collection("c").unwrap();
        db.insert("c", Uuid::new_v4(), Value::Integer(1)).unwrap();

        let snapshot = {
            let tx = db.begin_read();
            let s = tx.snapshot_hlc();
            assert_eq!(tx.db.reader_watermark(), Some(s));
            s
        };

        assert_eq!(db.reader_watermark(), None);
        assert!(snapshot > Hlc::ZERO);
    }

    #[test]
    fn test_gc_versions_when_last_reader_drops() {
        let (_dir, mut db) = setup();
        db.create_collection("c").unwrap();

        let key = Uuid::from_u128(11);
        db.insert("c", key, Value::Integer(1)).unwrap();
        let snapshot = db.current_hlc();
        db.register_reader_snapshot(snapshot);

        db.update("c", &key, Value::Integer(2)).unwrap();
        db.update("c", &key, Value::Integer(3)).unwrap();

        assert_eq!(
            db.snapshot_get("c", &key, snapshot).unwrap(),
            Some(Value::Integer(1))
        );
        assert!(db.debug_version_len("c", &key) >= 3);

        db.unregister_reader_snapshot(snapshot);

        assert_eq!(db.reader_watermark(), None);
        assert_eq!(db.debug_version_len("c", &key), 1);
        assert_eq!(db.get("c", &key).unwrap(), Some(Value::Integer(3)));
    }

    #[test]
    fn test_resolve_ref_simple() {
        let (_dir, mut db) = setup();
        db.create_collection("users").unwrap();
        db.create_collection("orders").unwrap();

        let user_key = Uuid::from_u128(1);
        db.insert(
            "users",
            user_key,
            Value::Object(BTreeMap::from([(
                "name".into(),
                Value::String("Alice".into()),
            )])),
        )
        .unwrap();

        // Insert an order referencing the user
        let order_key = Uuid::from_u128(2);
        db.insert(
            "orders",
            order_key,
            Value::Object(BTreeMap::from([
                ("product".into(), Value::String("Widget".into())),
                ("owner".into(), Value::Ref("users".into(), user_key)),
            ])),
        )
        .unwrap();

        // Resolve the ref
        let resolved = db.resolve_ref("users", &user_key).unwrap();
        assert!(resolved.is_some());
        let val = resolved.unwrap();
        assert_eq!(
            val.as_object().unwrap().get("name"),
            Some(&Value::String("Alice".into()))
        );
    }

    #[test]
    fn test_resolve_deep_nested_refs() {
        let (_dir, mut db) = setup();
        db.create_collection("a").unwrap();
        db.create_collection("b").unwrap();

        let key_a = Uuid::from_u128(10);
        let key_b = Uuid::from_u128(20);

        db.insert("a", key_a, Value::String("target_a".into()))
            .unwrap();
        db.insert(
            "b",
            key_b,
            Value::Object(BTreeMap::from([(
                "link".into(),
                Value::Ref("a".into(), key_a),
            )])),
        )
        .unwrap();

        // A document with a ref to b, which itself refs a
        let doc = Value::Object(BTreeMap::from([(
            "nested".into(),
            Value::Ref("b".into(), key_b),
        )]));

        let resolved = db.resolve_deep(&doc, 16).unwrap();
        // nested -> b's doc -> { link: "target_a" }
        let nested = resolved.as_object().unwrap().get("nested").unwrap();
        let link = nested.as_object().unwrap().get("link").unwrap();
        assert_eq!(link, &Value::String("target_a".into()));
    }

    #[test]
    fn test_resolve_deep_cycle_detection() {
        let (_dir, mut db) = setup();
        db.create_collection("items").unwrap();

        let key1 = Uuid::from_u128(100);
        let key2 = Uuid::from_u128(200);

        // key1 -> refs key2, key2 -> refs key1 (cycle)
        db.insert("items", key1, Value::Ref("items".into(), key2))
            .unwrap();
        db.insert("items", key2, Value::Ref("items".into(), key1))
            .unwrap();

        let doc = Value::Ref("items".into(), key1);
        let result = db.resolve_deep(&doc, 16);
        assert!(matches!(result, Err(GrumpyError::CyclicReference)));
    }

    #[test]
    fn test_resolve_ref_missing_target() {
        let (_dir, mut db) = setup();
        db.create_collection("items").unwrap();

        let missing = Uuid::from_u128(999);
        let result = db.resolve_ref("items", &missing).unwrap();
        assert!(result.is_none());
    }
}
