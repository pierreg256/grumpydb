//! Coordinator helpers for routing and protocol validation.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use futures::future::join_all;
use grumpydb::SharedDatabase;
use grumpydb::document::value::Value;
use grumpydb_ring::{Ring, RingConfig, RoutingKey};
use parking_lot::RwLock;
use serde_json::json;
use tokio::time::Duration;
use tokio::time::timeout;
use uuid::Uuid;

use crate::cluster::NodeIdentity;
use crate::cluster::handshake::{
    ClusterHello, GossipPayload, PeerGossipState, PeerKeyPath, PeerRpcContext, create_peer_index,
    delete_peer_value, drop_peer_index, fetch_peer_value, probe_peer_acceptance,
    query_peer_index_exact, query_peer_index_range, upsert_peer_value,
};
use crate::cluster::hints::{HintOperation, HintRecord};
use crate::cluster::schema::log::SchemaLog;
use crate::cluster::schema::materializer::MaterializerHandle;
use crate::cluster::schema::{ApplyOutcome, IndexKey, SchemaLogEntry, SchemaOp, SchemaState};
use crate::config::{ClusterSection, PeerEntry, WriterEntry};

// `create_peer_index` / `drop_peer_index` are still imported because the
// hint-replay path in [`Coordinator::replay_hint_to_peer`] can replay
// historical [`HintOperation::CreateIndex`] / [`HintOperation::DropIndex`]
// records that may exist on disk from pre-44c installations. New schema
// DDL no longer goes through hints — it propagates via the gossip path
// in [`crate::cluster::schema`].

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
    /// Phase 44b: cluster-wide schema state, lazily updated by gossip
    /// pulls. `None` when the binary runs in single-node mode without
    /// a data dir (e.g. some unit tests). Always `Some` in production.
    schema: Option<RwLock<SchemaState>>,
    /// Append-only on-disk schema log. Pairs with `schema`.
    schema_log: Option<SchemaLog>,
    /// Phase 44c: optional handle to the background materializer.
    /// `None` when running headless (most unit tests). Always `Some`
    /// in production after [`Self::attach_materializer`].
    materializer: Option<MaterializerHandle>,
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
            // Some deployments list the local node in `[cluster].peers`.
            // Keep the runtime-local entry (`up`, bound addr, last_seen=now)
            // instead of overriding it with optional static fields.
            if node_id == &local_node_id {
                continue;
            }
            ring.add_node(node_id.clone());
            peer_addrs.insert(node_id.clone(), addr.clone());
            peers.insert(
                node_id.clone(),
                PeerRuntime {
                    addr: addr.clone(),
                    status: canonical_status(
                        status.as_deref().unwrap_or("unknown"),
                        *last_seen_at_unix,
                    ),
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
            schema: None,
            schema_log: None,
            materializer: None,
        }
    }

    /// Attach a [`SchemaState`] + on-disk [`SchemaLog`] to this
    /// coordinator (Phase 44b). The state must already reflect the
    /// log (typically via [`SchemaLog::open`]).
    pub fn attach_schema(&mut self, state: SchemaState, log: SchemaLog) {
        self.schema = Some(RwLock::new(state));
        self.schema_log = Some(log);
    }

    /// Attach the background materializer handle (Phase 44c).
    /// Called once at startup after [`Self::attach_schema`].
    pub fn attach_materializer(&mut self, handle: MaterializerHandle) {
        self.materializer = Some(handle);
    }

    /// Per-cluster monotonic schema version known by this node
    /// (Phase 44b). Returns `0` when no schema state is attached.
    pub fn schema_version(&self) -> u64 {
        self.schema
            .as_ref()
            .map(|s| s.read().version())
            .unwrap_or(0)
    }

    /// Phase 44d: JSON snapshot of every locally-known schema entry.
    /// The shape is:
    ///
    /// ```json
    /// {
    ///   "node_id": "...",
    ///   "schema_version": 7,
    ///   "indexes": [
    ///     {"tenant":"_system","database":"demo","collection":"docs",
    ///      "index_name":"by_name","field_path":"name",
    ///      "tombstone":false,"last_modified_hlc":1700000123456}
    ///   ]
    /// }
    /// ```
    pub fn schema_status_json(&self) -> serde_json::Value {
        let entries: Vec<serde_json::Value> = match self.schema.as_ref() {
            None => Vec::new(),
            Some(lock) => {
                let s = lock.read();
                s.all_entries()
                    .map(|(k, e)| {
                        json!({
                            "tenant": k.tenant,
                            "database": k.database,
                            "collection": k.collection,
                            "index_name": k.index_name,
                            "field_path": e.field_path,
                            "tombstone": e.tombstone,
                            "last_modified_hlc": e.last_modified_hlc,
                        })
                    })
                    .collect()
            }
        };
        json!({
            "node_id": self.local_node_id,
            "schema_version": self.schema_version(),
            "indexes": entries,
        })
    }

    /// Phase 44d: returns `true` when a *live* (non-tombstoned)
    /// definition for the given index exists in the local schema
    /// state. Used by the QUERY error path to distinguish "this
    /// index is not in the cluster schema at all" (real error) from
    /// "the schema knows about it but the local materializer hasn't
    /// finished yet" (transient).
    pub fn schema_has_index(
        &self,
        tenant: &str,
        database: &str,
        collection: &str,
        index_name: &str,
    ) -> bool {
        let Some(lock) = self.schema.as_ref() else {
            return false;
        };
        let s = lock.read();
        let key = IndexKey {
            tenant: tenant.to_string(),
            database: database.to_string(),
            collection: collection.to_string(),
            index_name: index_name.to_string(),
        };
        s.get(&key).is_some()
    }

    /// Return every locally-known schema log entry strictly newer than
    /// `since`, in ascending version order. Used to serve
    /// [`crate::cluster::handshake::PeerDataOp::PullSchemaSince`].
    pub fn local_schema_diff_since(&self, since: u64) -> Vec<SchemaLogEntry> {
        self.schema
            .as_ref()
            .map(|s| s.read().entries_since(since))
            .unwrap_or_default()
    }

    /// Apply a batch of remote schema entries (typically the result
    /// of a gossip pull). Each entry is appended to the on-disk log
    /// before being applied to the in-memory state, so a crash
    /// midway leaves the log as the source of truth.
    ///
    /// Returns the number of entries that produced an `Applied`
    /// outcome (i.e. were not duplicates or LWW losers). For each
    /// `Applied` entry, a [`crate::cluster::schema::materializer::MaterializeJob`]
    /// is enqueued on the background materializer (when attached).
    pub fn apply_remote_schema_entries(&self, entries: &[SchemaLogEntry]) -> usize {
        let (Some(schema), Some(log)) = (self.schema.as_ref(), self.schema_log.as_ref()) else {
            return 0;
        };

        let mut applied = 0usize;
        let mut to_persist = Vec::with_capacity(entries.len());
        let mut state = schema.write();
        for entry in entries {
            match state.apply(entry) {
                ApplyOutcome::Applied => {
                    applied += 1;
                    to_persist.push(entry.clone());
                }
                ApplyOutcome::Stale | ApplyOutcome::Duplicate => {}
            }
        }
        drop(state);

        if !to_persist.is_empty()
            && let Err(e) = log.append_batch(&to_persist)
        {
            tracing::warn!(error = %e, count = to_persist.len(), "schema log append failed");
        }

        if applied > 0 {
            metrics::gauge!(
                "grumpydb_schema_version",
                "node_id" => self.local_node_id.clone()
            )
            .set(self.schema_version() as f64);
            metrics::counter!(
                "grumpydb_schema_pulls_total",
                "result" => "applied"
            )
            .increment(applied as u64);
        }

        if let Some(mat) = &self.materializer {
            for entry in &to_persist {
                mat.enqueue_from_entry(entry);
            }
        }

        applied
    }

    /// Apply one DDL operation originating on this node (Phase 44c).
    ///
    /// Steps:
    /// 1. Allocate the next [`SchemaState::next_version`].
    /// 2. Build a [`SchemaLogEntry`] stamped with the supplied HLC.
    /// 3. Insert it into the in-memory state.
    /// 4. Persist it to the on-disk schema log.
    /// 5. Return the entry so the caller can advertise its `version`
    ///    (and so tests can introspect).
    ///
    /// The local engine-level apply (`db.create_index` /
    /// `db.drop_index`) is **not** performed here: it stays in the
    /// DDL handler's `with_db` block so the client sees the result
    /// synchronously. Peers learn about the change via the gossip
    /// path, then materialize asynchronously through the background
    /// worker.
    ///
    /// Returns `None` when no schema state is attached (e.g. in
    /// some unit tests that build a `Coordinator` without a data dir).
    pub fn apply_local_ddl(&self, op: SchemaOp, hlc: u64) -> Option<SchemaLogEntry> {
        let (Some(schema), Some(log)) = (self.schema.as_ref(), self.schema_log.as_ref()) else {
            return None;
        };

        let mut state = schema.write();
        let version = state.next_version();
        let entry = SchemaLogEntry { version, hlc, op };
        // Apply unconditionally: a locally-issued DDL always advances
        // our view (we just allocated the version).
        let _ = state.apply(&entry);
        drop(state);

        if let Err(e) = log.append(&entry) {
            tracing::warn!(error = %e, "local schema log append failed");
        }

        metrics::gauge!(
            "grumpydb_schema_version",
            "node_id" => self.local_node_id.clone()
        )
        .set(self.schema_version() as f64);

        if let Some(mat) = &self.materializer {
            mat.enqueue_from_entry(&entry);
        }

        Some(entry)
    }

    /// Convenience shorthand: build a `CreateIndex` op and apply it
    /// locally. Returns the stamped log entry.
    pub fn apply_local_create_index(
        &self,
        tenant: &str,
        database: &str,
        collection: &str,
        index_name: &str,
        field_path: &str,
        hlc: u64,
    ) -> Option<SchemaLogEntry> {
        let key = IndexKey {
            tenant: tenant.to_string(),
            database: database.to_string(),
            collection: collection.to_string(),
            index_name: index_name.to_string(),
        };
        self.apply_local_ddl(
            SchemaOp::CreateIndex {
                key,
                field_path: field_path.to_string(),
            },
            hlc,
        )
    }

    /// Convenience shorthand: build a `DropIndex` op and apply it
    /// locally. Returns the stamped log entry.
    pub fn apply_local_drop_index(
        &self,
        tenant: &str,
        database: &str,
        collection: &str,
        index_name: &str,
        hlc: u64,
    ) -> Option<SchemaLogEntry> {
        let key = IndexKey {
            tenant: tenant.to_string(),
            database: database.to_string(),
            collection: collection.to_string(),
            index_name: index_name.to_string(),
        };
        self.apply_local_ddl(SchemaOp::DropIndex { key }, hlc)
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
            schema_version: self.schema_version(),
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
    /// Read/write concerns are bounded here. Key-level liveness checks are
    /// enforced by [`Self::validate_write_concern_for_key`] and
    /// [`Self::validate_read_concern_for_key`].
    pub fn validate_concerns(
        &self,
        read_concern: Option<u16>,
        write_concern: Option<u16>,
    ) -> Result<(), String> {
        let r = read_concern.unwrap_or(1) as usize;
        let w = write_concern.unwrap_or(1) as usize;

        if !(1..=self.n).contains(&r) || !(1..=self.n).contains(&w) {
            return Err(format!(
                "invalid consistency concerns: require R and W in [1, {}]",
                self.n
            ));
        }

        Ok(())
    }

    /// Validate read concern against the key's currently-live replica set.
    pub fn validate_read_concern_for_key(
        &self,
        database: &str,
        collection: &str,
        key_bytes: &[u8],
        read_concern: Option<u16>,
    ) -> Result<(), String> {
        let r = read_concern.unwrap_or(1) as usize;
        if !(1..=self.n).contains(&r) {
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
        if r > live {
            return Err(format!(
                "not enough live replicas for R={r}: have {live} live replicas in preference list"
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

    /// Fan out read acknowledgements to replica peers and wait until quorum
    /// `R` is satisfied or timeout/failures leave insufficient acknowledgements.
    pub async fn wait_for_read_ack_quorum(
        &self,
        database: &str,
        collection: &str,
        key_bytes: &[u8],
        read_concern: Option<u16>,
    ) -> Result<(), String> {
        let r = read_concern.unwrap_or(1) as usize;
        if r <= 1 {
            return Ok(());
        }

        let key = RoutingKey {
            database,
            collection,
            key_bytes,
        };
        let owners = self.ring.preference_list(&key, self.n);

        let mut acked = 1usize; // local replica
        let mut fanout_addrs = Vec::new();
        for node_id in owners {
            if node_id == self.local_node_id {
                continue;
            }
            if let Some(addr) = self.peer_addrs.get(&node_id) {
                fanout_addrs.push(addr.clone());
            }
        }

        let required_remote = r.saturating_sub(1);
        if fanout_addrs.len() < required_remote {
            return Err(format!(
                "read quorum cannot be satisfied: need {required_remote} remote acks, only {} replica peers available",
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
        if acked >= r {
            return Ok(());
        }

        Err(format!(
            "read quorum not reached: acked {acked}/{r}; failures: {}",
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

    /// Build a preview of hash-range ownership changes if a node is added.
    pub fn plan_rebalance_add_node(&self, node_id: &str) -> serde_json::Value {
        let mut ring = self.ring.clone();
        let ranges = ring.add_node(node_id.to_string());
        json!({
            "action": "add-node",
            "node_id": node_id,
            "range_count": ranges.len(),
            "ranges": ranges.into_iter().map(key_range_json).collect::<Vec<_>>(),
        })
    }

    /// Build a preview of hash-range ownership changes if a node is removed.
    pub fn plan_rebalance_remove_node(&self, node_id: &str) -> serde_json::Value {
        let mut ring = self.ring.clone();
        let ranges = ring.remove_node(&node_id.to_string());
        json!({
            "action": "remove-node",
            "node_id": node_id,
            "range_count": ranges.len(),
            "ranges": ranges.into_iter().map(key_range_json).collect::<Vec<_>>(),
        })
    }

    /// Execute an add-node rebalance plan (phase-49 scaffolding).
    pub fn execute_rebalance_add_node(&self, node_id: &str) -> serde_json::Value {
        let plan = self.plan_rebalance_add_node(node_id);
        let total = plan
            .get("range_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        metrics::counter!(
            "grumpydb_rebalance_transfers_total",
            "action" => "add-node"
        )
        .increment(total);
        json!({
            "action": "add-node",
            "node_id": node_id,
            "planned_ranges": total,
            "executed_ranges": total,
            "status": "planned-only",
        })
    }

    /// Execute a remove-node rebalance plan (phase-49 scaffolding).
    pub fn execute_rebalance_remove_node(&self, node_id: &str) -> serde_json::Value {
        let plan = self.plan_rebalance_remove_node(node_id);
        let total = plan
            .get("range_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        metrics::counter!(
            "grumpydb_rebalance_transfers_total",
            "action" => "remove-node"
        )
        .increment(total);
        json!({
            "action": "remove-node",
            "node_id": node_id,
            "planned_ranges": total,
            "executed_ranges": total,
            "status": "planned-only",
        })
    }

    /// Return non-local replica node ids for a key.
    pub fn replica_peer_nodes_for_key(
        &self,
        database: &str,
        collection: &str,
        key_bytes: &[u8],
    ) -> Vec<String> {
        let key = RoutingKey {
            database,
            collection,
            key_bytes,
        };
        self.ring
            .preference_list(&key, self.n)
            .into_iter()
            .filter(|node_id| node_id != &self.local_node_id)
            .collect()
    }

    /// Return non-local replicas currently marked unavailable for a key.
    pub fn unavailable_replica_peer_nodes_for_key(
        &self,
        database: &str,
        collection: &str,
        key_bytes: &[u8],
    ) -> Vec<String> {
        self.replica_peer_nodes_for_key(database, collection, key_bytes)
            .into_iter()
            .filter(|node_id| !self.is_peer_live(node_id))
            .collect()
    }

    /// Public liveness accessor used by background orchestration workers.
    pub fn peer_is_live(&self, node_id: &str) -> bool {
        self.is_peer_live(node_id)
    }

    /// Read one key from all live remote replicas in the preference list.
    pub async fn fanout_read_peer_values(
        &self,
        tenant: &str,
        database: &str,
        collection: &str,
        key: &str,
    ) -> Vec<(String, Result<Option<String>, String>)> {
        let peers: Vec<(String, String)> = self
            .replica_peer_nodes_for_key(database, collection, key.as_bytes())
            .into_iter()
            .filter_map(|node_id| {
                self.peer_addrs
                    .get(&node_id)
                    .map(|addr| (node_id, addr.clone()))
            })
            .collect();

        let server_version = format!("grumpydb-server/{}", env!("CARGO_PKG_VERSION"));
        let futures = peers.into_iter().map(|(node_id, addr)| {
            let server_version = server_version.clone();
            async move {
                let ctx = PeerRpcContext {
                    addr,
                    local_cluster_id: self.cluster_uuid,
                    local_node_id: self.local_node_uuid,
                    server_version,
                };
                let key_path = PeerKeyPath {
                    tenant: tenant.to_string(),
                    database: database.to_string(),
                    collection: collection.to_string(),
                    key: key.to_string(),
                };
                let v = fetch_peer_value(&ctx, &key_path).await;
                (node_id, v)
            }
        });

        join_all(futures).await
    }

    /// Query one index value from all live remote replicas in the preference list.
    pub async fn fanout_query_peer_candidates_exact(
        &self,
        tenant: &str,
        database: &str,
        collection: &str,
        key_bytes: &[u8],
        index_name: &str,
        value_json: &str,
    ) -> Vec<(String, Result<Vec<String>, String>)> {
        let peers: Vec<(String, String)> = self
            .replica_peer_nodes_for_key(database, collection, key_bytes)
            .into_iter()
            .filter_map(|node_id| {
                self.peer_addrs
                    .get(&node_id)
                    .map(|addr| (node_id, addr.clone()))
            })
            .collect();

        let server_version = format!("grumpydb-server/{}", env!("CARGO_PKG_VERSION"));
        let futures = peers.into_iter().map(|(node_id, addr)| {
            let server_version = server_version.clone();
            async move {
                let ctx = PeerRpcContext {
                    addr,
                    local_cluster_id: self.cluster_uuid,
                    local_node_id: self.local_node_uuid,
                    server_version,
                };
                let v = query_peer_index_exact(
                    &ctx, tenant, database, collection, index_name, value_json,
                )
                .await;
                (node_id, v)
            }
        });

        join_all(futures).await
    }

    /// Range-query one index from all live remote replicas in the preference list.
    #[allow(clippy::too_many_arguments)]
    pub async fn fanout_query_peer_candidates_range(
        &self,
        tenant: &str,
        database: &str,
        collection: &str,
        key_bytes: &[u8],
        index_name: &str,
        start_json: &str,
        end_json: &str,
    ) -> Vec<(String, Result<Vec<String>, String>)> {
        let peers: Vec<(String, String)> = self
            .replica_peer_nodes_for_key(database, collection, key_bytes)
            .into_iter()
            .filter_map(|node_id| {
                self.peer_addrs
                    .get(&node_id)
                    .map(|addr| (node_id, addr.clone()))
            })
            .collect();

        let server_version = format!("grumpydb-server/{}", env!("CARGO_PKG_VERSION"));
        let futures = peers.into_iter().map(|(node_id, addr)| {
            let server_version = server_version.clone();
            async move {
                let ctx = PeerRpcContext {
                    addr,
                    local_cluster_id: self.cluster_uuid,
                    local_node_id: self.local_node_uuid,
                    server_version,
                };
                let v = query_peer_index_range(
                    &ctx, tenant, database, collection, index_name, start_json, end_json,
                )
                .await;
                (node_id, v)
            }
        });

        join_all(futures).await
    }

    /// Apply one converged value to a remote replica.
    pub async fn repair_peer_value(
        &self,
        peer_node_id: &str,
        tenant: &str,
        database: &str,
        collection: &str,
        key: &str,
        value_json: &str,
    ) -> Result<(), String> {
        let addr = self
            .peer_addrs
            .get(peer_node_id)
            .ok_or_else(|| format!("unknown peer: {peer_node_id}"))?;
        let server_version = format!("grumpydb-server/{}", env!("CARGO_PKG_VERSION"));
        let ctx = PeerRpcContext {
            addr: addr.clone(),
            local_cluster_id: self.cluster_uuid,
            local_node_id: self.local_node_uuid,
            server_version,
        };
        let key_path = PeerKeyPath {
            tenant: tenant.to_string(),
            database: database.to_string(),
            collection: collection.to_string(),
            key: key.to_string(),
        };
        upsert_peer_value(&ctx, &key_path, value_json).await
    }

    /// Replay one hinted-handoff record to a peer.
    pub async fn replay_hint_to_peer(
        &self,
        peer_node_id: &str,
        hint: &HintRecord,
    ) -> Result<(), String> {
        let addr = self
            .peer_addrs
            .get(peer_node_id)
            .ok_or_else(|| format!("unknown peer: {peer_node_id}"))?;
        let server_version = format!("grumpydb-server/{}", env!("CARGO_PKG_VERSION"));
        let ctx = PeerRpcContext {
            addr: addr.clone(),
            local_cluster_id: self.cluster_uuid,
            local_node_id: self.local_node_uuid,
            server_version,
        };
        let key_path = PeerKeyPath {
            tenant: hint.tenant.clone(),
            database: hint.database.clone(),
            collection: hint.collection.clone(),
            key: hint.key.clone(),
        };

        match hint.resolved_operation() {
            HintOperation::Upsert { value_json } => {
                upsert_peer_value(&ctx, &key_path, &value_json).await
            }
            HintOperation::Delete => delete_peer_value(&ctx, &key_path).await,
            HintOperation::CreateIndex {
                index_name,
                field_path,
            } => {
                create_peer_index(
                    &ctx,
                    &hint.tenant,
                    &hint.database,
                    &hint.collection,
                    &index_name,
                    &field_path,
                )
                .await
            }
            HintOperation::DropIndex { index_name } => {
                drop_peer_index(
                    &ctx,
                    &hint.tenant,
                    &hint.database,
                    &hint.collection,
                    &index_name,
                )
                .await
            }
        }
    }

    /// Execute transfer to a newly-added node by shipping all keys whose
    /// primary owner changes from local node to `target_node_id`.
    pub async fn execute_rebalance_add_node_transfer(
        &self,
        target_node_id: &str,
        tenant: &str,
        database: &str,
        collection: &str,
        local_db: &SharedDatabase,
    ) -> serde_json::Value {
        let mut ring_after = self.ring.clone();
        ring_after.add_node(target_node_id.to_string());

        let scan = local_db.scan(collection, ..);
        let mut considered = 0u64;
        let mut transferred = 0u64;
        let mut failed = 0u64;

        let docs = match scan {
            Ok(docs) => docs,
            Err(e) => {
                return json!({
                    "action": "add-node-transfer",
                    "target_node_id": target_node_id,
                    "status": "error",
                    "error": e.to_string(),
                });
            }
        };

        for (id, value) in docs {
            let key = id.to_string();
            let before_key = RoutingKey {
                database,
                collection,
                key_bytes: key.as_bytes(),
            };
            let before_owner = self
                .ring
                .preference_list(&before_key, 1)
                .first()
                .cloned()
                .unwrap_or_default();
            let after_owner = ring_after
                .preference_list(&before_key, 1)
                .first()
                .cloned()
                .unwrap_or_default();

            if before_owner != self.local_node_id || after_owner != target_node_id {
                continue;
            }

            considered += 1;
            let value_json = serde_json::to_string(&value_to_serde_json(&value))
                .unwrap_or_else(|_| "null".to_string());
            match self
                .repair_peer_value(
                    target_node_id,
                    tenant,
                    database,
                    collection,
                    &key,
                    &value_json,
                )
                .await
            {
                Ok(()) => transferred += 1,
                Err(_) => failed += 1,
            }
        }

        metrics::counter!(
            "grumpydb_rebalance_transfers_total",
            "action" => "add-node-transfer"
        )
        .increment(transferred);
        metrics::counter!(
            "grumpydb_rebalance_transfer_failures_total",
            "action" => "add-node-transfer"
        )
        .increment(failed);

        json!({
            "action": "add-node-transfer",
            "target_node_id": target_node_id,
            "considered": considered,
            "transferred": transferred,
            "failed": failed,
            "status": if failed == 0 { "ok" } else { "partial" },
        })
    }

    /// Execute transfer for remove-node ownership shifts.
    pub async fn execute_rebalance_remove_node_transfer(
        &self,
        removed_node_id: &str,
        tenant: &str,
        database: &str,
        collection: &str,
        local_db: &SharedDatabase,
    ) -> serde_json::Value {
        let mut ring_after = self.ring.clone();
        ring_after.remove_node(&removed_node_id.to_string());

        let scan = local_db.scan(collection, ..);
        let mut considered = 0u64;
        let mut transferred = 0u64;
        let mut failed = 0u64;
        let mut retained_local = 0u64;

        let docs = match scan {
            Ok(docs) => docs,
            Err(e) => {
                return json!({
                    "action": "remove-node-transfer",
                    "removed_node_id": removed_node_id,
                    "status": "error",
                    "error": e.to_string(),
                });
            }
        };

        for (id, value) in docs {
            let key = id.to_string();
            let route_key = RoutingKey {
                database,
                collection,
                key_bytes: key.as_bytes(),
            };
            let before_owner = self
                .ring
                .preference_list(&route_key, 1)
                .first()
                .cloned()
                .unwrap_or_default();
            let after_owner = ring_after
                .preference_list(&route_key, 1)
                .first()
                .cloned()
                .unwrap_or_default();

            if before_owner != removed_node_id || after_owner == removed_node_id {
                continue;
            }
            considered += 1;

            if after_owner == self.local_node_id {
                retained_local += 1;
                continue;
            }

            let value_json = serde_json::to_string(&value_to_serde_json(&value))
                .unwrap_or_else(|_| "null".to_string());
            match self
                .repair_peer_value(
                    &after_owner,
                    tenant,
                    database,
                    collection,
                    &key,
                    &value_json,
                )
                .await
            {
                Ok(()) => transferred += 1,
                Err(_) => failed += 1,
            }
        }

        metrics::counter!(
            "grumpydb_rebalance_transfers_total",
            "action" => "remove-node-transfer"
        )
        .increment(transferred);
        metrics::counter!(
            "grumpydb_rebalance_transfer_failures_total",
            "action" => "remove-node-transfer"
        )
        .increment(failed);

        json!({
            "action": "remove-node-transfer",
            "removed_node_id": removed_node_id,
            "considered": considered,
            "retained_local": retained_local,
            "transferred": transferred,
            "failed": failed,
            "status": if failed == 0 { "ok" } else { "partial" },
        })
    }
}

fn value_to_serde_json(val: &Value) -> serde_json::Value {
    match val {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Integer(i) => serde_json::json!(*i),
        Value::Float(f) => serde_json::json!(*f),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Bytes(b) => {
            serde_json::json!({"$bytes": base64::engine::general_purpose::STANDARD.encode(b)})
        }
        Value::Ref(coll, id) => serde_json::json!({"$ref": {"collection": coll, "id": id}}),
        Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(value_to_serde_json).collect())
        }
        Value::Object(obj) => {
            let mut map = serde_json::Map::new();
            for (k, v) in obj {
                map.insert(k.clone(), value_to_serde_json(v));
            }
            serde_json::Value::Object(map)
        }
        Value::Tombstone { deleted_at_hlc, .. } => {
            serde_json::json!({"$tombstone": {"hlc": deleted_at_hlc}})
        }
        Value::Crdt { kind, payload } => serde_json::json!({
            "$crdt": {
                "kind": kind.as_str(),
                "payload_b64": base64::engine::general_purpose::STANDARD.encode(payload)
            }
        }),
    }
}

fn key_range_json(range: grumpydb_ring::KeyRange) -> serde_json::Value {
    json!({
        "start_inclusive": range.start_inclusive,
        "end_exclusive": range.end_exclusive,
        "from": range.from.map(|n| n.0),
        "to": range.to.0,
    })
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
    let incoming_status = canonical_status(status, last_seen_at_unix);
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
            if !incoming_addr.is_empty()
                && (!is_unroutable_advertised_addr(incoming_addr) || existing.addr.is_empty())
            {
                existing.addr = incoming_addr.to_string();
            }
            if incoming_is_fresher(existing.last_seen_at_unix, last_seen_at_unix) {
                // Do not downgrade a known runtime status to `unknown`.
                if incoming_status != "unknown" || existing.status.eq_ignore_ascii_case("unknown") {
                    existing.status = incoming_status;
                }
                existing.last_seen_at_unix = last_seen_at_unix;
                existing.vnode_assignments = incoming_vnodes;
            }
        }
        None => {
            peers.insert(
                node_id.to_string(),
                PeerRuntime {
                    addr: incoming_addr.to_string(),
                    status: incoming_status,
                    last_seen_at_unix,
                    vnode_assignments: incoming_vnodes,
                },
            );
        }
    }
}

fn canonical_status(status: &str, last_seen_at_unix: Option<u64>) -> String {
    if status.eq_ignore_ascii_case("unknown") && last_seen_at_unix.is_some() {
        "up".to_string()
    } else {
        status.to_string()
    }
}

fn is_unroutable_advertised_addr(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
    host == "0.0.0.0" || host == "127.0.0.1" || host == "::" || host == "[::]"
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
        // R=2 remains invalid in single-node mode (N=1).
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
    fn test_from_config_ignores_duplicate_local_peer_entry() {
        let id = identity();
        let mut cluster = ClusterSection::default();
        cluster.peers.push(PeerEntry {
            node_id: id.node_id.to_string(),
            addr: "node1:7390".to_string(),
            status: None,
            last_seen_at_unix: None,
            vnode_assignments: vec![],
        });

        let c = Coordinator::from_config(&id, &cluster, "127.0.0.1:6380");
        let topo = c.topology_json();
        let peers = topo
            .get("peers")
            .and_then(|v| v.as_array())
            .expect("peers array");
        let local = peers
            .iter()
            .find(|p| {
                p.get("node_id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|nid| nid == id.node_id.to_string())
            })
            .expect("local peer in topology");
        assert_eq!(local.get("status").and_then(|v| v.as_str()), Some("up"));
        assert_eq!(
            local.get("addr").and_then(|v| v.as_str()),
            Some("127.0.0.1:6380")
        );
        assert!(
            local
                .get("last_seen_at_unix")
                .and_then(|v| v.as_u64())
                .is_some()
        );
    }

    #[test]
    fn test_merge_peer_runtime_keeps_known_status_when_incoming_unknown() {
        let mut peers = BTreeMap::new();
        peers.insert(
            "n2".to_string(),
            PeerRuntime {
                addr: "node2:7390".to_string(),
                status: "up".to_string(),
                last_seen_at_unix: Some(100),
                vnode_assignments: vec![0, 1],
            },
        );

        merge_peer_runtime(
            &mut peers,
            "n2",
            Some("node2:7390"),
            "unknown",
            Some(200),
            &[0, 1],
            128,
        );

        let p = peers.get("n2").expect("peer exists");
        assert_eq!(p.status, "up");
        assert_eq!(p.last_seen_at_unix, Some(200));
    }

    #[test]
    fn test_merge_peer_runtime_does_not_override_routable_addr_with_wildcard() {
        let mut peers = BTreeMap::new();
        peers.insert(
            "n2".to_string(),
            PeerRuntime {
                addr: "node2:7390".to_string(),
                status: "up".to_string(),
                last_seen_at_unix: Some(100),
                vnode_assignments: vec![0, 1],
            },
        );

        merge_peer_runtime(
            &mut peers,
            "n2",
            Some("0.0.0.0:7390"),
            "up",
            Some(200),
            &[0, 1],
            128,
        );

        let p = peers.get("n2").expect("peer exists");
        assert_eq!(p.addr, "node2:7390");
        assert_eq!(p.last_seen_at_unix, Some(200));
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
    fn test_validate_read_concern_for_key_accepts_when_live_replicas_sufficient() {
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
            c.validate_read_concern_for_key("db", "users", b"k1", Some(2))
                .is_ok()
        );
    }

    #[test]
    fn test_validate_read_concern_for_key_rejects_when_live_replicas_insufficient() {
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
            .validate_read_concern_for_key("db", "users", b"k1", Some(2))
            .expect_err("R=2 should fail with only one live replica");
        assert!(err.contains("not enough live replicas"), "got: {err}");
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
            schema_version: 0,
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

    #[test]
    fn test_plan_rebalance_add_node_returns_delta_ranges() {
        let c = Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        let plan = c.plan_rebalance_add_node("11111111-1111-1111-1111-111111111111");
        assert_eq!(
            plan.get("action").and_then(|v| v.as_str()),
            Some("add-node")
        );
        assert_eq!(
            plan.get("node_id").and_then(|v| v.as_str()),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert!(
            plan.get("range_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0
        );
    }

    #[test]
    fn test_plan_rebalance_remove_node_returns_empty_for_unknown_node() {
        let c = Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        let plan = c.plan_rebalance_remove_node("unknown-node");
        assert_eq!(
            plan.get("action").and_then(|v| v.as_str()),
            Some("remove-node")
        );
        assert_eq!(plan.get("range_count").and_then(|v| v.as_u64()), Some(0));
    }

    #[test]
    fn test_replica_peer_nodes_for_key_excludes_local() {
        let mut cluster = ClusterSection::default();
        cluster.peers.push(PeerEntry {
            node_id: "11111111-1111-1111-1111-111111111111".to_string(),
            addr: "node-2:6390".to_string(),
            status: Some("up".to_string()),
            last_seen_at_unix: None,
            vnode_assignments: vec![],
        });

        let c = Coordinator::from_config(&identity(), &cluster, "127.0.0.1:6380");
        let peers = c.replica_peer_nodes_for_key("db", "users", b"k1");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0], "11111111-1111-1111-1111-111111111111");
    }

    #[test]
    fn test_unavailable_replica_peer_nodes_for_key_filters_down_peers() {
        let mut cluster = ClusterSection::default();
        cluster.peers.push(PeerEntry {
            node_id: "11111111-1111-1111-1111-111111111111".to_string(),
            addr: "node-2:6390".to_string(),
            status: Some("down".to_string()),
            last_seen_at_unix: None,
            vnode_assignments: vec![],
        });

        let c = Coordinator::from_config(&identity(), &cluster, "127.0.0.1:6380");
        let peers = c.unavailable_replica_peer_nodes_for_key("db", "users", b"k1");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0], "11111111-1111-1111-1111-111111111111");
    }

    #[test]
    fn test_execute_rebalance_add_node_reports_progress() {
        let c = Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        let out = c.execute_rebalance_add_node("11111111-1111-1111-1111-111111111111");
        assert_eq!(
            out.get("status").and_then(|v| v.as_str()),
            Some("planned-only")
        );
        assert!(
            out.get("executed_ranges")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0
        );
    }

    #[tokio::test]
    async fn test_execute_rebalance_remove_node_transfer_empty_collection_is_ok() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let db_path = tmp.path().join("db");
        let db = SharedDatabase::open(&db_path).expect("open db");
        db.create_collection("users").expect("create collection");

        let c = Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        let out = c
            .execute_rebalance_remove_node_transfer(
                "11111111-1111-1111-1111-111111111111",
                "tenant",
                "db",
                "users",
                &db,
            )
            .await;
        assert_eq!(
            out.get("action").and_then(|v| v.as_str()),
            Some("remove-node-transfer")
        );
        assert_eq!(out.get("failed").and_then(|v| v.as_u64()), Some(0));
    }

    #[tokio::test]
    async fn test_replay_hint_to_peer_returns_error_for_unknown_peer() {
        let c = Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        let hint = HintRecord {
            created_at_unix: 1,
            tenant: "tenant".to_string(),
            database: "db".to_string(),
            collection: "users".to_string(),
            key: "11111111-1111-1111-1111-111111111111".to_string(),
            operation: Some(HintOperation::Delete),
            payload_json: None,
        };
        let err = c
            .replay_hint_to_peer("unknown-peer", &hint)
            .await
            .expect_err("unknown peer should error");
        assert!(err.contains("unknown peer"), "got: {err}");
    }

    #[test]
    fn test_peer_is_live_returns_false_for_unknown_peer() {
        let c = Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        assert!(!c.peer_is_live("unknown-peer"));
    }

    // ── Phase 44b: schema state + pull RPC ─────────────────────────

    use crate::cluster::schema::{IndexKey, SchemaOp};

    fn schema_create(version: u64, hlc: u64, name: &str, field: &str) -> SchemaLogEntry {
        SchemaLogEntry {
            version,
            hlc,
            op: SchemaOp::CreateIndex {
                key: IndexKey {
                    tenant: "_system".into(),
                    database: "demo".into(),
                    collection: "docs".into(),
                    index_name: name.into(),
                },
                field_path: field.into(),
            },
        }
    }

    #[test]
    fn test_schema_version_zero_when_unattached() {
        let c = Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        assert_eq!(c.schema_version(), 0);
        assert!(c.local_schema_diff_since(0).is_empty());
        // Apply on an unattached coordinator silently does nothing.
        assert_eq!(
            c.apply_remote_schema_entries(&[schema_create(1, 100, "by_x", "x")]),
            0
        );
    }

    #[test]
    fn test_apply_remote_schema_entries_advances_version_and_persists() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let (log, state) = SchemaLog::open(dir.path()).expect("open schema log");
        let mut c =
            Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        c.attach_schema(state, log);

        let applied = c.apply_remote_schema_entries(&[
            schema_create(1, 100, "by_a", "a"),
            schema_create(2, 200, "by_b", "b"),
            // Replay of version 1 — must be a duplicate, not re-applied.
            schema_create(1, 100, "by_a", "a"),
        ]);
        assert_eq!(applied, 2);
        assert_eq!(c.schema_version(), 2);

        // Re-open the log on disk and verify both Applied entries are persisted.
        let (_log2, state2) = SchemaLog::open(dir.path()).expect("reopen");
        assert_eq!(state2.version(), 2);
    }

    #[test]
    fn test_local_schema_diff_since_returns_versions_above_threshold() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let (log, state) = SchemaLog::open(dir.path()).expect("open schema log");
        let mut c =
            Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        c.attach_schema(state, log);

        c.apply_remote_schema_entries(&[
            schema_create(1, 100, "by_a", "a"),
            schema_create(2, 200, "by_b", "b"),
            schema_create(3, 300, "by_c", "c"),
        ]);

        let diff = c.local_schema_diff_since(1);
        assert_eq!(diff.len(), 2);
        assert_eq!(diff[0].version, 2);
        assert_eq!(diff[1].version, 3);
    }

    #[test]
    fn test_gossip_payload_carries_local_schema_version() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let (log, state) = SchemaLog::open(dir.path()).expect("open schema log");
        let mut c =
            Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        c.attach_schema(state, log);
        c.apply_remote_schema_entries(&[schema_create(7, 700, "by_a", "a")]);

        assert_eq!(c.gossip_payload().schema_version, 7);
    }

    /// End-to-end smoke test of the Phase 44b pull RPC. We spin up a
    /// minimal acceptor that runs the same `pull_schema_from_peer`
    /// dispatch as the live peer listener, then issue the call from
    /// a separate client task.
    #[tokio::test]
    async fn test_pull_schema_from_peer_round_trip() {
        use crate::cluster::handshake::{
            HandshakeOutcome, PeerDataOp, PeerDataOpResponse, pull_schema_from_peer, read_frame,
            run_acceptor_with_hello_and_schema, write_frame,
        };

        let server_identity = identity();
        let server_uuid = server_identity.node_id;
        let cluster_uuid = server_identity.cluster_id;

        let dir = tempfile::TempDir::new().expect("tmp");
        let (log, state) = SchemaLog::open(dir.path()).expect("open schema log");
        let mut server_coord = Coordinator::from_config(
            &server_identity,
            &ClusterSection::default(),
            "127.0.0.1:6380",
        );
        server_coord.attach_schema(state, log);
        server_coord.apply_remote_schema_entries(&[
            schema_create(1, 100, "by_a", "a"),
            schema_create(2, 200, "by_b", "b"),
            schema_create(3, 300, "by_c", "c"),
        ]);
        let server_coord = Arc::new(server_coord);
        assert_eq!(server_coord.schema_version(), 3);

        // Client identity (must be in known_peers so the handshake
        // is accepted).
        let client_uuid = Uuid::new_v4();

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let server_addr = listener.local_addr().expect("local addr").to_string();

        let server_task = {
            let server_coord = server_coord.clone();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut known = HashSet::new();
                known.insert(client_uuid);
                let local_schema_version = server_coord.schema_version();
                let (outcome, _hello) = run_acceptor_with_hello_and_schema(
                    &mut stream,
                    cluster_uuid,
                    server_uuid,
                    "grumpydb-test",
                    &known,
                    local_schema_version,
                )
                .await
                .expect("acceptor handshake");
                assert!(matches!(outcome, HandshakeOutcome::Accepted { .. }));

                let op: PeerDataOp = read_frame(&mut stream).await.expect("read op");
                let resp = match op {
                    PeerDataOp::PullSchemaSince { since_version } => {
                        PeerDataOpResponse::SchemaDiff {
                            entries: server_coord.local_schema_diff_since(since_version),
                        }
                    }
                    other => panic!("expected PullSchemaSince, got {other:?}"),
                };
                write_frame(&mut stream, &resp).await.expect("write resp");
            })
        };

        let ctx = PeerRpcContext {
            addr: server_addr,
            local_cluster_id: cluster_uuid,
            local_node_id: client_uuid,
            server_version: "grumpydb-test-client".to_string(),
        };
        let entries = pull_schema_from_peer(&ctx, 1).await.expect("pull");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].version, 2);
        assert_eq!(entries[1].version, 3);

        let _ = server_task.await;
    }

    // ── Phase 44c: local DDL + materializer chain ────────────────────

    use grumpydb::SharedServer;
    use grumpydb::document::value::Value;
    use std::collections::BTreeMap;

    fn doc(name: &str, age: i64) -> Value {
        let mut map = BTreeMap::new();
        map.insert("name".into(), Value::String(name.into()));
        map.insert("age".into(), Value::Integer(age));
        Value::Object(map)
    }

    #[test]
    fn test_apply_local_ddl_returns_none_without_attached_schema() {
        let c = Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        assert!(
            c.apply_local_create_index("t", "d", "c", "by_x", "x", 100)
                .is_none()
        );
    }

    #[test]
    fn test_apply_local_ddl_advances_version_and_persists() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let (log, state) = SchemaLog::open(dir.path()).expect("open schema log");
        let mut c =
            Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        c.attach_schema(state, log);

        let e1 = c
            .apply_local_create_index("_system", "demo", "docs", "by_a", "a", 100)
            .expect("entry");
        assert_eq!(e1.version, 1);
        assert_eq!(c.schema_version(), 1);

        let e2 = c
            .apply_local_create_index("_system", "demo", "docs", "by_b", "b", 200)
            .expect("entry");
        assert_eq!(e2.version, 2);

        // Re-open the log and check both entries are persisted.
        let (_log2, state2) = SchemaLog::open(dir.path()).expect("reopen");
        assert_eq!(state2.version(), 2);
    }

    /// End-to-end: Coordinator with attached materializer, issue a
    /// local DDL, observe the on-disk index file appearing on the
    /// local SharedServer.
    #[tokio::test]
    async fn test_apply_local_ddl_materializes_index_on_disk() {
        use crate::cluster::schema::materializer;

        let dir = tempfile::TempDir::new().expect("tmp");
        let server = SharedServer::open(dir.path()).expect("open server");
        server.create_client("_system").expect("client");
        server.create_database("_system", "demo").expect("db");
        let db = server.database("_system", "demo").expect("open db");
        db.create_collection("docs").expect("collection");
        db.insert("docs", Uuid::new_v4(), doc("alice", 30))
            .expect("insert");

        let (log, state) = SchemaLog::open(dir.path()).expect("open schema log");
        let mut c =
            Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        c.attach_schema(state, log);
        c.attach_materializer(materializer::spawn(server.clone()));

        let entry = c
            .apply_local_create_index("_system", "demo", "docs", "by_name", "name", 100)
            .expect("entry");
        assert_eq!(entry.version, 1);

        let idx_path = dir
            .path()
            .join("_system")
            .join("demo")
            .join("docs")
            .join("idx_by_name.idx");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if idx_path.exists() {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("materializer did not create {idx_path:?} within 2s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Apply a remote schema entry on a node whose data dir already
    /// holds the underlying collection: the materializer must produce
    /// the local index file.
    #[tokio::test]
    async fn test_apply_remote_schema_entries_triggers_materialization() {
        use crate::cluster::schema::materializer;

        let dir = tempfile::TempDir::new().expect("tmp");
        let server = SharedServer::open(dir.path()).expect("open server");
        server.create_client("_system").expect("client");
        server.create_database("_system", "demo").expect("db");
        let db = server.database("_system", "demo").expect("open db");
        db.create_collection("docs").expect("collection");
        db.insert("docs", Uuid::new_v4(), doc("alice", 30))
            .expect("insert");

        let (log, state) = SchemaLog::open(dir.path()).expect("open schema log");
        let mut c =
            Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        c.attach_schema(state, log);
        c.attach_materializer(materializer::spawn(server.clone()));

        let applied = c.apply_remote_schema_entries(&[schema_create(7, 700, "by_name", "name")]);
        assert_eq!(applied, 1);
        assert_eq!(c.schema_version(), 7);

        let idx_path = dir
            .path()
            .join("_system")
            .join("demo")
            .join("docs")
            .join("idx_by_name.idx");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if idx_path.exists() {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("materializer did not create {idx_path:?} within 2s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    // ── Phase 44d: SCHEMA STATUS / SCHEMA VERSION introspection ──

    #[test]
    fn test_schema_status_json_when_unattached_is_empty() {
        let c = Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        let snap = c.schema_status_json();
        assert_eq!(snap["schema_version"], 0);
        assert!(snap["indexes"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_schema_status_json_lists_live_and_tombstoned_entries() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let (log, state) = SchemaLog::open(dir.path()).expect("open schema log");
        let mut c =
            Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        c.attach_schema(state, log);
        c.apply_remote_schema_entries(&[
            schema_create(1, 100, "by_a", "a"),
            schema_create(2, 200, "by_b", "b"),
        ]);
        c.apply_local_drop_index("_system", "demo", "docs", "by_a", 300);

        let snap = c.schema_status_json();
        assert_eq!(snap["schema_version"], 3);
        let indexes = snap["indexes"].as_array().unwrap();
        assert_eq!(indexes.len(), 2);
        let by_a = indexes
            .iter()
            .find(|e| e["index_name"] == "by_a")
            .expect("by_a entry");
        assert_eq!(by_a["tombstone"], true);
        let by_b = indexes
            .iter()
            .find(|e| e["index_name"] == "by_b")
            .expect("by_b entry");
        assert_eq!(by_b["tombstone"], false);
    }

    #[test]
    fn test_schema_has_index_returns_false_for_missing_or_tombstoned() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let (log, state) = SchemaLog::open(dir.path()).expect("open schema log");
        let mut c =
            Coordinator::from_config(&identity(), &ClusterSection::default(), "127.0.0.1:6380");
        c.attach_schema(state, log);
        c.apply_remote_schema_entries(&[schema_create(1, 100, "by_a", "a")]);

        assert!(c.schema_has_index("_system", "demo", "docs", "by_a"));
        assert!(!c.schema_has_index("_system", "demo", "docs", "by_missing"));

        c.apply_local_drop_index("_system", "demo", "docs", "by_a", 200);
        assert!(!c.schema_has_index("_system", "demo", "docs", "by_a"));
    }
}
