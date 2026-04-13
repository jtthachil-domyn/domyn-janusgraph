#!/usr/bin/env python3
# Copyright 2024 DomynGraph Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#      http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""
DomynGraph Engine — Live Demo Script
=====================================
Run this with the cluster up:  docker-compose up -d  (in domyngraph-docker/)
Then:  python3 demo.py

Requires:  pip install gremlinpython
"""

import time
import sys

try:
    from gremlin_python.driver.client import Client
except ImportError:
    print("ERROR: gremlinpython not installed. Run: pip install gremlinpython")
    sys.exit(1)

GREMLIN_URL = "ws://localhost:8182/gremlin"
GRAPH_ALIAS = "graph"

def section(title):
    print(f"\n{'='*70}")
    print(f"  {title}")
    print(f"{'='*70}\n")

def step(label, query):
    print(f"  >> {label}")
    start = time.time()
    result = client.submit(query).all().result()
    elapsed = int((time.time() - start) * 1000)
    print(f"     {result}  ({elapsed}ms)\n")
    return result

# ─────────────────────────────────────────────────────────────────────
#  CONNECT
# ─────────────────────────────────────────────────────────────────────
section("1. CONNECT TO DOMYNGRAPH")

print("  Connecting to Gremlin Server at", GREMLIN_URL)
print("  Alias:", GRAPH_ALIAS)
print()

try:
    client = Client(GREMLIN_URL, GRAPH_ALIAS)
    step("Health check — vertex count", "graph.traversal().V().count()")
except Exception as e:
    print(f"  ERROR: Cannot connect to DomynGraph. Is the cluster running?\n  {e}")
    sys.exit(1)

# ─────────────────────────────────────────────────────────────────────
#  SCHEMA & INDEXES
# ─────────────────────────────────────────────────────────────────────
section("2. SCHEMA — Auto-bootstrapped on Startup")

step("Index status (all should be ENABLED)",
     "org.janusgraph.domyn.procedures.SchemaProcedures.indexStatus(graph)")

step("Schema version",
     'graph.traversal().V().has("__type", "__schema_meta").values("schema_version")')

# ─────────────────────────────────────────────────────────────────────
#  PROCEDURE REGISTRY
# ─────────────────────────────────────────────────────────────────────
section("3. PROCEDURE ENGINE — APOC Equivalent")

step("List all registered procedures",
     "org.janusgraph.domyn.procedures.ProcedureRegistry.getInstance().listNames()")

step("Introspect kHop procedure (full metadata)",
     'org.janusgraph.domyn.procedures.ProcedureRegistry.getInstance().introspect("kHop")')

# ─────────────────────────────────────────────────────────────────────
#  FRESH DATA INGESTION
# ─────────────────────────────────────────────────────────────────────
section("4. DATA INGESTION — Build a Knowledge Graph")

client.submit("""
t = graph.traversal()

// Clean previous demo data
t.V().has('tenant_id', 'demo').drop().iterate()
graph.tx().commit()

// Companies
apple   = t.addV('Entity').property('name','Apple').property('type','Company').property('tenant_id','demo').property('external_id','demo-apple').next()
google  = t.addV('Entity').property('name','Google').property('type','Company').property('tenant_id','demo').property('external_id','demo-google').next()
nvidia  = t.addV('Entity').property('name','NVIDIA').property('type','Company').property('tenant_id','demo').property('external_id','demo-nvidia').next()

// People
tim     = t.addV('Entity').property('name','Tim Cook').property('type','Person').property('tenant_id','demo').property('external_id','demo-tim').next()
sundar  = t.addV('Entity').property('name','Sundar Pichai').property('type','Person').property('tenant_id','demo').property('external_id','demo-sundar').next()
jensen  = t.addV('Entity').property('name','Jensen Huang').property('type','Person').property('tenant_id','demo').property('external_id','demo-jensen').next()

// Concepts
ai      = t.addV('Concept').property('name','Artificial Intelligence').property('type','Technology').property('tenant_id','demo').property('external_id','demo-ai').next()
chips   = t.addV('Concept').property('name','Semiconductors').property('type','Technology').property('tenant_id','demo').property('external_id','demo-chips').next()

// Relationships
t.V(tim).addE('RELATION').to(apple).property('weight', 0.95).iterate()
t.V(sundar).addE('RELATION').to(google).property('weight', 0.95).iterate()
t.V(jensen).addE('RELATION').to(nvidia).property('weight', 0.95).iterate()
t.V(apple).addE('REFERENCES').to(ai).property('weight', 0.7).iterate()
t.V(google).addE('REFERENCES').to(ai).property('weight', 0.9).iterate()
t.V(nvidia).addE('REFERENCES').to(ai).property('weight', 0.95).iterate()
t.V(nvidia).addE('REFERENCES').to(chips).property('weight', 0.9).iterate()
t.V(apple).addE('REFERENCES').to(chips).property('weight', 0.5).iterate()
t.V(apple).addE('SIMILAR_TO').to(google).property('weight', 0.6).iterate()
t.V(google).addE('SIMILAR_TO').to(nvidia).property('weight', 0.7).iterate()

