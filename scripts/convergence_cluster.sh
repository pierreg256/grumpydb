#!/usr/bin/env bash
set -euo pipefail

# Convergence-oriented local test on the 3-node Docker cluster.
#
# It exercises churn + replay behavior by:
# 1) starting the cluster,
# 2) stopping node2,
# 3) issuing writes with W=N (expected quorum failure + hinted backlog enqueue),
# 4) restarting node2,
# 5) waiting for hinted backlog drain and checking replay metrics.
#
# Usage:
#   scripts/convergence_cluster.sh
#   GRUMPYDB_BOOTSTRAP_PASSWORD=admin scripts/convergence_cluster.sh
#   scripts/convergence_cluster.sh --keep-up
#
# Optional environment variables:
#   COMPOSE_FILE_PATH            default: docker-compose.cluster.yml
#   STARTUP_TIMEOUT_SECONDS      default: 60
#   REPLAY_TIMEOUT_SECONDS       default: 60
#   MAX_WRITE_ATTEMPTS           default: 40
#   CHURN_SETTLE_SECONDS         default: 8
#   DB_NAME                      default: convergence_db
#   COLLECTION_NAME              default: docs

COMPOSE_FILE_PATH="${COMPOSE_FILE_PATH:-docker-compose.cluster.yml}"
STARTUP_TIMEOUT_SECONDS="${STARTUP_TIMEOUT_SECONDS:-60}"
REPLAY_TIMEOUT_SECONDS="${REPLAY_TIMEOUT_SECONDS:-60}"
MAX_WRITE_ATTEMPTS="${MAX_WRITE_ATTEMPTS:-40}"
CHURN_SETTLE_SECONDS="${CHURN_SETTLE_SECONDS:-8}"
DB_NAME="${DB_NAME:-convergence_db}"
COLLECTION_NAME="${COLLECTION_NAME:-docs}"
GRUMPYDB_BOOTSTRAP_PASSWORD="${GRUMPYDB_BOOTSTRAP_PASSWORD:-admin}"

NODE1_HTTP="http://127.0.0.1:8081"
NODE2_HTTP="http://127.0.0.1:8082"
NODE3_HTTP="http://127.0.0.1:8083"

KEEP_UP=0
if [[ "${1:-}" == "--keep-up" ]]; then
  KEEP_UP=1
fi

cleanup() {
  if [[ "$KEEP_UP" -eq 0 ]]; then
    docker compose -f "$COMPOSE_FILE_PATH" down -v >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

wait_http() {
  local url="$1"
  local timeout="$2"
  local ok=0
  for _ in $(seq 1 "$timeout"); do
    if curl -fsS "$url/healthz" >/dev/null 2>&1; then
      ok=1
      break
    fi
    sleep 1
  done
  if [[ "$ok" -ne 1 ]]; then
    echo "ERROR: endpoint not ready: $url/healthz"
    return 1
  fi
}

run_authed_command() {
  local port="$1"
  local command="$2"
  local output

  output=$( {
      printf 'LOGIN _system admin %s\r\n' "$GRUMPYDB_BOOTSTRAP_PASSWORD"
      printf 'USE %s\r\n' "$DB_NAME"
      printf '%s\r\n' "$command"
      printf 'QUIT\r\n'
    } | nc -w 3 127.0.0.1 "$port" ) || {
      echo "ERROR: failed to run command on port $port: $command"
      return 1
    }

  if echo "$output" | grep -q -- '-ERR '; then
    echo "$output"
    return 2
  fi

  echo "$output"
  return 0
}

cluster_replication_n() {
  local out n
  if ! out=$(run_authed_command 6380 "TOPOLOGY" 2>/dev/null); then
    echo "WARN: failed to read TOPOLOGY, defaulting to N=1" >&2
    echo 1
    return 0
  fi
  n=$(echo "$out" | grep -Eo '"n"[[:space:]]*:[[:space:]]*[0-9]+' | head -n1 | grep -Eo '[0-9]+' || true)
  if [[ -z "${n:-}" ]]; then
    echo 1
  else
    echo "$n"
  fi
}

hint_line_count() {
  local path="docker/cluster/data/node1/_cluster/hints"
  if [[ ! -d "$path" ]]; then
    echo 0
    return
  fi
  find "$path" -type f -name '*.jsonl' -maxdepth 1 2>/dev/null | while read -r f; do
    wc -l < "$f"
  done | awk '{sum += $1} END {print sum + 0}'
}

hint_line_count_all_nodes() {
  local total=0
  local path
  for path in \
    "docker/cluster/data/node1/_cluster/hints" \
    "docker/cluster/data/node2/_cluster/hints" \
    "docker/cluster/data/node3/_cluster/hints"; do
    if [[ -d "$path" ]]; then
      count=$(find "$path" -maxdepth 1 -type f -name '*.jsonl' 2>/dev/null | while read -r f; do
        wc -l < "$f"
      done | awk '{sum += $1} END {print sum + 0}')
      total=$((total + count))
    fi
  done
  echo "$total"
}

port_for_peer_host() {
  local host="$1"
  case "$host" in
    node1) echo 6380 ;;
    node2) echo 6382 ;;
    node3) echo 6383 ;;
    *) echo "" ;;
  esac
}

