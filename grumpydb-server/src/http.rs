//! Observability HTTP server: `/healthz`, `/readyz`, `/metrics`.
//!
//! A small Hyper-based HTTP/1.1 server that runs alongside the TCP listener
//! and exposes liveness/readiness probes plus a Prometheus exposition
//! endpoint. Designed for Kubernetes probes and Prometheus scrape jobs.
//!
//! # Endpoints
//!
//! - `GET /healthz` — `200 OK` once the HTTP server is up. Used as a
//!   liveness probe: the process is alive and responding.
//! - `GET /readyz` — `200 OK` only when [`HttpState::ready`] has been
//!   flipped to `true` by the TCP listener (i.e. the database is open and
//!   accepting client connections). Returns `503 Service Unavailable`
//!   otherwise.
//! - `GET /metrics` — Prometheus exposition format produced by
//!   [`metrics_exporter_prometheus`].
//! - Anything else returns `404`.
//!
//! # Security
//!
//! These endpoints are deliberately **unauthenticated** in v5: they exist
//! so Prometheus and orchestrator probes can scrape them without managing
//! credentials. They expose only aggregate counters and gauges, never
//! tenant data. Bind the HTTP server to a private interface (or to the
//! loopback) when running in a hostile environment.
//!
//! TODO(v6): consider opt-in basic-auth or IP allowlisting for `/metrics`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::auth::store::AuthStore;

/// State observed by the HTTP server.
///
/// Shared with the TCP listener (so `/readyz` can flip on once the TCP
/// socket is bound) and owned by the spawned HTTP task (which renders the
/// Prometheus snapshot on every `/metrics` request and the JWKS document
/// on every `/.well-known/jwks.json` request).
pub struct HttpState {
    /// Set to `true` by the TCP listener once it has successfully bound.
    /// Read with `Acquire` ordering by `/readyz`.
    pub ready: AtomicBool,
    /// Prometheus exporter handle. Cheap to clone; rendering snapshots is
    /// O(metrics).
    pub prometheus: PrometheusHandle,
    /// Auth store reference for the JWKS endpoint. `None` only in the
    /// observability-unit-tests where no auth store is set up.
    pub auth_store: Option<Arc<parking_lot::RwLock<AuthStore>>>,
}

/// Initialise the global metrics recorder and register the GrumpyDB
/// metrics catalogue. Must be called exactly once per process — calling it
/// more than once will panic at the `install_recorder` step.
///
/// Returns a [`PrometheusHandle`] that can be cloned freely; every clone
/// renders the same global snapshot.
pub fn init_metrics() -> PrometheusHandle {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("install Prometheus recorder");

    // Describe everything up-front so `/metrics` lists every series with
    // its HELP/TYPE even before any sample has been recorded.
    use metrics::{Unit, describe_counter, describe_gauge, describe_histogram};

    describe_gauge!(
        "grumpydb_connections_active",
        Unit::Count,
        "Currently open TCP connections."
    );
    describe_counter!(
        "grumpydb_commands_total",
        Unit::Count,
        "Total commands processed (labelled by `cmd` and `result`)."
    );
    describe_histogram!(
        "grumpydb_command_duration_seconds",
        Unit::Seconds,
        "Per-command execution duration in seconds (labelled by `cmd`)."
    );

    // Buffer pool / WAL gauges. These are described here so they show up in
    // `/metrics` output (as zero) but are not yet wired to the engine.
    // TODO(phase-41): instrument the buffer pool and WAL writer to publish
    // these gauges when MVCC reads land.
    describe_gauge!(
        "grumpydb_buffer_pool_pages",
        Unit::Count,
        "Pages currently held in the buffer pool (labelled by `state` ∈ \
         {clean, dirty, pinned})."
    );
    describe_counter!(
        "grumpydb_wal_records_total",
        Unit::Count,
        "Total WAL records written since process start."
    );

    describe_counter!(
        "grumpydb_login_failures_total",
        Unit::Count,
        "Failed LOGIN attempts (labelled by `reason` ∈ \
         {invalid_credentials, rate_limited})."
    );
    describe_counter!(
        "grumpydb_rate_limit_hits_total",
        Unit::Count,
        "Rate-limit rejections (labelled by `kind` ∈ \
         {command, login, conn_per_ip, conn_global})."
    );

    // Phase 40a: static info gauge (always 1.0) labelled with the
    // node identity. Joining other series on `node_id` makes
    // multi-node Prometheus dashboards trivial to write.
    describe_gauge!(
        "grumpydb_node_info",
        Unit::Count,
        "Static node information (always 1.0) labelled with \
         `node_id`, `cluster_id`, `version`."
    );

    handle
}

