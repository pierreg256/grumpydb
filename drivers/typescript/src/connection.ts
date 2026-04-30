import * as net from "node:net";
import * as tls from "node:tls";
import * as readline from "node:readline";
import type { Response } from "./types.js";
import { encodeCommand, parseResponseLine } from "./protocol.js";
import { ConnectionError, ProtocolError } from "./errors.js";

/** Low-level connection to a GrumpyDB server. */
export class Connection {
  private socket: net.Socket;
  private rl: readline.Interface;
  private lineQueue: string[] = [];
  private lineResolve: ((line: string) => void) | null = null;
  private useTls = false;
  private rejectUnauthorized = false;
  private ca?: string | Buffer;
  private sessionToken: string | null = null;
  private currentDb: string | null = null;

  private constructor(socket: net.Socket) {
    this.socket = socket;
    this.rl = readline.createInterface({ input: socket, crlfDelay: Infinity });
    this.rl.on("line", (line: string) => {
      if (this.lineResolve) {
        const resolve = this.lineResolve;
        this.lineResolve = null;
        resolve(line);
      } else {
        this.lineQueue.push(line);
      }
    });
  }

  /** Connect to a GrumpyDB server. */
  static async connect(
    host: string,
    port: number,
    useTls: boolean,
    rejectUnauthorized = false,
    ca?: string | Buffer,
  ): Promise<Connection> {
    return new Promise((resolve, reject) => {
      const onConnect = () => {
        const conn = new Connection(socket);
        conn.useTls = useTls;
        conn.rejectUnauthorized = rejectUnauthorized;
        conn.ca = ca;
        // Read server banner
        conn.readLine().then(() => resolve(conn)).catch(reject);
      };

      let socket: net.Socket;
      if (useTls) {
        socket = tls.connect(
          { host, port, rejectUnauthorized, ca },
          onConnect,
        );
      } else {
        socket = net.connect({ host, port }, onConnect);
      }
      socket.once("error", (err: Error) =>
        reject(new ConnectionError(err.message)),
      );
    });
  }

  /** Send a command and read the response. */
  async execute(cmd: string): Promise<Response> {
    return this.executeInternal(cmd, true);
  }

  private async executeInternal(cmd: string, allowForward: boolean): Promise<Response> {
    this.socket.write(encodeCommand(cmd));
    this.updateSessionState(cmd);
    const resp = await this.readResponse();

    if (allowForward && resp.type === "error") {
      const target = parseForwardTarget(resp.message);
      if (target) {
        const fwd = await Connection.connect(
          target.host,
          target.port,
          this.useTls,
          this.rejectUnauthorized,
          this.ca,
        );
        if (this.sessionToken) {
          await fwd.executeInternal(`TOKEN ${this.sessionToken}`, false);
        }
        if (this.currentDb) {
          await fwd.executeInternal(`USE ${this.currentDb}`, false);
        }
        return fwd.executeInternal(cmd, false);
      }
    }

    return resp;
  }

  /** Close the connection. */
  close(): void {
    this.rl.close();
    this.socket.destroy();
  }

  private readLine(): Promise<string> {
    if (this.lineQueue.length > 0) {
      return Promise.resolve(this.lineQueue.shift()!);
    }
    return new Promise((resolve) => {
      this.lineResolve = resolve;
    });
  }

  private async readResponse(): Promise<Response> {
    const line = await this.readLine();
    const prefix = line[0];

    if (prefix === "$") {
      const len = parseInt(line.slice(1), 10);
      if (len < 0) return { type: "bulk", data: null };
      // Read the data line
      const dataLine = await this.readLine();
      return { type: "bulk", data: dataLine };
    }

    if (prefix === "*") {
      const count = parseInt(line.slice(1), 10);
      const items: Response[] = [];
      for (let i = 0; i < count; i++) {
        items.push(await this.readResponse());
      }
      return { type: "array", items };
    }

    return parseResponseLine(line + "\r\n");
  }

  private updateSessionState(cmd: string): void {
    const trimmed = cmd.trim();
    if (trimmed.startsWith("TOKEN ")) {
      const token = trimmed.slice("TOKEN ".length).trim();
      if (token.length > 0) {
        this.sessionToken = token;
      }
      return;
    }
    if (trimmed.startsWith("USE ")) {
      const db = trimmed.slice("USE ".length).trim();
      if (db.length > 0) {
        this.currentDb = db;
      }
    }
  }
}

function parseForwardTarget(message: string): { host: string; port: number } | null {
  const atIdx = message.indexOf("@");
  const semiIdx = message.indexOf(";");
  if (atIdx < 0 || semiIdx < 0 || semiIdx <= atIdx + 1) {
    return null;
  }
  const addr = message.slice(atIdx + 1, semiIdx).trim();
  const sep = addr.lastIndexOf(":");
  if (sep <= 0 || sep >= addr.length - 1) {
    return null;
  }
  const host = addr.slice(0, sep);
  const port = Number(addr.slice(sep + 1));
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    return null;
  }
  return { host, port };
}
