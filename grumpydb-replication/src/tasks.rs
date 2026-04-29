//! Leader-side and follower-side replication tasks.
//!
//! Slice 40e.4 of [Phase 40e](../../../docs/IMPLEMENTATION_PLAN_V4.md).
//! These types compose the lower slices into an **end-to-end** WAL
//! stream:
//!
//! - [`LeaderTask`] runs on the writer node. Once a follower's
//!   [`PeerSession`] has completed the handshake, the leader awaits
//!   the follower's [`Frame::Subscribe`] then drives a loop that
//!   pulls records from a [`WalTailer`] and ships them as
//!   [`Frame::WalRecord`] frames. It interleaves [`Frame::Heartbeat`]
//!   pings and processes inbound [`Frame::Ack`] frames to track
//!   per-follower lag.
//! - [`FollowerTask`] runs on a non-writer node. After connecting to
//!   the leader and handshaking, it sends a [`Frame::Subscribe`] from
//!   its locally-known watermark and applies each incoming
//!   [`Frame::WalRecord`] via a user-supplied [`ReplicationApplier`].
//!   Acks are sent back periodically.
//!
//! Both tasks are generic over the transport (the underlying
//! [`PeerSession`] is itself generic), so the entire protocol can be
//! exercised in unit tests over a `tokio::io::duplex` pair without
//! ever touching a TCP socket. Wiring into `tokio_rustls` lands in
//! slice 40e.6 as part of the server integration.
//!
//! ## Ack cadence
//!
//! The follower sends an [`Ack`] every [`AckPolicy::every_n`] applied
//! records, or every [`AckPolicy::interval`] elapsed wall time —
//! whichever fires first. This keeps the leader's lag estimate fresh
//! without burying the link in tiny acks during burst writes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use crate::frame::{Ack, Frame, Heartbeat, ReplicationError, Subscribe};
use crate::session::{PeerSession, SessionError};
use crate::tailer::{TailedRecord, WalTailer};

// ---------------------------------------------------------------------------
// Applier trait
// ---------------------------------------------------------------------------

/// Boundary trait implemented by the engine to apply incoming WAL
/// records on a follower.
///
/// `raw` is the canonical v2-encoded record bytes (as emitted by
/// [`crate::TailedRecord::raw`]). The applier is responsible for:
///
/// 1. Decoding the record (using [`grumpydb::wal::record::WalRecord::from_bytes_v2`]).
/// 2. Checking the **idempotency watermark** (slice 40e.5) — replay
///    of an already-applied `(origin_node_id, hlc)` MUST be a no-op
///    that returns `Ok(())`.
/// 3. Applying the record's effect to the page store + indexes,
///    then fsyncing the resulting WAL append on the follower.
///
/// The trait is async because step 3 typically involves disk I/O.
/// The implementation is expected to live in `grumpydb-server`
/// (slice 40e.6).
#[async_trait]
pub trait ReplicationApplier: Send + Sync {
    /// Applies a single canonical v2-encoded WAL record.
    async fn apply(&self, raw: &[u8]) -> Result<(), ApplyError>;
}

// Blanket impls so callers can use `Arc<T>` / `Arc<dyn ReplicationApplier>`
// transparently as the inner applier — the natural shape for the
// server-side wiring (slice 40e.6) where one concrete applier is
// shared across multiple per-peer FollowerTasks.

#[async_trait]
impl<T: ReplicationApplier + ?Sized> ReplicationApplier for Arc<T> {
    async fn apply(&self, raw: &[u8]) -> Result<(), ApplyError> {
        (**self).apply(raw).await
    }
}

/// Errors raised by the applier.
#[derive(Debug, Error)]
pub enum ApplyError {
    /// The record could not be decoded (corruption, version mismatch, …).
    #[error("apply: record decode failed: {0}")]
    Decode(String),
    /// The engine refused the record (constraint violation, etc).
    /// The follower will surface this and shut the stream down.
    #[error("apply: engine rejected record: {0}")]
    Engine(String),
    /// I/O failure while writing to the follower's local WAL.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Ack policy
// ---------------------------------------------------------------------------

/// Controls how often a follower sends [`Frame::Ack`] frames to its
/// leader.
///
/// The default policy ([`AckPolicy::default()`]) fires an ack every
/// 32 records OR every 250 ms — whichever comes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckPolicy {
    /// Send an ack at least every `every_n` applied records.
    pub every_n: u64,
    /// Send an ack at least every `interval` (wall-clock).
    pub interval: Duration,
}

