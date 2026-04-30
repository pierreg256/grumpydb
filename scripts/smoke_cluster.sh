#!/usr/bin/env bash
set -euo pipefail

# Reusable smoke test for the 3-node v5 demo cluster.
# Usage:
#   scripts/smoke_cluster.sh
#   GRUMPYDB_BOOTSTRAP_PASSWORD=secret scripts/smoke_cluster.sh
#   scripts/smoke_cluster.sh --keep-up
#
# Optional environment variables:
#   COMPOSE_FILE_PATH (default: docker-compose.cluster.yml)
#   HEALTH_URL (default: http://127.0.0.1:8081/healthz)
#   JWKS_URL   (default: http://127.0.0.1:8081/.well-known/jwks.json)
#   STARTUP_TIMEOUT_SECONDS (default: 40)
#   LOG_TAIL_LINES (default: 120)

COMPOSE_FILE_PATH="${COMPOSE_FILE_PATH:-docker-compose.cluster.yml}"
HEALTH_URL="${HEALTH_URL:-http://127.0.0.1:8081/healthz}"
JWKS_URL="${JWKS_URL:-http://127.0.0.1:8081/.well-known/jwks.json}"
STARTUP_TIMEOUT_SECONDS="${STARTUP_TIMEOUT_SECONDS:-40}"
LOG_TAIL_LINES="${LOG_TAIL_LINES:-120}"
GRUMPYDB_BOOTSTRAP_PASSWORD="${GRUMPYDB_BOOTSTRAP_PASSWORD:-admin}"

KEEP_UP=0
if [[ "${1:-}" == "--keep-up" ]]; then
  KEEP_UP=1
fi

cleanup() {
  if [[ "$KEEP_UP" -eq 0 ]]; then
    docker compose -f "$COMPOSE_FILE_PATH" down -v
  fi
}
trap cleanup EXIT

echo "==> Starting cluster"
GRUMPYDB_BOOTSTRAP_PASSWORD="$GRUMPYDB_BOOTSTRAP_PASSWORD" \
  docker compose -f "$COMPOSE_FILE_PATH" up -d --build

echo "==> Waiting for health endpoint: $HEALTH_URL"
ready=0
for i in $(seq 1 "$STARTUP_TIMEOUT_SECONDS"); do
  if curl -fsS "$HEALTH_URL" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done

if [[ "$ready" -ne 1 ]]; then
  echo "ERROR: health endpoint did not become ready in ${STARTUP_TIMEOUT_SECONDS}s"
  docker compose -f "$COMPOSE_FILE_PATH" ps
  docker compose -f "$COMPOSE_FILE_PATH" logs --no-color --tail=200
  exit 1
fi

echo "==> Health check"
curl -fsS "$HEALTH_URL"
echo

echo "==> JWKS check"
curl -fsS "$JWKS_URL"
echo

echo "==> Cluster logs (tail $LOG_TAIL_LINES)"
docker compose -f "$COMPOSE_FILE_PATH" logs --no-color --tail="$LOG_TAIL_LINES"

echo "==> Smoke test completed"
if [[ "$KEEP_UP" -eq 1 ]]; then
  echo "Cluster left running (--keep-up). Stop with: docker compose -f $COMPOSE_FILE_PATH down -v"
fi
