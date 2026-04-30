import { GrumpyClient } from "../src/index.js";

async function main(): Promise<void> {
  const client = await GrumpyClient.connectCluster({
    seeds: ["127.0.0.1:6380", "127.0.0.1:6381", "127.0.0.1:6382"],
    tenant: "acme",
    username: "admin",
    password: "admin",
    tls: false,
    jwksUrl: "http://127.0.0.1:8081/.well-known/jwks.json",
  });

  const topology = await client.topology();
  console.log("cluster:", topology.cluster_id, "peers:", topology.peers.length);

  const db = client.database("app");
  await db.createCollection("tasks");
  await db.insert("tasks", "k1", { title: "hello", done: false });

  const value = await db.get("tasks", "k1");
  console.log("task k1:", value);

  await client.close();
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
