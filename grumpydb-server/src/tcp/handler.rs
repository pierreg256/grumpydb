//! Per-connection handler: read commands, authorize, execute, respond.

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use base64::Engine as _;
use futures::FutureExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

use grumpydb::GrumpyError;
use grumpydb::concurrency::shared::SharedServer;
use grumpydb::document::crdt::merge_values as merge_crdt_values;
use grumpydb::document::value::{CrdtKind, Value};
use grumpydb::index::encoding::{encode_sortable_value, extract_field};
use grumpydb_protocol::{Command, MAX_LINE_LENGTH, PROTOCOL_VERSION, Response, parse_command};

use crate::auth::role::{ResourceScope, RoleAssignment, RoleName};
use crate::auth::store::AuthStore;
use crate::cluster::hints::{HintOperation, HintRecord, HintStore};
use crate::cluster::read_repair::{ReadRepairIntent, ReadRepairStore};
use crate::coordinator::Coordinator;
use crate::limits::Limits;
use crate::session::SessionContext;

/// Background pipelines used by connection handlers.
#[derive(Clone)]
pub struct RepairPipelines {
    pub hint_store: Arc<HintStore>,
    pub read_repair_store: Arc<ReadRepairStore>,
}

const VERIFIED_QUERY_MAX_CANDIDATES: usize = 4096;

/// Handle a single client connection (plaintext or TLS).
pub async fn handle_connection<S>(
    stream: S,
    peer: SocketAddr,
    auth_store: Arc<parking_lot::RwLock<AuthStore>>,
    shared_server: SharedServer,
    limits: Arc<Limits>,
    coordinator: Arc<Coordinator>,
    pipelines: RepairPipelines,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    let peer_ip = peer.ip();
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut session = SessionContext::new();
    let mut consecutive_errors: u32 = 0;
    let mut consecutive_rate_limits: u32 = 0;

    // Send banner
    let banner = format!("+GRUMPYDB {PROTOCOL_VERSION}\r\n");
    writer.write_all(banner.as_bytes()).await?;
    writer.flush().await?;

    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break; // EOF
        }

        // Per-IP command rate limit. Token-bucket refill is continuous, so a
        // bursty-but-bounded client stays well under the cap.
        if !limits.try_take_command(peer_ip) {
            tracing::warn!(peer = %peer, "command rate-limited");
            metrics::counter!(
                "grumpydb_rate_limit_hits_total",
                "kind" => "command"
            )
            .increment(1);
            write_resp(&mut writer, &Response::Error("rate limited".into())).await?;
            consecutive_rate_limits += 1;
            if consecutive_rate_limits > 10 {
                tracing::warn!(peer = %peer, "closing: too many consecutive rate-limit hits");
                break;
            }
            continue;
        }
        consecutive_rate_limits = 0;

        if line.len() > MAX_LINE_LENGTH {
            write_resp(&mut writer, &Response::Error("line too long".into())).await?;
            consecutive_errors += 1;
            if consecutive_errors > 10 {
                break;
            }
            continue;
        }

        let command = match parse_command(&line) {
            Ok(cmd) => cmd,
            Err(e) => {
                tracing::warn!(error = %e, "command parse failed");
                write_resp(&mut writer, &Response::Error(e.to_string())).await?;
                consecutive_errors += 1;
                if consecutive_errors > 10 {
                    break;
                }
                continue;
            }
        };

        consecutive_errors = 0;
        let cmd_name = command_name(base_command(&command));
        let cmd_span = tracing::info_span!(
            "command",
            cmd = cmd_name,
            user = session.username().ok().unwrap_or("-"),
            tenant = session.tenant().ok().unwrap_or("-"),
        );
        let _enter = cmd_span.enter();

        if let Err(e) = validate_consistency_command(&command, coordinator.as_ref()) {
            tracing::warn!(error = %e, "consistency validation failed");
            write_resp(&mut writer, &Response::Error(e)).await?;
            continue;
        }

        // QUIT
        if matches!(command, Command::Quit) {
            tracing::info!("client requested quit");
            write_resp(&mut writer, &Response::Ok("BYE".into())).await?;
            break;
        }

        // PING (always allowed)
        if matches!(command, Command::Ping) {
            write_resp(&mut writer, &Response::Ok("PONG".into())).await?;
            continue;
        }

        // Per-IP failed-login backoff: if this IP has been flooded with bad
        // logins, refuse all LOGIN attempts until the cooldown elapses.
        if matches!(command, Command::Login { .. })
            && let Some(retry_after) = limits.login_backoff(peer_ip)
        {
            tracing::warn!(
                peer = %peer,
                retry_after_secs = retry_after.as_secs(),
                "login rate-limited (failed-login backoff)"
            );
            metrics::counter!(
                "grumpydb_login_failures_total",
                "reason" => "rate_limited"
            )
            .increment(1);
            metrics::counter!(
                "grumpydb_rate_limit_hits_total",
                "kind" => "login"
            )
            .increment(1);
            let msg = format!(
                "rate limited (login: retry after {}s)",
                retry_after.as_secs().max(1)
            );
            write_resp(&mut writer, &Response::Error(msg)).await?;
            continue;
        }

        // Authorize
        if let Err(e) = session.authorize(&command) {
            tracing::warn!(error = %e, "authorization denied");
            write_resp(&mut writer, &Response::Error(e.to_string())).await?;
            continue;
        }

        if let Err(e) =
            validate_write_concern_runtime(&command, &session, coordinator.as_ref(), &shared_server)
        {
            tracing::warn!(error = %e, "write concern validation failed");
            write_resp(&mut writer, &Response::Error(e)).await?;
            continue;
        }

        if let Err(e) =
            validate_read_concern_runtime(&command, &session, coordinator.as_ref(), &shared_server)
        {
            tracing::warn!(error = %e, "read concern validation failed");
            write_resp(&mut writer, &Response::Error(e)).await?;
            continue;
        }

        if let Err(e) =
            wait_for_read_ack_quorum(&command, &session, coordinator.as_ref(), &shared_server).await
        {
            tracing::warn!(error = %e, "read ack quorum failed");
            write_resp(&mut writer, &Response::Error(e)).await?;
            continue;
        }

        // Execute (with panic isolation: a corrupt page or bug in the engine
        // must not tear down the entire server. Surface as Corruption error.)
        let started = std::time::Instant::now();
        let mut response = AssertUnwindSafe(execute_command(
            base_command(&command),
            &mut session,
            &auth_store,
            &shared_server,
            coordinator.as_ref(),
        ))
        .catch_unwind()
        .await
        .unwrap_or_else(|panic_payload| {
            let msg = panic_message(&panic_payload);
            tracing::error!(panic = %msg, "engine panic caught");
            Response::Error(format!("internal error (corruption): {msg}"))
        });

        response = maybe_converge_read_quorum(
            &command,
            response,
            &session,
            coordinator.as_ref(),
            &shared_server,
            pipelines.read_repair_store.as_ref(),
        )
        .await;

        if !matches!(&response, Response::Error(_))
            && let Err(e) = record_local_index_ddl(&command, &session, coordinator.as_ref()).await
        {
            tracing::warn!(error = %e, "local index ddl record failed");
            response = Response::Error(e);
        }

        if !matches!(&response, Response::Error(_))
            && let Err(e) = wait_for_write_ack_quorum(
                &command,
                &session,
                coordinator.as_ref(),
                pipelines.hint_store.as_ref(),
                &shared_server,
            )
            .await
        {
            tracing::warn!(error = %e, "write apply quorum failed");
            response = Response::Error(e);
        }

        let elapsed_us = started.elapsed().as_micros() as u64;
        let elapsed_secs = started.elapsed().as_secs_f64();
        let result_label = match &response {
            Response::Error(_) => "error",
            _ => "ok",
        };
        metrics::counter!(
            "grumpydb_commands_total",
            "cmd" => cmd_name,
            "result" => result_label
        )
        .increment(1);
        metrics::histogram!(
            "grumpydb_command_duration_seconds",
            "cmd" => cmd_name
        )
        .record(elapsed_secs);
        match &response {
            Response::Error(e) => {
                tracing::warn!(elapsed_us, error = %e, "command failed");
            }
            _ => tracing::debug!(elapsed_us, "command ok"),
        }

        // Track LOGIN outcomes for the failed-login backoff. We treat any
        // `Response::Error` here as a credential failure — wrong password,
        // unknown user, hash error all collapse to the same generic error
        // by design (anti-enumeration).
        if matches!(command, Command::Login { .. }) {
            match &response {
                Response::Error(_) => {
                    metrics::counter!(
                        "grumpydb_login_failures_total",
                        "reason" => "invalid_credentials"
                    )
                    .increment(1);
                    limits.record_failed_login(peer_ip);
                }
                _ => limits.record_successful_login(peer_ip),
            }
        }

        write_resp(&mut writer, &response).await?;
    }

    Ok(())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn query_route_key(command: &Command) -> Option<String> {
    match base_command(command) {
        Command::Query {
            collection,
            index_name,
            value,
        } => Some(format!("q:{collection}:{index_name}:{value}")),
        Command::QueryRange {
            collection,
            index_name,
            start,
            end,
        } => Some(format!("qr:{collection}:{index_name}:{start}:{end}")),
        _ => None,
    }
}