/// Spawn the HTTP server in the background and return immediately.
///
/// The server runs until the spawned task is aborted (or the process exits).
/// The returned [`JoinHandle`] can be ignored if the server should live for
/// the entire process lifetime.
///
/// # Errors
///
/// Returns an error if the bind address cannot be parsed or the TCP socket
/// cannot be opened.
pub async fn serve(
    state: Arc<HttpState>,
    bind: &str,
) -> Result<JoinHandle<()>, Box<dyn std::error::Error>> {
    let addr: SocketAddr = bind.parse()?;
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    tracing::info!(bind = %local, "HTTP observability server listening");

    let handle = tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "HTTP accept failed");
                    continue;
                }
            };
            let io = TokioIo::new(stream);
            let state = state.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req| {
                    let state = state.clone();
                    async move { handle_request(req, state).await }
                });
                if let Err(e) = ConnBuilder::new(TokioExecutor::new())
                    .serve_connection(io, svc)
                    .await
                {
                    tracing::debug!(peer = %peer, error = %e, "HTTP connection error");
                }
            });
        }
    });

    Ok(handle)
}

/// Top-level request router. Never panics: unexpected internal errors are
/// converted to `500 Internal Server Error`.
async fn handle_request(
    req: Request<Incoming>,
    state: Arc<HttpState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let resp = match (req.method(), req.uri().path()) {
        (&Method::GET, "/healthz") => text_response(StatusCode::OK, "ok"),
        (&Method::GET, "/readyz") => {
            if state.ready.load(Ordering::Acquire) {
                text_response(StatusCode::OK, "ready")
            } else {
                text_response(StatusCode::SERVICE_UNAVAILABLE, "not ready")
            }
        }
        (&Method::GET, "/metrics") => {
            let body = state.prometheus.render();
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/plain; version=0.0.4")
                .body(Full::new(Bytes::from(body)))
                .unwrap_or_else(|_| {
                    text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
                })
        }
        // Phase 39: JWKS public-keyset endpoint. Unauthenticated by design
        // (it is the *public* keyset). Returns `{"keys": []}` for HS256
        // deployments — symmetric secrets must never be exposed.
        (&Method::GET, "/.well-known/jwks.json") => {
            let keys = match &state.auth_store {
                Some(store) => store.read().jwks(),
                None => Vec::new(),
            };
            let body = serde_json::json!({ "keys": keys }).to_string();
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(body)))
                .unwrap_or_else(|_| {
                    text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
                })
        }
        _ => text_response(StatusCode::NOT_FOUND, "not found"),
    };
    Ok(resp)
}

