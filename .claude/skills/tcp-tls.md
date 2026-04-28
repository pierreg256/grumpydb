# Skill: TCP Server & TLS

## When to use this skill

When working on:
- `grumpydb-server/src/tcp/listener.rs` — TCP accept loop, TLS setup
- `grumpydb-server/src/tcp/handler.rs` — per-connection handler
- `grumpydb-server/src/config.rs` — server configuration
- `grumpydb-server/src/main.rs` — binary entry point

## Core principles

### Architecture

```
                 ┌─────────────────────────┐
                 │  tokio::net::TcpListener │
                 │  bind("0.0.0.0:6380")   │
                 └──────────┬──────────────┘
                            │ accept()
                            ▼
              ┌──────────────────────────────┐
              │  tokio_rustls::TlsAcceptor   │  ← optional (if tls.enabled)
              │  TLS 1.2 / 1.3 handshake     │
              └──────────────┬───────────────┘
                             │
                ┌────────────▼────────────┐
                │  tokio::spawn(           │
                │    handle_connection()   │  ← one task per connection
                │  )                       │
                └────────────┬────────────┘
                             │
         ┌───────────────────▼───────────────────┐
         │           Connection Handler           │
         │                                        │
         │  loop {                                │
         │    line = read_line(stream)             │
         │    cmd  = parse_command(line)           │
         │    session.authorize(cmd)?              │
         │    resp = execute(cmd, shared_server)   │
         │    write(stream, resp.serialize())      │
         │  }                                     │
         └────────────────────────────────────────┘
```

### TLS setup with rustls

```rust
use rustls::ServerConfig;
use rustls_pemfile::{certs, private_key};
use tokio_rustls::TlsAcceptor;

fn build_tls_config(cert_path: &Path, key_path: &Path) -> Result<ServerConfig> {
    let cert_file = std::fs::File::open(cert_path)?;
    let key_file = std::fs::File::open(key_path)?;

    let certs: Vec<_> = certs(&mut BufReader::new(cert_file))
        .collect::<Result<Vec<_>, _>>()?;

    let key = private_key(&mut BufReader::new(key_file))?
        .ok_or_else(|| GrumpyError::Io(
            io::Error::new(io::ErrorKind::InvalidData, "no private key found")
        ))?;

    let config = ServerConfig::builder()
        .with_no_client_auth()          // or .with_client_cert_verifier() for mTLS
        .with_single_cert(certs, key)
        .map_err(|e| GrumpyError::Io(
            io::Error::new(io::ErrorKind::InvalidData, e.to_string())
        ))?;

    Ok(config)
}
```

### Self-signed certificate generation (dev mode)

```rust
use rcgen::{CertificateParams, KeyPair};

fn generate_self_signed(cert_path: &Path, key_path: &Path) -> Result<()> {
    let key_pair = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec!["localhost".to_string()])?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "GrumpyDB Dev");

    let cert = params.self_signed(&key_pair)?;

    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;

    // Log warning
    tracing::warn!("Generated self-signed certificate for development. \
                    Use a CA-signed certificate in production.");
    Ok(())
}
```

### Accept loop

```rust
async fn listen(
    config: &ServerConfig,
    auth_store: Arc<AuthStore>,
    shared_server: SharedServer,
) -> Result<()> {
    let listener = TcpListener::bind(&config.bind).await?;
    let tls_acceptor = config.tls.as_ref().map(|tls| {
        let rustls_config = build_tls_config(&tls.cert_file, &tls.key_file).unwrap();
        TlsAcceptor::from(Arc::new(rustls_config))
    });

    let connection_count = Arc::new(AtomicUsize::new(0));

    tracing::info!("Listening on {} (TLS: {})", config.bind,
                   tls_acceptor.is_some());

    loop {
        let (tcp_stream, addr) = listener.accept().await?;

        // Connection limit
        if connection_count.load(Ordering::Relaxed) >= config.max_connections {
            // Write error and close
            let _ = tcp_stream.try_write(b"-ERR server busy\r\n");
            continue;
        }

        let count = connection_count.clone();
        let auth = auth_store.clone();
        let server = shared_server.clone();
        let acceptor = tls_acceptor.clone();

        tokio::spawn(async move {
            count.fetch_add(1, Ordering::Relaxed);
            tracing::debug!("Connection from {addr}");

            let result = if let Some(acceptor) = acceptor {
                // TLS mode
                match acceptor.accept(tcp_stream).await {
                    Ok(tls_stream) => handle_connection(tls_stream, auth, server).await,
                    Err(e) => {
                        tracing::warn!("TLS handshake failed from {addr}: {e}");
                        return;
                    }
                }
            } else {
                // Plaintext mode
                handle_connection(tcp_stream, auth, server).await
            };

            if let Err(e) = result {
                tracing::debug!("Connection {addr} closed: {e}");
            }
            count.fetch_sub(1, Ordering::Relaxed);
        });
    }
}
```

