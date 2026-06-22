# Domyn Nexus TCK Count Ledger

This file is the canonical count ledger for Cypher conformance measurements.
Do not replace one denominator with another. When reporting progress, name the
scope first, then the counts.

## Golden Rule

- Full-corpus numbers use `3,897 total / 3,830 considered`.
- Checked-in local Falkor runner numbers use `2,587 total / 2,125 considered`.
- OverGraph test counts are not Cypher TCK counts.
- Do not infer exact integer numerators from rounded percentages. If the JSON
  report is not available, record the rounded rate and mark exact numerators as
  unknown.

## Scope A: Full openCypher / OCG-Style Corpus

This is the production conformance denominator.

| Counter | Value | Meaning |
|---|---:|---|
| Total expanded scenarios | `3,897` | Full expanded openCypher/OCG-style scenario corpus. |
| Considered scenarios | `3,830` | Scenarios included in the measured denominator. |
| Skipped / rejected before measurement | `67` | `3,897 - 3,830`; excluded before parse/exec/result scoring. |
| Parse-ok rate | `82.1%` | Rounded reported rate over `3,830` considered scenarios. |
| Exec-ok rate | `64.7%` | Rounded reported rate over `3,830` considered scenarios. |
| Result-match rate | `42.7%` | Rounded reported rate over `3,830` considered scenarios. |
| Exact parse-ok count | Unknown | Need full-corpus JSON/raw report; cannot derive exactly from rounded `82.1%`. |
| Exact exec-ok count | Unknown | Need full-corpus JSON/raw report; cannot derive exactly from rounded `64.7%`. |
| Exact result-match count | Unknown | Need full-corpus JSON/raw report; cannot derive exactly from rounded `42.7%`. |

Approximate rounded-rate ranges, for sanity checks only:

| Metric | Possible integer range from rounded rate | Why not exact |
|---|---:|---|
| Parse-ok | `3,143-3,146` | Any of these can round to `82.1%` of `3,830`. |
| Exec-ok | `2,477-2,479` | Any of these can round to `64.7%` of `3,830`. |
| Result-match | `1,634-1,637` | Any of these can round to `42.7%` of `3,830`. |

## Scope B: Checked-In Local Falkor Runner

This is the current fast tranche dashboard produced by:

```bash
cargo test -p nexus-cypher --test tck_runner -- --nocapture
```

The runner currently points at:

```text
references/falkordb/tests/tck/features
```

The feature root can now be overridden explicitly:

```bash
TCK_FEATURE_ROOT=/absolute/path/to/features \
TCK_SCOPE=full-opencypher \
TCK_EXPECT_TOTAL=3897 \
TCK_EXPECT_CONSIDERED=3830 \
cargo test -p nexus-cypher --test tck_runner -- --nocapture
```

The `TCK_EXPECT_TOTAL` and `TCK_EXPECT_CONSIDERED` guards intentionally fail
the run if the measured denominator is not the denominator being reported. Use
them whenever publishing Scope A or Scope B numbers.

Current exact local-runner counts:

| Counter | Value | Meaning |
|---|---:|---|
| Raw local Gherkin scenarios | `1,615` | Raw `Scenario:` plus `Scenario Outline:` definitions under the Falkor feature root. |
| Raw local `Scenario Outline` definitions | `276` | Outline templates expanded by the runner. |
| Expanded local executable cases | `2,587` | Cases after `Scenario Outline` / `Examples` expansion. |
| Considered local cases | `2,125` | Local cases included in parse/exec/result denominator. |
| Local skipped / rejected before measurement | `462` | `2,587 - 2,125`; excluded before scoring. |
| Parse-ok | `1,758` | Parsed/bound far enough to count as parse-ok. Expected-error scenarios rejected by the binder are counted in `Expected-error ok`, not `Parse-ok`. |
| Exec-ok | `1,758` | Parsed and executed without an execution error. Expected-error scenarios rejected before execution are counted in `Expected-error ok`, not `Exec-ok`. |
| Result-ok | `2,125` | Correct positive result matches plus correctly rejected expected-error scenarios. |
| Result mismatch | `0` | Parsed and executed, but returned rows did not match expected rows. |
| Parse/setup errors | `0` | Query or setup failed before execution, excluding expected-error successes. |
| Execution errors | `0` | Parsed, but execution failed unexpectedly. |
| Expected-error ok | `367` | Scenario expected an error and Nexus rejected it. |

Current exact local-runner rates:

| Metric | Formula | Value |
|---|---|---:|
| Parse-ok | `1,758 / 2,125` | `82.7%` |
| Exec-ok | `1,758 / 2,125` | `82.7%` |
| Result-match | `2,125 / 2,125` | `100.0%` |

Local result-ok breakdown:

| Bucket | Count | Meaning |
|---|---:|---|
| Expected-error ok | `367` | Correctly rejected scenarios where failure is the expected behavior. |
| Positive result-ok | `1,758` | `2,125 - 367`; scenarios that returned the expected positive result, matched expected side effects, or passed a control-query check. |
| Exec-ok without comparable result table | `0` | Control-query support removed the last 13 non-comparable Scope B cases. |

Current local top failure buckets:

| Category | Failures | Parse errors | Exec errors | Result mismatches |
|---|---:|---:|---:|---:|
| None | `0` | `0` | `0` | `0` |

The local runner has no remaining non-comparable considered scenarios. The 462
skipped/rejected scenarios remain outside the local scoring denominator.
Ordinary Scope B failures are currently cleared: zero unexpected parse errors,
zero execution errors, and zero result mismatches.

## Scope B+: Skipped-Inclusive Experimental Local Runner

This is an experimental tranche mode for working through scenarios that Falkor
keeps behind upstream `@skip`, `@ignore`, or related skip-style tags. It is not
the canonical Scope B score, but it is useful for expanding coverage after
Scope B reaches zero ordinary failures.

Run with:

```bash
TCK_INCLUDE_UPSTREAM_SKIPPED=1 cargo test -p nexus-cypher --test tck_runner -- --nocapture
```

Category-focused runs use the same mode plus `TCK_CATEGORY=...`.

To lock the skipped-inclusive local denominator:

```bash
TCK_INCLUDE_UPSTREAM_SKIPPED=1 \
TCK_EXPECT_TOTAL=3870 \
TCK_EXPECT_CONSIDERED=3830 \
cargo test -p nexus-cypher --test tck_runner -- --nocapture
```

Current skipped-inclusive tranche checkpoints:

Latest full skipped-inclusive report:

| Counter | Value | Meaning |
|---|---:|---|
| Total expanded scenarios | `3,870` | Expanded cases reachable through the checked-in Falkor feature root when upstream skip tags are included. |
| Skipped | `40` | Scenarios still excluded by hard harness limits, not by upstream skip tags. |
| Considered | `3,830` | Denominator for skipped-inclusive tranche rates. |
| Parse-ok | `3,149` | `3,149 / 3,830 = 82.2%`; falls when invalid scenarios start failing correctly. |
| Exec-ok | `3,149` | `3,149 / 3,830 = 82.2%`; falls when invalid scenarios start failing correctly. |
| Result-ok | `3,830` | `3,830 / 3,830 = 100.0%`. |
| Expected-error ok | `681` | Correctly rejected negative scenarios. |
| Result mismatch | `0` | Parsed and executed but rows differed. |
| Parse errors | `0` | Remaining unexpected parse/setup failures. |
| Exec errors | `0` | Unexpected execution failures in skipped-inclusive mode. |

Latest top skipped-inclusive failure buckets:

| Category | Failures | Parse errors | Exec errors | Result mismatches | Notes |
|---|---:|---:|---:|---:|---|
| `expressions/temporal` | `0` | `0` | `0` | `0` | Cleared in skipped-inclusive mode after DST fallback arithmetic, large expanded-year parsing, named-zone offset recomputation for composed datetimes, fractional duration normalization, date/duration carry rules, control-query support, and list-literal matching for temporal strings with colons. Current compatibility layer is `1004 / 1004 = 100.0%` result-ok. |
| Non-temporal categories | `0` | `0` | `0` | `0` | Cleared in skipped-inclusive mode. |
| `expressions/graph` | `0` | `0` | `0` | `0` | Cleared by relationship type-test expressions such as `r:T2`. |
| `clauses/with-where` | `0` | `0` | `0` | `0` | Cleared by evaluating `WITH ... WHERE` predicates against a mixed alias/input scope. |
| `clauses/delete` | `0` | `0` | `0` | `0` | Cleared by invalid delete-target validation, grouped path/list delete application, and side-effect table scoring. |
| `clauses/return` | `0` | `0` | `0` | `0` | Cleared by aggregate grouping, DISTINCT/ORDER BY scope, and deleted-binding validation. |
| `clauses/with` | `0` | `0` | `0` | `0` | Cleared by DISTINCT/ORDER BY rule cascade. |

