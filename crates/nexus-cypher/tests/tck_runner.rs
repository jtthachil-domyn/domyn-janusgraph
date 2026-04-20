//! openCypher TCK runner — measures parse / execute / result-correctness rates
//! against the local Gherkin scenarios in
//! `references/falkordb/tests/tck/features/`.
//!
//! Runs as a single `cargo test` integration test. Use `-- --nocapture` to see
//! the per-category report:
//!
//!   cargo test -p nexus-cypher --test tck_runner -- --nocapture
//!
//! Set `TCK_CATEGORY=clauses/with` to filter to a single subtree while
//! iterating on a feature area.

#![allow(clippy::too_many_lines)]

use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use nexus_core::graph::Graph;
use nexus_core::properties::PropertyType;
use nexus_core::types::{EdgeId, Value, VertexId};
use nexus_cypher::executor::{RunResult, run_cypher_mut_in_memory_with_params};

/// One parsed scenario.
#[derive(Debug, Clone, Default)]
struct Scenario {
    name: String,
    given: Option<GivenKind>,
    setup_queries: Vec<String>,
    params: BTreeMap<String, String>,
    query: Option<String>,
    expected: Option<Expected>,
    skipped_reason: Option<&'static str>,
    outline: bool,
    examples: Vec<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Copy)]
enum GivenKind {
    EmptyGraph,
    AnyGraph,
    BinaryTree1,
    BinaryTree2,
}

#[derive(Debug, Clone)]
enum Expected {
    Empty,
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        ordered: bool,
    },
    Error {
        description: String,
    },
}

#[derive(Debug, Default, Clone, Serialize)]
struct Stats {
    total: usize,
    skipped: usize,   // unsupported step types (error expectation, fixtures, etc.)
    parse_ok: usize,  // setup + query both parsed
    exec_ok: usize,   // executed without error
    result_ok: usize, // result matched expected (primitive cells only)
    result_mismatch: usize, // parsed+executed, expected primitive cells, result wrong
    vertex_results: usize, // expected contains node/edge literals — not yet compared
    exec_err: usize,  // parsed but execution errored
    parse_err: usize, // parser rejected
    expected_error_ok: usize, // query failed as requested by a TCK error scenario
}

const TCK_ROOT_REL: &str = "../../references/falkordb/tests/tck/features";
static DEBUG_MISMATCH_COUNT: AtomicUsize = AtomicUsize::new(0);

fn tck_root() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest).join(TCK_ROOT_REL)
}

fn discover_feature_files(root: &Path, filter: Option<&str>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    if let Some(f) = filter {
        out.retain(|p| p.to_string_lossy().contains(f));
    }
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("feature") {
            out.push(p);
        }
    }
}

/// Minimal Gherkin parser: extract Scenarios with their step lines and expand
/// Scenario Outline / Examples tables into executable scenario instances.
fn parse_feature(contents: &str) -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    let mut current: Option<Scenario> = None;
    let mut background = Scenario::default();
    let mut in_background = false;
    let mut in_docstring = false;
    let mut docstring_target: Option<DocstringTarget> = None;
    let mut docstring_buf = String::new();
    let mut in_table = false;
    let mut pending_table: Option<TableTarget> = None;
    let mut table_rows: Vec<Vec<String>> = Vec::new();

    for raw_line in contents.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim_start();

        // Docstring boundary.
        if trimmed.starts_with("\"\"\"") {
            if in_docstring {
                // Close docstring — attach to current step.
                if let Some(tgt) = docstring_target.take() {
                    let target = if in_background && current.is_none() {
                        Some(&mut background)
                    } else {
                        current.as_mut()
                    };
                    if let Some(sc) = target {
                        apply_docstring(sc, tgt, std::mem::take(&mut docstring_buf));
                    }
                }
                in_docstring = false;
            } else {
                in_docstring = true;
                docstring_buf.clear();
            }
            continue;
        }
        if in_docstring {
            docstring_buf.push_str(raw_line);
            docstring_buf.push('\n');
            continue;
        }

        // Table row (closed when a non-| line appears).
        if trimmed.starts_with('|') {
            in_table = true;
            let cells: Vec<String> = trimmed
                .trim_matches('|')
                .split('|')
                .map(|c| c.trim().to_string())
                .collect();
            table_rows.push(cells);
            continue;
        } else if in_table {
            if in_background && current.is_none() {
                flush_table(Some(&mut background), &mut pending_table, &mut table_rows);
            } else {
                flush_table(current.as_mut(), &mut pending_table, &mut table_rows);
            }
            table_rows.clear();
            in_table = false;
        }

        if trimmed.starts_with("Background:") {
            if let Some(sc) = current.take() {
                push_scenario(&mut scenarios, sc);
            }
            in_background = true;
            continue;
        }

        if trimmed.starts_with("Scenario:") || trimmed.starts_with("Scenario Outline:") {
            // Finalize previous.
            if let Some(sc) = current.take() {
                push_scenario(&mut scenarios, sc);
            }
            in_background = false;
            let name = trimmed
                .trim_start_matches("Scenario Outline:")
                .trim_start_matches("Scenario:")
                .trim()
                .to_string();
            let mut sc = background.clone();
            sc.name = name;
            sc.outline = false;
            sc.examples.clear();
            if trimmed.starts_with("Scenario Outline:") {
                sc.outline = true;
            }
            current = Some(sc);
            continue;
        }

        if current.is_none() && !in_background {
            continue;
        }
        let sc = if in_background && current.is_none() {
            &mut background
        } else {
            current.as_mut().unwrap()
        };
        if sc.skipped_reason.is_some() {
            continue;
        }

        // --- Steps ---
        if let Some(rest) = trimmed.strip_prefix("Given ") {
            match rest.trim() {
                "an empty graph" => sc.given = Some(GivenKind::EmptyGraph),
                "any graph" => sc.given = Some(GivenKind::AnyGraph),
                "the binary-tree-1 graph" => sc.given = Some(GivenKind::BinaryTree1),
                "the binary-tree-2 graph" => sc.given = Some(GivenKind::BinaryTree2),
                _ => sc.skipped_reason = Some("unsupported Given"),
            }
        } else if let Some(rest) = trimmed.strip_prefix("And ") {
            handle_and_or_when(sc, rest, &mut docstring_target, &mut pending_table);
        } else if let Some(rest) = trimmed.strip_prefix("When ") {
            handle_and_or_when(sc, rest, &mut docstring_target, &mut pending_table);
        } else if let Some(rest) = trimmed.strip_prefix("Then ") {
            handle_then(sc, rest, &mut pending_table);
        } else if trimmed.starts_with("Examples:") {
            pending_table = Some(TableTarget::Examples);
        }
    }

    // Flush trailing table.
    if in_table {
        if in_background && current.is_none() {
            flush_table(Some(&mut background), &mut pending_table, &mut table_rows);
        } else {
            flush_table(current.as_mut(), &mut pending_table, &mut table_rows);
        }
    }
    if let Some(sc) = current {
        push_scenario(&mut scenarios, sc);
    }

    scenarios
}

