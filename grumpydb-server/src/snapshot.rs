//! Snapshot & restore tooling.
//!
//! This module produces and consumes `tar.gz` archives of an entire
//! GrumpyDB data directory (engine files **and** the `_auth/` tree),
//! together with a JSON manifest (`snapshot.json`) at the archive root.
//! Each archived file is checksummed (SHA-256); restore verifies every
//! checksum before publishing the file to its destination path.
//!
//! # Backends
//!
//! - **Local**: a path with no scheme (or an absolute path) writes/reads
//!   the archive on the local filesystem. Always available.
//! - **S3** (`s3://bucket/key`): AWS S3, gated behind the `cloud-aws`
//!   Cargo feature. Uses the standard credential chain
//!   ([`aws-config`](https://docs.rs/aws-config)).
//! - **Azure Blob** (`az://container/blob`): Azure Blob Storage, gated
//!   behind the `cloud-azure` feature. Authenticates via
//!   `AZURE_STORAGE_CONNECTION_STRING` if present, otherwise via
//!   `DefaultAzureCredential`.
//!
//! # Concurrency model (v5)
//!
//! v5 has no MVCC. To prevent torn writes during the file-copy phase,
//! callers should ensure no other process is mutating the data directory
//! while [`snapshot`] runs (typically by stopping the server, or by
//! relying on the in-process `SharedDatabase` write-lock if the
//! caller co-resides with the engine).
//!
//! This module operates **directly on file paths**: it does not open the
//! engine. It is therefore safe to run from a sidecar tool, but it is the
//! operator's responsibility to quiesce writes first.
//!
//! # File layout inside the archive
//!
//! ```text
//! snapshot.json
//! data/_auth/secret.key
//! data/_auth/users/admin.json
//! data/<tenant>/<database>/<collection>/data.db
//! data/<tenant>/<database>/<collection>/primary.idx
//! ...
//! ```

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::{Archive, Builder, Header};

/// Manifest schema version stored in [`Manifest::version`].
pub const MANIFEST_VERSION: u32 = 1;

/// Filename of the manifest entry inside the archive.
pub const MANIFEST_FILENAME: &str = "snapshot.json";

/// Root directory inside the archive that holds the actual data files.
pub const DATA_PREFIX: &str = "data/";

/// Options for [`snapshot`] and [`restore`].
#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    /// Source data directory (snapshot) or target data directory (restore).
    pub data_dir: PathBuf,
    /// Allow restore over a non-empty data directory.
    pub force: bool,
}

/// Manifest stored at the root of every archive as `snapshot.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Schema version of the manifest. See [`MANIFEST_VERSION`].
    pub version: u32,
    /// `CARGO_PKG_VERSION` of the producer.
    pub grumpydb_version: String,
    /// Wall-clock time at archive creation, in seconds since UNIX epoch.
    pub created_at_unix: u64,
    /// Every file embedded in the archive (excluding the manifest itself).
    pub data_files: Vec<FileEntry>,
}

/// One archived file (path relative to the data dir, byte size, sha256).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to the snapshot's data dir (forward slashes).
    pub path: String,
    /// Byte size of the file.
    pub size: u64,
    /// SHA-256 of the file's bytes, lower-case hex.
    pub sha256_hex: String,
}

/// Backend location for a snapshot archive. Parsed from a user-supplied
/// URL with [`Location::parse`].
#[derive(Debug, Clone)]
pub enum Location {
    /// Local filesystem path.
    Local(PathBuf),
    /// AWS S3 destination, available with the `cloud-aws` feature.
    #[cfg(feature = "cloud-aws")]
    S3 {
        /// S3 bucket name.
        bucket: String,
        /// Object key (no leading slash).
        key: String,
    },
    /// Azure Blob Storage destination, available with the `cloud-azure` feature.
    #[cfg(feature = "cloud-azure")]
    Azure {
        /// Container name.
        container: String,
        /// Blob name.
        blob: String,
    },
}

