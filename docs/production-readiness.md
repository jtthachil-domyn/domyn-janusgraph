# Domyn Nexus Production Readiness

This document tracks the implemented single-node production hardening surface
and the remaining gaps. It is intentionally conservative: a feature is marked
ready only when it has code and tests in this workspace.

The implementation roadmap is tracked in
[`opencypher-production-and-scale-roadmap.md`](opencypher-production-and-scale-roadmap.md).
The top-level product roadmap is tracked in
[`product/nexus-production-grade-master-plan.md`](product/nexus-production-grade-master-plan.md).
The Internal Beta product packaging plan is tracked in
[`product/foolproof-engine-product-packaging-plan.md`](product/foolproof-engine-product-packaging-plan.md),
with server docs under [`product/`](product/).
The query-language architecture decision is tracked in
[`query-language-architecture.md`](query-language-architecture.md).
Cypher conformance counters are tracked in
[`tck-count-ledger.md`](tck-count-ledger.md); use that file as the source of
truth for TCK denominators.
NornicDB reference gaps are tracked in
[`reference-nornicdb-gap-analysis.md`](reference-nornicdb-gap-analysis.md).

Mandatory tranche discipline: every tranche must start by reading the master
plan, the Internal Beta ledger, known limits, and this production-readiness
file. Every tranche must end by updating checklist status, test evidence,
benchmark evidence when relevant, known limits, and this status document. No
workaround may be treated as product behavior.

## Current Production Baseline

- Single-node graph engine with typed columnar properties and CSR adjacency.
- WAL recovery with typed values, CRC32 validation, explicit ID replay, and
  atomic snapshot writes.
- SWMR transactions with read guards held for the read transaction lifetime.
- Write transactions stage mutations on a cloned graph and swap only after all
  buffered operations validate.
- HTTP supports JSON config-file loading, env overrides for bind/auth/TLS,
  rustls-backed HTTPS, configurable query timeout, body size, concurrent query
  limits, global and per-tenant query rate limits, default result-row limits,
  approximate result-memory budgets, optional bearer/API-key auth with named
  read-only/read-write/admin principals and tenant scopes, readiness, health,
  manual compaction, structured error codes, slow-query detection, latency
  buckets, Prometheus-style metrics, optional JSONL audit logging for
  write/admin actions, a `backup_root` confinement option for admin hot
  backups, and a `production_mode` config validator that rejects startup
  configurations without auth, HTTP TLS, Bolt TLS when Bolt is enabled, audit
  logging, backup confinement, durable storage, and positive HTTP/Bolt query
  guardrails.
- Bolt supports connection limits, connection timeout, per-RUN query timeout,
  row and byte result budgets, optional credential verification, TLS/mTLS
  wrapping, RUN parameter-map extraction, multi-chunk message reads, PackStream
  list metadata for `fields`, `PULL {n}` batching, ROUTE metadata, read-only
  explicit transaction state, and Bolt failure-state handling (`FAILURE`
  followed by `IGNORED` until `RESET`). Explicit write transactions are rejected
  rather than falsely acknowledging rollback; write statements remain durable in
  auto-commit mode through `NexusEngine::execute_write`. `BoltServer::from_config`
  applies the parsed Bolt config to the runtime server, including TLS, auth,
  connection caps, timeout, row limit, and byte budget. It is closer to
  Neo4j-driver compatibility but not yet fully driver-certified.
- Python SDK supports indexed Cypher execution and parameter binding.
- GraphRAG benchmark-readiness queries are green in the local
  `graphrag_readiness` suite: 14/14 query patterns pass on the miniature
  FinReflect-shaped graph.
- Server product shell now has explicit CLI operations: `nexus-server
  check-config`, `nexus-server init`, `nexus-server backup`, and
  `nexus-server restore`, plus `cargo xtask verify-internal-beta` as the
  canonical local gate. The Docker image defaults to
  `/etc/domyn-nexus/config.json` and healthchecks `/health`.
- The real-data benchmark binary now loads the same 10-K triplet dataset used
  by the JanusGraph/Neo4j comparison: 47,542 vertices, 64,891 edges, 1,133
  predicates, and 18 tickers. The latest release run completed B3-B10 with
  indexed Cypher and B1-B2 ingestion. Latest p50 results from the June 2
  release run: B1 1.10ms, B2 70.19ms, B3 285.5us, B5 188.7us, B6 88.5us,
  B7 160.5us, B8 603.2us, B9 1.18ms, B10 338.4us. The formerly weak B9
  top-connected query now uses allocation-free incident-degree counting in the
  graph core and the Cypher one-hop `count(r)` aggregate fast path.
