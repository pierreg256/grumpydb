# Skill: Wire Protocol (RESP-like)

## When to use this skill

When working on:
- `grumpydb-protocol/src/command.rs` — Command enum
- `grumpydb-protocol/src/response.rs` — Response enum, wire serialization
- `grumpydb-protocol/src/parser.rs` — command parser
- Any code that reads/writes the GrumpyDB wire protocol

## Core principles

### Protocol overview

GrumpyDB uses a text-based protocol inspired by Redis RESP (REdis Serialization Protocol), operating over TCP (optionally TLS-wrapped). All communication is line-based with `\r\n` terminators.

### Response types — exact format

| Type | Prefix | Format | Example |
|------|--------|--------|---------|
| Simple string | `+` | `+<message>\r\n` | `+OK\r\n` |
| Error | `-` | `-ERR <message>\r\n` | `-ERR key not found\r\n` |
| Integer | `:` | `:<integer>\r\n` | `:42\r\n` |
| Bulk string | `$` | `$<length>\r\n<data>\r\n` | `$5\r\nhello\r\n` |
| Null | `$` | `$-1\r\n` | `$-1\r\n` |
| Array | `*` | `*<count>\r\n<elements>` | `*2\r\n$3\r\nfoo\r\n$3\r\nbar\r\n` |
| Token | `+` | `+TOKEN <access> [<refresh>]\r\n` | `+TOKEN eyJ... eyJ...\r\n` |

### Command format — single line

```
VERB [args...]\r\n
```

Commands are **case-insensitive** for the verb, case-sensitive for arguments.
JSON values and UUIDs are passed as-is (not parsed by the protocol layer).

### Complete command reference

```
── Authentication ──────────────────────────────
LOGIN <tenant> <username> <password>
TOKEN <jwt>
REFRESH <refresh_jwt>
WHOAMI

── Session ─────────────────────────────────────
USE <database>
PING
QUIT

── Database management ─────────────────────────
CREATE DATABASE <name>
DROP DATABASE <name>
LIST DATABASES

── Collection management ───────────────────────
CREATE COLLECTION <name>
DROP COLLECTION <name>
LIST COLLECTIONS

── CRUD ────────────────────────────────────────
INSERT <collection> <uuid> <json>
GET <collection> <uuid>
UPDATE <collection> <uuid> <json>
DELETE <collection> <uuid>
SCAN <collection> [<start_uuid> <end_uuid>]

── Index management ────────────────────────────
CREATE INDEX <collection> <index_name> <field_path>
DROP INDEX <collection> <index_name>
LIST INDEXES <collection>
QUERY <collection> <index_name> <json_value>
QUERYRANGE <collection> <index_name> <start_json> <end_json>

── Maintenance ─────────────────────────────────
COMPACT <collection>
FLUSH
COUNT <collection>

── User management (tenant_admin+) ─────────────
CREATE USER <username> <password>
DROP USER <username>
LIST USERS
GRANT <role> ON <resource> TO <username>
REVOKE <role> ON <resource> FROM <username>

── Tenant management (server_admin) ────────────
CREATE TENANT <name>
DROP TENANT <name>
LIST TENANTS
```

### Safety limits

```rust
pub const DEFAULT_PORT: u16 = 6380;
pub const PROTOCOL_VERSION: &str = "4.0.0";
pub const MAX_LINE_LENGTH: usize = 1_048_576;    // 1 MiB
pub const MAX_BULK_LENGTH: usize = 16_777_216;   // 16 MiB
```

- Parser MUST reject lines longer than `MAX_LINE_LENGTH`
- Bulk strings MUST reject data longer than `MAX_BULK_LENGTH`
- These limits prevent denial-of-service via memory exhaustion

