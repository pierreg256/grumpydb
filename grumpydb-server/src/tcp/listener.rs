//! TCP listener with optional TLS, connection accept loop, and graceful shutdown.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{Instrument, info_span};

use grumpydb::SharedServer;

use crate::auth::store::AuthStore;
use crate::config::ServerConfig;
use crate::tcp::handler::handle_connection;

/// Start the TCP server and listen for connections.
pub async fn listen(
    config: &ServerConfig,
    auth_store: Arc<parking_lot::RwLock<AuthStore>>,
    shared_server: SharedServer,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(&config.server.bind).await?;
    let connection_count = Arc::new(AtomicUsize::new(0));
    let max_connections = config.server.max_connections;

    let tls_acceptor = if config.tls.enabled {
        Some(build_tls_acceptor(config)?)
    } else {
        None
    };

    tracing::info!(
        bind = %config.server.bind,
        tls = config.tls.enabled,
        max_connections,
        "GrumpyDB server listening on {} (TLS: {})",
        config.server.bind,
        config.tls.enabled
    );

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (tcp_stream, addr) = accept_result?;

                if connection_count.load(Ordering::Relaxed) >= max_connections {
                    tracing::warn!(peer = %addr, "connection limit reached, rejecting");
                    let _ = tcp_stream.try_write(b"-ERR server busy\r\n");
                    continue;
                }

                let count = connection_count.clone();
                let auth = auth_store.clone();
                let server = shared_server.clone();
                let acceptor = tls_acceptor.clone();

                let span = info_span!("connection", peer = %addr, tls = acceptor.is_some());

                tokio::spawn(async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(active = count.load(Ordering::Relaxed), "connection accepted");

                    let result = if let Some(acceptor) = acceptor {
                        match acceptor.accept(tcp_stream).await {
                            Ok(tls_stream) => {
                                handle_connection(tls_stream, auth, server).await
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "TLS handshake failed");
                                Ok(())
                            }
                        }
                    } else {
                        handle_connection(tcp_stream, auth, server).await
                    };

                    if let Err(e) = result {
                        tracing::debug!(error = %e, "connection error");
                    }
                    count.fetch_sub(1, Ordering::Relaxed);
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
