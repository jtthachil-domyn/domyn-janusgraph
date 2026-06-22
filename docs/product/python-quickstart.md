# Domyn Nexus Python Quickstart

The Python SDK is for embedded GraphRAG workloads: the Rust engine runs inside
the Python process, with no HTTP or Bolt hop.

## Install

Internal Beta wheels are built with maturin. CI builds Linux x86_64, macOS
x86_64, and macOS arm64 wheels and runs `scripts/python-sdk-smoke.py` in a
clean virtual environment. For local development:

```bash
python3 -m venv .venv
. .venv/bin/activate
python3 -m pip install maturin
maturin develop -m crates/nexus-python/Cargo.toml
```

## In-Memory Graph

```python
import domyn_nexus as nx

graph = nx.Graph(vertex_capacity=1024, edge_capacity=4096)
graph.register_vertex_property("name", "string", indexed=True)

apple = graph.add_vertex("Entity")
graph.set_property(apple, "name", "Apple")
graph.build()

result = graph.cypher(
    "MATCH (n:Entity) WHERE n.name = $name RETURN n.name AS name",
    {"name": "Apple"},
)
print(result.to_dicts())
```

The in-memory graph is fast and convenient, but it is not WAL-backed.

## Persistent Graph

Use `Graph.open(path)` when mutations must survive process restart. Persistent
graphs route Cypher writes and direct mutation helpers through the same
`NexusEngine::execute_write()` path as the server product.

```python
import domyn_nexus as nx

graph = nx.Graph.open("/tmp/domyn-nexus-sdk-demo")

graph.cypher("CREATE (n:Entity {name: 'Apple'})")
graph.save_snapshot()

again = nx.Graph.open("/tmp/domyn-nexus-sdk-demo")
rows = again.cypher("MATCH (n:Entity) RETURN n.name AS name").to_dicts()
print(rows)
```

## Backup

Persistent graphs can create hot backups:

```python
manifest = graph.backup("/tmp/domyn-nexus-sdk-backup")
print(manifest["files"])
```

Backups contain a `backup-manifest.json` and can be restored by the server CLI:

```bash
nexus-server restore \
  --backup /tmp/domyn-nexus-sdk-backup \
  --data-dir /tmp/domyn-nexus-sdk-restored
```

## Vector Search

Persistent graphs expose named, storage-backed vector indexes:

```python
chunk = graph.cypher("CREATE (c:Chunk {id: 'chunk-1'}) RETURN c").to_dicts()[0]["c"]

vectors = graph.vector_index("chunks", 3)
vectors.upsert(chunk, [0.1, 0.2, 0.3])
print(vectors.search([0.1, 0.2, 0.3], k=1))
```

Standalone vector indexes remain available for transient in-memory retrieval:

```python
vec = nx.VectorIndex(dimension=3)
vec.add(apple, [0.1, 0.2, 0.3])
print(vec.search([0.1, 0.2, 0.3], k=1))
```

## Exceptions

The SDK exposes product-shaped exception classes:

```python
try:
    graph.cypher("MATCH (n) RETURN n CALL db.labels()")
except nx.CypherError as err:
    print(err)
```

Available classes:

- `NexusError`: base class for SDK product errors
- `CypherError`: parse, bind, plan, and execution errors
- `StorageError`: durable open, snapshot, backup, and WAL-backed write errors
- `SchemaError`: schema/type/property mistakes
- `VectorError`: vector dimension and vector persistence errors

## Current SDK Limits

- In-memory `Graph()` is not durable.
- Persistent `Graph.open(path)` is single-process; do not open the same data
  directory for writes from multiple Python processes.
- `Graph.vector_index(name, dim)` requires `Graph.open(path)`; use standalone
  `VectorIndex` for in-memory-only retrieval.
