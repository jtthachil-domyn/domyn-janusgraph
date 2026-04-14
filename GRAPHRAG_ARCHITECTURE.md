# GraphRAG Production Architecture for JanusGraph

## Core Principle

> To make JanusGraph fast for GraphRAG, move work from query-time to ingestion-time -- or pay for it on every query.

> **In distributed graph systems, node lookup is a distributed operation; traversal is a local one. Optimize accordingly.**

That single sentence is the entire benchmark distilled. Point lookups by internal ID are ~1ms. Entry via `g.V().has(...)` triggers a distributed index query costing 20--100ms. Uncontrolled fan-out multiplies that further. Every design decision in this document follows from this asymmetry.

This system guarantees bounded latency by ensuring all graph operations are O(local neighborhood), never O(graph size), with a target of **< 50ms per retrieval**.

---

## Two-Phase GraphRAG

All GraphRAG retrieval on distributed graphs reduces to two phases. Mixing them is the source of every latency problem.

```
Phase 1: RESOLVE                          Phase 2: TRAVERSE
+-------------------+                     +-------------------+
|                   |                     |                   |
|  User Query       |                     |  Internal IDs     |
|       |           |                     |       |           |
|       v           |                     |       v           |
|  Vector DB        |                     |  g.V(id)          |
|       |           |     external_id     |       |           |
|       v           |  ---- mapping --->  |       v           |
|  Top-K entities   |     (Redis)         |  Bounded          |
|  (external_ids)   |                     |  traversal        |
|                   |                     |       |           |
+-------------------+                     |       v           |
                                          |  Context for LLM  |
                                          |                   |
                                          +-------------------+
```

JanusGraph is strictly a traversal engine. All search and resolution are externalized. The graph never answers "find me entities matching X." It only answers "given this node, show me what's connected."

---

## The Problem

Three sources of latency dominate JanusGraph query cost:

| Source | What Happens | Cost |
|---|---|---|
| **Expensive entry** | `g.V().has('external_id', ...)` | Index -> Elasticsearch -> Cassandra -> deserialize |
| **Read amplification** | Cassandra reads for each property/edge | Grows with vertex degree and property count |
| **Fan-out explosion** | `g.V(id).out().out()` without limits | Adjacency reads multiply at each hop |

None of these are inherent to JanusGraph. They are consequences of using a distributed graph database the same way you'd use a local one.

---

## Hard Constraints

The following must never occur in production. Violating any of these breaks the latency guarantee.

### 1. No attribute-based graph entry

```groovy
// NEVER DO THIS
g.V().has('name', 'Apple Inc.')
g.V().has('external_id', 'AAPL:Revenue:FIN_METRIC')
```

Triggers: composite index lookup -> Elasticsearch query -> Cassandra read -> deserialization. Cost: 20--100ms per lookup. Use the ID mapping layer instead.

### 2. No unbounded traversal

```groovy
// NEVER DO THIS
g.V(id).repeat(out()).emit()
g.V(id).out().out().out()
```

Fan-out is exponential. A vertex with degree 50 produces 125,000 reads at 3 hops. Always scope by predicate, always apply `limit()` and `dedup()`.

### 3. No graph-wide scans

```groovy
// NEVER DO THIS
g.V().has('entity_type', 'FIN_METRIC')
g.V().has('tenant_id', 'AAPL').count()
```

Equivalent to a full Cassandra scan. Cost: seconds. Any query that starts with `g.V().has(...)` without a known vertex ID is a scan in disguise.

### 4. No graph-as-search-engine

All semantic search, entity resolution, and fuzzy matching belongs in the vector DB or search index. The graph receives only resolved internal IDs. If the graph is answering "which entity is this?", the architecture is wrong.

---

## Design Invariants

These properties must hold for every production query. They are not guidelines -- they are system guarantees. If any invariant is violated, the latency contract is void.

1. **Entry is always by internal vertex ID.** No query enters the graph via attribute lookup.
2. **Traversal depth is at most 2.** No query executes 3+ hops.
3. **Traversal is predicate-scoped.** No query uses unfiltered `out()` or `both()`.
4. **Result set is bounded.** Every traversal applies `dedup()` and `limit()`.
5. **No query depends on global graph state.** No aggregation, count, or scan over the full graph at query-time.

