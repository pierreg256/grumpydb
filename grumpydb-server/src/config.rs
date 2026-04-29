//! Server configuration: parsed from `grumpydb.toml` or CLI args.

use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::limits::LimitsConfig;

/// Top-level server configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub tls: TlsSection,
    #[serde(default)]
    pub auth: AuthSection,
    #[serde(default)]
    pub limits: LimitsSection,
    #[serde(default)]
    pub http: HttpSection,
    #[serde(default)]
    pub cluster: ClusterSection,
}

/// Network and data directory settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerSection {
    pub bind: String,
    pub max_connections: usize,
    pub data_dir: PathBuf,
}

/// TLS settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TlsSection {
    pub enabled: bool,
    pub cert_file: Option<String>,
    pub key_file: Option<String>,
}

/// Auth token lifetimes and signing algorithm.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthSection {
    #[serde(default = "default_access_ttl")]
    pub access_token_ttl_secs: u64,
    #[serde(default = "default_refresh_ttl")]
    pub refresh_token_ttl_secs: u64,
    /// JWT signing algorithm to use when bootstrapping a fresh auth
    /// store. `"rs256"` (default) generates an RSA-2048 keypair on
    /// first start — strongly recommended for production. `"hs256"`
    /// uses a 32-byte symmetric secret — cheaper to bootstrap (no
    /// keygen) so it's the right choice for short-lived test
    /// processes that spawn fresh data dirs in a tight loop.
    ///
    /// On a server that already has an auth store on disk, this field
    /// is IGNORED — the on-disk algorithm always wins.
    #[serde(default = "default_jwt_algorithm")]
    pub jwt_algorithm: String,
}

fn default_jwt_algorithm() -> String {
    "rs256".to_string()
}

/// Observability HTTP server settings.
///
/// Hosts the unauthenticated `/healthz`, `/readyz`, and `/metrics`
/// endpoints used by Kubernetes probes and Prometheus scrape jobs. See
/// [`crate::http`] for the endpoint catalogue and the security caveats.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HttpSection {
    /// HTTP bind address for `/healthz`, `/readyz`, `/metrics`. Default
    /// `0.0.0.0:6381`. Set to an empty string to disable the HTTP server
    /// entirely (useful for tests or air-gapped deployments).
    pub bind: String,
}

impl Default for HttpSection {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:6381".to_string(),
        }
    }
}

/// Cluster topology and membership configuration (Phase 40a).
///
/// In v5 the peer list is fully static — operators write each peer's
/// `node_id` and address into `grumpydb.toml`. v6 will repurpose the
/// same struct as the live view of the gossip-derived membership;
/// reserved fields like [`PeerEntry::status`] and
/// [`PeerEntry::vnode_assignments`] are part of the schema today even
/// though v5 ignores them, so the v5 → v6 transition is a behavior
/// change, not a config schema change.
///
/// All fields are optional. The all-default value (no peers, empty
/// `listen_peer`) leaves the server in single-node mode.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClusterSection {
    /// Required if any peer is configured. Must match every peer.
    /// Encoded as a UUID string in TOML. When `None`, the value is
    /// taken from the on-disk `node.json` (see
    /// [`crate::cluster::NodeIdentity`]).
    pub cluster_id: Option<String>,
    /// Bind address for the inter-node WAL stream port (Phase 40e).
    /// Empty string disables inter-node TCP entirely (single-node
    /// deployment).
    pub listen_peer: String,
    /// Static peer list. Each entry MUST include the peer's stable
    /// `node_id` so spoofing is detectable at the handshake stage.
    pub peers: Vec<PeerEntry>,
    /// Number of virtual nodes per physical node on the consistent
    /// hash ring (Phase 40c). Default 256, Cassandra-style.
    pub vnodes_per_node: u32,
    /// Tombstone GC grace period in seconds (Phase 40d). Default 10
    /// days.
    pub gc_grace_seconds: u64,
    /// Replication-lag threshold in seconds: above this, `/readyz`
    /// returns 503 (Phase 40e).
    pub max_lag_seconds: u64,
    /// Per-collection writer assignment. v5 manual; v6 dynamic.
    pub writers: Vec<WriterEntry>,
}

/// Static description of a single peer node.
///
/// `node_id` is verified during the cluster handshake — a peer that
/// presents a different id is rejected even if its `cluster_id` matches.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PeerEntry {
    /// Stable identifier of the peer (UUID string).
    pub node_id: String,
    /// Network address of the peer's `listen_peer` port (`host:port`).
    pub addr: String,
    /// Reserved for v6 gossip: live status enum (`up`, `down`, …).
    #[serde(default)]
    pub status: Option<String>,
    /// Reserved for v6 gossip: wall-clock seconds when the peer was
    /// last heard from.
    #[serde(default)]
    pub last_seen_at_unix: Option<u64>,
    /// Reserved for v6 gossip: ring slots owned by this peer.
    #[serde(default)]
    pub vnode_assignments: Vec<u32>,
}

