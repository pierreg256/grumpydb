<p align="center">
  <img src="docs/grumpy-logo.png" alt="GrumpyDB logo" width="200">
</p>

# GrumpyDB

**A document-oriented object database written in Rust.**

GrumpyDB stores schema-less JSON-like documents on disk with B+Tree indexing, page-based storage, WAL durability, and multi-tenant isolation. It can be used as an **embedded library** (linked directly into your Rust app) or as a **standalone server** accessed over TCP+TLS with JWT authentication and role-based access control.

---

## Quick Start

### Embedded — no server needed

```bash
cargo run --example grumpysh
```

```js
grumpy> use myapp
Switched to database "myapp"

grumpy [myapp]> db.createCollection("users")
Collection "users" created

grumpy [myapp]> db.users.insert({ name: "Alice", age: 30, email: "alice@example.com" })
Inserted: 3df9dde6-...

grumpy [myapp]> db.users.insert({ name: "Bob", age: 25, tags: ["dev", "rust"] })
Inserted: e7f8a9b0-...

grumpy [myapp]> db.users.find()
[
  { "_id": "3df9dde6-...", "name": "Alice", "age": 30, "email": "alice@example.com" },
  { "_id": "e7f8a9b0-...", "name": "Bob", "age": 25, "tags": ["dev", "rust"] }
]

grumpy [myapp]> db.users.createIndex("by_age", "age")
Index "by_age" created on field "age"

grumpy [myapp]> db.users.query("by_age", 30)
[{ "_id": "3df9dde6-...", "name": "Alice", "age": 30 }]

grumpy [myapp]> db.users.find({ age: 25 })
[{ "_id": "e7f8a9b0-...", "name": "Bob", "age": 25 }]
```

### Client/Server — multi-tenant with auth

```bash
# Terminal 1: Start the server
cargo build -p grumpydb-server
target/debug/grumpydb-server --data ./data --no-tls

# Terminal 2: Connect with the shell
cargo run --example grumpysh -- \
  --host localhost --port 6380 \
  --tenant _system --user admin --password admin
```

```js
Connected to GrumpyDB at localhost:6380
Authenticated as admin@_system

grumpy> use myapp
Switched to database "myapp"

grumpy [myapp]> db.users.insert({ name: "Alice", age: 30 })
Inserted: a1b2c3d4-...

grumpy [myapp]> db.users.count()
1
```

---

## Use as a Rust Library

Add GrumpyDB to your `Cargo.toml`:

```toml
[dependencies]
grumpydb = "3.1"
```

### Single-collection (simple key-value)

```rust
use grumpydb::{GrumpyDb, Value};
use uuid::Uuid;
use std::collections::BTreeMap;

let mut db = GrumpyDb::open(std::path::Path::new("./mydb")).unwrap();

let key = Uuid::new_v4();
let doc = Value::Object(BTreeMap::from([
    ("name".into(), Value::String("Alice".into())),
    ("age".into(), Value::Integer(30)),
]));

db.insert(key, doc).unwrap();
let result = db.get(&key).unwrap();
assert!(result.is_some());
db.close().unwrap();
```

### Multi-collection with secondary indexes

```rust
use grumpydb::Database;

let mut db = Database::open(std::path::Path::new("./myapp")).unwrap();
db.create_collection("users").unwrap();
db.create_index("users", "by_email", "email").unwrap();

let key = uuid::Uuid::new_v4();
db.insert("users", key, grumpydb::Value::Object(/* ... */)).unwrap();

// Query by index
let results = db.query("users", "by_email", &grumpydb::Value::String("alice@test.com".into())).unwrap();
db.close().unwrap();
```

### Thread-safe concurrent access

```rust
use grumpydb::SharedDatabase;

let db = SharedDatabase::open(std::path::Path::new("./myapp")).unwrap();

// Clone is cheap (Arc), share across threads
let db2 = db.clone();
std::thread::spawn(move || {
    db2.insert("users", uuid::Uuid::new_v4(), grumpydb::Value::Integer(42)).unwrap();
});

let count = db.document_count("users").unwrap();
```

---

## GrumpyShell

An interactive REPL with JavaScript-like syntax, relaxed JSON (unquoted keys, single quotes, trailing commas), and line editing with history.

```bash
# Embedded (no server)
cargo run --example grumpysh
cargo run --example grumpysh -- --data ./mydata
cargo run --example grumpysh -- --eval "use test; db.users.count()"

# Connected (TCP)
cargo run --example grumpysh -- --host localhost --tenant acme --user alice --password s3cr3t
```

### Commands

| Category | Commands |
|----------|----------|
| Database | `use <name>` |
| Collections | `db.createCollection("x")`, `db.dropCollection("x")`, `db.collections()` |
| CRUD | `db.x.insert({...})`, `db.x.get("id")`, `db.x.find()`, `db.x.find({age: 30})`, `db.x.update("id", {...})`, `db.x.delete("id")`, `db.x.count()` |
| Indexes | `db.x.createIndex("name", "field")`, `db.x.query("name", value)`, `db.x.queryRange("name", start, end)`, `db.x.indexes()` |
| References | `$ref("coll", "uuid")`, `db.x.resolve("id")`, `db.x.resolveDeep("id")` |
| Maintenance | `db.x.compact()`, `db.x.stats()`, `db.flush()` |

---

## Server

### Architecture

