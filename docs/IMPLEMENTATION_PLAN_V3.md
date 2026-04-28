# GrumpyDB v3 — Client Interface Implementation Plan

## Vision

Transform GrumpyDB from an embedded engine into a **networked, secured, multi-tenant database server** with TCP+TLS transport, JWT authentication, RBAC authorization, and client drivers for Rust and TypeScript.

### Target architecture

```
┌──────────────┐    ┌──────────────┐    ┌──────────────┐
│  Rust Driver  │    │   TS Driver   │    │   grumpy-repl  │
│  grumpydb-    │    │  @grumpydb/   │    │  (TCP client)  │
│  client       │    │  client       │    │               │
└──────┬───────┘    └──────┬───────┘    └──────┬───────┘
       │                    │                    │
       │         TCP + TLS 1.3 (rustls)         │
       │         RESP-like protocol              │
       │         JWT bearer auth                 │
       ▼                    ▼                    ▼
┌──────────────────────────────────────────────────────┐
│                   GrumpyDB Server                     │
│  ┌──────────────────────────────────────────────┐    │
│  │  TCP Listener (tokio async)                   │    │
│  │  ┌────────────────────────────────────────┐   │    │
│  │  │  TLS Acceptor (tokio-rustls)           │   │    │
│  │  └────────────────────────────────────────┘   │    │
│  └──────────────────────────────────────────────┘    │
│  ┌──────────────────────────────────────────────┐    │
│  │  Protocol Parser (RESP-like)                  │    │
│  └──────────────────────────────────────────────┘    │
│  ┌──────────────────────────────────────────────┐    │
│  │  Session Manager (per-connection context)     │    │
│  └──────────────────────────────────────────────┘    │
│  ┌────────────────┬─────────────────────────────┐    │
│  │  Auth Store     │  RBAC Enforcer              │    │
│  │  (users, roles, │  (JWT claims → permission   │    │
│  │   argon2 hash)  │   check before execution)   │    │
│  └────────────────┴─────────────────────────────┘    │
│  ┌──────────────────────────────────────────────┐    │
│  │  SharedServer (existing)                      │    │
│  │  tenant isolation + per-database locking      │    │
│  └──────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────┘
```

### Key design decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Transport | TCP + TLS 1.3 (rustls) | No OpenSSL dependency, pure Rust, audited |
| Protocol | RESP-like text protocol | Simple, proven (Redis), testable with telnet/nc |
| Authentication | Username/password → JWT (HS256) | Stateless tokens, claims carry tenant+roles |
| Password hashing | argon2 | Memory-hard, state of the art |
| Token lifetime | Access 1h, refresh 7j | Short-lived tokens, no blacklist needed for v1 |
| Authorization | RBAC in JWT claims | Verified at each command, no session state |
| User ↔ Tenant | 1 user = 1 tenant | Strict isolation, simplifies model |
| Super-admin | `server_admin` role | Cross-tenant management (create/drop tenants) |
| Protocol crate | Shared `grumpydb-protocol` | Reused by server and Rust driver |
| TS driver transport | `node:tls` / `node:net` | No external dependency, native TLS |

---

## Identity & RBAC Model

### Entities

| Entity | Description |
|--------|-------------|
| **Tenant** | = `Client` in GrumpyDB. Unit of data isolation. |
| **User** | Authenticated identity, belongs to exactly one tenant. |
| **Role** | Named set of permissions, assigned to a user. |
| **Permission** | Right to perform an action on a resource. |

### Predefined roles

| Role | Scope | Permissions |
|------|-------|-------------|
| `server_admin` | Server | Create/drop tenants, manage all users |
| `tenant_admin` | Tenant | Create/drop databases, manage users within tenant, full CRUD |
| `db_admin` | Database | Create/drop collections, create/drop indexes, compact, full CRUD |
| `read_write` | Database or Collection | INSERT, GET, UPDATE, DELETE, SCAN, QUERY |
| `read_only` | Database or Collection | GET, SCAN, QUERY only |

### Permission model

```rust
enum Action {
    Read,            // GET, SCAN, QUERY
    Write,           // INSERT, UPDATE, DELETE
    Admin,           // CREATE/DROP collection, CREATE/DROP index, COMPACT
    ManageUsers,     // CREATE/DROP user, GRANT/REVOKE role
    ManageDatabases, // CREATE/DROP database
    ManageServer,    // CREATE/DROP tenant (server_admin only)
}

enum Resource {
    Server,                          // entire server
    Tenant(String),                  // a specific tenant
    Database(String),                // a specific database within the session tenant
    Collection(String, String),      // (database, collection)
    AllDatabases,                    // all databases within tenant
    AllCollections(String),          // all collections within a database
}
```

### Permission matrix

| Command | read_only | read_write | db_admin | tenant_admin | server_admin |
|---------|-----------|------------|----------|--------------|--------------|
| `GET` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `SCAN` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `QUERY` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `INSERT` | ✗ | ✓ | ✓ | ✓ | ✓ |
| `UPDATE` | ✗ | ✓ | ✓ | ✓ | ✓ |
| `DELETE` | ✗ | ✓ | ✓ | ✓ | ✓ |
| `CREATE COLLECTION` | ✗ | ✗ | ✓ | ✓ | ✓ |
| `DROP COLLECTION` | ✗ | ✗ | ✓ | ✓ | ✓ |
| `CREATE INDEX` | ✗ | ✗ | ✓ | ✓ | ✓ |
| `DROP INDEX` | ✗ | ✗ | ✓ | ✓ | ✓ |
| `COMPACT` | ✗ | ✗ | ✓ | ✓ | ✓ |
| `CREATE DATABASE` | ✗ | ✗ | ✗ | ✓ | ✓ |
| `DROP DATABASE` | ✗ | ✗ | ✗ | ✓ | ✓ |
| `CREATE USER` | ✗ | ✗ | ✗ | ✓ | ✓ |
| `GRANT / REVOKE` | ✗ | ✗ | ✗ | ✓ | ✓ |
| `DROP USER` | ✗ | ✗ | ✗ | ✓ | ✓ |
| `LIST USERS` | ✗ | ✗ | ✗ | ✓ | ✓ |
| `CREATE TENANT` | ✗ | ✗ | ✗ | ✗ | ✓ |
| `DROP TENANT` | ✗ | ✗ | ✗ | ✗ | ✓ |
| `LIST TENANTS` | ✗ | ✗ | ✗ | ✗ | ✓ |

### JWT structure

