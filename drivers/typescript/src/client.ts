import { Connection } from "./connection.js";
import { DatabaseHandle } from "./database.js";
import type { ConnectOptions, Response, UserInfo } from "./types.js";
import { AuthError, ServerError } from "./errors.js";

/** GrumpyDB client. */
export class GrumpyClient {
  private conn: Connection;
  private accessToken: string | null = null;
  private refreshToken: string | null = null;

  private constructor(conn: Connection) {
    this.conn = conn;
  }

  /** Connect and authenticate to a GrumpyDB server. */
  static async connect(options: ConnectOptions): Promise<GrumpyClient> {
    const conn = await Connection.connect(
      options.host,
      options.port,
      options.tls ?? false,
      options.rejectUnauthorized ?? false,
      options.ca,
    );

    const client = new GrumpyClient(conn);
    await client.login(options.tenant, options.username, options.password);
    return client;
  }

  private async login(
    tenant: string,
    username: string,
    password: string,
  ): Promise<void> {
    const resp = await this.conn.execute(
      `LOGIN ${tenant} ${username} ${password}`,
    );
    if (resp.type === "ok" && resp.message.startsWith("TOKEN ")) {
      const parts = resp.message.slice(6).split(" ");
      this.accessToken = parts[0];
      this.refreshToken = parts[1] ?? null;
      // Set session token
      await this.conn.execute(`TOKEN ${this.accessToken}`);
    } else if (resp.type === "error") {
      throw new AuthError(resp.message);
    }
  }

  /** Select a database. */
  database(name: string): DatabaseHandle {
    return new DatabaseHandle(this.conn, name);
  }

  /** Create a database. */
  async createDatabase(name: string): Promise<void> {
    expectOk(await this.conn.execute(`CREATE DATABASE ${name}`));
  }

  /** Drop a database. */
  async dropDatabase(name: string): Promise<void> {
    expectOk(await this.conn.execute(`DROP DATABASE ${name}`));
  }

  /** List databases. */
  async listDatabases(): Promise<string[]> {
    return expectStringArray(await this.conn.execute("LIST DATABASES"));
  }

  /** Get session info. */
  async whoami(): Promise<UserInfo> {
    const resp = await this.conn.execute("WHOAMI");
    if (resp.type === "ok") {
      // Parse "USER alice TENANT acme ROLES ..."
      const parts = resp.message.split(" ");
      return {
        username: parts[1] ?? "",
        tenant: parts[3] ?? "",
        roles: parts.slice(5).join(" "),
      };
    }
    throw new ServerError(resp.type === "error" ? resp.message : "unexpected");
  }

  /** Close the connection. */
  async close(): Promise<void> {
    try {
      await this.conn.execute("QUIT");
    } catch {
      // Ignore errors on close
    }
    this.conn.close();
  }
}

function expectOk(resp: Response): void {
  if (resp.type === "error") throw new ServerError(resp.message);
  if (resp.type !== "ok") throw new ServerError("unexpected response");
}

function expectStringArray(resp: Response): string[] {
  if (resp.type === "error") throw new ServerError(resp.message);
  if (resp.type !== "array") throw new ServerError("expected array");
  return resp.items
    .filter((i): i is Extract<Response, { type: "bulk" }> => i.type === "bulk")
    .map((i) => i.data ?? "");
}
