import { createSign, generateKeyPairSync } from "node:crypto";
import { describe, expect, it, vi } from "vitest";

import { AuthError } from "./errors.js";
import { JwksCache } from "./jwks.js";

function toBase64Url(input: Buffer | string): string {
  const buf = Buffer.isBuffer(input) ? input : Buffer.from(input, "utf8");
  return buf
    .toString("base64")
    .replace(/=/g, "")
    .replace(/\+/g, "-")
    .replace(/\//g, "_");
}

function signJwtRS256(
  privatePem: string,
  kid: string,
  exp: number,
): string {
  const header = toBase64Url(JSON.stringify({ alg: "RS256", typ: "JWT", kid }));
  const payload = toBase64Url(JSON.stringify({ sub: "alice", exp }));
  const signingInput = `${header}.${payload}`;

  const signer = createSign("RSA-SHA256");
  signer.update(signingInput);
  signer.end();
  const signature = signer.sign(privatePem);

  return `${signingInput}.${toBase64Url(signature)}`;
}

describe("JwksCache", () => {
  it("verifies a valid RS256 token", async () => {
    const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
    const privatePem = privateKey.export({ type: "pkcs8", format: "pem" }).toString();
    const publicJwk = publicKey.export({ format: "jwk" }) as JsonWebKey;

    const kid = "kid-1";
    const token = signJwtRS256(privatePem, kid, Math.floor(Date.now() / 1000) + 120);

    const fetchMock = vi.fn(async () => ({
      ok: true,
      status: 200,
      json: async () => ({
        keys: [
          {
            kty: "RSA",
            alg: "RS256",
            use: "sig",
            kid,
            n: publicJwk.n,
            e: publicJwk.e,
          },
        ],
      }),
    }));

    vi.stubGlobal("fetch", fetchMock);
    const cache = new JwksCache("http://127.0.0.1:8081/.well-known/jwks.json");

    await expect(cache.verifyAccessToken(token)).resolves.toBeUndefined();
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("fails when kid is not in JWKS", async () => {
    const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
    const privatePem = privateKey.export({ type: "pkcs8", format: "pem" }).toString();
    const token = signJwtRS256(privatePem, "missing-kid", Math.floor(Date.now() / 1000) + 120);

    const fetchMock = vi.fn(async () => ({
      ok: true,
      status: 200,
      json: async () => ({ keys: [] }),
    }));

    vi.stubGlobal("fetch", fetchMock);
    const cache = new JwksCache("http://127.0.0.1:8081/.well-known/jwks.json");

    await expect(cache.verifyAccessToken(token)).rejects.toBeInstanceOf(AuthError);
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });
});