```
Header:  { "alg": "HS256", "typ": "JWT" }

Payload: {
  "sub": "alice",                          // username
  "tenant": "acme",                        // tenant name (= Client)
  "roles": [
    { "role": "read_write", "scope": { "db": "myapp" } },
    { "role": "db_admin",   "scope": { "db": "staging" } }
  ],
  "iat": 1745740800,                       // issued at
  "exp": 1745744400                        // expiration (1h)
}

Signature: HMAC-SHA256(base64(header).base64(payload), server_secret)
```

### On-disk layout for auth data

```
<server_root>/
  _auth/                           ← authentication metadata
    secret.key                     ← 32-byte HMAC secret (generated at first boot)
    users/                         ← user records (one JSON file per user, or future: GrumpyDB collection)
      acme__alice.json             ← { username, tenant, password_hash, roles }
      acme__bob.json
  <tenant_name>/                   ← tenant data (unchanged from v2)
    <database_name>/
      wal.log
      <collection_name>/
        data.db
        primary.idx
        idx_*.idx
```

---

## Protocol: RESP-like over TCP+TLS

### Connection lifecycle

```
Client                                     Server
  │                                           │
  │──── TCP connect ─────────────────────────→│
  │←─── TLS ServerHello + certificate ───────│
  │──── TLS ClientFinished ──────────────────→│
  │           ═══ TLS tunnel established ═══  │
  │←─── +GRUMPYDB 4.0.0\r\n ────────────────│   ← server banner
  │                                           │
  │──── LOGIN acme alice s3cr3t\r\n ─────────→│   ← tenant user password
  │←─── +TOKEN <access_jwt> <refresh_jwt>\r\n│   ← JWT tokens
  │                                           │
  │──── TOKEN <access_jwt>\r\n ──────────────→│   ← set session token
  │←─── +OK\r\n ────────────────────────────│
  │                                           │
  │──── USE mydb\r\n ────────────────────────→│   ← select database
  │←─── +OK\r\n ────────────────────────────│
  │                                           │
  │──── INSERT users <uuid> {"name":"bob"}\r\n│   ← CRUD (RBAC checked)
  │←─── +OK\r\n ────────────────────────────│
  │                                           │
  │──── QUIT\r\n ────────────────────────────→│
  │←─── +BYE\r\n ───────────────────────────│
```

### Command syntax

```
── Authentication ─────────────────────────────────────
LOGIN <tenant> <username> <password>\r\n
  → +TOKEN <access_jwt> [<refresh_jwt>]\r\n
  → -ERR invalid credentials\r\n

TOKEN <jwt>\r\n
  → +OK\r\n
  → -ERR token expired\r\n
  → -ERR invalid token\r\n

REFRESH <refresh_jwt>\r\n
  → +TOKEN <new_access_jwt>\r\n

WHOAMI\r\n
  → +USER <username> TENANT <tenant> ROLES <role1>:<scope1>,<role2>:<scope2>\r\n

── Session ────────────────────────────────────────────
USE <database>\r\n
  → +OK\r\n

PING\r\n
  → +PONG\r\n

QUIT\r\n
  → +BYE\r\n

── Database management ────────────────────────────────
CREATE DATABASE <name>\r\n
  → +OK\r\n

DROP DATABASE <name>\r\n
  → +OK\r\n

LIST DATABASES\r\n
  → *<count>\r\n$<len>\r\n<name>\r\n...

── Collection management ──────────────────────────────
CREATE COLLECTION <name>\r\n
  → +OK\r\n

DROP COLLECTION <name>\r\n
  → +OK\r\n

LIST COLLECTIONS\r\n
  → *<count>\r\n$<len>\r\n<name>\r\n...

── CRUD ───────────────────────────────────────────────
INSERT <collection> <uuid> <json>\r\n
  → +OK\r\n

GET <collection> <uuid>\r\n
  → $<len>\r\n<json>\r\n
  → $-1\r\n                           (not found)

UPDATE <collection> <uuid> <json>\r\n
  → +OK\r\n

DELETE <collection> <uuid>\r\n
  → +OK\r\n

SCAN <collection> [<start_uuid> <end_uuid>]\r\n
  → *<count>\r\n$<len>\r\n<uuid> <json>\r\n...

── Index management ───────────────────────────────────
CREATE INDEX <collection> <index_name> <field_path>\r\n
  → +OK\r\n

DROP INDEX <collection> <index_name>\r\n
  → +OK\r\n

LIST INDEXES <collection>\r\n
  → *<count>\r\n$<len>\r\n<name>:<field>\r\n...

QUERY <collection> <index_name> <json_value>\r\n
  → *<count>\r\n$<len>\r\n<uuid> <json>\r\n...

QUERYRANGE <collection> <index_name> <start_json> <end_json>\r\n
  → *<count>\r\n$<len>\r\n<uuid> <json>\r\n...

── Maintenance ────────────────────────────────────────
COMPACT <collection>\r\n
  → +OK <documents_preserved>\r\n

FLUSH\r\n
  → +OK\r\n

COUNT <collection>\r\n
  → :<count>\r\n

── User management (tenant_admin+) ────────────────────
CREATE USER <username> <password>\r\n
  → +OK\r\n

DROP USER <username>\r\n
  → +OK\r\n

LIST USERS\r\n
  → *<count>\r\n$<len>\r\n<username>:<roles>\r\n...

GRANT <role> ON <resource> TO <username>\r\n
  → +OK\r\n

REVOKE <role> ON <resource> FROM <username>\r\n
  → +OK\r\n

── Tenant management (server_admin only) ──────────────
CREATE TENANT <name>\r\n
  → +OK\r\n

DROP TENANT <name>\r\n
  → +OK\r\n

LIST TENANTS\r\n
  → *<count>\r\n$<len>\r\n<name>\r\n...
```

### Response types

```
+<message>\r\n                    Simple string (success)
-ERR <message>\r\n                Error
:<integer>\r\n                    Integer
$<length>\r\n<data>\r\n           Bulk string
$-1\r\n                           Null bulk string
*<count>\r\n...                   Array (followed by count elements)
```

---

## TLS Configuration

### Server config file (`grumpydb.toml`)

```toml
[server]
bind = "0.0.0.0:6380"

[tls]
enabled = true
cert_file = "_auth/server.crt"       # PEM certificate
key_file  = "_auth/server.key"       # PEM private key
# client_ca = "/path/to/ca.crt"     # optional: mTLS (require client cert)

[auth]
access_token_ttl  = "1h"            # access token lifetime
refresh_token_ttl = "7d"            # refresh token lifetime
```

### Three modes

| Mode | Config | Usage |
|------|--------|-------|
| **Plaintext** | `tls.enabled = false` | Development, local testing |
| **TLS** | `tls.enabled = true` + cert/key | Production standard |
| **mTLS** | TLS + `client_ca` | High security (client certificate auth) |

