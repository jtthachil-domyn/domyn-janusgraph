#!/usr/bin/env bash
set -euo pipefail

IMAGE="${DOMYN_NEXUS_DOCKER_IMAGE:-domyn-nexus:internal-beta}"
CONTAINER_NAME="${DOMYN_NEXUS_DOCKER_CONTAINER:-domyn-nexus-smoke-$$}"
HTTP_PORT="${DOMYN_NEXUS_DOCKER_HTTP_PORT:-18443}"
BOLT_PORT="${DOMYN_NEXUS_DOCKER_BOLT_PORT:-17687}"
ADMIN_TOKEN="${DOMYN_NEXUS_DOCKER_ADMIN_TOKEN:-replace-with-admin-token}"
WORK_DIR="${DOMYN_NEXUS_DOCKER_SMOKE_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/domyn-nexus-docker-smoke.XXXXXX")}"

TLS_DIR="$WORK_DIR/tls"
DATA_DIR="$WORK_DIR/data"
LOG_DIR="$WORK_DIR/logs"
BACKUP_DIR="$WORK_DIR/backups"
OPENSSL_CONFIG="$WORK_DIR/openssl.cnf"

cleanup() {
  docker stop "$CONTAINER_NAME" >/dev/null 2>&1 || true
  if [[ -z "${DOMYN_NEXUS_DOCKER_SMOKE_DIR:-}" ]]; then
    rm -rf "$WORK_DIR"
  fi
}
trap cleanup EXIT

mkdir -p "$TLS_DIR" "$DATA_DIR" "$LOG_DIR" "$BACKUP_DIR"
chmod 0777 "$DATA_DIR" "$LOG_DIR" "$BACKUP_DIR"

cat >"$OPENSSL_CONFIG" <<'EOF'
[req]
distinguished_name = req_distinguished_name
x509_extensions = v3_req
prompt = no

[req_distinguished_name]
CN = localhost

[v3_req]
subjectAltName = @alt_names

[alt_names]
DNS.1 = localhost
IP.1 = 127.0.0.1
EOF

openssl req \
  -x509 \
  -newkey rsa:2048 \
  -nodes \
  -keyout "$TLS_DIR/http.key" \
  -out "$TLS_DIR/http.crt" \
  -days 1 \
  -config "$OPENSSL_CONFIG" >/dev/null 2>&1

cp "$TLS_DIR/http.key" "$TLS_DIR/bolt.key"
cp "$TLS_DIR/http.crt" "$TLS_DIR/bolt.crt"
chmod 0644 "$TLS_DIR"/*.crt
chmod 0600 "$TLS_DIR"/*.key

echo "docker-smoke: starting $IMAGE as $CONTAINER_NAME"
docker run \
  -d \
  --rm \
  --name "$CONTAINER_NAME" \
  -p "127.0.0.1:${HTTP_PORT}:8443" \
  -p "127.0.0.1:${BOLT_PORT}:7687" \
  -v "$TLS_DIR:/etc/domyn-nexus/tls:ro" \
  -v "$DATA_DIR:/var/lib/domyn-nexus" \
  -v "$LOG_DIR:/var/log/domyn-nexus" \
  -v "$BACKUP_DIR:/var/backups/domyn-nexus" \
  "$IMAGE" >/dev/null

echo "docker-smoke: waiting for /health"
for _ in $(seq 1 60); do
  if curl -fk "https://127.0.0.1:${HTTP_PORT}/health" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

curl -fk "https://127.0.0.1:${HTTP_PORT}/health" >/dev/null

DOMYN_NEXUS_HTTP_URL="https://127.0.0.1:${HTTP_PORT}" \
DOMYN_NEXUS_SMOKE_TOKEN="$ADMIN_TOKEN" \
DOMYN_NEXUS_SMOKE_BACKUP_PATH="smoke/docker-$(date +%s)" \
DOMYN_NEXUS_CURL_INSECURE=1 \
  bash scripts/smoke-single-node.sh

echo "docker-smoke: ok"
