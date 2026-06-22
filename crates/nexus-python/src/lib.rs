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
use nexus_core::types::{Direction, EdgeId, Value, VertexId};
use nexus_cypher::context::QueryContext;
use nexus_cypher::executor::execute;
use nexus_index::composite::IndexSet;
use nexus_index::vector::VectorIndex as CoreVectorIndex;
use nexus_server::engine::NexusEngine;
use nexus_storage::persistence::NexusStore;
use parking_lot::RwLock;
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};
use std::collections::HashMap;
use std::sync::Arc;

pyo3::create_exception!(domyn_nexus, NexusError, PyException);
pyo3::create_exception!(domyn_nexus, CypherError, NexusError);
pyo3::create_exception!(domyn_nexus, StorageError, NexusError);
pyo3::create_exception!(domyn_nexus, SchemaError, NexusError);
pyo3::create_exception!(domyn_nexus, VectorError, NexusError);

// ---------------------------------------------------------------------------
// Value conversion helpers
// ---------------------------------------------------------------------------

fn coded_message(code: &str, err: impl std::fmt::Display) -> String {
    format!("{code}: {err}")
}

fn py_storage_error(err: impl std::fmt::Display) -> PyErr {
    StorageError::new_err(coded_message("PY_STORAGE_ERROR", err))
}

fn py_cypher_error(err: impl std::fmt::Display) -> PyErr {
    CypherError::new_err(coded_message("PY_CYPHER_ERROR", err))
}

fn py_schema_error(err: impl std::fmt::Display) -> PyErr {
    SchemaError::new_err(coded_message("PY_SCHEMA_ERROR", err))
}

