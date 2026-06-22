# ArangoDB Acquisition Due Diligence

> **Prepared for**: Domyn internal evaluation
> **Date**: May 2026
> **Status**: Pre-meeting research brief — questions marked with **[ASK]** are what we don't know and need to verify directly with ArangoDB.

---

## 1. Company Overview

| Attribute | Detail | Source |
|---|---|---|
| Founded | 2014 (codebase since 2011) | Tracxn, OpenHub |
| Founders | Claudius Weinberger, Frank Celler | CBInsights |
| HQ | Cologne, Germany (US: San Francisco) | Tracxn |
| Employees | ~123 (65 US, 22 Germany, 8 UK, 6 Spain, rest distributed) | LeadIQ |
| Total funding | $56-58M across 7 rounds | CBInsights |
| Last round | Corporate Minority, August 2025 (ORIX USA) | Tracxn |
| GitHub | 14.1k stars, 878 forks, 130+ contributors, ~52k commits | GitHub |
| Codebase | ~7.5M LOC. 54.9% C++, 40.8% JavaScript, rest TS/Python/CMake | OpenHub |
| License (since 3.12) | **BSL 1.1** (was Apache 2.0 before 3.12, Feb 2024). Community Edition capped at 100 GiB, non-commercial only | ArangoDB blog |
| Production deployments | 200+ (claimed) | arango.ai |
| DB-Engines ranking | Below Neo4j (51.76 score) — exact Arango score not public but consistently #2-#4 in multi-model | DB-Engines |
| Brand pivot | Rebranded from "ArangoDB" to "Arango" in 2025, now "Arango Contextual Data Platform" | arango.ai |

---

## 2. Funding History & Cap Table

| Round | Date | Amount | Lead / Participants |
|---|---|---|---|
| Seed | Nov 2016 | Undisclosed | Target Partners |
| Series A | Mar 2019 | Undisclosed | Bow Capital |
| Series B | Oct 2021 | $27.8M | Iris Capital, New Forge |
| Corporate Minority | Aug 2025 | Undisclosed | ORIX USA |
| **Total** | | **$56-58M** | |

**What we don't know — [ASK]:**
- Current valuation and implied price range
- Liquidation preferences on each round (1x non-participating? 2x?)
- Outstanding debt, SAFEs, or convertible instruments
- Current burn rate and runway (last known raise was Aug 2025 — 9 months ago)
- Cap table: % held by founders, employees (option pool), investors
- Any secondary sales or existing acquisition interest from other parties
- Whether the Aug 2025 "Corporate Minority" from ORIX was at a flat/down round vs. Series B valuation

**Signal to watch**: $56M raised over 10 years with 123 employees. At a rough ~$15M/year burn (industry-standard for this headcount), the Aug 2025 round was likely a runway extension, not a growth round. This suggests either (a) revenue is covering most of the burn, or (b) they're capital-constrained. **[ASK]** which.

---

## 3. Revenue & Business Metrics

**What's public:**
- Median enterprise contract value across Neo4j (a comp) is $441k/year (Vendr)
- Arango's pricing is "request pricing" — no published per-GB or per-core rates for Enterprise
- Community Edition: free, capped at 100 GiB, non-commercial use only
- ArangoGraph (managed cloud): developer tier starts at ~$42/month (2021 pricing); production pricing undisclosed

**What we don't know — [ASK]:**
- Current ARR (and split: cloud vs. self-hosted enterprise license)
- Gross margin on managed cloud (ArangoGraph)
- Customer count, ACV distribution, cohort retention, NRR
- Top 10 customers by revenue (and concentration risk)
- Sales motion: PLG vs. enterprise sales-led? Average sales cycle length?
- Churn rate — especially post-BSL license change (did any customers leave?)
- Revenue growth rate YoY
- Path to profitability or current EBITDA margin

---

## 4. Technical Architecture — Deep Dive

### 4.1 Storage Engine

