import { Connection } from "./connection.js";
import type { Response } from "./types.js";
import { ServerError } from "./errors.js";

/** Auth helper: login and manage tokens. */
export async function login(
  conn: Connection,
  tenant: string,
  username: string,
  password: string,
): Promise<{ access: string; refresh: string | null }> {
  const resp = await conn.execute(`LOGIN ${tenant} ${username} ${password}`);
  if (resp.type === "ok" && resp.message.startsWith("TOKEN ")) {
    const parts = resp.message.slice(6).split(" ");
    return { access: parts[0], refresh: parts[1] ?? null };
  }
  if (resp.type === "error") throw new ServerError(resp.message);
  throw new ServerError("unexpected login response");
}

/** Set session token. */
export async function setToken(
  conn: Connection,
  token: string,
): Promise<void> {
  const resp = await conn.execute(`TOKEN ${token}`);
  if (resp.type === "error") throw new ServerError(resp.message);
}

/** Refresh an access token. */
export async function refresh(
  conn: Connection,
  refreshToken: string,
): Promise<string> {
  const resp = await conn.execute(`REFRESH ${refreshToken}`);
  if (resp.type === "ok" && resp.message.startsWith("TOKEN ")) {
    return resp.message.slice(6);
  }
  if (resp.type === "error") throw new ServerError(resp.message);
  throw new ServerError("unexpected refresh response");
}
