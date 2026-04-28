//! Server configuration: parsed from `grumpydb.toml` or CLI args.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Top-level server configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub tls: TlsSection,
    #[serde(default)]
    pub auth: AuthSection,
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
"#;
        let cfg: ServerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.server.bind, "127.0.0.1:9999");
        assert_eq!(cfg.server.max_connections, 50);
        assert!(cfg.tls.enabled);
        assert_eq!(cfg.tls.cert_file.as_deref(), Some("cert.pem"));
        assert_eq!(cfg.auth.access_token_ttl_secs, 1800);
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
}