These invariants are what make the latency budget possible. Each one eliminates a class of distributed reads that would otherwise dominate query cost.

---

## Graph Shape Requirement

Graph quality determines query performance. A poorly shaped graph cannot be fixed at query-time.

The graph must be shaped for traversal: low fan-out, semantically meaningful predicates, and denormalized shortcuts for common paths. If the graph has high-degree hub nodes with generic predicates, no amount of query-time optimization will fix the latency. The ingestion pipeline is responsible for producing a graph that these invariants can operate on efficiently.

---

## Production Pipeline

```
User Query
   |
   v
Vector DB (semantic retrieval)
   |
   v
Top-K entities (external_id list)
   |
   v
ID Resolver (external_id -> internal_id)   <-- Redis, ~1-2ms
   |
   v
JanusGraph (g.V(id) -- direct, no index)   <-- ~1ms entry
   |
   v
1-2 hop filtered traversal (budget-limited) <-- 10-40ms
   |
   v
Context builder
   |
   v
LLM
```

---

## Optimization Layers

### Layer 1: Persistent ID Mapping (Eliminates the biggest latency hit)

The benchmark proved that `external_id -> internal_id` resolution is the dominant cost. During streamed ingestion, these mappings are captured for free. The next step is making them persistent and fast.

**Implementation:**

```
Redis / RocksDB / in-memory KV store
Key:   external_id (e.g., "AAPL:Revenue:FIN_METRIC")
Value: JanusGraph internal vertex ID (long)
```

**What this replaces:**

```
BEFORE:  g.V().has('external_id', 'AAPL:Revenue:FIN_METRIC')  -- index + ES + Cassandra
AFTER:   g.V(4208L)                                           -- direct O(1) lookup
```

**Latency impact:** ~20-50ms eliminated per entry point.

**Population strategy:**

- During streamed ingestion, every vertex insert returns its internal ID. Write these to the KV store as a side-effect of ingestion.
- For incremental ingestion, capture IDs as vertices are added.

**ID stability:**

Vertex IDs are stable as long as the underlying storage is unchanged. IDs are assigned by the persistent backend (Cassandra, HBase), not by the JVM -- so normal restarts do not invalidate them. If the graph is re-ingested, migrated, or storage is rebuilt, IDs must be regenerated and the mapping layer refreshed. For disaster recovery, persist the full mapping to disk alongside the KV store.

---

### Layer 2: Predicate-Scoped Traversal (Reduces fan-out by 5-10x)

Most GraphRAG queries don't need all edges. A financial question about Apple's revenue doesn't need "Operates_In" or "Filed_With" edges.

**Instead of:**

```groovy
g.V(id).out()
```

**Do:**

```groovy
g.V(id).out('Discloses', 'Has_Revenue')
```

**Why this matters:**

- The graph has 1,133 unique predicates. An unfiltered `out()` reads ALL edge types from Cassandra.
- Scoping to 2-3 relevant predicates reduces Cassandra reads by an order of magnitude.
- Relevance improves because the traversal only follows semantically appropriate paths.

**In the pipeline:**

The LLM (or a lightweight classifier) determines which predicates are relevant to the query before the graph traversal begins. This is cheap (~5ms with a small model) and dramatically reduces downstream work.

---

### Layer 3: Traversal Budgeting (Prevents explosion)

Hard limits on every traversal. Non-negotiable in production.

```groovy
g.V(id)
  .repeat(outE('Discloses').inV())
  .times(2)
  .dedup()
  .limit(50)
  .elementMap()
  .toList()
```

**Budget parameters:**

| Parameter | Recommended Value | Why |
|---|---|---|
| `max_depth` | 2 | 3+ hops rarely add useful context; cost grows exponentially |
| `max_nodes` | 50-100 | LLM context windows are bounded; more nodes = more noise |
| `dedup()` | Always | Prevents cycles from inflating result set |
| `limit()` | Always | Hard cap even after dedup |

