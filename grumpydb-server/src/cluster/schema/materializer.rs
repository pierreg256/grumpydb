//! Background materializer (Phase 44c).
//!
//! The schema log + gossip together describe **what** indexes ought
//! to exist on which collections. The materializer is the worker that
//! turns that abstract description into concrete on-disk state by
//! calling the engine's existing `Database::create_index` /
//! `Database::drop_index` routines.
//!
//! ## Design
//!
//! - One `tokio::spawn`'d task per server, running for the whole
//!   process lifetime.
//! - Driven by an `mpsc::UnboundedSender<MaterializeJob>` exposed via
//!   [`MaterializerHandle`] and consumed inside the worker.
//! - Each job is **idempotent**: `Build` is a no-op if the index
//!   already exists, `Drop` a no-op if it does not. Jobs may be
//!   replayed safely on crash + restart.
//! - The worker holds no locks across awaits; it acquires the
//!   `SharedDatabase` per job, runs the engine call, and releases.
//!
//! ## Failure handling
//!
//! On error, the worker logs and emits a Prometheus counter
//! (`grumpydb_schema_materialize_jobs_total{result="error"}`). It
//! does **not** retry automatically in 44c — the next gossip pull or
//! a future bootstrap-from-disk will re-enqueue. Adding a bounded
//! retry queue is left for 44d.
//!
//! ## Triggers
//!
//! Two call sites today (44c):
//! 1. `Coordinator::apply_remote_schema_entries` — gossip-driven.
//! 2. `Coordinator::apply_local_ddl` — DDL handler-driven (the local
//!    apply is also synchronous through `Database::create_index` so
//!    the client sees the effect immediately; the materializer call
//!    is informational and lets the bootstrap-on-restart path re-run
//!    a no-op safely).

use std::sync::Arc;

use grumpydb::SharedServer;
use grumpydb::error::GrumpyError;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::{IndexKey, SchemaLogEntry, SchemaOp};

/// One materialization job to be carried out by the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaterializeJob {
    /// Build (or no-op if already present) the secondary index for
    /// the given key, on the local node's shard.
    Build { key: IndexKey, field_path: String },
    /// Drop (or no-op if absent) the secondary index for the given
    /// key, on the local node's shard.
    Drop { key: IndexKey },
}

impl MaterializeJob {
    /// Build a [`MaterializeJob`] from one applied [`SchemaLogEntry`].
    /// Returns `None` for unsupported variants — currently every
    /// variant maps to a job.
    pub fn from_entry(entry: &SchemaLogEntry) -> Option<Self> {
        match &entry.op {
            SchemaOp::CreateIndex { key, field_path } => Some(Self::Build {
                key: key.clone(),
                field_path: field_path.clone(),
            }),
            SchemaOp::DropIndex { key } => Some(Self::Drop { key: key.clone() }),
        }
    }
}

/// Cheap-clone handle used by callers (Coordinator, gossip, DDL
/// handler) to enqueue jobs.
#[derive(Debug, Clone)]
pub struct MaterializerHandle {
    tx: UnboundedSender<MaterializeJob>,
}

impl MaterializerHandle {
    /// Enqueue one job. Errors silently when the worker has already
    /// shut down — the bootstrap-on-restart path will pick the work
    /// back up.
    pub fn enqueue(&self, job: MaterializeJob) {
        if let Err(e) = self.tx.send(job) {
            tracing::warn!(error = %e, "materializer enqueue dropped: worker is gone");
        }
    }

    /// Convenience helper: derive a job from a freshly-applied
    /// schema log entry and enqueue it.
    pub fn enqueue_from_entry(&self, entry: &SchemaLogEntry) {
        if let Some(job) = MaterializeJob::from_entry(entry) {
            self.enqueue(job);
        }
    }
}

/// Spawn the background worker and return its handle.
///
/// `shared_server` is what every job uses to look up the target
/// `SharedDatabase`. The worker keeps a clone for its entire lifetime.
pub fn spawn(shared_server: SharedServer) -> MaterializerHandle {
    let (tx, rx) = mpsc::unbounded_channel::<MaterializeJob>();
    let handle = MaterializerHandle { tx };

    tokio::spawn(async move {
        worker_loop(rx, Arc::new(shared_server)).await;
    });

    handle
}

