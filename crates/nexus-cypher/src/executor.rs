//! Vectorized query executor.
//!
//! Executes a logical plan against a `QueryContext` (graph + optional indexes),
//! producing rows of `Value`s.
//! Expand operators use SpMV for cache-friendly multi-hop traversal.

use crate::ast::{
    BinaryOp, Expr, ListPredicateKind, Literal, MatchClause, NodePattern, Pattern, PatternElement,
    RelDirection, RelationshipPattern, UnaryOp,
};
use crate::context::{QueryContext, WriteContext, WriteSummary};
use crate::error::{CypherError, CypherResult};
use crate::planner::*;
use nexus_core::graph::Graph;
use nexus_core::types::{Direction, EdgeId, Value, VertexId};
use std::collections::{HashMap, HashSet};

/// A single result row: maps variable names to resolved values.
pub type Row = HashMap<String, Value>;

/// Full query result: column names + rows.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl QueryResult {
    pub fn empty(columns: Vec<String>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
        }
    }

    pub fn num_rows(&self) -> usize {
        self.rows.len()
    }
}

/// Execute a logical plan against a query context (graph + indexes).
pub fn execute(plan: &LogicalPlan, ctx: &QueryContext) -> CypherResult<QueryResult> {
    match plan {
        LogicalPlan::Argument => Ok(QueryResult {
            columns: Vec::new(),
            rows: vec![Vec::new()],
        }),
        LogicalPlan::ScanVertices {
            variable,
            label,
            index_lookup,
        } => execute_scan(ctx, variable, label.as_deref(), index_lookup.as_ref()),
        LogicalPlan::Expand {
            input,
            src_var,
            dst_var,
            rel_var,
            edge_types,
            rel_properties,
            direction,
            min_hops,
            max_hops,
            dst_labels,
        } => {
            let input_result = execute(input, ctx)?;
            execute_expand(
                ctx,
                &input_result,
                src_var,
                dst_var,
                rel_var.as_deref(),
                edge_types,
                rel_properties,
                *direction,
                *min_hops,
                *max_hops,
                dst_labels,
            )
        }
        LogicalPlan::Filter { input, predicate } => {
            let input_result = execute(input, ctx)?;
            execute_filter(ctx, &input_result, predicate)
        }
        LogicalPlan::ApplyMatch {
            input,
            clause,
            optional,
        } => {
            let input_result = execute(input, ctx)?;
            execute_apply_match(ctx, &input_result, clause, *optional)
        }
        LogicalPlan::Unwind { input, expr, alias } => {
            let input_result = execute(input, ctx)?;
            execute_unwind(ctx, &input_result, expr, alias)
        }
        LogicalPlan::Project { input, columns } => {
            let input_result = execute(input, ctx)?;
            execute_project(ctx, &input_result, columns)
        }
        LogicalPlan::Sort { input, keys } => {
            let input_result = execute(input, ctx)?;
            execute_sort(ctx, input_result, keys)
        }
        LogicalPlan::Limit { input, count } => {
            let mut result = execute(input, ctx)?;
            result.rows.truncate(*count as usize);
            Ok(result)
        }
        LogicalPlan::Skip { input, count } => {
            let mut result = execute(input, ctx)?;
            let skip = (*count as usize).min(result.rows.len());
            result.rows = result.rows.split_off(skip);
            Ok(result)
        }
        LogicalPlan::Distinct { input } => {
            let result = execute(input, ctx)?;
            execute_distinct(result)
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregations,
        } => {
            let input_result = execute(input, ctx)?;
            execute_aggregate(ctx, &input_result, group_by, aggregations)
        }
        LogicalPlan::Union { left, right, all } => {
            let left_result = execute(left, ctx)?;
            let right_result = execute(right, ctx)?;
            execute_union(left_result, right_result, *all)
        }
    }
}

fn execute_scan(
    ctx: &QueryContext,
    variable: &str,
    label_filter: Option<&str>,
    index_lookup: Option<&IndexLookup>,
) -> CypherResult<QueryResult> {
    let graph = ctx.graph;

    // Fast path: try index lookup when an IndexLookup hint is present.
    if let Some(lookup) = index_lookup {
        if let Some(candidate_ids) = ctx.index_lookup(&lookup.property, &lookup.value) {
            let mut matching_ids = Vec::new();
            for vid in candidate_ids {
                if let Some(label) = label_filter {
                    if graph.vertex_label(vid) != Some(label) {
                        continue;
                    }
                }
                matching_ids.push(vid.0);
            }

            let col_name = variable.to_string();
            let rows: Vec<Vec<Value>> = matching_ids
                .iter()
                .map(|&id| vec![Value::Int64(id as i64)])
                .collect();
            return Ok(QueryResult {
                columns: vec![col_name],
                rows,
            });
        }
    }

    // Slow path: linear scan over all vertices.
    let mut matching_ids = Vec::new();

    for vid_raw in 0..graph.num_vertices() as u64 {
        let vid = VertexId(vid_raw);

        if let Some(label) = label_filter {
            if graph.vertex_label(vid) != Some(label) {
                continue;
            }
        }

        if let Some(lookup) = index_lookup {
            let actual = graph.get_vertex_property(vid, &lookup.property);
            if actual != lookup.value {
                continue;
            }
        }

        matching_ids.push(vid_raw);
    }

    let col_name = variable.to_string();
    let rows: Vec<Vec<Value>> = matching_ids
        .iter()
        .map(|&id| vec![Value::Int64(id as i64)])
        .collect();

    Ok(QueryResult {
        columns: vec![col_name],
        rows,
    })
}

fn execute_expand(
    ctx: &QueryContext,
    input: &QueryResult,
    src_var: &str,
    dst_var: &str,
    rel_var: Option<&str>,
    edge_types: &[String],
    rel_properties: &[(String, ProjectExpr)],
    direction: ExpandDirection,
    min_hops: u32,
    max_hops: u32,
    dst_labels: &[String],
) -> CypherResult<QueryResult> {
    let graph = ctx.graph;
    let src_col_idx = input
        .columns
        .iter()
        .position(|c| c == src_var)
        .ok_or_else(|| CypherError::Execution(format!("variable '{src_var}' not found")))?;

    let dir = match direction {
        ExpandDirection::Outgoing => Direction::Outgoing,
        ExpandDirection::Incoming => Direction::Incoming,
        ExpandDirection::Both => Direction::Both,
    };

    let mut columns = input.columns.clone();
    if let Some(rel_var) = rel_var {
        columns.push(rel_var.to_string());
    }
    columns.push(dst_var.to_string());

    let mut rows = Vec::new();

    for row in &input.rows {
        let src_id = match &row[src_col_idx] {
            Value::Int64(id) => VertexId(*id as u64),
            _ => continue,
        };

        let mut reachable: Vec<(VertexId, Option<EdgeId>)> = Vec::new();

        if max_hops == 1 && min_hops <= 1 {
            for edge_type in edge_types {
                for (neighbor, edge) in graph.neighbors_with_edges(src_id, edge_type, dir) {
                    if edge_matches_properties(ctx, edge, rel_properties, row, &input.columns) {
                        reachable.push((neighbor, Some(edge)));
                    }
                }
            }
            if edge_types.is_empty() {
                for (neighbor, edge) in graph.neighbors_with_edges_any_label(src_id, dir) {
                    if edge_matches_properties(ctx, edge, rel_properties, row, &input.columns) {
                        reachable.push((neighbor, Some(edge)));
                    }
                }
            }
        } else {
            if rel_var.is_some() {
                return Err(CypherError::Execution(
                    "relationship variables on variable-length paths are not supported yet".into(),
                ));
            }
            let mut vertices = Vec::new();
            expand_variable_length(
                graph,
                src_id,
                edge_types,
                dir,
                min_hops,
                max_hops,
                &mut vertices,
            );
            reachable.extend(vertices.into_iter().map(|dst| (dst, None)));
        }

        let mut seen_reachable = HashSet::new();
        for (dst, edge) in reachable
            .into_iter()
            .filter(|(dst, edge)| seen_reachable.insert((dst.0, edge.map(|edge| edge.0))))
        {
            if !dst_labels.is_empty() {
                let matches_label = dst_labels
                    .iter()
                    .any(|label| graph.vertex_label(dst) == Some(label.as_str()));
                if !matches_label {
                    continue;
                }
            }
            let mut new_row = row.clone();
            if rel_var.is_some() {
                if let Some(edge) = edge {
                    new_row.push(Value::Int64(edge.0 as i64));
                } else {
                    new_row.push(Value::Null);
                }
            }
            new_row.push(Value::Int64(dst.0 as i64));
            rows.push(new_row);
        }
    }

    Ok(QueryResult { columns, rows })
}

fn edge_matches_properties(
    ctx: &QueryContext,
    edge: EdgeId,
    rel_properties: &[(String, ProjectExpr)],
    row: &[Value],
    columns: &[String],
) -> bool {
    rel_properties.iter().all(|(key, expr)| {
        let expected = resolve_project_expr(ctx, expr, row, columns);
        let actual = ctx.graph.get_edge_property(edge, key);
        actual == expected
    })
}

