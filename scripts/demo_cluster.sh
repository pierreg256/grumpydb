#!/usr/bin/env bash
#
# scripts/demo_cluster.sh
#
# End-to-end demo on the 5-node v5 cluster.
#
# Steps:
#   1) wipe each node's data dir, BUT preserve _cluster/node.json
#   2) start the cluster, wait for /healthz on every node
#   3) on node1: TOPOLOGY, CREATE DATABASE demo_db,
#      ALTER DATABASE demo_db SET CONSISTENCY READ_CONCERN R=2 WRITE_CONCERN W=2
#      (N=3 is auto-derived: min(3, cluster_size)=3 on a 5-node cluster)
#      then USE demo_db, CREATE COLLECTION docs, CREATE INDEX by_name + by_age.
#      Note: CREATE INDEX is replicated by the server to the N replicas of a
#      DDL routing key, NOT to every node — so on a 5-node cluster only 3
#      nodes will end up with the index. The script discovers which nodes
#      have it via LIST INDEXES and routes the final QUERY there.
#   4) INSERT 10 documents {"name": <random>, "age": <random>}, following
#      "forward to <host>:<port>" replies on key-mismatch.
#   5) Per-node SCAN to expose the replica distribution.
#   6) QUERY by_name + by_age routed to a node that owns the index.
#
# Bash 3.2 compatible (macOS-friendly): no associative arrays.
#
# Usage:
#   scripts/demo_cluster.sh
#   GRUMPYDB_BOOTSTRAP_PASSWORD=admin scripts/demo_cluster.sh
#   scripts/demo_cluster.sh --keep-up        # leave the cluster running

set -euo pipefail

COMPOSE_FILE_PATH="${COMPOSE_FILE_PATH:-docker-compose.cluster.yml}"
STARTUP_TIMEOUT_SECONDS="${STARTUP_TIMEOUT_SECONDS:-60}"
DB_NAME="${DB_NAME:-demo_db}"
COLLECTION_NAME="${COLLECTION_NAME:-docs}"
GRUMPYDB_BOOTSTRAP_PASSWORD="${GRUMPYDB_BOOTSTRAP_PASSWORD:-admin}"

DATA_ROOT="docker/cluster/data"

KEEP_UP=0
if [[ "${1:-}" == "--keep-up" ]]; then
  KEEP_UP=1
fi

# Node id (1..5) → host TCP port for the wire protocol.
node_port() {
  case "$1" in
    1) echo 6380 ;;
    2) echo 6382 ;;
    3) echo 6383 ;;
    4) echo 6384 ;;
    5) echo 6385 ;;
    *) return 1 ;;
  esac
}

# Hostname (cluster.peers addr) → host TCP port mapping.
host_port() {
  case "$1" in
    node1) echo 6380 ;;
    node2) echo 6382 ;;
    node3) echo 6383 ;;
    node4) echo 6384 ;;
    node5) echo 6385 ;;
    *) return 1 ;;
  esac
}

node_http() {
  case "$1" in
    1) echo "http://127.0.0.1:8081" ;;
    2) echo "http://127.0.0.1:8082" ;;
    3) echo "http://127.0.0.1:8083" ;;
    4) echo "http://127.0.0.1:8084" ;;
    5) echo "http://127.0.0.1:8085" ;;
    *) return 1 ;;
  esac
}

