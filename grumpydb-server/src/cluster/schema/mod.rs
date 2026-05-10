//! Cluster-wide schema state (Phase 44a).
//!
//! Holds the in-memory representation of the **eventually-consistent
//! distributed schema** — currently only secondary index definitions.
//! Pairs with the on-disk append-only [`log::SchemaLog`] which is the
//! source of truth on restart.
//!
//! ## Design overview
//!
//! See `docs/SCHEMA_GOSSIP.md` for the complete design rationale. The
//! short version:
//!
//! - Every applied DDL operation (`CreateIndex`, `DropIndex`) is
//!   stamped with a monotonically-increasing per-cluster
//!   [`SchemaState::version`] (a `u64`) and an HLC timestamp.
//! - Conflicting CREATEs (same `(tenant, db, coll, name)` with
//!   different `field_path`) are resolved by **last-writer-wins on
//!   HLC** (ties broken by appending node UUID lexicographic order at
//!   a higher layer — handled in 44b when we wire gossip).
//! - DROPs leave a **tombstone** (kept forever in 44a; compaction is
//!   out of scope) so a late-arriving CREATE from a slow peer cannot
//!   resurrect a dropped index.
//!
//! ## Tranche 44a scope
//!
//! This module is **purely local**: there is no networking, no gossip
//! integration, and no impact on the existing `replicate_index_ddl`
//! path. It provides the data model and persistence; the integration
//! into `Coordinator` and the existing DDL handler comes in 44c.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub mod bootstrap;
pub mod log;
pub mod materializer;

#[cfg(test)]
mod tests;

/// Logical key of one schema entry.
///
/// The full path is `tenant / database / collection / index_name`. We
/// flatten it into a tuple-struct rather than nested maps because LWW
/// conflict resolution is per-key, not per-collection.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IndexKey {
    pub tenant: String,
    pub database: String,
    pub collection: String,
    pub index_name: String,
}

/// Value side of one schema entry.
///
/// `tombstone = true` means the index has been dropped: the
/// [`SchemaState`] keeps the entry to suppress late re-creations
/// arriving via gossip from a node that hadn't yet seen the DROP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Dot-separated field path the index is built on.
    pub field_path: String,
    /// HLC at which this entry's last-modification (CREATE or DROP)
    /// was observed.
    pub last_modified_hlc: u64,
    /// `true` once a DROP has been applied. `false` for live indexes.
    pub tombstone: bool,
}

/// One operation as recorded in the [`log::SchemaLog`] and exchanged
/// over future gossip pulls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SchemaOp {
    /// Create a secondary index.
    CreateIndex { key: IndexKey, field_path: String },
    /// Drop a previously-created secondary index.
    DropIndex { key: IndexKey },
}

impl SchemaOp {
    pub fn key(&self) -> &IndexKey {
        match self {
            SchemaOp::CreateIndex { key, .. } | SchemaOp::DropIndex { key } => key,
        }
    }
}

/// One persisted log record: an operation stamped with the cluster
/// `version` it advanced to and the HLC at which it was applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaLogEntry {
    /// Cluster-wide monotonic version this op produced.
    pub version: u64,
    /// HLC at apply time on the originating node.
    pub hlc: u64,
    /// The operation itself.
    pub op: SchemaOp,
}

/// In-memory view of the cluster schema as known to this node.
#[derive(Debug, Clone, Default)]
pub struct SchemaState {
    version: u64,
    indexes: BTreeMap<IndexKey, IndexEntry>,
    /// Historical log of every entry applied to this state, keyed by
    /// the entry's `version`. Used by [`SchemaState::entries_since`]
    /// to serve gossip pull requests without re-reading
    /// `schema.log`. Populated by [`SchemaState::apply`] for every
    /// non-`Duplicate` outcome.
    applied_log: BTreeMap<u64, SchemaLogEntry>,
}

/// Outcome of applying one entry.
///
/// Returned by [`SchemaState::apply`] so callers can decide whether to
/// trigger a materialization job (build / drop the on-disk index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The entry was applied and is *new or updated*. The caller
    /// should usually trigger a materialization job.
    Applied,
    /// The entry was ignored because a strictly-newer HLC was already
    /// present for the same key (LWW).
    Stale,
    /// The entry was ignored because its version was `<= self.version`
    /// **and** its HLC matched what we already knew (idempotent
    /// replay).
    Duplicate,
}

impl SchemaState {
    /// Build an empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current cluster-wide schema version known by this node.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Number of entries (live + tombstoned) currently held.
    pub fn len(&self) -> usize {
        self.indexes.len()
    }

