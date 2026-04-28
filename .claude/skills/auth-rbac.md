# Skill: Authentication & RBAC

## When to use this skill

When working on:
- `grumpydb-server/src/auth/user.rs` — User struct, password hashing
- `grumpydb-server/src/auth/role.rs` — Roles, permissions, actions
- `grumpydb-server/src/auth/jwt.rs` — JWT encode/decode/verify
- `grumpydb-server/src/auth/store.rs` — AuthStore persistence
- `grumpydb-server/src/session/mod.rs` — SessionContext, RBAC enforcer

## Core principles

### Identity model

```
Server
  └── Tenant ("acme")           ← = Client in engine layer
        └── User ("alice")      ← belongs to exactly 1 tenant
              └── RoleAssignment ← role + scope (database/collection)
```

- **One user = one tenant** (strict isolation)
- **Exception**: `server_admin` role can access all tenants

### Password hashing — argon2

```rust
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use argon2::password_hash::{SaltString, rand_core::OsRng};

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();  // Argon2id with default params
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| GrumpyError::AuthError(e.to_string()))?;
    Ok(hash.to_string())
}

pub fn verify_password(password: &str, hash: &str) -> Result<bool> {
    let parsed_hash = argon2::PasswordHash::new(hash)
        .map_err(|e| GrumpyError::AuthError(e.to_string()))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}
```

**Rules**:
- Always use `Argon2::default()` (Argon2id variant)
- Salt is random per hash (via `OsRng`)
- Hash output is PHC string format: `$argon2id$v=19$m=19456,t=2,p=1$...`
- Never store or log plaintext passwords

### JWT structure

```
Header:  { "alg": "HS256", "typ": "JWT" }

Payload (Claims):
{
  "sub": "alice",                           // username
  "tenant": "acme",                         // tenant name
  "roles": [
    { "role": "read_write", "scope": "db:myapp" },
    { "role": "db_admin",   "scope": "db:staging" }
  ],
  "iat": 1745740800,                        // issued at (unix timestamp)
  "exp": 1745744400                         // expiration (unix timestamp)
}

Signature: HMAC-SHA256(base64url(header).base64url(payload), secret)
```

### JWT operations

```rust
use jsonwebtoken::{encode, decode, Header, Algorithm, Validation,
                   EncodingKey, DecodingKey};

pub struct JwtConfig {
    pub secret: [u8; 32],
    pub access_ttl: Duration,     // default: 1 hour
    pub refresh_ttl: Duration,    // default: 7 days
}

pub fn generate_access_token(user: &User, config: &JwtConfig) -> Result<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let claims = Claims {
        sub: user.username.clone(),
        tenant: user.tenant.clone(),
        roles: user.roles.clone(),
        iat: now,
        exp: now + config.access_ttl.as_secs(),
    };
    let key = EncodingKey::from_secret(&config.secret);
    encode(&Header::default(), &claims, &key)
        .map_err(|e| GrumpyError::AuthError(e.to_string()))
}

pub fn verify_token(token: &str, config: &JwtConfig) -> Result<Claims> {
    let key = DecodingKey::from_secret(&config.secret);
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    decode::<Claims>(token, &key, &validation)
        .map(|data| data.claims)
        .map_err(|e| match e.kind() {
            jsonwebtoken::errors::ErrorKind::ExpiredSignature
                => GrumpyError::TokenExpired,
            _ => GrumpyError::InvalidToken(e.to_string()),
        })
}
```