- openCypher TCK measurement has two scopes that must not be mixed. The larger
  full-corpus target is 3,897 expanded scenarios, with 3,830 currently
  considered after 67 skips/rejections. The latest reported full-corpus rates
  are 82.1% parse-ok, 64.7% exec-ok, and 42.7% result-match. The checked-in
  Rust runner currently points at the local Falkor feature root only; that
  subset expands 1,615 raw Gherkin scenarios into 2,587 executable local cases,
  with 2,125 considered after 462 skips/rejections, and reports 82.7% parse-ok,
  82.7% exec-ok, and 100.0% result-match. It currently has zero parse errors,
  zero execution errors, and zero result mismatches in the considered local
  scope. The lower parse/exec percentages are expected because compile-time
  expected-error scenarios are counted separately as `expected_error_ok`.
  All considered local cases are now comparable and result-ok; the remaining
  local scope boundary is the 462 skipped/unsupported categories. Treat
  3,897/3,830 as the production
  conformance denominator; treat 2,587/2,125 as the current local tranche
  dashboard until the runner is pointed at the full corpus. The runner can now
  select that external feature root with `TCK_FEATURE_ROOT` and fail fast on
  denominator drift with `TCK_EXPECT_TOTAL` / `TCK_EXPECT_CONSIDERED`.
- Count hygiene: OverGraph has about 1,090 local Rust test annotations and is an
  engine hardening reference, not the Cypher TCK source. The historical OCG
  line in `domyn-nexus-build-plan.md` also uses the 3,897 total; that number is
  the full openCypher/OCG-style corpus size, not an OverGraph test count.
- Latest skipped-inclusive Falkor tranche mode is `3,830 / 3,830 = 100.0%`
  result-ok, with `3,149 / 3,830 = 82.2%` parse-ok and
  `3,149 / 3,830 = 82.2%` exec-ok. The lower parse/exec counts are expected:
  more negative scenarios are now rejected at bind time and counted under
  `expected_error_ok` (`681`). Skipped-inclusive ordinary failures are now
  cleared: zero unexpected parse errors, zero execution errors, and zero result
  mismatches. Side-effect table scoring is now enabled for write scenarios,
  including both canonical and upstream-skipped delete accounting. Control-query
  support is enabled, so post-write validation queries are scored. The temporal
  category now parses, executes, and result-matches every skipped-inclusive
  scenario: `1004 / 1004 = 100.0%`.
