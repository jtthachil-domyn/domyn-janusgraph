//! The NexusEngine wraps graph + query execution + indexes into a thread-safe
//! handle that the HTTP and Bolt layers share.
//!
//! Internally uses `TransactionalGraph` for SWMR (single-writer, multi-reader)
//! isolation: read transactions get a consistent snapshot of the committed
//! state, while write transactions buffer mutations and apply them atomically.

use nexus_core::graph::Graph;
use nexus_core::properties::PropertyType;
use nexus_core::transaction::{ReadTx, TransactionalGraph, TxError, WriteOp, WriteTx};
use nexus_core::types::{Direction, EdgeId, Value, VertexId};
use nexus_cypher::ast::{
    Expr, Literal, Pattern, PatternElement, RelDirection, ReturnClause, Statement, WriteQuery,
};
use nexus_cypher::context::QueryContext;
use nexus_cypher::error::CypherError;
use nexus_cypher::executor::{QueryResult, execute, run_cypher};
use nexus_cypher::planner::{LogicalPlan, MutationOp, PropertyValue, WritePlan, plan_write};
use nexus_index::composite::IndexSet;
use nexus_parser::{NexusParser, NexusParserError, ParsedStatement, WriteClause, WriteStatement};
use nexus_storage::persistence::NexusStore;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub struct NexusEngine {
    txn_graph: Arc<TransactionalGraph>,
    indexes: Arc<RwLock<IndexSet>>,
    store: Option<Arc<RwLock<NexusStore>>>,
}

impl NexusEngine {
    pub fn new(graph: Graph) -> Self {
        Self {
            txn_graph: Arc::new(TransactionalGraph::new(graph)),
            indexes: Arc::new(RwLock::new(IndexSet::new())),
            store: None,
        }
    }

    pub fn with_indexes(graph: Graph, indexes: IndexSet) -> Self {
        Self {
            txn_graph: Arc::new(TransactionalGraph::new(graph)),
            indexes: Arc::new(RwLock::new(indexes)),
            store: None,
        }
    }

    pub fn with_store(graph: Graph, store: NexusStore) -> Self {
        let indexes = build_indexes_from_graph(&graph);
        Self::with_indexes_and_store(graph, indexes, store)
    }

    pub fn with_indexes_and_store(graph: Graph, indexes: IndexSet, store: NexusStore) -> Self {
        Self {
            txn_graph: Arc::new(TransactionalGraph::new(graph)),
            indexes: Arc::new(RwLock::new(indexes)),
            store: Some(Arc::new(RwLock::new(store))),
        }
    }

    /// Execute a Cypher query using available indexes for O(1) lookups.
    /// Opens a read transaction for the duration of the query.
    pub fn execute_cypher(&self, query: &str) -> Result<QueryResult, CypherError> {
        self.execute_cypher_with_params(query, HashMap::new())
    }

    /// Execute a Cypher query with bound parameters and index support.
    ///
    /// Native openCypher parser handles reads and CREATE / DELETE writes; kyu
    /// is consulted only when the native parser rejects the input entirely.
    pub fn execute_cypher_with_params(
        &self,
        query: &str,
        params: HashMap<String, Value>,
    ) -> Result<QueryResult, CypherError> {
        match nexus_cypher::parser::Parser::parse(query) {
            Ok(Statement::Read(ast)) => self.execute_read_ast(&ast, params),
            Ok(Statement::Write(wq)) => self.execute_native_write(&wq, params),
            Err(_native_err) => match NexusParser::parse_statement_kyu(query) {
                Ok(ParsedStatement::Read(parsed)) => self.execute_read_ast(&parsed.ast, params),
                Ok(ParsedStatement::Write(parsed)) => {
                    self.execute_write_statement(&parsed.statement, params)
                }
                Err(err) => Err(nexus_parser_error_to_cypher(err)),
            },
        }
    }

    /// Execute a native-parsed write statement (CREATE / DELETE) through the
    /// durable `execute_write` path so mutations are WAL-logged and indexes
    /// are refreshed.
    fn execute_native_write(
        &self,
        write_query: &WriteQuery,
        params: HashMap<String, Value>,
    ) -> Result<QueryResult, CypherError> {
        nexus_cypher::binder::bind_write(write_query)?;
        let plan = plan_write(write_query)?;
        let return_clause = write_query.return_clause.clone();

        self.execute_write(|wtx| {
            // Borrow the committed graph + indexes only for the duration of
            // binding resolution; drop them before the WriteTx's commit swap.
            let snapshot_rows = {
                let committed = self.txn_graph.committed().read();
                let idx = self.indexes.read();
                collect_source_bindings(&plan, &committed, &idx, &params)
                    .map_err(|e| TxError::Durability(format!("cypher read error: {e}")))?
            };

            let committed = self.txn_graph.committed().read();
            let result = apply_native_write(
                wtx,
                &committed,
                &plan,
                &snapshot_rows,
                &params,
                return_clause.as_ref(),
            )
            .map_err(|e| TxError::Durability(format!("cypher write error: {e}")));
            drop(committed);
            result
        })
        .map_err(|e| CypherError::Execution(e.to_string()))
    }

    fn execute_read_ast(
        &self,
        ast: &nexus_cypher::ast::Query,
        params: HashMap<String, Value>,
    ) -> Result<QueryResult, CypherError> {
        let rtx = self.txn_graph.begin_read();
        let g = rtx.graph();
        let idx = self.indexes.read();
        let ctx = QueryContext::with_indexes(g, &idx).with_params(params);
        nexus_cypher::binder::bind_query(ast)?;
        let plan = nexus_cypher::planner::plan_query(ast)?;
        execute(&plan, &ctx)
    }

    fn execute_write_statement(
        &self,
        statement: &WriteStatement,
        params: HashMap<String, Value>,
    ) -> Result<QueryResult, CypherError> {
        self.execute_write(|wtx| {
            execute_write_statement_in_tx(wtx, statement, &params)
                .map_err(|e| TxError::Durability(format!("cypher write error: {e}")))
        })
        .map_err(|e| CypherError::Execution(e.to_string()))
    }

    /// Execute without indexes (backward-compatible path).
    pub fn execute_cypher_no_index(&self, query: &str) -> Result<QueryResult, CypherError> {
        let rtx = self.txn_graph.begin_read();
        let g = rtx.graph();
        run_cypher(query, g)
    }

