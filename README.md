# Domyn Nexus

Domyn Nexus is the native Rust graph database track inside Domyn Graph. It is being built as a Cypher-first, GraphRAG-focused, single-node database product with an embedded Python SDK path and a later distributed roadmap.

Current product stage: **Internal Beta in progress**. Nexus is not yet GA, not HA, and not distributed. The canonical roadmap and honesty contract are:

- [Production-grade master plan](docs/product/nexus-production-grade-master-plan.md)
- [Internal beta execution ledger](docs/product/internal-beta-plan-ledger.md)
- [Production readiness status](docs/production-readiness.md)
- [Known limits](docs/product/known-limits.md)

## Repository Status

The active branch for Nexus work is:

```bash
domyn-nexus/main
```

The configured GitHub remote is currently:

```bash
https://github.com/jtthachil-domyn/domyn-janusgraph.git
```

That remote name is historical. This branch contains the Rust-native Nexus implementation.

## Quick Start

Check the workspace:

```bash
cargo check --workspace
```

Run the test suite:

```bash
cargo test --workspace
```

Run the internal beta gate:

```bash
cargo xtask verify-internal-beta
```

Run the server locally:

```bash
cargo run -p nexus-server --bin nexus-server -- --config docs/examples/dev-single-node.json
```

For a fuller server walk-through, see [Server quickstart](docs/product/server-quickstart.md).

## Top-Level Layout

| Path | Purpose |
|---|---|
| [Cargo.toml](Cargo.toml) | Rust workspace manifest for all Nexus crates. |
| [Cargo.lock](Cargo.lock) | Locked dependency graph for reproducible builds and handoff. |
| [.cargo/](.cargo/) | Cargo aliases and local build profile settings. |
| [crates/](crates/) | Rust crates that make up the engine, query layer, server, SDK, benchmarks, and tooling. |
| [docs/](docs/) | Architecture, roadmap, product, operations, examples, and readiness documentation. |
| [deploy/](deploy/) | Docker Compose and Azure single-node deployment scaffolding. |
| [scripts/](scripts/) | Local smoke tests and optional driver/SDK verification scripts. |
| [references/](references/) | Local-only reference repos used for research; ignored from Git and not Nexus source. |
| [Dockerfile](Dockerfile) | Server container image build. |
| [CHANGELOG.md](CHANGELOG.md) | Human-readable release/change history. |

## Crates

| Crate | Purpose |
|---|---|
| [nexus-core](crates/nexus-core/) | Graph storage primitives: vertices, edges, CSR adjacency, properties, transactions, tombstones. |
| [nexus-storage](crates/nexus-storage/) | WAL, snapshots, backup/restore, recovery, and persistence lifecycle. |
| [nexus-index](crates/nexus-index/) | Composite, full-text, and vector indexes. |
| [nexus-cypher](crates/nexus-cypher/) | Cypher lexer, parser, binder, logical planner, executor, mutation semantics, and TCK runner. |
| [nexus-parser](crates/nexus-parser/) | Adapter/spike layer for external parser integration experiments. |
| [nexus-server](crates/nexus-server/) | HTTP API, Bolt server, config validation, TLS hooks, metrics, admin endpoints, durable engine wrapper. |
| [nexus-python](crates/nexus-python/) | PyO3 embedded SDK for in-process GraphRAG use cases. |
| [nexus-algebra](crates/nexus-algebra/) | GraphBLAS-style algebra and semiring primitives. |
| [nexus-algorithms](crates/nexus-algorithms/) | Graph algorithms built on the core/algebra layers. |
| [nexus-tenant](crates/nexus-tenant/) | Tenant namespace and storage layout support. |
| [nexus-distributed](crates/nexus-distributed/) | Distributed-readiness scaffolding; full Raft/sharding is intentionally deferred. |
| [nexus-bench](crates/nexus-bench/) | Synthetic, real-data, and GraphRAG benchmark binaries. |
| [xtask](crates/xtask/) | Repository automation, including `verify-internal-beta`. |

## Documentation Map

### Canonical Planning And Status

| Doc | Use |
|---|---|
| [Production-grade master plan](docs/product/nexus-production-grade-master-plan.md) | Top-level plan for Internal Beta, single-node GA, cloud preview, and Neo4j-class parity. Start here. |
| [Internal beta execution ledger](docs/product/internal-beta-plan-ledger.md) | Checklist and evidence ledger for beta gates. Update after every tranche. |
| [Production readiness status](docs/production-readiness.md) | Current claims, gaps, and measured readiness. |
| [Known limits](docs/product/known-limits.md) | Explicit unsupported or partial behavior. This is the honesty contract. |
| [openCypher production and scale roadmap](docs/opencypher-production-and-scale-roadmap.md) | Query-language and scale roadmap. |
| [TCK count ledger](docs/tck-count-ledger.md) | Exact TCK scope/counter definitions so pass-rate numbers do not drift. |
| [Query language architecture](docs/query-language-architecture.md) | Cypher, GQL, binder, IR, planner, and executor direction. |

### Product Docs

