//! Cluster identity and (statically configured) membership.
//!
//! Phase 40a establishes the durable per-node identity (`node_id`,
//! `cluster_id`) used by every later distribution phase (HLC, ring,
//! replication). The on-disk artefact is a single JSON file:
//!
//! ```text
//! <data_dir>/_cluster/node.json
//! ```
//!
//! ```json
//! {
//!   "node_id":         "8aa6...-4f3c-...",
//!   "cluster_id":      "5b1f...-7e21-...",
//!   "created_at_unix": 1735689600,
//!   "identity_version": 1
//! }
//! ```
//!
//! The file is non-secret (only opaque IDs) and is written `chmod 0644`.
//!
//! See [`NodeIdentity`] for the data model and [`handshake`] for the
//! peer-to-peer cluster handshake protocol.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod handshake;

/// Highest `identity_version` understood by this server.
pub const IDENTITY_VERSION_CURRENT: u32 = 1;

fn default_identity_version() -> u32 {
    IDENTITY_VERSION_CURRENT
}

/// Persistent identity of this node within a cluster.
///
/// Loaded from (or created at) `<data_dir>/_cluster/node.json`. The
/// values are stable for the lifetime of the data directory: deleting
/// the file effectively makes the node forget its place in the cluster
/// and treat itself as a brand-new member.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeIdentity {
    /// Stable, unique identifier for this physical node.
    pub node_id: Uuid,
    /// Identifier shared by every node belonging to the same cluster.
    /// All peers MUST agree on this value or the [`handshake`] is
    /// rejected.
    pub cluster_id: Uuid,
    /// Wall-clock seconds since the UNIX epoch when this identity was
    /// minted (informational).
    pub created_at_unix: u64,
    /// Schema version of `node.json`. Bumped on any
    /// backwards-incompatible change to the file layout.
    #[serde(default = "default_identity_version")]
    pub identity_version: u32,
}

/// Errors returned by [`NodeIdentity`] APIs.
#[derive(thiserror::Error, Debug)]
pub enum IdentityError {
    /// Underlying I/O failure while reading or writing `node.json`.
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    /// `node.json` exists but cannot be parsed (corrupt JSON, missing
    /// fields, or invalid UUID). The server refuses to silently
    /// overwrite it — operators must investigate.
    #[error("malformed node.json: {0}")]
    Malformed(String),
    /// [`NodeIdentity::create`] was called against a directory that
    /// already contains a `node.json`.
    #[error("identity already exists at {0} — refusing to overwrite")]
    AlreadyExists(PathBuf),
    /// On-disk `identity_version` is newer than what this binary
    /// supports. Operator action: upgrade the binary.
    #[error("unknown identity version {0}; this server requires <= {1}")]
    UnknownVersion(u32, u32),
}

/// Subdirectory (relative to the server data dir) holding cluster state.
const CLUSTER_DIR: &str = "_cluster";
/// Filename of the on-disk identity record.
const NODE_FILE: &str = "node.json";

impl NodeIdentity {
    /// Path to `<data_dir>/_cluster/node.json`.
    fn node_file(data_dir: &Path) -> PathBuf {
        data_dir.join(CLUSTER_DIR).join(NODE_FILE)
    }

    /// Load `<data_dir>/_cluster/node.json` if it exists.
    ///
    /// Returns `Ok(None)` if the file is absent (fresh data directory),
    /// `Ok(Some(_))` on a successful load, and
    /// `Err(IdentityError::Malformed)` / `Err(IdentityError::UnknownVersion)`
    /// if the file is present but unusable.
    pub fn load(data_dir: &Path) -> Result<Option<Self>, IdentityError> {
        let path = Self::node_file(data_dir);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let parsed: NodeIdentity = serde_json::from_slice(&bytes)
            .map_err(|e| IdentityError::Malformed(format!("{}: {e}", path.display())))?;
        if parsed.identity_version > IDENTITY_VERSION_CURRENT {
            return Err(IdentityError::UnknownVersion(
                parsed.identity_version,
                IDENTITY_VERSION_CURRENT,
            ));
        }
        Ok(Some(parsed))
    }

