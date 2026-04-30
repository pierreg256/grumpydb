# @grumpydb/client

TypeScript client driver for GrumpyDB.

## Install

```bash
npm install @grumpydb/client
```

## Quick Start

```typescript
import { GrumpyClient } from "@grumpydb/client";

const client = await GrumpyClient.connect({
  host: "127.0.0.1",
  port: 6380,
  tenant: "acme",
  username: "admin",
  password: "admin",
  tls: false,
  jwksUrl: "http://127.0.0.1:8081/.well-known/jwks.json",
});

const db = client.database("app");
await db.insert("tasks", "k1", { title: "hello" });
console.log(await db.get("tasks", "k1"));
await client.close();
```

## Cluster Bootstrap

Use `connectCluster` with one or more seeds. The client tries seeds in order
until one connection/login succeeds.

```typescript
const client = await GrumpyClient.connectCluster({
  seeds: ["127.0.0.1:6380", "127.0.0.1:6381", "127.0.0.1:6382"],
  tenant: "acme",
  username: "admin",
  password: "admin",
  tls: false,
  jwksUrl: "http://127.0.0.1:8081/.well-known/jwks.json",
});
```

## JWKS Verification

When `jwksUrl` is configured, the driver verifies LOGIN access tokens with
RS256 against the server JWKS endpoint. On unknown `kid`, the JWKS is refreshed
and verification is retried once.

## Examples

- `examples/cluster.ts`
- `examples/siblings.ts`

## Development

```bash
npm ci
npm run lint
npm test
npm run build
```

## License

Dual-licensed under MIT or Apache-2.0, at your option.