```
Clients (grumpysh, Rust driver, TypeScript driver, nc/telnet)
    │
    │  TCP + TLS 1.3 (rustls)
    │  RESP-like text protocol
    │  JWT authentication
    │
┌───▼──────────────────────────────────────────┐
│              GrumpyDB Server                  │
│  ┌─────────────────────────────────────────┐ │
│  │  TLS · Protocol Parser · RBAC Enforcer  │ │
│  └────────────────┬────────────────────────┘ │
│  ┌────────────────▼────────────────────────┐ │
│  │  Auth Store (argon2 + JWT HS256)        │ │
│  └────────────────┬────────────────────────┘ │
│  ┌────────────────▼────────────────────────┐ │
│  │  Engine: Tenants · Databases ·          │ │
│  │  Collections · B+Tree · WAL · Buffer    │ │
│  └─────────────────────────────────────────┘ │
└──────────────────────────────────────────────┘
```

### Running the server

```bash
cargo build -p grumpydb-server

# Plaintext (dev)
target/debug/grumpydb-server --data ./data --no-tls

# TLS (auto-generates self-signed cert)
target/debug/grumpydb-server --data ./data

# With config file
target/debug/grumpydb-server --config grumpydb.toml
```

At first launch, a default admin is created: `admin` (password: `admin`) in tenant `_system`.

### Configuration (`grumpydb.toml`)

```toml
[server]
bind = "0.0.0.0:6380"
max_connections = 1024
data_dir = "./data"

[tls]
enabled = true
# cert_file = "server.crt"    # auto-generated if absent
# key_file  = "server.key"

[auth]
access_token_ttl_secs = 3600      # 1 hour
refresh_token_ttl_secs = 604800   # 7 days
```

### User & tenant management

Connect as server admin via `nc localhost 6380`:

```
LOGIN _system admin admin
TOKEN <jwt>

CREATE TENANT acme
CREATE USER alice@acme s3cr3t
GRANT tenant_admin ON @acme TO alice@acme

LIST TENANTS
LIST USERS @acme
```

### Notation

| Syntax | Meaning |
|--------|---------|
| `alice` | User `alice` in current tenant |
| `alice@acme` | User `alice` in tenant `acme` |
| `mydb` | Database (or collection if `USE` is active) |
| `mydb@acme` | Database in tenant `acme` |
| `users:mydb` | Collection `users` in database `mydb` |
| `users:mydb@acme` | Collection in database in tenant |
| `@acme` | Tenant scope (for GRANT/REVOKE) |

### RBAC roles

| Role | Permissions |
|------|-------------|
| `server_admin` | Everything (cross-tenant) |
| `tenant_admin` | Manage databases, users, full CRUD within tenant |
| `db_admin` | Manage collections, indexes, CRUD within a database |
| `read_write` | INSERT, GET, UPDATE, DELETE, SCAN, QUERY |
| `read_only` | GET, SCAN, QUERY |

---

## Client Drivers

### Rust (`grumpydb-client`)

```rust
use grumpydb_client::GrumpyClient;

let mut client = GrumpyClient::connect("localhost", 6380, false).await?;
client.login("acme", "alice", "s3cr3t").await?;

let db = client.database("myapp").await?;
let key = uuid::Uuid::new_v4();
db.insert("users", key, &serde_json::json!({"name": "Bob"})).await?;
let doc = db.get("users", &key).await?;
```

### TypeScript (`@grumpydb/client`)

```typescript
import { GrumpyClient } from '@grumpydb/client';

const client = await GrumpyClient.connect({
  host: 'localhost', port: 6380, tls: false,
  tenant: 'acme', username: 'alice', password: 's3cr3t',
});

const db = client.database('myapp');
await db.insert('users', crypto.randomUUID(), { name: 'Bob' });
const doc = await db.get('users', '<uuid>');
await client.close();
```

---

## Storage Engine

Under the hood, GrumpyDB is a page-based storage engine:

- **8 KiB pages** with slotted layout and overflow chains for large documents
- **B+Tree indexes** — fixed-key (UUID primary) and variable-key (secondary)
- **Write-Ahead Log** for crash recovery (before-image undo)
- **Buffer pool** with LRU eviction and dirty page tracking
- **SWMR concurrency** — one writer or many readers per database
- **Compaction** — defragments data pages and rebuilds indexes
- **Document references** — `$ref("collection", "uuid")` with cycle-safe resolution

### On-disk layout

```
<data_dir>/
  _auth/                        # JWT secret + user records
  <tenant>/
    <database>/
      wal.log                   # Write-Ahead Log
      <collection>/
        data.db                 # Slotted pages (documents)
        primary.idx             # B+Tree: UUID → (page, slot)
        idx_<name>.idx          # Secondary B+Tree indexes
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for full technical details.

---

## Building & Testing

```bash
cargo build --workspace          # Build everything
cargo test --workspace           # Run all tests (~445)
cargo clippy --workspace -- -D warnings  # Lint
cargo doc --workspace --no-deps  # Generate docs
```

## Demo App

The `examples/taskman/` directory is a complete task manager CLI demonstrating every engine feature:

- **[Tutorial](examples/taskman/TUTORIAL.md)** — 7-chapter guide
- **[Cookbook](examples/taskman/COOKBOOK.md)** — recipes for common patterns
- **[Performance Guide](examples/taskman/PERFORMANCE.md)** — buffer pool tuning

```bash
cargo run --example taskman -- help
```

## License

MIT
