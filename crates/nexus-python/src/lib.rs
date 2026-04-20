//! Python SDK for Domyn Nexus via PyO3.
//!
//! Designed for direct embedding in GraphRAG pipelines — zero network overhead,
//! zero serialization between Python and the graph engine.
//!
//! Usage from Python:
//! ```python
//! import domyn_nexus as nx
//!
//! graph = nx.Graph(vertex_capacity=10000, edge_capacity=50000)
//! graph.register_vertex_property("name", "string", indexed=True)
//! graph.register_vertex_property("embedding", "bytes")
//!
//! v1 = graph.add_vertex("Entity")
//! graph.set_property(v1, "name", "Apple Inc.")
//! v2 = graph.add_vertex("Metric")
//! graph.add_edge(v1, v2, "DISCLOSES")
//! graph.build()
//!
//! result = graph.cypher("MATCH (n:Entity) RETURN n LIMIT 10")
//! for row in result.rows:
//!     print(row)
//!
//! # Vector search for RAG
//! vec_idx = nx.VectorIndex(dimension=384)
//! vec_idx.add(v1, embedding)
//! neighbors = vec_idx.search(query_embedding, k=5)
//!
//! # Graph algorithms
//! ranks = nx.pagerank(graph, "DISCLOSES", damping=0.85)
//! ```

use nexus_core::graph::Graph as CoreGraph;
use nexus_core::properties::PropertyType;
use nexus_core::types::{Direction, Value, VertexId};
use nexus_cypher::context::QueryContext;
use nexus_cypher::executor::execute;
use nexus_index::composite::IndexSet;
use nexus_index::vector::VectorIndex as CoreVectorIndex;
use parking_lot::RwLock;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Value conversion helpers
// ---------------------------------------------------------------------------

fn py_to_value(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(v) = obj.extract::<bool>() {
        return Ok(Value::Bool(v));
    }
    if let Ok(v) = obj.extract::<i64>() {
        return Ok(Value::Int64(v));
    }
    if let Ok(v) = obj.extract::<f64>() {
        return Ok(Value::Float64(v));
    }
    if let Ok(v) = obj.extract::<String>() {
        return Ok(Value::String(v));
    }
    if let Ok(v) = obj.extract::<Vec<u8>>() {
        return Ok(Value::Bytes(v));
    }
    Err(PyValueError::new_err(format!(
        "unsupported Python type for graph property: {}",
        obj.get_type().name()?
    )))
}

fn value_to_py(py: Python<'_>, v: &Value) -> PyObject {
    match v {
        Value::Null => py.None(),
        Value::Bool(b) => b.into_pyobject(py).unwrap().to_owned().into_any().unbind(),
        Value::Int64(i) => i.into_pyobject(py).unwrap().into_any().unbind(),
        Value::Float64(f) => f.into_pyobject(py).unwrap().into_any().unbind(),
        Value::String(s) => s.into_pyobject(py).unwrap().into_any().unbind(),
        Value::Bytes(b) => b.as_slice().into_pyobject(py).unwrap().into_any().unbind(),
        Value::List(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(value_to_py(py, item)).unwrap();
            }
            list.into_any().unbind()
        }
        Value::Map(entries) => {
            let dict = PyDict::new(py);
            for (k, v) in entries {
                dict.set_item(k, value_to_py(py, v)).unwrap();
            }
            dict.into_any().unbind()
        }
    }
}

fn str_to_property_type(s: &str) -> PyResult<PropertyType> {
    match s.to_lowercase().as_str() {
        "bool" | "boolean" => Ok(PropertyType::Bool),
        "int" | "int64" | "integer" => Ok(PropertyType::Int64),
        "float" | "float64" | "double" => Ok(PropertyType::Float64),
        "str" | "string" | "text" => Ok(PropertyType::String),
        "bytes" | "blob" | "binary" => Ok(PropertyType::Bytes),
        _ => Err(PyValueError::new_err(format!(
            "unknown property type: '{s}'. Use: bool, int64, float64, string, bytes"
        ))),
    }
}

// ---------------------------------------------------------------------------
// QueryResult
// ---------------------------------------------------------------------------

#[pyclass]
#[derive(Clone)]
struct QueryResult {
    #[pyo3(get)]
    columns: Vec<String>,
    inner_rows: Vec<Vec<Value>>,
}

#[pymethods]
impl QueryResult {
    #[getter]
    fn rows(&self, py: Python<'_>) -> PyResult<PyObject> {
        let outer = PyList::empty(py);
        for row in &self.inner_rows {
            let inner = PyList::empty(py);
            for val in row {
                inner.append(value_to_py(py, val))?;
            }
            outer.append(inner)?;
        }
        Ok(outer.into_any().unbind())
    }

    #[getter]
    fn num_rows(&self) -> usize {
        self.inner_rows.len()
    }