fn write_hint_operation(command: &Command) -> Option<HintOperation> {
    match base_command(command) {
        Command::Insert { value, .. } | Command::Update { value, .. } => {
            Some(HintOperation::Upsert {
                value_json: value.clone(),
            })
        }
        Command::Delete { .. } => Some(HintOperation::Delete),
        Command::PutWithVc {
            value,
            vector_clock,
            ..
        } => Some(HintOperation::Upsert {
            value_json: serde_json::json!({"value": value, "vector_clock": vector_clock})
                .to_string(),
        }),
        // Phase 44c: CREATE INDEX / DROP INDEX never produce hints
        // anymore — they propagate through the schema gossip path
        // ([`crate::cluster::schema`]). Pre-44c hints persisted on
        // disk are still replayable via [`HintRecord::resolved_operation`].
        _ => None,
    }
}

/// Phase 44c: record one DDL locally in the schema log. The cluster
/// schema version is bumped, the entry is persisted to
/// `_cluster/schema.log`, and a materialization job is enqueued on
/// the local materializer (no-op on this node since the engine call
/// already ran). Other peers learn about the change via the gossip
/// pull loop in [`crate::cluster::gossip`].
///
/// Replaces the legacy `replicate_index_ddl` (pre-44c), which used
/// preference-list routing and only reached N peers out of the
/// cluster — see `docs/SCHEMA_GOSSIP.md` for the full rationale.
async fn record_local_index_ddl(
    command: &Command,
    session: &SessionContext,
    coordinator: &Coordinator,
) -> Result<(), String> {
    let (collection, index_name, field_path) = match base_command(command) {
        Command::CreateIndex {
            collection,
            index_name,
            field_path,
        } => (
            collection.as_str(),
            index_name.as_str(),
            Some(field_path.as_str()),
        ),
        Command::DropIndex {
            collection,
            index_name,
        } => (collection.as_str(), index_name.as_str(), None),
        _ => return Ok(()),
    };

    let Some(db_name) = session.current_db() else {
        return Ok(());
    };
    let tenant = session.tenant().map_err(|e| e.to_string())?.to_string();

    // Cheap monotonic HLC proxy: ms-since-epoch. The schema state's
    // LWW resolution only requires a strictly-monotonic per-node
    // value; concurrent CREATEs across the cluster get tie-broken
    // deterministically by the BTreeMap insertion order downstream.
    // 44d/45 may swap this for the real `HlcClock` plumbed through.
    let hlc = now_unix_millis();

    let entry = match field_path {
        Some(path) => coordinator
            .apply_local_create_index(&tenant, db_name, collection, index_name, path, hlc),
        None => coordinator.apply_local_drop_index(&tenant, db_name, collection, index_name, hlc),
    };

    if let Some(entry) = entry {
        tracing::info!(
            schema_version = entry.version,
            tenant = %tenant,
            database = %db_name,
            collection = %collection,
            index_name = %index_name,
            "local schema DDL applied; gossip will propagate"
        );
    }

    Ok(())
}

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn maybe_converge_read_quorum(
    command: &Command,
    response: Response,
    session: &SessionContext,
    coordinator: &Coordinator,
    shared_server: &SharedServer,
    repair_store: &ReadRepairStore,
) -> Response {
    let (read_concern, _) = match effective_consistency_values(command, session, shared_server) {
        Ok(v) => v,
        Err(_) => return response,
    };
    if read_concern.unwrap_or(1) <= 1 {
        return response;
    }

    if matches!(
        base_command(command),
        Command::Query { .. } | Command::QueryRange { .. }
    ) {
        return verify_index_query_quorum(command, session, coordinator, shared_server).await;
    }

    let Some((collection, key)) = read_target(command) else {
        return response;
    };

    let Some(db_name) = session.current_db() else {
        return response;
    };
    let Ok(tenant) = session.tenant() else {
        return response;
    };

    let local_value = match &response {
        Response::Bulk(Some(s)) => Some(s.clone()),
        Response::Bulk(None) => None,
        _ => return response,
    };

    let remote_reads = coordinator
        .fanout_read_peer_values(tenant, db_name, collection, key)
        .await;

    let mut values = Vec::new();
    if let Some(v) = &local_value {
        values.push(v.clone());
    }
    for (_, v) in &remote_reads {
        if let Ok(Some(json)) = v {
            values.push(json.clone());
        }
    }
    if values.is_empty() {
        return response;
    }

    let canonical = choose_canonical_json(values);

    // Local convergence.
    if local_value.as_deref() != Some(canonical.as_str())
        && let Some(current_db) = session.current_db()
        && let Ok(db) = shared_server.database(tenant, current_db)
        && let Ok(id) = parse_uuid(key)
        && let Ok(val) = parse_json_value(&canonical)
    {
        if db.get(collection, &id).ok().flatten().is_some() {
            let _ = db.update(collection, &id, val);
        } else {
            let _ = db.insert(collection, id, val);
        }
    }

    // Remote convergence: immediate repair, durable retry on failure.
    for (node_id, res) in remote_reads {
        let needs_repair = match res {
            Ok(Some(v)) => v != canonical,
            Ok(None) => true,
            Err(_) => true,
        };
        if !needs_repair {
            continue;
        }

        let repaired = coordinator
            .repair_peer_value(&node_id, tenant, db_name, collection, key, &canonical)
            .await;
        match repaired {
            Ok(()) => {
                metrics::counter!("grumpydb_read_repair_applied_total").increment(1);
            }
            Err(e) => {
                tracing::warn!(error = %e, node_id = %node_id, key = %key, "read repair failed, enqueueing retry");
                let intent = ReadRepairIntent {
                    created_at_unix: now_unix(),
                    tenant: tenant.to_string(),
                    database: db_name.to_string(),
                    collection: collection.to_string(),
                    key: key.to_string(),
                    target_node_id: node_id,
                    value_json: canonical.clone(),
                    reason: "convergence-retry".to_string(),
                };
                if let Err(err) = repair_store.append(&intent) {
                    tracing::warn!(error = %err, "failed to persist read-repair retry intent");
                } else {
                    metrics::counter!("grumpydb_read_repair_intents_enqueued_total").increment(1);
                }
            }
        }
    }

    Response::Bulk(Some(canonical))
}

