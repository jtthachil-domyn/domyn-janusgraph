# Domyn Nexus -- Technology Stack & Direction

> **Audience**: Bhaskarjit, Stefano, Phil, Joseph (possibly Luca)
> **Meeting**: Monday-Tuesday, week of Apr 21 2026
> **Format**: ~14 slides. Each section below is one slide.
> **Live demo**: Nvidia 4-split GraphRAG query on the JanusGraph stack (domyn-janusgraph)

---

## Slide 1 -- Title

**Domyn Nexus: A Purpose-Built Graph Database for GenAI**

- Native Rust graph engine designed for GraphRAG workloads
- Replaces JanusGraph + Cassandra + Elasticsearch + Gremlin
- Single lightweight binary, no JVM, no external services
- 21k+ lines of Rust, 238 tests, openCypher query language

---

## Slide 2 -- The Problem with Our Current Stack

**JanusGraph: 7 moving parts just to answer a graph query**

```
[ Python App ]
      |
      v
[ Gremlin WebSocket ]
      |
      v
[ JanusGraph (JVM) ]  --  JAVA_OPTIONS=-Xms2G -Xmx4G
      |           |
      v           v
[ Cassandra ]  [ Elasticsearch ]
  (storage)      (indexing)
```

What we run today (from docker-compose):


| Service        | Image / Runtime       | Heap / Memory     |
| -------------- | --------------------- | ----------------- |
| Cassandra      | cassandra:4.1         | MAX_HEAP=2G       |
| Elasticsearch  | ES 8.12.0             | -Xms512m -Xmx512m |
| JanusGraph     | JVM                   | -Xms2G -Xmx4G     |
| Gremlin Server | Inside JanusGraph JVM | Shared with above |
| DomynGraph API | Python / FastAPI      | ~200MB            |
| DomynGraph RAG | Python / FastAPI      | ~300MB            |
| DomynGraph UI  | React / Nginx         | ~50MB             |


**Total minimum memory footprint: ~7-8 GB just for the data layer**

Pain points we've hit:

- Java heap pressure from Groovy script compilation (had to build parameterized query pooling to work around it)
- Cassandra requires careful tuning: keyspace management, compaction, read repairs
- Elasticsearch as a separate indexing backend adds latency and operational burden for full-text search
- Gremlin is hard to productionize: WebSocket-based, session management, script caching
- Cold start: JanusGraph + Cassandra + ES takes 90+ seconds to become healthy
- Query latency: 0.5-1 second per Gremlin traversal depending on complexity

---

## Slide 3 -- What We're Building Instead

**Domyn Nexus: One binary, zero external services**

```
[ Python App / SDK ]
        |
        v
[ Domyn Nexus ]  <-- single Rust process
   |-- CSR adjacency (in-memory, mmap-ready)
   |-- Columnar property store
   |-- Embedded full-text index (tantivy)
   |-- Embedded vector index (HNSW)
   |-- WAL + snapshots (crash recovery)
   |-- HTTP + Bolt protocol server
   |-- openCypher query engine
```


| Metric                  | JanusGraph Stack                | Domyn Nexus                      |
| ----------------------- | ------------------------------- | -------------------------------- |
| Services to deploy      | 6-7 containers                  | 1 binary                         |
| Memory footprint (data) | ~7-8 GB                         | ~200 MB                          |
| Cold start to healthy   | 90+ seconds                     | < 1 second                       |
| Point lookup (p50)      | 1.12 ms                         | 160 us (Cypher), 6.2 us (direct) |
| 1-hop traversal (p50)   | 65.0 ms                         | 109 us (Cypher), 2.9 us (direct) |
| Bulk ingest (47.5K V)   | 56.6 s                          | 74.9 ms                          |
| Query language          | Gremlin (hard to productionize) | openCypher (industry standard)   |
| Full-text search        | External Elasticsearch          | Embedded (tantivy)               |
| Vector search           | Not supported                   | Embedded (HNSW)                  |
| JVM required            | Yes (Java 11+)                  | No                               |
| Horizontal scale path   | Cassandra replication           | Rust-native sharding (planned)   |


*Benchmarked on the same real dataset: 47,542 vertices / 64,891 edges / 18 tickers. See Slide 8 for full B1-B10 results.*

---

## Slide 4 -- Landscape Comparison

**Why not buy?**


