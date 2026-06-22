#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${DOMYN_NEXUS_HTTP_URL:-http://127.0.0.1:18080}"
TOKEN="${DOMYN_NEXUS_SMOKE_TOKEN:-dev-token}"
BACKUP_PATH="${DOMYN_NEXUS_SMOKE_BACKUP_PATH:-smoke-backup}"

curl_base() {
  local args=(-fsS)
  if [[ "${DOMYN_NEXUS_CURL_INSECURE:-0}" == "1" ]]; then
    args+=(-k)
  fi
  if [[ -n "$TOKEN" ]]; then
    args+=(-H "Authorization: Bearer $TOKEN")
  fi
  curl "${args[@]}" "$@"
}

curl_json() {
  curl_base -H "content-type: application/json" "$@"
}

echo "smoke: health"
curl_base "$BASE_URL/health" >/dev/null

echo "smoke: ready"
curl_base "$BASE_URL/ready" >/dev/null

echo "smoke: metrics"
curl_base "$BASE_URL/metrics" | grep -q "domyn_nexus"

echo "smoke: write"
curl_json \
  -d '{"query":"CREATE (n:Smoke {name: $name}) RETURN n.name","params":{"name":"nexus-smoke"}}' \
  "$BASE_URL/cypher" | grep -q "nexus-smoke"

echo "smoke: read"
curl_json \
  -d '{"query":"MATCH (n:Smoke) WHERE n.name = $name RETURN count(n)","params":{"name":"nexus-smoke"}}' \
  "$BASE_URL/cypher" | grep -q '"rows"'

echo "smoke: backup"
curl_json \
  -d "{\"path\":\"$BACKUP_PATH\"}" \
  "$BASE_URL/admin/backup" | grep -q '"files"'

echo "smoke: ok"
