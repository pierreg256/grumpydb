//! Spawn the real `grumpydb-server` binary in the background for tests.

use std::net::{SocketAddr, TcpListener as StdListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use std::{env, io::Read};

use rand::Rng;
use rand::distributions::Alphanumeric;
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep};

/// A running `grumpydb-server` process scoped to a single test.
///
/// Dropping the value kills the underlying process and removes the temporary
/// data directory.
pub struct TestServer {
    /// Address the server is listening on (`127.0.0.1:<port>`).
    pub addr: SocketAddr,
    /// Address of the observability HTTP server
    /// (`/healthz`, `/readyz`, `/metrics`).
    pub http_addr: SocketAddr,
    /// Temporary data directory used by the server.
    pub data_dir: PathBuf,
    /// Bootstrap admin tenant (always `_system`).
    pub admin_tenant: &'static str,
    /// Bootstrap admin username (always `admin`).
    pub admin_user: &'static str,
    /// Random admin password generated for this server.
    pub admin_password: String,
    process: Child,
    _tmp: TempDir,
}

const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 60;
const STARTUP_RETRY_ATTEMPTS: usize = 3;

impl TestServer {
    /// Spawn a fresh server on a random port and wait until it accepts connections.
    ///
    /// Panics on any failure — these helpers are only used in tests.
    pub async fn spawn() -> Self {
        Self::spawn_with_extra_args(&[]).await
    }

    /// Spawn a fresh server with RS256 JWT (instead of the test default
    /// HS256). Use this only in tests that explicitly exercise JWKS or
    /// asymmetric token verification — it incurs a ~5–10 s RSA-2048
    /// keygen cost per spawn in debug builds.
    pub async fn spawn_rs256() -> Self {
        let cfg_dir = TempDir::new().expect("rs256 cfg tmpdir");
        let cfg_path = cfg_dir.path().join("grumpydb.toml");
        std::fs::write(&cfg_path, "[auth]\njwt_algorithm = \"rs256\"\n")
            .expect("write rs256 test config");
        let cfg_str = cfg_path.to_str().expect("utf8 cfg path").to_string();
        // Leak the TempDir so the file outlives the await — TestServer
        // doesn't own this auxiliary directory.
        std::mem::forget(cfg_dir);
        // Use a Vec<String> trick: spawn_with_extra_args takes `&[&str]`,
        // and we need to keep the strings alive across the await.
        let owned_args: Vec<String> = vec!["--config".into(), cfg_str];
        let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
        Self::spawn_with_extra_args(&arg_refs).await
    }

    /// Spawn a fresh server with extra CLI arguments appended after the
    /// default flags. Useful for tests that need to point at a custom config
    /// file (e.g. short token TTLs).
    pub async fn spawn_with_extra_args(extra: &[&str]) -> Self {
        let tmp = TempDir::new().expect("create tempdir");
        let data_dir = tmp.path().to_path_buf();
        let admin_password = random_password(32);
        let bin = locate_server_binary();
        let startup_timeout = startup_timeout();

        // Default to HS256 for the test harness — RSA-2048 keygen in
        // debug builds is too slow for the volume of TestServer
        // instances the workspace tests spawn (each server bootstraps
        // a fresh data dir). RS256-specific tests pass `--config` via
        // `extra` to override.
        let default_config_path = tmp.path().join("grumpydb.toml");
        if !extra.contains(&"--config") {
            std::fs::write(&default_config_path, "[auth]\njwt_algorithm = \"hs256\"\n")
                .expect("write default test config");
        }

        let mut attempt_errors = Vec::new();
        for attempt in 1..=STARTUP_RETRY_ATTEMPTS {
            let port = pick_free_port();
            let http_port = pick_free_port();
            let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            let http_addr: SocketAddr = format!("127.0.0.1:{http_port}").parse().unwrap();

            let mut cmd = Command::new(&bin);
            cmd.arg("--data")
                .arg(&data_dir)
                .arg("--no-tls")
                .arg("--bind")
                .arg(addr.to_string())
                .arg("--http-bind")
                .arg(http_addr.to_string())
                .arg("--bootstrap-password")
                .arg(&admin_password)
                .arg("--log-format")
                .arg("text")
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            if !extra.contains(&"--config") {
                cmd.arg("--config").arg(&default_config_path);
            }
            for a in extra {
                cmd.arg(a);
            }

            let mut process = cmd.spawn().unwrap_or_else(|e| {
                panic!(
                    "failed to spawn grumpydb-server (binary={}): {e}",
                    bin.display()
                )
            });

            match wait_until_ready(&mut process, addr, startup_timeout).await {
                Ok(()) => {
                    return Self {
                        addr,
                        http_addr,
                        data_dir,
                        admin_tenant: "_system",
                        admin_user: "admin",
                        admin_password,
                        process,
                        _tmp: tmp,
                    };
                }
                Err(err) => {
                    let _ = process.kill();
                    let _ = process.wait();
                    attempt_errors.push(format!(
                        "attempt {attempt}/{STARTUP_RETRY_ATTEMPTS} failed (tcp={addr}, http={http_addr}): {err}"
                    ));
                }
            }
        }

        panic!(
            "failed to start grumpydb-server after {STARTUP_RETRY_ATTEMPTS} attempts:\n{}",
            attempt_errors.join("\n")
        );
    }