Empirically, 1--2 hops capture the majority of useful context for GraphRAG while keeping read amplification bounded. Beyond 2 hops, latency grows faster than information gain.

---

### Layer 4: Precomputed Subgraph Cache (60ms -> 5ms)

Most queries hit the same companies, the same financial metrics, the same relations. Instead of traversing live every time, precompute the common subgraphs.

**What to cache:**

```json
{
  "entity_id": "AAPL:Revenue:FIN_METRIC",
  "internal_id": 4208,
  "1_hop": [
    {"id": 4210, "name": "Apple Inc.", "rel": "Discloses", "direction": "in"},
    {"id": 4215, "name": "App Store Revenue", "rel": "Has_Component", "direction": "out"}
  ],
  "2_hop_filtered": [
    {"id": 4220, "name": "Services Segment", "rel": "Part_Of", "path": "Revenue->App Store Revenue->Services Segment"}
  ]
}
```

**Where to store:**

| Tier | Storage | Use Case |
|---|---|---|
| Hot | Redis | Top 1-5% entities by query frequency |
| Warm | S3 / blob | All entities, refreshed on ingestion |
| Cold | Live traversal | Cache miss fallback |

**Invalidation:** On re-ingestion of a ticker, invalidate all cached subgraphs for that tenant.

**Impact:** Removes Cassandra from the hot path entirely for frequently queried entities.

---

### Layer 5: Hot Subgraph Layer (In-Memory Adjacency)

For the most frequently accessed nodes (top 1-5% by degree or query frequency), keep their full adjacency in application memory.

**Implementation:**

```python
hot_cache: dict[int, list[dict]] = {}

def get_neighbors(internal_id: int, predicates: list[str]) -> list[dict]:
    if internal_id in hot_cache:
        return [n for n in hot_cache[internal_id] if n["rel"] in predicates]
    return jg_traversal(internal_id, predicates)
```

**Size estimate for the benchmark dataset:**

- 47,542 vertices, 64,891 edges
- Top 5% = ~2,400 vertices with adjacency lists
- Memory: ~10-20MB -- trivially fits in application heap

**When this makes sense:**

- Small-to-medium graphs (< 10M edges)
- Stable graph (infrequent writes)
- Latency-sensitive retrieval (< 10ms target)

---

### Layer 6: Denormalized Graph Views (Reduces traversal depth)

Instead of requiring multi-hop traversal to assemble context, create summary edges during ingestion.

**Example:**

A 2-hop path like:

```
Apple Inc. --[Discloses]--> Revenue --[Has_Component]--> Services Segment
```

Can be precomputed as a direct edge:

```
Apple Inc. --[Has_Revenue_Component]--> Services Segment
   properties: { path: "Revenue->Services Segment", depth: 2 }
```

**When to use:**

- Common query patterns are known in advance
- Traversal depth > 1 is frequently needed
- Freshness requirements allow batch recomputation

**Trade-off:** Increases storage and ingestion complexity. Only worth it for high-frequency query patterns.

---

### Layer 7: Async Pre-fetching (Hides latency behind LLM processing)

When a user query arrives, you can predict likely graph entities before the full pipeline completes.

**Pattern:**

```
User: "Tell me about Apple's revenue risks"

PARALLEL:
  Thread 1: LLM extracts entities ["Apple", "revenue", "risk"]
  Thread 2: Vector DB returns top-K candidates
  Thread 3: Pre-fetch 1-hop for "AAPL" (most likely root)

By the time entity extraction completes, the subgraph is already in memory.
```

**Latency hiding:** If LLM entity extraction takes ~200ms and graph traversal takes ~60ms, pre-fetching hides the graph cost entirely.

---

### Layer 8: Gremlin Execution Optimization (Low-level, ~10-20ms savings)

**Use bytecode instead of script submission:**

```python
# SLOW: script-based (string parsing + compilation overhead)
client.submit("g.V(4208).out('Discloses').elementMap().toList()")

# FAST: bytecode (pre-compiled, no parsing)
g = traversal().withRemote(conn)
g.V(4208).out('Discloses').elementMap().toList()
```

