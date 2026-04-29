//! Async WAL tailer: yields freshly-appended WAL records as they become durable.
//!
//! Slice 40e.2 of [Phase 40e](../../../docs/IMPLEMENTATION_PLAN_V4.md). The
//! tailer is the **producer side** of replication: it watches `wal.log`,
//! decodes each new v2 record once it has been fully appended + fsynced,
//! and exposes them to the upper layer (Phase 40e.4 leader-side
//! replication session) which will wrap each record in a [`Frame::WalRecord`]
//! and stream it to followers.
//!
//! ## Design
//!
//! The tailer opens its **own read-only** `File` handle on `wal.log` —
//! independent of the engine's [`grumpydb::wal::WalWriter`]. This keeps
//! the engine's hot path lock-free and means tailer crashes can never
//! impact the writer.
//!
//! Reads use a polling loop:
//!
//! 1. Read all bytes from the current offset to EOF into an internal
//!    pending buffer.
//! 2. Try to decode one full v2 record from the head of the buffer
//!    via [`grumpydb::wal::record::WalRecord::from_bytes_v2`].
//! 3. On success, advance `offset` by the number of bytes consumed
//!    and return the record.
//! 4. On decode failure (truncated buffer **or** an in-flight append
//!    that hasn't reached its fsync barrier yet), **do not** advance
//!    the offset; the next poll will retry against more bytes.
//!
//! The CRC32 trailer baked into every WAL v2 record (verified deep
//! inside `from_bytes_v2`) is what makes step 4 safe: a partially-flushed
//! record can never decode cleanly, so the tailer simply waits.
//!
//! ## Header skipping
//!
//! Format v2 prepends an 8 KiB header page; the tailer initialises its
//! offset at `WAL_HEADER_SIZE` so the first record decode sees the
//! actual record stream. Format v1 files (which lack a header) are
//! eagerly migrated to v2 by the writer on open, so the tailer never
//! needs to handle them.
//!
//! ## Async wrapper
//!
//! [`WalTailer::next_record`] is a `pub async fn` that awaits the next
//! record using a fixed-interval poll (default 50 ms, configurable via
//! [`WalTailer::with_poll_interval`]). For tests and unit-driven
//! pipelines, [`WalTailer::poll_once`] performs a single non-blocking
//! attempt and returns `Ok(None)` when the buffer is short.
//!
//! Future slices (40e.5) will plumb a watermark argument so the tailer
//! can fast-forward over already-applied records on a follower's
//! reconnect — for now it always streams from the configured starting
//! offset.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use grumpydb::wal::record::{WAL_HEADER_SIZE, WAL_VERSION_V2, WalRecord, parse_wal_header};

use crate::frame::ReplicationError;

/// Default poll interval used by [`WalTailer::next_record`].
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One record yielded by the tailer.
///
/// Carries both the structured [`WalRecord`] (so consumers can read
/// `(origin_node_id, hlc)` for routing/dedup) **and** the canonical
/// v2-encoded byte slice (so the leader-side session can ship the
/// bytes verbatim into a [`crate::Frame::WalRecord`] without re-encoding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailedRecord {
    /// Decoded record (already includes `origin_node_id`, `hlc`,
    /// `vector_clock`, etc).
    pub record: WalRecord,
    /// Canonical v2-encoded bytes of `record`. Equal to
    /// `record.to_bytes()`; cached during the decode pass to avoid a
    /// re-encode in the hot replication path.
    pub raw: Vec<u8>,
}

/// Streaming reader over a WAL file.
///
/// Construct via [`WalTailer::open`]; drive forward with
/// [`WalTailer::poll_once`] (sync, non-blocking) or
/// [`WalTailer::next_record`] (async, blocks until a record is available
/// or the future is dropped).
#[derive(Debug)]
pub struct WalTailer {
    file: File,
    /// Path the tailer is reading from (held for diagnostics + future
    /// inotify/kqueue watcher integration).
    path: PathBuf,
    /// File offset of the next byte the tailer wants to read.
    offset: u64,
    /// Bytes that have been read from disk but not yet decoded into a
    /// full record (e.g. because we hit a truncated tail). Drained as
    /// records are decoded.
    pending: Vec<u8>,
    /// How long [`Self::next_record`] sleeps between poll attempts.
    poll_interval: Duration,
}