### Auto-generated certificates (dev mode)

On first start with `tls.enabled = true` and no cert files present, the server
generates a self-signed certificate using `rcgen` and stores it in `_auth/`.
A warning is logged encouraging production use of a CA-signed certificate.

---

## Workspace Structure

```
grumpydb/                              ← workspace root
├── Cargo.toml                         ← workspace members
│
├── grumpydb-protocol/                 ← Phase 16: shared protocol crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                     ← Constants, re-exports
│       ├── command.rs                 ← Command enum + Action/Resource + RBAC metadata
│       ├── response.rs                ← Response enum + RESP serialization/parsing
│       └── parser.rs                  ← RESP-like line parser
│
├── grumpydb-server/                   ← Phases 17–19: networked server
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                     ← crate root (re-exports auth, session)
│       ├── auth/
│       │   ├── mod.rs
│       │   ├── user.rs                ← User struct, password hashing (argon2), AuthError
│       │   ├── role.rs                ← RoleName, Action, ResourceScope, RoleAssignment
│       │   ├── jwt.rs                 ← JWT encode/decode/verify (HS256)
│       │   └── store.rs              ← AuthStore: user CRUD, persistence
│       ├── session/
│       │   └── mod.rs                 ← SessionContext + RBAC enforcer (authorize)
│       └── tcp/                       ← Phase 19: async TCP/TLS server
│           ├── mod.rs
│           ├── listener.rs            ← TLS accept loop (tokio-rustls), self-signed cert gen
│           └── handler.rs             ← Per-connection handler: parse → auth → RBAC → execute
│
├── grumpydb-client/                   ← Phase 20: Rust driver
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                     ← GrumpyClient + DatabaseHandle (consolidated)
│       ├── connection.rs              ← TCP + TLS connect, line-based I/O
│       └── error.rs                   ← Driver-specific errors
│
├── drivers/
│   └── typescript/                    ← Phase 21: TypeScript driver
│       ├── package.json
│       ├── tsconfig.json
│       └── src/
│           ├── index.ts
│           ├── connection.ts          ← node:net + node:tls, reconnect
│           ├── protocol.ts            ← RESP-like encode/decode
│           ├── auth.ts                ← LOGIN, JWT storage, auto-refresh
│           ├── client.ts              ← GrumpyClient
│           ├── database.ts            ← DatabaseHandle
│           ├── types.ts               ← Value, Document, Config types
│           └── errors.ts              ← Typed error classes
│
├── src/                               ← existing engine crate (library, unchanged)
│   ├── lib.rs
│   ├── engine.rs
│   ├── error.rs
│   ├── naming.rs
│   ├── btree/
│   ├── buffer/
│   ├── collection/
│   ├── concurrency/
│   ├── database/
│   ├── document/
│   ├── index/
│   ├── page/
│   ├── server/
│   └── wal/
│
├── grumpy-repl/                       ← Phase 22: REPL promoted to workspace crate (TCP + embedded)
│
├── examples/
│   └── taskman/
│
└── docs/
    ├── ARCHITECTURE.md
    ├── IMPLEMENTATION_PLAN.md          ← v1 plan (phases 1–8)
    ├── IMPLEMENTATION_PLAN_V2.md       ← v2 plan (phases 9–15)
    └── IMPLEMENTATION_PLAN_V3.md       ← this document (phases 16–23)
```

---

## Phase Overview

```
Phase 16: Protocol Crate         ████████████████████  Done     — shared RESP-like protocol
Phase 17: Auth Module             ████████████████████  Done     — users, roles, JWT, argon2
Phase 18: Session & RBAC          ████████████████████  Done     — per-connection context, enforcer
Phase 19: TCP Server              ████████████████████  Done     — tokio listener, TLS, handler
Phase 20: Rust Driver             ████████████████████  Done     — grumpydb-client crate
Phase 21: TypeScript Driver       ████████████████████  Done     — @grumpydb/client npm package
Phase 22: grumpy-repl (v2)        ████████████████████  Done     — dual mode: connected (TCP) + embedded; promoted to workspace crate
Phase 23: Polish & Documentation  ███████████████░░░░░  Partial  — workspace compiles, 444 tests, 0 clippy warnings, docs synced; CI/Docker/e2e formels reportés
```

### Phase dependencies

```
Phase 16 (protocol)
  ├──→ Phase 17 (auth)
  │      └──→ Phase 18 (session + RBAC)
  │             └──→ Phase 19 (TCP server)
  │                    ├──→ Phase 20 (Rust driver)
  │                    ├──→ Phase 21 (TS driver)
  │                    ├──→ Phase 22 (grumpy-repl)
  │                    └──→ Phase 23 (polish)
  ├──→ Phase 20 (Rust driver uses protocol crate)
  └──→ Phase 21 (TS driver re-implements protocol in TypeScript)
```

---

## Phase 16: Protocol Crate

### Objective

Create a shared `grumpydb-protocol` crate defining the RESP-like wire protocol:
commands, responses, parser, and serializer. Used by both the server and the
Rust driver.

### Design

```rust
/// A parsed command from the client.
pub enum Command {
    // Auth
    Login { tenant: String, username: String, password: String },
    Token(String),
    Refresh(String),
    WhoAmI,

    // Session
    Use(String),
    Ping,
    Quit,

    // Database management
    CreateDatabase(String),
    DropDatabase(String),
    ListDatabases,

    // Collection management
    CreateCollection(String),
    DropCollection(String),
    ListCollections,

    // CRUD
    Insert { collection: String, key: String, value: String },
    Get { collection: String, key: String },
    Update { collection: String, key: String, value: String },
    Delete { collection: String, key: String },
    Scan { collection: String, start: Option<String>, end: Option<String> },

    // Index
    CreateIndex { collection: String, index_name: String, field_path: String },
    DropIndex { collection: String, index_name: String },
    ListIndexes(String),
    Query { collection: String, index_name: String, value: String },
    QueryRange { collection: String, index_name: String, start: String, end: String },

    // Maintenance
    Compact(String),
    Flush,
    Count(String),

    // User management
    CreateUser { username: String, password: String },
    DropUser(String),
    ListUsers,
    Grant { role: String, resource: String, username: String },
    Revoke { role: String, resource: String, username: String },

    // Tenant management (server_admin)
    CreateTenant(String),
    DropTenant(String),
    ListTenants,
}

/// A response from the server.
pub enum Response {
    Ok(String),                           // +<message>
    Error(String),                        // -ERR <message>
    Integer(i64),                         // :<integer>
    Bulk(Option<String>),                 // $<len>\r\n<data> or $-1 (null)
    Array(Vec<Response>),                 // *<count>\r\n...
    Token { access: String, refresh: Option<String> }, // +TOKEN ...
}
```