**Persistent connections:**

- Maintain a connection pool to Gremlin Server
- Avoid per-query WebSocket handshake
- Use the gremlin-python `DriverRemoteConnection` with pool settings

---

## Canonical Query Templates

These are the production-ready Gremlin patterns for GraphRAG retrieval. Every query entering JanusGraph should be a variation of one of these.

### 1. Single entity context retrieval

The most common query: given an entity, get its immediate context.

```groovy
g.V(entity_id)
  .outE('Discloses', 'Has_Component')
  .inV()
  .dedup()
  .limit(20)
  .elementMap()
  .toList()
```

Latency: ~10-15ms. Use when the LLM needs context about a single entity.

### 2. Single entity with specific metric

Targeted retrieval when the query mentions a specific attribute. Uses the metric's internal ID directly (resolved via the mapping layer), avoiding string-match ambiguity.

```groovy
g.V(entity_id)
  .out('Discloses')
  .where(__.is(metric_id))
  .limit(1)
  .elementMap()
  .toList()
```

Or, if the metric is resolved upstream to its own vertex ID:

```groovy
g.V(metric_id).elementMap().toList()
```

Latency: ~1-5ms. Use for direct factual lookups ("What is Apple's revenue?").

### 3. Multi-entity parallel comparison

Compare the same metric across companies. Execute in parallel, merge results.

```python
from concurrent.futures import ThreadPoolExecutor

def fetch_metric(company_id, metric_id):
    return g.V(company_id).out('Discloses').where(__.is(metric_id)).limit(1).elementMap().toList()

with ThreadPoolExecutor(max_workers=4) as pool:
    futures = {
        ticker: pool.submit(fetch_metric, id_map[ticker], id_map[f'{ticker}:Gaming Revenue:FIN_METRIC'])
        for ticker in ['NVDA', 'AMD', 'INTC']
    }
    results = {ticker: f.result() for ticker, f in futures.items()}
```

Latency: ~10-15ms (parallel). Use for comparative analysis. All lookups are by internal ID -- no string matching.

### 4. Bounded multi-hop traversal

When deeper context is needed (e.g., supply chain, risk propagation).

```groovy
g.V(entity_id)
  .repeat(outE('Discloses', 'Subject_To').inV())
  .times(2)
  .dedup()
  .limit(50)
  .elementMap()
  .toList()
```

Latency: ~20-40ms. Use sparingly; prefer 1-hop with richer predicates.

### 5. Neighborhood summary (for context assembly)

Retrieve the structure around a node for LLM context windows.

```groovy
g.V(entity_id)
  .project('entity', 'neighbors', 'edges')
  .by(elementMap())
  .by(both().dedup().limit(30).elementMap().fold())
  .by(bothE().limit(30).elementMap().fold())
```

Latency: ~15-25ms. Returns entity + neighbors + edges in a single round-trip.

### 6. Tenant-scoped degree ranking

Find the most connected entities within a company's subgraph.

```groovy
g.V(entity_id)
  .out()
  .groupCount()
  .by('name')
  .order(local)
  .by(values, desc)
  .limit(local, 10)
```

Latency: ~15-25ms. Use for "What are the most important entities related to X?"

---

## End-to-End Example

**Query:** "Compare gaming and data center revenue of NVIDIA and AMD."

### Phase 1: Resolve (Vector DB + LLM)

The LLM extracts entities and the relevant predicates. The vector DB maps them to `external_id` values.

```
Entities:       NVIDIA, AMD
Metrics:        Gaming Revenue, Data Center Revenue
Predicates:     Discloses

Resolved external_ids:
  NVDA:NVIDIA:ORG
  AMD:AMD:ORG
  NVDA:Gaming Revenue:FIN_METRIC
  NVDA:Data Center Revenue:FIN_METRIC
  AMD:Gaming Revenue:FIN_METRIC
  AMD:Data Center Revenue:FIN_METRIC
```

### Phase 2: Map (Redis, ~1-2ms)