> **`PROTOCOL_VERSION` vs package version.** `PROTOCOL_VERSION` is the
> **wire** version advertised in the connection banner
> (`+GRUMPYDB <PROTOCOL_VERSION>\r\n`). It is **not** the workspace /
> crate version (which evolves at every release — `5.1.x`, `5.2.x`, …).
>
> The wire version bumps **only** on breaking wire changes (incompatible
> command grammar, removed verbs, response shape changes that would
> break a permissive driver). Additive changes ship without a bump:
> - New commands a v4 driver will simply never send (e.g. `SCHEMA
>   VERSION`, `SCHEMA STATUS` shipped in v5 phase 44d).
> - New rows with reserved prefixes inside an existing
>   `Response::Array` (e.g. the `_warning convergence: …` sentinel
>   shipped in v5 phase 44f). Drivers MUST ignore unrecognized rows
>   whose first byte is `_`.
>
> When a sync agent flags a "version mismatch" between this skill and
> the workspace `Cargo.toml` package version, it is almost always a
> false positive — the two are intentionally decoupled.

### Command → Action + Resource mapping

Every command maps to an `Action` (what it does) and a `Resource` (what it targets).
This mapping drives RBAC enforcement.

```rust
impl Command {
    pub fn required_action(&self) -> Action {
        match self {
            Command::Get { .. } | Command::Scan { .. } | Command::Query { .. }
            | Command::QueryRange { .. } | Command::Count { .. } => Action::Read,

            Command::Insert { .. } | Command::Update { .. }
            | Command::Delete { .. } => Action::Write,

            Command::CreateCollection(_) | Command::DropCollection(_)
            | Command::CreateIndex { .. } | Command::DropIndex { .. }
            | Command::Compact(_) | Command::Flush | Command::ListCollections
            | Command::ListIndexes(_) => Action::Admin,

            Command::CreateDatabase(_) | Command::DropDatabase(_)
            | Command::ListDatabases => Action::ManageDatabases,

            Command::CreateUser { .. } | Command::DropUser(_)
            | Command::ListUsers | Command::Grant { .. }
            | Command::Revoke { .. } => Action::ManageUsers,

            Command::CreateTenant(_) | Command::DropTenant(_)
            | Command::ListTenants => Action::ManageServer,

            // Session commands (no RBAC check needed)
            Command::Login { .. } | Command::Token(_) | Command::Refresh(_)
            | Command::WhoAmI | Command::Use(_) | Command::Ping
            | Command::Quit => Action::Read, // placeholder, bypassed
        }
    }
}
```

### Parser design rules

1. **Split on first space** to extract the verb (handle multi-word verbs: `CREATE DATABASE`, `LIST USERS`, etc.)
2. **JSON values**: everything after the last named argument is the JSON value (don't try to parse JSON — just capture the raw string)
3. **UUID format**: any string that matches `[0-9a-f-]{36}` is treated as a UUID argument
4. **Error reporting**: include the position/token that failed in the error message
5. **No allocation** for simple commands (LOGIN, PING, QUIT) — return parsed enum directly

### Serialization rules

- Always terminate with `\r\n`
- Bulk string length is byte length (not character length)
- Arrays are recursive: array elements can be any response type
- Null bulk string (`$-1\r\n`) represents missing values (e.g., GET on nonexistent key)

### Cluster sentinel rows in arrays (Phase 44f)

Verified `QUERY` / `QUERYRANGE` arrays with effective `R≥2` may include
a trailing sentinel entry whose first byte is `_`:

```text
_warning convergence: 2 peer(s) not yet materialized: [nodeB,nodeE]
```

Real QUERY result rows are formatted as `<uuid> {json}` and UUIDs never
start with `_`, so the sentinel is wire-distinguishable. Parsers MUST
treat it as a regular bulk string and must NOT reject the response.
Smart drivers SHOULD surface (or filter) any array entry that begins
with a leading `_` as a convergence warning, never as a document. The
related acceptor-side error string is
`index_not_yet_materialized:<index_name>` (exposed in the server crate
as `INDEX_NOT_YET_MATERIALIZED_PREFIX`).

## Common mistakes to avoid

1. **Forgetting `\r\n`** — every response line must end with CRLF
2. **Byte vs character length** in bulk strings — use `.len()` (bytes), not `.chars().count()`
3. **Parsing JSON as part of the protocol** — the protocol layer treats JSON as opaque strings
4. **Case-sensitive verb matching** — verbs must be case-insensitive (`insert` == `INSERT`)
5. **Multi-word commands** — `CREATE DATABASE` is one command, not `CREATE` with arg `DATABASE`