impl Default for AckPolicy {
    fn default() -> Self {
        Self {
            every_n: 32,
            interval: Duration::from_millis(250),
        }
    }
}

// ---------------------------------------------------------------------------
// Leader task
// ---------------------------------------------------------------------------

/// Per-follower task driven by the leader after a successful
/// responder-side handshake.
///
/// The task lifecycle is:
///
/// 1. Wait for the follower's [`Frame::Subscribe`] payload (the
///    follower advertises its starting watermark
///    `(start_node_id, start_hlc)`).
/// 2. Loop:
///    - Pull the next record from the [`WalTailer`].
///    - If the record's `(origin_node_id, hlc)` is at or above the
///      subscriber's start, ship a [`Frame::WalRecord`] frame.
///    - Periodically send [`Frame::Heartbeat`] pings carrying the
///      leader's current HLC.
///    - Service incoming [`Frame::Ack`] frames by updating the
///      observable handle returned by
///      [`LeaderTask::peer_high_water_handle`].
/// 3. Terminate cleanly on [`Frame::Bye`] from the follower or on
///    transport error.
///
/// The task is generic over the transport `S` so it can be exercised
/// over `tokio::io::duplex` in unit tests. Production callers wrap
/// it around a `tokio_rustls::server::TlsStream<TcpStream>`.
pub struct LeaderTask<S> {
    session: PeerSession<S>,
    tailer: WalTailer,
    /// Last HLC the follower acked (0 until the first ack arrives).
    peer_high_water: Arc<AtomicU64>,
    /// How often a heartbeat ping is sent (in absence of WAL traffic).
    heartbeat_interval: Duration,
}

