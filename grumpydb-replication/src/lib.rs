//! WAL-stream replication for GrumpyDB (Phase 40e).
//!
//! This crate provides the **wire protocol** and **streaming primitives**
//! used to ship WAL records between cluster peers. It is consumed by
//! `grumpydb-server` to expose the peer-to-peer replication endpoints
//! described in
//! [`docs/IMPLEMENTATION_PLAN_V4.md`](../../docs/IMPLEMENTATION_PLAN_V4.md)
//! Phase 40e.
//!
//! ## Slice 40e.1 — wire frames
//!
//! This first slice ships only the **pure** layer: typed frames + a
//! length-prefixed binary codec, with no I/O dependency. Upstream
//! consumers (server, peer session, WAL tailer) can be developed
//! against the codec alone and unit-tested in isolation.
//!
//! ### Frame layout
//!
//! Every frame uses a uniform 5-byte header:
//!
//! ```text
//! [0..4]  payload_len: u32 (LE)
//! [4]     frame_type:  u8
//! [5..]   payload bytes (length = payload_len)
//! ```
//!
//! The frame is followed by a `u32 LE` CRC32 checksum computed over
//! `frame_type || payload`. The full on-wire size of a frame is
//! `5 + payload_len + 4` bytes.
//!
//! Frame types defined in this slice:
//!
//! | Code | Name           | Direction         | Purpose                                   |
//! |------|----------------|-------------------|-------------------------------------------|
//! | 0x01 | `Hello`        | initiator → peer  | Opening handshake (cluster id, node id, JWT) |
//! | 0x02 | `HelloAck`     | peer → initiator  | Accept handshake; report peer high-water |
//! | 0x03 | `Subscribe`    | follower → leader | Request stream from `(node_id, hlc)`     |
//! | 0x04 | `WalRecord`    | leader → follower | One serialised v2 WAL record             |
//! | 0x05 | `Ack`          | follower → leader | Apply progress watermark                 |
//! | 0x06 | `Heartbeat`    | bidirectional     | Liveness ping (carries sender HLC)       |
//! | 0x07 | `Bye`          | either            | Graceful close                           |
//!
//! Bytes outside that table are reserved; an unknown frame type is a
//! protocol error and the connection is closed with [`FrameError::UnknownFrameType`].
//!
//! ### Maximum frame size
//!
//! Frames are bounded by [`MAX_FRAME_PAYLOAD`]. WAL records carry
//! page-sized payloads (8 KiB) plus headers; the limit is generous
//! enough to fit any future record without forcing fragmentation, but
//! tight enough to bound a malicious peer's memory amplification.

#![cfg_attr(
    not(test),
    warn(clippy::unwrap_used, clippy::panic, clippy::expect_used)
)]
#![warn(missing_docs)]

mod frame;
mod idempotent;
mod lag_tracker;
mod session;
mod tailer;
mod tasks;
mod writer_control;

pub use frame::{
    Ack, Bye, Frame, FrameError, FrameType, Heartbeat, Hello, HelloAck, MAX_FRAME_PAYLOAD,
    PROTOCOL_VERSION, ReplicationError, Subscribe, decode_frame, encode_frame,
};
pub use idempotent::{IdempotentApplier, resume_hlc_for};
pub use lag_tracker::LagTracker;
pub use session::{
    AuthRejection, MIN_SUPPORTED_VERSION, PeerAuthenticator, PeerIdentity, PeerSession,
    SessionError,
};
pub use tailer::{DEFAULT_POLL_INTERVAL, TailedRecord, WalTailer};
pub use tasks::{
    AckPolicy, ApplyError, FollowerError, FollowerShutdown, FollowerTask, LeaderError,
    LeaderShutdown, LeaderTask, ReplicationApplier,
};
pub use writer_control::{WriterAssignment, WriterNotAllowed};