| Component | Detail | Implication for Domyn |
|---|---|---|
| Engine | **RocksDB** (LSM-tree) | Write-optimized, not read-optimized. Write amplification on graph workloads (many small updates) is a known concern. DomynGraph uses CSR + columnar — fundamentally different. |
| Graph model | Edges stored as JSON documents with `_from`/`_to` attributes. Adjacency resolved via index lookups, NOT native index-free adjacency. | Every hop requires an index lookup into RocksDB. Neo4j has index-free adjacency (pointer chase). DomynGraph has CSR O(1) neighbor access. Arango is the slowest of the three architectures for multi-hop traversals. |
| Memory model | "Mostly in-memory" — indexes built at startup, data via mmap + RocksDB block cache | GC-free (C++), but RocksDB's memory management (block cache, memtables, bloom filters) is complex to tune. |
| Persistence | WAL via RocksDB. Snapshots via hot backup (Enterprise only). | Hot backup is Enterprise-only, which affects the acquisition value split. |
| Multi-model | Same RocksDB instance serves documents, graphs, key-value | The engine cannot be optimized purely for graph workloads without regressing document/KV performance. This is a structural ceiling. |

**[ASK]:**
- What's the p50/p99 latency for 2-hop and 4-hop traversals at 10M vertices?
- What's the write amplification ratio on graph-heavy workloads?
- Have they benchmarked against DomynGraph/Neo4j on the same dataset? Can we run our B1-B10 harness against their engine?
- Is there internal discussion about replacing RocksDB with a graph-native storage layer?

### 4.2 Query Language (AQL)

| Capability | AQL | openCypher / GQL | DomynGraph |
|---|---|---|---|
| Pattern matching | `FOR v, e IN 1..3 OUTBOUND start GRAPH 'g'` (imperative, requires start vertex) | `MATCH (a)-[r*1..3]->(b)` (declarative, no start vertex needed) | openCypher, 96.1% TCK |
| Bidirectional traversal | Supported via `ANY` direction keyword | Native `MATCH (a)--(b)` | Native |
| Variable-length paths | `IN min..max` syntax | `[*1..3]` with edge-uniqueness | Supported |
| Optional match | Not native — requires subquery workarounds | `OPTIONAL MATCH` | Supported |
| Pattern comprehensions | Not supported | `[(a)-->(b) WHERE b.x > 0 | b.name]` | Supported |
| Quantifier predicates | Not supported | `ALL(x IN list WHERE ...)` | Supported (596/596 TCK) |
| Aggregations | Supported via `COLLECT` | `WITH ... GROUP BY` / aggregate functions | Supported |
| Subqueries | Supported (verbose) | Supported | Supported |
| GQL roadmap | "Participating in discussions" — no timeline, openCypher MATCH rejected as "Won't Fix" (2020) | ISO standard (2024) | Already implementing |

**Critical risk**: AQL is a proprietary query language with no industry adoption outside ArangoDB. Acquiring Arango means maintaining AQL alongside openCypher indefinitely, or migrating all Arango customers to openCypher (massive effort, high churn risk). There is no middle path — AQL and openCypher are syntactically incompatible.

**[ASK]:**
- What % of customers use graph features vs. document-only?
- Is there any internal GQL/openCypher prototype or feasibility study?
- What's the AQL parser/executor architecture — could a Cypher frontend be bolted on without rewriting the storage layer?
- How do GraphRAG customers generate AQL today — LLM-generated, or fixed templates?

### 4.3 Vector Search

