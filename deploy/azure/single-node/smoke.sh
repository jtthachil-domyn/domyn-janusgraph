#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${1:-}"
TOKEN="${2:-}"

if [[ -z "$BASE_URL" || -z "$TOKEN" ]]; then
  echo "usage: deploy/azure/single-node/smoke.sh https://<host>:8443 <admin-token>" >&2
  exit 2
fi

curl_base() {
  curl -fk -H "Authorization: Bearer $TOKEN" "$@"
}

curl_json() {
  curl_base -H "content-type: application/json" "$@"
}

echo "azure-smoke: health"
curl -fk "$BASE_URL/health" >/dev/null

echo "azure-smoke: ready"
curl_base "$BASE_URL/ready" >/dev/null

echo "azure-smoke: metrics"
curl_base "$BASE_URL/metrics" | grep -q "domyn_nexus"

echo "azure-smoke: write"
curl_json \
  -d '{"query":"CREATE (n:AzureSmoke {name: '\''azure-smoke'\''}) RETURN n.name"}' \
  "$BASE_URL/cypher" | grep -q "azure-smoke"

echo "azure-smoke: backup"
curl_json \
  -d '{"path":"azure-smoke"}' \
  "$BASE_URL/admin/backup" | grep -q '"files"'

echo "azure-smoke: ok"
