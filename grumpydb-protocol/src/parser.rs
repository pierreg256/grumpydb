//! Command parser for the GrumpyDB wire protocol.
//!
//! Parses single-line text commands into [`Command`] variants.
//! Commands are case-insensitive for the verb, case-sensitive for arguments.

use crate::command::Command;
use crate::MAX_LINE_LENGTH;

/// Error during command parsing.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProtocolError {
    #[error("empty command")]
    Empty,
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    #[error("missing argument: {0}")]
    MissingArgument(String),
    #[error("line too long ({0} bytes, max {MAX_LINE_LENGTH})")]
    LineTooLong(usize),
}

/// Parse a single command line into a [`Command`].
///
/// The line may or may not include a trailing `\r\n`.
pub fn parse_command(line: &str) -> Result<Command, ProtocolError> {
    let line = line.trim_end_matches("\r\n").trim_end_matches('\n');

    if line.len() > MAX_LINE_LENGTH {
        return Err(ProtocolError::LineTooLong(line.len()));
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(ProtocolError::Empty);
    }

    // Split into verb + rest
    let (verb, rest) = split_first_word(trimmed);
    let verb_upper = verb.to_ascii_uppercase();

    match verb_upper.as_str() {
        // ── Authentication ──────────────────────────────────────────
        "LOGIN" => parse_login(rest),
        "TOKEN" => {
            let token = require_arg(rest, "TOKEN", "jwt")?;
            Ok(Command::Token(token.to_string()))
        }
        "REFRESH" => {
            let token = require_arg(rest, "REFRESH", "refresh_token")?;
            Ok(Command::Refresh(token.to_string()))
        }
        "WHOAMI" => Ok(Command::WhoAmI),

        // ── Session ─────────────────────────────────────────────────
        "USE" => {
            let db = require_arg(rest, "USE", "database")?;
            Ok(Command::Use(db.to_string()))
        }
        "PING" => Ok(Command::Ping),
        "QUIT" => Ok(Command::Quit),

        // ── Multi-word verbs ────────────────────────────────────────
        "CREATE" => parse_create(rest),
        "DROP" => parse_drop(rest),
        "LIST" => parse_list(rest),
        "GRANT" => parse_grant(rest),
        "REVOKE" => parse_revoke(rest),

        // ── CRUD ────────────────────────────────────────────────────
        "INSERT" => parse_insert(rest),
        "GET" => parse_get(rest),
        "UPDATE" => parse_update(rest),
        "DELETE" => parse_delete(rest),
        "SCAN" => parse_scan(rest),

        // ── Index queries ───────────────────────────────────────────
        "QUERY" => parse_query(rest),
        "QUERYRANGE" => parse_queryrange(rest),

        // ── Maintenance ─────────────────────────────────────────────
        "COMPACT" => {
            let coll = require_arg(rest, "COMPACT", "collection")?;
            Ok(Command::Compact(coll.to_string()))
        }
        "FLUSH" => Ok(Command::Flush),
        "COUNT" => {
            let coll = require_arg(rest, "COUNT", "collection")?;
            Ok(Command::Count(coll.to_string()))
        }

        _ => Err(ProtocolError::UnknownCommand(verb.to_string())),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Split on the first whitespace. Returns (word, rest).
fn split_first_word(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(pos) => (&s[..pos], s[pos..].trim_start()),
        None => (s, ""),
    }
}

/// Require a non-empty argument.
fn require_arg<'a>(
    rest: &'a str,
    command: &str,
    arg_name: &str,
) -> Result<&'a str, ProtocolError> {
    if rest.is_empty() {
        Err(ProtocolError::MissingArgument(format!(
            "{command} requires <{arg_name}>"
        )))
    } else {
        Ok(rest)
    }
}

// ── LOGIN ───────────────────────────────────────────────────────────────