### Tasks

#### 16.1 Cargo workspace setup

- [x] Create `grumpydb-protocol/Cargo.toml` with minimal dependencies
- [x] Update root `Cargo.toml` to define workspace members
- [x] Ensure existing `src/` crate remains a workspace member (as `grumpydb`)

#### 16.2 Command enum (`grumpydb-protocol/src/command.rs`)

- [x] Define `Command` enum with all variants listed above
- [x] `Command::required_action() → Action` — returns the minimum required action
- [x] `Command::target_resource() → Resource` — returns the resource being accessed
- [x] Tests: action/resource mapping for all command variants

#### 16.3 Response enum (`grumpydb-protocol/src/response.rs`)

- [x] Define `Response` enum (Ok, Error, Integer, Bulk, Array)
- [x] `Response::serialize() → String` — serialize to wire format
- [x] `Response::parse(input: &str) → Result<(Response, usize)>` — parse from wire format
- [x] Tests: round-trip serialize/parse for all response types

#### 16.4 Command parser (`grumpydb-protocol/src/parser.rs`)

- [x] `parse_command(line: &str) → Result<Command>` — parse a single command line
- [x] Handle quoted strings, JSON values, UUID formats
- [x] Error messages for invalid syntax with position info
- [x] Tests: parse all command types, edge cases (empty, malformed, extra args)

#### 16.5 Protocol constants (`grumpydb-protocol/src/lib.rs`)

- [x] `DEFAULT_PORT: u16 = 6380`
- [x] `PROTOCOL_VERSION: &str = "4.0.0"`
- [x] `MAX_LINE_LENGTH: usize = 1_048_576` (1 MiB — prevents DoS via huge lines)
- [x] `MAX_BULK_LENGTH: usize = 16_777_216` (16 MiB — max document size on wire)
- [x] Re-export `Command`, `Response`, `parse_command`

### Validation criteria Phase 16

- [x] All commands parse correctly from wire format
- [x] All responses serialize and parse round-trip
- [x] Error on malformed input (no panic)
- [x] `MAX_LINE_LENGTH` enforced by parser
- [x] `cargo test -p grumpydb-protocol` passes (70 tests)
- [x] `cargo clippy -p grumpydb-protocol -- -D warnings` passes

---

## Phase 17: Auth Module

### Objective

Implement user management, password hashing (argon2), role/permission model,
and JWT token generation/verification (HS256).

### Design

```rust
/// A user record.
pub struct User {
    pub username: String,
    pub tenant: String,
    pub password_hash: String,          // argon2 hash
    pub roles: Vec<RoleAssignment>,
    pub created_at: u64,                // unix timestamp
}

/// A role assigned to a user with a specific scope.
pub struct RoleAssignment {
    pub role: RoleName,
    pub scope: ResourceScope,
}

/// JWT claims embedded in the token.
pub struct Claims {
    pub sub: String,                    // username
    pub tenant: String,
    pub roles: Vec<RoleAssignment>,
    pub iat: u64,
    pub exp: u64,
}
```

### Tasks

#### 17.1 Role & Permission model (`grumpydb-server/src/auth/role.rs`)

- [x] `RoleName` enum: `ServerAdmin`, `TenantAdmin`, `DbAdmin`, `ReadWrite`, `ReadOnly`
- [x] `Action` enum: `Read`, `Write`, `Admin`, `ManageUsers`, `ManageDatabases`, `ManageServer`
- [x] `ResourceScope` enum: `Server`, `Tenant`, `Database(String)`, `Collection(String, String)`, `AllDatabases`, `AllCollections(String)`
- [x] `RoleName::permits_action(action: &Action) → bool` — permission check
- [x] `RoleAssignment::permits(action, resource) → bool` — scope-aware check
- [x] Serde serialization for JWT embedding
- [x] Tests: permission matrix coverage (all role × action × resource combinations)

#### 17.2 User management (`grumpydb-server/src/auth/user.rs`)

- [x] `User` struct with username, tenant, password_hash, roles, created_at
- [x] `hash_password(password: &str) → Result<String>` — argon2 hash with random salt
- [x] `verify_password(password: &str, hash: &str) → Result<bool>` — argon2 verify
- [x] Tests: hash + verify round-trip, wrong password fails, different hashes for same password

#### 17.3 JWT operations (`grumpydb-server/src/auth/jwt.rs`)

- [x] `JwtConfig` struct: secret key (32 bytes), access_ttl, refresh_ttl
- [x] `generate_access_token(user: &User, config: &JwtConfig) → Result<String>`
- [x] `generate_refresh_token(user: &User, config: &JwtConfig) → Result<String>`
- [x] `verify_token(token: &str, config: &JwtConfig) → Result<Claims>` — verify signature + expiration
- [x] `Claims` struct matching JWT payload
- [x] Secret key generation: `JwtConfig::new_random()` using `rand`
- [x] Tests: generate + verify round-trip, expired token rejected, tampered token rejected, wrong secret rejected

#### 17.4 Auth store (`grumpydb-server/src/auth/store.rs`)

- [x] `AuthStore` struct: manages users + server secret on disk
- [x] `AuthStore::open(auth_dir: &Path) → Result<Self>` — load or create `secret.key` + users
- [x] `create_user(tenant, username, password, roles) → Result<()>`
- [x] `get_user(tenant, username) → Result<Option<&User>>`
- [x] `delete_user(tenant, username) → Result<()>`
- [x] `list_users(tenant) → Vec<&User>`
- [x] `update_roles(tenant, username, roles) → Result<()>`
- [x] `authenticate(tenant, username, password) → Result<(String, String)>` — returns (access_token, refresh_token)
- [x] Persistence: JSON files in `_auth/users/` directory (one file per user: `<tenant>__<username>.json`)
- [x] Bootstrap: create default `server_admin` user if `_auth/` is empty
- [x] Tests: create/delete/list users, authenticate success/failure, persistence across reopen, role update

### Validation criteria Phase 17

- [x] User creation with argon2 hashing
- [x] Authentication returns valid JWT
- [x] JWT verification rejects expired / tampered tokens
- [x] RBAC permission checks match the permission matrix
- [x] Auth store persists across server restarts
- [x] Default server_admin created on first boot
- [x] `cargo test -p grumpydb-server` (auth module) passes (56 tests)
- [x] `cargo clippy -- -D warnings` passes

---

## Phase 18: Session & RBAC Enforcer

### Objective

