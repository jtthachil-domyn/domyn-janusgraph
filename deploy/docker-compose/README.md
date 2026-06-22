# Domyn Nexus Docker Compose Demo

This is a single-node demo stack:

- Domyn Nexus over HTTPS and Bolt/TLS
- Prometheus scraping `/metrics`
- Grafana with a pre-provisioned overview dashboard
- Smoke container that writes, reads, and creates a backup

It is not HA and does not include distributed replication.

## Run

```bash
deploy/docker-compose/up.sh
```

The script generates local self-signed certificates under
`deploy/docker-compose/certs/`, then runs:

```bash
docker compose -f deploy/docker-compose/docker-compose.yml --profile smoke up --build
```

## Endpoints

- Nexus HTTPS: <https://127.0.0.1:18443>
- Nexus Bolt/TLS: `127.0.0.1:17687`
- Prometheus: <http://127.0.0.1:19090>
- Grafana: <http://127.0.0.1:13000>

Grafana login:

- user: `admin`
- password: `domyn-nexus`

Demo tokens:

- admin: `compose-admin-token`
- read/write: `compose-writer-token`
- read-only: `compose-reader-token`

## Smoke

The `smoke` service runs when the `smoke` profile is enabled. It checks
`/health`, `/ready`, `/metrics`, a Cypher write/read path, and `/admin/backup`.

## Reset

```bash
docker compose -f deploy/docker-compose/docker-compose.yml down -v
```

This removes demo data, logs, backups, Prometheus data, and Grafana data.
