# Domyn Nexus Production Readiness

This document tracks the implemented single-node production hardening surface
and the remaining gaps. It is intentionally conservative: a feature is marked
ready only when it has code and tests in this workspace.

The implementation roadmap is tracked in
[`opencypher-production-and-scale-roadmap.md`](opencypher-production-and-scale-roadmap.md).

## Current Production Baseline

- Single-node graph engine with typed columnar properties and CSR adjacency.
- WAL recovery with typed values, CRC32 validation, explicit ID replay, and
  atomic snapshot writes.
- SWMR transactions with read guards held for the read transaction lifetime.
- Write transactions stage mutations on a cloned graph and swap only after all
  buffered operations validate.
- HTTP supports configurable query timeout, body size, optional bearer/API-key
  auth, readiness, health, and Prometheus-style metrics.
- Bolt supports connection limits, connection timeout, and optional credential
  verification, but it is not yet fully Neo4j-driver compatible.
- Python SDK supports indexed Cypher execution and parameter binding.
- GraphRAG benchmark-readiness queries are green in the local
  `graphrag_readiness` suite: 14/14 query patterns pass on the miniature
  FinReflect-shaped graph.
- The real-data benchmark binary now loads the same 10-K triplet dataset used
  by the JanusGraph/Neo4j comparison: 47,542 vertices, 64,891 edges, 1,133
  predicates, and 18 tickers. The latest release run completed B3-B10 with
  indexed Cypher and B1-B2 ingestion. p50 results: B1 1.12ms, B2 83.38ms,
  B3 169.5us, B5 111.1us, B6 51.2us, B7 97.5us, B8 500.7us, B9 2.30ms,
  B10 210.3us.
- openCypher TCK runner expands local Scenario Outlines, supports parameter
  tables, `Background` fixture setup, and expected-error scenarios, and
  currently measures 3,870 scenarios: 82.1% parse-ok, 64.7% exec-ok, and 42.8%
  result-match on 3,830 considered scenarios.
- The boolean-expression TCK category is now green in the local runner
  (150/150 result-match, including expected-error scenarios). The aggregation
  TCK category is also green (35/35 result-match, including expected-error
  scenarios). Temporal constructors/accessors have a first compatibility layer,
  but full temporal types, `duration.between`, current-time functions, and
  timezone semantics are still incomplete.
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
| Dynamic property creation | Supported for scalar vertex/edge properties; type enforcement applies after first write |
| Aggregates | Supported for local TCK aggregation scenarios; broader grouping/error rules still need hardening |
| `ORDER BY`, `SKIP`, `LIMIT`, `DISTINCT` | Partial |
| Relationship variables/properties | Partial; relationship variables, simple relationship properties, and edge-unique multi-hop matching work in simple patterns |
| `CREATE`, `MERGE`, `SET`, `DELETE`, `DETACH DELETE` | Partial; native path is WAL-backed through `NexusEngine::execute_write()` |
| `OPTIONAL MATCH`, `WITH`, `UNWIND`, `UNION`, `CALL` | Partial; `CALL` not supported |
| openCypher TCK compliance | Integrated as a measurement runner; not near-green |

## Remaining Production Gaps

- WAL segment rotation, snapshot retention, hot backup/restore, and broader
  recovery validation tooling. Truncated-tail and malformed-record WAL tests
  exist, but segment lifecycle is not implemented.
- Tombstone compaction, index cleanup hardening, and CSR compaction lifecycle.
- Incremental index maintenance for every mutation path; current server and
  Python paths rebuild indexes after writes where needed.
- Full-text update/delete semantics and vector HNSW/ANN indexing.
- TLS for HTTP and Bolt.
- Full Bolt PackStream compatibility, multi-chunk messages, transaction
  messages, routing, and parameter maps.
- Full openCypher parser/TCK frontend adoption.
- Binder/scope validation is started, but full Cypher scoping, temporal
  semantics, and broad error conformance are incomplete.
- Memory budgets, bounded result materialization, cancellation, and backpressure.
- Distributed Raft/sharding; `nexus-distributed` has metadata/log scaffolding
  only, not consensus or sharding.
