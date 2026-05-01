//! Per-connection handler: read commands, authorize, execute, respond.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

use grumpydb::concurrency::shared::SharedServer;
use grumpydb::document::value::Value;
use grumpydb_protocol::{Command, MAX_LINE_LENGTH, PROTOCOL_VERSION, Response, parse_command};

use crate::auth::role::{ResourceScope, RoleAssignment, RoleName};
use crate::auth::store::AuthStore;
use crate::coordinator::Coordinator;
use crate::limits::Limits;
use crate::session::SessionContext;

/// Handle a single client connection (plaintext or TLS).
pub async fn handle_connection<S>(
    stream: S,
    peer: SocketAddr,
    auth_store: Arc<parking_lot::RwLock<AuthStore>>,
    shared_server: SharedServer,
    limits: Arc<Limits>,
    coordinator: Arc<Coordinator>,
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

        if let Err(e) = validate_write_concern_runtime(&command, &session, coordinator.as_ref()) {
            tracing::warn!(error = %e, "write concern validation failed");
            write_resp(&mut writer, &Response::Error(e)).await?;
            continue;
        }

        // Execute (with panic isolation: a corrupt page or bug in the engine
        // must not tear down the entire server. Surface as Corruption error.)
        let started = std::time::Instant::now();
        let response = AssertUnwindSafe(execute_command(
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
        Command::Use(_) => "USE",
        Command::Ping => "PING",
        Command::Quit => "QUIT",
        Command::CreateDatabase(_) => "CREATE_DATABASE",
        Command::DropDatabase(_) => "DROP_DATABASE",
        Command::ListDatabases => "LIST_DATABASES",
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

fn validate_write_concern_runtime(
    command: &Command,
    session: &SessionContext,
    coordinator: &Coordinator,
) -> Result<(), String> {
    let (_, write_concern) = consistency_values(command);
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
            let val = parse_json_value(value)?;
            if s(db.get(collection, &uuid))?.is_some() {
                s(db.update(collection, &uuid, val))?;
            } else {
                s(db.insert(collection, uuid, val))?;
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
            let results = s(db.query(collection, index_name, &val))?;
            let items: Vec<Response> = results
                .into_iter()
                .map(|(k, v)| Response::Bulk(Some(format!("{} {}", k, value_to_json(&v)))))
                .collect();
            Ok(Response::Array(items))
        }),
        Command::QueryRange {
            collection,
            index_name,
            start,
            end,
        } => with_db(session, shared_server, |db| {
            let sv = parse_json_value(start)?;
            let ev = parse_json_value(end)?;
            let results = s(db.query_range(collection, index_name, &sv, &ev))?;
            let items: Vec<Response> = results
                .into_iter()
                .map(|(k, v)| Response::Bulk(Some(format!("{} {}", k, value_to_json(&v)))))
                .collect();
            Ok(Response::Array(items))
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

        // Already handled above
        Command::Ping | Command::Quit => Response::Ok("OK".into()),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

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
    }
}
