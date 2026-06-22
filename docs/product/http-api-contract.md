# Domyn Nexus HTTP API Contract

This is the Internal Beta HTTP contract. Additive fields are allowed, but
renaming or removing documented fields is a breaking product change.

All JSON errors use the same envelope:

```json
{
  "code": "ERROR_CODE",
  "message": "human readable message",
  "details": {},
  "error": "human readable message"
}
```

`error` is a compatibility alias for earlier beta clients. New clients should
use `message`.

## Auth

Production deployments send one of:

```http
Authorization: Bearer <token>
x-api-key: <token>
```

Development configs may disable auth.

## Health

```http
GET /health
```

Response:

```json
{
  "status": "ok"
}
```

## Readiness

```http
GET /ready
```

Ready means an engine or tenant is configured and the server can accept work.

```json
{
  "status": "ready"
}
```

## Cypher

```http
POST /cypher
Content-Type: application/json
```

Request:

```json
{
  "query": "MATCH (n:Entity) WHERE n.name = $name RETURN n.name AS name",
  "tenant": "default",
  "params": {
    "name": "Apple"
  }
}
```

`tenant` and `params` are optional.

Response:

```json
{
  "columns": ["name"],
  "rows": [["Apple"]],
  "time_ms": 0.42
}
```

Common errors:

```json
{
  "code": "CYPHER_ERROR",
  "message": "parse or execution error",
  "details": {},
  "error": "parse or execution error"
}
```

```json
{
  "code": "RESULT_ROW_LIMIT_EXCEEDED",
  "message": "query returned more rows than default_query_limit",
  "details": {},
  "error": "query returned more rows than default_query_limit"
}
```

## Tenants

List tenants:

```http
GET /tenants
```

Response:

```json
{
  "tenants": ["default", "AAPL"]
}
```

Create tenant:

```http
POST /tenants
Content-Type: application/json
```

Request:

```json
{
  "tenant_id": "AAPL",
  "vertex_capacity": 1024,
  "edge_capacity": 1024
}
```

`vertex_capacity` and `edge_capacity` are optional.

Response:

```json
{
  "tenants": ["default", "AAPL"]
}
```

Conflict:

```json
{
  "code": "TENANT_EXISTS",
  "message": "tenant already exists: AAPL",
  "details": {},
  "error": "tenant already exists: AAPL"
}
```

## Document Collections

Document collections store JSON documents in the same durable Nexus store as
graph snapshots, WAL, and vector snapshots. They are exposed as HTTP endpoints
and through the Cypher `document(collection, key)` and
`documents(collection, limit)` / `documentsBy(...)` functions. Nexus does not
expose AQL.

List collections:

```http
GET /collections?tenant=default
```

Response:

```json
{
  "collections": ["filings"]
}
```

Upsert a document:

```http
PUT /collections/filings/documents/nvda-2024
Content-Type: application/json
```

Request:

```json
{
  "tenant": "default",
  "document": {
    "ticker": "NVDA",
    "year": 2024,
    "form": "10-K"
  }
}
```

Response:

```json
{
  "ok": true
}
```

Read a document:

```http
GET /collections/filings/documents/nvda-2024?tenant=default
```

Response:

```json
{
  "collection": "filings",
  "key": "nvda-2024",
  "document": {
    "ticker": "NVDA",
    "year": 2024,
    "form": "10-K"
  }
}
```

List documents:

```http
GET /collections/filings/documents?tenant=default&limit=100
```

Response:

```json
{
  "documents": [
    {
      "collection": "filings",
      "key": "nvda-2024",
      "document": {
        "ticker": "NVDA"
      }
    }
  ]
}
```

Delete a document:

```http
DELETE /collections/filings/documents/nvda-2024?tenant=default
```

Response:

```json
{
  "deleted": true
}
```

Create a document secondary index:

```http
POST /collections/filings/indexes
Content-Type: application/json
```

Request:

```json
{
  "tenant": "default",
  "path": "ticker",
  "kind": "scalar"
}
```

`kind` is optional and defaults to `"scalar"`. Use `"full_text"` for a
tokenized full-text document index.

Response:

```json
{
  "collection": "filings",
  "path": "ticker",
  "kind": "scalar"
}
```

List document secondary indexes:

```http
GET /collections/filings/indexes?tenant=default
```

Response:

```json
{
  "indexes": [
    {
      "collection": "filings",
      "path": "ticker",
      "kind": "scalar"
    }
  ]
}
```

Search through a document secondary index:

```http
POST /collections/filings/documents/search
Content-Type: application/json
```

Request:

```json
{
  "tenant": "default",
  "path": "ticker",
  "value": "NVDA",
  "limit": 100
}
```

Response:

```json
{
  "documents": [
    {
      "collection": "filings",
      "key": "nvda-2024",
      "document": {
        "ticker": "NVDA"
      }
    }
  ]
}
```

Prefix, range, and full-text search use the same endpoint. Exactly one of
`value`, `prefix`, `text`, or `gte`/`lte` must be supplied.

```json
{
  "tenant": "default",
  "path": "ticker",
  "prefix": "NV",
  "limit": 100
}
```

```json
{
  "tenant": "default",
  "path": "year",
  "gte": 2023,
  "lte": 2024,
  "limit": 100
}
```

