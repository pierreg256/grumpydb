//! Coordinator helpers for routing and protocol validation.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::join_all;
use grumpydb_ring::{Ring, RingConfig, RoutingKey};
use parking_lot::RwLock;
use serde_json::json;
use tokio::time::Duration;
use tokio::time::timeout;
use uuid::Uuid;

use crate::cluster::NodeIdentity;
use crate::cluster::handshake::{
    ClusterHello, GossipPayload, PeerGossipState, probe_peer_acceptance,
};
use crate::config::{ClusterSection, PeerEntry, WriterEntry};

/// Lightweight server-side coordinator.
#[derive(Debug)]
pub struct Coordinator {
    local_node_id: String,
    local_node_uuid: Uuid,
    cluster_id: String,
    cluster_uuid: Uuid,
    n: usize,
    write_ack_timeout_ms: u64,
    vnodes_per_node: u32,
    ring: Ring<String>,
    peer_addrs: BTreeMap<String, String>,
    writers: Vec<WriterEntry>,
    peers: RwLock<BTreeMap<String, PeerRuntime>>,
}

#[derive(Debug, Clone)]
struct PeerRuntime {
    addr: String,
    status: String,
    last_seen_at_unix: Option<u64>,
    vnode_assignments: Vec<u32>,
}

impl Coordinator {
    /// Build coordinator state from static config and durable node identity.
    pub fn from_config(
        identity: &NodeIdentity,
        cluster: &ClusterSection,
        local_addr: &str,
    ) -> Self {
        let mut ring = Ring::new(RingConfig {
            vnodes_per_node: cluster.vnodes_per_node,
        });

        let local_node_id = identity.node_id.to_string();
        let cluster_id = identity.cluster_id.to_string();

        // Always include local node so single-node mode routes cleanly.
        ring.add_node(local_node_id.clone());

        let mut peer_addrs = BTreeMap::new();
        peer_addrs.insert(local_node_id.clone(), local_addr.to_string());

        let mut peers = BTreeMap::new();
        peers.insert(
            local_node_id.clone(),
            PeerRuntime {
                addr: local_addr.to_string(),
                status: "up".to_string(),
                last_seen_at_unix: Some(now_unix()),
                vnode_assignments: default_vnode_assignments(cluster.vnodes_per_node),
            },
        );

        for PeerEntry {
            node_id,
            addr,
            status,
            last_seen_at_unix,
            vnode_assignments,
        } in &cluster.peers
        {
            ring.add_node(node_id.clone());
            peer_addrs.insert(node_id.clone(), addr.clone());
            peers.insert(
                node_id.clone(),
                PeerRuntime {
                    addr: addr.clone(),
                    status: status.clone().unwrap_or_else(|| "unknown".to_string()),
                    last_seen_at_unix: *last_seen_at_unix,
                    vnode_assignments: if vnode_assignments.is_empty() {
                        default_vnode_assignments(cluster.vnodes_per_node)
                    } else {
                        vnode_assignments.clone()
                    },
                },
            );
        }

        let total_nodes = 1 + cluster.peers.len();
        // v6 default replication factor: N = min(3, cluster_size).
        let n = total_nodes.clamp(1, 3);

        Self {
            local_node_id,
            local_node_uuid: identity.node_id,
            cluster_id,
            cluster_uuid: identity.cluster_id,
            n,
            write_ack_timeout_ms: cluster.write_ack_timeout_ms,
            vnodes_per_node: cluster.vnodes_per_node,
            ring,
            peer_addrs,
            writers: cluster.writers.clone(),
            peers: RwLock::new(peers),
        }
    }

    /// Update peer liveness fields from the gossip runtime.
    pub fn update_peer_liveness(
        &self,
        node_id: &str,
        status: &str,
        last_seen_at_unix: Option<u64>,
    ) {
        let mut peers = self.peers.write();
        if let Some(peer) = peers.get_mut(node_id) {
            peer.status = status.to_string();
            if last_seen_at_unix.is_some() {
                peer.last_seen_at_unix = last_seen_at_unix;
            }
        }
    }