```
NVDA:NVIDIA:ORG                     -> 4208
AMD:AMD:ORG                         -> 5121
NVDA:Gaming Revenue:FIN_METRIC      -> 4315
NVDA:Data Center Revenue:FIN_METRIC -> 4322
AMD:Gaming Revenue:FIN_METRIC       -> 5240
AMD:Data Center Revenue:FIN_METRIC  -> 5247
```

### Phase 3: Traverse (JanusGraph, parallel, ~10-15ms)

Four queries execute concurrently. Each uses only internal IDs -- no string matching, no index lookups:

```groovy
g.V(4208).out('Discloses').where(__.is(4315)).limit(1).elementMap()   // NVDA -> Gaming Revenue
g.V(4208).out('Discloses').where(__.is(4322)).limit(1).elementMap()   // NVDA -> Data Center Revenue
g.V(5121).out('Discloses').where(__.is(5240)).limit(1).elementMap()   // AMD  -> Gaming Revenue
g.V(5121).out('Discloses').where(__.is(5247)).limit(1).elementMap()   // AMD  -> Data Center Revenue
```

### Phase 4: Assemble Context

Results are merged into a structured comparison and passed to the LLM:

```
NVIDIA:
  Gaming Revenue    -> {entity properties, 1-hop neighbors}
  Data Center Rev.  -> {entity properties, 1-hop neighbors}
AMD:
  Gaming Revenue    -> {entity properties, 1-hop neighbors}
  Data Center Rev.  -> {entity properties, 1-hop neighbors}
```

### Total latency: ~15-20ms

- Resolve: handled upstream (not counted in graph budget)
- Map: ~1ms (Redis)
- Traverse: ~10-15ms (4 parallel queries)
- Assemble: ~2ms (in-process)

Every invariant holds: entry by internal ID, depth = 1, predicate-scoped to `Discloses`, limit applied, no global state.

---

## Failure Modes and Mitigations

### 1. High-degree vertex explosion

**Problem:** Some vertices (e.g., "SEC", "Revenue", "United States") have hundreds or thousands of edges. A single `out()` on these vertices triggers massive Cassandra reads.

**Symptoms:** p99 latency spikes to 200ms+; individual queries occasionally take 500ms+.

**Mitigations:**

- Predicate filtering (Layer 2) -- reduces edge scan scope
- Degree-based throttling: if `bothE().count() > threshold`, switch to a precomputed summary instead of live traversal
- Move high-degree vertices permanently into the hot subgraph layer (Layer 5)
- Consider splitting "hub" vertices into per-tenant copies during ingestion to reduce degree

### 2. Cache misses on cold entities

**Problem:** Entities not in the hot cache or precomputed subgraph cache trigger live Cassandra traversals. First access to a cold entity is 3-5x slower than cached.

**Symptoms:** Bimodal latency distribution -- most queries at ~5ms, some at ~50ms.

**Mitigations:**

- Background warming: after ingestion, pre-traverse and cache the top entities by degree or predicted query frequency
- Async pre-fetching (Layer 7): when Vector DB returns candidates, immediately start warming their subgraphs
- Popularity-based promotion: track query frequency per entity, promote to hot cache when threshold crossed

### 3. Skewed query distribution (hot-spot problem)

**Problem:** A small number of entities (major companies, common financial terms) dominate query traffic. These entities may hit Redis and JanusGraph connection limits.

**Symptoms:** Redis key hotspotting; Gremlin Server thread saturation for specific vertices.

**Mitigations:**

- In-memory hot subgraph layer (Layer 5) -- bypasses both Redis and JanusGraph for the hottest entities
- Read-through cache with TTL: ensures hot entities are always served from memory
- Connection pool sizing: scale Gremlin Server pool relative to expected concurrency

### 4. Cassandra compaction and load spikes

**Problem:** Cassandra compaction events temporarily increase read latency. During bulk ingestion, read performance degrades.

**Symptoms:** Periodic latency variance; p95 diverges from p50 during maintenance windows.

**Mitigations:**

- Aggressive caching (Layers 4-5) -- shields the application from backend variance
- Separate read and write paths: during bulk ingestion, route all reads through cache with stale-while-revalidate
- SLA-based circuit breakers: if Cassandra latency exceeds threshold, serve entirely from cache with a staleness warning
- Schedule bulk ingestion during low-traffic windows