cleanup() {
  if [[ "$KEEP_UP" -eq 0 ]]; then
    echo
    echo "==> Tearing down cluster"
    docker compose -f "$COMPOSE_FILE_PATH" down -v >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

# ── Helpers ────────────────────────────────────────────────────────────

# Wipe the data dir but keep the node identity (`_cluster/node.json`).
wipe_node_data() {
  local node_dir="$1"
  local id_file="${node_dir}/_cluster/node.json"

  if [[ ! -f "$id_file" ]]; then
    echo "ERROR: identity file missing: $id_file" >&2
    return 1
  fi

  local backup
  backup=$(mktemp)
  cp "$id_file" "$backup"

  rm -rf -- "$node_dir"/* "$node_dir"/.[!.]* "$node_dir"/..?* 2>/dev/null || true
  mkdir -p "${node_dir}/_cluster"
  mv "$backup" "$id_file"
}

wait_http() {
  local url="$1"
  local timeout="$2"
  local label="$3"
  for _ in $(seq 1 "$timeout"); do
    if curl -fsS "${url}/healthz" >/dev/null 2>&1; then
      echo "    ${label} ready"
      return 0
    fi
    sleep 1
  done
  echo "ERROR: ${label} (${url}) did not become ready in ${timeout}s" >&2
  return 1
}

new_uuid() {
  if command -v uuidgen >/dev/null 2>&1; then
    uuidgen | tr '[:upper:]' '[:lower:]'
  else
    printf '00000000-0000-0000-0000-%012d' "$RANDOM$RANDOM"
  fi
}

# Run a command stream against the given TCP port. Logs in, USEs the demo DB,
# then runs every line on stdin, then QUITs. Output is the raw protocol response.
run_session_raw() {
  local port="$1"
  {
    printf 'LOGIN _system admin %s\r\n' "$GRUMPYDB_BOOTSTRAP_PASSWORD"
    printf 'USE %s\r\n' "$DB_NAME"
    cat
    printf 'QUIT\r\n'
  } | nc -w 5 127.0.0.1 "$port"
}

# Run a session and strip noisy lines (banner + JWT TOKEN + BYE).
# This turns each session into something legible.
run_session() {
  local port="$1"
  run_session_raw "$port" \
    | grep -v -E '^\+GRUMPYDB ' \
    | grep -v -E '^\+TOKEN ' \
    | grep -v -E '^\+BYE$'
}

# Run a single command. If response says "forward to <host>:<port>", retry on
# that host. Returns the (final) raw protocol response.
run_cmd_with_forward() {
  local port="$1"
  local cmd="$2"
  local out fwd_host fwd_port

  out=$(printf '%s\r\n' "$cmd" | run_session_raw "$port")

  fwd_host=$(printf '%s\n' "$out" \
    | sed -nE 's/.*forward to [^@]+@([^:; ]+):[0-9]+.*/\1/p' \
    | head -n1)

  if [[ -n "$fwd_host" ]]; then
    if fwd_port=$(host_port "$fwd_host" 2>/dev/null); then
      out=$(printf '%s\r\n' "$cmd" | run_session_raw "$fwd_port")
    fi
  fi

  printf '%s' "$out"
}

# Returns the count of items in a "*N\r\n" RESP array reply read from stdin,
# ignoring the OK responses for LOGIN/USE. Returns "0" when no array is present.
# Tolerates `set -euo pipefail`: grep returns 1 on no match, so the pipeline
# would otherwise abort the script. We strip the trailing `\r` because the
# RESP framing includes CR before the LF.
count_array_items() {
  tr -d '\r' \
    | ( grep -E '^\*[0-9]+$' || true ) \
    | tail -n1 \
    | tr -d '*'
}

# Pick a node id (1..5) that owns the index `idx_name` for the demo collection.
find_node_with_index() {
  local idx_name="$1"
  for n in 1 2 3 4 5; do
    local out
    out=$(printf 'LIST INDEXES %s\r\n' "$COLLECTION_NAME" | run_session_raw "$(node_port "$n")")
    if printf '%s\n' "$out" | grep -qE "^${idx_name}$"; then
      echo "$n"
      return 0
    fi
  done
  return 1
}

# ── 1) Cleanup ─────────────────────────────────────────────────────────

echo "==> Wiping per-node data (keeping _cluster/node.json identity)"
for n in 1 2 3 4 5; do
  wipe_node_data "${DATA_ROOT}/node${n}"
  echo "    cleaned ${DATA_ROOT}/node${n}"
done

# ── 2) Start cluster ───────────────────────────────────────────────────

echo
echo "==> Starting 5-node cluster"
GRUMPYDB_BOOTSTRAP_PASSWORD="$GRUMPYDB_BOOTSTRAP_PASSWORD" \
  docker compose -f "$COMPOSE_FILE_PATH" up -d --build >/dev/null

echo
echo "==> Waiting for /healthz on each node"
for n in 1 2 3 4 5; do
  wait_http "$(node_http "$n")" "$STARTUP_TIMEOUT_SECONDS" "node${n}"
done

# ── 3) Setup: topology, database, collection, indexes ──────────────────

echo
echo "==> Setup on node1: TOPOLOGY, CREATE DATABASE, ALTER CONSISTENCY R=2 W=2"
echo "    CREATE COLLECTION + CREATE INDEX by_name/by_age"
echo "    Phase 44: schema is gossiped, every node will converge automatically"
{
  printf 'TOPOLOGY\r\n'
  printf 'CREATE DATABASE %s\r\n' "$DB_NAME"
  printf 'ALTER DATABASE %s SET CONSISTENCY READ_CONCERN R=2 WRITE_CONCERN W=2\r\n' "$DB_NAME"
  printf 'SHOW DATABASE %s CONSISTENCY\r\n' "$DB_NAME"
  printf 'CREATE COLLECTION %s\r\n' "$COLLECTION_NAME"
  printf 'CREATE INDEX %s by_name name\r\n' "$COLLECTION_NAME"
  printf 'CREATE INDEX %s by_age age\r\n' "$COLLECTION_NAME"
  printf 'SCHEMA VERSION\r\n'
} | run_session "$(node_port 1)"

# CREATE COLLECTION is currently NOT part of the schema gossip
# (Phase 44 covers CREATE/DROP INDEX only; collection DDL follow-up is
# in the v5 backlog). To unblock the demo we explicitly create the
# collection on every node so any forwarded INSERT lands on a node
# that has it. INSERTs received via the peer-RPC path auto-create
# (see ensure_peer_write_target), but client-TCP forwards do not.
for n in 2 3 4 5; do
  printf 'CREATE COLLECTION %s\r\n' "$COLLECTION_NAME" \
    | run_session "$(node_port "$n")" >/dev/null || true
done

# ── 4) Inserts (with forward-follow) ───────────────────────────────────

NAMES=(alice bob charlie dave eve frank grace heidi ivan judy karl laura mallory niaj olivia peggy)
NUM_DOCS=10

DOC_KEYS=()
DOC_NAMES=()
DOC_AGES=()

for _ in $(seq 1 "$NUM_DOCS"); do
  DOC_KEYS+=("$(new_uuid)")
  DOC_NAMES+=("${NAMES[RANDOM % ${#NAMES[@]}]}")
  DOC_AGES+=("$(( (RANDOM % 60) + 18 ))")
done

QUERY_NAME="${DOC_NAMES[0]}"
QUERY_AGE="${DOC_AGES[0]}"

echo
echo "==> Inserting ${NUM_DOCS} docs (sending to node1, following forwards on key-mismatch)"
for i in $(seq 0 $((NUM_DOCS - 1))); do
  cmd=$(printf 'INSERT %s %s {"name":"%s","age":%s}' \
    "$COLLECTION_NAME" "${DOC_KEYS[$i]}" "${DOC_NAMES[$i]}" "${DOC_AGES[$i]}")
  out=$(run_cmd_with_forward "$(node_port 1)" "$cmd")
  reply=$(printf '%s\n' "$out" \
    | grep -E '^[+-][A-Z]' \
    | grep -v -E '^\+(GRUMPYDB|TOKEN|BYE)' \
    | tail -n1)
  printf '    [%2d] %s name=%-7s age=%2d -> %s\n' \
    "$((i + 1))" "${DOC_KEYS[$i]}" "${DOC_NAMES[$i]}" "${DOC_AGES[$i]}" "$reply"
done

# ── 5) Per-node SCAN: replica distribution ─────────────────────────────

echo
echo "==> Per-node SCAN — each node only stores keys whose preference list contains it (N=3)"
for n in 1 2 3 4 5; do
  out=$(printf 'SCAN %s\r\n' "$COLLECTION_NAME" | run_session "$(node_port "$n")")
  count=$(printf '%s' "$out" | count_array_items)
  count="${count:-0}"
  echo
  echo "--- node${n} (port $(node_port "$n"))   stored docs: ${count} ---"
  printf '%s\n' "$out" | grep -E '^[0-9a-f]{8}-' || true
done

# ── 6) Wait for schema gossip to converge across all nodes ─────────────

echo
echo "==> Waiting for SCHEMA VERSION to converge across all 5 nodes"
EXPECTED_VERSION=2  # CREATE INDEX by_name + CREATE INDEX by_age = 2 entries
deadline=$(($(date +%s) + 30))
converged=0
while [[ "$(date +%s)" -lt "$deadline" ]]; do
  all_ok=1
  for n in 1 2 3 4 5; do
    out=$(printf 'SCHEMA VERSION\r\n' | run_session "$(node_port "$n")" | tr -d '\r' | { grep -E '^:[0-9]+' || true; } | tail -n1)
    v="${out#:}"
    if [[ -z "$v" ]] || ! [[ "$v" =~ ^[0-9]+$ ]] || (( v < EXPECTED_VERSION )); then
      all_ok=0
      break
    fi
  done
  if [[ "$all_ok" -eq 1 ]]; then
    converged=1
    break
  fi
  sleep 1
done

if [[ "$converged" -ne 1 ]]; then
  echo "WARN: schema did not fully converge to version >= ${EXPECTED_VERSION} within 30s"
  echo "    per-node SCHEMA VERSION snapshot:"
  for n in 1 2 3 4 5; do
    v=$(printf 'SCHEMA VERSION\r\n' | run_session "$(node_port "$n")" | tr -d '\r' | { grep -E '^:[0-9]+' || true; } | tail -n1)
    echo "      node${n}: ${v}"
  done
else
  echo "    converged: every node sees schema_version >= ${EXPECTED_VERSION}"
fi

# ── 7) Per-node QUERY: every node should now answer ────────────────────

echo
echo "==> Per-node QUERY by_name=\"${QUERY_NAME}\" and by_age=${QUERY_AGE}"
echo "    Each node returns ONLY the docs whose key landed in its replica set,"
echo "    but the index is now materialized everywhere (Phase 44c)."
for n in 1 2 3 4 5; do
  echo
  echo "--- node${n} (port $(node_port "$n")) ---"
  {
    printf 'QUERY %s by_name "%s"\r\n' "$COLLECTION_NAME" "$QUERY_NAME"
    printf 'QUERY %s by_age %s\r\n' "$COLLECTION_NAME" "$QUERY_AGE"
  } | run_session "$(node_port "$n")"
done

echo
echo "==> Demo finished"
if [[ "$KEEP_UP" -eq 1 ]]; then
  echo "Cluster left running (--keep-up). Stop with:"
  echo "  docker compose -f ${COMPOSE_FILE_PATH} down -v"
fi