    /// Return the last-seen timestamp for a peer, if known.
    pub fn peer_last_seen(&self, node_id: &str) -> Option<u64> {
        self.peers
            .read()
            .get(node_id)
            .and_then(|p| p.last_seen_at_unix)
    }

    /// Runtime list of peers to probe for gossip convergence.
    pub fn gossip_probe_targets(&self) -> Vec<(String, String)> {
        self.peers
            .read()
            .iter()
            .filter_map(|(node_id, peer)| {
                if node_id == &self.local_node_id || peer.addr.is_empty() {
                    None
                } else {
                    Some((node_id.clone(), peer.addr.clone()))
                }
            })
            .collect()
    }

    /// Export local gossip payload for outgoing probes.
    pub fn gossip_payload(&self) -> GossipPayload {
        let peers = self.peers.read();
        let local = peers.get(&self.local_node_id);
        let status = local.map(|p| p.status.clone());
        let last_seen_at_unix = local.and_then(|p| p.last_seen_at_unix);
        let vnode_assignments = local
            .map(|p| p.vnode_assignments.clone())
            .unwrap_or_else(|| default_vnode_assignments(self.vnodes_per_node));

        let membership = peers
            .iter()
            .map(|(node_id, peer)| PeerGossipState {
                node_id: node_id.clone(),
                addr: peer.addr.clone(),
                status: peer.status.clone(),
                last_seen_at_unix: peer.last_seen_at_unix,
                vnode_assignments: peer.vnode_assignments.clone(),
            })
            .collect();

        GossipPayload {
            status,
            last_seen_at_unix,
            vnode_assignments,
            membership,
        }
    }

    /// Merge incoming gossip data received through a peer handshake.
    pub fn merge_gossip_from_peer(
        &self,
        peer_node_id: &str,
        hello: &ClusterHello,
        observed_at_unix: Option<u64>,
    ) {
        let mut peers = self.peers.write();

        let src_status = hello.status.as_deref().unwrap_or("up");
        let src_last_seen = hello.last_seen_at_unix.or(observed_at_unix);
        merge_peer_runtime(
            &mut peers,
            peer_node_id,
            None,
            src_status,
            src_last_seen,
            &hello.vnode_assignments,
            self.vnodes_per_node,
        );

        for entry in &hello.membership {
            if entry.node_id == self.local_node_id {
                continue;
            }
            merge_peer_runtime(
                &mut peers,
                &entry.node_id,
                Some(&entry.addr),
                &entry.status,
                entry.last_seen_at_unix,
                &entry.vnode_assignments,
                self.vnodes_per_node,
            );
        }
    }

    /// Validate read/write concerns.
    ///
    /// v6 Phase 45 keeps read concerns pinned to `R=1` (Phase 47 will
    /// enable `R>1`). Write concerns are bounded here and enforced at
    /// key-level by [`Self::validate_write_concern_for_key`].
    pub fn validate_concerns(
        &self,
        read_concern: Option<u16>,
        write_concern: Option<u16>,
    ) -> Result<(), String> {
        let r = read_concern.unwrap_or(1) as usize;
        let w = write_concern.unwrap_or(1) as usize;

        if r != 1 {
            return Err(
                "v6 currently supports R=1 only (read repair lands in phase 47)".to_string(),
            );
        }

        if !(1..=self.n).contains(&r) || !(1..=self.n).contains(&w) {
            return Err(format!(
                "invalid consistency concerns: require R and W in [1, {}]",
                self.n
            ));
        }

        Ok(())
    }

    /// Validate write concern against the key's currently-live replica set.
    pub fn validate_write_concern_for_key(
        &self,
        database: &str,
        collection: &str,
        key_bytes: &[u8],
        write_concern: Option<u16>,
    ) -> Result<(), String> {
        let w = write_concern.unwrap_or(1) as usize;
        if !(1..=self.n).contains(&w) {
            return Err(format!(
                "invalid consistency concerns: require R and W in [1, {}]",
                self.n
            ));
        }

        let key = RoutingKey {
            database,
            collection,
            key_bytes,
        };
        let owners = self.ring.preference_list(&key, self.n);
        let live = owners
            .iter()
            .filter(|node_id| self.is_peer_live(node_id))
            .count();
        if w > live {
            return Err(format!(
                "not enough live replicas for W={w}: have {live} live replicas in preference list"
            ));
        }

        Ok(())
    }

