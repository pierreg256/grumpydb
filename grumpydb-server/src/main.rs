//! GrumpyDB Server — binary entry point.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use grumpydb::SharedServer;
use grumpydb_server::auth::store::AuthStore;
use grumpydb_server::config::ServerConfig;
use grumpydb_server::http::{self, HttpState};
use grumpydb_server::snapshot::{self, Location, SnapshotOptions};
use grumpydb_server::tcp::listener;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    // Subcommand dispatch (Phase 38). The first positional arg may be a
    // subcommand verb; anything else (or no arg) falls through to
    // server mode.
    match args.get(1).map(String::as_str) {
        Some("snapshot") => return run_snapshot(&args[2..]).await,
        Some("restore") => return run_restore(&args[2..]).await,
        Some("--help") | Some("-h") => {
            print_usage();
            return Ok(());
        }
        _ => {}
    }

    // Phase 27: structured logging with `tracing`.
    // Format selection: --log-format json|text  (default: json in production-like
    // contexts, text when stdout is a TTY for nicer dev output).
    let log_format = get_arg(&args, "--log-format").unwrap_or_else(|| {
        if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            "text".into()
        } else {
            "json".into()
        }
    });

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("grumpydb=info,grumpydb_server=info,tokio=warn"));

    let registry = tracing_subscriber::registry().with(env_filter);
    match log_format.as_str() {
        "json" => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(false),
            )
            .init(),
        _ => registry.with(tracing_subscriber::fmt::layer()).init(),
    }

    let config_path = get_arg(&args, "--config").unwrap_or_else(|| "grumpydb.toml".into());
    let mut config = ServerConfig::load(&PathBuf::from(&config_path));

    // CLI overrides
    if let Some(bind) = get_arg(&args, "--bind") {
        config.server.bind = bind;
    }
    if let Some(data) = get_arg(&args, "--data") {
        config.server.data_dir = PathBuf::from(data);
    }
    if args.contains(&"--no-tls".to_string()) {
        config.tls.enabled = false;
    }
    if let Some(cert) = get_arg(&args, "--tls-cert") {
        config.tls.enabled = true;
        config.tls.cert_file = Some(cert);
    }
    if let Some(key) = get_arg(&args, "--tls-key") {
        config.tls.key_file = Some(key);
    }
    if let Some(http_bind) = get_arg(&args, "--http-bind") {
        config.http.bind = http_bind;
    }
    if args.contains(&"--no-http".to_string()) {
        config.http.bind.clear();
    }

    tracing::info!(
        data_dir = %config.server.data_dir.display(),
        bind = %config.server.bind,
        tls = config.tls.enabled,
        log_format = %log_format,
        "starting GrumpyDB server"
    );

    // Bootstrap password resolution (Phase 26): require an explicit password
    // for the first-ever start. After the initial admin exists on disk, this
    // value is ignored.
    //
    //   --bootstrap-password <pw>      CLI flag
    //   GRUMPYDB_BOOTSTRAP_PASSWORD    environment variable
    //
    // Refusing to bootstrap silently with `admin/admin` prevents the most
    // common production foot-gun.
    let bootstrap_password = get_arg(&args, "--bootstrap-password")
        .or_else(|| std::env::var("GRUMPYDB_BOOTSTRAP_PASSWORD").ok());

    // Initialize auth store
    let auth_dir = config.server.data_dir.join("_auth");
    let auth_store = AuthStore::open(
        &auth_dir,
        config.auth.access_token_ttl_secs,
        config.auth.refresh_token_ttl_secs,
        bootstrap_password.as_deref(),
    )?;
    let auth_store = Arc::new(parking_lot::RwLock::new(auth_store));

    // Initialize shared server
    let shared_server = SharedServer::open(&config.server.data_dir)?;

    // Initialise the global metrics recorder and observability HTTP server
    // (Phase 36). The HTTP server is unauthenticated by design — it serves
    // only `/healthz`, `/readyz`, Prometheus `/metrics` aggregates, and the
    // JWKS public-keyset (`/.well-known/jwks.json`, Phase 39).
    let prom = http::init_metrics();
    let http_state = Arc::new(HttpState {
        ready: AtomicBool::new(false),
        prometheus: prom,
        auth_store: Some(auth_store.clone()),
    });
    if !config.http.bind.is_empty() {
        let _http_handle = http::serve(http_state.clone(), &config.http.bind).await?;
    } else {
        tracing::info!("HTTP observability server disabled (http.bind is empty)");
    }

    // Start listening
    listener::listen(&config, auth_store, shared_server, http_state).await?;

    Ok(())
}