| Doc | Use |
|---|---|
| [Product docs index](docs/product/README.md) | Index for the product documentation set. |
| [Server quickstart](docs/product/server-quickstart.md) | Start and smoke-test the server. |
| [Config reference](docs/product/config-reference.md) | JSON config fields, defaults, and production validation rules. |
| [HTTP API contract](docs/product/http-api-contract.md) | Endpoint request/response shapes and errors. |
| [Operations runbook](docs/product/operations-runbook.md) | Backup, restore, compaction, smoke tests, and common operations. |
| [Security posture](docs/product/security-posture.md) | Current security claims and limits. |
| [Python quickstart](docs/product/python-quickstart.md) | Embedded SDK basics. |
| [GraphRAG cookbook](docs/product/graphrag-cookbook.md) | Ingestion and retrieval patterns for GraphRAG. |
| [SDK limits](docs/product/sdk-limits.md) | Python/Rust embedded SDK limits. |
| [Document collections](docs/product/document-collections.md) | Document collection support and current limitations. |
| [Helix/Neo4j positioning](docs/product/helix-neo4j-positioning.md) | Competitive positioning. |
| [Demo script](docs/product/demo-script.md) | Demo flow. |
| [Azure deployment](docs/product/deployment-azure.md) | Single-node Azure deployment outline. |

### Supporting Research And Architecture

| Doc | Use |
|---|---|
| [Original Nexus build plan](docs/domyn-nexus-build-plan.md) | Historical architecture/build context. Do not treat every status line as current. |
| [NornicDB gap analysis](docs/reference-nornicdb-gap-analysis.md) | Reference analysis against another Rust graph/vector system. |
| [ArangoDB due diligence](docs/arangodb-acquisition-due-diligence.md) | Multi-model/document strategy reference. |

### Examples And Ops Assets

| Path | Use |
|---|---|
| [docs/examples/dev-single-node.json](docs/examples/dev-single-node.json) | Development server config. |
| [docs/examples/production-single-node.json](docs/examples/production-single-node.json) | Production-style config template. |
| [docs/ops/monitoring.md](docs/ops/monitoring.md) | Monitoring setup notes. |
| [docs/ops/prometheus-alerts.yml](docs/ops/prometheus-alerts.yml) | Prometheus alert examples. |
| [docs/ops/grafana-dashboard.json](docs/ops/grafana-dashboard.json) | Grafana dashboard JSON. |

## Deployment And Smoke Tests

| Path | Purpose |
|---|---|
| [deploy/docker-compose/](deploy/docker-compose/) | Nexus + Prometheus + Grafana local deployment. |
| [deploy/azure/single-node/](deploy/azure/single-node/) | Azure single-node demo deployment scaffolding. |
| [scripts/run-dev-single-node.sh](scripts/run-dev-single-node.sh) | Start a local development server. |
| [scripts/smoke-single-node.sh](scripts/smoke-single-node.sh) | Generic HTTP smoke test. |
| [scripts/smoke-dev-single-node.sh](scripts/smoke-dev-single-node.sh) | Dev server smoke test. |
| [scripts/smoke-docker-single-node.sh](scripts/smoke-docker-single-node.sh) | Docker smoke test. |
| [scripts/neo4j-driver-smoke.py](scripts/neo4j-driver-smoke.py) | Optional Neo4j Python driver smoke test. |
| [scripts/python-sdk-smoke.py](scripts/python-sdk-smoke.py) | Optional Python SDK smoke test. |

## Reference Repos

The `references/` directory is intentionally ignored. It is for local research only:

- FalkorDB: TCK and Cypher behavior reference.
- KyuGraph/Kuzu-derived work: parser, binder, storage, and execution ideas.
- OverGraph: engine-hardening and test-parity inspiration.

Do not copy claims from reference repos into Nexus docs unless Nexus has tests proving the behavior.

## Handoff Workflow

Before starting a tranche:

1. Read the [master plan](docs/product/nexus-production-grade-master-plan.md).
2. Read the [internal beta ledger](docs/product/internal-beta-plan-ledger.md).
3. Read [known limits](docs/product/known-limits.md).
4. Check `git status --short`.

After finishing a tranche:

1. Run the relevant tests and record evidence.
2. Update docs in the same tranche as code.
3. Update known limits if anything is partial.
4. Commit in a scoped slice.
5. Push `domyn-nexus/main`.

No workaround rule: unsupported features must fail clearly. Partial features must be marked partial. Do not silently fall back to behavior that risks wrong answers.

## Why GitHub May Show Java Instead Of Rust

The remote repository name and history come from the JanusGraph track, which is Java-heavy. GitHub language stats are computed by GitHub Linguist over the repository/default branch and can be skewed by:

- the default branch still containing JanusGraph Java code,
- vendored/reference repos if they are ever committed,
- generated artifacts such as exported HTML, dashboards, or reports.

This branch is Rust-native. The `.gitattributes` file marks local reference and generated paths so that, if this branch becomes the default or is viewed independently, Linguist should focus on Nexus source instead of generated/reference material.