async fn verify_index_query_quorum(
    command: &Command,
    session: &SessionContext,
    coordinator: &Coordinator,
    shared_server: &SharedServer,
) -> Response {
    let (read_concern, _) = match effective_consistency_values(command, session, shared_server) {
        Ok(v) => v,
        Err(e) => return Response::Error(e),
    };

    let Some(db_name) = session.current_db() else {
        return Response::Error("no database selected (use USE <db>)".into());
    };
    let tenant = match session.tenant() {
        Ok(t) => t.to_string(),
        Err(e) => return Response::Error(e.to_string()),
    };

    let db = match shared_server.database(&tenant, db_name) {
        Ok(db) => db,
        Err(e) => return Response::Error(e.to_string()),
    };

    match base_command(command) {
        Command::Query {
            collection,
            index_name,
            value,
        } => {
            let expected = match parse_json_value(value) {
                Ok(v) => v,
                Err(e) => return Response::Error(e),
            };
            let field_path = match db.index_field_path(collection, index_name) {
                Ok(p) => p,
                Err(e) => return Response::Error(e.to_string()),
            };

            let mut candidates: HashSet<Uuid> = HashSet::new();
            match db.query(collection, index_name, &expected) {
                Ok(rows) => {
                    for (id, _) in rows {
                        let _ = candidates.insert(id);
                    }
                }
                Err(e) => return Response::Error(e.to_string()),
            }

            let route_key = format!("q:{collection}:{index_name}:{value}");
            let remote = coordinator
                .fanout_query_peer_candidates_exact(
                    &tenant,
                    db_name,
                    collection,
                    route_key.as_bytes(),
                    index_name,
                    value,
                )
                .await;
            for (node_id, res) in remote {
                let keys = match res {
                    Ok(keys) => keys,
                    Err(e) => {
                        return Response::Error(format!(
                            "verified query failed to fetch candidates from peer {node_id}: {e}"
                        ));
                    }
                };
                for key in keys {
                    match Uuid::parse_str(&key) {
                        Ok(id) => {
                            let _ = candidates.insert(id);
                        }
                        Err(e) => {
                            return Response::Error(format!(
                                "verified query received invalid candidate key from peer {node_id}: {e}"
                            ));
                        }
                    }
                }
            }

            if candidates.len() > VERIFIED_QUERY_MAX_CANDIDATES {
                return Response::Error(format!(
                    "verified query candidate limit exceeded: {} > {}",
                    candidates.len(),
                    VERIFIED_QUERY_MAX_CANDIDATES
                ));
            }

            let mut ids: Vec<Uuid> = candidates.into_iter().collect();
            ids.sort();

            let mut items = Vec::new();
            for id in ids {
                let key = id.to_string();
                if let Err(e) = coordinator
                    .wait_for_read_ack_quorum(db_name, collection, key.as_bytes(), read_concern)
                    .await
                {
                    return Response::Error(e);
                }

                match resolve_quorum_document_for_key(
                    coordinator,
                    &db,
                    &tenant,
                    db_name,
                    collection,
                    &key,
                )
                .await
                {
                    Ok(Some(doc)) => {
                        let matches =
                            extract_field(&doc, &field_path).is_some_and(|v| v == &expected);
                        if matches {
                            items.push(Response::Bulk(Some(format!(
                                "{} {}",
                                id,
                                value_to_json(&doc)
                            ))));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => return Response::Error(e),
                }
            }

            Response::Array(items)
        }
        Command::QueryRange {
            collection,
            index_name,
            start,
            end,
        } => {
            let start_value = match parse_json_value(start) {
                Ok(v) => v,
                Err(e) => return Response::Error(e),
            };
            let end_value = match parse_json_value(end) {
                Ok(v) => v,
                Err(e) => return Response::Error(e),
            };
            let start_encoded = match encode_sortable_value(&start_value) {
                Ok(v) => v,
                Err(e) => return Response::Error(e.to_string()),
            };
            let end_encoded = match encode_sortable_value(&end_value) {
                Ok(v) => v,
                Err(e) => return Response::Error(e.to_string()),
            };
            let field_path = match db.index_field_path(collection, index_name) {
                Ok(p) => p,
                Err(e) => return Response::Error(e.to_string()),
            };

            let mut candidates: HashSet<Uuid> = HashSet::new();
            match db.query_range(collection, index_name, &start_value, &end_value) {
                Ok(rows) => {
                    for (id, _) in rows {
                        let _ = candidates.insert(id);
                    }
                }
                Err(e) => return Response::Error(e.to_string()),
            }

            let route_key = format!("qr:{collection}:{index_name}:{start}:{end}");
            let remote = coordinator
                .fanout_query_peer_candidates_range(
                    &tenant,
                    db_name,
                    collection,
                    route_key.as_bytes(),
                    index_name,
                    start,
                    end,
                )
                .await;
            for (node_id, res) in remote {
                let keys = match res {
                    Ok(keys) => keys,
                    Err(e) => {
                        return Response::Error(format!(
                            "verified query failed to fetch candidates from peer {node_id}: {e}"
                        ));
                    }
                };
                for key in keys {
                    match Uuid::parse_str(&key) {
                        Ok(id) => {
                            let _ = candidates.insert(id);
                        }
                        Err(e) => {
                            return Response::Error(format!(
                                "verified query received invalid candidate key from peer {node_id}: {e}"
                            ));
                        }
                    }
                }
            }

            if candidates.len() > VERIFIED_QUERY_MAX_CANDIDATES {
                return Response::Error(format!(
                    "verified query candidate limit exceeded: {} > {}",
                    candidates.len(),
                    VERIFIED_QUERY_MAX_CANDIDATES
                ));
            }

            let mut ids: Vec<Uuid> = candidates.into_iter().collect();
            ids.sort();

            let mut items = Vec::new();
            for id in ids {
                let key = id.to_string();
                if let Err(e) = coordinator
                    .wait_for_read_ack_quorum(db_name, collection, key.as_bytes(), read_concern)
                    .await
                {
                    return Response::Error(e);
                }

                match resolve_quorum_document_for_key(
                    coordinator,
                    &db,
                    &tenant,
                    db_name,
                    collection,
                    &key,
                )
                .await
                {
                    Ok(Some(doc)) => {
                        let in_range = extract_field(&doc, &field_path)
                            .and_then(|v| encode_sortable_value(v).ok())
                            .is_some_and(|encoded| {
                                encoded >= start_encoded && encoded < end_encoded
                            });
                        if in_range {
                            items.push(Response::Bulk(Some(format!(
                                "{} {}",
                                id,
                                value_to_json(&doc)
                            ))));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => return Response::Error(e),
                }
            }

            Response::Array(items)
        }
        _ => response_error_internal(),
    }
}

async fn resolve_quorum_document_for_key(
    coordinator: &Coordinator,
    db: &grumpydb::concurrency::shared::SharedDatabase,
    tenant: &str,
    database: &str,
    collection: &str,
    key: &str,
) -> Result<Option<Value>, String> {
    let uuid = parse_uuid(key)?;
    let local_value_json = db
        .get(collection, &uuid)
        .map_err(|e| e.to_string())?
        .map(|v| value_to_json(&v));

    let remote_reads = coordinator
        .fanout_read_peer_values(tenant, database, collection, key)
        .await;

    let mut values = Vec::new();
    if let Some(v) = local_value_json {
        values.push(v);
    }
    for (node_id, res) in remote_reads {
        match res {
            Ok(Some(v)) => values.push(v),
            Ok(None) => {}
            Err(e) => {
                return Err(format!(
                    "verified query failed to read candidate from peer {node_id}: {e}"
                ));
            }
        }
    }
    if values.is_empty() {
        return Ok(None);
    }

    let canonical = choose_canonical_json(values);
    parse_json_value(&canonical).map(Some)
}

fn response_error_internal() -> Response {
    Response::Error("internal error: unsupported verified query command".into())
}

fn choose_canonical_json(values: Vec<String>) -> String {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for v in values {
        *counts.entry(v).or_insert(0) += 1;
    }

    let mut best = String::new();
    let mut best_count = 0usize;
    for (v, c) in counts {
        if c > best_count || (c == best_count && v > best) {
            best = v;
            best_count = c;
        }
    }
    best
}

/// Returns a stable string identifier for a command (for tracing fields).
fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::WithConsistency { command, .. } => command_name(command),
        Command::Login { .. } => "LOGIN",
        Command::Token(_) => "TOKEN",
        Command::Refresh(_) => "REFRESH",
        Command::WhoAmI => "WHOAMI",
        Command::Topology => "TOPOLOGY",
        Command::SnapshotHlc => "SNAPSHOT_HLC",
        Command::SchemaVersion => "SCHEMA_VERSION",
        Command::SchemaStatus => "SCHEMA_STATUS",
        Command::Use(_) => "USE",
        Command::Ping => "PING",
        Command::Quit => "QUIT",
        Command::CreateDatabase(_) => "CREATE_DATABASE",
        Command::DropDatabase(_) => "DROP_DATABASE",
        Command::ListDatabases => "LIST_DATABASES",
        Command::SetDatabaseConsistency { .. } => "SET_DATABASE_CONSISTENCY",
        Command::ResetDatabaseConsistency { .. } => "RESET_DATABASE_CONSISTENCY",
        Command::ShowDatabaseConsistency { .. } => "SHOW_DATABASE_CONSISTENCY",
        Command::CreateCollection(_) => "CREATE_COLLECTION",
        Command::DropCollection(_) => "DROP_COLLECTION",
        Command::ListCollections => "LIST_COLLECTIONS",
        Command::Insert { .. } => "INSERT",
        Command::Get { .. } => "GET",
        Command::Update { .. } => "UPDATE",
        Command::Delete { .. } => "DELETE",
        Command::PutWithVc { .. } => "PUT_WITH_VC",
        Command::Scan { .. } => "SCAN",
        Command::CreateIndex { .. } => "CREATE_INDEX",
        Command::DropIndex { .. } => "DROP_INDEX",
        Command::ListIndexes(_) => "LIST_INDEXES",
        Command::Query { .. } => "QUERY",
        Command::QueryRange { .. } => "QUERY_RANGE",
        Command::Compact(_) => "COMPACT",
        Command::Flush => "FLUSH",
        Command::Count(_) => "COUNT",
        Command::CreateUser { .. } => "CREATE_USER",
        Command::DropUser(_) => "DROP_USER",
        Command::ListUsers(_) => "LIST_USERS",
        Command::Grant { .. } => "GRANT",
        Command::Revoke { .. } => "REVOKE",
        Command::CreateTenant(_) => "CREATE_TENANT",
        Command::DropTenant(_) => "DROP_TENANT",
        Command::ListTenants => "LIST_TENANTS",
        Command::ElectWriter { .. } => "ELECT_WRITER",
        Command::PlanRebalanceAddNode { .. } => "REBALANCE_PLAN_ADD_NODE",
        Command::PlanRebalanceRemoveNode { .. } => "REBALANCE_PLAN_REMOVE_NODE",
        Command::ExecuteRebalanceAddNode { .. } => "REBALANCE_EXECUTE_ADD_NODE",
        Command::ExecuteRebalanceRemoveNode { .. } => "REBALANCE_EXECUTE_REMOVE_NODE",
    }
}

/// Extract a string description from a panic payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn base_command(command: &Command) -> &Command {
    match command {
        Command::WithConsistency { command, .. } => command,
        other => other,
    }
}

fn consistency_values(command: &Command) -> (Option<u16>, Option<u16>) {
    match command {
        Command::WithConsistency {
            read_concern,
            write_concern,
            ..
        } => (*read_concern, *write_concern),
        _ => (None, None),
    }
}

fn effective_consistency_values(
    command: &Command,
    session: &SessionContext,
    shared_server: &SharedServer,
) -> Result<(Option<u16>, Option<u16>), String> {
    let (explicit_read, explicit_write) = consistency_values(command);
    if explicit_read.is_some() || explicit_write.is_some() {
        return Ok((explicit_read, explicit_write));
    }

    let Some(db_name) = session.current_db() else {
        return Ok((None, None));
    };
    let tenant = session.tenant().map_err(|e| e.to_string())?;
    shared_server
        .database_consistency_defaults(tenant, db_name)
        .map_err(|e| e.to_string())
}

fn supports_consistency(command: &Command) -> bool {
    matches!(
        command,
        Command::Insert { .. }
            | Command::Get { .. }
            | Command::Update { .. }
            | Command::Delete { .. }
            | Command::PutWithVc { .. }
            | Command::Scan { .. }
            | Command::Query { .. }
            | Command::QueryRange { .. }
            | Command::Count(_)
    )
}

fn is_write_command(command: &Command) -> bool {
    matches!(
        command,
        Command::Insert { .. }
            | Command::Update { .. }
            | Command::Delete { .. }
            | Command::PutWithVc { .. }
    )
}

fn validate_consistency_command(
    command: &Command,
    coordinator: &Coordinator,
) -> Result<(), String> {
    let (r, w) = consistency_values(command);
    if r.is_none() && w.is_none() {
        return Ok(());
    }

    let base = base_command(command);
    if !supports_consistency(base) {
        return Err("consistency concerns are only supported for data commands".into());
    }

    let base = base_command(command);
    if !is_write_command(base) && w.is_some() {
        return Err("write concern is only supported for write commands".into());
    }

    coordinator.validate_concerns(r, w)
}

fn write_target(command: &Command) -> Option<(&str, &str)> {
    match base_command(command) {
        Command::Insert {
            collection, key, ..
        }
        | Command::Update {
            collection, key, ..
        }
        | Command::Delete { collection, key }
        | Command::PutWithVc {
            collection, key, ..
        } => Some((collection.as_str(), key.as_str())),
        _ => None,
    }
}

fn read_target(command: &Command) -> Option<(&str, &str)> {
    match base_command(command) {
        Command::Get { collection, key } => Some((collection.as_str(), key.as_str())),
        _ => None,
    }
}

fn validate_read_concern_runtime(
    command: &Command,
    session: &SessionContext,
    coordinator: &Coordinator,
    shared_server: &SharedServer,
) -> Result<(), String> {
    let (read_concern, _) = effective_consistency_values(command, session, shared_server)?;
    if read_concern.is_none() {
        return Ok(());
    }

    let Some((collection, key)) = read_target(command) else {
        let Some(route_key) = query_route_key(command) else {
            return Ok(());
        };

        let Some(db_name) = session.current_db() else {
            return Ok(());
        };

        let query_collection = match base_command(command) {
            Command::Query { collection, .. } | Command::QueryRange { collection, .. } => {
                collection.as_str()
            }
            _ => return Ok(()),
        };

        return coordinator.validate_read_concern_for_key(
            db_name,
            query_collection,
            route_key.as_bytes(),
            read_concern,
        );
    };

    let Some(db_name) = session.current_db() else {
        // Let normal command execution emit the canonical "no database selected"
        // error if needed.
        return Ok(());
    };

    coordinator.validate_read_concern_for_key(db_name, collection, key.as_bytes(), read_concern)
}

async fn wait_for_read_ack_quorum(
    command: &Command,
    session: &SessionContext,
    coordinator: &Coordinator,
    shared_server: &SharedServer,
) -> Result<(), String> {
    let (read_concern, _) = effective_consistency_values(command, session, shared_server)?;
    if read_concern.unwrap_or(1) <= 1 {
        return Ok(());
    }

    let Some((collection, key)) = read_target(command) else {
        let Some(route_key) = query_route_key(command) else {
            return Ok(());
        };
        let Some(db_name) = session.current_db() else {
            return Ok(());
        };

        let query_collection = match base_command(command) {
            Command::Query { collection, .. } | Command::QueryRange { collection, .. } => {
                collection.as_str()
            }
            _ => return Ok(()),
        };

        return coordinator
            .wait_for_read_ack_quorum(
                db_name,
                query_collection,
                route_key.as_bytes(),
                read_concern,
            )
            .await;
    };
    let Some(db_name) = session.current_db() else {
        return Ok(());
    };

    coordinator
        .wait_for_read_ack_quorum(db_name, collection, key.as_bytes(), read_concern)
        .await
}

fn validate_write_concern_runtime(
    command: &Command,
    session: &SessionContext,
    coordinator: &Coordinator,
    shared_server: &SharedServer,
) -> Result<(), String> {
    let (_, write_concern) = effective_consistency_values(command, session, shared_server)?;
    if write_concern.is_none() {
        return Ok(());
    }

    let Some((collection, key)) = write_target(command) else {
        return Ok(());
    };

    let Some(db_name) = session.current_db() else {
        // Let normal command execution emit the canonical "no database selected"
        // error if needed.
        return Ok(());
    };

    coordinator.validate_write_concern_for_key(db_name, collection, key.as_bytes(), write_concern)
}

async fn wait_for_write_ack_quorum(
    command: &Command,
    session: &SessionContext,
    coordinator: &Coordinator,
    hint_store: &HintStore,
    shared_server: &SharedServer,
) -> Result<(), String> {
    let (_, write_concern) = effective_consistency_values(command, session, shared_server)?;
    let w = write_concern.unwrap_or(1) as usize;
    if w <= 1 {
        return Ok(());
    }

    let Some((collection, key)) = write_target(command) else {
        return Ok(());
    };
    let Some(db_name) = session.current_db() else {
        return Ok(());
    };

    let tenant = session.tenant().map_err(|e| e.to_string())?.to_string();
    let operation = match write_hint_operation(command) {
        Some(op) => op,
        None => return Ok(()),
    };

    let replicas = coordinator.replica_peer_nodes_for_key(db_name, collection, key.as_bytes());
    let required_remote = w.saturating_sub(1);
    if replicas.len() < required_remote {
        return Err(format!(
            "write quorum cannot be satisfied: need {required_remote} remote acks, only {} replica peers available",
            replicas.len()
        ));
    }

    let mut acked_remote = 0usize;
    let mut failures = Vec::new();
    for node_id in replicas {
        let hint = HintRecord {
            created_at_unix: now_unix(),
            tenant: tenant.clone(),
            database: db_name.to_string(),
            collection: collection.to_string(),
            key: key.to_string(),
            operation: Some(operation.clone()),
            payload_json: None,
        };

        match coordinator.replay_hint_to_peer(&node_id, &hint).await {
            Ok(()) => acked_remote += 1,
            Err(e) => {
                failures.push(format!("{node_id}: {e}"));
                if let Err(store_err) = hint_store.append(&node_id, &hint) {
                    tracing::warn!(
                        error = %store_err,
                        node_id = %node_id,
                        "failed to persist hinted handoff record"
                    );
                } else {
                    metrics::counter!("grumpydb_hints_enqueued_total").increment(1);
                }
            }
        }
    }

    let acked = 1 + acked_remote;
    if acked >= w {
        return Ok(());
    }

    Err(format!(
        "write quorum not reached: acked {acked}/{w}; failures: {}",
        failures.join(" | ")
    ))
}

async fn write_resp<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    response: &Response,
) -> Result<(), std::io::Error> {
    writer.write_all(response.serialize().as_bytes()).await?;
    writer.flush().await
}

