//! TCP listener with optional TLS, connection accept loop, and graceful shutdown.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{Instrument, info_span};

use grumpydb::SharedServer;

use crate::auth::store::AuthStore;
use crate::cluster::NodeIdentity;
use crate::cluster::hints::HintStore;
use crate::cluster::read_repair::ReadRepairStore;
use crate::config::ServerConfig;
use crate::coordinator::Coordinator;
use crate::http::HttpState;
use crate::limits::{AcquireConnError, Limits, LimitsConfig};
use crate::tcp::handler::{RepairPipelines, handle_connection};

/// Start the TCP server and listen for connections.
pub async fn listen(
    config: &ServerConfig,
    auth_store: Arc<parking_lot::RwLock<AuthStore>>,
    shared_server: SharedServer,
    http_state: Arc<HttpState>,
    identity: Arc<NodeIdentity>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(&config.server.bind).await?;

    // Per-IP rate limits and connection caps.
    let mut limits_cfg: LimitsConfig = (&config.limits).into();
    // Honour the legacy `[server].max_connections` as an upper bound on the
    // global connection cap, so existing deployments aren't suddenly held
    // back by a higher new default.
    if config.server.max_connections > 0 {
        limits_cfg.max_conns_global = limits_cfg
            .max_conns_global
            .min(config.server.max_connections);
    }
    let limits = Arc::new(Limits::new(limits_cfg));

    let tls_acceptor = if config.tls.enabled {
        Some(build_tls_acceptor(config)?)
    } else {
        None
    };

    let local_node_id = identity.node_id.to_string();
    let coordinator_local_addr = config
        .cluster
        .peers
        .iter()
        .find(|p| p.node_id == local_node_id && !p.addr.is_empty())
        .map(|p| p.addr.as_str())
        .or({
            if config.cluster.listen_peer.is_empty() {
                None
            } else {
                Some(config.cluster.listen_peer.as_str())
            }
        })
        .unwrap_or(config.server.bind.as_str());

    let coordinator = Arc::new(Coordinator::from_config(
        identity.as_ref(),
        &config.cluster,
        coordinator_local_addr,
    ));
    let hint_store = Arc::new(HintStore::open(&config.server.data_dir)?);
    let read_repair_store = Arc::new(ReadRepairStore::open(&config.server.data_dir)?);

    tracing::info!(
        bind = %config.server.bind,
        tls = config.tls.enabled,
        node_id = %identity.node_id,
        cluster_id = %identity.cluster_id,
        max_conns_global = limits.config().max_conns_global,
        max_conns_per_ip = limits.config().max_conns_per_ip,
        commands_per_sec_per_ip = limits.config().commands_per_sec_per_ip,
        "GrumpyDB server listening on {} (TLS: {})",
        config.server.bind,
        config.tls.enabled
    );

    // Phase 40a: optionally spin up the inter-node handshake stub.
    // Phase 40e will graft the WAL streaming protocol onto the
    // accepted connections; v5 just performs the handshake and closes.
    if !config.cluster.listen_peer.is_empty() {
        crate::cluster::handshake::serve(
            config.clone(),
            identity.clone(),
            coordinator.clone(),
            shared_server.clone(),
        )
        .await?;
    }

    // v6 Phase 44 (tranche 1): background gossip probes that refresh
    // per-peer liveness and last-seen fields surfaced in TOPOLOGY.
    crate::cluster::gossip::spawn(
        config.cluster.clone(),
        identity.clone(),
        coordinator.clone(),
    );
    crate::cluster::hints::spawn_worker(hint_store.clone(), coordinator.clone());
    crate::cluster::read_repair::spawn_worker(read_repair_store.clone(), coordinator.clone());

    // Signal HTTP `/readyz` that we are now accepting connections.
    http_state.ready.store(true, Ordering::Release);

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (mut tcp_stream, addr) = accept_result?;

                if let Err(reason) = limits.try_acquire_conn_with_reason(addr.ip()) {
                    tracing::warn!(peer = %addr, ?reason, "rate limited (too many connections)");
                    let kind = match reason {
                        AcquireConnError::Global => "conn_global",
                        AcquireConnError::PerIp => "conn_per_ip",
                    };
                    metrics::counter!(
                        "grumpydb_rate_limit_hits_total",
                        "kind" => kind
                    )
                    .increment(1);
                    let _ = tcp_stream
                        .write_all(b"-ERR rate limited (too many connections from your IP)\r\n")
                        .await;
                    let _ = tcp_stream.shutdown().await;
                    continue;
                }

                metrics::gauge!("grumpydb_connections_active").increment(1.0);

                let auth = auth_store.clone();
                let server = shared_server.clone();
                let acceptor = tls_acceptor.clone();
                let limits_for_conn = limits.clone();
                let coordinator_for_conn = coordinator.clone();
                let hint_store_for_conn = hint_store.clone();
                let read_repair_store_for_conn = read_repair_store.clone();
                let peer_ip = addr.ip();

                let span = info_span!(
                    "connection",
                    peer = %addr,
                    tls = acceptor.is_some(),
                    node_id = %identity.node_id,
                );

                tokio::spawn(async move {
                    tracing::debug!("connection accepted");

                    let result = if let Some(acceptor) = acceptor {
                        match acceptor.accept(tcp_stream).await {
                            Ok(tls_stream) => {
                                handle_connection(
                                    tls_stream,
                                    addr,
                                    auth,
                                    server,
                                    limits_for_conn.clone(),
                                    coordinator_for_conn.clone(),
                                    RepairPipelines {
                                        hint_store: hint_store_for_conn.clone(),
                                        read_repair_store: read_repair_store_for_conn.clone(),
                                    },
                                )
                                .await
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "TLS handshake failed");
                                Ok(())
                            }
                        }
                    } else {
                        handle_connection(
                            tcp_stream,
                            addr,
                            auth,
                            server,
                            limits_for_conn.clone(),
                            coordinator_for_conn.clone(),
                            RepairPipelines {
                                hint_store: hint_store_for_conn.clone(),
                                read_repair_store: read_repair_store_for_conn.clone(),
                            },
                        )
                        .await
                    };

                    if let Err(e) = result {
                        tracing::debug!(error = %e, "connection error");
                    }
                    limits_for_conn.release_conn(peer_ip);
                    metrics::gauge!("grumpydb_connections_active").decrement(1.0);
                    tracing::debug!("connection closed");
                }.instrument(span));
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received shutdown signal, stopping...");
                break;
            }
        }
    }

    // Graceful shutdown: close the shared server
    tracing::info!("flushing data and shutting down...");
    Ok(())
}

