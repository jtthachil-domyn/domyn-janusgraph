# SDK Limits

The Python SDK is an embedded product surface. It is meant for local GraphRAG
pipelines where the graph engine runs inside the Python process.

## Durable Versus In-Memory Graphs

- `nx.Graph()` is in-memory and not WAL-backed.
- `nx.Graph.open(path)` is persistent and routes writes through the same
  WAL-before-apply engine path as the server.
- `save_snapshot()` and `backup(path)` require `Graph.open(path)`.

## Process Safety

Persistent SDK graphs are single-process. Do not open the same data directory
for writes from multiple Python processes.

For a shared service, run `nexus-server` and use HTTP or Bolt.

## Tenancy

The SDK does not enforce tenant authorization. You can model tenant scope with
a `tenant_id` property and filter queries yourself.

Use the server product when tenant authorization must be enforced before query
execution.

## Vector Indexes

- `Graph.open(path).vector_index(name, dim)` creates or loads a named,
  storage-backed vector index.
- `VectorIndex(dim)` remains available for transient in-memory retrieval.
- The SDK accepts `mode="hnsw"` and `mode="exact"` for API stability; the
  current core index keeps an exact oracle and HNSW-style ANN path internally.

## Exceptions

The SDK exposes:

- `NexusError`
- `CypherError`
- `StorageError`
- `SchemaError`
- `VectorError`

Exception messages include a stable SDK code prefix such as
`PY_CYPHER_ERROR` or `PY_STORAGE_ERROR`. Rich structured attributes are a future
enhancement.

## Packaging

The wheel workflow builds Linux x86_64, macOS x86_64, and macOS arm64 wheels
with maturin and runs `scripts/python-sdk-smoke.py` in a clean virtual
environment. A hosted CI run must be observed before marking wheel packaging as
fully complete.
