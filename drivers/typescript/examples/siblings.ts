import { GrumpyClient } from "../src/index.js";

async function main(): Promise<void> {
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
  const siblings = await db.getWithSiblings("tasks", "k1");

  if (siblings.length === 0) {
    console.log("key not found");
  } else {
    for (const sibling of siblings) {
      console.log("value:", sibling.value, "vectorClock:", sibling.vectorClock);
    }
  }

  await client.close();
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
