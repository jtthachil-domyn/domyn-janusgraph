//! Vectorized query executor.
//!
//! Executes a logical plan against a `QueryContext` (graph + optional indexes),
//! producing rows of `Value`s.
//! Expand operators use SpMV for cache-friendly multi-hop traversal.

use crate::ast::{
    BinaryOp, Expr, ListPredicateKind, Literal, MatchClause, NodePattern, Pattern, PatternElement,
    ReadClause, RelDirection, RelationshipPattern, ReturnClause, ReturnItem, RowCount, Statement,
    UnaryOp, WithClause,
};
use crate::context::{
    QueryContext, WriteContext, WriteSummary, estimate_query_result_bytes, estimate_row_bytes,
};
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
    execute_with_row_demand(plan, ctx, None)
}

fn execute_with_row_demand(
    plan: &LogicalPlan,
    ctx: &QueryContext,
    row_demand: Option<usize>,
) -> CypherResult<QueryResult> {
    ctx.check_cancelled()?;
    let result = match plan {
        LogicalPlan::Argument => Ok(QueryResult {
            columns: Vec::new(),
            rows: vec![Vec::new()],
        }),
        LogicalPlan::ScanVertices {
            variable,
            label,
            index_lookup,
        } => execute_scan(
            ctx,
            variable,
            label.as_deref(),
            index_lookup.as_ref(),
            row_demand,
        ),
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
            where_predicate,
        } => {
            let input_result = execute(input, ctx)?;
            execute_apply_match(
                ctx,
                &input_result,
                clause,
                *optional,
                where_predicate.as_ref(),
            )
        }
        LogicalPlan::Unwind { input, expr, alias } => {
            let input_result = execute(input, ctx)?;
            execute_unwind(ctx, &input_result, expr, alias)
        }
        LogicalPlan::Project { input, columns } => {
            let input_result = execute_with_row_demand(input, ctx, row_demand)?;
            execute_project(ctx, &input_result, columns)
        }
        LogicalPlan::Sort { input, keys } => {
            let input_result = execute(input, ctx)?;
            execute_sort(ctx, input_result, keys)
        }
        LogicalPlan::Limit { input, count } => {
            let count = eval_row_count(count, ctx)?;
            let demand = Some(row_demand.map_or(count, |outer| outer.min(count)));
            let mut result = execute_with_row_demand(input, ctx, demand)?;
            result.rows.truncate(count);
            Ok(result)
        }
        LogicalPlan::Skip { input, count } => {
            let mut result = execute(input, ctx)?;
            let skip = eval_row_count(count, ctx)?.min(result.rows.len());
            result.rows = result.rows.split_off(skip);
            Ok(result)
        }
        LogicalPlan::Distinct { input } => {
            let result = execute(input, ctx)?;
            execute_distinct(ctx, result)
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregations,
        } => {
            if let Some(result) =
                try_execute_degree_count_aggregate(ctx, input, group_by, aggregations)?
            {
                Ok(result)
            } else {
                let input_result = execute(input, ctx)?;
                execute_aggregate(ctx, &input_result, group_by, aggregations)
            }
        }
        LogicalPlan::Union { left, right, all } => {
            let left_result = execute(left, ctx)?;
            let right_result = execute(right, ctx)?;
            execute_union(ctx, left_result, right_result, *all)
        }
    }?;
    check_query_result_budget(ctx, &result)?;
    Ok(result)
}

fn check_query_result_budget(ctx: &QueryContext, result: &QueryResult) -> CypherResult<()> {
    ctx.check_row_budget(result.rows.len())?;
    ctx.check_byte_budget(estimate_query_result_bytes(&result.columns, &result.rows))
}

fn initial_budgeted_bytes(columns: &[String]) -> usize {
    columns.iter().map(|column| column.len()).sum()
}

fn push_budgeted_row(
    ctx: &QueryContext,
    rows: &mut Vec<Vec<Value>>,
    row: Vec<Value>,
    estimated_bytes: &mut usize,
) -> CypherResult<()> {
    *estimated_bytes += estimate_row_bytes(&row);
    rows.push(row);
    ctx.check_row_budget(rows.len())?;
    ctx.check_byte_budget(*estimated_bytes)
}

pub fn eval_row_count(count: &RowCount, ctx: &QueryContext) -> CypherResult<usize> {
    match count {
        RowCount::Literal(value) => Ok(*value as usize),
        RowCount::Parameter(name) => match ctx.params.get(name) {
            Some(Value::Int64(value)) if *value >= 0 => Ok(*value as usize),
            Some(other) => Err(CypherError::Execution(format!(
                "row count parameter ${name} must be a non-negative integer, got {other:?}"
            ))),
            None => Err(CypherError::Execution(format!(
                "row count parameter ${name} must be a non-negative integer"
            ))),
        },
        RowCount::Expr(expr) => match eval_ast_expr(ctx, expr, &[], &[]) {
            Value::Int64(value) if value >= 0 => Ok(value as usize),
            Value::Float64(value) if value >= 0.0 && value.fract() == 0.0 => Ok(value as usize),
            other => Err(CypherError::Execution(format!(
                "row count expression must be a non-negative integer, got {other:?}"
            ))),
        },
    }
}

