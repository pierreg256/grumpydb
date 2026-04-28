/** Connection options. */
export interface ConnectOptions {
  host: string;
  port: number;
  tls?: boolean;
  rejectUnauthorized?: boolean;
  ca?: string | Buffer;
  tenant: string;
  username: string;
  password: string;
}

/** Session info returned by whoami(). */
export interface UserInfo {
  username: string;
  tenant: string;
  roles: string;
}

/** Parsed RESP response. */
export type Response =
  | { type: "ok"; message: string }
  | { type: "error"; message: string }
  | { type: "integer"; value: number }
  | { type: "bulk"; data: string | null }
  | { type: "array"; items: Response[] };

/** Generic JSON value. */
export type Value =
  | string
  | number
  | boolean
  | null
  | Value[]
  | { [key: string]: Value };
