//! Wire frames for the replication protocol (Phase 40e slice 1).
//!
//! Layout — see crate-level documentation. Frames are pure data with
//! a length-prefixed binary codec; no I/O.

use std::io;

use thiserror::Error;
use uuid::Uuid;

/// Current replication wire-protocol version. Negotiated in the
/// [`Hello`]/[`HelloAck`] handshake; mismatched versions cause an
/// immediate connection close.
///
/// Bumped on **any** backwards-incompatible wire change. Forward
/// compatibility is opt-in: a peer that wants to remain interoperable
/// with older builds must continue to advertise the old version when
/// talking to them.
pub const PROTOCOL_VERSION: u16 = 1;

/// Hard upper bound on the payload portion of a single frame.
///
/// `1 MiB` is well above the largest possible WAL v2 record (an 8 KiB
/// page write + header) yet small enough to bound the memory cost of
/// a malicious oversized frame.
pub const MAX_FRAME_PAYLOAD: u32 = 1 << 20;

/// Frame header size in bytes: `payload_len(4) + frame_type(1)`.
pub const FRAME_HEADER_SIZE: usize = 5;

/// Trailer size in bytes: CRC32 of `frame_type || payload`.
pub const FRAME_TRAILER_SIZE: usize = 4;

// ---------------------------------------------------------------------------
// Frame type tag
// ---------------------------------------------------------------------------

/// One-byte tag identifying a frame's variant on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FrameType {
    /// Opening handshake from the connection initiator.
    Hello = 0x01,
    /// Acknowledgement of a [`FrameType::Hello`].
    HelloAck = 0x02,
    /// Subscription request from a follower to a leader.
    Subscribe = 0x03,
    /// One serialised WAL record (v2 wire shape).
    WalRecord = 0x04,
    /// Acknowledged apply watermark from a follower.
    Ack = 0x05,
    /// Liveness ping; carries the sender's HLC.
    Heartbeat = 0x06,
    /// Graceful close.
    Bye = 0x07,
}

impl FrameType {
    /// Parses a one-byte tag.
    pub fn from_u8(b: u8) -> Result<Self, FrameError> {
        match b {
            0x01 => Ok(Self::Hello),
            0x02 => Ok(Self::HelloAck),
            0x03 => Ok(Self::Subscribe),
            0x04 => Ok(Self::WalRecord),
            0x05 => Ok(Self::Ack),
            0x06 => Ok(Self::Heartbeat),
            0x07 => Ok(Self::Bye),
            _ => Err(FrameError::UnknownFrameType(b)),
        }
    }

    /// Returns the on-wire byte for this variant.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

// ---------------------------------------------------------------------------
// Payload structs
// ---------------------------------------------------------------------------

/// Opening handshake payload sent by the connection initiator.
///
/// Encoding: `protocol_version(u16 LE) || cluster_id(16 B) || node_id(16 B) || token_len(u16 LE) || token`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// Replication wire version the initiator speaks.
    pub protocol_version: u16,
    /// Cluster identifier (must match the peer's).
    pub cluster_id: Uuid,
    /// Node identifier of the initiator.
    pub node_id: Uuid,
    /// `cluster_peer` JWT (Phase 39) authenticating the initiator.
    pub token: String,
}

/// Response payload to a [`Hello`].
///
/// Encoding: `protocol_version(u16 LE) || node_id(16 B) || high_water_hlc(u64 LE)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloAck {
    /// Replication wire version negotiated (server responds with the
    /// minimum of its own and the initiator's version).
    pub protocol_version: u16,
    /// Node identifier of the acceptor.
    pub node_id: Uuid,
    /// Highest HLC the acceptor has durably applied. Useful for the
    /// follower to decide whether a snapshot bootstrap is required
    /// before it can request live tailing.
    pub high_water_hlc: u64,
}

/// Subscription request: "send me everything from this watermark onwards".
///
/// Encoding: `start_node_id(16 B) || start_hlc(u64 LE)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscribe {
    /// Node identifier of the WAL stream the follower wants to receive.
    /// In v5 single-writer this is always the writer's `node_id`.
    pub start_node_id: Uuid,
    /// First HLC the follower wants to *receive*. Records strictly
    /// older than `start_hlc` are skipped by the leader.
    pub start_hlc: u64,
}