    pub fn begin_read(&self) -> ReadTx<'_> {
        self.txn_graph.begin_read()
    }

    /// Begin a volatile write transaction.
    ///
    /// For durable server writes, prefer `execute_write()`: direct commits from
    /// this handle do not append to the WAL.
    pub fn begin_write(&self) -> WriteTx<'_> {
        self.txn_graph.begin_write()
    }

    /// Execute a write operation within a transaction.
    /// The closure receives a mutable `WriteTx`; the transaction is committed
    /// automatically on success, aborted on error.
    pub fn execute_write<F, T>(&self, f: F) -> Result<T, TxError>
    where
        F: FnOnce(&mut WriteTx<'_>) -> Result<T, TxError>,
    {
        let mut wtx = self.txn_graph.begin_write();
        match f(&mut wtx) {
            Ok(val) => {
                let prepared = wtx.prepare()?;
                let ops = prepared.ops().to_vec();
                let index_changes = self.capture_index_changes(&ops);
                self.append_wal_and_sync(&ops)?;
                prepared.commit()?;
                if let Some(changes) = index_changes {
                    self.apply_index_changes(changes);
                } else {
                    self.rebuild_indexes();
                }
                Ok(val)
            }
            Err(e) => {
                wtx.abort();
                Err(e)
            }
        }
    }

    /// Backward-compatible accessor for the committed graph lock.
    /// Prefer `begin_read()` / `begin_write()` for new code.
    pub fn graph(&self) -> &Arc<RwLock<Graph>> {
        self.txn_graph.committed()
    }

    pub fn indexes(&self) -> &Arc<RwLock<IndexSet>> {
        &self.indexes
    }

    pub fn store(&self) -> Option<&Arc<RwLock<NexusStore>>> {
        self.store.as_ref()
    }

    pub fn rebuild_indexes(&self) {
        let rtx = self.txn_graph.begin_read();
        let rebuilt = build_indexes_from_graph(rtx.graph());
        *self.indexes.write() = rebuilt;
    }

    pub fn vertex_count(&self) -> usize {
        let rtx = self.txn_graph.begin_read();
        rtx.graph().num_vertices()
    }

    pub fn edge_count(&self) -> u64 {
        let rtx = self.txn_graph.begin_read();
        rtx.graph().num_edges()
    }

    pub fn vertex_labels(&self) -> Vec<String> {
        let rtx = self.txn_graph.begin_read();
        rtx.graph().vertex_label_names()
    }

    pub fn edge_labels(&self) -> Vec<String> {
        let rtx = self.txn_graph.begin_read();
        rtx.graph().edge_label_names()
    }

    fn append_wal_and_sync(&self, ops: &[WriteOp]) -> Result<(), TxError> {
        if ops.is_empty() {
            return Ok(());
        }

        let Some(store) = &self.store else {
            return Ok(());
        };

        let mut store = store.write();
        for op in ops {
            match op {
                WriteOp::AddVertex { id, label } => {
                    store
                        .log_add_vertex(id.0, label)
                        .map_err(|e| TxError::Durability(e.to_string()))?;
                }
                WriteOp::SetVertexProperty { vertex, key, value } => {
                    store
                        .log_set_vertex_property(vertex.0, key, value)
                        .map_err(|e| TxError::Durability(e.to_string()))?;
                }
                WriteOp::AddEdge {
                    edge_id,
                    source,
                    target,
                    label,
                } => {
                    store
                        .log_add_edge(edge_id.0, source.0, target.0, label)
                        .map_err(|e| TxError::Durability(e.to_string()))?;
                }
                WriteOp::SetEdgeProperty { edge, key, value } => {
                    store
                        .log_set_edge_property(edge.0, key, value)
                        .map_err(|e| TxError::Durability(e.to_string()))?;
                }
                WriteOp::RemoveVertex { vertex } => {
                    store
                        .log_remove_vertex(vertex.0)
                        .map_err(|e| TxError::Durability(e.to_string()))?;
                }
                WriteOp::RemoveEdge { edge } => {
                    store
                        .log_remove_edge(edge.0)
                        .map_err(|e| TxError::Durability(e.to_string()))?;
                }
            }
        }
        store
            .wal_mut()
            .sync()
            .map_err(|e| TxError::Durability(e.to_string()))?;
        Ok(())
    }

    fn capture_index_changes(&self, ops: &[WriteOp]) -> Option<Vec<IndexChange>> {
        let graph = self.txn_graph.committed().read();
        let indexed_props: Vec<String> = graph
            .vertex_property_defs()
            .filter(|def| def.indexed || def.unique)
            .map(|def| def.name.clone())
            .collect();
        let indexed_lookup: HashMap<String, ()> = indexed_props
            .iter()
            .map(|name| (name.clone(), ()))
            .collect();

        let mut changes: HashMap<(u64, String), IndexChange> = HashMap::new();
        for op in ops {
            match op {
                WriteOp::SetVertexProperty { vertex, key, value } => {
                    if !indexed_lookup.contains_key(key) {
                        continue;
                    }

                    let old_value = graph
                        .get_vertex_property(*vertex, key)
                        .as_str()
                        .map(str::to_string);
                    let new_value = value.as_str().map(str::to_string);
                    changes
                        .entry((vertex.0, key.clone()))
                        .and_modify(|change| change.new_value = new_value.clone())
                        .or_insert_with(|| IndexChange {
                            vertex: *vertex,
                            key: key.clone(),
                            old_value,
                            new_value,
                        });
                }
                WriteOp::RemoveVertex { vertex } => {
                    for key in &indexed_props {
                        let old_value = graph
                            .get_vertex_property(*vertex, key)
                            .as_str()
                            .map(str::to_string);
                        let change_key = (vertex.0, key.clone());
                        if old_value.is_none() {
                            if let Some(change) = changes.get_mut(&change_key) {
                                change.new_value = None;
                            }
                            continue;
                        }
                        changes
                            .entry(change_key)
                            .and_modify(|change| change.new_value = None)
                            .or_insert_with(|| IndexChange {
                                vertex: *vertex,
                                key: key.clone(),
                                old_value,
                                new_value: None,
                            });
                    }
                }
                _ => {}
            }
        }

        Some(changes.into_values().collect())
    }

    fn apply_index_changes(&self, changes: Vec<IndexChange>) {
        if changes.is_empty() {
            return;
        }

        let mut indexes = self.indexes.write();
        for change in changes {
            let unique_slot = indexes.find_unique(&change.key).map(|(idx, _)| idx);
            let composite_slot = indexes.find_composite(&change.key).map(|(idx, _)| idx);

            if let Some(old) = change.old_value.as_deref() {
                if let Some(idx) = unique_slot {
                    let _ = indexes.unique_mut(idx).unwrap().remove(old);
                }
                if let Some(idx) = composite_slot {
                    indexes
                        .composite_mut(idx)
                        .unwrap()
                        .remove(old, change.vertex);
                }
            }

            if let Some(new) = change.new_value.as_deref() {
                if let Some(idx) = unique_slot {
                    let _ = indexes.unique_mut(idx).unwrap().insert(new, change.vertex);
                }
                if let Some(idx) = composite_slot {
                    indexes
                        .composite_mut(idx)
                        .unwrap()
                        .insert(new, change.vertex);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreatedBinding {
    Vertex(VertexId),
    Edge(EdgeId),
}

/// Resolve binding rows for the optional source (MATCH+WHERE) of a native
/// write plan against a consistent read snapshot.
fn collect_source_bindings(
    plan: &WritePlan,
    snapshot: &Graph,
    indexes: &IndexSet,
    params: &HashMap<String, Value>,
) -> Result<QueryResult, CypherError> {
    if let Some(source) = &plan.source {
        let ctx = QueryContext::with_indexes(snapshot, indexes).with_params(params.clone());
        nexus_cypher::executor::execute(source, &ctx)
    } else {
        // One empty binding row so mutations execute exactly once.
        Ok(QueryResult {
            columns: Vec::new(),
            rows: vec![Vec::new()],
        })
    }
}

/// Apply native-parsed mutations to a WriteTx, per binding row.
/// Returns the projected RETURN result (empty if no RETURN clause).
fn apply_native_write(
    wtx: &mut WriteTx<'_>,
    snapshot: &Graph,
    plan: &WritePlan,
    source_rows: &QueryResult,
    params: &HashMap<String, Value>,
    return_clause: Option<&ReturnClause>,
) -> Result<QueryResult, CypherError> {
    let mut projected_rows: Vec<Vec<Value>> = Vec::new();
    let mut columns: Vec<String> = Vec::new();
    let relationship_vars = plan
        .source
        .as_ref()
        .map(relationship_variables)
        .unwrap_or_default();

    for row in &source_rows.rows {
        let mut bindings: HashMap<String, CreatedBinding> = HashMap::new();
        let mut vertex_properties: HashMap<VertexId, HashMap<String, Value>> = HashMap::new();
        let mut edge_properties: HashMap<EdgeId, HashMap<String, Value>> = HashMap::new();

        // Seed bindings from the source result (node vars -> VertexId,
        // relationship vars -> EdgeId).
        for (col_idx, col_name) in source_rows.columns.iter().enumerate() {
            if let Some(Value::Int64(id)) = row.get(col_idx) {
                let binding = if relationship_vars.contains(col_name) {
                    CreatedBinding::Edge(EdgeId(*id as u64))
                } else {
                    CreatedBinding::Vertex(VertexId(*id as u64))
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
                    let label = labels.first().ok_or_else(|| {
                        CypherError::Execution("CREATE node requires a label".into())
                    })?;
                    let vid = wtx.add_vertex(label);
                    for (key, pv) in properties {
                        let value = resolve_native_property_value(
                            pv,
                            params,
                            snapshot,
                            &bindings,
                            &vertex_properties,
                            &edge_properties,
                        )?;
                        wtx.set_vertex_property(vid, key, value.clone());
                        vertex_properties
                            .entry(vid)
                            .or_default()
                            .insert(key.clone(), value);
                    }
                    bindings.insert(variable.clone(), CreatedBinding::Vertex(vid));
                }
                MutationOp::CreateEdge {
                    variable,
                    src_var,
                    dst_var,
                    rel_type,
                    properties,
                } => {
                    let src = resolve_vertex_binding(&bindings, src_var)?;
                    let dst = resolve_vertex_binding(&bindings, dst_var)?;
                    let edge = wtx.add_edge(src, dst, rel_type);
                    for (key, pv) in properties {
                        let value = resolve_native_property_value(
                            pv,
                            params,
                            snapshot,
                            &bindings,
                            &vertex_properties,
                            &edge_properties,
                        )?;
                        wtx.set_edge_property(edge, key, value.clone());
                        edge_properties
                            .entry(edge)
                            .or_default()
                            .insert(key.clone(), value);
                    }
                    if let Some(variable) = variable {
                        bindings.insert(variable.clone(), CreatedBinding::Edge(edge));
                    }
                }
                MutationOp::MergeNode {
                    variable,
                    labels,
                    properties,
                } => {
                    if bindings.contains_key(variable) {
                        resolve_vertex_binding(&bindings, variable)?;
                        continue;
                    }
                    let label = labels.first().ok_or_else(|| {
                        CypherError::Execution("MERGE node requires a label".into())
                    })?;
                    let resolved_props = resolve_native_property_pairs(
                        properties,
                        params,
                        snapshot,
                        &bindings,
                        &vertex_properties,
                        &edge_properties,
                    )?;
                    let vid = find_matching_vertex(snapshot, label, &resolved_props)
                        .unwrap_or_else(|| {
                            let vid = wtx.add_vertex(label);
                            for (key, value) in &resolved_props {
                                wtx.set_vertex_property(vid, key, value.clone());
                                vertex_properties
                                    .entry(vid)
                                    .or_default()
                                    .insert(key.clone(), value.clone());
                            }
                            vid
                        });
                    bindings.insert(variable.clone(), CreatedBinding::Vertex(vid));
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
                            resolve_edge_binding(&bindings, variable)?;
                            continue;
                        }
                    }
                    let src = resolve_vertex_binding(&bindings, src_var)?;
                    let dst = resolve_vertex_binding(&bindings, dst_var)?;
                    let resolved_props = resolve_native_property_pairs(
                        properties,
                        params,
                        snapshot,
                        &bindings,
                        &vertex_properties,
                        &edge_properties,
                    )?;
                    let edge = snapshot
                        .edge_between(src, dst, rel_type)
                        .unwrap_or_else(|| {
                            let edge = wtx.add_edge(src, dst, rel_type);
                            for (key, value) in &resolved_props {
                                wtx.set_edge_property(edge, key, value.clone());
                                edge_properties
                                    .entry(edge)
                                    .or_default()
                                    .insert(key.clone(), value.clone());
                            }
                            edge
                        });
                    if let Some(variable) = variable {
                        bindings.insert(variable.clone(), CreatedBinding::Edge(edge));
                    }
                }
                MutationOp::SetProperty {
                    variable,
                    key,
                    value,
                } => {
                    let value = resolve_native_property_value(
                        value,
                        params,
                        snapshot,
                        &bindings,
                        &vertex_properties,
                        &edge_properties,
                    )?;
                    set_bound_property(
                        wtx,
                        &bindings,
                        variable,
                        key,
                        value,
                        &mut vertex_properties,
                        &mut edge_properties,
                    )?;
                }
                MutationOp::RemoveProperty { variable, key } => {
                    set_bound_property(
                        wtx,
                        &bindings,
                        variable,
                        key,
                        Value::Null,
                        &mut vertex_properties,
                        &mut edge_properties,
                    )?;
                }
                MutationOp::Delete { variable, detach } => {
                    match bindings.get(variable).copied() {
                        Some(CreatedBinding::Edge(edge)) => {
                            wtx.remove_edge(edge);
                            bindings.remove(variable);
                        }
                        Some(CreatedBinding::Vertex(vid)) => {
                            if !*detach && snapshot_has_incident_edges(snapshot, vid) {
                                return Err(CypherError::Execution(format!(
                                    "cannot DELETE node '{variable}' with relationships; use DETACH DELETE"
                                )));
                            }
                            // `try_remove_vertex` cascades to incident edges in the core
                            // graph, and WAL replay re-cascades on recovery, so only the
                            // RemoveVertex op needs to be buffered.
                            wtx.remove_vertex(vid);
                            bindings.remove(variable);
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

        if let Some(rc) = return_clause {
            let projected = project_write_return(
                Some(rc),
                params,
                &bindings,
                Some(snapshot),
                &vertex_properties,
                &edge_properties,
            )?;
            if columns.is_empty() {
                columns = projected.columns;
            }
            projected_rows.extend(projected.rows);
        }
    }

    if return_clause.is_some() {
        Ok(QueryResult {
            columns,
            rows: projected_rows,
        })
    } else {
        Ok(QueryResult::empty(Vec::new()))
    }
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

fn resolve_vertex_binding(
    bindings: &HashMap<String, CreatedBinding>,
    variable: &str,
) -> Result<VertexId, CypherError> {
    match bindings.get(variable) {
        Some(CreatedBinding::Vertex(vid)) => Ok(*vid),
        Some(CreatedBinding::Edge(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is bound to a relationship, not a node"
        ))),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

fn resolve_edge_binding(
    bindings: &HashMap<String, CreatedBinding>,
    variable: &str,
) -> Result<EdgeId, CypherError> {
    match bindings.get(variable) {
        Some(CreatedBinding::Edge(edge)) => Ok(*edge),
        Some(CreatedBinding::Vertex(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is bound to a node, not a relationship"
        ))),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

fn resolve_native_property_value(
    pv: &PropertyValue,
    params: &HashMap<String, Value>,
    snapshot: &Graph,
    bindings: &HashMap<String, CreatedBinding>,
    vertex_properties: &HashMap<VertexId, HashMap<String, Value>>,
    edge_properties: &HashMap<EdgeId, HashMap<String, Value>>,
) -> Result<Value, CypherError> {
    match pv {
        PropertyValue::Literal(v) => Ok(v.clone()),
        PropertyValue::Parameter(name) => params
            .get(name)
            .cloned()
            .ok_or_else(|| CypherError::Execution(format!("missing parameter '${name}'"))),
        PropertyValue::Property(pr) => resolve_bound_property(
            snapshot,
            bindings,
            vertex_properties,
            edge_properties,
            &pr.variable,
            &pr.property,
        ),
        PropertyValue::Expr(expr) => {
            // Project current write bindings as a synthetic row so arithmetic,
            // lists, maps, and function calls inside CREATE/MERGE property maps
            // can be evaluated at mutation time.
            let mut columns: Vec<String> = Vec::with_capacity(bindings.len());
            let mut row: Vec<Value> = Vec::with_capacity(bindings.len());
            for (name, binding) in bindings {
                columns.push(name.clone());
                row.push(match binding {
                    CreatedBinding::Vertex(v) => Value::Int64(v.0 as i64),
                    CreatedBinding::Edge(e) => Value::Int64(e.0 as i64),
                });
            }
            let idx = nexus_index::composite::IndexSet::new();
            let ctx = QueryContext::with_indexes(snapshot, &idx).with_params(params.clone());
            Ok(nexus_cypher::executor::eval_ast_expr(
                &ctx, expr, &row, &columns,
            ))
        }
    }
}

fn resolve_native_property_pairs(
    properties: &[(String, PropertyValue)],
    params: &HashMap<String, Value>,
    snapshot: &Graph,
    bindings: &HashMap<String, CreatedBinding>,
    vertex_properties: &HashMap<VertexId, HashMap<String, Value>>,
    edge_properties: &HashMap<EdgeId, HashMap<String, Value>>,
) -> Result<Vec<(String, Value)>, CypherError> {
    properties
        .iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                resolve_native_property_value(
                    value,
                    params,
                    snapshot,
                    bindings,
                    vertex_properties,
                    edge_properties,
                )?,
            ))
        })
        .collect()
}

fn set_bound_property(
    wtx: &mut WriteTx<'_>,
    bindings: &HashMap<String, CreatedBinding>,
    variable: &str,
    key: &str,
    value: Value,
    vertex_properties: &mut HashMap<VertexId, HashMap<String, Value>>,
    edge_properties: &mut HashMap<EdgeId, HashMap<String, Value>>,
) -> Result<(), CypherError> {
    match bindings.get(variable) {
        Some(CreatedBinding::Vertex(vertex)) => {
            wtx.set_vertex_property(*vertex, key, value.clone());
            vertex_properties
                .entry(*vertex)
                .or_default()
                .insert(key.to_string(), value);
            Ok(())
        }
        Some(CreatedBinding::Edge(edge)) => {
            wtx.set_edge_property(*edge, key, value.clone());
            edge_properties
                .entry(*edge)
                .or_default()
                .insert(key.to_string(), value);
            Ok(())
        }
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

fn resolve_bound_property(
    snapshot: &Graph,
    bindings: &HashMap<String, CreatedBinding>,
    vertex_properties: &HashMap<VertexId, HashMap<String, Value>>,
    edge_properties: &HashMap<EdgeId, HashMap<String, Value>>,
    variable: &str,
    key: &str,
) -> Result<Value, CypherError> {
    match bindings.get(variable) {
        Some(CreatedBinding::Vertex(vertex)) => Ok(vertex_properties
            .get(vertex)
            .and_then(|props| props.get(key))
            .cloned()
            .unwrap_or_else(|| snapshot.get_vertex_property(*vertex, key))),
        Some(CreatedBinding::Edge(edge)) => Ok(edge_properties
            .get(edge)
            .and_then(|props| props.get(key))
            .cloned()
            .unwrap_or_else(|| snapshot.get_edge_property(*edge, key))),
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

fn snapshot_has_incident_edges(graph: &Graph, vid: VertexId) -> bool {
    for label in graph.edge_label_names() {
        if !graph.neighbors(vid, &label, Direction::Outgoing).is_empty()
            || !graph.neighbors(vid, &label, Direction::Incoming).is_empty()
        {
            return true;
        }
    }
    false
}

fn execute_write_statement_in_tx(
    wtx: &mut WriteTx<'_>,
    statement: &WriteStatement,
    params: &HashMap<String, Value>,
) -> Result<QueryResult, CypherError> {
    let mut bindings = HashMap::new();
    let mut vertex_properties: HashMap<VertexId, HashMap<String, Value>> = HashMap::new();
    let edge_properties: HashMap<EdgeId, HashMap<String, Value>> = HashMap::new();

    for clause in &statement.clauses {
        match clause {
            WriteClause::Create(patterns) => {
                for pattern in patterns {
                    apply_create_pattern(
                        wtx,
                        pattern,
                        params,
                        &mut bindings,
                        &mut vertex_properties,
                    )?;
                }
            }
        }
    }

    project_write_return(
        statement.return_clause.as_ref(),
        params,
        &bindings,
        None,
        &vertex_properties,
        &edge_properties,
    )
}

fn apply_create_pattern(
    wtx: &mut WriteTx<'_>,
    pattern: &Pattern,
    params: &HashMap<String, Value>,
    bindings: &mut HashMap<String, CreatedBinding>,
    vertex_properties: &mut HashMap<VertexId, HashMap<String, Value>>,
) -> Result<(), CypherError> {
    let Some(PatternElement::Node(first_node)) = pattern.elements.first() else {
        return Err(CypherError::Execution(
            "CREATE pattern must start with a node".into(),
        ));
    };

    let mut current = ensure_created_node(wtx, first_node, params, bindings, vertex_properties)?;
    let mut index = 1;

    while index < pattern.elements.len() {
        let PatternElement::Relationship(rel) = &pattern.elements[index] else {
            return Err(CypherError::Execution(
                "CREATE pattern expected a relationship after node".into(),
            ));
        };
        let Some(PatternElement::Node(next_node)) = pattern.elements.get(index + 1) else {
            return Err(CypherError::Execution(
                "CREATE relationship must be followed by a node".into(),
            ));
        };

        let next = ensure_created_node(wtx, next_node, params, bindings, vertex_properties)?;
        let label = single_relationship_type(rel)?;
        let (source, target) = match rel.direction {
            RelDirection::Outgoing => (current, next),
            RelDirection::Incoming => (next, current),
            RelDirection::Both => {
                return Err(CypherError::Execution(
                    "CREATE does not support undirected relationships yet".into(),
                ));
            }
        };

        let edge = wtx.add_edge(source, target, label);
        if let Some(variable) = &rel.variable {
            bind_created_variable(bindings, variable, CreatedBinding::Edge(edge))?;
        }

        current = next;
        index += 2;
    }

    Ok(())
}

fn ensure_created_node(
    wtx: &mut WriteTx<'_>,
    node: &nexus_cypher::ast::NodePattern,
    params: &HashMap<String, Value>,
    bindings: &mut HashMap<String, CreatedBinding>,
    vertex_properties: &mut HashMap<VertexId, HashMap<String, Value>>,
) -> Result<VertexId, CypherError> {
    if let Some(variable) = &node.variable {
        if let Some(binding) = bindings.get(variable) {
            let CreatedBinding::Vertex(vertex) = binding else {
                return Err(CypherError::Execution(format!(
                    "variable '{variable}' is already bound to a relationship"
                )));
            };
            apply_node_properties(wtx, *vertex, node, params, vertex_properties)?;
            return Ok(*vertex);
        }
    }

    let label = single_node_label(node)?;
    let vertex = wtx.add_vertex(label);
    if let Some(variable) = &node.variable {
        bind_created_variable(bindings, variable, CreatedBinding::Vertex(vertex))?;
    }
    apply_node_properties(wtx, vertex, node, params, vertex_properties)?;
    Ok(vertex)
}

fn apply_node_properties(
    wtx: &mut WriteTx<'_>,
    vertex: VertexId,
    node: &nexus_cypher::ast::NodePattern,
    params: &HashMap<String, Value>,
    vertex_properties: &mut HashMap<VertexId, HashMap<String, Value>>,
) -> Result<(), CypherError> {
    for (key, expr) in &node.properties {
        let value = eval_write_value(expr, params)?;
        wtx.set_vertex_property(vertex, key, value.clone());
        vertex_properties
            .entry(vertex)
            .or_default()
            .insert(key.clone(), value);
    }
    Ok(())
}

fn bind_created_variable(
    bindings: &mut HashMap<String, CreatedBinding>,
    variable: &str,
    binding: CreatedBinding,
) -> Result<(), CypherError> {
    match bindings.get(variable) {
        Some(existing) if *existing == binding => Ok(()),
        Some(_) => Err(CypherError::Execution(format!(
            "variable '{variable}' is already bound in this CREATE"
        ))),
        None => {
            bindings.insert(variable.to_string(), binding);
            Ok(())
        }
    }
}

fn single_node_label(node: &nexus_cypher::ast::NodePattern) -> Result<&str, CypherError> {
    match node.labels.as_slice() {
        [label] => Ok(label),
        [] => Err(CypherError::Execution(
            "CREATE node currently requires exactly one label".into(),
        )),
        _ => Err(CypherError::Execution(
            "CREATE node currently supports only one label".into(),
        )),
    }
}

fn single_relationship_type(
    rel: &nexus_cypher::ast::RelationshipPattern,
) -> Result<&str, CypherError> {
    match rel.rel_types.as_slice() {
        [label] => Ok(label),
        [] => Err(CypherError::Execution(
            "CREATE relationship currently requires exactly one type".into(),
        )),
        _ => Err(CypherError::Execution(
            "CREATE relationship currently supports only one type".into(),
        )),
    }
}

fn project_write_return(
    return_clause: Option<&ReturnClause>,
    params: &HashMap<String, Value>,
    bindings: &HashMap<String, CreatedBinding>,
    snapshot: Option<&Graph>,
    vertex_properties: &HashMap<VertexId, HashMap<String, Value>>,
    edge_properties: &HashMap<EdgeId, HashMap<String, Value>>,
) -> Result<QueryResult, CypherError> {
    let Some(return_clause) = return_clause else {
        return Ok(QueryResult::empty(Vec::new()));
    };

    let mut columns = Vec::new();
    let mut row = Vec::new();

    for item in &return_clause.items {
        columns.push(
            item.alias
                .clone()
                .unwrap_or_else(|| write_expr_alias(&item.expr)),
        );
        row.push(eval_write_return_expr(
            &item.expr,
            params,
            bindings,
            snapshot,
            vertex_properties,
            edge_properties,
        )?);
    }

    Ok(QueryResult {
        columns,
        rows: vec![row],
    })
}

fn eval_write_return_expr(
    expr: &Expr,
    params: &HashMap<String, Value>,
    bindings: &HashMap<String, CreatedBinding>,
    snapshot: Option<&Graph>,
    vertex_properties: &HashMap<VertexId, HashMap<String, Value>>,
    edge_properties: &HashMap<EdgeId, HashMap<String, Value>>,
) -> Result<Value, CypherError> {
    match expr {
        Expr::Variable(variable) => binding_to_value(variable, bindings),
        Expr::Property(property) => {
            let binding = bindings.get(&property.variable).ok_or_else(|| {
                CypherError::Execution(format!(
                    "variable '{}' is not bound in this CREATE",
                    property.variable
                ))
            })?;
            match binding {
                CreatedBinding::Vertex(vertex) => Ok(vertex_properties
                    .get(vertex)
                    .and_then(|props| props.get(&property.property))
                    .cloned()
                    .or_else(|| {
                        snapshot.map(|graph| graph.get_vertex_property(*vertex, &property.property))
                    })
                    .unwrap_or(Value::Null)),
                CreatedBinding::Edge(edge) => Ok(edge_properties
                    .get(edge)
                    .and_then(|props| props.get(&property.property))
                    .cloned()
                    .or_else(|| {
                        snapshot.map(|graph| graph.get_edge_property(*edge, &property.property))
                    })
                    .unwrap_or(Value::Null)),
            }
        }
        Expr::Literal(literal) => Ok(literal_to_value(literal)),
        Expr::Parameter(name) => params
            .get(name)
            .cloned()
            .ok_or_else(|| CypherError::Execution(format!("missing parameter '${name}'"))),
        other => Err(CypherError::Execution(format!(
            "unsupported write RETURN expression: {other:?}"
        ))),
    }
}

fn binding_to_value(
    variable: &str,
    bindings: &HashMap<String, CreatedBinding>,
) -> Result<Value, CypherError> {
    match bindings.get(variable) {
        Some(CreatedBinding::Vertex(vertex)) => Ok(Value::Int64(vertex.0 as i64)),
        Some(CreatedBinding::Edge(edge)) => Ok(Value::Int64(edge.0 as i64)),
        None => Err(CypherError::Execution(format!(
            "variable '{variable}' is not bound in this CREATE"
        ))),
    }
}

fn eval_write_value(expr: &Expr, params: &HashMap<String, Value>) -> Result<Value, CypherError> {
    match expr {
        Expr::Literal(literal) => Ok(literal_to_value(literal)),
        Expr::Parameter(name) => params
            .get(name)
            .cloned()
            .ok_or_else(|| CypherError::Execution(format!("missing parameter '${name}'"))),
        other => Err(CypherError::Execution(format!(
            "unsupported CREATE property expression: {other:?}"
        ))),
    }
}

fn literal_to_value(literal: &Literal) -> Value {
    match literal {
        Literal::Integer(value) => Value::Int64(*value),
        Literal::Float(value) => Value::Float64(*value),
        Literal::String(value) => Value::String(value.clone()),
        Literal::Bool(value) => Value::Bool(*value),
        Literal::Null => Value::Null,
    }
}

fn write_expr_alias(expr: &Expr) -> String {
    match expr {
        Expr::Variable(variable) => variable.clone(),
        Expr::Property(property) => format!("{}.{}", property.variable, property.property),
        Expr::FunctionCall { name, args } => {
            let args = args.iter().map(write_expr_alias).collect::<Vec<_>>();
            format!("{}({})", name, args.join(", "))
        }
        _ => "expr".into(),
    }
}

fn nexus_parser_error_to_cypher(err: NexusParserError) -> CypherError {
    match err {
        NexusParserError::Native(err) => err,
        NexusParserError::Mapping(message) => CypherError::Plan(message),
        NexusParserError::Kyu(message) => CypherError::Parse {
            position: 0,
            message,
        },
    }
}

#[derive(Debug)]
struct IndexChange {
    vertex: VertexId,
    key: String,
    old_value: Option<String>,
    new_value: Option<String>,
}

/// Builder for constructing a graph and wrapping it in an engine.
pub struct EngineBuilder {
    graph: Graph,
}

impl EngineBuilder {
    pub fn new(vertex_cap: usize, edge_cap: usize) -> Self {
        Self {
            graph: Graph::new(vertex_cap, edge_cap),
        }
    }

    pub fn register_vertex_property(
        &mut self,
        name: &str,
        prop_type: PropertyType,
        indexed: bool,
        unique: bool,
    ) -> &mut Self {
        self.graph
            .register_vertex_property(name, prop_type, indexed, unique);
        self
    }

    pub fn register_edge_property(
        &mut self,
        name: &str,
        prop_type: PropertyType,
        indexed: bool,
        unique: bool,
    ) -> &mut Self {
        self.graph
            .register_edge_property(name, prop_type, indexed, unique);
        self
    }

    pub fn add_vertex(&mut self, label: &str) -> VertexId {
        self.graph.add_vertex(label)
    }

    pub fn set_vertex_property(&mut self, v: VertexId, key: &str, val: Value) {
        self.graph.set_vertex_property(v, key, val);
    }

    pub fn add_edge(&mut self, src: VertexId, dst: VertexId, label: &str) {
        self.graph.add_edge(src, dst, label);
    }

    /// Finalize the graph and build indexes from registered property schema.
    ///
    /// For every vertex property marked `indexed` or `unique`, this scans
    /// all vertices once and populates the corresponding index in the
    /// `IndexSet`, then hands both to `NexusEngine`.
    pub fn build(mut self) -> NexusEngine {
        self.graph.build();

        let indexes = build_indexes_from_graph(&self.graph);

        NexusEngine::with_indexes(self.graph, indexes)
    }
}

fn build_indexes_from_graph(graph: &Graph) -> IndexSet {
    let mut indexes = IndexSet::new();

    let indexed_props: Vec<(String, bool, bool)> = graph
        .vertex_property_defs()
        .filter(|def| def.indexed || def.unique)
        .map(|def| (def.name.clone(), def.indexed, def.unique))
        .collect();

    struct IndexSlot {
        prop_name: String,
        unique_slot: Option<usize>,
        composite_slot: Option<usize>,
    }

    let mut slots: Vec<IndexSlot> = Vec::new();
    for (name, indexed, unique) in &indexed_props {
        let mut slot = IndexSlot {
            prop_name: name.clone(),
            unique_slot: None,
            composite_slot: None,
        };
        if *unique {
            slot.unique_slot = Some(indexes.add_unique(name));
        }
        if *indexed && !*unique {
            slot.composite_slot = Some(indexes.add_composite(name));
        }
        slots.push(slot);
    }

    let num_v = graph.num_vertices();
    for vid_raw in 0..num_v as u64 {
        let vid = VertexId(vid_raw);
        if graph.vertex_label(vid).is_none() {
            continue;
        }
        for slot in &slots {
            let val = graph.get_vertex_property(vid, &slot.prop_name);
            if let Value::String(ref s) = val {
                if let Some(uidx) = slot.unique_slot {
                    let _ = indexes.unique_mut(uidx).unwrap().insert(s, vid);
                }
                if let Some(cidx) = slot.composite_slot {
                    indexes.composite_mut(cidx).unwrap().insert(s, vid);
                }
            }
        }
    }

    indexes
}

/// Routes queries to per-tenant `NexusEngine` instances.
///
/// Each tenant gets its own fully isolated graph. A default tenant can be
/// configured so that requests without an explicit `tenant` field are
/// routed automatically.
pub struct MultiTenantEngine {
    tenants: RwLock<HashMap<String, Arc<NexusEngine>>>,
    default_tenant: Option<String>,
}

impl MultiTenantEngine {
    pub fn new() -> Self {
        Self {
            tenants: RwLock::new(HashMap::new()),
            default_tenant: None,
        }
    }

    pub fn with_default(tenant_id: &str, engine: NexusEngine) -> Self {
        let mut tenants = HashMap::new();
        tenants.insert(tenant_id.to_string(), Arc::new(engine));
        Self {
            tenants: RwLock::new(tenants),
            default_tenant: Some(tenant_id.to_string()),
        }
    }

    /// Wrap an already-shared engine as the default tenant.
    pub fn new_with_arc(tenant_id: &str, engine: Arc<NexusEngine>) -> Self {
        let mut tenants = HashMap::new();
        tenants.insert(tenant_id.to_string(), engine);
        Self {
            tenants: RwLock::new(tenants),
            default_tenant: Some(tenant_id.to_string()),
        }
    }

    pub fn register_tenant(&self, tenant_id: &str, engine: NexusEngine) {
        self.tenants
            .write()
            .insert(tenant_id.to_string(), Arc::new(engine));
    }

    pub fn get_engine(&self, tenant_id: &str) -> Option<Arc<NexusEngine>> {
        self.tenants.read().get(tenant_id).cloned()
    }

    pub fn default_engine(&self) -> Option<Arc<NexusEngine>> {
        self.default_tenant
            .as_ref()
            .and_then(|id| self.tenants.read().get(id).cloned())
    }

    /// Resolve the engine for a request: explicit tenant, then default, then error.
    pub fn resolve(&self, tenant_id: Option<&str>) -> Result<Arc<NexusEngine>, String> {
        if let Some(id) = tenant_id {
            self.get_engine(id)
                .ok_or_else(|| format!("tenant not found: {id}"))
        } else {
            self.default_engine()
                .ok_or_else(|| "no tenant specified and no default configured".to_string())
        }
    }

    pub fn list_tenants(&self) -> Vec<String> {
        self.tenants.read().keys().cloned().collect()
    }

    pub fn remove_tenant(&self, tenant_id: &str) -> bool {
        self.tenants.write().remove(tenant_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_engine(num_vertices: usize) -> NexusEngine {
        let mut b = EngineBuilder::new(num_vertices + 1, 4);
        b.register_vertex_property("name", PropertyType::String, true, false);
        for i in 0..num_vertices {
            let v = b.add_vertex("Entity");
            b.set_vertex_property(v, "name", Value::String(format!("v{i}")));
        }
        b.build()
    }

    #[test]
    fn multi_tenant_engine_routing() {
        let mt = MultiTenantEngine::new();

        let e1 = build_engine(1);
        let e2 = build_engine(2);

        mt.register_tenant("AAPL", e1);
        mt.register_tenant("NVDA", e2);

        assert_eq!(mt.get_engine("AAPL").unwrap().vertex_count(), 1);
        assert_eq!(mt.get_engine("NVDA").unwrap().vertex_count(), 2);
        assert!(mt.get_engine("MISSING").is_none());
        assert_eq!(mt.list_tenants().len(), 2);
    }

    #[test]
    fn multi_tenant_default_engine() {
        let e = build_engine(3);
        let mt = MultiTenantEngine::with_default("default", e);

        assert!(mt.default_engine().is_some());
        assert_eq!(mt.default_engine().unwrap().vertex_count(), 3);
    }

    #[test]
    fn multi_tenant_resolve() {
        let mt = MultiTenantEngine::with_default("main", build_engine(1));
        mt.register_tenant("other", build_engine(5));

        assert_eq!(mt.resolve(None).unwrap().vertex_count(), 1);
        assert_eq!(mt.resolve(Some("other")).unwrap().vertex_count(), 5);
        assert!(mt.resolve(Some("nope")).is_err());
    }

    #[test]
    fn multi_tenant_remove() {
        let mt = MultiTenantEngine::new();
        mt.register_tenant("tmp", build_engine(1));
        assert!(mt.get_engine("tmp").is_some());
        assert!(mt.remove_tenant("tmp"));
        assert!(mt.get_engine("tmp").is_none());
        assert!(!mt.remove_tenant("tmp"));
    }

    #[test]
    fn multi_tenant_new_with_arc() {
        let engine = Arc::new(build_engine(4));
        let mt = MultiTenantEngine::new_with_arc("shared", engine.clone());
        assert_eq!(mt.get_engine("shared").unwrap().vertex_count(), 4);
        assert!(Arc::ptr_eq(&mt.get_engine("shared").unwrap(), &engine));
    }

    #[test]
    fn engine_builder_populates_indexes() {
        let mut b = EngineBuilder::new(10, 4);
        b.register_vertex_property("name", PropertyType::String, true, false);
        b.register_vertex_property("external_id", PropertyType::String, true, true);

        for i in 0..5u64 {
            let v = b.add_vertex("Entity");
            b.set_vertex_property(v, "name", format!("Entity_{i}").into());
            b.set_vertex_property(v, "external_id", format!("EXT_{i}").into());
        }

        let engine = b.build();
        let idx = engine.indexes().read();

        // external_id is unique -> unique index
        let (_, uidx) = idx
            .find_unique("external_id")
            .expect("unique index for external_id");
        assert_eq!(uidx.get("EXT_3"), Some(VertexId(3)));
        assert_eq!(uidx.len(), 5);

        // name is indexed but not unique -> composite index
        let (_, cidx) = idx
            .find_composite("name")
            .expect("composite index for name");
        assert_eq!(cidx.get("Entity_0").len(), 1);
    }

    #[test]
    fn engine_write_tx_through_execute_write() {
        let engine = build_engine(1);
        assert_eq!(engine.vertex_count(), 1);

        engine
            .execute_write(|wtx| {
                wtx.add_vertex("Entity");
                Ok(())
            })
            .unwrap();

        assert_eq!(engine.vertex_count(), 2);
    }

    #[test]
    fn engine_begin_read_write() {
        let engine = build_engine(2);

        {
            let rtx = engine.begin_read();
            assert_eq!(rtx.graph().num_vertices(), 2);
        }

        {
            let mut wtx = engine.begin_write();
            wtx.add_vertex("Entity");
            wtx.commit().unwrap();
        }

        let rtx = engine.begin_read();
        assert_eq!(rtx.graph().num_vertices(), 3);
    }

    #[test]
    fn execute_cypher_uses_indexes() {
        let mut b = EngineBuilder::new(100, 4);
        b.register_vertex_property("name", PropertyType::String, true, false);
        b.register_vertex_property("external_id", PropertyType::String, true, true);

        for i in 0..50u64 {
            let v = b.add_vertex("Entity");
            b.set_vertex_property(v, "name", format!("Entity_{i}").into());
            b.set_vertex_property(v, "external_id", format!("EXT_{i}").into());
        }

        let engine = b.build();
        let result = engine
            .execute_cypher("MATCH (n:Entity) WHERE n.external_id = 'EXT_25' RETURN n")
            .unwrap();

        assert_eq!(result.num_rows(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(25));
    }

    #[test]
    fn execute_write_rebuilds_indexes_after_mutation() {
        let engine = build_engine(1);

        engine
            .execute_write(|wtx| {
                let v = wtx.add_vertex("Entity");
                wtx.set_vertex_property(v, "name", Value::String("Bob".into()));
                Ok(())
            })
            .unwrap();

        let idx = engine.indexes().read();
        let (_, name_idx) = idx.find_composite("name").unwrap();
        assert!(name_idx.get("Bob").contains(&VertexId(1)));
    }

    #[test]
    fn execute_write_updates_indexes_incrementally() {
        let engine = build_engine(1);

        engine
            .execute_write(|wtx| {
                wtx.set_vertex_property(VertexId(0), "name", Value::String("Alice".into()));
                Ok(())
            })
            .unwrap();

        let idx = engine.indexes().read();
        let (_, name_idx) = idx.find_composite("name").unwrap();
        assert!(!name_idx.get("v0").contains(&VertexId(0)));
        assert!(name_idx.get("Alice").contains(&VertexId(0)));
    }

    #[test]
    fn execute_write_rebuilds_indexes_after_vertex_delete() {
        let engine = build_engine(2);

        engine
            .execute_write(|wtx| {
                wtx.remove_vertex(VertexId(0));
                Ok(())
            })
            .unwrap();

        let idx = engine.indexes().read();
        let (_, name_idx) = idx.find_composite("name").unwrap();
        assert!(!name_idx.get("v0").contains(&VertexId(0)));
        assert!(name_idx.get("v1").contains(&VertexId(1)));
    }

    #[test]
    fn execute_write_persists_to_wal_before_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let store = NexusStore::open(&db_path).unwrap();

        let mut graph = Graph::new(4, 4);
        graph.register_vertex_property("name", PropertyType::String, true, false);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);

        engine
            .execute_write(|wtx| {
                let v = wtx.add_vertex("Entity");
                wtx.set_vertex_property(v, "name", Value::String("Durable".into()));
                Ok(())
            })
            .unwrap();

        drop(engine);

        let store = NexusStore::open(&db_path).unwrap();
        let recovered = store.load_graph(4, 4).unwrap();

        assert_eq!(recovered.vertex_label(VertexId(0)), Some("Entity"));
        assert_eq!(
            recovered.get_vertex_property(VertexId(0), "name").as_str(),
            Some("Durable")
        );
    }

    #[test]
    fn execute_cypher_create_node_with_params_and_return() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        let engine = builder.build();

        let mut params = HashMap::new();
        params.insert("name".to_string(), Value::String("Alice".into()));

        let result = engine
            .execute_cypher_with_params("CREATE (n:Entity {name: $name}) RETURN n.name", params)
            .unwrap();

        assert_eq!(result.columns, vec!["n.name"]);
        assert_eq!(result.rows, vec![vec![Value::String("Alice".into())]]);
        assert_eq!(engine.vertex_count(), 1);

        let read_back = engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'Alice' RETURN n")
            .unwrap();
        assert_eq!(read_back.rows, vec![vec![Value::Int64(0)]]);
    }

    #[test]
    fn execute_cypher_create_edge_pattern() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        let engine = builder.build();

        let result = engine
            .execute_cypher(
                "CREATE (a:Entity {name: 'Alice'})-[:KNOWS]->(b:Entity {name: 'Bob'}) \
                 RETURN a.name, b.name",
            )
            .unwrap();

        assert_eq!(
            result.rows,
            vec![vec![
                Value::String("Alice".into()),
                Value::String("Bob".into())
            ]]
        );
        assert_eq!(engine.vertex_count(), 2);
        assert_eq!(engine.edge_count(), 1);

        let read_back = engine
            .execute_cypher("MATCH (a:Entity)-[:KNOWS]->(b:Entity) RETURN a.name, b.name")
            .unwrap();
        assert_eq!(read_back.num_rows(), 1);
        assert_eq!(read_back.rows[0][0], Value::String("Alice".into()));
        assert_eq!(read_back.rows[0][1], Value::String("Bob".into()));
    }

    #[test]
    fn execute_cypher_create_registers_missing_property_schema() {
        let engine = EngineBuilder::new(0, 0).build();

        let result = engine
            .execute_cypher("CREATE (n:Entity {name: 'NoSchema'}) RETURN n")
            .unwrap();

        assert_eq!(result.num_rows(), 1);
        assert_eq!(engine.vertex_count(), 1);

        let read_back = engine
            .execute_cypher("MATCH (n:Entity) RETURN n.name")
            .unwrap();
        assert_eq!(read_back.rows[0][0], Value::String("NoSchema".into()));
    }

    #[test]
    fn execute_cypher_create_type_error_is_atomic() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        let engine = builder.build();

        let err = engine
            .execute_cypher("CREATE (n:Entity {name: 42}) RETURN n")
            .unwrap_err();

        assert!(err.to_string().contains("property type mismatch"));
        assert_eq!(engine.vertex_count(), 0);
    }

    #[test]
    fn execute_cypher_create_persists_to_wal_before_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let store = NexusStore::open(&db_path).unwrap();

        let mut graph = Graph::new(0, 0);
        graph.register_vertex_property("name", PropertyType::String, true, false);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);

        engine
            .execute_cypher("CREATE (n:Entity {name: 'CypherDurable'}) RETURN n")
            .unwrap();

        drop(engine);

        let store = NexusStore::open(&db_path).unwrap();
        let recovered = store.load_graph(4, 4).unwrap();

        assert_eq!(recovered.vertex_label(VertexId(0)), Some("Entity"));
        assert_eq!(
            recovered.get_vertex_property(VertexId(0), "name").as_str(),
            Some("CypherDurable")
        );
    }

    // --- C4: native CREATE / DELETE via the WriteTx + WAL path ---

    fn empty_engine_with_store(db_path: &std::path::Path) -> NexusEngine {
        let store = NexusStore::open(db_path).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.register_vertex_property("name", PropertyType::String, true, false);
        graph.build();
        NexusEngine::with_store(graph, store)
    }

    #[test]
    fn execute_cypher_delete_removes_node() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        let engine = builder.build();

        engine
            .execute_cypher("CREATE (n:Entity {name: 'Alice'})")
            .unwrap();
        engine
            .execute_cypher("CREATE (n:Entity {name: 'Bob'})")
            .unwrap();
        assert_eq!(engine.vertex_count(), 2);

        engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'Alice' DELETE n")
            .unwrap();

        let remaining = engine
            .execute_cypher("MATCH (n:Entity) RETURN n.name")
            .unwrap();
        assert_eq!(remaining.num_rows(), 1);
        assert_eq!(remaining.rows[0][0], Value::String("Bob".into()));
    }

    #[test]
    fn execute_cypher_delete_rejects_connected_node_without_detach() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        let engine = builder.build();

        engine
            .execute_cypher("CREATE (a:Entity {name: 'A'})-[:KNOWS]->(b:Entity {name: 'B'})")
            .unwrap();
        assert_eq!(engine.edge_count(), 1);

        let err = engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'A' DELETE n")
            .unwrap_err();
        assert!(err.to_string().contains("relationships"), "got: {err}");

        // No side effects: both nodes still present, edge still present.
        assert_eq!(engine.vertex_count(), 2);
        assert_eq!(engine.edge_count(), 1);
    }

    #[test]
    fn execute_cypher_detach_delete_removes_node_and_edges() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        let engine = builder.build();

        engine
            .execute_cypher("CREATE (a:Entity {name: 'A'})-[:KNOWS]->(b:Entity {name: 'B'})")
            .unwrap();

        engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'A' DETACH DELETE n")
            .unwrap();

        // Edge is tombstoned — no longer queryable (num_edges counts tombstones).
        let traversal = engine
            .execute_cypher("MATCH (a:Entity)-[:KNOWS]->(b:Entity) RETURN a, b")
            .unwrap();
        assert_eq!(traversal.num_rows(), 0);

        let remaining = engine
            .execute_cypher("MATCH (n:Entity) RETURN n.name")
            .unwrap();
        assert_eq!(remaining.num_rows(), 1);
        assert_eq!(remaining.rows[0][0], Value::String("B".into()));
    }

    #[test]
    fn execute_cypher_set_and_remove_property_update_indexes() {
        let engine = build_engine(1);

        engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'v0' SET n.name = 'Alice'")
            .unwrap();

        {
            let idx = engine.indexes().read();
            let (_, name_idx) = idx.find_composite("name").unwrap();
            assert!(!name_idx.get("v0").contains(&VertexId(0)));
            assert!(name_idx.get("Alice").contains(&VertexId(0)));
        }

        let result = engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'Alice' RETURN n")
            .unwrap();
        assert_eq!(result.rows, vec![vec![Value::Int64(0)]]);

        engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'Alice' REMOVE n.name")
            .unwrap();

        {
            let idx = engine.indexes().read();
            let (_, name_idx) = idx.find_composite("name").unwrap();
            assert!(!name_idx.get("Alice").contains(&VertexId(0)));
        }

        let result = engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'Alice' RETURN n")
            .unwrap();
        assert_eq!(result.num_rows(), 0);
    }

    #[test]
    fn execute_cypher_create_then_delete_does_not_leave_index_entry() {
        let engine = build_engine(0);

        engine
            .execute_cypher("CREATE (n:Entity {name: 'Temp'}) DELETE n")
            .unwrap();

        let idx = engine.indexes().read();
        let (_, name_idx) = idx.find_composite("name").unwrap();
        assert!(name_idx.get("Temp").is_empty());

        let result = engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'Temp' RETURN n")
            .unwrap();
        assert_eq!(result.num_rows(), 0);
    }

    #[test]
    fn execute_cypher_set_edge_property_and_delete_relationship_variable() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        builder.register_edge_property("weight", PropertyType::Float64, false, false);
        let engine = builder.build();

        engine
            .execute_cypher("CREATE (a:Entity {name: 'A'})-[:KNOWS]->(b:Entity {name: 'B'})")
            .unwrap();

        engine
            .execute_cypher("MATCH (a:Entity)-[r:KNOWS]->(b:Entity) SET r.weight = 0.75")
            .unwrap();

        let result = engine
            .execute_cypher("MATCH (a:Entity)-[r:KNOWS]->(b:Entity) RETURN r.weight")
            .unwrap();
        assert_eq!(result.rows, vec![vec![Value::Float64(0.75)]]);

        engine
            .execute_cypher("MATCH (a:Entity)-[r:KNOWS]->(b:Entity) DELETE r")
            .unwrap();

        let traversal = engine
            .execute_cypher("MATCH (a:Entity)-[r:KNOWS]->(b:Entity) RETURN r")
            .unwrap();
        assert_eq!(traversal.num_rows(), 0);
    }

    #[test]
    fn execute_cypher_merge_node_and_edge_are_idempotent() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        let engine = builder.build();

        engine
            .execute_cypher("MERGE (n:Entity {name: 'Alice'})")
            .unwrap();
        engine
            .execute_cypher("MERGE (n:Entity {name: 'Alice'})")
            .unwrap();

        let result = engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'Alice' RETURN n")
            .unwrap();
        assert_eq!(result.num_rows(), 1);

        engine
            .execute_cypher("MERGE (a:Entity {name: 'Alice'})-[:KNOWS]->(b:Entity {name: 'Bob'})")
            .unwrap();
        engine
            .execute_cypher("MERGE (a:Entity {name: 'Alice'})-[:KNOWS]->(b:Entity {name: 'Bob'})")
            .unwrap();

        let traversal = engine
            .execute_cypher("MATCH (a:Entity)-[:KNOWS]->(b:Entity) RETURN a.name, b.name")
            .unwrap();
        assert_eq!(traversal.num_rows(), 1);
        assert_eq!(engine.vertex_count(), 2);
    }

    #[test]
    fn execute_cypher_delete_persists_to_wal_before_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let engine = empty_engine_with_store(&db_path);

        engine
            .execute_cypher("CREATE (n:Entity {name: 'Keep'})")
            .unwrap();
        engine
            .execute_cypher("CREATE (n:Entity {name: 'Gone'})")
            .unwrap();
        engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'Gone' DELETE n")
            .unwrap();

        drop(engine);

        let store = NexusStore::open(&db_path).unwrap();
        let recovered = store.load_graph(4, 4).unwrap();

        // Two vertices were added; one was tombstoned via WAL replay.
        assert_eq!(recovered.vertex_label(VertexId(0)), Some("Entity"));
        assert!(recovered.vertex_label(VertexId(1)).is_none()); // tombstoned
        assert_eq!(
            recovered.get_vertex_property(VertexId(0), "name").as_str(),
            Some("Keep")
        );
    }

    #[test]
    fn execute_cypher_set_and_remove_persist_to_wal_before_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let engine = empty_engine_with_store(&db_path);

        engine
            .execute_cypher("CREATE (n:Entity {name: 'Old'})")
            .unwrap();
        engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'Old' SET n.name = 'New'")
            .unwrap();

        drop(engine);

        let store = NexusStore::open(&db_path).unwrap();
        let recovered = store.load_graph(4, 4).unwrap();

        assert_eq!(
            recovered.get_vertex_property(VertexId(0), "name").as_str(),
            Some("New")
        );

        let engine = NexusEngine::with_store(recovered, store);
        engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'New' REMOVE n.name")
            .unwrap();

        drop(engine);

        let store = NexusStore::open(&db_path).unwrap();
        let recovered = store.load_graph(4, 4).unwrap();
        assert!(recovered.get_vertex_property(VertexId(0), "name").is_null());
    }

    #[test]
    fn execute_cypher_relationship_delete_persists_to_wal_before_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let engine = empty_engine_with_store(&db_path);

        engine
            .execute_cypher("CREATE (a:Entity {name: 'A'})-[:KNOWS]->(b:Entity {name: 'B'})")
            .unwrap();
        engine
            .execute_cypher("MATCH (a:Entity)-[r:KNOWS]->(b:Entity) DELETE r")
            .unwrap();

        drop(engine);

        let store = NexusStore::open(&db_path).unwrap();
        let recovered = store.load_graph(4, 4).unwrap();
        assert!(
            recovered
                .neighbors(VertexId(0), "KNOWS", Direction::Outgoing)
                .is_empty()
        );
    }

    #[test]
    fn execute_cypher_detach_delete_persists_to_wal_before_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let engine = empty_engine_with_store(&db_path);

        engine
            .execute_cypher("CREATE (a:Entity {name: 'A'})-[:KNOWS]->(b:Entity {name: 'B'})")
            .unwrap();
        engine
            .execute_cypher("MATCH (n:Entity) WHERE n.name = 'A' DETACH DELETE n")
            .unwrap();

        drop(engine);

        let store = NexusStore::open(&db_path).unwrap();
        let recovered = store.load_graph(4, 4).unwrap();

        // "A" vertex is tombstoned; incident edge cascaded on replay.
        assert!(recovered.vertex_label(VertexId(0)).is_none());
        assert_eq!(recovered.vertex_label(VertexId(1)), Some("Entity"));
        // Edge exists as a tombstone but is not queryable.
        let outgoing =
            recovered.neighbors(VertexId(0), "KNOWS", nexus_core::types::Direction::Outgoing);
        assert!(outgoing.is_empty());
        let incoming =
            recovered.neighbors(VertexId(1), "KNOWS", nexus_core::types::Direction::Incoming);
        assert!(incoming.is_empty());
    }

    #[test]
    fn execute_cypher_multi_create_in_one_statement() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        let engine = builder.build();

        engine
            .execute_cypher(
                "CREATE (a:Entity {name: 'A'}), (b:Entity {name: 'B'}), (c:Entity {name: 'C'})",
            )
            .unwrap();

        assert_eq!(engine.vertex_count(), 3);
    }
}
