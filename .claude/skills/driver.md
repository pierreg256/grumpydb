# Skill: Client Drivers

## When to use this skill

When working on:
- `grumpydb-client/` — Rust driver crate
- `drivers/typescript/` — TypeScript/Node.js driver
- Any code that connects to GrumpyDB as a client

## Core principles

### Driver architecture (both languages)

```
┌─────────────────────────────────┐
│  GrumpyClient                    │  ← main entry point
│  ├── connection: Connection      │  ← TCP + TLS transport
│  ├── tokens: (access, refresh)   │  ← JWT token pair
│  └── tenant: String              │  ← current tenant
├─────────────────────────────────┤
│  DatabaseHandle                  │  ← scoped to one database
│  ├── client: &GrumpyClient      │
│  └── database: String            │
├─────────────────────────────────┤
│  Connection                      │  ← low-level I/O
│  ├── stream: TLS or TCP          │
│  ├── reader: BufReader           │
│  └── writer: BufWriter           │
└─────────────────────────────────┘
```

### Connection lifecycle

```
1. TCP connect (+ TLS handshake if enabled)
2. Read server banner: "+GRUMPYDB 4.0.0\r\n"
3. Send: "LOGIN <tenant> <user> <password>\r\n"
4. Receive: "+TOKEN <access> <refresh>\r\n"
5. Send: "TOKEN <access>\r\n" → "+OK\r\n"
6. Send: "USE <database>\r\n" → "+OK\r\n"
7. ... CRUD operations ...
8. Send: "QUIT\r\n" → "+BYE\r\n"
9. Close socket
```

### Rust driver specifics

#### Crate: `grumpydb-client`

```rust
// Builder pattern
let client = GrumpyClient::builder()
    .host("localhost")
    .port(6380)
    .tls(true)
    .build()
    .await?;

client.login("acme", "alice", "s3cr3t").await?;

// Scoped database handle
let db = client.database("myapp").await?;

// CRUD — all async
let key = Uuid::new_v4();
db.insert("users", key, json!({"name": "bob"})).await?;
let doc = db.get("users", &key).await?;           // Option<serde_json::Value>
db.update("users", &key, json!({"name": "bob2"})).await?;
db.delete("users", &key).await?;

// Scan
let results = db.scan("users", None, None).await?; // Vec<(Uuid, Value)>

// Index
db.create_index("users", "idx_name", "name").await?;
let bobs = db.query("users", "idx_name", &json!("bob")).await?;

// Admin
db.create_collection("logs").await?;
client.create_database("staging").await?;

// Close
client.close().await?;
```

#### Connection implementation

```rust
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

pub struct Connection {
    reader: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    writer: BufWriter<Box<dyn AsyncWrite + Unpin + Send>>,
}

impl Connection {
    pub async fn connect(host: &str, port: u16, tls: bool) -> Result<Self> {
        let tcp = TcpStream::connect((host, port)).await?;

        if tls {
            let connector = build_tls_connector()?;
            let domain = rustls::pki_types::ServerName::try_from(host)?;
            let tls_stream = connector.connect(domain, tcp).await?;
            Ok(Self::from_stream(tls_stream))
        } else {
            Ok(Self::from_stream(tcp))
        }
    }

    pub async fn send_command(&mut self, cmd: &str) -> Result<()> {
        self.writer.write_all(cmd.as_bytes()).await?;
        if !cmd.ends_with("\r\n") {
            self.writer.write_all(b"\r\n").await?;
        }
        self.writer.flush().await?;
        Ok(())
    }

    pub async fn read_response(&mut self) -> Result<Response> {
        let mut line = String::new();
        self.reader.read_line(&mut line).await?;
        // Parse RESP response...
        parse_response(&line)
    }
}
```

#### Error types

```rust
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("connection error: {0}")]
    Connection(#[from] std::io::Error),

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("server error: {0}")]
    Server(String),

    #[error("operation timed out")]
    Timeout,

    #[error("not connected")]
    NotConnected,
}
```

#### Auto-refresh logic

```rust
async fn execute_with_refresh(&mut self, cmd: &str) -> Result<Response> {
    let resp = self.connection.send_and_receive(cmd).await?;

    if is_token_expired_error(&resp) {
        // Try refresh
        let refresh_cmd = format!("REFRESH {}\r\n", self.refresh_token);
        let refresh_resp = self.connection.send_and_receive(&refresh_cmd).await?;

        if let Response::Token { access, .. } = refresh_resp {
            self.access_token = access.clone();
            // Re-set token on session
            self.connection.send_and_receive(&format!("TOKEN {}\r\n", access)).await?;
            // Retry original command
            return self.connection.send_and_receive(cmd).await;
        }
    }

    Ok(resp)
}
```

### TypeScript driver specifics

#### Package: `@grumpydb/client`

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

