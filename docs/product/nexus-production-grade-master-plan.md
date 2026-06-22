# Domyn Nexus Production-Grade Master Plan

This is the canonical roadmap for taking Domyn Nexus from Internal Beta to a
Neo4j-class, graph-native, multi-model database product.

Read this file before starting any tranche. Update it, the
[Internal Beta plan ledger](internal-beta-plan-ledger.md), the
[known limits](known-limits.md), and
[production readiness](../production-readiness.md) before calling a tranche
complete.

## Current Baseline

Product stage: **Internal Beta, single-node server first**.

North star: **beat Neo4j, HelixDB, and ArangoDB for GraphRAG and regulated AI
memory first**, then expand toward general-purpose graph database parity.

Current strengths:

- Graph engine with typed properties, CSR adjacency, WAL recovery, atomic
  snapshots, tombstone compaction, and query guardrails.
- Cypher-first query surface with TCK-measured progress and explicit unsupported
  feature rejection.
- HTTP and Bolt server surfaces with auth/TLS/config/metrics/audit/backup
  guardrails.
- Durable document collections, scalar/full-text indexes, BM25/TF-IDF ranking,
  vector indexes, cross-model write batches, and GraphRAG benchmarks.
- Packaging shell: server binary, Docker image, config templates, smoke scripts,
  CI gates, product docs, and `cargo xtask verify-internal-beta`.

Current hard limits:

- Not GA.
- Not HA.
- Not distributed.
- No Raft, leader election, automatic failover, or cross-shard Cypher.
- No full MVCC storage model yet; current core is single-writer with staged
  commits and read snapshots.
- Not fully Neo4j-driver certified yet.
- Search is not yet production-search complete: no persisted BM25 statistics,
  positional phrase index, language-aware analyzers, or highlight offsets.
- Documents are durable and indexed, but no schema validation, composite JSON
  indexes, or automatic document-to-graph materialization yet.

## Competitive Gap Matrix

| Product | What They Prove | What Nexus Must Match | Where Nexus Should Beat Them |
|---|---|---|---|
| Neo4j | Mature Cypher/Bolt ecosystem, clustering, admin tooling, drivers, full-text, vector, RBAC, monitoring, backup/restore | Driver certification, query compatibility, admin/ops depth, clustering/replication, security posture | Lower-latency GraphRAG, embedded Python, lower operational weight, graph+vector+document fusion |
| HelixDB | Rust graph+vector+text product shape, object-storage-backed cloud story, writer/readers, ACID claims, SDK/CLI-first packaging | Cloud architecture, object-store durability, reader scaling, MVCC, clean developer workflow | Cypher/Bolt compatibility, TCK evidence, regulated-data posture, Neo4j migration story |
| ArangoDB | Multi-model documents+graph+search product, AQL ergonomics, cluster/cloud maturity | Document query ergonomics, schema validation, search quality, operational maturity | Graph-native execution, Cypher/GQL path instead of AQL lock-in, GraphRAG benchmark focus |

## Milestone 1: Internal Beta

Goal: a safe, honest, single-node server that internal users can run and test.

Required:

- `cargo xtask verify-internal-beta` passes.
- Docker image builds and passes container smoke.
- Production config validator rejects insecure startup.
- Backup/restore into an empty data directory is verified.
- B1-B10 benchmark thresholds pass, including B9.
- GraphRAG retrieval benchmark reports recall and latency.
- Known unsupported features are tested as structured failures.
- Optional Neo4j driver smoke has been observed and recorded.
- Product docs cover quickstart, config, operations, API contracts, security,
  known limits, benchmarks, and troubleshooting.

Do not claim:

- GA
- HA
- distributed
- full Neo4j compatibility
- production-search completeness

Execution tracker: [internal-beta-plan-ledger.md](internal-beta-plan-ledger.md).

## Milestone 2: Single-Node GA

Goal: a boring, reliable single-node production database.

Required:

- Repeated crash/failure-injection suite for WAL, snapshots, backup, restore,
  compaction, vector/text indexes, and cross-model writes.
- Multi-day soak with mixed graph/document/vector/text workload.
- Upgrade/restart compatibility tests.
- Query cancellation and row/byte/time/memory budgets across every long-running
  operator.