### Connection handler

```rust
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    auth_store: Arc<AuthStore>,
    shared_server: SharedServer,
) -> Result<()> {
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    let mut session = SessionContext::new();

    // Send banner
    writer.write_all(format!("+GRUMPYDB {}\r\n", PROTOCOL_VERSION).as_bytes()).await?;
    writer.flush().await?;

    let mut line = String::new();
    let mut consecutive_errors = 0;

    loop {
        line.clear();

        // Read one line (enforce MAX_LINE_LENGTH)
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break; // EOF — client disconnected
        }

        if line.len() > MAX_LINE_LENGTH {
            write_response(&mut writer, &Response::Error("line too long".into())).await?;
            consecutive_errors += 1;
            if consecutive_errors > 10 { break; }
            continue;
        }

        // Parse
        let command = match parse_command(&line) {
            Ok(cmd) => cmd,
            Err(e) => {
                write_response(&mut writer, &Response::Error(e.to_string())).await?;
                consecutive_errors += 1;
                if consecutive_errors > 10 { break; }
                continue;
            }
        };

        consecutive_errors = 0;

        // QUIT
        if matches!(command, Command::Quit) {
            write_response(&mut writer, &Response::Ok("BYE".into())).await?;
            break;
        }

        // Authorize
        if let Err(e) = session.authorize(&command) {
            write_response(&mut writer, &Response::Error(e.to_string())).await?;
            continue;
        }

        // Execute
        let response = execute_command(&command, &mut session, &auth_store, &shared_server).await;
        write_response(&mut writer, &response).await?;
    }

    Ok(())
}
```

### Server configuration (`grumpydb.toml`)

```toml
[server]
bind = "0.0.0.0:6380"
max_connections = 1024
data_dir = "./data"

[tls]
enabled = true
cert_file = "_auth/server.crt"
key_file = "_auth/server.key"
# client_ca = "/path/to/ca.crt"     # uncomment for mTLS

[auth]
access_token_ttl = "1h"
refresh_token_ttl = "7d"
```

```rust
#[derive(Deserialize)]
pub struct ServerConfig {
    pub server: ServerSection,
    pub tls: Option<TlsSection>,
    pub auth: Option<AuthSection>,
}

#[derive(Deserialize)]
pub struct ServerSection {
    pub bind: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
}
```

### Graceful shutdown

```rust
use tokio::signal;

async fn shutdown_signal() {
    let ctrl_c = signal::ctrl_c();
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate()).unwrap();

    tokio::select! {
        _ = ctrl_c => tracing::info!("Received SIGINT, shutting down..."),
        _ = sigterm.recv() => tracing::info!("Received SIGTERM, shutting down..."),
    }
}
```

Use `tokio::select!` in the accept loop to break on shutdown signal, then flush and close `SharedServer`.

### Handler abstraction

The handler works with `AsyncRead + AsyncWrite` — it doesn't care if the stream is:
- `TcpStream` (plaintext)
- `TlsStream<TcpStream>` (TLS)

This makes the handler easy to test with mock streams.

## Security rules

1. **Connection limit**: reject over `max_connections` with `-ERR server busy\r\n`
2. **Line length limit**: reject lines over `MAX_LINE_LENGTH`
3. **Error limit**: close connections with > 10 consecutive errors
4. **No password logging**: never log LOGIN command arguments at INFO level
5. **TLS required in production**: warn loudly if `tls.enabled = false`
6. **Graceful shutdown**: flush all data before exiting

## Common mistakes to avoid

1. **Forgetting `writer.flush()`** — `BufWriter` buffers, must flush after each response
2. **Blocking the tokio runtime** — engine operations are sync; use `tokio::task::spawn_blocking()` for heavy ops
3. **Not handling EOF** — `read_line` returns 0 bytes on disconnect
4. **TLS cert reloading** — v1 loads cert at startup only; document that server restart is needed for cert rotation
5. **Resource leaks** — always decrement connection counter, even on panic (use `Drop` guard or `defer` pattern)
