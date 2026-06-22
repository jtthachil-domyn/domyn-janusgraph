//! The core `Graph` struct that ties together CSR adjacency and columnar properties.

use crate::csr::{AdjacencyError, AdjacencyStore};
use crate::properties::{PropertyError, PropertyStore, PropertyType};
use crate::types::*;
use std::collections::HashMap;
use std::mem::size_of;

/// Label dictionary: maps label names to IDs and back.
#[derive(Clone)]
pub struct LabelDictionary {
    name_to_id: HashMap<String, LabelId>,
    id_to_name: Vec<String>,
}

impl LabelDictionary {
    pub fn new() -> Self {
        Self {
            name_to_id: HashMap::new(),
            id_to_name: Vec::new(),
        }
    }

    pub fn get_or_create(&mut self, name: &str) -> LabelId {
        if let Some(&id) = self.name_to_id.get(name) {
            return id;
        }
        let id = LabelId(self.id_to_name.len() as u16);
        self.name_to_id.insert(name.to_string(), id);
        self.id_to_name.push(name.to_string());
        id
    }

    pub fn get(&self, name: &str) -> Option<LabelId> {
        self.name_to_id.get(name).copied()
    }

    pub fn name(&self, id: LabelId) -> Option<&str> {
        self.id_to_name.get(id.0 as usize).map(|s| s.as_str())
    }

    pub fn len(&self) -> usize {
        self.id_to_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_name.is_empty()
    }

    pub fn estimated_heap_bytes(&self) -> usize {
        self.name_to_id.capacity() * size_of::<(String, LabelId)>()
            + self.name_to_id.keys().map(String::capacity).sum::<usize>()
            + self.id_to_name.capacity() * size_of::<String>()
            + self.id_to_name.iter().map(String::capacity).sum::<usize>()
    }
}

impl Default for LabelDictionary {
    fn default() -> Self {
        Self::new()
    }
}

