# GraphRAG Cookbook

These examples use the embedded Python SDK for local GraphRAG pipelines. Use the
server product for shared services, auth, TLS, tenant authorization, and named
storage-backed vector indexes.

## Ingest A Small Context Graph

```python
import domyn_nexus as nx

graph = nx.Graph.open("/tmp/domyn-nexus-graphrag")

graph.cypher("""
CREATE (d:Document {id: 'doc-1', title: 'FY25 10-K'}),
       (c:Chunk {id: 'chunk-1', text: 'Revenue increased year over year'}),
       (e:Entity {name: 'revenue', kind: 'metric'}),
       (d)-[:CONTAINS]->(c),
       (c)-[:MENTIONS]->(e)
RETURN d.id AS document_id, c.id AS chunk_id, e.name AS entity
""")

graph.save_snapshot()
```

## Vector Plus Graph Retrieval

Persistent graphs expose named vector indexes that are saved alongside graph
storage. Keep the vertex ID returned by Cypher next to the embedding ID.

```python
row = graph.cypher(
    "MATCH (c:Chunk) WHERE c.id = $id RETURN c",
    {"id": "chunk-1"},
).to_dicts()[0]
chunk_vertex_id = row["c"]

vector = graph.vector_index("chunks", 3)
vector.add(chunk_vertex_id, [0.10, 0.20, 0.30])
hits = vector.search([0.10, 0.20, 0.29], k=5)

for vertex_id, distance in hits:
    context = graph.cypher(
        """
        MATCH (c:Chunk)-[:MENTIONS]->(e:Entity)
        WHERE id(c) = $vertex_id
        RETURN c.text AS text, collect(e.name) AS entities
        """,
        {"vertex_id": vertex_id},
    ).to_dicts()
    print(distance, context)
```

## Bounded Traversal

Use bounded traversal for context-window expansion.

```python
start = graph.cypher(
    "MATCH (c:Chunk) WHERE c.id = $id RETURN c",
    {"id": "chunk-1"},
).to_dicts()[0]["c"]

neighbor_ids = nx.subgraph(
    graph,
    edge_label="MENTIONS",
    start=start,
    max_depth=2,
    max_nodes=50,
)
print(neighbor_ids)
```

## Snapshot Recovery

Persistent SDK graphs use the same WAL-backed write path as the server. Reopen
the directory to validate recovery.

```python
graph = nx.Graph.open("/tmp/domyn-nexus-graphrag")
graph.cypher("CREATE (:Entity {name: 'gross margin'})")
graph.save_snapshot()

again = nx.Graph.open("/tmp/domyn-nexus-graphrag")
rows = again.cypher(
    "MATCH (e:Entity) WHERE e.name = 'gross margin' RETURN e.name AS name"
).to_dicts()
assert rows == [{"name": "gross margin"}]
```

## Hot Backup

```python
manifest = graph.backup("/tmp/domyn-nexus-graphrag-backup")
print(manifest["files"])
```

Restore the backup with the server CLI:

```bash
nexus-server restore \
  --backup /tmp/domyn-nexus-graphrag-backup \
  --data-dir /tmp/domyn-nexus-graphrag-restored
```

## Tenant-Scoped Retrieval

The embedded SDK is intentionally single-process and single-graph. For
tenant-scoped retrieval, store a `tenant_id` property and filter every query:

```python
tenant = "NVDA"
rows = graph.cypher(
    """
    MATCH (c:Chunk)-[:MENTIONS]->(e:Entity)
    WHERE c.tenant_id = $tenant AND e.name CONTAINS $needle
    RETURN c.id AS chunk_id, e.name AS entity
    LIMIT 20
    """,
    {"tenant": tenant, "needle": "revenue"},
).to_dicts()
```

For hard tenant authorization, use the server product. It enforces principal
tenant scope before executing a query.
