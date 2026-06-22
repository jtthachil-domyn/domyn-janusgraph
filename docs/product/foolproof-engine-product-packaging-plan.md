# Domyn Nexus Foolproof Engine + Product Packaging Plan

## Summary

Target: **Internal Beta**.

This plan is a supporting document. The canonical long-range roadmap is
[nexus-production-grade-master-plan.md](nexus-production-grade-master-plan.md).

Execution status is tracked line-by-line in
[internal-beta-plan-ledger.md](internal-beta-plan-ledger.md).

Product shells:

1. **Server Product**: one binary, Docker, config, auth/TLS, backup/restore,
   observability, and smoke tests.
2. **Embedded SDK Product**: Python/Rust SDK packaging for in-process GraphRAG.
3. **Cloud Demo Product**: hosted-looking deployment after server and SDK are
   stable.

Immediate implementation priority: **Server First**.

Mandatory workflow: each tranche must start by reading the master plan, this
plan, [internal-beta-plan-ledger.md](internal-beta-plan-ledger.md),
[known-limits.md](known-limits.md), and
[../production-readiness.md](../production-readiness.md). Each tranche must end
by updating checklist status, evidence, and known limits. No workaround may be
treated as product behavior.

## Foolproof Definition

For Internal Beta, foolproof means:

- no silent data loss for supported write paths
- no silent wrong query result for supported Cypher
- no unsecured production startup
- no unbounded query path
- no undocumented sharp edge

It does not mean mathematically bug-free or enterprise GA.

## Server Product Gate

Required deliverables:

- `nexus-server --config <path>`
- `nexus-server check-config --config <path>`
- `nexus-server init --profile dev|production`
- `nexus-server backup --config <path> --output <dir>`
- `nexus-server restore --backup <dir> --data-dir <dir>`
- Docker image with production config path `/etc/domyn-nexus/config.json`
- Docker healthcheck against `/health`
- JSON config templates and validation
- HTTP endpoint contract for `/health`, `/ready`, `/metrics`, `/cypher`,
  `/tenants`, `/admin/backup`, `/admin/compact`, and `/vectors/*`
- Bolt auto-commit support and explicit rejection of unsupported transaction
  paths
- product docs under `docs/product/`
- CI for format, tests, TCK, Docker build, and smoke
- `CHANGELOG.md`

## Canonical Verification

Run:

```bash
cargo xtask verify-internal-beta
```

Quick iteration:

```bash
cargo xtask verify-internal-beta --quick
```

The gate includes:

- `cargo fmt --check`
- `cargo test --workspace`
- local TCK Scope B
- skipped-inclusive TCK Scope B+
- GraphRAG readiness tests
- server binary compile
- GraphRAG retrieval benchmark
- real-data benchmark with `--assert-internal-beta` when the dataset is present
  or required
- optional HTTP smoke
- optional Neo4j driver smoke
- optional Docker build

## Engine Hardening Gate

The current line-by-line status lives in
[internal-beta-plan-ledger.md](internal-beta-plan-ledger.md). The local gate now
covers these failure-injection suites:

- WAL truncation/corruption during active write
- snapshot temp-file crash before rename
- backup during active writes
- restore from backup into fresh data dir
- restore rejects backup directories without `backup-manifest.json`
- backup/restore cutoff semantics are tested so writes after backup start do not
  appear in the restored copy
- compaction during query traffic
- vector snapshot save/load with deletes and updates

The gate also covers these unsupported-means-rejected cases:

- explicit Bolt write transactions
- production mode missing auth/TLS/storage/audit/backup/query limits
- unsupported Cypher parse/bind errors

And these query safety cases:

- row limit
- byte budget
- process memory budget
- cancellation before WAL append
- timeout while scanning/expanding/unwinding
- rate limit and concurrent-query rejection

## Embedded SDK Product Gate

Deliverables:

- maturin wheels for macOS arm64/x86_64 and Linux x86_64
- `Graph.open(path)`
- `Graph.save_snapshot()`
- `Graph.cypher(query, params={})`
- `Graph.vector_index(name, dim, mode="hnsw")`
- `Graph.backup(path)`
- typed Python exceptions matching server error codes
- examples for GraphRAG ingestion, vector + graph retrieval, bounded traversal,
  snapshot recovery, and tenant-scoped retrieval

Acceptance:

- clean venv install
- import and create graph
- persist/reopen graph
- Cypher with params
- vector search + traversal
- backup/restore

## Cloud Demo Product Gate

Deliverables:

- `deploy/azure/single-node/`
- `deploy/docker-compose/`
- Prometheus and Grafana wiring
- FinReflectKG sample loader
- demo script and security posture docs

Acceptance:

- one-command single-node demo deploy
- HTTPS and authenticated HTTP
- backup and restore demonstrated
- dashboard shows query, WAL, compaction, backup, vector, and memory metrics
- clear single-node, non-HA disclaimer

## Competitive Posture

HelixDB validates the market shape: Rust graph + vector for AI memory. Nexus
responds with:

- Cypher/Bolt compatibility
- TCK-measured openCypher progress
- GraphRAG benchmark wins
- Python embedded path
- regulated-data posture through auth, TLS, audit logs, backup/restore, and
  explicit production validation

Nexus should not become a wholesale fork of HelixDB, Kuzu, FalkorDB, OverGraph,
or Neo4j. Use them as references; keep the internal architecture centered on a
shared graph algebra/runtime, with Cypher first and future GQL alignment.