### 5. ID mapping inconsistency

**Problem:** If the Redis ID mapping and JanusGraph get out of sync (e.g., partial ingestion failure, manual graph edits), queries will hit wrong vertices or fail silently.

**Symptoms:** Incorrect context returned to LLM; `g.V(id)` returns unexpected vertex or empty result.

**Mitigations:**

- Atomic write: ID mapping write and vertex creation must be in the same logical transaction (streamed mode guarantees this)
- Health check: periodic sample-based validation that Redis IDs resolve to expected vertices
- Rebuild trigger: if validation failure rate exceeds threshold, rebuild the mapping from graph

---

## Latency Budget

Target: **< 50ms** from entity IDs to graph context.

**Live traversal path:**

| Component | Budget | How |
|---|---|---|
| ID resolution | 1-2ms | Redis lookup |
| Graph entry | 1ms | `g.V(id)` direct |
| 1-hop traversal | 10-20ms | Predicate-scoped, budgeted |
| 2-hop traversal | 20-40ms | Only if needed, hard-limited |
| Serialization | 2-5ms | `elementMap()` to JSON |
| **Total** | **~25-50ms** | Within budget |

**Cached path (precomputed subgraph):**

| Component | Budget | How |
|---|---|---|
| ID resolution | 1-2ms | Redis lookup |
| Cache lookup | 1-3ms | Redis/in-memory |
| **Total** | **~3-5ms** | 10x under budget |

---

## Observability and SLOs

Track the following per query to maintain the latency contract in production:

**Per-query metrics:**

| Metric | Purpose |
|---|---|
| Traversal depth | Detect invariant violations (depth > 2) |
| Nodes visited | Detect fan-out issues before they hit latency |
| Edges scanned | Correlates directly with Cassandra read cost |
| Cache hit/miss | Determines which latency path was taken |
| Latency breakdown | Resolve / Map / Traverse / Serialize -- isolates bottlenecks |

**Service-level objectives:**

| Percentile | Target | Breach action |
|---|---|---|
| p50 | < 20ms | Normal operation |
| p95 | < 50ms | Investigate cache miss rate |
| p99 | < 100ms | Check for high-degree vertices or Cassandra load |

If p99 consistently exceeds 100ms, the most likely causes are: uncached high-degree vertices, Cassandra compaction load, or an invariant violation in a new query path. The observability metrics above will isolate which.

---

## Why This Architecture Wins

### vs. Neo4j (query-time optimization)

Neo4j optimizes at query-time: its native storage engine makes `MATCH (n {name: ...})` fast enough that you don't need external caching for moderate-scale graphs. This works well up to single-server limits.

This architecture optimizes at system-level: by externalizing search and caching traversals, it achieves comparable retrieval latency (~5ms cached, ~25-50ms live) while running on horizontally scalable infrastructure. The trade-off is operational complexity for linear write scaling.

### vs. Memgraph (in-memory speed)

Memgraph gives sub-millisecond traversal latency by keeping the entire graph in RAM. This works for graphs that fit in memory on a single node.

This architecture achieves near-Memgraph latency for hot entities (3-5ms from the in-memory cache) while supporting graphs that exceed single-node memory via Cassandra's distributed storage. The hot subgraph layer is effectively an application-controlled version of what Memgraph does at the engine level.

### vs. naive GraphRAG (graph-as-search-engine)

Most GraphRAG implementations enter the graph via attribute search (`g.V().has(...)` or `MATCH (n {name: ...})`), treating the graph as both search engine and traversal engine. On distributed graphs, this doubles the latency budget: one round-trip for search, another for traversal.

This architecture separates the two concerns, eliminating the search round-trip from the graph layer entirely. The graph only does what it's structurally good at: following edges from known starting points.

---

## Recommended Stack

