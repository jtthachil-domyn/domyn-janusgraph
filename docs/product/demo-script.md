# Demo Script

This script demonstrates the single-node product shell. It is not a high
availability or distributed deployment.

## 1. Start The Stack

```bash
deploy/docker-compose/up.sh
```

The launcher generates local self-signed certificates and starts:

- Nexus over HTTPS on <https://127.0.0.1:18443>
- Bolt/TLS on `127.0.0.1:17687`
- Prometheus on <http://127.0.0.1:19090>
- Grafana on <http://127.0.0.1:13000>
- a smoke container that writes data and creates a backup

Optional: build a persistent FinReflectKG demo data directory before starting a
server that mounts it:

```bash
cargo run -p nexus-bench --bin finreflectkg_demo_loader -- \
  --triplet-dir /path/to/FinReflectKG/reflection \
  --data-dir /tmp/domyn-nexus-finreflect-demo \
  --ticker NVDA
```

The loader writes a Nexus graph snapshot plus a named vector snapshot
(`entities`) into the data directory.

## 2. Show Health And Readiness

```bash
curl -fk https://127.0.0.1:18443/health
curl -fk https://127.0.0.1:18443/ready
```

## 3. Run A Cypher Write

```bash
curl -fk \
  -H 'Authorization: Bearer compose-admin-token' \
  -H 'content-type: application/json' \
  -d '{"query":"CREATE (n:Demo {name: '\''nexus-demo'\''}) RETURN n.name AS name"}' \
  https://127.0.0.1:18443/cypher
```

## 4. Query The Graph

```bash
curl -fk \
  -H 'Authorization: Bearer compose-admin-token' \
  -H 'content-type: application/json' \
  -d '{"query":"MATCH (n:Demo) RETURN count(n) AS demos"}' \
  https://127.0.0.1:18443/cypher
```

## 5. Create A Backup

```bash
curl -fk \
  -H 'Authorization: Bearer compose-admin-token' \
  -H 'content-type: application/json' \
  -d '{"path":"demo-backup"}' \
  https://127.0.0.1:18443/admin/backup
```

## 6. Show Metrics

```bash
curl -fk \
  -H 'Authorization: Bearer compose-admin-token' \
  https://127.0.0.1:18443/metrics | grep domyn_nexus
```

Then open Grafana at <http://127.0.0.1:13000>:

- user: `admin`
- password: `domyn-nexus`

## 7. Explain The Boundary

Say this explicitly:

- This is single-node.
- It has auth, TLS, audit log path, backup, readiness, metrics, and bounded
  query controls.
- It is not distributed, not HA, and not cross-shard.
- Distributed mode remains a later Raft/sharding phase after single-node
  behavior is boring.
