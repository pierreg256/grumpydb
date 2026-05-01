//! v6 Phase 44: gossip-style membership probes.
//!
//! This first tranche keeps the implementation intentionally lightweight:
//! each node periodically probes configured peers over the existing
//! inter-node handshake port and updates in-memory peer liveness exposed by
//! `TOPOLOGY`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::net::TcpStream;

use crate::cluster::NodeIdentity;
use crate::cluster::handshake::{HandshakeOutcome, run_initiator};
use crate::config::ClusterSection;
use crate::coordinator::Coordinator;

/// Spawn the background gossip probe task.
///
/// No-op when no peers are configured.
pub fn spawn(cluster: ClusterSection, identity: Arc<NodeIdentity>, coordinator: Arc<Coordinator>) {
    if cluster.peers.is_empty() {
        return;
    }

    let probe_every = Duration::from_millis(cluster.gossip_probe_interval_ms.max(100));
    let dead_after = cluster.gossip_peer_dead_after_secs.max(1);
    let server_version = format!("grumpydb-server/{}", env!("CARGO_PKG_VERSION"));

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(probe_every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            for peer in &cluster.peers {
                probe_one_peer(
                    peer.node_id.as_str(),
                    peer.addr.as_str(),
                    dead_after,
                    &identity,
                    &server_version,
                    coordinator.as_ref(),
                )
                .await;
            }
        }
    });
}

async fn probe_one_peer(
    node_id: &str,
    addr: &str,
    dead_after_secs: u64,
    identity: &NodeIdentity,
    server_version: &str,
    coordinator: &Coordinator,
) {
    let now = now_unix();

    let stream = TcpStream::connect(addr).await;
    let mut stream = match stream {
        Ok(s) => s,
        Err(e) => {
            on_probe_failure(
                node_id,
                dead_after_secs,
                now,
                coordinator,
                &format!("connect failed: {e}"),
            );
            return;
        }
    };

    match run_initiator(
        &mut stream,
        identity.cluster_id,
        identity.node_id,
        server_version,
    )
    .await
    {
        Ok((_, HandshakeOutcome::Accepted { .. })) => {
            coordinator.update_peer_liveness(node_id, "up", Some(now));
        }
        Ok((_, HandshakeOutcome::Rejected { error })) => {
            on_probe_failure(
                node_id,
                dead_after_secs,
                now,
                coordinator,
                &format!("handshake rejected: {error}"),
            );
        }
        Err(e) => {
            on_probe_failure(
                node_id,
                dead_after_secs,
                now,
                coordinator,
                &format!("handshake error: {e}"),
            );
        }
    }
}

fn on_probe_failure(
    node_id: &str,
    dead_after_secs: u64,
    now: u64,
    coordinator: &Coordinator,
    reason: &str,
) {
    let status = match coordinator.peer_last_seen(node_id) {
        Some(last_seen) if now.saturating_sub(last_seen) <= dead_after_secs => "suspect",
        _ => "down",
    };

    tracing::debug!(node_id, status, reason, "gossip probe failure");
    coordinator.update_peer_liveness(node_id, status, None);
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::NodeIdentity;
    use crate::config::{ClusterSection, PeerEntry};
    use uuid::Uuid;

    fn identity() -> NodeIdentity {
        NodeIdentity {
            node_id: Uuid::new_v4(),
            cluster_id: Uuid::new_v4(),
            created_at_unix: 0,
            identity_version: 1,
        }
    }

    #[test]
    fn test_on_probe_failure_marks_down_when_no_last_seen() {
        let mut cluster = ClusterSection::default();
        cluster.peers.push(PeerEntry {
            node_id: "11111111-1111-1111-1111-111111111111".to_string(),
            addr: "127.0.0.1:6399".to_string(),
            status: None,
            last_seen_at_unix: None,
            vnode_assignments: vec![],
        });
        let coord = Coordinator::from_config(&identity(), &cluster, "127.0.0.1:6380");
        on_probe_failure(
            "11111111-1111-1111-1111-111111111111",
            5,
            100,
            &coord,
            "connect failed",
        );

        let topo = coord.topology_json();
        let peers = topo
            .get("peers")
            .and_then(|v| v.as_array())
            .expect("peers array");
        let peer = peers
            .iter()
            .find(|p| {
                p.get("node_id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| id == "11111111-1111-1111-1111-111111111111")
            })
            .expect("peer entry");
        assert_eq!(peer.get("status").and_then(|v| v.as_str()), Some("down"));
    }
}
