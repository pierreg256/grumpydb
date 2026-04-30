import { Connection } from "./connection.js";
import { DatabaseHandle } from "./database.js";
import type {
  ClusterConnectOptions,
  ClusterTopology,
  ConnectOptions,
  Response,
  UserInfo,
} from "./types.js";
import { AuthError, ServerError } from "./errors.js";
import { JwksCache } from "./jwks.js";

/** GrumpyDB client. */
export class GrumpyClient {
  private conn: Connection;
  private accessToken: string | null = null;
  private refreshToken: string | null = null;
  private topologyCache: ClusterTopology | null = null;
  private jwksCache: JwksCache | null = null;

  private constructor(conn: Connection, jwksUrl?: string) {
    this.conn = conn;
    if (jwksUrl) {
      this.jwksCache = new JwksCache(jwksUrl);
    }
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

    const client = new GrumpyClient(conn, options.jwksUrl);
    await client.login(options.tenant, options.username, options.password);
    return client;
  }

  /** Connect to a cluster via one or more `host:port` seeds. */
  static async connectCluster(
    options: ClusterConnectOptions,
  ): Promise<GrumpyClient> {
    if (options.seeds.length === 0) {
      throw new ServerError("connectCluster requires at least one seed");
    }

    let lastErr: unknown = null;
    for (const seed of options.seeds) {
      const parsed = parseSeed(seed);
      if (!parsed) {
        lastErr = new ServerError(`invalid seed '${seed}', expected host:port`);
        continue;
      }

      try {
        const conn = await Connection.connect(
          parsed.host,
          parsed.port,
          options.tls ?? false,
          options.rejectUnauthorized ?? false,
          options.ca,
        );
        const client = new GrumpyClient(conn, options.jwksUrl);
        await client.login(options.tenant, options.username, options.password);
        return client;
      } catch (e) {
        lastErr = e;
      }
    }

    throw lastErr ?? new ServerError("no reachable cluster seed");
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

      if (this.jwksCache && this.accessToken) {
        await this.jwksCache.verifyAccessToken(this.accessToken);
      }

      // Set session token
      await this.conn.execute(`TOKEN ${this.accessToken}`);
      try {
        await this.refreshTopology();
      } catch {
        // Keep login successful even when TOPOLOGY is unavailable.
      }
    } else if (resp.type === "error") {
      throw new AuthError(resp.message);
    }
  }

  /** Fetch latest TOPOLOGY and refresh local cache. */
  async refreshTopology(): Promise<ClusterTopology> {
    const resp = await this.conn.execute("TOPOLOGY");
    if (resp.type === "error") throw new ServerError(resp.message);
    if (resp.type !== "bulk" || resp.data == null) {
      throw new ServerError("unexpected TOPOLOGY response");
    }
    const parsed = JSON.parse(resp.data) as ClusterTopology;
    this.topologyCache = parsed;
    return parsed;
  }

  /** Return topology from cache or fetch from server when needed. */
  async topology(): Promise<ClusterTopology> {
    if (this.topologyCache) {
      return this.topologyCache;
    }
    return this.refreshTopology();
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

function parseSeed(seed: string): { host: string; port: number } | null {
  const idx = seed.lastIndexOf(":");
  if (idx <= 0 || idx >= seed.length - 1) {
    return null;
  }
  const host = seed.slice(0, idx);
  const port = Number(seed.slice(idx + 1));
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    return null;
  }
  return { host, port };
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
