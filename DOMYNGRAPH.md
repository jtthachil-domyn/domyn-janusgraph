# DomynGraph

**An extensible graph database engine built on JanusGraph.**

DomynGraph is not a wrapper around JanusGraph — it is a custom graph database distribution that ships JanusGraph with a procedure engine (like Neo4j's APOC), a graph algorithm engine (like Neo4j's GDS), and a native multi-tenancy system. It is a single codebase, a single build, a single distribution.

---

## 1. What is DomynGraph

### Problem

Existing graph databases are either:

- **Closed** — Neo4j Enterprise requires licensing and restricts extensibility
- **Limited** — Memgraph, TigerGraph lack the ecosystem and extensibility
- **Raw** — JanusGraph alone has no procedure layer, no algorithm library, no tenancy system

### Solution

DomynGraph = JanusGraph + three engine modules:

| Capability | Neo4j Equivalent | DomynGraph Module |
|---|---|---|
| Graph storage + traversal | Neo4j Core | JanusGraph Core (Cassandra + Gremlin) |
| Reusable procedures | APOC | `domyngraph-procedures` |
| Graph algorithms | GDS | `domyngraph-algorithms` |
| Multi-tenancy | Enterprise (limited) | `domyngraph-tenant` |
| Indexing | Built-in Lucene | Elasticsearch (explicit mappings) |

---

## 2. System Architecture


```mermaid
graph TB
    subgraph ExternalLayer ["External Consumers"]
        API["Your GraphRAG / APIs"]
    end

    subgraph DomynEngine ["DomynGraph Engine - New Modules"]
        Procedures["domyngraph-procedures<br/>(APOC + Registry)"]
        Algorithms["domyngraph-algorithms<br/>(GDS + ResourceMgr)"]
        Tenant["domyngraph-tenant<br/>(Multi-tenant + Schema Versioning)"]
    end

    subgraph JanusCore ["JanusGraph Core - Existing"]
        Core["janusgraph-core<br/>(Traversal, Schema, OLAP)"]
        Server["janusgraph-server<br/>(Gremlin Server - OLTP only)"]
        CQL["janusgraph-cql<br/>(Cassandra Backend)"]
        ES["janusgraph-es<br/>(Elasticsearch Index)"]
        All["janusgraph-all<br/>(Aggregator JAR)"]
    end

    subgraph Infrastructure ["Infrastructure Layer"]
        Cassandra["Cassandra 4.1<br/>(Tuned: tokens, concurrency)"]
        Elasticsearch["Elasticsearch 8.12<br/>(Explicit field mappings)"]
    end

    API --> Procedures
    API --> Algorithms
    API --> Tenant
    Procedures --> Core
    Algorithms --> Core
    Tenant --> Core
    All --> Procedures
    All --> Algorithms
    All --> Tenant
    Core --> CQL
    Core --> ES
    CQL --> Cassandra
    ES --> Elasticsearch
```

### Key Design Decisions

- **Cassandra** for storage: horizontally scalable, no single point of failure
- **Elasticsearch** for indexing: full-text search, explicit field mappings per property
- **Gremlin Server** for OLTP queries: 30s hard timeout, fixed thread pools
- **FulgoraGraphComputer** for OLAP algorithms: separate execution path, resource-controlled

---

## 3. Execution Model

### OLTP vs OLAP Separation

```mermaid
graph LR
    subgraph ClientLayer ["Client Request"]
        Client["GraphRAG / API Client"]
    end

    subgraph OLTPPath ["OLTP Path (30s timeout)"]
        GremlinServer["Gremlin Server<br/>threadPoolWorker: 8<br/>gremlinPool: 16"]
        ProcRegistry["ProcedureRegistry"]
        TenantMgr["TenantManager"]
    end

    subgraph OLAPPath ["OLAP Path (5min default)"]
        AlgoMgr["AlgorithmResourceManager"]
        FulgoraGC["FulgoraGraphComputer<br/>(separate thread pool)"]
        Timeout["Future.get with timeout<br/>cancel on expiry"]
    end

    subgraph Storage ["Storage Layer"]
        CassandraStore["Cassandra"]
        ESStore["Elasticsearch"]
    end

    Client -->|"traversal / procedure call"| GremlinServer
    Client -->|"algorithm execution"| AlgoMgr
    GremlinServer --> ProcRegistry
    GremlinServer --> TenantMgr
    AlgoMgr --> FulgoraGC
    AlgoMgr --> Timeout
    GremlinServer --> CassandraStore
    GremlinServer --> ESStore
    FulgoraGC --> CassandraStore
```