#[derive(Debug, Clone, Copy)]
enum TableTarget {
    Expected { ordered: bool },
    Examples,
    Parameters,
}

#[derive(Debug, Clone, Copy)]
enum DocstringTarget {
    Setup,
    Query,
}

fn handle_and_or_when(
    sc: &mut Scenario,
    rest: &str,
    docstring_target: &mut Option<DocstringTarget>,
    _pending: &mut Option<TableTarget>,
) {
    let t = rest.trim();
    if t.starts_with("having executed:") {
        *docstring_target = Some(DocstringTarget::Setup);
    } else if t.starts_with("executing query:") {
        *docstring_target = Some(DocstringTarget::Query);
    } else if t.starts_with("no side effects") || t.starts_with("the side effects should be") {
        // side-effect assertions silently ignored
    } else if t.starts_with("parameters are") {
        *_pending = Some(TableTarget::Parameters);
    } else if t.starts_with("there exists a procedure") {
        sc.skipped_reason = Some("procedure definition unsupported");
    } else {
        // Unknown And-step: tolerate silently (e.g. further side-effect details).
    }
}

fn handle_then(sc: &mut Scenario, rest: &str, pending_table: &mut Option<TableTarget>) {
    let t = rest.trim();
    if t.starts_with("the result should be, in any order:") {
        *pending_table = Some(TableTarget::Expected { ordered: false });
    } else if t.starts_with("the result should be, in order:") {
        *pending_table = Some(TableTarget::Expected { ordered: true });
    } else if t == "the result should be empty" {
        sc.expected = Some(Expected::Empty);
    } else if t.starts_with("the result should be (ignoring element order for lists):") {
        *pending_table = Some(TableTarget::Expected { ordered: false });
    } else if t.starts_with("a ") && t.contains("should be raised") {
        sc.expected = Some(Expected::Error {
            description: t.to_string(),
        });
    } else {
        sc.skipped_reason = Some("unsupported Then");
    }
}

fn flush_table(
    current: Option<&mut Scenario>,
    pending_table: &mut Option<TableTarget>,
    table_rows: &mut Vec<Vec<String>>,
) {
    let (Some(sc), Some(target)) = (current, pending_table.take()) else {
        return;
    };
    match target {
        TableTarget::Expected { ordered } => {
            if table_rows.is_empty() {
                sc.expected = Some(Expected::Empty);
            } else {
                let columns = table_rows[0].clone();
                let rows = table_rows[1..].to_vec();
                sc.expected = Some(Expected::Rows {
                    columns,
                    rows,
                    ordered,
                });
            }
        }
        TableTarget::Examples => {
            if let Some(headers) = table_rows.first() {
                for row in table_rows.iter().skip(1) {
                    let mut example = BTreeMap::new();
                    for (idx, key) in headers.iter().enumerate() {
                        let value = row.get(idx).cloned().unwrap_or_default();
                        example.insert(key.clone(), value);
                    }
                    sc.examples.push(example);
                }
            }
        }
        TableTarget::Parameters => {
            for row in table_rows {
                if row.len() >= 2 {
                    sc.params.insert(row[0].clone(), row[1].clone());
                }
            }
        }
    }
}