fn parse_login(rest: &str) -> Result<Command, ProtocolError> {
    let (tenant, rest) = split_first_word(rest);
    if tenant.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "LOGIN requires <tenant> <username> <password>".into(),
        ));
    }
    let (username, rest) = split_first_word(rest);
    if username.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "LOGIN requires <username>".into(),
        ));
    }
    if rest.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "LOGIN requires <password>".into(),
        ));
    }
    Ok(Command::Login {
        tenant: tenant.to_string(),
        username: username.to_string(),
        password: rest.to_string(),
    })
}

// ── CREATE ──────────────────────────────────────────────────────────────

fn parse_create(rest: &str) -> Result<Command, ProtocolError> {
    let (sub, rest) = split_first_word(rest);
    match sub.to_ascii_uppercase().as_str() {
        "DATABASE" => {
            let name = require_arg(rest, "CREATE DATABASE", "name")?;
            Ok(Command::CreateDatabase(name.to_string()))
        }
        "COLLECTION" => {
            let name = require_arg(rest, "CREATE COLLECTION", "name")?;
            Ok(Command::CreateCollection(name.to_string()))
        }
        "INDEX" => parse_create_index(rest),
        "USER" => parse_create_user(rest),
        "TENANT" => {
            let name = require_arg(rest, "CREATE TENANT", "name")?;
            Ok(Command::CreateTenant(name.to_string()))
        }
        "" => Err(ProtocolError::MissingArgument(
            "CREATE requires DATABASE|COLLECTION|INDEX|USER|TENANT".into(),
        )),
        _ => Err(ProtocolError::UnknownCommand(format!("CREATE {sub}"))),
    }
}

fn parse_create_index(rest: &str) -> Result<Command, ProtocolError> {
    let (collection, rest) = split_first_word(rest);
    if collection.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "CREATE INDEX requires <collection> <index_name> <field_path>".into(),
        ));
    }
    let (index_name, rest) = split_first_word(rest);
    if index_name.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "CREATE INDEX requires <index_name>".into(),
        ));
    }
    let field_path = require_arg(rest, "CREATE INDEX", "field_path")?;
    Ok(Command::CreateIndex {
        collection: collection.to_string(),
        index_name: index_name.to_string(),
        field_path: field_path.to_string(),
    })
}

fn parse_create_user(rest: &str) -> Result<Command, ProtocolError> {
    let (username, rest) = split_first_word(rest);
    if username.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "CREATE USER requires <username> <password>".into(),
        ));
    }
    if rest.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "CREATE USER requires <password>".into(),
        ));
    }
    Ok(Command::CreateUser {
        username: username.to_string(),
        password: rest.to_string(),
    })
}

// ── DROP ────────────────────────────────────────────────────────────────

fn parse_drop(rest: &str) -> Result<Command, ProtocolError> {
    let (sub, rest) = split_first_word(rest);
    match sub.to_ascii_uppercase().as_str() {
        "DATABASE" => {
            let name = require_arg(rest, "DROP DATABASE", "name")?;
            Ok(Command::DropDatabase(name.to_string()))
        }
        "COLLECTION" => {
            let name = require_arg(rest, "DROP COLLECTION", "name")?;
            Ok(Command::DropCollection(name.to_string()))
        }
        "INDEX" => {
            let (collection, rest) = split_first_word(rest);
            if collection.is_empty() {
                return Err(ProtocolError::MissingArgument(
                    "DROP INDEX requires <collection> <index_name>".into(),
                ));
            }
            let index_name = require_arg(rest, "DROP INDEX", "index_name")?;
            Ok(Command::DropIndex {
                collection: collection.to_string(),
                index_name: index_name.to_string(),
            })
        }
        "USER" => {
            let name = require_arg(rest, "DROP USER", "username")?;
            Ok(Command::DropUser(name.to_string()))
        }
        "TENANT" => {
            let name = require_arg(rest, "DROP TENANT", "name")?;
            Ok(Command::DropTenant(name.to_string()))
        }
        "" => Err(ProtocolError::MissingArgument(
            "DROP requires DATABASE|COLLECTION|INDEX|USER|TENANT".into(),
        )),
        _ => Err(ProtocolError::UnknownCommand(format!("DROP {sub}"))),
    }
}

