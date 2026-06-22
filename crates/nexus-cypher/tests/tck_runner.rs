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
//! iterating on a feature area. Set `TCK_FEATURE_ROOT=/path/to/features` to
//! run against a different openCypher feature corpus.

#![allow(clippy::too_many_lines)]

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    control_query: Option<String>,
    control_expected: Option<Expected>,
    expected_side_effects: Option<BTreeMap<String, i64>>,
    tags: Vec<String>,
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
        unordered_list_elements: bool,
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
    result_ok: usize, // result matched expected
    result_mismatch: usize, // parsed+executed, expected primitive cells, result wrong
    exec_err: usize,  // parsed but execution errored
    parse_err: usize, // parser rejected
    expected_error_ok: usize, // query failed as requested by a TCK error scenario
}

const TCK_ROOT_REL: &str = "../../references/falkordb/tests/tck/features";
static DEBUG_MISMATCH_COUNT: AtomicUsize = AtomicUsize::new(0);

fn default_tck_root() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest).join(TCK_ROOT_REL)
}

fn tck_root() -> PathBuf {
    std::env::var("TCK_FEATURE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_tck_root())
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
    parse_feature_with_options(contents, ParseFeatureOptions::default())
}

#[derive(Debug, Clone, Copy, Default)]
struct ParseFeatureOptions {
    include_upstream_skipped_tags: bool,
}

fn parse_feature_with_options(contents: &str, options: ParseFeatureOptions) -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    let mut current: Option<Scenario> = None;
    let mut background = Scenario::default();
    let mut pending_tags = Vec::new();
    let mut in_background = false;
    let mut in_docstring = false;
    let mut docstring_target: Option<DocstringTarget> = None;
    let mut docstring_buf = String::new();
    let mut in_table = false;
    let mut pending_table: Option<TableTarget> = None;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut result_target = ResultTarget::Main;

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
                .map(|c| unescape_gherkin_table_cell(c.trim()))
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
            pending_tags.clear();
            continue;
        }

        if trimmed.starts_with('@') {
            pending_tags.extend(
                trimmed
                    .split_whitespace()
                    .filter(|tag| tag.starts_with('@'))
                    .map(str::to_string),
            );
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
            sc.tags = std::mem::take(&mut pending_tags);
            if trimmed.starts_with("Scenario Outline:") {
                sc.outline = true;
            }
            if has_skip_tag(&sc.tags) && !options.include_upstream_skipped_tags {
                sc.skipped_reason = Some("scenario tagged skip");
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
            handle_and_or_when(
                sc,
                rest,
                &mut docstring_target,
                &mut pending_table,
                &mut result_target,
            );
        } else if let Some(rest) = trimmed.strip_prefix("When ") {
            handle_and_or_when(
                sc,
                rest,
                &mut docstring_target,
                &mut pending_table,
                &mut result_target,
            );
        } else if let Some(rest) = trimmed.strip_prefix("Then ") {
            handle_then(sc, rest, &mut pending_table, result_target);
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

fn unescape_gherkin_table_cell(cell: &str) -> String {
    let mut out = String::with_capacity(cell.len());
    let mut chars = cell.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('|') => out.push('|'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[derive(Debug, Clone, Copy)]
enum TableTarget {
    Expected {
        ordered: bool,
        unordered_list_elements: bool,
        target: ResultTarget,
    },
    SideEffects,
    Examples,
    Parameters,
}

#[derive(Debug, Clone, Copy)]
enum DocstringTarget {
    Setup,
    Query,
    ControlQuery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultTarget {
    Main,
    Control,
}

fn handle_and_or_when(
    sc: &mut Scenario,
    rest: &str,
    docstring_target: &mut Option<DocstringTarget>,
    _pending: &mut Option<TableTarget>,
    result_target: &mut ResultTarget,
) {
    let t = rest.trim();
    if t.starts_with("having executed:") {
        *docstring_target = Some(DocstringTarget::Setup);
    } else if t.starts_with("executing query:") {
        *docstring_target = Some(DocstringTarget::Query);
        *result_target = ResultTarget::Main;
    } else if t.starts_with("executing control query:") {
        *docstring_target = Some(DocstringTarget::ControlQuery);
        *result_target = ResultTarget::Control;
    } else if t.starts_with("no side effects") {
        sc.expected_side_effects = Some(BTreeMap::new());
    } else if t.starts_with("the side effects should be") {
        *_pending = Some(TableTarget::SideEffects);
    } else if t.starts_with("parameters are") {
        *_pending = Some(TableTarget::Parameters);
    } else if t.starts_with("there exists a procedure") {
        sc.skipped_reason = Some("procedure definition unsupported");
    } else {
        // Unknown And-step: tolerate silently (e.g. further side-effect details).
    }
}

fn handle_then(
    sc: &mut Scenario,
    rest: &str,
    pending_table: &mut Option<TableTarget>,
    result_target: ResultTarget,
) {
    let t = rest.trim();
    if t.starts_with("the result should be, in any order:") {
        *pending_table = Some(TableTarget::Expected {
            ordered: false,
            unordered_list_elements: false,
            target: result_target,
        });
    } else if t.starts_with("the result should be, in order:") {
        *pending_table = Some(TableTarget::Expected {
            ordered: true,
            unordered_list_elements: false,
            target: result_target,
        });
    } else if t == "the result should be empty" {
        set_expected(sc, result_target, Expected::Empty);
    } else if t.starts_with("the result should be (ignoring element order for lists):") {
        *pending_table = Some(TableTarget::Expected {
            ordered: false,
            unordered_list_elements: true,
            target: result_target,
        });
    } else if t.starts_with("a ") && t.contains("should be raised") {
        set_expected(
            sc,
            result_target,
            Expected::Error {
                description: t.to_string(),
            },
        );
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
        TableTarget::Expected {
            ordered,
            unordered_list_elements,
            target,
        } => {
            if table_rows.is_empty() {
                set_expected(sc, target, Expected::Empty);
            } else {
                let columns = table_rows[0].clone();
                let rows = table_rows[1..].to_vec();
                set_expected(
                    sc,
                    target,
                    Expected::Rows {
                        columns,
                        rows,
                        ordered,
                        unordered_list_elements,
                    },
                );
            }
        }
        TableTarget::SideEffects => {
            let mut effects = BTreeMap::new();
            for row in table_rows {
                if row.len() >= 2 {
                    let key = row[0].trim().to_string();
                    let count = row[1].trim().parse::<i64>().unwrap_or(0);
                    effects.insert(key, count);
                }
            }
            sc.expected_side_effects = Some(effects);
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

fn set_expected(sc: &mut Scenario, target: ResultTarget, expected: Expected) {
    match target {
        ResultTarget::Main => sc.expected = Some(expected),
        ResultTarget::Control => sc.control_expected = Some(expected),
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
        expanded.control_query = expanded
            .control_query
            .as_ref()
            .map(|q| substitute_placeholders(q, example));
        expanded.expected = expanded
            .expected
            .as_ref()
            .map(|expected| substitute_expected(expected, example));
        expanded.control_expected = expanded
            .control_expected
            .as_ref()
            .map(|expected| substitute_expected(expected, example));
        expanded.expected_side_effects = expanded.expected_side_effects.as_ref().map(|effects| {
            effects
                .iter()
                .map(|(key, value)| {
                    let key = substitute_placeholders(key, example);
                    let value = substitute_placeholders(&value.to_string(), example)
                        .parse()
                        .unwrap_or(*value);
                    (key, value)
                })
                .collect()
        });
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
            unordered_list_elements,
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
            unordered_list_elements: *unordered_list_elements,
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
        DocstringTarget::ControlQuery => sc.control_query = Some(trimmed),
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

fn has_skip_tag(tags: &[String]) -> bool {
    tags.iter().any(|tag| {
        let normalized = tag.trim_start_matches('@').to_ascii_lowercase();
        normalized.starts_with("skip") || normalized.starts_with("ignore")
    })
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
    let mut inferred = BTreeMap::new();
    for query in sc.setup_queries.iter().chain(sc.query.iter()) {
        for (key, property_type) in property_literals(query) {
            inferred
                .entry(key)
                .and_modify(|existing| {
                    if *existing != property_type {
                        *existing = PropertyType::Any;
                    }
                })
                .or_insert(property_type);
        }
    }
    for (key, property_type) in inferred {
        graph.register_vertex_property(&key, property_type, false, false);
        graph.register_edge_property(&key, property_type, false, false);
    }
}

fn property_literals(query: &str) -> Vec<(String, PropertyType)> {
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
        for (key, raw_value) in raw_map_entries(map) {
            out.push((key, property_type_from_literal(raw_value)));
        }
        idx = end + 1;
    }
    out
}

fn raw_map_entries(map: &str) -> Vec<(String, &str)> {
    let inner = map.trim().trim_start_matches('{').trim_end_matches('}');
    if inner.trim().is_empty() {
        return Vec::new();
    }
    split_top_level(inner, ',')
        .into_iter()
        .filter_map(|part| {
            let colon = find_top_level(part, ':')?;
            let key = parse_map_key(part[..colon].trim());
            let raw_value = part[colon + 1..].trim();
            Some((key, raw_value))
        })
        .collect()
}

fn property_type_from_literal(raw: &str) -> PropertyType {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("true") || raw.eq_ignore_ascii_case("false") {
        return PropertyType::Bool;
    }
    if parse_quoted_string(raw).is_some() {
        return PropertyType::String;
    }
    if raw.eq_ignore_ascii_case("null") || raw.starts_with('[') || raw.starts_with('{') {
        return PropertyType::Any;
    }
    if raw.parse::<i64>().is_ok() {
        return PropertyType::Int64;
    }
    if raw.parse::<f64>().is_ok() {
        return PropertyType::Float64;
    }
    PropertyType::Any
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
        || sc
            .control_query
            .as_deref()
            .is_some_and(contains_placeholder)
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
            Err(e) => {
                debug_error(sc, "setup_error", &e.to_string());
                return ScenarioOutcome::ParseOrSetupErr;
            }
        }
    }

    let query = sc.query.as_deref().unwrap();
    let before_effects = EffectSnapshot::capture(&graph);

    match run_cypher_mut_in_memory_with_params(query, &mut graph, params.clone()) {
        Err(e) => {
            if matches!(sc.expected, Some(Expected::Error { .. })) {
                return ScenarioOutcome::ExpectedErrorOk;
            }
            let msg = e.to_string();
            if msg.contains("parse error")
                || msg.contains("unexpected token")
                || msg.contains("unknown")
            {
                debug_error(sc, "parse_error", &msg);
                ScenarioOutcome::ParseErr
            } else {
                debug_error(sc, "exec_error", &msg);
                ScenarioOutcome::ExecErr
            }
        }
        Ok(run_result) => {
            if matches!(sc.expected, Some(Expected::Error { .. })) {
                debug_expected_error_mismatch(sc, &run_result);
                return ScenarioOutcome::ResultMismatch;
            }
            let after_effects = EffectSnapshot::capture(&graph);
            let effects_ok =
                compare_side_effects(sc, &before_effects, &after_effects).unwrap_or(true);

            let main_outcome = match (&sc.expected, run_result) {
                (Some(expected), RunResult::Read(qr)) => {
                    let mut outcome = compare_result(expected, &qr, &graph);
                    if matches!(outcome, ScenarioOutcome::ResultOk) && !effects_ok {
                        outcome = ScenarioOutcome::ResultMismatch;
                    }
                    debug_mismatch(sc, expected, &qr, &outcome);
                    outcome
                }
                (Some(Expected::Empty), RunResult::Write(_)) => {
                    if effects_ok {
                        ScenarioOutcome::ResultOk
                    } else {
                        ScenarioOutcome::ResultMismatch
                    }
                }
                (None, RunResult::Write(_)) if sc.expected_side_effects.is_some() => {
                    if effects_ok {
                        ScenarioOutcome::ResultOk
                    } else {
                        ScenarioOutcome::ResultMismatch
                    }
                }
                (None, _) => {
                    debug_uncompared(sc, "no expected rows or side effects");
                    ScenarioOutcome::ExecOk
                }
                _ => {
                    debug_uncompared(sc, "result type not comparable by current harness");
                    ScenarioOutcome::ExecOk
                }
            };

            if !matches!(
                main_outcome,
                ScenarioOutcome::ResultOk | ScenarioOutcome::ExecOk
            ) {
                return main_outcome;
            }

            let Some(control_query) = sc.control_query.as_deref() else {
                return main_outcome;
            };
            let control_expected = sc.control_expected.as_ref();
            match run_cypher_mut_in_memory_with_params(control_query, &mut graph, params) {
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("parse error")
                        || msg.contains("unexpected token")
                        || msg.contains("unknown")
                    {
                        debug_error(sc, "control_parse_error", &msg);
                        ScenarioOutcome::ParseErr
                    } else {
                        debug_error(sc, "control_exec_error", &msg);
                        ScenarioOutcome::ExecErr
                    }
                }
                Ok(RunResult::Read(qr)) => {
                    let Some(expected) = control_expected else {
                        debug_uncompared(sc, "control query has no expected result");
                        return main_outcome;
                    };
                    let outcome = compare_result(expected, &qr, &graph);
                    debug_mismatch(sc, expected, &qr, &outcome);
                    if matches!(outcome, ScenarioOutcome::ResultOk) {
                        ScenarioOutcome::ResultOk
                    } else {
                        outcome
                    }
                }
                Ok(run_result) => {
                    debug_expected_error_mismatch(sc, &run_result);
                    ScenarioOutcome::ResultMismatch
                }
            }
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

fn debug_expected_error_mismatch(sc: &Scenario, actual: &RunResult) {
    let Ok(raw_limit) = std::env::var("TCK_DEBUG_EXPECTED_ERRORS") else {
        return;
    };
    let limit = raw_limit.parse::<usize>().unwrap_or(10);
    let idx = DEBUG_MISMATCH_COUNT.fetch_add(1, Ordering::Relaxed);
    if idx >= limit {
        return;
    }
    eprintln!(
        "\n--- TCK expected-error mismatch #{}: {} ---",
        idx + 1,
        sc.name
    );
    if let Some(query) = &sc.query {
        eprintln!("query:\n{query}");
    }
    eprintln!("actual succeeded: {actual:?}");
}

fn debug_error(sc: &Scenario, kind: &str, message: &str) {
    let Ok(raw_limit) = std::env::var("TCK_DEBUG_ERRORS") else {
        return;
    };
    let limit = raw_limit.parse::<usize>().unwrap_or(10);
    let idx = DEBUG_MISMATCH_COUNT.fetch_add(1, Ordering::Relaxed);
    if idx >= limit {
        return;
    }
    eprintln!("\n--- TCK {kind} #{}: {} ---", idx + 1, sc.name);
    if let Some(query) = &sc.query {
        eprintln!("query:\n{query}");
    }
    eprintln!("error: {message}");
}

fn debug_uncompared(sc: &Scenario, reason: &str) {
    let Ok(raw_limit) = std::env::var("TCK_DEBUG_UNCOMPARED") else {
        return;
    };
    let limit = raw_limit.parse::<usize>().unwrap_or(10);
    let idx = DEBUG_MISMATCH_COUNT.fetch_add(1, Ordering::Relaxed);
    if idx >= limit {
        return;
    }
    eprintln!("\n--- TCK uncompared #{}: {} ---", idx + 1, sc.name);
    if let Some(query) = &sc.query {
        eprintln!("query:\n{query}");
    }
    eprintln!("reason: {reason}");
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

#[derive(Debug, Clone, Default, PartialEq)]
struct EffectSnapshot {
    nodes: BTreeSet<u64>,
    relationships: BTreeSet<u64>,
    labels: BTreeSet<String>,
    vertex_labels: BTreeMap<u64, BTreeSet<String>>,
    properties: BTreeMap<(char, u64, String), Value>,
}

impl EffectSnapshot {
    fn capture(graph: &Graph) -> Self {
        let mut snapshot = Self::default();

        for raw_id in 0..graph.num_vertices() as u64 {
            let vertex = VertexId(raw_id);
            let Some(label) = graph.vertex_label(vertex) else {
                continue;
            };
            snapshot.nodes.insert(raw_id);
            let labels = split_label_set(label).collect::<BTreeSet<_>>();
            snapshot.labels.extend(labels.iter().cloned());
            snapshot.vertex_labels.insert(raw_id, labels);
            for (key, value) in graph.get_vertex_properties(vertex) {
                snapshot.properties.insert(('v', raw_id, key), value);
            }
        }

        for edge in graph.edge_records() {
            snapshot.relationships.insert(edge.id.0);
            for (key, value) in edge.properties {
                snapshot.properties.insert(('e', edge.id.0, key), value);
            }
        }

        snapshot
    }

    fn diff(&self, after: &Self) -> BTreeMap<String, i64> {
        self.diff_with_delete_fallout(after, false)
    }

    fn diff_including_delete_fallout(&self, after: &Self) -> BTreeMap<String, i64> {
        self.diff_with_delete_fallout(after, true)
    }

    fn diff_with_delete_fallout(
        &self,
        after: &Self,
        include_delete_fallout: bool,
    ) -> BTreeMap<String, i64> {
        let mut out = BTreeMap::new();
        insert_effect(
            &mut out,
            "+nodes",
            after.nodes.difference(&self.nodes).count() as i64,
        );
        insert_effect(
            &mut out,
            "-nodes",
            self.nodes.difference(&after.nodes).count() as i64,
        );
        insert_effect(
            &mut out,
            "+relationships",
            after.relationships.difference(&self.relationships).count() as i64,
        );
        insert_effect(
            &mut out,
            "-relationships",
            self.relationships.difference(&after.relationships).count() as i64,
        );
        insert_effect(&mut out, "+labels", self.labels_added(after));
        let labels_removed = if include_delete_fallout {
            self.labels.difference(&after.labels).count() as i64
        } else {
            self.labels_removed(after)
        };
        insert_effect(&mut out, "-labels", labels_removed);

        let mut properties_added = 0;
        let mut properties_removed = 0;
        for (key, after_value) in &after.properties {
            match self.properties.get(key) {
                None => properties_added += 1,
                Some(before_value) if before_value != after_value => {
                    properties_added += 1;
                    properties_removed += 1;
                }
                Some(_) => {}
            }
        }
        for key in self.properties.keys() {
            if !after.properties.contains_key(key)
                && (include_delete_fallout || after.entity_is_live(key.0, key.1))
            {
                properties_removed += 1;
            }
        }
        insert_effect(&mut out, "+properties", properties_added);
        insert_effect(&mut out, "-properties", properties_removed);

        out
    }

    fn labels_added(&self, after: &Self) -> i64 {
        after.labels.difference(&self.labels).count() as i64
    }

    fn labels_removed(&self, after: &Self) -> i64 {
        self.nodes
            .intersection(&after.nodes)
            .flat_map(|id| {
                let before = self.vertex_labels.get(id).into_iter().flatten();
                let after = after.vertex_labels.get(id).cloned().unwrap_or_default();
                before
                    .filter(move |label| !after.contains(*label))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect::<BTreeSet<_>>()
            .len() as i64
    }

    fn entity_is_live(&self, kind: char, id: u64) -> bool {
        match kind {
            'v' => self.nodes.contains(&id),
            'e' => self.relationships.contains(&id),
            _ => false,
        }
    }
}

fn split_label_set(label: &str) -> impl Iterator<Item = String> + '_ {
    label
        .split(':')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
}

fn insert_effect(out: &mut BTreeMap<String, i64>, key: &str, value: i64) {
    if value != 0 {
        out.insert(key.to_string(), value);
    }
}

fn compare_side_effects(
    sc: &Scenario,
    before: &EffectSnapshot,
    after: &EffectSnapshot,
) -> Option<bool> {
    let expected = sc.expected_side_effects.as_ref()?;
    let expected = expected
        .iter()
        .filter(|(_, value)| **value != 0)
        .map(|(key, value)| (key.clone(), *value))
        .collect::<BTreeMap<_, _>>();
    let actual = before.diff(after);
    let alternate = if expected == actual {
        None
    } else {
        Some(before.diff_including_delete_fallout(after))
    };
    let ok = expected == actual || alternate.as_ref().is_some_and(|actual| expected == *actual);
    if !ok {
        debug_side_effect_mismatch(sc, &expected, &actual);
    }
    Some(ok)
}

fn debug_side_effect_mismatch(
    sc: &Scenario,
    expected: &BTreeMap<String, i64>,
    actual: &BTreeMap<String, i64>,
) {
    let Ok(raw_limit) = std::env::var("TCK_DEBUG_MISMATCHES") else {
        return;
    };
    let limit = raw_limit.parse::<usize>().unwrap_or(10);
    let idx = DEBUG_MISMATCH_COUNT.fetch_add(1, Ordering::Relaxed);
    if idx >= limit {
        return;
    }
    eprintln!(
        "\n--- TCK side-effect mismatch #{}: {} ---",
        idx + 1,
        sc.name
    );
    if let Some(query) = &sc.query {
        eprintln!("query:\n{query}");
    }
    eprintln!("expected side effects: {expected:?}");
    eprintln!("actual side effects:   {actual:?}");
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
            unordered_list_elements,
        } => {
            if !columns.is_empty() && columns != &actual.columns {
                return ScenarioOutcome::ResultMismatch;
            }

            let rows_match = if *ordered {
                rows.len() == actual.rows.len()
                    && rows
                        .iter()
                        .zip(&actual.rows)
                        .all(|(expected_row, actual_row)| {
                            row_matches(expected_row, actual_row, graph, *unordered_list_elements)
                        })
            } else {
                unordered_rows_match(rows, &actual.rows, graph, *unordered_list_elements)
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
        Value::String(s) => quote_cypher_string(s),
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

fn quote_cypher_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push('\n'),
            '\r' => out.push('\r'),
            '\t' => out.push('\t'),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn row_matches(
    expected: &[String],
    actual: &[Value],
    graph: &Graph,
    unordered_list_elements: bool,
) -> bool {
    expected.len() == actual.len()
        && expected
            .iter()
            .zip(actual)
            .all(|(expected_cell, actual_value)| {
                cell_matches_with_options(
                    expected_cell,
                    actual_value,
                    graph,
                    unordered_list_elements,
                )
            })
}

fn unordered_rows_match(
    expected_rows: &[Vec<String>],
    actual_rows: &[Vec<Value>],
    graph: &Graph,
    unordered_list_elements: bool,
) -> bool {
    if expected_rows.len() != actual_rows.len() {
        return false;
    }

    let mut candidates: Vec<(usize, Vec<usize>)> = expected_rows
        .iter()
        .enumerate()
        .map(|(expected_idx, expected)| {
            let matches = actual_rows
                .iter()
                .enumerate()
                .filter_map(|(actual_idx, actual)| {
                    row_matches(expected, actual, graph, unordered_list_elements)
                        .then_some(actual_idx)
                })
                .collect::<Vec<_>>();
            (expected_idx, matches)
        })
        .collect();

    if candidates.iter().any(|(_, matches)| matches.is_empty()) {
        if std::env::var("TCK_DEBUG_MATCH_CANDIDATES").ok().as_deref() == Some("1") {
            for (expected_idx, matches) in &candidates {
                if matches.is_empty() {
                    eprintln!(
                        "no candidate for expected row #{expected_idx}: {:?}",
                        expected_rows[*expected_idx]
                    );
                }
            }
        }
        return false;
    }

    candidates.sort_by_key(|(_, matches)| matches.len());

    fn assign_rows(
        position: usize,
        candidates: &[(usize, Vec<usize>)],
        used_actual: &mut [bool],
    ) -> bool {
        if position == candidates.len() {
            return true;
        }

        for &actual_idx in &candidates[position].1 {
            if used_actual[actual_idx] {
                continue;
            }
            used_actual[actual_idx] = true;
            if assign_rows(position + 1, candidates, used_actual) {
                return true;
            }
            used_actual[actual_idx] = false;
        }

        false
    }

    let mut used_actual = vec![false; actual_rows.len()];
    assign_rows(0, &candidates, &mut used_actual)
}

fn cell_matches(expected: &str, actual: &Value, graph: &Graph) -> bool {
    cell_matches_with_options(expected, actual, graph, false)
}

fn cell_matches_with_options(
    expected: &str,
    actual: &Value,
    graph: &Graph,
    unordered_list_elements: bool,
) -> bool {
    let expected = expected.trim();
    if numeric_cell_matches(expected, actual) {
        return true;
    }
    if is_map_literal_cell(expected) {
        return matches_map_literal(expected, actual, graph);
    }
    if is_node_literal_cell(expected) {
        return matches_node_literal(expected, actual, graph);
    }
    if is_path_list_literal_cell(expected) {
        return matches_path_list_literal(expected, actual, graph);
    }
    if is_path_literal_cell(expected) {
        return matches_path_literal(expected, actual, graph);
    }
    if is_list_literal_cell(expected) {
        return matches_list_literal_with_options(expected, actual, graph, unordered_list_elements);
    }
    if is_node_list_literal_cell(expected) {
        return matches_node_list_literal(expected, actual, graph);
    }
    if is_relationship_list_literal_cell(expected) {
        return matches_relationship_list_literal(expected, actual, graph);
    }
    if is_relationship_literal_cell(expected) {
        return matches_relationship_literal(expected, actual, graph);
    }
    canonical_cell(expected) == canonical_cell(&format_value(actual))
}

fn numeric_cell_matches(expected: &str, actual: &Value) -> bool {
    if let Ok(expected_int) = expected.parse::<i64>() {
        return matches!(actual, Value::Int64(actual_int) if *actual_int == expected_int);
    }
    if let Ok(expected_float) = expected.parse::<f64>() {
        return match actual {
            Value::Float64(actual_float) => float_cells_equal(expected_float, *actual_float),
            Value::Int64(actual_int) => float_cells_equal(expected_float, *actual_int as f64),
            _ => false,
        };
    }
    false
}

fn float_cells_equal(left: f64, right: f64) -> bool {
    if left == right {
        return true;
    }
    let scale = left.abs().max(right.abs()).max(1.0);
    (left - right).abs() <= f64::EPSILON * scale
}

fn is_map_literal_cell(cell: &str) -> bool {
    let cell = cell.trim();
    cell.starts_with('{') && cell.ends_with('}')
}

fn matches_map_literal(expected: &str, actual: &Value, graph: &Graph) -> bool {
    let Value::Map(actual) = actual else {
        return false;
    };
    let inner = expected
        .trim()
        .strip_prefix('{')
        .and_then(|cell| cell.strip_suffix('}'))
        .unwrap_or(expected.trim());
    if inner.trim().is_empty() {
        return actual.is_empty();
    }
    let expected_entries = split_top_level(inner, ',');
    expected_entries.len() == actual.len()
        && expected_entries.iter().all(|entry| {
            let Some(colon) = find_top_level(entry, ':') else {
                return false;
            };
            let key = parse_map_key(entry[..colon].trim());
            let value = entry[colon + 1..].trim();
            actual
                .iter()
                .find(|(actual_key, _)| actual_key == &key)
                .is_some_and(|(_, actual_value)| cell_matches(value, actual_value, graph))
        })
}

fn is_node_literal_cell(cell: &str) -> bool {
    let cell = cell.trim();
    cell.starts_with('(') && cell.ends_with(')') && !cell.contains("-[") && !cell.contains("]-")
}

fn is_relationship_literal_cell(cell: &str) -> bool {
    let cell = cell.trim();
    if cell.starts_with("[<") {
        return false;
    }
    if cell.starts_with("[(") {
        return false;
    }
    cell.starts_with("[:")
        || (cell.starts_with('[')
            && cell.ends_with(']')
            && !cell.starts_with("[[")
            && !cell.starts_with("['")
            && !cell.starts_with("[\"")
            && cell.contains(':')
            && !cell.contains(','))
}

fn is_relationship_list_literal_cell(cell: &str) -> bool {
    let cell = cell.trim();
    cell.starts_with("[[") && cell.ends_with("]]") && cell.contains("[:")
}

fn is_node_list_literal_cell(cell: &str) -> bool {
    let cell = cell.trim();
    cell.starts_with("[(") && cell.ends_with(")]") && cell.contains("(:")
}

fn is_list_literal_cell(cell: &str) -> bool {
    let cell = cell.trim();
    cell.starts_with('[') && cell.ends_with(']') && !is_relationship_literal_cell(cell)
}

fn is_path_literal_cell(cell: &str) -> bool {
    let cell = cell.trim();
    cell.starts_with('<') && cell.ends_with('>')
}

fn is_path_list_literal_cell(cell: &str) -> bool {
    let cell = cell.trim();
    cell.starts_with("[<") && cell.ends_with(">]")
}

fn matches_node_literal(expected: &str, actual: &Value, graph: &Graph) -> bool {
    let Some(vertex) = actual_vertex_id(actual) else {
        return false;
    };
    let Some(actual_label) = graph.vertex_label(vertex) else {
        return false;
    };
    let actual_labels: Vec<&str> = actual_label
        .split(':')
        .filter(|part| !part.is_empty())
        .collect();
    let inner = expected
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    let (labels, props) = parse_graph_literal_parts(inner);
    if !labels.is_empty()
        && !labels
            .iter()
            .all(|label| actual_labels.iter().any(|actual| actual == label))
    {
        return false;
    }
    props_match(&props, &graph.get_vertex_properties(vertex))
}

fn actual_vertex_id(actual: &Value) -> Option<VertexId> {
    match actual {
        Value::Int64(id) => Some(VertexId(*id as u64)),
        Value::Map(entries) => entries
            .iter()
            .find(|(key, _)| key == "__vertex_id")
            .and_then(|(_, value)| value.as_i64())
            .map(|id| VertexId(id as u64)),
        _ => None,
    }
}

fn matches_relationship_literal(expected: &str, actual: &Value, graph: &Graph) -> bool {
    let Some(edge) = actual_edge_id(actual) else {
        return false;
    };
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

fn actual_edge_id(actual: &Value) -> Option<EdgeId> {
    match actual {
        Value::Int64(id) => Some(EdgeId(*id as u64)),
        Value::Map(entries) => entries
            .iter()
            .find(|(key, _)| key == "__edge_id")
            .and_then(|(_, value)| value.as_i64())
            .map(|id| EdgeId(id as u64)),
        _ => None,
    }
}

fn matches_relationship_list_literal(expected: &str, actual: &Value, graph: &Graph) -> bool {
    let Value::List(items) = actual else {
        return false;
    };
    let inner = expected
        .trim()
        .strip_prefix('[')
        .and_then(|cell| cell.strip_suffix(']'))
        .unwrap_or(expected.trim());
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

fn matches_list_literal_with_options(
    expected: &str,
    actual: &Value,
    graph: &Graph,
    unordered_list_elements: bool,
) -> bool {
    let Value::List(items) = actual else {
        return false;
    };
    let inner = expected
        .trim()
        .strip_prefix('[')
        .and_then(|cell| cell.strip_suffix(']'))
        .unwrap_or(expected.trim());
    if inner.trim().is_empty() {
        return items.is_empty();
    }
    let expected_items = split_top_level(inner, ',');
    if expected_items.len() != items.len() {
        return false;
    }
    if unordered_list_elements {
        return unordered_list_items_match(&expected_items, items, graph);
    }
    expected_items
        .iter()
        .zip(items)
        .all(|(expected_item, actual_item)| {
            cell_matches_with_options(expected_item, actual_item, graph, unordered_list_elements)
        })
}

fn unordered_list_items_match(
    expected_items: &[&str],
    actual_items: &[Value],
    graph: &Graph,
) -> bool {
    let mut used = vec![false; actual_items.len()];
    'expected: for expected_item in expected_items {
        for (idx, actual_item) in actual_items.iter().enumerate() {
            if used[idx] {
                continue;
            }
            if cell_matches_with_options(expected_item, actual_item, graph, true) {
                used[idx] = true;
                continue 'expected;
            }
        }
        return false;
    }
    true
}

fn matches_node_list_literal(expected: &str, actual: &Value, graph: &Graph) -> bool {
    let Value::List(items) = actual else {
        return false;
    };
    let inner = expected
        .trim()
        .strip_prefix('[')
        .and_then(|cell| cell.strip_suffix(']'))
        .unwrap_or(expected.trim());
    if inner.trim().is_empty() {
        return items.is_empty();
    }
    let expected_items = split_top_level(inner, ',');
    expected_items.len() == items.len()
        && expected_items
            .iter()
            .zip(items)
            .all(|(expected_item, actual_item)| {
                matches_node_literal(expected_item, actual_item, graph)
            })
}

fn matches_path_list_literal(expected: &str, actual: &Value, graph: &Graph) -> bool {
    let Value::List(items) = actual else {
        return false;
    };
    let inner = expected
        .trim()
        .strip_prefix('[')
        .and_then(|cell| cell.strip_suffix(']'))
        .unwrap_or(expected.trim());
    if inner.trim().is_empty() {
        return items.is_empty();
    }
    let expected_items = split_top_level(inner, ',');
    expected_items.len() == items.len()
        && expected_items
            .iter()
            .zip(items)
            .all(|(expected_item, actual_item)| {
                matches_path_literal(expected_item, actual_item, graph)
            })
}

fn matches_path_literal(expected: &str, actual: &Value, graph: &Graph) -> bool {
    let Some((actual_nodes, actual_edges)) = path_components(actual) else {
        return false;
    };
    let Some((expected_nodes, expected_edges)) = parse_path_literal(expected) else {
        return false;
    };
    expected_nodes.len() == actual_nodes.len()
        && expected_edges.len() == actual_edges.len()
        && expected_nodes
            .iter()
            .zip(actual_nodes)
            .all(|(expected_node, actual_node)| {
                matches_node_literal(expected_node, actual_node, graph)
            })
        && expected_edges
            .iter()
            .zip(actual_edges)
            .all(|(expected_edge, actual_edge)| {
                matches_relationship_literal(expected_edge, actual_edge, graph)
            })
}

fn path_components(value: &Value) -> Option<(&[Value], &[Value])> {
    let Value::Map(entries) = value else {
        return None;
    };
    let nodes = entries.iter().find_map(|(key, value)| {
        if key == "__path_nodes" {
            if let Value::List(values) = value {
                Some(values.as_slice())
            } else {
                None
            }
        } else {
            None
        }
    })?;
    let edges = entries.iter().find_map(|(key, value)| {
        if key == "__path_edges" {
            if let Value::List(values) = value {
                Some(values.as_slice())
            } else {
                None
            }
        } else {
            None
        }
    })?;
    Some((nodes, edges))
}

fn parse_path_literal(expected: &str) -> Option<(Vec<String>, Vec<String>)> {
    let inner = expected.trim().strip_prefix('<')?.strip_suffix('>')?.trim();
    if inner.is_empty() {
        return Some((Vec::new(), Vec::new()));
    }

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let chars: Vec<char> = inner.chars().collect();
    let mut idx = 0usize;
    while idx < chars.len() {
        match chars[idx] {
            '(' => {
                let end = find_balanced_char(&chars, idx, '(', ')')?;
                nodes.push(chars[idx..=end].iter().collect());
                idx = end + 1;
            }
            '[' => {
                let end = find_balanced_char(&chars, idx, '[', ']')?;
                edges.push(chars[idx..=end].iter().collect());
                idx = end + 1;
            }
            _ => idx += 1,
        }
    }
    Some((nodes, edges))
}

fn find_balanced_char(chars: &[char], start: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut prev_escape = false;
    for idx in start..chars.len() {
        let ch = chars[idx];
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
            c if c == open => depth += 1,
            c if c == close => {
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

fn category_of(root: &Path, path: &Path) -> String {
    // e.g. .../features/clauses/match/Match1.feature -> "clauses/match"
    let rel = path.strip_prefix(root).unwrap_or(path);
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
    let include_upstream_skipped_tags = env_flag("TCK_INCLUDE_UPSTREAM_SKIPPED");
    let features = discover_feature_files(&root, filter.as_deref());

    let mut per_category: BTreeMap<String, Stats> = BTreeMap::new();
    let mut totals = Stats::default();

    for path in &features {
        let Ok(contents) = fs::read_to_string(path) else {
            continue;
        };
        let scenarios = parse_feature_with_options(
            &contents,
            ParseFeatureOptions {
                include_upstream_skipped_tags,
            },
        );
        let cat = category_of(&root, path);
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
                ScenarioOutcome::ExpectedErrorOk => {
                    entry.result_ok += 1;
                    entry.expected_error_ok += 1;
                    totals.result_ok += 1;
                    totals.expected_error_ok += 1;
                }
            }
        }
    }

    let considered = totals.total - totals.skipped;
    print_report(
        &per_category,
        &totals,
        considered,
        &root,
        filter.as_deref(),
        include_upstream_skipped_tags,
    );
    assert_expected_count("TCK_EXPECT_TOTAL", totals.total);
    assert_expected_count("TCK_EXPECT_CONSIDERED", considered);
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn assert_expected_count(env_name: &str, actual: usize) {
    let Ok(raw) = std::env::var(env_name) else {
        return;
    };
    let expected = raw
        .parse::<usize>()
        .unwrap_or_else(|err| panic!("{env_name} must be an integer, got {raw:?}: {err}"));
    assert_eq!(
        actual, expected,
        "{env_name} expected {expected} but TCK runner measured {actual}"
    );
}

fn print_report(
    per_category: &BTreeMap<String, Stats>,
    totals: &Stats,
    considered: usize,
    root: &Path,
    category_filter: Option<&str>,
    include_upstream_skipped_tags: bool,
) {
    eprintln!();
    eprintln!("=== openCypher TCK report — nexus-cypher ===");
    eprintln!("feature root: {}", root.display());
    if let Some(filter) = category_filter {
        eprintln!("category filter: {filter}");
    }
    if include_upstream_skipped_tags {
        eprintln!("mode: experimental, upstream @skip/@ignore tags included");
    }
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
    }
    print_json_report(
        per_category,
        totals,
        considered,
        root,
        category_filter,
        include_upstream_skipped_tags,
    );
    eprintln!();
}

#[derive(Debug, Serialize)]
struct TckJsonReport<'a> {
    corpus: TckCorpusReport,
    include_upstream_skipped_tags: bool,
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
struct TckCorpusReport {
    scope: String,
    feature_root: String,
    category_filter: Option<String>,
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

fn print_json_report(
    per_category: &BTreeMap<String, Stats>,
    totals: &Stats,
    considered: usize,
    root: &Path,
    category_filter: Option<&str>,
    include_upstream_skipped_tags: bool,
) {
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
        corpus: TckCorpusReport {
            scope: tck_scope_name(root, include_upstream_skipped_tags),
            feature_root: root.display().to_string(),
            category_filter: category_filter.map(str::to_string),
        },
        include_upstream_skipped_tags,
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

fn tck_scope_name(root: &Path, include_upstream_skipped_tags: bool) -> String {
    if let Ok(scope) = std::env::var("TCK_SCOPE") {
        return scope;
    }
    if std::env::var("TCK_FEATURE_ROOT").is_ok() {
        return "custom".to_string();
    }
    if include_upstream_skipped_tags {
        "checked-in-falkor-skipped-inclusive".to_string()
    } else if root == default_tck_root().as_path() {
        "checked-in-falkor".to_string()
    } else {
        "custom".to_string()
    }
}

#[test]
fn unordered_rows_match_handles_subset_label_literals() {
    let mut graph = Graph::new(8, 8);
    let abc = graph.add_vertex("A:B:C");
    let ab = graph.add_vertex("A:B");
    graph.build();

    let expected = vec![vec!["(:A:B)".to_string()], vec!["(:A:B:C)".to_string()]];
    let actual = vec![
        vec![Value::Int64(abc.0 as i64)],
        vec![Value::Int64(ab.0 as i64)],
    ];

    assert!(unordered_rows_match(&expected, &actual, &graph, false));
}

#[test]
fn cell_matches_path_and_path_list_literals() {
    let mut graph = Graph::new(8, 8);
    let a = graph.add_vertex("A");
    let b = graph.add_vertex("B");
    let edge = graph.add_edge(a, b, "R");
    graph.build();

    let path = Value::Map(vec![
        (
            "__path_nodes".into(),
            Value::List(vec![Value::Int64(a.0 as i64), Value::Int64(b.0 as i64)]),
        ),
        (
            "__path_edges".into(),
            Value::List(vec![Value::Int64(edge.0 as i64)]),
        ),
    ]);

    assert!(cell_matches("<(:A)-[:R]->(:B)>", &path, &graph));
    assert!(cell_matches(
        "[<(:A)-[:R]->(:B)>]",
        &Value::List(vec![path]),
        &graph
    ));
}

#[test]
fn cell_matches_node_and_relationship_list_literals() {
    let mut graph = Graph::new(8, 8);
    graph.register_vertex_property("name", PropertyType::String, false, false);
    graph.register_edge_property("name", PropertyType::String, false, false);
    let a = graph.add_vertex("A");
    graph.set_vertex_property(a, "name", Value::String("a".into()));
    let b = graph.add_vertex("B");
    graph.set_vertex_property(b, "name", Value::String("b".into()));
    let edge = graph.add_edge(a, b, "RA");
    graph.set_edge_property(edge, "name", Value::String("a".into()));
    graph.build();

    assert!(cell_matches(
        "[(:A {name: 'a'}), (:B {name: 'b'})]",
        &Value::List(vec![Value::Int64(a.0 as i64), Value::Int64(b.0 as i64)]),
        &graph
    ));
    assert!(cell_matches(
        "[[:RA {name: 'a'}]]",
        &Value::List(vec![Value::Map(vec![(
            "__edge_id".into(),
            Value::Int64(edge.0 as i64)
        )])]),
        &graph
    ));
}

#[test]
fn parse_feature_marks_skip_and_ignore_tags_as_skipped() {
    let scenarios = parse_feature(
        r#"
Feature: Skip tags

  @skip
  Scenario: skipped
    Given an empty graph
    When executing query:
      """
      RETURN 1
      """
    Then the result should be, in any order:
      | x |
      | 1 |

  @skipGrammarCheck @ignore
  Scenario: also skipped
    Given an empty graph
    When executing query:
      """
      RETURN 2
      """
    Then the result should be, in any order:
      | x |
      | 2 |

  Scenario: not skipped
    Given an empty graph
    When executing query:
      """
      RETURN 3 AS x
      """
    Then the result should be, in any order:
      | x |
      | 3 |
"#,
    );

    assert_eq!(scenarios.len(), 3);
    assert_eq!(scenarios[0].skipped_reason, Some("scenario tagged skip"));
    assert_eq!(scenarios[1].skipped_reason, Some("scenario tagged skip"));
    assert_eq!(scenarios[2].skipped_reason, None);
}
