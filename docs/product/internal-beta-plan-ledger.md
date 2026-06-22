# Internal Beta Plan Ledger

This ledger supports the canonical
[Nexus production-grade master plan](nexus-production-grade-master-plan.md).

This ledger maps the full "Foolproof Engine + Product Packaging" plan to the
current implementation state. It is the source of truth for what is done, what
is partial, and what remains.

Mandatory workflow: every tranche starts by reading the master plan, this
ledger, [known-limits.md](known-limits.md), and
[../production-readiness.md](../production-readiness.md). Every tranche ends by
updating checklist status, test evidence, benchmark evidence when relevant, and
known limits. No workaround may be treated as product behavior.

Status legend:

- **Done**: implemented and covered by tests or executable gate.
- **Partial**: implementation exists but coverage, packaging, or operational
  proof is incomplete.
- **Not started**: no meaningful implementation in this workspace yet.
- **Deferred**: intentionally outside the current Server-First beta tranche.

## Progress Summary

| Area | Done | Partial | Not started | Deferred |
|---|---:|---:|---:|---:|
| Engine foolproofing gate | 30 | 0 | 0 | 0 |
| Server product shell | 37 | 1 | 0 | 0 |
| Embedded SDK product | 10 | 4 | 0 | 0 |
| Cloud demo product | 6 | 5 | 0 | 0 |
| Competitive/product positioning | 4 | 1 | 0 | 0 |

Overall: the **server shell is now real**, and the local packaged-image path
builds and boots. The one-command Internal Beta gate passes locally. Internal
Beta is not complete until the optional Neo4j driver smoke has been observed in
CI and the Docker smoke has passed on a clean CI runner.

Latest full gate evidence:

- `cargo xtask verify-internal-beta` passed locally.
- Workspace tests passed.
- Scope B and skipped-inclusive Scope B+ TCK both reported `100.0%` result-ok
  on the checked-in Falkor corpus.
- GraphRAG readiness passed all 14 query patterns.
- GraphRAG retrieval benchmark passed with `vector_recall@10=0.631`.
- Real-data benchmark passed B1-B10 thresholds on 47,542 vertices and 64,891
  edges; B9 p50 was `1.06ms`.
- `docker build -t domyn-nexus:internal-beta .` passed.
- `scripts/smoke-docker-single-node.sh` passed against the packaged image.
- `scripts/smoke-dev-single-node.sh` passed against a self-started dev server.
- Endpoint contract tests now pin `/cypher`, `/tenants`, and `/vectors/*`
  response shapes, with matching examples in
  [http-api-contract.md](http-api-contract.md).
- Production config validation now emits structured issue codes, fields,
  messages, and remediation text instead of raw strings.
- Release workflow now builds and smokes the Docker image, publishes tagged and
  `latest` images to GHCR, and uploads the image reference alongside binary
  artifacts and checksums.
- Python SDK now has persistent `Graph.open(path)`, `save_snapshot()`,
  `backup(path)`, and storage-backed Cypher writes routed through
  `NexusEngine::execute_write()`.
- Python SDK wheel CI now builds Linux/macOS wheels and runs
  `scripts/python-sdk-smoke.py` in a clean virtual environment.
- Docker Compose demo shell now includes Nexus, Prometheus, Grafana, and a
  smoke-test container, with a local demo script and security posture doc.
- Azure single-node skeleton now includes Bicep infrastructure, cloud-init VM
  bootstrap, managed disk, Key Vault, backup storage account, smoke script, and
  deployment docs.
- FinReflectKG demo loader now creates a durable Nexus data directory with a
  graph snapshot and named vector index snapshot.

## 1. Engine Foolproofing Gate