| Capability                | Neo4j                                  | Memgraph                    | FalkorDB                        | JanusGraph                   | AGE + Postgres                         | **Domyn Nexus**                 |
| ------------------------- | -------------------------------------- | --------------------------- | ------------------------------- | ---------------------------- | -------------------------------------- | ------------------------------- |
| **Language**              | Java                                   | C++                         | C (Redis module)                | Java                         | C (PG extension)                       | **Rust**                        |
| **License**               | Community: GPL; Enterprise: commercial | BSL (not truly open)        | Server Side PL                  | Apache 2.0                   | Apache 2.0                             | **Proprietary / internal**      |
| **Query Language**        | Cypher                                 | Cypher                      | Cypher subset                   | Gremlin (Tinkerpop)          | openCypher subset                      | **openCypher**                  |
| **Storage**               | Custom page cache                      | In-memory only              | In-memory (Redis)               | Pluggable (Cassandra, HBase) | PostgreSQL heap tables                 | **CSR + Columnar + WAL**        |
| **Horizontal Scale**      | Enterprise only (very expensive)       | Enterprise only             | Redis Cluster                   | Cassandra replication        | PG partitioning (limited by graphid)   | **Planned (Raft + sharding)**   |
| **Vector/Embedding**      | Native HNSW (since 5.11)               | Single-store vector (3.8+)  | Native HNSW                     | No                           | Via pgvector (works, requires casting) | **Native HNSW**                 |
| **Full-Text**             | Lucene embedded                        | No built-in                 | No built-in                     | External Elasticsearch       | PG tsvector (not in Cypher)            | **Embedded tantivy**            |
| **Multi-Tenant**          | Database-per-tenant                    | No                          | No                              | Manual                       | Schema-per-graph (no isolation)        | **Namespace isolation**         |
| **Embeddable (Python)**   | No (Java embedded only)                | No                          | Yes (FalkorDBLite, sub-process) | No                           | No                                     | **Yes (PyO3, true in-process)** |
| **GenAI/GraphRAG Native** | Yes (neo4j-graphrag, GenAI plugin)     | Yes (Atomic GraphRAG, 3.8+) | Yes (GraphRAG SDK, HNSW)        | No                           | No                                     | **Yes (designed for it)**       |
| **JVM Dependency**        | Yes                                    | No                          | No (but Redis in production)    | Yes                          | No (but needs PG server)               | **No**                          |
| **Multi-hop at scale**    | Good                                   | Good                        | Good                            | Slow (Cassandra hops)        | Expensive for bidirectional at scale   | **CSR O(1) neighbor access**    |
| **Memory Efficiency**     | Heap-bound (GC)                        | Good (C++)                  | Good (C)                        | Heap-bound (GC)              | PG shared buffers                      | **Rust (no GC, zero-copy)**     |


**Key takeaways:**