impl<S> LeaderTask<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    /// Constructs the task. Does NOT consume the [`Subscribe`] frame —
    /// callers drive [`Self::run`] which awaits it as the first step.
    pub fn new(session: PeerSession<S>, tailer: WalTailer) -> Self {
        Self {
            session,
            tailer,
            peer_high_water: Arc::new(AtomicU64::new(0)),
            heartbeat_interval: Duration::from_secs(1),
        }
    }

    /// Builder: overrides the default heartbeat cadence (1 s).
    pub fn with_heartbeat_interval(mut self, d: Duration) -> Self {
        self.heartbeat_interval = d;
        self
    }

    /// Returns a handle that observers can poll to see the highest
    /// HLC the follower has acknowledged. Useful for lag metrics.
    pub fn peer_high_water_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.peer_high_water)
    }

    /// Drives the task to completion.
    ///
    /// Returns `Ok(LeaderShutdown)` on a clean termination (peer Bye,
    /// or an explicit shutdown) and `Err(_)` on transport / tailer
    /// failures.
    pub async fn run(self) -> Result<LeaderShutdown, LeaderError>
    where
        S: 'static,
    {
        let LeaderTask {
            session,
            mut tailer,
            peer_high_water,
            heartbeat_interval,
        } = self;

        // ----- 1. Wait for Subscribe (still on the unsplit session).
        let mut session = session;
        let sub = match session.recv_frame().await? {
            Frame::Subscribe(s) => s,
            Frame::Bye(b) => {
                return Ok(LeaderShutdown::PeerBye(b.reason));
            }
            other => {
                session
                    .send_frame(&Frame::Bye(crate::frame::Bye {
                        reason: format!("expected Subscribe, got {:?}", other.frame_type()),
                    }))
                    .await
                    .ok();
                return Err(LeaderError::UnexpectedFrame(other.frame_type()));
            }
        };
        let Subscribe {
            start_node_id,
            start_hlc,
        } = sub;

        // ----- 2. Split the session: separate reader + writer halves.
        let (mut reader, mut writer) = session.into_split();

        // ----- 3. Reader subtask: drain inbound frames into an mpsc.
        //
        // Splitting the read path off the leader's main loop is what
        // makes full-duplex safe: `recv_frame` is NOT cancellation-safe
        // and cannot be used inside a `tokio::select!` arm. Spawning
        // a dedicated reader gives us a cancellation-safe channel
        // receiver instead.
        let (tx, mut rx) = mpsc::channel::<Result<Frame, SessionError>>(64);
        let reader_handle = tokio::spawn(async move {
            loop {
                let frame = reader.recv_frame().await;
                let is_terminal = matches!(
                    &frame,
                    Err(SessionError::Closed)
                        | Err(SessionError::Replication(_))
                        | Ok(Frame::Bye(_))
                );
                if tx.send(frame).await.is_err() {
                    // Main task dropped the receiver — shut down.
                    return;
                }
                if is_terminal {
                    return;
                }
            }
        });
        // Critical: when this `run()` future is dropped (either cleanly
        // OR via outer `tokio::task::JoinHandle::abort`), the reader
        // subtask must also be aborted. Otherwise it keeps holding the
        // `ReadHalf` of the split stream alive — `tokio::io::split`
        // shares the underlying transport via an `Arc<Mutex<_>>`, so
        // the duplex peer never observes EOF and the test deadlocks.
        let _reader_abort_on_drop = AbortOnDrop(reader_handle.abort_handle());

        // ----- 4. Streaming loop on the writer side.
        let mut last_hb = Instant::now();
        let result = loop {
            // (a) Drain any inbound frame without blocking.
            match rx.try_recv() {
                Ok(Ok(Frame::Ack(a))) => {
                    if a.applied_node_id.as_u128() == start_node_id.as_u128() {
                        update_high_water(&peer_high_water, a.applied_hlc);
                    }
                }
                Ok(Ok(Frame::Bye(b))) => {
                    break Ok(LeaderShutdown::PeerBye(b.reason));
                }
                Ok(Ok(Frame::Heartbeat(_))) => { /* informational */ }
                Ok(Ok(other)) => {
                    let _ = writer
                        .send_frame(&Frame::Bye(crate::frame::Bye {
                            reason: format!("unexpected inbound frame {:?}", other.frame_type()),
                        }))
                        .await;
                    break Err(LeaderError::UnexpectedFrame(other.frame_type()));
                }
                Ok(Err(SessionError::Closed)) => {
                    break Ok(LeaderShutdown::PeerClosed);
                }
                Ok(Err(e)) => break Err(LeaderError::Session(e)),
                Err(mpsc::error::TryRecvError::Empty) => { /* nothing — fine */ }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    // Reader subtask finished without forwarding a
                    // terminal frame (very rare — typically because the
                    // remote dropped TCP). Treat as a clean close.
                    break Ok(LeaderShutdown::PeerClosed);
                }
            }

            // (b) Pull one WAL record (non-blocking).
            let pull = tailer.poll_once().map_err(LeaderError::Replication)?;
            match pull {
                Some(rec) => {
                    if should_ship(&rec, start_node_id, start_hlc) {
                        if let Err(e) = writer.send_frame(&Frame::WalRecord(rec.raw)).await {
                            break Err(LeaderError::Session(SessionError::Replication(e)));
                        }
                        last_hb = Instant::now();
                    }
                }
                None => {
                    // (c) Heartbeat if quiet too long.
                    if last_hb.elapsed() >= heartbeat_interval {
                        if let Err(e) = writer
                            .send_frame(&Frame::Heartbeat(Heartbeat { sender_hlc: 0 }))
                            .await
                        {
                            break Err(LeaderError::Session(SessionError::Replication(e)));
                        }
                        last_hb = Instant::now();
                    }
                    // Yield + brief sleep so the runtime can schedule
                    // the reader subtask and the WAL writer.
                    sleep(Duration::from_millis(5)).await;
                }
            }
        };

        // Drop the channel receiver to unblock the reader subtask if
        // it's still alive, then wait for it.
        drop(rx);
        let _ = reader_handle.await;
        result
    }
}

