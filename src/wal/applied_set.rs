//! Per-origin "highest applied HLC" tracker.
//!
//! Persisted at `<data_dir>/_replication/state.json`. In v5 single-writer
//! deployments the file is written but never gates writes (since the
//! origin equals self). Phase 40e replication apply will start using it
//! to drop duplicate WAL records on replay.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{GrumpyError, Result};
use crate::wal::hlc::Hlc;

/// Subdirectory (under the database data directory) where replication
/// state is persisted.
pub const REPLICATION_DIR: &str = "_replication";
/// File name for the persisted [`AppliedSet`] state.
pub const STATE_FILE: &str = "state.json";

/// Outcome of an [`AppliedSet::observe`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveOutcome {
    /// The `(origin, hlc)` pair is strictly newer than what we had
    /// recorded — the caller should apply the record.
    New,
    /// We have already observed an HLC `>= hlc` from this origin —
    /// the caller should skip the record (idempotent replay).
    AlreadyApplied,
}

/// Per-origin "highest applied HLC" tracker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedSet {
    /// `node_id` (as a UUID hyphenated string) → highest hlc applied
    /// from that origin.
    pub last_applied: BTreeMap<String, u64>,
    /// On-disk schema version of this struct (currently `1`).
    pub schema_version: u32,
}

impl Default for AppliedSet {
    fn default() -> Self {
        Self {
            last_applied: BTreeMap::new(),
            schema_version: 1,
        }
    }
}

impl AppliedSet {
    /// Loads the [`AppliedSet`] from `<data_dir>/_replication/state.json`,
    /// returning [`AppliedSet::default()`] when the file is missing.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let p = Self::state_path(data_dir);
        if !p.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(&p)?;
        let parsed: AppliedSet = serde_json::from_slice(&bytes)
            .map_err(|e| GrumpyError::Corruption(format!("invalid replication state.json: {e}")))?;
        Ok(parsed)
    }

    /// Persists the [`AppliedSet`] to `<data_dir>/_replication/state.json`.
    /// Creates the directory if it doesn't exist. The write is
    /// best-effort atomic via `tmp + rename`.
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let dir = data_dir.join(REPLICATION_DIR);
        std::fs::create_dir_all(&dir)?;
        let final_path = dir.join(STATE_FILE);
        let tmp_path = dir.join(format!("{STATE_FILE}.tmp"));
        let body = serde_json::to_vec_pretty(self)
            .map_err(|e| GrumpyError::Corruption(format!("serialize applied set: {e}")))?;
        std::fs::write(&tmp_path, body)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    /// Records that we have applied an HLC from a given origin. Returns
    /// `New` if `hlc` advanced our high-water mark, `AlreadyApplied`
    /// otherwise (idempotent replay).
    pub fn observe(&mut self, origin: u128, hlc: Hlc) -> ObserveOutcome {
        let key = Uuid::from_u128(origin).hyphenated().to_string();
        let entry = self.last_applied.entry(key).or_insert(0);
        if hlc.0 > *entry {
            *entry = hlc.0;
            ObserveOutcome::New
        } else {
            ObserveOutcome::AlreadyApplied
        }
    }

    /// Returns the highest-applied HLC for `origin`, or `Hlc::ZERO`
    /// if we have never observed it.
    pub fn high_water(&self, origin: u128) -> Hlc {
        let key = Uuid::from_u128(origin).hyphenated().to_string();
        Hlc::from_packed(self.last_applied.get(&key).copied().unwrap_or(0))
    }

    fn state_path(data_dir: &Path) -> PathBuf {
        data_dir.join(REPLICATION_DIR).join(STATE_FILE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_applied_set_default_load_returns_empty() {
        let dir = TempDir::new().unwrap();
        let s = AppliedSet::load(dir.path()).unwrap();
        assert!(s.last_applied.is_empty());
        assert_eq!(s.schema_version, 1);
    }

    #[test]
    fn test_applied_set_observe_advances_then_dedup() {
        let mut s = AppliedSet::default();
        let origin = 0xdeadbeef_u128;
        assert_eq!(s.observe(origin, Hlc::pack(10, 0)), ObserveOutcome::New);
        assert_eq!(
            s.observe(origin, Hlc::pack(10, 0)),
            ObserveOutcome::AlreadyApplied
        );
        assert_eq!(
            s.observe(origin, Hlc::pack(5, 0)),
            ObserveOutcome::AlreadyApplied
        );
        assert_eq!(s.observe(origin, Hlc::pack(11, 0)), ObserveOutcome::New);
        assert_eq!(s.high_water(origin), Hlc::pack(11, 0));
    }

    #[test]
    fn test_applied_set_save_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let mut s = AppliedSet::default();
        s.observe(1, Hlc::pack(100, 1));
        s.observe(2, Hlc::pack(200, 2));
        s.save(dir.path()).unwrap();

        let s2 = AppliedSet::load(dir.path()).unwrap();
        assert_eq!(s2.high_water(1), Hlc::pack(100, 1));
        assert_eq!(s2.high_water(2), Hlc::pack(200, 2));
        assert_eq!(s2.high_water(99), Hlc::ZERO);
    }

    #[test]
    fn test_applied_set_load_corruption_is_reported() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join(REPLICATION_DIR);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join(STATE_FILE), b"{not valid json").unwrap();
        let err = AppliedSet::load(dir.path()).unwrap_err();
        assert!(matches!(err, GrumpyError::Corruption(_)));
    }
}