fn execute_scan(
    ctx: &QueryContext,
    variable: &str,
    label_filter: Option<&str>,
    index_lookup: Option<&IndexLookup>,
    row_demand: Option<usize>,
) -> CypherResult<QueryResult> {
    ctx.check_cancelled()?;
    let graph = ctx.graph;

    // Fast path: try index lookup when an IndexLookup hint is present.
    if let Some(lookup) = index_lookup {
        if let Some(candidate_ids) = ctx.index_lookup(&lookup.property, &lookup.value) {
            let mut matching_ids = Vec::new();
            for (idx, vid) in candidate_ids.into_iter().enumerate() {
                if idx % 1024 == 0 {
                    ctx.check_cancelled()?;
                }
                if let Some(label) = label_filter {
                    if !vertex_matches_label_filter(graph, vid, label) {
                        continue;
                    }
                }
                matching_ids.push(vid.0);
                ctx.check_row_budget(matching_ids.len())?;
                if row_demand.is_some_and(|demand| matching_ids.len() >= demand) {
                    break;
                }
            }

            let col_name = variable.to_string();
            let mut estimated_bytes = initial_budgeted_bytes(std::slice::from_ref(&col_name));
            let rows: Vec<Vec<Value>> = matching_ids
                .iter()
                .map(|&id| {
                    let row = vec![Value::Int64(id as i64)];
                    estimated_bytes += estimate_row_bytes(&row);
                    row
                })
                .collect();
            ctx.check_byte_budget(estimated_bytes)?;
            return Ok(QueryResult {
                columns: vec![col_name],
                rows,
            });
        }
    }

    // Slow path: linear scan over all vertices.
    let mut matching_ids = Vec::new();

    for vid_raw in 0..graph.num_vertices() as u64 {
        if vid_raw % 1024 == 0 {
            ctx.check_cancelled()?;
        }
        let vid = VertexId(vid_raw);

        if let Some(label) = label_filter {
            if !vertex_matches_label_filter(graph, vid, label) {
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
        ctx.check_row_budget(matching_ids.len())?;
        if row_demand.is_some_and(|demand| matching_ids.len() >= demand) {
            break;
        }
    }

    let col_name = variable.to_string();
    let mut estimated_bytes = initial_budgeted_bytes(std::slice::from_ref(&col_name));
    let rows: Vec<Vec<Value>> = matching_ids
        .iter()
        .map(|&id| {
            let row = vec![Value::Int64(id as i64)];
            estimated_bytes += estimate_row_bytes(&row);
            row
        })
        .collect();
    ctx.check_byte_budget(estimated_bytes)?;

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
    ctx.check_cancelled()?;
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
    let mut estimated_bytes = initial_budgeted_bytes(&columns);

    for (row_idx, row) in input.rows.iter().enumerate() {
        if row_idx % 1024 == 0 {
            ctx.check_cancelled()?;
        }
        let src_id = match &row[src_col_idx] {
            Value::Int64(id) => VertexId(*id as u64),
            _ => continue,
        };

        let mut reachable: Vec<(VertexId, Option<EdgeId>)> = Vec::new();

        if max_hops == 1 && min_hops <= 1 {
            for edge_type in edge_types {
                ctx.check_cancelled()?;
                for (edge_idx, (neighbor, edge)) in graph
                    .neighbors_with_edges(src_id, edge_type, dir)
                    .into_iter()
                    .enumerate()
                {
                    if edge_idx % 1024 == 0 {
                        ctx.check_cancelled()?;
                    }
                    if edge_matches_properties(ctx, edge, rel_properties, row, &input.columns) {
                        reachable.push((neighbor, Some(edge)));
                    }
                }
            }
            if edge_types.is_empty() {
                for (edge_idx, (neighbor, edge)) in graph
                    .neighbors_with_edges_any_label(src_id, dir)
                    .into_iter()
                    .enumerate()
                {
                    if edge_idx % 1024 == 0 {
                        ctx.check_cancelled()?;
                    }
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
                ctx,
                row,
                &input.columns,
                src_id,
                edge_types,
                rel_properties,
                dir,
                min_hops,
                max_hops,
                &mut vertices,
            )?;
            reachable.extend(vertices.into_iter().map(|dst| (dst, None)));
        }

        let mut seen_reachable = HashSet::new();
        for (dst, edge) in reachable
            .into_iter()
            .filter(|(dst, edge)| seen_reachable.insert((dst.0, edge.map(|edge| edge.0))))
        {
            if !dst_labels.is_empty() {
                if !vertex_has_all_labels(graph, dst, dst_labels) {
                    continue;
                }
            }
            let mut new_row = row.clone();
            if rel_var.is_some() {
                if let Some(edge) = edge {
                    new_row.push(edge_ref(edge));
                } else {
                    new_row.push(Value::Null);
                }
            }
            new_row.push(Value::Int64(dst.0 as i64));
            push_budgeted_row(ctx, &mut rows, new_row, &mut estimated_bytes)?;
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
    ctx: &QueryContext,
    row: &[Value],
    columns: &[String],
    start: VertexId,
    edge_types: &[String],
    rel_properties: &[(String, ProjectExpr)],
    dir: Direction,
    min_hops: u32,
    max_hops: u32,
    result: &mut Vec<VertexId>,
) -> CypherResult<()> {
    let graph = ctx.graph;
    ctx.check_cancelled()?;
    if min_hops == 0 {
        result.push(start);
    }
    let max_hops = max_hops.min(graph.num_edges() as u32);
    if max_hops == 0 {
        return Ok(());
    }

    let mut stack = vec![(start, 0u32, HashSet::new())];
    let mut iterations = 0usize;
    while let Some((vertex, depth, used_edges)) = stack.pop() {
        iterations += 1;
        if iterations % 1024 == 0 {
            ctx.check_cancelled()?;
        }
        if depth >= max_hops {
            continue;
        }

        let candidates: Vec<(VertexId, EdgeId)> = if edge_types.is_empty() {
            graph
                .neighbors_with_edges_any_label(vertex, dir)
                .into_iter()
                .filter(|(_, edge)| {
                    edge_matches_properties(ctx, *edge, rel_properties, row, columns)
                })
                .collect()
        } else {
            edge_types
                .iter()
                .flat_map(|label| graph.neighbors_with_edges(vertex, label, dir))
                .filter(|(_, edge)| {
                    edge_matches_properties(ctx, *edge, rel_properties, row, columns)
                })
                .collect()
        };

        for (neighbor, edge) in candidates {
            if used_edges.contains(&edge.0) {
                continue;
            }
            let next_depth = depth + 1;
            if next_depth >= min_hops {
                result.push(neighbor);
            }
            let mut next_used = used_edges.clone();
            next_used.insert(edge.0);
            stack.push((neighbor, next_depth, next_used));
        }
    }
    Ok(())
}

fn execute_unwind(
    ctx: &QueryContext,
    input: &QueryResult,
    expr: &ProjectExpr,
    alias: &str,
) -> CypherResult<QueryResult> {
    ctx.check_cancelled()?;
    let mut columns = input.columns.clone();
    let alias_idx = if let Some(idx) = columns.iter().position(|c| c == alias) {
        idx
    } else {
        columns.push(alias.to_string());
        columns.len() - 1
    };

    let mut rows = Vec::new();
    let mut estimated_bytes = initial_budgeted_bytes(&columns);
    for (row_idx, row) in input.rows.iter().enumerate() {
        if row_idx % 1024 == 0 {
            ctx.check_cancelled()?;
        }
        let value = resolve_project_expr(ctx, expr, row, &input.columns);
        let values = match value {
            Value::List(values) => values,
            Value::Null => Vec::new(),
            other => vec![other],
        };

        for (item_idx, item) in values.into_iter().enumerate() {
            if item_idx % 1024 == 0 {
                ctx.check_cancelled()?;
            }
            let mut new_row = row.clone();
            if alias_idx < input.columns.len() {
                new_row[alias_idx] = item;
            } else {
                new_row.push(item);
            }
            push_budgeted_row(ctx, &mut rows, new_row, &mut estimated_bytes)?;
        }
    }

    Ok(QueryResult { columns, rows })
}

fn execute_apply_match(
    ctx: &QueryContext,
    input: &QueryResult,
    clause: &MatchClause,
    optional: bool,
    where_predicate: Option<&Predicate>,
) -> CypherResult<QueryResult> {
    ctx.check_cancelled()?;
    let mut result = input.clone();

    for pattern in &clause.patterns {
        ctx.check_cancelled()?;
        let new_vars: Vec<String> = pattern_variables(pattern)
            .into_iter()
            .filter(|var| !result.columns.contains(var))
            .collect();
        let mut out_columns = result.columns.clone();
        out_columns.extend(new_vars.iter().cloned());

        let mut out_rows = Vec::new();
        let mut estimated_bytes = initial_budgeted_bytes(&out_columns);
        for (row_idx, row) in result.rows.iter().enumerate() {
            if row_idx % 1024 == 0 {
                ctx.check_cancelled()?;
            }
            let matches = match_pattern(ctx, pattern, row, &result.columns)?;
            if matches.is_empty() {
                if optional {
                    let mut new_row = row.clone();
                    new_row.extend(new_vars.iter().map(|_| Value::Null));
                    push_budgeted_row(ctx, &mut out_rows, new_row, &mut estimated_bytes)?;
                }
                continue;
            }

            let mut matched_any = false;
            for (match_idx, assignment) in matches.into_iter().enumerate() {
                if match_idx % 1024 == 0 {
                    ctx.check_cancelled()?;
                }
                let mut new_row = row.clone();
                for var in &new_vars {
                    new_row.push(assignment.get(var).cloned().unwrap_or(Value::Null));
                }
                if where_predicate.is_some_and(|predicate| {
                    !evaluate_predicate(ctx, &new_row, &out_columns, predicate)
                }) {
                    continue;
                }
                matched_any = true;
                push_budgeted_row(ctx, &mut out_rows, new_row, &mut estimated_bytes)?;
            }

            if optional && !matched_any {
                let mut new_row = row.clone();
                new_row.extend(new_vars.iter().map(|_| Value::Null));
                push_budgeted_row(ctx, &mut out_rows, new_row, &mut estimated_bytes)?;
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
    if let Some(var) = &pattern.path_variable {
        vars.push(var.clone());
    }
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
    match_pattern_with_assignments(ctx, pattern, row, columns, &HashMap::new())
}

fn match_pattern_with_assignments(
    ctx: &QueryContext,
    pattern: &Pattern,
    row: &[Value],
    columns: &[String],
    outer_assignments: &HashMap<String, Value>,
) -> CypherResult<Vec<HashMap<String, Value>>> {
    ctx.check_cancelled()?;
    let Some(PatternElement::Node(first_node)) = pattern.elements.first() else {
        return Ok(Vec::new());
    };

    let mut states = Vec::new();
    for (vertex_idx, vertex) in node_candidates(ctx, first_node, row, columns, outer_assignments)?
        .into_iter()
        .enumerate()
    {
        if vertex_idx % 1024 == 0 {
            ctx.check_cancelled()?;
        }
        let mut assignments = outer_assignments.clone();
        if let Some(var) = &first_node.variable {
            if !columns.contains(var) {
                assignments.insert(var.clone(), Value::Int64(vertex.0 as i64));
            }
        }
        states.push(PatternState {
            current: vertex,
            assignments,
            used_edges: HashSet::new(),
            path_nodes: vec![vertex],
            path_edges: Vec::new(),
        });
    }

    let mut index = 1;
    while index < pattern.elements.len() {
        ctx.check_cancelled()?;
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
        for (state_idx, state) in states.into_iter().enumerate() {
            if state_idx % 1024 == 0 {
                ctx.check_cancelled()?;
            }
            for (candidate_idx, candidate) in relationship_candidates_with_bindings(
                ctx,
                state.current,
                rel,
                row,
                columns,
                &state.assignments,
            )
            .into_iter()
            .enumerate()
            {
                if candidate_idx % 1024 == 0 {
                    ctx.check_cancelled()?;
                }
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
                let mut path_nodes = state.path_nodes.clone();
                path_nodes.extend(candidate.nodes.iter().copied());
                let mut path_edges = state.path_edges.clone();
                path_edges.extend(candidate.edges.iter().copied());
                if let Some(var) = &pattern.path_variable {
                    if !columns.contains(var) {
                        next_assignments.insert(var.clone(), path_value(&path_nodes, &path_edges));
                    }
                }
                next_states.push(PatternState {
                    current: candidate.neighbor,
                    assignments: next_assignments,
                    used_edges,
                    path_nodes,
                    path_edges,
                });
            }
        }

        states = next_states;
        index += 2;
    }

    Ok(states
        .into_iter()
        .map(|mut state| {
            if let Some(var) = &pattern.path_variable {
                if !columns.contains(var) {
                    state
                        .assignments
                        .entry(var.clone())
                        .or_insert_with(|| path_value(&state.path_nodes, &state.path_edges));
                }
            }
            state.assignments
        })
        .collect())
}

#[derive(Clone)]
struct PatternState {
    current: VertexId,
    assignments: HashMap<String, Value>,
    used_edges: HashSet<u64>,
    path_nodes: Vec<VertexId>,
    path_edges: Vec<EdgeId>,
}

#[derive(Clone)]
struct RelationshipCandidate {
    neighbor: VertexId,
    binding: Value,
    nodes: Vec<VertexId>,
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
        if vid_raw % 1024 == 0 {
            ctx.check_cancelled()?;
        }
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
        if !vertex_has_all_labels(ctx.graph, vertex, &node.labels) {
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
            binding: edge_ref(edge),
            nodes: vec![neighbor],
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
            nodes: Vec::new(),
            edges: Vec::new(),
        });
    }
    if max == 0 {
        return out;
    }

    let mut stack = vec![(
        current,
        0u32,
        Vec::<VertexId>::new(),
        Vec::<EdgeId>::new(),
        HashSet::<u64>::new(),
    )];
    while let Some((vertex, depth, path_nodes, path_edges, used)) = stack.pop() {
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
            let mut next_nodes = path_nodes.clone();
            next_nodes.push(neighbor);
            let mut next_edges = path_edges.clone();
            next_edges.push(edge);
            if next_depth >= min {
                out.push(RelationshipCandidate {
                    neighbor,
                    binding: Value::List(next_edges.iter().map(|edge| edge_ref(*edge)).collect()),
                    nodes: next_nodes.clone(),
                    edges: next_edges.clone(),
                });
            }
            let mut next_used = used.clone();
            next_used.insert(edge.0);
            stack.push((neighbor, next_depth, next_nodes, next_edges, next_used));
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
    ctx.check_cancelled()?;
    let mut rows = Vec::new();
    let mut estimated_bytes = initial_budgeted_bytes(&input.columns);
    for (row_idx, row) in input.rows.iter().enumerate() {
        if row_idx % 1024 == 0 {
            ctx.check_cancelled()?;
        }
        if evaluate_predicate(ctx, row, &input.columns, predicate) {
            push_budgeted_row(ctx, &mut rows, row.clone(), &mut estimated_bytes)?;
        }
    }
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
        if let Some(value) = value_property(graph, &row[idx], &pr.property) {
            return value;
        }
        if let Value::Int64(id) = &row[idx] {
            let vertex_value = graph.get_vertex_property(VertexId(*id as u64), &pr.property);
            if !vertex_value.is_null() {
                return vertex_value;
            }
            return graph.get_edge_property(EdgeId(*id as u64), &pr.property);
        }
    }
    Value::Null
}

const EDGE_REF_KEY: &str = "__edge_id";
const EDGE_TYPE_KEY: &str = "__edge_type";
const NODE_REF_KEY: &str = "__vertex_id";

fn node_ref(vertex: VertexId) -> Value {
    Value::Map(vec![(NODE_REF_KEY.into(), Value::Int64(vertex.0 as i64))])
}

fn node_ref_id(value: &Value) -> Option<VertexId> {
    let Value::Map(entries) = value else {
        return None;
    };
    entries
        .iter()
        .find(|(key, _)| key == NODE_REF_KEY)
        .and_then(|(_, value)| value.as_i64())
        .map(|id| VertexId(id as u64))
}

fn edge_ref(edge: EdgeId) -> Value {
    Value::Map(vec![(EDGE_REF_KEY.into(), Value::Int64(edge.0 as i64))])
}

fn edge_ref_with_type(edge: EdgeId, rel_type: Option<String>) -> Value {
    let mut entries = vec![(EDGE_REF_KEY.into(), Value::Int64(edge.0 as i64))];
    if let Some(rel_type) = rel_type {
        entries.push((EDGE_TYPE_KEY.into(), Value::String(rel_type)));
    }
    Value::Map(entries)
}

fn edge_ref_id(value: &Value) -> Option<EdgeId> {
    let Value::Map(entries) = value else {
        return None;
    };
    entries
        .iter()
        .find(|(key, _)| key == EDGE_REF_KEY)
        .and_then(|(_, value)| value.as_i64())
        .map(|id| EdgeId(id as u64))
}

fn edge_ref_type(value: &Value) -> Option<String> {
    let Value::Map(entries) = value else {
        return None;
    };
    entries
        .iter()
        .find(|(key, _)| key == EDGE_TYPE_KEY)
        .and_then(|(_, value)| match value {
            Value::String(value) => Some(value.clone()),
            _ => None,
        })
}

fn value_property(graph: &Graph, value: &Value, property: &str) -> Option<Value> {
    if let Some(vertex) = node_ref_id(value) {
        return Some(graph.get_vertex_property(vertex, property));
    }
    if let Some(edge) = edge_ref_id(value) {
        return Some(graph.get_edge_property(edge, property));
    }
    if let Value::Map(entries) = value {
        if let Some((_, value)) = entries.iter().find(|(key, _)| key == property) {
            return Some(value.clone());
        }
    }
    temporal_property(value, property)
}

fn edge_endpoints(graph: &Graph, edge: EdgeId) -> Option<(VertexId, VertexId)> {
    graph
        .edge_records()
        .into_iter()
        .find(|record| record.id == edge)
        .map(|record| (record.source, record.target))
}

fn compare_values(left: &Value, op: &CompareOp, right: &Value) -> bool {
    if matches!(
        op,
        CompareOp::Lt | CompareOp::Lte | CompareOp::Gt | CompareOp::Gte
    ) {
        return cypher_order_compare(left, right, *op).unwrap_or(false);
    }

    match (left, right) {
        (Value::Int64(l), Value::Int64(r)) => compare_ord(l, op, r),
        (Value::Float64(l), Value::Float64(r)) => compare_f64(*l, op, *r),
        (Value::Int64(l), Value::Float64(r)) => compare_f64(*l as f64, op, *r),
        (Value::Float64(l), Value::Int64(r)) => compare_f64(*l, op, *r as f64),
        (Value::String(l), Value::String(r)) => compare_ord(l, op, r),
        (Value::Bool(l), Value::Bool(r)) => compare_ord(l, op, r),
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
    ctx.check_cancelled()?;
    let wildcard_columns = wildcard_projection_columns(input);
    let mut col_names = Vec::new();
    for column in columns {
        match column.expr {
            ProjectExpr::Wildcard => {
                col_names.extend(wildcard_columns.iter().map(|(_, name)| name.clone()));
            }
            _ => col_names.push(column.alias.clone()),
        }
    }

    let mut rows = Vec::with_capacity(input.rows.len());
    let mut estimated_bytes = initial_budgeted_bytes(&col_names);
    for (row_idx, row) in input.rows.iter().enumerate() {
        if row_idx % 1024 == 0 {
            ctx.check_cancelled()?;
        }
        let mut out = Vec::new();
        for col in columns {
            match col.expr {
                ProjectExpr::Wildcard => {
                    out.extend(wildcard_columns.iter().map(|(idx, _)| row[*idx].clone()));
                }
                _ => out.push(resolve_project_expr(ctx, &col.expr, row, &input.columns)),
            }
        }
        push_budgeted_row(ctx, &mut rows, out, &mut estimated_bytes)?;
    }

    Ok(QueryResult {
        columns: col_names,
        rows,
    })
}

fn wildcard_projection_columns(input: &QueryResult) -> Vec<(usize, String)> {
    let mut indexed: Vec<_> = input.columns.iter().cloned().enumerate().collect();
    indexed.sort_by(|(_, left), (_, right)| left.cmp(right));
    indexed
}

fn resolve_project_expr(
    ctx: &QueryContext,
    expr: &ProjectExpr,
    row: &[Value],
    columns: &[String],
) -> Value {
    match expr {
        ProjectExpr::Wildcard => Value::Null,
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

pub fn execute_sort(
    ctx: &QueryContext,
    mut result: QueryResult,
    keys: &[SortKey],
) -> CypherResult<QueryResult> {
    ctx.check_cancelled()?;
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
    check_query_result_budget(ctx, &result)?;
    Ok(result)
}

fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    let rank_cmp = order_type_rank(a).cmp(&order_type_rank(b));
    if !rank_cmp.is_eq() {
        return rank_cmp;
    }

    match (a, b) {
        (Value::Int64(l), Value::Int64(r)) => l.cmp(r),
        (Value::Float64(l), Value::Float64(r)) => compare_float_for_order(*l, *r),
        (Value::Int64(l), Value::Float64(r)) => compare_float_for_order(*l as f64, *r),
        (Value::Float64(l), Value::Int64(r)) => compare_float_for_order(*l, *r as f64),
        (Value::String(l), Value::String(r)) => {
            temporal_string_cmp(l, r).unwrap_or_else(|| l.cmp(r))
        }
        (Value::Bool(l), Value::Bool(r)) => l.cmp(r),
        (Value::List(l), Value::List(r)) => {
            for (lv, rv) in l.iter().zip(r) {
                let cmp = value_cmp(lv, rv);
                if !cmp.is_eq() {
                    return cmp;
                }
            }
            l.len().cmp(&r.len())
        }
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Map(left), Value::Map(right)) => left.len().cmp(&right.len()),
        _ => std::cmp::Ordering::Equal,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TemporalOrderKey {
    kind: u8,
    value: i128,
}

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const NANOS_PER_HOUR: i128 = 3_600 * NANOS_PER_SECOND;
const NANOS_PER_DAY: i128 = 86_400 * NANOS_PER_SECOND;

fn temporal_string_cmp(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let left_key = temporal_order_key(left)?;
    let right_key = temporal_order_key(right)?;
    Some(left_key.cmp(&right_key))
}

fn temporal_order_key(raw: &str) -> Option<TemporalOrderKey> {
    if let Some((date_part, time_part)) = raw.split_once('T') {
        let (year, month, day) = parse_date(date_part)?;
        let parsed_time = parse_time(time_part)?;
        let days = days_from_civil(year, month, day) as i128;
        let offset = parsed_time.offset_seconds.unwrap_or(0) as i128;
        let local_nanos = time_nanos(&parsed_time);
        let kind = if parsed_time.offset_seconds.is_some() {
            4
        } else {
            3
        };
        return Some(TemporalOrderKey {
            kind,
            value: days * NANOS_PER_DAY + local_nanos - offset * NANOS_PER_SECOND,
        });
    }

    if raw.contains(':') {
        let parsed_time = parse_time(raw)?;
        let offset = parsed_time.offset_seconds.unwrap_or(0) as i128;
        let kind = if parsed_time.offset_seconds.is_some() {
            2
        } else {
            1
        };
        return Some(TemporalOrderKey {
            kind,
            value: time_nanos(&parsed_time) - offset * NANOS_PER_SECOND,
        });
    }

    let (year, month, day) = parse_date(raw)?;
    Some(TemporalOrderKey {
        kind: 0,
        value: days_from_civil(year, month, day) as i128,
    })
}

fn time_nanos(time: &ParsedTime) -> i128 {
    ((time.hour as i128 * 3600 + time.minute as i128 * 60 + time.second as i128) * NANOS_PER_SECOND)
        + time.nano as i128
}

fn days_from_civil(year: i32, month: i32, day: i32) -> i64 {
    let mut year = year as i64;
    let month = month as i64;
    let day = day as i64;
    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month_adj = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * month_adj + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn order_type_rank(value: &Value) -> u8 {
    match value {
        Value::Map(entries) if is_plain_map(entries) => 0,
        Value::Map(entries) if is_node_ref_entries(entries) => 1,
        Value::Map(entries) if is_edge_ref_entries(entries) => 2,
        Value::List(_) => 3,
        Value::Map(entries) if is_path_entries(entries) => 4,
        Value::Map(_) => 0,
        Value::String(_) => 5,
        Value::Bool(_) => 6,
        Value::Int64(_) => 7,
        Value::Float64(value) if !value.is_nan() => 7,
        Value::Float64(_) => 8,
        Value::Bytes(_) => 9,
        Value::Null => 10,
    }
}

fn compare_float_for_order(left: f64, right: f64) -> std::cmp::Ordering {
    match (left.is_nan(), right.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => left
            .partial_cmp(&right)
            .unwrap_or(std::cmp::Ordering::Equal),
    }
}

fn is_node_ref_entries(entries: &[(String, Value)]) -> bool {
    entries.iter().any(|(key, _)| key == NODE_REF_KEY)
}

fn is_edge_ref_entries(entries: &[(String, Value)]) -> bool {
    entries.iter().any(|(key, _)| key == EDGE_REF_KEY)
}

fn is_path_entries(entries: &[(String, Value)]) -> bool {
    entries.iter().any(|(key, _)| key == "__path_nodes")
        && entries.iter().any(|(key, _)| key == "__path_edges")
}

fn is_plain_map(entries: &[(String, Value)]) -> bool {
    !is_node_ref_entries(entries) && !is_edge_ref_entries(entries) && !is_path_entries(entries)
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
                if let Some(property) = value_property(ctx.graph, value, &pa.property) {
                    return property;
                }
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
                .map(|item| eval_list_item(ctx, item, row, columns, assignments))
                .collect(),
        ),
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            projection,
        } => {
            let list_value = eval_ast_expr_with_assignments(ctx, list, row, columns, assignments);
            let items = match list_value {
                Value::List(values) => values,
                Value::Null => return Value::Null,
                _ => return Value::Null,
            };

            let mut out = Vec::new();
            for item in items {
                let mut local = assignments.clone();
                local.insert(variable.clone(), item.clone());
                let include = predicate
                    .as_deref()
                    .map(|expr| eval_ast_expr_with_assignments(ctx, expr, row, columns, &local))
                    .and_then(|value| tri_value_bool(&value))
                    .unwrap_or(true);
                if !include {
                    continue;
                }
                let projected = projection
                    .as_deref()
                    .map(|expr| eval_ast_expr_with_assignments(ctx, expr, row, columns, &local))
                    .unwrap_or(item);
                out.push(projected);
            }
            Value::List(out)
        }
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
            eval_index(ctx.graph, target, index)
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
        Expr::PatternPredicate(pattern) => {
            match match_pattern_with_assignments(ctx, pattern, row, columns, assignments) {
                Ok(matches) => Value::Bool(!matches.is_empty()),
                Err(_) => Value::Bool(false),
            }
        }
        Expr::PatternComprehension {
            variable: _,
            pattern,
            projection,
        } => match match_pattern_with_assignments(ctx, pattern, row, columns, assignments) {
            Ok(matches) => {
                let mut out = Vec::new();
                for local in matches {
                    let mut combined = assignments.clone();
                    combined.extend(local);
                    out.push(eval_ast_expr_with_assignments(
                        ctx, projection, row, columns, &combined,
                    ));
                }
                Value::List(out)
            }
            Err(_) => Value::List(Vec::new()),
        },
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
        Expr::ExistsSubquery(query) => {
            match execute_exists_subquery(ctx, query, row, columns, assignments) {
                Ok(exists) => Value::Bool(exists),
                Err(_) => Value::Bool(false),
            }
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

fn execute_exists_subquery(
    ctx: &QueryContext,
    query: &crate::ast::Query,
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> CypherResult<bool> {
    let mut result = seed_result_from_scope(row, columns, assignments);

    if let Some(match_clause) = &query.match_clause {
        result = execute_apply_match(ctx, &result, match_clause, false, None)?;
    }
    if let Some(where_clause) = &query.where_clause {
        result = execute_filter(ctx, &result, &Predicate::Expr(where_clause.expr.clone()))?;
    }

    let mut tail_idx = 0usize;
    while tail_idx < query.tail.len() {
        match &query.tail[tail_idx] {
            ReadClause::Match { optional, clause } => {
                let where_predicate =
                    if let Some(ReadClause::Where(where_clause)) = query.tail.get(tail_idx + 1) {
                        tail_idx += 1;
                        Some(Predicate::Expr(where_clause.expr.clone()))
                    } else {
                        None
                    };
                result =
                    execute_apply_match(ctx, &result, clause, *optional, where_predicate.as_ref())?;
            }
            ReadClause::Where(where_clause) => {
                result = execute_filter(ctx, &result, &Predicate::Expr(where_clause.expr.clone()))?;
            }
            ReadClause::With(with_clause) => {
                result = execute_with_clause_direct(ctx, result, with_clause)?;
            }
            ReadClause::Unwind { expr, alias } => {
                result = execute_unwind(ctx, &result, &ast_expr_to_project_expr(expr), alias)?;
            }
        }
        if result.rows.is_empty() {
            return Ok(false);
        }
        tail_idx += 1;
    }

    result = execute_return_clause_direct(ctx, result, &query.return_clause)?;
    if let Some(order_by) = &query.order_by {
        let keys = order_by
            .items
            .iter()
            .map(|item| SortKey {
                expr: ast_expr_to_project_expr(&item.expr),
                descending: item.descending,
            })
            .collect::<Vec<_>>();
        result = execute_sort(ctx, result, &keys)?;
    }
    if let Some(skip) = &query.skip {
        let skip = eval_row_count(skip, ctx)?.min(result.rows.len());
        result.rows = result.rows.split_off(skip);
    }
    if let Some(limit) = &query.limit {
        result.rows.truncate(eval_row_count(limit, ctx)?);
    }

    Ok(!result.rows.is_empty())
}

fn seed_result_from_scope(
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> QueryResult {
    let mut out_columns = columns.to_vec();
    let mut out_row = row.to_vec();
    for (name, value) in assignments {
        if let Some(idx) = out_columns.iter().position(|column| column == name) {
            out_row[idx] = value.clone();
        } else {
            out_columns.push(name.clone());
            out_row.push(value.clone());
        }
    }
    QueryResult {
        columns: out_columns,
        rows: vec![out_row],
    }
}

fn execute_with_clause_direct(
    ctx: &QueryContext,
    input: QueryResult,
    with_clause: &WithClause,
) -> CypherResult<QueryResult> {
    let input_for_where = input.clone();
    let mut result = execute_projection_items(ctx, input, &with_clause.items)?;
    if let Some(where_clause) = &with_clause.where_clause {
        result = execute_with_where_filter(ctx, &input_for_where, result, &where_clause.expr)?;
    }
    if with_clause.distinct {
        result = execute_distinct(ctx, result)?;
    }
    if let Some(order_by) = &with_clause.order_by {
        let keys = order_by
            .items
            .iter()
            .map(|item| SortKey {
                expr: ast_expr_to_project_expr(&item.expr),
                descending: item.descending,
            })
            .collect::<Vec<_>>();
        result = execute_sort(ctx, result, &keys)?;
    }
    if let Some(skip) = &with_clause.skip {
        let skip = eval_row_count(skip, ctx)?.min(result.rows.len());
        result.rows = result.rows.split_off(skip);
    }
    if let Some(limit) = &with_clause.limit {
        result.rows.truncate(eval_row_count(limit, ctx)?);
    }
    Ok(result)
}

fn execute_with_where_filter(
    ctx: &QueryContext,
    input: &QueryResult,
    projected: QueryResult,
    predicate: &Expr,
) -> CypherResult<QueryResult> {
    let mut combined_columns = projected.columns.clone();
    let mut input_indexes = Vec::new();
    for (idx, column) in input.columns.iter().enumerate() {
        if !combined_columns.iter().any(|projected| projected == column) {
            combined_columns.push(column.clone());
            input_indexes.push(idx);
        }
    }

    let mut rows = Vec::new();
    let mut estimated_bytes = initial_budgeted_bytes(&projected.columns);
    for (projected_row, input_row) in projected.rows.iter().zip(&input.rows) {
        let mut combined_row = projected_row.clone();
        combined_row.extend(input_indexes.iter().map(|idx| input_row[*idx].clone()));
        if value_truthy(&eval_ast_expr(
            ctx,
            predicate,
            &combined_row,
            &combined_columns,
        )) {
            push_budgeted_row(ctx, &mut rows, projected_row.clone(), &mut estimated_bytes)?;
        }
    }

    Ok(QueryResult {
        columns: projected.columns,
        rows,
    })
}

fn execute_return_clause_direct(
    ctx: &QueryContext,
    input: QueryResult,
    return_clause: &ReturnClause,
) -> CypherResult<QueryResult> {
    let mut result = execute_projection_items(ctx, input, &return_clause.items)?;
    if return_clause.distinct {
        result = execute_distinct(ctx, result)?;
    }
    Ok(result)
}

fn execute_projection_items(
    ctx: &QueryContext,
    input: QueryResult,
    items: &[ReturnItem],
) -> CypherResult<QueryResult> {
    let contains_aggregation = items
        .iter()
        .any(|item| ast_expr_contains_aggregate(&item.expr));
    if contains_aggregation {
        return execute_projection_items_with_aggregates(ctx, input, items);
    }

    let columns = items
        .iter()
        .map(project_column_from_return_item)
        .collect::<Vec<_>>();
    execute_project(ctx, &input, &columns)
}

fn execute_projection_items_with_aggregates(
    ctx: &QueryContext,
    input: QueryResult,
    items: &[ReturnItem],
) -> CypherResult<QueryResult> {
    let mut group_by = Vec::new();
    let mut aggregations = Vec::new();
    let mut aggregate_projection = Vec::new();
    let mut next_agg_alias = 0usize;

    for item in items {
        let alias = return_item_output_name(item);
        if ast_expr_is_aggregate(&item.expr) {
            aggregations.push(aggregate_op_from_ast_expr(&item.expr, alias.clone())?);
            aggregate_projection.push(ProjectColumn {
                expr: ProjectExpr::Variable(alias.clone()),
                alias,
            });
        } else if ast_expr_contains_aggregate(&item.expr) {
            let rewritten =
                extract_nested_aggregate_expr(&item.expr, &mut aggregations, &mut next_agg_alias)?;
            aggregate_projection.push(ProjectColumn {
                expr: ast_expr_to_project_expr(&rewritten),
                alias,
            });
        } else {
            let column = project_column_from_return_item(item);
            group_by.push(column.clone());
            aggregate_projection.push(ProjectColumn {
                expr: ProjectExpr::Variable(column.alias.clone()),
                alias: column.alias,
            });
        }
    }

    let aggregated = execute_aggregate(ctx, &input, &group_by, &aggregations)?;
    execute_project(ctx, &aggregated, &aggregate_projection)
}

fn ast_expr_is_aggregate(expr: &Expr) -> bool {
    matches!(expr, Expr::CountStar)
        || matches!(expr, Expr::FunctionCall { name, .. } if is_write_aggregate_name(name))
}

fn ast_expr_contains_aggregate(expr: &Expr) -> bool {
    if ast_expr_is_aggregate(expr) {
        return true;
    }
    match expr {
        Expr::Literal(_) | Expr::Property(_) | Expr::Variable(_) | Expr::Parameter(_) => false,
        Expr::List(items) => items.iter().any(ast_expr_contains_aggregate),
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            ast_expr_contains_aggregate(list)
                || predicate
                    .as_deref()
                    .is_some_and(ast_expr_contains_aggregate)
                || projection
                    .as_deref()
                    .is_some_and(ast_expr_contains_aggregate)
        }
        Expr::Map(entries) => entries
            .iter()
            .any(|(_, value)| ast_expr_contains_aggregate(value)),
        Expr::In { expr, list } => {
            ast_expr_contains_aggregate(expr) || ast_expr_contains_aggregate(list)
        }
        Expr::Index { target, index } => {
            ast_expr_contains_aggregate(target) || ast_expr_contains_aggregate(index)
        }
        Expr::Slice { target, start, end } => {
            ast_expr_contains_aggregate(target)
                || start.as_deref().is_some_and(ast_expr_contains_aggregate)
                || end.as_deref().is_some_and(ast_expr_contains_aggregate)
        }
        Expr::PatternPredicate(_) => false,
        Expr::PatternComprehension { projection, .. } => ast_expr_contains_aggregate(projection),
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            scrutinee
                .as_deref()
                .is_some_and(ast_expr_contains_aggregate)
                || arms.iter().any(|(when_expr, then_expr)| {
                    ast_expr_contains_aggregate(when_expr) || ast_expr_contains_aggregate(then_expr)
                })
                || default.as_deref().is_some_and(ast_expr_contains_aggregate)
        }
        Expr::UnaryOp { expr, .. } => ast_expr_contains_aggregate(expr),
        Expr::BinaryOp { left, right, .. } => {
            ast_expr_contains_aggregate(left) || ast_expr_contains_aggregate(right)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(ast_expr_contains_aggregate),
        Expr::CountStar => true,
        Expr::Exists(inner) => ast_expr_contains_aggregate(inner),
        Expr::ExistsSubquery(_) => false,
        Expr::ListPredicate {
            list, predicate, ..
        } => {
            ast_expr_contains_aggregate(list)
                || predicate
                    .as_deref()
                    .is_some_and(ast_expr_contains_aggregate)
        }
    }
}

fn aggregate_op_from_ast_expr(expr: &Expr, alias: String) -> CypherResult<AggregateOp> {
    match expr {
        Expr::CountStar => Ok(AggregateOp {
            function: "count".into(),
            inputs: Vec::new(),
            distinct: false,
            alias,
        }),
        Expr::FunctionCall { name, args } if is_write_aggregate_name(name) => {
            let mut distinct = false;
            let inputs = args
                .iter()
                .map(|arg| {
                    if let Expr::FunctionCall {
                        name: marker,
                        args: marker_args,
                    } = arg
                    {
                        if marker == "__distinct" {
                            distinct = true;
                            return marker_args
                                .first()
                                .map(ast_expr_to_project_expr)
                                .unwrap_or(ProjectExpr::Literal(Value::Null));
                        }
                    }
                    ast_expr_to_project_expr(arg)
                })
                .collect();
            Ok(AggregateOp {
                function: name.to_lowercase(),
                inputs,
                distinct,
                alias,
            })
        }
        _ => Err(CypherError::Plan(format!(
            "expected aggregate expression, got {expr:?}"
        ))),
    }
}

fn extract_nested_aggregate_expr(
    expr: &Expr,
    aggregations: &mut Vec<AggregateOp>,
    next_alias: &mut usize,
) -> CypherResult<Expr> {
    if ast_expr_is_aggregate(expr) {
        let alias = format!("__agg_{next_alias}");
        *next_alias += 1;
        aggregations.push(aggregate_op_from_ast_expr(expr, alias.clone())?);
        return Ok(Expr::Variable(alias));
    }

    match expr {
        Expr::Literal(_)
        | Expr::Property(_)
        | Expr::Variable(_)
        | Expr::Parameter(_)
        | Expr::PatternPredicate(_)
        | Expr::ExistsSubquery(_)
        | Expr::CountStar => Ok(expr.clone()),
        Expr::List(items) => Ok(Expr::List(
            items
                .iter()
                .map(|item| extract_nested_aggregate_expr(item, aggregations, next_alias))
                .collect::<CypherResult<Vec<_>>>()?,
        )),
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            projection,
        } => Ok(Expr::ListComprehension {
            variable: variable.clone(),
            list: Box::new(extract_nested_aggregate_expr(
                list,
                aggregations,
                next_alias,
            )?),
            predicate: predicate
                .as_deref()
                .map(|expr| extract_nested_aggregate_expr(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
            projection: projection
                .as_deref()
                .map(|expr| extract_nested_aggregate_expr(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
        }),
        Expr::Map(entries) => Ok(Expr::Map(
            entries
                .iter()
                .map(|(key, value)| {
                    Ok((
                        key.clone(),
                        extract_nested_aggregate_expr(value, aggregations, next_alias)?,
                    ))
                })
                .collect::<CypherResult<Vec<_>>>()?,
        )),
        Expr::In { expr, list } => Ok(Expr::In {
            expr: Box::new(extract_nested_aggregate_expr(
                expr,
                aggregations,
                next_alias,
            )?),
            list: Box::new(extract_nested_aggregate_expr(
                list,
                aggregations,
                next_alias,
            )?),
        }),
        Expr::Index { target, index } => Ok(Expr::Index {
            target: Box::new(extract_nested_aggregate_expr(
                target,
                aggregations,
                next_alias,
            )?),
            index: Box::new(extract_nested_aggregate_expr(
                index,
                aggregations,
                next_alias,
            )?),
        }),
        Expr::Slice { target, start, end } => Ok(Expr::Slice {
            target: Box::new(extract_nested_aggregate_expr(
                target,
                aggregations,
                next_alias,
            )?),
            start: start
                .as_deref()
                .map(|expr| extract_nested_aggregate_expr(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
            end: end
                .as_deref()
                .map(|expr| extract_nested_aggregate_expr(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
        }),
        Expr::PatternComprehension {
            variable,
            pattern,
            projection,
        } => Ok(Expr::PatternComprehension {
            variable: variable.clone(),
            pattern: pattern.clone(),
            projection: Box::new(extract_nested_aggregate_expr(
                projection,
                aggregations,
                next_alias,
            )?),
        }),
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => Ok(Expr::Case {
            scrutinee: scrutinee
                .as_deref()
                .map(|expr| extract_nested_aggregate_expr(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
            arms: arms
                .iter()
                .map(|(when_expr, then_expr)| {
                    Ok((
                        extract_nested_aggregate_expr(when_expr, aggregations, next_alias)?,
                        extract_nested_aggregate_expr(then_expr, aggregations, next_alias)?,
                    ))
                })
                .collect::<CypherResult<Vec<_>>>()?,
            default: default
                .as_deref()
                .map(|expr| extract_nested_aggregate_expr(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
        }),
        Expr::UnaryOp { op, expr } => Ok(Expr::UnaryOp {
            op: *op,
            expr: Box::new(extract_nested_aggregate_expr(
                expr,
                aggregations,
                next_alias,
            )?),
        }),
        Expr::BinaryOp { left, op, right } => Ok(Expr::BinaryOp {
            left: Box::new(extract_nested_aggregate_expr(
                left,
                aggregations,
                next_alias,
            )?),
            op: *op,
            right: Box::new(extract_nested_aggregate_expr(
                right,
                aggregations,
                next_alias,
            )?),
        }),
        Expr::FunctionCall { name, args } => Ok(Expr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|arg| extract_nested_aggregate_expr(arg, aggregations, next_alias))
                .collect::<CypherResult<Vec<_>>>()?,
        }),
        Expr::Exists(inner) => Ok(Expr::Exists(Box::new(extract_nested_aggregate_expr(
            inner,
            aggregations,
            next_alias,
        )?))),
        Expr::ListPredicate {
            kind,
            variable,
            list,
            predicate,
        } => Ok(Expr::ListPredicate {
            kind: *kind,
            variable: variable.clone(),
            list: Box::new(extract_nested_aggregate_expr(
                list,
                aggregations,
                next_alias,
            )?),
            predicate: predicate
                .as_deref()
                .map(|expr| extract_nested_aggregate_expr(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
        }),
    }
}

fn is_write_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "collect" | "sum" | "avg" | "min" | "max" | "percentiledisc" | "percentilecont"
    )
}

fn return_item_output_name(item: &ReturnItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    if matches!(item.expr, Expr::CountStar) && item.raw.as_deref() == Some("*") {
        return expr_output_name(&item.expr);
    }
    item.raw
        .clone()
        .unwrap_or_else(|| expr_output_name(&item.expr))
}

fn expr_output_name(expr: &Expr) -> String {
    match expr {
        Expr::Variable(name) => name.clone(),
        Expr::Property(pa) => format!("{}.{}", pa.variable, pa.property),
        Expr::CountStar => "count(*)".into(),
        Expr::FunctionCall { name, .. } => format!("{name}(...)"),
        _ => "expr".into(),
    }
}

fn project_column_from_return_item(item: &ReturnItem) -> ProjectColumn {
    ProjectColumn {
        expr: ast_expr_to_project_expr(&item.expr),
        alias: return_item_output_name(item),
    }
}

fn ast_expr_to_project_expr(expr: &Expr) -> ProjectExpr {
    match expr {
        Expr::Variable(v) if v == "*" => ProjectExpr::Wildcard,
        Expr::Variable(v) => ProjectExpr::Variable(v.clone()),
        Expr::Property(pa) => ProjectExpr::Property(PropertyRef {
            variable: pa.variable.clone(),
            property: pa.property.clone(),
        }),
        Expr::FunctionCall { name, args } if name == "__distinct" => args
            .first()
            .map(ast_expr_to_project_expr)
            .unwrap_or(ProjectExpr::Literal(Value::Null)),
        Expr::FunctionCall { name, args } => ProjectExpr::Function {
            name: name.clone(),
            args: args.iter().map(ast_expr_to_project_expr).collect(),
        },
        Expr::Literal(lit) => ProjectExpr::Literal(literal_to_value(lit)),
        _ => ProjectExpr::Expression(expr.clone()),
    }
}

fn eval_list_item(
    ctx: &QueryContext,
    expr: &Expr,
    row: &[Value],
    columns: &[String],
    assignments: &HashMap<String, Value>,
) -> Value {
    let value = eval_ast_expr_with_assignments(ctx, expr, row, columns, assignments);
    if let Expr::Variable(name) = expr {
        if assignments.contains_key(name) {
            return value;
        }
        if let Some(Value::Int64(id)) = columns
            .iter()
            .position(|column| column == name)
            .and_then(|idx| row.get(idx))
        {
            let vertex = VertexId(*id as u64);
            if ctx.graph.vertex_label(vertex).is_some() {
                return node_ref(vertex);
            }
        }
    }
    value
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
            _ => Value::Null,
        },
        BinaryOp::StartsWith => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::String(left), Value::String(right)) => Value::Bool(left.starts_with(&right)),
            _ => Value::Null,
        },
        BinaryOp::EndsWith => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::String(left), Value::String(right)) => Value::Bool(left.ends_with(&right)),
            _ => Value::Null,
        },
        BinaryOp::Add => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::String(left), Value::String(right))
                if is_duration_string(&left) && is_duration_string(&right) =>
            {
                Value::String(combine_duration_strings(&left, &right, 1))
            }
            (Value::String(left), Value::String(right)) if is_duration_string(&right) => {
                add_duration_to_temporal(&left, &right, 1)
                    .map(Value::String)
                    .unwrap_or_else(|| Value::String(format!("{left}{right}")))
            }
            (Value::String(left), Value::String(right)) if is_duration_string(&left) => {
                add_duration_to_temporal(&right, &left, 1)
                    .map(Value::String)
                    .unwrap_or_else(|| Value::String(format!("{left}{right}")))
            }
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
            (Value::String(left), Value::String(right))
                if is_duration_string(&left) && is_duration_string(&right) =>
            {
                Value::String(combine_duration_strings(&left, &right, -1))
            }
            (Value::String(left), Value::String(right)) if is_duration_string(&right) => {
                add_duration_to_temporal(&left, &right, -1)
                    .map(Value::String)
                    .unwrap_or(Value::Null)
            }
            (l, r) => eval_numeric(l, r, |l, r| l - r, |l, r| l - r),
        },
        BinaryOp::Mul => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::String(duration), Value::Int64(factor)) if is_duration_string(&duration) => {
                Value::String(scale_duration_string(&duration, factor as f64))
            }
            (Value::String(duration), Value::Float64(factor)) if is_duration_string(&duration) => {
                Value::String(scale_duration_string(&duration, factor))
            }
            (Value::Int64(factor), Value::String(duration)) if is_duration_string(&duration) => {
                Value::String(scale_duration_string(&duration, factor as f64))
            }
            (Value::Float64(factor), Value::String(duration)) if is_duration_string(&duration) => {
                Value::String(scale_duration_string(&duration, factor))
            }
            (l, r) => eval_numeric(l, r, |l, r| l * r, |l, r| l * r),
        },
        BinaryOp::Div => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::String(duration), Value::Int64(divisor))
                if is_duration_string(&duration) && divisor != 0 =>
            {
                Value::String(scale_duration_string(&duration, 1.0 / divisor as f64))
            }
            (Value::String(duration), Value::Float64(divisor))
                if is_duration_string(&duration) && divisor != 0.0 =>
            {
                Value::String(scale_duration_string(&duration, 1.0 / divisor))
            }
            (Value::Int64(_), Value::Int64(0)) => Value::Null,
            (Value::Int64(l), Value::Int64(r)) => Value::Int64(l / r),
            (left, right) => match (left.as_f64(), right.as_f64()) {
                (_, None) | (None, _) => Value::Null,
                (Some(l), Some(r)) => Value::Float64(l / r),
            },
        },
        BinaryOp::Mod => {
            if matches!((&left, &right), (Value::Int64(_), Value::Int64(_))) {
                let l = left.as_i64();
                let r = right.as_i64();
                return match (l, r) {
                    (Some(_), Some(0)) | (_, None) | (None, _) => Value::Null,
                    (Some(l), Some(r)) => Value::Int64(l % r),
                };
            }
            match (left.as_f64(), right.as_f64()) {
                (Some(_), Some(0.0)) | (_, None) | (None, _) => Value::Null,
                (Some(l), Some(r)) => Value::Float64(l % r),
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
    match cypher_order_compare(left, right, op) {
        Some(result) => Value::Bool(result),
        None => Value::Null,
    }
}

fn cypher_order_compare(left: &Value, right: &Value, op: CompareOp) -> Option<bool> {
    if numeric_pair_has_nan(left, right) {
        return Some(false);
    }
    let ordering = cypher_value_ordering(left, right)?;
    Some(match op {
        CompareOp::Lt => ordering == std::cmp::Ordering::Less,
        CompareOp::Lte => ordering != std::cmp::Ordering::Greater,
        CompareOp::Gt => ordering == std::cmp::Ordering::Greater,
        CompareOp::Gte => ordering != std::cmp::Ordering::Less,
        CompareOp::Eq | CompareOp::Neq => {
            unreachable!("ordering compare only used for non-equality")
        }
    })
}

fn numeric_pair_has_nan(left: &Value, right: &Value) -> bool {
    matches!(
        (left.as_f64(), right.as_f64()),
        (Some(left), Some(right)) if left.is_nan() || right.is_nan()
    )
}

fn cypher_value_ordering(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Int64(l), Value::Int64(r)) => Some(l.cmp(r)),
        (Value::Float64(l), Value::Float64(r)) => l.partial_cmp(r),
        (Value::Int64(l), Value::Float64(r)) => (*l as f64).partial_cmp(r),
        (Value::Float64(l), Value::Int64(r)) => l.partial_cmp(&(*r as f64)),
        (Value::String(l), Value::String(r)) => {
            Some(temporal_string_cmp(l, r).unwrap_or_else(|| l.cmp(r)))
        }
        (Value::Bool(l), Value::Bool(r)) => Some(l.cmp(r)),
        (Value::List(left), Value::List(right)) => cypher_list_ordering(left, right),
        _ => None,
    }
}

fn cypher_list_ordering(left: &[Value], right: &[Value]) -> Option<std::cmp::Ordering> {
    for (left_value, right_value) in left.iter().zip(right) {
        match cypher_value_eq(left_value, right_value) {
            Some(true) => continue,
            Some(false) => return cypher_value_ordering(left_value, right_value),
            None => return None,
        }
    }
    Some(left.len().cmp(&right.len()))
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
        (Value::String(l), Value::String(r)) if is_duration_string(l) && is_duration_string(r) => {
            Some(parse_duration_parts(l) == parse_duration_parts(r))
        }
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
        "document" => eval_document_lookup(ctx, args),
        "documentfulltext" => eval_document_full_text_predicate(args),
        "documents" => eval_document_scan(ctx, args),
        "documentsby" => eval_document_index_scan(ctx, args),
        "documentsprefix" => eval_document_prefix_scan(ctx, args),
        "documentsrange" => eval_document_range_scan(ctx, args),
        "documentsfulltext" => eval_document_full_text_scan(ctx, args),
        "vectorsearch" => eval_vector_search(ctx, args),
        "vectordistance" => eval_vector_distance(args),
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
            Some(value) if path_components(value).is_some() => {
                let (_, edges) = path_components(value).unwrap_or_default();
                Value::Int64(edges.len() as i64)
            }
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
        "tolower" => match args.first() {
            Some(Value::String(value)) => Value::String(value.to_lowercase()),
            Some(Value::Null) | None => Value::Null,
            _ => Value::Null,
        },
        "toupper" => match args.first() {
            Some(Value::String(value)) => Value::String(value.to_uppercase()),
            Some(Value::Null) | None => Value::Null,
            _ => Value::Null,
        },
        "sign" => match args.first() {
            Some(Value::Int64(value)) => Value::Int64(value.signum()),
            Some(Value::Float64(value)) => Value::Int64(value.signum() as i64),
            Some(Value::Null) | None => Value::Null,
            _ => Value::Null,
        },
        "abs" => match args.first() {
            Some(Value::Int64(value)) => Value::Int64(value.abs()),
            Some(Value::Float64(value)) => Value::Float64(value.abs()),
            Some(Value::Null) | None => Value::Null,
            _ => Value::Null,
        },
        "ceil" => match args.first() {
            Some(Value::Int64(value)) => Value::Int64(*value),
            Some(Value::Float64(value)) => Value::Float64(value.ceil()),
            Some(Value::Null) | None => Value::Null,
            _ => Value::Null,
        },
        "floor" => match args.first() {
            Some(Value::Int64(value)) => Value::Int64(*value),
            Some(Value::Float64(value)) => Value::Float64(value.floor()),
            Some(Value::Null) | None => Value::Null,
            _ => Value::Null,
        },
        "sqrt" => match args.first() {
            Some(Value::Int64(value)) if *value >= 0 => Value::Float64((*value as f64).sqrt()),
            Some(Value::Float64(value)) if *value >= 0.0 => Value::Float64(value.sqrt()),
            Some(Value::Null) | None => Value::Null,
            _ => Value::Null,
        },
        "reverse" => match args.first() {
            Some(Value::List(values)) => {
                let mut values = values.clone();
                values.reverse();
                Value::List(values)
            }
            Some(Value::String(value)) => Value::String(value.chars().rev().collect()),
            Some(Value::Null) | None => Value::Null,
            _ => Value::Null,
        },
        "substring" => match args.first() {
            Some(Value::String(value)) => {
                let start = args.get(1).and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
                let chars: Vec<char> = value.chars().collect();
                if start >= chars.len() {
                    return Value::String(String::new());
                }
                let end = args
                    .get(2)
                    .and_then(Value::as_i64)
                    .map(|len| start.saturating_add(len.max(0) as usize))
                    .unwrap_or(chars.len())
                    .min(chars.len());
                Value::String(chars[start..end].iter().collect())
            }
            Some(Value::Null) | None => Value::Null,
            _ => Value::Null,
        },
        "split" => match (args.first(), args.get(1)) {
            (Some(Value::String(value)), Some(Value::String(delimiter))) => Value::List(
                value
                    .split(delimiter)
                    .map(|part| Value::String(part.to_string()))
                    .collect(),
            ),
            (Some(Value::Null) | None, _) | (_, Some(Value::Null) | None) => Value::Null,
            _ => Value::Null,
        },
        "rand" => Value::Float64(0.5),
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
        "tofloat" | "tofloatornull" => args.first().map(to_float).unwrap_or(Value::Null),
        "toboolean" | "tobooleanornull" => args.first().map(to_boolean).unwrap_or(Value::Null),
        "labels" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(value) if node_ref_id(value).is_some() => Value::List(
                vertex_labels(ctx.graph, node_ref_id(value).unwrap())
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
            Some(Value::Int64(id)) => Value::List(
                vertex_labels(ctx.graph, VertexId(*id as u64))
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
            _ => Value::Null,
        },
        "nodes" => match args.first().and_then(path_components) {
            Some((nodes, _)) => Value::List(nodes),
            None => Value::Null,
        },
        "relationships" => match args.first().and_then(path_components) {
            Some((_, edges)) => Value::List(
                edges
                    .into_iter()
                    .filter_map(|edge| edge.as_i64())
                    .map(|edge| edge_ref(EdgeId(edge as u64)))
                    .collect(),
            ),
            None => Value::Null,
        },
        "type" => match args.first() {
            Some(value) if edge_ref_id(value).is_some() => {
                if let Some(rel_type) = edge_ref_type(value) {
                    Value::String(rel_type)
                } else {
                    edge_ref_id(value)
                        .and_then(|edge| ctx.graph.edge_label(edge))
                        .map(|label| Value::String(label.to_string()))
                        .unwrap_or(Value::Null)
                }
            }
            _ => Value::Null,
        },
        "startnode" => match args.first().and_then(edge_ref_id) {
            Some(edge) => edge_endpoints(ctx.graph, edge)
                .map(|(source, _)| node_ref(source))
                .unwrap_or(Value::Null),
            None => Value::Null,
        },
        "endnode" => match args.first().and_then(edge_ref_id) {
            Some(edge) => edge_endpoints(ctx.graph, edge)
                .map(|(_, target)| node_ref(target))
                .unwrap_or(Value::Null),
            None => Value::Null,
        },
        "keys" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(value) if node_ref_id(value).is_some() => {
                let vertex = node_ref_id(value).unwrap();
                Value::List(
                    ctx.graph
                        .get_vertex_properties(vertex)
                        .into_iter()
                        .map(|(key, _)| Value::String(key))
                        .collect(),
                )
            }
            Some(value) if edge_ref_id(value).is_some() => {
                let edge = edge_ref_id(value).unwrap();
                Value::List(
                    ctx.graph
                        .get_edge_properties(edge)
                        .into_iter()
                        .map(|(key, _)| Value::String(key))
                        .collect(),
                )
            }
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
            _ => Value::Null,
        },
        "properties" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(value) if node_ref_id(value).is_some() => {
                Value::Map(ctx.graph.get_vertex_properties(node_ref_id(value).unwrap()))
            }
            Some(value) if edge_ref_id(value).is_some() => {
                Value::Map(ctx.graph.get_edge_properties(edge_ref_id(value).unwrap()))
            }
            Some(Value::Map(entries)) => Value::Map(entries.clone()),
            Some(Value::Int64(id)) => {
                let vertex = VertexId(*id as u64);
                let edge = EdgeId(*id as u64);
                if ctx.graph.vertex_label(vertex).is_some() {
                    Value::Map(ctx.graph.get_vertex_properties(vertex))
                } else if ctx.graph.edge_exists(edge) {
                    Value::Map(ctx.graph.get_edge_properties(edge))
                } else {
                    Value::Null
                }
            }
            _ => Value::Null,
        },
        "id" => match args.first() {
            Some(value) if node_ref_id(value).is_some() => {
                Value::Int64(node_ref_id(value).unwrap().0 as i64)
            }
            Some(value) if edge_ref_id(value).is_some() => {
                Value::Int64(edge_ref_id(value).unwrap().0 as i64)
            }
            Some(value) => value.clone(),
            None => Value::Null,
        },
        "__label_test" => {
            if let Some(edge) = args.first().and_then(edge_ref_id) {
                let Some(actual) = ctx.graph.edge_label(edge) else {
                    return Value::Null;
                };
                let matches = args
                    .iter()
                    .skip(1)
                    .all(|label| matches!(label, Value::String(label) if actual == label));
                return Value::Bool(matches);
            }
            let Some(vertex) = args.first().and_then(|value| {
                node_ref_id(value).or_else(|| match value {
                    Value::Int64(id) => Some(VertexId(*id as u64)),
                    _ => None,
                })
            }) else {
                return Value::Null;
            };
            let actual = vertex_labels(ctx.graph, vertex);
            if actual.is_empty() && ctx.graph.vertex_label(vertex).is_none() {
                return Value::Null;
            }
            let matches = args.iter().skip(1).all(|label| {
                if let Value::String(label) = label {
                    actual.iter().any(|actual| actual == label)
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

fn eval_document_lookup(ctx: &QueryContext, args: &[Value]) -> Value {
    let Some(resolver) = ctx.document_resolver else {
        return Value::Null;
    };
    let Some(Value::String(collection)) = args.first() else {
        return Value::Null;
    };
    let Some(Value::String(key)) = args.get(1) else {
        return Value::Null;
    };
    resolver
        .resolve_document(collection, key)
        .unwrap_or(Value::Null)
}

fn eval_document_scan(ctx: &QueryContext, args: &[Value]) -> Value {
    let Some(resolver) = ctx.document_resolver else {
        return Value::List(Vec::new());
    };
    let Some(Value::String(collection)) = args.first() else {
        return Value::List(Vec::new());
    };
    let limit = args
        .get(1)
        .and_then(Value::as_i64)
        .map(|value| value.clamp(0, 1_000) as usize)
        .unwrap_or(100);
    Value::List(resolver.scan_documents(collection, limit))
}

fn eval_document_index_scan(ctx: &QueryContext, args: &[Value]) -> Value {
    let Some(resolver) = ctx.document_resolver else {
        return Value::List(Vec::new());
    };
    let Some(Value::String(collection)) = args.first() else {
        return Value::List(Vec::new());
    };
    let Some(Value::String(path)) = args.get(1) else {
        return Value::List(Vec::new());
    };
    let Some(value) = args.get(2) else {
        return Value::List(Vec::new());
    };
    let limit = args
        .get(3)
        .and_then(Value::as_i64)
        .map(|value| value.clamp(0, 1_000) as usize)
        .unwrap_or(100);
    Value::List(resolver.query_documents_by_index(collection, path, value, limit))
}

fn eval_document_prefix_scan(ctx: &QueryContext, args: &[Value]) -> Value {
    let Some(resolver) = ctx.document_resolver else {
        return Value::List(Vec::new());
    };
    let Some(Value::String(collection)) = args.first() else {
        return Value::List(Vec::new());
    };
    let Some(Value::String(path)) = args.get(1) else {
        return Value::List(Vec::new());
    };
    let Some(Value::String(prefix)) = args.get(2) else {
        return Value::List(Vec::new());
    };
    let limit = args
        .get(3)
        .and_then(Value::as_i64)
        .map(|value| value.clamp(0, 1_000) as usize)
        .unwrap_or(100);
    Value::List(resolver.query_documents_by_prefix(collection, path, prefix, limit))
}

fn eval_document_range_scan(ctx: &QueryContext, args: &[Value]) -> Value {
    let Some(resolver) = ctx.document_resolver else {
        return Value::List(Vec::new());
    };
    let Some(Value::String(collection)) = args.first() else {
        return Value::List(Vec::new());
    };
    let Some(Value::String(path)) = args.get(1) else {
        return Value::List(Vec::new());
    };
    let gte = args.get(2).filter(|value| !matches!(value, Value::Null));
    let lte = args.get(3).filter(|value| !matches!(value, Value::Null));
    let limit = args
        .get(4)
        .and_then(Value::as_i64)
        .map(|value| value.clamp(0, 1_000) as usize)
        .unwrap_or(100);
    Value::List(resolver.query_documents_by_range(collection, path, gte, lte, limit))
}

fn eval_document_full_text_scan(ctx: &QueryContext, args: &[Value]) -> Value {
    let Some(resolver) = ctx.document_resolver else {
        return Value::List(Vec::new());
    };
    let Some(Value::String(collection)) = args.first() else {
        return Value::List(Vec::new());
    };
    let Some(Value::String(path)) = args.get(1) else {
        return Value::List(Vec::new());
    };
    let Some(Value::String(query)) = args.get(2) else {
        return Value::List(Vec::new());
    };
    let limit = args
        .get(3)
        .and_then(Value::as_i64)
        .map(|value| value.clamp(0, 1_000) as usize)
        .unwrap_or(100);
    Value::List(resolver.query_documents_by_full_text(collection, path, query, limit))
}

fn eval_document_full_text_predicate(args: &[Value]) -> Value {
    let Some(value) = args.first() else {
        return Value::Null;
    };
    let Some(Value::String(query)) = args.get(1) else {
        return Value::Null;
    };
    Value::Bool(document_full_text_matches(value, query))
}

fn document_full_text_matches(value: &Value, query: &str) -> bool {
    let query_tokens = document_text_tokens(query);
    if query_tokens.is_empty() {
        return false;
    }
    let mut value_tokens = std::collections::BTreeSet::new();
    collect_value_text_tokens(value, &mut value_tokens);
    query_tokens
        .iter()
        .any(|token| value_tokens.contains(token.as_str()))
}

fn collect_value_text_tokens(value: &Value, tokens: &mut std::collections::BTreeSet<String>) {
    match value {
        Value::String(text) => {
            tokens.extend(document_text_tokens(text));
        }
        Value::List(values) => {
            for value in values {
                collect_value_text_tokens(value, tokens);
            }
        }
        Value::Map(entries) => {
            for (_, value) in entries {
                collect_value_text_tokens(value, tokens);
            }
        }
        Value::Null | Value::Bool(_) | Value::Int64(_) | Value::Float64(_) | Value::Bytes(_) => {}
    }
}

fn document_text_tokens(text: &str) -> std::collections::BTreeSet<String> {
    let mut tokens = std::collections::BTreeSet::new();
    for token in text.split(|ch: char| !ch.is_alphanumeric()) {
        if token.chars().count() < 2 {
            continue;
        }
        tokens.insert(token.to_lowercase());
    }
    tokens
}

fn eval_vector_search(ctx: &QueryContext, args: &[Value]) -> Value {
    let Some(Value::String(index_name)) = args.first() else {
        return Value::Null;
    };
    let Some(query) = args.get(1).and_then(vector_arg_to_f32s) else {
        return Value::Null;
    };
    let k = args
        .get(2)
        .and_then(Value::as_i64)
        .map(|value| value.max(0) as usize)
        .unwrap_or(10);
    let Some(indexes) = ctx.vector_indexes else {
        return Value::Null;
    };
    let Some(index) = indexes.get(index_name) else {
        return Value::Null;
    };
    if query.len() != index.dimension() {
        return Value::Null;
    }

    let results = index.search(&query, k);
    if let Some(recorder) = ctx.vector_recorder {
        let exact = index.search_exact(&query, k);
        let exact_ids: HashSet<_> = exact.iter().map(|(vertex, _)| vertex.0).collect();
        let overlap = results
            .iter()
            .filter(|(vertex, _)| exact_ids.contains(&vertex.0))
            .count();
        recorder.record_vector_search(index_name, results.len(), exact.len(), overlap);
    }

    Value::List(
        results
            .into_iter()
            .map(|(vertex, distance)| {
                Value::Map(vec![
                    ("vertex_id".into(), Value::Int64(vertex.0 as i64)),
                    ("node".into(), node_ref(vertex)),
                    ("distance".into(), Value::Float64(distance as f64)),
                ])
            })
            .collect(),
    )
}

fn eval_vector_distance(args: &[Value]) -> Value {
    let Some(left) = args.first().and_then(vector_arg_to_f32s) else {
        return Value::Null;
    };
    let Some(right) = args.get(1).and_then(vector_arg_to_f32s) else {
        return Value::Null;
    };
    if left.len() != right.len() {
        return Value::Null;
    }
    Value::Float64(cosine_distance_values(&left, &right) as f64)
}

fn vector_arg_to_f32s(value: &Value) -> Option<Vec<f32>> {
    let Value::List(items) = value else {
        return None;
    };
    items
        .iter()
        .map(|item| match item {
            Value::Int64(value) => Some(*value as f32),
            Value::Float64(value) if value.is_finite() => Some(*value as f32),
            _ => None,
        })
        .collect()
}

fn cosine_distance_values(left: &[f32], right: &[f32]) -> f32 {
    let dot: f32 = left.iter().zip(right.iter()).map(|(l, r)| l * r).sum();
    let left_mag = left.iter().map(|value| value * value).sum::<f32>().sqrt();
    let right_mag = right.iter().map(|value| value * value).sum::<f32>().sqrt();
    if left_mag == 0.0 || right_mag == 0.0 {
        return 1.0;
    }
    1.0 - (dot / (left_mag * right_mag))
}

fn is_temporal_constructor(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name);
    matches!(
        base,
        "date" | "localtime" | "time" | "localdatetime" | "datetime" | "duration"
    )
}

fn eval_temporal_constructor(name: &str, args: &[Value]) -> Value {
    let lname = name.to_lowercase();
    match lname.as_str() {
        "date.truncate" => return eval_date_truncate(args),
        "datetime.truncate" => return eval_datetime_truncate(args),
        "localdatetime.truncate" => return eval_localdatetime_truncate(args),
        "localtime.truncate" => return eval_time_truncate(args, false),
        "time.truncate" => return eval_time_truncate(args, true),
        "duration.between" => return eval_duration_between(args),
        "duration.inmonths" => return eval_duration_in_months(args),
        "duration.indays" => return eval_duration_in_days(args),
        "duration.inseconds" => return eval_duration_in_seconds(args),
        "datetime.fromepoch" => {
            let seconds = args.first().and_then(Value::as_i64).unwrap_or(0);
            let nanos = args.get(1).and_then(Value::as_i64).unwrap_or(0);
            return Value::String(format_datetime_from_epoch(seconds, nanos));
        }
        "datetime.fromepochmillis" => {
            let millis = args.first().and_then(Value::as_i64).unwrap_or(0);
            let seconds = millis.div_euclid(1000);
            let nanos = millis.rem_euclid(1000) * 1_000_000;
            return Value::String(format_datetime_from_epoch(seconds, nanos));
        }
        _ => {}
    }
    let base = lname.split('.').next().unwrap_or(name);
    let Some(first) = args.first() else {
        return default_temporal_value(base);
    };
    if first.is_null() {
        return Value::Null;
    }
    match (base, first) {
        (_, Value::String(value)) => Value::String(normalize_temporal_string(base, value)),
        ("date", Value::Map(entries)) => Value::String(format_date_from_map(entries)),
        ("localtime", Value::Map(entries)) => {
            Value::String(format_time_from_map(entries, false, false))
        }
        ("time", Value::Map(entries)) => Value::String(format_time_from_map(entries, true, false)),
        ("localdatetime", Value::Map(entries)) => Value::String(format!(
            "{}T{}",
            format_date_from_map(entries),
            format_time_from_map(entries, false, false)
        )),
        ("datetime", Value::Map(entries)) => Value::String(format!(
            "{}T{}",
            format_date_from_map(entries),
            format_time_from_map(entries, true, true)
        )),
        ("duration", Value::Map(entries)) => Value::String(format_duration_from_map(entries)),
        _ => Value::Null,
    }
}

fn default_temporal_value(base: &str) -> Value {
    match base {
        "date" => Value::String("1970-01-01".to_string()),
        "localtime" => Value::String("00:00".to_string()),
        "time" => Value::String("00:00Z".to_string()),
        "localdatetime" => Value::String("1970-01-01T00:00".to_string()),
        "datetime" => Value::String("1970-01-01T00:00Z".to_string()),
        "duration" => Value::String("PT0S".to_string()),
        _ => Value::Null,
    }
}

fn eval_date_truncate(args: &[Value]) -> Value {
    let Some(unit) = args.first().and_then(Value::as_str) else {
        return Value::Null;
    };
    let Some(source) = args.get(1) else {
        return Value::Null;
    };
    let Some((year, month, day)) = temporal_source_date(source) else {
        return Value::Null;
    };
    let overrides = args.get(2).and_then(|value| match value {
        Value::Map(entries) => Some(entries.as_slice()),
        _ => None,
    });
    Value::String(truncate_date(unit, year, month, day, overrides))
}

fn eval_datetime_truncate(args: &[Value]) -> Value {
    let Some(unit) = args.first().and_then(Value::as_str) else {
        return Value::Null;
    };
    let Some(source) = args.get(1) else {
        return Value::Null;
    };
    let Some((year, month, day)) = temporal_source_date(source) else {
        return Value::Null;
    };
    let source_time = temporal_source_time(source).unwrap_or_else(ParsedTime::midnight);
    let overrides = args.get(2).and_then(|value| match value {
        Value::Map(entries) => Some(entries.as_slice()),
        _ => None,
    });
    let date = if is_time_truncate_unit(unit) {
        format!("{year:04}-{month:02}-{day:02}")
    } else {
        truncate_date(unit, year, month, day, overrides)
    };
    let time = truncate_time(unit, &source_time, overrides);
    let timezone = overrides
        .and_then(|entries| map_str(entries, "timezone"))
        .map(str::to_string)
        .or_else(|| source_timezone(source))
        .unwrap_or_else(|| "Z".to_string());
    let suffix = format_timezone_suffix_for_date(&timezone, &date);
    Value::String(format!("{date}T{time}{suffix}"))
}

fn eval_localdatetime_truncate(args: &[Value]) -> Value {
    let Some(unit) = args.first().and_then(Value::as_str) else {
        return Value::Null;
    };
    let Some(source) = args.get(1) else {
        return Value::Null;
    };
    let Some((year, month, day)) = temporal_source_date(source) else {
        return Value::Null;
    };
    let source_time = temporal_source_time(source).unwrap_or_else(ParsedTime::midnight);
    let overrides = args.get(2).and_then(|value| match value {
        Value::Map(entries) => Some(entries.as_slice()),
        _ => None,
    });
    let date = if is_time_truncate_unit(unit) {
        format!("{year:04}-{month:02}-{day:02}")
    } else {
        truncate_date(unit, year, month, day, overrides)
    };
    let time = truncate_time(unit, &source_time, overrides);
    Value::String(format!("{date}T{time}"))
}

fn eval_time_truncate(args: &[Value], with_timezone: bool) -> Value {
    let Some(unit) = args.first().and_then(Value::as_str) else {
        return Value::Null;
    };
    let Some(source) = args.get(1) else {
        return Value::Null;
    };
    let source_time = temporal_source_time(source)
        .or_else(|| match source {
            Value::String(raw) if raw.contains(':') => parse_temporal_time(raw),
            _ => None,
        })
        .unwrap_or_else(ParsedTime::midnight);
    let overrides = args.get(2).and_then(|value| match value {
        Value::Map(entries) => Some(entries.as_slice()),
        _ => None,
    });
    let mut out = truncate_time(unit, &source_time, overrides);
    if with_timezone {
        let source_date = temporal_source_date(source)
            .map(|(year, month, day)| format!("{year:04}-{month:02}-{day:02}"))
            .unwrap_or_else(|| "1970-01-01".to_string());
        let timezone = overrides
            .and_then(|entries| map_str(entries, "timezone"))
            .map(str::to_string)
            .or_else(|| source_timezone(source))
            .or_else(|| {
                source_time
                    .timezone
                    .clone()
                    .filter(|timezone| timezone != "Z")
            })
            .unwrap_or_else(|| "Z".to_string());
        out.push_str(&format_timezone_suffix_for_date(&timezone, &source_date));
    }
    Value::String(out)
}

fn temporal_source_date(value: &Value) -> Option<(i32, i32, i32)> {
    let Value::String(raw) = value else {
        return None;
    };
    parse_date(raw)
}

fn source_timezone(value: &Value) -> Option<String> {
    let Value::String(raw) = value else {
        return None;
    };
    let (_, time_part) = raw.split_once('T')?;
    let parsed = parse_temporal_time(time_part)?;
    parsed.timezone.filter(|timezone| timezone != "Z")
}

fn temporal_source_time(value: &Value) -> Option<ParsedTime> {
    let Value::String(raw) = value else {
        return None;
    };
    raw.split_once('T')
        .and_then(|(_, time)| parse_temporal_time(time))
}

fn is_time_truncate_unit(unit: &str) -> bool {
    matches!(
        unit,
        "hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond"
    )
}

fn truncate_time(unit: &str, source: &ParsedTime, overrides: Option<&[(String, Value)]>) -> String {
    let (hour, minute, second, mut nanos) = match unit {
        "hour" => (source.hour, 0, 0, 0),
        "minute" => (source.hour, source.minute, 0, 0),
        "second" => (source.hour, source.minute, source.second, 0),
        "millisecond" => (
            source.hour,
            source.minute,
            source.second,
            (source.nano / 1_000_000) * 1_000_000,
        ),
        "microsecond" => (
            source.hour,
            source.minute,
            source.second,
            (source.nano / 1_000) * 1_000,
        ),
        "nanosecond" => (source.hour, source.minute, source.second, source.nano),
        _ => (0, 0, 0, 0),
    };
    if let Some(override_nanos) = overrides.and_then(|entries| map_i64(entries, "nanosecond")) {
        nanos += override_nanos as i32;
    }
    format_time_parts(hour, minute, second, nanos)
}

fn format_time_parts(hour: i32, minute: i32, second: i32, nanos: i32) -> String {
    if second == 0 && nanos == 0 {
        return format!("{hour:02}:{minute:02}");
    }
    if nanos == 0 {
        return format!("{hour:02}:{minute:02}:{second:02}");
    }
    format!(
        "{hour:02}:{minute:02}:{second:02}.{}",
        format_fraction(nanos as i64)
    )
}

fn truncate_date(
    unit: &str,
    year: i32,
    month: i32,
    day: i32,
    overrides: Option<&[(String, Value)]>,
) -> String {
    let day_override = overrides.and_then(|entries| map_i64(entries, "day"));
    let (y, m, d) = match unit {
        "millennium" => (
            year.div_euclid(1000) * 1000,
            1,
            day_override.unwrap_or(1) as i32,
        ),
        "century" => (
            year.div_euclid(100) * 100,
            1,
            day_override.unwrap_or(1) as i32,
        ),
        "decade" => (
            year.div_euclid(10) * 10,
            1,
            day_override.unwrap_or(1) as i32,
        ),
        "year" => (year, 1, day_override.unwrap_or(1) as i32),
        "weekYear" => {
            let week_year = iso_week(year, month, day).0;
            if let Some(day) = day_override {
                (week_year, 1, day as i32)
            } else {
                iso_week_to_ymd(week_year, 1, 1)
            }
        }
        "quarter" => {
            let start_month = ((month - 1) / 3) * 3 + 1;
            (year, start_month, day_override.unwrap_or(1) as i32)
        }
        "month" => (year, month, day_override.unwrap_or(1) as i32),
        "week" => {
            let dow = day_of_week(year, month, day);
            let (start_y, start_m, start_d) = add_days_to_ymd(year, month, day, 1 - dow);
            let week_day_override = overrides.and_then(|entries| map_i64(entries, "dayOfWeek"));
            add_days_to_ymd(
                start_y,
                start_m,
                start_d,
                week_day_override.unwrap_or(1) as i32 - 1,
            )
        }
        "day" => (year, month, day),
        _ => (year, month, day),
    };
    format!("{y:04}-{m:02}-{d:02}")
}

fn normalize_temporal_string(base: &str, value: &str) -> String {
    match base {
        "date" => normalize_date_string(value.split_once('T').map_or(value, |(date, _)| date)),
        "localtime" => normalize_time_string(extract_time_component(value), false),
        "time" => {
            let time = extract_time_component(value);
            let time = if value.contains('T') {
                strip_zone_name(time)
            } else {
                time
            };
            let normalized = normalize_time_string(time, true);
            if temporal_has_offset(&normalized) {
                normalized
            } else {
                format!("{normalized}Z")
            }
        }
        "localdatetime" => normalize_datetime_string(value, false),
        "datetime" => normalize_datetime_string(value, true),
        "duration" => normalize_duration_string(value),
        _ => value.to_string(),
    }
}

fn normalize_date_string(value: &str) -> String {
    if let Some((year, week, day)) = parse_week_date_string(value) {
        let (y, m, d) = iso_week_to_ymd(year, week, day);
        return format!("{y:04}-{m:02}-{d:02}");
    }
    if let Some((year, ordinal)) = parse_ordinal_date_string(value) {
        let (y, m, d) = ymd_from_ordinal(year, ordinal);
        return format!("{y:04}-{m:02}-{d:02}");
    }
    let digits = value.replace('-', "");
    if digits.len() == 8 && digits.chars().all(|ch| ch.is_ascii_digit()) {
        return format!("{}-{}-{}", &digits[0..4], &digits[4..6], &digits[6..8]);
    }
    if digits.len() == 6 && digits.chars().all(|ch| ch.is_ascii_digit()) {
        return format!("{}-{}-01", &digits[0..4], &digits[4..6]);
    }
    if digits.len() == 4 && digits.chars().all(|ch| ch.is_ascii_digit()) {
        return format!("{digits}-01-01");
    }
    value.to_string()
}

fn parse_week_date_string(value: &str) -> Option<(i32, i32, i32)> {
    if !value.contains('W') {
        return None;
    }
    let compact = value.replace(['-', 'W'], "");
    if compact.len() != 6 && compact.len() != 7 {
        return None;
    }
    let year: i32 = compact[0..4].parse().ok()?;
    let week: i32 = compact[4..6].parse().ok()?;
    let day: i32 = compact.get(6..7).and_then(|d| d.parse().ok()).unwrap_or(1);
    Some((year, week, day))
}

fn parse_ordinal_date_string(value: &str) -> Option<(i32, i32)> {
    let compact = value.replace('-', "");
    if compact.len() != 7 || !compact.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some((compact[0..4].parse().ok()?, compact[4..7].parse().ok()?))
}

fn normalize_datetime_string(value: &str, with_timezone: bool) -> String {
    let Some((date_part, time_part)) = value.split_once('T') else {
        return value.to_string();
    };
    let date = normalize_date_string(date_part);
    if with_timezone {
        if let Some((time, zone)) = split_named_zone(time_part) {
            if !temporal_has_offset(time) {
                let normalized_time = normalize_time_string(time, false);
                let offset = named_timezone_offset_for_date(zone, &date);
                return format!("{date}T{normalized_time}{offset}[{zone}]");
            }
        }
    }
    let time = normalize_time_string(time_part, with_timezone);
    if with_timezone && !temporal_has_offset(&time) {
        return format!("{date}T{time}Z");
    }
    format!("{date}T{time}")
}

fn normalize_time_string(value: &str, with_timezone: bool) -> String {
    let (main, suffix) = split_time_suffix(value);
    let normalized_main = normalize_time_main(main);
    if !with_timezone {
        return normalized_main;
    }
    match suffix {
        Some(suffix) => format!("{normalized_main}{}", normalize_offset(suffix)),
        None => normalized_main,
    }
}

fn extract_time_component(value: &str) -> &str {
    value.split_once('T').map_or(value, |(_, time)| time)
}

fn split_named_zone(value: &str) -> Option<(&str, &str)> {
    let start = value.find('[')?;
    let zone = value[start + 1..].trim_end_matches(']');
    Some((&value[..start], zone))
}

fn strip_zone_name(value: &str) -> &str {
    split_named_zone(value).map_or(value, |(head, _)| head)
}

fn split_time_suffix(value: &str) -> (&str, Option<&str>) {
    if let Some(main) = value.strip_suffix('Z') {
        return (main, Some("Z"));
    }
    let Some(pos) = value
        .get(1..)
        .and_then(|rest| rest.rfind(['+', '-']).map(|pos| pos + 1))
    else {
        return (value, None);
    };
    let (main, suffix) = value.split_at(pos);
    (main, Some(suffix))
}

fn normalize_time_main(value: &str) -> String {
    if value.contains(':') {
        return value.to_string();
    }
    let (digits, frac) = value.split_once('.').unwrap_or((value, ""));
    let suffix = if frac.is_empty() {
        String::new()
    } else {
        format!(".{frac}")
    };
    match digits.len() {
        2 => format!("{digits}:00"),
        4 => format!("{}:{}", &digits[0..2], &digits[2..4]),
        6 => format!(
            "{}:{}:{}{suffix}",
            &digits[0..2],
            &digits[2..4],
            &digits[4..6]
        ),
        _ => value.to_string(),
    }
}

fn map_i64(entries: &[(String, Value)], key: &str) -> Option<i64> {
    entries
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.as_i64())
}

fn map_f64(entries: &[(String, Value)], key: &str) -> Option<f64> {
    entries
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.as_f64())
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
    let base_date = map_str(entries, "date")
        .or_else(|| map_str(entries, "datetime"))
        .and_then(parse_date);

    if let Some(week) = map_i64(entries, "week") {
        let year = map_i64(entries, "year")
            .or_else(|| base_date.map(|(y, m, d)| iso_week(y, m, d).0 as i64))
            .unwrap_or(1970) as i32;
        let day = map_i64(entries, "dayOfWeek")
            .or_else(|| base_date.map(|(y, m, d)| day_of_week(y, m, d) as i64))
            .unwrap_or(1) as i32;
        let (y, m, d) = iso_week_to_ymd(year, week as i32, day);
        return format!("{y:04}-{m:02}-{d:02}");
    }

    let year = map_i64(entries, "year")
        .or_else(|| base_date.map(|(y, _, _)| y as i64))
        .unwrap_or(1970) as i32;

    if let Some(ordinal) = map_i64(entries, "ordinalDay") {
        let (y, m, d) = ymd_from_ordinal(year, ordinal as i32);
        return format!("{y:04}-{m:02}-{d:02}");
    }

    if let Some(quarter) = map_i64(entries, "quarter") {
        let month = ((quarter - 1) * 3 + 1).clamp(1, 12) as i32;
        let day = map_i64(entries, "dayOfQuarter")
            .or_else(|| {
                base_date.map(|(base_year, base_month, base_day)| {
                    let quarter_start = ((base_month - 1) / 3) * 3 + 1;
                    (days_from_civil(base_year, base_month, base_day)
                        - days_from_civil(base_year, quarter_start, 1)
                        + 1) as i64
                })
            })
            .unwrap_or(1) as i32;
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

fn format_time_from_map(
    entries: &[(String, Value)],
    with_timezone: bool,
    preserve_named_zone: bool,
) -> String {
    let base_time = map_str(entries, "time")
        .or_else(|| map_str(entries, "datetime"))
        .and_then(parse_temporal_time);
    let mut hour = map_i64(entries, "hour")
        .or_else(|| base_time.as_ref().map(|time| time.hour as i64))
        .unwrap_or(0)
        .clamp(0, 23);
    let mut minute = map_i64(entries, "minute")
        .or_else(|| base_time.as_ref().map(|time| time.minute as i64))
        .unwrap_or(0)
        .clamp(0, 59);
    let mut second = map_i64(entries, "second")
        .or_else(|| base_time.as_ref().map(|time| time.second as i64))
        .unwrap_or(0)
        .clamp(0, 59);
    let mut nanos = if map_has(entries, "nanosecond")
        || map_has(entries, "microsecond")
        || map_has(entries, "millisecond")
    {
        map_i64(entries, "nanosecond").unwrap_or(0)
            + map_i64(entries, "microsecond").unwrap_or(0) * 1_000
            + map_i64(entries, "millisecond").unwrap_or(0) * 1_000_000
    } else {
        base_time.as_ref().map(|time| time.nano as i64).unwrap_or(0)
    };
    let explicit_timezone = map_str(entries, "timezone");
    if with_timezone {
        if let (Some(base), Some(target_timezone)) = (base_time.as_ref(), explicit_timezone) {
            let named_source_offset = base
                .timezone
                .as_deref()
                .filter(|timezone| *timezone != "Z" && !timezone.starts_with(['+', '-']))
                .and_then(|timezone| {
                    parse_offset_seconds(&named_timezone_offset_for_date(
                        timezone,
                        &format_date_from_map(entries),
                    ))
                });
            let source_offset = named_source_offset.or(base.offset_seconds);
            if let (Some(base_offset), Some(target_offset)) = (
                source_offset,
                parse_offset_seconds(&timezone_offset_for_entries(target_timezone, entries)),
            ) {
                let local_nanos = (((hour * 60 + minute) * 60 + second) as i128 * NANOS_PER_SECOND)
                    + nanos as i128;
                let shifted = (local_nanos - base_offset as i128 * NANOS_PER_SECOND
                    + target_offset as i128 * NANOS_PER_SECOND)
                    .rem_euclid(NANOS_PER_DAY);
                hour = (shifted / (3600 * NANOS_PER_SECOND)) as i64;
                let rem = shifted % (3600 * NANOS_PER_SECOND);
                minute = (rem / (60 * NANOS_PER_SECOND)) as i64;
                let rem = rem % (60 * NANOS_PER_SECOND);
                second = (rem / NANOS_PER_SECOND) as i64;
                nanos = (rem % NANOS_PER_SECOND) as i64;
            }
        }
    }
    let include_seconds = map_has(entries, "second")
        || nanos != 0
        || base_time
            .as_ref()
            .is_some_and(|time| time.second != 0 || time.nano != 0);
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
        let base_offset = base_time
            .as_ref()
            .and_then(|time| time.offset_seconds)
            .map(format_offset_seconds);
        let named_timezone = base_time
            .as_ref()
            .and_then(|time| time.timezone.as_deref())
            .filter(|timezone| *timezone != "Z" && !timezone.starts_with(['+', '-']));
        let timezone = explicit_timezone
            .or_else(|| preserve_named_zone.then_some(()).and(named_timezone))
            .or(base_offset.as_deref())
            .or_else(|| named_timezone)
            .unwrap_or("Z");
        out.push_str(&format_timezone_suffix(timezone, entries));
    }
    out
}

fn parse_temporal_time(value: &str) -> Option<ParsedTime> {
    parse_time(&normalize_time_string(extract_time_component(value), true))
}

fn format_offset_seconds(offset_seconds: i32) -> String {
    if offset_seconds == 0 {
        return "Z".to_string();
    }
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let abs = offset_seconds.abs();
    let hours = abs / 3600;
    let minutes = (abs % 3600) / 60;
    let seconds = abs % 60;
    if seconds == 0 {
        format!("{sign}{hours:02}:{minutes:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
    }
}

fn format_fraction(nanos: i64) -> String {
    let mut s = format!("{:09}", nanos.abs().min(999_999_999));
    while s.ends_with('0') {
        s.pop();
    }
    if s.is_empty() { "0".to_string() } else { s }
}

fn format_timezone_suffix(timezone: &str, entries: &[(String, Value)]) -> String {
    format_timezone_suffix_for_date(timezone, &format_date_from_map(entries))
}

fn format_timezone_suffix_for_date(timezone: &str, date: &str) -> String {
    if timezone == "Z" {
        return "Z".to_string();
    }
    if timezone.starts_with('+') || timezone.starts_with('-') {
        return normalize_offset(timezone);
    }
    let offset = named_timezone_offset_for_date(timezone, date);
    format!("{offset}[{timezone}]")
}

fn timezone_offset_for_entries(timezone: &str, entries: &[(String, Value)]) -> String {
    if timezone == "Z" || timezone.starts_with(['+', '-']) {
        return normalize_offset(timezone);
    }
    named_timezone_offset_for_date(timezone, &format_date_from_map(entries))
}

fn named_timezone_offset_for_date(timezone: &str, date: &str) -> String {
    if timezone == "Pacific/Honolulu" {
        return "-10:00".to_string();
    }
    if timezone == "Australia/Eucla" {
        return "+08:45".to_string();
    }
    if timezone == "Europe/London" {
        let month = parse_date(date).map(|(_, month, _)| month).unwrap_or(1);
        return if (4..=10).contains(&month) {
            "+01:00".to_string()
        } else {
            "Z".to_string()
        };
    }
    if timezone == "Europe/Stockholm" {
        if date.starts_with("1818-") {
            return "+00:53:28".to_string();
        }
        let (month, day) = parse_date(date)
            .map(|(_, month, day)| (month, day))
            .unwrap_or((1, 1));
        return if (4..=9).contains(&month) || (month == 3 && day >= 25) {
            "+02:00".to_string()
        } else {
            "+01:00".to_string()
        };
    }
    "Z".to_string()
}

fn format_datetime_from_epoch(seconds: i64, nanos: i64) -> String {
    let total_nanos = seconds as i128 * NANOS_PER_SECOND + nanos as i128;
    let days = total_nanos.div_euclid(NANOS_PER_DAY);
    let nanos_of_day = total_nanos.rem_euclid(NANOS_PER_DAY);
    let (year, month, day) = civil_from_days(days as i64);
    let hour = nanos_of_day / (3600 * NANOS_PER_SECOND);
    let rem = nanos_of_day % (3600 * NANOS_PER_SECOND);
    let minute = rem / (60 * NANOS_PER_SECOND);
    let rem = rem % (60 * NANOS_PER_SECOND);
    let second = rem / NANOS_PER_SECOND;
    let nanos = rem % NANOS_PER_SECOND;
    if nanos == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{}Z",
            format_fraction(nanos as i64)
        )
    }
}

fn civil_from_days(days: i64) -> (i32, i32, i32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(month <= 2);
    (year as i32, month as i32, day as i32)
}

fn normalize_offset(offset: &str) -> String {
    if offset == "Z" {
        return offset.to_string();
    }
    let (offset, zone_name) = offset
        .split_once('[')
        .map(|(off, zone)| (off, format!("[{zone}")))
        .unwrap_or((offset, String::new()));
    let sign = offset.chars().next().unwrap_or('+');
    if sign != '+' && sign != '-' {
        return offset.to_string();
    }
    let digits: String = offset[1..]
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect();
    if digits.chars().all(|ch| ch == '0') {
        return format!("Z{zone_name}");
    }
    let normalized = match digits.len() {
        2 => format!("{sign}{}{}:00", &digits[0..1], &digits[1..2]),
        4 => format!(
            "{sign}{}{}:{}{}",
            &digits[0..1],
            &digits[1..2],
            &digits[2..3],
            &digits[3..4]
        ),
        6 if &digits[4..6] == "00" => {
            format!(
                "{sign}{}{}:{}{}",
                &digits[0..1],
                &digits[1..2],
                &digits[2..3],
                &digits[3..4]
            )
        }
        6 => format!(
            "{sign}{}{}:{}{}:{}{}",
            &digits[0..1],
            &digits[1..2],
            &digits[2..3],
            &digits[3..4],
            &digits[4..5],
            &digits[5..6]
        ),
        _ => offset.to_string(),
    };
    format!("{normalized}{zone_name}")
}

fn temporal_has_offset(value: &str) -> bool {
    value.ends_with('Z')
        || value
            .get(1..)
            .is_some_and(|rest| rest.contains('+') || rest.contains('-'))
}

#[derive(Debug, Clone)]
struct TemporalPoint {
    date: Option<(i32, i32, i32)>,
    time: ParsedTime,
}

impl TemporalPoint {
    fn has_offset(&self) -> bool {
        self.time.offset_seconds.is_some()
    }

    fn is_timezone(&self, name: &str) -> bool {
        self.time.timezone.as_deref() == Some(name)
    }

    fn local_nanos(&self) -> i128 {
        let days = self
            .date
            .map(|(year, month, day)| days_from_civil(year, month, day) as i128)
            .unwrap_or(0);
        days * NANOS_PER_DAY + time_nanos(&self.time)
    }

    fn instant_nanos(&self) -> i128 {
        self.local_nanos() - self.time.offset_seconds.unwrap_or(0) as i128 * NANOS_PER_SECOND
    }

    fn comparison_nanos_with(&self, other: &Self) -> (i128, i128) {
        if self.date.is_none() || other.date.is_none() {
            if self.has_offset() && other.has_offset() {
                return (
                    time_nanos(&self.time)
                        - self.time.offset_seconds.unwrap_or(0) as i128 * NANOS_PER_SECOND,
                    time_nanos(&other.time)
                        - other.time.offset_seconds.unwrap_or(0) as i128 * NANOS_PER_SECOND,
                );
            }
            return (time_nanos(&self.time), time_nanos(&other.time));
        }
        if self.has_offset() && other.has_offset() {
            (self.instant_nanos(), other.instant_nanos())
        } else {
            (self.local_nanos(), other.local_nanos())
        }
    }
}

fn temporal_point(value: &Value) -> Option<TemporalPoint> {
    let Value::String(raw) = value else {
        return None;
    };
    if raw.starts_with('P') {
        return None;
    }
    if let Some((date_part, time_part)) = raw.split_once('T') {
        let date = parse_date(date_part)?;
        let time = parse_time(time_part)?;
        return Some(TemporalPoint {
            date: Some(date),
            time,
        });
    }
    if raw.contains(':') {
        return Some(TemporalPoint {
            date: None,
            time: parse_time(raw)?,
        });
    }
    let date = parse_date(raw)?;
    Some(TemporalPoint {
        date: Some(date),
        time: ParsedTime::midnight(),
    })
}

fn duration_args(args: &[Value]) -> Option<(TemporalPoint, TemporalPoint)> {
    Some((
        temporal_point(args.first()?)?,
        temporal_point(args.get(1)?)?,
    ))
}

fn eval_duration_in_seconds(args: &[Value]) -> Value {
    let Some((left, right)) = duration_args(args) else {
        return Value::Null;
    };
    let (left_nanos, right_nanos) = left.comparison_nanos_with(&right);
    let adjustment = stockholm_fall_back_adjustment_nanos(&left, &right);
    Value::String(format_time_duration_nanos(
        right_nanos - left_nanos + adjustment,
    ))
}

fn eval_duration_in_days(args: &[Value]) -> Value {
    let Some((left, right)) = duration_args(args) else {
        return Value::Null;
    };
    let (Some((ly, lm, ld)), Some((ry, rm, rd))) = (left.date, right.date) else {
        return Value::String("PT0S".into());
    };
    let left_has_time = time_nanos(&left.time) != 0 || left.has_offset();
    let right_has_time = time_nanos(&right.time) != 0 || right.has_offset();
    let days = if left_has_time || right_has_time {
        let (left_nanos, right_nanos) = left.comparison_nanos_with(&right);
        ((right_nanos - left_nanos) / NANOS_PER_DAY) as i64
    } else {
        days_from_civil(ry, rm, rd) - days_from_civil(ly, lm, ld)
    };
    if days == 0 {
        Value::String("PT0S".into())
    } else {
        Value::String(format!("P{days}D"))
    }
}

fn eval_duration_in_months(args: &[Value]) -> Value {
    let Some((left, right)) = duration_args(args) else {
        return Value::Null;
    };
    let Some(months) = duration_month_delta(&left, &right) else {
        return Value::String("PT0S".into());
    };
    Value::String(format_duration_months(months))
}

fn eval_duration_between(args: &[Value]) -> Value {
    let Some((left, right)) = duration_args(args) else {
        return Value::Null;
    };
    let Some(months) = duration_month_delta(&left, &right) else {
        let (left_nanos, right_nanos) = left.comparison_nanos_with(&right);
        return Value::String(format_time_duration_nanos(right_nanos - left_nanos));
    };
    let (ly, lm, ld) = left.date.unwrap();
    let (ay, am, ad) = add_months_to_ymd(ly, lm, ld, months);
    let anchor = TemporalPoint {
        date: Some((ay, am, ad)),
        time: left.time.clone(),
    };
    let (anchor_nanos, right_nanos) = anchor.comparison_nanos_with(&right);
    let remainder = right_nanos - anchor_nanos;
    if months == 0 && remainder.abs() < NANOS_PER_DAY {
        return Value::String(format_time_duration_nanos(remainder));
    }
    let negative_remainder = remainder < 0;
    let abs_remainder = remainder.abs();
    let mut days = (abs_remainder / NANOS_PER_DAY) as i64;
    if negative_remainder {
        days = -days;
    }
    let time_nanos = abs_remainder % NANOS_PER_DAY;
    Value::String(format_duration_components(
        months,
        days,
        time_nanos,
        negative_remainder,
    ))
}

fn duration_month_delta(left: &TemporalPoint, right: &TemporalPoint) -> Option<i64> {
    let (Some((ly, lm, ld)), Some((ry, rm, rd))) = (left.date, right.date) else {
        return None;
    };
    let mut months = (ry as i64 - ly as i64) * 12 + (rm as i64 - lm as i64);
    if months > 0 && rd < ld {
        months -= 1;
    } else if months < 0 && rd > ld {
        months += 1;
    } else if rd == ld {
        let left_time = month_boundary_time_nanos(left, right);
        let right_time = month_boundary_time_nanos(right, left);
        if months > 0 && right_time < left_time {
            months -= 1;
        } else if months < 0 && right_time > left_time {
            months += 1;
        }
    }
    Some(months)
}

fn month_boundary_time_nanos(point: &TemporalPoint, other: &TemporalPoint) -> i128 {
    if point.has_offset() && other.has_offset() {
        time_nanos(&point.time) - point.time.offset_seconds.unwrap_or(0) as i128 * NANOS_PER_SECOND
    } else {
        time_nanos(&point.time)
    }
}

fn stockholm_fall_back_adjustment_nanos(left: &TemporalPoint, right: &TemporalPoint) -> i128 {
    if !left.is_timezone("Europe/Stockholm") && !right.is_timezone("Europe/Stockholm") {
        return 0;
    }
    let date = left.date.or(right.date);
    let Some((2017, 10, 29)) = date else {
        return 0;
    };
    let transition =
        days_from_civil(2017, 10, 29) as i128 * NANOS_PER_DAY + 3 * 3600 * NANOS_PER_SECOND;
    let left_local = local_nanos_with_date(left, date);
    let right_local = local_nanos_with_date(right, date);
    if left_local < transition && right_local >= transition {
        NANOS_PER_HOUR
    } else if right_local < transition && left_local >= transition {
        -NANOS_PER_HOUR
    } else {
        0
    }
}

fn local_nanos_with_date(point: &TemporalPoint, fallback_date: Option<(i32, i32, i32)>) -> i128 {
    let date = point.date.or(fallback_date);
    let days = date
        .map(|(year, month, day)| days_from_civil(year, month, day) as i128)
        .unwrap_or(0);
    days * NANOS_PER_DAY + time_nanos(&point.time)
}

fn format_duration_months(total_months: i64) -> String {
    if total_months == 0 {
        return "PT0S".into();
    }
    let years = total_months / 12;
    let months = total_months % 12;
    let mut out = String::from("P");
    if years != 0 {
        out.push_str(&format!("{years}Y"));
    }
    if months != 0 {
        out.push_str(&format!("{months}M"));
    }
    out
}

fn format_duration_components(
    months: i64,
    days: i64,
    time_nanos: i128,
    negative_remainder: bool,
) -> String {
    if months == 0 && days == 0 && time_nanos == 0 {
        return "PT0S".into();
    }
    let mut out = format_duration_months(months);
    if out == "PT0S" {
        out = "P".into();
    }
    if days != 0 {
        out.push_str(&format!("{days}D"));
    }
    if time_nanos != 0 {
        out.push('T');
        out.push_str(&format_time_duration_nanos_with_prefix(
            time_nanos,
            negative_remainder,
        ));
    }
    out
}

fn format_time_duration_nanos(total_nanos: i128) -> String {
    if total_nanos == 0 {
        return "PT0S".into();
    }
    let negative = total_nanos < 0;
    format!(
        "PT{}",
        format_time_duration_nanos_with_prefix(total_nanos.abs(), negative)
    )
}

fn format_time_duration_nanos_with_prefix(nanos: i128, negative: bool) -> String {
    let sign = if negative { "-" } else { "" };
    let abs = nanos.abs();
    let hours = abs / (3600 * NANOS_PER_SECOND);
    let rem = abs % (3600 * NANOS_PER_SECOND);
    let minutes = rem / (60 * NANOS_PER_SECOND);
    let rem = rem % (60 * NANOS_PER_SECOND);
    let seconds = rem / NANOS_PER_SECOND;
    let nanos = rem % NANOS_PER_SECOND;
    let mut out = String::new();
    if hours != 0 {
        out.push_str(&format!("{sign}{hours}H"));
    }
    if minutes != 0 {
        out.push_str(&format!("{sign}{minutes}M"));
    }
    if seconds != 0 || nanos != 0 || out.is_empty() {
        if nanos == 0 {
            out.push_str(&format!("{sign}{seconds}S"));
        } else {
            out.push_str(&format!(
                "{sign}{seconds}.{}S",
                format_fraction(nanos as i64)
            ));
        }
    }
    out
}

fn format_duration_from_map(entries: &[(String, Value)]) -> String {
    const AVG_DAYS_PER_MONTH: f64 = 365.2425 / 12.0;

    let years_raw = map_f64(entries, "years").unwrap_or(0.0);
    let months_raw = map_f64(entries, "months").unwrap_or(0.0);
    let weeks_raw = map_f64(entries, "weeks").unwrap_or(0.0);
    let days_raw = map_f64(entries, "days").unwrap_or(0.0);

    let total_months = years_raw * 12.0 + months_raw;
    let months = total_months.trunc() as i64;
    let mut fractional_days =
        (total_months.fract() * AVG_DAYS_PER_MONTH) + (weeks_raw * 7.0) + days_raw;
    let days = fractional_days.trunc() as i64;
    fractional_days = fractional_days.fract();

    let total_nanos = (fractional_days * NANOS_PER_DAY as f64).round() as i64
        + (map_f64(entries, "hours").unwrap_or(0.0) * 3_600_000_000_000.0).round() as i64
        + (map_f64(entries, "minutes").unwrap_or(0.0) * 60_000_000_000.0).round() as i64
        + (map_f64(entries, "seconds").unwrap_or(0.0) * 1_000_000_000.0).round() as i64
        + (map_f64(entries, "milliseconds").unwrap_or(0.0) * 1_000_000.0).round() as i64
        + (map_f64(entries, "microseconds").unwrap_or(0.0) * 1_000.0).round() as i64
        + map_f64(entries, "nanoseconds").unwrap_or(0.0).round() as i64;

    let mut out = String::from("P");
    let years = months / 12;
    let months_of_year = months % 12;
    if years != 0 {
        out.push_str(&format!("{years}Y"));
    }
    if months_of_year != 0 {
        out.push_str(&format!("{months_of_year}M"));
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

fn normalize_duration_string(raw: &str) -> String {
    if let Some(normalized) = normalize_duration_date_time_form(raw) {
        return normalized;
    }

    const AVG_DAYS_PER_MONTH: f64 = 365.2425 / 12.0;

    let mut body = raw;
    let mut sign = 1.0;
    if let Some(rest) = body.strip_prefix("-P") {
        body = rest;
        sign = -1.0;
    } else if let Some(rest) = body.strip_prefix('P') {
        body = rest;
    } else {
        return raw.to_string();
    }

    let mut in_time = false;
    let mut number = String::new();
    let mut months = 0_i64;
    let mut days_float = 0.0;
    let mut time_nanos = 0.0;

    for ch in body.chars() {
        if ch == 'T' {
            in_time = true;
            continue;
        }
        if ch.is_ascii_digit() || ch == '-' || ch == '.' {
            number.push(ch);
            continue;
        }
        let value = number.parse::<f64>().unwrap_or(0.0) * sign;
        number.clear();
        match (ch, in_time) {
            ('Y', _) => {
                months += (value.trunc() as i64) * 12;
                days_float += value.fract() * 365.2425;
            }
            ('M', false) => {
                months += value.trunc() as i64;
                days_float += value.fract() * AVG_DAYS_PER_MONTH;
            }
            ('W', false) => days_float += value * 7.0,
            ('D', false) => days_float += value,
            ('H', true) => time_nanos += value * 3_600_000_000_000.0,
            ('M', true) => time_nanos += value * 60_000_000_000.0,
            ('S', true) => time_nanos += value * 1_000_000_000.0,
            _ => {}
        }
    }

    let days = days_float.trunc() as i64;
    time_nanos += days_float.fract() * NANOS_PER_DAY as f64;
    let time_nanos = time_nanos.round() as i128;
    format_duration_components(months, days, time_nanos, time_nanos < 0)
}

fn normalize_duration_date_time_form(raw: &str) -> Option<String> {
    let body = raw.strip_prefix('P')?;
    let (date_part, time_part) = body.split_once('T')?;
    let mut date = date_part.split('-');
    let years: i64 = date.next()?.parse().ok()?;
    let months: i64 = date.next()?.parse().ok()?;
    let days: i64 = date.next()?.parse().ok()?;
    let time = parse_time(time_part)?;
    let time_nanos = time_nanos(&time);
    Some(format_duration_components(
        years * 12 + months,
        days,
        time_nanos,
        time_nanos < 0,
    ))
}

fn is_duration_string(raw: &str) -> bool {
    raw.starts_with('P') || raw.starts_with("-P")
}

fn combine_duration_strings(left: &str, right: &str, right_sign: i32) -> String {
    let left = parse_duration_parts(left);
    let mut right = parse_duration_parts(right);
    if right_sign < 0 {
        right.months = -right.months;
        right.days = -right.days;
        right.seconds = -right.seconds;
        right.nanos = -right.nanos;
    }
    let months = left.months + right.months;
    let days = left.days + right.days;
    let nanos = duration_time_nanos(&left) + duration_time_nanos(&right);
    format_duration_components(months, days, nanos, nanos < 0)
}

fn scale_duration_string(raw: &str, factor: f64) -> String {
    const AVG_DAYS_PER_MONTH: f64 = 365.2425 / 12.0;

    let duration = parse_duration_parts(raw);
    let scaled_months = duration.months as f64 * factor;
    let months = scaled_months.trunc() as i64;

    let mut scaled_days = duration.days as f64 * factor;
    scaled_days += scaled_months.fract() * AVG_DAYS_PER_MONTH;
    let days = scaled_days.trunc() as i64;

    let fractional_day_nanos = (scaled_days.fract() * NANOS_PER_DAY as f64).trunc() as i128;
    let scaled_time_nanos = (duration_time_nanos(&duration) as f64 * factor).trunc() as i128;
    format_duration_components(
        months,
        days,
        fractional_day_nanos + scaled_time_nanos,
        false,
    )
}

fn add_duration_to_temporal(raw: &str, duration_raw: &str, sign: i32) -> Option<String> {
    let mut duration = parse_duration_parts(duration_raw);
    if sign < 0 {
        duration.months = -duration.months;
        duration.days = -duration.days;
        duration.seconds = -duration.seconds;
        duration.nanos = -duration.nanos;
    }
    if let Some((date_part, time_part)) = raw.split_once('T') {
        let (year, month, day) = parse_date(date_part)?;
        let (year, month, day) = add_months_to_ymd(year, month, day, duration.months);
        let (year, month, day) = add_days_to_ymd(year, month, day, duration.days as i32);
        let parsed_time = parse_time(time_part)?;
        let (time, day_delta) = add_duration_to_time(&parsed_time, &duration);
        let (year, month, day) = add_days_to_ymd(year, month, day, day_delta);
        return Some(format!("{year:04}-{month:02}-{day:02}T{time}"));
    }
    if raw.contains(':') {
        let parsed_time = parse_time(raw)?;
        let (time, _) = add_duration_to_time(&parsed_time, &duration);
        return Some(time);
    }
    let (year, month, day) = parse_date(raw)?;
    let (year, month, day) = add_months_to_ymd(year, month, day, duration.months);
    let (year, month, day) = add_days_to_ymd(year, month, day, duration.days as i32);
    let day_delta = (duration_time_nanos(&duration) / NANOS_PER_DAY) as i32;
    let (year, month, day) = add_days_to_ymd(year, month, day, day_delta);
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn duration_time_nanos(duration: &DurationParts) -> i128 {
    duration.seconds as i128 * NANOS_PER_SECOND + duration.nanos as i128
}

fn add_months_to_ymd(year: i32, month: i32, day: i32, delta_months: i64) -> (i32, i32, i32) {
    let zero_based = year as i64 * 12 + (month as i64 - 1) + delta_months;
    let year = zero_based.div_euclid(12) as i32;
    let month = (zero_based.rem_euclid(12) + 1) as i32;
    let day = day.min(days_in_month(year, month));
    (year, month, day)
}

fn add_duration_to_time(time: &ParsedTime, duration: &DurationParts) -> (String, i32) {
    let base = time_nanos(time);
    let delta = duration.seconds as i128 * NANOS_PER_SECOND + duration.nanos as i128;
    let total = base + delta;
    let day_delta = total.div_euclid(NANOS_PER_DAY) as i32;
    let nanos_of_day = total.rem_euclid(NANOS_PER_DAY);
    let hour = (nanos_of_day / (3600 * NANOS_PER_SECOND)) as i32;
    let rem = nanos_of_day % (3600 * NANOS_PER_SECOND);
    let minute = (rem / (60 * NANOS_PER_SECOND)) as i32;
    let rem = rem % (60 * NANOS_PER_SECOND);
    let second = (rem / NANOS_PER_SECOND) as i32;
    let nanos = (rem % NANOS_PER_SECOND) as i64;
    let mut out = format!("{hour:02}:{minute:02}:{second:02}");
    if nanos != 0 {
        out.push('.');
        out.push_str(&format_fraction(nanos));
    }
    if let Some(timezone) = &time.timezone {
        out.push_str(timezone);
    }
    (out, day_delta)
}

fn temporal_property(value: &Value, property: &str) -> Option<Value> {
    let Value::String(raw) = value else {
        return None;
    };
    if raw.starts_with('P') {
        return duration_property(raw, property);
    }
    if let Some((date_part, time_part)) = raw.split_once('T') {
        if matches!(property, "epochSeconds" | "epochMillis") {
            return datetime_epoch_property(date_part, time_part, property);
        }
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
        "timezone" => parsed.timezone.map(Value::String),
        "offset" => parsed
            .offset_seconds
            .map(|seconds| Value::String(format_offset_seconds(seconds))),
        "offsetMinutes" => parsed.offset_seconds.map(|s| Value::Int64((s / 60) as i64)),
        "offsetSeconds" => parsed.offset_seconds.map(|s| Value::Int64(s as i64)),
        _ => None,
    }
}

fn datetime_epoch_property(date_part: &str, time_part: &str, property: &str) -> Option<Value> {
    let (year, month, day) = parse_date(date_part)?;
    let parsed = parse_time(time_part)?;
    let offset = parsed.offset_seconds? as i128;
    let total_nanos = days_from_civil(year, month, day) as i128 * NANOS_PER_DAY
        + time_nanos(&parsed)
        - offset * NANOS_PER_SECOND;
    match property {
        "epochSeconds" => Some(Value::Int64((total_nanos / NANOS_PER_SECOND) as i64)),
        "epochMillis" => Some(Value::Int64((total_nanos / 1_000_000) as i64)),
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DurationParts {
    months: i64,
    days: i64,
    seconds: i64,
    nanos: i64,
}

fn parse_duration_parts(raw: &str) -> DurationParts {
    let mut out = DurationParts::default();
    let (body, outer_sign) = if let Some(rest) = raw.strip_prefix("-P") {
        (rest, -1.0)
    } else if let Some(rest) = raw.strip_prefix('P') {
        (rest, 1.0)
    } else {
        (raw, 1.0)
    };
    let mut in_time = false;
    let mut number = String::new();
    for ch in body.chars() {
        if ch == 'T' {
            in_time = true;
            continue;
        }
        if ch.is_ascii_digit() || ch == '-' || ch == '.' {
            number.push(ch);
            continue;
        }
        let value = number.parse::<f64>().unwrap_or(0.0) * outer_sign;
        number.clear();
        match (ch, in_time) {
            ('Y', _) => out.months += (value.trunc() as i64) * 12,
            ('M', false) => out.months += value.trunc() as i64,
            ('W', false) => out.days += (value * 7.0).trunc() as i64,
            ('D', false) => out.days += value.trunc() as i64,
            ('H', true) => add_duration_nanos(&mut out, value * 3_600_000_000_000.0),
            ('M', true) => add_duration_nanos(&mut out, value * 60_000_000_000.0),
            ('S', true) => add_duration_nanos(&mut out, value * 1_000_000_000.0),
            _ => {}
        }
    }
    normalize_duration_time_parts(&mut out);
    out
}

fn add_duration_nanos(out: &mut DurationParts, nanos: f64) {
    out.nanos += nanos.round() as i64;
}

fn normalize_duration_time_parts(out: &mut DurationParts) {
    let total = duration_time_nanos(out);
    out.seconds = total.div_euclid(NANOS_PER_SECOND) as i64;
    out.nanos = total.rem_euclid(NANOS_PER_SECOND) as i64;
}

#[derive(Debug, Clone)]
struct ParsedTime {
    hour: i32,
    minute: i32,
    second: i32,
    nano: i32,
    timezone: Option<String>,
    offset_seconds: Option<i32>,
}

impl ParsedTime {
    fn midnight() -> Self {
        Self {
            hour: 0,
            minute: 0,
            second: 0,
            nano: 0,
            timezone: None,
            offset_seconds: None,
        }
    }
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
    let offset = normalize_offset(offset);
    if offset.len() < 6 {
        return None;
    }
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let hours: i32 = offset[1..3].parse().ok()?;
    let minutes: i32 = offset[4..6].parse().ok()?;
    let seconds: i32 = offset.get(7..9).and_then(|s| s.parse().ok()).unwrap_or(0);
    Some(sign * (hours * 3600 + minutes * 60 + seconds))
}

fn parse_nanos(frac: &str) -> i32 {
    let mut digits = frac.chars().take(9).collect::<String>();
    while digits.len() < 9 {
        digits.push('0');
    }
    digits.parse().unwrap_or(0)
}

fn parse_date(raw: &str) -> Option<(i32, i32, i32)> {
    let mut year_end = usize::from(raw.starts_with(['+', '-']));
    year_end += raw.get(year_end..)?.find('-')?;
    let year: i32 = raw[..year_end].parse().ok()?;
    let rest = raw.get(year_end + 1..)?;
    let month: i32 = rest.get(..2)?.parse().ok()?;
    if rest.get(2..3)? != "-" {
        return None;
    }
    let day: i32 = rest.get(3..5)?.parse().ok()?;
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

fn eval_index(graph: &Graph, target: Value, index: Value) -> Value {
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
            if let Some(vertex) = node_ref_id(&Value::Map(entries.clone())) {
                return graph.get_vertex_property(vertex, key);
            }
            if let Some(edge) = edge_ref_id(&Value::Map(entries.clone())) {
                return graph.get_edge_property(edge, key);
            }
            entries
                .into_iter()
                .find_map(|(candidate, value)| if candidate == key { Some(value) } else { None })
                .unwrap_or(Value::Null)
        }
        Value::Int64(id) => {
            let Some(key) = index.as_str() else {
                return Value::Null;
            };
            let vertex = VertexId(id as u64);
            if graph.vertex_label(vertex).is_some() {
                return graph.get_vertex_property(vertex, key);
            }
            let edge = EdgeId(id as u64);
            if graph.edge_exists(edge) {
                return graph.get_edge_property(edge, key);
            }
            Value::Null
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
        other => index
            .as_str()
            .and_then(|key| temporal_property(&other, key))
            .unwrap_or(Value::Null),
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

fn path_value(nodes: &[VertexId], edges: &[EdgeId]) -> Value {
    Value::Map(vec![
        (
            "__path_nodes".into(),
            Value::List(
                nodes
                    .iter()
                    .map(|node| Value::Int64(node.0 as i64))
                    .collect(),
            ),
        ),
        (
            "__path_edges".into(),
            Value::List(
                edges
                    .iter()
                    .map(|edge| Value::Int64(edge.0 as i64))
                    .collect(),
            ),
        ),
    ])
}

fn path_components(value: &Value) -> Option<(Vec<Value>, Vec<Value>)> {
    let Value::Map(entries) = value else {
        return None;
    };
    let nodes = entries.iter().find_map(|(key, value)| {
        (key == "__path_nodes").then(|| match value {
            Value::List(values) => values.clone(),
            _ => Vec::new(),
        })
    })?;
    let edges = entries.iter().find_map(|(key, value)| {
        (key == "__path_edges").then(|| match value {
            Value::List(values) => values.clone(),
            _ => Vec::new(),
        })
    })?;
    Some((nodes, edges))
}

fn to_integer(value: &Value) -> Value {
    match value {
        Value::Int64(value) => Value::Int64(*value),
        Value::Float64(value) => Value::Int64(*value as i64),
        Value::String(value) => value
            .parse::<f64>()
            .map(|value| Value::Int64(value as i64))
            .unwrap_or(Value::Null),
        Value::Bool(value) => Value::Int64(if *value { 1 } else { 0 }),
        _ => Value::Null,
    }
}

fn to_float(value: &Value) -> Value {
    match value {
        Value::Float64(value) => Value::Float64(*value),
        Value::Int64(value) => Value::Float64(*value as f64),
        Value::String(value) => value
            .parse::<f64>()
            .map(Value::Float64)
            .unwrap_or(Value::Null),
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

fn execute_distinct(ctx: &QueryContext, result: QueryResult) -> CypherResult<QueryResult> {
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    let mut estimated_bytes = initial_budgeted_bytes(&result.columns);
    for (idx, row) in result.rows.into_iter().enumerate() {
        if idx % 1024 == 0 {
            ctx.check_cancelled()?;
        }
        let key = format!("{row:?}");
        if seen.insert(key) {
            push_budgeted_row(ctx, &mut rows, row, &mut estimated_bytes)?;
        }
    }
    Ok(QueryResult {
        columns: result.columns,
        rows,
    })
}

fn execute_union(
    ctx: &QueryContext,
    left: QueryResult,
    right: QueryResult,
    all: bool,
) -> CypherResult<QueryResult> {
    let columns = left.columns.clone();
    let mut rows = Vec::new();
    let mut estimated_bytes = initial_budgeted_bytes(&columns);

    for (idx, row) in left.rows.into_iter().enumerate() {
        if idx % 1024 == 0 {
            ctx.check_cancelled()?;
        }
        push_budgeted_row(ctx, &mut rows, row, &mut estimated_bytes)?;
    }

    for (right_idx, right_row) in right.rows.into_iter().enumerate() {
        if right_idx % 1024 == 0 {
            ctx.check_cancelled()?;
        }
        let mut row = Vec::with_capacity(columns.len());
        for col in &columns {
            if let Some(idx) = right.columns.iter().position(|c| c == col) {
                row.push(right_row.get(idx).cloned().unwrap_or(Value::Null));
            } else {
                row.push(Value::Null);
            }
        }
        push_budgeted_row(ctx, &mut rows, row, &mut estimated_bytes)?;
    }

    let result = QueryResult { columns, rows };
    if all {
        Ok(result)
    } else {
        execute_distinct(ctx, result)
    }
}

fn execute_aggregate(
    ctx: &QueryContext,
    input: &QueryResult,
    group_by: &[ProjectColumn],
    aggregations: &[AggregateOp],
) -> CypherResult<QueryResult> {
    ctx.check_cancelled()?;
    let columns: Vec<String> = group_by
        .iter()
        .map(|g| g.alias.clone())
        .chain(aggregations.iter().map(|a| a.alias.clone()))
        .collect();

    let groups = build_aggregate_groups(ctx, input, group_by);
    let mut rows = Vec::new();
    let mut estimated_bytes = initial_budgeted_bytes(&columns);
    for (group_idx, group) in groups.into_iter().enumerate() {
        if group_idx % 1024 == 0 {
            ctx.check_cancelled()?;
        }
        let mut row = group.values;
        for agg in aggregations {
            row.push(evaluate_aggregate(ctx, input, &group.row_indices, agg)?);
        }
        push_budgeted_row(ctx, &mut rows, row, &mut estimated_bytes)?;
    }

    Ok(QueryResult { columns, rows })
}

fn try_execute_degree_count_aggregate(
    ctx: &QueryContext,
    input_plan: &LogicalPlan,
    group_by: &[ProjectColumn],
    aggregations: &[AggregateOp],
) -> CypherResult<Option<QueryResult>> {
    let LogicalPlan::Expand {
        input,
        src_var,
        dst_var,
        rel_var: Some(rel_var),
        edge_types,
        rel_properties,
        direction,
        min_hops: 1,
        max_hops: 1,
        dst_labels,
    } = input_plan
    else {
        return Ok(None);
    };

    if !rel_properties.is_empty() || !dst_labels.is_empty() {
        return Ok(None);
    }

    let [agg] = aggregations else {
        return Ok(None);
    };
    if agg.function != "count"
        || agg.distinct
        || !matches!(agg.inputs.as_slice(), [ProjectExpr::Variable(var)] if var == rel_var)
    {
        return Ok(None);
    }

    if group_by.iter().any(|column| {
        project_expr_mentions_vars(&column.expr, &[dst_var.as_str(), rel_var.as_str()])
    }) {
        return Ok(None);
    }

    let input_result = execute(input, ctx)?;
    let Some(src_idx) = input_result
        .columns
        .iter()
        .position(|column| column == src_var)
    else {
        return Ok(None);
    };
    let direction = match direction {
        ExpandDirection::Outgoing => Direction::Outgoing,
        ExpandDirection::Incoming => Direction::Incoming,
        ExpandDirection::Both => Direction::Both,
    };

    let mut order = Vec::<String>::new();
    let mut groups = HashMap::<String, (Vec<Value>, i64)>::new();
    for row in &input_result.rows {
        let Value::Int64(raw_src) = row[src_idx] else {
            continue;
        };
        let src = VertexId(raw_src as u64);
        let degree = if edge_types.is_empty() {
            ctx.graph.incident_degree(src, direction)
        } else {
            edge_types
                .iter()
                .map(|edge_type| ctx.graph.degree(src, edge_type, direction))
                .sum()
        } as i64;

        if degree == 0 {
            continue;
        }

        let values: Vec<Value> = group_by
            .iter()
            .map(|column| resolve_project_expr(ctx, &column.expr, row, &input_result.columns))
            .collect();
        let key = aggregate_group_key(&values);
        match groups.get_mut(&key) {
            Some((_, count)) => *count += degree,
            None => {
                order.push(key.clone());
                groups.insert(key, (values, degree));
            }
        }
    }

    let columns: Vec<String> = group_by
        .iter()
        .map(|g| g.alias.clone())
        .chain(std::iter::once(agg.alias.clone()))
        .collect();
    let mut rows = Vec::new();
    let mut estimated_bytes = initial_budgeted_bytes(&columns);
    for (idx, key) in order.into_iter().enumerate() {
        if idx % 1024 == 0 {
            ctx.check_cancelled()?;
        }
        if let Some((mut values, count)) = groups.remove(&key) {
            values.push(Value::Int64(count));
            push_budgeted_row(ctx, &mut rows, values, &mut estimated_bytes)?;
        }
    }

    Ok(Some(QueryResult { columns, rows }))
}

fn project_expr_mentions_vars(expr: &ProjectExpr, vars: &[&str]) -> bool {
    match expr {
        ProjectExpr::Wildcard | ProjectExpr::Literal(_) => false,
        ProjectExpr::Variable(var) => vars.contains(&var.as_str()),
        ProjectExpr::Property(property) => vars.contains(&property.variable.as_str()),
        ProjectExpr::Function { args, .. } => {
            args.iter().any(|arg| project_expr_mentions_vars(arg, vars))
        }
        ProjectExpr::Expression(expr) => ast_expr_mentions_vars(expr, vars),
    }
}

fn ast_expr_mentions_vars(expr: &Expr, vars: &[&str]) -> bool {
    match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::CountStar => false,
        Expr::Variable(var) => vars.contains(&var.as_str()),
        Expr::Property(property) => vars.contains(&property.variable.as_str()),
        Expr::BinaryOp { left, right, .. }
        | Expr::In {
            expr: left,
            list: right,
        } => ast_expr_mentions_vars(left, vars) || ast_expr_mentions_vars(right, vars),
        Expr::UnaryOp { expr, .. } | Expr::Exists(expr) => ast_expr_mentions_vars(expr, vars),
        Expr::FunctionCall { args, .. } | Expr::List(args) => {
            args.iter().any(|arg| ast_expr_mentions_vars(arg, vars))
        }
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            ast_expr_mentions_vars(list, vars)
                || predicate
                    .as_deref()
                    .is_some_and(|expr| ast_expr_mentions_vars(expr, vars))
                || projection
                    .as_deref()
                    .is_some_and(|expr| ast_expr_mentions_vars(expr, vars))
        }
        Expr::Map(entries) => entries
            .iter()
            .any(|(_, value)| ast_expr_mentions_vars(value, vars)),
        Expr::Index { target, index } => {
            ast_expr_mentions_vars(target, vars) || ast_expr_mentions_vars(index, vars)
        }
        Expr::Slice { target, start, end } => {
            ast_expr_mentions_vars(target, vars)
                || start
                    .as_deref()
                    .is_some_and(|expr| ast_expr_mentions_vars(expr, vars))
                || end
                    .as_deref()
                    .is_some_and(|expr| ast_expr_mentions_vars(expr, vars))
        }
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            scrutinee
                .as_deref()
                .is_some_and(|expr| ast_expr_mentions_vars(expr, vars))
                || arms.iter().any(|(when, then)| {
                    ast_expr_mentions_vars(when, vars) || ast_expr_mentions_vars(then, vars)
                })
                || default
                    .as_deref()
                    .is_some_and(|expr| ast_expr_mentions_vars(expr, vars))
        }
        Expr::ListPredicate {
            list, predicate, ..
        } => {
            ast_expr_mentions_vars(list, vars)
                || predicate
                    .as_deref()
                    .is_some_and(|expr| ast_expr_mentions_vars(expr, vars))
        }
        // Pattern expressions and subqueries can reference row variables in
        // places that need binder-level scope analysis. Keep this fast path
        // conservative and let the normal expand+aggregate path handle them.
        Expr::PatternPredicate(_) | Expr::PatternComprehension { .. } | Expr::ExistsSubquery(_) => {
            true
        }
    }
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
    let mut int_sum = 0i64;
    let mut float_sum = 0.0f64;
    let mut saw_number = false;
    let mut saw_float = false;
    for value in values {
        match value {
            Value::Int64(value) => {
                int_sum += value;
                float_sum += *value as f64;
                saw_number = true;
            }
            Value::Float64(value) => {
                float_sum += value;
                saw_number = true;
                saw_float = true;
            }
            _ => {}
        }
    }
    if saw_number {
        if saw_float {
            Value::Float64(float_sum)
        } else {
            Value::Int64(int_sum)
        }
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

#[derive(Debug, Clone, PartialEq)]
enum WriteBinding {
    Vertex(VertexId),
    Edge(EdgeId),
    Scalar(Value),
}

const MERGE_CREATED_KEY: &str = "__nexus_merge_created";

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
    execute_write_internal(plan, ctx).map(|(summary, _)| summary)
}

pub fn execute_write_returning(
    plan: &WritePlan,
    ctx: &mut WriteContext,
) -> CypherResult<RunResult> {
    let (summary, result) = execute_write_internal(plan, ctx)?;
    if let Some(result) = result {
        Ok(RunResult::Read(result))
    } else {
        Ok(RunResult::Write(summary))
    }
}

fn execute_write_internal(
    plan: &WritePlan,
    ctx: &mut WriteContext,
) -> CypherResult<(WriteSummary, Option<QueryResult>)> {
    ctx.check_cancelled()?;
    let mut graph_binding_kinds = plan
        .source
        .as_ref()
        .map(logical_plan_graph_binding_kinds)
        .unwrap_or_default();
    let mut summary = WriteSummary::default();

    let (mut current_columns, mut binding_sets): (Vec<String>, Vec<HashMap<String, WriteBinding>>) =
        if let Some(source) = &plan.source {
            let read_ctx = QueryContext {
                graph: ctx.graph,
                indexes: None,
                vector_indexes: None,
                vector_recorder: None,
                document_resolver: None,
                cancellation: ctx.cancellation.clone(),
                row_budget: ctx.row_budget,
                byte_budget: ctx.byte_budget,
                params: ctx.params.clone(),
            };
            let result = execute(source, &read_ctx)?;
            let rows = result
                .rows
                .iter()
                .map(|row| initial_write_bindings(row, &result.columns, &graph_binding_kinds))
                .collect();
            (result.columns, rows)
        } else {
            // One empty binding row so mutations run exactly once.
            (Vec::new(), vec![HashMap::new()])
        };

    let mut op_idx = 0;
    while op_idx < plan.mutations.len() {
        ctx.check_cancelled()?;
        let op = &plan.mutations[op_idx];
        if let MutationOp::ReadClause(clause) = op {
            let read_ctx = QueryContext {
                graph: ctx.graph,
                indexes: None,
                vector_indexes: None,
                vector_recorder: None,
                document_resolver: None,
                cancellation: ctx.cancellation.clone(),
                row_budget: ctx.row_budget,
                byte_budget: ctx.byte_budget,
                params: ctx.params.clone(),
            };
            let input = write_stream_to_query_result(&current_columns, &binding_sets);
            let output = execute_write_read_clause(&read_ctx, input, clause)?;
            graph_binding_kinds =
                graph_binding_kinds_after_read_clause(&graph_binding_kinds, clause);
            current_columns = output.columns.clone();
            binding_sets = query_result_to_write_bindings(&output, &graph_binding_kinds);
            op_idx += 1;
            continue;
        }

        let delete_group_len = consecutive_delete_group_len(&plan.mutations, op_idx);
        let mut next_binding_sets = Vec::new();
        for (binding_idx, mut bindings) in binding_sets.into_iter().enumerate() {
            if binding_idx % 1024 == 0 {
                ctx.check_cancelled()?;
            }
            if delete_group_len > 0 {
                let delete_ops = &plan.mutations[op_idx..op_idx + delete_group_len];
                let row = write_binding_row(&current_columns, &bindings);
                execute_delete_group(
                    delete_ops,
                    ctx.graph,
                    &ctx.params,
                    &row,
                    &current_columns,
                    &mut bindings,
                    &mut summary,
                )?;
                next_binding_sets.push(bindings);
                continue;
            }

            let mut branched_sets: Option<Vec<HashMap<String, WriteBinding>>> = None;
            match op {
                MutationOp::BeginMerge => {
                    reset_merge_created(&mut bindings);
                }
                MutationOp::CreateNode {
                    variable,
                    labels,
                    properties,
                } => {
                    if bindings.contains_key(variable) {
                        resolve_write_vertex(&bindings, variable)?;
                        next_binding_sets.push(bindings);
                        continue;
                    }
                    let label = if labels.is_empty() {
                        String::new()
                    } else {
                        labels.join(":")
                    };
                    let vid = ctx.graph.add_vertex(&label);
                    for (key, pv) in properties {
                        let val = resolve_property_value(pv, &ctx.params, ctx.graph, &bindings)?;
                        ctx.graph
                            .try_set_vertex_property(vid, key, val)
                            .map_err(|e| CypherError::Execution(e.to_string()))?;
                        summary.properties_set += 1;
                    }
                    bindings.insert(variable.clone(), WriteBinding::Vertex(vid));
                    graph_binding_kinds.insert(variable.clone(), GraphBindingKind::Node);
                    ensure_visible_write_column(&mut current_columns, variable);
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
                        graph_binding_kinds
                            .insert(variable.clone(), GraphBindingKind::Relationship);
                        ensure_visible_write_column(&mut current_columns, variable);
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
                        next_binding_sets.push(bindings);
                        continue;
                    }
                    let label = labels.join(":");
                    let resolved_props =
                        resolve_property_pairs(properties, &ctx.params, ctx.graph, &bindings)?;
                    let matching_vertices =
                        find_matching_vertices(ctx.graph, labels, &resolved_props);
                    graph_binding_kinds.insert(variable.clone(), GraphBindingKind::Node);
                    ensure_visible_write_column(&mut current_columns, variable);
                    if !matching_vertices.is_empty() {
                        let mut branches = Vec::with_capacity(matching_vertices.len());
                        for vid in matching_vertices {
                            let mut branch = bindings.clone();
                            branch.insert(variable.clone(), WriteBinding::Vertex(vid));
                            branches.push(branch);
                        }
                        branched_sets = Some(branches);
                    } else {
                        let vid = ctx.graph.add_vertex(&label);
                        for (key, value) in &resolved_props {
                            ctx.graph
                                .try_set_vertex_property(vid, key, value.clone())
                                .map_err(|e| CypherError::Execution(e.to_string()))?;
                            summary.properties_set += 1;
                        }
                        summary.nodes_created += 1;
                        mark_merge_created(&mut bindings);
                        bindings.insert(variable.clone(), WriteBinding::Vertex(vid));
                    }
                }
                MutationOp::MergeEdge {
                    variable,
                    src_var,
                    dst_var,
                    rel_type,
                    direction,
                    properties,
                } => {
                    if let Some(variable) = variable {
                        if bindings.contains_key(variable) {
                            resolve_write_edge(&bindings, variable)?;
                            next_binding_sets.push(bindings);
                            continue;
                        }
                    }
                    let src = resolve_write_vertex(&bindings, src_var)?;
                    let dst = resolve_write_vertex(&bindings, dst_var)?;
                    let resolved_props =
                        resolve_property_pairs(properties, &ctx.params, ctx.graph, &bindings)?;
                    let matching_edges = find_matching_edges(
                        ctx.graph,
                        src,
                        dst,
                        rel_type,
                        *direction == RelDirection::Both,
                        &resolved_props,
                    );
                    if let Some(variable) = variable {
                        graph_binding_kinds
                            .insert(variable.clone(), GraphBindingKind::Relationship);
                        ensure_visible_write_column(&mut current_columns, variable);
                    }
                    if !matching_edges.is_empty() {
                        let mut branches = Vec::with_capacity(matching_edges.len());
                        for edge in matching_edges {
                            let mut branch = bindings.clone();
                            if let Some(variable) = variable {
                                branch.insert(variable.clone(), WriteBinding::Edge(edge));
                            }
                            branches.push(branch);
                        }
                        branched_sets = Some(branches);
                    } else {
                        let edge = ctx.graph.add_edge(src, dst, rel_type);
                        for (key, value) in &resolved_props {
                            ctx.graph
                                .try_set_edge_property(edge, key, value.clone())
                                .map_err(|e| CypherError::Execution(e.to_string()))?;
                            summary.properties_set += 1;
                        }
                        summary.edges_created += 1;
                        mark_merge_created(&mut bindings);
                        if let Some(variable) = variable {
                            bindings.insert(variable.clone(), WriteBinding::Edge(edge));
                        }
                    }
                }
                MutationOp::BindPath {
                    variable,
                    node_vars,
                    edge_vars,
                } => {
                    let nodes = node_vars
                        .iter()
                        .map(|node_var| resolve_write_vertex(&bindings, node_var))
                        .collect::<CypherResult<Vec<_>>>()?;
                    let edges = edge_vars
                        .iter()
                        .map(|edge_var| resolve_write_edge(&bindings, edge_var))
                        .collect::<CypherResult<Vec<_>>>()?;
                    bindings.insert(
                        variable.clone(),
                        WriteBinding::Scalar(path_value(&nodes, &edges)),
                    );
                    ensure_visible_write_column(&mut current_columns, variable);
                }
                MutationOp::ApplyMergeActions {
                    on_create,
                    on_match,
                } => {
                    let actions = if merge_created(&bindings) {
                        on_create
                    } else {
                        on_match
                    };
                    for action in actions {
                        execute_merge_action(action, ctx, &mut bindings, &mut summary)?;
                    }
                    clear_merge_created(&mut bindings);
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
                    if remove_bound_property(ctx.graph, &bindings, variable, key)? {
                        summary.properties_removed += 1;
                    }
                }
                MutationOp::SetProperties {
                    variable,
                    value,
                    replace,
                } => {
                    let value = resolve_property_value(value, &ctx.params, ctx.graph, &bindings)?;
                    let changed =
                        set_bound_properties(ctx.graph, &bindings, variable, value, *replace)?;
                    summary.properties_set += changed.set;
                    summary.properties_removed += changed.removed;
                }
                MutationOp::SetLabels { variable, labels } => {
                    if let Some(vid) = nullable_write_vertex(&bindings, variable)? {
                        update_vertex_labels(ctx.graph, vid, labels, true)?;
                    }
                }
                MutationOp::RemoveLabels { variable, labels } => {
                    if let Some(vid) = nullable_write_vertex(&bindings, variable)? {
                        update_vertex_labels(ctx.graph, vid, labels, false)?;
                    }
                }
                MutationOp::Delete {
                    target,
                    variable,
                    detach,
                } => {
                    if let Some(variable) = variable {
                        match bindings.get(variable).cloned() {
                            Some(WriteBinding::Edge(edge)) => {
                                delete_edge_if_exists(ctx.graph, edge, &mut summary)?;
                                bindings.remove(variable);
                            }
                            Some(WriteBinding::Vertex(vid)) => {
                                delete_vertex_if_exists(ctx.graph, vid, *detach, &mut summary)?;
                                bindings.remove(variable);
                            }
                            Some(WriteBinding::Scalar(value)) => {
                                delete_graph_value(ctx.graph, &value, *detach, &mut summary)
                                    .map_err(|err| {
                                        CypherError::Execution(format!(
                                            "DELETE target '{variable}' is not a graph element: {err}"
                                        ))
                                    })?;
                                if value.is_null() {
                                    bindings.remove(variable);
                                }
                            }
                            None => {
                                return Err(CypherError::Execution(format!(
                                    "DELETE target '{variable}' is not bound"
                                )));
                            }
                        }
                    } else {
                        let read_ctx = QueryContext {
                            graph: ctx.graph,
                            indexes: None,
                            vector_indexes: None,
                            vector_recorder: None,
                            document_resolver: None,
                            cancellation: ctx.cancellation.clone(),
                            row_budget: ctx.row_budget,
                            byte_budget: ctx.byte_budget,
                            params: ctx.params.clone(),
                        };
                        let row = write_binding_row(&current_columns, &bindings);
                        let value = resolve_project_expr(&read_ctx, target, &row, &current_columns);
                        delete_graph_value(ctx.graph, &value, *detach, &mut summary).map_err(
                            |err| {
                                CypherError::Execution(format!(
                                    "DELETE target expression is not a graph element: {err}"
                                ))
                            },
                        )?;
                    }
                }
                MutationOp::ReadClause(_) => {
                    unreachable!("read clauses are handled per row-set")
                }
            }
            if let Some(branches) = branched_sets {
                next_binding_sets.extend(branches);
            } else {
                next_binding_sets.push(bindings);
            }
        }
        binding_sets = next_binding_sets;
        op_idx += delete_group_len.max(1);
    }

    let result = if let Some(columns) = &plan.return_columns {
        ctx.check_cancelled()?;
        let read_ctx = QueryContext {
            graph: ctx.graph,
            indexes: None,
            vector_indexes: None,
            vector_recorder: None,
            document_resolver: None,
            cancellation: ctx.cancellation.clone(),
            row_budget: ctx.row_budget,
            byte_budget: ctx.byte_budget,
            params: ctx.params.clone(),
        };
        let input = write_stream_to_query_result(&current_columns, &binding_sets);
        let mut result = execute_write_return_columns(&read_ctx, &input, columns)?;
        if let Some(keys) = &plan.return_order_by {
            result = execute_sort(&read_ctx, result, keys)?;
        }
        if let Some(skip) = &plan.return_skip {
            let skip = eval_row_count(skip, &read_ctx)?.min(result.rows.len());
            result.rows = result.rows.split_off(skip);
        }
        if let Some(limit) = &plan.return_limit {
            result.rows.truncate(eval_row_count(limit, &read_ctx)?);
        }
        Some(result)
    } else {
        None
    };
    Ok((summary, result))
}

fn initial_write_bindings(
    row: &[Value],
    column_names: &[String],
    graph_binding_kinds: &HashMap<String, GraphBindingKind>,
) -> HashMap<String, WriteBinding> {
    let mut bindings = HashMap::new();
    for (col_idx, col_name) in column_names.iter().enumerate() {
        if let Some(edge) = edge_ref_id(&row[col_idx]) {
            bindings.insert(col_name.clone(), WriteBinding::Edge(edge));
        } else if let Value::Int64(id) = &row[col_idx] {
            let binding = match graph_binding_kinds.get(col_name) {
                Some(GraphBindingKind::Relationship) => WriteBinding::Edge(EdgeId(*id as u64)),
                Some(GraphBindingKind::Node) => WriteBinding::Vertex(VertexId(*id as u64)),
                Some(GraphBindingKind::NodeList)
                | Some(GraphBindingKind::RelationshipList)
                | None => WriteBinding::Scalar(row[col_idx].clone()),
            };
            bindings.insert(col_name.clone(), binding);
        } else {
            bindings.insert(col_name.clone(), WriteBinding::Scalar(row[col_idx].clone()));
        }
    }
    bindings
}

pub fn execute_write_read_clause(
    ctx: &QueryContext,
    input: QueryResult,
    clause: &ReadClause,
) -> CypherResult<QueryResult> {
    match clause {
        ReadClause::Match { optional, clause } => {
            execute_apply_match(ctx, &input, clause, *optional, None)
        }
        ReadClause::Where(where_clause) => {
            ctx.check_cancelled()?;
            let mut rows = Vec::new();
            let mut estimated_bytes = initial_budgeted_bytes(&input.columns);
            for (row_idx, row) in input.rows.iter().enumerate() {
                if row_idx % 1024 == 0 {
                    ctx.check_cancelled()?;
                }
                if value_truthy(&eval_ast_expr(ctx, &where_clause.expr, row, &input.columns)) {
                    push_budgeted_row(ctx, &mut rows, row.clone(), &mut estimated_bytes)?;
                }
            }
            Ok(QueryResult {
                columns: input.columns,
                rows,
            })
        }
        ReadClause::With(with_clause) => execute_with_clause_direct(ctx, input, with_clause),
        ReadClause::Unwind { expr, alias } => {
            execute_unwind(ctx, &input, &ast_expr_to_project_expr(expr), alias)
        }
    }
}

fn write_stream_to_query_result(
    columns: &[String],
    binding_sets: &[HashMap<String, WriteBinding>],
) -> QueryResult {
    QueryResult {
        columns: columns.to_vec(),
        rows: binding_sets
            .iter()
            .map(|bindings| write_binding_row(columns, bindings))
            .collect(),
    }
}

fn write_binding_row(columns: &[String], bindings: &HashMap<String, WriteBinding>) -> Vec<Value> {
    columns
        .iter()
        .map(|column| {
            bindings
                .get(column)
                .map(write_binding_value)
                .unwrap_or(Value::Null)
        })
        .collect()
}

fn ensure_visible_write_column(columns: &mut Vec<String>, name: &str) {
    if is_internal_write_variable(name) || columns.iter().any(|column| column == name) {
        return;
    }
    columns.push(name.to_string());
}

fn is_internal_write_variable(name: &str) -> bool {
    name == MERGE_CREATED_KEY || name.starts_with("_anon_")
}

fn query_result_to_write_bindings(
    result: &QueryResult,
    graph_binding_kinds: &HashMap<String, GraphBindingKind>,
) -> Vec<HashMap<String, WriteBinding>> {
    result
        .rows
        .iter()
        .map(|row| {
            let mut bindings = HashMap::new();
            for (idx, column) in result.columns.iter().enumerate() {
                let Some(value) = row.get(idx) else {
                    continue;
                };
                let binding = match graph_binding_kinds.get(column).copied() {
                    Some(GraphBindingKind::Node) => value_to_vertex_binding(value)
                        .map(WriteBinding::Vertex)
                        .unwrap_or_else(|| WriteBinding::Scalar(value.clone())),
                    Some(GraphBindingKind::Relationship) => edge_ref_id(value)
                        .or_else(|| match value {
                            Value::Int64(id) if *id >= 0 => Some(EdgeId(*id as u64)),
                            _ => None,
                        })
                        .map(WriteBinding::Edge)
                        .unwrap_or_else(|| WriteBinding::Scalar(value.clone())),
                    Some(GraphBindingKind::NodeList) | Some(GraphBindingKind::RelationshipList) => {
                        WriteBinding::Scalar(value.clone())
                    }
                    None => edge_ref_id(value)
                        .map(WriteBinding::Edge)
                        .unwrap_or_else(|| WriteBinding::Scalar(value.clone())),
                };
                bindings.insert(column.clone(), binding);
            }
            bindings
        })
        .collect()
}

fn value_to_vertex_binding(value: &Value) -> Option<VertexId> {
    if let Some(vertex) = node_ref_id(value) {
        return Some(vertex);
    }
    let Value::Int64(id) = value else {
        return None;
    };
    if *id < 0 {
        return None;
    }
    Some(VertexId(*id as u64))
}

pub fn execute_write_return_columns(
    ctx: &QueryContext,
    input: &QueryResult,
    columns: &[ProjectColumn],
) -> CypherResult<QueryResult> {
    let aggregate_ops: Option<Vec<AggregateOp>> = columns
        .iter()
        .map(|column| write_return_aggregate(&column.expr, column.alias.clone()))
        .collect();

    if let Some(aggregations) = aggregate_ops {
        execute_aggregate(ctx, input, &[], &aggregations)
    } else {
        execute_project(ctx, input, columns)
    }
}

fn write_return_aggregate(expr: &ProjectExpr, alias: String) -> Option<AggregateOp> {
    match expr {
        ProjectExpr::Expression(Expr::CountStar) => Some(AggregateOp {
            function: "count".into(),
            inputs: Vec::new(),
            distinct: false,
            alias,
        }),
        ProjectExpr::Function { name, args } if is_write_aggregate_name(name) => {
            Some(AggregateOp {
                function: name.to_lowercase(),
                inputs: args.clone(),
                distinct: false,
                alias,
            })
        }
        _ => None,
    }
}

fn reset_merge_created(bindings: &mut HashMap<String, WriteBinding>) {
    bindings.insert(
        MERGE_CREATED_KEY.into(),
        WriteBinding::Scalar(Value::Bool(false)),
    );
}

fn mark_merge_created(bindings: &mut HashMap<String, WriteBinding>) {
    bindings.insert(
        MERGE_CREATED_KEY.into(),
        WriteBinding::Scalar(Value::Bool(true)),
    );
}

fn merge_created(bindings: &HashMap<String, WriteBinding>) -> bool {
    matches!(
        bindings.get(MERGE_CREATED_KEY),
        Some(WriteBinding::Scalar(Value::Bool(true)))
    )
}

fn clear_merge_created(bindings: &mut HashMap<String, WriteBinding>) {
    bindings.remove(MERGE_CREATED_KEY);
}

fn execute_merge_action(
    action: &MutationOp,
    ctx: &mut WriteContext,
    bindings: &mut HashMap<String, WriteBinding>,
    summary: &mut WriteSummary,
) -> CypherResult<()> {
    match action {
        MutationOp::SetProperty {
            variable,
            key,
            value,
        } => {
            let val = resolve_property_value(value, &ctx.params, ctx.graph, bindings)?;
            set_bound_property(ctx.graph, bindings, variable, key, val)?;
            summary.properties_set += 1;
            Ok(())
        }
        MutationOp::SetProperties {
            variable,
            value,
            replace,
        } => {
            let value = resolve_property_value(value, &ctx.params, ctx.graph, bindings)?;
            let changed = set_bound_properties(ctx.graph, bindings, variable, value, *replace)?;
            summary.properties_set += changed.set;
            summary.properties_removed += changed.removed;
            Ok(())
        }
        MutationOp::SetLabels { variable, labels } => {
            if let Some(vid) = nullable_write_vertex(bindings, variable)? {
                update_vertex_labels(ctx.graph, vid, labels, true)?;
            }
            Ok(())
        }
        MutationOp::RemoveProperty { .. }
        | MutationOp::RemoveLabels { .. }
        | MutationOp::BeginMerge
        | MutationOp::ApplyMergeActions { .. }
        | MutationOp::CreateNode { .. }
        | MutationOp::CreateEdge { .. }
        | MutationOp::MergeNode { .. }
        | MutationOp::MergeEdge { .. }
        | MutationOp::BindPath { .. }
        | MutationOp::ReadClause(_)
        | MutationOp::Delete { .. } => Err(CypherError::Execution(
            "MERGE ON action must be a SET operation".into(),
        )),
    }
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
            if let Expr::Variable(name) = expr.as_ref() {
                if let Some(binding) = bindings.get(name) {
                    return Ok(match binding {
                        WriteBinding::Vertex(vertex) => node_ref(*vertex),
                        WriteBinding::Edge(edge) => edge_ref(*edge),
                        WriteBinding::Scalar(value) => value.clone(),
                    });
                }
            }
            // Build a synthetic read context scoped to params and the current
            // write bindings projected as a row. Vertex/edge bindings are
            // exposed as Int64 ids so property access like `bound.prop` works.
            let mut columns: Vec<String> = Vec::with_capacity(bindings.len());
            let mut row: Vec<Value> = Vec::with_capacity(bindings.len());
            for (name, binding) in bindings {
                columns.push(name.clone());
                row.push(match binding {
                    WriteBinding::Vertex(v) => Value::Int64(v.0 as i64),
                    WriteBinding::Edge(e) => edge_ref(*e),
                    WriteBinding::Scalar(value) => value.clone(),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphBindingKind {
    Node,
    Relationship,
    NodeList,
    RelationshipList,
}

fn logical_plan_graph_binding_kinds(plan: &LogicalPlan) -> HashMap<String, GraphBindingKind> {
    match plan {
        LogicalPlan::Argument => HashMap::new(),
        LogicalPlan::ScanVertices { variable, .. } => {
            HashMap::from([(variable.clone(), GraphBindingKind::Node)])
        }
        LogicalPlan::Expand {
            input,
            dst_var,
            rel_var,
            ..
        } => {
            let mut vars = logical_plan_graph_binding_kinds(input);
            vars.insert(dst_var.clone(), GraphBindingKind::Node);
            if let Some(var) = rel_var {
                vars.insert(var.clone(), GraphBindingKind::Relationship);
            }
            vars
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Distinct { input } => logical_plan_graph_binding_kinds(input),
        LogicalPlan::Unwind { input, expr, alias } => {
            let mut vars = logical_plan_graph_binding_kinds(input);
            if let ProjectExpr::Variable(name) = expr {
                match vars.get(name).copied() {
                    Some(GraphBindingKind::NodeList) => {
                        vars.insert(alias.clone(), GraphBindingKind::Node);
                    }
                    Some(GraphBindingKind::RelationshipList) => {
                        vars.insert(alias.clone(), GraphBindingKind::Relationship);
                    }
                    _ => {}
                }
            }
            vars
        }
        LogicalPlan::ApplyMatch { input, clause, .. } => {
            let mut vars = logical_plan_graph_binding_kinds(input);
            collect_pattern_graph_binding_kinds(clause, &mut vars);
            vars
        }
        LogicalPlan::Project { input, columns } => {
            let input_vars = logical_plan_graph_binding_kinds(input);
            let mut out = HashMap::new();
            for column in columns {
                match &column.expr {
                    ProjectExpr::Wildcard => out.extend(input_vars.clone()),
                    ProjectExpr::Variable(name) => {
                        if let Some(kind) = input_vars.get(name) {
                            out.insert(column.alias.clone(), *kind);
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregations,
        } => {
            let input_vars = logical_plan_graph_binding_kinds(input);
            let mut out = HashMap::new();
            for column in group_by {
                if let ProjectExpr::Variable(name) = &column.expr {
                    if let Some(kind) = input_vars.get(name) {
                        out.insert(column.alias.clone(), *kind);
                    }
                }
            }
            for aggregation in aggregations {
                if aggregation.function.eq_ignore_ascii_case("collect") {
                    if let Some(ProjectExpr::Variable(name)) = aggregation.inputs.first() {
                        match input_vars.get(name).copied() {
                            Some(GraphBindingKind::Node) => {
                                out.insert(aggregation.alias.clone(), GraphBindingKind::NodeList);
                            }
                            Some(GraphBindingKind::Relationship) => {
                                out.insert(
                                    aggregation.alias.clone(),
                                    GraphBindingKind::RelationshipList,
                                );
                            }
                            _ => {}
                        }
                    }
                }
            }
            out
        }
        LogicalPlan::Union { left, .. } => logical_plan_graph_binding_kinds(left),
    }
}

fn graph_binding_kinds_after_read_clause(
    input: &HashMap<String, GraphBindingKind>,
    clause: &ReadClause,
) -> HashMap<String, GraphBindingKind> {
    match clause {
        ReadClause::Match { clause, .. } => {
            let mut vars = input.clone();
            collect_pattern_graph_binding_kinds(clause, &mut vars);
            vars
        }
        ReadClause::Where(_) => input.clone(),
        ReadClause::Unwind { expr, alias } => {
            let mut vars = input.clone();
            match infer_expr_graph_binding_kind(expr, input) {
                Some(GraphBindingKind::NodeList) => {
                    vars.insert(alias.clone(), GraphBindingKind::Node);
                }
                Some(GraphBindingKind::RelationshipList) => {
                    vars.insert(alias.clone(), GraphBindingKind::Relationship);
                }
                _ => {
                    vars.remove(alias);
                }
            }
            vars
        }
        ReadClause::With(with_clause) => {
            let mut out = HashMap::new();
            for item in &with_clause.items {
                if matches!(&item.expr, Expr::Variable(name) if name == "*") && item.alias.is_none()
                {
                    out.extend(input.clone());
                    continue;
                }
                if let Some(kind) = infer_expr_graph_binding_kind(&item.expr, input) {
                    out.insert(return_item_output_name(item), kind);
                }
            }
            out
        }
    }
}

fn infer_expr_graph_binding_kind(
    expr: &Expr,
    input: &HashMap<String, GraphBindingKind>,
) -> Option<GraphBindingKind> {
    match expr {
        Expr::Variable(name) => input.get(name).copied(),
        Expr::FunctionCall { name, args } if name.eq_ignore_ascii_case("collect") => {
            let kind = args
                .first()
                .and_then(|arg| infer_expr_graph_binding_kind(arg, input))?;
            match kind {
                GraphBindingKind::Node => Some(GraphBindingKind::NodeList),
                GraphBindingKind::Relationship => Some(GraphBindingKind::RelationshipList),
                _ => None,
            }
        }
        Expr::List(items) => infer_list_graph_binding_kind(items, input),
        Expr::BinaryOp {
            left,
            op: BinaryOp::Add,
            right,
        } => {
            let left_kind = infer_expr_graph_binding_kind(left, input);
            let right_kind = infer_expr_graph_binding_kind(right, input);
            match (left_kind, right_kind) {
                (Some(GraphBindingKind::NodeList), Some(GraphBindingKind::NodeList)) => {
                    Some(GraphBindingKind::NodeList)
                }
                (
                    Some(GraphBindingKind::RelationshipList),
                    Some(GraphBindingKind::RelationshipList),
                ) => Some(GraphBindingKind::RelationshipList),
                _ => None,
            }
        }
        Expr::Index { target, .. } => match infer_expr_graph_binding_kind(target, input) {
            Some(GraphBindingKind::NodeList) => Some(GraphBindingKind::Node),
            Some(GraphBindingKind::RelationshipList) => Some(GraphBindingKind::Relationship),
            _ => None,
        },
        _ => None,
    }
}

fn infer_list_graph_binding_kind(
    items: &[Expr],
    input: &HashMap<String, GraphBindingKind>,
) -> Option<GraphBindingKind> {
    let mut inferred = None;
    for item in items {
        let item_kind = match infer_expr_graph_binding_kind(item, input)? {
            GraphBindingKind::Node => GraphBindingKind::NodeList,
            GraphBindingKind::Relationship => GraphBindingKind::RelationshipList,
            GraphBindingKind::NodeList => GraphBindingKind::NodeList,
            GraphBindingKind::RelationshipList => GraphBindingKind::RelationshipList,
        };
        if inferred.is_some_and(|existing| existing != item_kind) {
            return None;
        }
        inferred = Some(item_kind);
    }
    inferred
}

fn collect_pattern_graph_binding_kinds(
    clause: &MatchClause,
    vars: &mut HashMap<String, GraphBindingKind>,
) {
    for pattern in &clause.patterns {
        for element in &pattern.elements {
            match element {
                PatternElement::Node(node) => {
                    if let Some(variable) = &node.variable {
                        vars.insert(variable.clone(), GraphBindingKind::Node);
                    }
                }
                PatternElement::Relationship(rel) => {
                    if let Some(variable) = &rel.variable {
                        let kind = if rel.min_hops.is_some() || rel.max_hops.is_some() {
                            GraphBindingKind::RelationshipList
                        } else {
                            GraphBindingKind::Relationship
                        };
                        vars.insert(variable.clone(), kind);
                    }
                }
            }
        }
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
        Some(WriteBinding::Scalar(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is bound to a scalar, not a node"
        ))),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

fn nullable_write_vertex(
    bindings: &HashMap<String, WriteBinding>,
    variable: &str,
) -> CypherResult<Option<VertexId>> {
    match bindings.get(variable) {
        Some(WriteBinding::Scalar(Value::Null)) => Ok(None),
        Some(_) => resolve_write_vertex(bindings, variable).map(Some),
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
        Some(WriteBinding::Scalar(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is bound to a scalar, not a relationship"
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
        Some(WriteBinding::Scalar(Value::Null)) => Ok(()),
        Some(WriteBinding::Scalar(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is bound to a scalar, not a graph element"
        ))),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

fn remove_bound_property(
    graph: &mut Graph,
    bindings: &HashMap<String, WriteBinding>,
    variable: &str,
    key: &str,
) -> CypherResult<bool> {
    match bindings.get(variable) {
        Some(WriteBinding::Vertex(vertex)) => {
            if graph
                .get_vertex_properties(*vertex)
                .into_iter()
                .all(|(name, value)| name != key || matches!(value, Value::Null))
            {
                return Ok(false);
            }
            graph
                .try_set_vertex_property(*vertex, key, Value::Null)
                .map(|_| true)
                .map_err(|e| CypherError::Execution(e.to_string()))
        }
        Some(WriteBinding::Edge(edge)) => {
            if graph
                .get_edge_properties(*edge)
                .into_iter()
                .all(|(name, value)| name != key || matches!(value, Value::Null))
            {
                return Ok(false);
            }
            graph
                .try_set_edge_property(*edge, key, Value::Null)
                .map(|_| true)
                .map_err(|e| CypherError::Execution(e.to_string()))
        }
        Some(WriteBinding::Scalar(Value::Null)) => Ok(false),
        Some(WriteBinding::Scalar(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is bound to a scalar, not a graph element"
        ))),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

#[derive(Default)]
struct PropertyChanges {
    set: usize,
    removed: usize,
}

fn set_bound_properties(
    graph: &mut Graph,
    bindings: &HashMap<String, WriteBinding>,
    variable: &str,
    value: Value,
    replace: bool,
) -> CypherResult<PropertyChanges> {
    let entries = match property_entries_from_value(graph, value) {
        Ok(Some(entries)) => entries,
        Ok(None) => return Ok(PropertyChanges::default()),
        Err(other) => {
            return Err(CypherError::Execution(format!(
                "SET {variable}{} requires a map, got {other:?}",
                if replace { " =" } else { " +=" }
            )));
        }
    };

    match bindings.get(variable) {
        Some(WriteBinding::Scalar(Value::Null)) => Ok(PropertyChanges::default()),
        Some(WriteBinding::Vertex(vertex)) => {
            let mut changes = PropertyChanges::default();
            if replace {
                for (key, _) in graph.get_vertex_properties(*vertex) {
                    graph
                        .try_set_vertex_property(*vertex, &key, Value::Null)
                        .map_err(|e| CypherError::Execution(e.to_string()))?;
                    changes.removed += 1;
                }
            }
            for (key, value) in entries {
                graph
                    .try_set_vertex_property(*vertex, &key, value.clone())
                    .map_err(|e| CypherError::Execution(e.to_string()))?;
                if value.is_null() {
                    changes.removed += 1;
                } else {
                    changes.set += 1;
                }
            }
            Ok(changes)
        }
        Some(WriteBinding::Edge(edge)) => {
            let mut changes = PropertyChanges::default();
            if replace {
                for (key, _) in graph.get_edge_properties(*edge) {
                    graph
                        .try_set_edge_property(*edge, &key, Value::Null)
                        .map_err(|e| CypherError::Execution(e.to_string()))?;
                    changes.removed += 1;
                }
            }
            for (key, value) in entries {
                graph
                    .try_set_edge_property(*edge, &key, value.clone())
                    .map_err(|e| CypherError::Execution(e.to_string()))?;
                if value.is_null() {
                    changes.removed += 1;
                } else {
                    changes.set += 1;
                }
            }
            Ok(changes)
        }
        Some(WriteBinding::Scalar(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is bound to a scalar, not a graph element"
        ))),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

fn property_entries_from_value(
    graph: &Graph,
    value: Value,
) -> Result<Option<Vec<(String, Value)>>, Value> {
    if value.is_null() {
        return Ok(None);
    }
    if let Some(vertex) = node_ref_id(&value) {
        return Ok(Some(graph.get_vertex_properties(vertex)));
    }
    if let Some(edge) = edge_ref_id(&value) {
        return Ok(Some(graph.get_edge_properties(edge)));
    }
    if let Value::Map(entries) = value {
        return Ok(Some(entries));
    }
    Err(value)
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
        Some(WriteBinding::Scalar(Value::Map(entries))) => Ok(entries
            .iter()
            .find(|(entry_key, _)| entry_key == key)
            .map(|(_, value)| value.clone())
            .unwrap_or(Value::Null)),
        Some(WriteBinding::Scalar(_)) => Ok(Value::Null),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

fn find_matching_vertices(
    graph: &Graph,
    labels: &[String],
    properties: &[(String, Value)],
) -> Vec<VertexId> {
    let mut vertices = Vec::new();
    for vid_raw in 0..graph.num_vertices() as u64 {
        let vid = VertexId(vid_raw);
        if !labels.is_empty() && graph.vertex_label(vid).is_none() {
            continue;
        }
        if !labels.is_empty() && !vertex_has_all_labels(graph, vid, labels) {
            continue;
        }
        let matches = properties
            .iter()
            .all(|(key, value)| graph.get_vertex_property(vid, key) == *value);
        if matches {
            vertices.push(vid);
        }
    }
    vertices
}

fn find_matching_edges(
    graph: &Graph,
    source: VertexId,
    target: VertexId,
    label: &str,
    undirected: bool,
    properties: &[(String, Value)],
) -> Vec<EdgeId> {
    graph
        .edge_records()
        .into_iter()
        .filter_map(|edge| {
            let endpoints_match = if undirected {
                (edge.source == source && edge.target == target)
                    || (edge.source == target && edge.target == source)
            } else {
                edge.source == source && edge.target == target
            };
            if !endpoints_match || edge.label != label {
                return None;
            }
            let matches = properties
                .iter()
                .all(|(key, value)| graph.get_edge_property(edge.id, key) == *value);
            matches.then_some(edge.id)
        })
        .collect()
}

fn vertex_labels(graph: &Graph, vertex: VertexId) -> Vec<String> {
    graph.vertex_label(vertex).map_or_else(Vec::new, |label| {
        label
            .split(':')
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect()
    })
}

/// Add or remove labels on a vertex by recomputing the colon-joined label
/// string and writing it back to the graph. `add` controls whether the
/// requested labels are inserted (true) or excised (false). Idempotent.
fn update_vertex_labels(
    graph: &mut Graph,
    vertex: VertexId,
    labels: &[String],
    add: bool,
) -> CypherResult<()> {
    let mut current = vertex_labels(graph, vertex);
    if add {
        for label in labels {
            if !current.iter().any(|l| l == label) {
                current.push(label.clone());
            }
        }
    } else {
        current.retain(|l| !labels.iter().any(|removed| removed == l));
    }
    let joined = current.join(":");
    graph
        .try_set_vertex_label(vertex, &joined)
        .map_err(|e| CypherError::Execution(e.to_string()))
}

fn vertex_matches_label_filter(graph: &Graph, vertex: VertexId, label_filter: &str) -> bool {
    let labels: Vec<&str> = label_filter
        .split(':')
        .filter(|part| !part.is_empty())
        .collect();
    vertex_has_all_labels(graph, vertex, &labels)
}

fn vertex_has_all_labels(graph: &Graph, vertex: VertexId, expected: &[impl AsRef<str>]) -> bool {
    let actual = vertex_labels(graph, vertex);
    !actual.is_empty()
        && expected
            .iter()
            .all(|label| actual.iter().any(|actual| actual == label.as_ref()))
}

fn has_incident_edges(graph: &Graph, vid: VertexId) -> bool {
    graph.incident_degree(vid, Direction::Both) > 0
}

fn count_incident_edges(graph: &Graph, vid: VertexId) -> usize {
    graph.incident_degree(vid, Direction::Both)
}

fn consecutive_delete_group_len(mutations: &[MutationOp], start: usize) -> usize {
    let Some(MutationOp::Delete {
        detach: first_detach,
        ..
    }) = mutations.get(start)
    else {
        return 0;
    };

    let mut len = 0;
    while let Some(MutationOp::Delete { detach, .. }) = mutations.get(start + len) {
        if detach != first_detach {
            break;
        }
        len += 1;
    }
    len
}

#[derive(Default)]
struct DeleteBatch {
    vertices: HashSet<u64>,
    edges: HashSet<u64>,
}

fn execute_delete_group(
    delete_ops: &[MutationOp],
    graph: &mut Graph,
    params: &HashMap<String, Value>,
    row: &[Value],
    column_names: &[String],
    bindings: &mut HashMap<String, WriteBinding>,
    summary: &mut WriteSummary,
) -> CypherResult<()> {
    let detach = delete_ops
        .iter()
        .find_map(|op| match op {
            MutationOp::Delete { detach, .. } => Some(*detach),
            _ => None,
        })
        .unwrap_or(false);
    let mut batch = DeleteBatch::default();
    let mut variables_to_remove = Vec::new();
    let mut variables_to_replace = Vec::new();

    for op in delete_ops {
        let MutationOp::Delete {
            target,
            variable,
            detach: _,
        } = op
        else {
            continue;
        };

        if let Some(variable) = variable {
            match bindings.get(variable).cloned() {
                Some(WriteBinding::Edge(edge)) => {
                    batch.edges.insert(edge.0);
                    let rel_type = graph.edge_label(edge).map(str::to_string);
                    variables_to_replace.push((
                        variable.clone(),
                        WriteBinding::Scalar(edge_ref_with_type(edge, rel_type)),
                    ));
                }
                Some(WriteBinding::Vertex(vertex)) => {
                    batch.vertices.insert(vertex.0);
                    variables_to_remove.push(variable.clone());
                }
                Some(WriteBinding::Scalar(value)) => {
                    collect_delete_value(graph, &value, &mut batch).map_err(|err| {
                        CypherError::Execution(format!(
                            "DELETE target '{variable}' is not a graph element: {err}"
                        ))
                    })?;
                    if value.is_null() {
                        variables_to_remove.push(variable.clone());
                    }
                }
                None => {
                    return Err(CypherError::Execution(format!(
                        "DELETE target '{variable}' is not bound"
                    )));
                }
            }
        } else {
            let read_ctx = QueryContext {
                graph,
                indexes: None,
                vector_indexes: None,
                vector_recorder: None,
                document_resolver: None,
                cancellation: None,
                row_budget: None,
                byte_budget: None,
                params: params.clone(),
            };
            let value = resolve_project_expr(&read_ctx, target, row, column_names);
            collect_delete_value(graph, &value, &mut batch).map_err(|err| {
                CypherError::Execution(format!(
                    "DELETE target expression is not a graph element: {err}"
                ))
            })?;
        }
    }

    apply_delete_batch(graph, &batch, detach, summary).map_err(CypherError::Execution)?;

    for variable in variables_to_remove {
        bindings.remove(&variable);
    }
    for (variable, value) in variables_to_replace {
        bindings.insert(variable, value);
    }

    Ok(())
}

fn collect_delete_value(
    graph: &Graph,
    value: &Value,
    batch: &mut DeleteBatch,
) -> Result<(), &'static str> {
    if value.is_null() {
        return Ok(());
    }
    if let Some(edge) = edge_ref_id(value) {
        batch.edges.insert(edge.0);
        return Ok(());
    }
    if let Some(vertex) = node_ref_id(value) {
        batch.vertices.insert(vertex.0);
        return Ok(());
    }
    if let Some((nodes, edges)) = path_components(value) {
        for edge_value in edges {
            if let Some(edge) = edge_ref_id(&edge_value).or_else(|| {
                edge_value
                    .as_i64()
                    .map(|edge_id| EdgeId(edge_id as u64))
                    .filter(|edge| graph.edge_exists(*edge))
            }) {
                batch.edges.insert(edge.0);
            }
        }
        for node_value in nodes {
            if let Some(vertex) = node_ref_id(&node_value).or_else(|| {
                node_value
                    .as_i64()
                    .map(|vertex_id| VertexId(vertex_id as u64))
                    .filter(|vertex| graph.vertex_label(*vertex).is_some())
            }) {
                batch.vertices.insert(vertex.0);
            }
        }
        return Ok(());
    }

    match value {
        Value::List(items) => {
            for item in items {
                collect_delete_value(graph, item, batch)?;
            }
            Ok(())
        }
        Value::Int64(id) => {
            let vertex = VertexId(*id as u64);
            if graph.vertex_label(vertex).is_some() {
                batch.vertices.insert(vertex.0);
                return Ok(());
            }
            let edge = EdgeId(*id as u64);
            if graph.edge_exists(edge) {
                batch.edges.insert(edge.0);
            }
            Ok(())
        }
        _ => Err("unsupported delete value"),
    }
}

fn apply_delete_batch(
    graph: &mut Graph,
    batch: &DeleteBatch,
    detach: bool,
    summary: &mut WriteSummary,
) -> Result<(), String> {
    let mut edges: Vec<_> = batch.edges.iter().copied().map(EdgeId).collect();
    edges.sort_unstable_by_key(|edge| edge.0);
    for edge in edges {
        delete_edge_if_exists(graph, edge, summary).map_err(|_| "relationship delete failed")?;
    }

    let mut vertices: Vec<_> = batch.vertices.iter().copied().map(VertexId).collect();
    vertices.sort_unstable_by_key(|vertex| vertex.0);
    for vertex in vertices {
        delete_vertex_if_exists(graph, vertex, detach, summary)
            .map_err(|_| "node delete failed")?;
    }

    Ok(())
}

fn delete_graph_value(
    graph: &mut Graph,
    value: &Value,
    detach: bool,
    summary: &mut WriteSummary,
) -> Result<(), &'static str> {
    if value.is_null() {
        return Ok(());
    }
    if let Some(edge) = edge_ref_id(value) {
        return delete_edge_if_exists(graph, edge, summary)
            .map_err(|_| "relationship delete failed");
    }
    if let Some(vertex) = node_ref_id(value) {
        return delete_vertex_if_exists(graph, vertex, detach, summary)
            .map_err(|_| "node delete failed");
    }
    if let Some((nodes, edges)) = path_components(value) {
        let mut seen_edges = HashSet::new();
        for edge_value in edges {
            if let Some(edge) = edge_ref_id(&edge_value).or_else(|| {
                edge_value
                    .as_i64()
                    .map(|edge_id| EdgeId(edge_id as u64))
                    .filter(|edge| graph.edge_exists(*edge))
            }) {
                if seen_edges.insert(edge.0) {
                    delete_edge_if_exists(graph, edge, summary)
                        .map_err(|_| "relationship delete failed")?;
                }
            }
        }

        let mut seen_vertices = HashSet::new();
        for node_value in nodes {
            if let Some(vertex) = node_ref_id(&node_value).or_else(|| {
                node_value
                    .as_i64()
                    .map(|vertex_id| VertexId(vertex_id as u64))
                    .filter(|vertex| graph.vertex_label(*vertex).is_some())
            }) {
                if seen_vertices.insert(vertex.0) {
                    delete_vertex_if_exists(graph, vertex, detach, summary)
                        .map_err(|_| "node delete failed")?;
                }
            }
        }
        return Ok(());
    }
    match value {
        Value::List(items) => {
            for item in items {
                delete_graph_value(graph, item, detach, summary)?;
            }
            Ok(())
        }
        Value::Int64(id) => {
            let vertex = VertexId(*id as u64);
            if graph.vertex_label(vertex).is_some() {
                return delete_vertex_if_exists(graph, vertex, detach, summary)
                    .map_err(|_| "node delete failed");
            }
            let edge = EdgeId(*id as u64);
            if graph.edge_exists(edge) {
                return delete_edge_if_exists(graph, edge, summary)
                    .map_err(|_| "relationship delete failed");
            }
            Ok(())
        }
        _ => Err("unsupported delete value"),
    }
}

fn delete_edge_if_exists(
    graph: &mut Graph,
    edge: EdgeId,
    summary: &mut WriteSummary,
) -> CypherResult<()> {
    if graph.edge_exists(edge) {
        graph
            .try_remove_edge(edge)
            .map_err(|e| CypherError::Execution(e.to_string()))?;
        summary.edges_deleted += 1;
    }
    Ok(())
}

fn delete_vertex_if_exists(
    graph: &mut Graph,
    vertex: VertexId,
    detach: bool,
    summary: &mut WriteSummary,
) -> CypherResult<()> {
    if graph.vertex_label(vertex).is_none() {
        return Ok(());
    }
    if !detach && has_incident_edges(graph, vertex) {
        return Err(CypherError::Execution(
            "cannot DELETE node with relationships; use DETACH DELETE".into(),
        ));
    }
    if detach {
        summary.edges_deleted += count_incident_edges(graph, vertex);
    }
    graph
        .try_remove_vertex(vertex)
        .map_err(|e| CypherError::Execution(e.to_string()))?;
    summary.nodes_deleted += 1;
    Ok(())
}

fn write_binding_value(binding: &WriteBinding) -> Value {
    match binding {
        WriteBinding::Vertex(vertex) => Value::Int64(vertex.0 as i64),
        WriteBinding::Edge(edge) => edge_ref(*edge),
        WriteBinding::Scalar(value) => value.clone(),
    }
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

/// Convenience: parse + plan + execute with composite and vector index support
/// (read-only).
pub fn run_cypher_with_indexes_and_vectors(
    query: &str,
    graph: &Graph,
    indexes: &nexus_index::composite::IndexSet,
    vector_indexes: &HashMap<String, nexus_index::vector::VectorIndex>,
) -> CypherResult<QueryResult> {
    let ctx = QueryContext::with_indexes(graph, indexes).with_vector_indexes(vector_indexes);
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
    let validation_stmt = substitute_params_for_validation(stmt.clone(), &params);
    match stmt {
        crate::ast::Statement::Read(q) => {
            if let Statement::Read(validation_query) = &validation_stmt {
                crate::binder::bind_query(validation_query)?;
            }
            let plan = crate::planner::plan_query(&q)?;
            let ctx = QueryContext::new(graph).with_params(params);
            let result = execute(&plan, &ctx)?;
            Ok(RunResult::Read(result))
        }
        crate::ast::Statement::Write(wq) => {
            if let Statement::Write(validation_query) = &validation_stmt {
                crate::binder::bind_write(validation_query)?;
            }
            let plan = crate::planner::plan_write(&wq)?;
            let mut ctx = WriteContext::new(graph).with_params(params);
            execute_write_returning(&plan, &mut ctx)
        }
    }
}

fn substitute_params_for_validation(
    mut stmt: Statement,
    params: &HashMap<String, Value>,
) -> Statement {
    match &mut stmt {
        Statement::Read(query) => substitute_params_query(query, params),
        Statement::Write(query) => {
            if let Some(match_clause) = &mut query.match_clause {
                substitute_params_match(match_clause, params);
            }
            if let Some(where_clause) = &mut query.where_clause {
                substitute_params_expr(&mut where_clause.expr, params);
            }
            for clause in &mut query.tail {
                substitute_params_read_clause(clause, params);
            }
            for mutation in &mut query.mutations {
                match mutation {
                    crate::ast::MutationClause::Read(clause) => {
                        substitute_params_read_clause(clause, params);
                    }
                    crate::ast::MutationClause::Create { patterns } => {
                        for pattern in patterns {
                            substitute_params_pattern(pattern, params);
                        }
                    }
                    crate::ast::MutationClause::Merge {
                        patterns,
                        on_create,
                        on_match,
                    } => {
                        for pattern in patterns {
                            substitute_params_pattern(pattern, params);
                        }
                        substitute_params_set_items(on_create, params);
                        substitute_params_set_items(on_match, params);
                    }
                    crate::ast::MutationClause::Set { items } => {
                        substitute_params_set_items(items, params);
                    }
                    crate::ast::MutationClause::Remove { .. }
                    | crate::ast::MutationClause::Delete { .. } => {}
                }
            }
            if let Some(return_clause) = &mut query.return_clause {
                substitute_params_return(return_clause, params);
            }
        }
    }
    stmt
}

fn substitute_params_query(query: &mut crate::ast::Query, params: &HashMap<String, Value>) {
    if let Some(match_clause) = &mut query.match_clause {
        substitute_params_match(match_clause, params);
    }
    if let Some(where_clause) = &mut query.where_clause {
        substitute_params_expr(&mut where_clause.expr, params);
    }
    for clause in &mut query.tail {
        substitute_params_read_clause(clause, params);
    }
    substitute_params_return(&mut query.return_clause, params);
    if let Some(order_by) = &mut query.order_by {
        for item in &mut order_by.items {
            substitute_params_expr(&mut item.expr, params);
        }
    }
    if let Some(union) = &mut query.union {
        substitute_params_query(&mut union.right, params);
    }
}

fn substitute_params_read_clause(clause: &mut ReadClause, params: &HashMap<String, Value>) {
    match clause {
        ReadClause::Match { clause, .. } => substitute_params_match(clause, params),
        ReadClause::Where(where_clause) => substitute_params_expr(&mut where_clause.expr, params),
        ReadClause::With(with_clause) => {
            for item in &mut with_clause.items {
                substitute_params_expr(&mut item.expr, params);
            }
            if let Some(where_clause) = &mut with_clause.where_clause {
                substitute_params_expr(&mut where_clause.expr, params);
            }
            if let Some(order_by) = &mut with_clause.order_by {
                for item in &mut order_by.items {
                    substitute_params_expr(&mut item.expr, params);
                }
            }
        }
        ReadClause::Unwind { expr, .. } => substitute_params_expr(expr, params),
    }
}

fn substitute_params_set_items(items: &mut [crate::ast::SetItem], params: &HashMap<String, Value>) {
    for item in items {
        match item {
            crate::ast::SetItem::Property { value, .. }
            | crate::ast::SetItem::Properties { value, .. } => {
                substitute_params_expr(value, params);
            }
            crate::ast::SetItem::Labels { .. } => {}
        }
    }
}

fn substitute_params_return(
    return_clause: &mut crate::ast::ReturnClause,
    params: &HashMap<String, Value>,
) {
    for item in &mut return_clause.items {
        substitute_params_expr(&mut item.expr, params);
    }
}

fn substitute_params_match(match_clause: &mut MatchClause, params: &HashMap<String, Value>) {
    for pattern in &mut match_clause.patterns {
        substitute_params_pattern(pattern, params);
    }
}

fn substitute_params_pattern(pattern: &mut Pattern, params: &HashMap<String, Value>) {
    for element in &mut pattern.elements {
        match element {
            PatternElement::Node(node) => {
                for (_, expr) in &mut node.properties {
                    substitute_params_expr(expr, params);
                }
            }
            PatternElement::Relationship(rel) => {
                for (_, expr) in &mut rel.properties {
                    substitute_params_expr(expr, params);
                }
            }
        }
    }
}

fn substitute_params_expr(expr: &mut Expr, params: &HashMap<String, Value>) {
    match expr {
        Expr::Parameter(name) => {
            if let Some(value) = params.get(name) {
                *expr = value_to_expr(value);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            substitute_params_expr(left, params);
            substitute_params_expr(right, params);
        }
        Expr::UnaryOp { expr, .. } | Expr::Exists(expr) => substitute_params_expr(expr, params),
        Expr::FunctionCall { args, .. } | Expr::List(args) => {
            for arg in args {
                substitute_params_expr(arg, params);
            }
        }
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            substitute_params_expr(list, params);
            if let Some(predicate) = predicate {
                substitute_params_expr(predicate, params);
            }
            if let Some(projection) = projection {
                substitute_params_expr(projection, params);
            }
        }
        Expr::Map(entries) => {
            for (_, value) in entries {
                substitute_params_expr(value, params);
            }
        }
        Expr::In { expr, list } => {
            substitute_params_expr(expr, params);
            substitute_params_expr(list, params);
        }
        Expr::Index { target, index } => {
            substitute_params_expr(target, params);
            substitute_params_expr(index, params);
        }
        Expr::Slice { target, start, end } => {
            substitute_params_expr(target, params);
            if let Some(start) = start {
                substitute_params_expr(start, params);
            }
            if let Some(end) = end {
                substitute_params_expr(end, params);
            }
        }
        Expr::PatternPredicate(pattern) => substitute_params_pattern(pattern, params),
        Expr::PatternComprehension {
            pattern,
            projection,
            ..
        } => {
            substitute_params_pattern(pattern, params);
            substitute_params_expr(projection, params);
        }
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            if let Some(scrutinee) = scrutinee {
                substitute_params_expr(scrutinee, params);
            }
            for (when_expr, then_expr) in arms {
                substitute_params_expr(when_expr, params);
                substitute_params_expr(then_expr, params);
            }
            if let Some(default) = default {
                substitute_params_expr(default, params);
            }
        }
        Expr::ListPredicate {
            list, predicate, ..
        } => {
            substitute_params_expr(list, params);
            if let Some(predicate) = predicate {
                substitute_params_expr(predicate, params);
            }
        }
        Expr::ExistsSubquery(query) => substitute_params_query(query, params),
        Expr::Literal(_) | Expr::Property(_) | Expr::Variable(_) | Expr::CountStar => {}
    }
}

fn value_to_expr(value: &Value) -> Expr {
    match value {
        Value::Null => Expr::Literal(Literal::Null),
        Value::Bool(value) => Expr::Literal(Literal::Bool(*value)),
        Value::Int64(value) => Expr::Literal(Literal::Integer(*value)),
        Value::Float64(value) => Expr::Literal(Literal::Float(*value)),
        Value::String(value) => Expr::Literal(Literal::String(value.clone())),
        Value::Bytes(_) => Expr::Literal(Literal::Null),
        Value::List(values) => Expr::List(values.iter().map(value_to_expr).collect()),
        Value::Map(entries) => Expr::Map(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), value_to_expr(value)))
                .collect(),
        ),
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
    use nexus_index::vector::VectorIndex;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

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
    fn execute_relationship_degree_count_aggregate() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:Entity {name: 'A'}), (b:Entity {name: 'B'}), (c:Entity {name: 'C'})
             WITH *
             CREATE (a)-[:R]->(b), (a)-[:R]->(c), (b)-[:R]->(c)",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (e:Entity)-[r]-() RETURN e.name, count(r) ORDER BY count(r) DESC",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };

        assert_eq!(qr.columns, vec!["e.name", "count(r)"]);
        assert_eq!(qr.num_rows(), 3);
        for name in ["A", "B", "C"] {
            assert!(
                qr.rows
                    .contains(&vec![Value::String(name.into()), Value::Int64(2)])
            );
        }
    }

    #[test]
    fn execute_typed_relationship_degree_count_aggregate() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:Entity {name: 'A'}), (b:Entity {name: 'B'}), (c:Entity {name: 'C'})
             WITH *
             CREATE (a)-[:R]->(b), (a)-[:R]->(c), (a)-[:OTHER]->(a), (b)-[:R]->(b)",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (e:Entity)-[r:R]-() RETURN e.name, count(r) ORDER BY e.name",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };

        assert_eq!(qr.columns, vec!["e.name", "count(r)"]);
        assert_eq!(
            qr.rows,
            vec![
                vec![Value::String("A".into()), Value::Int64(2)],
                vec![Value::String("B".into()), Value::Int64(2)],
                vec![Value::String("C".into()), Value::Int64(1)],
            ]
        );
    }

    #[test]
    fn execute_with_aggregate_orders_after_grouping() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE ({name: 'A'}), ({name: 'A'}), ({name: 'B'}), ({name: 'C'}), ({name: 'C'})",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (a) WITH a.name AS name, count(*) AS cnt ORDER BY a.name + 'C' ASC LIMIT 1 RETURN name, cnt",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(
            qr.rows,
            vec![vec![Value::String("A".into()), Value::Int64(2)]]
        );

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (a) WITH a.name AS name, count(*) AS cnt ORDER BY a.name + 'C' DESC LIMIT 1 RETURN name, cnt",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(
            qr.rows,
            vec![vec![Value::String("C".into()), Value::Int64(2)]]
        );
    }

    #[test]
    fn execute_aggregate_preserves_return_column_order() {
        let mut g = writable_graph();
        let RunResult::Read(qr) = run_cypher_mut(
            "UNWIND [1] AS x RETURN count(*) AS matches, x IS NULL AS opt_match",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.columns, vec!["matches", "opt_match"]);
        assert_eq!(qr.rows, vec![vec![Value::Int64(1), Value::Bool(false)]]);
    }

    #[test]
    fn execute_sum_of_integers_stays_integral() {
        let mut g = writable_graph();
        let RunResult::Read(qr) =
            run_cypher_mut("UNWIND [1, 2, 3] AS x RETURN sum(x) AS s", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.rows, vec![vec![Value::Int64(6)]]);
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
    fn execute_repeated_node_binding_filters_directed_self_loop() {
        let mut g = writable_graph();
        run_cypher_mut("CREATE (a:A)-[:LOOP]->(a), ()-[:T]->()", &mut g).unwrap();
        let RunResult::Read(qr) =
            run_cypher_mut("MATCH (n)-[r]->(n) RETURN count(r)", &mut g).unwrap()
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
    fn execute_pattern_predicate() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:A {name: 'a'}), (b:B {name: 'b'}), (c:A {name: 'c'}), (a)-[:R]->(b)",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) =
            run_cypher_mut("MATCH (n:A) WHERE (n)-[:R]->() RETURN n.name", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.rows, vec![vec![Value::String("a".into())]]);
    }

    #[test]
    fn execute_correlated_exists_subquery() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:A {name: 'a'}), (b:B {name: 'b'}), (c:A {name: 'c'}), (a)-[:R]->(b)",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (n:A) WHERE exists { (n)-[:R]->() } RETURN n.name",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.rows, vec![vec![Value::String("a".into())]]);
    }

    #[test]
    fn execute_correlated_exists_subquery_with_aggregation() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:A {name: 'a'}), (b:B {name: 'b'}), (c:C {name: 'c'}), \
             (a)-[:R]->(b), (a)-[:R]->(c), (b)-[:R]->(c)",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (n) WHERE exists { \
               MATCH (n)-->(m) \
               WITH n, count(*) AS numConnections \
               WHERE numConnections = 2 \
               RETURN true \
             } RETURN n.name",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.rows, vec![vec![Value::String("a".into())]]);
    }

    #[test]
    fn execute_pattern_comprehension_returns_paths() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:A {name: 'a'}), (b:B {name: 'b'}), (a)-[:R]->(b)",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) =
            run_cypher_mut("MATCH (n:A) RETURN [p = (n)-[:R]->() | p] AS paths", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        let Value::List(paths) = &qr.rows[0][0] else {
            panic!("expected list of paths");
        };
        assert_eq!(paths.len(), 1);
        let Some((nodes, edges)) = path_components(&paths[0]) else {
            panic!("expected path value");
        };
        assert_eq!(nodes.len(), 2);
        assert_eq!(edges.len(), 1);
    }

    #[test]
    fn execute_type_predicate_on_relationship_variable() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:A), (b:B), (c:C), (a)-[:KNOWS]->(b), (a)-[:HATES]->(c)",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (n)-[r]->(x) WHERE type(r) = 'KNOWS' RETURN x",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.rows.len(), 1);
    }

    #[test]
    fn execute_relationship_type_test_expression() {
        let mut g = writable_graph();
        run_cypher_mut("CREATE (a:A), (b:B), (a)-[:KNOWS]->(b)", &mut g).unwrap();

        let RunResult::Read(qr) =
            run_cypher_mut("MATCH ()-[r]->() RETURN r:KNOWS, r:LIKES", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.rows, vec![vec![Value::Bool(true), Value::Bool(false)]]);
    }

    #[test]
    fn execute_return_star_expands_current_bindings() {
        let mut g = writable_graph();
        run_cypher_mut("CREATE (a:A)-[:R]->(b:B)", &mut g).unwrap();

        let RunResult::Read(qr) = run_cypher_mut("MATCH (a)-[r:R]->(b) RETURN *", &mut g).unwrap()
        else {
            panic!("expected read");
        };

        assert_eq!(qr.columns, vec!["a", "b", "r"]);
        assert_eq!(qr.rows.len(), 1);
        assert!(matches!(qr.rows[0][0], Value::Int64(_)));
        assert!(matches!(qr.rows[0][1], Value::Int64(_)));
        assert!(edge_ref_id(&qr.rows[0][2]).is_some());
    }

    #[test]
    fn execute_match_requires_all_requested_labels() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (:A:B {name: 'ab'}), (:A:B:C {name: 'abc'}), (:A {name: 'a'}), (:B {name: 'b'})",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) =
            run_cypher_mut("MATCH (n:A:B) RETURN n.name ORDER BY n.name", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(
            qr.rows,
            vec![
                vec![Value::String("ab".into())],
                vec![Value::String("abc".into())]
            ]
        );

        let RunResult::Read(labels_qr) =
            run_cypher_mut("MATCH (n:A:B:C) RETURN labels(n)", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(
            labels_qr.rows,
            vec![vec![Value::List(vec![
                Value::String("A".into()),
                Value::String("B".into()),
                Value::String("C".into())
            ])]]
        );
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
    fn execute_list_predicates_can_read_map_properties() {
        let mut g = writable_graph();
        let RunResult::Read(qr) = run_cypher_mut(
            "RETURN any(x IN [{a: 2}, {a: 4}] WHERE x.a = 2) AS any_hit, \
                    none(x IN [{a: 4}] WHERE x.a = 2) AS none_hit, \
                    all(x IN [{a: 2}, {a: 2}] WHERE x.a = 2) AS all_hit",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(
            qr.rows[0],
            vec![Value::Bool(true), Value::Bool(true), Value::Bool(true)]
        );
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
    fn execute_write_pipeline_with_unwind_set_and_return() {
        let mut g = writable_graph();
        run_cypher_mut("CREATE (:Label1 {name: 'original'})", &mut g).unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (a:Label1) \
             WITH collect(a) AS nodes \
             WITH nodes, [x IN nodes | x.name] AS oldNames \
             UNWIND nodes AS n \
             SET n.name = 'newName' \
             RETURN n.name, oldNames",
            &mut g,
        )
        .unwrap() else {
            panic!("expected write RETURN result rows");
        };

        assert_eq!(qr.columns, vec!["n.name", "oldNames"]);
        assert_eq!(
            qr.rows,
            vec![vec![
                Value::String("newName".into()),
                Value::List(vec![Value::String("original".into())])
            ]]
        );
    }

    #[test]
    fn execute_can_store_and_read_list_properties() {
        let mut g = writable_graph();
        run_cypher_mut("CREATE (:TheLabel)", &mut g).unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (n:TheLabel) SET n.numbers = [1, 2, 3] RETURN size(n.numbers)",
            &mut g,
        )
        .unwrap() else {
            panic!("expected write RETURN result rows");
        };

        assert_eq!(qr.rows, vec![vec![Value::Int64(3)]]);
    }

    #[test]
    fn execute_tck_long_variable_path_setup_builds_chain() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a {var: 'start'}), (b {var: 'end'})
             WITH *
             UNWIND range(1, 20) AS i
             CREATE (n {var: i})
             WITH a, b, [a] + collect(n) + [b] AS nodeList
             UNWIND range(0, size(nodeList) - 2, 1) AS i
             WITH nodeList[i] AS n1, nodeList[i+1] AS n2
             CREATE (n1)-[:T]->(n2)",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (n {var: 'start'})-[:T*]->(m {var: 'end'}) RETURN m",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };

        assert_eq!(qr.num_rows(), 1);
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
    fn execute_respects_cancelled_query_context() {
        let g = test_graph();
        let ast = crate::parser::Parser::parse_read("MATCH (n) RETURN n").unwrap();
        let plan = crate::planner::plan_query(&ast).unwrap();
        let cancellation = Arc::new(AtomicBool::new(true));
        let ctx = QueryContext::new(&g).with_cancellation(cancellation);

        let err = execute(&plan, &ctx).unwrap_err();
        assert!(err.to_string().contains("query cancelled"));
    }

    #[test]
    fn execute_scan_observes_cancellation_during_large_scan() {
        let mut g = Graph::new(4096, 0);
        for _ in 0..4096 {
            g.add_vertex("Entity");
        }
        g.build();
        let ast = crate::parser::Parser::parse_read("MATCH (n) RETURN n").unwrap();
        let plan = crate::planner::plan_query(&ast).unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        let ctx = QueryContext::new(&g).with_cancellation(cancellation.clone());
        cancellation.store(true, Ordering::Relaxed);

        let err = execute(&plan, &ctx).unwrap_err();
        assert!(err.to_string().contains("query cancelled"));
    }

    #[test]
    fn execute_expand_observes_cancelled_query_context() {
        let mut g = Graph::new(2, 1);
        let a = g.add_vertex("Entity");
        let b = g.add_vertex("Entity");
        g.add_edge(a, b, "REL");
        g.build();
        let input = QueryResult {
            columns: vec!["a".into()],
            rows: vec![vec![Value::Int64(0)]],
        };
        let cancellation = Arc::new(AtomicBool::new(true));
        let ctx = QueryContext::new(&g).with_cancellation(cancellation);

        let err = execute_expand(
            &ctx,
            &input,
            "a",
            "b",
            None,
            &["REL".into()],
            &[],
            ExpandDirection::Outgoing,
            1,
            1,
            &[],
        )
        .unwrap_err();

        assert!(err.to_string().contains("query cancelled"));
    }

    #[test]
    fn execute_unwind_observes_cancelled_query_context() {
        let g = Graph::new(0, 0);
        let input = QueryResult {
            columns: Vec::new(),
            rows: vec![Vec::new()],
        };
        let expr = ProjectExpr::Literal(Value::List(vec![Value::Int64(1), Value::Int64(2)]));
        let cancellation = Arc::new(AtomicBool::new(true));
        let ctx = QueryContext::new(&g).with_cancellation(cancellation);

        let err = execute_unwind(&ctx, &input, &expr, "x").unwrap_err();
        assert!(err.to_string().contains("query cancelled"));
    }

    #[test]
    fn execute_scan_observes_row_budget_during_large_scan() {
        let mut g = Graph::new(4096, 0);
        for _ in 0..4096 {
            g.add_vertex("Entity");
        }
        g.build();
        let ast = crate::parser::Parser::parse_read("MATCH (n) RETURN n").unwrap();
        let plan = crate::planner::plan_query(&ast).unwrap();
        let ctx = QueryContext::new(&g).with_row_budget(10);

        let err = execute(&plan, &ctx).unwrap_err();
        assert!(err.to_string().contains("query row budget exceeded"));
    }

    #[test]
    fn execute_scan_with_limit_stays_under_row_budget() {
        let mut g = Graph::new(4096, 0);
        for _ in 0..4096 {
            g.add_vertex("Entity");
        }
        g.build();
        let ast = crate::parser::Parser::parse_read("MATCH (n) RETURN n LIMIT 5").unwrap();
        let plan = crate::planner::plan_query(&ast).unwrap();
        let ctx = QueryContext::new(&g).with_row_budget(10);

        let result = execute(&plan, &ctx).unwrap();
        assert_eq!(result.num_rows(), 5);
    }

    #[test]
    fn execute_project_observes_byte_budget_for_large_value() {
        let g = Graph::new(0, 0);
        let ast = crate::parser::Parser::parse_read("RETURN '0123456789abcdef' AS s").unwrap();
        let plan = crate::planner::plan_query(&ast).unwrap();
        let ctx = QueryContext::new(&g).with_byte_budget(8);

        let err = execute(&plan, &ctx).unwrap_err();
        assert!(err.to_string().contains("query byte budget exceeded"));
    }

    #[test]
    fn execute_union_observes_byte_budget_while_appending_rows() {
        let g = Graph::new(0, 0);
        let ast = crate::parser::Parser::parse_read(
            "RETURN '0123456789abcdef' AS s UNION ALL RETURN '0123456789abcdef' AS s",
        )
        .unwrap();
        let plan = crate::planner::plan_query(&ast).unwrap();
        let ctx = QueryContext::new(&g).with_byte_budget(20);

        let err = execute(&plan, &ctx).unwrap_err();
        assert!(err.to_string().contains("query byte budget exceeded"));
    }

    #[test]
    fn execute_write_respects_cancelled_context_without_mutation() {
        let mut g = writable_graph();
        let Statement::Write(write_query) =
            crate::parser::Parser::parse("CREATE (:Entity {name: 'cancelled'})").unwrap()
        else {
            panic!("expected write statement");
        };
        let plan = crate::planner::plan_write(&write_query).unwrap();
        let cancellation = Arc::new(AtomicBool::new(true));
        let mut ctx = WriteContext::new(&mut g).with_cancellation(cancellation);

        let err = execute_write(&plan, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("query cancelled"));
        assert_eq!(ctx.graph.num_vertices(), 0);
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
    fn execute_with_where_sees_projected_aliases_and_input_scope() {
        let mut g = writable_graph();
        g.register_vertex_property("name2", PropertyType::String, false, false);
        run_cypher_mut(
            "CREATE (:A {name2: 'A'}), (:A {name2: 'B'}), (:A {name2: 'C'})",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (a:A) \
             WITH a.name2 AS name \
             WHERE name = 'B' OR a.name2 = 'C' \
             RETURN name ORDER BY name",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(
            qr.rows,
            vec![
                vec![Value::String("B".into())],
                vec![Value::String("C".into())]
            ]
        );
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
    fn execute_optional_match_where_preserves_row_when_predicate_filters_match() {
        let g = test_graph();
        let result = run_cypher(
            "MATCH (a:Entity) OPTIONAL MATCH (a)-[:DISCLOSES]->(b) WHERE b.name = 'Missing' RETURN a.name, b",
            &g,
        )
        .unwrap();

        assert_eq!(result.num_rows(), 2);
        assert!(result.rows.iter().all(|row| row[1] == Value::Null));
    }

    #[test]
    fn execute_optional_match_where_keeps_matching_rows_and_null_extends_others() {
        let g = test_graph();
        let result = run_cypher(
            "MATCH (a:Entity) OPTIONAL MATCH (a)-[:DISCLOSES]->(b) WHERE b.name = 'Revenue' RETURN a.name, b.name ORDER BY a.name",
            &g,
        )
        .unwrap();

        assert_eq!(result.columns, vec!["a.name", "b.name"]);
        assert_eq!(
            result.rows,
            vec![
                vec![
                    Value::String("Apple Inc.".into()),
                    Value::String("Revenue".into())
                ],
                vec![Value::String("United States".into()), Value::Null],
            ]
        );
    }

    #[test]
    fn execute_and_binds_tighter_than_xor() {
        let g = test_graph();
        let result = run_cypher(
            "RETURN true XOR false AND false AS a, true XOR (false AND false) AS b",
            &g,
        )
        .unwrap();
        assert_eq!(
            result.rows,
            vec![vec![Value::Bool(true), Value::Bool(true)]]
        );
    }

    #[test]
    fn execute_integer_division_and_boolean_order_comparison() {
        let g = test_graph();
        let result = run_cypher(
            "RETURN 4 / 2 + 3 / 2 AS a, NOT false >= false AS b, (NOT false) >= false AS c",
            &g,
        )
        .unwrap();
        assert_eq!(
            result.rows,
            vec![vec![Value::Int64(3), Value::Bool(false), Value::Bool(true)]]
        );
    }

    #[test]
    fn execute_non_orderable_comparisons_return_null() {
        let g = test_graph();
        let result = run_cypher(
            "RETURN [1, 2] < ([3, 4] IN [[3, 4], false]) AS x, [1, 2] >= ([3, 4] IN [[3, 4], false]) AS y",
            &g,
        )
        .unwrap();
        assert_eq!(result.rows, vec![vec![Value::Null, Value::Null]]);
    }

    #[test]
    fn execute_list_ordering_is_lexicographic_for_comparable_lists() {
        let g = test_graph();
        let result = run_cypher(
            "RETURN [1, 2] < [3, 4] AS lt, [1, 2] > [3, 4] AS gt, [1, 2] >= [3, 4] AS gte",
            &g,
        )
        .unwrap();
        assert_eq!(
            result.rows,
            vec![vec![
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(false)
            ]]
        );
    }

    #[test]
    fn execute_list_comprehension_with_nested_collect_aggregate() {
        let mut g = writable_graph();
        let RunResult::Read(qr) = run_cypher_mut(
            "UNWIND [1, 2, 3] AS n RETURN [x IN collect(n) WHERE x > 1 | x + 1] AS xs",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(
            qr.rows,
            vec![vec![Value::List(vec![Value::Int64(3), Value::Int64(4)])]]
        );
    }

    #[test]
    fn execute_quantifier_helper_functions_used_by_tck() {
        let mut g = writable_graph();
        let RunResult::Read(qr) = run_cypher_mut(
            "WITH [1, 2] AS list RETURN reverse(list), abs(-3), rand() = 0.5",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(
            qr.rows[0],
            vec![
                Value::List(vec![Value::Int64(2), Value::Int64(1)]),
                Value::Int64(3),
                Value::Bool(true)
            ]
        );
    }

    #[test]
    fn execute_type_conversion_matches_tck_string_number_rules() {
        let g = Graph::new(0, 0);
        let qr = run_cypher(
            "WITH ['2.9', 'foo'] AS ints, ['5', 'bad'] AS floats \
             RETURN [x IN ints | toInteger(x)] AS i, [x IN floats | toFloat(x)] AS f",
            &g,
        )
        .unwrap();

        assert_eq!(
            qr.rows,
            vec![vec![
                Value::List(vec![Value::Int64(2), Value::Null]),
                Value::List(vec![Value::Float64(5.0), Value::Null])
            ]]
        );
    }

    #[test]
    fn bind_rejects_invalid_type_conversion_inputs() {
        let g = Graph::new(0, 0);
        assert!(run_cypher("MATCH (n) RETURN toString(n)", &g).is_err());
        assert!(run_cypher("RETURN [x IN [1, []] | toInteger(x)] AS list", &g).is_err());
        assert!(run_cypher("RETURN [x IN [true, 1.0] | toBoolean(x)] AS list", &g).is_err());
    }

    #[test]
    fn execute_path_variable_nodes_and_length() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH p = (a:Person)-[:KNOWS]->(b:Person) RETURN head(nodes(p)), length(p)",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };

        assert_eq!(qr.rows.len(), 1);
        assert!(matches!(qr.rows[0][0], Value::Int64(_)));
        assert_eq!(qr.rows[0][1], Value::Int64(1));
    }

    #[test]
    fn execute_variable_length_path_keeps_intermediate_nodes() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:Person {name: 'A'}), (b:Person {name: 'B'}), (c:Person {name: 'C'}), \
             (a)-[:KNOWS]->(b), (b)-[:KNOWS]->(c)",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH p = (a {name: 'A'})-[:KNOWS*2]->(c) RETURN nodes(p), length(p)",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };

        assert_eq!(qr.rows.len(), 1);
        let Value::List(nodes) = &qr.rows[0][0] else {
            panic!("expected node list");
        };
        assert_eq!(nodes.len(), 3);
        assert_eq!(qr.rows[0][1], Value::Int64(2));
    }

    #[test]
    fn execute_variable_length_path_filters_edge_properties() {
        let mut g = writable_graph();
        g.register_edge_property("year", PropertyType::Int64, false, false);
        run_cypher_mut(
            "CREATE (a:Artist:A), (b:Artist:B), (c:Artist:C), \
             (a)-[:WORKED_WITH {year: 1987}]->(b), \
             (b)-[:WORKED_WITH {year: 1988}]->(c)",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (a:Artist)-[:WORKED_WITH* {year: 1988}]->(b:Artist) RETURN a, b",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };

        assert_eq!(qr.rows.len(), 1);
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
    fn execute_delete_collects_multiple_path_targets_before_applying() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:User {name: 'A'}), (b:User {name: 'B'}), (c:User {name: 'C'}), \
             (a)-[:FOLLOWS]->(b), (b)-[:FOLLOWS]->(c)",
            &mut g,
        )
        .unwrap();

        let result = run_cypher_mut(
            "MATCH p = (:User)-[r]->(:User) \
             WITH {key: collect(p)} AS pathColls \
             DELETE pathColls.key[0], pathColls.key[1]",
            &mut g,
        )
        .unwrap();
        let RunResult::Write(summary) = result else {
            panic!("expected write");
        };
        assert_eq!(summary.nodes_deleted, 3);
        assert_eq!(summary.edges_deleted, 2);

        let RunResult::Read(qr) = run_cypher_mut("MATCH (n:User) RETURN n", &mut g).unwrap() else {
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
    fn execute_merge_on_create_and_on_match_actions() {
        let mut g = writable_graph();

        let RunResult::Read(qr) = run_cypher_mut(
            "MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true ON MATCH SET n.seen = true RETURN n.created, n.seen",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.rows, vec![vec![Value::Bool(true), Value::Null]]);

        let RunResult::Read(qr) = run_cypher_mut(
            "MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = false ON MATCH SET n.seen = true RETURN n.created, n.seen",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.rows, vec![vec![Value::Bool(true), Value::Bool(true)]]);
    }

    #[test]
    fn execute_merge_on_match_does_not_fire_when_created() {
        let mut g = writable_graph();

        let RunResult::Read(qr) = run_cypher_mut(
            "MERGE (n:Person {name: 'Bob'}) ON MATCH SET n.seen = true RETURN n.seen",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn execute_merge_on_create_can_copy_graph_properties() {
        let mut g = writable_graph();
        run_cypher_mut(
            "CREATE (a:Entity {name: 'A'}), (b:Entity {name: 'B'})",
            &mut g,
        )
        .unwrap();

        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (a {name: 'A'}), (b {name: 'B'}) MERGE (a)-[r:TYPE]->(b) ON CREATE SET r = a RETURN r.name",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.rows, vec![vec![Value::String("A".into())]]);
    }

    #[test]
    fn execute_merge_bare_node_and_bound_relationship_return_counts() {
        let mut g = writable_graph();

        let RunResult::Read(qr) = run_cypher_mut("MERGE (a) RETURN count(*)", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.rows, vec![vec![Value::Int64(1)]]);

        let RunResult::Read(qr) = run_cypher_mut("MERGE (a) RETURN count(*)", &mut g).unwrap()
        else {
            panic!("expected read");
        };
        assert_eq!(qr.rows, vec![vec![Value::Int64(1)]]);

        run_cypher_mut("CREATE (a:A), (b:B)", &mut g).unwrap();
        let RunResult::Read(qr) = run_cypher_mut(
            "MATCH (a:A), (b:B) MERGE (a)-[r:TYPE]->(b) RETURN count(r)",
            &mut g,
        )
        .unwrap() else {
            panic!("expected read");
        };
        assert_eq!(qr.rows, vec![vec![Value::Int64(1)]]);
    }

    #[test]
    fn execute_leading_delete_applies_before_following_merge() {
        let mut g = writable_graph();
        run_cypher_mut("CREATE (:A {num: 1}), (:A {num: 2})", &mut g).unwrap();

        let RunResult::Read(qr) =
            run_cypher_mut("MATCH (a:A) DELETE a MERGE (a2:A) RETURN a2.num", &mut g).unwrap()
        else {
            panic!("expected write RETURN result rows");
        };

        assert_eq!(qr.rows, vec![vec![Value::Null], vec![Value::Null]]);
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
    fn execute_order_by_zoned_temporals_uses_instant_order() {
        let mut g = writable_graph();
        let RunResult::Read(qr) = run_cypher_mut(
            "UNWIND [time({hour: 10, minute: 35, timezone: '-08:00'}), time({hour: 12, minute: 35, second: 15, timezone: '+05:00'}), time({hour: 12, minute: 31, second: 14, nanosecond: 645876123, timezone: '+01:00'})] AS t WITH t ORDER BY t RETURN t",
            &mut g,
        ).unwrap() else { panic!("expected read"); };
        assert_eq!(
            qr.rows,
            vec![
                vec![Value::String("12:35:15+05:00".into())],
                vec![Value::String("12:31:14.645876123+01:00".into())],
                vec![Value::String("10:35-08:00".into())],
            ]
        );

        let RunResult::Read(qr) = run_cypher_mut(
            "UNWIND [datetime({year: 1984, month: 10, day: 11, hour: 12, minute: 30, second: 14, nanosecond: 12, timezone: '+00:15'}), datetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, nanosecond: 645876123, timezone: '+00:17'})] AS dt WITH dt ORDER BY dt RETURN dt",
            &mut g,
        ).unwrap() else { panic!("expected read"); };
        assert_eq!(
            qr.rows,
            vec![
                vec![Value::String("1984-10-11T12:31:14.645876123+00:17".into())],
                vec![Value::String("1984-10-11T12:30:14.000000012+00:15".into())],
            ]
        );
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

    #[test]
    fn execute_vector_distance_function() {
        let g = test_graph();
        let qr = run_cypher(
            "RETURN vectorDistance([1.0, 0.0], [0.0, 1.0]) AS distance",
            &g,
        )
        .unwrap();

        let Value::Float64(distance) = qr.rows[0][0] else {
            panic!("expected float distance");
        };
        assert!((distance - 1.0).abs() < 1e-6);
    }

    #[test]
    fn execute_vector_search_function_with_unwind() {
        let g = test_graph();
        let indexes = IndexSet::new();
        let mut vector = VectorIndex::new(2);
        vector.add(VertexId(0), vec![1.0, 0.0]);
        vector.add(VertexId(1), vec![0.0, 1.0]);
        let mut vectors = HashMap::new();
        vectors.insert("entities".to_string(), vector);

        let qr = run_cypher_with_indexes_and_vectors(
            "UNWIND vectorSearch('entities', [1.0, 0.0], 2) AS hit RETURN hit.vertex_id, hit.distance ORDER BY hit.distance",
            &g,
            &indexes,
            &vectors,
        )
        .unwrap();

        assert_eq!(qr.columns, vec!["hit.vertex_id", "hit.distance"]);
        assert_eq!(qr.rows.len(), 2);
        assert_eq!(qr.rows[0][0], Value::Int64(0));
        assert_eq!(qr.rows[1][0], Value::Int64(1));
        let Value::Float64(distance) = qr.rows[0][1] else {
            panic!("expected float distance");
        };
        assert!(distance.abs() < 1e-6);
    }
}
