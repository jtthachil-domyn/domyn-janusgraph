# Domyn Nexus Full Production Roadmap

This is the canonical roadmap for turning Domyn Nexus from an alpha-quality
single-node graph engine into a production database product. It coordinates
three tracks that must move together: openCypher conformance, production engine
hardening, and distributed-readiness scaffolding.

## Verified Baseline

- Workspace test baseline before this roadmap: 212 passing tests.
- Local Falkor/openCypher TCK copy: 1,615 total Gherkin scenarios.
- Previous TCK runner baseline: 1,124 considered scenarios, 43.6% parse-ok,
  36.9% exec-ok, 14.7% primitive result-match.
- Local OverGraph Rust suite: 1,095 listed Rust tests. Treat OverGraph as a
  storage/operations hardening reference, not as an openCypher conformance
  source.
- `nexus-distributed` is not a distributed database yet. It starts with stable
  placement and replication-log types, then later grows into Raft and sharding.

## Current Execution Status

- Measurement completeness has started: the TCK runner expands Scenario
  Outlines, reads parameter tables, handles expected-error scenarios, loads the
  current binary-tree fixtures, and emits JSON summaries.
- The current expanded TCK measurement is 3,870 total scenarios, 3,830
  considered, 82.1% parse-ok, 64.7% exec-ok, and 42.8% result-match.
- Binder/scope work has started in `nexus-cypher` with variable-kind tracking,
  early undefined-variable checks, and strict boolean-literal validation. Full
  Cypher grouping and broad type rules are not complete.
- Expressions have advanced: boolean expected-error conformance is green in the
  local runner, list predicates execute with scoped variables, and temporal
  constructors/accessors have a compatibility layer. Full temporal value types,
  current-time functions, duration arithmetic, and timezone semantics remain
  pending.
- Aggregation is green in the local runner: grouped projection, `DISTINCT`
  inside aggregate calls, `collect`, `count`, `sum`, `avg`, `min`, `max`,
  `percentileDisc`, and `percentileCont` pass the local aggregation category.
  Broader binder enforcement for aggregate/non-aggregate grouping rules still
  needs hardening.
- Pattern matching has moved forward: TCK `Background` setup is now applied by
  the runner, relationship property predicates are preserved through
  AST/planning/execution, null-bound variables no longer expand into full
  scans, and multi-relationship initial patterns route through the edge-unique
  pattern matcher.
- List semantics now include null-aware equality/`IN` behavior and explicit
  null slice-bound handling, moving the local `expressions/list` category from
  106 to 121 result-matching scenarios.
- OverGraph-style recovery work has started with malformed/truncated WAL record
  handling. WAL rotation, retention, and hot backup are still pending.
- Distributed readiness has metadata and deterministic commit-record
  scaffolding. There is still no Raft, network replication, sharding, or
  cross-shard Cypher.

## Implementation Rules

- The server write path must remain `NexusEngine::execute_write()` so all
  durable writes are WAL-before-apply and atomically committed.
- The in-memory helper `run_cypher_mut_in_memory()` remains non-durable and is
  only for tests, embedded convenience, and TCK measurement.
- Semantic validation belongs in `nexus-cypher` binder/planner/executor layers,
  not in `nexus-server`.
- KyuGraph/Kuzu should be used for parser, binder, and vectorized execution
  ideas. Nexus should not become a wholesale fork.
- Falkor/openCypher TCK is the Cypher compatibility source of truth.
- OverGraph is the durability, compaction, vector, pagination, and crash
  recovery reference.
- Full Raft and cross-shard Cypher stay deferred until local write/recovery and
  Cypher semantics are substantially stable.

## Track 1: openCypher Conformance

Target: make Nexus a TCK-measured openCypher implementation instead of a growing
handwritten subset.

### Phase 1: Measurement Completeness

- Expand `Scenario Outline` / `Examples` into executable scenarios.
- Treat expected-error scenarios as measurable pass/fail outcomes.
- Add fixture graph loading for local TCK fixtures, starting with
  `binary-tree-1` and `binary-tree-2`.
