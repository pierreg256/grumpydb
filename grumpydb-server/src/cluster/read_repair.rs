//! Background read-repair intent queue (Phase 47 scaffolding).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::coordinator::Coordinator;

const CLUSTER_DIR: &str = "_cluster";
const REPAIR_FILE: &str = "read_repair.jsonl";

/// Durable intent describing a key to repair in background.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadRepairIntent {
    pub created_at_unix: u64,
    pub tenant: String,
    pub database: String,
    pub collection: String,
    pub key: String,
    pub target_node_id: String,
    pub value_json: String,
    pub reason: String,
}

/// File-backed queue for read-repair intents.
#[derive(Debug, Clone)]
pub struct ReadRepairStore {
    file_path: PathBuf,
}

impl ReadRepairStore {
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let dir = data_dir.join(CLUSTER_DIR);
        fs::create_dir_all(&dir)?;
        Ok(Self {
            file_path: dir.join(REPAIR_FILE),
        })
    }

    pub fn append(&self, intent: &ReadRepairIntent) -> io::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)?;
        let line = serde_json::to_string(intent)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.flush()
    }

    pub fn backlog_len(&self) -> io::Result<usize> {
        if !self.file_path.exists() {
            return Ok(0);
        }
        let f = File::open(&self.file_path)?;
        let reader = BufReader::new(f);
        let mut count = 0usize;
        for line in reader.lines() {
            if !line?.trim().is_empty() {
                count += 1;
            }
        }
        Ok(count)
    }

    pub fn drain(&self, limit: usize) -> io::Result<Vec<ReadRepairIntent>> {
        if limit == 0 || !self.file_path.exists() {
            return Ok(Vec::new());
        }

        let f = File::open(&self.file_path)?;
        let reader = BufReader::new(f);
        let mut drained = Vec::new();
        let mut keep = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if drained.len() < limit {
                let intent: ReadRepairIntent = serde_json::from_str(&line)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                drained.push(intent);
            } else {
                keep.push(line);
            }
        }

        if keep.is_empty() {
            fs::remove_file(&self.file_path)?;
            return Ok(drained);
        }

        let mut wf = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.file_path)?;
        for line in keep {
            wf.write_all(line.as_bytes())?;
            wf.write_all(b"\n")?;
        }
        wf.flush()?;
        Ok(drained)
    }
}

/// Spawn a lightweight background worker that drains read-repair intents and
/// records observability metrics.
pub fn spawn_worker(store: Arc<ReadRepairStore>, coordinator: Arc<Coordinator>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            let intents = match store.drain(64) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "read-repair drain failed");
                    continue;
                }
            };
            if intents.is_empty() {
                continue;
            }

            for intent in intents {
                let result = coordinator
                    .repair_peer_value(
                        &intent.target_node_id,
                        &intent.tenant,
                        &intent.database,
                        &intent.collection,
                        &intent.key,
                        &intent.value_json,
                    )
                    .await;
                match result {
                    Ok(()) => {
                        metrics::counter!(
                            "grumpydb_read_repair_intents_processed_total",
                            "result" => "repaired"
                        )
                        .increment(1);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, target = %intent.target_node_id, key = %intent.key, "read-repair replay failed, re-enqueueing");
                        let _ = store.append(&intent);
                        metrics::counter!(
                            "grumpydb_read_repair_intents_processed_total",
                            "result" => "retry_enqueued"
                        )
                        .increment(1);
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

    fn sample_intent(key: &str) -> ReadRepairIntent {
        ReadRepairIntent {
            created_at_unix: 1,
            tenant: "t".to_string(),
            database: "db".to_string(),
            collection: "users".to_string(),
            key: key.to_string(),
            target_node_id: "node-b".to_string(),
            value_json: "{\"name\":\"alice\"}".to_string(),
            reason: "quorum-partial".to_string(),
        }
    }

    #[test]
    fn test_read_repair_store_append_backlog_and_drain() {
        let tmp = TempDir::new().expect("tmp");
        let store = ReadRepairStore::open(tmp.path()).expect("open");
        store.append(&sample_intent("k1")).expect("append 1");
        store.append(&sample_intent("k2")).expect("append 2");
        assert_eq!(store.backlog_len().expect("len"), 2);

        let first = store.drain(1).expect("drain one");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].key, "k1");
        assert_eq!(store.backlog_len().expect("len after one"), 1);

        let second = store.drain(8).expect("drain all");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].key, "k2");
        assert_eq!(store.backlog_len().expect("len after all"), 0);
    }
}