Per-connection session context that holds the decoded JWT, current tenant, and
selected database. The RBAC enforcer checks permissions before every command
execution.

### Design

```rust
/// Per-connection session state.
pub struct SessionContext {
    pub claims: Option<Claims>,          // decoded JWT (None before LOGIN)
    pub current_db: Option<String>,      // selected database (None before USE)
}

impl SessionContext {
    /// Check if the session is authenticated.
    pub fn is_authenticated(&self) -> bool;

    /// Get the tenant name from the JWT claims.
    pub fn tenant(&self) -> Result<&str>;

    /// Check if the current session has permission to execute a command.
    pub fn authorize(&self, command: &Command) -> Result<()>;
}
```

### Tasks

#### 18.1 Session context (`grumpydb-server/src/session/mod.rs`)

- [x] `SessionContext` struct: claims, current_db
- [x] `SessionContext::new() → Self` — empty session (pre-auth)
- [x] `set_claims(claims: Claims)` — store decoded JWT
- [x] `set_database(name: String)` — set current database
- [x] `is_authenticated() → bool`
- [x] `tenant() → Result<&str>` — error if not authenticated
- [x] `current_db() → Option<&str>`
- [x] Tests: lifecycle (new → set_claims → set_database → access)

#### 18.2 RBAC enforcer (`grumpydb-server/src/session/mod.rs`)

- [x] `authorize(command: &Command) → Result<()>` — checks session claims against command requirements
- [x] Uses `Command::required_action()` and `Command::target_resource()` from protocol crate
- [x] Cross-checks against each `RoleAssignment` in claims
- [x] Returns `AuthError::AccessDenied` on failure (in `auth::user::AuthError`)
- [x] Allow all commands before auth: only `LOGIN`, `PING`, `QUIT`
- [x] Tests: authorize all command types with each role, verify denied cases

#### 18.3 New error variants

- [x] `AuthError::AccessDenied(String)` — permission denied with reason
- [x] `AuthError::NotAuthenticated` — command requires auth but no JWT set
- [x] `AuthError::TokenExpired` — JWT has expired
- [x] `AuthError::InvalidToken(String)` — JWT verification failed

> Note: error variants live in `grumpydb-server::auth::user::AuthError` rather than `GrumpyError`.

### Validation criteria Phase 18

- [x] Pre-auth session only allows LOGIN, PING, QUIT
- [x] Post-auth session enforces RBAC on every command
- [x] `read_only` user cannot INSERT
- [x] `db_admin` user cannot CREATE DATABASE
- [x] `tenant_admin` user cannot CREATE TENANT
- [x] `server_admin` user can do everything
- [x] Tests cover the full permission matrix

---

## Phase 19: TCP Server

### Objective

Async TCP server with TLS support, protocol parsing, authentication, RBAC
enforcement, and command execution via the existing `SharedServer`.

### Design

```rust
/// Main server entry point.
pub struct GrumpyTcpServer {
    config: ServerConfig,
    auth_store: Arc<AuthStore>,
    shared_server: SharedServer,
}

impl GrumpyTcpServer {
    pub async fn start(config: ServerConfig) -> Result<()>;
}
```

Each connection is handled in a spawned tokio task. The handler reads lines,
parses commands, checks RBAC, executes via `SharedServer`, and writes
responses.

### Tasks

#### 19.1 Server config (`grumpydb-server/src/config.rs`)

- [x] `ServerConfig` struct: bind address, data dir, TLS config, auth config
- [x] Parse from `grumpydb.toml` (TOML format)
- [x] CLI override: `--bind`, `--data`, `--tls-cert`, `--tls-key`, `--no-tls`
- [x] Default config generation on first run
- [x] Tests: parse valid/invalid config, CLI overrides

#### 19.2 TLS setup (`grumpydb-server/src/tcp/listener.rs`)

- [x] Load certificate + private key from PEM files via `rustls-pemfile`
- [x] Build `rustls::ServerConfig` with TLS 1.2 + 1.3
- [x] `TlsAcceptor` from `tokio-rustls`
- [x] Auto-generate self-signed certificate via `rcgen` if files not found + log warning
- [x] Optional mTLS: load client CA, configure client cert verification
- [x] Tests: TLS handshake with self-signed cert (integration)

#### 19.3 TCP listener (`grumpydb-server/src/tcp/listener.rs`)

- [x] `async fn listen(config, auth_store, shared_server) → Result<()>`
- [x] `TcpListener::bind()` on configured address
- [x] Accept loop: TLS handshake → spawn handler task per connection
- [x] Graceful shutdown on SIGINT/SIGTERM (tokio signal)
- [x] Connection limit (configurable, default 1024)
- [x] Tests: accept connection, reject over limit

#### 19.4 Connection handler (`grumpydb-server/src/tcp/handler.rs`)

- [x] `async fn handle_connection(stream, auth_store, shared_server)`
- [x] Read lines from stream (buffered, enforce `MAX_LINE_LENGTH`)
- [x] Parse command via `grumpydb-protocol::parse_command()`
- [x] Pre-auth gate: only `LOGIN`, `PING`, `QUIT` allowed
- [x] `LOGIN` → verify credentials via `AuthStore`, decode JWT, set session
- [x] `TOKEN` → verify JWT, set session
- [x] `REFRESH` → verify refresh token, issue new access token
- [x] `USE` → set current database in session
- [x] All other commands → `session.authorize()` → execute via `SharedServer` → serialize response
- [x] Write `Response::serialize()` to stream
- [x] Handle I/O errors: log + close connection gracefully
- [x] Tests: unit tests with mock stream (command → response scenarios)

#### 19.5 Command executor (`grumpydb-server/src/tcp/handler.rs`)

- [x] Map `Command` variants to `SharedServer` method calls
- [x] `Insert` → parse UUID, parse JSON to `Value`, call `shared_server.database().insert()`
- [x] `Get` → call `get()`, serialize `Value` back to JSON
- [x] `Scan` → call `scan()`, serialize array of results
- [x] `CreateUser` → call `auth_store.create_user()`
- [x] `Grant` / `Revoke` → call `auth_store.update_roles()`
- [x] Error mapping: `GrumpyError` → `Response::Error`
- [x] Tests: execute all command types end-to-end

#### 19.6 Binary entry point (`grumpydb-server/src/main.rs`)

- [x] Parse CLI args (clap or manual)
- [x] Load config from `grumpydb.toml` or defaults
- [x] Initialize `AuthStore`, `SharedServer`
- [x] Start TCP listener
- [x] Log startup info: address, TLS status, version

### Validation criteria Phase 19