/// Errors produced by snapshot/restore operations.
#[derive(thiserror::Error, Debug)]
pub enum SnapshotError {
    /// Filesystem or I/O failure.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    /// User-supplied URL was malformed.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// Manifest could not be parsed or was missing.
    #[error("manifest: {0}")]
    Manifest(String),
    /// Restore destination is non-empty and `--force` was not set.
    #[error("destination not empty (use --force): {0}")]
    NotEmpty(String),
    /// SHA-256 mismatch between archive content and manifest.
    #[error("checksum mismatch on {file}: expected {expected}, got {actual}")]
    Checksum {
        /// Relative path of the offending file.
        file: String,
        /// Expected hex digest from the manifest.
        expected: String,
        /// Actual hex digest computed from the archive.
        actual: String,
    },
    /// Cloud provider error (network, credentials, permissions).
    #[error("cloud: {0}")]
    Cloud(String),
    /// URL scheme requires a Cargo feature that is not compiled in.
    #[error("backend not enabled: {0} (rebuild with --features {1})")]
    BackendDisabled(String, String),
    /// Catch-all for unexpected errors.
    #[error("other: {0}")]
    Other(String),
}

impl Location {
    /// Parse a user-supplied URL into a backend location.
    ///
    /// Accepted shapes:
    ///
    /// - `/abs/path`, `./rel/path`, `path` → `Location::Local`.
    /// - `s3://bucket/key` → `Location::S3` (requires `cloud-aws`).
    /// - `az://container/blob` → `Location::Azure` (requires `cloud-azure`).
    pub fn parse(s: &str) -> Result<Self, SnapshotError> {
        if let Some(rest) = s.strip_prefix("s3://") {
            let (bucket, key) = split_once_strict(rest, '/')
                .ok_or_else(|| SnapshotError::InvalidUrl(format!("s3 URL missing key: {s}")))?;
            if bucket.is_empty() || key.is_empty() {
                return Err(SnapshotError::InvalidUrl(format!(
                    "s3 URL has empty bucket or key: {s}"
                )));
            }
            #[cfg(feature = "cloud-aws")]
            {
                return Ok(Location::S3 {
                    bucket: bucket.to_string(),
                    key: key.to_string(),
                });
            }
            #[cfg(not(feature = "cloud-aws"))]
            {
                let _ = (bucket, key);
                return Err(SnapshotError::BackendDisabled(
                    "s3".into(),
                    "cloud-aws".into(),
                ));
            }
        }

        if let Some(rest) = s.strip_prefix("az://") {
            let (container, blob) = split_once_strict(rest, '/').ok_or_else(|| {
                SnapshotError::InvalidUrl(format!("az URL missing blob name: {s}"))
            })?;
            if container.is_empty() || blob.is_empty() {
                return Err(SnapshotError::InvalidUrl(format!(
                    "az URL has empty container or blob: {s}"
                )));
            }
            #[cfg(feature = "cloud-azure")]
            {
                return Ok(Location::Azure {
                    container: container.to_string(),
                    blob: blob.to_string(),
                });
            }
            #[cfg(not(feature = "cloud-azure"))]
            {
                let _ = (container, blob);
                return Err(SnapshotError::BackendDisabled(
                    "az".into(),
                    "cloud-azure".into(),
                ));
            }
        }

        // Reject malformed URI-ish strings (anything that looks like a
        // scheme but doesn't match a recognised one). A "scheme prefix" is
        // an ASCII alphabetic byte followed by `[a-z0-9+.-]*` and a colon
        // — but only if the colon comes before any path separator.
        if looks_like_scheme(s) {
            return Err(SnapshotError::InvalidUrl(format!(
                "unsupported or malformed URL: {s}"
            )));
        }

        Ok(Location::Local(PathBuf::from(s)))
    }
}

fn looks_like_scheme(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' {
            // Found a colon before any path separator — treat as a scheme.
            return i > 0;
        }
        if b == b'/' || b == b'\\' {
            return false;
        }
        if !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) {
            return false;
        }
    }
    false
}

fn split_once_strict(s: &str, sep: char) -> Option<(&str, &str)> {
    let idx = s.find(sep)?;
    Some((&s[..idx], &s[idx + sep.len_utf8()..]))
}

