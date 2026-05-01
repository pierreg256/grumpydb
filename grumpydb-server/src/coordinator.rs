//! Coordinator helpers for routing and protocol validation.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use grumpydb_ring::{Ring, RingConfig, RoutingKey};
use parking_lot::RwLock;
use serde_json::json;

use crate::cluster::NodeIdentity;
use crate::config::{ClusterSection, PeerEntry, WriterEntry};

/// Lightweight server-side coordinator.
#[derive(Debug)]
pub struct Coordinator {
    local_node_id: String,
    cluster_id: String,
    n: usize,
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
                vnode_assignments: Vec::new(),
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
                    vnode_assignments: vnode_assignments.clone(),
                },
            );
        }

        let total_nodes = 1 + cluster.peers.len();
        // v6 default replication factor: N = min(3, cluster_size).
        let n = total_nodes.clamp(1, 3);

        Self {
            local_node_id,
            cluster_id,
            n,
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

    /// Validate read/write concerns.
    ///
    /// v6 Phase 45 keeps read concerns pinned to `R=1` (Phase 47 will
    /// enable `R>1`) and still enforces `W=1` until write-ack fanout is
    /// fully wired.
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

        if w != 1 {
            return Err(
                "v6 phase 45 in progress: W>1 write acknowledgements are not enabled yet"
                    .to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClusterSection;
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
}