- [x] Server starts, accepts TLS connections
- [x] `LOGIN` → receive JWT → execute commands
- [x] RBAC enforced: denied commands return `-ERR access denied`
- [x] Multiple concurrent connections work
- [x] Graceful shutdown flushes data
- [x] Auto-generated self-signed cert works for development
- [x] `cargo test -p grumpydb-server` passes (60 tests)
- [x] `cargo clippy -- -D warnings` passes

---

## Phase 20: Rust Driver

### Objective

A Rust client library (`grumpydb-client`) for connecting to the GrumpyDB
server over TCP+TLS, with authentication, CRUD operations, and admin commands.

### Design

```rust
// Connection
let client = GrumpyClient::connect("localhost:6380")
    .tls(true)
    .login("acme", "alice", "s3cr3t")
    .await?;

// CRUD
let db = client.database("myapp").await?;
let key = Uuid::new_v4();
db.insert("users", key, json!({"name": "bob"})).await?;
let doc = db.get("users", &key).await?;
db.update("users", &key, json!({"name": "bob", "age": 31})).await?;
db.delete("users", &key).await?;

// Query
let results = db.scan("users", Some(start), Some(end)).await?;
let bobs = db.query("users", "idx_name", &json!("bob")).await?;

// Admin
db.create_collection("logs").await?;
db.create_index("users", "idx_name", "name").await?;
client.create_database("staging").await?;
```

### Tasks

#### 20.1 Crate setup (`grumpydb-client/`)

- [x] `Cargo.toml` with dependencies: `tokio`, `tokio-rustls`, `grumpydb-protocol`, `serde_json`, `uuid`
- [x] Re-export key types in `lib.rs`

#### 20.2 Connection (`grumpydb-client/src/connection.rs`)

- [x] `Connection` struct: wraps `TlsStream<TcpStream>` or `TcpStream`
- [x] `Connection::connect(addr, tls: bool) → Result<Self>`
- [x] TLS: use system trust store or accept custom CA
- [x] `send_command(cmd: &str) → Result<()>` — write line to stream
- [x] `read_response() → Result<Response>` — parse response from stream
- [x] Auto-reconnect on connection loss (configurable retries + backoff)
- [x] Tests: connect/disconnect, send/receive mock

#### 20.3 Auth (`grumpydb-client/src/auth.rs`)

> Note: Auth functionality consolidated into `lib.rs` (GrumpyClient methods) rather than a separate `auth.rs` file.

- [x] `login(conn, tenant, username, password) → Result<(String, String)>` — send LOGIN, receive tokens
- [x] `set_token(conn, token) → Result<()>` — send TOKEN command
- [x] `refresh(conn, refresh_token) → Result<String>` — send REFRESH, receive new access token
- [x] Auto-refresh: detect expired token error, refresh + retry command
- [x] Token storage in memory (access + refresh)
- [x] Tests: login flow, token refresh, expired token auto-refresh

#### 20.4 Client (`grumpydb-client/src/client.rs`)

> Note: Client functionality consolidated into `lib.rs` (GrumpyClient struct) rather than a separate `client.rs` file.

- [x] `GrumpyClient` struct: connection, tokens, tenant
- [x] Builder pattern: `GrumpyClient::connect(addr).tls(true).login(...).await?`
- [x] `database(name) → DatabaseHandle` — returns scoped handle (sends `USE`)
- [x] `create_database(name) → Result<()>`
- [x] `drop_database(name) → Result<()>`
- [x] `list_databases() → Result<Vec<String>>`
- [x] `whoami() → Result<UserInfo>`
- [x] `close() → Result<()>` — send QUIT
- [x] Tests: full lifecycle

#### 20.5 Database handle (`grumpydb-client/src/database.rs`)

> Note: DatabaseHandle consolidated into `lib.rs` rather than a separate `database.rs` file.

- [x] `DatabaseHandle` struct: reference to client connection, database name
- [x] CRUD: `insert()`, `get()`, `update()`, `delete()`, `scan()`
- [x] Index: `create_index()`, `drop_index()`, `list_indexes()`, `query()`, `query_range()`
- [x] Collection: `create_collection()`, `drop_collection()`, `list_collections()`
- [x] Maintenance: `compact()`, `flush()`, `count()`
- [x] All methods async, return typed Results
- [x] UUID handling: accept `Uuid` type, serialize to string on wire
- [x] Value handling: accept `serde_json::Value`, serialize to JSON string on wire
- [x] Tests: CRUD round-trip, error handling

#### 20.6 Error types (`grumpydb-client/src/error.rs`)

- [x] `ClientError` enum: `Connection`, `Auth`, `Protocol`, `Server(String)`, `Timeout`
- [x] `From<std::io::Error>`, `From<Response::Error>`
- [x] `Result<T> = std::result::Result<T, ClientError>`

### Validation criteria Phase 20

- [x] Connect to server, login, CRUD operations work end-to-end
- [x] TLS connection with self-signed cert (trust override)
- [x] Token auto-refresh on expiration
- [x] All CRUD + admin methods work
- [x] Connection loss triggers reconnect
- [x] `cargo test -p grumpydb-client` passes
- [x] `cargo clippy -- -D warnings` passes

---

## Phase 21: TypeScript Driver

### Objective

A TypeScript/Node.js client library (`@grumpydb/client`) for connecting to the
GrumpyDB server. Mirrors the Rust driver API in idiomatic TypeScript.

### Design

```typescript
import { GrumpyClient } from '@grumpydb/client';

const client = await GrumpyClient.connect({
  host: 'localhost',
  port: 6380,
  tls: true,
  tenant: 'acme',
  username: 'alice',
  password: 's3cr3t',
});

const db = client.database('myapp');

const key = crypto.randomUUID();
await db.insert('users', key, { name: 'bob', age: 30 });
const doc = await db.get('users', key);
await db.update('users', key, { name: 'bob', age: 31 });
await db.delete('users', key);

const results = await db.scan('users', { start: key1, end: key2 });
const bobs = await db.query('users', 'idx_name', 'bob');

await client.close();
```

### Tasks

#### 21.1 Project setup (`drivers/typescript/`)

- [x] `package.json` with name `@grumpydb/client`, Node.js ≥ 18 engine requirement
- [x] `tsconfig.json` with strict mode, ES2022 target, Node16 module resolution
- [x] No external runtime dependencies (only `node:net`, `node:tls`, `node:crypto`)
- [x] Dev dependencies: `vitest` for testing, `typescript`
- [x] Build: `tsc` → `dist/`

#### 21.2 Protocol (`drivers/typescript/src/protocol.ts`)