fn push_scenario(out: &mut Vec<Scenario>, mut sc: Scenario) {
    if !sc.outline {
        out.push(sc);
        return;
    }

    if sc.examples.is_empty() {
        sc.skipped_reason = Some("scenario outline examples missing");
        out.push(sc);
        return;
    }

    for (idx, example) in sc.examples.iter().enumerate() {
        let mut expanded = sc.clone();
        expanded.outline = false;
        expanded.examples = Vec::new();
        expanded.name = format!("{} <{}>", expanded.name, idx + 1);
        expanded.setup_queries = expanded
            .setup_queries
            .iter()
            .map(|q| substitute_placeholders(q, example))
            .collect();
        expanded.params = expanded
            .params
            .iter()
            .map(|(key, value)| {
                (
                    substitute_placeholders(key, example),
                    substitute_placeholders(value, example),
                )
            })
            .collect();
        expanded.query = expanded
            .query
            .as_ref()
            .map(|q| substitute_placeholders(q, example));
        expanded.expected = expanded
            .expected
            .as_ref()
            .map(|expected| substitute_expected(expected, example));
        out.push(expanded);
    }
}

fn substitute_expected(expected: &Expected, example: &BTreeMap<String, String>) -> Expected {
    match expected {
        Expected::Empty => Expected::Empty,
        Expected::Error { description } => Expected::Error {
            description: substitute_placeholders(description, example),
        },
        Expected::Rows {
            columns,
            rows,
            ordered,
        } => Expected::Rows {
            columns: columns
                .iter()
                .map(|cell| substitute_placeholders(cell, example))
                .collect(),
            rows: rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|cell| substitute_placeholders(cell, example))
                        .collect()
                })
                .collect(),
            ordered: *ordered,
        },
    }
}

fn substitute_placeholders(input: &str, example: &BTreeMap<String, String>) -> String {
    let mut out = input.to_string();
    for (key, value) in example {
        out = out.replace(&format!("<{key}>"), value);
    }
    out
}

fn apply_docstring(sc: &mut Scenario, target: DocstringTarget, text: String) {
    let trimmed = text.trim_end_matches('\n').to_string();
    match target {
        DocstringTarget::Setup => sc.setup_queries.push(trimmed),
        DocstringTarget::Query => sc.query = Some(trimmed),
    }
}

fn contains_placeholder(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1].is_ascii_alphabetic() {
            return true;
        }
        i += 1;
    }
    false
}

/// Build an empty schemaless graph. For the TCK runner we pre-register a
/// generous set of property keys as Float64 or String variants to survive
/// ad-hoc property use. This is intentionally permissive: the goal is to
/// measure Cypher coverage, not core-graph schema validation.
fn build_permissive_graph() -> Graph {
    let mut g = Graph::new(64, 64);
    g.build();
    g
}

fn build_graph(kind: Option<GivenKind>) -> Graph {
    match kind.unwrap_or(GivenKind::AnyGraph) {
        GivenKind::EmptyGraph | GivenKind::AnyGraph => build_permissive_graph(),
        GivenKind::BinaryTree1 => build_binary_tree_graph(false),
        GivenKind::BinaryTree2 => build_binary_tree_graph(true),
    }
}

fn build_binary_tree_graph(split_c_labels: bool) -> Graph {
    let mut g = Graph::new(32, 64);
    for key in &[
        "name", "id", "value", "age", "count", "weight", "prop", "key", "label", "x", "y", "z",
        "a", "b", "c", "d", "n", "m",
    ] {
        g.register_vertex_property(key, PropertyType::String, false, false);
        g.register_edge_property(key, PropertyType::String, false, false);
    }

    let a = add_named_vertex(&mut g, "A", "a");
    let b1 = add_named_vertex(&mut g, "X", "b1");
    let b2 = add_named_vertex(&mut g, "X", "b2");
    let b3 = add_named_vertex(&mut g, "X", "b3");
    let b4 = add_named_vertex(&mut g, "X", "b4");

    let c11 = add_named_vertex(&mut g, "X", "c11");
    let c12 = add_named_vertex(&mut g, if split_c_labels { "Y" } else { "X" }, "c12");
    let c21 = add_named_vertex(&mut g, "X", "c21");
    let c22 = add_named_vertex(&mut g, if split_c_labels { "Y" } else { "X" }, "c22");
    let c31 = add_named_vertex(&mut g, "X", "c31");
    let c32 = add_named_vertex(&mut g, if split_c_labels { "Y" } else { "X" }, "c32");
    let c41 = add_named_vertex(&mut g, "X", "c41");
    let c42 = add_named_vertex(&mut g, if split_c_labels { "Y" } else { "X" }, "c42");

    g.add_edge(a, b1, "KNOWS");
    g.add_edge(a, b2, "KNOWS");
    g.add_edge(a, b3, "FOLLOWS");
    g.add_edge(a, b4, "FOLLOWS");

    g.add_edge(b1, c11, "FRIEND");
    g.add_edge(b1, c12, "FRIEND");
    g.add_edge(b2, c21, "FRIEND");
    g.add_edge(b2, c22, "FRIEND");
    g.add_edge(b3, c31, "FRIEND");
    g.add_edge(b3, c32, "FRIEND");
    g.add_edge(b4, c41, "FRIEND");
    g.add_edge(b4, c42, "FRIEND");

    g.add_edge(b1, b2, "FRIEND");
    g.add_edge(b2, b3, "FRIEND");
    g.add_edge(b3, b4, "FRIEND");
    g.add_edge(b4, b1, "FRIEND");

    g.build();
    g
}

