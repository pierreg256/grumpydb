import type { Response } from "./types.js";
import { ProtocolError } from "./errors.js";

/** Encode a command line for the wire. */
export function encodeCommand(cmd: string): string {
  return cmd.endsWith("\r\n") ? cmd : cmd + "\r\n";
}

/** Parse a single-line RESP response. */
export function parseResponseLine(line: string): Response {
  if (line.length === 0) throw new ProtocolError("empty response");

  const prefix = line[0];
  const body = line.slice(1).replace(/\r?\n$/, "");

  switch (prefix) {
    case "+":
      return { type: "ok", message: body };
    case "-": {
      const msg = body.startsWith("ERR ") ? body.slice(4) : body;
      return { type: "error", message: msg };
    }
    case ":":
      return { type: "integer", value: parseInt(body, 10) };
    case "$": {
      const len = parseInt(body, 10);
      if (len < 0) return { type: "bulk", data: null };
      // Bulk data follows on next line(s) — handled by Connection
      return { type: "bulk", data: "" }; // placeholder
    }
    case "*": {
      const count = parseInt(body, 10);
      // Array elements follow — handled by Connection
      return { type: "array", items: new Array(count) };
    }
    default:
      throw new ProtocolError(`unknown response prefix: ${prefix}`);
  }
}