// ── LIST ────────────────────────────────────────────────────────────────

fn parse_list(rest: &str) -> Result<Command, ProtocolError> {
    let (sub, rest) = split_first_word(rest);
    match sub.to_ascii_uppercase().as_str() {
        "DATABASES" => Ok(Command::ListDatabases),
        "COLLECTIONS" => Ok(Command::ListCollections),
        "INDEXES" => {
            let coll = require_arg(rest, "LIST INDEXES", "collection")?;
            Ok(Command::ListIndexes(coll.to_string()))
        }
        "USERS" => {
            if rest.is_empty() {
                Ok(Command::ListUsers(None))
            } else {
                Ok(Command::ListUsers(Some(rest.to_string())))
            }
        }
        "TENANTS" => Ok(Command::ListTenants),
        "" => Err(ProtocolError::MissingArgument(
            "LIST requires DATABASES|COLLECTIONS|INDEXES|USERS|TENANTS".into(),
        )),
        _ => Err(ProtocolError::UnknownCommand(format!("LIST {sub}"))),
    }
}

// ── CRUD ────────────────────────────────────────────────────────────────

fn parse_insert(rest: &str) -> Result<Command, ProtocolError> {
    let (collection, rest) = split_first_word(rest);
    if collection.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "INSERT requires <collection> <uuid> <json>".into(),
        ));
    }
    let (key, rest) = split_first_word(rest);
    if key.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "INSERT requires <uuid>".into(),
        ));
    }
    if rest.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "INSERT requires <json>".into(),
        ));
    }
    Ok(Command::Insert {
        collection: collection.to_string(),
        key: key.to_string(),
        value: rest.to_string(),
    })
}

fn parse_get(rest: &str) -> Result<Command, ProtocolError> {
    let (collection, rest) = split_first_word(rest);
    if collection.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "GET requires <collection> <uuid>".into(),
        ));
    }
    let key = require_arg(rest, "GET", "uuid")?;
    let (key, _) = split_first_word(key);
    Ok(Command::Get {
        collection: collection.to_string(),
        key: key.to_string(),
    })
}

fn parse_update(rest: &str) -> Result<Command, ProtocolError> {
    let (collection, rest) = split_first_word(rest);
    if collection.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "UPDATE requires <collection> <uuid> <json>".into(),
        ));
    }
    let (key, rest) = split_first_word(rest);
    if key.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "UPDATE requires <uuid>".into(),
        ));
    }
    if rest.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "UPDATE requires <json>".into(),
        ));
    }
    Ok(Command::Update {
        collection: collection.to_string(),
        key: key.to_string(),
        value: rest.to_string(),
    })
}

fn parse_delete(rest: &str) -> Result<Command, ProtocolError> {
    let (collection, rest) = split_first_word(rest);
    if collection.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "DELETE requires <collection> <uuid>".into(),
        ));
    }
    let key = require_arg(rest, "DELETE", "uuid")?;
    let (key, _) = split_first_word(key);
    Ok(Command::Delete {
        collection: collection.to_string(),
        key: key.to_string(),
    })
}

fn parse_scan(rest: &str) -> Result<Command, ProtocolError> {
    let (collection, rest) = split_first_word(rest);
    if collection.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "SCAN requires <collection>".into(),
        ));
    }
    if rest.is_empty() {
        return Ok(Command::Scan {
            collection: collection.to_string(),
            start: None,
            end: None,
        });
    }
    let (start, rest) = split_first_word(rest);
    if rest.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "SCAN range requires both <start_uuid> <end_uuid>".into(),
        ));
    }
    let (end, _) = split_first_word(rest);
    Ok(Command::Scan {
        collection: collection.to_string(),
        start: Some(start.to_string()),
        end: Some(end.to_string()),
    })
}

