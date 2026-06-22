# Domyn Nexus Full Production Roadmap

This is the canonical roadmap for turning Domyn Nexus from an alpha-quality
single-node graph engine into a production database product. It coordinates
three tracks that must move together: openCypher conformance, production engine
hardening, and distributed-readiness scaffolding.

TCK denominators and counter definitions live in
[`tck-count-ledger.md`](tck-count-ledger.md). Update that ledger before changing
any TCK count in this roadmap.

The query-language architecture decision lives in
[`query-language-architecture.md`](query-language-architecture.md). It defines
the Cypher-first, GQL-ready frontend strategy and the graph-algebra IR boundary
that this roadmap should preserve.

NornicDB is tracked as an additional product/transaction/GraphRAG reference in
[`reference-nornicdb-gap-analysis.md`](reference-nornicdb-gap-analysis.md).
Use it for transaction contracts, Bolt-driver gates, hybrid graph+vector
benchmarks, and packaging expectations; do not treat it as a wholesale storage
engine donor.

## Verified Baseline

- Workspace test baseline before this roadmap: 212 passing tests.
- Full openCypher/OCG-style target corpus: 3,897 expanded scenarios. The latest
  reported full-corpus run considers 3,830 scenarios after 67 skips/rejections
  and reports 82.1% parse-ok, 64.7% exec-ok, and 42.7% result-match.
- Checked-in local Falkor runner scope: 1,615 raw Gherkin scenarios before
  `Scenario Outline` expansion.
- Previous TCK runner baseline: 1,124 considered scenarios, 43.6% parse-ok,
  36.9% exec-ok, 14.7% primitive result-match.
- Current local OverGraph Rust checkout: about 1,090 listed Rust test
  annotations. Treat OverGraph as a
  storage/operations hardening reference, not as an openCypher conformance
  source.
- Count hygiene: 3,897/3,830 is the full-corpus denominator; 2,587/2,125 is the
  current checked-in local Falkor runner denominator. The local runner starts
  from 1,615 raw Gherkin scenarios, expands 276 `Scenario Outline` definitions,
  and currently measures 2,587 executable local cases. OverGraph is separate:
  about 1,090 local Rust test annotations for engine hardening, not Cypher TCK.
- `nexus-distributed` is not a distributed database yet. It starts with stable
  placement and replication-log types, then later grows into Raft and sharding.

## Current Execution Status

- Measurement completeness has started: the TCK runner expands Scenario
  Outlines, reads parameter tables, handles expected-error scenarios, honors
  upstream skip/ignore tags, loads the current binary-tree fixtures, and emits
  JSON summaries.
- The current full-corpus measurement is 3,897 total expanded scenarios, 3,830
  considered, 82.1% parse-ok, 64.7% exec-ok, and 42.7% result-match. The
  checked-in local Falkor subset measurement is 2,587 total expanded scenarios,
  2,125 considered, 82.7% parse-ok, 82.7% exec-ok, and 100.0% result-match,
  with zero parse errors, zero execution errors, and zero result mismatches in
  the considered local scope. The parse/exec percentages count expected
  compile-time errors
  separately, so they can drop when more expected-error scenarios become
  correctly rejected.
- This clears Gate A, Gate B, Gate C, and Gate D on the current local Falkor
  measurement. Gate E is cleared for the checked-in local Falkor runner. The
  next conformance step is broadening the runner beyond the checked-in Falkor
  feature root and reporting the full external corpus with exact counters.
- Binder/scope work has started in `nexus-cypher` with variable-kind tracking,
  early undefined-variable checks, and strict boolean-literal validation. Full
  Cypher grouping and broad type rules are not complete.
- Expressions have advanced: boolean expected-error conformance is green in the
  local runner, quantifier/list-predicate conformance is green in the local
  runner, conditional expressions, string expressions, math/precedence,
  path expressions, map access, null semantics, literals, comparison, and
  type conversion are green in the local runner, and temporal constructors /
  accessors plus skipped-inclusive temporal arithmetic have a compatibility
  layer. In skipped-inclusive mode, temporal now parses and executes every
  scenario, has zero ordinary failures, and is `1004 / 1004 = 100.0%`
  result-ok. Production still needs real typed temporal values rather than
  string-normalized compatibility values.