/// Apply-watermark report from a follower.
///
/// Encoding: `applied_node_id(16 B) || applied_hlc(u64 LE)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ack {
    /// Node identifier whose stream this watermark applies to.
    pub applied_node_id: Uuid,
    /// Highest HLC the follower has durably applied.
    pub applied_hlc: u64,
}

/// Liveness ping carrying the sender's current HLC.
///
/// Encoding: `sender_hlc(u64 LE)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    /// Sender's current HLC (lets the receiver detect lag).
    pub sender_hlc: u64,
}

/// Graceful close. Empty payload (`reason_len(u16 LE) || reason`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Bye {
    /// Optional human-readable close reason.
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Frame enum (decoded shape)
// ---------------------------------------------------------------------------

/// A fully-decoded replication frame.
///
/// Variants whose payload carries opaque bytes (currently only
/// [`Frame::WalRecord`]) hold the raw `Vec<u8>` so the engine
/// can decode it via the existing [`grumpydb::wal::record::WalRecord::from_bytes`](https://docs.rs/grumpydb)
/// path without an extra copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Opening handshake from the initiator.
    Hello(Hello),
    /// Acknowledgement of [`Frame::Hello`].
    HelloAck(HelloAck),
    /// Subscription request.
    Subscribe(Subscribe),
    /// One serialised WAL record (opaque bytes — the engine decodes them).
    WalRecord(Vec<u8>),
    /// Apply watermark.
    Ack(Ack),
    /// Liveness ping.
    Heartbeat(Heartbeat),
    /// Graceful close.
    Bye(Bye),
}

