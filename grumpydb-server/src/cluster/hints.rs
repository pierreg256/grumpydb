//! Durable hinted-handoff backlog storage (Phase 48 groundwork).
//!
//! Hints are stored as JSON lines under:
//! `<data_dir>/_cluster/hints/<target_node_id>.jsonl`
//! so each target replica has an isolated backlog file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::coordinator::Coordinator;

const CLUSTER_DIR: &str = "_cluster";
const HINTS_DIR: &str = "hints";

/// Hint operation replayed on a recovering replica.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HintOperation {
    Upsert { value_json: String },
    Delete,
}

/// Durable hint payload for a single write that could not be sent to a peer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HintRecord {
    /// UNIX timestamp when the hint was recorded.
    pub created_at_unix: u64,
    /// Tenant carrying the write.
    #[serde(default = "default_tenant")]
    pub tenant: String,
    /// Database carrying the write.
    pub database: String,
    /// Collection carrying the write.
    pub collection: String,
    /// Key (UUID string) affected by the write.
    pub key: String,
    /// Logical write operation to replay.
    #[serde(default)]
    pub operation: Option<HintOperation>,
    /// Legacy payload field preserved for backward-compatible replay.
    #[serde(default)]
    pub payload_json: Option<String>,
}

fn default_tenant() -> String {
    "_default".to_string()
}

impl HintRecord {
    /// Resolve operation from the modern schema or legacy payload fallback.
    pub fn resolved_operation(&self) -> HintOperation {
        if let Some(op) = &self.operation {
            return op.clone();
        }
        match self.payload_json.as_deref() {
            Some("{\"$delete\":true}") => HintOperation::Delete,
            Some(value_json) => HintOperation::Upsert {
                value_json: value_json.to_string(),
            },
            None => HintOperation::Upsert {
                value_json: "null".to_string(),
            },
        }
    }
}

/// File-backed hint backlog partitioned by target node id.
#[derive(Debug, Clone)]
pub struct HintStore {
    base_dir: PathBuf,
}

impl HintStore {
    /// Build a store rooted at `<data_dir>/_cluster/hints`.
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let base_dir = data_dir.join(CLUSTER_DIR).join(HINTS_DIR);
        fs::create_dir_all(&base_dir)?;
        Ok(Self { base_dir })
    }

    /// Append a durable hint for a target replica.
    pub fn append(&self, target_node_id: &str, hint: &HintRecord) -> io::Result<()> {
        let path = self.node_file(target_node_id);
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        let line = serde_json::to_string(hint)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.flush()
    }

    /// Return current backlog length for a target node.
    pub fn backlog_len(&self, target_node_id: &str) -> io::Result<usize> {
        let path = self.node_file(target_node_id);
        if !path.exists() {
            return Ok(0);
        }

        let f = File::open(path)?;
        let reader = BufReader::new(f);
        let mut count = 0usize;
        for line in reader.lines() {
            if !line?.trim().is_empty() {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Drain up to `limit` hints for the target node and persist the remainder.
    pub fn drain_for_node(
        &self,
        target_node_id: &str,
        limit: usize,
    ) -> io::Result<Vec<HintRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let path = self.node_file(target_node_id);
        if !path.exists() {
            return Ok(Vec::new());
        }

        let f = File::open(&path)?;
        let reader = BufReader::new(f);
        let mut drained = Vec::new();
        let mut keep = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if drained.len() < limit {
                let hint: HintRecord = serde_json::from_str(&line)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                drained.push(hint);
            } else {
                keep.push(line);
            }
        }

        if keep.is_empty() {
            fs::remove_file(path)?;
            return Ok(drained);
        }

        let mut wf = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        for line in keep {
            wf.write_all(line.as_bytes())?;
            wf.write_all(b"\n")?;
        }
        wf.flush()?;
        Ok(drained)
    }

    /// List node ids that currently have a hint backlog file.
    pub fn list_targets(&self) -> io::Result<Vec<String>> {
        let mut targets = Vec::new();
        for entry in fs::read_dir(&self.base_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(stripped) = name.strip_suffix(".jsonl") {
                targets.push(stripped.to_string());
            }
        }
        targets.sort();
        Ok(targets)
    }

    fn node_file(&self, target_node_id: &str) -> PathBuf {
        self.base_dir.join(format!("{target_node_id}.jsonl"))
    }
}

