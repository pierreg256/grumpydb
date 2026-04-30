import { createPublicKey, verify as verifySignature } from "node:crypto";

import { AuthError } from "./errors.js";

type JwtHeader = {
  alg?: string;
  kid?: string;
};

type JwtPayload = {
  exp?: number;
};

type Jwk = {
  kty: string;
  alg?: string;
  use?: string;
  kid: string;
  n: string;
  e: string;
};

type JwksDocument = {
  keys: Jwk[];
};

function decodeBase64Url(input: string): Buffer {
  const normalized = input.replace(/-/g, "+").replace(/_/g, "/");
  const pad = normalized.length % 4;
  const padded = pad === 0 ? normalized : normalized + "=".repeat(4 - pad);
  return Buffer.from(padded, "base64");
}

function parseJsonSegment<T>(segment: string): T {
  return JSON.parse(decodeBase64Url(segment).toString("utf8")) as T;
}

export class JwksCache {
  private url: string;
  private byKid = new Map<string, Jwk>();

  constructor(url: string) {
    this.url = url;
  }

  async verifyAccessToken(token: string): Promise<void> {
    const parts = token.split(".");
    if (parts.length !== 3) {
      throw new AuthError("invalid JWT format");
    }

    const [headerSeg, payloadSeg, sigSeg] = parts;
    const header = parseJsonSegment<JwtHeader>(headerSeg);
    const payload = parseJsonSegment<JwtPayload>(payloadSeg);

    if (header.alg !== "RS256") {
      throw new AuthError(`unsupported JWT alg '${header.alg ?? "unknown"}'`);
    }
    if (!header.kid) {
      throw new AuthError("JWT header missing kid");
    }

    if (!this.byKid.has(header.kid)) {
      await this.refresh();
    }
    if (!this.byKid.has(header.kid)) {
      await this.refresh();
    }

    const jwk = this.byKid.get(header.kid);
    if (!jwk) {
      throw new AuthError(`kid '${header.kid}' not found in JWKS`);
    }

    const signingInput = Buffer.from(`${headerSeg}.${payloadSeg}`, "utf8");
    const signature = decodeBase64Url(sigSeg);
    const publicKey = createPublicKey({ key: jwk, format: "jwk" });

    const valid = verifySignature("RSA-SHA256", signingInput, publicKey, signature);
    if (!valid) {
      throw new AuthError("JWT RS256 signature verification failed");
    }

    if (typeof payload.exp === "number") {
      const now = Math.floor(Date.now() / 1000);
      if (payload.exp <= now) {
        throw new AuthError("JWT token expired");
      }
    }
  }

  private async refresh(): Promise<void> {
    const res = await fetch(this.url);
    if (!res.ok) {
      throw new AuthError(`JWKS fetch failed with HTTP ${res.status}`);
    }

    const doc = (await res.json()) as JwksDocument;
    this.byKid.clear();

    for (const key of doc.keys ?? []) {
      if (key.kty !== "RSA") {
        continue;
      }
      if (key.alg && key.alg !== "RS256") {
        continue;
      }
      if (key.use && key.use !== "sig") {
        continue;
      }
      this.byKid.set(key.kid, key);
    }
  }
}