    /// Fan out write acknowledgements to replica peers and wait until quorum
    /// `W` is satisfied or timeout/failures leave insufficient acknowledgements.
    pub async fn wait_for_write_ack_quorum(
        &self,
        database: &str,
        collection: &str,
        key_bytes: &[u8],
        write_concern: Option<u16>,
    ) -> Result<(), String> {
        let w = write_concern.unwrap_or(1) as usize;
        if w <= 1 {
            return Ok(());
        }

        let key = RoutingKey {
            database,
            collection,
            key_bytes,
        };
        let owners = self.ring.preference_list(&key, self.n);

        let mut acked = 1usize; // local node ack
        let mut fanout_addrs = Vec::new();
        for node_id in owners {
            if node_id == self.local_node_id {
                continue;
            }
            if let Some(addr) = self.peer_addrs.get(&node_id) {
                fanout_addrs.push(addr.clone());
            }
        }

        let required_remote = w.saturating_sub(1);
        if fanout_addrs.len() < required_remote {
            return Err(format!(
                "write quorum cannot be satisfied: need {required_remote} remote acks, only {} replica peers available",
                fanout_addrs.len()
            ));
        }

        let timeout_dur = Duration::from_millis(self.write_ack_timeout_ms.max(50));
        let server_version = format!("grumpydb-server/{}", env!("CARGO_PKG_VERSION"));
        let probes = fanout_addrs.into_iter().map(|addr| {
            let server_version = server_version.clone();
            async move {
                let r = timeout(
                    timeout_dur,
                    probe_peer_acceptance(
                        &addr,
                        self.cluster_uuid,
                        self.local_node_uuid,
                        &server_version,
                    ),
                )
                .await;

                match r {
                    Ok(Ok(())) => Ok::<(), String>(()),
                    Ok(Err(e)) => Err(format!("{addr}: {e}")),
                    Err(_) => Err(format!(
                        "{addr}: timeout after {}ms",
                        timeout_dur.as_millis()
                    )),
                }
            }
        });

        let results = join_all(probes).await;
        let mut failures = Vec::new();
        for res in results {
            match res {
                Ok(()) => acked += 1,
                Err(e) => failures.push(e),
            }
        }
        if acked >= w {
            return Ok(());
        }

        Err(format!(
            "write quorum not reached: acked {acked}/{w}; failures: {}",
            failures.join(" | ")
        ))
    }

    /// Enforce primary-owner placement for read paths and return a forward
    /// hint if needed.
    pub fn enforce_local_owner(
        &self,
        database: &str,
        collection: &str,
        key_bytes: &[u8],
    ) -> Result<(), String> {
        let key = RoutingKey {
            database,
            collection,
            key_bytes,
        };
        let owners = self.ring.preference_list(&key, self.n);
        if owners.is_empty() {
            return Ok(());
        }
        if owners[0] == self.local_node_id {
            return Ok(());
        }

        let owner_node = owners[0].clone();
        let owner_addr = self
            .peer_addrs
            .get(&owner_node)
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        Err(format!(
            "forward to {owner_node}@{owner_addr}; not the owner"
        ))
    }

    /// Enforce v6 multi-writer replica placement for write paths.
    ///
    /// During phase 45, writes are accepted on any node that belongs to the
    /// preference list of size `N` for the key.
    pub fn enforce_local_write_replica(
        &self,
        database: &str,
        collection: &str,
        key_bytes: &[u8],
    ) -> Result<(), String> {
        let key = RoutingKey {
            database,
            collection,
            key_bytes,
        };
        let owners = self.ring.preference_list(&key, self.n);
        if owners.iter().any(|n| n == &self.local_node_id) {
            return Ok(());
        }

        let owner_node = owners
            .first()
            .cloned()
            .unwrap_or_else(|| self.local_node_id.clone());
        let owner_addr = self
            .peer_addrs
            .get(&owner_node)
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        Err(format!(
            "forward to {owner_node}@{owner_addr}; local node is outside write replica set"
        ))
    }