/// Aborts the wrapped task when this guard is dropped.
///
/// Used by [`LeaderTask::run`] to ensure that the inner reader subtask
/// terminates whenever the outer `run()` future is dropped — including
/// the case where the outer task was aborted via
/// [`tokio::task::JoinHandle::abort`] (which does NOT propagate to
/// children spawned inside the future). Without this guard, the reader
/// subtask keeps holding the [`tokio::io::ReadHalf`] of a split stream
/// alive, leaving the duplex peer waiting indefinitely for EOF.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn update_high_water(slot: &Arc<AtomicU64>, candidate: u64) {
    let mut cur = slot.load(Ordering::Relaxed);
    while candidate > cur {
        match slot.compare_exchange_weak(cur, candidate, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => cur = actual,
        }
    }
}

/// Reasons a [`LeaderTask`] terminated cleanly.
#[derive(Debug, PartialEq, Eq)]
pub enum LeaderShutdown {
    /// Follower sent a `Bye` with the given reason.
    PeerBye(String),
    /// Stream closed at EOF without a Bye.
    PeerClosed,
}

/// Errors raised by [`LeaderTask::run`].
#[derive(Debug, Error)]
pub enum LeaderError {
    /// Session-layer error.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// Underlying replication error (codec / I/O).
    #[error(transparent)]
    Replication(#[from] ReplicationError),
    /// Follower sent an unexpected frame mid-stream.
    #[error("unexpected frame from follower: {0:?}")]
    UnexpectedFrame(crate::frame::FrameType),
}

fn should_ship(rec: &TailedRecord, start_node: Uuid, start_hlc: u64) -> bool {
    // Subscriber follows a single origin (in v5 single-writer regime
    // there's exactly one). Drop records from other origins.
    if rec.record.origin_node_id != start_node.as_u128() {
        return false;
    }
    rec.record.hlc.0 >= start_hlc
}

// ---------------------------------------------------------------------------
// Follower task
// ---------------------------------------------------------------------------

/// Per-leader task driven by a follower after a successful
/// initiator-side handshake.
///
/// 1. Sends a [`Frame::Subscribe`] from the local watermark
///    (provided to [`Self::run`]).
/// 2. Loops: receives [`Frame::WalRecord`], applies it via
///    [`ReplicationApplier::apply`], periodically acks back.
/// 3. Sends a final [`Frame::Bye`] on clean shutdown.
pub struct FollowerTask<S> {
    session: PeerSession<S>,
    applier: Arc<dyn ReplicationApplier>,
    ack_policy: AckPolicy,
    /// Monotonic high-water HLC of records this task has applied.
    /// Exposed so `/readyz` can compute lag against the leader.
    applied_high_water: Arc<AtomicU64>,
}

impl<S> FollowerTask<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    /// Constructs the task.
    pub fn new(session: PeerSession<S>, applier: Arc<dyn ReplicationApplier>) -> Self {
        Self {
            session,
            applier,
            ack_policy: AckPolicy::default(),
            applied_high_water: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Overrides the default ack policy.
    pub fn with_ack_policy(mut self, p: AckPolicy) -> Self {
        self.ack_policy = p;
        self
    }

    /// Returns a handle observers can read for the local apply
    /// watermark. Updated atomically after each `apply()` returns Ok.
    pub fn applied_high_water_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.applied_high_water)
    }