    fn to_dicts(&self, py: Python<'_>) -> PyResult<PyObject> {
        let outer = PyList::empty(py);
        for row in &self.inner_rows {
            let dict = PyDict::new(py);
            for (col, val) in self.columns.iter().zip(row.iter()) {
                dict.set_item(col, value_to_py(py, val))?;
            }
            outer.append(dict)?;
        }
        Ok(outer.into_any().unbind())
    }

    fn __repr__(&self) -> String {
        format!(
            "QueryResult(columns={:?}, rows={})",
            self.columns,
            self.inner_rows.len()
        )
    }

    fn __len__(&self) -> usize {
        self.inner_rows.len()
    }
}

// ---------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------

#[pyclass]
struct Graph {
    inner: Arc<RwLock<CoreGraph>>,
    indexes: Arc<RwLock<IndexSet>>,
    built: bool,
}

#[pymethods]
impl Graph {
    #[new]
    #[pyo3(signature = (vertex_capacity=1024, edge_capacity=4096))]
    fn new(vertex_capacity: usize, edge_capacity: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(CoreGraph::new(vertex_capacity, edge_capacity))),
            indexes: Arc::new(RwLock::new(IndexSet::new())),
            built: false,
        }
    }

    #[pyo3(signature = (name, property_type, indexed=false, unique=false))]
    fn register_vertex_property(
        &self,
        name: &str,
        property_type: &str,
        indexed: bool,
        unique: bool,
    ) -> PyResult<()> {
        let pt = str_to_property_type(property_type)?;
        self.inner
            .write()
            .register_vertex_property(name, pt, indexed, unique);
        Ok(())
    }

    #[pyo3(signature = (name, property_type, indexed=false, unique=false))]
    fn register_edge_property(
        &self,
        name: &str,
        property_type: &str,
        indexed: bool,
        unique: bool,
    ) -> PyResult<()> {
        let pt = str_to_property_type(property_type)?;
        self.inner
            .write()
            .register_edge_property(name, pt, indexed, unique);
        Ok(())
    }

    fn add_vertex(&self, label: &str) -> PyResult<u64> {
        let vid = self.inner.write().add_vertex(label);
        Ok(vid.0)
    }

    fn set_property(&self, vertex_id: u64, key: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let val = py_to_value(value)?;
        {
            self.inner
                .write()
                .try_set_vertex_property(VertexId(vertex_id), key, val)
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
        }
        if self.built {
            self.refresh_indexes();
        }
        Ok(())
    }

    fn add_edge(&self, source: u64, target: u64, label: &str) -> PyResult<u64> {
        let eid = self
            .inner
            .write()
            .add_edge(VertexId(source), VertexId(target), label);
        Ok(eid.0)
    }

    fn set_edge_property(&self, edge_id: u64, key: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let val = py_to_value(value)?;
        self.inner
            .write()
            .try_set_edge_property(nexus_core::types::EdgeId(edge_id), key, val)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(())
    }

    fn build(&mut self) -> PyResult<()> {
        if self.built {
            return Err(PyRuntimeError::new_err("graph already built"));
        }
        let mut g = self.inner.write();
        g.build();

        let mut indexes = self.indexes.write();
        Self::populate_indexes(&g, &mut indexes);

        self.built = true;
        Ok(())
    }

    fn rebuild(&self) -> PyResult<()> {
        if !self.built {
            return Err(PyRuntimeError::new_err("call build() before rebuild()"));
        }
        let mut g = self.inner.write();
        g.rebuild();

        let mut indexes = self.indexes.write();
        *indexes = IndexSet::new();
        Self::populate_indexes(&g, &mut indexes);
        Ok(())
    }

    #[pyo3(signature = (query, params=None))]
    fn cypher(&self, query: &str, params: Option<&Bound<'_, PyDict>>) -> PyResult<QueryResult> {
        if !self.built {
            return Err(PyRuntimeError::new_err(
                "call graph.build() before running queries",
            ));
        }
        let params = params
            .map(py_dict_to_params)
            .transpose()?
            .unwrap_or_default();
        let g = self.inner.read();
        let idx = self.indexes.read();
        let ctx = QueryContext::with_indexes(&g, &idx).with_params(params);
        let ast = nexus_cypher::parser::Parser::parse_read(query)
            .map_err(|e| PyRuntimeError::new_err(format!("Parse error: {e}")))?;
        nexus_cypher::binder::bind_query(&ast)
            .map_err(|e| PyRuntimeError::new_err(format!("Bind error: {e}")))?;
        let plan = nexus_cypher::planner::plan_query(&ast)
            .map_err(|e| PyRuntimeError::new_err(format!("Plan error: {e}")))?;
        let result = execute(&plan, &ctx)
            .map_err(|e| PyRuntimeError::new_err(format!("Execution error: {e}")))?;
        Ok(QueryResult {
            columns: result.columns,
            inner_rows: result.rows,
        })
    }

    fn get_property(&self, vertex_id: u64, key: &str, py: Python<'_>) -> PyResult<PyObject> {
        let g = self.inner.read();
        let val = g.get_vertex_property(VertexId(vertex_id), key);
        Ok(value_to_py(py, &val))
    }

    fn get_all_properties(&self, vertex_id: u64, py: Python<'_>) -> PyResult<PyObject> {
        let g = self.inner.read();
        let props = g.get_vertex_properties(VertexId(vertex_id));
        let dict = PyDict::new(py);
        for (k, v) in &props {
            dict.set_item(k, value_to_py(py, v))?;
        }
        Ok(dict.into_any().unbind())
    }

    fn vertex_label(&self, vertex_id: u64) -> PyResult<Option<String>> {
        let g = self.inner.read();
        Ok(g.vertex_label(VertexId(vertex_id)).map(String::from))
    }

    fn neighbors(&self, vertex_id: u64, edge_label: &str, direction: &str) -> PyResult<Vec<u64>> {
        let dir = match direction.to_lowercase().as_str() {
            "out" | "outgoing" => Direction::Outgoing,
            "in" | "incoming" => Direction::Incoming,
            "both" => Direction::Both,
            _ => {
                return Err(PyValueError::new_err(
                    "direction must be 'out', 'in', or 'both'",
                ));
            }
        };
        let g = self.inner.read();
        let ids = g.neighbors(VertexId(vertex_id), edge_label, dir);
        Ok(ids.into_iter().map(|v| v.0).collect())
    }

    #[getter]
    fn num_vertices(&self) -> usize {
        self.inner.read().num_vertices()
    }

    #[getter]
    fn num_edges(&self) -> u64 {
        self.inner.read().num_edges()
    }

    #[getter]
    fn vertex_labels(&self) -> Vec<String> {
        self.inner.read().vertex_label_names()
    }

    #[getter]
    fn edge_labels(&self) -> Vec<String> {
        self.inner.read().edge_label_names()
    }

    #[getter]
    fn is_built(&self) -> bool {
        self.built
    }

    fn __repr__(&self) -> String {
        let g = self.inner.read();
        format!(
            "Graph(vertices={}, edges={}, built={})",
            g.num_vertices(),
            g.num_edges(),
            self.built
        )
    }
}

