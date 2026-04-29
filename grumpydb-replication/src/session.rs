//! Authenticated peer session over an async stream.
//!
//! Slice 40e.3 of [Phase 40e](../../../docs/IMPLEMENTATION_PLAN_V4.md). The
//! session sits **above** the wire codec ([`crate::frame`]) and **below**
//! the leader/follower tasks (slice 40e.4). Its single responsibility is
//! to:
//!
//! 1. Ferry [`Frame`]s in both directions over an async byte stream
//!    using the length-prefixed binary codec.
//! 2. Run the **opening handshake**: negotiate protocol version,
//!    confirm the cluster identity, authenticate the initiator's
//!    `cluster_peer` JWT (Phase 39).
//!
//! The session is generic over the transport
//! (`S: AsyncRead + AsyncWrite + Unpin + Send`) so production callers
//! can use a `tokio_rustls::server::TlsStream` / `TlsStream<TcpStream>`,
//! while unit tests use a paired `tokio::io::duplex` to exercise the
//! whole protocol without TLS or sockets.
//!
//! ## Authentication boundary
//!
//! JWT verification is delegated to a trait
//! [`PeerAuthenticator`] supplied by the upper layer (the server's
//! `AuthStore` will implement it). The replication crate therefore
//! stays decoupled from the JWT/RSA code and can be unit-tested with
//! a fake authenticator.
//!
//! ## Handshake protocol
//!
//! ```text
//!   initiator                              responder
//!   ─────────                              ─────────
//!     Hello { version, cluster_id,         (validate cluster_id matches,
//!             node_id, token } ─────────►  verify token via PeerAuthenticator,
//!                                          check token's node_id matches Hello.node_id,
//!                                          intersect protocol_version)
//!     ◄────────────────────── HelloAck { negotiated_version,
//!                                        node_id, high_water_hlc }
//! ```
//!
//! On any handshake failure (cluster mismatch, token rejection,
//! version below `MIN_SUPPORTED_VERSION`, malformed payload), the
//! responder closes the connection with a [`Frame::Bye`] carrying a
//! short reason string before dropping the stream — this gives the
//! initiator's logs an actionable diagnostic.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::frame::{
    Bye, Frame, FrameError, Hello, HelloAck, MAX_FRAME_PAYLOAD, PROTOCOL_VERSION, ReplicationError,
    decode_frame, encode_frame,
};

/// Lowest replication wire version this build is willing to talk.
/// The handshake refuses anything below this number.
pub const MIN_SUPPORTED_VERSION: u16 = 1;

/// The local node's published identity for the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    /// Cluster this node belongs to. Both ends must agree.
    pub cluster_id: Uuid,
    /// Stable identifier of the local node.
    pub node_id: Uuid,
}

/// Token-verification trait implemented by the upper layer.
///
/// The replication crate calls this once per inbound handshake; the
/// implementation is expected to:
///
/// 1. Validate the JWT signature using the cluster's public RS256 key
///    (loaded from `_auth/jwt/rs256_current.pub`, with `next_pub`
///    accepted during rotation).
/// 2. Confirm the token carries the **`cluster_peer` role** and is not
///    expired.
/// 3. Return the **issuing node's** `node_id` (extracted from a
///    custom claim such as `node_id` in the JWT body).
///
/// Any verification error must be returned as
/// [`AuthRejection::InvalidToken`] — never as a panic.
#[async_trait]
pub trait PeerAuthenticator: Send + Sync {
    /// Verifies `token` and returns the issuing node's id on success.
    async fn verify_peer_token(&self, token: &str) -> Result<Uuid, AuthRejection>;
}

/// Reasons a token is rejected.
#[derive(Debug, Error)]
pub enum AuthRejection {
    /// Signature, expiry, audience, role, or shape failure.
    #[error("invalid cluster_peer token: {0}")]
    InvalidToken(String),
}