    /// Drives the task. `start_node_id` is the writer-id this
    /// follower is subscribing to (the v5 single writer); `start_hlc`
    /// is the local resume watermark — slice 40e.5 will source it
    /// from `applied_set.json`.
    pub async fn run(
        mut self,
        start_node_id: Uuid,
        start_hlc: u64,
    ) -> Result<FollowerShutdown, FollowerError> {
        // ----- 1. Send Subscribe -----
        self.session
            .send_frame(&Frame::Subscribe(Subscribe {
                start_node_id,
                start_hlc,
            }))
            .await
            .map_err(|e| FollowerError::Session(SessionError::Replication(e)))?;

        // ----- 2. Apply loop -----
        let mut last_ack = Instant::now();
        let mut applied_since_ack: u64 = 0;
        loop {
            match self.session.recv_frame().await {
                Ok(Frame::WalRecord(raw)) => {
                    // Track the HLC for the watermark BEFORE applying so
                    // we never expose an applied HLC that the engine
                    // refused.
                    let hlc = peek_record_hlc(&raw).unwrap_or(0);
                    self.applier.apply(&raw).await?;
                    if hlc > 0 {
                        update_high_water(&self.applied_high_water, hlc);
                    }
                    applied_since_ack += 1;

                    // Maybe ack.
                    if applied_since_ack >= self.ack_policy.every_n
                        || last_ack.elapsed() >= self.ack_policy.interval
                    {
                        self.session
                            .send_frame(&Frame::Ack(Ack {
                                applied_node_id: start_node_id,
                                applied_hlc: self.applied_high_water.load(Ordering::Relaxed),
                            }))
                            .await
                            .map_err(|e| FollowerError::Session(SessionError::Replication(e)))?;
                        applied_since_ack = 0;
                        last_ack = Instant::now();
                    }
                }
                Ok(Frame::Heartbeat(_)) => { /* informational */ }
                Ok(Frame::Bye(b)) => {
                    return Ok(FollowerShutdown::LeaderBye(b.reason));
                }
                Ok(other) => {
                    self.session
                        .send_frame(&Frame::Bye(crate::frame::Bye {
                            reason: format!("unexpected frame {:?}", other.frame_type()),
                        }))
                        .await
                        .ok();
                    return Err(FollowerError::UnexpectedFrame(other.frame_type()));
                }
                Err(SessionError::Closed) => return Ok(FollowerShutdown::LeaderClosed),
                Err(e) => return Err(FollowerError::Session(e)),
            }
        }
    }
}

/// Reasons a [`FollowerTask`] terminated cleanly.
#[derive(Debug, PartialEq, Eq)]
pub enum FollowerShutdown {
    /// Leader sent a Bye with the given reason.
    LeaderBye(String),
    /// Leader closed the stream at EOF.
    LeaderClosed,
}