- MVCC or equivalent versioned snapshot storage plan implemented and tested, so
  graph, document, vector, and text reads observe one transaction snapshot.
- Official Neo4j Python and JavaScript driver smoke required in CI; Java driver
  smoke added before GA.
- Search quality upgraded with persisted BM25 stats, phrase positions,
  language-aware analyzers, highlight offsets, and hybrid vector+BM25 ranking.
- Document collections upgraded with schema validation, composite JSON indexes,
  array/object indexing, and document-to-graph materialization rules.
- Python wheels and Rust/Python SDK examples validated in clean environments.

GA acceptance:

- No supported feature silently returns wrong results.
- Backup/restore validates graph topology, documents, vectors, text indexes,
  and representative query results.
- Known limits are narrow, accurate, and tested.
- Operational runbooks cover incident response, restore, compaction, upgrades,
  memory pressure, and driver compatibility.

## GraphRAG And Multi-Model Advantage Track

Goal: **beat Neo4j, HelixDB, and ArangoDB for AI memory workloads**.

This track runs across Internal Beta, Single-node GA, and Cloud Preview. It is
the product reason Nexus exists, not a side quest.

Product capabilities:

- One engine for graph, JSON documents, vector search, BM25/full-text search,
  tenant isolation, and Cypher/GQL-style querying.
- Hybrid retrieval that can combine vector candidate search, BM25 lexical
  search, document filters, graph expansion, and reranker-ready output.
- GraphRAG APIs for bounded traversal, context subgraph extraction, document
  chunk retrieval, entity expansion, and tenant-scoped retrieval.
- Benchmarks for FinReflectKG B1-B10, GraphRAG retrieval recall, hybrid search
  latency, tenant-filtered vector search, and document+graph query latency.

Public interfaces:

- HTTP: `/cypher`, `/tx/batch`, `/collections/*`, `/vectors/*`,
  `/admin/backup`, `/admin/compact`, and `/metrics`.
- Cypher/GQL-style: `MATCH DOCUMENT`, `document(...)`, `documents(...)`,
  `documentsBy(...)`, `documentFullText(...)`, and `vectorSearch(...)`.
- Future query surface: `HYBRID SEARCH` or an equivalent internal logical
  operator. Do not add public syntax until semantics and ranking evidence are
  pinned.
- Python SDK: `Graph.open(path)`, `Graph.cypher(query, params={})`,
  `Graph.backup(path)`, `Graph.vector_index(...)`, document collection APIs,
  and typed exceptions matching server error codes.

Acceptance:

- B1-B10 remain green and B9 remains below threshold.
- Hybrid retrieval benchmark reports recall and latency.
- Search quality tests compare exact oracle vs HNSW/vector path.
- Document, full-text, and vector mutations survive restart and backup/restore.

## Milestone 3: Cloud Preview

Goal: a hosted-looking, Helix-style deployment without fake distributed claims.

Architecture:

- Gateway authenticates and routes requests.
- Single writer owns all mutations.
- Read replicas serve read-only traffic from durable published snapshots.
- Object storage is the durable source of truth for snapshots, WAL/commit logs,
  documents, vector indexes, text indexes, and manifests.
- Local SSD and memory are caches only; deleting them must not lose committed
  data.

Required:

- Object-store abstraction with local filesystem backend first and Azure Blob
  backend second.
- Atomic publish manifests with checksums.
- Reader process can rebuild from empty local cache.
- Writer crash before publish exposes no partial state.
- Writer crash after publish is recoverable.
- Docker Compose demo includes Nexus, Prometheus, Grafana, and smoke container.
- Azure single-node demo exposes authenticated HTTPS, backup/restore, metrics,
  and FinReflectKG load/query path.

Do not claim:

- HA until writer failover works.
- distributed writes until replication works.
- cross-shard Cypher until query routing and semantics are implemented.

## Milestone 4: Neo4j-Class / Enterprise Parity

Goal: credible enterprise graph database parity, then selective superiority for
GraphRAG and regulated AI memory.

Required:

- Full external Cypher/openCypher/GQL tracking with stable denominators.
- GQL-aligned semantic binder and graph-algebra logical IR, not syntax-shaped
  AST execution.
