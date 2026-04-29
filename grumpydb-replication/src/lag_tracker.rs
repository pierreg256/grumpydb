//! Replication lag tracking for observability — slice 40e.7.
//!
//! This module tracks the lag (delay) between the leader's current HLC and
//! each follower's acknowledged HLC. Lag is measured in logical clock ticks,
//! which can be converted to wall-clock seconds for alerting and `/readyz`
//! gating.
//!
//! Lag is continuously updated as acks arrive from followers; a Prometheus
//! gauge metric is exposed for each peer so dashboards can track convergence
//! and alert on lagging replicas.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

/// Tracks replication lag per peer (origin_node_id).
///
/// The lag is simply `leader_hlc - peer_hlc`, where:
/// - `leader_hlc`: the highest HLC the leader has seen (from its own clock)
/// - `peer_hlc`: the highest HLC acknowledged by the peer (peer's apply watermark)
///
/// A lag of 0 means the peer has caught up to the leader. Positive lag means
/// the peer is behind. Negative lag should never occur (indicates a bug).
///
/// All updates use either atomic ops (leader HLC) or a Mutex (peer map)
/// so multiple threads (leader task, metrics scraper) can safely read/write.
#[derive(Debug)]
pub struct LagTracker {
    /// Maps peer node_id → peer's last acknowledged HLC.
    /// The leader's HLC is stored separately (see [`leader_hlc`]).
    peer_hlc: Arc<Mutex<HashMap<Uuid, u64>>>,
    /// Leader's current HLC (updated as the leader applies new records).
    leader_hlc: Arc<AtomicU64>,
}

impl LagTracker {
    /// Create a new lag tracker with no peers.
    pub fn new() -> Self {
        Self {
            peer_hlc: Arc::new(Mutex::new(HashMap::new())),
            leader_hlc: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Update the leader's current HLC.
    /// Called when the leader applies a new WAL record.
    pub fn set_leader_hlc(&self, hlc: u64) {
        self.leader_hlc.store(hlc, Ordering::Release);
    }

    /// Update a peer's acknowledged HLC.
    /// Called when an Ack frame arrives from the peer.
    pub fn set_peer_hlc(&self, peer_id: Uuid, hlc: u64) {
        if let Ok(mut map) = self.peer_hlc.lock() {
            map.insert(peer_id, hlc);
        }
    }

    /// Get the lag for a specific peer.
    /// Returns `leader_hlc - peer_hlc`. Returns `None` if peer is unknown.
    pub fn peer_lag(&self, peer_id: Uuid) -> Option<u64> {
        let leader = self.leader_hlc.load(Ordering::Acquire);
        if let Ok(map) = self.peer_hlc.lock() {
            map.get(&peer_id)
                .map(|peer_hlc| leader.saturating_sub(*peer_hlc))
        } else {
            None
        }
    }

    /// Get the maximum lag across all peers.
    /// Returns `None` if no peers are registered.
    pub fn max_lag(&self) -> Option<u64> {
        let leader = self.leader_hlc.load(Ordering::Acquire);
        if let Ok(map) = self.peer_hlc.lock() {
            if map.is_empty() {
                return None;
            }
            Some(
                map.values()
                    .map(|peer_hlc| leader.saturating_sub(*peer_hlc))
                    .max()
                    .unwrap_or(0),
            )
        } else {
            None
        }
    }

    /// Get all peer lags as a snapshot.
    pub fn all_lags(&self) -> HashMap<Uuid, u64> {
        let leader = self.leader_hlc.load(Ordering::Acquire);
        if let Ok(map) = self.peer_hlc.lock() {
            map.iter()
                .map(|(&id, &peer_hlc)| (id, leader.saturating_sub(peer_hlc)))
                .collect()
        } else {
            HashMap::new()
        }
    }
}

impl Default for LagTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lag_tracker_new_has_no_peers() {
        let tracker = LagTracker::new();
        assert_eq!(tracker.max_lag(), None);
    }

    #[test]
    fn test_leader_hlc_set() {
        let tracker = LagTracker::new();
        tracker.set_leader_hlc(100);
        // Without peers, no lag to report
        assert_eq!(tracker.max_lag(), None);
    }

    #[test]
    fn test_peer_lag_on_catchup() {
        let tracker = LagTracker::new();
        let peer = Uuid::new_v4();

        tracker.set_leader_hlc(100);
        tracker.set_peer_hlc(peer, 100);

        assert_eq!(tracker.peer_lag(peer), Some(0));
        assert_eq!(tracker.max_lag(), Some(0));
    }

    #[test]
    fn test_peer_lag_behind() {
        let tracker = LagTracker::new();
        let peer = Uuid::new_v4();

        tracker.set_leader_hlc(150);
        tracker.set_peer_hlc(peer, 100);

        assert_eq!(tracker.peer_lag(peer), Some(50));
        assert_eq!(tracker.max_lag(), Some(50));
    }

    #[test]
    fn test_multiple_peers_max_lag() {
        let tracker = LagTracker::new();
        let peer1 = Uuid::new_v4();
        let peer2 = Uuid::new_v4();
        let peer3 = Uuid::new_v4();

        tracker.set_leader_hlc(200);
        tracker.set_peer_hlc(peer1, 200); // lag = 0
        tracker.set_peer_hlc(peer2, 150); // lag = 50
        tracker.set_peer_hlc(peer3, 100); // lag = 100

        assert_eq!(tracker.peer_lag(peer1), Some(0));
        assert_eq!(tracker.peer_lag(peer2), Some(50));
        assert_eq!(tracker.peer_lag(peer3), Some(100));
        assert_eq!(tracker.max_lag(), Some(100));
    }

    #[test]
    fn test_all_lags_snapshot() {
        let tracker = LagTracker::new();
        let peer1 = Uuid::new_v4();
        let peer2 = Uuid::new_v4();

        tracker.set_leader_hlc(100);
        tracker.set_peer_hlc(peer1, 80);
        tracker.set_peer_hlc(peer2, 60);

        let lags = tracker.all_lags();
        assert_eq!(lags.get(&peer1), Some(&20));
        assert_eq!(lags.get(&peer2), Some(&40));
        assert_eq!(lags.len(), 2);
    }

    #[test]
    fn test_lag_saturates_at_zero() {
        let tracker = LagTracker::new();
        let peer = Uuid::new_v4();

        tracker.set_leader_hlc(100);
        tracker.set_peer_hlc(peer, 150); // peer ahead of leader (should not happen)

        // saturating_sub ensures we never go negative
        assert_eq!(tracker.peer_lag(peer), Some(0));
    }

    #[test]
    fn test_unknown_peer_returns_none() {
        let tracker = LagTracker::new();
        let unknown = Uuid::new_v4();

        tracker.set_leader_hlc(100);

        assert_eq!(tracker.peer_lag(unknown), None);
    }
}