// ── Public API ───────────────────────────────────────────────────────────

/// Take a snapshot of `opts.data_dir` and write the resulting `tar.gz` to
/// `dest`.
///
/// For [`Location::Local`], the destination behaves as follows:
///
/// - If the path ends with `.tar.gz` or `.tgz`, it is used verbatim.
/// - If the path is an existing directory, the archive is written to
///   `<dir>/grumpydb-snapshot-<unix-ts>.tar.gz`.
/// - Otherwise the path is used verbatim (parent directories must exist).
pub async fn snapshot(opts: &SnapshotOptions, dest: &Location) -> Result<(), SnapshotError> {
    let bytes = build_archive(&opts.data_dir)?;
    write_to(dest, bytes).await
}

/// Restore a snapshot from `src` into `opts.data_dir`.
///
/// Refuses to overwrite a non-empty `data_dir` unless `opts.force` is set.
/// Every file is verified against its SHA-256 from the manifest before
/// being committed; on the first mismatch the partial output directory
/// is removed and [`SnapshotError::Checksum`] is returned.
pub async fn restore(opts: &SnapshotOptions, src: &Location) -> Result<(), SnapshotError> {
    let bytes = read_from(src).await?;
    extract_archive(&bytes, &opts.data_dir, opts.force)
}

// ── Local backend implementation ─────────────────────────────────────────

/// Walk `root` recursively (depth-first). Returns paths relative to
/// `root`, sorted lexicographically for determinism.
fn walk_files(root: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    fn rec(root: &Path, cur: &Path, out: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
        for entry in fs::read_dir(cur)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                rec(root, &path, out)?;
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .map_err(|e| std::io::Error::other(e.to_string()))?
                    .to_path_buf();
                out.push(rel);
            }
        }
        Ok(())
    }
    rec(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Build a `tar.gz` byte buffer containing `snapshot.json` and every file
/// under `data_dir` (placed under `data/`).
fn build_archive(data_dir: &Path) -> Result<Vec<u8>, SnapshotError> {
    if !data_dir.exists() {
        return Err(SnapshotError::Other(format!(
            "data directory does not exist: {}",
            data_dir.display()
        )));
    }

    let rel_paths = walk_files(data_dir)?;
    let mut entries = Vec::with_capacity(rel_paths.len());

    // Buffer files in memory once each, then build the manifest with their
    // checksums, then write the archive (manifest first, then all files).
    let mut file_bytes: Vec<(PathBuf, Vec<u8>)> = Vec::with_capacity(rel_paths.len());
    for rel in &rel_paths {
        let abs = data_dir.join(rel);
        let bytes = fs::read(&abs)?;
        let entry = FileEntry {
            path: to_archive_path(rel),
            size: bytes.len() as u64,
            sha256_hex: sha256_hex(&bytes),
        };
        entries.push(entry);
        file_bytes.push((rel.clone(), bytes));
    }

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        grumpydb_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        data_files: entries,
    };
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|e| SnapshotError::Manifest(e.to_string()))?;

    let buf = Vec::new();
    let gz = GzEncoder::new(buf, Compression::default());
    let mut tar = Builder::new(gz);

    // Manifest first.
    let mut header = Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(manifest.created_at_unix);
    header.set_cksum();
    tar.append_data(&mut header, MANIFEST_FILENAME, manifest_bytes.as_slice())?;

    for (rel, bytes) in file_bytes {
        let archive_path = format!("{DATA_PREFIX}{}", to_archive_path(&rel));
        let mut header = Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(manifest.created_at_unix);
        header.set_cksum();
        tar.append_data(&mut header, &archive_path, bytes.as_slice())?;
    }

    let gz = tar.into_inner()?;
    let buf = gz.finish()?;
    Ok(buf)
}

