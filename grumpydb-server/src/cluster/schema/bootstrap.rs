//! Bootstrap the schema log from an existing data dir on first start.
//!
//! When a node is upgraded from a pre-44a binary, its data dir already
//! contains materialized indexes (one `idx_<name>.idx` file per index
//! per collection) but no `_cluster/schema.log`. To make the upgrade
//! zero-touch, we walk the data dir on first start and synthesize one
//! [`SchemaLogEntry`] per discovered index.
//!
//! ## On-disk layout (today)
//!
//! ```text
//! <data_dir>/
//!   <tenant>/
//!     <database>/
//!       <collection>/
//!         data.db
//!         primary.idx
//!         idx_<name>.idx
//! ```
//!
//! Tenants are siblings of the engine-internal directories
//! `_cluster`, `_auth`, `_system`'s reserved subdirs, etc. We use the
//! presence of `<collection>/data.db` as the "this is a real
//! collection" marker (same heuristic as `Database::open`).
//!
//! ## Synthesised entries
//!
//! - One [`SchemaOp::CreateIndex`] per discovered `idx_<name>.idx`.
//! - `version` is allocated sequentially starting at 1.
//! - `hlc = 0` so any incoming gossip with a real HLC always wins
//!   (LWW). Operationally this means: if a peer simultaneously runs a
//!   real CREATE INDEX on the same key, the peer's HLC is non-zero
//!   and supersedes our synthesized record — exactly what we want.
//!
//! ## Idempotency
//!
//! [`bootstrap_from_data_dir`] is a **no-op** when `schema.log`
//! already exists. We never overwrite an existing log.

use std::collections::BTreeSet;
use std::path::Path;

use super::log::{SchemaLog, SchemaLogError};
use super::{IndexKey, SchemaLogEntry, SchemaOp, SchemaState};

/// Names of subdirectories under `<data_dir>` that are NOT tenants.
const ENGINE_RESERVED_DIRS: &[&str] = &["_cluster", "_auth"];

/// Result of [`bootstrap_from_data_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapReport {
    /// Number of synthesized log entries.
    pub entries_written: usize,
    /// `true` if the log already existed and bootstrap was a no-op.
    pub already_initialized: bool,
}

/// If `<data_dir>/_cluster/schema.log` does not yet exist, walk
/// `<data_dir>` and write one CREATE INDEX entry per discovered
/// `idx_<name>.idx` file. Returns a report describing what happened.
///
/// On error, the (possibly partial) log on disk is left as-is — the
/// caller should treat any error as fatal at startup time.
pub fn bootstrap_from_data_dir(data_dir: &Path) -> Result<BootstrapReport, SchemaLogError> {
    let log_path = SchemaLog::path_for(data_dir);
    if log_path.exists() {
        return Ok(BootstrapReport {
            entries_written: 0,
            already_initialized: true,
        });
    }

    let discovered = discover_indexes(data_dir)?;

    // Open the log (creates the file). Then append the synthesized
    // entries as a single batch.
    let (log, _empty_state) = SchemaLog::open(data_dir)?;

    let entries: Vec<SchemaLogEntry> = discovered
        .into_iter()
        .enumerate()
        .map(|(i, (key, field_path))| SchemaLogEntry {
            version: (i + 1) as u64,
            hlc: 0,
            op: SchemaOp::CreateIndex { key, field_path },
        })
        .collect();
    let entries_written = entries.len();
    log.append_batch(&entries)?;

    Ok(BootstrapReport {
        entries_written,
        already_initialized: false,
    })
}

/// Walk the data dir and return discovered indexes, sorted for
/// deterministic version assignment.
///
/// `field_path` is **NOT** recoverable from the on-disk filename
/// (`idx_<name>.idx`) alone — the field path is stored inside the
/// index file's header (or in collection metadata). For the bootstrap
/// path we use the index *name* as a placeholder field_path, on the
/// assumption that operators conventionally use names like `by_email`
/// or `name`. The field_path is only used by the materializer at the
/// CREATE step to actually know which document field to index, but
/// for bootstrap the index file already exists, so the field_path is
/// effectively informational. **Any incoming gossip CREATE for the
/// same key will overwrite this with the real field_path** because
/// its HLC will be > 0.
///
/// In tranche 44b/44c we'll plumb through the real field_path by
/// adding a `field_path()` accessor on `Collection` /
/// `SecondaryIndex`. For 44a we keep the bootstrap intentionally
/// minimal.
fn discover_indexes(data_dir: &Path) -> Result<Vec<(IndexKey, String)>, SchemaLogError> {
    let reserved: BTreeSet<&str> = ENGINE_RESERVED_DIRS.iter().copied().collect();
    let mut found = Vec::new();

    let tenants = match std::fs::read_dir(data_dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e.into()),
    };

    for tenant_entry in tenants.flatten() {
        if !is_dir(&tenant_entry)? {
            continue;
        }
        let tenant = tenant_entry.file_name().to_string_lossy().to_string();
        if tenant.starts_with('.') || reserved.contains(tenant.as_str()) {
            continue;
        }
        let tenant_path = tenant_entry.path();

        let dbs = match std::fs::read_dir(&tenant_path) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for db_entry in dbs.flatten() {
            if !is_dir(&db_entry)? {
                continue;
            }
            let database = db_entry.file_name().to_string_lossy().to_string();
            if database.starts_with('.') || reserved.contains(database.as_str()) {
                continue;
            }
            let db_path = db_entry.path();

            let colls = match std::fs::read_dir(&db_path) {
                Ok(it) => it,
                Err(_) => continue,
            };
            for coll_entry in colls.flatten() {
                if !is_dir(&coll_entry)? {
                    continue;
                }
                let collection = coll_entry.file_name().to_string_lossy().to_string();
                if collection.starts_with('.') || reserved.contains(collection.as_str()) {
                    continue;
                }
                let coll_path = coll_entry.path();
                if !coll_path.join("data.db").exists() {
                    continue;
                }

                let files = match std::fs::read_dir(&coll_path) {
                    Ok(it) => it,
                    Err(_) => continue,
                };
                for file in files.flatten() {
                    let fname = file.file_name().to_string_lossy().to_string();
                    if let Some(idx_name) = parse_index_filename(&fname) {
                        let key = IndexKey {
                            tenant: tenant.clone(),
                            database: database.clone(),
                            collection: collection.clone(),
                            index_name: idx_name.to_string(),
                        };
                        // See discover_indexes() doc comment: we use
                        // the index name as a placeholder field_path
                        // until 44b plumbs through the real value.
                        found.push((key, idx_name.to_string()));
                    }
                }
            }
        }
    }

    // Stable order for deterministic version numbering.
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Return the index name if the filename matches `idx_<name>.idx`.
fn parse_index_filename(name: &str) -> Option<&str> {
    name.strip_prefix("idx_")
        .and_then(|s| s.strip_suffix(".idx"))
}

fn is_dir(entry: &std::fs::DirEntry) -> Result<bool, SchemaLogError> {
    Ok(entry.file_type()?.is_dir())
}

/// Replay the schema log into a state. Convenience wrapper used by
/// tests and the `SchemaState`-from-path constructor in 44c.
pub fn load_state(data_dir: &Path) -> Result<SchemaState, SchemaLogError> {
    let (_log, state) = SchemaLog::open(data_dir)?;
    Ok(state)
}
