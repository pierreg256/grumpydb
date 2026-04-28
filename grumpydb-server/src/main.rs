//! GrumpyDB Server — binary entry point.

use std::path::PathBuf;
use std::sync::Arc;

use grumpydb::SharedServer;
use grumpydb_server::auth::store::AuthStore;
use grumpydb_server::config::ServerConfig;
use grumpydb_server::tcp::listener;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

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

    // Start listening
    listener::listen(&config, auth_store, shared_server).await?;

    Ok(())
}

fn get_arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}