    /// Return the static topology snapshot exposed by `TOPOLOGY`.
    pub fn topology_json(&self) -> serde_json::Value {
        let peers: Vec<serde_json::Value> = self
            .peers
            .read()
            .iter()
            .map(|(node_id, peer)| {
                json!({
                    "node_id": node_id,
                    "addr": peer.addr,
                    "status": peer.status,
                    "last_seen_at_unix": peer.last_seen_at_unix,
                    "vnode_assignments": peer.vnode_assignments,
                })
            })
            .collect();

        let writers: Vec<serde_json::Value> = self
            .writers
            .iter()
            .map(|w| {
                json!({
                    "collection": w.collection,
                    "node_id": w.node_id,
                })
            })
            .collect();

        json!({
            "cluster_id": self.cluster_id,
            "local_node_id": self.local_node_id,
            "n": self.n,
            "write_ack_timeout_ms": self.write_ack_timeout_ms,
            "vnodes_per_node": self.vnodes_per_node,
            "peers": peers,
            "writers": writers,
        })
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn default_vnode_assignments(vnodes_per_node: u32) -> Vec<u32> {
    (0..vnodes_per_node).collect()
}

fn merge_peer_runtime(
    peers: &mut BTreeMap<String, PeerRuntime>,
    node_id: &str,
    addr: Option<&str>,
    status: &str,
    last_seen_at_unix: Option<u64>,
    vnode_assignments: &[u32],
    vnodes_per_node: u32,
) {
    let incoming_vnodes = if vnode_assignments.is_empty() {
        default_vnode_assignments(vnodes_per_node)
    } else {
        vnode_assignments.to_vec()
    };

    let incoming_addr = addr.unwrap_or_default();
    let incoming_is_fresher =
        |current: Option<u64>, incoming: Option<u64>| match (current, incoming) {
            (None, Some(_)) => true,
            (Some(cur), Some(newer)) => newer >= cur,
            _ => false,
        };

    match peers.get_mut(node_id) {
        Some(existing) => {
            if !incoming_addr.is_empty() {
                existing.addr = incoming_addr.to_string();
            }
            if incoming_is_fresher(existing.last_seen_at_unix, last_seen_at_unix) {
                existing.status = status.to_string();
                existing.last_seen_at_unix = last_seen_at_unix;
                existing.vnode_assignments = incoming_vnodes;
            }
        }
        None => {
            peers.insert(
                node_id.to_string(),
                PeerRuntime {
                    addr: incoming_addr.to_string(),
                    status: status.to_string(),
                    last_seen_at_unix,
                    vnode_assignments: incoming_vnodes,
                },
            );
        }
    }
}

impl Coordinator {
    fn is_peer_live(&self, node_id: &str) -> bool {
        let peers = self.peers.read();
        let Some(peer) = peers.get(node_id) else {
            return false;
        };
        // Phase 45 tranche 2: optimistic on unknown/suspect, hard-fail on down.
        !peer.status.eq_ignore_ascii_case("down")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::handshake::{HandshakeOutcome, run_acceptor};
    use crate::config::ClusterSection;
    use std::collections::HashSet;
    use std::sync::Arc;
    use tokio::net::TcpListener;
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
    fn test_validate_concerns_v5_only() {
        let c = Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        assert!(c.validate_concerns(Some(1), Some(1)).is_ok());
        assert!(c.validate_concerns(Some(2), Some(1)).is_err());
        // W=2 remains invalid in single-node mode (N=1).
        assert!(c.validate_concerns(Some(1), Some(2)).is_err());
    }

    #[test]
    fn test_topology_json_has_expected_fields() {
        let c = Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        let topo = c.topology_json();
        assert!(topo.get("cluster_id").is_some());
        assert!(topo.get("local_node_id").is_some());
        assert_eq!(topo.get("n").and_then(|v| v.as_u64()), Some(1));
        let peers = topo
            .get("peers")
            .and_then(|v| v.as_array())
            .expect("peers array");
        assert!(!peers.is_empty());
        assert!(peers[0].get("status").is_some());
        assert!(peers[0].get("last_seen_at_unix").is_some());
    }

    #[test]
    fn test_update_peer_liveness_is_applied() {
        let mut cluster = ClusterSection::default();
        cluster.peers.push(PeerEntry {
            node_id: "11111111-1111-1111-1111-111111111111".to_string(),
            addr: "node-2:6390".to_string(),
            status: None,
            last_seen_at_unix: None,
            vnode_assignments: Vec::new(),
        });
        let c = Coordinator::from_config(&identity(), &cluster, "127.0.0.1:6380");
        c.update_peer_liveness("11111111-1111-1111-1111-111111111111", "up", Some(42));
        assert_eq!(
            c.peer_last_seen("11111111-1111-1111-1111-111111111111"),
            Some(42)
        );
    }

    #[test]
    fn test_replication_factor_defaults_to_cluster_size_capped_at_three() {
        let mut cluster = ClusterSection::default();
        for i in 0..4 {
            cluster.peers.push(PeerEntry {
                node_id: format!("00000000-0000-0000-0000-00000000000{i}"),
                addr: format!("node-{i}:6390"),
                status: None,
                last_seen_at_unix: None,
                vnode_assignments: vec![],
            });
        }

        let c = Coordinator::from_config(&identity(), &cluster, "127.0.0.1:6380");
        let topo = c.topology_json();
        assert_eq!(topo.get("n").and_then(|v| v.as_u64()), Some(3));
    }

    #[test]
    fn test_enforce_local_write_replica_accepts_local_node_when_in_preference_list() {
        let mut cluster = ClusterSection::default();
        cluster.peers.push(PeerEntry {
            node_id: "11111111-1111-1111-1111-111111111111".to_string(),
            addr: "node-2:6390".to_string(),
            status: None,
            last_seen_at_unix: None,
            vnode_assignments: vec![],
        });
        let c = Coordinator::from_config(&identity(), &cluster, "127.0.0.1:6380");
        assert!(
            c.enforce_local_write_replica("db", "users", b"some-key")
                .is_ok()
        );
    }

    async fn start_peer_acceptor(cluster_id: Uuid, peer_node_id: Uuid, known_peer: Uuid) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut known = HashSet::new();
            known.insert(known_peer);
            let res = run_acceptor(
                &mut stream,
                cluster_id,
                peer_node_id,
                "grumpydb-test/phase45",
                &known,
            )
            .await
            .expect("acceptor handshake");
            assert!(matches!(res, HandshakeOutcome::Accepted { .. }));
        });

        addr.to_string()
    }

