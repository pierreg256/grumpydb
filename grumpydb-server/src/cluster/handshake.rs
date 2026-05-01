//! Cluster handshake protocol.
//!
//! When two nodes connect over the inter-node port (`cluster.listen_peer`)
//! they immediately exchange a JSON-encoded `ClusterHello` /
//! `ClusterHelloResponse` pair so each side can verify the other belongs
//! to the same cluster *and* is the peer the static config expects.
//!
//! # Wire format (v5/v6/v7 contract)
//!
//! Both messages are length-prefixed JSON: a 4-byte big-endian unsigned
//! length followed by exactly that many UTF-8 JSON bytes. Length is
//! capped at [`MAX_HELLO_BYTES`] to bound parsing memory.
//!
//! # Why JSON
//!
//! Handshake happens once per connection, so encoding cost is irrelevant
//! and human-readable framing is convenient for debugging. Phase 40e
//! switches to a binary frame for the WAL stream itself.
//!
//! # Reserved fields
//!
//! `capabilities`, `status`, `last_seen_at_unix`, `vnode_assignments`
//! are present in the schema even though v5 ignores them: v6 (gossip)
//! will populate them so v5 → v6 is a behavior change, not a config
//! schema change.

use std::collections::HashSet;
use std::io;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use uuid::Uuid;

use crate::cluster::NodeIdentity;
use crate::config::ServerConfig;

/// Maximum size of a single handshake frame (including the JSON body
/// but excluding the 4-byte length prefix). 64 KiB is wildly larger
/// than any plausible handshake payload — the cap exists strictly to
/// stop a malicious peer from forcing an OOM allocation.
pub const MAX_HELLO_BYTES: usize = 64 * 1024;

/// First message from initiator to acceptor on a peer connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClusterHello {
    /// Cluster identifier of the initiator. Must match the acceptor's.
    pub cluster_id: Uuid,
    /// Node identifier of the initiator. Must appear in the acceptor's
    /// static peer list.
    pub node_id: Uuid,
    /// Free-form server version string (e.g. crate version + git sha).
    pub server_version: String,
    /// Reserved capability flags for forward compatibility. v6 will use
    /// them to negotiate gossip vs. direct WAL streaming.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// Acceptor's reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClusterHelloResponse {
    /// Cluster identifier of the acceptor (echoed for confirmation).
    pub cluster_id: Uuid,
    /// Node identifier of the acceptor.
    pub node_id: Uuid,
    /// Free-form server version string of the acceptor.
    pub server_version: String,
    /// `true` when both `cluster_id` and `node_id` checks passed and
    /// the connection is accepted; `false` otherwise.
    pub accepted: bool,
    /// Machine-readable error tag when `accepted == false`. Known
    /// values: `cluster_id_mismatch`, `unknown_peer`, `protocol_error`.
    pub error: Option<String>,
}

/// Outcome of [`run_acceptor`] / [`run_initiator`].
#[derive(Debug, Clone)]
pub enum HandshakeOutcome {
    /// Handshake succeeded; the connection may be reused for the
    /// real protocol (WAL stream, gossip, …).
    Accepted {
        /// Identity of the remote peer.
        peer_node_id: Uuid,
    },
    /// Handshake completed but the acceptor refused. The connection
    /// MUST be closed by the caller.
    Rejected {
        /// Machine-readable error tag.
        error: String,
    },
}

/// Errors returned by the framed I/O helpers.
#[derive(thiserror::Error, Debug)]
pub enum HandshakeError {
    /// Underlying I/O failure or unexpected EOF.
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    /// JSON encode/decode failure.
    #[error("malformed handshake JSON: {0}")]
    Malformed(String),
    /// Length prefix exceeded [`MAX_HELLO_BYTES`].
    #[error("handshake frame too large: {len} bytes (max {max})", max = MAX_HELLO_BYTES)]
    FrameTooLarge {
        /// The advertised length, in bytes.
        len: u32,
    },
}

/// Write a length-prefixed JSON frame to `stream`.
async fn write_frame<W, T>(stream: &mut W, value: &T) -> Result<(), HandshakeError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value).map_err(|e| HandshakeError::Malformed(e.to_string()))?;
    if bytes.len() > MAX_HELLO_BYTES {
        return Err(HandshakeError::FrameTooLarge {
            len: bytes.len() as u32,
        });
    }
    let len = (bytes.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Read a length-prefixed JSON frame from `stream`.
async fn read_frame<R, T>(stream: &mut R) -> Result<T, HandshakeError>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if (len as usize) > MAX_HELLO_BYTES {
        return Err(HandshakeError::FrameTooLarge { len });
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(|e| HandshakeError::Malformed(e.to_string()))
}

/// Drive the acceptor side of a handshake.
///
/// Reads the initiator's [`ClusterHello`], validates it against
/// `local_cluster_id` and `known_peers`, then writes
/// [`ClusterHelloResponse`]. The connection is *not* closed by this
/// function — the caller decides what to do with it next (Phase 40e
/// will start the WAL stream when `Accepted`).
pub async fn run_acceptor<S>(
    stream: &mut S,
    local_cluster_id: Uuid,
    local_node_id: Uuid,
    server_version: &str,
    known_peers: &HashSet<Uuid>,
) -> Result<HandshakeOutcome, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let hello: ClusterHello = read_frame(stream).await?;

    let (accepted, error) = if hello.cluster_id != local_cluster_id {
        (false, Some("cluster_id_mismatch".to_string()))
    } else if !known_peers.contains(&hello.node_id) {
        (false, Some("unknown_peer".to_string()))
    } else {
        (true, None)
    };

    let response = ClusterHelloResponse {
        cluster_id: local_cluster_id,
        node_id: local_node_id,
        server_version: server_version.to_string(),
        accepted,
        error: error.clone(),
    };
    write_frame(stream, &response).await?;

    Ok(if accepted {
        HandshakeOutcome::Accepted {
            peer_node_id: hello.node_id,
        }
    } else {
        HandshakeOutcome::Rejected {
            error: error.unwrap_or_else(|| "unknown".to_string()),
        }
    })
}