- Emit machine-readable JSON with totals, considered count, skipped count,
  parse-ok, exec-ok, result-ok, mismatch counts, expected-error matches, and
  top failing categories.

### Phase 2: Binder and Scope

- Add a binder stage between parser and planner.
- Track variable kinds: node, relationship, path, scalar, list, map, unknown.
- Validate variable visibility across `WITH`, `UNWIND`, `OPTIONAL MATCH`,
  `RETURN`, and write clauses.
- Reject undefined variables and invalid aggregate shapes before physical
  planning.
- Continue moving semantic checks from parser/planner into binder as TCK gaps are
  closed.

### Phase 3: Expressions

- Implement Cypher three-valued logic: true, false, null.
- Fix null propagation for comparisons, boolean ops, arithmetic, string ops,
  list operations, and map operations.
- Implement TCK-critical functions: `labels`, `type`, `id`, `keys`,
  `properties`, `size`, `length`, `nodes`, `relationships`, `head`, `last`,
  `tail`, `range`, `coalesce`, `toString`, `toInteger`, `toFloat`,
  `toBoolean`.
- Implement list predicates `any`, `all`, `none`, `single` with true bound
  predicate-variable evaluation.
- Add a real temporal value model. The current compatibility layer normalizes
  common constructors/accessors to strings for TCK progress, but production
  Cypher needs typed date, time, localdatetime, datetime, and duration values.

### Phase 4: Aggregation and Projection

- Implement grouped aggregation. **Current status: implemented for local TCK
  coverage.**
- Support `DISTINCT` inside aggregate calls. **Current status: implemented.**
- Support `collect`, `count(*)`, `count(expr)`, `sum`, `avg`, `min`, `max`.
  **Current status: implemented; percentile aggregates are also covered.**
- Enforce Cypher grouping rules in binder. **Current status: partial; keep
  extending through failure-driven TCK work.**

### Phase 5: Patterns and Paths

- Add path values for named paths.
- Implement `nodes(p)`, `relationships(p)`, and `length(p)`.
- Fix variable-length path uniqueness semantics to match openCypher.
- Track relationship properties in pattern matching.
- Preserve relationship variable bindings through read and write plans.

### Phase 6: Clause Completion

- Harden `OPTIONAL MATCH` outer-join semantics.
- Complete `WITH ... WHERE`, `WITH ... ORDER BY`, `WITH ... SKIP`, and
  `WITH ... LIMIT`.
- Complete `UNION` column compatibility and expected error behavior.
- Keep `CALL` as a later phase unless TCK categories force a minimal procedure
  implementation earlier.

### Phase 7: Writes

- Complete `CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`.
- Add `ON CREATE SET` and `ON MATCH SET`.
- Ensure every server-executed write goes through WAL-before-apply.

### Gates

- Gate A: parse-ok >= 75% on local Falkor TCK.
- Gate B: exec-ok >= 65%.
- Gate C: primitive result-match >= 50%.
- Gate D: primitive result-match >= 80% with expected-error support enabled.
- Gate E: near-green local Falkor TCK, with every skipped scenario documented.

## Track 2: OverGraph-Style Engine Parity

Target: use OverGraph's storage and operations suite as the production hardening
reference for the single-node engine.

### WAL and Recovery

- Add corrupt-tail, truncated-record, malformed-record, wrong-version,
  replay-after-delete, and last-write-wins tests.
- Add WAL segment rotation and retention.
- Add recovery validation that compares topology, IDs, properties, indexes, and
  query results before and after restart.

### Compaction

- Compact vertex and edge tombstones.
- Compact `edge_meta` and adjacency deltas.
- Add manual compaction first, then threshold-driven background compaction.
- Ensure snapshot, WAL, and compaction cannot produce split-brain state after a
  crash.

### Indexes

