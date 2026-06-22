# Domyn Nexus Server Quickstart

Domyn Nexus Internal Beta targets a single-node server first. The server exposes
HTTP, Bolt, vector endpoints, backup, compaction, readiness, and Prometheus
metrics from one binary.

## Build

```bash
cargo build --release -p nexus-server --bin nexus-server
```

## Generate Config

Development config:

```bash
cargo run -p nexus-server --bin nexus-server -- init --profile dev --output /tmp/domyn-nexus-dev/config.json
```

Production template:

```bash
cargo run -p nexus-server --bin nexus-server -- init --profile production --output /tmp/domyn-nexus-prod/config.json
```

Validate before startup:

```bash
cargo run -p nexus-server --bin nexus-server -- check-config --config /tmp/domyn-nexus-prod/config.json
```

Production mode refuses insecure startup unless auth, TLS, storage, audit log,
backup root, query limits, and Bolt limits are configured.

## Run Locally

One-command local smoke. This starts a temporary dev server, waits for
readiness, runs health/ready/metrics/Cypher/backup checks, and shuts the server
down:

```bash
scripts/smoke-dev-single-node.sh
```

Manual server start:

```bash
scripts/run-dev-single-node.sh
```

Smoke an already-running server:

```bash
scripts/smoke-single-node.sh
```

Manual run:

```bash
cargo run -p nexus-server --bin nexus-server -- --config docs/examples/dev-single-node.json
```

## Docker

Build:

```bash
docker build -t domyn-nexus:internal-beta .
```

Build and smoke the packaged image:

```bash
scripts/smoke-docker-single-node.sh
```

Run with mounted config, TLS, data, logs, and backups:

```bash
docker run --rm \
  -p 8443:8443 \
  -p 7687:7687 \
  -v "$PWD/docs/examples/production-single-node.json:/etc/domyn-nexus/config.json:ro" \
  -v "$PWD/.local/tls:/etc/domyn-nexus/tls:ro" \
  -v "$PWD/.local/data:/var/lib/domyn-nexus" \
  -v "$PWD/.local/logs:/var/log/domyn-nexus" \
  -v "$PWD/.local/backups:/var/backups/domyn-nexus" \
  domyn-nexus:internal-beta
```

The image defaults to `/etc/domyn-nexus/config.json` and healthchecks
`https://127.0.0.1:8443/health`.

## HTTP Examples

The stable Internal Beta request/response shapes are documented in
[HTTP API Contract](http-api-contract.md).

Health:

```bash
curl -k https://127.0.0.1:8443/health
```

Readiness:

```bash
curl -k https://127.0.0.1:8443/ready
```

Cypher:

```bash
curl -k https://127.0.0.1:8443/cypher \
  -H "Authorization: Bearer replace-with-admin-token" \
  -H "Content-Type: application/json" \
  -d '{"query":"MATCH (n) RETURN count(n) AS count","params":{}}'
```

Metrics:

```bash
curl -k https://127.0.0.1:8443/metrics
```

Backup:

```bash
curl -k https://127.0.0.1:8443/admin/backup \
  -H "Authorization: Bearer replace-with-admin-token" \
  -H "Content-Type: application/json" \
  -d '{"path":"daily/manual-001"}'
```

Compaction:

```bash
curl -k https://127.0.0.1:8443/admin/compact \
  -H "Authorization: Bearer replace-with-admin-token" \
  -H "Content-Type: application/json" \
  -d '{}'
```

Vector index:

```bash
curl -k https://127.0.0.1:8443/vectors/entities \
  -H "Authorization: Bearer replace-with-admin-token" \
  -H "Content-Type: application/json" \
  -d '{"dimension":384}'
```

## CLI Backup And Restore

Create a backup:

```bash
cargo run -p nexus-server --bin nexus-server -- backup \
  --config docs/examples/production-single-node.json \
  --output /var/backups/domyn-nexus/manual-001
```

Restore into a fresh data directory:

```bash
cargo run -p nexus-server --bin nexus-server -- restore \
  --backup /var/backups/domyn-nexus/manual-001 \
  --data-dir /var/lib/domyn-nexus-restored
```

## Internal Beta Gate

Quick gate:

```bash
cargo xtask verify-internal-beta --quick
```

Full gate:

```bash
cargo xtask verify-internal-beta
```

Use `DOMYN_NEXUS_REAL_DATA_DIR` when the real FinReflectKG benchmark dataset is
not at the default local path.

The full gate builds `domyn-nexus:internal-beta` and runs the Docker smoke by
default. Use `--skip-docker` only when the local Docker daemon is unavailable.

Real-data benchmark threshold mode:

```bash
cargo run -p nexus-bench --release --bin real_data_bench -- --assert-internal-beta
```

This fails if B1/B2 regress beyond 2x the current Nexus baseline, if B3-B10
fall behind the recorded Neo4j/JanusGraph references, or if B9 exceeds the
Internal Beta p50 target.