/// Drive the initiator side of a handshake.
///
/// Sends [`ClusterHello`] and reads back the acceptor's
/// [`ClusterHelloResponse`].
pub async fn run_initiator<S>(
    stream: &mut S,
    local_cluster_id: Uuid,
    local_node_id: Uuid,
    server_version: &str,
) -> Result<(ClusterHelloResponse, HandshakeOutcome), HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let hello = ClusterHello {
        cluster_id: local_cluster_id,
        node_id: local_node_id,
        server_version: server_version.to_string(),
        capabilities: Vec::new(),
    };
    write_frame(stream, &hello).await?;

    let response: ClusterHelloResponse = read_frame(stream).await?;
    let outcome = if response.accepted {
        HandshakeOutcome::Accepted {
            peer_node_id: response.node_id,
        }
    } else {
        HandshakeOutcome::Rejected {
            error: response
                .error
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
        }
    };
    Ok((response, outcome))
}

/// Probe one peer using the cluster handshake and return `Ok(())` if the
/// peer accepts this node as a valid cluster member.
pub async fn probe_peer_acceptance(
    addr: &str,
    local_cluster_id: Uuid,
    local_node_id: Uuid,
    server_version: &str,
) -> Result<(), String> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect failed: {e}"))?;

    let (_, outcome) = run_initiator(&mut stream, local_cluster_id, local_node_id, server_version)
        .await
        .map_err(|e| format!("handshake error: {e}"))?;

    match outcome {
        HandshakeOutcome::Accepted { .. } => Ok(()),
        HandshakeOutcome::Rejected { error } => Err(format!("handshake rejected: {error}")),
    }
}