// ── Index queries ───────────────────────────────────────────────────────

fn parse_query(rest: &str) -> Result<Command, ProtocolError> {
    let (collection, rest) = split_first_word(rest);
    if collection.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "QUERY requires <collection> <index_name> <json_value>".into(),
        ));
    }
    let (index_name, rest) = split_first_word(rest);
    if index_name.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "QUERY requires <index_name>".into(),
        ));
    }
    if rest.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "QUERY requires <json_value>".into(),
        ));
    }
    Ok(Command::Query {
        collection: collection.to_string(),
        index_name: index_name.to_string(),
        value: rest.to_string(),
    })
}

fn parse_queryrange(rest: &str) -> Result<Command, ProtocolError> {
    let (collection, rest) = split_first_word(rest);
    if collection.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "QUERYRANGE requires <collection> <index_name> <start> <end>".into(),
        ));
    }
    let (index_name, rest) = split_first_word(rest);
    if index_name.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "QUERYRANGE requires <index_name>".into(),
        ));
    }
    // For QUERYRANGE, the start and end are JSON values that may contain spaces.
    // We split on the first space to get start, rest is end.
    let (start, rest) = split_first_word(rest);
    if start.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "QUERYRANGE requires <start>".into(),
        ));
    }
    if rest.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "QUERYRANGE requires <end>".into(),
        ));
    }
    Ok(Command::QueryRange {
        collection: collection.to_string(),
        index_name: index_name.to_string(),
        start: start.to_string(),
        end: rest.to_string(),
    })
}

// ── GRANT / REVOKE ──────────────────────────────────────────────────────

fn parse_grant(rest: &str) -> Result<Command, ProtocolError> {
    // GRANT <role> ON <resource> TO <username>
    let (role, rest) = split_first_word(rest);
    if role.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "GRANT requires <role> ON <resource> TO <username>".into(),
        ));
    }
    let (on_kw, rest) = split_first_word(rest);
    if !on_kw.eq_ignore_ascii_case("ON") {
        return Err(ProtocolError::MissingArgument(
            "GRANT syntax: GRANT <role> ON <resource> TO <username>".into(),
        ));
    }
    let (resource, rest) = split_first_word(rest);
    if resource.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "GRANT requires <resource>".into(),
        ));
    }
    let (to_kw, rest) = split_first_word(rest);
    if !to_kw.eq_ignore_ascii_case("TO") {
        return Err(ProtocolError::MissingArgument(
            "GRANT syntax: GRANT <role> ON <resource> TO <username>".into(),
        ));
    }
    let username = require_arg(rest, "GRANT", "username")?;
    let (username, _) = split_first_word(username);
    Ok(Command::Grant {
        role: role.to_string(),
        resource: resource.to_string(),
        username: username.to_string(),
    })
}