fn add_named_vertex(graph: &mut Graph, label: &str, name: &str) -> nexus_core::types::VertexId {
    let vertex = graph.add_vertex(label);
    graph.set_vertex_property(vertex, "name", Value::String(name.into()));
    vertex
}

fn register_properties_for_scenario(graph: &mut Graph, sc: &Scenario) {
    for query in sc.setup_queries.iter().chain(sc.query.iter()) {
        for (key, value) in property_literals(query) {
            let property_type = value_to_property_type(&value);
            graph.register_vertex_property(&key, property_type, false, false);
            graph.register_edge_property(&key, property_type, false, false);
        }
    }
}

fn property_literals(query: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let bytes = query.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] != b'{' {
            idx += 1;
            continue;
        }
        let Some(end) = find_matching_brace(query, idx) else {
            break;
        };
        let map = &query[idx..=end];
        if let Value::Map(entries) = parse_tck_value(map) {
            out.extend(entries);
        }
        idx = end + 1;
    }
    out
}

fn find_matching_brace(input: &str, start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut prev_escape = false;
    for (idx, ch) in input.char_indices().skip_while(|(idx, _)| *idx < start) {
        if in_string {
            if ch == '\'' && !prev_escape {
                in_string = false;
            }
            prev_escape = ch == '\\' && !prev_escape;
            if ch != '\\' {
                prev_escape = false;
            }
            continue;
        }
        match ch {
            '\'' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn value_to_property_type(value: &Value) -> PropertyType {
    match value {
        Value::Bool(_) => PropertyType::Bool,
        Value::Int64(_) => PropertyType::Int64,
        Value::Float64(_) => PropertyType::Float64,
        Value::Bytes(_) => PropertyType::Bytes,
        _ => PropertyType::String,
    }
}

/// Run one scenario. Returns which stats counters to bump.
fn run_scenario(sc: &Scenario) -> ScenarioOutcome {
    run_scenario_inner(sc)
}

fn run_scenario_inner(sc: &Scenario) -> ScenarioOutcome {
    if sc.skipped_reason.is_some() || sc.query.is_none() {
        return ScenarioOutcome::Skipped;
    }
    // Scenario-outline placeholders like `<pattern>`, `<dir>`, `<invalid>`
    // are not executable Cypher. Skip any query/setup that contains them.
    if sc.setup_queries.iter().any(|q| contains_placeholder(q))
        || sc.query.as_deref().is_some_and(contains_placeholder)
    {
        return ScenarioOutcome::Skipped;
    }

    let mut graph = build_graph(sc.given);
    register_properties_for_scenario(&mut graph, sc);
    let params = parse_params(&sc.params);

    // Run setup queries (best-effort: if any fails, the scenario isn't "parse_ok").
    for setup in &sc.setup_queries {
        match run_cypher_mut_in_memory_with_params(setup, &mut graph, params.clone()) {
            Ok(_) => {}
            Err(_) => return ScenarioOutcome::ParseOrSetupErr,
        }
    }

    let query = sc.query.as_deref().unwrap();
    match run_cypher_mut_in_memory_with_params(query, &mut graph, params) {
        Err(e) => {
            if matches!(sc.expected, Some(Expected::Error { .. })) {
                return ScenarioOutcome::ExpectedErrorOk;
            }
            let msg = e.to_string();
            if msg.contains("parse error")
                || msg.contains("unexpected token")
                || msg.contains("unknown")
            {
                ScenarioOutcome::ParseErr
            } else {
                ScenarioOutcome::ExecErr
            }
        }
        Ok(run_result) => {
            if matches!(sc.expected, Some(Expected::Error { .. })) {
                return ScenarioOutcome::ResultMismatch;
            }
            let Some(expected) = sc.expected.as_ref() else {
                return ScenarioOutcome::ExecOk;
            };
            let qr = match run_result {
                RunResult::Read(qr) => qr,
                RunResult::Write(_) => {
                    return ScenarioOutcome::ExecOk;
                }
            };
            let outcome = compare_result(expected, &qr, &graph);
            debug_mismatch(sc, expected, &qr, &outcome);
            outcome
        }
    }
}

fn debug_mismatch(
    sc: &Scenario,
    expected: &Expected,
    actual: &nexus_cypher::executor::QueryResult,
    outcome: &ScenarioOutcome,
) {
    if !matches!(outcome, ScenarioOutcome::ResultMismatch) {
        return;
    }
    let Ok(raw_limit) = std::env::var("TCK_DEBUG_MISMATCHES") else {
        return;
    };
    let limit = raw_limit.parse::<usize>().unwrap_or(10);
    let idx = DEBUG_MISMATCH_COUNT.fetch_add(1, Ordering::Relaxed);
    if idx >= limit {
        return;
    }
    eprintln!("\n--- TCK mismatch #{}: {} ---", idx + 1, sc.name);
    if let Some(query) = &sc.query {
        eprintln!("query:\n{query}");
    }
    eprintln!("expected: {expected:?}");
    eprintln!("actual columns: {:?}", actual.columns);
    eprintln!("actual rows: {:?}", actual.rows);
}

fn parse_params(raw: &BTreeMap<String, String>) -> HashMap<String, Value> {
    raw.iter()
        .map(|(key, value)| (key.clone(), parse_tck_value(value)))
        .collect()
}

fn parse_tck_value(raw: &str) -> Value {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("null") {
        return Value::Null;
    }
    if raw.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if raw.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if let Some(value) = parse_quoted_string(raw) {
        return Value::String(value);
    }
    if raw.starts_with('[') && raw.ends_with(']') {
        let inner = &raw[1..raw.len() - 1];
        if inner.trim().is_empty() {
            return Value::List(Vec::new());
        }
        return Value::List(
            split_top_level(inner, ',')
                .into_iter()
                .map(|part| parse_tck_value(part))
                .collect(),
        );
    }
    if raw.starts_with('{') && raw.ends_with('}') {
        let inner = &raw[1..raw.len() - 1];
        if inner.trim().is_empty() {
            return Value::Map(Vec::new());
        }
        let entries = split_top_level(inner, ',')
            .into_iter()
            .filter_map(|part| {
                let colon = find_top_level(part, ':')?;
                let key = part[..colon].trim();
                let value = part[colon + 1..].trim();
                Some((parse_map_key(key), parse_tck_value(value)))
            })
            .collect();
        return Value::Map(entries);
    }
    if let Ok(value) = raw.parse::<i64>() {
        return Value::Int64(value);
    }
    if let Ok(value) = raw.parse::<f64>() {
        return Value::Float64(value);
    }
    Value::String(raw.to_string())
}

fn parse_quoted_string(raw: &str) -> Option<String> {
    if raw.len() < 2 || !raw.starts_with('\'') || !raw.ends_with('\'') {
        return None;
    }
    let inner = &raw[1..raw.len() - 1];
    Some(inner.replace("\\'", "'"))
}

fn parse_map_key(raw: &str) -> String {
    parse_quoted_string(raw).unwrap_or_else(|| raw.to_string())
}

fn split_top_level(input: &str, delimiter: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut prev_escape = false;

    for (idx, ch) in input.char_indices() {
        if in_string {
            if ch == '\'' && !prev_escape {
                in_string = false;
            }
            prev_escape = ch == '\\' && !prev_escape;
            if ch != '\\' {
                prev_escape = false;
            }
            continue;
        }

        match ch {
            '\'' => in_string = true,
            '[' | '{' | '(' => depth += 1,
            ']' | '}' | ')' => depth -= 1,
            c if c == delimiter && depth == 0 => {
                parts.push(input[start..idx].trim());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(input[start..].trim());
    parts
}

fn find_top_level(input: &str, needle: char) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut prev_escape = false;

    for (idx, ch) in input.char_indices() {
        if in_string {
            if ch == '\'' && !prev_escape {
                in_string = false;
            }
            prev_escape = ch == '\\' && !prev_escape;
            if ch != '\\' {
                prev_escape = false;
            }
            continue;
        }

        match ch {
            '\'' => in_string = true,
            '[' | '{' | '(' => depth += 1,
            ']' | '}' | ')' => depth -= 1,
            c if c == needle && depth == 0 => return Some(idx),
            _ => {}
        }
    }
    None
}

enum ScenarioOutcome {
    Skipped,
    ParseErr,
    ParseOrSetupErr,
    ExecErr,
    ExecOk,          // ran, no expected or not compared
    ResultOk,        // result matched
    ResultMismatch,  // result differed
    VertexResults,   // expected contained node/edge literals
    ExpectedErrorOk, // query failed as requested by TCK
}

fn compare_result(
    expected: &Expected,
    actual: &nexus_cypher::executor::QueryResult,
    graph: &Graph,
) -> ScenarioOutcome {
    match expected {
        Expected::Empty => {
            if actual.rows.is_empty() {
                ScenarioOutcome::ResultOk
            } else {
                ScenarioOutcome::ResultMismatch
            }
        }
        Expected::Rows {
            columns,
            rows,
            ordered,
        } => {
            // Path values are not represented in QueryResult yet. Node and
            // relationship literals are compared graph-aware below.
            if rows.iter().flatten().any(|cell| is_path_literal_cell(cell)) {
                return ScenarioOutcome::VertexResults;
            }
            if !columns.is_empty() && columns != &actual.columns {
                return ScenarioOutcome::ResultMismatch;
            }

            let rows_match = if *ordered {
                rows.len() == actual.rows.len()
                    && rows
                        .iter()
                        .zip(&actual.rows)
                        .all(|(expected_row, actual_row)| {
                            row_matches(expected_row, actual_row, graph)
                        })
            } else {
                unordered_rows_match(rows, &actual.rows, graph)
            };

            if rows_match {
                ScenarioOutcome::ResultOk
            } else {
                ScenarioOutcome::ResultMismatch
            }
        }
        Expected::Error { .. } => ScenarioOutcome::ResultMismatch,
    }
}

/// Format a Value as Cypher-literal-like string to match TCK table cells.
fn format_value(v: &Value) -> String {
    match v {
        Value::Int64(n) => n.to_string(),
        Value::Float64(f) => {
            // TCK uses integer-looking floats like "1.0", not "1"
            if f.fract() == 0.0 {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        Value::String(s) => format!("'{s}'"),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Bytes(_) => "<bytes>".to_string(),
        Value::List(items) => {
            let inner: Vec<String> = items.iter().map(format_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Map(entries) => {
            let inner: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{k}: {}", format_value(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

fn row_matches(expected: &[String], actual: &[Value], graph: &Graph) -> bool {
    expected.len() == actual.len()
        && expected
            .iter()
            .zip(actual)
            .all(|(expected_cell, actual_value)| cell_matches(expected_cell, actual_value, graph))
}

fn unordered_rows_match(
    expected_rows: &[Vec<String>],
    actual_rows: &[Vec<Value>],
    graph: &Graph,
) -> bool {
    if expected_rows.len() != actual_rows.len() {
        return false;
    }

    let mut used = vec![false; actual_rows.len()];
    for expected in expected_rows {
        let Some(idx) = actual_rows.iter().enumerate().find_map(|(idx, actual)| {
            if !used[idx] && row_matches(expected, actual, graph) {
                Some(idx)
            } else {
                None
            }
        }) else {
            return false;
        };
        used[idx] = true;
    }
    true
}

fn cell_matches(expected: &str, actual: &Value, graph: &Graph) -> bool {
    let expected = expected.trim();
    if is_node_literal_cell(expected) {
        return matches_node_literal(expected, actual, graph);
    }
    if is_relationship_literal_cell(expected) {
        return matches_relationship_literal(expected, actual, graph);
    }
    if is_relationship_list_literal_cell(expected) {
        return matches_relationship_list_literal(expected, actual, graph);
    }
    canonical_cell(expected) == canonical_cell(&format_value(actual))
}

fn is_node_literal_cell(cell: &str) -> bool {
    let cell = cell.trim();
    cell.starts_with('(') && cell.ends_with(')') && !cell.contains("-[") && !cell.contains("]-")
}

fn is_relationship_literal_cell(cell: &str) -> bool {
    let cell = cell.trim();
    cell.starts_with("[:")
        || (cell.starts_with('[')
            && cell.ends_with(']')
            && !cell.starts_with("[[")
            && cell.contains(':')
            && !cell.contains(','))
}

fn is_relationship_list_literal_cell(cell: &str) -> bool {
    let cell = cell.trim();
    cell.starts_with("[[") && cell.ends_with("]]") && cell.contains("[:")
}

fn is_path_literal_cell(cell: &str) -> bool {
    let cell = cell.trim();
    cell.contains("-[") || cell.contains("]->") || cell.contains("<-[")
}

fn matches_node_literal(expected: &str, actual: &Value, graph: &Graph) -> bool {
    let Value::Int64(id) = actual else {
        return false;
    };
    let vertex = VertexId(*id as u64);
    let Some(actual_label) = graph.vertex_label(vertex) else {
        return false;
    };
    let inner = expected
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    let (labels, props) = parse_graph_literal_parts(inner);
    if !labels.is_empty() && !labels.iter().any(|label| label == actual_label) {
        return false;
    }
    props_match(&props, &graph.get_vertex_properties(vertex))
}

fn matches_relationship_literal(expected: &str, actual: &Value, graph: &Graph) -> bool {
    let Value::Int64(id) = actual else {
        return false;
    };
    let edge = EdgeId(*id as u64);
    let Some(actual_label) = graph.edge_label(edge) else {
        return false;
    };
    let inner = expected
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    let (labels, props) = parse_graph_literal_parts(inner);
    if !labels.is_empty() && !labels.iter().any(|label| label == actual_label) {
        return false;
    }
    props_match(&props, &graph.get_edge_properties(edge))
}

fn matches_relationship_list_literal(expected: &str, actual: &Value, graph: &Graph) -> bool {
    let Value::List(items) = actual else {
        return false;
    };
    let inner = expected
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if inner.trim().is_empty() {
        return items.is_empty();
    }
    let expected_items = split_top_level(inner, ',');
    expected_items.len() == items.len()
        && expected_items
            .iter()
            .zip(items)
            .all(|(expected_item, actual_item)| {
                matches_relationship_literal(expected_item, actual_item, graph)
            })
}

fn parse_graph_literal_parts(inner: &str) -> (Vec<String>, Vec<(String, Value)>) {
    let prop_start = inner.find('{');
    let prop_end = inner.rfind('}');
    let label_part = prop_start.map_or(inner, |idx| &inner[..idx]);
    let props = match (prop_start, prop_end) {
        (Some(start), Some(end)) if end > start => parse_tck_value(&inner[start..=end]),
        _ => Value::Map(Vec::new()),
    };

    let labels = label_part
        .split(':')
        .skip(1)
        .filter_map(|part| {
            let label = part
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches('`')
                .trim();
            if label.is_empty() {
                None
            } else {
                Some(label.to_string())
            }
        })
        .collect();
    let props = match props {
        Value::Map(entries) => entries,
        _ => Vec::new(),
    };
    (labels, props)
}

fn props_match(expected: &[(String, Value)], actual: &[(String, Value)]) -> bool {
    expected.iter().all(|(key, expected_value)| {
        actual
            .iter()
            .find(|(actual_key, _)| actual_key == key)
            .is_some_and(|(_, actual_value)| actual_value == expected_value)
    })
}

fn canonical_cell(cell: &str) -> String {
    let mut out = String::new();
    let mut in_string = false;
    let mut prev_escape = false;

    for ch in cell.trim().chars() {
        if in_string {
            out.push(ch);
            if ch == '\'' && !prev_escape {
                in_string = false;
            }
            prev_escape = ch == '\\' && !prev_escape;
            if ch != '\\' {
                prev_escape = false;
            }
            continue;
        }

        if ch == '\'' {
            in_string = true;
            out.push(ch);
        } else if !ch.is_whitespace() {
            out.push(ch);
        }
    }

    out
}

fn category_of(path: &Path) -> String {
    // e.g. .../features/clauses/match/Match1.feature -> "clauses/match"
    let rel = path.strip_prefix(tck_root()).unwrap_or(path);
    let comps: Vec<&str> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if comps.len() >= 2 {
        format!("{}/{}", comps[0], comps[1])
    } else {
        comps
            .first()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "root".to_string())
    }
}

#[test]
fn tck_report() {
    let root = tck_root();
    if !root.exists() {
        eprintln!("TCK root not found at {} — skipping", root.display());
        return;
    }
    let filter = std::env::var("TCK_CATEGORY").ok();
    let features = discover_feature_files(&root, filter.as_deref());

    let mut per_category: BTreeMap<String, Stats> = BTreeMap::new();
    let mut totals = Stats::default();

    for path in &features {
        let Ok(contents) = fs::read_to_string(path) else {
            continue;
        };
        let scenarios = parse_feature(&contents);
        let cat = category_of(path);
        let entry = per_category.entry(cat).or_default();

        for sc in &scenarios {
            entry.total += 1;
            totals.total += 1;
            match run_scenario(sc) {
                ScenarioOutcome::Skipped => {
                    entry.skipped += 1;
                    totals.skipped += 1;
                }
                ScenarioOutcome::ParseErr => {
                    entry.parse_err += 1;
                    totals.parse_err += 1;
                }
                ScenarioOutcome::ParseOrSetupErr => {
                    entry.parse_err += 1;
                    totals.parse_err += 1;
                }
                ScenarioOutcome::ExecErr => {
                    entry.parse_ok += 1;
                    entry.exec_err += 1;
                    totals.parse_ok += 1;
                    totals.exec_err += 1;
                }
                ScenarioOutcome::ExecOk => {
                    entry.parse_ok += 1;
                    entry.exec_ok += 1;
                    totals.parse_ok += 1;
                    totals.exec_ok += 1;
                }
                ScenarioOutcome::ResultOk => {
                    entry.parse_ok += 1;
                    entry.exec_ok += 1;
                    entry.result_ok += 1;
                    totals.parse_ok += 1;
                    totals.exec_ok += 1;
                    totals.result_ok += 1;
                }
                ScenarioOutcome::ResultMismatch => {
                    entry.parse_ok += 1;
                    entry.exec_ok += 1;
                    entry.result_mismatch += 1;
                    totals.parse_ok += 1;
                    totals.exec_ok += 1;
                    totals.result_mismatch += 1;
                }
                ScenarioOutcome::VertexResults => {
                    entry.parse_ok += 1;
                    entry.exec_ok += 1;
                    entry.vertex_results += 1;
                    totals.parse_ok += 1;
                    totals.exec_ok += 1;
                    totals.vertex_results += 1;
                }
                ScenarioOutcome::ExpectedErrorOk => {
                    entry.result_ok += 1;
                    entry.expected_error_ok += 1;
                    totals.result_ok += 1;
                    totals.expected_error_ok += 1;
                }
            }
        }
    }

    print_report(&per_category, &totals);
}

fn print_report(per_category: &BTreeMap<String, Stats>, totals: &Stats) {
    eprintln!();
    eprintln!("=== openCypher TCK report — nexus-cypher ===");
    eprintln!();
    eprintln!(
        "{:<30} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6}",
        "category",
        "total",
        "skip",
        "parseE",
        "execE",
        "parseOK",
        "execOK",
        "mismch",
        "errOK",
        "RESULT✓"
    );
    eprintln!("{}", "-".repeat(109));
    for (cat, s) in per_category {
        eprintln!(
            "{:<30} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6}",
            cat,
            s.total,
            s.skipped,
            s.parse_err,
            s.exec_err,
            s.parse_ok,
            s.exec_ok,
            s.result_mismatch,
            s.expected_error_ok,
            s.result_ok,
        );
    }
    eprintln!("{}", "-".repeat(109));
    eprintln!(
        "{:<30} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6}",
        "TOTAL",
        totals.total,
        totals.skipped,
        totals.parse_err,
        totals.exec_err,
        totals.parse_ok,
        totals.exec_ok,
        totals.result_mismatch,
        totals.expected_error_ok,
        totals.result_ok,
    );
    let considered = totals.total - totals.skipped;
    if considered > 0 {
        let pct = |n: usize| (n * 100) as f64 / considered as f64;
        eprintln!();
        eprintln!(
            "considered (non-skipped): {}, parse-ok {:.1}%, exec-ok {:.1}%, result-match {:.1}%",
            considered,
            pct(totals.parse_ok),
            pct(totals.exec_ok),
            pct(totals.result_ok),
        );
        eprintln!(
            "note: {} scenarios had node/edge literals in expected output (not yet compared)",
            totals.vertex_results,
        );
    }
    print_json_report(per_category, totals, considered);
    eprintln!();
}

#[derive(Debug, Serialize)]
struct TckJsonReport<'a> {
    total: usize,
    considered: usize,
    skipped: usize,
    parse_ok: usize,
    exec_ok: usize,
    result_ok: usize,
    result_mismatch: usize,
    parse_err: usize,
    exec_err: usize,
    expected_error_ok: usize,
    rates: TckRates,
    by_category: &'a BTreeMap<String, Stats>,
    top_failure_buckets: Vec<FailureBucket>,
}

#[derive(Debug, Serialize)]
struct TckRates {
    parse_ok_pct: f64,
    exec_ok_pct: f64,
    result_ok_pct: f64,
}

#[derive(Debug, Serialize)]
struct FailureBucket {
    category: String,
    failures: usize,
    parse_err: usize,
    exec_err: usize,
    result_mismatch: usize,
}

fn print_json_report(per_category: &BTreeMap<String, Stats>, totals: &Stats, considered: usize) {
    let rate = |n: usize| {
        if considered == 0 {
            0.0
        } else {
            (n * 100) as f64 / considered as f64
        }
    };

    let mut top_failure_buckets: Vec<FailureBucket> = per_category
        .iter()
        .map(|(category, stats)| FailureBucket {
            category: category.clone(),
            failures: stats.parse_err + stats.exec_err + stats.result_mismatch,
            parse_err: stats.parse_err,
            exec_err: stats.exec_err,
            result_mismatch: stats.result_mismatch,
        })
        .filter(|bucket| bucket.failures > 0)
        .collect();
    top_failure_buckets.sort_by(|a, b| {
        b.failures
            .cmp(&a.failures)
            .then_with(|| a.category.cmp(&b.category))
    });
    top_failure_buckets.truncate(10);

    let report = TckJsonReport {
        total: totals.total,
        considered,
        skipped: totals.skipped,
        parse_ok: totals.parse_ok,
        exec_ok: totals.exec_ok,
        result_ok: totals.result_ok,
        result_mismatch: totals.result_mismatch,
        parse_err: totals.parse_err,
        exec_err: totals.exec_err,
        expected_error_ok: totals.expected_error_ok,
        rates: TckRates {
            parse_ok_pct: rate(totals.parse_ok),
            exec_ok_pct: rate(totals.exec_ok),
            result_ok_pct: rate(totals.result_ok),
        },
        by_category: per_category,
        top_failure_buckets,
    };

    let compact = serde_json::to_string(&report).expect("TCK JSON report should serialize");
    eprintln!("TCK_JSON_SUMMARY {compact}");

    if let Ok(path) = std::env::var("TCK_JSON_OUT") {
        let pretty =
            serde_json::to_string_pretty(&report).expect("TCK JSON report should serialize");
        fs::write(path, pretty).expect("TCK_JSON_OUT should be writable");
    }
}