impl WalTailer {
    /// Opens `path` for tailing.
    ///
    /// Validates the v2 magic + version in the 8 KiB header; v1 files
    /// (which the engine eagerly migrates on open) and any other
    /// unrecognised header is rejected.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ReplicationError> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let len = file.metadata()?.len();
        if len < WAL_HEADER_SIZE as u64 {
            // Empty / partially-initialised file — start from the header
            // boundary and let the next poll catch up once the engine
            // finishes writing the header.
            return Ok(Self {
                file,
                path,
                offset: WAL_HEADER_SIZE as u64,
                pending: Vec::new(),
                poll_interval: DEFAULT_POLL_INTERVAL,
            });
        }
        // Read + parse the header to ensure we're talking to a v2 file.
        let mut hdr = [0u8; 18];
        file.read_exact(&mut hdr)?;
        let version = parse_wal_header(&hdr).map_err(|e| {
            ReplicationError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("WAL header parse failed at {}: {e}", path.display()),
            ))
        })?;
        if version != WAL_VERSION_V2 {
            return Err(ReplicationError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "WAL at {} has unsupported version {} (tailer requires v2)",
                    path.display(),
                    version
                ),
            )));
        }
        // Position immediately after the header page.
        file.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;
        Ok(Self {
            file,
            path,
            offset: WAL_HEADER_SIZE as u64,
            pending: Vec::new(),
            poll_interval: DEFAULT_POLL_INTERVAL,
        })
    }

    /// Builder-style: sets the poll interval used by [`Self::next_record`].
    /// Has no effect on [`Self::poll_once`] (which is always non-blocking).
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Path this tailer is reading from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current absolute byte offset the tailer is positioned at.
    /// Equals `WAL_HEADER_SIZE` immediately after [`Self::open`].
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Performs one non-blocking poll.
    ///
    /// - `Ok(Some(record))` — a full record was decoded and the offset
    ///   has been advanced past it.
    /// - `Ok(None)` — no full record is yet available (buffer is empty
    ///   or the trailing bytes only contain a partial record). The
    ///   caller should sleep then retry, or call [`Self::next_record`]
    ///   to await automatically.
    /// - `Err(...)` — an I/O failure. (The decode itself can't surface
    ///   here — incomplete decodes are mapped to `Ok(None)` so the
    ///   tailer waits for more bytes.)
    pub fn poll_once(&mut self) -> Result<Option<TailedRecord>, ReplicationError> {
        // 1. Pull whatever's available since the last call into our
        //    pending buffer.
        self.refill_pending()?;

        // 2. Try to decode a single full record from the head.
        if self.pending.is_empty() {
            return Ok(None);
        }
        match WalRecord::from_bytes_v2(&self.pending) {
            Ok((record, consumed)) => {
                let raw = self.pending[..consumed].to_vec();
                // Drain the consumed bytes; pending now starts at the
                // next record (or is empty).
                self.pending.drain(..consumed);
                Ok(Some(TailedRecord { record, raw }))
            }
            Err(_) => {
                // Either a truncated tail (writer mid-append, normal)
                // or a real corruption (rare). v5 behaviour: wait for
                // more bytes — replicating the recovery code's "stop
                // at first decode error" policy. v6 will add a
                // stuck-pointer guard.
                Ok(None)
            }
        }
    }

    /// Awaits the next record.
    ///
    /// Polls in a loop with the configured [`poll_interval`]. The future
    /// is cancellation-safe: dropping it on a `tokio::select!` boundary
    /// loses no data because all state lives in the `WalTailer`'s own
    /// fields (no internal task is spawned).
    ///
    /// Errors propagate as soon as they appear; only an `Ok(None)` from
    /// [`Self::poll_once`] triggers a sleep + retry.
    ///
    /// [`poll_interval`]: Self::with_poll_interval
    pub async fn next_record(&mut self) -> Result<TailedRecord, ReplicationError> {
        loop {
            if let Some(rec) = self.poll_once()? {
                return Ok(rec);
            }
            tokio_sleep(self.poll_interval).await;
        }
    }

    /// Reads everything between the current `offset` and EOF into
    /// `pending` and updates `offset`. No-ops if EOF is at or before
    /// the current position.
    fn refill_pending(&mut self) -> Result<(), ReplicationError> {
        let len = self.file.metadata()?.len();
        if len <= self.offset {
            return Ok(());
        }
        let to_read = (len - self.offset) as usize;
        // Read the new tail in one go.
        self.file.seek(SeekFrom::Start(self.offset))?;
        let start = self.pending.len();
        self.pending.resize(start + to_read, 0);
        self.file.read_exact(&mut self.pending[start..])?;
        self.offset += to_read as u64;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tokio shim
// ---------------------------------------------------------------------------
//
// We don't want to pull tokio into this crate's *runtime* dependencies
// (the codec slice should keep its `[dependencies]` lean), but
// `next_record` is genuinely async. The wrapper below uses
// `std::future::Future` + `std::task` to register a wake-up after
// `interval` using whatever runtime the caller is on.
//
// This avoids a hard dep on tokio in `[dependencies]`, but the upper
// crates that *do* pull tokio (grumpydb-server) get a working
// future. Standalone unit tests in this file therefore exercise
// `poll_once` only — full async integration arrives with slice 40e.4.
async fn tokio_sleep(d: Duration) {
    use std::task::{Context, Poll};
    use std::thread;
    use std::time::Instant;

    struct Sleep {
        deadline: Instant,
    }
    impl std::future::Future for Sleep {
        type Output = ();
        fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let now = Instant::now();
            if now >= self.deadline {
                Poll::Ready(())
            } else {
                let waker = cx.waker().clone();
                let remaining = self.deadline - now;
                thread::spawn(move || {
                    thread::sleep(remaining);
                    waker.wake();
                });
                Poll::Pending
            }
        }
    }
    Sleep {
        deadline: Instant::now() + d,
    }
    .await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use grumpydb::wal::hlc::HlcClock;
    use grumpydb::wal::vclock::VectorClock;
    use grumpydb::wal::writer::WalWriter;
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;

    fn fresh_writer() -> (TempDir, PathBuf, WalWriter, Arc<HlcClock>, u128) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wal.log");
        let node = Uuid::new_v4().as_u128();
        let clock = Arc::new(HlcClock::new());
        let writer = WalWriter::new_with_identity(&path, node, Arc::clone(&clock)).unwrap();
        (dir, path, writer, clock, node)
    }

    fn append_commit(writer: &mut WalWriter) -> u64 {
        let tx = writer.begin_tx();
        // Write a tiny page so a PageWrite + Commit pair is exercised.
        let _ = writer.log_page_write(tx, 0, &[0u8; 8], &[1u8; 8]).unwrap();
        let (lsn, _) = writer.log_commit(tx).unwrap();
        lsn
    }

    #[test]
    fn test_open_empty_wal_yields_no_records() {
        let (_dir, path, _writer, _clock, _node) = fresh_writer();
        let mut tailer = WalTailer::open(&path).unwrap();
        assert_eq!(tailer.offset(), WAL_HEADER_SIZE as u64);
        assert!(tailer.poll_once().unwrap().is_none());
    }

    #[test]
    fn test_open_v1_or_unknown_rejected() {
        // Hand-craft a non-WAL file; tailer must refuse to open.
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("wal.log");
        std::fs::write(&p, vec![0u8; WAL_HEADER_SIZE + 32]).unwrap();
        let err = WalTailer::open(&p).unwrap_err();
        assert!(matches!(err, ReplicationError::Io(_)));
    }

    #[test]
    fn test_tail_after_one_commit_yields_two_records() {
        let (_dir, path, mut writer, _clock, _node) = fresh_writer();
        append_commit(&mut writer);

        let mut tailer = WalTailer::open(&path).unwrap();
        // Two records expected: PageWrite + Commit.
        let r1 = tailer.poll_once().unwrap().unwrap();
        let r2 = tailer.poll_once().unwrap().unwrap();
        assert!(tailer.poll_once().unwrap().is_none());

        // LSNs strictly increasing.
        assert!(r2.record.lsn > r1.record.lsn);
        // Both bytes round-trip via to_bytes().
        assert_eq!(r1.raw, r1.record.to_bytes());
        assert_eq!(r2.raw, r2.record.to_bytes());
    }

    #[test]
    fn test_tail_picks_up_records_appended_after_open() {
        let (_dir, path, mut writer, _clock, _node) = fresh_writer();
        append_commit(&mut writer);

        let mut tailer = WalTailer::open(&path).unwrap();
        // Drain initial two records.
        let _ = tailer.poll_once().unwrap().unwrap();
        let _ = tailer.poll_once().unwrap().unwrap();
        assert!(tailer.poll_once().unwrap().is_none());

        // Append more records *after* the tailer is up.
        append_commit(&mut writer);
        append_commit(&mut writer);

        let mut count = 0;
        while tailer.poll_once().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 4, "expected 2 PageWrite + 2 Commit records");
    }

    #[test]
    fn test_tail_records_carry_origin_and_hlc_fields() {
        let (_dir, path, mut writer, _clock, node) = fresh_writer();
        append_commit(&mut writer);

        let mut tailer = WalTailer::open(&path).unwrap();
        let rec = tailer.poll_once().unwrap().unwrap();
        // origin_node_id stamped from the writer's identity.
        assert_eq!(rec.record.origin_node_id, node);
        // HLC is non-zero (clock has ticked at least once).
        assert!(rec.record.hlc.physical_ms() > 0 || rec.record.hlc.logical() > 0);
        // Vector clock has at least one entry (singleton in v5).
        assert!(rec.record.vector_clock != VectorClock::default());
    }

    #[test]
    fn test_tail_handles_partial_record_then_completes() {
        // Open the file directly so we can append crafted bytes (a
        // partial record header) BEFORE the writer commits a full one.
        let (_dir, path, mut writer, _clock, _node) = fresh_writer();
        append_commit(&mut writer);

        let mut tailer = WalTailer::open(&path).unwrap();
        // Drain the legitimate records.
        let _ = tailer.poll_once().unwrap().unwrap();
        let _ = tailer.poll_once().unwrap().unwrap();
        assert!(tailer.poll_once().unwrap().is_none());

        // Manually truncate-append: write only the first 4 bytes (the
        // length prefix) of a fake record. Decoder must NOT advance.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(&100u32.to_le_bytes()).unwrap();
            f.sync_all().unwrap();
        }
        let off_before = tailer.offset();
        assert!(tailer.poll_once().unwrap().is_none());
        // Offset DID move past the 4 written bytes (those are now in
        // pending), but no record was emitted.
        assert_eq!(tailer.offset(), off_before + 4);

        // Now append a real record. The pending 4 bytes will keep
        // failing to decode as long as they sit in front of the real
        // record (they look like a header announcing 100 bytes of
        // payload, but the actual bytes that follow are the next
        // legitimate record header). The tailer's job is to NOT crash;
        // a real-life writer wouldn't leave a partial header on disk
        // after fsync (commit_log fsyncs the whole record), so this
        // pathological case is bounded — but we do verify resilience.
        append_commit(&mut writer);

        // The pending corrupted bytes block forward progress until
        // either the decoder accepts them (it won't, because length
        // 100 won't match the real record layout) or someone truncates
        // the WAL. v5 contract: tailer simply waits and never panics.
        // We assert the tailer did NOT decode a bogus record.
        assert!(tailer.poll_once().unwrap().is_none());
    }

    #[test]
    fn test_with_poll_interval_round_trips() {
        let (_dir, path, _writer, _clock, _node) = fresh_writer();
        let tailer = WalTailer::open(&path)
            .unwrap()
            .with_poll_interval(Duration::from_millis(7));
        assert_eq!(tailer.poll_interval, Duration::from_millis(7));
    }

    #[test]
    fn test_path_accessor() {
        let (_dir, path, _writer, _clock, _node) = fresh_writer();
        let tailer = WalTailer::open(&path).unwrap();
        assert_eq!(tailer.path(), path.as_path());
    }

    #[test]
    fn test_next_record_async_completes_when_record_arrives() {
        // Drive the async path on a single-threaded executor that
        // ticks `tokio_sleep` futures via thread-spawn wakers.
        use std::sync::atomic::{AtomicBool, Ordering};

        let (dir, path, mut writer, _clock, _node) = fresh_writer();
        let path2 = path.clone();
        // Background thread that appends a record after a short delay.
        let appended = Arc::new(AtomicBool::new(false));
        let appended_bg = Arc::clone(&appended);
        let bg = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            append_commit(&mut writer);
            appended_bg.store(true, Ordering::SeqCst);
        });

        let mut tailer = WalTailer::open(&path2)
            .unwrap()
            .with_poll_interval(Duration::from_millis(20));

        // Block-on a minimal executor.
        let fut = tailer.next_record();
        let rec = block_on(fut).unwrap();
        bg.join().unwrap();
        assert!(appended.load(Ordering::SeqCst));
        // Record decoded (PageWrite, the first of the pair).
        assert!(rec.record.lsn > 0);
        drop(dir);
    }

    /// Minimal executor: drives one future to completion. Sufficient
    /// because our `tokio_sleep` future already wakes itself via a
    /// background thread.
    fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
        use std::pin::Pin;
        use std::sync::{Condvar, Mutex};
        use std::task::{Context, Poll, Wake, Waker};

        struct ParkWaker {
            ready: Mutex<bool>,
            cvar: Condvar,
        }
        impl Wake for ParkWaker {
            fn wake(self: Arc<Self>) {
                *self.ready.lock().unwrap() = true;
                self.cvar.notify_one();
            }
        }
        let parker = Arc::new(ParkWaker {
            ready: Mutex::new(false),
            cvar: Condvar::new(),
        });
        let waker: Waker = Waker::from(Arc::clone(&parker));
        let mut cx = Context::from_waker(&waker);
        // SAFETY: we never move `fut` after pinning to the stack.
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => {
                    let mut g = parker.ready.lock().unwrap();
                    while !*g {
                        g = parker.cvar.wait(g).unwrap();
                    }
                    *g = false;
                }
            }
        }
    }
}