fn to_archive_path(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// Extract a `tar.gz` archive into `data_dir`.
fn extract_archive(bytes: &[u8], data_dir: &Path, force: bool) -> Result<(), SnapshotError> {
    if data_dir.exists() {
        let mut iter = fs::read_dir(data_dir)?;
        if iter.next().is_some() && !force {
            return Err(SnapshotError::NotEmpty(data_dir.display().to_string()));
        }
    } else {
        fs::create_dir_all(data_dir)?;
    }

    // First pass: collect the manifest and all entries' bytes from the tar.
    let mut manifest: Option<Manifest> = None;
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();

    {
        let gz = GzDecoder::new(bytes);
        let mut archive = Archive::new(gz);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry
                .path()?
                .to_string_lossy()
                .replace('\\', "/")
                .to_string();
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            if path == MANIFEST_FILENAME {
                let m: Manifest = serde_json::from_slice(&buf)
                    .map_err(|e| SnapshotError::Manifest(e.to_string()))?;
                manifest = Some(m);
            } else if let Some(rel) = path.strip_prefix(DATA_PREFIX) {
                files.push((rel.to_string(), buf));
            } else {
                // Ignore unknown entries (forward-compat).
            }
        }
    }

    let manifest = manifest.ok_or_else(|| {
        SnapshotError::Manifest(format!("missing {MANIFEST_FILENAME} in archive"))
    })?;

    if manifest.version != MANIFEST_VERSION {
        return Err(SnapshotError::Manifest(format!(
            "unsupported manifest version {} (this build supports {})",
            manifest.version, MANIFEST_VERSION
        )));
    }

    // Second pass: verify checksums BEFORE writing anything.
    use std::collections::HashMap;
    let actual: HashMap<&str, &[u8]> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();

    for entry in &manifest.data_files {
        let bytes = actual.get(entry.path.as_str()).ok_or_else(|| {
            SnapshotError::Manifest(format!("manifest references missing file: {}", entry.path))
        })?;
        let got = sha256_hex(bytes);
        if got != entry.sha256_hex {
            // Best-effort cleanup: nothing has been written yet, so no-op.
            return Err(SnapshotError::Checksum {
                file: entry.path.clone(),
                expected: entry.sha256_hex.clone(),
                actual: got,
            });
        }
    }

    // Third pass: write everything atomically-ish.
    for (rel, bytes) in &files {
        let abs = data_dir.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&abs)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }

    Ok(())
}

// ── Backend dispatch ─────────────────────────────────────────────────────

async fn write_to(dest: &Location, bytes: Vec<u8>) -> Result<(), SnapshotError> {
    match dest {
        Location::Local(p) => write_local(p, &bytes),
        #[cfg(feature = "cloud-aws")]
        Location::S3 { bucket, key } => write_s3(bucket, key, bytes).await,
        #[cfg(feature = "cloud-azure")]
        Location::Azure { container, blob } => write_azure(container, blob, bytes).await,
    }
}

async fn read_from(src: &Location) -> Result<Vec<u8>, SnapshotError> {
    match src {
        Location::Local(p) => Ok(fs::read(p)?),
        #[cfg(feature = "cloud-aws")]
        Location::S3 { bucket, key } => read_s3(bucket, key).await,
        #[cfg(feature = "cloud-azure")]
        Location::Azure { container, blob } => read_azure(container, blob).await,
    }
}