| Plan item | Status | Evidence | Next action |
|---|---|---|---|
| Define foolproof as no silent data loss, no silent wrong result, no unsecured production startup, no unbounded query path, no undocumented sharp edge | Done | [foolproof-engine-product-packaging-plan.md](foolproof-engine-product-packaging-plan.md) and [known-limits.md](known-limits.md) | Keep updated when gaps move. |
| `cargo xtask verify-internal-beta` command | Done | `crates/xtask`, `.cargo/config.toml` alias | Run full non-quick gate. |
| Gate runs `cargo fmt --check` | Done | `xtask` step | Complete. |
| Gate runs `cargo test --workspace` | Done | `xtask` step | Complete. |
| Gate runs local TCK Scope B | Done | `xtask` step | Complete. |
| Gate runs skipped-inclusive Scope B+ | Done | `xtask` step | Complete. |
| Gate runs GraphRAG readiness tests | Done | `xtask` step | Complete. |
| Gate runs real-data benchmark smoke | Done | Full release gate run executed `real_data_bench --assert-internal-beta` against 47,542 V / 64,891 E and passed B1-B10 thresholds | Keep dataset path stable in benchmark CI. |
| Gate asserts B1-B10 thresholds | Done | `real_data_bench --assert-internal-beta` | Tune thresholds as hardware baselines mature. |
| Gate runs GraphRAG retrieval benchmark | Done | Full release gate run executed `graphrag_retrieval_bench --assert-internal-beta`; `vector_recall@10=0.631` cleared the default beta threshold | Raise recall threshold after HNSW tuning. |
| GraphRAG vector recall reported every run | Done | `graphrag_retrieval_bench` prints `vector_recall@k` and assertion mode checks minimum recall | Consider higher recall target after HNSW tuning. |
| HTTP single-node smoke | Done | `scripts/smoke-dev-single-node.sh` self-starts a temporary dev server, runs health/ready/metrics/Cypher/backup smoke, and shuts it down; `xtask --run-http-smoke` uses this harness | Complete. |
| Optional Neo4j driver smoke | Done | `scripts/neo4j-driver-smoke.py` exists; `xtask` self-starts Nexus when `DOMYN_NEXUS_RUN_NEO4J_DRIVER_SMOKE=1`; CI includes an optional `neo4j-driver-smoke` job gated by `RUN_NEO4J_DRIVER_SMOKE` | Observe first enabled CI run. |
| Docker build + container smoke | Done | `docker build -t domyn-nexus:internal-beta .` completed from a clean image context, and `scripts/smoke-docker-single-node.sh` booted the image with TLS/auth/data/log/backup mounts and passed health/ready/metrics/Cypher/backup smoke | Keep this in the full gate and CI. |
| WAL truncation/corruption during active write | Done | WAL malformed/truncated/corrupt record tests exist in `nexus-storage`; `durable_recovery_skips_truncated_active_wal_tail_after_committed_write` proves a committed server write recovers while a torn active WAL tail is ignored | Complete. |
| Snapshot temp-file crash before rename | Done | Atomic tmp+fsync+rename implemented; `open_removes_stale_snapshot_tmp_without_replacing_committed_snapshot` proves stale `snapshot.json.tmp` residue is removed without replacing the last committed snapshot | Complete. |
| Backup during active writes | Done | `hot_backup_during_concurrent_writes_restores_a_valid_cutoff` runs a backup while writes continue and verifies restore produces a valid, queryable cutoff | Complete. |
| Restore from backup into fresh data dir | Done | Storage and server CLI helper roundtrip tests | Complete. |
| Restore rejects backup without manifest | Done | `restore_rejects_backup_without_manifest` | Complete. |
| Compaction during query traffic | Done | `compact_storage_is_safe_while_queries_are_running` runs concurrent read queries while tombstone compaction rebuilds storage and indexes | Complete. |
| Vector snapshot save/load with deletes and updates | Done | `vector_update_delete_compact_survives_hot_backup_restore` covers update, delete, vector compaction, hot backup, restore, and search-path validation | Complete. |
| Explicit Bolt write transactions return clear error | Done | `explicit_write_transaction_rejection_is_documented_and_specific` verifies the exact Bolt error code/message for explicit write tx rejection | Add official driver smoke for this case under the separate driver-smoke item. |
| Production mode rejects missing auth/TLS/storage/audit/backup/query limits | Done | `nexus-server` config tests and `check-config` CLI | Complete. |
| Unsupported Cypher fails at bind/parse with structured errors | Done | Binder rejects invalid scenarios, TCK expected-error accounting is green locally, parser rejects trailing tokens, and `unsupported_feature_matrix_rejects_without_partial_write` prevents unsupported `CALL`/`LOAD CSV` tails from partially mutating state | Expand the matrix when new unsupported clauses are discovered. |
| Row limit | Done | HTTP/executor tests and config | Complete. |
| Byte budget | Done | Executor/server result byte-budget paths and tests | Complete. |
| Process memory budget | Done | `test_cypher_endpoint_rejects_when_process_memory_budget_is_exceeded` verifies pre-query rejection and product error shape | Complete. |
| Cancellation before WAL append | Done | `execute_cypher_with_cancellation_token_does_not_append_wal` verifies cancelled durable writes do not mutate memory or replay from WAL after reopen | Complete. |
| Timeout while scanning/expanding/unwinding | Done | HTTP/Bolt timeout paths flip cancellation tokens; executor coverage now proves scan, expand, and unwind operators observe cancellation before continuing work | Complete. |
| Rate limit and concurrent-query rejection | Done | HTTP rate/concurrency tests exist | Complete. |