- Aggregation is green in the local runner: grouped projection, `DISTINCT`
  inside aggregate calls, `collect`, `count`, `sum`, `avg`, `min`, `max`,
  `percentileDisc`, and `percentileCont` pass the local aggregation category.
  Broader binder enforcement for aggregate/non-aggregate grouping rules still
  needs hardening.
- Pattern matching has moved forward: TCK `Background` setup is now applied by
  the runner, relationship property predicates are preserved through
  AST/planning/execution, null-bound variables no longer expand into full
  scans, and multi-relationship initial patterns route through the edge-unique
  pattern matcher. The skipped-inclusive `clauses/match` bucket has zero
  ordinary failures after supporting relationship-list variable-length syntax
  such as `[rs*]`.
- Pattern expressions are green in the local runner: existential pattern
  predicates like `WHERE (n)-->()`, pattern comprehensions like
  `[p = (n)-->() | p]`, path-variable materialization, and path/path-list
  literal result matching now pass the local `expressions/pattern` category
  (26/26 result-match).
- Path expressions are green in the local runner: `nodes(p)`,
  `relationships(p)`, `length(p)`, `nodes(null)`, and `relationships(null)`
  pass the local considered path-expression scenarios.
- Graph expressions are green in skipped-inclusive mode: labels, keys,
  properties, node/relationship/path literal matching, and relationship
  type-test predicates such as `r:T` are covered.
- `clauses/match` is green in the local considered scope after variable-kind
  conflict checks, `RETURN *`, anonymous relationship creation for fixture
  setup, `<-->` parser handling after bound nodes, variable-length path node
  materialization, and relationship-property filters on variable-length
  expansion. Current status: 113 parse/exec-ok positive cases plus 82
  expected-error passes, with no remaining local mismatches or unexpected
  execution errors.
- List semantics now include null-aware equality/`IN` behavior and explicit
  null slice-bound handling, real list-comprehension execution, nested
  aggregate lifting inside expressions, path-variable materialization for
  `nodes(p)` / `relationships(p)` / `length(p)`, graph-aware node-list result
  matching in the TCK runner, list/map property storage through `PropertyType::Any`,
  and static expected-error checks for invalid `IN`, indexing, and `range`
  usage. The local `expressions/list` category is now green: 175/175
  considered scenarios, including 49 expected-error scenarios.
- `WITH ... WHERE` is green in skipped-inclusive mode after planning a mixed
  predicate scope where both projected aliases and pre-WITH input variables
  are visible to the filter, then projecting back down to the declared WITH
  columns.
- Quantifier semantics are green in the local runner: `any`, `all`, `none`,
  and `single` evaluate scoped predicate variables over scalar, map, node, and
  relationship lists, and the runner now recognizes the related expected-error
  scenarios.
- MERGE has moved from parser-only support toward executable semantics:
  bare-node MERGE, anonymous internal MERGE variables, already-bound
  relationship endpoints, relationship property matching, aggregate
  write-return projections, path bindings, relationship branching over parallel
  and undirected existing matches, `startNode(r)`, and `endNode(r)` are
  implemented. `ON CREATE SET` and `ON MATCH SET` are wired through both the
  in-memory and WAL-backed server paths for SET-style actions, including
  graph-property copying such as `SET r = a`. MERGE path bindings now work in
  the WAL-backed server path and have recovery coverage. The local and
  skipped-inclusive `clauses/merge` buckets now have zero result mismatches.
  The WAL-backed server path now also maintains a staged graph for interleaved
  read/write clause streams, so later `WITH`, `UNWIND`, `MATCH`, and final
  `RETURN` clauses can read uncommitted writes before the durable commit.