impl Graph {
    fn refresh_indexes(&self) {
        let g = self.inner.read();
        let mut indexes = self.indexes.write();
        *indexes = IndexSet::new();
        Self::populate_indexes(&g, &mut indexes);
    }

    fn populate_indexes(graph: &CoreGraph, indexes: &mut IndexSet) {
        struct Slot {
            prop_name: String,
            unique_slot: Option<usize>,
            composite_slot: Option<usize>,
        }

        let indexed_props: Vec<(String, bool, bool)> = graph
            .vertex_property_defs()
            .filter(|def| def.indexed || def.unique)
            .map(|def| (def.name.clone(), def.indexed, def.unique))
            .collect();

        let mut slots: Vec<Slot> = Vec::new();
        for (name, indexed, unique) in &indexed_props {
            let mut slot = Slot {
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
    }
}

fn py_dict_to_params(dict: &Bound<'_, PyDict>) -> PyResult<HashMap<String, Value>> {
    let mut params = HashMap::new();
    for (key, value) in dict.iter() {
        let key = key.extract::<String>()?;
        params.insert(key, py_to_value(&value)?);
    }
    Ok(params)
}

// ---------------------------------------------------------------------------
// VectorIndex — for RAG embedding search
// ---------------------------------------------------------------------------

#[pyclass]
struct VectorIndex {
    inner: CoreVectorIndex,
}

#[pymethods]
impl VectorIndex {
    #[new]
    fn new(dimension: usize) -> Self {
        Self {
            inner: CoreVectorIndex::new(dimension),
        }
    }

    fn add(&mut self, vertex_id: u64, embedding: Vec<f32>) -> PyResult<()> {
        if embedding.len() != self.inner.dimension() {
            return Err(PyValueError::new_err(format!(
                "embedding dimension mismatch: expected {}, got {}",
                self.inner.dimension(),
                embedding.len()
            )));
        }
        self.inner.add(VertexId(vertex_id), embedding);
        Ok(())
    }

    fn search(&self, query: Vec<f32>, k: usize) -> PyResult<Vec<(u64, f32)>> {
        if query.len() != self.inner.dimension() {
            return Err(PyValueError::new_err(format!(
                "query dimension mismatch: expected {}, got {}",
                self.inner.dimension(),
                query.len()
            )));
        }
        let results = self.inner.search(&query, k);
        Ok(results.into_iter().map(|(v, d)| (v.0, d)).collect())
    }

    fn search_within(&self, query: Vec<f32>, threshold: f32) -> PyResult<Vec<(u64, f32)>> {
        if query.len() != self.inner.dimension() {
            return Err(PyValueError::new_err("query dimension mismatch"));
        }
        let results = self.inner.search_within(&query, threshold);
        Ok(results.into_iter().map(|(v, d)| (v.0, d)).collect())
    }

    #[getter]
    fn dimension(&self) -> usize {
        self.inner.dimension()
    }

    #[getter]
    fn size(&self) -> usize {
        self.inner.len()
    }

    fn __repr__(&self) -> String {
        format!(
            "VectorIndex(dimension={}, entries={})",
            self.inner.dimension(),
            self.inner.len()
        )
    }

    fn __len__(&self) -> usize {
        self.inner.len()
    }
}

// ---------------------------------------------------------------------------
// Graph algorithm wrappers
// ---------------------------------------------------------------------------

/// Run PageRank on the forward adjacency matrix for a given edge label.
#[pyfunction]
#[pyo3(signature = (graph, edge_label, damping=0.85, max_iterations=100, tolerance=1e-6))]
fn pagerank(
    graph: &Graph,
    edge_label: &str,
    damping: f64,
    max_iterations: usize,
    tolerance: f64,
) -> PyResult<Vec<(u64, f64)>> {
    let g = graph.inner.read();
    let matrix = g
        .forward_matrix(edge_label)
        .ok_or_else(|| PyValueError::new_err(format!("edge label not found: '{edge_label}'")))?;
    let result = nexus_algorithms::pagerank::pagerank(matrix, damping, max_iterations, tolerance);
    Ok(result.ranks)
}

/// Find connected components in the graph for a given edge label.
#[pyfunction]
#[pyo3(signature = (graph, edge_label, max_iterations=100))]
fn connected_components(
    graph: &Graph,
    edge_label: &str,
    max_iterations: usize,
) -> PyResult<Vec<(u64, u64)>> {
    let g = graph.inner.read();
    let matrix = g
        .forward_matrix(edge_label)
        .ok_or_else(|| PyValueError::new_err(format!("edge label not found: '{edge_label}'")))?;
    let result =
        nexus_algorithms::connected_components::connected_components(matrix, max_iterations);
    Ok(result.assignments)
}

/// Find shortest paths from a source vertex (unweighted, Dijkstra).
#[pyfunction]
fn shortest_path(graph: &Graph, edge_label: &str, source: u64) -> PyResult<Vec<(u64, f64)>> {
    let g = graph.inner.read();
    let matrix = g
        .forward_matrix(edge_label)
        .ok_or_else(|| PyValueError::new_err(format!("edge label not found: '{edge_label}'")))?;
    let result = nexus_algorithms::shortest_path::shortest_path_unweighted(matrix, source);
    Ok(result.distances)
}

/// BFS from a start vertex, returning (vertex_id, depth) pairs.
#[pyfunction]
#[pyo3(signature = (graph, edge_label, start, max_depth=10))]
fn bfs(graph: &Graph, edge_label: &str, start: u64, max_depth: u32) -> PyResult<Vec<(u64, u32)>> {
    let g = graph.inner.read();
    let matrix = g
        .forward_matrix(edge_label)
        .ok_or_else(|| PyValueError::new_err(format!("edge label not found: '{edge_label}'")))?;
    let result = nexus_algebra::spmv::bfs(matrix, start, max_depth);
    Ok(result.iter().map(|(&v, &d)| (v, d)).collect())
}

/// Bounded subgraph extraction from a start vertex.
#[pyfunction]
#[pyo3(signature = (graph, edge_label, start, max_depth=2, max_nodes=100))]
fn subgraph(
    graph: &Graph,
    edge_label: &str,
    start: u64,
    max_depth: u32,
    max_nodes: usize,
) -> PyResult<Vec<u64>> {
    let g = graph.inner.read();
    let matrix = g
        .forward_matrix(edge_label)
        .ok_or_else(|| PyValueError::new_err(format!("edge label not found: '{edge_label}'")))?;
    Ok(nexus_algebra::spmv::bounded_traversal(
        matrix, start, max_depth, max_nodes,
    ))
}

// ---------------------------------------------------------------------------
// Module definition
// ---------------------------------------------------------------------------

#[pymodule]
fn domyn_nexus(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Graph>()?;
    m.add_class::<VectorIndex>()?;
    m.add_class::<QueryResult>()?;
    m.add_function(wrap_pyfunction!(pagerank, m)?)?;
    m.add_function(wrap_pyfunction!(connected_components, m)?)?;
    m.add_function(wrap_pyfunction!(shortest_path, m)?)?;
    m.add_function(wrap_pyfunction!(bfs, m)?)?;
    m.add_function(wrap_pyfunction!(subgraph, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