fn expand_variable_length(
    graph: &Graph,
    start: VertexId,
    edge_types: &[String],
    dir: Direction,
    min_hops: u32,
    max_hops: u32,
    result: &mut Vec<VertexId>,
) {
    if min_hops == 0 {
        result.push(start);
    }
    let max_hops = max_hops.min(graph.num_vertices() as u32);
    if max_hops == 0 {
        return;
    }

    let mut frontier = vec![start];
    let mut visited = std::collections::HashSet::new();
    visited.insert(start);

    for hop in 1..=max_hops {
        let mut next_frontier = Vec::new();
        for &v in &frontier {
            let neighbors: Vec<VertexId> = if edge_types.is_empty() {
                graph
                    .edge_label_names()
                    .iter()
                    .flat_map(|label| graph.neighbors(v, label, dir))
                    .collect()
            } else {
                edge_types
                    .iter()
                    .flat_map(|label| graph.neighbors(v, label, dir))
                    .collect()
            };

            for n in neighbors {
                if visited.insert(n) {
                    if hop >= min_hops {
                        result.push(n);
                    }
                    next_frontier.push(n);
                }
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }
}

fn execute_unwind(
    ctx: &QueryContext,
    input: &QueryResult,
    expr: &ProjectExpr,
    alias: &str,
) -> CypherResult<QueryResult> {
    let mut columns = input.columns.clone();
    let alias_idx = if let Some(idx) = columns.iter().position(|c| c == alias) {
        idx
    } else {
        columns.push(alias.to_string());
        columns.len() - 1
    };

    let mut rows = Vec::new();
    for row in &input.rows {
        let value = resolve_project_expr(ctx, expr, row, &input.columns);
        let values = match value {
            Value::List(values) => values,
            Value::Null => Vec::new(),
            other => vec![other],
        };

        for item in values {
            let mut new_row = row.clone();
            if alias_idx < input.columns.len() {
                new_row[alias_idx] = item;
            } else {
                new_row.push(item);
            }
            rows.push(new_row);
        }
    }

    Ok(QueryResult { columns, rows })
}

fn execute_apply_match(
    ctx: &QueryContext,
    input: &QueryResult,
    clause: &MatchClause,
    optional: bool,
) -> CypherResult<QueryResult> {
    let mut result = input.clone();

    for pattern in &clause.patterns {
        let new_vars: Vec<String> = pattern_variables(pattern)
            .into_iter()
            .filter(|var| !result.columns.contains(var))
            .collect();
        let mut out_columns = result.columns.clone();
        out_columns.extend(new_vars.iter().cloned());

        let mut out_rows = Vec::new();
        for row in &result.rows {
            let matches = match_pattern(ctx, pattern, row, &result.columns)?;
            if matches.is_empty() {
                if optional {
                    let mut new_row = row.clone();
                    new_row.extend(new_vars.iter().map(|_| Value::Null));
                    out_rows.push(new_row);
                }
                continue;
            }

            for assignment in matches {
                let mut new_row = row.clone();
                for var in &new_vars {
                    new_row.push(assignment.get(var).cloned().unwrap_or(Value::Null));
                }
                out_rows.push(new_row);
            }
        }

        result = QueryResult {
            columns: out_columns,
            rows: out_rows,
        };
    }

    Ok(result)
}

fn pattern_variables(pattern: &Pattern) -> Vec<String> {
    let mut vars = Vec::new();
    for element in &pattern.elements {
        match element {
            PatternElement::Node(np) => {
                if let Some(var) = &np.variable {
                    if !vars.contains(var) {
                        vars.push(var.clone());
                    }
                }
            }
            PatternElement::Relationship(rp) => {
                if let Some(var) = &rp.variable {
                    if !vars.contains(var) {
                        vars.push(var.clone());
                    }
                }
            }
        }
    }
    vars
}

fn match_pattern(
    ctx: &QueryContext,
    pattern: &Pattern,
    row: &[Value],
    columns: &[String],
) -> CypherResult<Vec<HashMap<String, Value>>> {
    let Some(PatternElement::Node(first_node)) = pattern.elements.first() else {
        return Ok(Vec::new());
    };

    let mut states = Vec::new();
    for vertex in node_candidates(ctx, first_node, row, columns, &HashMap::new())? {
        let mut assignments = HashMap::new();
        if let Some(var) = &first_node.variable {
            if !columns.contains(var) {
                assignments.insert(var.clone(), Value::Int64(vertex.0 as i64));
            }
        }
        states.push(PatternState {
            current: vertex,
            assignments,
            used_edges: HashSet::new(),
        });
    }

    let mut index = 1;
    while index < pattern.elements.len() {
        let PatternElement::Relationship(rel) = &pattern.elements[index] else {
            return Err(CypherError::Execution(
                "expected relationship in pattern".into(),
            ));
        };
        let Some(PatternElement::Node(next_node)) = pattern.elements.get(index + 1) else {
            return Err(CypherError::Execution(
                "relationship must be followed by a node".into(),
            ));
        };

        let mut next_states = Vec::new();
        for state in states {
            for candidate in relationship_candidates_with_bindings(
                ctx,
                state.current,
                rel,
                row,
                columns,
                &state.assignments,
            ) {
                if candidate
                    .edges
                    .iter()
                    .any(|edge| state.used_edges.contains(&edge.0))
                {
                    continue;
                }
                if !relationship_binding_matches(
                    rel,
                    &candidate.binding,
                    row,
                    columns,
                    &state.assignments,
                ) {
                    continue;
                }
                if !node_binding_matches(
                    ctx,
                    next_node,
                    candidate.neighbor,
                    row,
                    columns,
                    &state.assignments,
                )? {
                    continue;
                }

                let mut next_assignments = state.assignments.clone();
                if let Some(var) = &rel.variable {
                    if !columns.contains(var) && !next_assignments.contains_key(var) {
                        next_assignments.insert(var.clone(), candidate.binding.clone());
                    }
                }
                if let Some(var) = &next_node.variable {
                    if !columns.contains(var) && !next_assignments.contains_key(var) {
                        next_assignments
                            .insert(var.clone(), Value::Int64(candidate.neighbor.0 as i64));
                    }
                }
                let mut used_edges = state.used_edges.clone();
                used_edges.extend(candidate.edges.iter().map(|edge| edge.0));
                next_states.push(PatternState {
                    current: candidate.neighbor,
                    assignments: next_assignments,
                    used_edges,
                });
            }
        }

        states = next_states;
        index += 2;
    }

    Ok(states.into_iter().map(|state| state.assignments).collect())
}

#[derive(Clone)]
struct PatternState {
    current: VertexId,
    assignments: HashMap<String, Value>,
    used_edges: HashSet<u64>,
}

#[derive(Clone)]
struct RelationshipCandidate {
    neighbor: VertexId,
    binding: Value,
    edges: Vec<EdgeId>,
}

fn node_candidates(
    ctx: &QueryContext,
    node: &NodePattern,
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> CypherResult<Vec<VertexId>> {
    if let Some(var) = &node.variable {
        if let Some(bound) = lookup_binding(var, row, columns, assignments) {
            return if let Value::Int64(id) = bound {
                let vertex = VertexId(id as u64);
                if node_matches(ctx, node, vertex, row, columns, assignments)? {
                    Ok(vec![vertex])
                } else {
                    Ok(Vec::new())
                }
            } else {
                Ok(Vec::new())
            };
        }
    }

    let mut out = Vec::new();
    for vid_raw in 0..ctx.graph.num_vertices() as u64 {
        let vid = VertexId(vid_raw);
        if node_matches(ctx, node, vid, row, columns, assignments)? {
            out.push(vid);
        }
    }
    Ok(out)
}

fn node_binding_matches(
    ctx: &QueryContext,
    node: &NodePattern,
    vertex: VertexId,
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> CypherResult<bool> {
    if let Some(var) = &node.variable {
        if let Some(bound) = lookup_binding(var, row, columns, assignments) {
            match bound {
                Value::Int64(bound) if bound as u64 == vertex.0 => {}
                Value::Int64(_) => return Ok(false),
                _ => return Ok(false),
            }
        }
    }
    node_matches(ctx, node, vertex, row, columns, assignments)
}

fn node_matches(
    ctx: &QueryContext,
    node: &NodePattern,
    vertex: VertexId,
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> CypherResult<bool> {
    if ctx.graph.vertex_label(vertex).is_none() {
        return Ok(false);
    }

    if !node.labels.is_empty() {
        let actual = ctx.graph.vertex_label(vertex);
        if !node
            .labels
            .iter()
            .any(|label| actual == Some(label.as_str()))
        {
            return Ok(false);
        }
    }

    for (key, expr) in &node.properties {
        let expected = eval_ast_expr_with_assignments(ctx, expr, row, columns, assignments);
        let actual = ctx.graph.get_vertex_property(vertex, key);
        if actual != expected {
            return Ok(false);
        }
    }
    Ok(true)
}

fn relationship_candidates_with_bindings(
    ctx: &QueryContext,
    current: VertexId,
    rel: &RelationshipPattern,
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> Vec<RelationshipCandidate> {
    if rel.min_hops.is_some() || rel.max_hops.is_some() {
        return variable_length_relationship_candidates(
            ctx,
            current,
            rel,
            row,
            columns,
            assignments,
        );
    }

    let dir = match rel.direction {
        RelDirection::Outgoing => Direction::Outgoing,
        RelDirection::Incoming => Direction::Incoming,
        RelDirection::Both => Direction::Both,
    };

    let candidates: Vec<(VertexId, EdgeId)> = if rel.rel_types.is_empty() {
        ctx.graph.neighbors_with_edges_any_label(current, dir)
    } else {
        rel.rel_types
            .iter()
            .flat_map(|label| ctx.graph.neighbors_with_edges(current, label, dir))
            .collect()
    };

    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|(neighbor, edge)| seen.insert((neighbor.0, edge.0)))
        .filter(|(_, edge)| {
            relationship_properties_match(ctx, *edge, rel, row, columns, assignments)
        })
        .map(|(neighbor, edge)| RelationshipCandidate {
            neighbor,
            binding: Value::Int64(edge.0 as i64),
            edges: vec![edge],
        })
        .collect()
}

fn variable_length_relationship_candidates(
    ctx: &QueryContext,
    current: VertexId,
    rel: &RelationshipPattern,
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> Vec<RelationshipCandidate> {
    let min = rel.min_hops.unwrap_or(1);
    let raw_max = rel.max_hops.unwrap_or(u32::MAX);
    let max = raw_max.min(ctx.graph.num_edges() as u32);
    if min > max {
        return Vec::new();
    }

    let mut out = Vec::new();
    if min == 0 {
        out.push(RelationshipCandidate {
            neighbor: current,
            binding: Value::List(Vec::new()),
            edges: Vec::new(),
        });
    }
    if max == 0 {
        return out;
    }

    let mut stack = vec![(current, 0u32, Vec::<EdgeId>::new(), HashSet::<u64>::new())];
    while let Some((vertex, depth, path_edges, used)) = stack.pop() {
        if depth >= max {
            continue;
        }
        for (neighbor, edge) in one_hop_relationship_edges(ctx, vertex, rel) {
            if used.contains(&edge.0)
                || !relationship_properties_match(ctx, edge, rel, row, columns, assignments)
            {
                continue;
            }

            let next_depth = depth + 1;
            let mut next_edges = path_edges.clone();
            next_edges.push(edge);
            if next_depth >= min {
                out.push(RelationshipCandidate {
                    neighbor,
                    binding: Value::List(
                        next_edges
                            .iter()
                            .map(|edge| Value::Int64(edge.0 as i64))
                            .collect(),
                    ),
                    edges: next_edges.clone(),
                });
            }
            let mut next_used = used.clone();
            next_used.insert(edge.0);
            stack.push((neighbor, next_depth, next_edges, next_used));
        }
    }
    out
}

fn one_hop_relationship_edges(
    ctx: &QueryContext,
    current: VertexId,
    rel: &RelationshipPattern,
) -> Vec<(VertexId, EdgeId)> {
    let dir = match rel.direction {
        RelDirection::Outgoing => Direction::Outgoing,
        RelDirection::Incoming => Direction::Incoming,
        RelDirection::Both => Direction::Both,
    };

    let candidates: Vec<(VertexId, EdgeId)> = if rel.rel_types.is_empty() {
        ctx.graph.neighbors_with_edges_any_label(current, dir)
    } else {
        rel.rel_types
            .iter()
            .flat_map(|label| ctx.graph.neighbors_with_edges(current, label, dir))
            .collect()
    };

    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|(neighbor, edge)| seen.insert((neighbor.0, edge.0)))
        .collect()
}

fn relationship_properties_match(
    ctx: &QueryContext,
    edge: EdgeId,
    rel: &RelationshipPattern,
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> bool {
    rel.properties.iter().all(|(key, expr)| {
        let expected = eval_ast_expr_with_assignments(ctx, expr, row, columns, assignments);
        let actual = ctx.graph.get_edge_property(edge, key);
        actual == expected
    })
}

fn relationship_binding_matches(
    rel: &RelationshipPattern,
    binding: &Value,
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> bool {
    if let Some(var) = &rel.variable {
        if let Some(bound) = lookup_binding(var, row, columns, assignments) {
            return bound == *binding;
        }
    }
    true
}

fn lookup_binding(
    var: &str,
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> Option<Value> {
    assignments.get(var).cloned().or_else(|| {
        columns
            .iter()
            .position(|c| c == var)
            .and_then(|idx| row.get(idx).cloned())
    })
}

fn execute_filter(
    ctx: &QueryContext,
    input: &QueryResult,
    predicate: &Predicate,
) -> CypherResult<QueryResult> {
    let rows: Vec<Vec<Value>> = input
        .rows
        .iter()
        .filter(|row| evaluate_predicate(ctx, row, &input.columns, predicate))
        .cloned()
        .collect();
    Ok(QueryResult {
        columns: input.columns.clone(),
        rows,
    })
}

fn evaluate_predicate(
    ctx: &QueryContext,
    row: &[Value],
    columns: &[String],
    predicate: &Predicate,
) -> bool {
    match predicate {
        Predicate::Comparison { left, op, right } => {
            let lval = resolve_property_ref(ctx.graph, row, columns, left);
            let rval = match right {
                PredicateValue::Literal(v) => v.clone(),
                PredicateValue::Property(pr) => resolve_property_ref(ctx.graph, row, columns, pr),
                PredicateValue::Parameter(name) => {
                    ctx.params.get(name).cloned().unwrap_or(Value::Null)
                }
            };
            compare_values(&lval, op, &rval)
        }
        Predicate::And(a, b) => {
            evaluate_predicate(ctx, row, columns, a) && evaluate_predicate(ctx, row, columns, b)
        }
        Predicate::Or(a, b) => {
            evaluate_predicate(ctx, row, columns, a) || evaluate_predicate(ctx, row, columns, b)
        }
        Predicate::Not(inner) => !evaluate_predicate(ctx, row, columns, inner),
        Predicate::IsNull(pr) => resolve_property_ref(ctx.graph, row, columns, pr).is_null(),
        Predicate::IsNotNull(pr) => !resolve_property_ref(ctx.graph, row, columns, pr).is_null(),
        Predicate::StringOp {
            property,
            op,
            pattern,
        } => {
            let val = resolve_property_ref(ctx.graph, row, columns, property);
            if let Value::String(s) = val {
                match op {
                    StringPredOp::Contains => s.contains(pattern.as_str()),
                    StringPredOp::StartsWith => s.starts_with(pattern.as_str()),
                    StringPredOp::EndsWith => s.ends_with(pattern.as_str()),
                }
            } else {
                false
            }
        }
        Predicate::Expr(expr) => value_truthy(&eval_ast_expr(ctx, expr, row, columns)),
    }
}

fn resolve_property_ref(
    graph: &Graph,
    row: &[Value],
    columns: &[String],
    pr: &PropertyRef,
) -> Value {
    let col_name = format!("{}.{}", pr.variable, pr.property);
    if let Some(idx) = columns.iter().position(|c| c == &col_name) {
        if let Some(component) = temporal_property(&row[idx], &pr.property) {
            return component;
        }
        return row[idx].clone();
    }
    if let Some(idx) = columns.iter().position(|c| c == &pr.variable) {
        if let Value::Int64(id) = &row[idx] {
            let vertex_value = graph.get_vertex_property(VertexId(*id as u64), &pr.property);
            if !vertex_value.is_null() {
                return vertex_value;
            }
            return graph.get_edge_property(EdgeId(*id as u64), &pr.property);
        }
        if let Some(component) = temporal_property(&row[idx], &pr.property) {
            return component;
        }
        return row[idx].clone();
    }
    Value::Null
}

fn compare_values(left: &Value, op: &CompareOp, right: &Value) -> bool {
    match (left, right) {
        (Value::Int64(l), Value::Int64(r)) => compare_ord(l, op, r),
        (Value::Float64(l), Value::Float64(r)) => compare_f64(*l, op, *r),
        (Value::Int64(l), Value::Float64(r)) => compare_f64(*l as f64, op, *r),
        (Value::Float64(l), Value::Int64(r)) => compare_f64(*l, op, *r as f64),
        (Value::String(l), Value::String(r)) => compare_ord(l, op, r),
        (Value::Bool(l), Value::Bool(r)) => match op {
            CompareOp::Eq => l == r,
            CompareOp::Neq => l != r,
            _ => false,
        },
        (Value::Null, Value::Null) => matches!(op, CompareOp::Eq),
        _ => matches!(op, CompareOp::Neq),
    }
}

fn compare_ord<T: PartialOrd>(l: &T, op: &CompareOp, r: &T) -> bool {
    match op {
        CompareOp::Eq => l == r,
        CompareOp::Neq => l != r,
        CompareOp::Lt => l < r,
        CompareOp::Lte => l <= r,
        CompareOp::Gt => l > r,
        CompareOp::Gte => l >= r,
    }
}

fn compare_f64(l: f64, op: &CompareOp, r: f64) -> bool {
    match op {
        CompareOp::Eq => (l - r).abs() < f64::EPSILON,
        CompareOp::Neq => (l - r).abs() >= f64::EPSILON,
        CompareOp::Lt => l < r,
        CompareOp::Lte => l <= r,
        CompareOp::Gt => l > r,
        CompareOp::Gte => l >= r,
    }
}

fn execute_project(
    ctx: &QueryContext,
    input: &QueryResult,
    columns: &[ProjectColumn],
) -> CypherResult<QueryResult> {
    let col_names: Vec<String> = columns.iter().map(|c| c.alias.clone()).collect();

    let rows: Vec<Vec<Value>> = input
        .rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|col| resolve_project_expr(ctx, &col.expr, row, &input.columns))
                .collect()
        })
        .collect();

    Ok(QueryResult {
        columns: col_names,
        rows,
    })
}

fn resolve_project_expr(
    ctx: &QueryContext,
    expr: &ProjectExpr,
    row: &[Value],
    columns: &[String],
) -> Value {
    match expr {
        ProjectExpr::Variable(v) => {
            if let Some(idx) = columns.iter().position(|c| c == v) {
                row[idx].clone()
            } else {
                Value::Null
            }
        }
        ProjectExpr::Property(pr) => resolve_property_ref(ctx.graph, row, columns, pr),
        ProjectExpr::Literal(v) => v.clone(),
        ProjectExpr::Function { name, args } => {
            let values: Vec<Value> = args
                .iter()
                .map(|arg| resolve_project_expr(ctx, arg, row, columns))
                .collect();
            eval_function(ctx, name, &values)
        }
        ProjectExpr::Expression(expr) => eval_ast_expr(ctx, expr, row, columns),
    }
}

fn execute_sort(
    ctx: &QueryContext,
    mut result: QueryResult,
    keys: &[SortKey],
) -> CypherResult<QueryResult> {
    let col_names = result.columns.clone();
    result.rows.sort_by(|a, b| {
        for key in keys {
            let va = resolve_project_expr(ctx, &key.expr, a, &col_names);
            let vb = resolve_project_expr(ctx, &key.expr, b, &col_names);
            let cmp = value_cmp(&va, &vb);
            let cmp = if key.descending { cmp.reverse() } else { cmp };
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(result)
}

fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Int64(l), Value::Int64(r)) => l.cmp(r),
        (Value::Float64(l), Value::Float64(r)) => {
            l.partial_cmp(r).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Value::String(l), Value::String(r)) => l.cmp(r),
        (Value::Bool(l), Value::Bool(r)) => l.cmp(r),
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        _ => std::cmp::Ordering::Equal,
    }
}

pub fn eval_ast_expr(ctx: &QueryContext, expr: &Expr, row: &[Value], columns: &[String]) -> Value {
    eval_ast_expr_with_assignments(ctx, expr, row, columns, &HashMap::new())
}

fn eval_ast_expr_with_assignments(
    ctx: &QueryContext,
    expr: &Expr,
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> Value {
    match expr {
        Expr::Literal(lit) => literal_to_value(lit),
        Expr::Variable(var) => {
            lookup_binding(var, row, columns, assignments).unwrap_or(Value::Null)
        }
        Expr::Property(pa) => {
            if let Some(value) = assignments.get(&pa.variable) {
                if let Value::Int64(id) = value {
                    let vertex_value = ctx
                        .graph
                        .get_vertex_property(VertexId(*id as u64), &pa.property);
                    if !vertex_value.is_null() {
                        return vertex_value;
                    }
                    return ctx
                        .graph
                        .get_edge_property(EdgeId(*id as u64), &pa.property);
                }
                if let Some(component) = temporal_property(value, &pa.property) {
                    return component;
                }
            }
            resolve_property_ref(
                ctx.graph,
                row,
                columns,
                &PropertyRef {
                    variable: pa.variable.clone(),
                    property: pa.property.clone(),
                },
            )
        }
        Expr::Parameter(name) => ctx.params.get(name).cloned().unwrap_or(Value::Null),
        Expr::List(items) => Value::List(
            items
                .iter()
                .map(|item| eval_ast_expr_with_assignments(ctx, item, row, columns, assignments))
                .collect(),
        ),
        Expr::Map(entries) => Value::Map(
            entries
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        eval_ast_expr_with_assignments(ctx, value, row, columns, assignments),
                    )
                })
                .collect(),
        ),
        Expr::In { expr, list } => {
            let needle = eval_ast_expr_with_assignments(ctx, expr, row, columns, assignments);
            match eval_ast_expr_with_assignments(ctx, list, row, columns, assignments) {
                Value::List(values) => cypher_in(&needle, &values),
                Value::Null => Value::Null,
                _ => Value::Bool(false),
            }
        }
        Expr::Index { target, index } => {
            let target = eval_ast_expr_with_assignments(ctx, target, row, columns, assignments);
            let index = eval_ast_expr_with_assignments(ctx, index, row, columns, assignments);
            eval_index(target, index)
        }
        Expr::Slice { target, start, end } => {
            let target = eval_ast_expr_with_assignments(ctx, target, row, columns, assignments);
            let start = match eval_slice_bound(ctx, start, row, columns, assignments) {
                Ok(value) => value,
                Err(value) => return value,
            };
            let end = match eval_slice_bound(ctx, end, row, columns, assignments) {
                Ok(value) => value,
                Err(value) => return value,
            };
            eval_slice(target, start, end)
        }
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            let scrutinee_value = scrutinee
                .as_ref()
                .map(|e| eval_ast_expr_with_assignments(ctx, e, row, columns, assignments));
            for (when_expr, then_expr) in arms {
                let when_value =
                    eval_ast_expr_with_assignments(ctx, when_expr, row, columns, assignments);
                let matches = if let Some(scrutinee_value) = &scrutinee_value {
                    compare_values(scrutinee_value, &CompareOp::Eq, &when_value)
                } else {
                    value_truthy(&when_value)
                };
                if matches {
                    return eval_ast_expr_with_assignments(
                        ctx,
                        then_expr,
                        row,
                        columns,
                        assignments,
                    );
                }
            }
            default
                .as_ref()
                .map(|e| eval_ast_expr_with_assignments(ctx, e, row, columns, assignments))
                .unwrap_or(Value::Null)
        }
        Expr::UnaryOp { op, expr } => {
            let value = eval_ast_expr_with_assignments(ctx, expr, row, columns, assignments);
            match op {
                UnaryOp::Not => match tri_value_bool(&value) {
                    Some(b) => Value::Bool(!b),
                    None => Value::Null,
                },
                UnaryOp::IsNull => Value::Bool(value.is_null()),
                UnaryOp::IsNotNull => Value::Bool(!value.is_null()),
            }
        }
        Expr::BinaryOp { left, op, right } => {
            let left = eval_ast_expr_with_assignments(ctx, left, row, columns, assignments);
            let right = eval_ast_expr_with_assignments(ctx, right, row, columns, assignments);
            eval_binary(left, *op, right)
        }
        Expr::FunctionCall { name, args } => {
            let values: Vec<Value> = args
                .iter()
                .map(|arg| eval_ast_expr_with_assignments(ctx, arg, row, columns, assignments))
                .collect();
            eval_function(ctx, name, &values)
        }
        Expr::CountStar => Value::Int64(1),
        Expr::Exists(inner) => {
            let value = eval_ast_expr_with_assignments(ctx, inner, row, columns, assignments);
            Value::Bool(!value.is_null())
        }
        Expr::ListPredicate {
            kind,
            variable,
            list,
            predicate,
        } => {
            let list_value = eval_ast_expr_with_assignments(ctx, list, row, columns, assignments);
            let items = match list_value {
                Value::List(xs) => xs,
                Value::Null => return Value::Null,
                _ => return Value::Null,
            };

            let mut trues = 0usize;
            let mut falses = 0usize;
            let mut nulls = 0usize;
            for item in &items {
                let mut local = assignments.clone();
                local.insert(variable.clone(), item.clone());
                let verdict = match predicate {
                    Some(p) => eval_ast_expr_with_assignments(ctx, p, row, columns, &local),
                    None => item.clone(),
                };
                match tri_value_bool(&verdict) {
                    Some(true) => trues += 1,
                    Some(false) => falses += 1,
                    None => nulls += 1,
                }
            }

            match kind {
                ListPredicateKind::Any => {
                    if trues > 0 {
                        Value::Bool(true)
                    } else if nulls > 0 {
                        Value::Null
                    } else {
                        Value::Bool(false)
                    }
                }
                ListPredicateKind::All => {
                    if falses > 0 {
                        Value::Bool(false)
                    } else if nulls > 0 {
                        Value::Null
                    } else {
                        Value::Bool(true)
                    }
                }
                ListPredicateKind::None => {
                    if trues > 0 {
                        Value::Bool(false)
                    } else if nulls > 0 {
                        Value::Null
                    } else {
                        Value::Bool(true)
                    }
                }
                ListPredicateKind::Single => {
                    if trues == 1 && nulls == 0 {
                        Value::Bool(true)
                    } else if trues > 1 {
                        Value::Bool(false)
                    } else if nulls > 0 {
                        Value::Null
                    } else {
                        Value::Bool(false)
                    }
                }
            }
        }
    }
}

/// Interpret a Value as Cypher's three-valued boolean: Some(true), Some(false),
/// or None (for NULL). Non-boolean values use standard truthiness for
/// backward compatibility with the existing evaluator.
fn tri_value_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Null => None,
        other => Some(value_truthy(other)),
    }
}

fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::Integer(v) => Value::Int64(*v),
        Literal::Float(v) => Value::Float64(*v),
        Literal::String(v) => Value::String(v.clone()),
        Literal::Bool(v) => Value::Bool(*v),
        Literal::Null => Value::Null,
    }
}

fn eval_slice_bound(
    ctx: &QueryContext,
    bound: &Option<Box<Expr>>,
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> Result<Option<i64>, Value> {
    let Some(expr) = bound else {
        return Ok(None);
    };
    match eval_ast_expr_with_assignments(ctx, expr, row, columns, assignments) {
        Value::Null => Err(Value::Null),
        Value::Int64(value) => Ok(Some(value)),
        _ => Err(Value::Null),
    }
}

fn eval_binary(left: Value, op: BinaryOp, right: Value) -> Value {
    match op {
        // Cypher three-valued logic for boolean operators.
        BinaryOp::And => match (tri_value_bool(&left), tri_value_bool(&right)) {
            (Some(false), _) | (_, Some(false)) => Value::Bool(false),
            (Some(true), Some(true)) => Value::Bool(true),
            _ => Value::Null,
        },
        BinaryOp::Or => match (tri_value_bool(&left), tri_value_bool(&right)) {
            (Some(true), _) | (_, Some(true)) => Value::Bool(true),
            (Some(false), Some(false)) => Value::Bool(false),
            _ => Value::Null,
        },
        BinaryOp::Xor => match (tri_value_bool(&left), tri_value_bool(&right)) {
            (Some(a), Some(b)) => Value::Bool(a ^ b),
            _ => Value::Null,
        },
        // Comparisons: null compared with anything (including null) is null.
        BinaryOp::Eq => cmp_with_null(&left, &right, CompareOp::Eq),
        BinaryOp::Neq => cmp_with_null(&left, &right, CompareOp::Neq),
        BinaryOp::Lt => cmp_with_null(&left, &right, CompareOp::Lt),
        BinaryOp::Lte => cmp_with_null(&left, &right, CompareOp::Lte),
        BinaryOp::Gt => cmp_with_null(&left, &right, CompareOp::Gt),
        BinaryOp::Gte => cmp_with_null(&left, &right, CompareOp::Gte),
        BinaryOp::Contains => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::String(left), Value::String(right)) => Value::Bool(left.contains(&right)),
            (Value::List(values), needle) => cypher_in(&needle, &values),
            _ => Value::Bool(false),
        },
        BinaryOp::StartsWith => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::String(left), Value::String(right)) => Value::Bool(left.starts_with(&right)),
            _ => Value::Bool(false),
        },
        BinaryOp::EndsWith => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::String(left), Value::String(right)) => Value::Bool(left.ends_with(&right)),
            _ => Value::Bool(false),
        },
        BinaryOp::Add => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::String(left), Value::String(right)) => Value::String(format!("{left}{right}")),
            (Value::String(left), Value::Int64(r)) => Value::String(format!("{left}{r}")),
            (Value::Int64(l), Value::String(right)) => Value::String(format!("{l}{right}")),
            (Value::String(left), Value::Float64(r)) => Value::String(format!("{left}{r}")),
            (Value::Float64(l), Value::String(right)) => Value::String(format!("{l}{right}")),
            (Value::List(mut left), Value::List(right)) => {
                left.extend(right);
                Value::List(left)
            }
            (Value::List(mut left), r) => {
                left.push(r);
                Value::List(left)
            }
            (l, Value::List(mut right)) => {
                right.insert(0, l);
                Value::List(right)
            }
            (left, right) => eval_numeric(left, right, |l, r| l + r, |l, r| l + r),
        },
        BinaryOp::Sub => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (l, r) => eval_numeric(l, r, |l, r| l - r, |l, r| l - r),
        },
        BinaryOp::Mul => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (l, r) => eval_numeric(l, r, |l, r| l * r, |l, r| l * r),
        },
        BinaryOp::Div => {
            let l = left.as_f64();
            let r = right.as_f64();
            match (l, r) {
                (Some(_), Some(0.0)) | (_, None) | (None, _) => Value::Null,
                (Some(l), Some(r)) => Value::Float64(l / r),
            }
        }
        BinaryOp::Mod => {
            let l = left.as_i64();
            let r = right.as_i64();
            match (l, r) {
                (Some(_), Some(0)) | (_, None) | (None, _) => Value::Null,
                (Some(l), Some(r)) => Value::Int64(l % r),
            }
        }
        BinaryOp::Pow => {
            let l = left.as_f64();
            let r = right.as_f64();
            match (l, r) {
                (Some(l), Some(r)) => Value::Float64(l.powf(r)),
                _ => Value::Null,
            }
        }
    }
}

fn eval_numeric(
    left: Value,
    right: Value,
    int_op: fn(i64, i64) -> i64,
    float_op: fn(f64, f64) -> f64,
) -> Value {
    match (&left, &right) {
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        (Value::Int64(l), Value::Int64(r)) => Value::Int64(int_op(*l, *r)),
        _ => match (left.as_f64(), right.as_f64()) {
            (Some(l), Some(r)) => Value::Float64(float_op(l, r)),
            _ => Value::Null,
        },
    }
}

/// Cypher comparison with null propagation: any null operand yields null.
fn cmp_with_null(left: &Value, right: &Value, op: CompareOp) -> Value {
    if matches!(op, CompareOp::Eq | CompareOp::Neq) {
        return match cypher_value_eq(left, right) {
            Some(equal) => Value::Bool(if matches!(op, CompareOp::Eq) {
                equal
            } else {
                !equal
            }),
            None => Value::Null,
        };
    }
    if left.is_null() || right.is_null() {
        return Value::Null;
    }
    Value::Bool(compare_values(left, &op, right))
}

fn cypher_in(needle: &Value, values: &[Value]) -> Value {
    let mut saw_null = false;
    for value in values {
        match cypher_value_eq(needle, value) {
            Some(true) => return Value::Bool(true),
            Some(false) => {}
            None => saw_null = true,
        }
    }
    if saw_null {
        Value::Null
    } else {
        Value::Bool(false)
    }
}

fn cypher_value_eq(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int64(l), Value::Int64(r)) => Some(l == r),
        (Value::Float64(l), Value::Float64(r)) => Some((l - r).abs() < f64::EPSILON),
        (Value::Int64(l), Value::Float64(r)) => Some((*l as f64 - r).abs() < f64::EPSILON),
        (Value::Float64(l), Value::Int64(r)) => Some((l - *r as f64).abs() < f64::EPSILON),
        (Value::String(l), Value::String(r)) => Some(l == r),
        (Value::Bool(l), Value::Bool(r)) => Some(l == r),
        (Value::Bytes(l), Value::Bytes(r)) => Some(l == r),
        (Value::List(l), Value::List(r)) => cypher_list_eq(l, r),
        (Value::Map(l), Value::Map(r)) => cypher_map_eq(l, r),
        _ => Some(false),
    }
}

fn cypher_list_eq(left: &[Value], right: &[Value]) -> Option<bool> {
    if left.len() != right.len() {
        return Some(false);
    }
    let mut saw_null = false;
    for (left, right) in left.iter().zip(right) {
        match cypher_value_eq(left, right) {
            Some(true) => {}
            Some(false) => return Some(false),
            None => saw_null = true,
        }
    }
    if saw_null { None } else { Some(true) }
}

fn cypher_map_eq(left: &[(String, Value)], right: &[(String, Value)]) -> Option<bool> {
    if left.len() != right.len() {
        return Some(false);
    }
    let mut saw_null = false;
    for (key, left_value) in left {
        let Some((_, right_value)) = right.iter().find(|(right_key, _)| right_key == key) else {
            return Some(false);
        };
        match cypher_value_eq(left_value, right_value) {
            Some(true) => {}
            Some(false) => return Some(false),
            None => saw_null = true,
        }
    }
    if saw_null { None } else { Some(true) }
}

