# NornicDB Reference Gap Analysis

NornicDB is a useful reference for Domyn Nexus, but not a drop-in code source.
It is written in Go, uses a different storage architecture, and mixes graph,
vector, temporal MVCC, protocol compatibility, and AI-native product surfaces in
one system. Nexus should borrow its externally visible contracts and test ideas,
not its engine wholesale.

Reference links:

- Repository: <https://github.com/orneryd/NornicDB>
- Transaction guide: <https://github.com/orneryd/NornicDB/blob/main/docs/user-guides/transactions.md>
- License: <https://github.com/orneryd/NornicDB/blob/main/LICENSE.md>

## What Helps Nexus Quickly

### 1. Transaction Contract Tests

NornicDB's strongest architectural signal is snapshot-isolated transaction
behavior: repeatable reads, read-your-writes inside a transaction, safe
rollback, and conflict detection. Nexus should use this as a contract suite.

Current Nexus state:

- Read transactions hold a read guard for their lifetime, so repeatable reads
  are real.
- Writes are single-writer and staged on a cloned graph before atomic publish.
- Server writes are WAL-before-apply through `NexusEngine::execute_write()`.
- There is no MVCC version chain, no concurrent writer conflict detection, and
  no non-blocking writer publish while long readers are active.

Immediate bridge:

- Add tests proving uncommitted writes are invisible.
- Add tests proving prepared WAL-ready writes are invisible until the final
  commit swap.
- Keep explicit MVCC and conflict detection listed as future work.

### 2. Bolt Compatibility as a Product Gate

NornicDB treats Neo4j driver compatibility as a product surface, not a nice-to-have.
Nexus should do the same.

Current Nexus state:

- Bolt supports auth, TLS/mTLS, multi-chunk reads, `RUN` parameters,
  `PULL {n}`, `ROUTE`, read-only explicit transactions, failure state, query
  timeout, row budgets, and byte budgets.
- Explicit write transactions are still rejected.
- There is no official Neo4j driver certification suite in CI yet.

Immediate bridge:

- Add driver-level integration tests using at least the official Python Neo4j
  driver or JavaScript driver against local Nexus.
- The first official-driver smoke script now lives at
  [`../scripts/neo4j-driver-smoke.py`](../scripts/neo4j-driver-smoke.py). It
  verifies connectivity, auto-commit `CREATE`, parameterized `MATCH`, and
  relationship creation through the Neo4j Python driver.
- Treat "driver can run GraphRAG CRUD and read patterns" as a pre-production
  gate, separate from unit-level Bolt packet tests.

### 3. Hybrid Graph + Vector Benchmarks

NornicDB's GraphRAG pitch is graph traversal and vector search in the same
engine. Nexus now has named vector indexes, HNSW-style ANN, exact-oracle
metrics, vector snapshot persistence, and `vectorSearch(...)` in Cypher.

Immediate bridge:

- Add benchmark cases for vector-only, vector+1-hop, vector+2-hop, and
  vector+property-filtered expansion.
- Track recall against exact search in benchmark output.
- Keep the synthetic lane in `nexus-bench` as
  `graphrag_retrieval_bench` so vector+graph performance can be measured
  without the local 10-K dataset.
- Keep B1-B10 graph benchmarks, but add a GraphRAG retrieval lane rather than
  hiding vector performance inside generic query timings.

### 4. Operational Packaging

NornicDB's repository surfaces Docker, Helm, config examples, admin UI, and
protocol endpoints prominently. Nexus has the runtime hooks, but packaging is
not yet at the same product level.

Immediate bridge:

- Add a production example config covering HTTP TLS, Bolt TLS, auth principals,
  storage paths, backup root, audit log path, query limits, memory budget, and
  compaction threshold.
- Keep the example under
  [`examples/production-single-node.json`](examples/production-single-node.json)
  and validate it in `nexus-server` tests so it stays loadable.
- Add a container smoke test that boots the single-node service, checks
  `/health`, `/ready`, `/metrics`, Bolt handshake, one write, one read, and one
  backup.
- The first HTTP/admin smoke script now lives at
  [`../scripts/smoke-single-node.sh`](../scripts/smoke-single-node.sh). Bolt
  driver smoke still needs an official-driver test harness.
- A local dev config and launcher now live at
  [`examples/dev-single-node.json`](examples/dev-single-node.json) and
  [`../scripts/run-dev-single-node.sh`](../scripts/run-dev-single-node.sh), so
  smoke testing does not require hand-built TLS material.
- A root `Dockerfile` and `.dockerignore` now exist for single-binary packaging;
  the ignore file excludes `target/` and reference checkouts from build context.
- Keep Helm/Kubernetes as later work until single-node config and container
  behavior are boring.

## What Does Not Bridge Quickly

- Real MVCC: Nexus currently uses SWMR plus staged graph swaps. Adding MVCC
  means versioned vertex/edge/property records, pruning, timestamped reads,
  conflict validation, and query/executor awareness of read versions.
- Distributed writes: Nexus has deterministic write-op records and placement
  metadata, but no Raft or follower replay service. NornicDB's distributed
  claims do not remove that work.
- GPU execution and local embedding inference: useful for product direction,
  but not the current single-node database hardening bottleneck.
- Wholesale Go code reuse: the storage engine and runtime model are different
  enough that porting would likely be slower than implementing narrow tests and
  contracts in Rust.

## Gap Mapping

| NornicDB Surface | Nexus Current State | Bridge Action |
|---|---|---|
| Snapshot isolation | Read guards provide repeatable reads; writes are single-writer staged swaps | Add publish/visibility tests now; design MVCC later |
| Read-your-writes | Server write streams use a staged graph; bare `WriteTx` is a mutation buffer | Keep server behavior tested; do not promise bare `WriteTx` read API |
| Conflict detection | Not needed under single writer; no MVCC conflicts | Future MVCC tranche |
| Historical reads | Not implemented | Future temporal/MVCC storage model |
| Neo4j compatibility | Strong unit-level Bolt progress; optional Python-driver smoke script exists; CI gate pending | Add official driver CI suite |
| Hybrid graph+vector | Vector APIs, HNSW-style ANN, snapshots, Cypher functions exist | Add retrieval benchmarks and recall reports |
| Product packaging | Runtime hooks exist; examples/container tests incomplete | Add production config example and container smoke |

## Concrete Next Test Tranches

1. Transaction visibility tests in `nexus-core`.
2. Promote the official Neo4j driver smoke script into CI against a live
   `nexus-server` process.
3. Hybrid vector+graph benchmark lane in `nexus-bench`.
4. Production example config plus config-loader test.
5. Container smoke script once the production config is stable.

## Production Rule

Do not claim NornicDB-equivalent transactional behavior until Nexus has either:

- documented SWMR semantics as the production contract, or
- implemented real MVCC with versioned reads, conflict detection, pruning, and
  recovery tests.