### OLTP (Online Transaction Processing)

- **Engine:** Gremlin Server
- **Operations:** traversals, procedure calls, tenant operations, CRUD
- **Timeout:** 30 seconds (hard limit in `gremlin-server-domyngraph.yaml`)
- **Thread pool:** Fixed (`threadPoolWorker: 8`, `gremlinPool: 16`)
- **Access:** WebSocket on port 8182

### OLAP (Online Analytical Processing)

- **Engine:** FulgoraGraphComputer (JanusGraph's GraphComputer implementation)
- **Operations:** PageRank, shortest distance, connected components, BFS
- **Timeout:** Configurable per algorithm (default 5 minutes)
- **Thread pool:** Separate from Gremlin Server, controlled by `AlgorithmResourceManager`
- **Access:** Programmatic via `AlgorithmResourceManager.execute()`

**These two execution paths are architecturally separated.** An algorithm cannot starve Gremlin Server, and a slow query cannot block algorithm execution.

---

## 4. Module Breakdown

### Module Dependency Graph

```mermaid
graph TD
    subgraph DomynModules ["DomynGraph Modules"]
        Proc["domyngraph-procedures"]
        Algo["domyngraph-algorithms"]
        Ten["domyngraph-tenant"]
    end

    subgraph JanusCoreModules ["JanusGraph Core"]
        JCore["janusgraph-core"]
        JDriver["janusgraph-driver"]
        JAll["janusgraph-all"]
    end

    subgraph TinkerPop ["Apache TinkerPop 3.7.3"]
        GremlinCore["gremlin-core"]
        GremlinServer["gremlin-server"]
    end

    Proc --> JCore
    Algo --> JCore
    Ten --> JCore
    JAll --> Proc
    JAll --> Algo
    JAll --> Ten
    JCore --> JDriver
    JCore --> GremlinCore
    GremlinServer --> GremlinCore
```

### `domyngraph-procedures` — APOC Equivalent

**Purpose:** Reusable, registered, introspectable graph procedures.

```mermaid
classDiagram
    class DomynProcedure {
        <<interface>>
        +name() String
        +definition() ProcedureDefinition
        +execute(ProcedureContext, Map) Object
    }

    class ProcedureContext {
        -tenantId : String
        -graph : JanusGraph
        -g : GraphTraversalSource
        -requestId : String
        -startTimeMs : long
        +create(graph, tenantId) ProcedureContext
        +logStart(procedureName)
        +logEnd(procedureName, success)
        +getElapsedMs() long
    }

    class ProcedureRegistry {
        -procedures : ConcurrentHashMap
        +getInstance() ProcedureRegistry
        +register(DomynProcedure)
        +get(name) DomynProcedure
        +listNames() List
        +introspect(name) Map
        +call(name, ctx, args) Object
    }

    class ProcedureDefinition {
        -name : String
        -version : String
        -inputs : List~ParameterDef~
        -outputType : String
        -deterministic : boolean
        +toMap() Map
    }

    class PathProcedures {
        +kHop(g, name, hops, edgeLabel, maxPaths) List
        +shortestPath(g, from, to, maxDepth) List
        +neighbors(g, name, depth) List
    }

    class ExternalIdProcedures {
        +getByExternalId(g, id) Optional
        +assignExternalId(vertex) String
    }

    class SchemaProcedures {
        +indexStatus(graph) Map
        +awaitIndex(graph, indexName, timeout)
    }

    DomynProcedure <|.. PathProcedures
    DomynProcedure <|.. ExternalIdProcedures
    DomynProcedure <|.. SchemaProcedures
    ProcedureRegistry --> DomynProcedure
    ProcedureRegistry --> ProcedureContext
    DomynProcedure --> ProcedureDefinition
```

### `domyngraph-algorithms` — GDS Equivalent

**Purpose:** Resource-controlled graph algorithms running on GraphComputer.

```mermaid
classDiagram
    class AlgorithmResourceManager {
        +getInstance() AlgorithmResourceManager
        +execute(graph, program, config) AlgorithmResult
        -validateResources(config)
    }

    class AlgorithmConfig {
        -maxIterations : int
        -timeoutMs : long
        -memoryLimitMb : long
        -workerThreads : int
        +defaults() AlgorithmConfig
        +builder() Builder
    }

    class AlgorithmResult~T~ {
        -status : Status
        -result : T
        -elapsedMs : long
        -algorithmName : String
        +isSuccess() boolean
        +getResult() T
        +toMap() Map
    }

    class DomynPageRankVertexProgram {
        +PAGE_RANK : String
        +build() Builder
        -dampingFactor : double
        -convergenceThreshold : double
    }

    class DomynShortestDistanceVertexProgram {
        +DISTANCE : String
        +build() Builder
        -weightProperty : String
    }

    class ConnectedComponentsVertexProgram {
        +COMPONENT : String
        +build() Builder
    }

    class BFSVertexProgram {
        +DEPTH : String
        +build() Builder
        -seed : long
    }

    AlgorithmResourceManager --> AlgorithmConfig
    AlgorithmResourceManager --> AlgorithmResult
    AlgorithmResourceManager --> DomynPageRankVertexProgram
    AlgorithmResourceManager --> DomynShortestDistanceVertexProgram
    AlgorithmResourceManager --> ConnectedComponentsVertexProgram
    AlgorithmResourceManager --> BFSVertexProgram
```

### Algorithm Execution Flow

```mermaid
sequenceDiagram
    participant Client
    participant ARM as AlgorithmResourceManager
    participant Config as AlgorithmConfig
    participant GC as FulgoraGraphComputer
    participant Future as Future~ComputerResult~

    Client->>ARM: execute(graph, program, config)
    ARM->>ARM: validateResources(config)
    ARM->>ARM: log ALGO_START
    ARM->>GC: graph.compute().workers(n).program(vp)
    GC->>Future: submit()
    ARM->>Future: get(timeoutMs, MILLISECONDS)

    alt Success
        Future-->>ARM: ComputerResult
        ARM->>ARM: log ALGO_END SUCCESS
        ARM-->>Client: AlgorithmResult.success(result)
    else Timeout
        Future-->>ARM: TimeoutException
        ARM->>Future: cancel(true)
        ARM->>ARM: log ALGO_END TIMEOUT
        ARM-->>Client: AlgorithmResult.timeout()
    else Error
        Future-->>ARM: ExecutionException
        ARM->>ARM: log ALGO_END ERROR
        ARM-->>Client: AlgorithmResult.error(msg)
    end
```

### `domyngraph-tenant` — Multi-Tenancy Engine

**Purpose:** DB-level tenant isolation, schema versioning, migration.

```mermaid
classDiagram
    class TenantManager {
        -strategy : TenantIsolationStrategy
        -migrationManager : SchemaMigrationManager
        +createTenant(tenantId) JanusGraph
        +openTenant(tenantId) JanusGraph
        +traversal(tenantId) GraphTraversalSource
        +closeTenant(tenantId)
        +dropTenant(tenantId)
        +listTenants() Set
    }

    class TenantIsolationStrategy {
        <<enumeration>>
        KEYSPACE_PER_TENANT
        SHARED_GRAPH
    }

    class TenantSchemaInitializer {
        +SCHEMA_VERSION : int
        +initialize(graph)
    }

    class SchemaMigrationManager {
        -migrations : List~SchemaMigration~
        +getCurrentVersion(graph) int
        +migrateTo(graph, targetVersion)
        +migrateToLatest(graph)
    }

    class SchemaMigration {
        <<interface>>
        +version() int
        +description() String
        +apply(graph)
    }

    class TenantAwareTraversalSource {
        -tenantId : String
        +V() GraphTraversal
        +E() GraphTraversal
        +addVertex(label) Vertex
    }

    TenantManager --> TenantIsolationStrategy
    TenantManager --> SchemaMigrationManager
    TenantManager --> TenantSchemaInitializer
    TenantManager --> TenantAwareTraversalSource
    SchemaMigrationManager --> SchemaMigration
```

---

## 5. Data Model

### Graph Schema Overview

```mermaid
graph LR
    subgraph VertexLabels ["Vertex Labels"]
        Doc["Document"]
        Chunk["Chunk"]
        Entity["Entity"]
        Concept["Concept"]
    end

    Doc -->|CONTAINS| Chunk
    Chunk -->|CONTAINS| Entity
    Entity -->|RELATION| Entity
    Entity -->|REFERENCES| Doc
    Entity -->|SIMILAR_TO| Entity
    Concept -->|REFERENCES| Entity
    Doc -->|REFERENCES| Doc
```

### Vertex Labels

| Label | Purpose |
|---|---|
| `Entity` | Named entities (people, companies, concepts) |
| `Chunk` | Text chunks from documents |
| `Document` | Source documents |
| `Concept` | Abstract concepts / topics |

### Edge Labels

| Label | Multiplicity | Purpose |
|---|---|---|
| `RELATION` | MULTI | General relationships between entities |
| `CONTAINS` | MULTI | Document contains chunk, chunk contains entity |
| `REFERENCES` | MULTI | Cross-references between entities/documents |
| `SIMILAR_TO` | MULTI | Similarity edges (from embeddings, algorithms) |

### Property Keys

| Property | Type | Indexed | Purpose |
|---|---|---|---|
| `name` | String | Composite + Text (ES) | Primary name of the vertex |
| `type` | String | Composite + Keyword (ES) | Subtype classification |
| `tenant_id` | String | Composite + Keyword (ES) | Tenant isolation key |
| `external_id` | String | Composite (unique) + Keyword (ES) | External system integration key |
| `created_at` | Long | ES (sortable) | Creation timestamp |
| `description` | String | Text (ES) | Full-text searchable description |
| `weight` | Double | — | Edge weight for algorithms |
| `embedding` | byte[] | — | Vector embedding (stored, not indexed) |
| `metadata` | String | — | JSON metadata blob |

---

## 6. Indexing Strategy

### Dual Index Architecture

```mermaid
graph TB
    subgraph Query ["Query Types"]
        ExactLookup["Exact Lookup<br/>(get by ID, filter by type)"]
        FullText["Full-Text Search<br/>(search by name, description)"]
        RangeQuery["Range Query<br/>(filter by created_at)"]
    end

    subgraph CompositeIdx ["Composite Indexes (Cassandra)"]
        byExtId["byExternalId (unique)"]
        byTenant["byTenantId"]
        byName["byName"]
        byType["byType"]
        byTenantType["byTenantAndType"]
    end

    subgraph MixedIdx ["Mixed Index (Elasticsearch)"]
        searchIdx["search index"]
        nameText["name: TEXT (analyzed)"]
        typeKw["type: STRING (keyword)"]
        tenantKw["tenant_id: STRING (keyword)"]
        descText["description: TEXT (analyzed)"]
        createdDef["created_at: DEFAULT (long)"]
    end

    ExactLookup --> CompositeIdx
    FullText --> MixedIdx
    RangeQuery --> MixedIdx
    searchIdx --> nameText
    searchIdx --> typeKw
    searchIdx --> tenantKw
    searchIdx --> descText
    searchIdx --> createdDef
```

### Index Lifecycle

```mermaid
stateDiagram-v2
    [*] --> INSTALLED : mgmt.buildIndex().buildCompositeIndex()
    INSTALLED --> REGISTERED : mgmt.commit()
    REGISTERED --> ENABLED : SchemaProcedures.awaitIndex()

    state ENABLED {
        [*] --> QueryReady
        QueryReady --> Reindexing : SchemaAction.REINDEX
        Reindexing --> QueryReady : reindex complete
    }

    REGISTERED --> ENABLED : awaitIndex triggers REINDEX
```

JanusGraph indexes transition through states: `INSTALLED` -> `REGISTERED` -> `ENABLED`. Use `SchemaProcedures.awaitIndex()` to manage this lifecycle — it waits for `REGISTERED`, triggers `REINDEX`, then waits for `ENABLED`.

---

## 7. Multi-Tenancy Model

### Isolation Strategy Comparison

```mermaid
graph TB
    subgraph KeyspaceMode ["KEYSPACE_PER_TENANT (Production)"]
        direction TB
        TenantA_KS["Tenant A"]
        TenantB_KS["Tenant B"]
        KS_A["Cassandra Keyspace: domyn_tenant_a"]
        KS_B["Cassandra Keyspace: domyn_tenant_b"]
        ES_A["ES Index: domyn_tenant_a"]
        ES_B["ES Index: domyn_tenant_b"]

        TenantA_KS --> KS_A
        TenantA_KS --> ES_A
        TenantB_KS --> KS_B
        TenantB_KS --> ES_B
    end

    subgraph SharedMode ["SHARED_GRAPH (Development)"]
        direction TB
        TenantA_SH["Tenant A"]
        TenantB_SH["Tenant B"]
        SharedKS["Shared Keyspace: domyn_shared"]
        SharedES["Shared ES Index: domyn_shared"]
        Filter["TenantAwareTraversalSource<br/>auto-injects has tenant_id"]

        TenantA_SH --> Filter
        TenantB_SH --> Filter
        Filter --> SharedKS
        Filter --> SharedES
    end
```

### Tenant Lifecycle Flow

```mermaid
sequenceDiagram
    participant Client
    participant TM as TenantManager
    participant CGF as ConfiguredGraphFactory
    participant TSI as TenantSchemaInitializer
    participant SMM as SchemaMigrationManager

    Client->>TM: createTenant("acme_corp")
    TM->>TM: validateTenantId()
    TM->>CGF: create("domyn_acme_corp")
    CGF-->>TM: JanusGraph instance
    TM->>TSI: initialize(graph)
    TSI->>TSI: create vertex labels
    TSI->>TSI: create edge labels
    TSI->>TSI: create property keys
    TSI->>TSI: create composite indexes
    TSI->>TSI: create mixed index (ES)
    TSI->>TSI: create __schema_meta vertex (v1)
    TM->>SMM: migrateToLatest(graph)
    SMM->>SMM: getCurrentVersion() = 1
    SMM->>SMM: no migrations needed
    TM-->>Client: JanusGraph (ready)

    Note over Client,SMM: Later, opening an existing tenant...

    Client->>TM: openTenant("acme_corp")
    TM->>CGF: open("domyn_acme_corp")
    CGF-->>TM: JanusGraph instance
    TM->>SMM: migrateToLatest(graph)
    SMM->>SMM: getCurrentVersion() = 1
    SMM->>SMM: apply v2, v3... if registered
    TM-->>Client: JanusGraph (migrated)
```

### KEYSPACE_PER_TENANT (recommended for production)

- Each tenant gets its own Cassandra keyspace (`domyn_<tenantId>`)
- Each tenant gets its own Elasticsearch index
- Complete physical isolation — no data can leak between tenants
- Higher resource cost (one graph instance per tenant)
- Uses `ConfiguredGraphFactory` under the hood

### SHARED_GRAPH (lightweight, development/small deployments)

- All tenants share one keyspace and one ES index
- Isolation enforced by `tenant_id` property on every vertex/edge
- `TenantAwareTraversalSource` auto-injects `has('tenant_id', tenantId)`
- Lower cost, but requires discipline — every query must filter by tenant

---

## 8. Procedure Execution Flow

```mermaid
sequenceDiagram
    participant Client
    participant Registry as ProcedureRegistry
    participant Ctx as ProcedureContext
    participant Proc as DomynProcedure
    participant Graph as JanusGraph

    Client->>Registry: call("kHop", ctx, args)
    Registry->>Registry: get("kHop")
    Registry->>Ctx: logStart("kHop")
    Ctx->>Ctx: MDC.put(requestId, tenantId)
    Registry->>Proc: execute(ctx, args)
    Proc->>Graph: g.V().has("name", name).repeat(both())...
    Graph-->>Proc: List~Path~
    Proc-->>Registry: result
    Registry->>Ctx: logEnd("kHop", success=true)
    Ctx->>Ctx: MDC.remove()
    Registry-->>Client: List~Path~
```

---

## 9. Docker Deployment Architecture

```mermaid
graph TB
    subgraph DockerCompose ["docker-compose (domyngraph-docker/)"]
        subgraph JG ["JanusGraph Container (port 8182)"]
            GServer["Gremlin Server"]
            DomynProc["DomynProcedurePlugin"]
            DomynAlgo["DomynAlgorithmPlugin"]
            DomynTen["DomynTenantPlugin"]
            InitScript["init-schema.groovy"]
        end

        subgraph Cass ["Cassandra Container (port 9042)"]
            CassDB["Cassandra 4.1<br/>MAX_HEAP_SIZE=2G"]
        end

        subgraph ESC ["Elasticsearch Container (port 9200)"]
            ESNode["Elasticsearch 8.12<br/>single-node, 512m heap"]
        end
    end

    GServer --> CassDB
    GServer --> ESNode
    GServer --> DomynProc
    GServer --> DomynAlgo
    GServer --> DomynTen
    GServer --> InitScript
```

---

## 10. Build & Run

### Prerequisites

- Java 8+ (Java 11 recommended)
- Maven 3.2.5+
- Docker + Docker Compose

### Build

```bash
cd domyn-janusgraph

# Build only DomynGraph modules (fast)
mvn clean install -pl domyngraph-procedures,domyngraph-algorithms,domyngraph-tenant \
    -am -DskipTests -Drat.skip=true -Dcheckstyle.skip=true \
    -Denforcer.skip=true -Dcyclonedx.skip=true

# Build everything
mvn clean install -DskipTests -Drat.skip=true -Dcheckstyle.skip=true \
    -Denforcer.skip=true -Dcyclonedx.skip=true
```

### Run (Docker)

```bash
cd domyngraph-docker
docker-compose up -d

# Verify services
curl http://localhost:9200/_cluster/health  # Elasticsearch
docker exec domyn-cassandra cqlsh -e "describe cluster"  # Cassandra
curl http://localhost:8182                   # JanusGraph / Gremlin Server
```

### Connect (Gremlin Console)

```bash
bin/gremlin.sh
:remote connect tinkerpop.server conf/remote.yaml
:remote console
```

---

## 11. Example Usage

```groovy
// List all registered procedures
DomynProcedures.list()
// => ["getByExternalId", "indexStatus", "kHop"]

// Describe a procedure
DomynProcedures.describe("kHop")
// => {name: "kHop", version: "1.0", inputs: [...], outputType: "path[]", ...}

// Create a tenant
mgr = new TenantManager(TenantIsolationStrategy.KEYSPACE_PER_TENANT)
graph = mgr.createTenant("acme_corp")
g = graph.traversal()

// Add data with external IDs
v1 = g.addV("Entity").property("name", "Tesla").property("type", "Company").next()
ExternalIdProcedures.assignExternalId(v1)

v2 = g.addV("Entity").property("name", "Elon Musk").property("type", "Person").next()
ExternalIdProcedures.assignExternalId(v2)

g.V(v1).addE("RELATION").to(v2).property("weight", 0.95).iterate()
graph.tx().commit()

// k-Hop traversal
PathProcedures.kHop(g, "Tesla", 2, null, 100)

// Lookup by external ID
ExternalIdProcedures.getByExternalId(g, "some-uuid-here")

// Check index status
SchemaProcedures.indexStatus(graph)

// Run PageRank (OLAP — separate execution path)
config = AlgorithmConfig.builder().maxIterations(30).timeoutMs(60000).build()
program = DomynPageRankVertexProgram.build().vertexCount(10000).dampingFactor(0.85).create()
result = AlgorithmResourceManager.getInstance().execute(graph, program, config)
result.isSuccess()     // true
result.getElapsedMs()  // timing in ms
```

---

## 12. Extending DomynGraph

### Adding a Procedure

```java
public class MyProcedure implements DomynProcedure {

    private static final ProcedureDefinition DEF = ProcedureDefinition.builder("myProc")
        .version("1.0")
        .description("Does something useful")
        .addInput(ParameterDef.required("param1", "String", "First param"))
        .outputType("vertex[]")
        .deterministic(true)
        .build();

    @Override public String name() { return "myProc"; }
    @Override public ProcedureDefinition definition() { return DEF; }

    @Override
    public Object execute(ProcedureContext ctx, Map<String, Object> args) {
        return ctx.traversal().V().has("name", args.get("param1")).toList();
    }
}
```

Then register it in `DomynProcedurePlugin.registerBuiltinProcedures()`.

### Adding an Algorithm

```mermaid
graph LR
    A["1. Implement VertexProgram"] --> B["2. Register in DomynAlgorithmPlugin"]
    B --> C["3. Execute via AlgorithmResourceManager"]
    C --> D["4. Handle AlgorithmResult"]
```

1. Implement a `VertexProgram<T>` (see `DomynPageRankVertexProgram` as reference)
2. Always run through `AlgorithmResourceManager.execute()` with an `AlgorithmConfig`
3. Register in `DomynAlgorithmPlugin`

### Adding a Schema Migration

```mermaid
graph LR
    V1["Schema v1<br/>(initial)"] -->|"V2AddTagProperty"| V2["Schema v2<br/>(+tag property)"]
    V2 -->|"V3AddEmbeddingIndex"| V3["Schema v3<br/>(+embedding index)"]
```

```java
public class V2AddTagProperty implements SchemaMigration {
    @Override public int version() { return 2; }
    @Override public String description() { return "Add tag property + index"; }

    @Override
    public void apply(JanusGraph graph) {
        JanusGraphManagement mgmt = graph.openManagement();
        PropertyKey tag = mgmt.makePropertyKey("tag").dataType(String.class).make();
        mgmt.buildIndex("byTag", Vertex.class).addKey(tag).buildCompositeIndex();
        mgmt.commit();
    }
}
```

Then add to `SchemaMigrationManager`:

```java
migrationManager.addMigration(new V2AddTagProperty());
```

---

## 13. Design Principles

1. **Never expose raw Gremlin externally** — all access goes through procedures with `ProcedureContext`
2. **Always use `external_id`** — JanusGraph internal IDs are non-portable; external_id is the integration key
3. **Enforce tenant isolation** — every query in shared mode must filter by `tenant_id`
4. **OLTP is not OLAP** — traversals and algorithms run on separate execution paths with separate resource limits
5. **Schema is versioned** — every graph tracks its schema version; migrations are applied on open
6. **Indexes have lifecycles** — never query an index that isn't ENABLED; use `awaitIndex()` after creation

---

## 14. Project Structure

```mermaid
graph TD
    subgraph Repo ["domyn-janusgraph (fork of JanusGraph)"]
        subgraph NewModules ["DomynGraph Modules (NEW)"]
            DP["domyngraph-procedures/<br/>10 Java sources + SPI"]
            DA["domyngraph-algorithms/<br/>9 Java sources + SPI"]
            DT["domyngraph-tenant/<br/>8 Java sources + SPI"]
            DD["domyngraph-docker/<br/>docker-compose + config"]
        end

        subgraph ExistingModules ["JanusGraph Modules (existing, 2 modified)"]
            RootPom["pom.xml<br/>(+3 module entries)"]
            AllPom["janusgraph-all/pom.xml<br/>(+3 dependency entries)"]
            DistYaml["janusgraph-dist/.../gremlin-server-domyngraph.yaml"]
        end

        README["DOMYNGRAPH.md"]
    end
```

---

## 15. Syncing with Upstream JanusGraph

This is a fork. To pull in the latest JanusGraph changes:

```bash
git fetch upstream
git merge upstream/master
```

DomynGraph modules are additive — they do not modify existing JanusGraph source files (only `pom.xml` and `janusgraph-all/pom.xml` have additions). Merge conflicts should be minimal.

---

## License

Apache License 2.0 (same as JanusGraph)