| Attribute | ArangoDB | DomynGraph |
|---|---|---|
| Library | **Faiss** (Facebook AI Similarity Search) | HNSW (native, planned) |
| Index type | **IVF** (Inverted File) with optional HNSW layers. Factory string: `"IVF100_HNSW10,Flat"` | HNSW (brute-force baseline today, native HNSW on roadmap) |
| Similarity | Cosine, L2, Inner Product (since 3.12.6) | Cosine (planned: all standard metrics) |
| Dimensions | Configurable per index | Configurable |
| Quantization | Not documented | int8, PQ planned |
| Pre-filtering | **Not supported** — vector index cannot filter on other fields before search. Major limitation for multi-tenant (GitHub issue #21690). | Planned: integrated with graph queries |
| In-process? | Same process as the database | Same process |
| Maturity | Introduced 3.12.4 (mid-2024). IVF requires training phase; recall degrades if data distribution shifts post-training. | Brute-force baseline today; HNSW on roadmap |

**Critical issue**: The vector index **cannot pre-filter** by tenant or other attributes before search. For FinReflectKG with 18 tickers, this means vector search returns results across all tenants, requiring post-filtering that degrades recall. This is a known open issue (#21690) with no resolution.

**[ASK]:**
- What's the recall@10 at 10M vectors, 1024 dimensions?
- Is there a roadmap for pre-filtered vector search?
- Can the Faiss index be swapped for a different implementation (HNSW, ScaNN) without major refactoring?

### 4.4 Distributed / Clustering

| Feature | Detail |
|---|---|
| Sharding | Hash-based. Graph traversals cross shards = network hops. |
| SmartGraphs | Enterprise-only. Requires `smartGraphAttribute` on ALL vertices (string). Co-locates vertices with same attribute. Cannot overlap — one collection can only belong to one SmartGraph. |
| OneShard | Recommended for most graph workloads — puts everything on one DB-Server. Defeats the purpose of horizontal scale for large graphs. |
| Replication | Synchronous replication across followers. Write latency increases with replication factor. |
| Coordinator | Stateless query routers. Graph traversals are coordinator-heavy — intermediate results shipped back for assembly. |

**The honest assessment**: ArangoDB's distributed story for graphs is weak. SmartGraphs are a workaround for the fact that graph traversals don't shard well. OneShard is the recommended path for most graph workloads, which means vertical scale (bigger machine), not horizontal scale. This is the same limitation as single-node Neo4j Community, but dressed up as a cluster feature.

**[ASK]:**
- What % of production deployments use SmartGraphs vs. OneShard vs. single-node?
- At FinReflectKG scale (10M V / 17.5M E), what's the recommended deployment topology?
- Has anyone successfully run a graph workload across multiple shards without SmartGraphs? What was the traversal performance?

### 4.5 Embeddability

**ArangoDB cannot be embedded in-process.** It is always a separate server process connected to via HTTP REST API. There is no in-process library mode (unlike SQLite, DuckDB, or DomynGraph's PyO3 SDK).

The Python driver (`python-arango`) communicates over HTTP. Every query involves a network round-trip, serialization/deserialization, and REST overhead.

This is a fundamental architectural difference from DomynGraph, which can be imported into a Python process and queried with zero network overhead.

---

## 5. Product: Arango Contextual Data Platform 4.0

### 5.1 Agentic AI Suite (March 2026)

| Service | What it does | Maturity |
|---|---|---|
| **AutoGraph** | Automated knowledge graph construction from unstructured data | New (4.0) |
| **AQLizer** | Natural language → AQL query generation | New (4.0) |
| **GraphRAG** | Entity extraction → KG construction → NL query interface | Launched Oct 2025 |
| **VectorRAG** | Vector similarity retrieval | Launched Oct 2025 |
| **HybridRAG** | Combined graph + vector retrieval | Launched Oct 2025 |
| **ContextRAG** | Lexical + semantic + graph retrieval combined | New (4.0) |
| **AutoRAG** | Automatic retrieval strategy selection | New (4.0) |
| **Reasoner** | AI-powered query optimization | New (4.0) |
| **Ada** | AI digital assistant | New (4.0) |
| **GraphML** | ML on graphs (embeddings, link prediction) | Existing |
| **Graph Analytics** | PageRank, community detection, etc. | Existing |

**Assessment**: This is a marketing-heavy product launch. 20+ "AI services" in a single release is a red flag for depth. AutoGraph, AQLizer, Reasoner, and Ada are almost certainly thin LLM wrappers. The substantive pieces are GraphRAG/VectorRAG/HybridRAG, which are the same pipeline pattern we built on JanusGraph (LLM → query generation → retrieval → context → answer).

**[ASK]:**
- Which of these 20+ services have paying customers today?
- What LLM(s) power AQLizer/Reasoner/Ada? Are they bundled or BYOLLM?
- Is AutoGraph a production-quality entity extraction pipeline, or a demo?
- How does GraphRAG handle multi-tenant retrieval given the vector pre-filtering limitation?
- Show us a real customer deployment of the Agentic AI Suite — not a demo.

### 5.2 Cloud Platform (ArangoGraph / AMP)

| Attribute | Detail |
|---|---|
| Architecture | Kubernetes-based. Control plane (30+ microservices) + data clusters per region. |
| Cloud providers | AWS, GCP, Azure (GA since 2020) |
| Operator | `kube-arangodb` — open-source Kubernetes operator |
| Regions | Multiple per cloud provider |
| Pricing | "Request pricing" for production; developer tier ~$42/month (2021) |
| HA | Synchronous replication, automated failover |
| Monitoring | Built-in metrics, alerting |

**[ASK]:**
- How many production customers are on ArangoGraph vs. self-hosted?
- What's the cloud ARR vs. self-hosted license ARR?
- What's the control plane operational burden? How many people run it?
- What's the average monthly bill for a production ArangoGraph customer?
- SLA: what's the committed uptime? Compensation for downtime?
- Could the Kubernetes operator + control plane be repurposed to deploy DomynGraph instead of ArangoDB underneath?

---

## 6. Known Problems & Risks

### 6.1 Technical Issues (from GitHub issues, 2024-2025)

| Issue | Severity | Detail |
|---|---|---|
| **Query stalls** (#21190) | Critical | ArangoDB enters unresponsive state, scheduler queue overflow. Restart provides 2 min relief. Requires thread tuning workaround. |
| **RocksDB data corruption** (#20841) | Critical | v3.12.0 upgrade caused "Compaction sees out-of-order keys" due to ICU upgrade side effects. Recurred after patch for some users. |
| **Enterprise slower than Community** (#21459) | High | Enterprise graph traversals slower than Community on SmartGraphs with parallel execution enabled. |
| **Vector pre-filter impossible** (#21690) | High | Vector index cannot filter by tenant/workspace before search. Breaks multi-tenant vector retrieval. |
| **Web UI regression** (#21566) | Medium | New 3.12 UI has undersized results area, no resize capability. |
| **Declining commit activity** | Medium | OpenHub shows decreasing year-over-year commits. May indicate resource constraints or maturity plateau. |

### 6.2 Strategic Risks

| Risk | Assessment |
|---|---|
| **AQL lock-in** | Proprietary query language with no ecosystem outside Arango. No openCypher/GQL support and no credible timeline to add it. Acquiring this means maintaining a dead-end query language or migrating customers (high churn risk). |
| **BSL license change** | Switched from Apache 2.0 to BSL 1.1 in Feb 2024. Community Edition capped at 100 GiB, non-commercial. This likely alienated open-source community contributors and hobbyist adoption pipeline. |
| **Multi-model ceiling** | The engine serves documents, graphs, and key-value through the same RocksDB instance. Graph-specific optimizations (CSR adjacency, graph-aware caching, traversal-specific memory layout) cannot be added without breaking the document/KV model. |
| **Not embeddable** | Server-only deployment. Cannot be used as an in-process library. This is orthogonal to DomynGraph's core value proposition (PyO3 SDK, zero-copy in-process queries). |
| **Vector search immaturity** | IVF-based (requires training, degrades on distribution shift), no pre-filtering, introduced mid-2024. Not competitive with Neo4j's native HNSW or purpose-built vector DBs. |
| **Cloud concentration risk** | "200+ production environments" with unknown revenue concentration. If top 5 customers represent >50% of revenue, churn from any one is existential. |

---

## 7. Strategic Fit Analysis — What Would Domyn Actually Acquire?

### 7.1 The Four Possible Acquisition Theses

| Thesis | What you get | What you lose | Verdict |
|---|---|---|---|
| **A. Acqui-hire** | ~40-60 C++ engineers, some with deep database internals experience | Pay acquisition price for talent you could recruit directly | Expensive acqui-hire at any price >$20M |
| **B. Cloud platform** | Kubernetes operator, control plane (30+ microservices), AWS/GCP/Azure presence, existing customers | Coupled to ArangoDB engine; repurposing for DomynGraph is a major integration effort | Interesting if cloud ARR is significant and platform is decoupled enough |
| **C. Customer base** | 200+ production deployments, enterprise relationships, sales pipeline | Customers chose ArangoDB for AQL + multi-model, not openCypher + graph-native. Migration = churn. | High risk — customer fit is unclear |
| **D. Engine IP** | 7.5M LOC C++ database, Faiss vector integration, RocksDB tuning, SmartGraph sharding | RocksDB-based, not CSR-native. AQL, not Cypher. Multi-model, not graph-first. Architecturally opposite to DomynGraph. | Low value — DomynGraph's architecture is already better for graph workloads |

### 7.2 The Core Tension

DomynGraph is built on a thesis: **purpose-built beats general-purpose for graph + AI workloads**. The entire slide deck, benchmark suite, and competitive analysis argues that Neo4j's JVM overhead, JanusGraph's Cassandra complexity, and AGE's PostgreSQL limitations all stem from not being purpose-built.

ArangoDB is the most general-purpose option on the market — documents + graphs + key-value + now vectors + now AI services, all in one engine. Acquiring it would **directly contradict the technical narrative** that justified building DomynGraph in the first place.

The acquisition only makes sense if:
1. The cloud platform and customer base are valuable enough to justify maintaining two engines during a multi-year migration, AND
2. The customer base is graph-heavy (not document-heavy), AND
3. The price reflects the risks (BSL license change, AQL lock-in, vector immaturity, declining commit velocity)

### 7.3 Key Questions to Resolve the Thesis — [ASK IN THE MEETING]

**The five questions that determine whether to proceed:**

1. **"What percentage of your revenue comes from customers using graph features as their primary workload?"** — If <30%, the customer base doesn't transfer to a graph-first product.

2. **"What's your current ARR, and what's the cloud vs. self-hosted split?"** — If cloud ARR is <$3M, the managed platform isn't worth the integration cost.

3. **"What's your burn rate and runway?"** — Determines negotiating leverage. If runway is <12 months, this is a distressed acquisition and price should reflect that.

4. **"Have you explored adding openCypher or GQL support? What's the technical feasibility?"** — If the answer is "not feasible without rewriting the query engine," then AQL is a permanent liability.

5. **"Can we run our benchmark harness (B1-B10, same dataset: 47.5K V / 64.9K E) against your engine?"** — Real numbers, not claims. If ArangoDB can't match Neo4j on graph traversals, the engine has negative value for our use case.

---

## 8. Competitor Context — Where Arango Sits

| Database | Graph focus | License | Vector | Embeddable | Query | Market position |
|---|---|---|---|---|---|---|
| **Neo4j** | High (native) | GPL/Commercial | HNSW (native) | No | Cypher | Market leader (#1) |
| **ArangoDB** | Medium (multi-model) | BSL 1.1 | Faiss IVF | No | AQL (proprietary) | #2-4, repositioning as "AI data platform" |
| **TigerGraph** | High (native) | Proprietary | Unknown | No | GSQL (proprietary) | Enterprise analytics focus |
| **NebulaGraph** | High (distributed) | Apache 2.0 | No | No | nGQL / openCypher | Strong in Asia-Pacific |
| **FalkorDB** | High (Redis-based) | SSPL | HNSW (native) | Partial (sub-process) | Cypher subset | Small, fast, GraphRAG SDK |
| **DomynGraph** | High (purpose-built) | Internal | HNSW (planned) | Yes (PyO3) | openCypher (96.1% TCK) | Pre-production, best benchmarks |

**If the goal is to acquire a competitive position in the graph database market, ArangoDB is not the strongest target.** NebulaGraph (Apache 2.0, distributed, openCypher) or FalkorDB (fast, GraphRAG SDK, Cypher) would be more architecturally aligned. ArangoDB's value is in its brand recognition, customer base, and managed cloud — not its graph engine.

---

## 9. Recommended Diligence Process

| Phase | Duration | Activities |
|---|---|---|
| **1. Information request** | 1 week | Send structured data request: ARR, customer list (anonymized), cap table, P&L, engineering org chart, architecture docs |
| **2. Technical evaluation** | 2 weeks | Run B1-B10 benchmark harness against ArangoDB. Evaluate AQL → Cypher feasibility. Assess cloud platform decoupling. |
| **3. Customer interviews** | 2 weeks | Talk to 5-10 Arango customers (ideally graph-heavy). Understand switching costs, satisfaction, and willingness to migrate to openCypher. |
| **4. Financial modeling** | 1 week | Build acquisition model: price scenarios, integration cost, customer retention assumptions, headcount plan |
| **5. Decision** | 1 week | Go/no-go based on findings |

---

*All public data sourced from: arango.ai, GitHub (arangodb/arangodb), neo4j.com/pricing, CBInsights, Tracxn, Vendr, OpenHub, and ArangoDB documentation. Pricing and metrics are as of April-May 2026. Private data marked [ASK] must be obtained directly from ArangoDB.*
