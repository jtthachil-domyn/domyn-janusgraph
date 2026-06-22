# Security Posture

This document describes the Internal Beta single-node posture. It is not a SOC
2, ISO 27001, FedRAMP, or formal compliance claim.

## Implemented Guardrails

- Production mode rejects insecure startup when required auth, TLS, storage,
  audit log, backup root, or query limits are missing.
- HTTP supports bearer/API-key principals with read-only, read/write, and admin
  roles.
- Tenant-scoped principals are enforced before query execution on HTTP routes.
- HTTP TLS and Bolt TLS are configurable.
- Audit logs can be written to a configured JSONL path.
- Query timeout, row limit, byte budget, process memory budget, rate limits, and
  concurrent-query limits are configurable.
- Hot backup and restore are available through CLI and admin HTTP.
- Metrics are exposed for query, WAL, snapshot, compaction, vector, and memory
  signals.

## Known Boundaries

- The current product target is single-node. There is no HA replication or
  distributed sharding in the Internal Beta shell.
- Explicit Bolt write transactions are rejected with a documented error. Bolt
  auto-commit writes are supported.
- Embedded Python SDK persistent graphs are single-process; do not open the same
  data directory from multiple writers.
- Self-signed certificates in the Docker Compose demo are for local demo use
  only.
- Demo tokens in `deploy/docker-compose/config/nexus.json` must be replaced for
  any shared environment.

## Operational Defaults

For a production-like single-node deployment:

1. Generate real TLS certificates.
2. Replace all demo tokens with secret-managed values.
3. Set a durable `storage_path`.
4. Set a backup root on persistent storage.
5. Enable audit logging.
6. Set query timeout, row limit, byte budget, memory budget, and concurrency
   limits.
7. Monitor `/ready`, `/metrics`, WAL bytes, snapshot bytes, vector snapshot
   bytes, query latency, active queries, and memory estimates.
8. Run restore drills regularly.

## Non-Claims

Domyn Nexus Internal Beta does not yet claim:

- multi-node fault tolerance
- cross-shard Cypher
- distributed transactions
- formal regulatory certification
- hardware enclave or customer-managed-key encryption
- automatic cloud rebalancing