    /// `true` if no entry has ever been applied.
    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty()
    }

    /// Returns the live (non-tombstoned) entry for the key, if any.
    pub fn get(&self, key: &IndexKey) -> Option<&IndexEntry> {
        self.indexes.get(key).filter(|e| !e.tombstone)
    }

    /// Returns the entry whether it's a tombstone or not.
    pub fn get_any(&self, key: &IndexKey) -> Option<&IndexEntry> {
        self.indexes.get(key)
    }

    /// Iterate over live (non-tombstoned) entries.
    pub fn live_entries(&self) -> impl Iterator<Item = (&IndexKey, &IndexEntry)> {
        self.indexes.iter().filter(|(_, e)| !e.tombstone)
    }

    /// Iterate over all entries (including tombstones).
    pub fn all_entries(&self) -> impl Iterator<Item = (&IndexKey, &IndexEntry)> {
        self.indexes.iter()
    }

    /// Apply one [`SchemaLogEntry`] to the state.
    ///
    /// `version` is advanced to `max(self.version, entry.version)`.
    /// LWW resolution is performed against the existing entry's
    /// `last_modified_hlc`.
    ///
    /// Side effect: when the outcome is [`ApplyOutcome::Applied`] the
    /// entry is appended to the in-memory `applied_log` so future
    /// gossip pulls can serve it via [`Self::entries_since`].
    pub fn apply(&mut self, entry: &SchemaLogEntry) -> ApplyOutcome {
        let key = entry.op.key().clone();
        let outcome = match self.indexes.get(&key) {
            None => {
                self.insert_or_replace(&key, entry);
                ApplyOutcome::Applied
            }
            Some(existing) => {
                if entry.hlc > existing.last_modified_hlc {
                    self.insert_or_replace(&key, entry);
                    ApplyOutcome::Applied
                } else if entry.hlc == existing.last_modified_hlc
                    && entry_matches(existing, &entry.op)
                {
                    ApplyOutcome::Duplicate
                } else {
                    ApplyOutcome::Stale
                }
            }
        };

        if entry.version > self.version {
            self.version = entry.version;
        }
        if outcome == ApplyOutcome::Applied {
            self.applied_log.insert(entry.version, entry.clone());
        }
        outcome
    }

    /// Return every applied [`SchemaLogEntry`] whose version is
    /// strictly greater than `since`, ordered by ascending version.
    ///
    /// This is the read side of the gossip pull RPC: a peer that
    /// observes our `schema_version > theirs` calls us with
    /// `since = their_version` and gets the diff to catch up.
    pub fn entries_since(&self, since: u64) -> Vec<SchemaLogEntry> {
        self.applied_log
            .range((std::ops::Bound::Excluded(since), std::ops::Bound::Unbounded))
            .map(|(_, e)| e.clone())
            .collect()
    }

    fn insert_or_replace(&mut self, key: &IndexKey, entry: &SchemaLogEntry) {
        let new_entry = match &entry.op {
            SchemaOp::CreateIndex { field_path, .. } => IndexEntry {
                field_path: field_path.clone(),
                last_modified_hlc: entry.hlc,
                tombstone: false,
            },
            SchemaOp::DropIndex { .. } => IndexEntry {
                // Preserve the field_path from the existing live entry
                // when transitioning to tombstone, so observers can still
                // see what was dropped. If we have no prior entry, the
                // field_path is unknown; we record an empty string.
                field_path: self
                    .indexes
                    .get(key)
                    .map(|e| e.field_path.clone())
                    .unwrap_or_default(),
                last_modified_hlc: entry.hlc,
                tombstone: true,
            },
        };
        self.indexes.insert(key.clone(), new_entry);
    }

    /// Allocate the next version (used by the local CREATE/DROP paths
    /// to stamp new operations originating on this node).
    pub fn next_version(&mut self) -> u64 {
        self.version += 1;
        self.version
    }

    /// Force-set the version. Used by the bootstrap routine when
    /// rebuilding from an existing data dir.
    //
    // Wired into `Coordinator::new()` and the gossip pull path in
    // tranches 44b–c. Kept available now to avoid yo-yo additions.
    #[allow(dead_code)]
    pub(crate) fn set_version(&mut self, v: u64) {
        self.version = v;
    }
}

fn entry_matches(existing: &IndexEntry, op: &SchemaOp) -> bool {
    match op {
        SchemaOp::CreateIndex { field_path, .. } => {
            !existing.tombstone && existing.field_path == *field_path
        }
        SchemaOp::DropIndex { .. } => existing.tombstone,
    }
}