```
+------------------+     +-------------------+     +-----------------+
|                  |     |                   |     |                 |
|   Vector DB      |---->|   Redis           |---->|  JanusGraph     |
|   (Weaviate /    |     |   (ID mapping +   |     |  (traversal     |
|    Pinecone /    |     |    subgraph cache) |     |   engine only)  |
|    Milvus)       |     |                   |     |                 |
+------------------+     +-------------------+     +-----------------+
        |                                                   |
        |            +------------------+                   |
        +----------->|                  |<------------------+
                     |   Context        |
                     |   Builder        |
                     |                  |
                     +--------+---------+
                              |
                              v
                     +------------------+
                     |                  |
                     |      LLM         |
                     |                  |
                     +------------------+
```

| Layer | Technology | Role |
|---|---|---|
| Retrieval | Vector DB | Semantic search, top-K entity retrieval |
| Mapping | Redis | `external_id -> internal_id`, subgraph cache |
| Graph | JanusGraph | Bounded traversal via `g.V(id)` only |
| Cache | Redis (hot) / S3 (warm) | Precomputed subgraphs for frequent entities |
| Optional | Feature store | Precomputed summaries, entity embeddings |

---

## Where Each Database Fits

| Use Case | Best Fit | Why |
|---|---|---|
| Low-latency interactive GraphRAG | Neo4j | Native index-free adjacency, tightly coupled search + traversal |
| Large-scale distributed graphs | JanusGraph + external search | Cassandra scales horizontally; separate search layer handles resolution |
| Hybrid (search + traversal in one) | Neo4j | Less operational complexity |
| Multi-datacenter, high write throughput | JanusGraph | Cassandra replication, tunable consistency |
| Sub-millisecond, memory-constrained | Memgraph | Full in-memory, no disk I/O |

The choice isn't "which is faster." It's "where does your scale and deployment model push you."

---

## The Shift in Mental Model

| | Neo4j Approach | JanusGraph Approach |
|---|---|---|
| Graph role | Queryable system (search + traverse) | Precomputed + traversable structure |
| Entry pattern | `MATCH (n {name: ...})` | `g.V(id)` -- ID provided externally |
| Optimization target | Query planner, indexes | Ingestion pipeline, caching layers |
| Scaling model | Scale up (bigger instance) | Scale out (more Cassandra nodes) |
| Best for | Interactive, low-latency, moderate scale | Distributed, high-throughput, large scale |

---

## Competitive Advantage

Because DomynGraph controls the full pipeline -- ingestion, ID mapping, traversal, and context assembly -- it can implement optimizations that are impossible in a generic graph query interface:

- **Ingestion-time ID capture** eliminates the resolution bottleneck
- **Predicate-aware traversal** reduces irrelevant reads by 90%+
- **Precomputed subgraphs** remove the database from the hot path entirely
- **Traversal budgeting** guarantees bounded latency regardless of graph topology

This is something Neo4j users often don't do, because Neo4j's native performance makes it unnecessary at moderate scale. At large scale and in distributed deployments, these optimizations are not optional -- they are the architecture.

---

## What This Architecture Explicitly Avoids

These are intentional trade-offs, not limitations.

- **Query planners.** The graph never plans a query. Every traversal is fully specified by the application layer -- predicates, depth, and limits are determined before the graph is touched.
- **Join-heavy graph queries.** No `match()` patterns, no multi-path joins, no subgraph isomorphism. These are powerful but unpredictable in latency on distributed backends.
- **Global graph reasoning at runtime.** No `g.V().count()`, no full-graph aggregations, no `pageRank()` at query-time. All global analysis is batch and offline.
- **Schema-less exploration at query time.** The set of valid predicates and traversal patterns is known at ingestion time. The query layer does not discover graph structure -- it executes against known structure.

These constraints are what make the latency guarantees possible. Relaxing any of them re-introduces the distributed read costs that this architecture is designed to eliminate.

---

GraphRAG performance is not a property of the graph database -- it is a property of the system architecture.

Distributed graphs are not slow. Unbounded queries are.

When identity resolution is externalized and traversal is bounded, distributed graph systems deliver predictable, low-latency retrieval at scale.

This architecture does not make JanusGraph behave like a single-node graph database. It uses JanusGraph for what it is best at: deterministic traversal over large, distributed graphs.