fn parse_revoke(rest: &str) -> Result<Command, ProtocolError> {
    // REVOKE <role> ON <resource> FROM <username>
    let (role, rest) = split_first_word(rest);
    if role.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "REVOKE requires <role> ON <resource> FROM <username>".into(),
        ));
    }
    let (on_kw, rest) = split_first_word(rest);
    if !on_kw.eq_ignore_ascii_case("ON") {
        return Err(ProtocolError::MissingArgument(
            "REVOKE syntax: REVOKE <role> ON <resource> FROM <username>".into(),
        ));
    }
    let (resource, rest) = split_first_word(rest);
    if resource.is_empty() {
        return Err(ProtocolError::MissingArgument(
            "REVOKE requires <resource>".into(),
        ));
    }
    let (from_kw, rest) = split_first_word(rest);
    if !from_kw.eq_ignore_ascii_case("FROM") {
        return Err(ProtocolError::MissingArgument(
            "REVOKE syntax: REVOKE <role> ON <resource> FROM <username>".into(),
        ));
    }
    let username = require_arg(rest, "REVOKE", "username")?;
    let (username, _) = split_first_word(username);
    Ok(Command::Revoke {
        role: role.to_string(),
        resource: resource.to_string(),
        username: username.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Session commands ────────────────────────────────────────────

    #[test]
    fn test_parse_ping() {
        assert_eq!(parse_command("PING\r\n").unwrap(), Command::Ping);
        assert_eq!(parse_command("ping").unwrap(), Command::Ping);
    }

    #[test]
    fn test_parse_quit() {
        assert_eq!(parse_command("QUIT\r\n").unwrap(), Command::Quit);
    }

    #[test]
    fn test_parse_use() {
        assert_eq!(
            parse_command("USE mydb\r\n").unwrap(),
            Command::Use("mydb".into())
        );
    }

    #[test]
    fn test_parse_whoami() {
        assert_eq!(parse_command("WHOAMI\r\n").unwrap(), Command::WhoAmI);
    }

    // ── Auth commands ───────────────────────────────────────────────

    #[test]
    fn test_parse_login() {
        assert_eq!(
            parse_command("LOGIN acme alice s3cr3t\r\n").unwrap(),
            Command::Login {
                tenant: "acme".into(),
                username: "alice".into(),
                password: "s3cr3t".into(),
            }
        );
    }

    #[test]
    fn test_parse_login_password_with_spaces() {
        assert_eq!(
            parse_command("LOGIN acme alice my secret pass\r\n").unwrap(),
            Command::Login {
                tenant: "acme".into(),
                username: "alice".into(),
                password: "my secret pass".into(),
            }
        );
    }

    #[test]
    fn test_parse_login_missing_args() {
        assert!(parse_command("LOGIN\r\n").is_err());
        assert!(parse_command("LOGIN acme\r\n").is_err());
        assert!(parse_command("LOGIN acme alice\r\n").is_err());
    }

    #[test]
    fn test_parse_token() {
        assert_eq!(
            parse_command("TOKEN eyJhbGciOiJIUzI1NiJ9.xxx.yyy\r\n").unwrap(),
            Command::Token("eyJhbGciOiJIUzI1NiJ9.xxx.yyy".into())
        );
    }

    #[test]
    fn test_parse_refresh() {
        assert_eq!(
            parse_command("REFRESH eyJ.xxx\r\n").unwrap(),
            Command::Refresh("eyJ.xxx".into())
        );
    }

    // ── CRUD commands ───────────────────────────────────────────────

    #[test]
    fn test_parse_insert() {
        assert_eq!(
            parse_command("INSERT users a3b4c5d6-1234-5678-9abc-def012345678 {\"name\":\"bob\"}\r\n").unwrap(),
            Command::Insert {
                collection: "users".into(),
                key: "a3b4c5d6-1234-5678-9abc-def012345678".into(),
                value: "{\"name\":\"bob\"}".into(),
            }
        );
    }

    #[test]
    fn test_parse_insert_json_with_spaces() {
        assert_eq!(
            parse_command("INSERT users abc123 { \"name\": \"bob\" }\r\n").unwrap(),
            Command::Insert {
                collection: "users".into(),
                key: "abc123".into(),
                value: "{ \"name\": \"bob\" }".into(),
            }
        );
    }

    #[test]
    fn test_parse_get() {
        assert_eq!(
            parse_command("GET users abc123\r\n").unwrap(),
            Command::Get {
                collection: "users".into(),
                key: "abc123".into(),
            }
        );
    }

    #[test]
    fn test_parse_update() {
        assert_eq!(
            parse_command("UPDATE users abc123 {\"age\":31}\r\n").unwrap(),
            Command::Update {
                collection: "users".into(),
                key: "abc123".into(),
                value: "{\"age\":31}".into(),
            }
        );
    }

    #[test]
    fn test_parse_delete() {
        assert_eq!(
            parse_command("DELETE users abc123\r\n").unwrap(),
            Command::Delete {
                collection: "users".into(),
                key: "abc123".into(),
            }
        );
    }

    #[test]
    fn test_parse_scan_no_range() {
        assert_eq!(
            parse_command("SCAN users\r\n").unwrap(),
            Command::Scan {
                collection: "users".into(),
                start: None,
                end: None,
            }
        );
    }

    #[test]
    fn test_parse_scan_with_range() {
        assert_eq!(
            parse_command("SCAN users start-uuid end-uuid\r\n").unwrap(),
            Command::Scan {
                collection: "users".into(),
                start: Some("start-uuid".into()),
                end: Some("end-uuid".into()),
            }
        );
    }

    #[test]
    fn test_parse_count() {
        assert_eq!(
            parse_command("COUNT users\r\n").unwrap(),
            Command::Count("users".into())
        );
    }

    // ── Database/Collection management ──────────────────────────────

    #[test]
    fn test_parse_create_database() {
        assert_eq!(
            parse_command("CREATE DATABASE staging\r\n").unwrap(),
            Command::CreateDatabase("staging".into())
        );
    }

    #[test]
    fn test_parse_drop_database() {
        assert_eq!(
            parse_command("DROP DATABASE staging\r\n").unwrap(),
            Command::DropDatabase("staging".into())
        );
    }

    #[test]
    fn test_parse_list_databases() {
        assert_eq!(
            parse_command("LIST DATABASES\r\n").unwrap(),
            Command::ListDatabases
        );
    }

    #[test]
    fn test_parse_create_collection() {
        assert_eq!(
            parse_command("CREATE COLLECTION users\r\n").unwrap(),
            Command::CreateCollection("users".into())
        );
    }

    #[test]
    fn test_parse_drop_collection() {
        assert_eq!(
            parse_command("DROP COLLECTION users\r\n").unwrap(),
            Command::DropCollection("users".into())
        );
    }

    #[test]
    fn test_parse_list_collections() {
        assert_eq!(
            parse_command("LIST COLLECTIONS\r\n").unwrap(),
            Command::ListCollections
        );
    }

    // ── Index commands ──────────────────────────────────────────────

    #[test]
    fn test_parse_create_index() {
        assert_eq!(
            parse_command("CREATE INDEX users idx_email email\r\n").unwrap(),
            Command::CreateIndex {
                collection: "users".into(),
                index_name: "idx_email".into(),
                field_path: "email".into(),
            }
        );
    }

    #[test]
    fn test_parse_drop_index() {
        assert_eq!(
            parse_command("DROP INDEX users idx_email\r\n").unwrap(),
            Command::DropIndex {
                collection: "users".into(),
                index_name: "idx_email".into(),
            }
        );
    }

    #[test]
    fn test_parse_list_indexes() {
        assert_eq!(
            parse_command("LIST INDEXES users\r\n").unwrap(),
            Command::ListIndexes("users".into())
        );
    }

    #[test]
    fn test_parse_query() {
        assert_eq!(
            parse_command("QUERY users idx_name \"bob\"\r\n").unwrap(),
            Command::Query {
                collection: "users".into(),
                index_name: "idx_name".into(),
                value: "\"bob\"".into(),
            }
        );
    }

    #[test]
    fn test_parse_queryrange() {
        assert_eq!(
            parse_command("QUERYRANGE users idx_age 20 30\r\n").unwrap(),
            Command::QueryRange {
                collection: "users".into(),
                index_name: "idx_age".into(),
                start: "20".into(),
                end: "30".into(),
            }
        );
    }

    // ── Maintenance ─────────────────────────────────────────────────

    #[test]
    fn test_parse_compact() {
        assert_eq!(
            parse_command("COMPACT users\r\n").unwrap(),
            Command::Compact("users".into())
        );
    }

    #[test]
    fn test_parse_flush() {
        assert_eq!(parse_command("FLUSH\r\n").unwrap(), Command::Flush);
    }

    // ── User management ─────────────────────────────────────────────

    #[test]
    fn test_parse_create_user() {
        assert_eq!(
            parse_command("CREATE USER bob s3cr3t\r\n").unwrap(),
            Command::CreateUser {
                username: "bob".into(),
                password: "s3cr3t".into(),
            }
        );
    }

    #[test]
    fn test_parse_drop_user() {
        assert_eq!(
            parse_command("DROP USER bob\r\n").unwrap(),
            Command::DropUser("bob".into())
        );
    }

    #[test]
    fn test_parse_list_users() {
        assert_eq!(
            parse_command("LIST USERS\r\n").unwrap(),
            Command::ListUsers(None)
        );
        assert_eq!(
            parse_command("LIST USERS @acme\r\n").unwrap(),
            Command::ListUsers(Some("@acme".into()))
        );
    }

    #[test]
    fn test_parse_grant() {
        assert_eq!(
            parse_command("GRANT read_write ON mydb TO bob\r\n").unwrap(),
            Command::Grant {
                role: "read_write".into(),
                resource: "mydb".into(),
                username: "bob".into(),
            }
        );
    }

    #[test]
    fn test_parse_revoke() {
        assert_eq!(
            parse_command("REVOKE read_write ON mydb FROM bob\r\n").unwrap(),
            Command::Revoke {
                role: "read_write".into(),
                resource: "mydb".into(),
                username: "bob".into(),
            }
        );
    }

    // ── Tenant management ───────────────────────────────────────────

    #[test]
    fn test_parse_create_tenant() {
        assert_eq!(
            parse_command("CREATE TENANT acme\r\n").unwrap(),
            Command::CreateTenant("acme".into())
        );
    }

    #[test]
    fn test_parse_drop_tenant() {
        assert_eq!(
            parse_command("DROP TENANT acme\r\n").unwrap(),
            Command::DropTenant("acme".into())
        );
    }

    #[test]
    fn test_parse_list_tenants() {
        assert_eq!(
            parse_command("LIST TENANTS\r\n").unwrap(),
            Command::ListTenants
        );
    }

    // ── Error cases ─────────────────────────────────────────────────

    #[test]
    fn test_parse_empty() {
        assert!(matches!(
            parse_command("\r\n"),
            Err(ProtocolError::Empty)
        ));
        assert!(matches!(parse_command(""), Err(ProtocolError::Empty)));
    }

    #[test]
    fn test_parse_unknown_command() {
        assert!(matches!(
            parse_command("FOOBAR\r\n"),
            Err(ProtocolError::UnknownCommand(_))
        ));
    }

    #[test]
    fn test_parse_missing_args() {
        assert!(parse_command("INSERT\r\n").is_err());
        assert!(parse_command("INSERT users\r\n").is_err());
        assert!(parse_command("INSERT users key\r\n").is_err());
        assert!(parse_command("GET\r\n").is_err());
        assert!(parse_command("GET users\r\n").is_err());
        assert!(parse_command("USE\r\n").is_err());
    }

    #[test]
    fn test_parse_case_insensitive() {
        assert_eq!(parse_command("ping\r\n").unwrap(), Command::Ping);
        assert_eq!(parse_command("Ping\r\n").unwrap(), Command::Ping);
        assert_eq!(parse_command("PING\r\n").unwrap(), Command::Ping);
        assert_eq!(
            parse_command("create database mydb\r\n").unwrap(),
            Command::CreateDatabase("mydb".into())
        );
    }

    #[test]
    fn test_parse_grant_bad_syntax() {
        assert!(parse_command("GRANT read_write mydb bob\r\n").is_err());
        assert!(parse_command("GRANT\r\n").is_err());
    }

    #[test]
    fn test_parse_revoke_bad_syntax() {
        assert!(parse_command("REVOKE read_write mydb bob\r\n").is_err());
    }
}
