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

/// Auth token lifetimes.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthSection {
    #[serde(default = "default_access_ttl")]
    pub access_token_ttl_secs: u64,
    #[serde(default = "default_refresh_ttl")]
    pub refresh_token_ttl_secs: u64,
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
"#;
        let cfg: ServerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.server.bind, "127.0.0.1:9999");
        assert_eq!(cfg.server.max_connections, 50);
        assert!(cfg.tls.enabled);
        assert_eq!(cfg.tls.cert_file.as_deref(), Some("cert.pem"));
        assert_eq!(cfg.auth.access_token_ttl_secs, 1800);
        assert_eq!(cfg.http.bind, "127.0.0.1:7777");
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
