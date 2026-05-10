# Migrating from v4 to v5

This guide summarizes the main breaking and operational changes when upgrading
GrumpyDB from v4.x to v5.0.0.

## 1. Version and packaging

- Rust crates moved to `5.0.0` major versions.
- TypeScript package `@grumpydb/client` moved to `5.0.0`.

## 2. JWT defaults changed to RS256

- New v5 deployments default to `RS256` JWT signing.
- Public keys are exposed at `GET /.well-known/jwks.json` on the HTTP port.
- Existing v4 data directories keep their configured algorithm for backward compatibility.

Recommended:

- Keep the HTTP endpoint reachable by trusted clients that verify tokens.
- Configure driver-side JWKS verification:
  - Rust: set `GrumpyClient::set_jwks_url(...)` before `login(...)`.
  - TypeScript: pass `jwksUrl` in `connect(...)` / `connectCluster(...)`.

## 3. Cluster topology and routing hints

- `TOPOLOGY` is a first-class API for ring/peer metadata.
- Clients can bootstrap from multiple seeds and refresh topology.
- In v5, writer ownership remains single-writer by assignment (`cluster.writers`).

## 4. Sibling API contract (v5)

- `get_with_siblings` / `getWithSiblings` are available for app-level reconciliation.
- Current v5 engine behavior returns at most one sibling (LWW) with an empty vector-clock token.
- Multi-sibling conflict surfaces are planned for future versions.

## 4.1 QUERY / QUERYRANGE convergence sentinel (driver guidance)

Starting with the Phase 44f schema-gossip work, verified `QUERY` /
`QUERYRANGE` responses with effective `R≥2` may include a sentinel
entry of the form:

```text
_warning convergence: 2 peer(s) not yet materialized: [nodeB,nodeE]
```

This sentinel is appended to the trailing `Response::Array` when one or
more peers were skipped because their local materializer had not yet
caught up with a known cluster-wide index (`index_not_yet_materialized:`
acceptor-side error). The leading `_` is intentional: real result rows
are always `<uuid> {json}` pairs and UUIDs never start with `_`, so the
sentinel is unambiguously distinguishable.

**Driver guidance:**

- v4 drivers (and any client unaware of the sentinel) can safely treat
  it as an opaque bulk string and ignore it — it will never collide
  with a UUID-shaped row.
- v5+ drivers SHOULD either filter out array entries that begin with a
  leading underscore, or surface them as a typed convergence warning
  to the application so it can choose to retry the query.
- The sentinel is non-fatal: rows preceding it are valid verified
  matches; only the result set may be incomplete with respect to the
  skipped peers.

See [`docs/SCHEMA_GOSSIP.md`](SCHEMA_GOSSIP.md) §5.5 for the full
acceptor / coordinator / retry contract.

## 5. License model

- Project licensing is now dual: `MIT OR Apache-2.0`.

## 6. Upgrade checklist

1. Back up data directories and auth material.
2. Upgrade binaries/crates/packages to `5.0.0`.
3. Validate auth mode and, if moving to RS256, verify JWKS endpoint exposure.
4. Update client config to use topology seeds and JWKS verification.
5. Run smoke tests:
   - login/refresh flow
   - CRUD + index queries
   - TOPOLOGY and cluster forwarding behavior
6. Roll out progressively and monitor `/healthz`, `/readyz`, `/metrics`.