| Category | Total | Skipped | Considered | Parse-ok | Exec-ok | Result-ok | Expected-error ok | Mismatch | Notes |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `expressions/literals` | `131` | `0` | `131` | `106` | `106` | `131` | `25` | `0` | Fully green in skipped-inclusive mode after Unicode string escapes, Gherkin table-cell unescaping, and invalid numeric-literal rejection. |
| `clauses/with-orderBy` | `292` | `0` | `292` | `223` | `223` | `292` | `69` | `0` | Fully green in skipped-inclusive mode after temporal ordering, temporal `+ duration(...)` sort expressions, aggregate `ORDER BY` error validation, and post-aggregation `WITH ... ORDER BY ... LIMIT` planning. |
| `clauses/with-skip-limit` | `9` | `0` | `9` | `9` | `9` | `9` | `0` | `0` | Fully green after row-count expressions and dependent RHS predicate fallback. |
| `clauses/return-skip-limit` | `31` | `0` | `31` | `15` | `15` | `31` | `16` | `0` | Fully green after row-count expressions in `SKIP`/`LIMIT`; invalid row-count cases are expected-error passes. |
| `expressions/existentialSubqueries` | `10` | `0` | `10` | `9` | `9` | `10` | `1` | `0` | Fully green in skipped-inclusive mode after parsing `EXISTS { ... }` as a correlated read subquery instead of a placeholder boolean. |
| `expressions/list` | `185` | `0` | `185` | `126` | `126` | `185` | `59` | `0` | Fully green after invalid aggregation in list comprehensions and `size(path)` expected-error validation. |
| `expressions/path` | `7` | `0` | `7` | `5` | `5` | `7` | `2` | `0` | Fully green after rejecting `length(node)`, `length(relationship)`, and path-vs-size misuse. |
| `expressions/pattern` | `50` | `0` | `50` | `31` | `31` | `50` | `19` | `0` | Fully green in skipped-inclusive mode after scoped pattern-comprehension bindings and strict expected-error checks for illegal pattern predicates. |
| `expressions/precedence` | `105` | `1` | `104` | `104` | `104` | `104` | `0` | `0` | Fully green after left-associative exponentiation, float modulo, and row-count expression support. |
| `expressions/string` | `32` | `0` | `32` | `32` | `32` | `32` | `0` | `0` | Fully green in skipped-inclusive mode after newline string formatting and Cypher-null semantics for string operators on non-string operands. |
| `expressions/temporal` | `1004` | `0` | `1004` | `1004` | `1004` | `1004` | `0` | `0` | Fully green in skipped-inclusive mode after DST fallback arithmetic, large expanded-year parsing, named-zone offset recomputation for composed datetimes, fractional duration normalization, date/duration carry rules, control-query support, and list-literal matching for temporal strings with colons. |

Important interpretation rule: in this runner, `parse-ok` and `exec-ok` can go
down when an invalid query starts failing correctly. That is progress when the
same scenario moves into `expected-error ok` and `result-ok` increases.

## Scope C: OverGraph

OverGraph is an engine hardening reference, not an openCypher TCK source.

| Counter | Value | Meaning |
|---|---:|---|
| Local Rust test annotations | About `1,090` | Tests under `references/overgraph`; useful for WAL, compaction, vector, pagination, recovery, and operations parity. |

## Counter Definitions

| Counter | Definition |
|---|---|
| `total` | Expanded scenario count in the selected corpus. |
| `skipped` | Scenarios excluded before parse/exec/result scoring. Causes include skip/ignore tags, unsupported fixtures, unsupported `Then` assertions, procedure definitions, unexpanded placeholders, or harness limitations. |
| `considered` | `total - skipped`; denominator for parse-ok, exec-ok, and result-match rates. |
| `parse-ok` | Scenario parsed/bound far enough to attempt execution. Expected-error scenarios that correctly fail at parse/bind time count as `result-ok`, not `parse-ok`, in the current local runner. |
| `exec-ok` | Scenario parsed and executed without execution error. |
| `result-ok` / `result-match` | Scenario is judged correct: either returned expected rows or correctly raised an expected error. |
| `result mismatch` | Scenario parsed and executed, but actual rows differed from expected rows. |
| `expected-error ok` | Scenario expected an error and Nexus correctly rejected it. |

## Reporting Template

Use this exact shape in progress notes:

```text
Scope: full corpus OR checked-in local Falkor runner
Total: N
Skipped/rejected: N
Considered: N
Parse-ok: N/N = X%
Exec-ok: N/N = X%
Result-match: N/N = X%
Expected-error ok: N
Top failure buckets: ...
```

If exact numerator counts are not available, write:

```text
Exact numerator unavailable; only rounded rate was reported.
```
