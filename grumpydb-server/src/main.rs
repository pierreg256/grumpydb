//! GrumpyDB Server — binary entry point.

use std::path::PathBuf;
use std::sync::Arc;

use grumpydb::SharedServer;
use grumpydb_server::auth::store::AuthStore;
use grumpydb_server::config::ServerConfig;
use grumpydb_server::tcp::listener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // Parse CLI args (simple manual parsing)
    let args: Vec<String> = std::env::args().collect();
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

    tracing::info!("Data directory: {}", config.server.data_dir.display());

    // Initialize auth store
    let auth_dir = config.server.data_dir.join("_auth");
    let auth_store = AuthStore::open(
        &auth_dir,
        config.auth.access_token_ttl_secs,
        config.auth.refresh_token_ttl_secs,
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