- The boolean-expression TCK category is now green in the local runner
  (150/150 result-match, including expected-error scenarios). The aggregation
  TCK category is also green (35/35 result-match, including expected-error
  scenarios). The pattern-expression TCK category is green (26/26
  result-match), including existential pattern predicates and pattern
  comprehensions. The quantifier-expression TCK category is green (596/596
  considered scenarios, including 12 expected-error scenarios). The list
  expression category is green (175/175 considered scenarios, including 49
  expected-error scenarios). The type-conversion category is green (43/43
  considered scenarios, including 22 expected-error scenarios). The literal
  expression category is green (95/95 considered scenarios, including 8
  expected-error scenarios). The conditional-expression category is green
  (13/13 considered scenarios), covering simple and searched `CASE`. The string
  expression category is green (26/26 considered scenarios), covering
  `CONTAINS`, `STARTS WITH`, `ENDS WITH`, `substring`, `split`, and null-aware
  string predicates. The map-expression category is green (34/34 considered
  scenarios), including case-sensitive keyword keys and dynamic access on null.
  The null-expression category is green (42/42 considered scenarios), including
  property null checks with multiple unnamed return expressions. The math and
  precedence categories are green (5/5 and 96/96 considered scenarios),
  including `sqrt()` and precedence-aware default expression aliases. The path
  expression category is green (3/3 considered scenarios), including
  `nodes(null)` and `relationships(null)`. The
  graph-expression category is green (58/58
  considered scenarios, including 16 expected-error scenarios), covering
  `labels`, `type`, `id`, `keys`, `properties`, dynamic property access, and
  invalid graph-function targets. The comparison-expression category is green
  (71/71 considered scenarios, including 1 expected-error scenario), covering
  chained comparisons, mixed numeric comparison, `NaN` comparison behavior, and
  graph values inside comparison lists. `UNWIND`, `REMOVE`, `RETURN
  SKIP/LIMIT`, and `WITH SKIP/LIMIT` are green in the local runner. The
  `clauses/set` category is green in the local runner
  (41/41 considered result-ok, including 2 expected-error scenarios), with
  parser and execution errors cleared for property-map replacement/append,
  optional-null SET no-ops, invalid map-list property assignment rejection, and
  write RETURN `SKIP`/`LIMIT`. The `clauses/delete` category is execution-green in the local
  runner (19/19 result-ok for considered cases), including
  null deletes, grouped path/list/map delete targets, nested map/list path
  deletes, repeated-row deletes, and side-effect table checks. CREATE and MERGE validate illegal bound-variable reuse,
  missing/multiple relationship types, undefined property-map variables, invalid
  map/list-of-map graph property values, null MERGE property maps, and new
  predicates on already-bound variables. MERGE is currently zero-mismatch in
  both Scope B and skipped-inclusive mode. CREATE, SET, REMOVE, WITH, and
  RETURN have zero ordinary failures in the local and skipped-inclusive
  runners after the clause-stream write executor tranche. Durable server writes
  now support MERGE path bindings, expression DELETE targets, and interleaved
  read/write clause streams through `NexusEngine::execute_write()` with
  WAL-before-apply recovery coverage. The server path maintains a staged graph
  during the transaction, so later `WITH`, `UNWIND`, `MATCH`, and final
  `RETURN` clauses can read uncommitted writes before the WAL-backed commit. The
  `clauses/match` category is green in the local considered scope: 113
  parse/exec-ok positive cases plus 82 expected-error passes, with no remaining
  mismatches or unexpected execution errors.
  Temporal constructors/accessors and skipped-inclusive temporal arithmetic are
  green in the current runner; production still needs real typed temporal
  values instead of string-normalized compatibility values.
- `RETURN ... ORDER BY` and `WITH ... ORDER BY` have no remaining local
  mismatches or unexpected execution errors, including sorting by projected
  aliases, aggregate aliases, aggregate order expressions, mixed Cypher value
  types, and lexicographic list values.
- `nexus-distributed` contains metadata and replication-log scaffolding only:
  placement, IDs, read consistency markers, and deterministic commit records.

## Cypher Compatibility Matrix

| Area | Status |
|---|---|
| `MATCH` node scan | Supported |
| Single-hop relationship expand | Supported |
| Variable-length paths | Partial; semantics are not full openCypher |
| Label filters | Supported for source and destination nodes |
| Property equality/range filters | Partial; node predicates, source predicate pushdown through relationship expansion, and simple relationship property predicates are supported |
| Query parameters | Supported in executor, HTTP, and Python |
| `CONTAINS`, `STARTS WITH`, `ENDS WITH` | Supported for string predicates |
| List equality, `IN`, indexing, slicing | Partial; null-aware list equality/`IN` and explicit-null slice bounds are supported |
| `RETURN` variables/properties | Supported for vertex variables/properties |
| Dynamic property creation | Supported for vertex/edge properties; auto-created keys use flexible `Any` storage while explicitly registered schema keys remain type-enforced |
| Aggregates | Supported for local TCK aggregation scenarios; broader grouping/error rules still need hardening |
| `ORDER BY`, `SKIP`, `LIMIT`, `DISTINCT` | Partial |
| Relationship variables/properties | Partial; relationship variables, simple relationship properties, and edge-unique multi-hop matching work in simple patterns |
| `CREATE`, `MERGE`, `SET`, `DELETE`, `DETACH DELETE` | Partial; native path is WAL-backed through `NexusEngine::execute_write()` |
| `OPTIONAL MATCH`, `WITH`, `UNWIND`, `UNION`, `CALL` | Partial; `CALL` not supported |
| openCypher TCK compliance | Scope B local runner is `2,125 / 2,125 = 100.0%` with zero ordinary failures; skipped-inclusive mode is `3,830 / 3,830 = 100.0%`, also with zero ordinary failures. Full external corpus measurement is still tracked separately. |

## Remaining Production Gaps