/// Handshake-time errors. Wrap the lower-level codec errors and the
/// I/O layer.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Underlying transport / codec error.
    #[error(transparent)]
    Replication(#[from] ReplicationError),
    /// Frame received in a state where it is not allowed (e.g. a
    /// `Subscribe` arrived before the `Hello`).
    #[error("unexpected frame during handshake: {0:?}")]
    UnexpectedFrame(crate::frame::FrameType),
    /// Stream closed before the expected frame arrived.
    #[error("connection closed during handshake")]
    Closed,
    /// Cluster identifier mismatch between initiator and responder.
    #[error("cluster_id mismatch: local={local}, remote={remote}")]
    ClusterMismatch {
        /// Local cluster id.
        local: Uuid,
        /// Remote-claimed cluster id.
        remote: Uuid,
    },
    /// Initiator advertised a wire version this build cannot speak.
    #[error("protocol version {peer} below minimum supported {min}")]
    VersionTooLow {
        /// Version advertised by the peer.
        peer: u16,
        /// Local minimum.
        min: u16,
    },
    /// Initiator advertised a wire version *higher* than this build,
    /// and we are unwilling to downgrade. (Phase 40e v1 only speaks
    /// version 1, so this fires only for hypothetical future peers.)
    #[error("protocol version {peer} above local maximum {max}")]
    VersionTooHigh {
        /// Version advertised by the peer.
        peer: u16,
        /// Local maximum.
        max: u16,
    },
    /// JWT verification failed.
    #[error("authentication rejected: {0}")]
    AuthRejected(#[from] AuthRejection),
    /// `Hello.node_id` does not match the `node_id` claim inside the
    /// JWT (token-vs-payload spoofing attempt).
    #[error("Hello.node_id ({hello}) does not match token's node_id ({token})")]
    NodeIdSpoofing {
        /// `node_id` advertised in the `Hello` payload.
        hello: Uuid,
        /// `node_id` extracted from the verified token.
        token: Uuid,
    },
}

/// An authenticated, framed channel between two peers.
///
/// Constructed via [`PeerSession::connect_initiator`] or
/// [`PeerSession::accept_responder`]. After the handshake, callers
/// shuttle [`Frame`]s with [`PeerSession::send_frame`] /
/// [`PeerSession::recv_frame`]. The session owns its read buffer so
/// partial frames at the TCP boundary are handled transparently.
pub struct PeerSession<S> {
    stream: S,
    /// Remote node's identifier, obtained from the handshake.
    remote_node_id: Uuid,
    /// On the initiator side: the high-water HLC reported by the
    /// responder in its `HelloAck`. `None` on the responder side
    /// (the initiator never sends one).
    remote_high_water_hlc: Option<u64>,
    /// Negotiated wire version.
    negotiated_version: u16,
    /// Read buffer holding bytes that have been pulled from the
    /// transport but not yet decoded into a full frame.
    read_buf: Vec<u8>,
}

impl<S> std::fmt::Debug for PeerSession<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerSession")
            .field("remote_node_id", &self.remote_node_id)
            .field("remote_high_water_hlc", &self.remote_high_water_hlc)
            .field("negotiated_version", &self.negotiated_version)
            .field("read_buf_len", &self.read_buf.len())
            .finish()
    }
}

