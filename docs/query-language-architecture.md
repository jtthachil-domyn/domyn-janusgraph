# Domyn Nexus Query Language Architecture

This document records the long-term query-language decision for Domyn Nexus:
ship Cypher compatibility first, build the core around graph-algebra runtime
primitives, and keep the internal representation ready for a future GQL
frontend without making the internal IR equal to GQL syntax.

It is a design guide, not a production-readiness claim. Current implementation
status still lives in [`production-readiness.md`](production-readiness.md), and
TCK counters still live in [`tck-count-ledger.md`](tck-count-ledger.md).

## Decision

Domyn Nexus should use this architecture:

```text
Cypher frontend      future GQL frontend      optional tooling frontends
      |                      |                          |
      v                      v                          v
 parser AST              parser AST              validated request form
      |                      |                          |
      +-----------> semantic binder / scope checker <---+
                                |
                                v
                 graph-algebra logical IR
                                |
                                v
                 optimizer / physical planner
                                |
                                v
                 execution operators over Nexus storage
```

The product surface should remain Cypher-first for the near term. Cypher has
the ecosystem gravity today: Neo4j compatibility, LLM-generated query patterns,
existing GraphRAG examples, and user familiarity.

The internal design should remain GQL-ready. GQL is the standards direction for
property-graph querying, and Cypher is close enough to it that a future GQL
frontend should lower into the same binder and logical IR.

The IR should not be "GQL encoded internally." It should be lower-level than
GQL and execution-oriented:

- scan
- expand
- traverse
- filter
- join
- project
- aggregate
- sort
- limit
- path materialization
- variable binding
- cardinality tracking
- mutation operators
- write barriers

That keeps optimizer freedom and prevents syntax-level quirks from leaking into
the runtime.

## cypher-guard Boundary

[`cypher-guard`](https://github.com/neo4j-field/cypher-guard) is useful, but it
does not replace the Nexus query stack.

Use it as inspiration for:

- preflight validation
- schema-aware query checks
- safer LLM-generated Cypher workflows
- user-facing diagnostics
- guardrail-style query correction

Do not treat it as:

- a parser replacement
- a semantic binder for a database engine
- a planner
- an executor
- a storage/runtime layer
- a path to openCypher or GQL conformance by itself

The hard part is not basic syntax validation. The hard part is database-grade
semantic binding and planning.

## Binder Is The Center

The binder/planner layer is the heart of the system. This is where most serious
graph databases accumulate complexity.

The binder must own:

- variable visibility across `MATCH`, `WITH`, `UNWIND`, `RETURN`, and writes
- variable kinds: node, relationship, path, scalar, list, map, unknown
- graph pattern legality
- optional-match nullability
- aggregation phases and grouping rules
- expression type inference
- path semantics
- deleted-binding legality after writes
- tenant/schema visibility
- function/procedure signatures
- expected-error conformance for TCK scenarios

The planner should receive a bound query, not raw syntax. That lets Cypher and
future GQL converge on the same operator tree.

## Cypher vs GQL vs AQL

### Cypher

Cypher is the correct first-class language surface now.

Reasons:

- strongest graph ecosystem familiarity
- Neo4j compatibility expectations
- existing openCypher/Falkor-style TCK coverage
- best LLM generation behavior today
- direct fit for the current GraphRAG query patterns

Cost:

- Cypher compatibility is not "just a parser"; it requires binder, planner,
  null semantics, path semantics, aggregation, and error behavior.

### GQL

GQL is the correct strategic standard to align with.

Reasons:

- property-graph standardization direction
- heavy Cypher influence
- future enterprise/procurement relevance
- good target for multi-frontend portability

Cost:

- implementing a GQL frontend later still requires a full parser and semantic
  binder mapping; it is not automatic.
- the internal IR should be graph algebra, not GQL syntax.

### AQL

AQL should be treated as a technical reference, not the main compatibility
target.

Useful ideas:

- document + graph + search integration
- traversal integration
- distributed query execution patterns

Reasons not to prioritize AQL as the main surface:

- weaker property-graph ecosystem convergence
- less Neo4j/GraphRAG familiarity
- weaker LLM-generated-query prior
- not the standardization path for property-graph interoperability

## Execution Plan

### Phase 1: Stabilize Cypher On The Existing IR

- Keep measuring against the local Falkor/openCypher runner and the full
  external corpus separately.
- Keep semantic checks in `nexus-cypher`, not in `nexus-server`.
- Finish binder coverage before adding more planner shortcuts.
- Keep server writes WAL-backed through `NexusEngine::execute_write()`.
- Keep `run_cypher_mut_in_memory()` explicitly non-durable.

### Phase 2: Make The IR More Graph-Algebraic

- Ensure every AST construct lowers into bound logical operators.
- Separate syntax features from runtime operators.
- Represent path values, relationship values, nullable bindings, grouping keys,
  and mutation barriers explicitly.
- Track cardinality and variable kinds in the bound plan.
- Avoid exposing Cypher-specific syntax details to physical execution.

### Phase 3: Add GQL Readiness Hooks

- Add a language-neutral binder input model where feasible.
- Keep function/procedure registries signature-driven.
- Keep graph pattern binding independent of Cypher token names.
- Document every Cypher-specific behavior that would differ under GQL.
- Add internal tests that lower equivalent Cypher/GQL-shaped forms into the
  same logical operators before building a full GQL parser.

### Phase 4: Add A GQL Frontend Later

Only start a GQL parser after:

- Cypher TCK counters are stable and honest.
- binder scope/type/nullability behavior is boring.
- write semantics and WAL-backed server execution are stable.
- the logical IR can represent graph algebra without relying on Cypher AST
  details.

The GQL frontend should parse and bind into the same logical IR, then reuse the
same optimizer and executor.

## Non-Goals

- Do not make the internal IR identical to GQL.
- Do not make the internal IR a prettier Cypher AST.
- Do not treat cypher-guard as a database runtime.
- Do not prioritize AQL compatibility ahead of Cypher/GQL.
- Do not add a GQL parser before the binder is strong enough to share.
- Do not claim production query-language readiness from parser pass rates alone.

## Acceptance Criteria

This strategy is working when:

- Cypher and future GQL features lower through the same binder and logical IR.
- syntax-specific code ends before planning.
- semantic errors are raised before execution.
- planner tests assert operator trees, not parser trivia.
- execution operators know about graph algebra, not query-language grammar.
- TCK expected-error scenarios pass through binder errors, not accidental parse
  failures.

## Relationship To The Roadmap

This document sharpens the openCypher track in
[`opencypher-production-and-scale-roadmap.md`](opencypher-production-and-scale-roadmap.md).
The roadmap says what to implement; this document says what shape the language
stack should have while implementing it.

The practical order remains:

1. Cypher compatibility and TCK honesty.
2. Binder and graph-algebra IR hardening.
3. Production engine hardening.
4. GQL frontend once the shared IR is mature.
5. Distributed query execution after single-node semantics are stable.