graph.tx().commit()
'Ingested 8 vertices + 10 edges'
""").all().result()
print("  >> Created knowledge graph: 3 companies, 3 people, 2 concepts, 10 relationships\n")

step("Verify demo vertices",
     "graph.traversal().V().has('tenant_id','demo').count()")

# ─────────────────────────────────────────────────────────────────────
#  TRAVERSAL PROCEDURES
# ─────────────────────────────────────────────────────────────────────
section("5. GRAPH TRAVERSAL — Procedures in Action")

step("kHop: Who/what is 1 hop from NVIDIA?",
     'org.janusgraph.domyn.procedures.PathProcedures.neighbors(graph.traversal(), "NVIDIA", 1)')

step("kHop: 2-hop paths from Tim Cook",
     'org.janusgraph.domyn.procedures.PathProcedures.kHop(graph.traversal(), "Tim Cook", 2, null, 10).collect { it.toString() }')

step("Shortest path: Jensen Huang → Apple",
     'org.janusgraph.domyn.procedures.PathProcedures.shortestPath(graph.traversal(), "Jensen Huang", "Apple", 5).collect { it.toString() }')

# ─────────────────────────────────────────────────────────────────────
#  EXTERNAL ID LOOKUP
# ─────────────────────────────────────────────────────────────────────
section("6. EXTERNAL ID — UUID-based Vertex Lookup")

step('Lookup by external_id "demo-nvidia"',
     'v = org.janusgraph.domyn.procedures.ExternalIdProcedures.getByExternalId(graph.traversal(), "demo-nvidia"); v.isPresent() ? v.get().value("name") : "NOT FOUND"')

step('Lookup nonexistent ID',
     'v = org.janusgraph.domyn.procedures.ExternalIdProcedures.getByExternalId(graph.traversal(), "does-not-exist"); v.isPresent() ? v.get().value("name") : "NOT FOUND"')

# ─────────────────────────────────────────────────────────────────────
#  TENANT ISOLATION
# ─────────────────────────────────────────────────────────────────────
section("7. MULTI-TENANCY — Data Isolation")

step('Tenant "demo" vertex count',
     "graph.traversal().V().has('tenant_id','demo').count()")

step('Tenant "t1" vertex count (from earlier tests)',
     "graph.traversal().V().has('tenant_id','t1').count()")

step('TenantAwareTraversalSource — only sees "demo" data',
     'new org.janusgraph.domyn.tenant.TenantAwareTraversalSource(graph.traversal(), "demo").V().values("name").toList()')

# ─────────────────────────────────────────────────────────────────────
#  GRAPH ALGORITHMS (OLAP)
# ─────────────────────────────────────────────────────────────────────
section("8. GRAPH ALGORITHMS — OLAP Engine (separate from OLTP)")

print("  Running BFS from 'NVIDIA' (uses GraphComputer — separate OLAP execution)...\n")
bfs_nvidia = client.submit("""
import org.janusgraph.domyn.algorithms.*

seedId = graph.traversal().V().has('name', 'NVIDIA').id().next()

program = BFSVertexProgram.build().seed((long) seedId).maxDepth(5).create(graph)
config = AlgorithmConfig.builder().timeoutMs(60000).workerThreads(2).memoryLimitMb(512).build()
AlgorithmResourceManager.getInstance().execute(graph, program, config).toMap()
""").all().result()
print(f"  >> BFS from NVIDIA: {bfs_nvidia}\n")

step("BFS depths from NVIDIA (hop distance to every reachable vertex)",
     """graph.traversal().V().has('domyn.bfs.depth').has('tenant_id','demo')
        .project('name','hops_from_nvidia')
        .by('name')
        .by('domyn.bfs.depth')
        .order().by('hops_from_nvidia', org.apache.tinkerpop.gremlin.process.traversal.Order.asc)
        .toList()""")

print("  Running Connected Components...\n")
cc_result = client.submit("""
import org.janusgraph.domyn.algorithms.*

program = ConnectedComponentsVertexProgram.build().maxIterations(20).create(graph)
config = AlgorithmConfig.builder().timeoutMs(60000).workerThreads(2).memoryLimitMb(512).build()
AlgorithmResourceManager.getInstance().execute(graph, program, config).toMap()
""").all().result()
print(f"  >> Connected Components: {cc_result}\n")

step("Component assignments (vertices in the same component are connected)",
     """graph.traversal().V().has('domyn.connectedComponents.component').has('tenant_id','demo')
        .project('name','component')
        .by('name')
        .by('domyn.connectedComponents.component')
        .toList()""")

# ─────────────────────────────────────────────────────────────────────
#  DONE
# ─────────────────────────────────────────────────────────────────────
section("DEMO COMPLETE")

print("""  DomynGraph Engine delivers:

    [1] Procedure Engine      — registered, introspectable, versioned (APOC equivalent)
    [2] Graph Algorithms       — PageRank, BFS, Connected Components, SSSP (GDS equivalent)
    [3] Multi-Tenancy          — per-tenant isolation via tenant_id filtering
    [4] Schema Management      — auto-bootstrap, versioned, 7 indexes all ENABLED
    [5] External ID System     — UUID-based lookup via unique composite index
    [6] OLTP/OLAP Separation   — Gremlin Server (30s) vs GraphComputer (5min)
    [7] Production Docker      — Cassandra 4.1 + Elasticsearch 8.12 + JanusGraph

  Built on JanusGraph. Single codebase. Single distribution.
""")

client.close()