impl Frame {
    /// Returns the [`FrameType`] tag of this variant.
    pub fn frame_type(&self) -> FrameType {
        match self {
            Frame::Hello(_) => FrameType::Hello,
            Frame::HelloAck(_) => FrameType::HelloAck,
            Frame::Subscribe(_) => FrameType::Subscribe,
            Frame::WalRecord(_) => FrameType::WalRecord,
            Frame::Ack(_) => FrameType::Ack,
            Frame::Heartbeat(_) => FrameType::Heartbeat,
            Frame::Bye(_) => FrameType::Bye,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors raised by frame encode/decode.
#[derive(Debug, Error)]
pub enum FrameError {
    /// Payload length above [`MAX_FRAME_PAYLOAD`].
    #[error("frame payload length {0} exceeds maximum {MAX_FRAME_PAYLOAD}")]
    PayloadTooLarge(u32),
    /// Frame type byte does not map to a known variant.
    #[error("unknown frame type 0x{0:02x}")]
    UnknownFrameType(u8),
    /// Decoded payload shorter than the minimum required for its type.
    #[error("truncated payload for frame type {kind:?}: need {need} bytes, got {got}")]
    TruncatedPayload {
        /// Frame variant being decoded.
        kind: FrameType,
        /// Minimum bytes required for that variant.
        need: usize,
        /// Bytes actually present.
        got: usize,
    },
    /// Trailing bytes after the payload that do not belong to this frame.
    #[error("trailing bytes after frame payload: {0}")]
    TrailingBytes(usize),
    /// CRC32 trailer mismatch.
    #[error("frame CRC mismatch")]
    CrcMismatch,
    /// String-typed field is not valid UTF-8.
    #[error("invalid UTF-8 in frame string field")]
    InvalidUtf8,
    /// String/byte length prefix exceeds the remaining payload.
    #[error("invalid length prefix in frame: {0}")]
    InvalidLengthPrefix(&'static str),
}

/// Top-level replication error type. Wider variants will be added in
/// later slices (40e.2+) when the I/O layer lands; for now the codec
/// only ever raises [`FrameError`].
#[derive(Debug, Error)]
pub enum ReplicationError {
    /// Wire-codec error.
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// Underlying I/O error.
    #[error(transparent)]
    Io(#[from] io::Error),
}

// ---------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------

/// Encodes a frame to bytes ready to write to the wire.
///
/// Returns the full on-wire envelope (`header || payload || crc`).
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, FrameError> {
    let payload = encode_payload(frame);
    if payload.len() > MAX_FRAME_PAYLOAD as usize {
        return Err(FrameError::PayloadTooLarge(payload.len() as u32));
    }
    let mut out = Vec::with_capacity(FRAME_HEADER_SIZE + payload.len() + FRAME_TRAILER_SIZE);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.push(frame.frame_type().as_u8());
    out.extend_from_slice(&payload);

    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&[frame.frame_type().as_u8()]);
    hasher.update(&payload);
    out.extend_from_slice(&hasher.finalize().to_le_bytes());

    Ok(out)
}

fn encode_payload(frame: &Frame) -> Vec<u8> {
    match frame {
        Frame::Hello(h) => {
            let token_bytes = h.token.as_bytes();
            let mut buf = Vec::with_capacity(2 + 16 + 16 + 2 + token_bytes.len());
            buf.extend_from_slice(&h.protocol_version.to_le_bytes());
            buf.extend_from_slice(h.cluster_id.as_bytes());
            buf.extend_from_slice(h.node_id.as_bytes());
            buf.extend_from_slice(&(token_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(token_bytes);
            buf
        }
        Frame::HelloAck(a) => {
            let mut buf = Vec::with_capacity(2 + 16 + 8);
            buf.extend_from_slice(&a.protocol_version.to_le_bytes());
            buf.extend_from_slice(a.node_id.as_bytes());
            buf.extend_from_slice(&a.high_water_hlc.to_le_bytes());
            buf
        }
        Frame::Subscribe(s) => {
            let mut buf = Vec::with_capacity(16 + 8);
            buf.extend_from_slice(s.start_node_id.as_bytes());
            buf.extend_from_slice(&s.start_hlc.to_le_bytes());
            buf
        }
        Frame::WalRecord(bytes) => bytes.clone(),
        Frame::Ack(a) => {
            let mut buf = Vec::with_capacity(16 + 8);
            buf.extend_from_slice(a.applied_node_id.as_bytes());
            buf.extend_from_slice(&a.applied_hlc.to_le_bytes());
            buf
        }
        Frame::Heartbeat(hb) => hb.sender_hlc.to_le_bytes().to_vec(),
        Frame::Bye(b) => {
            let reason_bytes = b.reason.as_bytes();
            let mut buf = Vec::with_capacity(2 + reason_bytes.len());
            buf.extend_from_slice(&(reason_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(reason_bytes);
            buf
        }
    }
}

// ---------------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------------

/// Result of attempting to decode a frame from a buffer.
#[derive(Debug, PartialEq, Eq)]
enum DecodeStep {
    /// Successfully decoded one frame; reports how many bytes were consumed.
    Done { frame: Frame, consumed: usize },
    /// Buffer does not yet contain a full frame.
    NeedMore,
}

/// Decodes a single frame from the head of `buf`.
///
/// Returns `Ok(Some((frame, consumed)))` on success — `consumed` is the
/// total number of bytes from `buf[0..]` that should be removed from the
/// caller's read buffer.
///
/// Returns `Ok(None)` when the buffer does not yet contain a complete
/// frame (the caller should `read` more from the socket and retry).
pub fn decode_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>, FrameError> {
    match decode_step(buf)? {
        DecodeStep::Done { frame, consumed } => Ok(Some((frame, consumed))),
        DecodeStep::NeedMore => Ok(None),
    }
}

fn decode_step(buf: &[u8]) -> Result<DecodeStep, FrameError> {
    if buf.len() < FRAME_HEADER_SIZE {
        return Ok(DecodeStep::NeedMore);
    }
    let payload_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if payload_len > MAX_FRAME_PAYLOAD {
        return Err(FrameError::PayloadTooLarge(payload_len));
    }
    let total = FRAME_HEADER_SIZE
        .saturating_add(payload_len as usize)
        .saturating_add(FRAME_TRAILER_SIZE);
    if buf.len() < total {
        return Ok(DecodeStep::NeedMore);
    }
    let kind = FrameType::from_u8(buf[4])?;
    let payload = &buf[FRAME_HEADER_SIZE..FRAME_HEADER_SIZE + payload_len as usize];

    let crc_bytes = &buf[FRAME_HEADER_SIZE + payload_len as usize..total];
    let on_wire_crc = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&[kind.as_u8()]);
    hasher.update(payload);
    if hasher.finalize() != on_wire_crc {
        return Err(FrameError::CrcMismatch);
    }

    let frame = decode_payload(kind, payload)?;
    Ok(DecodeStep::Done {
        frame,
        consumed: total,
    })
}

fn decode_payload(kind: FrameType, payload: &[u8]) -> Result<Frame, FrameError> {
    match kind {
        FrameType::Hello => {
            const FIXED: usize = 2 + 16 + 16 + 2;
            if payload.len() < FIXED {
                return Err(FrameError::TruncatedPayload {
                    kind,
                    need: FIXED,
                    got: payload.len(),
                });
            }
            let protocol_version = u16::from_le_bytes([payload[0], payload[1]]);
            let cluster_id = Uuid::from_slice(&payload[2..18])
                .map_err(|_| FrameError::InvalidLengthPrefix("cluster_id"))?;
            let node_id = Uuid::from_slice(&payload[18..34])
                .map_err(|_| FrameError::InvalidLengthPrefix("node_id"))?;
            let token_len = u16::from_le_bytes([payload[34], payload[35]]) as usize;
            let token_start = FIXED;
            let token_end = token_start
                .checked_add(token_len)
                .ok_or(FrameError::InvalidLengthPrefix("token_len overflow"))?;
            if payload.len() < token_end {
                return Err(FrameError::TruncatedPayload {
                    kind,
                    need: token_end,
                    got: payload.len(),
                });
            }
            if payload.len() != token_end {
                return Err(FrameError::TrailingBytes(payload.len() - token_end));
            }
            let token = std::str::from_utf8(&payload[token_start..token_end])
                .map_err(|_| FrameError::InvalidUtf8)?
                .to_string();
            Ok(Frame::Hello(Hello {
                protocol_version,
                cluster_id,
                node_id,
                token,
            }))
        }
        FrameType::HelloAck => {
            const FIXED: usize = 2 + 16 + 8;
            if payload.len() != FIXED {
                if payload.len() < FIXED {
                    return Err(FrameError::TruncatedPayload {
                        kind,
                        need: FIXED,
                        got: payload.len(),
                    });
                }
                return Err(FrameError::TrailingBytes(payload.len() - FIXED));
            }
            let protocol_version = u16::from_le_bytes([payload[0], payload[1]]);
            let node_id = Uuid::from_slice(&payload[2..18])
                .map_err(|_| FrameError::InvalidLengthPrefix("node_id"))?;
            let high_water_hlc = u64::from_le_bytes([
                payload[18],
                payload[19],
                payload[20],
                payload[21],
                payload[22],
                payload[23],
                payload[24],
                payload[25],
            ]);
            Ok(Frame::HelloAck(HelloAck {
                protocol_version,
                node_id,
                high_water_hlc,
            }))
        }
        FrameType::Subscribe => {
            const FIXED: usize = 16 + 8;
            if payload.len() != FIXED {
                if payload.len() < FIXED {
                    return Err(FrameError::TruncatedPayload {
                        kind,
                        need: FIXED,
                        got: payload.len(),
                    });
                }
                return Err(FrameError::TrailingBytes(payload.len() - FIXED));
            }
            let start_node_id = Uuid::from_slice(&payload[0..16])
                .map_err(|_| FrameError::InvalidLengthPrefix("start_node_id"))?;
            let start_hlc = u64::from_le_bytes([
                payload[16],
                payload[17],
                payload[18],
                payload[19],
                payload[20],
                payload[21],
                payload[22],
                payload[23],
            ]);
            Ok(Frame::Subscribe(Subscribe {
                start_node_id,
                start_hlc,
            }))
        }
        FrameType::WalRecord => Ok(Frame::WalRecord(payload.to_vec())),
        FrameType::Ack => {
            const FIXED: usize = 16 + 8;
            if payload.len() != FIXED {
                if payload.len() < FIXED {
                    return Err(FrameError::TruncatedPayload {
                        kind,
                        need: FIXED,
                        got: payload.len(),
                    });
                }
                return Err(FrameError::TrailingBytes(payload.len() - FIXED));
            }
            let applied_node_id = Uuid::from_slice(&payload[0..16])
                .map_err(|_| FrameError::InvalidLengthPrefix("applied_node_id"))?;
            let applied_hlc = u64::from_le_bytes([
                payload[16],
                payload[17],
                payload[18],
                payload[19],
                payload[20],
                payload[21],
                payload[22],
                payload[23],
            ]);
            Ok(Frame::Ack(Ack {
                applied_node_id,
                applied_hlc,
            }))
        }
        FrameType::Heartbeat => {
            const FIXED: usize = 8;
            if payload.len() != FIXED {
                if payload.len() < FIXED {
                    return Err(FrameError::TruncatedPayload {
                        kind,
                        need: FIXED,
                        got: payload.len(),
                    });
                }
                return Err(FrameError::TrailingBytes(payload.len() - FIXED));
            }
            let sender_hlc = u64::from_le_bytes([
                payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6],
                payload[7],
            ]);
            Ok(Frame::Heartbeat(Heartbeat { sender_hlc }))
        }
        FrameType::Bye => {
            if payload.len() < 2 {
                return Err(FrameError::TruncatedPayload {
                    kind,
                    need: 2,
                    got: payload.len(),
                });
            }
            let reason_len = u16::from_le_bytes([payload[0], payload[1]]) as usize;
            let total = 2usize
                .checked_add(reason_len)
                .ok_or(FrameError::InvalidLengthPrefix("reason_len overflow"))?;
            if payload.len() < total {
                return Err(FrameError::TruncatedPayload {
                    kind,
                    need: total,
                    got: payload.len(),
                });
            }
            if payload.len() != total {
                return Err(FrameError::TrailingBytes(payload.len() - total));
            }
            let reason = std::str::from_utf8(&payload[2..total])
                .map_err(|_| FrameError::InvalidUtf8)?
                .to_string();
            Ok(Frame::Bye(Bye { reason }))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sample_hello() -> Frame {
        Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            cluster_id: Uuid::nil(),
            node_id: Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            token: "eyJhbGciOiJIUzI1NiJ9.payload.signature".to_string(),
        })
    }

    fn sample_helloack() -> Frame {
        Frame::HelloAck(HelloAck {
            protocol_version: PROTOCOL_VERSION,
            node_id: Uuid::from_u128(42),
            high_water_hlc: 1_234_567,
        })
    }

    fn sample_subscribe() -> Frame {
        Frame::Subscribe(Subscribe {
            start_node_id: Uuid::from_u128(7),
            start_hlc: 9_999,
        })
    }

    fn sample_walrecord() -> Frame {
        // Fake but realistic WAL bytes (the codec is opaque on this slice).
        Frame::WalRecord(vec![0xa5; 256])
    }

    fn sample_ack() -> Frame {
        Frame::Ack(Ack {
            applied_node_id: Uuid::from_u128(7),
            applied_hlc: 9_998,
        })
    }

    fn sample_heartbeat() -> Frame {
        Frame::Heartbeat(Heartbeat {
            sender_hlc: 12_345_678,
        })
    }

    fn sample_bye() -> Frame {
        Frame::Bye(Bye {
            reason: "writer step-down".into(),
        })
    }

    fn samples() -> Vec<Frame> {
        vec![
            sample_hello(),
            sample_helloack(),
            sample_subscribe(),
            sample_walrecord(),
            sample_ack(),
            sample_heartbeat(),
            sample_bye(),
        ]
    }

    #[test]
    fn test_frametype_round_trip() {
        for code in [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07] {
            let t = FrameType::from_u8(code).unwrap();
            assert_eq!(t.as_u8(), code);
        }
    }

    #[test]
    fn test_unknown_frametype_rejected() {
        for bad in [0x00u8, 0x08, 0x42, 0xff] {
            assert!(matches!(
                FrameType::from_u8(bad),
                Err(FrameError::UnknownFrameType(_))
            ));
        }
    }

    #[test]
    fn test_encode_decode_round_trip_all_variants() {
        for original in samples() {
            let bytes = encode_frame(&original).unwrap();
            let (decoded, consumed) = decode_frame(&bytes).unwrap().unwrap();
            assert_eq!(consumed, bytes.len());
            assert_eq!(decoded, original);
        }
    }

    #[test]
    fn test_decode_partial_returns_need_more() {
        let bytes = encode_frame(&sample_hello()).unwrap();
        // Cut at every prefix length and verify NeedMore.
        for cut in 0..bytes.len() {
            let res = decode_frame(&bytes[..cut]).unwrap();
            assert!(
                res.is_none(),
                "expected NeedMore at cut={cut} (full len={})",
                bytes.len()
            );
        }
        // Full length succeeds.
        let (frame, consumed) = decode_frame(&bytes).unwrap().unwrap();
        assert_eq!(frame, sample_hello());
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_decode_two_frames_in_one_buffer() {
        let mut buf = encode_frame(&sample_subscribe()).unwrap();
        buf.extend(encode_frame(&sample_ack()).unwrap());

        let (a, n1) = decode_frame(&buf).unwrap().unwrap();
        assert_eq!(a, sample_subscribe());
        let (b, n2) = decode_frame(&buf[n1..]).unwrap().unwrap();
        assert_eq!(b, sample_ack());
        assert_eq!(n1 + n2, buf.len());
    }

    #[test]
    fn test_decode_oversize_payload_rejected() {
        // Hand-craft a header announcing > MAX_FRAME_PAYLOAD bytes.
        let mut buf = (MAX_FRAME_PAYLOAD + 1).to_le_bytes().to_vec();
        buf.push(FrameType::WalRecord.as_u8());
        let err = decode_frame(&buf).unwrap_err();
        assert!(matches!(err, FrameError::PayloadTooLarge(_)));
    }

    #[test]
    fn test_decode_unknown_type_after_full_frame_rejected() {
        // Build a complete-length frame whose type byte is unknown.
        let payload = vec![1u8, 2, 3];
        let mut buf = (payload.len() as u32).to_le_bytes().to_vec();
        buf.push(0xee); // unknown type
        buf.extend_from_slice(&payload);
        // CRC is irrelevant — type check fires first.
        buf.extend_from_slice(&[0u8; 4]);
        let err = decode_frame(&buf).unwrap_err();
        assert!(matches!(err, FrameError::UnknownFrameType(0xee)));
    }

    #[test]
    fn test_decode_corrupted_crc_rejected() {
        let mut bytes = encode_frame(&sample_helloack()).unwrap();
        // Flip a bit inside the payload (CRC won't match).
        let mid = FRAME_HEADER_SIZE + 1;
        bytes[mid] ^= 0xff;
        let err = decode_frame(&bytes).unwrap_err();
        assert!(matches!(err, FrameError::CrcMismatch));
    }

    #[test]
    fn test_encode_oversize_payload_rejected() {
        let huge = vec![0u8; (MAX_FRAME_PAYLOAD as usize) + 1];
        let err = encode_frame(&Frame::WalRecord(huge)).unwrap_err();
        assert!(matches!(err, FrameError::PayloadTooLarge(_)));
    }

    #[test]
    fn test_decode_helloack_truncated_rejected() {
        // Header says payload_len = 5 but HelloAck needs 26.
        let mut buf = 5u32.to_le_bytes().to_vec();
        buf.push(FrameType::HelloAck.as_u8());
        buf.extend_from_slice(&[0u8; 5]);
        // Compute correct CRC so we surface the *truncation* error, not CRC.
        let mut h = crc32fast::Hasher::new();
        h.update(&[FrameType::HelloAck.as_u8()]);
        h.update(&[0u8; 5]);
        buf.extend_from_slice(&h.finalize().to_le_bytes());
        let err = decode_frame(&buf).unwrap_err();
        assert!(matches!(err, FrameError::TruncatedPayload { .. }));
    }

    #[test]
    fn test_frame_type_method_consistent() {
        for f in samples() {
            let bytes = encode_frame(&f).unwrap();
            assert_eq!(bytes[4], f.frame_type().as_u8());
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Any random frame survives a round-trip through encode → decode.
        #[test]
        fn proptest_round_trip_arbitrary_payloads(
            payload_seed in any::<u64>(),
            kind_pick in 0u8..7,
            wal_payload in proptest::collection::vec(any::<u8>(), 0..2048),
            token in "[ -~]{0,128}",
            reason in "[ -~]{0,128}",
        ) {
            let frame = match kind_pick {
                0 => Frame::Hello(Hello {
                    protocol_version: PROTOCOL_VERSION,
                    cluster_id: Uuid::from_u128(payload_seed as u128),
                    node_id: Uuid::from_u128((payload_seed as u128).wrapping_mul(31)),
                    token,
                }),
                1 => Frame::HelloAck(HelloAck {
                    protocol_version: PROTOCOL_VERSION,
                    node_id: Uuid::from_u128(payload_seed as u128),
                    high_water_hlc: payload_seed,
                }),
                2 => Frame::Subscribe(Subscribe {
                    start_node_id: Uuid::from_u128(payload_seed as u128),
                    start_hlc: payload_seed,
                }),
                3 => Frame::WalRecord(wal_payload),
                4 => Frame::Ack(Ack {
                    applied_node_id: Uuid::from_u128(payload_seed as u128),
                    applied_hlc: payload_seed,
                }),
                5 => Frame::Heartbeat(Heartbeat { sender_hlc: payload_seed }),
                _ => Frame::Bye(Bye { reason }),
            };
            let bytes = encode_frame(&frame).unwrap();
            let (decoded, consumed) = decode_frame(&bytes).unwrap().unwrap();
            prop_assert_eq!(decoded, frame);
            prop_assert_eq!(consumed, bytes.len());
        }
    }
}