### Role & permission model

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RoleName {
    ServerAdmin,     // cross-tenant, full access
    TenantAdmin,     // within tenant: manage databases, users
    DbAdmin,         // within database: manage collections, indexes
    ReadWrite,       // within scope: full CRUD
    ReadOnly,        // within scope: read only
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Action {
    Read,             // GET, SCAN, QUERY, COUNT
    Write,            // INSERT, UPDATE, DELETE
    Admin,            // CREATE/DROP COLLECTION/INDEX, COMPACT, FLUSH
    ManageUsers,      // CREATE/DROP USER, GRANT, REVOKE
    ManageDatabases,  // CREATE/DROP DATABASE
    ManageServer,     // CREATE/DROP TENANT
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ResourceScope {
    Server,                            // entire server
    Tenant,                            // current tenant
    Database(String),                  // specific database
    Collection(String, String),        // (database, collection)
    AllDatabases,                      // all databases in tenant
    AllCollections(String),            // all collections in database
}
```

### Permission check logic

```rust
impl RoleName {
    pub fn permits(&self, action: &Action) -> bool {
        match self {
            RoleName::ServerAdmin  => true,  // god mode
            RoleName::TenantAdmin  => !matches!(action, Action::ManageServer),
            RoleName::DbAdmin      => matches!(action, Action::Read | Action::Write | Action::Admin),
            RoleName::ReadWrite    => matches!(action, Action::Read | Action::Write),
            RoleName::ReadOnly     => matches!(action, Action::Read),
        }
    }
}

impl RoleAssignment {
    pub fn permits(&self, action: &Action, target: &ResourceScope) -> bool {
        // 1. Role must permit the action type
        if !self.role.permits(action) {
            return false;
        }
        // 2. Scope must cover the target resource
        self.scope.covers(target)
    }
}
```

### Scope coverage rules

| Assignment scope | Covers... |
|-----------------|-----------|
| `Server` | Everything |
| `Tenant` | All databases and collections in the tenant |
| `AllDatabases` | All databases (but not user management) |
| `Database("X")` | Database X and all its collections |
| `AllCollections("X")` | All collections in database X |
| `Collection("X", "Y")` | Only collection Y in database X |

### Session context

```rust
pub struct SessionContext {
    pub claims: Option<Claims>,       // None before LOGIN
    pub current_db: Option<String>,   // None before USE
}

impl SessionContext {
    /// Commands allowed before authentication.
    const PRE_AUTH_COMMANDS: &[&str] = &["LOGIN", "PING", "QUIT"];

    pub fn authorize(&self, command: &Command) -> Result<()> {
        // 1. Pre-auth commands always allowed
        if command.is_pre_auth() {
            return Ok(());
        }

        // 2. Must be authenticated
        let claims = self.claims.as_ref()
            .ok_or(GrumpyError::NotAuthenticated)?;

        // 3. Token not expired (already verified by JWT decode, but belt-and-suspenders)
        // 4. Check action against roles
        let action = command.required_action();
        let resource = command.target_resource(self.current_db.as_deref());

        let permitted = claims.roles.iter()
            .any(|ra| ra.permits(&action, &resource));

        if permitted {
            Ok(())
        } else {
            Err(GrumpyError::AccessDenied(format!(
                "{:?} on {:?} denied for user '{}'",
                action, resource, claims.sub
            )))
        }
    }
}
```

### Auth store on disk

```
_auth/
  secret.key                          ← 32 raw bytes (HMAC secret)
  users/
    acme__alice.json                  ← { username, tenant, password_hash, roles, created_at }
    acme__bob.json
    _system__admin.json               ← server_admin bootstrap user
```

- **Secret key**: generated once with `rand::thread_rng().gen::<[u8; 32]>()`
- **User files**: JSON, named `<tenant>__<username>.json`
- **Bootstrap**: if `_auth/users/` is empty, create a `_system__admin` user with `server_admin` role

### Security invariants

1. Passwords are **never** stored, logged, or returned in responses
2. JWT secret is **never** sent over the wire
3. Failed auth returns the same error for "wrong password" and "user not found" (prevent user enumeration)
4. Tenant isolation: user JWT contains tenant → all operations scoped to that tenant
5. `server_admin` is the only role that bypasses tenant scoping

## Common mistakes to avoid

1. **Leaking password in error messages** — always use generic "invalid credentials"
2. **Not checking token expiration** — `jsonwebtoken` checks it by default, but verify `validate_exp = true`
3. **Comparing tokens as strings** — always decode + verify signature
4. **Forgetting scope check** — a `ReadWrite` on `Database("X")` must NOT access `Database("Y")`
5. **Mutable secret** — the HMAC secret is generated once and never changes (key rotation is v2)