/// Static writer assignment for a collection (single-writer mode).
///
/// `collection` may be the literal `*` to assign the database default.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct WriterEntry {
    /// Either an exact collection name or `*` for the database default.
    pub collection: String,
    /// `node_id` of the node responsible for accepting writes.
    pub node_id: String,
}

impl Default for ClusterSection {
    fn default() -> Self {
        Self {
            cluster_id: None,
            listen_peer: String::new(),
            peers: Vec::new(),
            vnodes_per_node: 256,
            gc_grace_seconds: 864_000,
            max_lag_seconds: 5,
            writers: Vec::new(),
        }
    }
}

/// Connection-level rate limit and brute-force protection settings.
///
/// Mirrors [`LimitsConfig`] but with serde defaults so an absent or partial
/// `[limits]` block is filled in with sensible values matching
/// [`LimitsConfig::default`].
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LimitsSection {
    pub commands_per_sec_per_ip: u32,
    pub commands_burst_per_ip: u32,
    pub failed_logins_per_min_per_ip: u32,
    pub max_conns_per_ip: usize,
    pub max_conns_global: usize,
    /// Bypass all limits for loopback peers (default `true`).
    /// See [`crate::limits::LimitsConfig::bypass_loopback`].
    pub bypass_loopback: bool,
}

impl Default for LimitsSection {
    fn default() -> Self {
        let d = LimitsConfig::default();
        Self {
            commands_per_sec_per_ip: d.commands_per_sec_per_ip,
            commands_burst_per_ip: d.commands_burst_per_ip,
            failed_logins_per_min_per_ip: d.failed_logins_per_min_per_ip,
            max_conns_per_ip: d.max_conns_per_ip,
            max_conns_global: d.max_conns_global,
            bypass_loopback: d.bypass_loopback,
        }
    }
}

impl From<&LimitsSection> for LimitsConfig {
    fn from(s: &LimitsSection) -> Self {
        Self {
            commands_per_sec_per_ip: s.commands_per_sec_per_ip,
            commands_burst_per_ip: s.commands_burst_per_ip,
            failed_logins_per_min_per_ip: s.failed_logins_per_min_per_ip,
            max_conns_per_ip: s.max_conns_per_ip,
            max_conns_global: s.max_conns_global,
            bypass_loopback: s.bypass_loopback,
        }
    }
}

fn default_access_ttl() -> u64 {
    3600
}
fn default_refresh_ttl() -> u64 {
    604800
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:6380".to_string(),
            max_connections: 1024,
            data_dir: PathBuf::from("./data"),
        }
    }
}

#[allow(clippy::derivable_impls)]
impl Default for TlsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            cert_file: None,
            key_file: None,
        }
    }
}

impl Default for AuthSection {
    fn default() -> Self {
        Self {
            access_token_ttl_secs: default_access_ttl(),
            refresh_token_ttl_secs: default_refresh_ttl(),
            jwt_algorithm: default_jwt_algorithm(),
        }
    }
}

#[allow(clippy::derivable_impls)]
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server: ServerSection::default(),
            tls: TlsSection::default(),
            auth: AuthSection::default(),
            limits: LimitsSection::default(),
            http: HttpSection::default(),
            cluster: ClusterSection::default(),
        }
    }
}

