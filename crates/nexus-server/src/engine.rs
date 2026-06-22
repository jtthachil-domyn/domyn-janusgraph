//! The NexusEngine wraps graph + query execution + indexes into a thread-safe
//! handle that the HTTP and Bolt layers share.
//!
//! Internally uses `TransactionalGraph` for SWMR (single-writer, multi-reader)
//! isolation: read transactions get a consistent snapshot of the committed
//! state, while write transactions buffer mutations and apply them atomically.

use nexus_core::graph::{Graph, GraphCompactionPressure, GraphCompactionStats};
use nexus_core::properties::PropertyType;
use nexus_core::transaction::{ReadTx, TransactionalGraph, TxError, WriteOp, WriteTx};
use nexus_core::types::{Direction, EdgeId, Value, VertexId};
use nexus_cypher::ast::{
    BinaryOp, Expr, Literal, Pattern, PatternElement, PropertyAccess, ReadClause, RelDirection,
    ReturnClause, ReturnItem, Statement, WriteQuery,
};
use nexus_cypher::context::{
    DocumentResolver, QueryContext, VectorSearchRecorder, estimate_query_result_bytes,
};
use nexus_cypher::error::CypherError;
use nexus_cypher::executor::{
    QueryResult, eval_row_count, execute, execute_sort, execute_write_read_clause,
    execute_write_return_columns, run_cypher,
};
use nexus_cypher::planner::{
    LogicalPlan, MutationOp, ProjectExpr, PropertyValue, WritePlan, plan_write,
};
use nexus_index::composite::IndexSet;
use nexus_index::vector::VectorIndex;
use nexus_parser::{NexusParser, NexusParserError, ParsedStatement, WriteClause, WriteStatement};
use nexus_storage::persistence::{
    BackupManifest, DocumentIndexDef, DocumentIndexKind, DocumentIndexQuery, DocumentRecord,
    DocumentSearchHit, NexusStore, StoreMetrics,
};
use nexus_storage::wal::WalOp;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub struct NexusEngine {
    txn_graph: Arc<TransactionalGraph>,
    indexes: Arc<RwLock<IndexSet>>,
    vector_indexes: Arc<RwLock<HashMap<String, VectorIndex>>>,
    vector_metrics: Arc<VectorSearchRuntime>,
    store: Option<Arc<RwLock<NexusStore>>>,
}

#[derive(Debug, Clone)]
pub enum DocumentMutation {
    Upsert {
        collection: String,
        key: String,
        document: serde_json::Value,
    },
    Delete {
        collection: String,
        key: String,
    },
}

#[derive(Debug, Clone)]
pub enum VectorMutation {
    Upsert {
        index: String,
        vertex: VertexId,
        embedding: Vec<f32>,
    },
    Remove {
        index: String,
        vertex: VertexId,
    },
}

#[derive(Debug)]
pub struct CrossModelBatchResult {
    pub cypher: Option<QueryResult>,
    pub cypher_results: Vec<QueryResult>,
    pub document_ops: usize,
    pub vector_ops: usize,
}

#[derive(Debug, Default)]
pub struct DocumentWriteBatch {
    ops: Vec<WalOp>,
}

impl DocumentWriteBatch {
    pub fn upsert(
        &mut self,
        collection: &str,
        key: &str,
        document: serde_json::Value,
    ) -> Result<(), TxError> {
        if !is_safe_document_name(collection) || !is_safe_document_name(key) {
            return Err(TxError::Durability(
                "document collection and key must be non-empty ASCII names using only letters, digits, '_' or '-'".into(),
            ));
        }
        self.ops.push(WalOp::UpsertDocument {
            collection: collection.to_string(),
            key: key.to_string(),
            document,
        });
        Ok(())
    }

    pub fn delete(&mut self, collection: &str, key: &str) -> Result<(), TxError> {
        if !is_safe_document_name(collection) || !is_safe_document_name(key) {
            return Err(TxError::Durability(
                "document collection and key must be non-empty ASCII names using only letters, digits, '_' or '-'".into(),
            ));
        }
        self.ops.push(WalOp::DeleteDocument {
            collection: collection.to_string(),
            key: key.to_string(),
        });
        Ok(())
    }

    pub fn ops(&self) -> &[WalOp] {
        &self.ops
    }

    pub fn upsert_vector(
        &mut self,
        index: &str,
        vertex: VertexId,
        embedding: Vec<f32>,
    ) -> Result<(), TxError> {
        if !is_safe_document_name(index) {
            return Err(TxError::Durability(
                "vector index name must be non-empty ASCII using only letters, digits, '_' or '-'"
                    .into(),
            ));
        }
        self.ops.push(WalOp::UpsertVector {
            index: index.to_string(),
            vertex_id: vertex.0,
            embedding,
        });
        Ok(())
    }

    pub fn remove_vector(&mut self, index: &str, vertex: VertexId) -> Result<(), TxError> {
        if !is_safe_document_name(index) {
            return Err(TxError::Durability(
                "vector index name must be non-empty ASCII using only letters, digits, '_' or '-'"
                    .into(),
            ));
        }
        self.ops.push(WalOp::RemoveVector {
            index: index.to_string(),
            vertex_id: vertex.0,
        });
        Ok(())
    }
}

fn is_safe_document_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

#[derive(Default)]
struct VectorSearchRuntime {
    global: VectorSearchCounters,
    by_index: RwLock<HashMap<String, VectorSearchCounters>>,
}

#[derive(Default)]
struct VectorSearchCounters {
    searches_total: AtomicU64,
    results_returned_total: AtomicU64,
    exact_candidates_total: AtomicU64,
    exact_overlap_total: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VectorSearchMetrics {
    pub searches_total: u64,
    pub results_returned_total: u64,
    pub exact_candidates_total: u64,
    pub exact_overlap_total: u64,
}

impl VectorSearchCounters {
    fn record(&self, results: usize, exact_candidates: usize, exact_overlap: usize) {
        self.searches_total.fetch_add(1, Ordering::Relaxed);
        self.results_returned_total
            .fetch_add(results as u64, Ordering::Relaxed);
        self.exact_candidates_total
            .fetch_add(exact_candidates as u64, Ordering::Relaxed);
        self.exact_overlap_total
            .fetch_add(exact_overlap as u64, Ordering::Relaxed);
    }

    fn snapshot(&self) -> VectorSearchMetrics {
        VectorSearchMetrics {
            searches_total: self.searches_total.load(Ordering::Relaxed),
            results_returned_total: self.results_returned_total.load(Ordering::Relaxed),
            exact_candidates_total: self.exact_candidates_total.load(Ordering::Relaxed),
            exact_overlap_total: self.exact_overlap_total.load(Ordering::Relaxed),
        }
    }
}

impl VectorSearchRuntime {
    fn record(
        &self,
        index_name: &str,
        results: usize,
        exact_candidates: usize,
        exact_overlap: usize,
    ) {
        self.global.record(results, exact_candidates, exact_overlap);
        self.by_index
            .write()
            .entry(index_name.to_string())
            .or_default()
            .record(results, exact_candidates, exact_overlap);
    }

    fn snapshot(&self) -> VectorSearchMetrics {
        self.global.snapshot()
    }

    fn snapshot_by_index(&self) -> HashMap<String, VectorSearchMetrics> {
        self.by_index
            .read()
            .iter()
            .map(|(name, counters)| (name.clone(), counters.snapshot()))
            .collect()
    }
}

impl VectorSearchRecorder for VectorSearchRuntime {
    fn record_vector_search(
        &self,
        index_name: &str,
        results_returned: usize,
        exact_candidates: usize,
        exact_overlap: usize,
    ) {
        self.record(
            index_name,
            results_returned,
            exact_candidates,
            exact_overlap,
        );
    }
}

fn json_to_value(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(value),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Value::Int64(value)
            } else {
                Value::Float64(number.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(value) => Value::String(value),
        serde_json::Value::Array(values) => {
            Value::List(values.into_iter().map(json_to_value).collect())
        }
        serde_json::Value::Object(map) => Value::Map(
            map.into_iter()
                .map(|(key, value)| (key, json_to_value(value)))
                .collect(),
        ),
    }
}

fn value_to_json(value: &Value) -> Option<serde_json::Value> {
    match value {
        Value::Null => Some(serde_json::Value::Null),
        Value::Bool(value) => Some(serde_json::Value::Bool(*value)),
        Value::Int64(value) => Some(serde_json::json!(value)),
        Value::Float64(value) => {
            serde_json::Number::from_f64(*value).map(serde_json::Value::Number)
        }
        Value::String(value) => Some(serde_json::Value::String(value.clone())),
        Value::Bytes(_) => None,
        Value::List(values) => values
            .iter()
            .map(value_to_json)
            .collect::<Option<Vec<_>>>()
            .map(serde_json::Value::Array),
        Value::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (key, value) in entries {
                map.insert(key.clone(), value_to_json(value)?);
            }
            Some(serde_json::Value::Object(map))
        }
    }
}

fn document_record_to_value(record: DocumentRecord) -> Value {
    Value::Map(vec![
        ("collection".into(), Value::String(record.collection)),
        ("key".into(), Value::String(record.key)),
        ("document".into(), json_to_value(record.document)),
    ])
}

impl DocumentResolver for NexusEngine {
    fn resolve_document(&self, collection: &str, key: &str) -> Option<Value> {
        self.load_document(collection, key)
            .ok()
            .flatten()
            .map(json_to_value)
    }

    fn scan_documents(&self, collection: &str, limit: usize) -> Vec<Value> {
        self.list_documents(collection, Some(limit))
            .unwrap_or_default()
            .into_iter()
            .map(document_record_to_value)
            .collect()
    }

    fn query_documents_by_index(
        &self,
        collection: &str,
        path: &str,
        value: &Value,
        limit: usize,
    ) -> Vec<Value> {
        let Some(value) = value_to_json(value) else {
            return Vec::new();
        };
        match self.query_documents_by_index(collection, path, value.clone(), Some(limit)) {
            Ok(records) => records.into_iter().map(document_record_to_value).collect(),
            Err(_) => self
                .list_documents(collection, Some(limit))
                .unwrap_or_default()
                .into_iter()
                .filter(|record| {
                    json_path_value(&record.document, path).is_some_and(|found| found == &value)
                })
                .map(document_record_to_value)
                .collect(),
        }
    }

    fn query_documents_by_prefix(
        &self,
        collection: &str,
        path: &str,
        prefix: &str,
        limit: usize,
    ) -> Vec<Value> {
        match self.query_documents_by_index_with(
            collection,
            path,
            DocumentIndexQuery::Prefix(prefix.to_string()),
            Some(limit),
        ) {
            Ok(records) => records.into_iter().map(document_record_to_value).collect(),
            Err(_) => self
                .list_documents(collection, Some(limit))
                .unwrap_or_default()
                .into_iter()
                .filter(|record| {
                    json_path_value(&record.document, path)
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|found| found.starts_with(prefix))
                })
                .map(document_record_to_value)
                .collect(),
        }
    }

    fn query_documents_by_range(
        &self,
        collection: &str,
        path: &str,
        gte: Option<&Value>,
        lte: Option<&Value>,
        limit: usize,
    ) -> Vec<Value> {
        let gte_json = gte.and_then(value_to_json);
        let lte_json = lte.and_then(value_to_json);
        match self.query_documents_by_index_with(
            collection,
            path,
            DocumentIndexQuery::Range {
                gte: gte_json.clone(),
                lte: lte_json.clone(),
            },
            Some(limit),
        ) {
            Ok(records) => records.into_iter().map(document_record_to_value).collect(),
            Err(_) => self
                .list_documents(collection, Some(limit))
                .unwrap_or_default()
                .into_iter()
                .filter(|record| {
                    json_path_value(&record.document, path).is_some_and(|found| {
                        json_scalar_in_range(found, gte_json.as_ref(), lte_json.as_ref())
                    })
                })
                .map(document_record_to_value)
                .collect(),
        }
    }

    fn query_documents_by_full_text(
        &self,
        collection: &str,
        path: &str,
        query: &str,
        limit: usize,
    ) -> Vec<Value> {
        match self.query_documents_by_index_with(
            collection,
            path,
            DocumentIndexQuery::FullText(query.to_string()),
            Some(limit),
        ) {
            Ok(records) => records.into_iter().map(document_record_to_value).collect(),
            Err(_) => self
                .list_documents(collection, Some(limit))
                .unwrap_or_default()
                .into_iter()
                .filter(|record| {
                    json_path_value(&record.document, path)
                        .is_some_and(|found| json_full_text_matches(found, query))
                })
                .map(document_record_to_value)
                .collect(),
        }
    }
}

fn json_path_value<'a>(
    document: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let mut current = document;
    for segment in path.split('.') {
        current = current.as_object()?.get(segment)?;
    }
    Some(current)
}

