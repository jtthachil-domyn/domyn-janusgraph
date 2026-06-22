//! Query execution context: holds references to the graph and optional indexes.

use nexus_core::graph::Graph;
use nexus_core::types::{Value, VertexId};
use nexus_index::composite::IndexSet;
use nexus_index::vector::VectorIndex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{CypherError, CypherResult};

/// Optional telemetry hook for vector-search callers.
///
/// `nexus-cypher` owns the query semantics, but it should not know about
/// Prometheus, HTTP, or server runtime state. Servers can implement this hook
/// and attach it to a `QueryContext` when they want vector functions to emit
/// recall/overlap counters.
pub trait VectorSearchRecorder {
    fn record_vector_search(
        &self,
        index_name: &str,
        results_returned: usize,
        exact_candidates: usize,
        exact_overlap: usize,
    );
}

/// Optional document lookup hook used by server-backed Cypher execution.
///
/// The Cypher crate keeps this abstract so document collections can live in
/// storage/server code today and later lower through the same internal IR for
/// GQL without baking file layout details into query execution.
pub trait DocumentResolver {
    fn resolve_document(&self, collection: &str, key: &str) -> Option<Value>;

    fn scan_documents(&self, _collection: &str, _limit: usize) -> Vec<Value> {
        Vec::new()
    }

    fn query_documents_by_index(
        &self,
        _collection: &str,
        _path: &str,
        _value: &Value,
        _limit: usize,
    ) -> Vec<Value> {
        Vec::new()
    }

    fn query_documents_by_prefix(
        &self,
        _collection: &str,
        _path: &str,
        _prefix: &str,
        _limit: usize,
    ) -> Vec<Value> {
        Vec::new()
    }

    fn query_documents_by_range(
        &self,
        _collection: &str,
        _path: &str,
        _gte: Option<&Value>,
        _lte: Option<&Value>,
        _limit: usize,
    ) -> Vec<Value> {
        Vec::new()
    }

    fn query_documents_by_full_text(
        &self,
        _collection: &str,
        _path: &str,
        _query: &str,
        _limit: usize,
    ) -> Vec<Value> {
        Vec::new()
    }
}

/// Read-side query context: graph + optional indexes + parameters.
///
/// The executor receives this instead of a bare `&Graph` so it can
/// transparently use index lookups when an `IndexSet` is available and
/// resolve `$param` references in WHERE clauses.
pub struct QueryContext<'a> {
    pub graph: &'a Graph,
    pub indexes: Option<&'a IndexSet>,
    pub vector_indexes: Option<&'a HashMap<String, VectorIndex>>,
    pub vector_recorder: Option<&'a dyn VectorSearchRecorder>,
    pub document_resolver: Option<&'a dyn DocumentResolver>,
    pub cancellation: Option<Arc<AtomicBool>>,
    pub row_budget: Option<usize>,
    pub byte_budget: Option<usize>,
    pub params: HashMap<String, Value>,
}

/// Write-side execution context: mutable graph handle + parameters.
///
/// Kept distinct from `QueryContext` so the read executor's immutable
/// borrow guarantees remain unchanged.
pub struct WriteContext<'a> {
    pub graph: &'a mut Graph,
    pub params: HashMap<String, Value>,
    pub cancellation: Option<Arc<AtomicBool>>,
    pub row_budget: Option<usize>,
    pub byte_budget: Option<usize>,
}

impl<'a> WriteContext<'a> {
    pub fn new(graph: &'a mut Graph) -> Self {
        Self {
            graph,
            params: HashMap::new(),
            cancellation: None,
            row_budget: None,
            byte_budget: None,
        }
    }

    pub fn with_params(mut self, params: HashMap<String, Value>) -> Self {
        self.params = params;
        self
    }

    pub fn with_cancellation(mut self, cancellation: Arc<AtomicBool>) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub fn with_row_budget(mut self, row_budget: usize) -> Self {
        self.row_budget = Some(row_budget);
        self
    }

    pub fn with_byte_budget(mut self, byte_budget: usize) -> Self {
        self.byte_budget = Some(byte_budget);
        self
    }

    pub fn check_cancelled(&self) -> CypherResult<()> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(|token| token.load(Ordering::Relaxed))
        {
            return Err(CypherError::Execution("query cancelled".into()));
        }
        Ok(())
    }

    pub fn check_row_budget(&self, rows: usize) -> CypherResult<()> {
        if self.row_budget.is_some_and(|budget| rows > budget) {
            return Err(CypherError::Execution(format!(
                "query row budget exceeded: produced {rows} rows"
            )));
        }
        Ok(())
    }

    pub fn check_byte_budget(&self, bytes: usize) -> CypherResult<()> {
        if self.byte_budget.is_some_and(|budget| bytes > budget) {
            return Err(CypherError::Execution(format!(
                "query byte budget exceeded: estimated {bytes} bytes"
            )));
        }
        Ok(())
    }
}