    /// Send `SIGKILL` to the server and reap the child.
    ///
    /// The data directory is preserved on disk so the next [`Self::restart`]
    /// can recover from it. Used by crash-recovery integration tests.
    pub async fn crash(&mut self) {
        // `Child::kill` on Unix calls `libc::kill(pid, SIGKILL)`, which is
        // what we want: no graceful shutdown, no checkpoint, no FD flush.
        let _ = self.process.kill();
        let _ = self.process.wait();
    }

    /// Spawn a fresh `grumpydb-server` on the SAME data directory and the
    /// SAME bind port as the previous incarnation, then wait until it
    /// accepts connections.
    ///
    /// Reuses the bootstrap admin credentials stored on disk — no new
    /// `--bootstrap-password` is needed because the auth store is persistent.
    ///
    /// Polls TCP up to 10 seconds, which gives the OS plenty of time to
    /// release the listening port between the SIGKILL and the rebind.
    pub async fn restart(&mut self) {
        let bin = locate_server_binary();

        let mut cmd = Command::new(&bin);
        cmd.arg("--data")
            .arg(&self.data_dir)
            .arg("--no-tls")
            .arg("--bind")
            .arg(self.addr.to_string())
            .arg("--http-bind")
            .arg(self.http_addr.to_string())
            .arg("--log-format")
            .arg("text")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let process = cmd.spawn().unwrap_or_else(|e| {
            panic!(
                "failed to respawn grumpydb-server (binary={}): {e}",
                bin.display()
            )
        });

        // Replace the dead handle so `Drop` reaps the live one.
        self.process = process;

        wait_until_ready(&mut self.process, self.addr, startup_timeout())
            .await
            .unwrap_or_else(|e| panic!("failed to restart grumpydb-server: {e}"));
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Best-effort: ask politely, then enforce.
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn random_password(len: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..len).map(|_| rng.sample(Alphanumeric) as char).collect()
}

/// Bind to port 0 to grab a free port, then drop the listener so the server
/// can take it. Tiny race window — acceptable for tests.
fn pick_free_port() -> u16 {
    let listener = StdListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

async fn wait_until_ready(
    process: &mut Child,
    addr: SocketAddr,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<std::io::Error> = None;
    while Instant::now() < deadline {
        if let Some(status) = process.try_wait().map_err(|e| {
            format!("failed to poll grumpydb-server process state while waiting for {addr}: {e}")
        })? {
            let stderr = read_stderr(process);
            return Err(format!(
                "grumpydb-server exited before readiness on {addr} with status {status}; stderr:\n{stderr}"
            ));
        }

        match TcpStream::connect(addr).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                sleep(Duration::from_millis(50)).await;
            }
        }
    }
    Err(format!(
        "grumpydb-server at {addr} did not become ready within {:?}: {:?}",
        timeout, last_err
    ))
}

fn startup_timeout() -> Duration {
    let secs = env::var("GRUMPYDB_TEST_SERVER_STARTUP_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

fn read_stderr(process: &mut Child) -> String {
    let Some(mut stderr) = process.stderr.take() else {
        return "<stderr unavailable>".to_string();
    };

    let mut s = String::new();
    if stderr.read_to_string(&mut s).is_err() {
        return "<failed to read stderr>".to_string();
    }

    if s.is_empty() {
        "<stderr empty>".to_string()
    } else {
        s
    }
}

/// Locate the `grumpydb-server` binary under `target/{debug,release}` by
/// walking up from this crate's manifest dir until a `target/` is found.
///
/// If the binary cannot be located, falls back to running
/// `cargo build --bin grumpydb-server` once. This handles the case where
/// `cargo test --workspace` schedules the root crate's integration tests
/// before the server binary has been linked.
fn locate_server_binary() -> PathBuf {
    if let Some(p) = find_binary() {
        return p;
    }

    // Fallback: build the binary on demand.
    let status = Command::new(env!("CARGO"))
        .args(["build", "--bin", "grumpydb-server"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("invoke cargo build");
    if !status.success() {
        panic!("`cargo build --bin grumpydb-server` failed (exit {status})");
    }

    find_binary().expect("grumpydb-server binary still missing after build")
}

fn find_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("GRUMPYDB_SERVER_BIN") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }

    let bin_name = if cfg!(windows) {
        "grumpydb-server.exe"
    } else {
        "grumpydb-server"
    };

    let mut current: &Path = Path::new(env!("CARGO_MANIFEST_DIR"));
    loop {
        let target = current.join("target");
        if target.is_dir() {
            for profile in ["debug", "release"] {
                let candidate = target.join(profile).join(bin_name);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
        current = current.parent()?;
    }
}