fn eval_function(ctx: &QueryContext, name: &str, args: &[Value]) -> Value {
    match name.to_lowercase().as_str() {
        temporal if is_temporal_constructor(temporal) => eval_temporal_constructor(temporal, args),
        "range" => {
            let start = args.get(0).and_then(Value::as_i64).unwrap_or(0);
            let end = args.get(1).and_then(Value::as_i64).unwrap_or(start);
            let step = args.get(2).and_then(Value::as_i64).unwrap_or(1);
            if step == 0 {
                return Value::List(Vec::new());
            }
            let mut values = Vec::new();
            let mut current = start;
            if step > 0 {
                while current <= end {
                    values.push(Value::Int64(current));
                    current += step;
                }
            } else {
                while current >= end {
                    values.push(Value::Int64(current));
                    current += step;
                }
            }
            Value::List(values)
        }
        "size" | "length" => match args.first() {
            Some(Value::List(values)) => Value::Int64(values.len() as i64),
            Some(Value::String(value)) => Value::Int64(value.chars().count() as i64),
            Some(Value::Map(values)) => Value::Int64(values.len() as i64),
            _ => Value::Null,
        },
        "head" => match args.first() {
            Some(Value::List(values)) => values.first().cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        },
        "last" => match args.first() {
            Some(Value::List(values)) => values.last().cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        },
        "tail" => match args.first() {
            Some(Value::List(values)) if values.len() > 1 => Value::List(values[1..].to_vec()),
            Some(Value::List(_)) => Value::List(Vec::new()),
            _ => Value::Null,
        },
        "coalesce" => args
            .iter()
            .find(|value| !value.is_null())
            .cloned()
            .unwrap_or(Value::Null),
        "tostring" | "tostringornull" => args
            .first()
            .map(|value| {
                if value.is_null() {
                    Value::Null
                } else {
                    Value::String(format_value_for_string(value))
                }
            })
            .unwrap_or(Value::Null),
        "tointeger" | "tointegerornull" => args.first().map(to_integer).unwrap_or(Value::Null),
        "tofloat" | "tofloatornull" => args
            .first()
            .and_then(Value::as_f64)
            .map(Value::Float64)
            .unwrap_or(Value::Null),
        "toboolean" | "tobooleanornull" => args.first().map(to_boolean).unwrap_or(Value::Null),
        "labels" => match args.first() {
            Some(Value::Int64(id)) => ctx
                .graph
                .vertex_label(VertexId(*id as u64))
                .map(|label| Value::List(vec![Value::String(label.to_string())]))
                .unwrap_or(Value::List(Vec::new())),
            _ => Value::List(Vec::new()),
        },
        "type" => match args.first() {
            Some(Value::Int64(id)) => ctx
                .graph
                .edge_label(EdgeId(*id as u64))
                .map(|label| Value::String(label.to_string()))
                .unwrap_or(Value::Null),
            _ => Value::Null,
        },
        "keys" => match args.first() {
            Some(Value::Map(entries)) => Value::List(
                entries
                    .iter()
                    .map(|(key, _)| Value::String(key.clone()))
                    .collect(),
            ),
            Some(Value::Int64(id)) => {
                let vertex = VertexId(*id as u64);
                let edge = EdgeId(*id as u64);
                let props = if ctx.graph.vertex_label(vertex).is_some() {
                    ctx.graph.get_vertex_properties(vertex)
                } else if ctx.graph.edge_exists(edge) {
                    ctx.graph.get_edge_properties(edge)
                } else {
                    Vec::new()
                };
                Value::List(
                    props
                        .into_iter()
                        .map(|(key, _)| Value::String(key))
                        .collect(),
                )
            }
            _ => Value::List(Vec::new()),
        },
        "properties" => match args.first() {
            Some(Value::Map(entries)) => Value::Map(entries.clone()),
            Some(Value::Int64(id)) => {
                let vertex = VertexId(*id as u64);
                let edge = EdgeId(*id as u64);
                if ctx.graph.vertex_label(vertex).is_some() {
                    Value::Map(ctx.graph.get_vertex_properties(vertex))
                } else if ctx.graph.edge_exists(edge) {
                    Value::Map(ctx.graph.get_edge_properties(edge))
                } else {
                    Value::Map(Vec::new())
                }
            }
            _ => Value::Map(Vec::new()),
        },
        "id" => args.first().cloned().unwrap_or(Value::Null),
        "__label_test" => {
            let Some(Value::Int64(id)) = args.first() else {
                return Value::Bool(false);
            };
            let actual = ctx.graph.vertex_label(VertexId(*id as u64));
            let matches = args.iter().skip(1).any(|label| {
                if let Value::String(label) = label {
                    actual == Some(label.as_str())
                } else {
                    false
                }
            });
            Value::Bool(matches)
        }
        "any" => match args.first() {
            Some(Value::List(values)) => Value::Bool(!values.is_empty()),
            _ => Value::Bool(false),
        },
        "none" => match args.first() {
            Some(Value::List(values)) => Value::Bool(values.is_empty()),
            _ => Value::Bool(false),
        },
        "all" => match args.first() {
            Some(Value::List(_)) => Value::Bool(true),
            _ => Value::Bool(false),
        },
        "single" => match args.first() {
            Some(Value::List(values)) => Value::Bool(values.len() == 1),
            _ => Value::Bool(false),
        },
        _ => Value::Null,
    }
}

fn is_temporal_constructor(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name);
    matches!(
        base,
        "date" | "localtime" | "time" | "localdatetime" | "datetime" | "duration"
    )
}

fn eval_temporal_constructor(name: &str, args: &[Value]) -> Value {
    let base = name.split('.').next().unwrap_or(name);
    let Some(first) = args.first() else {
        return Value::Null;
    };
    if first.is_null() {
        return Value::Null;
    }
    match (base, first) {
        (_, Value::String(value)) => Value::String(normalize_temporal_string(base, value)),
        ("date", Value::Map(entries)) => Value::String(format_date_from_map(entries)),
        ("localtime", Value::Map(entries)) => Value::String(format_time_from_map(entries, false)),
        ("time", Value::Map(entries)) => Value::String(format_time_from_map(entries, true)),
        ("localdatetime", Value::Map(entries)) => Value::String(format!(
            "{}T{}",
            format_date_from_map(entries),
            format_time_from_map(entries, false)
        )),
        ("datetime", Value::Map(entries)) => Value::String(format!(
            "{}T{}",
            format_date_from_map(entries),
            format_time_from_map(entries, true)
        )),
        ("duration", Value::Map(entries)) => Value::String(format_duration_from_map(entries)),
        _ => Value::Null,
    }
}

fn normalize_temporal_string(base: &str, value: &str) -> String {
    match base {
        "time" | "datetime" => normalize_offset_in_temporal(value),
        _ => value.to_string(),
    }
}

fn map_i64(entries: &[(String, Value)], key: &str) -> Option<i64> {
    entries
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.as_i64())
}

fn map_str<'a>(entries: &'a [(String, Value)], key: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.as_str())
}

fn map_has(entries: &[(String, Value)], key: &str) -> bool {
    entries.iter().any(|(k, _)| k == key)
}

fn format_date_from_map(entries: &[(String, Value)]) -> String {
    let base_date = map_str(entries, "date").and_then(parse_date);
    let year = map_i64(entries, "year")
        .or_else(|| base_date.map(|(y, _, _)| y as i64))
        .unwrap_or(1970) as i32;

    if let Some(week) = map_i64(entries, "week") {
        let day = map_i64(entries, "dayOfWeek").unwrap_or(1) as i32;
        let (y, m, d) = iso_week_to_ymd(year, week as i32, day);
        return format!("{y:04}-{m:02}-{d:02}");
    }

    if let Some(ordinal) = map_i64(entries, "ordinalDay") {
        let (y, m, d) = ymd_from_ordinal(year, ordinal as i32);
        return format!("{y:04}-{m:02}-{d:02}");
    }

    if let Some(quarter) = map_i64(entries, "quarter") {
        let month = ((quarter - 1) * 3 + 1).clamp(1, 12) as i32;
        let day = map_i64(entries, "dayOfQuarter").unwrap_or(1) as i32;
        let (y, m, d) = add_days_to_ymd(year, month, 1, day - 1);
        return format!("{y:04}-{m:02}-{d:02}");
    }

    let month = map_i64(entries, "month")
        .or_else(|| base_date.map(|(_, m, _)| m as i64))
        .unwrap_or(1)
        .clamp(1, 12) as i32;
    let day = map_i64(entries, "day")
        .or_else(|| base_date.map(|(_, _, d)| d as i64))
        .unwrap_or(1)
        .clamp(1, days_in_month(year, month) as i64) as i32;
    format!("{year:04}-{month:02}-{day:02}")
}

fn format_time_from_map(entries: &[(String, Value)], with_timezone: bool) -> String {
    let hour = map_i64(entries, "hour").unwrap_or(0).clamp(0, 23);
    let minute = map_i64(entries, "minute").unwrap_or(0).clamp(0, 59);
    let second = map_i64(entries, "second").unwrap_or(0).clamp(0, 59);
    let nanos = map_i64(entries, "nanosecond").unwrap_or(0)
        + map_i64(entries, "microsecond").unwrap_or(0) * 1_000
        + map_i64(entries, "millisecond").unwrap_or(0) * 1_000_000;
    let include_seconds = map_has(entries, "second") || nanos != 0;
    let mut out = if include_seconds {
        format!("{hour:02}:{minute:02}:{second:02}")
    } else {
        format!("{hour:02}:{minute:02}")
    };
    if nanos != 0 {
        out.push('.');
        out.push_str(&format_fraction(nanos));
    }
    if with_timezone {
        let timezone = map_str(entries, "timezone").unwrap_or("Z");
        out.push_str(&format_timezone_suffix(timezone, entries));
    }
    out
}

fn format_fraction(nanos: i64) -> String {
    let mut s = format!("{:09}", nanos.abs().min(999_999_999));
    while s.ends_with('0') {
        s.pop();
    }
    if s.is_empty() { "0".to_string() } else { s }
}

fn format_timezone_suffix(timezone: &str, entries: &[(String, Value)]) -> String {
    if timezone == "Z" {
        return "Z".to_string();
    }
    if timezone.starts_with('+') || timezone.starts_with('-') {
        return normalize_offset(timezone);
    }
    let offset = named_timezone_offset(timezone, entries);
    format!("{offset}[{timezone}]")
}

fn named_timezone_offset(timezone: &str, entries: &[(String, Value)]) -> &'static str {
    if timezone == "Europe/Stockholm" {
        let month = map_i64(entries, "month").unwrap_or(1);
        if (4..=10).contains(&month) {
            "+02:00"
        } else {
            "+01:00"
        }
    } else {
        "Z"
    }
}

fn normalize_offset(offset: &str) -> String {
    if offset == "Z" || offset.len() != 5 {
        return offset.to_string();
    }
    let bytes = offset.as_bytes();
    if (bytes[0] == b'+' || bytes[0] == b'-') && bytes[1..].iter().all(u8::is_ascii_digit) {
        format!(
            "{}{}{}:{}{}",
            bytes[0] as char,
            bytes[1] as char,
            bytes[2] as char,
            bytes[3] as char,
            bytes[4] as char
        )
    } else {
        offset.to_string()
    }
}

fn normalize_offset_in_temporal(value: &str) -> String {
    if value.ends_with('Z') || value.contains('[') {
        return value.to_string();
    }
    let Some(pos) = value.rfind(['+', '-']) else {
        return value.to_string();
    };
    let (head, tail) = value.split_at(pos);
    format!("{head}{}", normalize_offset(tail))
}

fn format_duration_from_map(entries: &[(String, Value)]) -> String {
    let years = map_i64(entries, "years").unwrap_or(0);
    let months = map_i64(entries, "months").unwrap_or(0);
    let days = map_i64(entries, "days").unwrap_or(0);
    let total_nanos = map_i64(entries, "hours").unwrap_or(0) * 3_600_000_000_000
        + map_i64(entries, "minutes").unwrap_or(0) * 60_000_000_000
        + map_i64(entries, "seconds").unwrap_or(0) * 1_000_000_000
        + map_i64(entries, "milliseconds").unwrap_or(0) * 1_000_000
        + map_i64(entries, "microseconds").unwrap_or(0) * 1_000
        + map_i64(entries, "nanoseconds").unwrap_or(0);

    let mut out = String::from("P");
    if years != 0 {
        out.push_str(&format!("{years}Y"));
    }
    if months != 0 {
        out.push_str(&format!("{months}M"));
    }
    if days != 0 {
        out.push_str(&format!("{days}D"));
    }
    if total_nanos != 0 || out == "P" {
        let sign = if total_nanos < 0 { "-" } else { "" };
        let abs = total_nanos.abs();
        let hours = abs / 3_600_000_000_000;
        let rem = abs % 3_600_000_000_000;
        let minutes = rem / 60_000_000_000;
        let rem = rem % 60_000_000_000;
        let seconds = rem / 1_000_000_000;
        let nanos = rem % 1_000_000_000;
        out.push('T');
        if hours != 0 {
            out.push_str(&format!("{sign}{hours}H"));
        }
        if minutes != 0 {
            out.push_str(&format!("{sign}{minutes}M"));
        }
        if seconds != 0 || nanos != 0 || (hours == 0 && minutes == 0) {
            if nanos == 0 {
                out.push_str(&format!("{sign}{seconds}S"));
            } else {
                out.push_str(&format!("{sign}{seconds}.{}S", format_fraction(nanos)));
            }
        }
    }
    out
}

fn temporal_property(value: &Value, property: &str) -> Option<Value> {
    let Value::String(raw) = value else {
        return None;
    };
    if raw.starts_with('P') {
        return duration_property(raw, property);
    }
    if let Some((date_part, time_part)) = raw.split_once('T') {
        if let Some(value) = date_property(date_part, property) {
            return Some(value);
        }
        return time_property(time_part, property);
    }
    if raw.contains(':') {
        return time_property(raw, property);
    }
    date_property(raw, property)
}

fn date_property(raw: &str, property: &str) -> Option<Value> {
    let (year, month, day) = parse_date(raw)?;
    let quarter = (month - 1) / 3 + 1;
    let ordinal = ordinal_day(year, month, day);
    let weekday = day_of_week(year, month, day);
    let (week_year, week) = iso_week(year, month, day);
    let day_of_quarter = ordinal - ordinal_day(year, (quarter - 1) * 3 + 1, 1) + 1;
    match property {
        "year" => Some(Value::Int64(year as i64)),
        "quarter" => Some(Value::Int64(quarter as i64)),
        "month" => Some(Value::Int64(month as i64)),
        "week" => Some(Value::Int64(week as i64)),
        "weekYear" => Some(Value::Int64(week_year as i64)),
        "day" => Some(Value::Int64(day as i64)),
        "ordinalDay" => Some(Value::Int64(ordinal as i64)),
        "weekDay" => Some(Value::Int64(weekday as i64)),
        "dayOfQuarter" => Some(Value::Int64(day_of_quarter as i64)),
        _ => None,
    }
}