- Interleaved read/write clauses are green in the local runner: the in-memory
  TCK path now executes write streams clause-by-clause, so `CREATE ... WITH ...
  UNWIND ... CREATE`, aggregation after writes, repeated aliasing through
  `WITH`, and anonymous write/`WITH *` edge cases behave as a row pipeline.
  Skipped-inclusive mode is now `3,830 / 3,830 = 100.0%` result-ok with zero
  ordinary failures. The durable server path now has the same staged read/write
  pipeline for the covered server features, with WAL recovery tests. Control-query
  support and side-effect table scoring are both enabled in the runner.
- DELETE has moved forward in the local runner: null deletes are no-ops,
  repeated-row deletes skip already-deleted graph elements, and expression
  targets such as `nodeMap.key[0]` can delete evaluated graph values in the
  in-memory path.
  Multiple DELETE targets are now collected before application, so grouped
  path/list deletes remove all targeted relationships before targeted nodes.
  The durable server path now supports ordinary variable targets and expression
  targets, including scalar path/list values, with WAL recovery tests.
- OverGraph-style recovery work has started with malformed/truncated WAL record
  handling, live WAL segment rotation, recovery across rotated live segments,
  compaction-time archive retention for redundant segments, and snapshot
  retention for prior `snapshot.json` generations. Store-level hot
  backup/restore now copies snapshot, catalog, active WAL, and rotated live WAL
  segments and has restore tests; admin/cloud backup workflows are still
  pending.
- Server configuration now has an opt-in `production_mode` validator. In that
  mode the config must provide auth, HTTP TLS, audit logging, backup root
  confinement, durable storage, positive query timeout/concurrency/default
  limit, and a positive query-memory budget before it is considered acceptable
  for production startup. When Bolt is enabled, production mode also requires
  Bolt TLS and positive Bolt connection/query/result guardrails.
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
- Cypher and future GQL frontends must lower through a shared semantic binder
  into graph-algebra logical operators. The internal IR should be
  execution-oriented, not a Cypher AST clone or a direct GQL encoding.
- KyuGraph/Kuzu should be used for parser, binder, and vectorized execution
  ideas. Nexus should not become a wholesale fork.
- Falkor/openCypher TCK is the Cypher compatibility source of truth.
- OverGraph is the durability, compaction, vector, pagination, and crash
  recovery reference.
- NornicDB is a product-surface reference for transaction contracts,
  Neo4j/Bolt compatibility gates, hybrid graph+vector retrieval benchmarks, and
  deployability. Its temporal MVCC model is a future design target, not a claim
  about current Nexus SWMR semantics.
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
- Select corpora with `TCK_FEATURE_ROOT` and lock reported denominators with
  `TCK_EXPECT_TOTAL` / `TCK_EXPECT_CONSIDERED`, so Scope A full-corpus numbers
  cannot be confused with Scope B local-runner numbers.

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
  predicate-variable evaluation. **Current status: green in the local TCK
  quantifier category.**
- Implement type-conversion semantics for `toString`, `toInteger`, `toFloat`,
  and `toBoolean`, including expected-error rejection for invalid complex
  values. **Current status: green in the local TCK type-conversion category.**
- Implement openCypher numeric literal forms: decimal, hexadecimal, octal,
  leading-dot floats, exponent notation, signed min integer, and out-of-range
  numeric rejection. **Current status: green in the local TCK literals category.**
- Implement graph functions and graph-value access: `labels`, `type`, `id`,
  `keys`, `properties`, dynamic property access, and invalid graph-function
  target rejection. **Current status: near-green in the local TCK
  graph-expression category.**
- Implement comparison semantics: chained comparisons, `NaN` equality and
  ordering behavior, mixed numeric comparison, permissive fixture property
  typing in the TCK harness, and graph-value handling inside list comparisons.
  **Current status: green in the local TCK comparison category.**
- Implement `CASE` and string functions/predicates. **Current status:
  conditional expressions and string expressions are green in the local TCK
  categories.**
- Implement map access semantics. **Current status: green in the local TCK map
  category, including case-sensitive keyword keys and null dynamic access.**