fn json_scalar_in_range(
    value: &serde_json::Value,
    gte: Option<&serde_json::Value>,
    lte: Option<&serde_json::Value>,
) -> bool {
    if matches!(
        value,
        serde_json::Value::Null | serde_json::Value::Array(_) | serde_json::Value::Object(_)
    ) {
        return false;
    }
    if let Some(lower) = gte
        && !json_scalar_cmp(value, lower).is_some_and(|ord| {
            matches!(ord, std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        })
    {
        return false;
    }
    if let Some(upper) = lte
        && !json_scalar_cmp(value, upper)
            .is_some_and(|ord| matches!(ord, std::cmp::Ordering::Less | std::cmp::Ordering::Equal))
    {
        return false;
    }
    true
}

fn json_scalar_cmp(
    left: &serde_json::Value,
    right: &serde_json::Value,
) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (serde_json::Value::String(left), serde_json::Value::String(right)) => {
            Some(left.cmp(right))
        }
        (serde_json::Value::Number(left), serde_json::Value::Number(right)) => {
            let left = left.as_f64()?;
            let right = right.as_f64()?;
            left.partial_cmp(&right)
        }
        (serde_json::Value::Bool(left), serde_json::Value::Bool(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

fn json_full_text_matches(value: &serde_json::Value, query: &str) -> bool {
    let query_tokens = json_text_tokens(query);
    if query_tokens.is_empty() {
        return false;
    }
    let mut value_tokens = std::collections::BTreeSet::new();
    collect_json_text_tokens(value, &mut value_tokens);
    query_tokens
        .iter()
        .any(|token| value_tokens.contains(token.as_str()))
}

fn collect_json_text_tokens(
    value: &serde_json::Value,
    tokens: &mut std::collections::BTreeSet<String>,
) {
    match value {
        serde_json::Value::String(text) => {
            tokens.extend(json_text_tokens(text));
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_json_text_tokens(value, tokens);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values() {
                collect_json_text_tokens(value, tokens);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn json_text_tokens(text: &str) -> std::collections::BTreeSet<String> {
    let mut tokens = std::collections::BTreeSet::new();
    for token in text.split(|ch: char| !ch.is_alphanumeric()) {
        if token.chars().count() < 2 {
            continue;
        }
        tokens.insert(token.to_lowercase());
    }
    tokens
}

fn write_op_to_wal_op(op: &WriteOp) -> WalOp {
    match op {
        WriteOp::AddVertex { id, label } => WalOp::AddVertex {
            id: id.0,
            label: label.clone(),
        },
        WriteOp::SetVertexProperty { vertex, key, value } => WalOp::SetVertexProperty {
            vertex_id: vertex.0,
            key: key.clone(),
            value: value.clone(),
        },
        WriteOp::SetVertexLabel { vertex, label } => WalOp::SetVertexLabel {
            vertex_id: vertex.0,
            label: label.clone(),
        },
        WriteOp::AddEdge {
            edge_id,
            source,
            target,
            label,
        } => WalOp::AddEdge {
            edge_id: edge_id.0,
            source: source.0,
            target: target.0,
            label: label.clone(),
        },
        WriteOp::SetEdgeProperty { edge, key, value } => WalOp::SetEdgeProperty {
            edge_id: edge.0,
            key: key.clone(),
            value: value.clone(),
        },
        WriteOp::RemoveVertex { vertex } => WalOp::RemoveVertex {
            vertex_id: vertex.0,
        },
        WriteOp::RemoveEdge { edge } => WalOp::RemoveEdge { edge_id: edge.0 },
    }
}

fn apply_write_ops_to_graph(staged: &mut Graph, ops: &[WriteOp]) -> Result<(), TxError> {
    for op in ops {
        match op {
            WriteOp::AddVertex { id, label } => {
                staged.try_add_vertex_with_id(id.0, label)?;
            }
            WriteOp::SetVertexProperty { vertex, key, value } => {
                staged.try_set_vertex_property(*vertex, key, value.clone())?;
            }
            WriteOp::SetVertexLabel { vertex, label } => {
                staged.try_set_vertex_label(*vertex, label)?;
            }
            WriteOp::AddEdge {
                edge_id,
                source,
                target,
                label,
            } => {
                staged.try_add_edge_with_id(edge_id.0, *source, *target, label)?;
            }
            WriteOp::SetEdgeProperty { edge, key, value } => {
                staged.try_set_edge_property(*edge, key, value.clone())?;
            }
            WriteOp::RemoveVertex { vertex } => {
                staged.try_remove_vertex(*vertex)?;
            }
            WriteOp::RemoveEdge { edge } => {
                staged.try_remove_edge(*edge)?;
            }
        }
    }
    Ok(())
}

impl NexusEngine {
    pub fn new(graph: Graph) -> Self {
        Self {
            txn_graph: Arc::new(TransactionalGraph::new(graph)),
            indexes: Arc::new(RwLock::new(IndexSet::new())),
            vector_indexes: Arc::new(RwLock::new(HashMap::new())),
            vector_metrics: Arc::new(VectorSearchRuntime::default()),
            store: None,
        }
    }

    pub fn with_indexes(graph: Graph, indexes: IndexSet) -> Self {
        Self {
            txn_graph: Arc::new(TransactionalGraph::new(graph)),
            indexes: Arc::new(RwLock::new(indexes)),
            vector_indexes: Arc::new(RwLock::new(HashMap::new())),
            vector_metrics: Arc::new(VectorSearchRuntime::default()),
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
            vector_indexes: Arc::new(RwLock::new(HashMap::new())),
            vector_metrics: Arc::new(VectorSearchRuntime::default()),
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
        self.execute_cypher_with_params_and_cancellation(query, params, None)
    }

    pub fn execute_cypher_with_params_and_cancellation(
        &self,
        query: &str,
        params: HashMap<String, Value>,
        cancellation: Option<Arc<AtomicBool>>,
    ) -> Result<QueryResult, CypherError> {
        self.execute_cypher_with_params_cancellation_and_row_budget(
            query,
            params,
            cancellation,
            None,
        )
    }

    pub fn execute_cypher_with_params_cancellation_and_row_budget(
        &self,
        query: &str,
        params: HashMap<String, Value>,
        cancellation: Option<Arc<AtomicBool>>,
        row_budget: Option<usize>,
    ) -> Result<QueryResult, CypherError> {
        self.execute_cypher_with_params_cancellation_and_limits(
            query,
            params,
            cancellation,
            row_budget,
            None,
        )
    }

    pub fn execute_cypher_with_params_cancellation_and_limits(
        &self,
        query: &str,
        params: HashMap<String, Value>,
        cancellation: Option<Arc<AtomicBool>>,
        row_budget: Option<usize>,
        byte_budget: Option<usize>,
    ) -> Result<QueryResult, CypherError> {
        match nexus_cypher::parser::Parser::parse(query) {
            Ok(Statement::Read(ast)) => {
                self.execute_read_ast(&ast, params, cancellation, row_budget, byte_budget)
            }
            Ok(Statement::Write(wq)) => {
                self.execute_native_write(&wq, params, cancellation, row_budget, byte_budget)
            }
            Err(native_err) => {
                if native_entrypoint_claims(query) {
                    return Err(native_err);
                }
                match NexusParser::parse_statement_kyu(query) {
                    Ok(ParsedStatement::Read(parsed)) => self.execute_read_ast(
                        &parsed.ast,
                        params,
                        cancellation,
                        row_budget,
                        byte_budget,
                    ),
                    Ok(ParsedStatement::Write(parsed)) => {
                        self.execute_write_statement(&parsed.statement, params, cancellation)
                    }
                    Err(err) => Err(nexus_parser_error_to_cypher(err)),
                }
            }
        }
    }

    /// Execute a native-parsed write statement (CREATE / DELETE) through the
    /// durable `execute_write` path so mutations are WAL-logged and indexes
    /// are refreshed.
    fn execute_native_write(
        &self,
        write_query: &WriteQuery,
        params: HashMap<String, Value>,
        cancellation: Option<Arc<AtomicBool>>,
        row_budget: Option<usize>,
        byte_budget: Option<usize>,
    ) -> Result<QueryResult, CypherError> {
        check_cancellation_token(cancellation.as_ref())?;
        nexus_cypher::binder::bind_write(write_query)?;
        check_cancellation_token(cancellation.as_ref())?;
        let plan = plan_write(write_query)?;
        check_cancellation_token(cancellation.as_ref())?;
        self.execute_write(|wtx| {
            // Borrow the committed graph + indexes only for the duration of
            // binding resolution; drop them before the WriteTx's commit swap.
            let snapshot_rows = {
                let committed = self.txn_graph.committed().read();
                let idx = self.indexes.read();
                collect_source_bindings(
                    &plan,
                    &committed,
                    &idx,
                    &params,
                    cancellation.clone(),
                    row_budget,
                    byte_budget,
                )
                .map_err(|e| TxError::Durability(format!("cypher read error: {e}")))?
            };
            check_cancellation_token(cancellation.as_ref())
                .map_err(|e| TxError::Durability(format!("cypher write error: {e}")))?;

            let committed = self.txn_graph.committed().read();
            let result = apply_native_write(
                wtx,
                &committed,
                &plan,
                &snapshot_rows,
                &params,
                cancellation.clone(),
                row_budget,
                byte_budget,
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
        cancellation: Option<Arc<AtomicBool>>,
        row_budget: Option<usize>,
        byte_budget: Option<usize>,
    ) -> Result<QueryResult, CypherError> {
        let rtx = self.txn_graph.begin_read();
        let g = rtx.graph();
        let idx = self.indexes.read();
        let vector_indexes = self.vector_indexes.read();
        let mut ctx = QueryContext::with_indexes(g, &idx)
            .with_vector_indexes(&vector_indexes)
            .with_vector_recorder(self.vector_metrics.as_ref())
            .with_document_resolver(self)
            .with_params(params);
        if let Some(cancellation) = cancellation {
            ctx = ctx.with_cancellation(cancellation);
        }
        if let Some(row_budget) = row_budget {
            ctx = ctx.with_row_budget(row_budget);
        }
        if let Some(byte_budget) = byte_budget {
            ctx = ctx.with_byte_budget(byte_budget);
        }
        nexus_cypher::binder::bind_query(ast)?;
        let plan = nexus_cypher::planner::plan_query(ast)?;
        execute(&plan, &ctx)
    }

    fn execute_write_statement(
        &self,
        statement: &WriteStatement,
        params: HashMap<String, Value>,
        cancellation: Option<Arc<AtomicBool>>,
    ) -> Result<QueryResult, CypherError> {
        check_cancellation_token(cancellation.as_ref())?;
        self.execute_write(|wtx| {
            execute_write_statement_in_tx(wtx, statement, &params, cancellation.as_ref())
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
                self.append_wal_and_sync(prepared.tx_id().0, &ops)?;
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

    /// Execute graph and document mutations as one durable cross-model commit.
    ///
    /// The commit record is appended and fsynced before either model is made
    /// visible. If the process crashes after the WAL append, recovery replays
    /// both the graph and document operations from the same commit record.
    pub fn execute_cross_model_write<F, T>(&self, f: F) -> Result<T, TxError>
    where
        F: FnOnce(&mut WriteTx<'_>, &mut DocumentWriteBatch) -> Result<T, TxError>,
    {
        let mut wtx = self.txn_graph.begin_write();
        let mut documents = DocumentWriteBatch::default();
        match f(&mut wtx, &mut documents) {
            Ok(val) => {
                let prepared = wtx.prepare()?;
                let graph_ops = prepared.ops().to_vec();
                let index_changes = self.capture_index_changes(&graph_ops);
                let mut wal_ops: Vec<WalOp> = graph_ops.iter().map(write_op_to_wal_op).collect();
                wal_ops.extend(documents.ops().iter().cloned());
                self.append_cross_model_wal_and_sync(prepared.tx_id().0, wal_ops)?;
                self.apply_committed_document_ops(documents.ops())?;
                self.apply_committed_vector_ops(documents.ops())?;
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

    pub fn execute_cross_model_batch(
        &self,
        cypher: Option<(&str, HashMap<String, Value>)>,
        document_mutations: Vec<DocumentMutation>,
        vector_mutations: Vec<VectorMutation>,
        cancellation: Option<Arc<AtomicBool>>,
        row_budget: Option<usize>,
        byte_budget: Option<usize>,
    ) -> Result<CrossModelBatchResult, CypherError> {
        let cypher_statements = cypher
            .map(|(query, params)| (query.to_string(), params))
            .into_iter()
            .collect();
        self.execute_cross_model_batch_multi(
            cypher_statements,
            document_mutations,
            vector_mutations,
            cancellation,
            row_budget,
            byte_budget,
        )
    }

    /// Execute native Cypher writes plus document/vector mutations as one
    /// durable commit record.
    ///
    /// All graph statements are parsed and bound before any WAL append. During
    /// execution, each statement reads from a staged graph containing earlier
    /// statements in the same batch, then appends its own operations into the
    /// same WriteTx. The whole batch becomes visible only after the cross-model
    /// WAL commit is fsynced.
    pub fn execute_cross_model_batch_multi(
        &self,
        cypher_statements: Vec<(String, HashMap<String, Value>)>,
        document_mutations: Vec<DocumentMutation>,
        vector_mutations: Vec<VectorMutation>,
        cancellation: Option<Arc<AtomicBool>>,
        row_budget: Option<usize>,
        byte_budget: Option<usize>,
    ) -> Result<CrossModelBatchResult, CypherError> {
        if cypher_statements.is_empty()
            && document_mutations.is_empty()
            && vector_mutations.is_empty()
        {
            return Err(CypherError::Execution(
                "cross-model batch requires at least one operation".into(),
            ));
        }

        let mut planned_cypher = Vec::new();
        for (idx, (query, params)) in cypher_statements.into_iter().enumerate() {
            match nexus_cypher::parser::Parser::parse(&query)? {
                Statement::Write(write_query) => {
                    check_cancellation_token(cancellation.as_ref())?;
                    nexus_cypher::binder::bind_write(&write_query)?;
                    check_cancellation_token(cancellation.as_ref())?;
                    let plan = plan_write(&write_query)?;
                    planned_cypher.push((plan, params));
                }
                Statement::Read(_) => {
                    return Err(CypherError::Execution(format!(
                        "cross-model batch cypher statement #{} must be a write statement",
                        idx + 1
                    )));
                }
            }
        }
        self.validate_vector_mutations(&vector_mutations)
            .map_err(|e| CypherError::Execution(e.to_string()))?;

        self.execute_cross_model_write(|wtx, docs| {
            for mutation in &document_mutations {
                match mutation {
                    DocumentMutation::Upsert {
                        collection,
                        key,
                        document,
                    } => docs.upsert(collection, key, document.clone())?,
                    DocumentMutation::Delete { collection, key } => docs.delete(collection, key)?,
                }
            }
            for mutation in &vector_mutations {
                match mutation {
                    VectorMutation::Upsert {
                        index,
                        vertex,
                        embedding,
                    } => docs.upsert_vector(index, *vertex, embedding.clone())?,
                    VectorMutation::Remove { index, vertex } => {
                        docs.remove_vector(index, *vertex)?
                    }
                }
            }

            let mut cypher_results = Vec::new();
            for (plan, params) in &planned_cypher {
                let staged_snapshot = self
                    .staged_graph_for_write_tx(wtx)
                    .map_err(|e| TxError::Durability(format!("cypher stage error: {e}")))?;
                let idx = self.indexes.read();
                let snapshot_rows = collect_source_bindings(
                    plan,
                    &staged_snapshot,
                    &idx,
                    params,
                    cancellation.clone(),
                    row_budget,
                    byte_budget,
                )
                .map_err(|e| TxError::Durability(format!("cypher read error: {e}")))?;
                drop(idx);
                check_cancellation_token(cancellation.as_ref())
                    .map_err(|e| TxError::Durability(format!("cypher write error: {e}")))?;

                let result = apply_native_write(
                    wtx,
                    &staged_snapshot,
                    plan,
                    &snapshot_rows,
                    params,
                    cancellation.clone(),
                    row_budget,
                    byte_budget,
                )
                .map_err(|e| TxError::Durability(format!("cypher write error: {e}")))?;
                cypher_results.push(result);
            }

            Ok(CrossModelBatchResult {
                cypher: cypher_results.last().cloned(),
                cypher_results,
                document_ops: document_mutations.len(),
                vector_ops: vector_mutations.len(),
            })
        })
        .map_err(|e| CypherError::Execution(e.to_string()))
    }

    fn staged_graph_for_write_tx(&self, wtx: &WriteTx<'_>) -> Result<Graph, TxError> {
        let committed = self.txn_graph.committed().read();
        let mut staged = committed.clone();
        drop(committed);
        apply_write_ops_to_graph(&mut staged, wtx.ops())?;
        Ok(staged)
    }

    /// Backward-compatible accessor for the committed graph lock.
    /// Prefer `begin_read()` / `begin_write()` for new code.
    pub fn graph(&self) -> &Arc<RwLock<Graph>> {
        self.txn_graph.committed()
    }

    pub fn indexes(&self) -> &Arc<RwLock<IndexSet>> {
        &self.indexes
    }

    pub fn index_memory_estimate_bytes(&self) -> usize {
        self.indexes.read().estimated_heap_bytes()
    }

    pub fn vector_index_memory_estimate_bytes(&self) -> usize {
        self.vector_index_memory_estimates_by_name()
            .into_iter()
            .map(|(_, bytes)| bytes)
            .sum()
    }

    pub fn vector_index_memory_estimates_by_name(&self) -> HashMap<String, usize> {
        let indexes = self.vector_indexes.read();
        indexes
            .iter()
            .map(|(name, index)| {
                (
                    name.clone(),
                    name.capacity()
                        + size_of::<(String, VectorIndex)>()
                        + index.estimated_heap_bytes(),
                )
            })
            .collect()
    }

    pub fn vector_index_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.vector_indexes.read().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn vector_index_dimension(&self, name: &str) -> Option<usize> {
        self.vector_indexes
            .read()
            .get(name)
            .map(|index| index.dimension())
    }

    pub fn create_vector_index(&self, name: &str, dimension: usize) -> Result<(), TxError> {
        let index = {
            let indexes = self.vector_indexes.read();
            if let Some(existing) = indexes.get(name) {
                if existing.dimension() != dimension {
                    return Err(TxError::Durability(format!(
                        "vector index '{name}' dimension mismatch: existing {}, requested {dimension}",
                        existing.dimension()
                    )));
                }
                existing.clone()
            } else {
                VectorIndex::new(dimension)
            }
        };
        self.persist_vector_snapshot(name, &index)?;
        self.vector_indexes.write().insert(name.to_string(), index);
        Ok(())
    }

    pub fn load_vector_index(&self, name: &str) -> Result<bool, TxError> {
        let Some(store) = &self.store else {
            return Ok(false);
        };
        let loaded = store
            .read()
            .load_vector_index(name)
            .map_err(|err| TxError::Durability(err.to_string()))?;
        let Some(index) = loaded else {
            return Ok(false);
        };
        self.vector_indexes.write().insert(name.to_string(), index);
        Ok(true)
    }

    pub fn load_all_vector_indexes(&self) -> Result<Vec<String>, TxError> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        let names = store
            .read()
            .list_vector_indexes()
            .map_err(|err| TxError::Durability(err.to_string()))?;
        for name in &names {
            self.load_vector_index(name)?;
        }
        Ok(names)
    }

    pub fn upsert_vector(
        &self,
        name: &str,
        vertex: VertexId,
        embedding: Vec<f32>,
    ) -> Result<(), TxError> {
        let mut next = {
            let indexes = self.vector_indexes.read();
            indexes
                .get(name)
                .cloned()
                .ok_or_else(|| TxError::Durability(format!("vector index not found: {name}")))?
        };
        if embedding.len() != next.dimension() {
            return Err(TxError::Durability(format!(
                "embedding dimension mismatch for vector index '{name}': expected {}, got {}",
                next.dimension(),
                embedding.len()
            )));
        }
        next.update(vertex, embedding);
        self.persist_vector_snapshot(name, &next)?;
        self.vector_indexes.write().insert(name.to_string(), next);
        Ok(())
    }

    pub fn remove_vector(&self, name: &str, vertex: VertexId) -> Result<bool, TxError> {
        let mut next = {
            let indexes = self.vector_indexes.read();
            indexes
                .get(name)
                .cloned()
                .ok_or_else(|| TxError::Durability(format!("vector index not found: {name}")))?
        };
        let removed = next.remove(vertex);
        if removed {
            self.persist_vector_snapshot(name, &next)?;
            self.vector_indexes.write().insert(name.to_string(), next);
        }
        Ok(removed)
    }

    pub fn compact_vector_index(&self, name: &str) -> Result<(), TxError> {
        let mut next = {
            let indexes = self.vector_indexes.read();
            indexes
                .get(name)
                .cloned()
                .ok_or_else(|| TxError::Durability(format!("vector index not found: {name}")))?
        };
        next.compact();
        self.persist_vector_snapshot(name, &next)?;
        self.vector_indexes.write().insert(name.to_string(), next);
        Ok(())
    }

    pub fn vector_search(
        &self,
        name: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(VertexId, f32)>, TxError> {
        let indexes = self.vector_indexes.read();
        let index = indexes
            .get(name)
            .ok_or_else(|| TxError::Durability(format!("vector index not found: {name}")))?;
        if query.len() != index.dimension() {
            return Err(TxError::Durability(format!(
                "query dimension mismatch for vector index '{name}': expected {}, got {}",
                index.dimension(),
                query.len()
            )));
        }
        let results = index.search(query, k);
        let exact = index.search_exact(query, k);
        let exact_ids: HashSet<_> = exact.iter().map(|(vertex, _)| vertex.0).collect();
        let overlap = results
            .iter()
            .filter(|(vertex, _)| exact_ids.contains(&vertex.0))
            .count();
        self.vector_metrics
            .record(name, results.len(), exact.len(), overlap);
        Ok(results)
    }

    pub fn vector_search_metrics(&self) -> VectorSearchMetrics {
        self.vector_metrics.snapshot()
    }

    pub fn vector_search_metrics_by_index(&self) -> HashMap<String, VectorSearchMetrics> {
        self.vector_metrics.snapshot_by_index()
    }

    fn validate_vector_mutations(&self, mutations: &[VectorMutation]) -> Result<(), TxError> {
        if mutations.is_empty() {
            return Ok(());
        }
        let indexes = self.vector_indexes.read();
        for mutation in mutations {
            match mutation {
                VectorMutation::Upsert {
                    index, embedding, ..
                } => {
                    let existing = indexes.get(index).ok_or_else(|| {
                        TxError::Durability(format!("vector index not found: {index}"))
                    })?;
                    if existing.dimension() != embedding.len() {
                        return Err(TxError::Durability(format!(
                            "embedding dimension mismatch for vector index '{index}': expected {}, got {}",
                            existing.dimension(),
                            embedding.len()
                        )));
                    }
                }
                VectorMutation::Remove { index, .. } => {
                    if !indexes.contains_key(index) {
                        return Err(TxError::Durability(format!(
                            "vector index not found: {index}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn upsert_document(
        &self,
        collection: &str,
        key: &str,
        document: serde_json::Value,
    ) -> Result<(), TxError> {
        let Some(store) = &self.store else {
            return Err(TxError::Durability(
                "document store unavailable: no durable store".into(),
            ));
        };
        store
            .write()
            .upsert_document(collection, key, &document)
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    pub fn load_document(
        &self,
        collection: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, TxError> {
        let Some(store) = &self.store else {
            return Err(TxError::Durability(
                "document store unavailable: no durable store".into(),
            ));
        };
        store
            .read()
            .load_document(collection, key)
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    pub fn delete_document(&self, collection: &str, key: &str) -> Result<bool, TxError> {
        let Some(store) = &self.store else {
            return Err(TxError::Durability(
                "document store unavailable: no durable store".into(),
            ));
        };
        store
            .write()
            .delete_document(collection, key)
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    pub fn list_document_collections(&self) -> Result<Vec<String>, TxError> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        store
            .read()
            .list_document_collections()
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    pub fn list_documents(
        &self,
        collection: &str,
        limit: Option<usize>,
    ) -> Result<Vec<DocumentRecord>, TxError> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        store
            .read()
            .list_documents(collection, limit)
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    pub fn create_document_index(
        &self,
        collection: &str,
        path: &str,
    ) -> Result<DocumentIndexDef, TxError> {
        self.create_document_index_with_kind(collection, path, DocumentIndexKind::Scalar)
    }

    pub fn create_document_index_with_kind(
        &self,
        collection: &str,
        path: &str,
        kind: DocumentIndexKind,
    ) -> Result<DocumentIndexDef, TxError> {
        let Some(store) = &self.store else {
            return Err(TxError::Durability(
                "document store unavailable: no durable store".into(),
            ));
        };
        store
            .write()
            .create_document_index_with_kind(collection, path, kind)
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    pub fn list_document_indexes(
        &self,
        collection: Option<&str>,
    ) -> Result<Vec<DocumentIndexDef>, TxError> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        store
            .read()
            .list_document_indexes(collection)
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    pub fn query_documents_by_index(
        &self,
        collection: &str,
        path: &str,
        value: serde_json::Value,
        limit: Option<usize>,
    ) -> Result<Vec<DocumentRecord>, TxError> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        store
            .read()
            .query_documents_by_index(collection, path, &value, limit)
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    pub fn query_documents_by_index_with(
        &self,
        collection: &str,
        path: &str,
        query: DocumentIndexQuery,
        limit: Option<usize>,
    ) -> Result<Vec<DocumentRecord>, TxError> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        store
            .read()
            .query_documents_by_index_with(collection, path, query, limit)
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    pub fn query_document_hits_by_index_with(
        &self,
        collection: &str,
        path: &str,
        query: DocumentIndexQuery,
        limit: Option<usize>,
    ) -> Result<Vec<DocumentSearchHit>, TxError> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        store
            .read()
            .query_document_hits_by_index_with(collection, path, query, limit)
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    pub fn store(&self) -> Option<&Arc<RwLock<NexusStore>>> {
        self.store.as_ref()
    }

    pub fn storage_metrics(&self) -> Option<StoreMetrics> {
        self.store
            .as_ref()
            .and_then(|store| store.read().metrics().ok())
    }

    pub fn rebuild_indexes(&self) {
        let rtx = self.txn_graph.begin_read();
        let rebuilt = build_indexes_from_graph(rtx.graph());
        *self.indexes.write() = rebuilt;
    }

    fn persist_vector_snapshot(&self, name: &str, index: &VectorIndex) -> Result<(), TxError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        store
            .write()
            .save_vector_index(name, index)
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    /// Compact tombstones and refresh durable state.
    ///
    /// This is a maintenance operation, not a logical Cypher write: it does not
    /// append new WAL entries. If the engine has a store, it writes a fresh
    /// snapshot after compaction so old WAL tombstones become redundant.
    pub fn compact_storage(&self) -> Result<GraphCompactionStats, TxError> {
        let stats = {
            let mut graph = self.txn_graph.committed().write();
            graph
                .compact_tombstones()
                .map_err(|err| TxError::Durability(err.to_string()))?
        };

        self.rebuild_indexes();

        if let Some(store) = &self.store {
            let graph = self.txn_graph.committed().read();
            store
                .write()
                .save_snapshot(&graph)
                .map_err(|err| TxError::Durability(err.to_string()))?;
        }

        Ok(stats)
    }

    /// Persist the current committed graph into a crash-safe snapshot.
    ///
    /// Logical writes are already WAL-backed through `execute_write()`. This
    /// method creates an explicit checkpoint without changing graph contents.
    pub fn save_snapshot(&self) -> Result<(), TxError> {
        let Some(store) = &self.store else {
            return Err(TxError::Durability(
                "save_snapshot unavailable: engine has no durable store".into(),
            ));
        };

        let graph = self.txn_graph.committed().read();
        store
            .write()
            .save_snapshot(&graph)
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    pub fn compaction_pressure(&self) -> GraphCompactionPressure {
        let rtx = self.txn_graph.begin_read();
        rtx.graph().compaction_pressure()
    }

    pub fn compact_storage_if_needed(
        &self,
        threshold: usize,
    ) -> Result<Option<GraphCompactionStats>, TxError> {
        if threshold == 0 {
            return Ok(None);
        }

        let pressure = self.compaction_pressure();
        if pressure.total() < threshold {
            return Ok(None);
        }

        self.compact_storage().map(Some)
    }

    pub fn backup_to(&self, path: impl AsRef<std::path::Path>) -> Result<BackupManifest, TxError> {
        let Some(store) = &self.store else {
            return Err(TxError::Durability(
                "backup unavailable: engine has no durable store".into(),
            ));
        };

        store
            .write()
            .backup_to(path)
            .map_err(|err| TxError::Durability(err.to_string()))
    }

    pub fn vertex_count(&self) -> usize {
        let rtx = self.txn_graph.begin_read();
        rtx.graph().num_vertices()
    }

    pub fn edge_count(&self) -> u64 {
        let rtx = self.txn_graph.begin_read();
        rtx.graph().num_edges()
    }

    pub fn graph_memory_estimate_bytes(&self) -> usize {
        let rtx = self.txn_graph.begin_read();
        rtx.graph().estimated_heap_bytes()
    }

    pub fn total_memory_estimate_bytes(&self) -> usize {
        self.graph_memory_estimate_bytes()
            + self.index_memory_estimate_bytes()
            + self.vector_index_memory_estimate_bytes()
    }

    pub fn vertex_labels(&self) -> Vec<String> {
        let rtx = self.txn_graph.begin_read();
        rtx.graph().vertex_label_names()
    }

    pub fn edge_labels(&self) -> Vec<String> {
        let rtx = self.txn_graph.begin_read();
        rtx.graph().edge_label_names()
    }

    fn append_wal_and_sync(&self, tx_id: u64, ops: &[WriteOp]) -> Result<(), TxError> {
        if ops.is_empty() {
            return Ok(());
        }
        let wal_ops = ops.iter().map(write_op_to_wal_op).collect();
        self.append_cross_model_wal_and_sync(tx_id, wal_ops)
    }

    fn append_cross_model_wal_and_sync(&self, tx_id: u64, ops: Vec<WalOp>) -> Result<(), TxError> {
        if ops.is_empty() {
            return Ok(());
        }
        let Some(store) = &self.store else {
            return Ok(());
        };

        let mut store = store.write();
        store
            .log_commit(tx_id, ops)
            .map_err(|e| TxError::Durability(e.to_string()))?;
        store
            .wal_mut()
            .sync()
            .map_err(|e| TxError::Durability(e.to_string()))?;
        Ok(())
    }

    fn apply_committed_document_ops(&self, ops: &[WalOp]) -> Result<(), TxError> {
        if !ops.iter().any(|op| {
            matches!(
                op,
                WalOp::UpsertDocument { .. } | WalOp::DeleteDocument { .. }
            )
        }) {
            return Ok(());
        }
        let Some(store) = &self.store else {
            return Err(TxError::Durability(
                "document store unavailable: no durable store".into(),
            ));
        };
        store
            .write()
            .apply_committed_document_ops(ops)
            .map_err(|e| TxError::Durability(e.to_string()))
    }

    fn apply_committed_vector_ops(&self, ops: &[WalOp]) -> Result<(), TxError> {
        if !ops
            .iter()
            .any(|op| matches!(op, WalOp::UpsertVector { .. } | WalOp::RemoveVector { .. }))
        {
            return Ok(());
        }

        for op in ops {
            match op {
                WalOp::UpsertVector {
                    index,
                    vertex_id,
                    embedding,
                } => {
                    let mut next = {
                        let indexes = self.vector_indexes.read();
                        indexes.get(index).cloned().ok_or_else(|| {
                            TxError::Durability(format!("vector index not found: {index}"))
                        })?
                    };
                    if next.dimension() != embedding.len() {
                        return Err(TxError::Durability(format!(
                            "embedding dimension mismatch for vector index '{index}': expected {}, got {}",
                            next.dimension(),
                            embedding.len()
                        )));
                    }
                    next.update(VertexId(*vertex_id), embedding.clone());
                    self.persist_vector_snapshot(index, &next)?;
                    self.vector_indexes.write().insert(index.clone(), next);
                }
                WalOp::RemoveVector { index, vertex_id } => {
                    let mut next = {
                        let indexes = self.vector_indexes.read();
                        indexes.get(index).cloned().ok_or_else(|| {
                            TxError::Durability(format!("vector index not found: {index}"))
                        })?
                    };
                    if next.remove(VertexId(*vertex_id)) {
                        self.persist_vector_snapshot(index, &next)?;
                        self.vector_indexes.write().insert(index.clone(), next);
                    }
                }
                _ => {}
            }
        }
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

#[derive(Debug, Clone, PartialEq)]
enum CreatedBinding {
    Vertex(VertexId),
    Edge(EdgeId),
    Scalar(Value),
}

const EDGE_REF_KEY: &str = "__edge_id";
const NODE_REF_KEY: &str = "__vertex_id";
const MERGE_CREATED_KEY: &str = "__nexus_merge_created";

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

fn reset_merge_created_binding(bindings: &mut HashMap<String, CreatedBinding>) {
    bindings.insert(
        MERGE_CREATED_KEY.into(),
        CreatedBinding::Scalar(Value::Bool(false)),
    );
}

fn mark_merge_created_binding(bindings: &mut HashMap<String, CreatedBinding>) {
    bindings.insert(
        MERGE_CREATED_KEY.into(),
        CreatedBinding::Scalar(Value::Bool(true)),
    );
}

fn merge_created_binding(bindings: &HashMap<String, CreatedBinding>) -> bool {
    matches!(
        bindings.get(MERGE_CREATED_KEY),
        Some(CreatedBinding::Scalar(Value::Bool(true)))
    )
}

fn clear_merge_created_binding(bindings: &mut HashMap<String, CreatedBinding>) {
    bindings.remove(MERGE_CREATED_KEY);
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

/// Resolve binding rows for the optional source (MATCH+WHERE) of a native
/// write plan against a consistent read snapshot.
fn collect_source_bindings(
    plan: &WritePlan,
    snapshot: &Graph,
    indexes: &IndexSet,
    params: &HashMap<String, Value>,
    cancellation: Option<Arc<AtomicBool>>,
    row_budget: Option<usize>,
    byte_budget: Option<usize>,
) -> Result<QueryResult, CypherError> {
    check_cancellation_token(cancellation.as_ref())?;
    if let Some(source) = &plan.source {
        let mut ctx = QueryContext::with_indexes(snapshot, indexes).with_params(params.clone());
        if let Some(cancellation) = cancellation {
            ctx = ctx.with_cancellation(cancellation);
        }
        if let Some(row_budget) = row_budget {
            ctx = ctx.with_row_budget(row_budget);
        }
        if let Some(byte_budget) = byte_budget {
            ctx = ctx.with_byte_budget(byte_budget);
        }
        nexus_cypher::executor::execute(source, &ctx)
    } else {
        // One empty binding row so mutations execute exactly once.
        Ok(QueryResult {
            columns: Vec::new(),
            rows: vec![Vec::new()],
        })
    }
}

/// Apply native-parsed mutations to a WriteTx as a row pipeline.
///
/// Every mutation is mirrored into a local staged graph immediately, while the
/// same logical operation is buffered into the durable `WriteTx`. Later read
/// clauses (`WITH`, `UNWIND`, `MATCH`, `WHERE`) execute against that staged
/// graph, giving server writes the same read-your-own-writes semantics as the
/// in-memory TCK executor without committing before WAL fsync.
fn apply_native_write(
    wtx: &mut WriteTx<'_>,
    snapshot: &Graph,
    plan: &WritePlan,
    source_rows: &QueryResult,
    params: &HashMap<String, Value>,
    cancellation: Option<Arc<AtomicBool>>,
    row_budget: Option<usize>,
    byte_budget: Option<usize>,
) -> Result<QueryResult, CypherError> {
    check_cancellation_token(cancellation.as_ref())?;
    check_row_budget(row_budget, source_rows.rows.len())?;
    check_byte_budget(
        byte_budget,
        estimate_query_result_bytes(&source_rows.columns, &source_rows.rows),
    )?;
    let mut staged = snapshot.clone();
    let mut graph_binding_kinds = plan
        .source
        .as_ref()
        .map(logical_plan_graph_binding_kinds)
        .unwrap_or_default();
    let mut current_columns = source_rows.columns.clone();
    let mut binding_sets: Vec<HashMap<String, CreatedBinding>> = source_rows
        .rows
        .iter()
        .map(|row| initial_created_bindings(row, &source_rows.columns, &graph_binding_kinds))
        .collect();

    let mut op_idx = 0;
    while op_idx < plan.mutations.len() {
        check_cancellation_token(cancellation.as_ref())?;
        let op = &plan.mutations[op_idx];
        if let MutationOp::ReadClause(clause) = op {
            let idx = IndexSet::new();
            let mut read_ctx =
                QueryContext::with_indexes(&staged, &idx).with_params(params.clone());
            if let Some(cancellation) = cancellation.clone() {
                read_ctx = read_ctx.with_cancellation(cancellation);
            }
            if let Some(row_budget) = row_budget {
                read_ctx = read_ctx.with_row_budget(row_budget);
            }
            if let Some(byte_budget) = byte_budget {
                read_ctx = read_ctx.with_byte_budget(byte_budget);
            }
            let input = created_stream_to_query_result(&current_columns, &binding_sets);
            let output = execute_write_read_clause(&read_ctx, input, clause)?;
            check_row_budget(row_budget, output.rows.len())?;
            check_byte_budget(
                byte_budget,
                estimate_query_result_bytes(&output.columns, &output.rows),
            )?;
            graph_binding_kinds =
                graph_binding_kinds_after_read_clause(&graph_binding_kinds, clause);
            current_columns = output.columns.clone();
            binding_sets = query_result_to_created_bindings(&output, &graph_binding_kinds);
            op_idx += 1;
            continue;
        }

        let mut next_binding_sets = Vec::new();
        for (binding_idx, mut bindings) in binding_sets.into_iter().enumerate() {
            if binding_idx % 1024 == 0 {
                check_cancellation_token(cancellation.as_ref())?;
            }
            let mut vertex_properties: HashMap<VertexId, HashMap<String, Value>> = HashMap::new();
            let mut edge_properties: HashMap<EdgeId, HashMap<String, Value>> = HashMap::new();
            let mut vertex_labels: HashMap<VertexId, String> = HashMap::new();
            match op {
                MutationOp::BeginMerge => {
                    reset_merge_created_binding(&mut bindings);
                }
                MutationOp::CreateNode {
                    variable,
                    labels,
                    properties,
                } => {
                    if bindings.contains_key(variable) {
                        resolve_vertex_binding(&bindings, variable)?;
                    } else {
                        let label = labels.first().ok_or_else(|| {
                            CypherError::Execution("CREATE node requires a label".into())
                        })?;
                        let vid = wtx.add_vertex(label);
                        staged
                            .try_add_vertex_with_id(vid.0, label)
                            .map_err(|e| CypherError::Execution(e.to_string()))?;
                        vertex_labels.insert(vid, label.clone());
                        for (key, pv) in properties {
                            let value = resolve_native_property_value(
                                pv,
                                params,
                                &staged,
                                &bindings,
                                &vertex_properties,
                                &edge_properties,
                            )?;
                            staged
                                .try_set_vertex_property(vid, key, value.clone())
                                .map_err(|e| CypherError::Execution(e.to_string()))?;
                            wtx.set_vertex_property(vid, key, value.clone());
                            vertex_properties
                                .entry(vid)
                                .or_default()
                                .insert(key.clone(), value);
                        }
                        bindings.insert(variable.clone(), CreatedBinding::Vertex(vid));
                    }
                    graph_binding_kinds.insert(variable.clone(), GraphBindingKind::Node);
                    ensure_visible_created_column(&mut current_columns, variable);
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
                    staged
                        .try_add_edge_with_id(edge.0, src, dst, rel_type)
                        .map_err(|e| CypherError::Execution(e.to_string()))?;
                    for (key, pv) in properties {
                        let value = resolve_native_property_value(
                            pv,
                            params,
                            &staged,
                            &bindings,
                            &vertex_properties,
                            &edge_properties,
                        )?;
                        staged
                            .try_set_edge_property(edge, key, value.clone())
                            .map_err(|e| CypherError::Execution(e.to_string()))?;
                        wtx.set_edge_property(edge, key, value.clone());
                        edge_properties
                            .entry(edge)
                            .or_default()
                            .insert(key.clone(), value);
                    }
                    if let Some(variable) = variable {
                        bindings.insert(variable.clone(), CreatedBinding::Edge(edge));
                        graph_binding_kinds
                            .insert(variable.clone(), GraphBindingKind::Relationship);
                        ensure_visible_created_column(&mut current_columns, variable);
                    }
                }
                MutationOp::MergeNode {
                    variable,
                    labels,
                    properties,
                } => {
                    if bindings.contains_key(variable) {
                        resolve_vertex_binding(&bindings, variable)?;
                    } else {
                        let label = labels.first().ok_or_else(|| {
                            CypherError::Execution("MERGE node requires a label".into())
                        })?;
                        let resolved_props = resolve_native_property_pairs(
                            properties,
                            params,
                            &staged,
                            &bindings,
                            &vertex_properties,
                            &edge_properties,
                        )?;
                        let vid = if let Some(vid) =
                            find_matching_vertex(&staged, label, &resolved_props)
                        {
                            vid
                        } else {
                            let vid = wtx.add_vertex(label);
                            staged
                                .try_add_vertex_with_id(vid.0, label)
                                .map_err(|e| CypherError::Execution(e.to_string()))?;
                            vertex_labels.insert(vid, label.clone());
                            for (key, value) in &resolved_props {
                                staged
                                    .try_set_vertex_property(vid, key, value.clone())
                                    .map_err(|e| CypherError::Execution(e.to_string()))?;
                                wtx.set_vertex_property(vid, key, value.clone());
                                vertex_properties
                                    .entry(vid)
                                    .or_default()
                                    .insert(key.clone(), value.clone());
                            }
                            mark_merge_created_binding(&mut bindings);
                            vid
                        };
                        bindings.insert(variable.clone(), CreatedBinding::Vertex(vid));
                    }
                    graph_binding_kinds.insert(variable.clone(), GraphBindingKind::Node);
                    ensure_visible_created_column(&mut current_columns, variable);
                }
                MutationOp::MergeEdge {
                    variable,
                    src_var,
                    dst_var,
                    rel_type,
                    direction: _,
                    properties,
                } => {
                    if let Some(variable) = variable {
                        if bindings.contains_key(variable) {
                            resolve_edge_binding(&bindings, variable)?;
                            graph_binding_kinds
                                .insert(variable.clone(), GraphBindingKind::Relationship);
                            ensure_visible_created_column(&mut current_columns, variable);
                            next_binding_sets.push(bindings);
                            continue;
                        }
                    }
                    let src = resolve_vertex_binding(&bindings, src_var)?;
                    let dst = resolve_vertex_binding(&bindings, dst_var)?;
                    let resolved_props = resolve_native_property_pairs(
                        properties,
                        params,
                        &staged,
                        &bindings,
                        &vertex_properties,
                        &edge_properties,
                    )?;
                    let edge = if let Some(edge) = staged.edge_between(src, dst, rel_type) {
                        edge
                    } else {
                        let edge = wtx.add_edge(src, dst, rel_type);
                        staged
                            .try_add_edge_with_id(edge.0, src, dst, rel_type)
                            .map_err(|e| CypherError::Execution(e.to_string()))?;
                        for (key, value) in &resolved_props {
                            staged
                                .try_set_edge_property(edge, key, value.clone())
                                .map_err(|e| CypherError::Execution(e.to_string()))?;
                            wtx.set_edge_property(edge, key, value.clone());
                            edge_properties
                                .entry(edge)
                                .or_default()
                                .insert(key.clone(), value.clone());
                        }
                        mark_merge_created_binding(&mut bindings);
                        edge
                    };
                    if let Some(variable) = variable {
                        bindings.insert(variable.clone(), CreatedBinding::Edge(edge));
                        graph_binding_kinds
                            .insert(variable.clone(), GraphBindingKind::Relationship);
                        ensure_visible_created_column(&mut current_columns, variable);
                    }
                }
                MutationOp::BindPath {
                    variable,
                    node_vars,
                    edge_vars,
                } => {
                    let nodes = node_vars
                        .iter()
                        .map(|node_var| resolve_vertex_binding(&bindings, node_var))
                        .collect::<Result<Vec<_>, _>>()?;
                    let edges = edge_vars
                        .iter()
                        .map(|edge_var| resolve_edge_binding(&bindings, edge_var))
                        .collect::<Result<Vec<_>, _>>()?;
                    bindings.insert(
                        variable.clone(),
                        CreatedBinding::Scalar(path_value(&nodes, &edges)),
                    );
                    ensure_visible_created_column(&mut current_columns, variable);
                }
                MutationOp::ApplyMergeActions {
                    on_create,
                    on_match,
                } => {
                    let actions = if merge_created_binding(&bindings) {
                        on_create
                    } else {
                        on_match
                    };
                    for action in actions {
                        apply_native_merge_action(
                            action,
                            wtx,
                            &mut staged,
                            params,
                            &bindings,
                            &mut vertex_properties,
                            &mut edge_properties,
                            &mut vertex_labels,
                        )?;
                    }
                    clear_merge_created_binding(&mut bindings);
                }
                MutationOp::SetProperty {
                    variable,
                    key,
                    value,
                } => {
                    let value = resolve_native_property_value(
                        value,
                        params,
                        &staged,
                        &bindings,
                        &vertex_properties,
                        &edge_properties,
                    )?;
                    set_bound_property(
                        wtx,
                        &mut staged,
                        &bindings,
                        variable,
                        key,
                        value,
                        &mut vertex_properties,
                        &mut edge_properties,
                    )?;
                }
                MutationOp::RemoveProperty { variable, key } => {
                    remove_bound_property(
                        wtx,
                        &mut staged,
                        &bindings,
                        variable,
                        key,
                        &mut vertex_properties,
                        &mut edge_properties,
                    )?;
                }
                MutationOp::SetProperties {
                    variable,
                    value,
                    replace,
                } => {
                    let value = resolve_native_property_value(
                        value,
                        params,
                        &staged,
                        &bindings,
                        &vertex_properties,
                        &edge_properties,
                    )?;
                    set_bound_properties(
                        wtx,
                        &mut staged,
                        &bindings,
                        variable,
                        value,
                        *replace,
                        &mut vertex_properties,
                        &mut edge_properties,
                    )?;
                }
                MutationOp::SetLabels { variable, labels } => {
                    set_bound_labels(
                        wtx,
                        &mut staged,
                        &bindings,
                        &mut vertex_labels,
                        variable,
                        labels,
                        true,
                    )?;
                }
                MutationOp::RemoveLabels { variable, labels } => {
                    set_bound_labels(
                        wtx,
                        &mut staged,
                        &bindings,
                        &mut vertex_labels,
                        variable,
                        labels,
                        false,
                    )?;
                }
                MutationOp::Delete {
                    variable: Some(variable),
                    detach,
                    ..
                } => {
                    match bindings.get(variable).cloned() {
                        Some(CreatedBinding::Edge(edge)) => {
                            if staged.edge_exists(edge) {
                                staged
                                    .try_remove_edge(edge)
                                    .map_err(|e| CypherError::Execution(e.to_string()))?;
                                wtx.remove_edge(edge);
                            }
                            bindings
                                .insert(variable.clone(), CreatedBinding::Scalar(edge_ref(edge)));
                        }
                        Some(CreatedBinding::Vertex(vid)) => {
                            if !*detach && snapshot_has_incident_edges(&staged, vid) {
                                return Err(CypherError::Execution(format!(
                                    "cannot DELETE node '{variable}' with relationships; use DETACH DELETE"
                                )));
                            }
                            if staged.vertex_label(vid).is_some() {
                                staged
                                    .try_remove_vertex(vid)
                                    .map_err(|e| CypherError::Execution(e.to_string()))?;
                                // `try_remove_vertex` cascades to incident edges in the core
                                // graph, and WAL replay re-cascades on recovery, so only the
                                // RemoveVertex op needs to be buffered.
                                wtx.remove_vertex(vid);
                            }
                            bindings.remove(variable);
                        }
                        Some(CreatedBinding::Scalar(value)) => {
                            delete_native_graph_value(wtx, &mut staged, &value, *detach)?;
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
                }
                MutationOp::Delete {
                    variable: None,
                    target,
                    detach,
                } => {
                    let value = eval_write_project_expr(
                        target,
                        params,
                        &bindings,
                        Some(&staged),
                        &vertex_properties,
                        &edge_properties,
                    )?;
                    delete_native_graph_value(wtx, &mut staged, &value, *detach)?;
                }
                MutationOp::ReadClause(_) => unreachable!("read clauses are handled above"),
            }
            next_binding_sets.push(bindings);
        }
        binding_sets = next_binding_sets;
        check_row_budget(row_budget, binding_sets.len())?;
        let current = created_stream_to_query_result(&current_columns, &binding_sets);
        check_byte_budget(
            byte_budget,
            estimate_query_result_bytes(&current.columns, &current.rows),
        )?;
        op_idx += 1;
    }

    if let Some(columns) = &plan.return_columns {
        let idx = IndexSet::new();
        let mut read_ctx = QueryContext::with_indexes(&staged, &idx).with_params(params.clone());
        if let Some(cancellation) = cancellation {
            read_ctx = read_ctx.with_cancellation(cancellation);
        }
        if let Some(row_budget) = row_budget {
            read_ctx = read_ctx.with_row_budget(row_budget);
        }
        if let Some(byte_budget) = byte_budget {
            read_ctx = read_ctx.with_byte_budget(byte_budget);
        }
        let input = created_stream_to_query_result(&current_columns, &binding_sets);
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
        Ok(result)
    } else {
        Ok(QueryResult::empty(Vec::new()))
    }
}

fn initial_created_bindings(
    row: &[Value],
    column_names: &[String],
    graph_binding_kinds: &HashMap<String, GraphBindingKind>,
) -> HashMap<String, CreatedBinding> {
    let mut bindings = HashMap::new();
    for (col_idx, col_name) in column_names.iter().enumerate() {
        let Some(value) = row.get(col_idx) else {
            continue;
        };
        let binding = match graph_binding_kinds.get(col_name).copied() {
            Some(GraphBindingKind::Relationship) => edge_ref_id(value)
                .or_else(|| match value {
                    Value::Int64(id) if *id >= 0 => Some(EdgeId(*id as u64)),
                    _ => None,
                })
                .map(CreatedBinding::Edge)
                .unwrap_or_else(|| CreatedBinding::Scalar(value.clone())),
            Some(GraphBindingKind::Node) => value_to_vertex_binding(value)
                .map(CreatedBinding::Vertex)
                .unwrap_or_else(|| CreatedBinding::Scalar(value.clone())),
            Some(GraphBindingKind::NodeList) | Some(GraphBindingKind::RelationshipList) | None => {
                CreatedBinding::Scalar(value.clone())
            }
        };
        bindings.insert(col_name.clone(), binding);
    }
    bindings
}

fn created_stream_to_query_result(
    columns: &[String],
    binding_sets: &[HashMap<String, CreatedBinding>],
) -> QueryResult {
    QueryResult {
        columns: columns.to_vec(),
        rows: binding_sets
            .iter()
            .map(|bindings| created_binding_row(columns, bindings))
            .collect(),
    }
}

fn created_binding_row(
    columns: &[String],
    bindings: &HashMap<String, CreatedBinding>,
) -> Vec<Value> {
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

fn query_result_to_created_bindings(
    result: &QueryResult,
    graph_binding_kinds: &HashMap<String, GraphBindingKind>,
) -> Vec<HashMap<String, CreatedBinding>> {
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
                        .map(CreatedBinding::Vertex)
                        .unwrap_or_else(|| CreatedBinding::Scalar(value.clone())),
                    Some(GraphBindingKind::Relationship) => edge_ref_id(value)
                        .or_else(|| match value {
                            Value::Int64(id) if *id >= 0 => Some(EdgeId(*id as u64)),
                            _ => None,
                        })
                        .map(CreatedBinding::Edge)
                        .unwrap_or_else(|| CreatedBinding::Scalar(value.clone())),
                    Some(GraphBindingKind::NodeList) | Some(GraphBindingKind::RelationshipList) => {
                        CreatedBinding::Scalar(value.clone())
                    }
                    None => edge_ref_id(value)
                        .map(CreatedBinding::Edge)
                        .unwrap_or_else(|| CreatedBinding::Scalar(value.clone())),
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

fn ensure_visible_created_column(columns: &mut Vec<String>, name: &str) {
    if name == MERGE_CREATED_KEY
        || name.starts_with("_anon_")
        || columns.iter().any(|column| column == name)
    {
        return;
    }
    columns.push(name.to_string());
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

fn collect_pattern_graph_binding_kinds(
    clause: &nexus_cypher::ast::MatchClause,
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
                        vars.insert(variable.clone(), GraphBindingKind::Relationship);
                    }
                }
            }
        }
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
        Some(CreatedBinding::Scalar(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is bound to a scalar, not a node"
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
        Some(CreatedBinding::Scalar(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is bound to a scalar, not a relationship"
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
            if let Expr::Variable(name) = expr.as_ref() {
                if let Some(binding) = bindings.get(name) {
                    return Ok(match binding {
                        CreatedBinding::Vertex(vertex) => node_ref(*vertex),
                        CreatedBinding::Edge(edge) => edge_ref(*edge),
                        CreatedBinding::Scalar(value) => value.clone(),
                    });
                }
            }
            // Project current write bindings as a synthetic row so arithmetic,
            // lists, maps, and function calls inside CREATE/MERGE property maps
            // can be evaluated at mutation time.
            let mut columns: Vec<String> = Vec::with_capacity(bindings.len());
            let mut row: Vec<Value> = Vec::with_capacity(bindings.len());
            for (name, binding) in bindings {
                columns.push(name.clone());
                row.push(match binding {
                    CreatedBinding::Vertex(v) => node_ref(*v),
                    CreatedBinding::Edge(e) => edge_ref(*e),
                    CreatedBinding::Scalar(value) => value.clone(),
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

#[allow(clippy::too_many_arguments)]
fn apply_native_merge_action(
    action: &MutationOp,
    wtx: &mut WriteTx<'_>,
    staged: &mut Graph,
    params: &HashMap<String, Value>,
    bindings: &HashMap<String, CreatedBinding>,
    vertex_properties: &mut HashMap<VertexId, HashMap<String, Value>>,
    edge_properties: &mut HashMap<EdgeId, HashMap<String, Value>>,
    vertex_labels: &mut HashMap<VertexId, String>,
) -> Result<(), CypherError> {
    match action {
        MutationOp::SetProperty {
            variable,
            key,
            value,
        } => {
            let value = resolve_native_property_value(
                value,
                params,
                staged,
                bindings,
                vertex_properties,
                edge_properties,
            )?;
            set_bound_property(
                wtx,
                staged,
                bindings,
                variable,
                key,
                value,
                vertex_properties,
                edge_properties,
            )
        }
        MutationOp::SetProperties {
            variable,
            value,
            replace,
        } => {
            let value = resolve_native_property_value(
                value,
                params,
                staged,
                bindings,
                vertex_properties,
                edge_properties,
            )?;
            set_bound_properties(
                wtx,
                staged,
                bindings,
                variable,
                value,
                *replace,
                vertex_properties,
                edge_properties,
            )
        }
        MutationOp::SetLabels { variable, labels } => {
            set_bound_labels(wtx, staged, bindings, vertex_labels, variable, labels, true)
        }
        _ => Err(CypherError::Execution(
            "MERGE ON action must be a SET operation".into(),
        )),
    }
}

fn set_bound_property(
    wtx: &mut WriteTx<'_>,
    staged: &mut Graph,
    bindings: &HashMap<String, CreatedBinding>,
    variable: &str,
    key: &str,
    value: Value,
    vertex_properties: &mut HashMap<VertexId, HashMap<String, Value>>,
    edge_properties: &mut HashMap<EdgeId, HashMap<String, Value>>,
) -> Result<(), CypherError> {
    match bindings.get(variable) {
        Some(CreatedBinding::Vertex(vertex)) => {
            staged
                .try_set_vertex_property(*vertex, key, value.clone())
                .map_err(|e| CypherError::Execution(e.to_string()))?;
            wtx.set_vertex_property(*vertex, key, value.clone());
            vertex_properties
                .entry(*vertex)
                .or_default()
                .insert(key.to_string(), value);
            Ok(())
        }
        Some(CreatedBinding::Edge(edge)) => {
            staged
                .try_set_edge_property(*edge, key, value.clone())
                .map_err(|e| CypherError::Execution(e.to_string()))?;
            wtx.set_edge_property(*edge, key, value.clone());
            edge_properties
                .entry(*edge)
                .or_default()
                .insert(key.to_string(), value);
            Ok(())
        }
        Some(CreatedBinding::Scalar(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is not a graph element"
        ))),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
fn remove_bound_property(
    wtx: &mut WriteTx<'_>,
    staged: &mut Graph,
    bindings: &HashMap<String, CreatedBinding>,
    variable: &str,
    key: &str,
    vertex_properties: &mut HashMap<VertexId, HashMap<String, Value>>,
    edge_properties: &mut HashMap<EdgeId, HashMap<String, Value>>,
) -> Result<(), CypherError> {
    match bindings.get(variable) {
        Some(CreatedBinding::Vertex(vertex)) => {
            let local = vertex_properties
                .get(vertex)
                .and_then(|properties| properties.get(key));
            let current = local
                .cloned()
                .unwrap_or_else(|| staged.get_vertex_property(*vertex, key));
            if matches!(current, Value::Null) {
                return Ok(());
            }
            staged
                .try_set_vertex_property(*vertex, key, Value::Null)
                .map_err(|e| CypherError::Execution(e.to_string()))?;
            wtx.set_vertex_property(*vertex, key, Value::Null);
            vertex_properties
                .entry(*vertex)
                .or_default()
                .insert(key.to_string(), Value::Null);
            Ok(())
        }
        Some(CreatedBinding::Edge(edge)) => {
            let local = edge_properties
                .get(edge)
                .and_then(|properties| properties.get(key));
            let current = local
                .cloned()
                .unwrap_or_else(|| staged.get_edge_property(*edge, key));
            if matches!(current, Value::Null) {
                return Ok(());
            }
            staged
                .try_set_edge_property(*edge, key, Value::Null)
                .map_err(|e| CypherError::Execution(e.to_string()))?;
            wtx.set_edge_property(*edge, key, Value::Null);
            edge_properties
                .entry(*edge)
                .or_default()
                .insert(key.to_string(), Value::Null);
            Ok(())
        }
        Some(CreatedBinding::Scalar(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is not a graph element"
        ))),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
fn set_bound_properties(
    wtx: &mut WriteTx<'_>,
    staged: &mut Graph,
    bindings: &HashMap<String, CreatedBinding>,
    variable: &str,
    value: Value,
    replace: bool,
    vertex_properties: &mut HashMap<VertexId, HashMap<String, Value>>,
    edge_properties: &mut HashMap<EdgeId, HashMap<String, Value>>,
) -> Result<(), CypherError> {
    let entries =
        match native_property_entries_from_value(staged, value, vertex_properties, edge_properties)
        {
            Ok(Some(entries)) => entries,
            Ok(None) => return Ok(()),
            Err(other) => {
                return Err(CypherError::Execution(format!(
                    "SET {variable}{} requires a map, got {other:?}",
                    if replace { " =" } else { " +=" }
                )));
            }
        };

    match bindings.get(variable) {
        Some(CreatedBinding::Vertex(vertex)) => {
            if replace {
                let mut keys: Vec<String> = staged
                    .get_vertex_properties(*vertex)
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect();
                if let Some(local) = vertex_properties.get(vertex) {
                    keys.extend(local.keys().cloned());
                }
                keys.sort();
                keys.dedup();
                for key in keys {
                    staged
                        .try_set_vertex_property(*vertex, &key, Value::Null)
                        .map_err(|e| CypherError::Execution(e.to_string()))?;
                    wtx.set_vertex_property(*vertex, &key, Value::Null);
                    vertex_properties
                        .entry(*vertex)
                        .or_default()
                        .insert(key, Value::Null);
                }
            }
            for (key, value) in entries {
                staged
                    .try_set_vertex_property(*vertex, &key, value.clone())
                    .map_err(|e| CypherError::Execution(e.to_string()))?;
                wtx.set_vertex_property(*vertex, &key, value.clone());
                vertex_properties
                    .entry(*vertex)
                    .or_default()
                    .insert(key, value);
            }
            Ok(())
        }
        Some(CreatedBinding::Edge(edge)) => {
            if replace {
                let mut keys: Vec<String> = staged
                    .get_edge_properties(*edge)
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect();
                if let Some(local) = edge_properties.get(edge) {
                    keys.extend(local.keys().cloned());
                }
                keys.sort();
                keys.dedup();
                for key in keys {
                    staged
                        .try_set_edge_property(*edge, &key, Value::Null)
                        .map_err(|e| CypherError::Execution(e.to_string()))?;
                    wtx.set_edge_property(*edge, &key, Value::Null);
                    edge_properties
                        .entry(*edge)
                        .or_default()
                        .insert(key, Value::Null);
                }
            }
            for (key, value) in entries {
                staged
                    .try_set_edge_property(*edge, &key, value.clone())
                    .map_err(|e| CypherError::Execution(e.to_string()))?;
                wtx.set_edge_property(*edge, &key, value.clone());
                edge_properties.entry(*edge).or_default().insert(key, value);
            }
            Ok(())
        }
        Some(CreatedBinding::Scalar(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is not a graph element"
        ))),
        None => Err(CypherError::Execution(format!(
            "undefined variable '{variable}'"
        ))),
    }
}

fn native_property_entries_from_value(
    snapshot: &Graph,
    value: Value,
    vertex_properties: &HashMap<VertexId, HashMap<String, Value>>,
    edge_properties: &HashMap<EdgeId, HashMap<String, Value>>,
) -> Result<Option<Vec<(String, Value)>>, Value> {
    if value.is_null() {
        return Ok(None);
    }
    if let Some(vertex) = node_ref_id(&value) {
        let mut entries: HashMap<String, Value> =
            snapshot.get_vertex_properties(vertex).into_iter().collect();
        if let Some(local) = vertex_properties.get(&vertex) {
            entries.extend(local.clone());
        }
        let mut entries: Vec<_> = entries
            .into_iter()
            .filter(|(_, value)| !value.is_null())
            .collect();
        entries.sort_by(|(left, _), (right, _)| left.cmp(right));
        return Ok(Some(entries));
    }
    if let Some(edge) = edge_ref_id(&value) {
        let mut entries: HashMap<String, Value> =
            snapshot.get_edge_properties(edge).into_iter().collect();
        if let Some(local) = edge_properties.get(&edge) {
            entries.extend(local.clone());
        }
        let mut entries: Vec<_> = entries
            .into_iter()
            .filter(|(_, value)| !value.is_null())
            .collect();
        entries.sort_by(|(left, _), (right, _)| left.cmp(right));
        return Ok(Some(entries));
    }
    if let Value::Map(entries) = value {
        return Ok(Some(entries));
    }
    Err(value)
}

fn set_bound_labels(
    wtx: &mut WriteTx<'_>,
    staged: &mut Graph,
    bindings: &HashMap<String, CreatedBinding>,
    pending_labels: &mut HashMap<VertexId, String>,
    variable: &str,
    labels: &[String],
    add: bool,
) -> Result<(), CypherError> {
    let vertex = resolve_vertex_binding(bindings, variable)?;
    let mut current = pending_labels
        .get(&vertex)
        .map(|label| split_label_string(label))
        .or_else(|| staged.vertex_label(vertex).map(split_label_string))
        .unwrap_or_default();

    if add {
        for label in labels {
            if !current.iter().any(|existing| existing == label) {
                current.push(label.clone());
            }
        }
    } else {
        current.retain(|existing| !labels.iter().any(|removed| removed == existing));
    }

    let joined = current.join(":");
    staged
        .try_set_vertex_label(vertex, &joined)
        .map_err(|e| CypherError::Execution(e.to_string()))?;
    wtx.set_vertex_label(vertex, &joined);
    pending_labels.insert(vertex, joined);
    Ok(())
}

fn split_label_string(label: &str) -> Vec<String> {
    label
        .split(':')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
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
        Some(CreatedBinding::Scalar(_)) => Err(CypherError::Execution(format!(
            "variable '{variable}' is not a graph element"
        ))),
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

#[derive(Default)]
struct NativeDeleteBatch {
    vertices: HashSet<u64>,
    edges: HashSet<u64>,
}

fn delete_native_graph_value(
    wtx: &mut WriteTx<'_>,
    staged: &mut Graph,
    value: &Value,
    detach: bool,
) -> Result<(), CypherError> {
    let mut batch = NativeDeleteBatch::default();
    collect_native_delete_value(staged, value, &mut batch).map_err(|err| {
        CypherError::Execution(format!(
            "DELETE target expression is not a graph element: {err}"
        ))
    })?;

    let mut edges: Vec<_> = batch.edges.into_iter().map(EdgeId).collect();
    edges.sort_unstable_by_key(|edge| edge.0);
    for edge in edges {
        if staged.edge_exists(edge) {
            staged
                .try_remove_edge(edge)
                .map_err(|e| CypherError::Execution(e.to_string()))?;
            wtx.remove_edge(edge);
        }
    }

    let mut vertices: Vec<_> = batch.vertices.into_iter().map(VertexId).collect();
    vertices.sort_unstable_by_key(|vertex| vertex.0);
    for vertex in vertices {
        if !detach && snapshot_has_incident_edges(staged, vertex) {
            return Err(CypherError::Execution(
                "cannot DELETE node with relationships; use DETACH DELETE".into(),
            ));
        }
        if staged.vertex_label(vertex).is_some() {
            staged
                .try_remove_vertex(vertex)
                .map_err(|e| CypherError::Execution(e.to_string()))?;
            wtx.remove_vertex(vertex);
        }
    }

    Ok(())
}

fn collect_native_delete_value(
    snapshot: &Graph,
    value: &Value,
    batch: &mut NativeDeleteBatch,
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
                    .filter(|edge| snapshot.edge_exists(*edge))
            }) {
                batch.edges.insert(edge.0);
            }
        }
        for node_value in nodes {
            if let Some(vertex) = node_ref_id(&node_value).or_else(|| {
                node_value
                    .as_i64()
                    .map(|vertex_id| VertexId(vertex_id as u64))
                    .filter(|vertex| snapshot.vertex_label(*vertex).is_some())
            }) {
                batch.vertices.insert(vertex.0);
            }
        }
        return Ok(());
    }
    match value {
        Value::List(items) => {
            for item in items {
                collect_native_delete_value(snapshot, item, batch)?;
            }
            Ok(())
        }
        Value::Int64(id) => {
            let vertex = VertexId(*id as u64);
            if snapshot.vertex_label(vertex).is_some() {
                batch.vertices.insert(vertex.0);
                return Ok(());
            }
            let edge = EdgeId(*id as u64);
            if snapshot.edge_exists(edge) {
                batch.edges.insert(edge.0);
            }
            Ok(())
        }
        _ => Err("unsupported delete value"),
    }
}

fn execute_write_statement_in_tx(
    wtx: &mut WriteTx<'_>,
    statement: &WriteStatement,
    params: &HashMap<String, Value>,
    cancellation: Option<&Arc<AtomicBool>>,
) -> Result<QueryResult, CypherError> {
    check_cancellation_token(cancellation)?;
    let mut bindings = HashMap::new();
    let mut vertex_properties: HashMap<VertexId, HashMap<String, Value>> = HashMap::new();
    let edge_properties: HashMap<EdgeId, HashMap<String, Value>> = HashMap::new();

    for clause in &statement.clauses {
        check_cancellation_token(cancellation)?;
        match clause {
            WriteClause::Create(patterns) => {
                for (pattern_idx, pattern) in patterns.iter().enumerate() {
                    if pattern_idx % 1024 == 0 {
                        check_cancellation_token(cancellation)?;
                    }
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
                CreatedBinding::Scalar(_) => Ok(Value::Null),
            }
        }
        Expr::Literal(literal) => Ok(literal_to_value(literal)),
        Expr::Parameter(name) => params
            .get(name)
            .cloned()
            .ok_or_else(|| CypherError::Execution(format!("missing parameter '${name}'"))),
        other => Ok(eval_write_ast_expr_with_bindings(
            other, params, bindings, snapshot,
        )),
    }
}

fn eval_write_project_expr(
    expr: &ProjectExpr,
    params: &HashMap<String, Value>,
    bindings: &HashMap<String, CreatedBinding>,
    snapshot: Option<&Graph>,
    vertex_properties: &HashMap<VertexId, HashMap<String, Value>>,
    edge_properties: &HashMap<EdgeId, HashMap<String, Value>>,
) -> Result<Value, CypherError> {
    match expr {
        ProjectExpr::Wildcard => Err(CypherError::Execution(
            "DELETE * is not a graph element".into(),
        )),
        ProjectExpr::Variable(variable) => binding_to_value(variable, bindings),
        ProjectExpr::Property(property) => resolve_bound_property(
            snapshot.ok_or_else(|| {
                CypherError::Execution("property DELETE requires a graph snapshot".into())
            })?,
            bindings,
            vertex_properties,
            edge_properties,
            &property.variable,
            &property.property,
        ),
        ProjectExpr::Literal(value) => Ok(value.clone()),
        ProjectExpr::Expression(expr) => eval_write_return_expr(
            expr,
            params,
            bindings,
            snapshot,
            vertex_properties,
            edge_properties,
        ),
        ProjectExpr::Function { name, args } => {
            let args = args
                .iter()
                .map(project_expr_to_ast_expr)
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    CypherError::Execution(format!(
                        "unsupported DELETE target expression: {expr:?}"
                    ))
                })?;
            Ok(eval_write_ast_expr_with_bindings(
                &Expr::FunctionCall {
                    name: name.clone(),
                    args,
                },
                params,
                bindings,
                snapshot,
            ))
        }
    }
}

fn project_expr_to_ast_expr(expr: &ProjectExpr) -> Option<Expr> {
    match expr {
        ProjectExpr::Variable(variable) => Some(Expr::Variable(variable.clone())),
        ProjectExpr::Property(property) => Some(Expr::Property(PropertyAccess {
            variable: property.variable.clone(),
            property: property.property.clone(),
        })),
        ProjectExpr::Literal(value) => Some(Expr::Literal(value_to_literal(value)?)),
        ProjectExpr::Expression(expr) => Some(expr.clone()),
        ProjectExpr::Function { name, args } => Some(Expr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(project_expr_to_ast_expr)
                .collect::<Option<Vec<_>>>()?,
        }),
        ProjectExpr::Wildcard => None,
    }
}

fn value_to_literal(value: &Value) -> Option<Literal> {
    match value {
        Value::Int64(value) => Some(Literal::Integer(*value)),
        Value::Float64(value) => Some(Literal::Float(*value)),
        Value::String(value) => Some(Literal::String(value.clone())),
        Value::Bool(value) => Some(Literal::Bool(*value)),
        Value::Null => Some(Literal::Null),
        Value::List(_) | Value::Map(_) | Value::Bytes(_) => None,
    }
}

fn eval_write_ast_expr_with_bindings(
    expr: &Expr,
    params: &HashMap<String, Value>,
    bindings: &HashMap<String, CreatedBinding>,
    snapshot: Option<&Graph>,
) -> Value {
    let Some(snapshot) = snapshot else {
        return Value::Null;
    };
    let mut columns = Vec::with_capacity(bindings.len());
    let mut row = Vec::with_capacity(bindings.len());
    for (name, binding) in bindings {
        columns.push(name.clone());
        row.push(write_binding_value(binding));
    }
    let idx = nexus_index::composite::IndexSet::new();
    let ctx = QueryContext::with_indexes(snapshot, &idx).with_params(params.clone());
    nexus_cypher::executor::eval_ast_expr(&ctx, expr, &row, &columns)
}

fn binding_to_value(
    variable: &str,
    bindings: &HashMap<String, CreatedBinding>,
) -> Result<Value, CypherError> {
    match bindings.get(variable) {
        Some(CreatedBinding::Vertex(vertex)) => Ok(Value::Int64(vertex.0 as i64)),
        Some(CreatedBinding::Edge(edge)) => Ok(edge_ref(*edge)),
        Some(CreatedBinding::Scalar(value)) => Ok(value.clone()),
        None => Err(CypherError::Execution(format!(
            "variable '{variable}' is not bound in this CREATE"
        ))),
    }
}

fn write_binding_value(binding: &CreatedBinding) -> Value {
    match binding {
        CreatedBinding::Vertex(vertex) => node_ref(*vertex),
        CreatedBinding::Edge(edge) => edge_ref(*edge),
        CreatedBinding::Scalar(value) => value.clone(),
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

pub(crate) fn native_entrypoint_claims(query: &str) -> bool {
    let keyword: String = query
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_alphabetic())
        .map(|ch| ch.to_ascii_uppercase())
        .collect();

    matches!(
        keyword.as_str(),
        "MATCH"
            | "OPTIONAL"
            | "WITH"
            | "UNWIND"
            | "RETURN"
            | "CREATE"
            | "MERGE"
            | "SET"
            | "REMOVE"
            | "DELETE"
            | "DETACH"
    )
}

fn check_cancellation_token(cancellation: Option<&Arc<AtomicBool>>) -> Result<(), CypherError> {
    if cancellation.is_some_and(|token| token.load(Ordering::Relaxed)) {
        return Err(CypherError::Execution("query cancelled".into()));
    }
    Ok(())
}

fn check_row_budget(row_budget: Option<usize>, rows: usize) -> Result<(), CypherError> {
    if row_budget.is_some_and(|budget| rows > budget) {
        return Err(CypherError::Execution(format!(
            "query row budget exceeded: produced {rows} rows"
        )));
    }
    Ok(())
}

fn check_byte_budget(byte_budget: Option<usize>, bytes: usize) -> Result<(), CypherError> {
    if byte_budget.is_some_and(|budget| bytes > budget) {
        return Err(CypherError::Execution(format!(
            "query byte budget exceeded: estimated {bytes} bytes"
        )));
    }
    Ok(())
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
    fn execute_cypher_with_cancellation_token_stops_read_query() {
        let engine = build_engine(10);
        let cancellation = Arc::new(AtomicBool::new(true));

        let err = engine
            .execute_cypher_with_params_and_cancellation(
                "MATCH (n:Entity) RETURN n.name",
                HashMap::new(),
                Some(cancellation),
            )
            .unwrap_err();

        assert!(err.to_string().contains("query cancelled"));
    }

    #[test]
    fn execute_cypher_with_cancellation_token_stops_write_before_commit() {
        let engine = build_engine(0);
        let cancellation = Arc::new(AtomicBool::new(true));

        let err = engine
            .execute_cypher_with_params_and_cancellation(
                "CREATE (:Entity {name: 'cancelled'})",
                HashMap::new(),
                Some(cancellation),
            )
            .unwrap_err();

        assert!(err.to_string().contains("query cancelled"));
        assert_eq!(engine.vertex_count(), 0);
    }

    #[test]
    fn execute_cypher_with_cancellation_token_does_not_append_wal() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = NexusStore::open(dir.path()).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);
        let cancellation = Arc::new(AtomicBool::new(true));

        let err = engine
            .execute_cypher_with_params_and_cancellation(
                "CREATE (:Entity {name: 'cancelled-durable'})",
                HashMap::new(),
                Some(cancellation),
            )
            .unwrap_err();

        assert!(err.to_string().contains("query cancelled"));
        assert_eq!(engine.vertex_count(), 0);
        drop(engine);

        let reopened = NexusStore::open(dir.path()).unwrap();
        let recovered = reopened.load_graph(4, 4).unwrap();
        assert_eq!(recovered.num_vertices(), 0);
    }

    #[test]
    fn execute_cypher_with_row_budget_stops_large_read() {
        let engine = build_engine(2);

        let err = engine
            .execute_cypher_with_params_cancellation_and_row_budget(
                "MATCH (n:Entity) RETURN n.name",
                HashMap::new(),
                None,
                Some(1),
            )
            .unwrap_err();

        assert!(err.to_string().contains("query row budget exceeded"));
    }

    #[test]
    fn execute_cypher_with_byte_budget_stops_large_value() {
        let engine = build_engine(1);

        let err = engine
            .execute_cypher_with_params_cancellation_and_limits(
                "RETURN '0123456789abcdef' AS s",
                HashMap::new(),
                None,
                None,
                Some(8),
            )
            .unwrap_err();

        assert!(err.to_string().contains("query byte budget exceeded"));
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
    fn compact_storage_clears_tombstones_and_rebuilds_indexes() {
        let engine = build_engine(2);

        engine
            .execute_write(|wtx| {
                wtx.remove_vertex(VertexId(0));
                Ok(())
            })
            .unwrap();

        let stats = engine.compact_storage().unwrap();

        assert_eq!(stats.deleted_vertices_cleared, 1);
        let graph = engine.graph().read();
        assert!(graph.vertex_label(VertexId(0)).is_none());
        assert!(graph.get_vertex_property(VertexId(0), "name").is_null());
        drop(graph);

        let idx = engine.indexes().read();
        let (_, name_idx) = idx.find_composite("name").unwrap();
        assert!(!name_idx.get("v0").contains(&VertexId(0)));
        assert!(name_idx.get("v1").contains(&VertexId(1)));
    }

    #[test]
    fn compact_storage_is_safe_while_queries_are_running() {
        let engine = Arc::new(build_engine(50));
        engine
            .execute_write(|wtx| {
                for id in 0..10 {
                    wtx.remove_vertex(VertexId(id));
                }
                Ok(())
            })
            .unwrap();

        let barrier = Arc::new(std::sync::Barrier::new(5));
        let mut readers = Vec::new();
        for _ in 0..4 {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            readers.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..25 {
                    let result = engine
                        .execute_cypher("MATCH (n:Entity) RETURN count(n)")
                        .unwrap();
                    assert_eq!(result.rows[0][0], Value::Int64(40));
                }
            }));
        }

        barrier.wait();
        let stats = engine.compact_storage().unwrap();

        for reader in readers {
            reader.join().unwrap();
        }
        assert_eq!(stats.deleted_vertices_cleared, 10);
        assert_eq!(engine.compaction_pressure().total(), 0);
    }

    #[test]
    fn compact_storage_if_needed_respects_threshold() {
        let engine = build_engine(2);

        engine
            .execute_write(|wtx| {
                wtx.remove_vertex(VertexId(0));
                Ok(())
            })
            .unwrap();

        assert_eq!(engine.compaction_pressure().deleted_vertices, 1);
        assert!(engine.compact_storage_if_needed(2).unwrap().is_none());
        let stats = engine.compact_storage_if_needed(1).unwrap().unwrap();
        assert_eq!(stats.deleted_vertices_cleared, 1);
        assert!(engine.compaction_pressure().is_empty());
        assert!(engine.compact_storage_if_needed(1).unwrap().is_none());
    }

    #[test]
    fn vector_index_write_through_persists_to_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);

        engine.create_vector_index("entities", 2).unwrap();
        engine
            .upsert_vector("entities", VertexId(0), vec![1.0, 0.0])
            .unwrap();
        engine
            .upsert_vector("entities", VertexId(1), vec![0.0, 1.0])
            .unwrap();
        assert!(engine.remove_vector("entities", VertexId(0)).unwrap());

        let results = engine.vector_search("entities", &[1.0, 0.0], 2).unwrap();
        assert!(!results.iter().any(|(id, _)| *id == VertexId(0)));

        drop(engine);

        let store = NexusStore::open(&db_path).unwrap();
        let loaded = store.load_vector_index("entities").unwrap().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.tombstone_count(), 0);
        assert_eq!(loaded.search(&[0.0, 1.0], 1)[0].0, VertexId(1));
    }

    #[test]
    fn cross_model_batch_persists_vector_ops_before_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);

        engine.create_vector_index("entities", 2).unwrap();
        engine
            .execute_cross_model_batch(
                Some((
                    "CREATE (n:Entity {name: 'NVDA'}) RETURN n.name AS name",
                    HashMap::new(),
                )),
                vec![DocumentMutation::Upsert {
                    collection: "filings".into(),
                    key: "nvda-2024".into(),
                    document: serde_json::json!({ "ticker": "NVDA" }),
                }],
                vec![VectorMutation::Upsert {
                    index: "entities".into(),
                    vertex: VertexId(0),
                    embedding: vec![1.0, 0.0],
                }],
                None,
                None,
                None,
            )
            .unwrap();
        drop(engine);

        let recovered = NexusStore::open(&db_path).unwrap();
        assert_eq!(
            recovered
                .load_document("filings", "nvda-2024")
                .unwrap()
                .unwrap()["ticker"],
            "NVDA"
        );
        let graph = recovered.load_graph(4, 4).unwrap();
        assert_eq!(
            graph.get_vertex_property(VertexId(0), "name"),
            Value::String("NVDA".into())
        );
        let vectors = recovered.load_vector_index("entities").unwrap().unwrap();
        assert_eq!(vectors.search_exact(&[1.0, 0.0], 1)[0].0, VertexId(0));
    }

    #[test]
    fn cross_model_batch_runs_multiple_cypher_writes_atomically() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);

        let batch = engine
            .execute_cross_model_batch_multi(
                vec![
                    (
                        "CREATE (n:Entity {name: 'NVDA'}) RETURN n.name AS name".into(),
                        HashMap::new(),
                    ),
                    (
                        "MATCH (n:Entity) WHERE n.name = 'NVDA' SET n.status = 'seen' RETURN n.status AS status".into(),
                        HashMap::new(),
                    ),
                ],
                vec![DocumentMutation::Upsert {
                    collection: "filings".into(),
                    key: "nvda-2024".into(),
                    document: serde_json::json!({ "ticker": "NVDA" }),
                }],
                Vec::new(),
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(batch.cypher_results.len(), 2);
        assert_eq!(batch.cypher_results[0].columns, vec!["name"]);
        assert_eq!(
            batch.cypher_results[1].rows,
            vec![vec![Value::String("seen".into())]]
        );
        drop(engine);

        let recovered = NexusStore::open(&db_path).unwrap();
        assert_eq!(
            recovered
                .load_document("filings", "nvda-2024")
                .unwrap()
                .unwrap()["ticker"],
            "NVDA"
        );
        let graph = recovered.load_graph(4, 4).unwrap();
        assert_eq!(
            graph.get_vertex_property(VertexId(0), "name"),
            Value::String("NVDA".into())
        );
        assert_eq!(
            graph.get_vertex_property(VertexId(0), "status"),
            Value::String("seen".into())
        );
    }

    #[test]
    fn cross_model_batch_multi_statement_failure_aborts_all_models() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.register_vertex_property("name", PropertyType::String, true, false);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);

        let err = engine
            .execute_cross_model_batch_multi(
                vec![
                    (
                        "CREATE (n:Entity {name: 'NVDA'}) RETURN n.name AS name".into(),
                        HashMap::new(),
                    ),
                    (
                        "MATCH (n:Entity) WHERE n.name = 'NVDA' SET n.name = 42 RETURN n.name AS name".into(),
                        HashMap::new(),
                    ),
                ],
                vec![DocumentMutation::Upsert {
                    collection: "filings".into(),
                    key: "nvda-2024".into(),
                    document: serde_json::json!({ "ticker": "NVDA" }),
                }],
                Vec::new(),
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("type mismatch"));

        assert!(
            engine
                .load_document("filings", "nvda-2024")
                .unwrap()
                .is_none()
        );
        assert_eq!(engine.vertex_count(), 0);
    }

    #[test]
    fn vector_update_delete_compact_survives_hot_backup_restore() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let backup_path = dir.path().join("backup");
        let restore_path = dir.path().join("restore");
        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);

        engine.create_vector_index("entities", 2).unwrap();
        engine
            .upsert_vector("entities", VertexId(0), vec![1.0, 0.0])
            .unwrap();
        engine
            .upsert_vector("entities", VertexId(1), vec![0.0, 1.0])
            .unwrap();
        engine
            .upsert_vector("entities", VertexId(2), vec![0.7, 0.7])
            .unwrap();
        engine
            .upsert_vector("entities", VertexId(1), vec![0.0, 0.95])
            .unwrap();
        assert!(engine.remove_vector("entities", VertexId(0)).unwrap());
        engine.compact_vector_index("entities").unwrap();

        let manifest = engine.backup_to(&backup_path).unwrap();
        assert!(
            manifest
                .files
                .iter()
                .any(|file| file == "vectors/entities.json")
        );
        drop(engine);

        NexusStore::restore_backup(&backup_path, &restore_path).unwrap();
        let restored = NexusStore::open(&restore_path).unwrap();
        let loaded = restored.load_vector_index("entities").unwrap().unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.tombstone_count(), 0);
        assert!(
            !loaded
                .search(&[1.0, 0.0], 3)
                .iter()
                .any(|(id, _)| *id == VertexId(0)),
            "deleted vectors must not reappear after compacted backup restore"
        );
        assert_eq!(loaded.search(&[0.0, 1.0], 1)[0].0, VertexId(1));
        assert_eq!(loaded.search(&[0.7, 0.7], 1)[0].0, VertexId(2));
    }

    #[test]
    fn vector_index_loads_from_store_into_engine() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        {
            let store = NexusStore::open(&db_path).unwrap();
            let mut vectors = VectorIndex::new(2);
            vectors.add(VertexId(42), vec![0.0, 1.0]);
            store.save_vector_index("entities", &vectors).unwrap();
        }

        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);

        assert!(engine.load_vector_index("entities").unwrap());
        assert_eq!(engine.vector_index_names(), vec!["entities".to_string()]);
        assert_eq!(
            engine.vector_search("entities", &[0.0, 1.0], 1).unwrap()[0].0,
            VertexId(42)
        );
    }

    #[test]
    fn vector_indexes_auto_load_from_store_into_engine() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        {
            let store = NexusStore::open(&db_path).unwrap();
            let mut entities = VectorIndex::new(2);
            entities.add(VertexId(42), vec![0.0, 1.0]);
            let mut chunks = VectorIndex::new(2);
            chunks.add(VertexId(7), vec![1.0, 0.0]);
            store.save_vector_index("entities", &entities).unwrap();
            store.save_vector_index("chunks", &chunks).unwrap();
        }

        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);

        assert_eq!(
            engine.load_all_vector_indexes().unwrap(),
            vec!["chunks".to_string(), "entities".to_string()]
        );
        assert_eq!(
            engine.vector_index_names(),
            vec!["chunks".to_string(), "entities".to_string()]
        );
        assert_eq!(
            engine.vector_search("entities", &[0.0, 1.0], 1).unwrap()[0].0,
            VertexId(42)
        );
        assert_eq!(
            engine.vector_search("chunks", &[1.0, 0.0], 1).unwrap()[0].0,
            VertexId(7)
        );
    }

    #[test]
    fn document_collections_persist_through_engine() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);

        engine
            .upsert_document(
                "filings",
                "nvda-2024",
                serde_json::json!({
                    "ticker": "NVDA",
                    "year": 2024,
                    "tags": ["10-K", "risk"]
                }),
            )
            .unwrap();

        assert_eq!(
            engine.list_document_collections().unwrap(),
            vec!["filings".to_string()]
        );
        assert_eq!(
            engine
                .load_document("filings", "nvda-2024")
                .unwrap()
                .unwrap()["ticker"],
            "NVDA"
        );
        assert_eq!(
            engine.list_documents("filings", Some(1)).unwrap()[0].key,
            "nvda-2024"
        );
        assert!(engine.delete_document("filings", "nvda-2024").unwrap());
        assert!(
            engine
                .load_document("filings", "nvda-2024")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn execute_cypher_can_read_document_collection() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);
        engine
            .upsert_document(
                "filings",
                "nvda-2024",
                serde_json::json!({
                    "ticker": "NVDA",
                    "year": 2024,
                    "nested": { "form": "10-K" }
                }),
            )
            .unwrap();

        let qr = engine
            .execute_cypher("RETURN document('filings', 'nvda-2024').ticker AS ticker, document('filings', 'nvda-2024').nested.form AS form")
            .unwrap();

        assert_eq!(qr.columns, vec!["ticker", "form"]);
        assert_eq!(
            qr.rows,
            vec![vec![
                Value::String("NVDA".into()),
                Value::String("10-K".into())
            ]]
        );
    }

    #[test]
    fn execute_cypher_can_scan_document_collection_with_unwind() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);
        engine
            .upsert_document(
                "filings",
                "aapl-2024",
                serde_json::json!({ "ticker": "AAPL", "year": 2024 }),
            )
            .unwrap();
        engine
            .upsert_document(
                "filings",
                "nvda-2024",
                serde_json::json!({ "ticker": "NVDA", "year": 2024 }),
            )
            .unwrap();
        engine.create_document_index("filings", "ticker").unwrap();

        let qr = engine
            .execute_cypher(
                "UNWIND documents('filings', 10) AS doc \
                 RETURN doc.key AS key, doc.document.ticker AS ticker ORDER BY key",
            )
            .unwrap();

        assert_eq!(qr.columns, vec!["key", "ticker"]);
        assert_eq!(
            qr.rows,
            vec![
                vec![
                    Value::String("aapl-2024".into()),
                    Value::String("AAPL".into())
                ],
                vec![
                    Value::String("nvda-2024".into()),
                    Value::String("NVDA".into())
                ],
            ]
        );

        let indexed = engine
            .execute_cypher(
                "UNWIND documentsBy('filings', 'ticker', 'NVDA', 10) AS doc \
                 RETURN doc.key AS key, doc.document.year AS year",
            )
            .unwrap();
        assert_eq!(indexed.columns, vec!["key", "year"]);
        assert_eq!(
            indexed.rows,
            vec![vec![Value::String("nvda-2024".into()), Value::Int64(2024)]]
        );
    }

    #[test]
    fn execute_cypher_can_match_document_collection() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let store = NexusStore::open(&db_path).unwrap();
        let mut graph = Graph::new(0, 0);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);
        engine
            .upsert_document(
                "filings",
                "aapl-2024",
                serde_json::json!({
                    "ticker": "AAPL",
                    "year": 2024,
                    "body": "cash flow and supply chain notes"
                }),
            )
            .unwrap();
        engine
            .upsert_document(
                "filings",
                "nvda-2024",
                serde_json::json!({
                    "ticker": "NVDA",
                    "year": 2024,
                    "body": "revenue growth and risk factors"
                }),
            )
            .unwrap();
        engine
            .upsert_document(
                "filings",
                "nvda-2025",
                serde_json::json!({
                    "ticker": "NVDA",
                    "year": 2025,
                    "body": "revenue outlook and services margin"
                }),
            )
            .unwrap();

        let qr = engine
            .execute_cypher(
                "MATCH DOCUMENT doc IN filings \
                 WHERE doc.document.ticker = 'NVDA' \
                 RETURN doc.key AS key, doc.document.year AS year ORDER BY year",
            )
            .unwrap();

        assert_eq!(qr.columns, vec!["key", "year"]);
        assert_eq!(
            qr.rows,
            vec![
                vec![Value::String("nvda-2024".into()), Value::Int64(2024)],
                vec![Value::String("nvda-2025".into()), Value::Int64(2025)]
            ]
        );

        engine.create_document_index("filings", "ticker").unwrap();
        let indexed = engine
            .execute_cypher(
                "MATCH DOCUMENT doc IN filings \
                 WHERE doc.document.ticker = 'NVDA' \
                 RETURN doc.key AS key ORDER BY key",
            )
            .unwrap();
        assert_eq!(indexed.columns, vec!["key"]);
        assert_eq!(
            indexed.rows,
            vec![
                vec![Value::String("nvda-2024".into())],
                vec![Value::String("nvda-2025".into())]
            ]
        );

        let prefix_without_index = engine
            .execute_cypher(
                "MATCH DOCUMENT doc IN filings \
                 WHERE doc.document.ticker STARTS WITH 'NV' \
                 RETURN doc.key AS key ORDER BY key",
            )
            .unwrap();
        assert_eq!(
            prefix_without_index.rows,
            vec![
                vec![Value::String("nvda-2024".into())],
                vec![Value::String("nvda-2025".into())]
            ]
        );

        let range_without_index = engine
            .execute_cypher(
                "MATCH DOCUMENT doc IN filings \
                 WHERE doc.document.year > 2024 \
                 RETURN doc.key AS key",
            )
            .unwrap();
        assert_eq!(
            range_without_index.rows,
            vec![vec![Value::String("nvda-2025".into())]]
        );

        engine.create_document_index("filings", "year").unwrap();
        let range_indexed = engine
            .execute_cypher(
                "MATCH DOCUMENT doc IN filings \
                 WHERE doc.document.year > 2024 \
                 RETURN doc.key AS key ORDER BY key",
            )
            .unwrap();
        assert_eq!(
            range_indexed.rows,
            vec![vec![Value::String("nvda-2025".into())]]
        );

        let full_text_without_index = engine
            .execute_cypher(
                "MATCH DOCUMENT doc IN filings \
                 WHERE documentFullText(doc.document.body, 'revenue risk') \
                 RETURN doc.key AS key ORDER BY key",
            )
            .unwrap();
        assert_eq!(
            full_text_without_index.rows,
            vec![
                vec![Value::String("nvda-2024".into())],
                vec![Value::String("nvda-2025".into())]
            ]
        );

        engine
            .create_document_index_with_kind("filings", "body", DocumentIndexKind::FullText)
            .unwrap();
        let full_text_indexed = engine
            .execute_cypher(
                "MATCH DOCUMENT doc IN filings \
                 WHERE documentFullText(doc.document.body, 'risk') \
                 RETURN doc.key AS key",
            )
            .unwrap();
        assert_eq!(
            full_text_indexed.rows,
            vec![vec![Value::String("nvda-2024".into())]]
        );

        let substring_contains = engine
            .execute_cypher(
                "MATCH DOCUMENT doc IN filings \
                 WHERE doc.document.body CONTAINS 'venue' \
                 RETURN doc.key AS key ORDER BY key",
            )
            .unwrap();
        assert_eq!(
            substring_contains.rows,
            vec![
                vec![Value::String("nvda-2024".into())],
                vec![Value::String("nvda-2025".into())]
            ]
        );
    }

    #[test]
    fn execute_cross_model_write_persists_graph_and_document_in_one_commit() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");

        {
            let store = NexusStore::open(&db_path).unwrap();
            let mut graph = Graph::new(0, 0);
            graph.register_vertex_property("name", PropertyType::String, true, false);
            graph.build();
            let engine = NexusEngine::with_store(graph, store);

            let vertex = engine
                .execute_cross_model_write(|wtx, docs| {
                    let vertex = wtx.add_vertex("Document");
                    wtx.set_vertex_property(vertex, "name", Value::String("NVDA filing".into()));
                    docs.upsert(
                        "filings",
                        "nvda-2024",
                        serde_json::json!({
                            "ticker": "NVDA",
                            "vertex_id": vertex.0
                        }),
                    )?;
                    Ok(vertex)
                })
                .unwrap();

            assert_eq!(vertex, VertexId(0));
            assert_eq!(
                engine
                    .load_document("filings", "nvda-2024")
                    .unwrap()
                    .unwrap()["ticker"],
                "NVDA"
            );
        }

        let reopened = NexusStore::open(&db_path).unwrap();
        let recovered = reopened.load_graph(4, 4).unwrap();
        assert_eq!(recovered.vertex_label(VertexId(0)), Some("Document"));
        assert_eq!(
            recovered.get_vertex_property(VertexId(0), "name"),
            Value::String("NVDA filing".into())
        );
        assert_eq!(
            reopened
                .load_document("filings", "nvda-2024")
                .unwrap()
                .unwrap()["vertex_id"],
            0
        );
    }

    #[test]
    fn vector_index_dimension_errors_do_not_replace_existing_index() {
        let engine = build_engine(0);
        engine.create_vector_index("entities", 2).unwrap();
        engine
            .upsert_vector("entities", VertexId(7), vec![0.0, 1.0])
            .unwrap();

        assert!(
            engine
                .upsert_vector("entities", VertexId(8), vec![0.0, 1.0, 2.0])
                .is_err()
        );

        let results = engine.vector_search("entities", &[0.0, 1.0], 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, VertexId(7));
    }

    #[test]
    fn vector_search_metrics_track_exact_overlap() {
        let engine = build_engine(0);
        engine.create_vector_index("entities", 2).unwrap();
        engine
            .upsert_vector("entities", VertexId(0), vec![1.0, 0.0])
            .unwrap();
        engine
            .upsert_vector("entities", VertexId(1), vec![0.0, 1.0])
            .unwrap();

        let results = engine.vector_search("entities", &[1.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);

        let metrics = engine.vector_search_metrics();
        assert_eq!(metrics.searches_total, 1);
        assert_eq!(metrics.results_returned_total, 2);
        assert_eq!(metrics.exact_candidates_total, 2);
        assert_eq!(metrics.exact_overlap_total, 2);
        let by_index = engine.vector_search_metrics_by_index();
        assert_eq!(by_index.get("entities"), Some(&metrics));
    }

    #[test]
    fn execute_cypher_can_query_named_vector_index() {
        let engine = build_engine(2);
        engine.create_vector_index("entities", 2).unwrap();
        engine
            .upsert_vector("entities", VertexId(0), vec![1.0, 0.0])
            .unwrap();
        engine
            .upsert_vector("entities", VertexId(1), vec![0.0, 1.0])
            .unwrap();
        assert_eq!(
            engine.vector_search_metrics(),
            VectorSearchMetrics::default()
        );

        let result = engine
            .execute_cypher(
                "UNWIND vectorSearch('entities', [1.0, 0.0], 2) AS hit \
                 RETURN hit.vertex_id, hit.distance ORDER BY hit.distance",
            )
            .unwrap();

        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][0], Value::Int64(0));
        assert_eq!(result.rows[1][0], Value::Int64(1));

        let metrics = engine.vector_search_metrics();
        assert_eq!(metrics.searches_total, 1);
        assert_eq!(metrics.results_returned_total, 2);
        assert_eq!(metrics.exact_candidates_total, 2);
        assert_eq!(metrics.exact_overlap_total, 2);
        let by_index = engine.vector_search_metrics_by_index();
        assert_eq!(by_index.get("entities"), Some(&metrics));
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
    fn durable_recovery_skips_truncated_active_wal_tail_after_committed_write() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let store = NexusStore::open(&db_path).unwrap();

        let mut graph = Graph::new(4, 4);
        graph.register_vertex_property("name", PropertyType::String, true, false);
        graph.build();
        let engine = NexusEngine::with_store(graph, store);

        engine
            .execute_cypher("CREATE (:Entity {name: 'durable'})")
            .unwrap();
        drop(engine);

        {
            use std::io::Write as _;

            let mut wal = std::fs::OpenOptions::new()
                .append(true)
                .open(db_path.join("graph.wal"))
                .unwrap();
            wal.write_all(b"123\t{\"AddVertex\":{\"id\":1,\"label\":\"torn\"")
                .unwrap();
            wal.sync_all().unwrap();
        }

        let store = NexusStore::open(&db_path).unwrap();
        let recovered = store.load_graph(4, 4).unwrap();

        assert_eq!(
            recovered.get_vertex_property(VertexId(0), "name").as_str(),
            Some("durable")
        );
        assert_eq!(
            recovered.vertex_label(VertexId(1)),
            None,
            "truncated WAL tail must not create a partial vertex"
        );
    }

    #[test]
    fn hot_backup_during_concurrent_writes_restores_a_valid_cutoff() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let backup_path = dir.path().join("backup");
        let restore_path = dir.path().join("restore");
        let store = NexusStore::open(&db_path).unwrap();

        let mut graph = Graph::new(64, 4);
        graph.register_vertex_property("name", PropertyType::String, true, false);
        graph.build();
        let engine = Arc::new(NexusEngine::with_store(graph, store));
        let written = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let writer_engine = Arc::clone(&engine);
        let writer_count = Arc::clone(&written);
        let writer = std::thread::spawn(move || {
            for idx in 0..30 {
                writer_engine
                    .execute_cypher(&format!("CREATE (:Entity {{name: 'w{idx}'}})"))
                    .unwrap();
                writer_count.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });

        while written.load(Ordering::SeqCst) < 5 {
            std::thread::yield_now();
        }

        let manifest = engine.backup_to(&backup_path).unwrap();
        assert!(manifest.files.iter().any(|file| file == "graph.wal"));

        writer.join().unwrap();
        assert_eq!(engine.vertex_count(), 30);

        NexusStore::restore_backup(&backup_path, &restore_path).unwrap();
        let restored = NexusStore::open(&restore_path).unwrap();
        let graph = restored.load_graph(64, 4).unwrap();
        let restored_count = graph.num_vertices();

        assert!(
            (5..=30).contains(&restored_count),
            "backup should restore a consistent cutoff, got {restored_count} vertices"
        );
        for id in 0..restored_count as u64 {
            assert_eq!(graph.vertex_label(VertexId(id)), Some("Entity"));
            assert!(
                graph
                    .get_vertex_property(VertexId(id), "name")
                    .as_str()
                    .is_some()
            );
        }
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
    fn unsupported_feature_matrix_rejects_without_partial_write() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        let engine = builder.build();

        let unsupported_queries = [
            "CALL db.labels() RETURN label",
            "CREATE (:Entity {name: 'partial-call'}) CALL db.labels()",
            "CREATE (:Entity {name: 'partial-load'}) LOAD CSV FROM 'file:///x.csv' AS row RETURN row",
        ];

        for query in unsupported_queries {
            let err = match engine.execute_cypher(query) {
                Ok(result) => panic!("unsupported query must fail: {query}, got {result:?}"),
                Err(err) => err,
            };
            assert!(
                !err.to_string().is_empty(),
                "unsupported query produced an empty error: {query}"
            );
        }
        assert_eq!(engine.vertex_count(), 0);
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
    fn execute_cypher_merge_on_create_and_on_match_actions() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        builder.register_vertex_property("created", PropertyType::Bool, false, false);
        builder.register_vertex_property("seen", PropertyType::Bool, false, false);
        let engine = builder.build();

        let result = engine
            .execute_cypher(
                "MERGE (n:Entity {name: 'Alice'}) ON CREATE SET n.created = true ON MATCH SET n.seen = true RETURN n.created, n.seen",
            )
            .unwrap();
        assert_eq!(result.rows, vec![vec![Value::Bool(true), Value::Null]]);

        let result = engine
            .execute_cypher(
                "MERGE (n:Entity {name: 'Alice'}) ON CREATE SET n.created = false ON MATCH SET n.seen = true RETURN n.created, n.seen",
            )
            .unwrap();
        assert_eq!(
            result.rows,
            vec![vec![Value::Bool(true), Value::Bool(true)]]
        );
    }

    #[test]
    fn execute_cypher_merge_on_create_copies_graph_properties() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        builder.register_edge_property("name", PropertyType::String, false, false);
        let engine = builder.build();

        engine
            .execute_cypher("CREATE (a:Entity {name: 'A'}), (b:Entity {name: 'B'})")
            .unwrap();

        let result = engine
            .execute_cypher(
                "MATCH (a {name: 'A'}), (b {name: 'B'}) MERGE (a)-[r:TYPE]->(b) ON CREATE SET r = a RETURN r.name",
            )
            .unwrap();
        assert_eq!(result.rows, vec![vec![Value::String("A".into())]]);
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
    fn compact_storage_snapshots_compacted_graph_and_compacts_wal() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let engine = empty_engine_with_store(&db_path);

        engine
            .execute_cypher(
                "CREATE (a:Entity {name: 'A'}), (b:Entity {name: 'B'}) CREATE (a)-[:KNOWS]->(b)",
            )
            .unwrap();
        engine
            .execute_cypher("MATCH (a:Entity) WHERE a.name = 'A' DETACH DELETE a")
            .unwrap();

        let stats = engine.compact_storage().unwrap();

        assert_eq!(stats.deleted_vertices_cleared, 1);
        assert_eq!(stats.deleted_edges_cleared, 1);
        assert_eq!(stats.tombstoned_edge_meta_removed, 1);

        drop(engine);

        let store = NexusStore::open(&db_path).unwrap();
        assert!(store.recover().unwrap().is_empty());
        let recovered = store.load_graph(4, 4).unwrap();

        assert!(recovered.vertex_label(VertexId(0)).is_none());
        assert!(recovered.get_vertex_property(VertexId(0), "name").is_null());
        assert_eq!(recovered.vertex_label(VertexId(1)), Some("Entity"));
        assert_eq!(
            recovered.get_vertex_property(VertexId(1), "name"),
            Value::String("B".into())
        );
        assert!(recovered.edge_records().is_empty());
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
    fn execute_cypher_label_mutations_persist_to_wal_before_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let engine = empty_engine_with_store(&db_path);

        engine
            .execute_cypher("CREATE (n:Entity {name: 'Labelled'})")
            .unwrap();
        engine
            .execute_cypher(
                "MATCH (n:Entity) WHERE n.name = 'Labelled' SET n:Person REMOVE n:Entity",
            )
            .unwrap();

        drop(engine);

        let store = NexusStore::open(&db_path).unwrap();
        let recovered = store.load_graph(4, 4).unwrap();

        assert_eq!(recovered.vertex_label(VertexId(0)), Some("Person"));
        assert_eq!(
            recovered.get_vertex_property(VertexId(0), "name").as_str(),
            Some("Labelled")
        );

        let engine = NexusEngine::with_store(recovered, store);
        let result = engine
            .execute_cypher("MATCH (n:Person) RETURN n.name")
            .unwrap();
        assert_eq!(result.num_rows(), 1);
        assert_eq!(result.rows[0][0], Value::String("Labelled".into()));
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
    fn execute_cypher_merge_path_binding_persists_to_wal_before_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let engine = empty_engine_with_store(&db_path);

        let result = engine
            .execute_cypher(
                "MERGE p = (a:Entity {name: 'A'})-[:KNOWS]->(b:Entity {name: 'B'}) \
                 RETURN length(p)",
            )
            .unwrap();
        assert_eq!(result.rows, vec![vec![Value::Int64(1)]]);

        drop(engine);

        let store = NexusStore::open(&db_path).unwrap();
        let recovered = store.load_graph(4, 4).unwrap();

        assert_eq!(recovered.vertex_label(VertexId(0)), Some("Entity"));
        assert_eq!(recovered.vertex_label(VertexId(1)), Some("Entity"));
        assert_eq!(
            recovered.neighbors(VertexId(0), "KNOWS", Direction::Outgoing),
            vec![VertexId(1)]
        );
    }

    #[test]
    fn execute_cypher_delete_expression_target_persists_to_wal_before_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let engine = empty_engine_with_store(&db_path);

        engine
            .execute_cypher("CREATE (a:Entity {name: 'A'})-[:KNOWS]->(b:Entity {name: 'B'})")
            .unwrap();
        engine
            .execute_cypher("MATCH (a:Entity)-[r:KNOWS]->(b:Entity) DELETE [r]")
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
    fn execute_cypher_write_stream_reads_staged_writes() {
        let mut builder = EngineBuilder::new(0, 0);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        let engine = builder.build();

        let result = engine
            .execute_cypher(
                "CREATE (n:Entity {name: 'A'}) \
                 WITH n \
                 MATCH (m:Entity) WHERE m.name = n.name \
                 RETURN m.name",
            )
            .unwrap();

        assert_eq!(result.rows, vec![vec![Value::String("A".into())]]);
    }

    #[test]
    fn execute_cypher_write_stream_persists_to_wal_before_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let engine = empty_engine_with_store(&db_path);

        let result = engine
            .execute_cypher(
                "CREATE (a:Entity {name: 'A'}) \
                 WITH a \
                 UNWIND ['B', 'C'] AS friend \
                 CREATE (b:Entity {name: friend}) \
                 CREATE (a)-[:KNOWS]->(b) \
                 RETURN count(b)",
            )
            .unwrap();
        assert_eq!(result.rows, vec![vec![Value::Int64(2)]]);

        drop(engine);

        let store = NexusStore::open(&db_path).unwrap();
        let recovered = store.load_graph(4, 4).unwrap();

        assert_eq!(recovered.vertex_label(VertexId(0)), Some("Entity"));
        assert_eq!(recovered.vertex_label(VertexId(1)), Some("Entity"));
        assert_eq!(recovered.vertex_label(VertexId(2)), Some("Entity"));
        assert_eq!(
            recovered.neighbors(VertexId(0), "KNOWS", Direction::Outgoing),
            vec![VertexId(1), VertexId(2)]
        );
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