- WAL segment rotation is implemented for live WAL files, recovery reads across
  rotated live segments, snapshot compaction can archive retained redundant WAL
  segments, and snapshot retention keeps prior `snapshot.json` generations.
  Store-level hot backup/restore is implemented and tested for
  snapshot-plus-rotated-WAL recovery. HTTP now exposes a bearer/API-key
  protected `POST /admin/backup` endpoint for durable engines, returning the
  backup manifest and tracking backup started/completed/failed counters. In
  production, set `http.backup_root` or `DOMYN_NEXUS_BACKUP_ROOT`; when set,
  backup requests must use relative child paths under that root, and absolute
  paths or `..` escapes are rejected before any file IO begins.
  Restore remains intentionally offline-only to avoid replacing a live engine
  under traffic. Cloud archival and broader recovery validation tooling are
  still pending.
- HTTP can append JSONL audit events for WAL-backed Cypher writes and admin
  backup/compaction actions when `http.audit_log_path` or
  `DOMYN_NEXUS_AUDIT_LOG_PATH` is configured. Events include timestamp,
  principal name, action, tenant, outcome, and a short detail string. The
  legacy top-level `auth_token` remains accepted as an all-tenant admin token
  for compatibility; configured `auth_principals` can be restricted by role and
  tenant. Audit write failures are counted and warned through tracing; a
  stricter fail-closed audit policy remains pending.
- A config-driven `nexus-server` binary is available for single-node startup.
  It loads `--config` or `DOMYN_NEXUS_CONFIG`, applies env overrides, validates
  `production_mode`, recovers durable graph state from `storage_path`, starts
  HTTP/HTTPS, and starts Bolt when enabled. The initial smoke script
  `scripts/smoke-single-node.sh` checks health, readiness, metrics, one write,
  one read, and an admin backup against a running server. A root Dockerfile is
  present for single-binary container packaging, with `.dockerignore` excluding
  `target/` and reference checkouts from build context. `docs/examples`
  includes both production and local-dev single-node configs; the dev config is
  paired with `scripts/run-dev-single-node.sh` and the HTTP smoke script.
- Tombstone compaction is implemented as a stable-ID maintenance operation:
  deleted vertex/edge property payloads are cleared, tombstoned edge metadata
  and adjacency deltas are rebuilt out of CSR, indexes are rebuilt, and durable
  engines snapshot the compacted graph. HTTP writes now use the configured
  compaction threshold to schedule one background compaction job at a time, and
  `/metrics` exposes compaction pressure and job counters. Query lifecycle
  counters are also exposed for active, total, succeeded, failed, timed-out, and
  result-limited `/cypher` requests, plus over-capacity rejections, global
  rate-limit rejections, returned row/byte totals, elapsed milliseconds,
  slow-query totals, latency buckets, configured query guardrails, and process
  RSS with an optional `process_memory_budget_bytes` alert threshold, plus
  graph-core, schema-index, vector-index, and total internal owned-memory
  estimates for trend visibility. Durable engines also expose WAL sequence,
  active/live/archive WAL bytes,
  recoverable WAL bytes, snapshot/archive bytes, snapshot freshness, vector
  snapshot bytes, and catalog bytes. When `process_memory_budget_bytes` is
  configured and RSS is observable on the host, `/cypher` rejects new queries
  above that process-memory ceiling before acquiring a worker, and metrics
  expose `domyn_nexus_queries_memory_rejected_total`. A
  bearer/API-key protected `POST /admin/compact` endpoint can run manual
  default-tenant or tenant-specific compaction and returns before/after
  pressure plus compaction stats. Deeper index cleanup validation remains
  pending.
- Incremental index maintenance for every mutation path; current server and
  Python paths rebuild indexes after writes where needed.
- Full-text and vector indexes now have explicit mutation lifecycles.
  Tantivy-backed full-text indexes can update/remove vertices without stale
  search hits after commit. The embedded vector index has a deterministic
  HNSW-style ANN graph, exact search as a correctness oracle, exact threshold
  search for set semantics, update/remove tombstones, in-memory compaction, and
  Python SDK mutation APIs. `nexus-index` can now atomically save/load active
  vector snapshots and rebuild HNSW on restart without resurrecting tombstones.
  `NexusStore` owns named vector snapshots under `vectors/`, reports vector
  snapshot count/bytes, and carries those files through hot backup/restore.
  `NexusEngine` owns named vector indexes and write-through upsert/remove
  operations persist through `NexusStore`. HTTP vector APIs now expose named
  index list/create/upsert/search/remove/compact operations. Cypher now exposes
  `vectorSearch(index, query, k)` and `vectorDistance(left, right)` for
  read-side vector retrieval and scoring. Vector search metrics now cover both
  HTTP vector search and Cypher `vectorSearch(...)`, exposing global and
  per-index exact-oracle overlap counters for recall tracking. Starter
  Prometheus alert rules, a Grafana dashboard, and a monitoring runbook now
  live under `docs/ops/`; deployment-specific threshold tuning remains pending.