- Incrementally maintain unique, composite, full-text, and vector indexes on
  create, set, remove, delete, and compaction.
- Add stale-entry tests for property updates and deletes.
- Add rebuild-from-graph validation for every index type.

### Vector Search

- Replace brute-force vector search with HNSW or a compatible ANN layer.
- Keep exact search as the correctness oracle.
- Add dense-vector recall, restart, and compaction parity tests.

### Pagination and Bounds

- Add cursor-based pagination for node scans, edge scans, and traversal results.
- Add bounded result limits, memory ceilings, and query cancellation.
- Test stable order, deleted records, cursor round-trips, and bounded traversal.

### Operations

- Add config for WAL mode, snapshot interval, compaction thresholds, memory
  budget, query timeout, and index rebuild mode.
- Add metrics for WAL bytes, recovery time, compaction time, active queries,
  memory estimate, index sizes, and query latency buckets.
- Keep health and readiness separate: health means process alive; readiness
  means reads/writes can be served.

### Gates

- Port an initial 50 OverGraph-inspired tests into Nexus equivalents.
- Expand to 250 parity tests across WAL, compaction, indexes, vector,
  pagination, and crash recovery.
- Do not claim production-ready until crash/recovery and compaction tests pass
  under repeated runs.

## Track 3: Distributed Readiness

Target: avoid single-node-only assumptions without prematurely implementing full
sharding.

### Metadata and Placement

- Define stable `NodeId`, `ShardId`, `ReplicaId`, `TenantPlacement`,
  `ReplicationRole`, and `ReadConsistency` types.
- Route by tenant ID first.
- Keep the default deployment as single-node/single-shard.

### Replication Log

- Wrap WAL operations in a replication operation type.
- Make future commit records deterministic and serializable.
- Preserve exact write ordering for replay and future Raft replication.

### Shard-Local Engine Boundary

- Define a local-engine trait that can apply a commit record.
- Keep network consensus out of this phase.
- Keep single-node execution fast and direct.

### Future Leader-Follower Flow

The intended future flow is:

1. Client sends write to leader.
2. Leader validates and builds a deterministic commit record.
3. Leader appends the replicated log.
4. Leader applies to local WAL/graph.
5. Followers replay the same commit record.
6. Reads choose local, leader-linearizable, or follower-stale consistency.

Unsupported in distributed v1: cross-shard Cypher, distributed transactions,
automatic rebalancing, and scatter-gather traversal.

## Required Verification Commands

Run these after every semantic tranche:

```bash
cargo test -p nexus-cypher --lib
cargo test -p nexus-cypher --test tck_runner -- --nocapture
cargo test -p nexus-server --lib
cargo test --workspace
```

Targeted TCK runs:

```bash
TCK_CATEGORY=clauses/with cargo test -p nexus-cypher --test tck_runner -- --nocapture
TCK_CATEGORY=clauses/unwind cargo test -p nexus-cypher --test tck_runner -- --nocapture
TCK_CATEGORY=expressions/list cargo test -p nexus-cypher --test tck_runner -- --nocapture
TCK_CATEGORY=expressions/aggregation cargo test -p nexus-cypher --test tck_runner -- --nocapture
TCK_CATEGORY=clauses/merge cargo test -p nexus-cypher --test tck_runner -- --nocapture
```

Use `TCK_JSON_OUT=/tmp/nexus-tck.json` when a machine-readable report is needed.

## Do Not Claim Production-Ready Until

- Local openCypher TCK is near-green or every gap is explicitly documented.
- WAL rotation, snapshot retention, recovery validation, and compaction are
  implemented and tested.
- Indexes remain correct through mutation, delete, restart, and compaction.
- HTTP and Bolt have TLS, auth, structured errors, rate/backpressure, and
  graceful shutdown.
- Observability includes metrics, slow-query logs, and readiness checks.
- Memory budgets and bounded result behavior prevent unbounded OOM paths.
- Distributed claims are limited to implemented behavior.