```json
{
  "tenant": "default",
  "path": "body",
  "text": "revenue risk",
  "ranking": "bm25",
  "phrase": false,
  "fuzzy_distance": 1,
  "stem": true,
  "snippets": true,
  "explain": true,
  "limit": 100
}
```

Full-text responses include optional `score`, `snippet`, and `explanation`
fields when relevant. Supported ranking modes are `match_count`, `tf_idf`, and
`bm25`; omitted `ranking` uses BM25.

Cypher key lookup:

```cypher
RETURN document('filings', 'nvda-2024').ticker AS ticker
```

Cypher bounded scan:

```cypher
UNWIND documents('filings', 100) AS doc
RETURN doc.key AS key, doc.document.ticker AS ticker
ORDER BY key
```

Cypher indexed scan:

```cypher
UNWIND documentsBy('filings', 'ticker', 'NVDA', 100) AS doc
RETURN doc.key AS key, doc.document.ticker AS ticker
ORDER BY key
```

Cypher MATCH-style document scan:

```cypher
MATCH DOCUMENT doc IN filings
WHERE doc.document.ticker = 'NVDA'
RETURN doc.key AS key, doc.document.ticker AS ticker
ORDER BY key
```

`MATCH DOCUMENT` is scan-backed by default. Simple exact equality,
`STARTS WITH` prefix, and scalar range predicates on `doc.document.*` are pushed
into scalar document indexes when possible, with bounded scan fallback if no
matching index exists. Explicit `documentFullText(doc.document.path, query)`
predicates push into full-text document indexes when possible. `CONTAINS`
remains exact substring semantics. Use `documentsBy(...)` or HTTP document
index search for explicit indexed exact/prefix/range/full-text lookup.

## Cross-Model Batch

Commit ordered graph writes plus document/vector mutations in one WAL-backed
batch:

```http
POST /tx/batch
Content-Type: application/json
```

Request:

```json
{
  "tenant": "default",
  "cypher_statements": [
    {
      "query": "CREATE (n:Document {name: $name}) RETURN n.name AS name",
      "params": {
        "name": "NVDA filing"
      }
    },
    {
      "query": "MATCH (n:Document) WHERE n.name = $name SET n.status = 'indexed' RETURN n.status AS status",
      "params": {
        "name": "NVDA filing"
      }
    }
  ],
  "documents": [
    {
      "op": "upsert",
      "collection": "filings",
      "key": "nvda-2024",
      "document": {
        "ticker": "NVDA"
      }
    }
  ],
  "vectors": [
    {
      "op": "upsert",
      "index": "entities",
      "vertex_id": 0,
      "embedding": [1.0, 0.0]
    }
  ]
}
```

Legacy clients may send one write statement as `cypher` instead of
`cypher_statements`. Do not send both.

Response:

```json
{
  "committed": true,
  "document_ops": 1,
  "vector_ops": 1,
  "cypher": {
    "columns": ["status"],
    "rows": [["indexed"]],
    "time_ms": 1.2
  },
  "cypher_results": [
    {
      "columns": ["name"],
      "rows": [["NVDA filing"]],
      "time_ms": 1.2
    },
    {
      "columns": ["status"],
      "rows": [["indexed"]],
      "time_ms": 1.2
    }
  ],
  "time_ms": 1.2
}
```

`cypher` is the final Cypher result for compatibility. `cypher_results`
contains every Cypher result in statement order. If any operation fails, no
graph/document/vector mutation from the batch is committed.

## Vector Indexes

List named vector indexes:

```http
GET /vectors?tenant=default
```

Response:

```json
{
  "indexes": ["entities"]
}
```

Create or load an index:

```http
POST /vectors/entities
Content-Type: application/json
```

Request:

```json
{
  "tenant": "default",
  "dimension": 384
}
```

Response:

```json
{
  "ok": true
}
```

Upsert an embedding:

```http
POST /vectors/entities/upsert
Content-Type: application/json
```

Request:

```json
{
  "tenant": "default",
  "vertex_id": 42,
  "embedding": [0.1, 0.2, 0.3]
}
```

Response:

```json
{
  "ok": true
}
```

Search:

```http
POST /vectors/entities/search
Content-Type: application/json
```

Request:

```json
{
  "tenant": "default",
  "query": [0.1, 0.2, 0.3],
  "k": 10
}
```

Response:

```json
{
  "results": [
    {
      "vertex_id": 42,
      "distance": 0.0
    }
  ]
}
```

Remove an embedding:

```http
DELETE /vectors/entities/42?tenant=default
```

Response:

```json
{
  "removed": true
}
```

Compact an index:

```http
POST /vectors/entities/compact
Content-Type: application/json
```

Request:

```json
{
  "tenant": "default"
}
```

Response:

```json
{
  "ok": true
}
```

Missing index:

```json
{
  "code": "VECTOR_INDEX_NOT_FOUND",
  "message": "vector index not found",
  "details": {},
  "error": "vector index not found"
}
```

## Admin

Hot backup:

```http
POST /admin/backup
Content-Type: application/json
```

Request:

```json
{
  "tenant": "default",
  "path": "daily/manual-001"
}
```

Manual compaction:

```http
POST /admin/compact
Content-Type: application/json
```

Request:

```json
{
  "tenant": "default",
  "threshold": 1
}
```

## Metrics

```http
GET /metrics
```

Returns Prometheus text format. Stable metric families are documented in the
operations runbook.