async fn execute_command(
    command: &Command,
    session: &mut SessionContext,
    auth_store: &Arc<parking_lot::RwLock<AuthStore>>,
    shared_server: &SharedServer,
    coordinator: &Coordinator,
) -> Response {
    match command {
        Command::WithConsistency { .. } => {
            Response::Error("internal error: consistency wrapper not unwrapped".into())
        }

        // ── Auth ────────────────────────────────────────────────
        Command::Login {
            tenant,
            username,
            password,
        } => {
            let store = auth_store.read();
            match store.authenticate(tenant, username, password) {
                Ok((access, refresh)) => {
                    // Set session from the access token
                    if let Ok(claims) = store.verify_token(&access) {
                        session.set_claims(claims);
                    }
                    // Ensure the tenant exists as a client in the engine
                    let _ = shared_server.create_client(tenant);
                    tracing::info!(tenant = %tenant, user = %username, "login success");
                    Response::Ok(format!("TOKEN {access} {refresh}"))
                }
                Err(e) => {
                    tracing::warn!(
                        tenant = %tenant,
                        user = %username,
                        error = %e,
                        "login failed"
                    );
                    Response::Error("invalid credentials".into())
                }
            }
        }
        Command::Token(token) => {
            let store = auth_store.read();
            match store.verify_token(token) {
                Ok(claims) => {
                    let user = claims.sub.clone();
                    let tenant = claims.tenant.clone();
                    session.set_claims(claims);
                    tracing::info!(tenant = %tenant, user = %user, "session resumed via token");
                    Response::Ok("OK".into())
                }
                Err(e) => {
                    tracing::warn!(error = %e, "token verification failed");
                    Response::Error(e.to_string())
                }
            }
        }
        Command::Refresh(refresh_token) => {
            let store = auth_store.read();
            match store.refresh_access_token(refresh_token) {
                Ok(new_access) => {
                    tracing::info!("access token refreshed");
                    Response::Ok(format!("TOKEN {new_access}"))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "token refresh failed");
                    Response::Error(e.to_string())
                }
            }
        }
        Command::WhoAmI => match session.claims() {
            Some(claims) => {
                let roles: Vec<String> = claims
                    .roles
                    .iter()
                    .map(|r| format!("{}:{:?}", r.role, r.scope))
                    .collect();
                Response::Ok(format!(
                    "USER {} TENANT {} ROLES {}",
                    claims.sub,
                    claims.tenant,
                    roles.join(",")
                ))
            }
            None => Response::Error("not authenticated".into()),
        },
        Command::Topology => Response::Bulk(Some(coordinator.topology_json().to_string())),
        Command::SnapshotHlc => with_db(session, shared_server, |db| {
            let snapshot = db.begin_read().snapshot_hlc().0;
            let value = i64::try_from(snapshot)
                .map_err(|_| format!("snapshot HLC {snapshot} does not fit in i64"))?;
            Ok(Response::Integer(value))
        }),
        Command::SchemaVersion => Response::Integer(coordinator.schema_version() as i64),
        Command::SchemaStatus => Response::Bulk(Some(coordinator.schema_status_json().to_string())),

        // ── Session ─────────────────────────────────────────────
        Command::Use(db_name) => {
            let tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            // Create the database if it doesn't exist (like embedded `use`)
            let _ = shared_server.create_database(&tenant, db_name);
            session.set_database(db_name.clone());
            Response::Ok("OK".into())
        }

        // ── Database management ─────────────────────────────────
        Command::CreateDatabase(name) => {
            let tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            match shared_server.create_database(&tenant, name) {
                Ok(()) => Response::Ok("OK".into()),
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Command::DropDatabase(name) => {
            let tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            match shared_server.drop_database(&tenant, name) {
                Ok(()) => Response::Ok("OK".into()),
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Command::ListDatabases => {
            let tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            match shared_server.list_databases(&tenant) {
                Ok(dbs) => {
                    let items: Vec<Response> =
                        dbs.into_iter().map(|d| Response::Bulk(Some(d))).collect();
                    Response::Array(items)
                }
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Command::SetDatabaseConsistency {
            database,
            read_concern,
            write_concern,
        } => {
            let tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            match shared_server.set_database_consistency_defaults(
                &tenant,
                database,
                *read_concern,
                *write_concern,
            ) {
                Ok(()) => Response::Ok("OK".into()),
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Command::ResetDatabaseConsistency { database } => {
            let tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            match shared_server.reset_database_consistency_defaults(&tenant, database) {
                Ok(()) => Response::Ok("OK".into()),
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Command::ShowDatabaseConsistency { database } => {
            let tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            match shared_server.database_consistency_defaults(&tenant, database) {
                Ok((read_concern, write_concern)) => Response::Bulk(Some(
                    serde_json::json!({
                        "database": database,
                        "read_concern": read_concern,
                        "write_concern": write_concern,
                    })
                    .to_string(),
                )),
                Err(e) => Response::Error(e.to_string()),
            }
        }

        // ── Collection management ───────────────────────────────
        Command::CreateCollection(name) => with_db(session, shared_server, |db| {
            s(db.create_collection(name))?;
            Ok(Response::Ok("OK".into()))
        }),
        Command::DropCollection(name) => with_db(session, shared_server, |db| {
            s(db.drop_collection(name))?;
            Ok(Response::Ok("OK".into()))
        }),
        Command::ListCollections => with_db(session, shared_server, |db| {
            let cols = db.list_collections();
            let items: Vec<Response> = cols.into_iter().map(|c| Response::Bulk(Some(c))).collect();
            Ok(Response::Array(items))
        }),

        // ── CRUD ────────────────────────────────────────────────
        Command::Insert {
            collection,
            key,
            value,
        } => with_db(session, shared_server, |db| {
            if let Some(db_name) = session.current_db()
                && let Err(e) =
                    coordinator.enforce_local_write_replica(db_name, collection, key.as_bytes())
            {
                return Err(e);
            }
            let uuid = parse_uuid(key)?;
            let val = parse_json_value(value)?;
            s(db.insert(collection, uuid, val))?;
            Ok(Response::Ok("OK".into()))
        }),
        Command::Get { collection, key } => with_db(session, shared_server, |db| {
            if let Some(db_name) = session.current_db()
                && let Err(e) = coordinator.enforce_local_owner(db_name, collection, key.as_bytes())
            {
                return Err(e);
            }
            let uuid = parse_uuid(key)?;
            match s(db.get(collection, &uuid))? {
                Some(val) => Ok(Response::Bulk(Some(value_to_json(&val)))),
                None => Ok(Response::Bulk(None)),
            }
        }),
        Command::Update {
            collection,
            key,
            value,
        } => with_db(session, shared_server, |db| {
            if let Some(db_name) = session.current_db()
                && let Err(e) =
                    coordinator.enforce_local_write_replica(db_name, collection, key.as_bytes())
            {
                return Err(e);
            }
            let uuid = parse_uuid(key)?;
            let val = parse_json_value(value)?;
            s(db.update(collection, &uuid, val))?;
            Ok(Response::Ok("OK".into()))
        }),
        Command::Delete { collection, key } => with_db(session, shared_server, |db| {
            if let Some(db_name) = session.current_db()
                && let Err(e) =
                    coordinator.enforce_local_write_replica(db_name, collection, key.as_bytes())
            {
                return Err(e);
            }
            let uuid = parse_uuid(key)?;
            s(db.delete(collection, &uuid))?;
            Ok(Response::Ok("OK".into()))
        }),
        Command::PutWithVc {
            collection,
            key,
            value,
            vector_clock,
        } => with_db(session, shared_server, |db| {
            if let Some(db_name) = session.current_db()
                && let Err(e) =
                    coordinator.enforce_local_write_replica(db_name, collection, key.as_bytes())
            {
                return Err(e);
            }
            // v5 stores the reconciled value through the regular write path;
            // the vector clock is accepted at protocol level for v6 interop.
            let _: serde_json::Value = serde_json::from_str(vector_clock)
                .map_err(|e| format!("invalid vector_clock JSON: {e}"))?;
            let uuid = parse_uuid(key)?;
            let incoming = parse_json_value(value)?;
            if let Some(existing) = s(db.get(collection, &uuid))? {
                let reconciled = match (existing.is_crdt(), incoming.is_crdt()) {
                    (true, true) => merge_crdt_values(&existing, &incoming)
                        .map_err(|e| format!("CRDT merge failed: {e}"))?,
                    _ => incoming,
                };
                s(db.update(collection, &uuid, reconciled))?;
            } else {
                match db.insert(collection, uuid, incoming.clone()) {
                    Ok(()) => {}
                    Err(GrumpyError::DuplicateKey(_)) => {
                        // A hidden tombstone still occupies the key; update replaces it.
                        s(db.update(collection, &uuid, incoming))?;
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            Ok(Response::Ok("OK".into()))
        }),
        Command::Scan {
            collection,
            start,
            end,
        } => with_db(session, shared_server, |db| {
            let results = if let (Some(sv), Some(ev)) = (start, end) {
                let s_uuid = parse_uuid(sv)?;
                let e_uuid = parse_uuid(ev)?;
                s(db.scan(collection, s_uuid..=e_uuid))?
            } else {
                s(db.scan(collection, ..))?
            };
            let items: Vec<Response> = results
                .into_iter()
                .map(|(k, v)| Response::Bulk(Some(format!("{} {}", k, value_to_json(&v)))))
                .collect();
            Ok(Response::Array(items))
        }),
        Command::Count(collection) => with_db(session, shared_server, |db| {
            let count = s(db.document_count(collection))?;
            Ok(Response::Integer(count as i64))
        }),

        // ── Index management ────────────────────────────────────
        Command::CreateIndex {
            collection,
            index_name,
            field_path,
        } => with_db(session, shared_server, |db| {
            s(db.create_index(collection, index_name, field_path))?;
            Ok(Response::Ok("OK".into()))
        }),
        Command::DropIndex {
            collection,
            index_name,
        } => with_db(session, shared_server, |db| {
            s(db.drop_index(collection, index_name))?;
            Ok(Response::Ok("OK".into()))
        }),
        Command::ListIndexes(collection) => with_db(session, shared_server, |db| {
            let names = s(db.list_indexes(collection))?;
            let items: Vec<Response> = names.into_iter().map(|n| Response::Bulk(Some(n))).collect();
            Ok(Response::Array(items))
        }),
        Command::Query {
            collection,
            index_name,
            value,
        } => with_db(session, shared_server, |db| {
            let val = parse_json_value(value)?;
            match db.query(collection, index_name, &val) {
                Ok(results) => {
                    let items: Vec<Response> = results
                        .into_iter()
                        .map(|(k, v)| Response::Bulk(Some(format!("{} {}", k, value_to_json(&v)))))
                        .collect();
                    Ok(Response::Array(items))
                }
                Err(grumpydb::error::GrumpyError::IndexNotFound(_)) => Err(refine_index_not_found(
                    coordinator,
                    session,
                    collection,
                    index_name,
                )),
                Err(e) => Err(e.to_string()),
            }
        }),
        Command::QueryRange {
            collection,
            index_name,
            start,
            end,
        } => with_db(session, shared_server, |db| {
            let sv = parse_json_value(start)?;
            let ev = parse_json_value(end)?;
            match db.query_range(collection, index_name, &sv, &ev) {
                Ok(results) => {
                    let items: Vec<Response> = results
                        .into_iter()
                        .map(|(k, v)| Response::Bulk(Some(format!("{} {}", k, value_to_json(&v)))))
                        .collect();
                    Ok(Response::Array(items))
                }
                Err(grumpydb::error::GrumpyError::IndexNotFound(_)) => Err(refine_index_not_found(
                    coordinator,
                    session,
                    collection,
                    index_name,
                )),
                Err(e) => Err(e.to_string()),
            }
        }),

        // ── Maintenance ─────────────────────────────────────────
        Command::Compact(collection) => with_db(session, shared_server, |db| {
            let count = s(db.compact(collection))?;
            Ok(Response::Ok(format!("OK {count}")))
        }),
        Command::Flush => with_db(session, shared_server, |db| {
            s(db.flush())?;
            Ok(Response::Ok("OK".into()))
        }),

        // ── User management ─────────────────────────────────────
        // Username can be "tenant/user" for server_admin cross-tenant ops,
        // or plain "user" (uses session tenant).
        Command::CreateUser { username, password } => {
            let session_tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            let (target_tenant, target_user) = split_tenant_user(username, &session_tenant);
            let mut store = auth_store.write();
            match store.create_user(&target_tenant, &target_user, password, vec![]) {
                Ok(()) => Response::Ok("OK".into()),
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Command::DropUser(username) => {
            let session_tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            let (target_tenant, target_user) = split_tenant_user(username, &session_tenant);
            let mut store = auth_store.write();
            match store.delete_user(&target_tenant, &target_user) {
                Ok(()) => Response::Ok("OK".into()),
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Command::ListUsers(specifier) => {
            let session_tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            // Support "LIST USERS @acme" for cross-tenant listing
            let target_tenant = match specifier {
                Some(spec) if spec.starts_with('@') => spec[1..].to_string(),
                Some(spec) => spec.to_string(),
                None => session_tenant,
            };
            let store = auth_store.read();
            let users = store.list_users(&target_tenant);
            let items: Vec<Response> = users
                .into_iter()
                .map(|u| {
                    let roles: Vec<String> = u.roles.iter().map(|r| r.role.to_string()).collect();
                    Response::Bulk(Some(format!(
                        "{}@{}:{}",
                        u.username,
                        u.tenant,
                        roles.join(",")
                    )))
                })
                .collect();
            Response::Array(items)
        }
        Command::Grant {
            role,
            resource,
            username,
        } => {
            let session_tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            let (target_tenant, target_user) = split_tenant_user(username, &session_tenant);
            let role_name = match RoleName::from_str_name(role) {
                Some(r) => r,
                None => return Response::Error(format!("unknown role: {role}")),
            };
            let scope = parse_resource(resource, &session_tenant, session.current_db());
            let mut store = auth_store.write();
            let user = match store.get_user(&target_tenant, &target_user) {
                Some(u) => u.clone(),
                None => return Response::Error(format!("user not found: {username}")),
            };
            let mut roles = user.roles;
            roles.push(RoleAssignment {
                role: role_name,
                scope,
            });
            match store.update_roles(&target_tenant, &target_user, roles) {
                Ok(()) => Response::Ok("OK".into()),
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Command::Revoke {
            role,
            resource,
            username,
        } => {
            let session_tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            let (target_tenant, target_user) = split_tenant_user(username, &session_tenant);
            let role_name = match RoleName::from_str_name(role) {
                Some(r) => r,
                None => return Response::Error(format!("unknown role: {role}")),
            };
            let scope = parse_resource(resource, &session_tenant, session.current_db());
            let mut store = auth_store.write();
            let user = match store.get_user(&target_tenant, &target_user) {
                Some(u) => u.clone(),
                None => return Response::Error(format!("user not found: {username}")),
            };
            let roles: Vec<RoleAssignment> = user
                .roles
                .into_iter()
                .filter(|ra| !(ra.role == role_name && ra.scope == scope))
                .collect();
            match store.update_roles(&target_tenant, &target_user, roles) {
                Ok(()) => Response::Ok("OK".into()),
                Err(e) => Response::Error(e.to_string()),
            }
        }

        // ── Tenant management ───────────────────────────────────
        Command::CreateTenant(name) => match shared_server.create_client(name) {
            Ok(()) => Response::Ok("OK".into()),
            Err(e) => Response::Error(e.to_string()),
        },
        Command::DropTenant(name) => match shared_server.drop_client(name) {
            Ok(()) => Response::Ok("OK".into()),
            Err(e) => Response::Error(e.to_string()),
        },
        Command::ListTenants => {
            let clients = shared_server.list_clients();
            let items: Vec<Response> = clients
                .into_iter()
                .filter(|c| c != "_auth") // _auth is internal, not a tenant
                .map(|c| Response::Bulk(Some(c)))
                .collect();
            Response::Array(items)
        }

        // ── Cluster management ──────────────────────────────────────────
        Command::ElectWriter {
            node_id,
            database,
            collection,
        } => {
            // v5: accept the command and return OK. v6+ will enforce single-writer
            // constraints via RAFT coordination.
            tracing::info!(
                node_id = %node_id,
                database = %database,
                collection = collection.as_ref().map(|c| c.as_str()).unwrap_or("*"),
                "failover request (v5 in-memory only; persistence deferred to v6)"
            );
            Response::Ok(format!(
                "elected {} as writer for {}/{}",
                node_id,
                database,
                collection.as_ref().map(|c| c.as_str()).unwrap_or("*")
            ))
        }
        Command::PlanRebalanceAddNode { node_id } => Response::Bulk(Some(
            coordinator.plan_rebalance_add_node(node_id).to_string(),
        )),
        Command::PlanRebalanceRemoveNode { node_id } => Response::Bulk(Some(
            coordinator.plan_rebalance_remove_node(node_id).to_string(),
        )),
        Command::ExecuteRebalanceAddNode {
            node_id,
            collection,
        } => {
            let tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            let db_name = match session.current_db() {
                Some(db) => db.to_string(),
                None => return Response::Error("no database selected (use USE <db>)".into()),
            };
            let db = match shared_server.database(&tenant, &db_name) {
                Ok(db) => db,
                Err(e) => return Response::Error(e.to_string()),
            };
            let out = coordinator
                .execute_rebalance_add_node_transfer(node_id, &tenant, &db_name, collection, &db)
                .await;
            Response::Bulk(Some(out.to_string()))
        }
        Command::ExecuteRebalanceRemoveNode {
            node_id,
            collection,
        } => {
            let tenant = match session.tenant() {
                Ok(t) => t.to_string(),
                Err(e) => return Response::Error(e.to_string()),
            };
            let db_name = match session.current_db() {
                Some(db) => db.to_string(),
                None => return Response::Error("no database selected (use USE <db>)".into()),
            };
            let db = match shared_server.database(&tenant, &db_name) {
                Ok(db) => db,
                Err(e) => return Response::Error(e.to_string()),
            };
            let out = coordinator
                .execute_rebalance_remove_node_transfer(node_id, &tenant, &db_name, collection, &db)
                .await;
            Response::Bulk(Some(out.to_string()))
        }

        // Already handled above
        Command::Ping | Command::Quit => Response::Ok("OK".into()),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Phase 44d: refine the engine's `IndexNotFound` so callers can
/// distinguish "this index does not exist anywhere in the cluster"
/// from "the cluster schema knows about it but this node hasn't
/// finished materializing it yet". The latter is transient (gossip
/// converges in seconds) and should be retried.
fn refine_index_not_found(
    coordinator: &Coordinator,
    session: &SessionContext,
    collection: &str,
    index_name: &str,
) -> String {
    let tenant = session.tenant().map(|t| t.to_string()).unwrap_or_default();
    let db = session.current_db().unwrap_or("");

    if !tenant.is_empty()
        && !db.is_empty()
        && coordinator.schema_has_index(&tenant, db, collection, index_name)
    {
        format!(
            "index '{index_name}' is in cluster schema (version {}) but not yet materialized on this node; retry shortly",
            coordinator.schema_version()
        )
    } else {
        format!("index not found: {index_name}")
    }
}

/// Execute a closure with a SharedDatabase handle from the current session.
fn with_db<F>(session: &SessionContext, shared_server: &SharedServer, f: F) -> Response
where
    F: FnOnce(&grumpydb::concurrency::shared::SharedDatabase) -> Result<Response, String>,
{
    let tenant = match session.tenant() {
        Ok(t) => t.to_string(),
        Err(e) => return Response::Error(e.to_string()),
    };
    let db_name = match session.current_db() {
        Some(db) => db.to_string(),
        None => return Response::Error("no database selected (use USE <db>)".into()),
    };
    match shared_server.database(&tenant, &db_name) {
        Ok(db) => match f(&db) {
            Ok(resp) => resp,
            Err(e) => Response::Error(e),
        },
        Err(e) => Response::Error(e.to_string()),
    }
}

fn parse_uuid(s: &str) -> Result<Uuid, String> {
    Uuid::parse_str(s).map_err(|e| format!("invalid UUID '{s}': {e}"))
}

/// Split "user@tenant" into (tenant, user), or use default_tenant if no @.
fn split_tenant_user(input: &str, default_tenant: &str) -> (String, String) {
    if let Some((u, t)) = input.split_once('@') {
        (t.to_string(), u.to_string())
    } else {
        (default_tenant.to_string(), input.to_string())
    }
}

/// Parse a resource specifier into a `ResourceScope`.
///
/// Syntax: `[collection:][database][@tenant]`
///
/// | Input | Scope |
/// |-------|-------|
/// | `@acme` | Tenant "acme" |
/// | `mydb` | Database "mydb" (current tenant), or Collection if USE is active |
/// | `mydb@acme` | Database "mydb" in tenant "acme" |
/// | `users:mydb` | Collection "users" in database "mydb" (current tenant) |
/// | `users:mydb@acme` | Collection "users" in "mydb" of tenant "acme" |
///
/// Ambiguity rule for bare `name`: if a database is selected (via USE),
/// it's a **collection** in that database. Otherwise it's a **database**.
fn parse_resource(input: &str, session_tenant: &str, current_db: Option<&str>) -> ResourceScope {
    // Step 1: Split off @tenant suffix
    let (before_at, _tenant) = if let Some(at_pos) = input.rfind('@') {
        (&input[..at_pos], &input[at_pos + 1..])
    } else {
        (input, session_tenant)
    };

    // Step 2: "@tenant" alone → Tenant scope
    if before_at.is_empty() {
        return ResourceScope::Tenant;
    }

    // Step 3: Split on ":" for collection:database
    if let Some((collection, database)) = before_at.split_once(':') {
        return ResourceScope::Collection {
            database: database.to_string(),
            collection: collection.to_string(),
        };
    }

    // Step 4: Bare name — database or collection depending on context
    let name = before_at;
    if let Some(db) = current_db {
        // USE is active → it's a collection in the current database
        ResourceScope::Collection {
            database: db.to_string(),
            collection: name.to_string(),
        }
    } else {
        // No USE → it's a database
        ResourceScope::Database {
            name: name.to_string(),
        }
    }
}

/// Map any error to String for use in with_db closures.
fn s<T, E: std::fmt::Display>(r: Result<T, E>) -> Result<T, String> {
    r.map_err(|e| e.to_string())
}

/// Parse a JSON string into a GrumpyDB Value.
fn parse_json_value(s: &str) -> Result<Value, String> {
    let json: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("invalid JSON: {e}"))?;
    Ok(json_to_value(&json))
}

/// Convert serde_json::Value to grumpydb::Value.
fn json_to_value(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(arr) => Value::Array(arr.iter().map(json_to_value).collect()),
        serde_json::Value::Object(obj) => {
            if let Some(crdt_json) = obj.get("$crdt")
                && let Some(crdt_obj) = crdt_json.as_object()
            {
                let kind = crdt_obj
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .and_then(CrdtKind::from_name);
                let payload = crdt_obj
                    .get("payload_b64")
                    .and_then(|v| v.as_str())
                    .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok());
                if let (Some(kind), Some(payload)) = (kind, payload) {
                    return Value::Crdt { kind, payload };
                }
            }
            let mut map = BTreeMap::new();
            for (k, v) in obj {
                map.insert(k.clone(), json_to_value(v));
            }
            Value::Object(map)
        }
    }
}

/// Convert grumpydb::Value to JSON string.
fn value_to_json(val: &Value) -> String {
    let json = value_to_serde_json(val);
    serde_json::to_string(&json).unwrap_or_else(|_| "null".into())
}

fn value_to_serde_json(val: &Value) -> serde_json::Value {
    match val {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Integer(i) => serde_json::json!(i),
        Value::Float(f) => serde_json::json!(f),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Bytes(b) => serde_json::json!(format!("<{} bytes>", b.len())),
        Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(value_to_serde_json).collect())
        }
        Value::Object(obj) => {
            let map: serde_json::Map<String, serde_json::Value> = obj
                .iter()
                .map(|(k, v)| (k.clone(), value_to_serde_json(v)))
                .collect();
            serde_json::Value::Object(map)
        }
        Value::Ref(collection, uuid) => {
            serde_json::json!({"$ref": {"collection": collection, "uuid": uuid.to_string()}})
        }
        Value::Tombstone { deleted_at_hlc, .. } => {
            serde_json::json!({"$tombstone": {"hlc": deleted_at_hlc}})
        }
        Value::Crdt { kind, payload } => serde_json::json!({
            "$crdt": {
                "kind": kind.as_str(),
                "payload_b64": base64::engine::general_purpose::STANDARD.encode(payload)
            }
        }),
    }
}