- Implement null-expression semantics and stable aliases for unnamed null
  predicates. **Current status: green in the local TCK null-expression
  category.**
- Implement math and precedence semantics. **Current status: green in the local
  TCK mathematical and precedence categories, including `sqrt()` and
  precedence-aware default expression aliases.**
- Implement path function null semantics. **Current status: green in the local
  TCK path-expression category.**
- Add a real temporal value model. The current compatibility layer normalizes
  common constructors/accessors, truncation, ordering, DST fallback arithmetic,
  expanded-year ranges, named-zone recomposition, and duration arithmetic to
  strings for TCK progress and reaches `1004 / 1004 = 100.0%` result-ok with
  zero ordinary skipped-inclusive temporal failures, but production Cypher needs
  typed date, time, localdatetime, datetime, and duration values.

### Phase 4: Aggregation and Projection

- Implement grouped aggregation. **Current status: implemented for local TCK
  coverage.**
- Support `DISTINCT` inside aggregate calls. **Current status: implemented.**
- Support `collect`, `count(*)`, `count(expr)`, `sum`, `avg`, `min`, `max`.
  **Current status: implemented; percentile aggregates are also covered.**
- Enforce Cypher grouping rules in binder. **Current status: partial; keep
  extending through failure-driven TCK work.**
- Support `RETURN ... ORDER BY` over projected aliases, aggregate aliases, and
  list values. **Current status: near-green in the local `clauses/return-orderby`
  category.**

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
- Complete `SET` property-map replacement and append semantics, optional-null
  SET no-ops, and write-query `RETURN SKIP/LIMIT`. **Current status:
  near-green in the local `clauses/set` category, with parse and execution
  errors cleared.**
- Complete `REMOVE` property and label semantics. **Current status: local
  `clauses/remove` is parser/execution green, including missing-property
  REMOVE no-ops; side-effect table scoring is implemented in the harness.**
- Complete expression delete targets. **Current status: in-memory/TCK path
  supports evaluated graph values, null no-ops, paths, lists, nested map/list
  access, and grouped multi-target path deletes; the WAL-backed server path now
  supports evaluated expression targets, including scalar path/list values, with
  recovery coverage.**
- Complete durable MERGE path-binding commit records. **Current status:
  in-memory/TCK path supports `MERGE p = (...) RETURN p`; the WAL-backed server
  path supports `MutationOp::BindPath` as a deterministic in-query binding and
  persists the underlying node/relationship operations through WAL.**
- Add `ON CREATE SET` and `ON MATCH SET`. **Current status: parser, binder,
  planner, in-memory executor, and WAL-backed server path support SET-style ON
  actions; the local `clauses/merge` category has zero parse errors, zero
  execution errors, and zero result mismatches in the considered local subset.**
- Complete durable interleaved write/read streams. **Current status: the
  WAL-backed server path mirrors each write into a staged graph and the
  `WriteTx`, allowing `CREATE ... WITH ... UNWIND ... CREATE ... RETURN` and
  read-your-own-writes queries to execute before WAL-backed atomic commit.**
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
- Add WAL segment rotation and retention. **Current status: live WAL files now
  rotate by size, recovery reads active plus rotated live segments, and
  snapshot compaction archives retained redundant segments.**
- Add snapshot retention. **Current status: prior `snapshot.json` generations
  are archived before atomic replacement and pruned by configured retention.**
- Add hot backup and restore. **Current status: store-level backup syncs the
  WAL, copies snapshot/catalog/live WAL files plus rotated live segments, writes
  a manifest, and restore is tested against snapshot-plus-WAL state.**
- Add recovery validation that compares topology, IDs, properties, indexes, and
  query results before and after restart.

### Compaction

- Compact vertex and edge tombstones. **Current status: manual stable-ID
  compaction clears deleted vertex/edge property payloads without remapping IDs.**
- Compact `edge_meta` and adjacency deltas. **Current status: manual compaction
  rebuilds CSR/incident indexes, removes tombstoned edge metadata, and drains
  adjacency deltas.**