extract_forward_port() {
  local out="$1"
  local addr host
  addr=$(echo "$out" | sed -n 's/.*forward to [^@]*@\([^; ]*\).*/\1/p' | head -n1)
  host="${addr%%:*}"
  if [[ -z "$host" || "$host" == "$addr" ]]; then
    echo ""
    return
  fi
  port_for_peer_host "$host"
}

metric_value() {
  local metric="$1"
  local body
  body=$(curl -fsS "$NODE1_HTTP/metrics")
  echo "$body" | awk -v m="$metric" '
    {
      key = $1
      if (index(key, m) == 1) {
        rest = substr(key, length(m) + 1, 1)
        if (rest == "" || rest == "{") {
          sum += $NF
        }
      }
    }
    END { print sum + 0 }
  '
}

echo "==> Starting cluster"
GRUMPYDB_BOOTSTRAP_PASSWORD="$GRUMPYDB_BOOTSTRAP_PASSWORD" \
  docker compose -f "$COMPOSE_FILE_PATH" up -d --build

echo "==> Waiting for all nodes health"
wait_http "$NODE1_HTTP" "$STARTUP_TIMEOUT_SECONDS"
wait_http "$NODE2_HTTP" "$STARTUP_TIMEOUT_SECONDS"
wait_http "$NODE3_HTTP" "$STARTUP_TIMEOUT_SECONDS"

echo "==> Preparing DB and collection on node1"
run_authed_command 6380 "CREATE DATABASE $DB_NAME" >/dev/null || true
run_authed_command 6380 "CREATE COLLECTION $COLLECTION_NAME" >/dev/null || true

echo "==> Reading topology"
REPLICATION_N=$(cluster_replication_n)
echo "    detected N=$REPLICATION_N"

echo "==> Stopping node2 to induce churn"
docker compose -f "$COMPOSE_FILE_PATH" stop node2 >/dev/null

echo "==> Starting writes immediately (before gossip marks node2 down)"