## 2. Server Product Shell First

| Plan item | Status | Evidence | Next action |
|---|---|---|---|
| `nexus-server --config <path>` | Done | Existing server startup path | Complete. |
| `nexus-server check-config --config <path>` | Done | CLI implemented and tested | Complete. |
| `nexus-server init --profile dev|production` | Done | CLI implemented and tested | Complete. |
| `nexus-server backup --config <path> --output <dir>` | Done | CLI implemented and roundtrip tested | Complete. |
| `nexus-server restore --backup <dir> --data-dir <dir>` | Done | CLI implemented and roundtrip tested | Complete. |
| Docker production image builds release binary only | Done | Docker build completed release `nexus-server` inside the builder stage and copied only the binary plus production config into the runtime image | Complete. |
| Docker healthcheck calls `/health` | Done | Dockerfile healthcheck uses `curl -fk https://127.0.0.1:8443/health` | Complete. |
| Default Docker config path `/etc/domyn-nexus/config.json` | Done | Dockerfile copy/env path | Complete. |
| No dev token in production image | Done | Production template uses placeholder principals, not `dev-token` | Complete. |
| JSON config v1 | Done | `NexusServerConfig` JSON templates | Complete. |
| Schema docs for every config field | Done | [config-reference.md](config-reference.md) | Keep in sync with new fields. |
| Production and dev templates | Done | `docs/examples/production-single-node.json`, `docs/examples/dev-single-node.json` | Complete. |
| Config validation errors with remediation text | Done | `ConfigValidationIssue` carries stable `code`, `field`, `message`, and `remediation`; `check-config` prints those fields and [config-reference.md](config-reference.md) documents the shape | Complete. |
| Freeze `/health` endpoint | Done | Endpoint and docs | Complete. |
| Freeze `/ready` endpoint | Done | Endpoint and docs | Complete. |
| Freeze `/metrics` endpoint | Done | Endpoint and docs | Complete. |
| Freeze `/cypher` endpoint | Done | `contract_cypher_endpoint_response_shape` pins request/response JSON shape; [http-api-contract.md](http-api-contract.md) documents the example | Complete. |
| Freeze `/tenants` endpoint | Done | `contract_tenants_endpoint_response_shapes` pins list/create/conflict shapes; [http-api-contract.md](http-api-contract.md) documents the examples | Complete. |
| Freeze `/admin/backup` endpoint | Done | Endpoint, docs, tests | Complete. |
| Freeze `/admin/compact` endpoint | Done | Endpoint, docs, tests | Complete. |
| Freeze `/vector/*` endpoints | Done | `contract_vector_endpoint_response_shapes` pins create/list/upsert/search/delete/compact and missing-index error shapes; [http-api-contract.md](http-api-contract.md) documents the examples | Complete. |
| All errors return `{ code, message, details }` | Done | Handler-generated errors and JSON/body-limit extractor rejections include `code`, `message`, and `details`; tests cover auth, Cypher, backup, vector, malformed JSON, missing content type, and payload-too-large errors | Complete. |
| Bolt auto-commit writes supported | Done | Server/Bolt path uses same engine entry point | Complete. |
| Explicit Bolt write transactions rejected with documented error | Done | Code rejects unsupported explicit write tx paths and unit coverage verifies the documented code/message | Add official driver smoke under the optional driver gate. |
| Official Neo4j Python driver smoke optional gate | Done | Optional CI job installs the Neo4j Python driver and runs `scripts/smoke-dev-single-node.sh python3 scripts/neo4j-driver-smoke.py` when `RUN_NEO4J_DRIVER_SMOKE=1` | Observe first enabled CI run. |
| `docs/product/server-quickstart.md` | Done | Created | Complete. |
| `docs/product/config-reference.md` | Done | Created | Complete. |
| `docs/product/operations-runbook.md` | Done | Created | Complete. |
| `docs/product/known-limits.md` | Done | Created | Complete. |
| `docs/product/helix-neo4j-positioning.md` | Done | Created | Complete. |
| GitHub Actions for test/format/Docker/smoke | Done | CI has format, workspace tests, both TCK scopes, server compile, Docker build, Docker container smoke, and optional Neo4j driver smoke | Watch first clean hosted run. |
| Tagged release artifact: binary + Docker image + templates + checksums | Done | Release workflow packages binary/templates/scripts/changelog/checksum, smokes the Docker image, publishes tagged and `latest` images to GHCR, and uploads the image reference artifact | Observe first tagged release run. |
| `CHANGELOG.md` | Done | Created | Complete. |

