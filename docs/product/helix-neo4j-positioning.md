# HelixDB / Neo4j Positioning

HelixDB validates the market: Rust graph + vector storage for AI memory is a
real product category, not a private hallucination. It does not mean Nexus
should copy a DSL-first product shape.

References:

- [HelixDB repository](https://github.com/HelixDB/helix-db)
- [HelixDB architecture](https://docs.helix-db.com/database/architecture)
- [HelixDB guarantees](https://docs.helix-db.com/database/guarantees)
- [HelixDB multi-tenancy](https://docs.helix-db.com/database/multi-tenancy)

## Nexus Position

Domyn Nexus is a Cypher-compatible, single-node-first graph + vector engine for
GraphRAG and regulated-data memory systems.

Differentiators:

- Cypher and Bolt compatibility path, instead of a new query language first.
- openCypher/TCK measurement as an engineering gate.
- GraphRAG benchmark suite on real FinReflectKG-style data.
- Python embedded path for in-process retrieval pipelines.
- Production-mode guardrails: auth, TLS, audit log, backup root, query limits,
  and explicit unsupported-feature rejection.

## HelixDB Takeaways

Useful product ideas:

- one-command local developer experience
- graph + vector as one mental model
- tenant-scoped data boundaries
- clear SDK-first examples
- simple hosted demo story

Do not copy blindly:

- DSL-first ergonomics if Cypher/Bolt compatibility is the buyer expectation.
- Product claims that outrun durability, recovery, or operational evidence.
- Cloud-looking packaging before backup/restore and config validation are
  boring locally.

## Neo4j Takeaways

Neo4j remains the compatibility anchor because:

- Cypher is widely known.
- LLMs generate Cypher better than bespoke DSLs.
- Enterprise buyers understand Bolt, Browser, Aura, APOC, and GDS shapes.
- Migration stories are easiest when the query surface is familiar.

Nexus should not try to be a full Neo4j clone immediately. The practical target:

- support the GraphRAG and operational subset extremely well
- reject unsupported features clearly
- keep expanding TCK coverage behind measurable gates
- preserve a Bolt/Cypher migration path for users who already have Neo4j mental
  models

## Product Order

1. Server product: binary, Docker, config, auth/TLS, backup/restore,
   observability, smoke.
2. Embedded SDK product: Python/Rust packaging for local GraphRAG.
3. Cloud demo product: hosted-looking deployment after server/SDK gates are
   stable.

This order keeps us from losing to better packaging while also avoiding fake
enterprise polish over a brittle core.