fn get_arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

// ── Subcommands (Phase 38: snapshot / restore) ─────────────────────────

fn print_usage() {
    eprintln!(
        r#"grumpydb-server — networked GrumpyDB

USAGE:
    grumpydb-server [OPTIONS]                       # run the server
    grumpydb-server snapshot [OPTIONS] <DEST>       # write a snapshot
    grumpydb-server restore  [OPTIONS] <SRC>        # restore a snapshot

SERVER OPTIONS:
    --config <toml>             Path to grumpydb.toml (default: ./grumpydb.toml)
    --bind   <addr:port>        Override server.bind
    --data   <dir>              Override server.data_dir
    --no-tls                    Disable TLS
    --tls-cert <pem>            Enable TLS with this cert (and --tls-key)
    --tls-key  <pem>            Private key for TLS
    --http-bind <addr:port>     Override http.bind
    --no-http                   Disable the observability HTTP server
    --bootstrap-password <pw>   Initial admin password (first start only)
    --log-format json|text      Log output format (default: auto)

SNAPSHOT / RESTORE OPTIONS:
    --data <dir>                Source (snapshot) or target (restore) data dir
    --config <toml>             Optional config (only for symmetry)
    --force                     [restore] Allow overwriting non-empty data dir

DESTINATIONS / SOURCES:
    /local/path/snap.tar.gz     Local file
    /local/dir/                 Local directory (filename auto-generated)
    s3://bucket/key             AWS S3 (requires --features cloud-aws)
    az://container/blob         Azure Blob Storage (requires --features cloud-azure)

NOTE:
    snapshot/restore operate on the on-disk files directly. The server
    must NOT be running against the same data directory at the same time.
"#
    );
}

fn init_cli_tracing() {
    // Subcommands are short-lived CLIs, not daemons — force human-readable
    // output even on non-TTY stdout. Tolerate double-init across
    // process re-entry in tests.
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("grumpydb=info,grumpydb_server=info"));
    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .try_init();
}

async fn run_snapshot(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    init_cli_tracing();

    let dest_str = first_positional(args).ok_or("snapshot: missing destination URL")?;
    let data_dir = get_arg(args, "--data").ok_or("snapshot: --data <dir> is required")?;

    let dest = Location::parse(&dest_str)
        .map_err(|e| format!("snapshot: invalid destination '{dest_str}': {e}"))?;
    let opts = SnapshotOptions {
        data_dir: PathBuf::from(&data_dir),
        force: false,
    };

    tracing::info!(
        data_dir = %opts.data_dir.display(),
        dest = %dest_str,
        "creating snapshot"
    );
    snapshot::snapshot(&opts, &dest).await?;
    tracing::info!("snapshot complete");
    Ok(())
}

async fn run_restore(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    init_cli_tracing();

    let src_str = first_positional(args).ok_or("restore: missing source URL")?;
    let data_dir = get_arg(args, "--data").ok_or("restore: --data <dir> is required")?;
    let force = args.iter().any(|a| a == "--force");

    let src = Location::parse(&src_str)
        .map_err(|e| format!("restore: invalid source '{src_str}': {e}"))?;
    let opts = SnapshotOptions {
        data_dir: PathBuf::from(&data_dir),
        force,
    };

    tracing::info!(
        data_dir = %opts.data_dir.display(),
        src = %src_str,
        force = opts.force,
        "restoring snapshot"
    );
    snapshot::restore(&opts, &src).await?;
    tracing::info!("restore complete");
    Ok(())
}

/// Return the first positional argument (anything not starting with `--`,
/// skipping flag values).
fn first_positional(args: &[String]) -> Option<String> {
    let known_flags_with_value = [
        "--data",
        "--config",
        "--bind",
        "--http-bind",
        "--log-format",
    ];
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if known_flags_with_value.iter().any(|f| f == a) {
            i += 2; // skip the flag and its value
            continue;
        }
        if a.starts_with("--") {
            i += 1;
            continue;
        }
        return Some(a.clone());
    }
    None
}
