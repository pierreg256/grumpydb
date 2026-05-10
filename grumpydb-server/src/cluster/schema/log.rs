//! Append-only JSONL persistence for the schema log.
//!
//! ## On-disk layout
//!
//! ```text
//! <data_dir>/_cluster/schema.log
//! ```
//!
//! One [`SchemaLogEntry`] per line, JSON-encoded. The file is opened
//! in `O_APPEND` mode for writes and never rewritten. Compaction is
//! deliberately out of scope for tranche 44a — at ~200 bytes per
//! entry, even thousands of operations stay well under 1 MB.
//!
//! On startup, [`SchemaLog::open`] reads the file from beginning to
//! end and replays every record into a [`SchemaState`]. A corrupted
//! line aborts the load (the operator must investigate rather than
//! silently lose schema state).

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

use super::{SchemaLogEntry, SchemaState};

/// Subdirectory holding cluster state (mirrors what `NodeIdentity`
/// uses today).
const CLUSTER_DIR: &str = "_cluster";
/// Filename of the schema log.
const SCHEMA_LOG_FILE: &str = "schema.log";

/// Errors returned by [`SchemaLog`] APIs.
#[derive(thiserror::Error, Debug)]
pub enum SchemaLogError {
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    /// One line of `schema.log` could not be parsed. The line number
    /// is 1-based.
    #[error("malformed schema.log at line {line}: {message}")]
    Malformed { line: usize, message: String },
}

/// Append-only schema log handle.
///
/// Cheap to clone (`Arc<Mutex<File>>`-equivalent under the hood). All
/// writes are serialized through an internal `Mutex`.
#[derive(Debug)]
pub struct SchemaLog {
    path: PathBuf,
    file: Mutex<File>,
}

impl SchemaLog {
    /// Path to `<data_dir>/_cluster/schema.log`.
    pub fn path_for(data_dir: &Path) -> PathBuf {
        data_dir.join(CLUSTER_DIR).join(SCHEMA_LOG_FILE)
    }

    /// Open (creating if needed) the schema log under `data_dir`,
    /// replaying every record into a fresh [`SchemaState`].
    ///
    /// Returns the writer handle plus the rebuilt state.
    pub fn open(data_dir: &Path) -> Result<(Self, SchemaState), SchemaLogError> {
        let path = Self::path_for(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Replay first (read-only).
        let state = if path.exists() {
            replay_into_state(&path)?
        } else {
            SchemaState::new()
        };

        // Then open the writer in append mode.
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
        }

        Ok((
            Self {
                path,
                file: Mutex::new(file),
            },
            state,
        ))
    }

    /// Append one entry to the log and `flush` it.
    ///
    /// Note: we deliberately do not `fsync` per-write in 44a (the
    /// engine's WAL already provides crash durability for data; a lost
    /// schema entry is recoverable via gossip pull from any other
    /// up-to-date peer). 44d may revisit if benchmarks show value.
    pub fn append(&self, entry: &SchemaLogEntry) -> Result<(), SchemaLogError> {
        let line = serde_json::to_string(entry).map_err(|e| SchemaLogError::Malformed {
            line: 0,
            message: e.to_string(),
        })?;
        let mut f = self.file.lock();
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.flush()?;
        Ok(())
    }

    /// Append a batch of entries atomically with respect to other
    /// concurrent appenders (single mutex acquisition).
    pub fn append_batch(&self, entries: &[SchemaLogEntry]) -> Result<(), SchemaLogError> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut buf = Vec::with_capacity(entries.len() * 256);
        for entry in entries {
            let line = serde_json::to_string(entry).map_err(|e| SchemaLogError::Malformed {
                line: 0,
                message: e.to_string(),
            })?;
            buf.extend_from_slice(line.as_bytes());
            buf.push(b'\n');
        }
        let mut f = self.file.lock();
        f.write_all(&buf)?;
        f.flush()?;
        Ok(())
    }

    /// Filesystem path of the log (informational; tests + diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read every line of `schema.log` and return the rebuilt state.
fn replay_into_state(path: &Path) -> Result<SchemaState, SchemaLogError> {
    let f = File::open(path)?;
    let reader = BufReader::new(f);
    let mut state = SchemaState::new();
    for (idx, line) in reader.lines().enumerate() {
        let line_no = idx + 1;
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: SchemaLogEntry =
            serde_json::from_str(&line).map_err(|e| SchemaLogError::Malformed {
                line: line_no,
                message: e.to_string(),
            })?;
        // We deliberately ignore the per-apply outcome here: the log is
        // already a total order, every record is applied unconditionally.
        // LWW resolution is handled at apply-time for entries received
        // over the wire, not for replay.
        state.apply(&entry);
    }
    Ok(state)
}