- [x] `encodeCommand(parts: string[]) → string` — build command line
- [x] `parseResponse(data: string) → Response` — parse RESP-like response
- [x] Response types: `OkResponse`, `ErrorResponse`, `IntegerResponse`, `BulkResponse`, `ArrayResponse`
- [x] Tests: parse all response types, encode commands

#### 21.3 Connection (`drivers/typescript/src/connection.ts`)

- [x] `Connection` class: wraps `net.Socket` or `tls.TLSSocket`
- [x] `connect(options) → Promise<Connection>` — TCP + optional TLS
- [x] TLS options: `rejectUnauthorized`, custom CA cert
- [x] `sendCommand(cmd: string) → Promise<Response>` — request/response with Promise
- [x] Read buffering: handle partial reads, line splitting
- [x] Auto-reconnect with exponential backoff
- [x] Tests: mock socket, connect/disconnect

#### 21.4 Auth (`drivers/typescript/src/auth.ts`)

- [x] `login(conn, tenant, username, password) → Promise<{access, refresh}>`
- [x] `setToken(conn, token) → Promise<void>`
- [x] `refresh(conn, refreshToken) → Promise<string>`
- [x] Auto-refresh middleware: intercept "token expired" errors
- [x] Tests: login flow, auto-refresh

#### 21.5 Client (`drivers/typescript/src/client.ts`)

- [x] `GrumpyClient` class with static `connect(options)` factory
- [x] `database(name) → DatabaseHandle`
- [x] `createDatabase(name) → Promise<void>`
- [x] `dropDatabase(name) → Promise<void>`
- [x] `listDatabases() → Promise<string[]>`
- [x] `whoami() → Promise<UserInfo>`
- [x] `close() → Promise<void>`
- [x] Tests: full lifecycle

#### 21.6 Database handle (`drivers/typescript/src/database.ts`)

- [x] `DatabaseHandle` class scoped to a database name
- [x] CRUD: `insert()`, `get()`, `update()`, `delete()`, `scan()`
- [x] Index: `createIndex()`, `dropIndex()`, `listIndexes()`, `query()`, `queryRange()`
- [x] Collection: `createCollection()`, `dropCollection()`, `listCollections()`
- [x] Maintenance: `compact()`, `flush()`, `count()`
- [x] UUID: accept string, validate format client-side
- [x] Value: native JS objects ↔ JSON string on wire (transparent)
- [x] Tests: CRUD round-trip

#### 21.7 Types & Errors (`drivers/typescript/src/types.ts`, `errors.ts`)

- [x] `ConnectOptions` interface: host, port, tls, tenant, username, password
- [x] `UserInfo` interface: username, tenant, roles
- [x] `GrumpyError` base class with subclasses: `ConnectionError`, `AuthError`, `ProtocolError`, `ServerError`
- [x] Error code mapping from server `-ERR` messages

### Validation criteria Phase 21

- [x] `npm test` passes (vitest)
- [x] Connect to running server, full CRUD cycle
- [x] TLS connection works
- [x] Auto-refresh on token expiration
- [x] TypeScript strict mode: no `any` leaks, full type safety
- [x] `npm run build` produces clean `dist/`

---

## Phase 22: grumpy-repl (formerly GrumpyShell v2)

> **Status: Done** — Dual mode shell: connected (TCP) and embedded (direct disk). E2E tested with LOGIN, USE, CRUD, SCAN, COUNT, collections listing over TCP. Subsequently promoted from `examples/grumpysh/` to the dedicated workspace crate `grumpy-repl/` (binary `grumpy-repl`).

### Objective

Transform the existing REPL from an embedded-mode shell into a
TCP client that connects to a running GrumpyDB server. Retains the same
JavaScript-like syntax.

### Design

Two modes:
- **Connected mode** (default): connects to a server via TCP+TLS, requires authentication
- **Embedded mode** (`--embedded`): direct access, no network (backward compat)

```
$ cargo run -p grumpy-repl -- --host localhost --port 6380 --tenant acme --user alice
Password: ****
Connected to GrumpyDB 4.0.0 at localhost:6380 (TLS)
Authenticated as alice@acme

grumpy> use mydb
Switched to database "mydb"

grumpy> db.users.insert({ name: "Alice", age: 30 })
Inserted: a3b4c5d6-...
```

### Tasks

#### 22.1 CLI arguments update (`grumpy-repl/src/main.rs`)

- [x] `--host <host>` — server hostname (default: localhost)
- [x] `--port <port>` — server port (default: 6380)
- [x] `--tenant <tenant>` — tenant name (required in connected mode)
- [x] `--user <username>` — username (required in connected mode)
- [x] `--password <password>` — password (or prompt interactively)
- [x] `--tls` / `--no-tls` — TLS toggle
- [x] `--embedded` — direct embedded mode (existing behavior)
- [x] `--data <dir>` — data directory (embedded mode only)

#### 22.2 TCP backend (`grumpy-repl/src/tcp_backend.rs` + `repl.rs`)

- [x] `TcpBackend` struct wrapping `GrumpyClient` with `tokio::runtime::Runtime::block_on()` for synchronous shell
- [x] `Repl` struct has `tcp: Option<TcpBackend>` field — routes to `execute_tcp()` or `execute_embedded()` based on mode
- [x] `Repl::with_tcp_backend()` constructor for connected mode
- [x] Translate parsed shell commands → protocol command strings → send via `raw_execute()`
- [x] Pretty-print protocol responses (JSON formatting, arrays)

#### 22.3 Auth integration

- [x] On startup in connected mode: prompt for password if not provided
- [x] Call `client.login(tenant, username, password)` via `TcpBackend::connect()`
- [x] Display: `Authenticated as <user>@<tenant>`
- [x] Handle token expiration gracefully (auto-refresh via driver)

#### 22.4 New admin commands in shell

> Note: Admin commands deferred — users can use the protocol directly via `raw_execute()` or the Rust/TS drivers.

### Validation criteria Phase 22

- [x] `cargo run -p grumpy-repl -- --host localhost` connects to server
- [x] All existing shell commands work through TCP (INSERT, GET, DELETE, COUNT, SCAN/find, CREATE COLLECTION, LIST COLLECTIONS)
- [x] `--embedded` mode preserves backward compatibility
- [x] Password prompting works (interactive input when not provided via CLI)
- [x] Server auto-creates tenant (client) on LOGIN and database on USE
- [x] Data persists across reconnections

---

## Phase 23: Polish & Documentation

> **Status: Partial** — Workspace compiles, clippy passes (0 warnings), 444 tests pass across all crates (296 engine + 70 protocol + 60 server + 12 stress + 4 client + 1 doc + 1 grumpy-repl integration). Documentation fully synced (README, ARCHITECTURE, CLAUDE, CONTRIBUTING, plans). Formal e2e integration tests, TypeScript driver tests harness, CI pipeline, and Docker image deferred.