impl<S> PeerSession<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    /// Returns the remote peer's `node_id` (set during the handshake).
    pub fn remote_node_id(&self) -> Uuid {
        self.remote_node_id
    }

    /// Returns the remote peer's high-water HLC (initiator side only).
    pub fn remote_high_water_hlc(&self) -> Option<u64> {
        self.remote_high_water_hlc
    }

    /// Returns the negotiated wire version.
    pub fn negotiated_version(&self) -> u16 {
        self.negotiated_version
    }

    /// Drives the **initiator** side of the handshake: send `Hello`,
    /// receive `HelloAck`.
    ///
    /// `token` is the local node's `cluster_peer` JWT (issued at
    /// bootstrap; see Phase 39 `_auth/cluster_peer.token`).
    pub async fn connect_initiator(
        mut stream: S,
        local: PeerIdentity,
        token: String,
    ) -> Result<Self, SessionError> {
        let hello = Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            cluster_id: local.cluster_id,
            node_id: local.node_id,
            token,
        });
        write_frame_raw(&mut stream, &hello).await?;
        let mut read_buf = Vec::with_capacity(256);
        let frame = read_one_frame(&mut stream, &mut read_buf).await?;
        match frame {
            Frame::HelloAck(ack) => {
                if ack.protocol_version < MIN_SUPPORTED_VERSION {
                    return Err(SessionError::VersionTooLow {
                        peer: ack.protocol_version,
                        min: MIN_SUPPORTED_VERSION,
                    });
                }
                if ack.protocol_version > PROTOCOL_VERSION {
                    return Err(SessionError::VersionTooHigh {
                        peer: ack.protocol_version,
                        max: PROTOCOL_VERSION,
                    });
                }
                Ok(Self {
                    stream,
                    remote_node_id: ack.node_id,
                    remote_high_water_hlc: Some(ack.high_water_hlc),
                    negotiated_version: ack.protocol_version,
                    read_buf,
                })
            }
            Frame::Bye(b) => Err(SessionError::Replication(ReplicationError::Io(
                std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    format!("responder closed: {}", b.reason),
                ),
            ))),
            other => Err(SessionError::UnexpectedFrame(other.frame_type())),
        }
    }

    /// Drives the **responder** side of the handshake: receive `Hello`,
    /// validate cluster + JWT, send `HelloAck`.
    ///
    /// `local` is the local node's identity (used for cluster validation
    /// and the `HelloAck` payload). `local_high_water_hlc` is the
    /// highest HLC durably applied locally — reported in the `HelloAck`
    /// so the initiator can decide whether a snapshot bootstrap is
    /// needed before live tailing.
    ///
    /// On any rejection, a [`Frame::Bye`] is sent with a short reason
    /// before the error propagates. Best-effort — if the stream is
    /// already broken the Bye write silently fails.
    pub async fn accept_responder(
        mut stream: S,
        local: PeerIdentity,
        local_high_water_hlc: u64,
        authenticator: Arc<dyn PeerAuthenticator>,
    ) -> Result<Self, SessionError> {
        let mut read_buf = Vec::with_capacity(256);
        let frame = read_one_frame(&mut stream, &mut read_buf).await?;
        let hello = match frame {
            Frame::Hello(h) => h,
            other => {
                let _ = write_bye(&mut stream, "expected Hello").await;
                return Err(SessionError::UnexpectedFrame(other.frame_type()));
            }
        };

        // 1. Cluster id must match.
        if hello.cluster_id != local.cluster_id {
            let _ = write_bye(&mut stream, "cluster_id mismatch").await;
            return Err(SessionError::ClusterMismatch {
                local: local.cluster_id,
                remote: hello.cluster_id,
            });
        }

        // 2. Protocol version intersection.
        if hello.protocol_version < MIN_SUPPORTED_VERSION {
            let _ = write_bye(&mut stream, "protocol version below minimum").await;
            return Err(SessionError::VersionTooLow {
                peer: hello.protocol_version,
                min: MIN_SUPPORTED_VERSION,
            });
        }
        // The responder picks min(local_max, peer). With a single
        // version available today this is always PROTOCOL_VERSION.
        let negotiated = hello.protocol_version.min(PROTOCOL_VERSION);

        // 3. Verify token via the supplied authenticator.
        let token_node_id = match authenticator.verify_peer_token(&hello.token).await {
            Ok(id) => id,
            Err(e) => {
                let _ = write_bye(&mut stream, "auth rejected").await;
                return Err(SessionError::AuthRejected(e));
            }
        };

        // 4. Token-vs-payload consistency: the node_id baked into the
        //    JWT must match the one the peer claims in its Hello.
        if token_node_id != hello.node_id {
            let _ = write_bye(&mut stream, "node_id spoofing").await;
            return Err(SessionError::NodeIdSpoofing {
                hello: hello.node_id,
                token: token_node_id,
            });
        }

        let ack = Frame::HelloAck(HelloAck {
            protocol_version: negotiated,
            node_id: local.node_id,
            high_water_hlc: local_high_water_hlc,
        });
        write_frame_raw(&mut stream, &ack).await?;

        Ok(Self {
            stream,
            remote_node_id: hello.node_id,
            remote_high_water_hlc: None,
            negotiated_version: negotiated,
            read_buf,
        })
    }

    /// Sends a frame.
    pub async fn send_frame(&mut self, frame: &Frame) -> Result<(), ReplicationError> {
        write_frame_raw(&mut self.stream, frame).await
    }

    /// Awaits the next frame from the peer.
    ///
    /// Returns [`SessionError::Closed`] when the stream is closed
    /// cleanly between frames (a graceful termination from the peer
    /// **without** an explicit `Bye` is treated as `Closed`).
    pub async fn recv_frame(&mut self) -> Result<Frame, SessionError> {
        match read_one_frame(&mut self.stream, &mut self.read_buf).await {
            Ok(f) => Ok(f),
            Err(SessionError::Closed) => Err(SessionError::Closed),
            Err(e) => Err(e),
        }
    }

    /// Best-effort send a `Bye` and shut the stream down cleanly.
    /// Errors are swallowed — this is invoked when the upper layer
    /// has already decided to tear the session down.
    pub async fn close(mut self, reason: impl Into<String>) {
        let _ = write_bye(&mut self.stream, &reason.into()).await;
        let _ = self.stream.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// I/O helpers (free functions so they're shared by both connect and accept)
// ---------------------------------------------------------------------------

async fn write_frame_raw<W>(stream: &mut W, frame: &Frame) -> Result<(), ReplicationError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let bytes = encode_frame(frame).map_err(ReplicationError::Frame)?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

async fn write_bye<W>(stream: &mut W, reason: &str) -> Result<(), ReplicationError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let bye = Frame::Bye(Bye {
        reason: reason.to_string(),
    });
    write_frame_raw(stream, &bye).await
}