- TLS deployment wiring is implemented for HTTP and Bolt via rustls-backed
  listeners and config-file/env fields. Optional mTLS/client certificate
  enforcement is wired through `TlsConfig`; certificate/key/client-CA changes
  are detected and reloaded for new TLS handshakes while keeping the last known
  good config if a rotation is temporarily invalid.
- Full Bolt driver certification and deeper protocol compatibility.
  Current Bolt compatibility includes RUN parameter maps, multi-chunk reads,
  `fields` as a PackStream list, `PULL {n}` batching, ROUTE metadata, per-RUN
  query timeout, row/byte result budgets, and read-only explicit transactions.
  Bolt failure state now returns `IGNORED` until `RESET`. Explicit write
  transactions are rejected until a real transactional Bolt write state is
  implemented. An optional official Neo4j Python-driver smoke script exists at
  `scripts/neo4j-driver-smoke.py`; it is not yet a mandatory CI gate.
- Full openCypher parser/TCK frontend adoption.
- Binder/scope validation is started, but full Cypher scoping, real typed
  temporal values, and broad error conformance are incomplete.
- The long-term language target is Cypher-first with a future GQL frontend
  lowering into the same graph-algebra IR. The IR must remain execution-oriented
  rather than becoming a Cypher or GQL AST clone.
- Durable server writes now support ordinary variable `DELETE`, expression
  delete targets such as `DELETE [r]`, MERGE path bindings such as
  `MERGE p = (...) RETURN length(p)`, and interleaved write/read streams such as
  `CREATE ... WITH ... UNWIND ... CREATE ... RETURN`, with WAL recovery tests
  for the server paths.
- `MERGE ... ON CREATE SET ...` and `ON MATCH SET ...` are wired through the
  parser, binder, planner, in-memory executor, and WAL-backed server write path
  for SET-style actions. The local `clauses/merge` TCK category has zero parse
  errors, zero execution errors, and zero result mismatches in the considered
  local subset.
- HTTP `/cypher` passes `default_query_limit` into the Cypher executor as a
  row budget, so high-cardinality scans, expands, unwinds, projections,
  aggregations, and staged write streams can abort before materializing an
  oversized `QueryResult`. `/cypher` also passes `query_memory_budget_bytes`
  into the Cypher executor as an approximate byte budget, so large strings,
  lists, maps, wide rows, `UNION`, `DISTINCT`, and aggregate result shaping can
  abort during execution; the HTTP layer keeps the same byte estimate as a
  final pre-JSON guard. `/cypher` accepts at most `max_concurrent_queries`
  blocking query jobs at once. A fixed-window global
  request rate limit is available through `max_query_rate_per_sec`
  (`0` disables it), and a per-tenant fixed-window request limit is available
  through `max_query_rate_per_tenant_per_sec`. Rejected requests return `429`
  before acquiring a query worker and increment
  `domyn_nexus_queries_rate_limited_total`. HTTP read-query timeouts now trip a
  cooperative cancellation token checked by recursive Cypher operators and
  high-cardinality scan/expand/projection loops, so timed-out reads can stop
  inside the executor instead of continuing indefinitely in the blocking pool.
  WAL-backed server writes also honor the same cancellation token while they
  are still in the safe-to-abort phase: parsing, binding, planning, source-row
  collection, and staged mutation application before WAL append. Once WAL
  append begins, the engine intentionally completes the commit path rather than
  reporting a cancelled write that may already be durable. True streamed HTTP
  responses, spill-to-disk, and stricter per-query memory accounting are still
  pending.
- Distributed Raft/sharding; `nexus-distributed` has metadata/log scaffolding
  only, not consensus or sharding.
- Real MVCC is not implemented. Nexus has SWMR transactions, read-transaction
  repeatability, staged write commits, and WAL-before-apply durable server
  writes. It does not yet have version-chain reads, historical/as-of queries,
  concurrent writer conflict detection, or MVCC pruning. NornicDB-style
  temporal MVCC remains a future storage-model tranche.