- Official driver compatibility matrix across Python, JavaScript, Java, and Go.
- Administration: users, roles, privileges, tenant/database lifecycle, query
  listing and cancellation, backup inspection, consistency check, and upgrade
  tooling.
- Observability: Prometheus metrics, tracing spans, slow query logs, query
  plans/explain, memory/index/WAL/search dashboards, and actionable alerting.
- Security: stronger RBAC policy model, mTLS, audit fail-closed mode,
  encryption-at-rest integration, and a compliance evidence package.
- Replication and HA: leader-follower replication, failover, read consistency
  modes, follower reads, and eventually sharding / tenant placement.
- Ecosystem: Python, JavaScript, Java, and Go driver smoke; SDK examples;
  migration guide from Neo4j; GraphRAG cookbook; cloud demo scripts.
- Query optimization: query plan/explain, cost/cardinality model, slow-query
  analysis, tracing, and production dashboards.

Long-term acceptance:

- HA failover demo works with no committed data loss.
- Driver compatibility suites are green for the supported subset.
- Recovery time, durability, and consistency guarantees are documented and
  tested.
- GraphRAG workloads beat Neo4j/Helix/Arango on measured latency, retrieval
  quality, and operational simplicity.

## Mandatory Tranche Workflow

Every tranche must start by reading:

1. This master plan.
2. [internal-beta-plan-ledger.md](internal-beta-plan-ledger.md).
3. [known-limits.md](known-limits.md).
4. [../production-readiness.md](../production-readiness.md).

Every tranche must end by updating:

1. Checklist status in the relevant ledger.
2. Test evidence.
3. Benchmark evidence when performance or retrieval changes.
4. Known limits.
5. Production readiness status.

Rules:

- No workaround may become product behavior.
- If a feature is partial, mark it partial.
- If a feature is unsupported, reject it clearly.
- If a claim is not backed by code and tests, do not make the claim.
- Do not mix TCK denominators; use [../tck-count-ledger.md](../tck-count-ledger.md).
- Do not represent local Scope B success as full external corpus success.
- Do not market lower-level in-memory helpers as durable server paths.

## Required Evidence By Milestone

Internal Beta:

- `cargo fmt --check`
- `cargo test --workspace`
- local TCK Scope B and skipped-inclusive Scope B+
- GraphRAG readiness
- real-data B1-B10 benchmark assertions
- GraphRAG retrieval benchmark assertions
- Docker build and smoke
- config validation tests
- backup/restore tests
- optional Neo4j driver smoke observed and recorded

Single-node GA:

- multi-day soak
- repeated crash/failure injection
- backup/restore under write load
- compaction under query load
- memory pressure and query cancellation tests
- persisted index restart tests
- Python wheel clean-venv install
- official Neo4j Python and JavaScript driver smoke required

Cloud Preview:

- object-store publish/recovery tests
- reader rebuild from empty cache
- writer crash before/after publish
- Azure deployment smoke
- Prometheus/Grafana dashboard smoke
- authenticated HTTPS demo

Neo4j-class:

- external TCK corpus tracking
- official driver compatibility matrix
- HA failover tests
- cluster backup/restore tests
- long-running mixed workload soak
- security and audit evidence checks

## Assumptions And Defaults

- Do **all three strategic axes**, but in sequence:
  1. Internal Beta
  2. Single-node GA
  3. Cloud Preview
  4. Neo4j-class enterprise parity
- Server product remains first.
- Cypher remains the public query language now.
- Internals should move toward graph algebra / GQL-aligned semantics, not AQL
  and not a Helix-style DSL-first model.
- Documents are first-class, but queried through Cypher/GQL-style surfaces.
- Distributed sharding is deferred until replication/read-replica foundations
  are correct.
- Every future coding tranche must begin by reading:
  - `docs/product/nexus-production-grade-master-plan.md`
  - `docs/product/internal-beta-plan-ledger.md`
  - `docs/product/known-limits.md`
  - `docs/production-readiness.md`
- Every tranche must end by updating the relevant checklist and known-limits
  entries.
- If a feature is partial, the product must either reject unsupported cases
  clearly or document the limitation. No silent fallback, no fake compatibility,
  no workaround marketed as done.