/// Reads bytes from `stream` until `read_buf` contains a full frame,
/// returns the decoded frame, and drains the consumed bytes from
/// `read_buf`. Maps a clean EOF (zero bytes returned with an empty
/// pending buffer) to [`SessionError::Closed`].
async fn read_one_frame<R>(stream: &mut R, read_buf: &mut Vec<u8>) -> Result<Frame, SessionError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        // 1. Try to decode something already buffered.
        match decode_frame(read_buf) {
            Ok(Some((frame, consumed))) => {
                read_buf.drain(..consumed);
                return Ok(frame);
            }
            Ok(None) => { /* fall through to read more */ }
            Err(e) => {
                return Err(SessionError::Replication(ReplicationError::Frame(e)));
            }
        }
        // 2. Read more bytes from the transport.
        let mut tmp = [0u8; 8192];
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| SessionError::Replication(ReplicationError::Io(e)))?;
        if n == 0 {
            // EOF.
            if read_buf.is_empty() {
                return Err(SessionError::Closed);
            }
            // Some bytes lingered without a complete frame — surface
            // as a frame-codec truncation error so callers get
            // actionable telemetry rather than a silent close.
            return Err(SessionError::Replication(ReplicationError::Frame(
                FrameError::TruncatedPayload {
                    kind: crate::frame::FrameType::Hello, // best-effort tag — unknown at this layer
                    need: MAX_FRAME_PAYLOAD as usize,
                    got: read_buf.len(),
                },
            )));
        }
        read_buf.extend_from_slice(&tmp[..n]);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Heartbeat, Subscribe};

    /// Test authenticator that accepts a hard-coded token and embeds
    /// a configured `node_id`. Mimics what the real `AuthStore`-backed
    /// implementation will do once Phase 40e.4 wires it in.
    struct FakeAuth {
        accepted_token: String,
        token_node_id: Uuid,
    }

    #[async_trait]
    impl PeerAuthenticator for FakeAuth {
        async fn verify_peer_token(&self, token: &str) -> Result<Uuid, AuthRejection> {
            if token == self.accepted_token {
                Ok(self.token_node_id)
            } else {
                Err(AuthRejection::InvalidToken("bad token".into()))
            }
        }
    }

    fn ids() -> (Uuid, Uuid, Uuid) {
        let cluster = Uuid::from_u128(0x42);
        let initiator = Uuid::from_u128(0xa);
        let responder = Uuid::from_u128(0xb);
        (cluster, initiator, responder)
    }

    fn make_auth(token: &str, expected_node_id: Uuid) -> Arc<dyn PeerAuthenticator> {
        Arc::new(FakeAuth {
            accepted_token: token.to_string(),
            token_node_id: expected_node_id,
        })
    }

    #[tokio::test]
    async fn test_handshake_success_two_way_traffic() {
        let (cluster, initiator, responder) = ids();
        let token = "valid-jwt".to_string();
        let auth = make_auth(&token, initiator);

        let (a, b) = tokio::io::duplex(64 * 1024);

        let init_id = PeerIdentity {
            cluster_id: cluster,
            node_id: initiator,
        };
        let resp_id = PeerIdentity {
            cluster_id: cluster,
            node_id: responder,
        };

        let init_token = token.clone();
        let init_handle =
            tokio::spawn(
                async move { PeerSession::connect_initiator(a, init_id, init_token).await },
            );
        let resp_handle =
            tokio::spawn(async move { PeerSession::accept_responder(b, resp_id, 42, auth).await });

        let (init_res, resp_res) = tokio::join!(init_handle, resp_handle);
        let mut initiator_sess = init_res.unwrap().unwrap();
        let mut responder_sess = resp_res.unwrap().unwrap();

        // Identity wired correctly on both sides.
        assert_eq!(initiator_sess.remote_node_id(), responder);
        assert_eq!(initiator_sess.remote_high_water_hlc(), Some(42));
        assert_eq!(initiator_sess.negotiated_version(), PROTOCOL_VERSION);

        assert_eq!(responder_sess.remote_node_id(), initiator);
        assert_eq!(responder_sess.remote_high_water_hlc(), None);
        assert_eq!(responder_sess.negotiated_version(), PROTOCOL_VERSION);

        // Round-trip a payload frame to verify post-handshake I/O.
        initiator_sess
            .send_frame(&Frame::Heartbeat(Heartbeat { sender_hlc: 999 }))
            .await
            .unwrap();
        let received = responder_sess.recv_frame().await.unwrap();
        match received {
            Frame::Heartbeat(hb) => assert_eq!(hb.sender_hlc, 999),
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handshake_cluster_mismatch_rejected() {
        let (cluster_a, initiator, responder) = ids();
        let cluster_b = Uuid::from_u128(0x99);
        let auth = make_auth("token", initiator);

        let (a, b) = tokio::io::duplex(64 * 1024);
        let init_id = PeerIdentity {
            cluster_id: cluster_b,
            node_id: initiator,
        };
        let resp_id = PeerIdentity {
            cluster_id: cluster_a,
            node_id: responder,
        };

        let init_handle = tokio::spawn(async move {
            PeerSession::connect_initiator(a, init_id, "token".into()).await
        });
        let resp_handle =
            tokio::spawn(async move { PeerSession::accept_responder(b, resp_id, 0, auth).await });

        let (init_res, resp_res) = tokio::join!(init_handle, resp_handle);
        let resp_err = resp_res.unwrap().unwrap_err();
        assert!(
            matches!(resp_err, SessionError::ClusterMismatch { .. }),
            "unexpected: {resp_err:?}"
        );
        // Initiator should have received the Bye and surfaced an
        // error too (not necessarily the same shape — it sees a
        // ConnectionRefused-flavoured I/O error from the Bye frame).
        let init_err = init_res.unwrap().unwrap_err();
        assert!(matches!(init_err, SessionError::Replication(_)));
    }

    #[tokio::test]
    async fn test_handshake_bad_token_rejected() {
        let (cluster, initiator, responder) = ids();
        let auth = make_auth("the-good-one", initiator);

        let (a, b) = tokio::io::duplex(64 * 1024);
        let init_id = PeerIdentity {
            cluster_id: cluster,
            node_id: initiator,
        };
        let resp_id = PeerIdentity {
            cluster_id: cluster,
            node_id: responder,
        };

        let init_handle = tokio::spawn(async move {
            PeerSession::connect_initiator(a, init_id, "the-bad-one".into()).await
        });
        let resp_handle =
            tokio::spawn(async move { PeerSession::accept_responder(b, resp_id, 0, auth).await });

        let (init_res, resp_res) = tokio::join!(init_handle, resp_handle);
        let resp_err = resp_res.unwrap().unwrap_err();
        assert!(
            matches!(resp_err, SessionError::AuthRejected(_)),
            "unexpected: {resp_err:?}"
        );
        let init_err = init_res.unwrap().unwrap_err();
        assert!(matches!(init_err, SessionError::Replication(_)));
    }

    #[tokio::test]
    async fn test_handshake_node_id_spoofing_rejected() {
        let (cluster, initiator, responder) = ids();
        // Authenticator says the token belongs to `initiator`, but the
        // initiator advertises a DIFFERENT node_id in its Hello.
        let auth = make_auth("token", initiator);
        let liar = Uuid::from_u128(0xdead);

        let (a, b) = tokio::io::duplex(64 * 1024);
        let init_id = PeerIdentity {
            cluster_id: cluster,
            node_id: liar,
        };
        let resp_id = PeerIdentity {
            cluster_id: cluster,
            node_id: responder,
        };

        let init_handle = tokio::spawn(async move {
            PeerSession::connect_initiator(a, init_id, "token".into()).await
        });
        let resp_handle =
            tokio::spawn(async move { PeerSession::accept_responder(b, resp_id, 0, auth).await });

        let (init_res, resp_res) = tokio::join!(init_handle, resp_handle);
        let resp_err = resp_res.unwrap().unwrap_err();
        assert!(
            matches!(resp_err, SessionError::NodeIdSpoofing { .. }),
            "unexpected: {resp_err:?}"
        );
        let init_err = init_res.unwrap().unwrap_err();
        assert!(matches!(init_err, SessionError::Replication(_)));
    }

    #[tokio::test]
    async fn test_handshake_responder_receives_unexpected_first_frame() {
        let (cluster, _initiator, responder) = ids();
        let auth = make_auth("tok", Uuid::nil());

        let (mut a, b) = tokio::io::duplex(64 * 1024);
        let resp_id = PeerIdentity {
            cluster_id: cluster,
            node_id: responder,
        };

        let resp_handle =
            tokio::spawn(async move { PeerSession::accept_responder(b, resp_id, 0, auth).await });

        // Send a Subscribe BEFORE any Hello — illegal per the protocol.
        let bad = Frame::Subscribe(Subscribe {
            start_node_id: Uuid::nil(),
            start_hlc: 0,
        });
        let bytes = encode_frame(&bad).unwrap();
        a.write_all(&bytes).await.unwrap();
        a.flush().await.unwrap();

        let resp_err = resp_handle.await.unwrap().unwrap_err();
        assert!(
            matches!(resp_err, SessionError::UnexpectedFrame(_)),
            "unexpected: {resp_err:?}"
        );
    }

    #[tokio::test]
    async fn test_recv_frame_after_clean_close_reports_closed() {
        let (cluster, initiator, responder) = ids();
        let auth = make_auth("token", initiator);

        let (a, b) = tokio::io::duplex(64 * 1024);
        let init_id = PeerIdentity {
            cluster_id: cluster,
            node_id: initiator,
        };
        let resp_id = PeerIdentity {
            cluster_id: cluster,
            node_id: responder,
        };

        let init_handle = tokio::spawn(async move {
            PeerSession::connect_initiator(a, init_id, "token".into()).await
        });
        let resp_handle =
            tokio::spawn(async move { PeerSession::accept_responder(b, resp_id, 0, auth).await });

        let (init_res, resp_res) = tokio::join!(init_handle, resp_handle);
        let initiator_sess = init_res.unwrap().unwrap();
        let mut responder_sess = resp_res.unwrap().unwrap();

        // Initiator drops without sending a Bye → responder sees Closed.
        drop(initiator_sess);
        let err = responder_sess.recv_frame().await.unwrap_err();
        assert!(matches!(err, SessionError::Closed));
    }

    #[tokio::test]
    async fn test_close_sends_bye_then_shuts_down() {
        let (cluster, initiator, responder) = ids();
        let auth = make_auth("token", initiator);

        let (a, b) = tokio::io::duplex(64 * 1024);
        let init_id = PeerIdentity {
            cluster_id: cluster,
            node_id: initiator,
        };
        let resp_id = PeerIdentity {
            cluster_id: cluster,
            node_id: responder,
        };

        let init_handle = tokio::spawn(async move {
            PeerSession::connect_initiator(a, init_id, "token".into()).await
        });
        let resp_handle =
            tokio::spawn(async move { PeerSession::accept_responder(b, resp_id, 0, auth).await });

        let (init_res, resp_res) = tokio::join!(init_handle, resp_handle);
        let initiator_sess = init_res.unwrap().unwrap();
        let mut responder_sess = resp_res.unwrap().unwrap();

        initiator_sess.close("done").await;
        let frame = responder_sess.recv_frame().await.unwrap();
        match frame {
            Frame::Bye(b) => assert_eq!(b.reason, "done"),
            other => panic!("expected Bye, got {other:?}"),
        }
    }
}
