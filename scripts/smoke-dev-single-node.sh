#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_ROOT="${DOMYN_NEXUS_DEV_ROOT:-$(mktemp -d /tmp/domyn-nexus-dev-smoke.XXXXXX)}"
HTTP_ADDR="${DOMYN_NEXUS_HTTP_ADDR:-127.0.0.1:18080}"
BOLT_ADDR="${DOMYN_NEXUS_BOLT_ADDR:-127.0.0.1:17687}"
TOKEN="${DOMYN_NEXUS_SMOKE_TOKEN:-dev-token}"
LOG_PATH="$RUNTIME_ROOT/nexus-server.log"
CONFIG_PATH="$RUNTIME_ROOT/config.json"

mkdir -p "$RUNTIME_ROOT/data" "$RUNTIME_ROOT/backups" "$RUNTIME_ROOT/audit"

cat >"$CONFIG_PATH" <<JSON
{
  "production_mode": false,
  "auth_token": "$TOKEN",
  "storage_path": "$RUNTIME_ROOT/data",
  "http": {
    "bind_addr": "$HTTP_ADDR",
    "query_timeout_secs": 30,
    "max_body_bytes": 1048576,
    "max_concurrent_queries": 16,
    "max_query_rate_per_sec": 0,
    "max_query_rate_per_tenant_per_sec": 0,
    "slow_query_ms": 250,
    "wal_segment_bytes": 67108864,
    "wal_retention_segments": 4,
    "snapshot_retention": 2,
    "compaction_threshold": 1000,
    "query_memory_budget_bytes": 67108864,
    "process_memory_budget_bytes": 0,
    "default_query_limit": 10000,
    "vector_index_mode": "Hnsw",
    "backup_root": "$RUNTIME_ROOT/backups",
    "audit_log_path": "$RUNTIME_ROOT/audit/audit.jsonl"
  },
  "bolt": {
    "enabled": true,
    "bind_addr": "$BOLT_ADDR",
    "max_connections": 64,
    "query_timeout_secs": 30,
    "default_query_limit": 10000,
    "query_memory_budget_bytes": 67108864
  }
}
JSON

echo "smoke-dev: starting nexus-server"
echo "  root: $RUNTIME_ROOT"
echo "  http: http://$HTTP_ADDR"
echo "  bolt: bolt://$BOLT_ADDR"
echo "  log:  $LOG_PATH"

cargo run -p nexus-server --bin nexus-server -- --config "$CONFIG_PATH" >"$LOG_PATH" 2>&1 &
SERVER_PID=$!

cleanup() {
  if kill -0 "$SERVER_PID" >/dev/null 2>&1; then
    kill "$SERVER_PID" >/dev/null 2>&1 || true
    wait "$SERVER_PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

BASE_URL="http://$HTTP_ADDR"
for _ in $(seq 1 120); do
  if curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE_URL/ready" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$SERVER_PID" >/dev/null 2>&1; then
    echo "nexus-server exited before readiness" >&2
    tail -80 "$LOG_PATH" >&2 || true
    exit 1
  fi
  sleep 0.25
done

if ! curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE_URL/ready" >/dev/null; then
  echo "nexus-server did not become ready" >&2
  tail -80 "$LOG_PATH" >&2 || true
  exit 1
fi

if [[ "$#" -gt 0 ]]; then
  DOMYN_NEXUS_HTTP_URL="$BASE_URL" \
  DOMYN_NEXUS_SMOKE_TOKEN="$TOKEN" \
  NEXUS_BOLT_URI="bolt://$BOLT_ADDR" \
  NEXUS_BOLT_USER="${NEXUS_BOLT_USER:-neo4j}" \
  NEXUS_BOLT_PASSWORD="${NEXUS_BOLT_PASSWORD:-$TOKEN}" \
  "$@"
else
  DOMYN_NEXUS_HTTP_URL="$BASE_URL" \
  DOMYN_NEXUS_SMOKE_TOKEN="$TOKEN" \
  DOMYN_NEXUS_SMOKE_BACKUP_PATH="smoke-backup" \
  bash "$ROOT/scripts/smoke-single-node.sh"
fi

echo "smoke-dev: ok"