fn build_tls_acceptor(config: &ServerConfig) -> Result<TlsAcceptor, Box<dyn std::error::Error>> {
    let cert_path = config
        .tls
        .cert_file
        .as_deref()
        .unwrap_or("_auth/server.crt");
    let key_path = config.tls.key_file.as_deref().unwrap_or("_auth/server.key");

    // Auto-generate self-signed cert if files don't exist
    if !Path::new(cert_path).exists() || !Path::new(key_path).exists() {
        generate_self_signed_cert(cert_path, key_path)?;
    }

    let cert_file = std::fs::File::open(cert_path)?;
    let key_file = std::fs::File::open(key_path)?;

    let certs: Vec<_> = rustls_pemfile::certs(&mut std::io::BufReader::new(cert_file))
        .collect::<Result<Vec<_>, _>>()?;

    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(key_file))?
        .ok_or("no private key found in key file")?;

    let tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(TlsAcceptor::from(Arc::new(tls_config)))
}

fn generate_self_signed_cert(
    cert_path: &str,
    key_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use rcgen::{CertificateParams, KeyPair};

    tracing::warn!(
        "Generating self-signed certificate for development. \
         Use a CA-signed certificate in production."
    );

    // Ensure parent directories exist
    if let Some(parent) = Path::new(cert_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let key_pair = KeyPair::generate()?;
    let params = CertificateParams::new(vec!["localhost".to_string()])?;
    let cert = params.self_signed(&key_pair)?;

    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;

    Ok(())
}
