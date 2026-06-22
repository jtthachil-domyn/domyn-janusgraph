# Domyn Nexus Known Limits

This file is part of the product contract. If a feature is not supported, Nexus
should reject it clearly instead of silently returning wrong data.

This file supports the canonical
[Nexus production-grade master plan](nexus-production-grade-master-plan.md).
Every tranche that moves a feature boundary must update this file in the same
change. No workaround may be treated as product behavior.

## Product Stage

Current target: **Internal Beta, single-node server**.

Not GA. Not distributed. Not an HA database yet.

## Deployment Limits

- Single process, single node.
- No Raft, no leader election, no automatic failover.
- No cross-shard Cypher.
- No distributed transactions.
- Backups are local-directory backups; cloud object storage is a deployment
  wrapper, not built into the engine yet.
- No full MVCC storage model yet. Nexus currently has single-writer staged
  commits and read snapshots, not concurrent-writer MVCC with conflict
  detection and pruning.

## Query Limits

- The local Falkor TCK scopes are near-green/green, but external corpus tracking
  remains separate in [TCK count ledger](../tck-count-ledger.md).
- `CALL` and procedures are intentionally minimal.
- Explicit Bolt write transactions are rejected. Auto-commit Bolt writes are
  supported.
- Neo4j driver compatibility is not fully certified yet. Driver smoke exists,
  but full multi-driver compatibility is still a GA/enterprise gate.
- Unsupported Cypher should fail at parse or bind time with a structured error.
- Temporal support is TCK-oriented and still needs production timezone/calendar
  hardening before regulated workloads rely on it.

## Storage Limits

- WAL and snapshot recovery paths are implemented and tested, but production
  soak testing is still required.
- Hot backup exists; repeated failure-injection runs are still a beta gate.
- Compaction exists; long-running compaction under heavy mixed traffic still
  needs soak coverage.
- Single-node durability depends on using the server `execute_write()` path.
  Lower-level in-memory helpers are not WAL-backed convenience APIs.

## Document Collection Limits

- Document Collections v0 stores durable JSON documents and exposes HTTP CRUD,
  exact scalar secondary indexes, and Cypher `document(...)`, `documents(...)`,
  and `documentsBy(...)` access.
- Document upserts/deletes are WAL-recoverable as standalone operations and
  can participate in the same commit record as graph writes through
  `NexusEngine::execute_cross_model_write()` and the public `/tx/batch`
  endpoint.
- `/tx/batch` supports ordered native Cypher write statements plus document
  upserts/deletes and vector upserts/removes in one cross-model WAL commit.
  Explicit Bolt multi-statement write transactions are still rejected.
- There is no AQL dialect. The intended query path is Cypher now and
  GQL-aligned semantics over time.
- Document secondary indexes support exact, string prefix, string/numeric range,
  deterministic full-text token search, phrase filtering, bounded fuzzy token
  matching, simple suffix stemming, snippets, score explanations, and selectable
  BM25/TF-IDF/match-count ranking. Full-text search is still lightweight: no
  language-aware stemming, phrase positional index, highlight offsets, custom
  analyzers, or persisted BM25 statistics yet. No array/object scalar or
  composite document indexes yet.
- Bounded Cypher scans exist through `documents(collection, limit)` and
  `MATCH DOCUMENT doc IN collection`. Indexed exact scans exist through
  `documentsBy(collection, path, value, limit)`. `MATCH DOCUMENT` pushes simple
  exact, prefix, and scalar range predicates on `doc.document.*` into scalar
  document indexes when possible, with bounded scan fallback when no matching
  index exists. Explicit `documentFullText(doc.document.path, query)`
  predicates push into full-text document indexes when possible. `CONTAINS`
  remains substring semantics and is not rewritten to full-text search.
  Boolean-composed and nested disjunctive predicate pushdown are not implemented
  yet.
- No automatic document-to-graph materialization yet.
- No schema validation for document shapes yet.

## Index And Vector Limits

- Unique/composite/full-text/vector indexes exist and are maintained through
  supported server mutations.
- HNSW vector search is available, with exact search retained as the recall
  oracle.
- Vector recall metrics must be reported on every benchmark run.

## Security Limits

- Production mode requires auth, TLS, audit log, backup root, storage, and query
  limits.
- RBAC is role-based and tenant-scoped, but not yet a full policy language.
- No SOC 2, ISO 27001, FedRAMP, or formal compliance package.

## Internal Beta Exit Criteria

Do not claim Internal Beta until:

- `cargo xtask verify-internal-beta` passes.
- Docker image builds and passes smoke.
- Production config validator rejects insecure startup.
- Restore from backup into an empty data directory is verified.
- B1-B10 benchmarks are green, including B9 under the current threshold.
- Known unsupported features have explicit tests that assert rejection.
