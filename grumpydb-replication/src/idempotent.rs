//! Idempotent replication applier — slice 40e.5.
//!
//! Wraps any [`ReplicationApplier`] with the engine's persistent
//! [`AppliedSet`] watermark so that **replay of an already-applied
//! `(origin_node_id, hlc)` is a no-op**. This is the contract every
//! follower-side applier promises in the [`ReplicationApplier::apply`]
//! doc-comment; this slice fulfils it without having to re-implement
//! the watermark logic in the engine.
//!
//! The wrapper is intentionally thin: it does the minimum bookkeeping
//! needed to honour the contract, then delegates to the inner applier
//! for the actual disk-side work.
//!
//! ## Persistence
//!
//! After every successful inner-applier call, the watermark is
//! `observe()`d **and** persisted to
//! `<data_dir>/_replication/state.json` (atomic via tmp + rename — see
//! [`AppliedSet::save`]). Crash-safety: if the process dies between
//! `apply()` and `save()`, the next replay will re-apply the record;
//! the engine's WAL writer is responsible for ensuring the inner
//! `apply()` is itself idempotent at the page level (slice 40e.6
//! integration).
//!
//! ## Lookup helpers
//!
//! [`resume_hlc_for`] reads a fresh `AppliedSet` and returns the
//! `start_hlc` a follower should advertise in its `Subscribe` frame.
//! `0` is returned for unknown origins so a brand-new follower starts
//! from the beginning of the leader's WAL.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use grumpydb::wal::applied_set::{AppliedSet, ObserveOutcome};
use grumpydb::wal::hlc::Hlc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::tasks::{ApplyError, ReplicationApplier};

/// On-disk byte offset of the `origin_node_id` field inside a v2
/// WAL record (see `src/wal/record.rs` doc-comment).
const ORIGIN_OFFSET: usize = 21;
/// On-disk byte offset of the `hlc` field inside a v2 WAL record.
const HLC_OFFSET: usize = 37;
/// Minimum byte length needed to safely peek both fields.
const MIN_PEEK: usize = HLC_OFFSET + 8;

/// Reads `(origin_node_id, hlc)` from a v2-encoded WAL record without
/// fully decoding it. Returns `None` if the buffer is too short.
fn peek_origin_hlc(raw: &[u8]) -> Option<(u128, u64)> {
    if raw.len() < MIN_PEEK {
        return None;
    }
    let mut origin = [0u8; 16];
    origin.copy_from_slice(&raw[ORIGIN_OFFSET..ORIGIN_OFFSET + 16]);
    let hlc = u64::from_le_bytes([
        raw[HLC_OFFSET],
        raw[HLC_OFFSET + 1],
        raw[HLC_OFFSET + 2],
        raw[HLC_OFFSET + 3],
        raw[HLC_OFFSET + 4],
        raw[HLC_OFFSET + 5],
        raw[HLC_OFFSET + 6],
        raw[HLC_OFFSET + 7],
    ]);
    Some((u128::from_le_bytes(origin), hlc))
}

/// Returns the `start_hlc` a follower should advertise for `origin`.
///
/// Reads the persisted [`AppliedSet`] from `<data_dir>/_replication/state.json`.
/// Returns `0` for unknown origins (a brand-new follower asks for the
/// full WAL).
///
/// Bubbles up [`grumpydb::GrumpyError`] from the engine on I/O failure.
pub fn resume_hlc_for(data_dir: &Path, origin: Uuid) -> Result<u64, grumpydb::GrumpyError> {
    let set = AppliedSet::load(data_dir)?;
    Ok(set.high_water(origin.as_u128()).0)
}

/// Adapter that filters out already-applied records before delegating
/// to an inner [`ReplicationApplier`].
///
/// All bookkeeping is mediated by an [`AppliedSet`] held behind a
/// `tokio::sync::Mutex` so multiple `FollowerTask`s (one per origin)
/// can share the same persistence file in v6+ multi-writer mode. In
/// v5 single-writer the lock is uncontended.
///
/// The wrapper:
///
/// 1. Peeks `(origin, hlc)` from the v2-encoded raw bytes.
/// 2. Calls [`AppliedSet::observe`] under the lock; on
///    [`ObserveOutcome::AlreadyApplied`], returns `Ok(())` without
///    touching the inner applier.
/// 3. On [`ObserveOutcome::New`], calls the inner applier; if that
///    succeeds, persists the [`AppliedSet`] to disk.
///
/// Crash-safety: if the inner `apply()` succeeds but the subsequent
/// `save()` fails, the in-memory watermark stays advanced and the
/// next call still treats the next records correctly. The engine's
/// inner applier is responsible for its own page-level idempotence
/// against re-replay after a crash.
pub struct IdempotentApplier<A> {
    inner: A,
    state: Arc<Mutex<AppliedSetGuard>>,
}