fn write_local(path: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
    let final_path = if path.is_dir() {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        path.join(format!("grumpydb-snapshot-{ts}.tar.gz"))
    } else {
        path.to_path_buf()
    };
    if let Some(parent) = final_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::File::create(&final_path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

// ── AWS S3 backend ───────────────────────────────────────────────────────

#[cfg(feature = "cloud-aws")]
async fn write_s3(bucket: &str, key: &str, bytes: Vec<u8>) -> Result<(), SnapshotError> {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let client = aws_sdk_s3::Client::new(&config);
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(aws_sdk_s3::primitives::ByteStream::from(bytes))
        .send()
        .await
        .map_err(|e| SnapshotError::Cloud(format!("S3 PutObject failed: {e}")))?;
    Ok(())
}

#[cfg(feature = "cloud-aws")]
async fn read_s3(bucket: &str, key: &str) -> Result<Vec<u8>, SnapshotError> {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let client = aws_sdk_s3::Client::new(&config);
    let resp = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| SnapshotError::Cloud(format!("S3 GetObject failed: {e}")))?;
    let bytes = resp
        .body
        .collect()
        .await
        .map_err(|e| SnapshotError::Cloud(format!("S3 stream read failed: {e}")))?;
    Ok(bytes.into_bytes().to_vec())
}

// ── Azure Blob backend ───────────────────────────────────────────────────

#[cfg(feature = "cloud-azure")]
async fn write_azure(container: &str, blob: &str, bytes: Vec<u8>) -> Result<(), SnapshotError> {
    let blob_client = azure_blob_client(container, blob)?;
    blob_client
        .put_block_blob(bytes)
        .await
        .map_err(|e| SnapshotError::Cloud(format!("Azure put_block_blob failed: {e}")))?;
    Ok(())
}

#[cfg(feature = "cloud-azure")]
async fn read_azure(container: &str, blob: &str) -> Result<Vec<u8>, SnapshotError> {
    use futures::StreamExt;
    let blob_client = azure_blob_client(container, blob)?;
    let mut stream = blob_client.get().into_stream();
    let mut buf = Vec::new();
    while let Some(value) = stream.next().await {
        let value =
            value.map_err(|e| SnapshotError::Cloud(format!("Azure get blob chunk failed: {e}")))?;
        let data = value
            .data
            .collect()
            .await
            .map_err(|e| SnapshotError::Cloud(format!("Azure data collect failed: {e}")))?;
        buf.extend_from_slice(&data);
    }
    Ok(buf)
}

#[cfg(feature = "cloud-azure")]
fn azure_blob_client(
    container: &str,
    blob: &str,
) -> Result<azure_storage_blobs::prelude::BlobClient, SnapshotError> {
    use azure_storage::StorageCredentials;
    use azure_storage_blobs::prelude::ClientBuilder;

    let (account, credentials) = if let Ok(conn) = std::env::var("AZURE_STORAGE_CONNECTION_STRING")
    {
        // Naive parser: extract AccountName=...;AccountKey=...
        let mut account = None;
        let mut key = None;
        for part in conn.split(';') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix("AccountName=") {
                account = Some(v.to_string());
            } else if let Some(v) = part.strip_prefix("AccountKey=") {
                key = Some(v.to_string());
            }
        }
        let account = account.ok_or_else(|| {
            SnapshotError::Cloud("AZURE_STORAGE_CONNECTION_STRING missing AccountName".into())
        })?;
        let key = key.ok_or_else(|| {
            SnapshotError::Cloud("AZURE_STORAGE_CONNECTION_STRING missing AccountKey".into())
        })?;
        (account, StorageCredentials::access_key("placeholder", key))
    } else if let Ok(account) = std::env::var("AZURE_STORAGE_ACCOUNT") {
        let creds = azure_identity::create_default_credential().map_err(|e| {
            SnapshotError::Cloud(format!("DefaultAzureCredential init failed: {e}"))
        })?;
        (account, StorageCredentials::token_credential(creds))
    } else {
        return Err(SnapshotError::Cloud(
            "set AZURE_STORAGE_CONNECTION_STRING or AZURE_STORAGE_ACCOUNT".into(),
        ));
    };

    let client = ClientBuilder::new(account, credentials).blob_client(container, blob);
    Ok(client)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn populate(dir: &Path) {
        fs::create_dir_all(dir.join("_auth/users")).unwrap();
        fs::write(dir.join("_auth/secret.key"), b"hunter2").unwrap();
        fs::write(dir.join("_auth/users/admin.json"), b"{\"u\":1}").unwrap();
        fs::create_dir_all(dir.join("tenant/db/coll")).unwrap();
        fs::write(dir.join("tenant/db/coll/data.db"), vec![0xab; 4096]).unwrap();
        fs::write(dir.join("tenant/db/coll/primary.idx"), vec![0xcd; 256]).unwrap();
    }

    fn read_dir_recursive(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        for rel in walk_files(root).unwrap() {
            let bytes = fs::read(root.join(&rel)).unwrap();
            out.push((rel, bytes));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    #[test]
    fn test_location_parse_local() {
        let cases = [("/tmp/foo", "/tmp/foo"), ("./foo", "./foo"), ("foo", "foo")];
        for (input, expected) in cases {
            let loc = Location::parse(input).unwrap();
            #[allow(unreachable_patterns)]
            match loc {
                Location::Local(p) => assert_eq!(p, PathBuf::from(expected)),
                _ => panic!("expected local for {input}"),
            }
        }
    }

    #[test]
    fn test_location_parse_invalid() {
        assert!(matches!(
            Location::parse("http://foo").unwrap_err(),
            SnapshotError::InvalidUrl(_)
        ));
        assert!(matches!(
            Location::parse("s3:/missing-slash").unwrap_err(),
            SnapshotError::InvalidUrl(_)
        ));
        assert!(matches!(
            Location::parse("s3://").unwrap_err(),
            SnapshotError::InvalidUrl(_)
        ));
        assert!(matches!(
            Location::parse("s3://bucket").unwrap_err(),
            SnapshotError::InvalidUrl(_)
        ));
    }

    #[cfg(feature = "cloud-aws")]
    #[test]
    fn test_location_parse_s3_enabled() {
        match Location::parse("s3://my-bucket/path/to/key.tar.gz").unwrap() {
            Location::S3 { bucket, key } => {
                assert_eq!(bucket, "my-bucket");
                assert_eq!(key, "path/to/key.tar.gz");
            }
            #[allow(unreachable_patterns)]
            _ => panic!("expected S3"),
        }
    }

    #[cfg(not(feature = "cloud-aws"))]
    #[test]
    fn test_location_parse_s3_disabled() {
        match Location::parse("s3://b/k").unwrap_err() {
            SnapshotError::BackendDisabled(scheme, feature) => {
                assert_eq!(scheme, "s3");
                assert_eq!(feature, "cloud-aws");
            }
            other => panic!("expected BackendDisabled, got {other:?}"),
        }
    }

    #[cfg(feature = "cloud-azure")]
    #[test]
    fn test_location_parse_azure_enabled() {
        match Location::parse("az://my-cont/some/blob.tar.gz").unwrap() {
            Location::Azure { container, blob } => {
                assert_eq!(container, "my-cont");
                assert_eq!(blob, "some/blob.tar.gz");
            }
            #[allow(unreachable_patterns)]
            _ => panic!("expected Azure"),
        }
    }

    #[cfg(not(feature = "cloud-azure"))]
    #[test]
    fn test_location_parse_azure_disabled() {
        match Location::parse("az://c/b").unwrap_err() {
            SnapshotError::BackendDisabled(scheme, feature) => {
                assert_eq!(scheme, "az");
                assert_eq!(feature, "cloud-azure");
            }
            other => panic!("expected BackendDisabled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_manifest_round_trip_local() {
        let src = TempDir::new().unwrap();
        populate(src.path());
        let archive_dir = TempDir::new().unwrap();
        let archive_path = archive_dir.path().join("snap.tar.gz");

        snapshot(
            &SnapshotOptions {
                data_dir: src.path().to_path_buf(),
                force: false,
            },
            &Location::Local(archive_path.clone()),
        )
        .await
        .unwrap();
        assert!(archive_path.exists());

        let dest = TempDir::new().unwrap();
        // dest is a fresh dir that already exists but is empty — must work.
        restore(
            &SnapshotOptions {
                data_dir: dest.path().to_path_buf(),
                force: false,
            },
            &Location::Local(archive_path.clone()),
        )
        .await
        .unwrap();

        assert_eq!(
            read_dir_recursive(src.path()),
            read_dir_recursive(dest.path()),
            "restored tree must be byte-equal to source"
        );
    }

    #[tokio::test]
    async fn test_restore_refuses_non_empty() {
        let src = TempDir::new().unwrap();
        populate(src.path());
        let archive_dir = TempDir::new().unwrap();
        let archive_path = archive_dir.path().join("snap.tar.gz");
        snapshot(
            &SnapshotOptions {
                data_dir: src.path().to_path_buf(),
                force: false,
            },
            &Location::Local(archive_path.clone()),
        )
        .await
        .unwrap();

        let dest = TempDir::new().unwrap();
        fs::write(dest.path().join("preexisting.txt"), b"hi").unwrap();

        let err = restore(
            &SnapshotOptions {
                data_dir: dest.path().to_path_buf(),
                force: false,
            },
            &Location::Local(archive_path),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SnapshotError::NotEmpty(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn test_restore_with_force() {
        let src = TempDir::new().unwrap();
        populate(src.path());
        let archive_dir = TempDir::new().unwrap();
        let archive_path = archive_dir.path().join("snap.tar.gz");
        snapshot(
            &SnapshotOptions {
                data_dir: src.path().to_path_buf(),
                force: false,
            },
            &Location::Local(archive_path.clone()),
        )
        .await
        .unwrap();

        let dest = TempDir::new().unwrap();
        fs::write(dest.path().join("preexisting.txt"), b"hi").unwrap();

        restore(
            &SnapshotOptions {
                data_dir: dest.path().to_path_buf(),
                force: true,
            },
            &Location::Local(archive_path),
        )
        .await
        .unwrap();
        // The pre-existing file is left in place; the snapshot is overlaid.
        assert!(dest.path().join("preexisting.txt").exists());
        assert!(dest.path().join("_auth/secret.key").exists());
    }

    #[tokio::test]
    async fn test_local_destination_directory_appends_filename() {
        let src = TempDir::new().unwrap();
        populate(src.path());
        let dest_dir = TempDir::new().unwrap();
        snapshot(
            &SnapshotOptions {
                data_dir: src.path().to_path_buf(),
                force: false,
            },
            &Location::Local(dest_dir.path().to_path_buf()),
        )
        .await
        .unwrap();
        let entries: Vec<_> = fs::read_dir(dest_dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries.len(), 1, "got {entries:?}");
        assert!(entries[0].starts_with("grumpydb-snapshot-"));
        assert!(entries[0].ends_with(".tar.gz"));
    }

    #[tokio::test]
    async fn test_checksum_mismatch_aborts_restore() {
        let src = TempDir::new().unwrap();
        populate(src.path());
        let archive_dir = TempDir::new().unwrap();
        let archive_path = archive_dir.path().join("snap.tar.gz");
        snapshot(
            &SnapshotOptions {
                data_dir: src.path().to_path_buf(),
                force: false,
            },
            &Location::Local(archive_path.clone()),
        )
        .await
        .unwrap();

        // Corrupt the archive: decode → mutate one entry's bytes → re-encode.
        let corrupted = corrupt_first_data_entry(&archive_path);
        fs::write(&archive_path, corrupted).unwrap();

        let dest = TempDir::new().unwrap();
        let err = restore(
            &SnapshotOptions {
                data_dir: dest.path().to_path_buf(),
                force: false,
            },
            &Location::Local(archive_path),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SnapshotError::Checksum { .. }), "got {err:?}");
    }

    /// Re-encode an existing archive after flipping every byte of the first
    /// `data/...` entry. The manifest is preserved, so SHA-256 will mismatch.
    fn corrupt_first_data_entry(path: &Path) -> Vec<u8> {
        let raw = fs::read(path).unwrap();
        let gz = GzDecoder::new(raw.as_slice());
        let mut archive = Archive::new(gz);

        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        let mut corrupted_one = false;
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).unwrap();
            if !corrupted_one && path.starts_with(DATA_PREFIX) {
                for b in buf.iter_mut() {
                    *b ^= 0xff;
                }
                corrupted_one = true;
            }
            entries.push((path, buf));
        }
        assert!(corrupted_one, "no data/ entry found");

        let buf = Vec::new();
        let gz = GzEncoder::new(buf, Compression::default());
        let mut tar = Builder::new(gz);
        for (path, buf) in entries {
            let mut header = Header::new_gnu();
            header.set_size(buf.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, &path, buf.as_slice()).unwrap();
        }
        let gz = tar.into_inner().unwrap();
        gz.finish().unwrap()
    }
}