    #[tokio::test]
    async fn test_wait_for_write_ack_quorum_succeeds_with_peer_ack() {
        let identity = identity();
        let peer_id = Uuid::new_v4();
        let addr = start_peer_acceptor(identity.cluster_id, peer_id, identity.node_id).await;

        let mut cluster = ClusterSection {
            write_ack_timeout_ms: 500,
            ..ClusterSection::default()
        };
        cluster.peers.push(PeerEntry {
            node_id: peer_id.to_string(),
            addr,
            status: Some("up".into()),
            last_seen_at_unix: None,
            vnode_assignments: vec![],
        });

        let c = Arc::new(Coordinator::from_config(
            &identity,
            &cluster,
            "127.0.0.1:6380",
        ));
        c.wait_for_write_ack_quorum("db", "users", b"k1", Some(2))
            .await
            .expect("W=2 quorum should pass with one peer ack");
    }

    #[tokio::test]
    async fn test_wait_for_write_ack_quorum_times_out_with_partial_acks() {
        let identity = identity();
        let peer_id = Uuid::new_v4();

        let mut cluster = ClusterSection {
            write_ack_timeout_ms: 100,
            ..ClusterSection::default()
        };
        cluster.peers.push(PeerEntry {
            node_id: peer_id.to_string(),
            addr: "127.0.0.1:1".to_string(),
            status: Some("up".into()),
            last_seen_at_unix: None,
            vnode_assignments: vec![],
        });

        let c = Coordinator::from_config(&identity, &cluster, "127.0.0.1:6380");
        let err = c
            .wait_for_write_ack_quorum("db", "users", b"k1", Some(2))
            .await
            .expect_err("W=2 should fail when remote ack is unavailable");
        assert!(err.contains("write quorum not reached"), "got: {err}");
    }