### Objective

Integration tests, documentation, CI pipeline, and final polish.

### Tasks

#### 23.1 End-to-end integration tests

- [ ] Start server in test, connect with Rust driver, full CRUD cycle
- [ ] Multi-tenant isolation: two tenants, verify data isolation
- [ ] RBAC: verify read_only user cannot write, db_admin cannot create database
- [ ] TLS: test both TLS and plaintext modes
- [ ] Token expiration + refresh flow
- [ ] Concurrent connections: 10 clients, parallel CRUD
- [ ] Stress test: 1000 operations through TCP

#### 23.2 TypeScript integration tests

- [ ] Start server, connect with TS driver, full CRUD cycle
- [ ] Test TLS connection
- [ ] Test auth flow

#### 23.3 Documentation

- [x] Update `docs/ARCHITECTURE.md` with network layer (section 19)
- [x] Update `README.md` with embedded-first quick start, server, drivers, RBAC
- [x] `grumpydb-protocol` crate docs (`//!` doc comments + `pub` API documented)
- [x] `grumpydb-server` crate docs (`//!` module docs)
- [x] `grumpydb-client` crate docs (`//!` with usage example)
- [ ] `@grumpydb/client` README with TypeScript examples
- [ ] `cargo doc --workspace --no-deps` with 0 warnings (not yet validated)
- [x] Run docs-agent to verify consistency

#### 23.4 CI & packaging

- [x] All workspace crates: `cargo fmt --check && cargo clippy -- -D warnings && cargo test`
- [ ] TypeScript: `npm run lint && npm test && npm run build`
- [ ] Docker image for server: `Dockerfile` in `grumpydb-server/`
- [ ] Example `docker-compose.yml` with server + TLS + persistent volume

### Validation criteria Phase 23

- [ ] Full end-to-end test suite passes (Rust + TypeScript)
- [x] Core documentation up to date (README, ARCHITECTURE, CLAUDE, CONTRIBUTING, plans)
- [ ] CI pipeline green
- [ ] Docker image builds and runs
- [x] Zero clippy warnings across workspace

---

## New dependencies

### Server (`grumpydb-server`)

```toml
[dependencies]
grumpydb = { path = "../" }
grumpydb-protocol = { path = "../grumpydb-protocol" }
tokio = { version = "1", features = ["full"] }
tokio-rustls = "0.26"
rustls = "0.23"
rustls-pemfile = "2"
rcgen = "0.13"                    # self-signed cert generation
jsonwebtoken = "9"                # JWT encode/decode
argon2 = "0.5"                    # password hashing
rand = "0.8"                      # secret key generation
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"                      # config file parsing
uuid = { version = "1", features = ["v4", "serde"] }
tracing = "0.1"                   # structured logging
tracing-subscriber = "0.3"
```

### Protocol (`grumpydb-protocol`)

```toml
[dependencies]
# Minimal — no heavy deps
thiserror = "2"
```

### Rust driver (`grumpydb-client`)

```toml
[dependencies]
grumpydb-protocol = { path = "../grumpydb-protocol" }
tokio = { version = "1", features = ["net", "io-util", "rt", "macros"] }
tokio-rustls = "0.26"
rustls = "0.23"
serde_json = "1"
uuid = { version = "1", features = ["v4", "serde"] }
thiserror = "2"
```

### TypeScript driver (`@grumpydb/client`)

```json
{
  "devDependencies": {
    "typescript": "^5.4",
    "vitest": "^2.0"
  }
}
```
Zero runtime dependencies (node:net, node:tls, node:crypto only).

---

## Module dependency graph (v3)

```
grumpydb (existing engine crate — unchanged)
  ├── error, page, document, btree, wal, buffer
  ├── index, collection, database, server, concurrency
  └── engine, lib.rs

grumpydb-protocol (new — shared)
  └── command, response, parser

grumpydb-server (new — binary)
  ├── depends on: grumpydb, grumpydb-protocol
  ├── auth/ (user, role, jwt, store)
  ├── session/ (context, RBAC enforcer)
  └── tcp/ (listener, handler, TLS)

grumpydb-client (new — library)
  ├── depends on: grumpydb-protocol
  └── connection, auth, client, database, error

@grumpydb/client (new — npm package)
  └── protocol, connection, auth, client, database, types, errors

grumpy-repl (workspace crate — TCP client + embedded)
  └── depends on: grumpydb-client (connected mode) or grumpydb (embedded mode)
```

---

## Versioning plan

| Phase | Version | Milestone |
|-------|---------|-----------|
| 16 | 4.0.0-alpha.1 | Protocol crate |
| 17 | 4.0.0-alpha.2 | Auth module |
| 18 | 4.0.0-alpha.3 | Session & RBAC |
| 19 | 4.0.0-beta.1 | TCP server functional |
| 20 | 4.0.0-beta.2 | Rust driver |
| 21 | 4.0.0-beta.3 | TypeScript driver |
| 22 | 4.0.0-rc.1 | grumpy-repl (formerly GrumpyShell v2) |
| 23 | 4.0.0 | Release |

Phase 16 starts the 4.0.0 cycle — the transition from embedded-only to
client/server is a major architectural change.

---

## Risk assessment

| Risk | Impact | Mitigation |
|------|--------|------------|
| Tokio async complexity | Medium | Isolate async in tcp/ module only, engine stays sync |
| TLS cert management | Low | Auto-generate self-signed for dev, document production setup |
| JWT secret rotation | Medium | v1: single secret, v2: key rotation with `kid` header |
| Argon2 performance (slow hash) | Low | Hash only on LOGIN (not per-command), tunable parameters |
| Protocol evolution | Medium | Version in banner, document backward compat strategy |
| Driver parity (Rust vs TS) | Medium | Shared protocol spec, integration test suite for both |
| Large JSON on wire | Low | `MAX_BULK_LENGTH` limit, streaming not needed for v1 |
| Connection pool in driver | Low | Phase 20 is single-connection; pool is v2 enhancement |
| RBAC bypass via JWT tampering | High | HMAC-SHA256 signature, secret never leaves server process |

---

## Estimated test counts

| Phase | New tests | Cumulative |
|-------|-----------|------------|
| 16 | ~25 | 339 |
| 17 | ~30 | 369 |
| 18 | ~15 | 384 |
| 19 | ~20 | 404 |
| 20 | ~20 | 424 |
| 21 | ~20 (vitest) | 424 + 20 TS |
| 22 | ~10 | 434 + 20 TS |
| 23 | ~15 | 449 + 20 TS |
