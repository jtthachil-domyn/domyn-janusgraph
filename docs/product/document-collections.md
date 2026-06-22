# Domyn Nexus Document Collections

Document Collections v0 gives Nexus an Arango-style multi-model storage surface
without adopting AQL. Documents are JSON objects stored in named collections and
can be read from Cypher through `document(collection, key)`,
`documents(collection, limit)`, indexed `documentsBy(...)` scans, and
`MATCH DOCUMENT` scan syntax.

## Direction

- **Frontend language:** Cypher now, GQL-aligned internal semantics over time.
- **Not AQL:** Nexus should not add an AQL dialect unless there is a separate
  product decision.
- **Model:** JSON documents live alongside graph, vector, full-text, WAL,
  snapshots, and backups in the same durable store.
- **Durability:** standalone document upserts/deletes are WAL-logged before the
  JSON file mutation and replayed on startup if the process crashes in between.

## HTTP API

List collections:

```http
GET /collections
```

List documents in a collection:

```http
GET /collections/filings/documents?limit=100
```

Upsert a document:

```http
PUT /collections/filings/documents/nvda-2024
Content-Type: application/json

{
  "document": {
    "ticker": "NVDA",
    "year": 2024,
    "form": "10-K"
  }
}
```

Get a document:

```http
GET /collections/filings/documents/nvda-2024
```

Delete a document:

```http
DELETE /collections/filings/documents/nvda-2024
```

Create a scalar secondary index:

```http
POST /collections/filings/indexes
Content-Type: application/json

{
  "path": "ticker"
}
```

Create a full-text token index:

```http
POST /collections/filings/indexes
Content-Type: application/json

{
  "path": "body",
  "kind": "full_text"
}
```

Search through an index:

```http
POST /collections/filings/documents/search
Content-Type: application/json

{
  "path": "ticker",
  "value": "NVDA",
  "limit": 100
}
```

Prefix and range index scans use the same endpoint:

```http
POST /collections/filings/documents/search
Content-Type: application/json

{
  "path": "ticker",
  "prefix": "NV",
  "limit": 100
}
```

```http
POST /collections/filings/documents/search
Content-Type: application/json

{
  "path": "year",
  "gte": 2023,
  "lte": 2024,
  "limit": 100
}
```

Full-text search uses the same endpoint with `text`. Optional controls are
`ranking`, `phrase`, `fuzzy_distance`, `stem`, `snippets`, and `explain`.
Supported ranking modes are `bm25` (default and recommended lexical ranking),
`tf_idf` (baseline/debugging mode), and `match_count` (legacy deterministic
compatibility mode):

```http
POST /collections/filings/documents/search
Content-Type: application/json

{
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

`CONTAINS` is not rewritten to full-text search. In Cypher it means substring
matching, so `doc.document.body CONTAINS 'venue'` remains exact substring
semantics even when a full-text index exists. Use `documentFullText(...)` when
token-search semantics are intended.

Commit a graph write and document write together:

```http
POST /tx/batch
Content-Type: application/json

{
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
        "ticker": "NVDA",
        "graph_key": "NVDA filing"
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

`/tx/batch` accepts ordered native Cypher write statements plus any number of
document upserts/deletes and vector upserts/removes. Legacy clients may still
send one statement as `cypher`; multi-statement clients should use
`cypher_statements`. The server appends one cross-model WAL commit record before
applying any model. If a crash happens after the WAL append, startup recovery
replays the graph, document, and vector operations from that same commit record.
Vector batch writes require the named vector index to already exist.

## Cypher Access

```cypher
RETURN document('filings', 'nvda-2024').ticker AS ticker
```

The function returns a Cypher map or `null` if the document does not exist.
Nested fields work through normal map/property access:

```cypher
RETURN document('filings', 'nvda-2024').metadata.form AS form
```

Bounded collection scans are available through `documents(collection, limit)`.
They are designed to be used with `UNWIND`:

```cypher
UNWIND documents('filings', 100) AS doc
RETURN doc.key AS key, doc.document.ticker AS ticker
ORDER BY key
```

Each returned row is a map with `collection`, `key`, and `document` fields.
The runtime clamps the requested limit to a maximum of 1,000 documents.

The same scan is available through MATCH-style document syntax:

```cypher
MATCH DOCUMENT doc IN filings
WHERE doc.document.ticker = 'NVDA'
RETURN doc.key AS key, doc.document.form AS form
```

`MATCH DOCUMENT doc IN filings` is scan-backed by default and pushes simple
scalar predicates on `doc.document.*` into document indexes when possible:
exact equality, `STARTS WITH` string prefix, and `<` / `<=` / `>` / `>=`
range predicates. Full-text token predicates use the explicit
`documentFullText(value, query)` function:

```cypher
MATCH DOCUMENT doc IN filings
WHERE documentFullText(doc.document.body, 'revenue risk')
RETURN doc.key AS key
```

If a matching index does not exist, Nexus falls back to a bounded scan and
applies the original predicate. Quoted and parameterized collection names are
also accepted, for example `MATCH DOCUMENT doc IN 'filings'`.

Exact scalar secondary indexes are available through `documentsBy(collection,
path, value, limit)`:

```cypher
UNWIND documentsBy('filings', 'ticker', 'NVDA', 100) AS doc
RETURN doc.key AS key, doc.document.form AS form
```

Scalar indexes support exact matches on string, numeric, and boolean JSON values
at dot-separated paths such as `ticker` or `metadata.form`. HTTP index search
also supports string prefix scans and string/numeric range scans. Full-text
indexes tokenize strings, arrays, and objects under the indexed path into
lowercase alphanumeric tokens and return documents ranked by BM25, TF-IDF, or
match-count score. Full-text search also supports phrase filtering, bounded
fuzzy token matching, simple suffix stemming, snippets, and score explanations.

## Current Limits

- Scalar document secondary indexes support exact, prefix, and range search.
  Full-text document indexes support deterministic token search, BM25, TF-IDF,
  and match-count ranking, phrase filtering, bounded fuzzy token matching,
  simple suffix stemming, snippets, and score explanations. They do not yet
  support language-aware stemming, phrase positional indexes, highlight offsets,
  or custom analyzers.
- Array/object scalar and composite document indexes are not implemented yet.
- `MATCH DOCUMENT` pushes simple exact, prefix, scalar range, and explicit
  `documentFullText(...)` predicates into document indexes when possible, and
  otherwise falls back to a bounded scan. It does not yet push
  boolean-composed or nested disjunctive predicates into document indexes.
- No document-to-graph automatic materialization yet.
- No schema validation for document shapes yet.
- `/tx/batch` supports ordered native Cypher write statements plus document and
  vector upserts/deletes. Explicit Bolt multi-statement write transactions are
  still rejected; use HTTP `/tx/batch` for the current product transaction
  surface.
- No AQL. Future query work should lower Cypher and GQL-facing constructs into
  Nexus' internal graph/document operators.