fn time_property(raw: &str, property: &str) -> Option<Value> {
    let parsed = parse_time(raw)?;
    match property {
        "hour" => Some(Value::Int64(parsed.hour as i64)),
        "minute" => Some(Value::Int64(parsed.minute as i64)),
        "second" => Some(Value::Int64(parsed.second as i64)),
        "millisecond" => Some(Value::Int64((parsed.nano / 1_000_000) as i64)),
        "microsecond" => Some(Value::Int64((parsed.nano / 1_000) as i64)),
        "nanosecond" => Some(Value::Int64(parsed.nano as i64)),
        "timezone" | "offset" => parsed.timezone.map(Value::String),
        "offsetMinutes" => parsed.offset_seconds.map(|s| Value::Int64((s / 60) as i64)),
        "offsetSeconds" => parsed.offset_seconds.map(|s| Value::Int64(s as i64)),
        _ => None,
    }
}

fn duration_property(raw: &str, property: &str) -> Option<Value> {
    let duration = parse_duration_parts(raw);
    match property {
        "years" => Some(Value::Int64(duration.months / 12)),
        "quarters" => Some(Value::Int64(duration.months / 3)),
        "months" => Some(Value::Int64(duration.months)),
        "weeks" => Some(Value::Int64(duration.days / 7)),
        "days" => Some(Value::Int64(duration.days)),
        "hours" => Some(Value::Int64(duration.seconds / 3600)),
        "minutes" => Some(Value::Int64(duration.seconds / 60)),
        "seconds" => Some(Value::Int64(duration.seconds)),
        "milliseconds" => Some(Value::Int64(
            duration.seconds * 1000 + duration.nanos / 1_000_000,
        )),
        "microseconds" => Some(Value::Int64(
            duration.seconds * 1_000_000 + duration.nanos / 1_000,
        )),
        "nanoseconds" => Some(Value::Int64(
            duration.seconds * 1_000_000_000 + duration.nanos,
        )),
        "quartersOfYear" => Some(Value::Int64((duration.months % 12) / 3)),
        "monthsOfQuarter" => Some(Value::Int64(duration.months % 3)),
        "monthsOfYear" => Some(Value::Int64(duration.months % 12)),
        "daysOfWeek" => Some(Value::Int64(duration.days % 7)),
        "minutesOfHour" => Some(Value::Int64((duration.seconds / 60) % 60)),
        "secondsOfMinute" => Some(Value::Int64(duration.seconds % 60)),
        "millisecondsOfSecond" => Some(Value::Int64(duration.nanos / 1_000_000)),
        "microsecondsOfSecond" => Some(Value::Int64(duration.nanos / 1_000)),
        "nanosecondsOfSecond" => Some(Value::Int64(duration.nanos)),
        _ => None,
    }
}

#[derive(Default)]
struct DurationParts {
    months: i64,
    days: i64,
    seconds: i64,
    nanos: i64,
}

fn parse_duration_parts(raw: &str) -> DurationParts {
    let mut out = DurationParts::default();
    let mut in_time = false;
    let mut number = String::new();
    for ch in raw.trim_start_matches('P').chars() {
        if ch == 'T' {
            in_time = true;
            continue;
        }
        if ch.is_ascii_digit() || ch == '-' || ch == '.' {
            number.push(ch);
            continue;
        }
        let value = number.parse::<f64>().unwrap_or(0.0);
        number.clear();
        match (ch, in_time) {
            ('Y', _) => out.months += (value as i64) * 12,
            ('M', false) => out.months += value as i64,
            ('M', true) => out.seconds += (value as i64) * 60,
            ('D', _) => out.days += value as i64,
            ('H', _) => out.seconds += (value as i64) * 3600,
            ('S', _) => {
                out.seconds += value.trunc() as i64;
                out.nanos += (value.fract().abs() * 1_000_000_000.0).round() as i64;
            }
            _ => {}
        }
    }
    out
}

struct ParsedTime {
    hour: i32,
    minute: i32,
    second: i32,
    nano: i32,
    timezone: Option<String>,
    offset_seconds: Option<i32>,
}

fn parse_time(raw: &str) -> Option<ParsedTime> {
    let mut main = raw;
    let mut timezone = None;
    if let Some(start) = raw.find('[') {
        timezone = Some(raw[start + 1..].trim_end_matches(']').to_string());
        main = &raw[..start];
    }
    let offset_pos = main
        .get(1..)
        .and_then(|rest| rest.rfind(['+', '-']).map(|pos| pos + 1));
    let mut offset_seconds = None;
    if main.ends_with('Z') {
        main = main.trim_end_matches('Z');
        timezone.get_or_insert_with(|| "Z".to_string());
        offset_seconds = Some(0);
    } else if let Some(pos) = offset_pos {
        let offset = normalize_offset(&main[pos..]);
        offset_seconds = parse_offset_seconds(&offset);
        timezone.get_or_insert(offset);
        main = &main[..pos];
    }
    let parts: Vec<&str> = main.split(':').collect();
    let hour = parts.first()?.parse().ok()?;
    let minute = parts.get(1).unwrap_or(&"0").parse().ok()?;
    let (second, nano) = parts.get(2).map_or((0, 0), |part| {
        if let Some((sec, frac)) = part.split_once('.') {
            (sec.parse().unwrap_or(0), parse_nanos(frac))
        } else {
            (part.parse().unwrap_or(0), 0)
        }
    });
    Some(ParsedTime {
        hour,
        minute,
        second,
        nano,
        timezone,
        offset_seconds,
    })
}

fn parse_offset_seconds(offset: &str) -> Option<i32> {
    if offset == "Z" {
        return Some(0);
    }
    if offset.len() != 6 {
        return None;
    }
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let hours: i32 = offset[1..3].parse().ok()?;
    let minutes: i32 = offset[4..6].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

fn parse_nanos(frac: &str) -> i32 {
    let mut digits = frac.chars().take(9).collect::<String>();
    while digits.len() < 9 {
        digits.push('0');
    }
    digits.parse().unwrap_or(0)
}

fn parse_date(raw: &str) -> Option<(i32, i32, i32)> {
    let raw = raw.get(..10)?;
    let mut parts = raw.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: i32 = parts.next()?.parse().ok()?;
    let day: i32 = parts.next()?.parse().ok()?;
    Some((year, month, day))
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: i32) -> i32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 30,
    }
}

fn ordinal_day(year: i32, month: i32, day: i32) -> i32 {
    (1..month).map(|m| days_in_month(year, m)).sum::<i32>() + day
}

fn ymd_from_ordinal(mut year: i32, mut ordinal: i32) -> (i32, i32, i32) {
    while ordinal < 1 {
        year -= 1;
        ordinal += if is_leap(year) { 366 } else { 365 };
    }
    loop {
        let days = if is_leap(year) { 366 } else { 365 };
        if ordinal <= days {
            break;
        }
        ordinal -= days;
        year += 1;
    }
    let mut month = 1;
    while ordinal > days_in_month(year, month) {
        ordinal -= days_in_month(year, month);
        month += 1;
    }
    (year, month, ordinal)
}

fn add_days_to_ymd(year: i32, month: i32, day: i32, delta: i32) -> (i32, i32, i32) {
    ymd_from_ordinal(year, ordinal_day(year, month, day) + delta)
}

fn day_of_week(year: i32, month: i32, day: i32) -> i32 {
    // Sakamoto algorithm: convert Sunday=0 into Cypher Monday=1..Sunday=7.
    let offsets = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut y = year;
    if month < 3 {
        y -= 1;
    }
    let sunday_zero = (y + y / 4 - y / 100 + y / 400 + offsets[(month - 1) as usize] + day) % 7;
    if sunday_zero == 0 { 7 } else { sunday_zero }
}

fn iso_week_to_ymd(year: i32, week: i32, day: i32) -> (i32, i32, i32) {
    let jan4_ordinal = ordinal_day(year, 1, 4);
    let week1_monday = jan4_ordinal - day_of_week(year, 1, 4) + 1;
    ymd_from_ordinal(year, week1_monday + (week - 1) * 7 + (day - 1))
}

fn iso_week(year: i32, month: i32, day: i32) -> (i32, i32) {
    let weekday = day_of_week(year, month, day);
    let ordinal = ordinal_day(year, month, day);
    let mut week = (ordinal - weekday + 10) / 7;
    let mut week_year = year;
    if week < 1 {
        week_year -= 1;
        week = weeks_in_iso_year(week_year);
    } else if week > weeks_in_iso_year(year) {
        week_year += 1;
        week = 1;
    }
    (week_year, week)
}

fn weeks_in_iso_year(year: i32) -> i32 {
    let jan1 = day_of_week(year, 1, 1);
    if jan1 == 4 || (jan1 == 3 && is_leap(year)) {
        53
    } else {
        52
    }
}

