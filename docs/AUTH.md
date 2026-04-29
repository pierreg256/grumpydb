# GrumpyDB — Authentication & Authorisation

This document describes how GrumpyDB authenticates clients (JWT) and
authorises commands (RBAC). It covers both the legacy HS256 path and the
default-since-v5 RS256 + JWKS path.

## Overview

| Layer | Mechanism |
|---|---|
| Transport | TCP + TLS 1.3 (rustls), self-signed cert auto-generated for dev |
| Authentication | JWT bearer token: `LOGIN tenant user password` → access + refresh |
| Authorisation | Role-based (RBAC): `Action × Resource` checked before every command |
| Inter-node auth | Special `cluster_peer` role + `Action::ReplicationPeer` (Phase 40+) |

## JWT signing algorithm

GrumpyDB v5 supports two algorithms:

| Algorithm | Status | Use case |
|---|---|---|
| `HS256` | Legacy (v4 deployments) | Single-node, symmetric secret in `_auth/secret.key` |
| `RS256` | Default for new deployments | Distributed: any node verifies tokens issued by any other node using only the public key (JWKS) |

The active algorithm is recorded in `_auth/jwt/config.json`.

### RS256 layout on disk

```
_auth/
├── secret.key                ← legacy HS256 secret (kept for migration)
├── jwt/
│   ├── config.json           ← which algorithm is active, TTLs, kid pointers
│   ├── cluster_peer.token    ← long-lived inter-node auth token
│   └── keys/
│       ├── <kid_current>.pem      ← private PEM (chmod 600)
│       ├── <kid_current>.pub.pem  ← public PEM (chmod 644)
│       ├── <kid_next>.pem
│       └── <kid_next>.pub.pem
└── users/
    └── <tenant>__<username>.json
```

Key facts:

- **Two keys in the keyring at all times**: `current` (used to sign new tokens)
  and `next` (already published in JWKS so a planned rotation does not break
  in-flight verifiers).
- **`kid`** = `hex(sha256(spki_der)[..8])` — short, stable, unique per key.
- **Permissions** are tightened automatically: private PEMs become `0600`,
  public PEMs become `0644`. Detected loose permissions are tightened on
  startup with a warning.

## JWKS endpoint

The HTTP observability server (port 6381 by default) exposes the public keyset
at `/.well-known/jwks.json`:

```bash
curl http://localhost:6381/.well-known/jwks.json
```

```json
{
  "keys": [
    {
      "kty": "RSA",
      "alg": "RS256",
      "use": "sig",
      "kid": "a1b2c3d4e5f60718",
      "n": "0vx7agoebGcQSuuPiLJXZptN9nndrQmbX...",
      "e": "AQAB"
    },
    {
      "kty": "RSA",
      "alg": "RS256",
      "use": "sig",
      "kid": "f7e6d5c4b3a29180",
      "n": "...",
      "e": "AQAB"
    }
  ]
}
```

The endpoint is **unauthenticated by design** — it is the *public* keyset.
For HS256 deployments it returns `{"keys": []}` (symmetric secrets must
never be exposed publicly).

## Key rotation

```bash
grumpydb-server auth rotate-jwt-keys --data ./data
```

1. The current `next` key becomes the new `current`.
2. A fresh RSA-2048 keypair is generated and becomes the new `next`.
3. Both keys remain in the JWKS until the previously-current key is older
   than `keep_old_for_secs` (default 7 days).
4. Tokens already in flight continue to verify because the verifier picks
   the matching key by `kid`.

Operationally: rotate keys on a schedule (every 30 / 60 days), then wait
for the longest token TTL (refresh, default 7 days) before deleting old
private PEMs from disk.

## Migration from v4 (HS256) to v5 (RS256)

```bash
grumpydb-server auth migrate --to rs256 --data ./data
```

1. Generates an RS256 keyring next to the existing `secret.key`.
2. Writes `_auth/jwt/config.json` with `algorithm = "rs256"`.
3. **Keeps** `secret.key` so previously-issued HS256 tokens still verify
   until they expire (default access TTL: 1 hour, refresh TTL: 7 days).
4. New tokens are signed with RS256 going forward.

Plan a hard cutover: after the longest TTL window, the operator deletes
`secret.key` and the migration is complete.

## Bootstrap (first-ever start)

The server REFUSES to start without a bootstrap password — `_system/admin/admin`
is no longer a silent default since v5.

```bash
grumpydb-server --data ./data --bootstrap-password 's3cr3t!' --no-tls
# or via env var:
GRUMPYDB_BOOTSTRAP_PASSWORD='s3cr3t!' grumpydb-server --data ./data --no-tls
```

This creates `_system/admin` with the supplied password. Subsequent starts
do NOT need the flag.

## RBAC roles

| Role | Scope | Permits |
|---|---|---|
| `server_admin` | Server | Create/drop tenants, manage all users |
| `tenant_admin` | Tenant | Create/drop databases, manage users within tenant, full CRUD |
| `db_admin` | Database | Create/drop collections, indexes, compact, full CRUD |
| `read_write` | Database / Collection | INSERT, GET, UPDATE, DELETE, SCAN, QUERY |
| `read_only` | Database / Collection | GET, SCAN, QUERY only |
| `cluster_peer` | Server | Inter-node WAL stream replication only — denies user data ops |

The `cluster_peer` role exists from v5 onward. v5 uses it only on a stub
basis (a token is generated and stored at `_auth/jwt/cluster_peer.token`);
Phase 40e wires it into the WAL stream protocol.

## Wire-protocol primitives

| Command | Purpose |
|---|---|
| `LOGIN <tenant> <user> <password>` | Returns `TOKEN <access> <refresh>` |
| `TOKEN <access>` | Resume a session from a previously-issued access token |
| `REFRESH <refresh>` | Swap a refresh token for a fresh access token |
| `WHOAMI` | Returns the current session identity |
| `QUIT` | Closes the connection |

`LOGIN`, `TOKEN`, `REFRESH`, `PING`, and `QUIT` are the only **pre-auth**
commands. Everything else requires a valid session.

## Security defaults

- `secret.key` and private PEMs are `chmod 600` on creation; loose
  permissions on existing files are tightened on startup with a warning.
- `argon2` is used for password hashing.
- Failed login backoff (Phase 35): exponential, capped at 60s. Loopback
  bypass is **on by default** (`bypass_loopback = true`); production
  deployments should set `bypass_loopback = false`.

## See also

- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — the broader server architecture.
- [`docs/IMPLEMENTATION_PLAN_V4.md`](IMPLEMENTATION_PLAN_V4.md) — Phase 39
  details and follow-up phases.