/// Errors raised by [`FollowerTask::run`].
#[derive(Debug, Error)]
pub enum FollowerError {
    /// Session-layer error.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// Replication codec / I/O error.
    #[error(transparent)]
    Replication(#[from] ReplicationError),
    /// Apply failed.
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// Leader sent an unexpected frame mid-stream.
    #[error("unexpected frame from leader: {0:?}")]
    UnexpectedFrame(crate::frame::FrameType),
}

/// Reads the HLC field straight from a v2-encoded WAL record without
/// fully decoding it. Returns `None` if the buffer is too short.
///
/// Layout (slice 40e.2 / WAL v2 docs):
/// `[0..4] record_len, [4..12] lsn, [12..20] tx_id, [20] op_type,
/// [21..37] origin_node_id, [37..45] hlc, …`
fn peek_record_hlc(raw: &[u8]) -> Option<u64> {
    if raw.len() < 45 {
        return None;
    }
    Some(u64::from_le_bytes([
        raw[37], raw[38], raw[39], raw[40], raw[41], raw[42], raw[43], raw[44],
    ]))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use grumpydb::wal::hlc::HlcClock;
    use grumpydb::wal::writer::WalWriter;
    use tempfile::TempDir;
    use tokio::time::timeout;

    use super::*;
    use crate::frame::PROTOCOL_VERSION;
    use crate::session::{PeerAuthenticator, PeerIdentity};

    /// Captures every record the follower applies, in order.
    struct CaptureApplier {
        records: Mutex<Vec<Vec<u8>>>,
    }

    impl CaptureApplier {
        fn new() -> Self {
            Self {
                records: Mutex::new(Vec::new()),
            }
        }
        fn snapshot(&self) -> Vec<Vec<u8>> {
            self.records.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ReplicationApplier for CaptureApplier {
        async fn apply(&self, raw: &[u8]) -> Result<(), ApplyError> {
            self.records.lock().unwrap().push(raw.to_vec());
            Ok(())
        }
    }

    struct AlwaysAcceptAuth(Uuid);
    #[async_trait]
    impl PeerAuthenticator for AlwaysAcceptAuth {
        async fn verify_peer_token(&self, _t: &str) -> Result<Uuid, crate::session::AuthRejection> {
            Ok(self.0)
        }
    }

    /// Spawns a tailer + writer + temp dir; appends `n` commits.
    fn write_commits(dir: &TempDir, node_id: u128, n: usize) -> std::path::PathBuf {
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

    /// End-to-end happy-path: leader ships records produced by a real
    /// WAL writer; follower applies all of them.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_leader_follower_end_to_end() {
        let cluster = Uuid::from_u128(0xc1);
        let writer_node = Uuid::new_v4();
        let writer_node_u128 = writer_node.as_u128();
        let follower_node = Uuid::new_v4();

        // 1. Produce 5 commits = 10 WAL records (PageWrite + Commit each).
        let dir = TempDir::new().unwrap();
        let wal_path = write_commits(&dir, writer_node_u128, 5);

        // 2. Wire transport + auth.
        let (a, b) = tokio::io::duplex(64 * 1024);
        let leader_id = PeerIdentity {
            cluster_id: cluster,
            node_id: writer_node,
        };
        let follower_id = PeerIdentity {
            cluster_id: cluster,
            node_id: follower_node,
        };
        let auth_for_leader = Arc::new(AlwaysAcceptAuth(follower_node));

        // 3. Drive the responder + initiator handshakes concurrently.
        let leader_handle = tokio::spawn(async move {
            let session = PeerSession::accept_responder(a, leader_id, 0, auth_for_leader)
                .await
                .unwrap();
            let tailer = WalTailer::open(&wal_path).unwrap();
            let task =
                LeaderTask::new(session, tailer).with_heartbeat_interval(Duration::from_millis(50));
            task.run().await
        });

        let captured = Arc::new(CaptureApplier::new());
        let captured_for_task = Arc::clone(&captured);
        let follower_handle = tokio::spawn(async move {
            let session = PeerSession::connect_initiator(b, follower_id, "tok".into())
                .await
                .unwrap();
            assert_eq!(session.negotiated_version(), PROTOCOL_VERSION);
            let task = FollowerTask::new(session, captured_for_task).with_ack_policy(AckPolicy {
                every_n: 1,
                interval: Duration::from_millis(50),
            });
            task.run(writer_node, 0).await
        });

        // 4. Wait until the follower has applied all 10 records, then
        //    cleanly shut down by dropping the leader-side stream.
        let captured_for_check = Arc::clone(&captured);
        let waiter = async move {
            loop {
                if captured_for_check.snapshot().len() >= 10 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        timeout(Duration::from_secs(5), waiter)
            .await
            .expect("follower never reached 10 applied records");

        // 5. Tear the leader task down — abort the join handle, since
        //    its loop is otherwise infinite. The follower will see EOF.
        leader_handle.abort();
        let follower_res = timeout(Duration::from_secs(5), follower_handle)
            .await
            .expect("follower never terminated")
            .unwrap();
        // Follower terminates with LeaderClosed (clean EOF) once the
        // duplex peer is dropped.
        match follower_res {
            Ok(FollowerShutdown::LeaderClosed) | Ok(FollowerShutdown::LeaderBye(_)) => {}
            Err(e) => panic!("follower errored: {e:?}"),
        }
        assert_eq!(captured.snapshot().len(), 10);
    }

    /// The leader honours `start_hlc`: records strictly older than the
    /// subscriber's watermark are NOT shipped.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_leader_skips_records_below_subscribe_watermark() {
        let cluster = Uuid::from_u128(0xc2);
        let writer = Uuid::new_v4();
        let follower = Uuid::new_v4();

        let dir = TempDir::new().unwrap();
        let wal_path = write_commits(&dir, writer.as_u128(), 3);

        // Read all records to learn what HLCs exist.
        let mut probe = WalTailer::open(&wal_path).unwrap();
        let mut hlcs = Vec::new();
        while let Some(r) = probe.poll_once().unwrap() {
            hlcs.push(r.record.hlc.0);
        }
        // We expect 6 records (3 commits × 2). Pick the median HLC as
        // the watermark — the follower should receive only records
        // with HLC >= mid.
        assert_eq!(hlcs.len(), 6);
        let mid = hlcs[3];
        let expected = hlcs.iter().filter(|h| **h >= mid).count();

        // Wire the protocol.
        let (a, b) = tokio::io::duplex(64 * 1024);
        let leader_id = PeerIdentity {
            cluster_id: cluster,
            node_id: writer,
        };
        let follower_id = PeerIdentity {
            cluster_id: cluster,
            node_id: follower,
        };
        let auth = Arc::new(AlwaysAcceptAuth(follower));

        let leader_handle = tokio::spawn(async move {
            let session = PeerSession::accept_responder(a, leader_id, 0, auth)
                .await
                .unwrap();
            let tailer = WalTailer::open(&wal_path).unwrap();
            LeaderTask::new(session, tailer).run().await
        });

        let captured = Arc::new(CaptureApplier::new());
        let captured_for_task = Arc::clone(&captured);
        let follower_handle = tokio::spawn(async move {
            let session = PeerSession::connect_initiator(b, follower_id, "tok".into())
                .await
                .unwrap();
            FollowerTask::new(session, captured_for_task)
                .with_ack_policy(AckPolicy {
                    every_n: 1,
                    interval: Duration::from_millis(50),
                })
                .run(writer, mid)
                .await
        });

        // Wait until the follower has the expected number of records.
        let captured_for_check = Arc::clone(&captured);
        let waiter = async move {
            loop {
                if captured_for_check.snapshot().len() >= expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        timeout(Duration::from_secs(5), waiter)
            .await
            .expect("follower never received expected count");

        leader_handle.abort();
        let _ = timeout(Duration::from_secs(2), follower_handle).await;

        let got = captured.snapshot();
        assert_eq!(got.len(), expected);
        // Verify each shipped record is at or above the watermark.
        for r in &got {
            let hlc = peek_record_hlc(r).unwrap();
            assert!(hlc >= mid, "shipped record below watermark: {hlc} < {mid}");
        }
    }

    /// Follower's `applied_high_water_handle()` reflects progress.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_follower_high_water_advances() {
        let cluster = Uuid::from_u128(0xc3);
        let writer = Uuid::new_v4();
        let follower = Uuid::new_v4();
        let dir = TempDir::new().unwrap();
        let wal_path = write_commits(&dir, writer.as_u128(), 4);

        let (a, b) = tokio::io::duplex(64 * 1024);
        let leader_id = PeerIdentity {
            cluster_id: cluster,
            node_id: writer,
        };
        let follower_id = PeerIdentity {
            cluster_id: cluster,
            node_id: follower,
        };
        let auth = Arc::new(AlwaysAcceptAuth(follower));

        let leader_handle = tokio::spawn(async move {
            let session = PeerSession::accept_responder(a, leader_id, 0, auth)
                .await
                .unwrap();
            let tailer = WalTailer::open(&wal_path).unwrap();
            LeaderTask::new(session, tailer).run().await
        });

        let captured = Arc::new(CaptureApplier::new());
        let captured_for_task = Arc::clone(&captured);
        let session_fut = async move {
            let session = PeerSession::connect_initiator(b, follower_id, "tok".into())
                .await
                .unwrap();
            let task = FollowerTask::new(session, captured_for_task).with_ack_policy(AckPolicy {
                every_n: 1,
                interval: Duration::from_millis(20),
            });
            let watermark_handle = task.applied_high_water_handle();
            (task, watermark_handle)
        };
        let (task, watermark_handle) = session_fut.await;
        let follower_handle = tokio::spawn(task.run(writer, 0));

        // Wait for the follower to drain everything.
        let captured_for_check = Arc::clone(&captured);
        let waiter = async move {
            loop {
                if captured_for_check.snapshot().len() >= 8 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        timeout(Duration::from_secs(5), waiter)
            .await
            .expect("follower never drained");

        // Watermark should have advanced past zero.
        let hw = watermark_handle.load(Ordering::Relaxed);
        assert!(hw > 0, "applied watermark never advanced: {hw}");

        leader_handle.abort();
        let _ = timeout(Duration::from_secs(2), follower_handle).await;
    }

    /// Leader's `peer_high_water_handle()` advances as acks arrive.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_leader_peer_high_water_tracks_acks() {
        let cluster = Uuid::from_u128(0xc4);
        let writer = Uuid::new_v4();
        let follower = Uuid::new_v4();
        let dir = TempDir::new().unwrap();
        let wal_path = write_commits(&dir, writer.as_u128(), 3);

        let (a, b) = tokio::io::duplex(64 * 1024);
        let leader_id = PeerIdentity {
            cluster_id: cluster,
            node_id: writer,
        };
        let follower_id = PeerIdentity {
            cluster_id: cluster,
            node_id: follower,
        };
        let auth = Arc::new(AlwaysAcceptAuth(follower));

        // Pre-allocate the leader's peer high-water handle so we can
        // observe it from the test thread. Plumb it via a oneshot
        // channel from the spawned leader task — the alternative
        // (awaiting `accept_responder` inline before spawning the
        // follower) deadlocks because the responder blocks waiting
        // for a Hello that hasn't been sent yet.
        let (hw_tx, hw_rx) = tokio::sync::oneshot::channel::<Arc<AtomicU64>>();
        let leader_handle = tokio::spawn(async move {
            let session = PeerSession::accept_responder(a, leader_id, 0, auth)
                .await
                .unwrap();
            let tailer = WalTailer::open(&wal_path).unwrap();
            let task =
                LeaderTask::new(session, tailer).with_heartbeat_interval(Duration::from_millis(50));
            let hw = task.peer_high_water_handle();
            let _ = hw_tx.send(hw);
            task.run().await
        });

        let captured = Arc::new(CaptureApplier::new());
        let follower_handle = tokio::spawn(async move {
            let session = PeerSession::connect_initiator(b, follower_id, "tok".into())
                .await
                .unwrap();
            FollowerTask::new(session, captured)
                .with_ack_policy(AckPolicy {
                    every_n: 1,
                    interval: Duration::from_millis(20),
                })
                .run(writer, 0)
                .await
        });

        let peer_hw = hw_rx.await.unwrap();

        // Poll the leader's view of the follower's progress.
        let waiter = async {
            loop {
                if peer_hw.load(Ordering::Relaxed) > 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        timeout(Duration::from_secs(5), waiter)
            .await
            .expect("leader never observed an Ack");

        leader_handle.abort();
        let _ = timeout(Duration::from_secs(2), follower_handle).await;
    }

    /// `should_ship` filters records from other origins.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_should_ship_filters_by_origin() {
        let dir = TempDir::new().unwrap();
        let writer = Uuid::new_v4();
        let path = write_commits(&dir, writer.as_u128(), 1);
        let mut tailer = WalTailer::open(&path).unwrap();
        let rec = tailer.poll_once().unwrap().unwrap();
        // Same origin → ship if HLC threshold met.
        assert!(should_ship(&rec, writer, 0));
        // Different origin → never ship.
        let other = Uuid::new_v4();
        assert!(!should_ship(&rec, other, 0));
    }

    /// `peek_record_hlc` returns None on too-short buffers and the
    /// correct value on real records.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_peek_record_hlc() {
        // Too short.
        assert_eq!(peek_record_hlc(&[0u8; 10]), None);

        // Real record from the WAL writer.
        let dir = TempDir::new().unwrap();
        let path = write_commits(&dir, Uuid::new_v4().as_u128(), 1);
        let mut t = WalTailer::open(&path).unwrap();
        let rec = t.poll_once().unwrap().unwrap();
        let hlc = peek_record_hlc(&rec.raw).unwrap();
        assert_eq!(hlc, rec.record.hlc.0);
    }
}