/// Summary of effects for a write statement, returned to the caller.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WriteSummary {
    pub nodes_created: usize,
    pub nodes_deleted: usize,
    pub edges_created: usize,
    pub edges_deleted: usize,
    pub properties_set: usize,
    pub properties_removed: usize,
}

impl<'a> QueryContext<'a> {
    pub fn new(graph: &'a Graph) -> Self {
        Self {
            graph,
            indexes: None,
            vector_indexes: None,
            vector_recorder: None,
            document_resolver: None,
            cancellation: None,
            row_budget: None,
            byte_budget: None,
            params: HashMap::new(),
        }
    }

    pub fn with_indexes(graph: &'a Graph, indexes: &'a IndexSet) -> Self {
        Self {
            graph,
            indexes: Some(indexes),
            vector_indexes: None,
            vector_recorder: None,
            document_resolver: None,
            cancellation: None,
            row_budget: None,
            byte_budget: None,
            params: HashMap::new(),
        }
    }

    pub fn with_params(mut self, params: HashMap<String, Value>) -> Self {
        self.params = params;
        self
    }

    pub fn with_vector_indexes(mut self, vector_indexes: &'a HashMap<String, VectorIndex>) -> Self {
        self.vector_indexes = Some(vector_indexes);
        self
    }

    pub fn with_vector_recorder(mut self, recorder: &'a dyn VectorSearchRecorder) -> Self {
        self.vector_recorder = Some(recorder);
        self
    }

    pub fn with_document_resolver(mut self, resolver: &'a dyn DocumentResolver) -> Self {
        self.document_resolver = Some(resolver);
        self
    }

    pub fn with_cancellation(mut self, cancellation: Arc<AtomicBool>) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub fn with_row_budget(mut self, row_budget: usize) -> Self {
        self.row_budget = Some(row_budget);
        self
    }

    pub fn with_byte_budget(mut self, byte_budget: usize) -> Self {
        self.byte_budget = Some(byte_budget);
        self
    }

    pub fn check_cancelled(&self) -> CypherResult<()> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(|token| token.load(Ordering::Relaxed))
        {
            return Err(CypherError::Execution("query cancelled".into()));
        }
        Ok(())
    }

    pub fn check_row_budget(&self, rows: usize) -> CypherResult<()> {
        if self.row_budget.is_some_and(|budget| rows > budget) {
            return Err(CypherError::Execution(format!(
                "query row budget exceeded: produced {rows} rows"
            )));
        }
        Ok(())
    }

    pub fn check_byte_budget(&self, bytes: usize) -> CypherResult<()> {
        if self.byte_budget.is_some_and(|budget| bytes > budget) {
            return Err(CypherError::Execution(format!(
                "query byte budget exceeded: estimated {bytes} bytes"
            )));
        }
        Ok(())
    }

    /// Try to look up vertex IDs by property value using indexes.
    /// Returns `None` if no suitable index exists, falling back to scan.
    pub fn index_lookup(&self, property: &str, value: &Value) -> Option<Vec<VertexId>> {
        let indexes = self.indexes?;

        // Try unique index first (O(1), at most one result)
        if let Some((_, unique_idx)) = indexes.find_unique(property) {
            if let Value::String(s) = value {
                return unique_idx.get(s).map(|vid| vec![vid]);
            }
        }

        // Try composite index (O(1) lookup, may return multiple)
        if let Some((_, composite_idx)) = indexes.find_composite(property) {
            if let Value::String(s) = value {
                let results = composite_idx.get(s);
                if !results.is_empty() {
                    return Some(results);
                }
            }
        }

        None
    }
}

pub fn estimate_query_result_bytes(columns: &[String], rows: &[Vec<Value>]) -> usize {
    columns.iter().map(|column| column.len()).sum::<usize>()
        + rows
            .iter()
            .map(|row| estimate_row_bytes(row))
            .sum::<usize>()
}

pub fn estimate_row_bytes(row: &[Value]) -> usize {
    row.iter().map(estimate_value_bytes).sum()
}

pub fn estimate_value_bytes(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int64(_) | Value::Float64(_) => 8,
        Value::String(value) => value.len(),
        Value::Bytes(bytes) => bytes.len(),
        Value::List(items) => items.iter().map(estimate_value_bytes).sum(),
        Value::Map(entries) => entries
            .iter()
            .map(|(key, value)| key.len() + estimate_value_bytes(value))
            .sum(),
    }
}