- Add manual compaction first, then threshold-driven background compaction.
  **Current status: manual `NexusEngine::compact_storage()` is implemented;
  `compact_storage_if_needed(threshold)` is implemented; HTTP schedules
  threshold-triggered compaction in a background blocking task using
  `ServerConfig::compaction_threshold`, with one active compaction job at a
  time. `POST /admin/compact` is implemented for manual default-tenant or
  tenant-specific compaction and returns before/after pressure plus compaction
  stats.**
- Ensure snapshot, WAL, and compaction cannot produce split-brain state after a
  crash. **Current status: durable compaction writes a fresh snapshot through
  the existing atomic snapshot/WAL checkpoint path.**

### Indexes

- Incrementally maintain unique, composite, full-text, and vector indexes on
  create, set, remove, delete, and compaction.
- Add stale-entry tests for property updates and deletes. **Current status:
  full-text update/remove tests and vector update/remove/compact tests exist in
  `nexus-index`; vector save/load tests prove tombstones do not survive
  restart. `NexusStore` now owns named vector snapshots and includes them in
  metrics plus hot backup/restore. `NexusEngine` owns named vector indexes with
  write-through upsert/remove/search APIs. HTTP vector endpoints expose
  list/create/upsert/search/remove/compact; Cypher exposes
  `vectorSearch(index, query, k)` and `vectorDistance(left, right)`.**
- Add rebuild-from-graph validation for every index type.

### Vector Search

- Replace brute-force vector search with HNSW or a compatible ANN layer.
  **Current status: the embedded vector index now builds a deterministic
  HNSW-style ANN graph for larger indexes, keeps exact nearest-neighbor search
  as an oracle, keeps threshold search exact, and supports update/remove with
  tombstones, explicit in-memory compaction, and atomic active-vector snapshots.**
- Keep exact search as the correctness oracle. **Current status: exact-oracle
  tests, ANN recall-overlap tests, delete filtering tests, compaction tests, and
  restart round-trip tests are in `nexus-index`.**
- Add dense-vector recall, restart, and compaction parity tests. **Current
  status: in-memory recall/compaction tests and vector snapshot restart tests
  exist; `NexusStore` can save/load/backup/restore vector snapshots;
  `NexusEngine` persists vector mutations through the store; HTTP vector APIs
  are covered by server lifecycle tests; Cypher vector functions are covered by
  cypher/server tests; vector search metrics cover both HTTP search and Cypher
  `vectorSearch(...)` calls, exposing global and per-index exact-oracle overlap
  counters for recall tracking. Starter Prometheus alert rules, a Grafana
  dashboard, and a monitoring runbook live under `docs/ops/`; production
  threshold tuning remains pending.**

### Pagination and Bounds

- Add cursor-based pagination for node scans, edge scans, and traversal results.
- Add bounded result limits, memory ceilings, and query cancellation.
  **Current status: HTTP `/cypher` passes `default_query_limit` into the Cypher
  executor as a row budget, so high-cardinality scans, expands, unwinds,
  projections, aggregations, and staged write streams can abort before
  materializing an oversized `QueryResult`. HTTP also passes
  `query_memory_budget_bytes` into the executor as an approximate byte budget,
  so large rows, strings, lists, maps, `UNION`, `DISTINCT`, and aggregate
  result shaping can abort during execution; the HTTP layer keeps the same
  estimate as a final pre-JSON guard.
  `process_memory_budget_bytes` now rejects new `/cypher` admissions before
  worker acquisition when host RSS is observable and already over budget.
  `max_concurrent_queries` bounds the number of blocking query jobs admitted at
  once. A fixed-window global request rate limit is available through
  `max_query_rate_per_sec` (`0` disables it); over-budget requests return `429`
  before acquiring a query worker. A per-tenant fixed-window request limit is
  available through `max_query_rate_per_tenant_per_sec`. HTTP read-query
  timeouts now set a cooperative cancellation token checked by recursive
  Cypher operators and high-cardinality scan/expand/projection loops.
  WAL-backed server writes honor that token before the durable point of no
  return: source-row collection and staged mutation application can abort
  without graph/WAL changes; after WAL append starts, commit completion is
  intentional. True streamed HTTP responses and spill behavior remain pending.**