### Server Internal Beta Acceptance

| Acceptance item | Status | Evidence | Next action |
|---|---|---|---|
| Fresh machine can run Docker and pass smoke in under 10 minutes | Partial | Local Docker image build and `scripts/smoke-docker-single-node.sh` passed; not yet repeated on a fresh machine or CI runner | Add CI service/container smoke and repeat on a clean host. |
| Production validator prevents insecure startup | Done | Config tests and `check-config` | Complete. |
| `cargo xtask verify-internal-beta` passes | Done | One-command full gate passed locally, including release benchmarks, Docker build, and Docker container smoke | Add the same gate to CI with stable runners. |
| B1-B10 benchmark remains green including B9 | Done | Release `real_data_bench --assert-internal-beta` passed; B9 p50 was below 2ms | Keep threshold in every full gate. |
| TCK Scope B and B+ remain `100.0% result-ok` | Done | Quick gate reports Scope B and B+ green | Complete. |
| Known unsupported features listed and tested as explicit failures | Done | Known limits listed; binder/TCK expected-error tests exist; server product matrix verifies unsupported `CALL`/`LOAD CSV` forms fail without partial writes | Expand as the public unsupported matrix grows. |

## 3. Embedded SDK Product Second

| Plan item | Status | Evidence | Next action |
|---|---|---|---|
| Python wheels via maturin for macOS arm64/x86_64 and Linux x86_64 | Partial | `.github/workflows/python-wheels.yml` builds Linux x86_64, macOS x86_64, and macOS arm64 wheels with maturin | Observe first hosted CI run and attach artifacts to releases if desired. |
| `Graph.open(path)` | Done | Python `Graph.open(path)` opens `NexusStore`, loads the graph, and wraps `NexusEngine::with_store` | Add wheel/venv runtime test. |
| `Graph.save_snapshot()` | Done | Persistent SDK graphs call `NexusEngine::save_snapshot()`; in-memory graphs reject with a clear error | Add wheel/venv runtime test. |
| `Graph.cypher(query, params={})` | Done | In-memory graphs support parameterized reads; persistent graphs route reads/writes through `NexusEngine::execute_cypher_with_params` | Complete. |
| `Graph.vector_index(name, dim, mode="hnsw")` | Done | Persistent SDK graphs expose `NamedVectorIndex` over `NexusEngine` named vector methods; create/load/upsert/search/remove/compact persist through storage snapshots | Complete. |
| `Graph.backup(path)` | Done | Persistent SDK graphs call `NexusEngine::backup_to` and return the backup manifest as a Python dict | Add wheel/venv runtime test. |
| Typed exception classes matching server error codes | Done | SDK exposes `NexusError`, `CypherError`, `StorageError`, `SchemaError`, and `VectorError`; major Cypher/storage/schema/vector paths map into the typed hierarchy | Add richer structured attributes later if needed. |
| GraphRAG ingestion example | Done | [graphrag-cookbook.md](graphrag-cookbook.md) includes document/chunk/entity ingestion with persistent `Graph.open(path)` | Complete. |
| Vector + graph retrieval example | Done | [graphrag-cookbook.md](graphrag-cookbook.md) includes standalone `VectorIndex` search followed by graph expansion | Complete. |
| Bounded traversal example | Done | [graphrag-cookbook.md](graphrag-cookbook.md) documents `nx.subgraph(...)` for bounded context expansion | Complete. |
| Snapshot recovery example | Done | [graphrag-cookbook.md](graphrag-cookbook.md) reopens a persistent graph after `save_snapshot()` | Complete. |
| Tenant-scoped retrieval example | Partial | [graphrag-cookbook.md](graphrag-cookbook.md) shows `tenant_id` filtering and states that hard tenant authorization belongs to the server product | Decide whether embedded tenant namespaces become a first-class SDK API. |
| Clean venv wheel install test | Partial | `scripts/python-sdk-smoke.py` imports the installed wheel, exercises in-memory and persistent graphs, snapshot, backup, vector search, and typed exceptions inside the wheel workflow | Observe first hosted CI run. |
| Persist/reopen graph test | Partial | `scripts/python-sdk-smoke.py` exercises persistent create, snapshot, reopen, query, and backup through the installed wheel workflow | Observe first hosted CI run. |