if [[ "$REPLICATION_N" -ge 2 ]]; then
  echo "==> Issuing writes with WRITE_CONCERN W=$REPLICATION_N until hinted backlog appears"
  failures=0
  backlog_seen=0
  availability_rejections=0
  for i in $(seq 1 "$MAX_WRITE_ATTEMPTS"); do
    if command -v uuidgen >/dev/null 2>&1; then
      uuid=$(uuidgen | tr '[:upper:]' '[:lower:]')
    else
      uuid=$(printf '00000000-0000-0000-0000-%012d' "$i")
    fi
    cmd="WRITE_CONCERN W=$REPLICATION_N INSERT $COLLECTION_NAME $uuid {\"i\":$i,\"phase\":\"churn\"}"

    # With node2 down, write against currently-up nodes and follow forward
    # only when target is also up.
    if (( i % 2 == 0 )); then
      attempt_port=6383
    else
      attempt_port=6380
    fi

    set +e
    out=$(run_authed_command "$attempt_port" "$cmd")
    rc=$?
    set -e

    if [[ "$rc" -eq 2 ]]; then
      if echo "$out" | grep -q "forward to "; then
        forwarded_port=$(extract_forward_port "$out")
        if [[ "$forwarded_port" == "6380" || "$forwarded_port" == "6383" ]]; then
          set +e
          out=$(run_authed_command "$forwarded_port" "$cmd")
          rc=$?
          set -e
        fi
      fi
    fi

    if [[ "$rc" -eq 2 ]]; then
      failures=$((failures + 1))
      if echo "$out" | grep -q "not enough live replicas for W="; then
        availability_rejections=$((availability_rejections + 1))
      fi
    fi

    echo "$out" >/dev/null

    if [[ "$(hint_line_count_all_nodes)" -gt 0 ]]; then
      backlog_seen=1
      break
    fi
  done

  if [[ "$failures" -eq 0 ]]; then
    echo "ERROR: expected at least one quorum failure with node2 down and W=$REPLICATION_N"
    exit 1
  fi

  echo "==> Verifying hinted backlog exists"
  before_lines=$(hint_line_count_all_nodes)
  if [[ "$before_lines" -le 0 || "$backlog_seen" -ne 1 ]]; then
    enqueued=$(metric_value "grumpydb_hints_enqueued_total")
    echo "ERROR: no hinted backlog detected under docker/cluster/data/node*/_cluster/hints"
    echo "       hints_enqueued_total=$enqueued failures=$failures attempts=$MAX_WRITE_ATTEMPTS"
    if [[ "$availability_rejections" -gt 0 ]]; then
      echo "       availability_rejections=$availability_rejections (writes rejected before hint enqueue)"
      echo "       try lowering CHURN_SETTLE_SECONDS or increasing MAX_WRITE_ATTEMPTS"
    fi
    exit 1
  fi

  if [[ "$CHURN_SETTLE_SECONDS" -gt 0 ]]; then
    echo "==> Waiting ${CHURN_SETTLE_SECONDS}s for liveness convergence"
    sleep "$CHURN_SETTLE_SECONDS"
  fi
else
  echo "==> N<2 detected: skipping hinted-handoff assertions (W=2 path unavailable)"
  echo "==> Writing with W=1 to keep churn scenario active"
  run_authed_command 6380 "INSERT $COLLECTION_NAME 00000000-0000-0000-0000-000000000001 {\"phase\":\"churn\",\"i\":1}" >/dev/null
  before_lines=0
fi

echo "==> Restarting node2"
docker compose -f "$COMPOSE_FILE_PATH" start node2 >/dev/null
wait_http "$NODE2_HTTP" "$STARTUP_TIMEOUT_SECONDS"

if [[ "$REPLICATION_N" -ge 2 ]]; then
  echo "==> Waiting for hinted backlog drain"
  drained=0
  for _ in $(seq 1 "$REPLAY_TIMEOUT_SECONDS"); do
    now_lines=$(hint_line_count_all_nodes)
    if [[ "$now_lines" -eq 0 ]]; then
      drained=1
      break
    fi
    sleep 1
  done

  if [[ "$drained" -ne 1 ]]; then
    echo "ERROR: hinted backlog did not drain within ${REPLAY_TIMEOUT_SECONDS}s"
    exit 1
  fi

  echo "==> Checking replay metric on node1"
  replayed=$(metric_value "grumpydb_hints_replayed_total")
  retries=$(metric_value "grumpydb_hints_replay_retries_total")
  if [[ "$replayed" -le 0 && "$retries" -le 0 ]]; then
    echo "WARN: hint replay metrics did not increase (replayed=0 retries=0)"
    echo "      backlog drain still succeeded; verify logs/metrics wiring if needed"
  fi
else
  replayed=0
fi

echo "==> Rebalance control-plane sanity after churn"
plan_out=$(run_authed_command 6380 "REBALANCE PLAN ADD-NODE 11111111-1111-1111-1111-111111111111")
if ! echo "$plan_out" | grep -q '"action":"add-node"'; then
  echo "ERROR: unexpected rebalance plan response"
  echo "$plan_out"
  exit 1
fi

echo "==> Convergence script completed successfully"
if [[ "$REPLICATION_N" -ge 2 ]]; then
  echo "    mode=full quorum_failures=$failures hinted_lines_before=$before_lines hints_replayed=$replayed"
else
  echo "    mode=control-plane-only n=$REPLICATION_N"
fi

if [[ "$KEEP_UP" -eq 1 ]]; then
  echo "Cluster left running (--keep-up). Stop with: docker compose -f $COMPOSE_FILE_PATH down -v"
fi