fn py_vector_error(err: impl std::fmt::Display) -> PyErr {
    VectorError::new_err(coded_message("PY_VECTOR_ERROR", err))
}

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
    if let Ok(v) = obj.downcast::<PyBytes>() {
        return Ok(Value::Bytes(v.as_bytes().to_vec()));
    }
    if let Ok(items) = obj.downcast::<PyList>() {
        let mut values = Vec::with_capacity(items.len());
        for item in items.iter() {
            values.push(py_to_value(&item)?);
        }
        return Ok(Value::List(values));
    }
    if let Ok(entries) = obj.downcast::<PyDict>() {
        let mut values = Vec::with_capacity(entries.len());
        for (key, value) in entries.iter() {
            values.push((key.extract::<String>()?, py_to_value(&value)?));
        }
        return Ok(Value::Map(values));
    }
    Err(py_schema_error(format!(
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

fn serde_json_to_py(py: Python<'_>, value: &serde_json::Value) -> PyResult<PyObject> {
    match value {
        serde_json::Value::Null => Ok(py.None()),
        serde_json::Value::Bool(value) => {
            Ok((*value).into_pyobject(py)?.to_owned().into_any().unbind())
        }
        serde_json::Value::Number(value) => {
            if let Some(int) = value.as_i64() {
                Ok(int.into_pyobject(py)?.into_any().unbind())
            } else if let Some(uint) = value.as_u64() {
                Ok(uint.into_pyobject(py)?.into_any().unbind())
            } else {
                Ok(value
                    .as_f64()
                    .unwrap_or_default()
                    .into_pyobject(py)?
                    .into_any()
                    .unbind())
            }
        }
        serde_json::Value::String(value) => Ok(value.into_pyobject(py)?.into_any().unbind()),
        serde_json::Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(serde_json_to_py(py, item)?)?;
            }
            Ok(list.into_any().unbind())
        }
        serde_json::Value::Object(entries) => {
            let dict = PyDict::new(py);
            for (key, value) in entries {
                dict.set_item(key, serde_json_to_py(py, value)?)?;
            }
            Ok(dict.into_any().unbind())
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
        "any" | "value" | "json" => Ok(PropertyType::Any),
        _ => Err(py_schema_error(format!(
            "unknown property type: '{s}'. Use: bool, int64, float64, string, bytes, any"
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
    engine: Option<Arc<NexusEngine>>,
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
            engine: None,
            built: false,
        }
    }

    /// Open a durable graph directory produced by NexusStore/NexusEngine.
    ///
    /// Cypher writes and direct mutation helpers on an opened graph use the
    /// same WAL-before-apply path as the server product.
    #[staticmethod]
    fn open(path: String) -> PyResult<Self> {
        let store = NexusStore::open(&path).map_err(py_storage_error)?;
        let graph = store.load_graph(0, 0).map_err(py_storage_error)?;
        let engine = Arc::new(NexusEngine::with_store(graph, store));
        let inner = Arc::clone(engine.graph());
        let indexes = Arc::clone(engine.indexes());

        Ok(Self {
            inner,
            indexes,
            engine: Some(engine),
            built: true,
        })
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
        if let Some(engine) = &self.engine {
            let vid = engine
                .execute_write(|wtx| Ok(wtx.add_vertex(label)))
                .map_err(py_storage_error)?;
            return Ok(vid.0);
        }

        let vid = self.inner.write().add_vertex(label);
        Ok(vid.0)
    }

    fn set_property(&self, vertex_id: u64, key: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let val = py_to_value(value)?;
        if let Some(engine) = &self.engine {
            engine
                .execute_write(|wtx| {
                    wtx.set_vertex_property(VertexId(vertex_id), key, val);
                    Ok(())
                })
                .map_err(py_storage_error)?;
            return Ok(());
        }

        {
            self.inner
                .write()
                .try_set_vertex_property(VertexId(vertex_id), key, val)
                .map_err(py_schema_error)?;
        }
        if self.built {
            self.refresh_indexes();
        }
        Ok(())
    }

    fn add_edge(&self, source: u64, target: u64, label: &str) -> PyResult<u64> {
        if let Some(engine) = &self.engine {
            let eid = engine
                .execute_write(|wtx| Ok(wtx.add_edge(VertexId(source), VertexId(target), label)))
                .map_err(py_storage_error)?;
            return Ok(eid.0);
        }

        let eid = self
            .inner
            .write()
            .add_edge(VertexId(source), VertexId(target), label);
        Ok(eid.0)
    }

    fn set_edge_property(&self, edge_id: u64, key: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let val = py_to_value(value)?;
        if let Some(engine) = &self.engine {
            engine
                .execute_write(|wtx| {
                    wtx.set_edge_property(EdgeId(edge_id), key, val);
                    Ok(())
                })
                .map_err(py_storage_error)?;
            return Ok(());
        }

        self.inner
            .write()
            .try_set_edge_property(EdgeId(edge_id), key, val)
            .map_err(py_schema_error)?;
        Ok(())
    }

    fn build(&mut self) -> PyResult<()> {
        if self.built {
            return Err(py_schema_error("graph already built"));
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
            return Err(py_schema_error("call build() before rebuild()"));
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
            return Err(py_schema_error("call graph.build() before running queries"));
        }
        let params = params
            .map(py_dict_to_params)
            .transpose()?
            .unwrap_or_default();

        if let Some(engine) = &self.engine {
            let result = engine
                .execute_cypher_with_params(query, params)
                .map_err(|e| py_cypher_error(format!("Cypher error: {e}")))?;
            return Ok(QueryResult {
                columns: result.columns,
                inner_rows: result.rows,
            });
        }

        let g = self.inner.read();
        let idx = self.indexes.read();
        let ctx = QueryContext::with_indexes(&g, &idx).with_params(params);
        let ast = nexus_cypher::parser::Parser::parse_read(query)
            .map_err(|e| py_cypher_error(format!("Parse error: {e}")))?;
        nexus_cypher::binder::bind_query(&ast)
            .map_err(|e| py_cypher_error(format!("Bind error: {e}")))?;
        let plan = nexus_cypher::planner::plan_query(&ast)
            .map_err(|e| py_cypher_error(format!("Plan error: {e}")))?;
        let result =
            execute(&plan, &ctx).map_err(|e| py_cypher_error(format!("Execution error: {e}")))?;
        Ok(QueryResult {
            columns: result.columns,
            inner_rows: result.rows,
        })
    }

    fn save_snapshot(&self) -> PyResult<()> {
        let Some(engine) = &self.engine else {
            return Err(py_storage_error(
                "save_snapshot() requires Graph.open(path); in-memory Graph has no durable store",
            ));
        };
        engine.save_snapshot().map_err(py_storage_error)
    }

    fn backup(&self, path: String) -> PyResult<PyObject> {
        let Some(engine) = &self.engine else {
            return Err(py_storage_error(
                "backup() requires Graph.open(path); in-memory Graph has no durable store",
            ));
        };
        let manifest = engine.backup_to(path).map_err(py_storage_error)?;
        Python::with_gil(|py| {
            let value = serde_json::to_value(&manifest).map_err(py_storage_error)?;
            serde_json_to_py(py, &value)
        })
    }

    #[pyo3(signature = (name, dimension, mode="hnsw"))]
    fn vector_index(&self, name: &str, dimension: usize, mode: &str) -> PyResult<NamedVectorIndex> {
        let Some(engine) = &self.engine else {
            return Err(py_vector_error(
                "Graph.vector_index(name, dim) requires Graph.open(path); use VectorIndex for in-memory standalone search",
            ));
        };
        if dimension == 0 {
            return Err(py_vector_error(
                "vector index dimension must be greater than zero",
            ));
        }
        match mode.to_ascii_lowercase().as_str() {
            "hnsw" | "exact" => {}
            other => {
                return Err(py_vector_error(format!(
                    "unsupported vector index mode: {other}; use 'hnsw' or 'exact'"
                )));
            }
        }

        let loaded = engine.load_vector_index(name).map_err(py_vector_error)?;
        if loaded {
            let existing = engine.vector_index_dimension(name).ok_or_else(|| {
                py_vector_error(format!("vector index loaded but not available: {name}"))
            })?;
            if existing != dimension {
                return Err(py_vector_error(format!(
                    "vector index '{name}' dimension mismatch: existing {existing}, requested {dimension}"
                )));
            }
        } else {
            engine
                .create_vector_index(name, dimension)
                .map_err(py_vector_error)?;
        }

        Ok(NamedVectorIndex {
            engine: Arc::clone(engine),
            name: name.to_string(),
            dimension,
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

    #[getter]
    fn is_persistent(&self) -> bool {
        self.engine.is_some()
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
// NamedVectorIndex — durable vector manager bound to Graph.open(path)
// ---------------------------------------------------------------------------

#[pyclass]
struct NamedVectorIndex {
    engine: Arc<NexusEngine>,
    name: String,
    dimension: usize,
}

#[pymethods]
impl NamedVectorIndex {
    fn upsert(&self, vertex_id: u64, embedding: Vec<f32>) -> PyResult<()> {
        self.check_dimension(&embedding, "embedding")?;
        self.engine
            .upsert_vector(&self.name, VertexId(vertex_id), embedding)
            .map_err(py_vector_error)
    }

    fn add(&self, vertex_id: u64, embedding: Vec<f32>) -> PyResult<()> {
        self.upsert(vertex_id, embedding)
    }

    fn update(&self, vertex_id: u64, embedding: Vec<f32>) -> PyResult<()> {
        self.upsert(vertex_id, embedding)
    }

    fn remove(&self, vertex_id: u64) -> PyResult<bool> {
        self.engine
            .remove_vector(&self.name, VertexId(vertex_id))
            .map_err(py_vector_error)
    }

    fn compact(&self) -> PyResult<()> {
        self.engine
            .compact_vector_index(&self.name)
            .map_err(py_vector_error)
    }

    fn search(&self, query: Vec<f32>, k: usize) -> PyResult<Vec<(u64, f32)>> {
        self.check_dimension(&query, "query")?;
        let results = self
            .engine
            .vector_search(&self.name, &query, k)
            .map_err(py_vector_error)?;
        Ok(results.into_iter().map(|(v, d)| (v.0, d)).collect())
    }

    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    #[getter]
    fn dimension(&self) -> usize {
        self.dimension
    }

    fn __repr__(&self) -> String {
        format!(
            "NamedVectorIndex(name={:?}, dimension={})",
            self.name, self.dimension
        )
    }
}

impl NamedVectorIndex {
    fn check_dimension(&self, embedding: &[f32], label: &str) -> PyResult<()> {
        if embedding.len() != self.dimension {
            return Err(py_vector_error(format!(
                "{label} dimension mismatch for vector index '{}': expected {}, got {}",
                self.name,
                self.dimension,
                embedding.len()
            )));
        }
        Ok(())
    }
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
            return Err(py_vector_error(format!(
                "embedding dimension mismatch: expected {}, got {}",
                self.inner.dimension(),
                embedding.len()
            )));
        }
        self.inner.add(VertexId(vertex_id), embedding);
        Ok(())
    }

    fn update(&mut self, vertex_id: u64, embedding: Vec<f32>) -> PyResult<()> {
        if embedding.len() != self.inner.dimension() {
            return Err(py_vector_error(format!(
                "embedding dimension mismatch: expected {}, got {}",
                self.inner.dimension(),
                embedding.len()
            )));
        }
        self.inner.update(VertexId(vertex_id), embedding);
        Ok(())
    }

    fn remove(&mut self, vertex_id: u64) -> bool {
        self.inner.remove(VertexId(vertex_id))
    }

    fn compact(&mut self) {
        self.inner.compact();
    }

    fn save(&self, path: String) -> PyResult<()> {
        self.inner.save_to_path(path).map_err(py_vector_error)
    }

    #[staticmethod]
    fn load(path: String) -> PyResult<Self> {
        let inner = CoreVectorIndex::load_from_path(path).map_err(py_vector_error)?;
        Ok(Self { inner })
    }

    fn search(&self, query: Vec<f32>, k: usize) -> PyResult<Vec<(u64, f32)>> {
        if query.len() != self.inner.dimension() {
            return Err(py_vector_error(format!(
                "query dimension mismatch: expected {}, got {}",
                self.inner.dimension(),
                query.len()
            )));
        }
        let results = self.inner.search(&query, k);
        Ok(results.into_iter().map(|(v, d)| (v.0, d)).collect())
    }

    fn search_exact(&self, query: Vec<f32>, k: usize) -> PyResult<Vec<(u64, f32)>> {
        if query.len() != self.inner.dimension() {
            return Err(py_vector_error(format!(
                "query dimension mismatch: expected {}, got {}",
                self.inner.dimension(),
                query.len()
            )));
        }
        let results = self.inner.search_exact(&query, k);
        Ok(results.into_iter().map(|(v, d)| (v.0, d)).collect())
    }

    fn search_within(&self, query: Vec<f32>, threshold: f32) -> PyResult<Vec<(u64, f32)>> {
        if query.len() != self.inner.dimension() {
            return Err(py_vector_error("query dimension mismatch"));
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

    #[getter]
    fn tombstones(&self) -> usize {
        self.inner.tombstone_count()
    }

    fn __repr__(&self) -> String {
        format!(
            "VectorIndex(dimension={}, entries={}, tombstones={})",
            self.inner.dimension(),
            self.inner.len(),
            self.inner.tombstone_count()
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
        .ok_or_else(|| py_schema_error(format!("edge label not found: '{edge_label}'")))?;
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
        .ok_or_else(|| py_schema_error(format!("edge label not found: '{edge_label}'")))?;
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
        .ok_or_else(|| py_schema_error(format!("edge label not found: '{edge_label}'")))?;
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
        .ok_or_else(|| py_schema_error(format!("edge label not found: '{edge_label}'")))?;
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
        .ok_or_else(|| py_schema_error(format!("edge label not found: '{edge_label}'")))?;
    Ok(nexus_algebra::spmv::bounded_traversal(
        matrix, start, max_depth, max_nodes,
    ))
}

// ---------------------------------------------------------------------------
// Module definition
// ---------------------------------------------------------------------------

#[pymodule]
fn domyn_nexus(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add_class::<Graph>()?;
    m.add_class::<VectorIndex>()?;
    m.add_class::<NamedVectorIndex>()?;
    m.add_class::<QueryResult>()?;
    m.add("NexusError", py.get_type::<NexusError>())?;
    m.add("CypherError", py.get_type::<CypherError>())?;
    m.add("StorageError", py.get_type::<StorageError>())?;
    m.add("SchemaError", py.get_type::<SchemaError>())?;
    m.add("VectorError", py.get_type::<VectorError>())?;
    m.add_function(wrap_pyfunction!(pagerank, m)?)?;
    m.add_function(wrap_pyfunction!(connected_components, m)?)?;
    m.add_function(wrap_pyfunction!(shortest_path, m)?)?;
    m.add_function(wrap_pyfunction!(bfs, m)?)?;
    m.add_function(wrap_pyfunction!(subgraph, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