async fn worker_loop(mut rx: UnboundedReceiver<MaterializeJob>, shared_server: Arc<SharedServer>) {
    while let Some(job) = rx.recv().await {
        let kind = match &job {
            MaterializeJob::Build { .. } => "build",
            MaterializeJob::Drop { .. } => "drop",
        };
        let started = std::time::Instant::now();

        let result = run_job(&job, shared_server.as_ref());

        let elapsed_secs = started.elapsed().as_secs_f64();
        let result_label = if result.is_ok() { "ok" } else { "error" };
        metrics::counter!(
            "grumpydb_schema_materialize_jobs_total",
            "kind" => kind,
            "result" => result_label
        )
        .increment(1);
        metrics::histogram!(
            "grumpydb_schema_materialize_duration_seconds",
            "kind" => kind
        )
        .record(elapsed_secs);

        match (&job, result) {
            (MaterializeJob::Build { key, field_path }, Ok(())) => {
                tracing::info!(
                    tenant = %key.tenant,
                    database = %key.database,
                    collection = %key.collection,
                    index_name = %key.index_name,
                    field_path = %field_path,
                    duration_ms = (elapsed_secs * 1000.0) as u64,
                    "schema converged: build"
                );
            }
            (MaterializeJob::Drop { key }, Ok(())) => {
                tracing::info!(
                    tenant = %key.tenant,
                    database = %key.database,
                    collection = %key.collection,
                    index_name = %key.index_name,
                    duration_ms = (elapsed_secs * 1000.0) as u64,
                    "schema converged: drop"
                );
            }
            (job, Err(e)) => {
                tracing::warn!(?job, error = %e, "schema materialize failed");
            }
        }
    }
}

fn run_job(job: &MaterializeJob, shared_server: &SharedServer) -> Result<(), String> {
    match job {
        MaterializeJob::Build { key, field_path } => build_index(shared_server, key, field_path),
        MaterializeJob::Drop { key } => drop_index(shared_server, key),
    }
}

fn build_index(
    shared_server: &SharedServer,
    key: &IndexKey,
    field_path: &str,
) -> Result<(), String> {
    let db = match shared_server.database(&key.tenant, &key.database) {
        Ok(db) => db,
        // The database does not exist yet on this node. Skip silently:
        // when the first INSERT routes here, the database will be
        // created and the next gossip tick will re-enqueue this job.
        Err(GrumpyError::DatabaseNotFound(_)) => {
            tracing::debug!(
                tenant = %key.tenant,
                database = %key.database,
                "skip materialize: database not present locally yet"
            );
            return Ok(());
        }
        Err(e) => return Err(format!("open database: {e}")),
    };

    match db.create_index(&key.collection, &key.index_name, field_path) {
        Ok(()) => Ok(()),
        Err(GrumpyError::IndexAlreadyExists(_)) => {
            // Idempotent: the local index is already there.
            Ok(())
        }
        Err(GrumpyError::CollectionNotFound(_)) => {
            // Same rationale as DatabaseNotFound above.
            tracing::debug!(
                tenant = %key.tenant,
                database = %key.database,
                collection = %key.collection,
                "skip materialize: collection not present locally yet"
            );
            Ok(())
        }
        Err(e) => Err(format!("create_index: {e}")),
    }
}

