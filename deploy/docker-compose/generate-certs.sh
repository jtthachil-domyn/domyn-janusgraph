#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CERT_DIR="$ROOT_DIR/certs"
OPENSSL_CONFIG="$CERT_DIR/openssl.cnf"

mkdir -p "$CERT_DIR"

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
DNS.2 = nexus
IP.1 = 127.0.0.1
EOF

openssl req \
  -x509 \
  -newkey rsa:2048 \
  -nodes \
  -keyout "$CERT_DIR/http.key" \
  -out "$CERT_DIR/http.crt" \
  -days 30 \
  -config "$OPENSSL_CONFIG" >/dev/null 2>&1

cp "$CERT_DIR/http.key" "$CERT_DIR/bolt.key"
cp "$CERT_DIR/http.crt" "$CERT_DIR/bolt.crt"
chmod 0644 "$CERT_DIR"/*.crt
chmod 0600 "$CERT_DIR"/*.key

echo "wrote demo certificates to $CERT_DIR"
