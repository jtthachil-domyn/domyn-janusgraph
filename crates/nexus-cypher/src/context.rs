//! Query execution context: holds references to the graph and optional indexes.

use nexus_core::graph::Graph;
use nexus_core::types::{Value, VertexId};
use nexus_index::composite::IndexSet;
use std::collections::HashMap;

/// Read-side query context: graph + optional indexes + parameters.
///
/// The executor receives this instead of a bare `&Graph` so it can
/// transparently use index lookups when an `IndexSet` is available and
/// resolve `$param` references in WHERE clauses.
pub struct QueryContext<'a> {
    pub graph: &'a Graph,
    pub indexes: Option<&'a IndexSet>,
    pub params: HashMap<String, Value>,
}

/// Write-side execution context: mutable graph handle + parameters.
///
/// Kept distinct from `QueryContext` so the read executor's immutable
/// borrow guarantees remain unchanged.
pub struct WriteContext<'a> {
    pub graph: &'a mut Graph,
    pub params: HashMap<String, Value>,
}

impl<'a> WriteContext<'a> {
    pub fn new(graph: &'a mut Graph) -> Self {
        Self {
            graph,
            params: HashMap::new(),
        }
    }

    pub fn with_params(mut self, params: HashMap<String, Value>) -> Self {
        self.params = params;
        self
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
            params: HashMap::new(),
        }
    }

    pub fn with_indexes(graph: &'a Graph, indexes: &'a IndexSet) -> Self {
        Self {
            graph,
            indexes: Some(indexes),
            params: HashMap::new(),
        }
    }

    pub fn with_params(mut self, params: HashMap<String, Value>) -> Self {
        self.params = params;
        self
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