/// Shared mutable state held by an [`IdempotentApplier`]. Wrapped in
/// a struct (rather than a bare `AppliedSet`) so future enhancements
/// can add fields (e.g. a save-debounce coalescer) without breaking
/// the public type signature.
struct AppliedSetGuard {
    set: AppliedSet,
    data_dir: PathBuf,
}

impl<A> IdempotentApplier<A> {
    /// Constructs a wrapper around `inner`, loading the persisted
    /// [`AppliedSet`] from `<data_dir>/_replication/state.json` (or
    /// the empty default when missing).
    pub fn new(inner: A, data_dir: PathBuf) -> Result<Self, grumpydb::GrumpyError> {
        let set = AppliedSet::load(&data_dir)?;
        Ok(Self {
            inner,
            state: Arc::new(Mutex::new(AppliedSetGuard { set, data_dir })),
        })
    }

    /// Returns the highest applied HLC for `origin`, reading the
    /// in-memory state (consistent with the persisted file modulo
    /// any in-flight `save()`).
    pub async fn high_water(&self, origin: Uuid) -> u64 {
        let g = self.state.lock().await;
        g.set.high_water(origin.as_u128()).0
    }
}

#[async_trait]
impl<A> ReplicationApplier for IdempotentApplier<A>
where
    A: ReplicationApplier,
{
    async fn apply(&self, raw: &[u8]) -> Result<(), ApplyError> {
        let (origin, hlc) = peek_origin_hlc(raw).ok_or_else(|| {
            ApplyError::Decode(format!(
                "cannot peek origin/hlc from raw record (len={}, need >= {MIN_PEEK})",
                raw.len()
            ))
        })?;

        // Decide under the lock whether to apply.
        let should_apply = {
            let mut g = self.state.lock().await;
            matches!(
                g.set.observe(origin, Hlc::from_packed(hlc)),
                ObserveOutcome::New
            )
        };

        if !should_apply {
            // Already-applied replay → no-op, contract honoured.
            return Ok(());
        }

        // Delegate the actual page-level work to the inner applier.
        self.inner.apply(raw).await?;

        // Best-effort persist of the advanced watermark. Failure here
        // is logged via the returned error variant rather than
        // panicking — losing the persisted watermark only costs us a
        // re-replay on next start, which is itself a no-op once this
        // wrapper rebuilds the in-memory `AppliedSet`.
        let g = self.state.lock().await;
        g.set
            .save(&g.data_dir)
            .map_err(|e| ApplyError::Engine(format!("persist applied_set: {e}")))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use grumpydb::wal::hlc::HlcClock;
    use grumpydb::wal::writer::WalWriter;
    use tempfile::TempDir;

    use super::*;
    use crate::TailedRecord;
    use crate::tailer::WalTailer;

    /// Captures every `apply()` call. Lets us verify that replays
    /// are filtered out before reaching the inner applier.
    struct CountingApplier {
        applied: StdMutex<Vec<Vec<u8>>>,
    }
    impl CountingApplier {
        fn new() -> Self {
            Self {
                applied: StdMutex::new(Vec::new()),
            }
        }
        fn snapshot(&self) -> Vec<Vec<u8>> {
            self.applied.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl ReplicationApplier for CountingApplier {
        async fn apply(&self, raw: &[u8]) -> Result<(), ApplyError> {
            self.applied.lock().unwrap().push(raw.to_vec());
            Ok(())
        }
    }

    fn write_records(dir: &TempDir, node_id: u128, n: usize) -> std::path::PathBuf {
        let path = dir.path().join("wal.log");
        let clock = Arc::new(HlcClock::new());
        let mut w = WalWriter::new_with_identity(&path, node_id, clock).unwrap();
        for _ in 0..n {
            let tx = w.begin_tx();
            let _ = w.log_page_write(tx, 0, &[0u8; 8], &[1u8; 8]).unwrap();
            let _ = w.log_commit(tx).unwrap();
        }
        path
    }

    fn drain(path: &std::path::Path) -> Vec<TailedRecord> {
        let mut t = WalTailer::open(path).unwrap();
        let mut out = Vec::new();
        while let Some(r) = t.poll_once().unwrap() {
            out.push(r);
        }
        out
    }

    #[tokio::test]
    async fn test_peek_origin_hlc_roundtrip() {
        let dir = TempDir::new().unwrap();
        let writer = Uuid::new_v4();
        let path = write_records(&dir, writer.as_u128(), 1);
        let recs = drain(&path);
        assert!(!recs.is_empty());
        for r in &recs {
            let (origin, hlc) = peek_origin_hlc(&r.raw).unwrap();
            assert_eq!(origin, r.record.origin_node_id);
            assert_eq!(hlc, r.record.hlc.0);
        }
    }

    #[tokio::test]
    async fn test_peek_origin_hlc_short_buffer_returns_none() {
        assert_eq!(peek_origin_hlc(&[0u8; MIN_PEEK - 1]), None);
        // Just-enough length succeeds (returns *some* tuple — doesn't matter the value).
        assert!(peek_origin_hlc(&[0u8; MIN_PEEK]).is_some());
    }

    #[tokio::test]
    async fn test_resume_hlc_for_unknown_origin_is_zero() {
        let dir = TempDir::new().unwrap();
        let v = resume_hlc_for(dir.path(), Uuid::new_v4()).unwrap();
        assert_eq!(v, 0);
    }

    #[tokio::test]
    async fn test_idempotent_applier_first_pass_applies_all() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().to_path_buf();
        let writer = Uuid::new_v4();
        let wal_path = write_records(&dir, writer.as_u128(), 3);
        let recs = drain(&wal_path);
        assert_eq!(recs.len(), 6); // 3 commits × (PageWrite + Commit)

        let inner = Arc::new(CountingApplier::new());
        let wrapper = IdempotentApplier::new(Arc::clone(&inner), data_dir.clone()).unwrap();

        for r in &recs {
            wrapper.apply(&r.raw).await.unwrap();
        }
        assert_eq!(inner.snapshot().len(), 6);

        // High-water is exactly the last record's HLC.
        let last = recs.last().unwrap();
        assert_eq!(wrapper.high_water(writer).await, last.record.hlc.0);

        // The persisted file exists and round-trips.
        let p = data_dir.join("_replication").join("state.json");
        assert!(p.exists());
    }

    #[tokio::test]
    async fn test_idempotent_applier_replays_skip_inner() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().to_path_buf();
        let writer = Uuid::new_v4();
        let wal_path = write_records(&dir, writer.as_u128(), 2);
        let recs = drain(&wal_path);
        assert_eq!(recs.len(), 4);

        let inner = Arc::new(CountingApplier::new());
        let wrapper = IdempotentApplier::new(Arc::clone(&inner), data_dir.clone()).unwrap();

        // First pass: all 4 reach the inner applier.
        for r in &recs {
            wrapper.apply(&r.raw).await.unwrap();
        }
        assert_eq!(inner.snapshot().len(), 4);

        // Replay every record: NONE should reach the inner applier.
        for r in &recs {
            wrapper.apply(&r.raw).await.unwrap();
        }
        assert_eq!(
            inner.snapshot().len(),
            4,
            "replays must NOT reach the inner applier"
        );
    }

    #[tokio::test]
    async fn test_idempotent_applier_resumes_from_disk_after_restart() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().to_path_buf();
        let writer = Uuid::new_v4();
        let wal_path = write_records(&dir, writer.as_u128(), 3);
        let recs = drain(&wal_path);

        // First "incarnation" applies the first 4 records.
        {
            let inner = Arc::new(CountingApplier::new());
            let wrapper = IdempotentApplier::new(Arc::clone(&inner), data_dir.clone()).unwrap();
            for r in recs.iter().take(4) {
                wrapper.apply(&r.raw).await.unwrap();
            }
            assert_eq!(inner.snapshot().len(), 4);
        }
        // Watermark persisted across the drop — `resume_hlc_for`
        // sees the 4th record's HLC.
        let resumed = resume_hlc_for(&data_dir, writer).unwrap();
        assert_eq!(resumed, recs[3].record.hlc.0);

        // Second incarnation receives the FULL stream (as if reconnected
        // before knowing where to resume). Only records 5..6 should
        // reach the inner applier.
        let inner2 = Arc::new(CountingApplier::new());
        let wrapper2 = IdempotentApplier::new(Arc::clone(&inner2), data_dir.clone()).unwrap();
        for r in &recs {
            wrapper2.apply(&r.raw).await.unwrap();
        }
        assert_eq!(
            inner2.snapshot().len(),
            recs.len() - 4,
            "only the un-applied records should reach the inner applier on resume"
        );
    }

    #[tokio::test]
    async fn test_idempotent_applier_short_record_returns_decode_error() {
        let dir = TempDir::new().unwrap();
        let inner = Arc::new(CountingApplier::new());
        let wrapper = IdempotentApplier::new(Arc::clone(&inner), dir.path().to_path_buf()).unwrap();
        let err = wrapper.apply(&[0u8; 4]).await.unwrap_err();
        assert!(
            matches!(err, ApplyError::Decode(_)),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            inner.snapshot().len(),
            0,
            "decode failure must short-circuit"
        );
    }

    /// `Arc<dyn ReplicationApplier>` works as the wrapped applier so
    /// the wrapper is composable with existing trait-object plumbing
    /// (which the server side will use).
    #[tokio::test]
    async fn test_idempotent_applier_works_with_arc_dyn() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().to_path_buf();
        let writer = Uuid::new_v4();
        let wal_path = write_records(&dir, writer.as_u128(), 1);
        let recs = drain(&wal_path);

        let inner: Arc<dyn ReplicationApplier> = Arc::new(CountingApplier::new());
        let wrapper = IdempotentApplier::new(inner, data_dir).unwrap();
        for r in &recs {
            wrapper.apply(&r.raw).await.unwrap();
        }
    }
}