// CRUD
const key = crypto.randomUUID();
await db.insert('users', key, { name: 'bob' });
const doc = await db.get('users', key);         // object | null
await db.update('users', key, { name: 'bob2' });
await db.delete('users', key);

await client.close();
```

#### Connection with `node:tls` / `node:net`

```typescript
import * as net from 'node:net';
import * as tls from 'node:tls';

class Connection {
  private socket: net.Socket;
  private buffer: string = '';

  static async connect(options: ConnectOptions): Promise<Connection> {
    return new Promise((resolve, reject) => {
      const socket = options.tls
        ? tls.connect({
            host: options.host,
            port: options.port,
            rejectUnauthorized: options.rejectUnauthorized ?? true,
            ca: options.ca,
          })
        : net.connect({ host: options.host, port: options.port });

      socket.once('connect', () => resolve(new Connection(socket)));
      socket.once('secureConnect', () => resolve(new Connection(socket)));
      socket.once('error', reject);
    });
  }

  async sendCommand(cmd: string): Promise<Response> {
    return new Promise((resolve, reject) => {
      this.socket.write(cmd.endsWith('\r\n') ? cmd : cmd + '\r\n');
      // Read response from buffer...
      this.onNextLine((line) => {
        try { resolve(parseResponse(line)); }
        catch (e) { reject(e); }
      });
    });
  }
}
```

#### Read buffering (critical for TCP)

TCP doesn't guarantee message boundaries. Must buffer partial reads:

```typescript
private onData(chunk: Buffer): void {
  this.buffer += chunk.toString('utf-8');

  let newlineIndex: number;
  while ((newlineIndex = this.buffer.indexOf('\r\n')) !== -1) {
    const line = this.buffer.slice(0, newlineIndex + 2);
    this.buffer = this.buffer.slice(newlineIndex + 2);
    this.handleLine(line);
  }
}
```

#### TypeScript interfaces

```typescript
interface ConnectOptions {
  host: string;
  port: number;
  tls?: boolean;
  rejectUnauthorized?: boolean;
  ca?: string | Buffer;
  tenant: string;
  username: string;
  password: string;
}

interface UserInfo {
  username: string;
  tenant: string;
  roles: RoleAssignment[];
}

type Value = string | number | boolean | null | Value[] | { [key: string]: Value };
```

### API parity checklist

Both drivers MUST implement all of these methods:

| Method | Rust signature | TS signature |
|--------|---------------|--------------|
| `connect` | `async fn connect(host, port, tls) -> Result<Self>` | `static async connect(opts): Promise<Client>` |
| `login` | `async fn login(&mut self, tenant, user, pass)` | `included in connect()` |
| `close` | `async fn close(self)` | `async close(): Promise<void>` |
| `database` | `async fn database(&self, name) -> DatabaseHandle` | `database(name): DatabaseHandle` |
| `create_database` | `async fn create_database(&self, name)` | `async createDatabase(name)` |
| `drop_database` | `async fn drop_database(&self, name)` | `async dropDatabase(name)` |
| `list_databases` | `async fn list_databases(&self) -> Vec<String>` | `async listDatabases(): string[]` |
| `insert` | `async fn insert(&self, coll, key, val)` | `async insert(coll, key, val)` |
| `get` | `async fn get(&self, coll, key) -> Option<Value>` | `async get(coll, key): Value\|null` |
| `update` | `async fn update(&self, coll, key, val)` | `async update(coll, key, val)` |
| `delete` | `async fn delete(&self, coll, key)` | `async delete(coll, key)` |
| `scan` | `async fn scan(&self, coll, start, end)` | `async scan(coll, opts?)` |
| `create_index` | `async fn create_index(&self, coll, name, field)` | `async createIndex(coll, name, field)` |
| `query` | `async fn query(&self, coll, idx, val)` | `async query(coll, idx, val)` |

### Reconnect strategy

```
Attempt 1: immediate retry
Attempt 2: wait 100ms
Attempt 3: wait 500ms
Attempt 4: wait 2s
Attempt 5: wait 5s
Then: give up, return ConnectionError
```

Exponential backoff with jitter. Configurable max retries.

## Common mistakes to avoid

1. **Not handling partial TCP reads** — TCP is a byte stream, not message-based. Always buffer.
2. **Blocking the event loop (Node.js)** — all I/O must be async/callback-based
3. **Not flushing write buffer** — BufWriter holds data until flushed
4. **Ignoring TLS certificate validation** — default to `rejectUnauthorized: true`; only disable for dev
5. **Not re-setting TOKEN after refresh** — the server session needs the new token
6. **UUID validation** — validate UUID format client-side before sending to avoid wasted round-trips
7. **JSON serialization** — use `serde_json::to_string()` (Rust) / `JSON.stringify()` (TS), never manual formatting