fn text_response(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from_static(body.as_bytes())))
        .expect("static response always builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// Rendering-only Prometheus handle for tests so that no two tests
    /// race on `install_recorder` (which may only be called once per
    /// process). This lets each test build its own `HttpState`.
    fn isolated_handle() -> PrometheusHandle {
        // Build a recorder but DON'T install it as the global one — just
        // grab its handle. The handle still renders an empty snapshot
        // (containing whatever was described/recorded against it) which is
        // fine for endpoint-shape tests.
        PrometheusBuilder::new()
            .install_recorder()
            // If install fails because the global recorder is already
            // installed (e.g. in another test), fall back to building a
            // standalone (non-global) handle.
            .unwrap_or_else(|_| PrometheusBuilder::new().build_recorder().handle())
    }

    /// Spawn the HTTP server on an ephemeral port and return its address.
    async fn spawn_for_test(ready: bool) -> (SocketAddr, JoinHandle<()>) {
        let prom = isolated_handle();
        // Always describe AND emit at least one sample so `/metrics` is
        // non-empty for shape assertions. `describe_gauge!` writes to the
        // currently-installed global recorder; if `install_recorder`
        // succeeded above, that's our `prom`. If it didn't (because a prior
        // test already installed one), we still get a sample on whatever
        // global is in place — and since `prom` falls back to a *local*
        // recorder in that path, render will be empty until a sample lands
        // there. We work around that by describing AND emitting through
        // both paths defensively.
        metrics::describe_gauge!(
            "grumpydb_connections_active",
            "Currently open TCP connections."
        );
        metrics::gauge!("grumpydb_connections_active").set(0.0);
        let state = Arc::new(HttpState {
            ready: AtomicBool::new(ready),
            prometheus: prom,
            auth_store: None,
        });

        // Bind to :0 to grab a free port, but the public `serve` API takes a
        // string. Resolve the port first via a throwaway std listener, then
        // hand the address off.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let bind = format!("127.0.0.1:{port}");

        let handle = serve(state.clone(), &bind).await.expect("serve");
        // Allow the listener to bind.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Expose state via a side channel: tests that need to flip readiness
        // do so via a separate spawn (not needed in current tests).
        (bind.parse().unwrap(), handle)
    }

    /// Minimal HTTP/1.1 GET helper: returns (status, body).
    async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).await.expect("tcp connect");
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        let text = String::from_utf8_lossy(&buf).to_string();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        (status, body)
    }

    #[tokio::test]
    async fn test_healthz_always_ok() {
        let (addr, h) = spawn_for_test(false).await;
        let (status, body) = http_get(addr, "/healthz").await;
        assert_eq!(status, 200, "body: {body}");
        assert_eq!(body, "ok");
        h.abort();
    }

    #[tokio::test]
    async fn test_unknown_path_404() {
        let (addr, h) = spawn_for_test(true).await;
        let (status, _body) = http_get(addr, "/no-such-thing").await;
        assert_eq!(status, 404);
        h.abort();
    }

    #[tokio::test]
    async fn test_metrics_endpoint_returns_prometheus_format() {
        let (addr, h) = spawn_for_test(true).await;
        let mut stream = TcpStream::connect(addr).await.expect("tcp connect");
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        let text = String::from_utf8_lossy(&buf).to_string();

        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        assert_eq!(status, 200);

        // Header sanity: content-type should start with text/plain.
        assert!(
            text.to_ascii_lowercase()
                .contains("content-type: text/plain"),
            "missing content-type header in: {text}"
        );

        // Body content is verified by the workspace-level integration test
        // (`tests/server_http.rs`), which exercises the real server with a
        // properly-installed global recorder. Here we only assert the
        // endpoint plumbing — status code + content-type — to avoid races
        // with other tests over the global Prometheus recorder.
        h.abort();
    }

    #[tokio::test]
    async fn test_readyz_503_when_not_ready_then_200() {
        // Custom spawn: keep a handle on the state to flip readiness.
        let prom = isolated_handle();
        metrics::describe_gauge!(
            "grumpydb_connections_active",
            "Currently open TCP connections."
        );
        let state = Arc::new(HttpState {
            ready: AtomicBool::new(false),
            prometheus: prom,
            auth_store: None,
        });
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let bind = format!("127.0.0.1:{port}");
        let h = serve(state.clone(), &bind).await.expect("serve");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let addr: SocketAddr = bind.parse().unwrap();

        let (status, _) = http_get(addr, "/readyz").await;
        assert_eq!(status, 503);

        state.ready.store(true, Ordering::Release);
        let (status, body) = http_get(addr, "/readyz").await;
        assert_eq!(status, 200, "body: {body}");
        h.abort();
    }
}
