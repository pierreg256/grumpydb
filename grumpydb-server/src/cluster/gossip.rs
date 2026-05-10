//! v6 Phase 44: gossip-style membership probes.
//!
//! Each node periodically probes peers over the inter-node handshake path,
//! advertises its own membership view, and converges runtime topology in
//! memory.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cluster::NodeIdentity;
use crate::cluster::handshake::{
    ClusterHelloResponse, GossipPayload, PeerRpcContext, probe_peer_with_gossip,
    pull_schema_from_peer,
};
use crate::config::ClusterSection;
use crate::coordinator::Coordinator;

/// Spawn the background gossip probe task.
///
/// No-op when no peers are configured.
pub fn spawn(cluster: ClusterSection, identity: Arc<NodeIdentity>, coordinator: Arc<Coordinator>) {
    let probe_every = Duration::from_millis(cluster.gossip_probe_interval_ms.max(100));
    let dead_after = cluster.gossip_peer_dead_after_secs.max(1);
    let server_version = format!("grumpydb-server/{}", env!("CARGO_PKG_VERSION"));

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(probe_every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            let payload = coordinator.gossip_payload();
            for (node_id, addr) in coordinator.gossip_probe_targets() {
                probe_one_peer(
                    node_id.as_str(),
                    addr.as_str(),
                    dead_after,
                    &identity,
                    &server_version,
                    payload.clone(),
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
    payload: GossipPayload,
    coordinator: &Coordinator,
) {
    let now = now_unix();

    match probe_peer_with_gossip(
        addr,
        identity.cluster_id,
        identity.node_id,
        server_version,
        payload,
    )
    .await
    {
        Ok(response) => {
            coordinator.update_peer_liveness(node_id, "up", Some(now));
            // Phase 44b: opportunistically pull schema entries from
            // any peer that advertises a higher schema_version. The
            // gossip path is symmetric, so a peer that's behind us
            // will pull from us on its own next tick — no action
            // needed in that direction here.
            maybe_pull_schema(addr, identity, server_version, &response, coordinator).await;
        }
        Err(e) => {
            on_probe_failure(node_id, dead_after_secs, now, coordinator, &e);
        }
    }
}

async fn maybe_pull_schema(
    addr: &str,
    identity: &NodeIdentity,
    server_version: &str,
    response: &ClusterHelloResponse,
    coordinator: &Coordinator,
) {
    let local_version = coordinator.schema_version();
    if response.schema_version <= local_version {
        return;
    }

    let ctx = PeerRpcContext {
        addr: addr.to_string(),
        local_cluster_id: identity.cluster_id,
        local_node_id: identity.node_id,
        server_version: server_version.to_string(),
    };

    match pull_schema_from_peer(&ctx, local_version).await {
        Ok(entries) => {
            let n = entries.len();
            let applied = coordinator.apply_remote_schema_entries(&entries);
            tracing::debug!(
                addr,
                received = n,
                applied,
                local_version,
                remote_version = response.schema_version,
                "schema pull from peer"
            );
        }
        Err(e) => {
            tracing::warn!(addr, error = %e, "schema pull from peer failed");
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