- Test stable order, deleted records, cursor round-trips, and bounded traversal.

### Operations

- Add config for WAL mode, snapshot interval, compaction thresholds, memory
  budget, query timeout, index rebuild mode, and backup destination
  confinement.
- Add metrics for WAL bytes, recovery time, compaction time, active queries,
  memory estimate, index sizes, and query latency buckets.
  **Current status: `/metrics` exposes vertex/edge/tenant counts plus
  compaction pressure gauges for deleted vertex payloads, tombstoned edges, and
  adjacency delta edges, plus background compaction active/scheduled/completed/
  failed/skipped counters. Query lifecycle counters are exposed for active,
  total, succeeded, failed, timed-out, and result-limited `/cypher` requests,
  along with over-capacity rejections, global rate-limit rejections, returned
  row/byte totals, and cumulative elapsed milliseconds. Slow-query totals and
  latency buckets are exposed. Durable engines expose WAL sequence,
  active/live/archive WAL bytes, recoverable WAL bytes, snapshot/archive bytes,
  vector snapshot count/bytes, and catalog bytes. Recovery-time metrics, memory
  estimates, and richer per-index metrics remain pending.**
- Add audit logging for security-sensitive actions.
  **Current status: HTTP can append JSONL audit events for Cypher writes and
  admin backup/compaction when `audit_log_path` is configured, and exposes
  audit success/failure counters.**
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

Full-corpus runs should always pin the expected denominator:

```bash
TCK_FEATURE_ROOT=/absolute/path/to/features \
TCK_SCOPE=full-opencypher \
TCK_EXPECT_TOTAL=3897 \
TCK_EXPECT_CONSIDERED=3830 \
cargo test -p nexus-cypher --test tck_runner -- --nocapture
```

## Do Not Claim Production-Ready Until

- Local openCypher TCK is near-green or every gap is explicitly documented.
- WAL rotation, snapshot retention, recovery validation, and compaction are
  implemented and tested. **Current status: WAL rotation and snapshot retention
  are implemented; manual stable-ID tombstone/CSR compaction is implemented;
  background scheduling and broader crash-injection coverage remain pending.**
- Indexes remain correct through mutation, delete, restart, and compaction.
- HTTP and Bolt have TLS, auth, structured errors, rate/backpressure, and
  graceful shutdown. **Current status: HTTP errors now include stable machine
  codes for auth, tenant lookup, query limits/rate limits/capacity, query
  timeout/panic, result bounds, and compaction failures. JSON config-file
  loading maps runtime knobs into `ServerConfig`; HTTP supports a legacy
  all-tenant admin token plus named `auth_principals` with
  read-only/read-write/admin roles and tenant scopes; env overrides cover bind,
  legacy auth token, TLS paths, backup root, and audit log path; and
  rustls-backed TLS listeners are available for HTTP and Bolt.
  Bolt now parses RUN parameter maps, reads multi-chunk messages, sends `fields`
  as a PackStream list, supports `PULL {n}` batching, returns ROUTE metadata,
  applies per-RUN query timeout plus row/byte result budgets, keeps read-only
  explicit transaction state, and follows Bolt failure-state behavior by
  returning `IGNORED` until `RESET`. Explicit write transactions are rejected
  rather than falsely acknowledging rollback. `BoltServer::from_config` wires
  the parsed Bolt config into the runtime server so these limits are not just
  validated on paper. Optional mTLS/client
  certificate enforcement is wired through `TlsConfig`; cert/key/client-CA files
  are hot-reloaded for new handshakes when their metadata changes.
  Official-driver certification and real explicit write transactions remain
  pending.**
- Observability includes metrics, slow-query logs, and readiness checks.
- Memory budgets and bounded result behavior prevent unbounded OOM paths.
- Distributed claims are limited to implemented behavior.