## 4. Cloud Demo Product Third

| Plan item | Status | Evidence | Next action |
|---|---|---|---|
| `deploy/azure/single-node/` | Done | `deploy/azure/single-node/` contains Bicep, cloud-init, example parameters, smoke script, and README | Run against a clean Azure resource group. |
| VM or container app deployment | Partial | `main.bicep` provisions a Linux VM and cloud-init starts the Nexus container; it has not been validated in Azure yet | Execute deployment and fix provider/runtime issues. |
| Managed disk | Done | `main.bicep` provisions and attaches a managed data disk; `cloud-init.yaml` formats/mounts it at `/var/lib/domyn-nexus` | Validate in Azure. |
| Key Vault secrets | Partial | `main.bicep` creates Key Vault secrets and grants the VM managed identity `Key Vault Secrets User`; `cloud-init.yaml` fetches tokens from Key Vault at boot and writes `/etc/domyn-nexus/config.json` locally | Validate in a clean Azure resource group. |
| TLS cert mounting | Partial | Docker Compose demo mounts generated local certs into Nexus; Azure Bicep accepts optional PEM cert/key parameters, stores them in Key Vault, and `cloud-init.yaml` installs/validates them before Nexus starts, with a self-signed demo fallback | Validate real cert install in Azure and add rotation guidance. |
| Backup path to Blob | Partial | `main.bicep` creates a storage account and `nexus-backups` Blob container, grants the VM managed identity `Storage Blob Data Contributor`, and `cloud-init.yaml` installs an hourly backup-to-Blob sync timer | Validate upload permissions and large-backup behavior in Azure. |
| Azure Monitor/Grafana wiring | Partial | Docker Compose demo includes Prometheus/Grafana provisioning; Azure Bicep creates a Log Analytics workspace, Azure Monitor Agent, a Data Collection Rule, and a VM DCR association for performance counters and syslog | Validate telemetry in Azure and add Nexus `/metrics` scraping/dashboard wiring. |
| `deploy/docker-compose/` with Nexus/Prometheus/Grafana/smoke | Done | `deploy/docker-compose/docker-compose.yml` defines Nexus, Prometheus, Grafana, and a smoke-test service; `deploy/docker-compose/up.sh` generates demo certs and starts the stack | Run on a clean Docker host. |
| FinReflectKG sample loader | Done | `finreflectkg_demo_loader` builds a durable graph snapshot and named vector snapshot from triplet JSON into a Nexus data directory | Add larger demo dataset automation later. |
| `docs/product/deployment-azure.md` | Done | Created and linked from [README.md](README.md) | Complete. |
| `docs/product/demo-script.md` and `security-posture.md` | Done | Created and linked from [README.md](README.md) | Complete. |

## 5. Competitive Posture

| Plan item | Status | Evidence | Next action |
|---|---|---|---|
| Treat HelixDB as market validation, not code donor | Done | [helix-neo4j-positioning.md](helix-neo4j-positioning.md) | Complete. |
| Package around Cypher/Bolt compatibility | Done | Docs and server product path | Continue driver smoke. |
| Package around GraphRAG benchmarks | Done | GraphRAG retrieval and real-data benchmark assertion modes both ran and passed in the release gate path | Keep thresholds documented. |
| Package around Python embedding | Partial | SDK exists but product packaging incomplete | SDK phase. |
| Package around regulated-data readiness | Done | Production config validation, auth/TLS/audit/backup docs, and [security-posture.md](security-posture.md) | Complete. |

## Immediate Next Tranches

1. Observe clean hosted CI runs for Docker smoke, optional Neo4j driver smoke,
   release publishing, and Python wheel smoke.
2. Finish embedded SDK product polish: observed wheel CI and a decision on
   first-class embedded tenant namespaces.
3. Validate the Cloud Demo shell on real infrastructure: clean-host compose run,
   Azure resource-group deployment, Blob backup sync, managed-identity Key
   Vault secret retrieval, and Azure Monitor wiring.