fn drop_index(shared_server: &SharedServer, key: &IndexKey) -> Result<(), String> {
    let db = match shared_server.database(&key.tenant, &key.database) {
        Ok(db) => db,
        Err(GrumpyError::DatabaseNotFound(_)) => return Ok(()),
        Err(e) => return Err(format!("open database: {e}")),
    };

    match db.drop_index(&key.collection, &key.index_name) {
        Ok(()) => Ok(()),
        Err(GrumpyError::IndexNotFound(_)) => Ok(()),
        Err(GrumpyError::CollectionNotFound(_)) => Ok(()),
        Err(e) => Err(format!("drop_index: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::schema::{SchemaLogEntry, SchemaOp};
    use grumpydb::document::value::Value;
    use std::collections::BTreeMap;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn key(tenant: &str, db: &str, coll: &str, name: &str) -> IndexKey {
        IndexKey {
            tenant: tenant.into(),
            database: db.into(),
            collection: coll.into(),
            index_name: name.into(),
        }
    }

    fn doc(name: &str, age: i64) -> Value {
        let mut map = BTreeMap::new();
        map.insert("name".into(), Value::String(name.into()));
        map.insert("age".into(), Value::Integer(age));
        Value::Object(map)
    }

    #[test]
    fn test_from_entry_maps_create_to_build() {
        let entry = SchemaLogEntry {
            version: 1,
            hlc: 100,
            op: SchemaOp::CreateIndex {
                key: key("t", "d", "c", "by_x"),
                field_path: "x".into(),
            },
        };
        let job = MaterializeJob::from_entry(&entry).expect("job");
        match job {
            MaterializeJob::Build { key: k, field_path } => {
                assert_eq!(k.index_name, "by_x");
                assert_eq!(field_path, "x");
            }
            _ => panic!("expected Build"),
        }
    }

    #[test]
    fn test_from_entry_maps_drop_to_drop() {
        let entry = SchemaLogEntry {
            version: 2,
            hlc: 200,
            op: SchemaOp::DropIndex {
                key: key("t", "d", "c", "by_x"),
            },
        };
        let job = MaterializeJob::from_entry(&entry).expect("job");
        assert!(matches!(job, MaterializeJob::Drop { .. }));
    }

    /// run_job is sync. We verify it actually creates the on-disk
    /// index file by setting up a SharedServer with one document
    /// and inspecting the directory afterwards.
    #[test]
    fn test_run_job_build_creates_index_on_disk() {
        let dir = TempDir::new().expect("tmp");
        let server = SharedServer::open(dir.path()).expect("open server");
        server.create_client("_system").expect("client");
        let db = server
            .database("_system", "demo")
            .or_else(|_| {
                server.create_database("_system", "demo")?;
                server.database("_system", "demo")
            })
            .expect("db");
        db.create_collection("docs").expect("collection");
        db.insert("docs", Uuid::new_v4(), doc("alice", 30))
            .expect("insert");
        db.insert("docs", Uuid::new_v4(), doc("bob", 25))
            .expect("insert");

        let job = MaterializeJob::Build {
            key: key("_system", "demo", "docs", "by_name"),
            field_path: "name".into(),
        };
        run_job(&job, &server).expect("build");

        // Re-running the same job is a no-op (idempotency).
        run_job(&job, &server).expect("idempotent rerun");

        // The index file must exist on disk.
        let idx_path = dir
            .path()
            .join("_system")
            .join("demo")
            .join("docs")
            .join("idx_by_name.idx");
        assert!(idx_path.exists(), "expected {idx_path:?} to exist");

        // The index must answer queries.
        let results = db
            .query("docs", "by_name", &Value::String("alice".into()))
            .expect("query");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_run_job_build_skips_missing_database_silently() {
        let dir = TempDir::new().expect("tmp");
        let server = SharedServer::open(dir.path()).expect("open server");
        let job = MaterializeJob::Build {
            key: key("_system", "ghost", "docs", "by_x"),
            field_path: "x".into(),
        };
        run_job(&job, &server).expect("missing db is not an error");
    }

    #[test]
    fn test_run_job_drop_is_idempotent_when_missing() {
        let dir = TempDir::new().expect("tmp");
        let server = SharedServer::open(dir.path()).expect("open server");
        server.create_client("_system").expect("client");
        server.create_database("_system", "demo").expect("db");
        let db = server.database("_system", "demo").expect("open db");
        db.create_collection("docs").expect("collection");

        let job = MaterializeJob::Drop {
            key: key("_system", "demo", "docs", "never_existed"),
        };
        run_job(&job, &server).expect("drop missing index is no-op");
    }

    /// End-to-end: spawn the worker, enqueue a job, wait for the
    /// on-disk side effect, then assert. Uses a poll loop so the test
    /// is robust against scheduler jitter without sleeping eagerly.
    #[tokio::test]
    async fn test_spawn_worker_processes_enqueued_jobs() {
        let dir = TempDir::new().expect("tmp");
        let server = SharedServer::open(dir.path()).expect("open server");
        server.create_client("_system").expect("client");
        server.create_database("_system", "demo").expect("db");
        let db = server.database("_system", "demo").expect("open db");
        db.create_collection("docs").expect("collection");
        db.insert("docs", Uuid::new_v4(), doc("alice", 30))
            .expect("insert");

        let handle = spawn(server.clone());
        handle.enqueue(MaterializeJob::Build {
            key: key("_system", "demo", "docs", "by_name"),
            field_path: "name".into(),
        });

        let idx_path = dir
            .path()
            .join("_system")
            .join("demo")
            .join("docs")
            .join("idx_by_name.idx");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if idx_path.exists() {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("materializer did not create {idx_path:?} within 2s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
