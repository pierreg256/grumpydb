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
    this.socket.write(encodeCommand(cmd));
    return this.readResponse();
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
}