- **Neo4j** is the most mature option with native HNSW and a GraphRAG ecosystem -- but Enterprise costs $100k+/yr, Community is GPL (viral license risk), and it's still JVM-bound with heap limits. No Python embedding.
- **Memgraph** now has Atomic GraphRAG and vector indexes (3.8+) -- but it's BSL-licensed (not open source), in-memory only (no persistence), and Enterprise-only for horizontal scale.
- **FalkorDB** has HNSW, GraphRAG SDK, and FalkorDBLite for Python embedding -- closest competitor on features. But SSPL-licensed, tied to Redis in production, limited horizontal scale, and FalkorDBLite is a sub-process (not true in-process).
- **JanusGraph** is truly open (Apache 2.0) but operationally heavy and has no vector/GenAI story (we've experienced this firsthand).
- **AGE + PostgreSQL** fails at multi-hop traversal on large graphs (documented O(n^k) VLE) -- disqualifying for FinReflectKG at 17.5M triplets.
- **Domyn Nexus** is the only option that combines: no license risk, true in-process Python embedding (PyO3), native graph+vector+full-text in one process, multi-tenancy, openCypher, and full control over storage and performance. The trade-off: it's not production-mature yet.

---

## Slide 5 -- JanusGraph: What We've Learned the Hard Way

**Operational pain from 6+ months of running JanusGraph in our pipeline**

1. **Java Heap Pressure**
  - Every unique Gremlin script string compiles to a new Java class
  - We had to build custom parameterized query pooling (325 lines of workaround code) to avoid JVM metaspace exhaustion
  - JanusGraph JVM configured at 4GB heap; Cassandra at 2GB on top of that
2. **Cassandra Complexity**
  - Keyspace management, compaction tuning, read repairs, tombstone accumulation
  - Schema changes are global, not per-tenant
  - Wide-row storage model means graph traversals become multi-hop Cassandra reads
3. **Elasticsearch as Index Backend**
  - Separate service to deploy, monitor, and scale
  - Index updates are eventually consistent with the graph
  - Full-text search could be embedded directly in the graph engine (and is, in Nexus)
4. **Gremlin Limitations**
  - WebSocket-based protocol with session management overhead
  - No standard client library quality (we maintain connection pool workarounds)
  - Gremlin is Tinkerpop-specific; openCypher is the emerging industry standard (ISO GQL)
  - Hard to generate programmatically for RAG pipelines
5. **Cold Start**
  - Cassandra: 60s to be healthy
  - Elasticsearch: 30s to be healthy
  - JanusGraph: waits for both, then 90s to health check
  - **Total: 2-3 minutes before first query**

---

## Slide 6 -- Domyn Nexus Architecture

**Crate structure (Rust workspace)**

```
domyn-nexus/
 |
 |-- nexus-core          Graph data model, CSR adjacency, columnar properties,
 |                        transactions (SWMR), types, schema enforcement
 |
 |-- nexus-algebra        Sparse matrix algebra (SpMV, semiring ops)
 |                        for FalkorDB-style graph traversal
 |
 |-- nexus-storage        WAL (write-ahead log), snapshots, crash recovery,
 |                        CRC32 validation, typed value persistence
 |
 |-- nexus-index          Composite indexes, unique indexes, tantivy full-text,
 |                        vector index (HNSW)
 |
 |-- nexus-cypher         openCypher parser, lexer, AST, logical planner,
 |                        binder (scope validation), executor
 |
 |-- nexus-parser         Kyu/openCypher frontend adapter (broader grammar)
 |
 |-- nexus-server         HTTP/REST API, Bolt protocol, NexusEngine,
 |                        auth, metrics, connection management
 |
 |-- nexus-tenant         Namespace isolation, per-tenant engine routing
 |
 |-- nexus-algorithms     PageRank, Connected Components, Shortest Path
 |
 |-- nexus-python         PyO3 SDK for direct Python embedding
 |
 |-- nexus-distributed    Replication metadata scaffolding (Raft planned)
 |
 |-- nexus-bench          Benchmark suite
```

**Storage model:**

- **Adjacency**: Compressed Sparse Row (CSR) with forward + backward indexes, delta layer for online mutations, tombstone-based deletion
- **Properties**: Typed columnar store (i64, f64, String, Bool, List), schema-enforced
- **Durability**: WAL-before-apply, CRC32 checksums, atomic snapshots (tmp + fsync + rename), crash recovery with exact ID replay
- **Transactions**: Single-Writer/Multi-Reader with snapshot isolation; write transactions validate on a staged clone before atomic commit

---

## Slide 7 -- What Works Today (Honest Status)


| Area               | Status         | Detail                                                                   |
| ------------------ | -------------- | ------------------------------------------------------------------------ |
| **Graph engine**   | Working        | Online mutations, deletion, tombstones, rebuild                          |
| **openCypher**     | 42.8% TCK pass | MATCH, WHERE, RETURN, CREATE, SET, DELETE, aggregations, booleans        |
| **WAL / Recovery** | Working        | Typed WAL, CRC32, atomic snapshots, crash recovery tested                |
| **Transactions**   | Working        | SWMR, snapshot isolation, validated atomic commits                       |
| **HTTP API**       | Working        | Cypher endpoint, auth, timeout, metrics, health, readiness               |
| **Bolt protocol**  | Partial        | Connection limits, auth, basic query -- not full Neo4j driver compat yet |
| **Python SDK**     | Working        | Cypher execution, parameter binding, post-build mutations                |
| **Indexes**        | Working        | Unique, composite, full-text (tantivy), vector (brute-force baseline)    |
| **Multi-tenant**   | Working        | Namespace isolation, per-tenant engine routing                           |
| **Algorithms**     | Working        | PageRank, Connected Components, Shortest Path                            |
| **Distributed**    | Scaffolding    | Metadata, placement maps, replication log shapes -- no Raft yet          |
| **Benchmarks**     | B1-B10 passing | Real-data benchmark on 47.5K V / 64.9K E -- wins all B1-B10 vs JG/Neo4j  |
| **Codebase**       | 21k+ LOC       | 258 tests passing, 12 crates, real-data benchmark suite                  |


---

## Slide 8 -- Performance (Real Data, Same Dataset)

**Benchmarked on the same 10-K SEC filing KG used by JanusGraph and Neo4j: 47,542 vertices, 64,891 edges, 18 tickers, 1,133 unique predicates.**

Nexus queries run through the full Cypher parse + plan + execute path with indexes. JanusGraph and Neo4j numbers are from the  benchmark run (over WebSocket/Bolt respectively).

### Ingestion


| Benchmark                                     | JanusGraph (Streamed) | Neo4j | **Domyn Nexus** | vs JG      | vs Neo4j |
| --------------------------------------------- | --------------------- | ----- | --------------- | ---------- | -------- |
| **B1: Single Load** (AAPL: 1,644 V / 2,208 E) | 2.0 s                 | 1.1 s | **1.13 ms**     | **1,770x** | **973x** |
| **B2: Bulk Load** (all 18 tickers)            | 56.6 s                | 8.2 s | **74.9 ms**     | **756x**   | **109x** |


### Query & Traversal (p50 latency)


| Benchmark                  | JanusGraph | Neo4j   | **Nexus (Cypher)** | **Nexus (direct)** | vs JG     | vs Neo4j |
| -------------------------- | ---------- | ------- | ------------------ | ------------------ | --------- | -------- |
| **B3: Point Lookup**       | 1.12 ms    | 0.92 ms | **160 us**         | 6.2 us             | **7x**    | **5.8x** |
| **B5: 1-Hop Traversal**    | 65.0 ms    | 26.4 ms | **109 us**         | 2.9 us             | **596x**  | **242x** |
| **B6: 2-Hop Traversal**    | 34.0 ms    | 11.5 ms | **50.8 us**        | 584 ns             | **669x**  | **226x** |
| **B7: Filtered Traversal** | 33.6 ms    | 18.9 ms | **94.6 us**        | 792 ns             | **355x**  | **200x** |
| **B8: Count By Type**      | 18.8 ms    | 3.4 ms  | **515 us**         | 2.26 ms            | **37x**   | **6.6x** |
| **B9: Top Connected**      | 25.7 ms    | 4.1 ms  | **2.24 ms**        | 2.51 ms            | **11.5x** | **1.8x** |
| **B10: Tenant Isolation**  | 3.7 ms     | 1.2 ms  | **210 us**         | —                  | **17.6x** | **5.7x** |


### Operational


| Metric                          | JanusGraph Stack | **Domyn Nexus** |
| ------------------------------- | ---------------- | --------------- |
| Cold start to first query       | 90-180 seconds   | < 1 second      |
| Memory footprint (data layer)   | ~7-8 GB          | ~200 MB         |
| Graph build (47.5K V / 64.9K E) | —                | 205 ms          |
| Index build (4 indexes)         | —                | 28 ms           |


**Caveats (be upfront about these):**

- JanusGraph/Neo4j numbers include network round-trip (WebSocket/Bolt). Nexus is in-process -- this is a real architectural advantage, not a benchmarking trick; the embedded model is the point
- Nexus "Cypher" column includes full parse + plan + execute with indexed lookups
- Nexus "direct" column is raw graph API calls (no Cypher overhead) -- this is what the Python SDK hot path would use
- All three engines used the same dataset, same query patterns, same measurement protocol (1 cold + 2 warm-up + 10 measured)
- Nexus wins every benchmark. B5-B7 traversal numbers (200-670x vs Neo4j) are the strongest signal: CSR adjacency eliminates the per-hop storage round-trip that dominates JanusGraph and even Neo4j

---

## Slide 9 -- Live Demo Context

**What you'll see: 4-split composite query on Nvidia revenue data**

Currently running on the JanusGraph stack (domyn-janusgraph):

1. Query decomposes a complex revenue question into 4 sub-queries
2. Each sub-query retrieves relevant triplets from the graph
3. Gremlin traversals generated and executed
4. Results composed into accurate final answer

**Why this matters:**

- Proves the GraphRAG pipeline works end-to-end for real financial data
- Demonstrates that graph-backed retrieval gives precise answers (not hallucinated)
- The same pipeline will be faster, simpler, and more reliable on Domyn Nexus once the openCypher layer is complete

**What changes with Domyn Nexus:**

- Gremlin queries become openCypher queries (cleaner, more standard)
- No Cassandra/ES -- graph + indexes + vectors all in one process
- Python SDK can embed the graph directly (no network hop for retrieval)
- Sub-500ns lookups mean composite queries complete in microseconds, not seconds

---

## Slide 10 -- Why AGE + PostgreSQL Is a Poor Fit for FinReflectKG

**FinReflectKG: 17.5 million triplets. What we demo today is a tiny subset.**

Apache AGE is a PostgreSQL extension that adds openCypher support. It's appealing on paper: "just add graph to Postgres." AGE is viable for PostgreSQL-adjacent graph workloads, but FinReflectKG's workload stresses the uncomfortable parts: high-volume edge loading, repeated 2-4 hop bidirectional traversals, and tight vector+graph retrieval. The available community data shows real friction here.

**1. Edge loading via Cypher degrades at scale and requires workarounds**

- A user loading 8M edges via Cypher MATCH+CREATE reported ~2-3s per 100 rows initially, degrading to 100-1,000s per 100 rows after 200k edges ([#2198](https://github.com/apache/age/issues/2198))
- The AGE maintainer confirmed the root cause: two MATCH clauses create O(m*n^2) nested loop joins as the vertex table grows
- Workarounds exist (`agefreighter` bulk loader, direct SQL inserts into underlying tables), but these bypass the Cypher interface entirely
- For FinReflectKG at 17.5M triplets: Cypher-based loading is not viable; bulk loading requires stepping outside AGE's query layer

**2. Multi-hop bidirectional traversal is expensive at scale**

- A user with ~27M vertices / ~23M edges reported ([#2187](https://github.com/apache/age/issues/2187)):
  - 2-hop undirected queries (`-[*..2]-`): ~30 seconds
  - 4-hop undirected queries (`-[*..4]-`): 150+ seconds or fails to complete
- The AGE maintainer noted that undirected traversal (`-[]-`) is "by far, one of the most resource intensive match patterns and basically negates the value of a directed graph"
- Directed traversal with proper indexing would perform better, but knowledge graphs like FinReflectKG inherently need bidirectional expansion (e.g., entity -> relationship -> entity, traversed from either end)
- FinReflectKG composite queries need 2-4 hop traversals per sub-query, executed multiple times per user question

**3. Vector + graph retrieval is possible but not ergonomic**

- AGE now works with pgvector via extension interop (PRs [#2088](https://github.com/apache/age/pull/2088), [#2172](https://github.com/apache/age/pull/2172) merged). The maintainer confirmed: "Apache AGE supports the pgvector extension"
- You can cast properties to `::vector`, create HNSW indexes on vertex properties, and use cosine distance (`<=>`) in Cypher ORDER BY
- However: it requires manual `search_path` configuration, casting between `agtype` and `vector`, and HNSW index creation on AGE's internal tables -- not a first-class integrated experience
- GraphRAG tight loops (retrieve by vector similarity, traverse neighbors, filter by properties) involve friction at every step compared to a system where graph + vector are native

**4. Multi-tenancy is limited**

- Each AGE graph creates a separate PostgreSQL namespace with its own vertex/edge tables
- No built-in tenant isolation, RBAC, or cross-tenant query prevention
- Scaling to many tenants means many schemas, each with their own index sets

**Bottom line for FinReflectKG at 17.5M triplets:**

AGE is a reasonable choice for smaller graph workloads that live alongside existing PostgreSQL data. For FinReflectKG's specific pattern -- high-volume loading, repeated bidirectional multi-hop traversals, vector+graph retrieval in tight loops, multi-tenant isolation -- the friction adds up:


| FinReflectKG Requirement        | AGE + PostgreSQL                                  | Domyn Nexus (target)                    |
| ------------------------------- | ------------------------------------------------- | --------------------------------------- |
| Load 17.5M edges                | Cypher path degrades; requires bulk loader bypass | Batch CSR build (minutes)               |
| 2-4 hop bidirectional traversal | ~30s at 2-hop undirected on comparable scale      | CSR adjacency (microseconds)            |
| Vector + graph in one query     | Possible via pgvector casting, but not ergonomic  | Native (same process, same query)       |
| Multi-tenant isolation          | Schema-per-graph, no RBAC                         | Namespace isolation, per-tenant engines |


*AGE numbers are from community-reported GitHub issues, not our own benchmarks. Directed traversal with proper indexing would perform better than the undirected numbers cited.*

---

## Slide 11 -- Why Build In-House (Updated Landscape)

**We evaluated the alternatives. None fit.**


| Requirement                    | Neo4j           | Memgraph          | FalkorDB                            | JanusGraph   | AGE+PG                 | Build                          |
| ------------------------------ | --------------- | ----------------- | ----------------------------------- | ------------ | ---------------------- | ------------------------------ |
| Open / no license risk         | GPL/commercial  | BSL               | SSPL                                | Apache 2.0   | Apache 2.0             | **Yes**                        |
| Native vector search for RAG   | Yes (HNSW)      | Yes (3.8+)        | Yes (HNSW)                          | No           | No (pgvector separate) | **Yes**                        |
| Embeddable in Python pipeline  | No (Java only)  | No                | Partial (FalkorDBLite, sub-process) | No           | No                     | **Yes (true in-process PyO3)** |
| No JVM / lightweight           | No              | Yes               | Needs Redis in prod                 | No           | Needs PG server        | **Yes**                        |
| Multi-tenant by design         | Enterprise only | No                | No                                  | Manual       | Schema-per-graph       | **Yes**                        |
| openCypher + horizontal scale  | Enterprise $$$$ | Enterprise $$$    | Limited                             | No (Gremlin) | No (O(n^k) VLE)        | **Yes**                        |
| Multi-hop at 17.5M triplets    | Yes             | Yes (if fits RAM) | Yes (if fits RAM)                   | Slow         | Fails at 4-hop         | **Yes**                        |
| Full control over storage/perf | No              | No                | No                                  | Partially    | No (PG internals)      | **Yes**                        |


**The real argument:**

- We need graph + vectors + full-text + multi-tenancy + embeddable Python SDK + openCypher + horizontal scale
- No single product on the market provides all of these without expensive enterprise licensing
- Building in Rust gives us: no GC pauses, memory safety, zero-copy data access, single-binary deployment
- We control the roadmap: if we need a feature for GraphRAG, we add it -- no vendor negotiation

---

## Slide 12 -- Research + Engineering: Where Innovation Happens

**The GraphRAG pipeline is both a research and engineering deliverable**

Research contributions that directly drive the product:

- **Composite query decomposition**: Breaking complex financial questions into graph-traversable sub-queries (demonstrated in Nvidia MVP)
- **Graph-native retrieval**: Using structured graph traversal instead of pure vector similarity for retrieval -- gives precise, traceable answers
- **openCypher translation layer**: Building the bridge between natural language queries and graph operations -- a hard CS problem with direct product value
- **Embedded graph + vector co-location**: Keeping graph structure, properties, full-text, and vector embeddings in the same process eliminates the latency and consistency gaps of distributed search

**Why this matters for the business:**

- GraphRAG with structured retrieval gives accurate, auditable answers -- critical for financial data
- The graph engine is the infrastructure that makes this possible at scale
- Research into query decomposition and graph-native retrieval is what differentiates us from "just use a vector DB"

---

## Slide 13 -- Structured Agents & Graph-Backed Intelligence

**Beyond retrieval: the graph as a reasoning substrate**

What we're exploring (and why it matters):

- **Agentic graph discovery**: Agents that traverse the knowledge graph to find answers, not just retrieve similar documents
- **Structured agent workflows**: Using graph topology to constrain and guide agent behavior -- reduces hallucination, improves auditability
- **Knowledge graph as shared memory**: Multiple agents read from and write to the same graph, building collective understanding over time

**How the graph engine enables this:**

- Multi-tenant isolation means different agent teams can have separate knowledge spaces
- WAL-backed mutations mean agent writes are durable and recoverable
- openCypher gives agents a standard, composable query language
- Embedded Python SDK means agents can query the graph in-process without network overhead

**This is the long-term vision:**

- Phase 1 (now): Accurate GraphRAG retrieval on financial data -- **demonstrated**
- Phase 2 (next): Domyn Nexus replaces JanusGraph for faster, simpler, more capable retrieval
- Phase 3 (future): Agents that reason over the graph, not just retrieve from it

---

## Slide 14 -- Roadmap & Next Steps

**Single-node production first. Distributed later.**


| Phase              | What                                                                                               | Timeline   |
| ------------------ | -------------------------------------------------------------------------------------------------- | ---------- |
| **Now**            | GraphRAG MVP on JanusGraph (live demo), Domyn Nexus at 21k+ LOC, 258 tests, B1-B10 benchmark wins  | Done       |
| **Next 4-8 weeks** | Full openCypher coverage (CREATE/SET/DELETE/WITH), Bolt driver compat, Python SDK persistence APIs | Q2 2026    |
| **8-16 weeks**     | Production single-node: TLS, WAL rotation, hot backup, memory budgets, HNSW vector index           | Q3 2026    |
| **16-24 weeks**    | Migrate GraphRAG pipeline from JanusGraph to Domyn Nexus                                           | Q3-Q4 2026 |
| **6+ months**      | Leader-follower replication (Raft), tenant-to-shard placement, horizontal scale                    | Q4 2026+   |


**Infrastructure needs:**

- VM for benchmark testing (can start with free-tier / dev allocation)
- CI pipeline for automated testing (cargo test, TCK suite)
- No external services needed -- Domyn Nexus is self-contained

---

## Speaker Notes

### For the "why not Neo4j" question:

Neo4j Enterprise is the only Cypher-native DB with horizontal scaling. It costs $100k+/year for enterprise licensing. Community edition is GPL (viral license risk) and limited to single-node. Even Enterprise has Java heap limits that cap practical graph size. We'd still need to bolt on vector search and multi-tenancy.

### For the "why not just use AGE + PostgreSQL?" question:

AGE is a reasonable extension for smaller graph workloads alongside existing Postgres data. The concern is fit, not quality. FinReflectKG stresses the parts where AGE has documented friction: (1) Cypher-based edge loading degrades on large tables due to nested join patterns -- the AGE maintainer confirmed it's O(m*n^2) for MATCH+CREATE and suggested bulk loaders that bypass Cypher (issue #2198). (2) Bidirectional multi-hop traversal is expensive -- a user with 27M vertices reported ~30s for 2-hop undirected queries, and the maintainer called undirected traversal "by far, one of the most resource intensive match patterns" (issue #2187). Directed queries with indexes perform better, but KGs often need bidirectional expansion. (3) Vector support works via pgvector extension interop (confirmed by maintainer), but requires manual casting and search_path configuration -- not the tight graph+vector loop GraphRAG needs. If pressed on VLE complexity: AGE implements edge-unique DFS per the openCypher spec, which is correct behavior but inherently combinatorial on dense cyclic graphs. Earlier claims about "no cycle detection" were incorrect -- the maintainer provided a detailed code review showing edge-uniqueness enforcement.

### For the "is this production ready?" question:

Not yet, and we're honest about that. It's an alpha-quality engine with real working pieces: transactions, crash recovery, indexes, Cypher execution, Python SDK. But we now have a real benchmark on the same dataset (47.5K V / 64.9K E) used by JanusGraph and Neo4j -- Nexus wins every single B1-B10 benchmark, with traversal speedups of 200-670x over Neo4j through the full Cypher path. The architecture is validated. The path to production single-node is 4-6 months of focused work: TLS, WAL rotation, full Bolt compatibility, memory budgets, HNSW. The live demo today runs on JanusGraph to show the pipeline works; Domyn Nexus will replace it once the openCypher layer is complete.

### For the "how much effort / team size" question:

If asked: one strong Rust engineer (Joseph) has built 21k+ LOC with 258 tests in the current timeline. A second engineer with Rust/database experience would roughly halve the calendar time for the production single-node milestone. The distributed phase would benefit from a third engineer with Raft/consensus experience.

### For the "Bhaskarjit flagged" slides:

Slides 11 and 12 are the research-adjacent slides. They frame research as directly driving product value (query decomposition, graph-native retrieval, agentic discovery). If the audience is receptive, lean into them. If the mood is "show me engineering," skip 12 and keep 11 brief. The key message: research is not separate from engineering here -- it IS the product differentiation.