/// Phase 40a peer-port stub.
///
/// Binds `config.cluster.listen_peer` and accepts inter-node TCP
/// connections, performing only the [`run_acceptor`] handshake before
/// closing the socket. Phase 40e will replace the close with the WAL
/// streaming loop. The function returns as soon as the listener is
/// bound; the accept loop runs in a detached background task so the
/// main TCP listener can come up immediately afterwards.
///
/// Returns `Ok(())` on a clean bind. Bind failures propagate as an
/// `Err` so the operator sees them at startup rather than as silent
/// background warnings.
pub async fn serve(
    config: ServerConfig,
    identity: Arc<NodeIdentity>,
) -> Result<(), Box<dyn std::error::Error>> {
    let bind = config.cluster.listen_peer.clone();
    if bind.is_empty() {
        return Ok(());
    }

    // Build the static peer set and validate it against `node.json`.
    let known_peers: HashSet<Uuid> = config
        .cluster
        .peers
        .iter()
        .filter_map(|p| Uuid::parse_str(&p.node_id).ok())
        .collect();

    // If `cluster_id` is configured, sanity-check it against the
    // on-disk identity. Mismatch is fatal — the operator is mixing
    // two different clusters' state.
    if let Some(configured) = &config.cluster.cluster_id {
        match Uuid::parse_str(configured) {
            Ok(u) if u != identity.cluster_id => {
                return Err(format!(
                    "cluster.cluster_id ({u}) does not match node.json ({})",
                    identity.cluster_id
                )
                .into());
            }
            Err(e) => {
                return Err(format!("cluster.cluster_id is not a valid UUID: {e}").into());
            }
            _ => {}
        }
    }

    let listener = TcpListener::bind(&bind).await?;
    let local = listener.local_addr()?;
    tracing::info!(
        bind = %local,
        node_id = %identity.node_id,
        cluster_id = %identity.cluster_id,
        peers = known_peers.len(),
        "cluster peer listener bound (handshake-only stub; Phase 40e adds WAL stream)"
    );

    let server_version = format!("grumpydb-server/{}", env!("CARGO_PKG_VERSION"));

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut stream, peer)) => {
                    let known = known_peers.clone();
                    let identity = identity.clone();
                    let server_version = server_version.clone();
                    tokio::spawn(async move {
                        match run_acceptor(
                            &mut stream,
                            identity.cluster_id,
                            identity.node_id,
                            &server_version,
                            &known,
                        )
                        .await
                        {
                            Ok(HandshakeOutcome::Accepted { peer_node_id }) => {
                                tracing::info!(
                                    peer = %peer,
                                    peer_node_id = %peer_node_id,
                                    "cluster peer handshake accepted"
                                );
                            }
                            Ok(HandshakeOutcome::Rejected { error }) => {
                                tracing::warn!(
                                    peer = %peer,
                                    error,
                                    "cluster peer handshake rejected"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(peer = %peer, error = %e, "cluster handshake I/O error");
                            }
                        }
                        // Phase 40a: close after handshake. Phase 40e
                        // will graft the WAL streaming loop here.
                        let _ = stream.shutdown().await;
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "cluster peer accept failed");
                }
            }
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    fn version() -> &'static str {
        "grumpydb-test/0.0.0"
    }

    #[tokio::test]
    async fn test_handshake_accepts_matching_cluster_id() {
        let cluster_id = Uuid::new_v4();
        let node_a = Uuid::new_v4();
        let node_b = Uuid::new_v4();

        let (mut a, mut b) = duplex(8192);

        let mut peers_for_b = HashSet::new();
        peers_for_b.insert(node_a);

        let acceptor = tokio::spawn(async move {
            run_acceptor(&mut b, cluster_id, node_b, version(), &peers_for_b).await
        });
        let initiator =
            tokio::spawn(async move { run_initiator(&mut a, cluster_id, node_a, version()).await });

        let acc = acceptor.await.unwrap().unwrap();
        let (resp, init_outcome) = initiator.await.unwrap().unwrap();

        assert!(matches!(
            acc,
            HandshakeOutcome::Accepted { peer_node_id } if peer_node_id == node_a
        ));
        assert!(matches!(
            init_outcome,
            HandshakeOutcome::Accepted { peer_node_id } if peer_node_id == node_b
        ));
        assert!(resp.accepted);
        assert!(resp.error.is_none());
        assert_eq!(resp.cluster_id, cluster_id);
        assert_eq!(resp.node_id, node_b);
    }

    #[tokio::test]
    async fn test_handshake_rejects_mismatched_cluster_id() {
        let cluster_a = Uuid::new_v4();
        let cluster_b = Uuid::new_v4();
        let node_a = Uuid::new_v4();
        let node_b = Uuid::new_v4();

        let (mut a, mut b) = duplex(8192);
        let mut peers_for_b = HashSet::new();
        peers_for_b.insert(node_a);

        let acceptor = tokio::spawn(async move {
            run_acceptor(&mut b, cluster_b, node_b, version(), &peers_for_b).await
        });
        let initiator =
            tokio::spawn(async move { run_initiator(&mut a, cluster_a, node_a, version()).await });

        let acc = acceptor.await.unwrap().unwrap();
        let (resp, init_outcome) = initiator.await.unwrap().unwrap();

        assert!(matches!(
            acc,
            HandshakeOutcome::Rejected { ref error } if error == "cluster_id_mismatch"
        ));
        assert!(matches!(
            init_outcome,
            HandshakeOutcome::Rejected { ref error } if error == "cluster_id_mismatch"
        ));
        assert!(!resp.accepted);
        assert_eq!(resp.error.as_deref(), Some("cluster_id_mismatch"));
    }

    #[tokio::test]
    async fn test_handshake_rejects_unknown_node_id() {
        let cluster_id = Uuid::new_v4();
        let node_a = Uuid::new_v4();
        let node_b = Uuid::new_v4();
        let other = Uuid::new_v4();

        let (mut a, mut b) = duplex(8192);
        // node_a is NOT in node_b's known peers
        let mut peers_for_b = HashSet::new();
        peers_for_b.insert(other);

        let acceptor = tokio::spawn(async move {
            run_acceptor(&mut b, cluster_id, node_b, version(), &peers_for_b).await
        });
        let initiator =
            tokio::spawn(async move { run_initiator(&mut a, cluster_id, node_a, version()).await });

        let acc = acceptor.await.unwrap().unwrap();
        let (resp, init_outcome) = initiator.await.unwrap().unwrap();

        assert!(matches!(
            acc,
            HandshakeOutcome::Rejected { ref error } if error == "unknown_peer"
        ));
        assert!(matches!(
            init_outcome,
            HandshakeOutcome::Rejected { ref error } if error == "unknown_peer"
        ));
        assert!(!resp.accepted);
        assert_eq!(resp.error.as_deref(), Some("unknown_peer"));
    }

    #[tokio::test]
    async fn test_handshake_frame_too_large_rejected() {
        // Build an initiator with a huge capabilities vec that exceeds
        // MAX_HELLO_BYTES. write_frame should reject before any I/O.
        let cluster_id = Uuid::new_v4();
        let node_a = Uuid::new_v4();

        let (mut a, _b) = duplex(8192);
        let huge = "x".repeat(MAX_HELLO_BYTES);
        let hello = ClusterHello {
            cluster_id,
            node_id: node_a,
            server_version: version().into(),
            capabilities: vec![huge],
        };
        let err = write_frame(&mut a, &hello).await.unwrap_err();
        assert!(
            matches!(err, HandshakeError::FrameTooLarge { .. }),
            "{err:?}"
        );
    }
}
