import { Connection } from "./connection.js";
import type { Response, Value } from "./types.js";
import { ServerError } from "./errors.js";

/** Handle scoped to a specific database. */
export class DatabaseHandle {
  private conn: Connection;
  private db: string;
  private initialized = false;

  constructor(conn: Connection, db: string) {
    this.conn = conn;
    this.db = db;
  }

  private async ensureDb(): Promise<void> {
    if (!this.initialized) {
      expectOk(await this.conn.execute(`USE ${this.db}`));
      this.initialized = true;
    }
  }

  // ── CRUD ──────────────────────────────────────────────────

  async insert(collection: string, key: string, value: Value): Promise<void> {
    await this.ensureDb();
    const json = JSON.stringify(value);
    expectOk(
      await this.conn.execute(`INSERT ${collection} ${key} ${json}`),
    );
  }

  async get(collection: string, key: string): Promise<Value | null> {
    await this.ensureDb();
    const resp = await this.conn.execute(`GET ${collection} ${key}`);
    if (resp.type === "bulk") {
      return resp.data === null ? null : JSON.parse(resp.data);
    }
    if (resp.type === "error") throw new ServerError(resp.message);
    throw new ServerError("unexpected response");
  }

  /**
   * Return siblings for app-level reconciliation.
   *
   * v5 returns at most one sibling with an empty vector-clock token.
   */
  async getWithSiblings(
    collection: string,
    key: string,
  ): Promise<Array<{ value: Value; vectorClock: string }>> {
    const value = await this.get(collection, key);
    if (value === null) {
      return [];
    }
    return [{ value, vectorClock: "{}" }];
  }

  async update(collection: string, key: string, value: Value): Promise<void> {
    await this.ensureDb();
    const json = JSON.stringify(value);
    expectOk(
      await this.conn.execute(`UPDATE ${collection} ${key} ${json}`),
    );
  }

  async delete(collection: string, key: string): Promise<void> {
    await this.ensureDb();
    expectOk(await this.conn.execute(`DELETE ${collection} ${key}`));
  }

  async scan(
    collection: string,
    options?: { start?: string; end?: string },
  ): Promise<Array<{ key: string; value: Value }>> {
    await this.ensureDb();
    let cmd = `SCAN ${collection}`;
    if (options?.start && options?.end) {
      cmd += ` ${options.start} ${options.end}`;
    }
    const resp = await this.conn.execute(cmd);
    return parseKvArray(resp);
  }

  async count(collection: string): Promise<number> {
    await this.ensureDb();
    const resp = await this.conn.execute(`COUNT ${collection}`);
    if (resp.type === "integer") return resp.value;
    if (resp.type === "error") throw new ServerError(resp.message);
    throw new ServerError("unexpected response");
  }

  // ── Collection management ─────────────────────────────────

  async createCollection(name: string): Promise<void> {
    await this.ensureDb();
    expectOk(await this.conn.execute(`CREATE COLLECTION ${name}`));
  }

  async dropCollection(name: string): Promise<void> {
    await this.ensureDb();
    expectOk(await this.conn.execute(`DROP COLLECTION ${name}`));
  }

  async listCollections(): Promise<string[]> {
    await this.ensureDb();
    return expectStringArray(await this.conn.execute("LIST COLLECTIONS"));
  }

  // ── Index management ──────────────────────────────────────

  async createIndex(
    collection: string,
    indexName: string,
    fieldPath: string,
  ): Promise<void> {
    await this.ensureDb();
    expectOk(
      await this.conn.execute(
        `CREATE INDEX ${collection} ${indexName} ${fieldPath}`,
      ),
    );
  }

  async dropIndex(collection: string, indexName: string): Promise<void> {
    await this.ensureDb();
    expectOk(
      await this.conn.execute(`DROP INDEX ${collection} ${indexName}`),
    );
  }

  async listIndexes(collection: string): Promise<string[]> {
    await this.ensureDb();
    return expectStringArray(
      await this.conn.execute(`LIST INDEXES ${collection}`),
    );
  }

  async query(
    collection: string,
    indexName: string,
    value: Value,
  ): Promise<Array<{ key: string; value: Value }>> {
    await this.ensureDb();
    const json = JSON.stringify(value);
    const resp = await this.conn.execute(
      `QUERY ${collection} ${indexName} ${json}`,
    );
    return parseKvArray(resp);
  }

  async queryRange(
    collection: string,
    indexName: string,
    start: Value,
    end: Value,
  ): Promise<Array<{ key: string; value: Value }>> {
    await this.ensureDb();
    const resp = await this.conn.execute(
      `QUERYRANGE ${collection} ${indexName} ${JSON.stringify(start)} ${JSON.stringify(end)}`,
    );
    return parseKvArray(resp);
  }

  // ── Maintenance ───────────────────────────────────────────

  async compact(collection: string): Promise<void> {
    await this.ensureDb();
    expectOk(await this.conn.execute(`COMPACT ${collection}`));
  }

  async flush(): Promise<void> {
    await this.ensureDb();
    expectOk(await this.conn.execute("FLUSH"));
  }
}

// ── Helpers ─────────────────────────────────────────────────

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

function parseKvArray(
  resp: Response,
): Array<{ key: string; value: Value }> {
  if (resp.type === "error") throw new ServerError(resp.message);
  if (resp.type !== "array") throw new ServerError("expected array");
  return resp.items
    .filter((i): i is Extract<Response, { type: "bulk" }> => i.type === "bulk")
    .map((i) => {
      const data = i.data ?? "";
      const spaceIdx = data.indexOf(" ");
      if (spaceIdx < 0) return { key: data, value: null };
      const key = data.slice(0, spaceIdx);
      const value = JSON.parse(data.slice(spaceIdx + 1));
      return { key, value };
    });
}