/// The core graph: CSR adjacency + columnar properties + label dictionaries.
///
/// Before `build()`, edges accumulate in CsrBuilders. After `build()`, the
/// CSR matrices are finalized for fast reads, but mutations remain possible:
/// new edges go into a delta layer, and `rebuild()` merges them into fresh
/// CSR matrices.
#[derive(Clone)]
pub struct Graph {
    vertex_labels: LabelDictionary,
    edge_labels: LabelDictionary,
    vertex_label_assignments: Vec<LabelId>,
    adjacency: AdjacencyStore,
    vertex_properties: PropertyStore,
    edge_properties: PropertyStore,
    num_vertices: usize,
    built: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GraphCompactionStats {
    pub deleted_vertices_cleared: usize,
    pub deleted_edges_cleared: usize,
    pub tombstoned_edge_meta_removed: usize,
    pub delta_edges_compacted: usize,
    pub live_edges_after: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GraphCompactionPressure {
    pub deleted_vertices: usize,
    pub tombstoned_edges: usize,
    pub delta_edges: usize,
}

impl GraphCompactionPressure {
    pub fn total(&self) -> usize {
        self.deleted_vertices + self.tombstoned_edges + self.delta_edges
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

impl Graph {
    /// Create a new graph with pre-allocated capacity.
    pub fn new(vertex_capacity: usize, edge_capacity: usize) -> Self {
        Self {
            vertex_labels: LabelDictionary::new(),
            edge_labels: LabelDictionary::new(),
            vertex_label_assignments: Vec::with_capacity(vertex_capacity),
            adjacency: AdjacencyStore::new(vertex_capacity),
            vertex_properties: PropertyStore::new(vertex_capacity),
            edge_properties: PropertyStore::new(edge_capacity),
            num_vertices: 0,
            built: false,
        }
    }

    /// Register a vertex property key in the schema.
    pub fn register_vertex_property(
        &mut self,
        name: &str,
        property_type: PropertyType,
        indexed: bool,
        unique: bool,
    ) -> PropertyKeyId {
        self.vertex_properties
            .register_property(name, property_type, indexed, unique)
    }

    /// Register an edge property key in the schema.
    pub fn register_edge_property(
        &mut self,
        name: &str,
        property_type: PropertyType,
        indexed: bool,
        unique: bool,
    ) -> PropertyKeyId {
        self.edge_properties
            .register_property(name, property_type, indexed, unique)
    }

    /// Add a vertex with a label. Returns its ID.
    ///
    /// Works both before and after `build()`. Post-build vertices are
    /// immediately available in the property store; they join the CSR
    /// adjacency structure on `rebuild()`.
    pub fn add_vertex(&mut self, label: &str) -> VertexId {
        self.try_add_vertex(label)
            .expect("adding a vertex should not fail")
    }

    pub fn try_add_vertex(&mut self, label: &str) -> Result<VertexId, GraphError> {
        let label_id = self.vertex_labels.get_or_create(label);
        let row = self.vertex_properties.allocate_row();
        self.vertex_label_assignments.push(label_id);
        self.num_vertices += 1;
        self.adjacency.set_num_vertices(self.num_vertices);
        Ok(VertexId(row as u64))
    }

    /// Add a vertex at a specific ID slot (used during WAL replay).
    /// Pads with empty rows up to `id` if the graph has fewer vertices.
    pub fn add_vertex_with_id(&mut self, id: u64, label: &str) -> VertexId {
        self.try_add_vertex_with_id(id, label)
            .expect("adding a vertex with an explicit ID should not fail")
    }

    pub fn try_add_vertex_with_id(&mut self, id: u64, label: &str) -> Result<VertexId, GraphError> {
        let label_id = self.vertex_labels.get_or_create(label);
        let target = id as usize;

        while self.num_vertices <= target {
            self.vertex_properties.allocate_row();
            self.vertex_label_assignments.push(LabelId(u16::MAX));
            self.num_vertices += 1;
        }
        self.vertex_label_assignments[target] = label_id;
        self.adjacency.set_num_vertices(self.num_vertices);
        Ok(VertexId(id))
    }

    /// Set a vertex property.
    pub fn set_vertex_property(&mut self, vertex: VertexId, key: &str, value: Value) {
        let _ = self.try_set_vertex_property(vertex, key, value);
    }

    pub fn try_set_vertex_property(
        &mut self,
        vertex: VertexId,
        key: &str,
        value: Value,
    ) -> Result<(), GraphError> {
        if !self.vertex_exists(vertex) {
            return Err(GraphError::VertexOutOfRange {
                vertex,
                vertices: self.num_vertices,
            });
        }
        if self.vertex_properties.property_id(key).is_none() {
            if matches!(value, Value::Null) {
                return Err(GraphError::Property(PropertyError::UnknownProperty(
                    key.to_string(),
                )));
            }
            self.vertex_properties
                .register_property(key, PropertyType::Any, false, false);
        }
        self.vertex_properties
            .try_set_by_name(vertex.0 as usize, key, value)?;
        Ok(())
    }

    /// Add an edge between two vertices with a label. Returns the edge ID.
    ///
    /// Works both before and after `build()`. Post-build edges go into the
    /// adjacency delta layer and are queryable immediately via `neighbors()`.
    pub fn add_edge(&mut self, source: VertexId, target: VertexId, label: &str) -> EdgeId {
        self.try_add_edge(source, target, label)
            .expect("edge endpoint IDs must reference existing vertices")
    }

    pub fn try_add_edge(
        &mut self,
        source: VertexId,
        target: VertexId,
        label: &str,
    ) -> Result<EdgeId, GraphError> {
        self.validate_vertex(source)?;
        self.validate_vertex(target)?;
        let label_id = self.edge_labels.get_or_create(label);
        let eid = self.adjacency.try_add_edge(source, target, label_id)?;
        self.edge_properties.allocate_row();
        Ok(eid)
    }

    /// Add an edge with a specific edge ID (used during WAL replay).
    /// Pads edge property rows up to `edge_id` if needed.
    pub fn add_edge_with_id(
        &mut self,
        edge_id: u64,
        source: VertexId,
        target: VertexId,
        label: &str,
    ) -> EdgeId {
        self.try_add_edge_with_id(edge_id, source, target, label)
            .expect("edge endpoint IDs must reference existing vertices")
    }

    pub fn try_add_edge_with_id(
        &mut self,
        edge_id: u64,
        source: VertexId,
        target: VertexId,
        label: &str,
    ) -> Result<EdgeId, GraphError> {
        self.validate_vertex(source)?;
        self.validate_vertex(target)?;
        let label_id = self.edge_labels.get_or_create(label);
        let target_row = edge_id as usize;

        while self.edge_properties.count() <= target_row {
            self.edge_properties.allocate_row();
        }

        Ok(self
            .adjacency
            .try_add_edge_with_id(edge_id, source, target, label_id)?)
    }

    pub fn remove_edge(&mut self, edge: EdgeId) {
        let _ = self.try_remove_edge(edge);
    }

    pub fn try_remove_edge(&mut self, edge: EdgeId) -> Result<(), GraphError> {
        self.adjacency.remove_edge(edge)?;
        Ok(())
    }

    pub fn remove_vertex(&mut self, vertex: VertexId) {
        let _ = self.try_remove_vertex(vertex);
    }

    pub fn try_remove_vertex(&mut self, vertex: VertexId) -> Result<(), GraphError> {
        self.validate_vertex(vertex)?;

        let incident_edges = self.adjacency.incident_edges(vertex);

        for edge in incident_edges {
            self.adjacency.remove_edge(edge)?;
        }

        self.vertex_label_assignments[vertex.0 as usize] = LabelId(u16::MAX);
        Ok(())
    }

    /// Set an edge property.
    pub fn set_edge_property(&mut self, edge: EdgeId, key: &str, value: Value) {
        let _ = self.try_set_edge_property(edge, key, value);
    }

    pub fn try_set_edge_property(
        &mut self,
        edge: EdgeId,
        key: &str,
        value: Value,
    ) -> Result<(), GraphError> {
        if !self.adjacency.edge_exists(edge) {
            return Err(GraphError::EdgeOutOfRange {
                edge,
                edges: self.adjacency.num_edges(),
            });
        }
        if self.edge_properties.property_id(key).is_none() {
            if matches!(value, Value::Null) {
                return Err(GraphError::Property(PropertyError::UnknownProperty(
                    key.to_string(),
                )));
            }
            self.edge_properties
                .register_property(key, PropertyType::Any, false, false);
        }
        self.edge_properties
            .try_set_by_name(edge.0 as usize, key, value)?;
        Ok(())
    }

    /// Finalize the graph: builds all CSR matrices.
    pub fn build(&mut self) {
        self.adjacency.build();
        self.built = true;
    }

    /// Merge delta edges into fresh CSR matrices with the current vertex count.
    ///
    /// Call periodically after batches of mutations to keep the CSR read path
    /// optimal. Queries work correctly without calling this -- the delta layer
    /// is always consulted -- but a rebuild eliminates the linear scan cost of
    /// the delta.
    pub fn rebuild(&mut self) {
        assert!(self.built, "rebuild() requires a prior build()");
        self.adjacency.rebuild();
    }

    pub fn try_rebuild(&mut self) -> Result<(), GraphError> {
        if !self.built {
            return Err(GraphError::NotBuilt);
        }
        self.adjacency.rebuild();
        Ok(())
    }

    pub fn compaction_pressure(&self) -> GraphCompactionPressure {
        GraphCompactionPressure {
            deleted_vertices: self
                .vertex_label_assignments
                .iter()
                .enumerate()
                .filter(|(row, label)| {
                    label.0 == u16::MAX && self.vertex_properties.row_has_values(*row)
                })
                .count(),
            tombstoned_edges: self.adjacency.tombstoned_edge_count(),
            delta_edges: self.adjacency.delta_edge_count(),
        }
    }

    /// Compact tombstones without remapping stable vertex or edge IDs.
    ///
    /// Vertex and edge IDs are row IDs and are referenced by WAL entries, query
    /// results, and external callers. This compaction therefore clears deleted
    /// property payloads and rebuilds adjacency/CSR metadata, but intentionally
    /// keeps row slots allocated.
    pub fn compact_tombstones(&mut self) -> Result<GraphCompactionStats, GraphError> {
        if !self.built {
            return Err(GraphError::NotBuilt);
        }

        let pressure = self.compaction_pressure();
        let mut stats = GraphCompactionStats {
            tombstoned_edge_meta_removed: pressure.tombstoned_edges,
            delta_edges_compacted: pressure.delta_edges,
            ..GraphCompactionStats::default()
        };

        for row in 0..self.vertex_label_assignments.len() {
            if self.vertex_label_assignments[row].0 == u16::MAX {
                self.vertex_properties.clear_row(row)?;
                stats.deleted_vertices_cleared += 1;
            }
        }

        for row in 0..self.edge_properties.count() {
            if !self.adjacency.edge_exists(EdgeId(row as u64)) {
                self.edge_properties.clear_row(row)?;
                stats.deleted_edges_cleared += 1;
            }
        }

        self.adjacency.rebuild();
        stats.live_edges_after = self.adjacency.live_edge_count();
        Ok(stats)
    }

    /// Approximate heap memory owned by the graph core: label dictionaries,
    /// vertex-label assignments, adjacency structures, and property stores.
    ///
    /// This intentionally does not try to account for allocator fragmentation,
    /// temporary query rows, index memory, or runtime/server overhead. It is an
    /// operational trend metric, not an exact process-memory accounting tool.
    pub fn estimated_heap_bytes(&self) -> usize {
        self.vertex_labels.estimated_heap_bytes()
            + self.edge_labels.estimated_heap_bytes()
            + self.vertex_label_assignments.capacity() * size_of::<LabelId>()
            + self.adjacency.estimated_heap_bytes()
            + self.vertex_properties.estimated_heap_bytes()
            + self.edge_properties.estimated_heap_bytes()
    }

    // --- Read operations (available after build) ---

    pub fn num_vertices(&self) -> usize {
        self.num_vertices
    }

    pub fn num_edges(&self) -> u64 {
        self.adjacency.num_edges()
    }

    pub fn is_built(&self) -> bool {
        self.built
    }

    /// Get the label of a vertex.
    pub fn vertex_label(&self, vertex: VertexId) -> Option<&str> {
        self.vertex_label_assignments
            .get(vertex.0 as usize)
            .and_then(|&lid| self.vertex_labels.name(lid))
    }

    /// Replace the label of an existing vertex with a new colon-joined label
    /// string. Used by `SET n:Label` / `REMOVE n:Label` after the executor
    /// computes the new label set.
    pub fn try_set_vertex_label(
        &mut self,
        vertex: VertexId,
        label: &str,
    ) -> Result<(), GraphError> {
        self.validate_vertex(vertex)?;
        let label_id = self.vertex_labels.get_or_create(label);
        self.vertex_label_assignments[vertex.0 as usize] = label_id;
        Ok(())
    }

    /// Get a vertex property.
    pub fn get_vertex_property(&self, vertex: VertexId, key: &str) -> Value {
        self.vertex_properties.get_by_name(vertex.0 as usize, key)
    }

    /// Get all properties of a vertex.
    pub fn get_vertex_properties(&self, vertex: VertexId) -> Vec<(String, Value)> {
        self.vertex_properties.get_all(vertex.0 as usize)
    }

    /// Get all properties of an edge.
    pub fn get_edge_properties(&self, edge: EdgeId) -> Vec<(String, Value)> {
        self.edge_properties.get_all(edge.0 as usize)
    }

    pub fn get_edge_property(&self, edge: EdgeId, key: &str) -> Value {
        self.edge_properties.get_by_name(edge.0 as usize, key)
    }

    /// Resolve a full `Vertex` struct.
    pub fn resolve_vertex(&self, id: VertexId) -> Option<Vertex> {
        let label = self.vertex_label(id)?.to_string();
        let properties = self.get_vertex_properties(id);
        Some(Vertex {
            id,
            label,
            properties,
        })
    }

    /// Get neighbors via the adjacency store.
    pub fn neighbors(&self, vertex: VertexId, label: &str, direction: Direction) -> Vec<VertexId> {
        if let Some(label_id) = self.edge_labels.get(label) {
            self.adjacency
                .neighbors(vertex, label_id, direction)
                .into_iter()
                .filter(|&neighbor| self.vertex_exists(neighbor))
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn neighbors_with_edges(
        &self,
        vertex: VertexId,
        label: &str,
        direction: Direction,
    ) -> Vec<(VertexId, EdgeId)> {
        if let Some(label_id) = self.edge_labels.get(label) {
            self.adjacency
                .neighbors_with_edges(vertex, label_id, direction)
                .into_iter()
                .filter(|&(neighbor, _)| self.vertex_exists(neighbor))
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn neighbors_with_edges_any_label(
        &self,
        vertex: VertexId,
        direction: Direction,
    ) -> Vec<(VertexId, EdgeId)> {
        self.adjacency
            .neighbors_with_edges_any_label(vertex, direction)
            .into_iter()
            .filter(|&(neighbor, _)| self.vertex_exists(neighbor))
            .collect()
    }

    pub fn incident_edges(&self, vertex: VertexId) -> Vec<EdgeId> {
        self.adjacency.incident_edges(vertex)
    }

    pub fn degree(&self, vertex: VertexId, label: &str, direction: Direction) -> usize {
        if !self.vertex_exists(vertex) {
            return 0;
        }
        self.edge_labels
            .get(label)
            .map(|label_id| self.adjacency.degree(vertex, label_id, direction))
            .unwrap_or(0)
    }

    pub fn incident_degree(&self, vertex: VertexId, direction: Direction) -> usize {
        if !self.vertex_exists(vertex) {
            return 0;
        }
        self.adjacency.incident_degree(vertex, direction)
    }

    pub fn edge_between(&self, source: VertexId, target: VertexId, label: &str) -> Option<EdgeId> {
        let label_id = self.edge_labels.get(label)?;
        self.adjacency
            .edges_for_label(label_id)
            .into_iter()
            .find_map(|(edge, src, dst)| {
                if src == source && dst == target {
                    Some(edge)
                } else {
                    None
                }
            })
    }

    /// Access the forward CSR matrix for a given edge label (for SpMV in nexus-algebra).
    pub fn forward_matrix(&self, label: &str) -> Option<&crate::csr::CsrMatrix> {
        self.edge_labels
            .get(label)
            .and_then(|lid| self.adjacency.forward_matrix(lid))
    }

    /// Access the backward CSR matrix for a given edge label.
    pub fn backward_matrix(&self, label: &str) -> Option<&crate::csr::CsrMatrix> {
        self.edge_labels
            .get(label)
            .and_then(|lid| self.adjacency.backward_matrix(lid))
    }

    /// All registered edge labels.
    pub fn edge_label_names(&self) -> Vec<String> {
        (0..self.edge_labels.len())
            .filter_map(|i| self.edge_labels.name(LabelId(i as u16)).map(String::from))
            .collect()
    }

    pub fn edge_records(&self) -> Vec<Edge> {
        let mut edges = Vec::new();
        for label_id in self.adjacency.labels() {
            let Some(label) = self.edge_labels.name(label_id) else {
                continue;
            };
            for (id, source, target) in self.adjacency.edges_for_label(label_id) {
                edges.push(Edge {
                    id,
                    source,
                    target,
                    label: label.to_string(),
                    properties: self.get_edge_properties(id),
                });
            }
        }
        edges
    }

    pub fn edge_exists(&self, edge: EdgeId) -> bool {
        self.adjacency.edge_exists(edge)
    }

    pub fn edge_meta(&self, edge: EdgeId) -> Option<crate::csr::EdgeMeta> {
        self.adjacency.edge_meta(edge)
    }

    pub fn edge_label(&self, edge: EdgeId) -> Option<&str> {
        let meta = self.adjacency.edge_meta(edge)?;
        self.edge_labels.name(meta.label)
    }

    /// Iterate registered vertex property definitions (name, indexed, unique).
    pub fn vertex_property_defs(&self) -> impl Iterator<Item = &crate::properties::PropertyKeyDef> {
        self.vertex_properties.key_defs()
    }

    pub fn edge_property_defs(&self) -> impl Iterator<Item = &crate::properties::PropertyKeyDef> {
        self.edge_properties.key_defs()
    }

    /// All registered vertex labels.
    pub fn vertex_label_names(&self) -> Vec<String> {
        (0..self.vertex_labels.len())
            .filter_map(|i| self.vertex_labels.name(LabelId(i as u16)).map(String::from))
            .collect()
    }

    fn vertex_exists(&self, vertex: VertexId) -> bool {
        self.vertex_label_assignments
            .get(vertex.0 as usize)
            .is_some_and(|label| label.0 != u16::MAX)
    }

    fn validate_vertex(&self, vertex: VertexId) -> Result<(), GraphError> {
        if self.vertex_exists(vertex) {
            Ok(())
        } else {
            Err(GraphError::VertexOutOfRange {
                vertex,
                vertices: self.num_vertices,
            })
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error(transparent)]
    Property(#[from] PropertyError),
    #[error(transparent)]
    Adjacency(#[from] AdjacencyError),
    #[error("vertex {vertex:?} is out of range or unallocated; graph has {vertices} vertices")]
    VertexOutOfRange { vertex: VertexId, vertices: usize },
    #[error("edge {edge:?} is out of range or unallocated; graph has {edges} allocated edge IDs")]
    EdgeOutOfRange { edge: EdgeId, edges: u64 },
    #[error("graph must be built before rebuild")]
    NotBuilt,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test_graph() -> Graph {
        let mut g = Graph::new(6, 8);

        g.register_vertex_property("name", PropertyType::String, true, false);
        g.register_vertex_property("entity_type", PropertyType::String, true, false);
        g.register_vertex_property("external_id", PropertyType::String, true, true);
        g.register_vertex_property("tenant_id", PropertyType::String, true, false);

        g.register_edge_property("weight", PropertyType::Float64, false, false);

        // Apple ecosystem subgraph
        let apple = g.add_vertex("Entity");
        g.set_vertex_property(apple, "name", "Apple Inc.".into());
        g.set_vertex_property(apple, "entity_type", "ORG".into());
        g.set_vertex_property(apple, "external_id", "AAPL:Apple Inc.:ORG".into());
        g.set_vertex_property(apple, "tenant_id", "AAPL".into());

        let revenue = g.add_vertex("Entity");
        g.set_vertex_property(revenue, "name", "Revenue".into());
        g.set_vertex_property(revenue, "entity_type", "FIN_METRIC".into());
        g.set_vertex_property(revenue, "external_id", "AAPL:Revenue:FIN_METRIC".into());
        g.set_vertex_property(revenue, "tenant_id", "AAPL".into());

        let services = g.add_vertex("Entity");
        g.set_vertex_property(services, "name", "Services Segment".into());
        g.set_vertex_property(services, "entity_type", "SEGMENT".into());

        let usa = g.add_vertex("Entity");
        g.set_vertex_property(usa, "name", "United States".into());
        g.set_vertex_property(usa, "entity_type", "GEOGRAPHY".into());

        let e1 = g.add_edge(apple, revenue, "Discloses");
        g.set_edge_property(e1, "weight", Value::Float64(0.95));

        g.add_edge(revenue, services, "Has_Component");
        g.add_edge(apple, usa, "Operates_In");

        g.build();
        g
    }

    #[test]
    fn graph_construction_and_query() {
        let g = build_test_graph();

        assert_eq!(g.num_vertices(), 4);
        assert_eq!(g.num_edges(), 3);

        let v0 = g.resolve_vertex(VertexId(0)).unwrap();
        assert_eq!(v0.label, "Entity");
        assert!(
            v0.properties
                .iter()
                .any(|(k, v)| k == "name" && v.as_str() == Some("Apple Inc."))
        );

        // Outgoing Discloses from Apple
        let disclosed = g.neighbors(VertexId(0), "Discloses", Direction::Outgoing);
        assert_eq!(disclosed.len(), 1);
        assert_eq!(disclosed[0], VertexId(1)); // Revenue

        // Incoming Discloses to Revenue
        let disclosers = g.neighbors(VertexId(1), "Discloses", Direction::Incoming);
        assert_eq!(disclosers.len(), 1);
        assert_eq!(disclosers[0], VertexId(0)); // Apple

        // CSR matrix is available for SpMV
        let fwd = g.forward_matrix("Discloses").unwrap();
        assert_eq!(fwd.degree(0), 1);
    }

    #[test]
    fn graph_mutation_after_build() {
        let mut g = Graph::new(10, 10);
        g.register_vertex_property("name", PropertyType::String, true, false);

        let v0 = g.add_vertex("Entity");
        g.set_vertex_property(v0, "name", "Alice".into());
        let v1 = g.add_vertex("Entity");
        g.set_vertex_property(v1, "name", "Bob".into());
        g.add_edge(v0, v1, "KNOWS");
        g.build();

        assert_eq!(g.num_vertices(), 2);
        assert_eq!(g.num_edges(), 1);

        // Add vertex AFTER build
        let v2 = g.add_vertex("Entity");
        g.set_vertex_property(v2, "name", "Charlie".into());
        assert_eq!(g.num_vertices(), 3);

        // Add edge AFTER build
        g.add_edge(v1, v2, "KNOWS");
        assert_eq!(g.num_edges(), 2);

        // Delta edges should be queryable immediately
        let neighbors = g.neighbors(v1, "KNOWS", Direction::Outgoing);
        assert!(neighbors.contains(&v2));

        // Rebuild merges delta into CSR
        g.rebuild();
        let neighbors = g.neighbors(v1, "KNOWS", Direction::Outgoing);
        assert!(neighbors.contains(&v2));

        // Original edges still work after rebuild
        let neighbors = g.neighbors(v0, "KNOWS", Direction::Outgoing);
        assert!(neighbors.contains(&v1));
    }

    #[test]
    fn graph_multiple_edge_labels() {
        let g = build_test_graph();

        let labels = g.edge_label_names();
        assert!(labels.contains(&"Discloses".to_string()));
        assert!(labels.contains(&"Has_Component".to_string()));
        assert!(labels.contains(&"Operates_In".to_string()));

        // Apple operates in USA
        let geo = g.neighbors(VertexId(0), "Operates_In", Direction::Outgoing);
        assert_eq!(geo.len(), 1);
        assert_eq!(
            g.get_vertex_property(geo[0], "name").as_str(),
            Some("United States")
        );
    }

    #[test]
    fn graph_try_mutations_enforce_schema_and_vertex_bounds() {
        let mut g = Graph::new(2, 2);
        g.register_vertex_property("count", PropertyType::Int64, false, false);
        let v0 = g.add_vertex("Entity");

        assert!(
            g.try_set_vertex_property(v0, "count", Value::String("wrong".into()))
                .is_err()
        );
        assert!(g.get_vertex_property(v0, "count").is_null());

        assert!(g.try_add_edge(v0, VertexId(99), "KNOWS").is_err());
        assert_eq!(g.num_edges(), 0);
    }

    #[test]
    fn dynamic_properties_default_to_any_without_weakening_explicit_schema() {
        let mut g = Graph::new(4, 2);
        let a = g.add_vertex("Entity");
        let b = g.add_vertex("Entity");

        g.try_set_vertex_property(a, "dynamic", Value::Int64(42))
            .unwrap();
        g.try_set_vertex_property(b, "dynamic", Value::String("forty-two".into()))
            .unwrap();

        assert_eq!(g.get_vertex_property(a, "dynamic"), Value::Int64(42));
        assert_eq!(
            g.get_vertex_property(b, "dynamic"),
            Value::String("forty-two".into())
        );

        g.register_vertex_property("strict", PropertyType::Int64, false, false);
        assert!(
            g.try_set_vertex_property(a, "strict", Value::String("nope".into()))
                .is_err()
        );
    }

    #[test]
    fn edge_records_include_post_build_delta_edges() {
        let mut g = Graph::new(4, 4);
        let a = g.add_vertex("Entity");
        let b = g.add_vertex("Entity");
        let c = g.add_vertex("Entity");
        let e0 = g.add_edge(a, b, "KNOWS");
        g.build();
        let e1 = g.add_edge(b, c, "KNOWS");

        let records = g.edge_records();
        assert!(
            records
                .iter()
                .any(|e| e.id == e0 && e.source == a && e.target == b)
        );
        assert!(
            records
                .iter()
                .any(|e| e.id == e1 && e.source == b && e.target == c)
        );
    }

    #[test]
    fn edge_delete_tombstone_hides_neighbors_and_snapshots() {
        let mut g = Graph::new(4, 4);
        let a = g.add_vertex("Entity");
        let b = g.add_vertex("Entity");
        let edge = g.add_edge(a, b, "KNOWS");
        g.build();

        assert_eq!(g.neighbors(a, "KNOWS", Direction::Outgoing), vec![b]);
        g.try_remove_edge(edge).unwrap();

        assert!(g.neighbors(a, "KNOWS", Direction::Outgoing).is_empty());
        assert!(!g.edge_exists(edge));
        assert!(g.edge_records().is_empty());
    }

    #[test]
    fn graph_neighbors_with_edges_and_edge_property_lookup() {
        let mut g = Graph::new(2, 2);
        g.register_edge_property("weight", PropertyType::Float64, false, false);
        let a = g.add_vertex("Entity");
        let b = g.add_vertex("Entity");
        let edge = g.add_edge(a, b, "KNOWS");
        g.set_edge_property(edge, "weight", Value::Float64(0.75));
        g.build();

        assert_eq!(
            g.neighbors_with_edges(a, "KNOWS", Direction::Outgoing),
            vec![(b, edge)]
        );
        assert_eq!(g.edge_between(a, b, "KNOWS"), Some(edge));
        assert_eq!(g.get_edge_property(edge, "weight"), Value::Float64(0.75));
    }

    #[test]
    fn graph_degree_counts_typed_and_any_label_edges() {
        let mut g = Graph::new(4, 4);
        let a = g.add_vertex("Entity");
        let b = g.add_vertex("Entity");
        let c = g.add_vertex("Entity");
        let ab = g.add_edge(a, b, "KNOWS");
        g.add_edge(b, a, "KNOWS");
        g.add_edge(a, a, "KNOWS");
        g.add_edge(a, c, "MENTIONS");
        g.build();

        assert_eq!(g.degree(a, "KNOWS", Direction::Outgoing), 2);
        assert_eq!(g.degree(a, "KNOWS", Direction::Incoming), 2);
        assert_eq!(g.degree(a, "KNOWS", Direction::Both), 3);
        assert_eq!(g.incident_degree(a, Direction::Both), 4);

        let delta = g.add_edge(c, a, "KNOWS");
        assert_eq!(g.degree(a, "KNOWS", Direction::Incoming), 3);
        assert_eq!(g.incident_degree(a, Direction::Both), 5);

        g.try_remove_edge(ab).unwrap();
        g.try_remove_edge(delta).unwrap();
        assert_eq!(g.degree(a, "KNOWS", Direction::Outgoing), 1);
        assert_eq!(g.degree(a, "KNOWS", Direction::Incoming), 2);
        assert_eq!(g.incident_degree(a, Direction::Both), 3);
        assert_eq!(g.degree(a, "UNKNOWN", Direction::Both), 0);
    }

    #[test]
    fn vertex_delete_tombstones_incident_edges() {
        let mut g = Graph::new(4, 4);
        let a = g.add_vertex("Entity");
        let b = g.add_vertex("Entity");
        let c = g.add_vertex("Entity");
        let ab = g.add_edge(a, b, "KNOWS");
        let bc = g.add_edge(b, c, "KNOWS");
        g.build();

        g.try_remove_vertex(b).unwrap();

        assert!(g.vertex_label(b).is_none());
        assert!(!g.edge_exists(ab));
        assert!(!g.edge_exists(bc));
        assert!(g.neighbors(a, "KNOWS", Direction::Outgoing).is_empty());
        assert!(g.edge_records().is_empty());
    }

    #[test]
    fn compact_tombstones_clears_deleted_payloads_and_rebuilds_adjacency() {
        let mut g = Graph::new(4, 4);
        g.register_vertex_property("name", PropertyType::String, true, false);
        g.register_edge_property("weight", PropertyType::Float64, false, false);
        let a = g.add_vertex("Entity");
        let b = g.add_vertex("Entity");
        g.set_vertex_property(a, "name", Value::String("A".into()));
        g.set_vertex_property(b, "name", Value::String("B".into()));
        let ab = g.add_edge(a, b, "KNOWS");
        g.set_edge_property(ab, "weight", Value::Float64(1.0));
        g.build();

        let c = g.add_vertex("Entity");
        g.set_vertex_property(c, "name", Value::String("C".into()));
        let bc = g.add_edge(b, c, "KNOWS");
        g.set_edge_property(bc, "weight", Value::Float64(2.0));
        g.try_remove_vertex(b).unwrap();

        assert!(g.edge_meta(ab).is_some());
        assert!(g.edge_meta(bc).is_some());
        assert_eq!(
            g.compaction_pressure(),
            GraphCompactionPressure {
                deleted_vertices: 1,
                tombstoned_edges: 2,
                delta_edges: 1,
            }
        );

        let stats = g.compact_tombstones().unwrap();

        assert_eq!(stats.deleted_vertices_cleared, 1);
        assert_eq!(stats.deleted_edges_cleared, 2);
        assert_eq!(stats.tombstoned_edge_meta_removed, 2);
        assert_eq!(stats.delta_edges_compacted, 1);
        assert_eq!(stats.live_edges_after, 0);
        assert!(g.get_vertex_property(b, "name").is_null());
        assert!(g.get_edge_property(ab, "weight").is_null());
        assert!(g.get_edge_property(bc, "weight").is_null());
        assert!(g.edge_meta(ab).is_none());
        assert!(g.edge_meta(bc).is_none());
        assert!(g.neighbors(a, "KNOWS", Direction::Outgoing).is_empty());
        assert!(g.neighbors(c, "KNOWS", Direction::Incoming).is_empty());
        assert!(g.compaction_pressure().is_empty());
    }

    #[test]
    fn graph_estimated_heap_bytes_tracks_core_owned_memory() {
        let mut g = Graph::new(0, 0);
        let empty_estimate = g.estimated_heap_bytes();

        g.register_vertex_property("name", PropertyType::String, true, false);
        g.register_vertex_property("payload", PropertyType::Any, false, false);
        let a = g.add_vertex("Entity");
        let b = g.add_vertex("Entity");
        g.set_vertex_property(a, "name", Value::String("Alpha".repeat(8)));
        g.set_vertex_property(
            a,
            "payload",
            Value::List(vec![
                Value::String("nested".repeat(4)),
                Value::Map(vec![("key".into(), Value::String("value".repeat(4)))]),
            ]),
        );
        g.add_edge(a, b, "RELATES_TO");
        g.build();

        let populated_estimate = g.estimated_heap_bytes();
        assert!(populated_estimate > empty_estimate);
        assert!(populated_estimate > 0);
    }
}
