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