    #[test]
    fn test_validate_write_concern_for_key_rejects_when_live_replicas_insufficient() {
        let mut cluster = ClusterSection::default();
        cluster.peers.push(PeerEntry {
            node_id: "11111111-1111-1111-1111-111111111111".to_string(),
            addr: "node-2:6390".to_string(),
            status: Some("down".to_string()),
            last_seen_at_unix: None,
            vnode_assignments: vec![],
        });

        let c = Coordinator::from_config(&identity(), &cluster, "127.0.0.1:6380");
        let err = c
            .validate_write_concern_for_key("db", "users", b"k1", Some(2))
            .expect_err("W=2 should fail with only one live replica");
        assert!(err.contains("not enough live replicas"), "got: {err}");
    }

    #[test]
    fn test_validate_write_concern_for_key_accepts_when_live_replicas_sufficient() {
        let mut cluster = ClusterSection::default();
        cluster.peers.push(PeerEntry {
            node_id: "11111111-1111-1111-1111-111111111111".to_string(),
            addr: "node-2:6390".to_string(),
            status: Some("up".to_string()),
            last_seen_at_unix: None,
            vnode_assignments: vec![],
        });

        let c = Coordinator::from_config(&identity(), &cluster, "127.0.0.1:6380");
        assert!(
            c.validate_write_concern_for_key("db", "users", b"k1", Some(2))
                .is_ok()
        );
    }

    #[test]
    fn test_merge_gossip_from_peer_adds_unknown_membership_entries() {
        let mut cluster = ClusterSection::default();
        cluster.peers.push(PeerEntry {
            node_id: "11111111-1111-1111-1111-111111111111".to_string(),
            addr: "node-2:6390".to_string(),
            status: Some("up".to_string()),
            last_seen_at_unix: Some(100),
            vnode_assignments: vec![0, 1, 2],
        });

        let c = Coordinator::from_config(&identity(), &cluster, "127.0.0.1:6380");
        let hello = ClusterHello {
            cluster_id: Uuid::new_v4(),
            node_id: Uuid::new_v4(),
            server_version: "grumpydb-test".to_string(),
            capabilities: vec!["gossip-membership-v1".to_string()],
            status: Some("up".to_string()),
            last_seen_at_unix: Some(200),
            vnode_assignments: vec![5, 6],
            membership: vec![PeerGossipState {
                node_id: "22222222-2222-2222-2222-222222222222".to_string(),
                addr: "node-3:6390".to_string(),
                status: "suspect".to_string(),
                last_seen_at_unix: Some(150),
                vnode_assignments: vec![9, 10],
            }],
        };

        c.merge_gossip_from_peer("11111111-1111-1111-1111-111111111111", &hello, Some(210));

        let topo = c.topology_json();
        let peers = topo
            .get("peers")
            .and_then(|v| v.as_array())
            .expect("peers array");

        let discovered = peers
            .iter()
            .find(|p| {
                p.get("node_id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| id == "22222222-2222-2222-2222-222222222222")
            })
            .expect("gossip-discovered peer");
        assert_eq!(
            discovered.get("status").and_then(|v| v.as_str()),
            Some("suspect")
        );
        assert_eq!(
            discovered.get("addr").and_then(|v| v.as_str()),
            Some("node-3:6390")
        );
    }
}