    /// Generate a fresh identity and persist it under `data_dir`.
    ///
    /// If `cluster_id` is `None`, a new random cluster id is generated
    /// (bootstrap of the very first node of a new cluster). Otherwise
    /// the supplied id is used (this node joins an existing cluster).
    /// `node_id` is always freshly generated.
    ///
    /// Refuses to overwrite an existing `node.json`: operators wanting
    /// to wipe must `rm -r <data_dir>/_cluster` first.
    pub fn create(data_dir: &Path, cluster_id: Option<Uuid>) -> Result<Self, IdentityError> {
        let path = Self::node_file(data_dir);
        if path.exists() {
            return Err(IdentityError::AlreadyExists(path));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let identity = NodeIdentity {
            node_id: Uuid::new_v4(),
            cluster_id: cluster_id.unwrap_or_else(Uuid::new_v4),
            created_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            identity_version: IDENTITY_VERSION_CURRENT,
        };
        let json = serde_json::to_vec_pretty(&identity)
            .map_err(|e| IdentityError::Malformed(format!("serialize: {e}")))?;
        fs::write(&path, &json)?;
        // Tighten permissions to 0644 on Unix. node.json is non-secret
        // but there's no reason to leave it world-writable either.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
        }
        Ok(identity)
    }

    /// Convenience entry point used by `main.rs` on every startup:
    /// load if present, otherwise mint a brand-new identity.
    pub fn load_or_create(data_dir: &Path) -> Result<Self, IdentityError> {
        if let Some(existing) = Self::load(data_dir)? {
            return Ok(existing);
        }
        Self::create(data_dir, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_node_identity_create_and_load_round_trip() {
        let tmp = TempDir::new().unwrap();
        let created = NodeIdentity::create(tmp.path(), None).unwrap();
        let loaded = NodeIdentity::load(tmp.path()).unwrap().unwrap();
        assert_eq!(created, loaded);
        assert_eq!(loaded.identity_version, IDENTITY_VERSION_CURRENT);
    }

    #[test]
    fn test_node_identity_load_returns_none_when_missing() {
        let tmp = TempDir::new().unwrap();
        let loaded = NodeIdentity::load(tmp.path()).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn test_node_identity_create_refuses_overwrite() {
        let tmp = TempDir::new().unwrap();
        let _ = NodeIdentity::create(tmp.path(), None).unwrap();
        let err = NodeIdentity::create(tmp.path(), None).unwrap_err();
        assert!(matches!(err, IdentityError::AlreadyExists(_)));
    }

    #[test]
    fn test_node_identity_load_rejects_corrupt_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(CLUSTER_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(NODE_FILE), b"garbage{not json").unwrap();
        let err = NodeIdentity::load(tmp.path()).unwrap_err();
        assert!(matches!(err, IdentityError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn test_node_identity_unknown_version_rejected() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(CLUSTER_DIR);
        fs::create_dir_all(&dir).unwrap();
        let json = serde_json::json!({
            "node_id": Uuid::new_v4(),
            "cluster_id": Uuid::new_v4(),
            "created_at_unix": 0,
            "identity_version": 999,
        });
        fs::write(dir.join(NODE_FILE), json.to_string().as_bytes()).unwrap();
        let err = NodeIdentity::load(tmp.path()).unwrap_err();
        assert!(
            matches!(err, IdentityError::UnknownVersion(999, _)),
            "got {err:?}"
        );
    }

    #[test]
    fn test_node_identity_load_or_create_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let first = NodeIdentity::load_or_create(tmp.path()).unwrap();
        let second = NodeIdentity::load_or_create(tmp.path()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn test_node_identity_create_with_explicit_cluster_id() {
        let tmp = TempDir::new().unwrap();
        let cluster_id = Uuid::new_v4();
        let id = NodeIdentity::create(tmp.path(), Some(cluster_id)).unwrap();
        assert_eq!(id.cluster_id, cluster_id);
    }
}