/// Spawn hinted-handoff replay orchestration.
pub fn spawn_worker(store: Arc<HintStore>, coordinator: Arc<Coordinator>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            let targets = match store.list_targets() {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "hint worker failed to list targets");
                    continue;
                }
            };

            for target in targets {
                if !coordinator.peer_is_live(&target) {
                    continue;
                }

                let hints = match store.drain_for_node(&target, 128) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, target = %target, "hint worker failed to drain backlog");
                        continue;
                    }
                };

                if hints.is_empty() {
                    continue;
                }

                for hint in hints {
                    match coordinator.replay_hint_to_peer(&target, &hint).await {
                        Ok(()) => {
                            metrics::counter!("grumpydb_hints_replayed_total").increment(1);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, target = %target, key = %hint.key, "hint replay failed, re-enqueueing");
                            let _ = store.append(&target, &hint);
                            metrics::counter!("grumpydb_hints_replay_retries_total").increment(1);
                        }
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_hint(key: &str) -> HintRecord {
        HintRecord {
            created_at_unix: 1,
            tenant: "t".to_string(),
            database: "db".to_string(),
            collection: "users".to_string(),
            key: key.to_string(),
            operation: Some(HintOperation::Upsert {
                value_json: "{\"name\":\"alice\"}".to_string(),
            }),
            payload_json: None,
        }
    }

    #[test]
    fn test_hint_store_append_and_backlog_len() {
        let tmp = TempDir::new().expect("tmp");
        let store = HintStore::open(tmp.path()).expect("open");
        store
            .append("node-b", &sample_hint("k1"))
            .expect("append 1");
        store
            .append("node-b", &sample_hint("k2"))
            .expect("append 2");

        assert_eq!(store.backlog_len("node-b").expect("len"), 2);
        assert_eq!(store.backlog_len("node-c").expect("len empty"), 0);
    }

    #[test]
    fn test_hint_store_drain_for_node_keeps_tail() {
        let tmp = TempDir::new().expect("tmp");
        let store = HintStore::open(tmp.path()).expect("open");
        store
            .append("node-b", &sample_hint("k1"))
            .expect("append 1");
        store
            .append("node-b", &sample_hint("k2"))
            .expect("append 2");
        store
            .append("node-b", &sample_hint("k3"))
            .expect("append 3");

        let drained = store.drain_for_node("node-b", 2).expect("drain");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].key, "k1");
        assert_eq!(drained[1].key, "k2");
        assert_eq!(store.backlog_len("node-b").expect("len"), 1);

        let drained_last = store.drain_for_node("node-b", 10).expect("drain all");
        assert_eq!(drained_last.len(), 1);
        assert_eq!(drained_last[0].key, "k3");
        assert_eq!(store.backlog_len("node-b").expect("len empty"), 0);
    }

    #[test]
    fn test_hint_store_lists_targets() {
        let tmp = TempDir::new().expect("tmp");
        let store = HintStore::open(tmp.path()).expect("open");
        store
            .append("node-c", &sample_hint("k1"))
            .expect("append c");
        store
            .append("node-a", &sample_hint("k2"))
            .expect("append a");
        let targets = store.list_targets().expect("targets");
        assert_eq!(targets, vec!["node-a".to_string(), "node-c".to_string()]);
    }

    #[test]
    fn test_hint_record_resolved_operation_prefers_modern_operation() {
        let hint = HintRecord {
            created_at_unix: 1,
            tenant: "t".to_string(),
            database: "db".to_string(),
            collection: "c".to_string(),
            key: "k".to_string(),
            operation: Some(HintOperation::Delete),
            payload_json: Some("{\"x\":1}".to_string()),
        };
        assert_eq!(hint.resolved_operation(), HintOperation::Delete);
    }

    #[test]
    fn test_hint_record_resolved_operation_legacy_delete_payload() {
        let hint = HintRecord {
            created_at_unix: 1,
            tenant: "t".to_string(),
            database: "db".to_string(),
            collection: "c".to_string(),
            key: "k".to_string(),
            operation: None,
            payload_json: Some("{\"$delete\":true}".to_string()),
        };
        assert_eq!(hint.resolved_operation(), HintOperation::Delete);
    }

    #[test]
    fn test_hint_record_resolved_operation_legacy_upsert_payload() {
        let hint = HintRecord {
            created_at_unix: 1,
            tenant: "t".to_string(),
            database: "db".to_string(),
            collection: "c".to_string(),
            key: "k".to_string(),
            operation: None,
            payload_json: Some("{\"name\":\"alice\"}".to_string()),
        };
        assert_eq!(
            hint.resolved_operation(),
            HintOperation::Upsert {
                value_json: "{\"name\":\"alice\"}".to_string()
            }
        );
    }

    #[test]
    fn test_hint_record_deserialize_legacy_defaults_tenant_and_operation() {
        let raw = r#"{"created_at_unix":1,"database":"db","collection":"c","key":"k","payload_json":"{\"name\":\"alice\"}"}"#;
        let hint: HintRecord = serde_json::from_str(raw).expect("deserialize legacy hint");
        assert_eq!(hint.tenant, "_default");
        assert!(hint.operation.is_none());
        assert_eq!(
            hint.resolved_operation(),
            HintOperation::Upsert {
                value_json: "{\"name\":\"alice\"}".to_string()
            }
        );
    }

    #[test]
    fn test_hint_store_persists_backlog_across_reopen() {
        let tmp = TempDir::new().expect("tmp");
        {
            let store = HintStore::open(tmp.path()).expect("open");
            store
                .append("node-a", &sample_hint("k1"))
                .expect("append 1");
            store
                .append("node-a", &sample_hint("k2"))
                .expect("append 2");
            assert_eq!(store.backlog_len("node-a").expect("len before reopen"), 2);
        }

        let store = HintStore::open(tmp.path()).expect("reopen");
        assert_eq!(store.backlog_len("node-a").expect("len after reopen"), 2);
        let drained = store.drain_for_node("node-a", 10).expect("drain");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].key, "k1");
        assert_eq!(drained[1].key, "k2");
    }

    #[test]
    fn test_hint_store_drain_zero_preserves_backlog() {
        let tmp = TempDir::new().expect("tmp");
        let store = HintStore::open(tmp.path()).expect("open");
        store.append("node-a", &sample_hint("k1")).expect("append");
        let drained = store
            .drain_for_node("node-a", 0)
            .expect("drain with zero limit");
        assert!(drained.is_empty());
        assert_eq!(store.backlog_len("node-a").expect("len"), 1);
    }
}
