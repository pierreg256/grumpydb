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
  jwksUrl?: string;
}

/** Cluster connect options (seed list instead of a single host/port). */
export interface ClusterConnectOptions {
  seeds: string[];
  tls?: boolean;
  rejectUnauthorized?: boolean;
  ca?: string | Buffer;
  tenant: string;
  username: string;
  password: string;
  jwksUrl?: string;
}

/** Topology peer entry. */
export interface TopologyPeer {
  node_id: string;
  addr: string;
}

/** Topology writer entry. */
export interface TopologyWriter {
  collection: string;
  node_id: string;
}

/** Cluster topology payload returned by TOPOLOGY. */
export interface ClusterTopology {
  cluster_id: string;
  local_node_id: string;
  n: number;
  vnodes_per_node: number;
  peers: TopologyPeer[];
  writers?: TopologyWriter[];
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
