#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_ROOT="${DOMYN_NEXUS_DEV_ROOT:-/tmp/domyn-nexus-dev}"

mkdir -p "$RUNTIME_ROOT/data" "$RUNTIME_ROOT/backups" "$RUNTIME_ROOT/audit"

echo "Starting Domyn Nexus dev single-node"
echo "  HTTP:  http://127.0.0.1:18080"
echo "  Bolt:  bolt://127.0.0.1:17687"
echo "  Token: dev-token"
echo
echo "In another shell:"
echo "  DOMYN_NEXUS_HTTP_URL=http://127.0.0.1:18080 DOMYN_NEXUS_SMOKE_TOKEN=dev-token bash scripts/smoke-single-node.sh"
echo

exec cargo run -p nexus-server --bin nexus-server -- --config "$ROOT/docs/examples/dev-single-node.json"