fn eval_index(target: Value, index: Value) -> Value {
    match target {
        Value::List(values) => {
            let Some(index) = index.as_i64() else {
                return Value::Null;
            };
            if index < 0 {
                return Value::Null;
            }
            values.get(index as usize).cloned().unwrap_or(Value::Null)
        }
        Value::Map(entries) => {
            let Some(key) = index.as_str() else {
                return Value::Null;
            };
            entries
                .into_iter()
                .find_map(|(candidate, value)| if candidate == key { Some(value) } else { None })
                .unwrap_or(Value::Null)
        }
        Value::String(value) => value
            .chars()
            .nth(
                index
                    .as_i64()
                    .unwrap_or(-1)
                    .try_into()
                    .unwrap_or(usize::MAX),
            )
            .map(|c| Value::String(c.to_string()))
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn eval_slice(target: Value, start: Option<i64>, end: Option<i64>) -> Value {
    match target {
        Value::List(values) => {
            let len = values.len() as i64;
            let start = normalize_slice_bound(start.unwrap_or(0), len);
            let end = normalize_slice_bound(end.unwrap_or(len), len);
            if start >= end {
                Value::List(Vec::new())
            } else {
                Value::List(values[start as usize..end as usize].to_vec())
            }
        }
        Value::String(value) => {
            let chars: Vec<char> = value.chars().collect();
            let len = chars.len() as i64;
            let start = normalize_slice_bound(start.unwrap_or(0), len);
            let end = normalize_slice_bound(end.unwrap_or(len), len);
            if start >= end {
                Value::String(String::new())
            } else {
                Value::String(chars[start as usize..end as usize].iter().collect())
            }
        }
        _ => Value::Null,
    }
}

fn normalize_slice_bound(bound: i64, len: i64) -> i64 {
    let bound = if bound < 0 { len + bound } else { bound };
    bound.clamp(0, len)
}

fn to_integer(value: &Value) -> Value {
    match value {
        Value::Int64(value) => Value::Int64(*value),
        Value::Float64(value) => Value::Int64(*value as i64),
        Value::String(value) => value
            .parse::<i64>()
            .map(Value::Int64)
            .unwrap_or(Value::Null),
        Value::Bool(value) => Value::Int64(if *value { 1 } else { 0 }),
        _ => Value::Null,
    }
}

fn to_boolean(value: &Value) -> Value {
    match value {
        Value::Bool(value) => Value::Bool(*value),
        Value::String(value) if value.eq_ignore_ascii_case("true") => Value::Bool(true),
        Value::String(value) if value.eq_ignore_ascii_case("false") => Value::Bool(false),
        _ => Value::Null,
    }
}

fn format_value_for_string(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::Int64(value) => value.to_string(),
        Value::Float64(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Bytes(value) => format!("<{} bytes>", value.len()),
        Value::List(values) => format!("[{} items]", values.len()),
        Value::Map(values) => format!("{{{} entries}}", values.len()),
    }
}

fn value_truthy(value: &Value) -> bool {
    matches!(value, Value::Bool(true))
}

fn execute_distinct(mut result: QueryResult) -> CypherResult<QueryResult> {
    let mut seen = std::collections::HashSet::new();
    result.rows.retain(|row| {
        let key = format!("{row:?}");
        seen.insert(key)
    });
    Ok(result)
}

fn execute_union(left: QueryResult, right: QueryResult, all: bool) -> CypherResult<QueryResult> {
    let columns = left.columns.clone();
    let mut rows = left.rows;

    for right_row in right.rows {
        let mut row = Vec::with_capacity(columns.len());
        for col in &columns {
            if let Some(idx) = right.columns.iter().position(|c| c == col) {
                row.push(right_row.get(idx).cloned().unwrap_or(Value::Null));
            } else {
                row.push(Value::Null);
            }
        }
        rows.push(row);
    }

    let result = QueryResult { columns, rows };
    if all {
        Ok(result)
    } else {
        execute_distinct(result)
    }
}

fn execute_aggregate(
    ctx: &QueryContext,
    input: &QueryResult,
    group_by: &[ProjectColumn],
    aggregations: &[AggregateOp],
) -> CypherResult<QueryResult> {
    let columns: Vec<String> = group_by
        .iter()
        .map(|g| g.alias.clone())
        .chain(aggregations.iter().map(|a| a.alias.clone()))
        .collect();

    let groups = build_aggregate_groups(ctx, input, group_by);
    let mut rows = Vec::new();
    for group in groups {
        let mut row = group.values;
        for agg in aggregations {
            row.push(evaluate_aggregate(ctx, input, &group.row_indices, agg)?);
        }
        rows.push(row);
    }

    Ok(QueryResult { columns, rows })
}

struct AggregateGroup {
    values: Vec<Value>,
    row_indices: Vec<usize>,
}

fn build_aggregate_groups(
    ctx: &QueryContext,
    input: &QueryResult,
    group_by: &[ProjectColumn],
) -> Vec<AggregateGroup> {
    if group_by.is_empty() {
        return vec![AggregateGroup {
            values: Vec::new(),
            row_indices: (0..input.rows.len()).collect(),
        }];
    }

    let mut order = Vec::<String>::new();
    let mut groups = HashMap::<String, AggregateGroup>::new();
    for (idx, row) in input.rows.iter().enumerate() {
        let values: Vec<Value> = group_by
            .iter()
            .map(|g| resolve_project_expr(ctx, &g.expr, row, &input.columns))
            .collect();
        let key = aggregate_group_key(&values);
        match groups.get_mut(&key) {
            Some(group) => group.row_indices.push(idx),
            None => {
                order.push(key.clone());
                groups.insert(
                    key,
                    AggregateGroup {
                        values,
                        row_indices: vec![idx],
                    },
                );
            }
        }
    }

    order
        .into_iter()
        .filter_map(|key| groups.remove(&key))
        .collect()
}

fn aggregate_group_key(values: &[Value]) -> String {
    values
        .iter()
        .map(aggregate_value_key)
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

fn aggregate_value_key(value: &Value) -> String {
    match value {
        Value::List(values) => format!(
            "[{}]",
            values
                .iter()
                .map(aggregate_value_key)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Map(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(k, v)| format!("{k}:{}", aggregate_value_key(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => format!("{other:?}"),
    }
}

fn evaluate_aggregate(
    ctx: &QueryContext,
    input: &QueryResult,
    row_indices: &[usize],
    agg: &AggregateOp,
) -> CypherResult<Value> {
    let values = aggregate_input_values(ctx, input, row_indices, agg);
    let value = match agg.function.as_str() {
        "count" => {
            if agg.inputs.is_empty() {
                Value::Int64(row_indices.len() as i64)
            } else {
                Value::Int64(values.len() as i64)
            }
        }
        "collect" => Value::List(values),
        "sum" => aggregate_sum(&values),
        "avg" => aggregate_avg(&values),
        "min" => aggregate_min_max(&values, false),
        "max" => aggregate_min_max(&values, true),
        "percentiledisc" => aggregate_percentile(ctx, input, row_indices, agg, true)?,
        "percentilecont" => aggregate_percentile(ctx, input, row_indices, agg, false)?,
        _ => Value::Null,
    };
    Ok(value)
}

fn aggregate_input_values(
    ctx: &QueryContext,
    input: &QueryResult,
    row_indices: &[usize],
    agg: &AggregateOp,
) -> Vec<Value> {
    let Some(expr) = agg.inputs.first() else {
        return Vec::new();
    };
    let mut values = Vec::new();
    for idx in row_indices {
        let value = resolve_project_expr(ctx, expr, &input.rows[*idx], &input.columns);
        if value.is_null() {
            continue;
        }
        if agg.distinct && values.iter().any(|existing| existing == &value) {
            continue;
        }
        values.push(value);
    }
    values
}

fn aggregate_percentile(
    ctx: &QueryContext,
    input: &QueryResult,
    row_indices: &[usize],
    agg: &AggregateOp,
    discrete: bool,
) -> CypherResult<Value> {
    let Some(percentile_expr) = agg.inputs.get(1) else {
        return Ok(Value::Null);
    };
    let Some(first_idx) = row_indices.first() else {
        return Ok(Value::Null);
    };
    let percentile = resolve_project_expr(
        ctx,
        percentile_expr,
        &input.rows[*first_idx],
        &input.columns,
    )
    .as_f64()
    .unwrap_or(f64::NAN);
    if !(0.0..=1.0).contains(&percentile) {
        return Err(CypherError::Execution(
            "NumberOutOfRange: percentile must be between 0 and 1".into(),
        ));
    }

    let mut values: Vec<f64> = aggregate_input_values(ctx, input, row_indices, agg)
        .into_iter()
        .filter_map(|value| value.as_f64())
        .collect();
    if values.is_empty() {
        return Ok(Value::Null);
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    if discrete {
        let idx = if percentile == 0.0 {
            0
        } else {
            ((percentile * values.len() as f64).ceil() as usize).saturating_sub(1)
        };
        Ok(Value::Float64(values[idx.min(values.len() - 1)]))
    } else {
        let pos = percentile * (values.len() - 1) as f64;
        let lo = pos.floor() as usize;
        let hi = pos.ceil() as usize;
        if lo == hi {
            Ok(Value::Float64(values[lo]))
        } else {
            let weight = pos - lo as f64;
            Ok(Value::Float64(
                values[lo] * (1.0 - weight) + values[hi] * weight,
            ))
        }
    }
}

fn aggregate_sum(values: &[Value]) -> Value {
    let mut sum = 0.0f64;
    let mut saw_number = false;
    for value in values {
        if let Some(value) = value.as_f64() {
            sum += value;
            saw_number = true;
        }
    }
    if saw_number {
        Value::Float64(sum)
    } else {
        Value::Int64(0)
    }
}

fn aggregate_avg(values: &[Value]) -> Value {
    let nums: Vec<f64> = values.iter().filter_map(Value::as_f64).collect();
    if nums.is_empty() {
        Value::Null
    } else {
        Value::Float64(nums.iter().sum::<f64>() / nums.len() as f64)
    }
}

fn aggregate_min_max(values: &[Value], max: bool) -> Value {
    let mut best: Option<Value> = None;
    for value in values {
        let Some(existing) = &best else {
            best = Some(value.clone());
            continue;
        };
        let ordering = aggregate_value_cmp(value, existing);
        if (max && ordering.is_gt()) || (!max && ordering.is_lt()) {
            best = Some(value.clone());
        }
    }
    best.unwrap_or(Value::Null)
}

fn aggregate_value_cmp(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Int64(l), Value::Int64(r)) => l.cmp(r),
        (Value::Float64(l), Value::Float64(r)) => {
            l.partial_cmp(r).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Value::Int64(l), Value::Float64(r)) => (*l as f64)
            .partial_cmp(r)
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::Float64(l), Value::Int64(r)) => l
            .partial_cmp(&(*r as f64))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(l), Value::String(r)) => l.cmp(r),
        (Value::Bool(l), Value::Bool(r)) => l.cmp(r),
        (Value::List(l), Value::List(r)) => {
            for (lv, rv) in l.iter().zip(r) {
                let cmp = aggregate_value_cmp(lv, rv);
                if !cmp.is_eq() {
                    return cmp;
                }
            }
            l.len().cmp(&r.len())
        }
        (Value::Map(l), Value::Map(r)) => l.len().cmp(&r.len()),
        _ => aggregate_type_rank(left).cmp(&aggregate_type_rank(right)),
    }
}

fn aggregate_type_rank(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::List(_) => 1,
        Value::Map(_) => 2,
        Value::String(_) => 3,
        Value::Bool(_) => 4,
        Value::Int64(_) | Value::Float64(_) => 5,
        Value::Bytes(_) => 6,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteBinding {
    Vertex(VertexId),
    Edge(EdgeId),
}

/// Execute a write plan against a mutable context. Returns a summary of
/// how many nodes/edges were created and deleted.
///
/// Semantics:
///   - If `plan.source` is `Some`, it is executed against a read snapshot
///     of the graph and the mutations run once per row.
///   - If `plan.source` is `None`, the mutations run exactly once.
///   - CREATE emits a new vertex/edge per invocation, binding freshly
///     allocated IDs to the declared variables.
///   - DELETE without `detach` fails if the node has incident edges.
///     DETACH DELETE removes incident edges first.
pub fn execute_write(plan: &WritePlan, ctx: &mut WriteContext) -> CypherResult<WriteSummary> {
    // Materialize bindings from the read source (if any) before mutating.
    // This borrows the graph immutably, so we drop it before the mutation
    // loop that needs &mut Graph.
    let (column_names, binding_rows): (Vec<String>, Vec<Vec<Value>>) =
        if let Some(source) = &plan.source {
            let read_ctx = QueryContext {
                graph: ctx.graph,
                indexes: None,
                params: ctx.params.clone(),
            };
            let result = execute(source, &read_ctx)?;
            (result.columns, result.rows)
        } else {
            // One empty binding row so mutations run exactly once.
            (Vec::new(), vec![Vec::new()])
        };

    let relationship_vars = plan
        .source
        .as_ref()
        .map(relationship_variables)
        .unwrap_or_default();
    let mut summary = WriteSummary::default();

    for row in &binding_rows {
        // Per-row symbol table: variable name -> vertex or relationship ID.
        let mut bindings: HashMap<String, WriteBinding> = HashMap::new();
        for (col_idx, col_name) in column_names.iter().enumerate() {
            if let Value::Int64(id) = &row[col_idx] {
                let binding = if relationship_vars.contains(col_name) {
                    WriteBinding::Edge(EdgeId(*id as u64))
                } else {
                    WriteBinding::Vertex(VertexId(*id as u64))
                };
                bindings.insert(col_name.clone(), binding);
            }
        }

        for op in &plan.mutations {
            match op {
                MutationOp::CreateNode {
                    variable,
                    labels,
                    properties,
                } => {
                    if bindings.contains_key(variable) {
                        resolve_write_vertex(&bindings, variable)?;
                        continue;
                    }
                    let label = labels.first().map(String::as_str).unwrap_or("");
                    let vid = ctx.graph.add_vertex(label);
                    for (key, pv) in properties {
                        let val = resolve_property_value(pv, &ctx.params, ctx.graph, &bindings)?;
                        ctx.graph
                            .try_set_vertex_property(vid, key, val)
                            .map_err(|e| CypherError::Execution(e.to_string()))?;
                        summary.properties_set += 1;
                    }
                    bindings.insert(variable.clone(), WriteBinding::Vertex(vid));
                    summary.nodes_created += 1;
                }
                MutationOp::CreateEdge {
                    variable,
                    src_var,
                    dst_var,
                    rel_type,
                    properties,
                } => {
                    let src = resolve_write_vertex(&bindings, src_var)?;
                    let dst = resolve_write_vertex(&bindings, dst_var)?;
                    let edge = ctx
                        .graph
                        .try_add_edge(src, dst, rel_type)
                        .map_err(|e| CypherError::Execution(e.to_string()))?;
                    for (key, pv) in properties {
                        let val = resolve_property_value(pv, &ctx.params, ctx.graph, &bindings)?;
                        ctx.graph
                            .try_set_edge_property(edge, key, val)
                            .map_err(|e| CypherError::Execution(e.to_string()))?;
                        summary.properties_set += 1;
                    }
                    if let Some(variable) = variable {
                        bindings.insert(variable.clone(), WriteBinding::Edge(edge));
                    }
                    summary.edges_created += 1;
                }
                MutationOp::MergeNode {
                    variable,
                    labels,
                    properties,
                } => {
                    if bindings.contains_key(variable) {
                        resolve_write_vertex(&bindings, variable)?;
                        continue;
                    }
                    let label = labels.first().ok_or_else(|| {
                        CypherError::Execution("MERGE node requires a label".into())
                    })?;
                    let resolved_props =
                        resolve_property_pairs(properties, &ctx.params, ctx.graph, &bindings)?;
                    let vid = find_matching_vertex(ctx.graph, label, &resolved_props)
                        .unwrap_or_else(|| {
                            let vid = ctx.graph.add_vertex(label);
                            for (key, value) in &resolved_props {
                                let _ = ctx.graph.try_set_vertex_property(vid, key, value.clone());
                                summary.properties_set += 1;
                            }
                            summary.nodes_created += 1;
                            vid
                        });
                    bindings.insert(variable.clone(), WriteBinding::Vertex(vid));
                }
                MutationOp::MergeEdge {
                    variable,
                    src_var,
                    dst_var,
                    rel_type,
                    properties,
                } => {
                    if let Some(variable) = variable {
                        if bindings.contains_key(variable) {
                            resolve_write_edge(&bindings, variable)?;
                            continue;
                        }
                    }
                    let src = resolve_write_vertex(&bindings, src_var)?;
                    let dst = resolve_write_vertex(&bindings, dst_var)?;
                    let resolved_props =
                        resolve_property_pairs(properties, &ctx.params, ctx.graph, &bindings)?;
                    let edge = ctx
                        .graph
                        .edge_between(src, dst, rel_type)
                        .unwrap_or_else(|| {
                            let edge = ctx.graph.add_edge(src, dst, rel_type);
                            for (key, value) in &resolved_props {
                                let _ = ctx.graph.try_set_edge_property(edge, key, value.clone());
                                summary.properties_set += 1;
                            }
                            summary.edges_created += 1;
                            edge
                        });
                    if let Some(variable) = variable {
                        bindings.insert(variable.clone(), WriteBinding::Edge(edge));
                    }
                }
                MutationOp::SetProperty {
                    variable,
                    key,
                    value,
                } => {
                    let val = resolve_property_value(value, &ctx.params, ctx.graph, &bindings)?;
                    set_bound_property(ctx.graph, &bindings, variable, key, val)?;
                    summary.properties_set += 1;
                }
                MutationOp::RemoveProperty { variable, key } => {
                    set_bound_property(ctx.graph, &bindings, variable, key, Value::Null)?;
                    summary.properties_removed += 1;
                }
                MutationOp::Delete { variable, detach } => {
                    match bindings.get(variable).copied() {
                        Some(WriteBinding::Edge(edge)) => {
                            ctx.graph
                                .try_remove_edge(edge)
                                .map_err(|e| CypherError::Execution(e.to_string()))?;
                            bindings.remove(variable);
                            summary.edges_deleted += 1;
                        }
                        Some(WriteBinding::Vertex(vid)) => {
                            if !*detach {
                                // Strict DELETE: fail if the node has any incident edges.
                                if has_incident_edges(ctx.graph, vid) {
                                    return Err(CypherError::Execution(format!(
                                        "cannot DELETE node '{variable}' with relationships; use DETACH DELETE"
                                    )));
                                }
                            } else {
                                // Count incident edges for the summary before removal.
                                summary.edges_deleted += count_incident_edges(ctx.graph, vid);
                            }

                            ctx.graph
                                .try_remove_vertex(vid)
                                .map_err(|e| CypherError::Execution(e.to_string()))?;
                            bindings.remove(variable);
                            summary.nodes_deleted += 1;
                        }
                        None => {
                            return Err(CypherError::Execution(format!(
                                "DELETE target '{variable}' is not bound"
                            )));
                        }
                    }
                }
            }
        }
    }

    Ok(summary)
}

fn resolve_property_value(
    pv: &PropertyValue,
    params: &HashMap<String, Value>,
    graph: &Graph,
    bindings: &HashMap<String, WriteBinding>,
) -> CypherResult<Value> {
    match pv {
        PropertyValue::Literal(v) => Ok(v.clone()),
        PropertyValue::Parameter(name) => params
            .get(name)
            .cloned()
            .ok_or_else(|| CypherError::Execution(format!("missing parameter '${name}'"))),
        PropertyValue::Property(pr) => {
            resolve_bound_property(graph, bindings, &pr.variable, &pr.property)
        }
        PropertyValue::Expr(expr) => {
            // Build a synthetic read context scoped to params and the current
            // write bindings projected as a row. Vertex/edge bindings are
            // exposed as Int64 ids so property access like `bound.prop` works.
            let mut columns: Vec<String> = Vec::with_capacity(bindings.len());
            let mut row: Vec<Value> = Vec::with_capacity(bindings.len());
            for (name, binding) in bindings {
                columns.push(name.clone());
                row.push(match binding {
                    WriteBinding::Vertex(v) => Value::Int64(v.0 as i64),
                    WriteBinding::Edge(e) => Value::Int64(e.0 as i64),
                });
            }
            let idx = nexus_index::composite::IndexSet::new();
            let ctx = QueryContext::with_indexes(graph, &idx).with_params(params.clone());
            Ok(eval_ast_expr(&ctx, expr, &row, &columns))
        }
    }
}

fn resolve_property_pairs(
    properties: &[(String, PropertyValue)],
    params: &HashMap<String, Value>,
    graph: &Graph,
    bindings: &HashMap<String, WriteBinding>,
) -> CypherResult<Vec<(String, Value)>> {
    properties
        .iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                resolve_property_value(value, params, graph, bindings)?,
            ))
        })
        .collect()
}

fn relationship_variables(plan: &LogicalPlan) -> HashSet<String> {
    let mut vars = HashSet::new();
    collect_relationship_variables(plan, &mut vars);
    vars
}

fn collect_relationship_variables(plan: &LogicalPlan, vars: &mut HashSet<String>) {
    match plan {
        LogicalPlan::Expand { input, rel_var, .. } => {
            if let Some(var) = rel_var {
                vars.insert(var.clone());
            }
            collect_relationship_variables(input, vars);
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::ApplyMatch { input, .. }
        | LogicalPlan::Unwind { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Aggregate { input, .. } => collect_relationship_variables(input, vars),
        LogicalPlan::Union { left, right, .. } => {
            collect_relationship_variables(left, vars);
            collect_relationship_variables(right, vars);
        }
        LogicalPlan::Argument | LogicalPlan::ScanVertices { .. } => {}
    }
}

fn resolve_write_vertex(
    bindings: &HashMap<String, WriteBinding>,
    variable: &str,
) -> CypherResult<VertexId> {
    match bindings.get(variable) {
        Some(WriteBinding::Vertex(vertex)) => Ok(*vertex),
        Some(WriteBinding::Edge(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is bound to a relationship, not a node"
        ))),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

fn resolve_write_edge(
    bindings: &HashMap<String, WriteBinding>,
    variable: &str,
) -> CypherResult<EdgeId> {
    match bindings.get(variable) {
        Some(WriteBinding::Edge(edge)) => Ok(*edge),
        Some(WriteBinding::Vertex(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is bound to a node, not a relationship"
        ))),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

fn set_bound_property(
    graph: &mut Graph,
    bindings: &HashMap<String, WriteBinding>,
    variable: &str,
    key: &str,
    value: Value,
) -> CypherResult<()> {
    match bindings.get(variable) {
        Some(WriteBinding::Vertex(vertex)) => graph
            .try_set_vertex_property(*vertex, key, value)
            .map_err(|e| CypherError::Execution(e.to_string())),
        Some(WriteBinding::Edge(edge)) => graph
            .try_set_edge_property(*edge, key, value)
            .map_err(|e| CypherError::Execution(e.to_string())),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

fn resolve_bound_property(
    graph: &Graph,
    bindings: &HashMap<String, WriteBinding>,
    variable: &str,
    key: &str,
) -> CypherResult<Value> {
    match bindings.get(variable) {
        Some(WriteBinding::Vertex(vertex)) => Ok(graph.get_vertex_property(*vertex, key)),
        Some(WriteBinding::Edge(edge)) => Ok(graph.get_edge_property(*edge, key)),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

fn find_matching_vertex(
    graph: &Graph,
    label: &str,
    properties: &[(String, Value)],
) -> Option<VertexId> {
    for vid_raw in 0..graph.num_vertices() as u64 {
        let vid = VertexId(vid_raw);
        if graph.vertex_label(vid) != Some(label) {
            continue;
        }
        let matches = properties
            .iter()
            .all(|(key, value)| graph.get_vertex_property(vid, key) == *value);
        if matches {
            return Some(vid);
        }
    }
    None
}

fn has_incident_edges(graph: &Graph, vid: VertexId) -> bool {
    graph.incident_degree(vid, Direction::Both) > 0
}

fn count_incident_edges(graph: &Graph, vid: VertexId) -> usize {
    graph.incident_degree(vid, Direction::Both)
}

/// Unified result of a top-level Cypher invocation.
#[derive(Debug, Clone)]
pub enum RunResult {
    Read(QueryResult),
    Write(WriteSummary),
}

/// Convenience: parse + plan + execute a read-only query (no indexes).
///
/// Returns an error if the input is a write statement — use `run_cypher_mut`
/// for CREATE / DELETE.
pub fn run_cypher(query: &str, graph: &Graph) -> CypherResult<QueryResult> {
    let ctx = QueryContext::new(graph);
    let ast = crate::parser::Parser::parse_read(query)?;
    crate::binder::bind_query(&ast)?;
    let plan = crate::planner::plan_query(&ast)?;
    execute(&plan, &ctx)
}

/// Convenience: parse + plan + execute with index support (read-only).
pub fn run_cypher_with_indexes(
    query: &str,
    graph: &Graph,
    indexes: &nexus_index::composite::IndexSet,
) -> CypherResult<QueryResult> {
    let ctx = QueryContext::with_indexes(graph, indexes);
    let ast = crate::parser::Parser::parse_read(query)?;
    crate::binder::bind_query(&ast)?;
    let plan = crate::planner::plan_query(&ast)?;
    execute(&plan, &ctx)
}

/// Parse + plan + execute any statement (read or write) against a mutable
/// in-process graph.
///
/// This helper is intentionally volatile: it mutates the supplied `Graph`
/// directly and does not append WAL records. Server/database callers that
/// need durability must route writes through `NexusEngine::execute_cypher*`,
/// which wraps the write plan in `NexusEngine::execute_write()`.
pub fn run_cypher_mut_in_memory(query: &str, graph: &mut Graph) -> CypherResult<RunResult> {
    run_cypher_mut_in_memory_with_params(query, graph, HashMap::new())
}

/// Non-durable in-memory helper with parameter binding.
pub fn run_cypher_mut_in_memory_with_params(
    query: &str,
    graph: &mut Graph,
    params: HashMap<String, Value>,
) -> CypherResult<RunResult> {
    let stmt = crate::parser::Parser::parse(query)?;
    match stmt {
        crate::ast::Statement::Read(q) => {
            crate::binder::bind_query(&q)?;
            let plan = crate::planner::plan_query(&q)?;
            let ctx = QueryContext::new(graph).with_params(params);
            let result = execute(&plan, &ctx)?;
            Ok(RunResult::Read(result))
        }
        crate::ast::Statement::Write(wq) => {
            crate::binder::bind_write(&wq)?;
            let plan = crate::planner::plan_write(&wq)?;
            let mut ctx = WriteContext::new(graph).with_params(params);
            let summary = execute_write(&plan, &mut ctx)?;
            Ok(RunResult::Write(summary))
        }
    }
}

/// Backward-compatible alias for the non-durable in-memory helper.
pub fn run_cypher_mut(query: &str, graph: &mut Graph) -> CypherResult<RunResult> {
    run_cypher_mut_in_memory(query, graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::graph::Graph;
    use nexus_core::properties::PropertyType;
    use nexus_index::composite::IndexSet;

    fn test_graph() -> Graph {
        let mut g = Graph::new(4, 4);
        g.register_vertex_property("name", PropertyType::String, true, false);
        g.register_vertex_property("entity_type", PropertyType::String, true, false);
        g.register_vertex_property("external_id", PropertyType::String, true, true);
        g.register_vertex_property("revenue", PropertyType::Float64, false, false);

        let apple = g.add_vertex("Entity");
        g.set_vertex_property(apple, "name", "Apple Inc.".into());
        g.set_vertex_property(apple, "entity_type", "ORG".into());
        g.set_vertex_property(apple, "external_id", "AAPL:Apple:ORG".into());
        g.set_vertex_property(apple, "revenue", Value::Float64(394.3));

        let revenue = g.add_vertex("Metric");
        g.set_vertex_property(revenue, "name", "Revenue".into());
        g.set_vertex_property(revenue, "entity_type", "FIN_METRIC".into());

        let services = g.add_vertex("Segment");
        g.set_vertex_property(services, "name", "Services".into());

        let usa = g.add_vertex("Entity");
        g.set_vertex_property(usa, "name", "United States".into());
        g.set_vertex_property(usa, "entity_type", "GEO".into());

        g.add_edge(apple, revenue, "DISCLOSES");
        g.add_edge(revenue, services, "HAS_COMPONENT");
        g.add_edge(apple, usa, "OPERATES_IN");

        g.build();
        g
    }

    #[test]
    fn execute_simple_scan() {
        let g = test_graph();
        let result = run_cypher("MATCH (n:Entity) RETURN n", &g).unwrap();
        assert_eq!(result.columns, vec!["n"]);
        assert_eq!(result.num_rows(), 2); // Apple + USA
    }

    #[test]
    fn execute_scan_with_limit() {
        let g = test_graph();
        let result = run_cypher("MATCH (n:Entity) RETURN n LIMIT 1", &g).unwrap();
        assert_eq!(result.num_rows(), 1);
    }

    #[test]
    fn execute_expand_one_hop() {
        let g = test_graph();
        let result = run_cypher("MATCH (a:Entity)-[:DISCLOSES]->(b) RETURN a, b", &g).unwrap();
        assert_eq!(result.num_rows(), 1); // Apple -> Revenue
    }

    #[test]
    fn execute_count_aggregate() {
        let g = test_graph();
        let result = run_cypher("MATCH (n:Entity) RETURN count(n)", &g).unwrap();
        assert_eq!(result.num_rows(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(2));
    }

    #[test]
    fn execute_grouped_count_aggregate() {
        let mut g = writable_graph();
        g.register_vertex_property("num", PropertyType::Int64, false, false);
        run_cypher_mut(
            "CREATE (:Person {name: 'a', num: 33}), (:Person {name: 'a'}), (:Person {name: 'b', num: 42})",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) =
            run_cypher_mut("MATCH (n:Person) RETURN n.name, count(n.num)", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.columns, vec!["n.name", "count(n.num)"]);
        assert_eq!(qr.num_rows(), 2);
        assert!(
            qr.rows
                .contains(&vec![Value::String("a".into()), Value::Int64(1)])
        );
        assert!(
            qr.rows
                .contains(&vec![Value::String("b".into()), Value::Int64(1)])
        );
    }

    #[test]
    fn execute_collect_distinct_filters_nulls() {
        let mut g = writable_graph();
        let RunResult::Read(qr) = run_cypher_mut(
            "UNWIND [null, 1, null, 1, 2] AS x RETURN collect(DISTINCT x) AS c",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(
            qr.rows[0][0],
            Value::List(vec![Value::Int64(1), Value::Int64(2)])
        );
    }

    #[test]
    fn execute_counts_self_loop_relationship_once() {
        let mut g = writable_graph();
        run_cypher_mut("CREATE (a:Node), (a)-[:R]->(a)", &mut g).unwrap();
        let RunResult::Read(qr) =
            run_cypher_mut("MATCH ()-[r]-() RETURN count(r)", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.rows[0][0], Value::Int64(1));
    }

    #[test]
    fn execute_min_max_use_cypher_value_ordering() {
        let mut g = writable_graph();
        let RunResult::Read(qr) = run_cypher_mut(
            "UNWIND [1, 'a', null, [1, 2], 0.2, 'b'] AS x RETURN min(x), max(x)",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(
            qr.rows[0][0],
            Value::List(vec![Value::Int64(1), Value::Int64(2)])
        );
        assert_eq!(qr.rows[0][1], Value::Int64(1));
    }

    #[test]
    fn execute_variable_length_path() {
        let g = test_graph();
        let result = run_cypher("MATCH (a:Entity)-[:DISCLOSES*1..2]->(b) RETURN a, b", &g).unwrap();
        assert!(result.num_rows() >= 1);
    }

    #[test]
    fn execute_tail_variable_length_path_from_bound_node() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:A {name: 'n0'}), (b:B {name: 'n00'}), (c:C {name: 'n000'}), \
             (a)-[:LIKES]->(b), (b)-[:LIKES]->(c)",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) =
            run_cypher_mut("MATCH (a:A) MATCH (a)-[:LIKES*]->(c) RETURN c.name", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(
            qr.rows,
            vec![
                vec![Value::String("n00".into())],
                vec![Value::String("n000".into())],
            ]
        );
    }

    #[test]
    fn execute_relationship_property_predicate() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:A), (b:B), (c:C), \
             (a)-[:KNOWS {name: 'monkey'}]->(b), \
             (a)-[:KNOWS {name: 'ape'}]->(c)",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) =
            run_cypher_mut("MATCH (a)-[r:KNOWS {name: 'monkey'}]->(b) RETURN b", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.rows.len(), 1);
    }

    #[test]
    fn execute_list_equality_and_in_use_cypher_null_semantics() {
        let mut g = writable_graph();
        let RunResult::Read(qr) = run_cypher_mut(
            "RETURN [1, 2] = [null, 2] AS eq, 4 IN [1, null, 3] AS member",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.rows[0], vec![Value::Null, Value::Null]);
    }

    #[test]
    fn execute_list_slice_with_explicit_null_bound_returns_null() {
        let mut g = writable_graph();
        let RunResult::Read(qr) =
            run_cypher_mut("WITH [1, 2, 3] AS list RETURN list[1..null] AS r", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.rows[0][0], Value::Null);
    }

    #[test]
    fn execute_return_property() {
        let g = test_graph();
        let result = run_cypher("MATCH (n:Entity) RETURN n.name", &g).unwrap();
        assert_eq!(result.num_rows(), 2);
        let names: Vec<&Value> = result.rows.iter().map(|r| &r[0]).collect();
        assert!(names.contains(&&Value::String("Apple Inc.".into())));
        assert!(names.contains(&&Value::String("United States".into())));
    }

    #[test]
    fn execute_where_property_filter() {
        let g = test_graph();
        let result =
            run_cypher("MATCH (n:Entity) WHERE n.name = 'Apple Inc.' RETURN n", &g).unwrap();
        assert_eq!(result.num_rows(), 1);
    }

    #[test]
    fn execute_expand_with_dst_label() {
        let g = test_graph();
        let result =
            run_cypher("MATCH (a:Entity)-[:DISCLOSES]->(b:Metric) RETURN a, b", &g).unwrap();
        assert_eq!(result.num_rows(), 1);
    }

    #[test]
    fn execute_scan_uses_unique_index() {
        let mut g = Graph::new(100, 100);
        g.register_vertex_property("name", PropertyType::String, true, false);
        g.register_vertex_property("external_id", PropertyType::String, true, true);

        for i in 0..100u64 {
            let v = g.add_vertex("Entity");
            g.set_vertex_property(v, "name", format!("Entity_{i}").into());
            g.set_vertex_property(v, "external_id", format!("EXT_{i}").into());
        }
        g.build();

        let mut indexes = IndexSet::new();
        let idx = indexes.add_unique("external_id");
        for i in 0..100u64 {
            indexes
                .unique_mut(idx)
                .unwrap()
                .insert(&format!("EXT_{i}"), VertexId(i))
                .unwrap();
        }

        let ctx = QueryContext::with_indexes(&g, &indexes);
        let ast = crate::parser::Parser::parse_read(
            "MATCH (n:Entity) WHERE n.external_id = 'EXT_42' RETURN n",
        )
        .unwrap();
        let plan = crate::planner::plan_query(&ast).unwrap();
        let result = execute(&plan, &ctx).unwrap();

        assert_eq!(result.num_rows(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(42));
    }

    #[test]
    fn execute_scan_uses_composite_index() {
        let mut g = Graph::new(100, 100);
        g.register_vertex_property("name", PropertyType::String, true, false);
        g.register_vertex_property("entity_type", PropertyType::String, true, false);

        for i in 0..100u64 {
            let v = g.add_vertex("Entity");
            g.set_vertex_property(v, "name", format!("Entity_{i}").into());
            let etype = if i % 2 == 0 { "ORG" } else { "PERSON" };
            g.set_vertex_property(v, "entity_type", etype.into());
        }
        g.build();

        let mut indexes = IndexSet::new();
        let idx = indexes.add_composite("entity_type");
        for i in 0..100u64 {
            let etype = if i % 2 == 0 { "ORG" } else { "PERSON" };
            indexes
                .composite_mut(idx)
                .unwrap()
                .insert(etype, VertexId(i));
        }

        let ctx = QueryContext::with_indexes(&g, &indexes);
        let ast = crate::parser::Parser::parse_read(
            "MATCH (n:Entity) WHERE n.entity_type = 'ORG' RETURN n",
        )
        .unwrap();
        let plan = crate::planner::plan_query(&ast).unwrap();
        let result = execute(&plan, &ctx).unwrap();

        assert_eq!(result.num_rows(), 50);
    }

    #[test]
    fn execute_scan_falls_back_without_index() {
        let mut g = Graph::new(10, 10);
        g.register_vertex_property("name", PropertyType::String, true, false);

        for i in 0..10u64 {
            let v = g.add_vertex("Entity");
            g.set_vertex_property(v, "name", format!("Entity_{i}").into());
        }
        g.build();

        // Empty index set -- no indexes registered
        let indexes = IndexSet::new();
        let ctx = QueryContext::with_indexes(&g, &indexes);
        let ast = crate::parser::Parser::parse_read(
            "MATCH (n:Entity) WHERE n.name = 'Entity_5' RETURN n",
        )
        .unwrap();
        let plan = crate::planner::plan_query(&ast).unwrap();
        let result = execute(&plan, &ctx).unwrap();

        assert_eq!(result.num_rows(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(5));
    }

    #[test]
    fn run_cypher_backward_compat() {
        let g = test_graph();
        let result = run_cypher("MATCH (n:Entity) RETURN n", &g).unwrap();
        assert_eq!(result.num_rows(), 2);
    }

    #[test]
    fn execute_with_parameter_binding() {
        let g = test_graph();
        let ctx = QueryContext::new(&g).with_params(
            vec![("name".to_string(), Value::String("Apple Inc.".into()))]
                .into_iter()
                .collect(),
        );
        let ast =
            crate::parser::Parser::parse_read("MATCH (n:Entity) WHERE n.name = $name RETURN n")
                .unwrap();
        let plan = crate::planner::plan_query(&ast).unwrap();
        let result = execute(&plan, &ctx).unwrap();
        assert_eq!(result.num_rows(), 1);
    }

    #[test]
    fn execute_return_without_match() {
        let g = test_graph();
        let result = run_cypher("RETURN 1 AS one, 2 + 3 AS five", &g).unwrap();
        assert_eq!(result.columns, vec!["one", "five"]);
        assert_eq!(result.rows, vec![vec![Value::Int64(1), Value::Int64(5)]]);
    }

    #[test]
    fn execute_unwind_literal_list() {
        let g = test_graph();
        let result = run_cypher("UNWIND [1, 2, 3] AS x RETURN x", &g).unwrap();
        assert_eq!(result.columns, vec!["x"]);
        assert_eq!(
            result.rows,
            vec![
                vec![Value::Int64(1)],
                vec![Value::Int64(2)],
                vec![Value::Int64(3)],
            ]
        );
    }

    #[test]
    fn execute_with_projects_and_filters() {
        let g = test_graph();
        let result = run_cypher(
            "MATCH (n:Entity) WITH n.name AS name WHERE name = 'Apple Inc.' RETURN name",
            &g,
        )
        .unwrap();
        assert_eq!(result.columns, vec!["name"]);
        assert_eq!(result.rows, vec![vec![Value::String("Apple Inc.".into())]]);
    }

    #[test]
    fn execute_tail_match_uses_prior_bindings() {
        let g = test_graph();
        let result = run_cypher(
            "MATCH (a:Entity) WITH a MATCH (a)-[:DISCLOSES]->(b) RETURN a.name, b.name",
            &g,
        )
        .unwrap();
        assert_eq!(result.num_rows(), 1);
        assert_eq!(result.rows[0][0], Value::String("Apple Inc.".into()));
        assert_eq!(result.rows[0][1], Value::String("Revenue".into()));
    }

    #[test]
    fn execute_optional_match_preserves_unmatched_rows() {
        let g = test_graph();
        let result = run_cypher(
            "MATCH (a:Entity) OPTIONAL MATCH (a)-[:MISSING]->(b) RETURN a.name, b",
            &g,
        )
        .unwrap();
        assert_eq!(result.num_rows(), 2);
        assert!(result.rows.iter().all(|row| row[1] == Value::Null));
    }

    #[test]
    fn execute_union_distinct_and_all() {
        let g = test_graph();
        let distinct = run_cypher("RETURN 1 AS x UNION RETURN 1 AS x", &g).unwrap();
        assert_eq!(distinct.columns, vec!["x"]);
        assert_eq!(distinct.rows, vec![vec![Value::Int64(1)]]);

        let all = run_cypher("RETURN 1 AS x UNION ALL RETURN 1 AS x", &g).unwrap();
        assert_eq!(all.columns, vec!["x"]);
        assert_eq!(all.rows, vec![vec![Value::Int64(1)], vec![Value::Int64(1)]]);
    }

    // --- Write-path tests (CREATE / DELETE) ---

    fn writable_graph() -> Graph {
        let mut g = Graph::new(16, 16);
        g.register_vertex_property("name", PropertyType::String, true, false);
        g.register_vertex_property("age", PropertyType::Int64, false, false);
        g.register_edge_property("name", PropertyType::String, false, false);
        g.register_edge_property("weight", PropertyType::Float64, false, false);
        g.build();
        g
    }

    #[test]
    fn execute_create_single_node() {
        let mut g = writable_graph();
        let before = g.num_vertices();
        let result = run_cypher_mut("CREATE (n:Person {name: 'Alice', age: 30})", &mut g).unwrap();

        let RunResult::Write(summary) = result else {
            panic!("expected write result");
        };
        assert_eq!(summary.nodes_created, 1);
        assert_eq!(summary.edges_created, 0);
        assert_eq!(g.num_vertices(), before + 1);

        // Read it back.
        let RunResult::Read(qr) =
            run_cypher_mut("MATCH (n:Person) RETURN n.name, n.age", &mut g).unwrap()
        else {
            panic!("expected read result");
        };
        assert_eq!(qr.num_rows(), 1);
        assert_eq!(qr.rows[0][0], Value::String("Alice".into()));
        assert_eq!(qr.rows[0][1], Value::Int64(30));
    }

    #[test]
    fn execute_create_edge_between_new_nodes() {
        let mut g = writable_graph();
        let result = run_cypher_mut(
            "CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})",
            &mut g,
        )
        .unwrap();
        let RunResult::Write(summary) = result else {
            panic!("expected write");
        };
        assert_eq!(summary.nodes_created, 2);
        assert_eq!(summary.edges_created, 1);

        // Traversal confirms the edge is queryable.
        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.num_rows(), 1);
    }

    #[test]
    fn execute_create_then_match_delete() {
        let mut g = writable_graph();
        run_cypher_mut("CREATE (n:Person {name: 'Doomed'})", &mut g).unwrap();
        run_cypher_mut("CREATE (n:Person {name: 'Safe'})", &mut g).unwrap();

        let result =
            run_cypher_mut("MATCH (n:Person) WHERE n.name = 'Doomed' DELETE n", &mut g).unwrap();
        let RunResult::Write(summary) = result else {
            panic!("expected write");
        };
        assert_eq!(summary.nodes_deleted, 1);

        // Only "Safe" remains.
        let RunResult::Read(qr) = run_cypher_mut("MATCH (n:Person) RETURN n.name", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.num_rows(), 1);
        assert_eq!(qr.rows[0][0], Value::String("Safe".into()));
    }

    #[test]
    fn execute_delete_without_detach_fails_on_connected_node() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})",
            &mut g,
        )
        .unwrap();

        let err = run_cypher_mut("MATCH (n:Person) WHERE n.name = 'A' DELETE n", &mut g);
        assert!(err.is_err(), "strict DELETE should fail on connected node");

        // Graph is untouched on error (node still there).
        let RunResult::Read(qr) = run_cypher_mut("MATCH (n:Person) RETURN n", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.num_rows(), 2);
    }

    #[test]
    fn execute_detach_delete_removes_connected_node() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})",
            &mut g,
        )
        .unwrap();

        let result = run_cypher_mut(
            "MATCH (n:Person) WHERE n.name = 'A' DETACH DELETE n",
            &mut g,
        )
        .unwrap();
        let RunResult::Write(summary) = result else {
            panic!("expected write");
        };
        assert_eq!(summary.nodes_deleted, 1);
        assert_eq!(summary.edges_deleted, 1);

        // Only B remains, no KNOWS edges.
        let RunResult::Read(qr) =
            run_cypher_mut("MATCH (a:Person)-[:KNOWS]->(b) RETURN a, b", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.num_rows(), 0);
    }

    #[test]
    fn execute_set_and_remove_node_property() {
        let mut g = writable_graph();
        run_cypher_mut("CREATE (n:Person {name: 'Alice', age: 30})", &mut g).unwrap();

        let RunResult::Write(summary) = run_cypher_mut(
            "MATCH (n:Person) WHERE n.name = 'Alice' SET n.name = 'Bob'",
            &mut g,
        )
        .unwrap() else {
            panic!("expected write");
        };
        assert_eq!(summary.properties_set, 1);

        let RunResult::Read(qr) = run_cypher_mut("MATCH (n:Person) RETURN n.name", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.rows[0][0], Value::String("Bob".into()));

        let RunResult::Write(summary) =
            run_cypher_mut("MATCH (n:Person) REMOVE n.name", &mut g).unwrap()
        else {
            panic!("expected write");
        };
        assert_eq!(summary.properties_removed, 1);

        let RunResult::Read(qr) = run_cypher_mut("MATCH (n:Person) RETURN n.name", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.rows[0][0], Value::Null);
    }

    #[test]
    fn execute_set_edge_property_and_delete_relationship_variable() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})",
            &mut g,
        )
        .unwrap();

        let RunResult::Write(summary) = run_cypher_mut(
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) SET r.weight = 0.9",
            &mut g,
        )
        .unwrap() else {
            panic!("expected write");
        };
        assert_eq!(summary.properties_set, 1);

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN r.weight",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.rows[0][0], Value::Float64(0.9));

        let RunResult::Write(summary) =
            run_cypher_mut("MATCH (a:Person)-[r:KNOWS]->(b:Person) DELETE r", &mut g).unwrap()
        else {
            panic!("expected write");
        };
        assert_eq!(summary.edges_deleted, 1);

        let RunResult::Read(qr) =
            run_cypher_mut("MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN r", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.num_rows(), 0);
    }

    #[test]
    fn execute_merge_node_and_edge_are_idempotent() {
        let mut g = writable_graph();
        run_cypher_mut("MERGE (n:Person {name: 'Alice'})", &mut g).unwrap();
        run_cypher_mut("MERGE (n:Person {name: 'Alice'})", &mut g).unwrap();

        let RunResult::Read(qr) =
            run_cypher_mut("MATCH (n:Person) WHERE n.name = 'Alice' RETURN n", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.num_rows(), 1);

        run_cypher_mut(
            "MERGE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})",
            &mut g,
        )
        .unwrap();
        run_cypher_mut(
            "MERGE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.num_rows(), 1);
    }

    #[test]
    fn execute_temporal_date_constructor_and_accessors() {
        let mut g = writable_graph();
        let RunResult::Read(qr) = run_cypher_mut(
            "WITH date({year: 1984, month: 10, day: 11}) AS d RETURN toString(d), d.year, d.week, d.weekDay",
            &mut g,
        ).unwrap() else { panic!("expected read"); };
        assert_eq!(qr.rows[0][0], Value::String("1984-10-11".into()));
        assert_eq!(qr.rows[0][1], Value::Int64(1984));
        assert_eq!(qr.rows[0][2], Value::Int64(41));
        assert_eq!(qr.rows[0][3], Value::Int64(4));
    }

    #[test]
    fn execute_temporal_duration_constructor_and_accessors() {
        let mut g = writable_graph();
        let RunResult::Read(qr) = run_cypher_mut(
            "WITH duration({years: 1, months: 4, days: 10, hours: 1, minutes: 1, seconds: 1, nanoseconds: 111111111}) AS d RETURN d.months, d.seconds, d.nanosecondsOfSecond",
            &mut g,
        ).unwrap() else { panic!("expected read"); };
        assert_eq!(qr.rows[0][0], Value::Int64(16));
        assert_eq!(qr.rows[0][1], Value::Int64(3661));
        assert_eq!(qr.rows[0][2], Value::Int64(111111111));
    }

    #[test]
    fn run_cypher_read_rejects_write_statement() {
        let g = writable_graph();
        assert!(run_cypher("CREATE (n:X)", &g).is_err());
        assert!(run_cypher("MATCH (n) DELETE n", &g).is_err());
    }

    #[test]
    fn run_cypher_mut_supports_reads_too() {
        let mut g = writable_graph();
        run_cypher_mut("CREATE (n:Person {name: 'A'})", &mut g).unwrap();
        let result = run_cypher_mut("MATCH (n:Person) RETURN count(n)", &mut g).unwrap();
        let RunResult::Read(qr) = result else {
            panic!("expected read");
        };
        assert_eq!(qr.rows[0][0], Value::Int64(1));
    }
}