impl ServerConfig {
    /// Load config from a TOML file, falling back to defaults for missing fields.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read config: {e}"))?;
        toml::from_str(&content).map_err(|e| format!("invalid config: {e}"))
    }

    /// Load config from file if it exists, otherwise use defaults.
    pub fn load(path: &Path) -> Self {
        if path.exists() {
            Self::from_file(path).unwrap_or_default()
        } else {
            Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.server.bind, "0.0.0.0:6380");
        assert_eq!(cfg.server.max_connections, 1024);
        assert!(!cfg.tls.enabled);
        assert_eq!(cfg.auth.access_token_ttl_secs, 3600);
    }

    #[test]
    fn test_parse_toml() {
        let toml = r#"
[server]
bind = "127.0.0.1:9999"
max_connections = 50

[tls]
enabled = true
cert_file = "cert.pem"
key_file = "key.pem"

[auth]
access_token_ttl_secs = 1800

[http]
bind = "127.0.0.1:7777"

[cluster]
cluster_id = "5b1f3a40-7e21-4fa9-8d3a-1b6c0a7b8e9f"
listen_peer = "0.0.0.0:6390"
vnodes_per_node = 128
gc_grace_seconds = 3600
max_lag_seconds = 10
peers = [
  { node_id = "11111111-1111-1111-1111-111111111111", addr = "node-2:6390" },
  { node_id = "22222222-2222-2222-2222-222222222222", addr = "node-3:6390", status = "up", last_seen_at_unix = 1700000000, vnode_assignments = [0, 1, 2] },
]
writers = [
  { collection = "*", node_id = "11111111-1111-1111-1111-111111111111" },
  { collection = "events", node_id = "22222222-2222-2222-2222-222222222222" },
]
"#;
        let cfg: ServerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.server.bind, "127.0.0.1:9999");
        assert_eq!(cfg.server.max_connections, 50);
        assert!(cfg.tls.enabled);
        assert_eq!(cfg.tls.cert_file.as_deref(), Some("cert.pem"));
        assert_eq!(cfg.auth.access_token_ttl_secs, 1800);
        assert_eq!(cfg.http.bind, "127.0.0.1:7777");

        assert_eq!(
            cfg.cluster.cluster_id.as_deref(),
            Some("5b1f3a40-7e21-4fa9-8d3a-1b6c0a7b8e9f")
        );
        assert_eq!(cfg.cluster.listen_peer, "0.0.0.0:6390");
        assert_eq!(cfg.cluster.vnodes_per_node, 128);
        assert_eq!(cfg.cluster.gc_grace_seconds, 3600);
        assert_eq!(cfg.cluster.max_lag_seconds, 10);
        assert_eq!(cfg.cluster.peers.len(), 2);
        assert_eq!(cfg.cluster.peers[0].addr, "node-2:6390");
        assert_eq!(cfg.cluster.peers[1].status.as_deref(), Some("up"));
        assert_eq!(cfg.cluster.peers[1].last_seen_at_unix, Some(1700000000));
        assert_eq!(cfg.cluster.peers[1].vnode_assignments, vec![0, 1, 2]);
        assert_eq!(cfg.cluster.writers.len(), 2);
        assert_eq!(cfg.cluster.writers[0].collection, "*");
        assert_eq!(cfg.cluster.writers[1].collection, "events");
    }

    #[test]
    fn test_default_cluster_section() {
        let cfg = ServerConfig::default();
        assert!(cfg.cluster.cluster_id.is_none());
        assert!(cfg.cluster.listen_peer.is_empty());
        assert!(cfg.cluster.peers.is_empty());
        assert!(cfg.cluster.writers.is_empty());
        assert_eq!(cfg.cluster.vnodes_per_node, 256);
        assert_eq!(cfg.cluster.gc_grace_seconds, 864_000);
        assert_eq!(cfg.cluster.max_lag_seconds, 5);
    }

    #[test]
    fn test_default_http_section() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.http.bind, "0.0.0.0:6381");
    }

    #[test]
    fn test_parse_http_section_disabled_by_empty_bind() {
        let toml = r#"
[http]
bind = ""
"#;
        let cfg: ServerConfig = toml::from_str(toml).unwrap();
        assert!(cfg.http.bind.is_empty());
    }

    #[test]
    fn test_parse_partial_toml() {
        let toml = r#"
[server]
bind = "localhost:6380"
"#;
        let cfg: ServerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.server.bind, "localhost:6380");
        assert_eq!(cfg.server.max_connections, 1024); // default
        assert!(!cfg.tls.enabled); // default
    }

    #[test]
    fn test_parse_empty_toml() {
        let cfg: ServerConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.server.bind, "0.0.0.0:6380");
    }

    #[test]
    fn test_parse_limits_section() {
        let toml = r#"
[limits]
commands_per_sec_per_ip = 50
commands_burst_per_ip = 80
failed_logins_per_min_per_ip = 3
max_conns_per_ip = 25
max_conns_global = 500
"#;
        let cfg: ServerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.limits.commands_per_sec_per_ip, 50);
        assert_eq!(cfg.limits.commands_burst_per_ip, 80);
        assert_eq!(cfg.limits.failed_logins_per_min_per_ip, 3);
        assert_eq!(cfg.limits.max_conns_per_ip, 25);
        assert_eq!(cfg.limits.max_conns_global, 500);
    }

    #[test]
    fn test_limits_section_defaults_match_limits_config() {
        let s = LimitsSection::default();
        let c: LimitsConfig = (&s).into();
        let d = LimitsConfig::default();
        assert_eq!(c.commands_per_sec_per_ip, d.commands_per_sec_per_ip);
        assert_eq!(c.commands_burst_per_ip, d.commands_burst_per_ip);
        assert_eq!(
            c.failed_logins_per_min_per_ip,
            d.failed_logins_per_min_per_ip
        );
        assert_eq!(c.max_conns_per_ip, d.max_conns_per_ip);
        assert_eq!(c.max_conns_global, d.max_conns_global);
    }
}
