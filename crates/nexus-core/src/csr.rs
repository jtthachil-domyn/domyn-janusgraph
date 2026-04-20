//! Compressed Sparse Row (CSR) adjacency store.
//!
//! Each edge label gets its own `CsrMatrix`, enabling predicate-scoped traversal
//! by selecting which matrices to operate on. The CSR format also serves as the
//! native format for GraphBLAS-style sparse matrix-vector multiply (SpMV).
//!
//! Design influences:
//! - FalkorDB: per-label sparse matrices, linear algebra traversal
//! - Kuzu/LadybugDB: CSR with join indices, cache-friendly layout

use crate::types::{Direction, EdgeId, LabelId, VertexId};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeMeta {
    pub source: VertexId,
    pub target: VertexId,
    pub label: LabelId,
    pub deleted: bool,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AdjacencyError {
    #[error(
        "edge endpoint out of range: source={source_id}, target={target_id}, num_vertices={num_vertices}"
    )]
    VertexOutOfRange {
        source_id: u64,
        target_id: u64,
        num_vertices: usize,
    },
    #[error("edge {0:?} does not exist")]
    EdgeNotFound(EdgeId),
}

/// A single CSR matrix representing edges of one label in one direction.
///
/// For N vertices, `offsets` has length N+1. Neighbors of vertex `i` are
/// stored in `neighbors[offsets[i]..offsets[i+1]]`.
///
/// This is also the storage format consumed by SpMV in `nexus-algebra`.
#[derive(Debug, Clone)]
pub struct CsrMatrix {
    /// Length = num_vertices + 1. `offsets[i]` is the start index in `neighbors`
    /// for vertex i's adjacency list.
    pub offsets: Vec<u64>,
    /// Flat array of neighbor vertex IDs, grouped by source vertex.
    pub neighbors: Vec<u64>,
    /// Parallel to `neighbors`: the edge ID for each entry, used to look up
    /// edge properties in the columnar store.
    pub edge_ids: Vec<u64>,
}

impl CsrMatrix {
    pub fn empty(num_vertices: usize) -> Self {
        Self {
            offsets: vec![0; num_vertices + 1],
            neighbors: Vec::new(),
            edge_ids: Vec::new(),
        }
    }

    /// Number of vertices this matrix covers.
    pub fn num_vertices(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Total number of edges (non-zero entries) in this matrix.
    pub fn num_edges(&self) -> usize {
        self.neighbors.len()
    }

    /// Out-degree of vertex `v` in O(1).
    pub fn degree(&self, v: u64) -> usize {
        let idx = v as usize;
        if idx >= self.num_vertices() {
            return 0;
        }
        (self.offsets[idx + 1] - self.offsets[idx]) as usize
    }

    /// Neighbors of vertex `v` as a slice -- O(1) access, cache-friendly iteration.
    pub fn neighbors_of(&self, v: u64) -> &[u64] {
        let idx = v as usize;
        if idx >= self.num_vertices() {
            return &[];
        }
        let start = self.offsets[idx] as usize;
        let end = self.offsets[idx + 1] as usize;
        &self.neighbors[start..end]
    }

    /// Edge IDs for edges from vertex `v`.
    pub fn edge_ids_of(&self, v: u64) -> &[u64] {
        let idx = v as usize;
        if idx >= self.num_vertices() {
            return &[];
        }
        let start = self.offsets[idx] as usize;
        let end = self.offsets[idx + 1] as usize;
        &self.edge_ids[start..end]
    }
}

/// Builder that accumulates edges and produces a finalized `CsrMatrix`.
#[derive(Clone)]
pub struct CsrBuilder {
    num_vertices: usize,
    edges: Vec<(u64, u64, u64)>, // (source, target, edge_id)
}

impl CsrBuilder {
    pub fn new(num_vertices: usize) -> Self {
        Self {
            num_vertices,
            edges: Vec::new(),
        }
    }

    pub fn with_capacity(num_vertices: usize, edge_capacity: usize) -> Self {
        Self {
            num_vertices,
            edges: Vec::with_capacity(edge_capacity),
        }
    }

    pub fn add_edge(&mut self, source: u64, target: u64, edge_id: u64) -> bool {
        if source as usize >= self.num_vertices || target as usize >= self.num_vertices {
            return false;
        }
        self.edges.push((source, target, edge_id));
        true
    }

    pub fn set_num_vertices(&mut self, num_vertices: usize) {
        self.num_vertices = num_vertices;
    }

    /// Build the CSR matrix. Sorts edges by source vertex for CSR layout.
    pub fn build(mut self) -> CsrMatrix {
        self.edges.sort_unstable_by_key(|&(src, tgt, _)| (src, tgt));

        let mut offsets = vec![0u64; self.num_vertices + 1];
        let mut neighbors = Vec::with_capacity(self.edges.len());
        let mut edge_ids = Vec::with_capacity(self.edges.len());

        for &(src, _, _) in &self.edges {
            if (src as usize) < self.num_vertices {
                offsets[src as usize + 1] += 1;
            }
        }

        // Prefix sum to get offsets
        for i in 1..offsets.len() {
            offsets[i] += offsets[i - 1];
        }

        for &(src, tgt, eid) in &self.edges {
            if (src as usize) < self.num_vertices {
                neighbors.push(tgt);
                edge_ids.push(eid);
            }
        }

        CsrMatrix {
            offsets,
            neighbors,
            edge_ids,
        }
    }
}

/// The complete adjacency store: one forward + backward CSR per edge label.
///
/// Forward CSR: source -> targets (outgoing edges)
/// Backward CSR: target -> sources (incoming edges)
///
/// After `build()`, mutations go into a delta layer (adjacency lists) that is
/// merged into new CSR matrices on `rebuild()`. This hybrid approach keeps the
/// CSR read path fast while allowing the graph to remain mutable.
#[derive(Clone)]
pub struct AdjacencyStore {
    num_vertices: usize,
    next_edge_id: u64,
    /// Forward CSR per label: source -> targets
    forward: HashMap<LabelId, CsrBuilder>,
    /// Backward CSR per label: target -> sources
    backward: HashMap<LabelId, CsrBuilder>,
    /// Finalized forward matrices (populated after `build()`)
    forward_csr: HashMap<LabelId, CsrMatrix>,
    /// Finalized backward matrices
    backward_csr: HashMap<LabelId, CsrMatrix>,
    /// Post-build edge additions stored as adjacency lists until next rebuild
    delta_forward: HashMap<LabelId, Vec<(u64, u64, u64)>>,
    delta_backward: HashMap<LabelId, Vec<(u64, u64, u64)>>,
    /// Stable edge metadata, indexed by edge ID. This is the source of truth for
    /// existence checks, snapshot enumeration, and tombstones.
    edge_meta: HashMap<u64, EdgeMeta>,
    /// Per-vertex incident edge index across all labels. Entries are append-only
    /// until rebuild/compaction; tombstoned edges are filtered at read time.
    incident_by_vertex: Vec<Vec<u64>>,
    built: bool,
}

impl AdjacencyStore {
    pub fn new(num_vertices: usize) -> Self {
        Self {
            num_vertices,
            next_edge_id: 0,
            forward: HashMap::new(),
            backward: HashMap::new(),
            forward_csr: HashMap::new(),
            backward_csr: HashMap::new(),
            delta_forward: HashMap::new(),
            delta_backward: HashMap::new(),
            edge_meta: HashMap::new(),
            incident_by_vertex: vec![Vec::new(); num_vertices],
            built: false,
        }
    }

    /// Add an edge. Returns the assigned edge ID.
    ///
    /// Before `build()`, edges go into CsrBuilders. After `build()`, edges
    /// accumulate in the delta layer and become part of the CSR on `rebuild()`.
    pub fn add_edge(&mut self, source: VertexId, target: VertexId, label: LabelId) -> EdgeId {
        self.try_add_edge(source, target, label)
            .expect("edge endpoints must be within the adjacency store vertex range")
    }

    pub fn try_add_edge(
        &mut self,
        source: VertexId,
        target: VertexId,
        label: LabelId,
    ) -> Result<EdgeId, AdjacencyError> {
        let eid = self.next_edge_id;
        self.try_add_edge_with_id(eid, source, target, label)
    }

    fn insert_edge_unchecked(
        &mut self,
        edge_id: u64,
        source: VertexId,
        target: VertexId,
        label: LabelId,
    ) {
        if self.built {
            self.delta_forward
                .entry(label)
                .or_default()
                .push((source.0, target.0, edge_id));
            self.delta_backward
                .entry(label)
                .or_default()
                .push((target.0, source.0, edge_id));
        } else {
            self.forward
                .entry(label)
                .or_insert_with(|| CsrBuilder::new(self.num_vertices))
                .add_edge(source.0, target.0, edge_id);

            self.backward
                .entry(label)
                .or_insert_with(|| CsrBuilder::new(self.num_vertices))
                .add_edge(target.0, source.0, edge_id);
        }
    }

    /// Add an edge with a specific edge ID (used during WAL replay).
    /// Advances `next_edge_id` past the given ID to prevent collisions.
    pub fn add_edge_with_id(
        &mut self,
        edge_id: u64,
        source: VertexId,
        target: VertexId,
        label: LabelId,
    ) -> EdgeId {
        self.try_add_edge_with_id(edge_id, source, target, label)
            .expect("edge endpoints must be within the adjacency store vertex range")
    }

    pub fn try_add_edge_with_id(
        &mut self,
        edge_id: u64,
        source: VertexId,
        target: VertexId,
        label: LabelId,
    ) -> Result<EdgeId, AdjacencyError> {
        if source.0 as usize >= self.num_vertices || target.0 as usize >= self.num_vertices {
            return Err(AdjacencyError::VertexOutOfRange {
                source_id: source.0,
                target_id: target.0,
                num_vertices: self.num_vertices,
            });
        }

        if edge_id >= self.next_edge_id {
            self.next_edge_id = edge_id + 1;
        }

        self.edge_meta.insert(
            edge_id,
            EdgeMeta {
                source,
                target,
                label,
                deleted: false,
            },
        );
        self.record_incident_edge(edge_id, source, target);
        self.insert_edge_unchecked(edge_id, source, target, label);

        Ok(EdgeId(edge_id))
    }

    fn record_incident_edge(&mut self, edge_id: u64, source: VertexId, target: VertexId) {
        let max_vertex = source.0.max(target.0) as usize;
        if max_vertex >= self.incident_by_vertex.len() {
            self.incident_by_vertex
                .resize_with(max_vertex + 1, Vec::new);
        }
        self.incident_by_vertex[source.0 as usize].push(edge_id);
        if source != target {
            self.incident_by_vertex[target.0 as usize].push(edge_id);
        }
    }

    pub fn remove_edge(&mut self, edge_id: EdgeId) -> Result<(), AdjacencyError> {
        let Some(meta) = self.edge_meta.get_mut(&edge_id.0) else {
            return Err(AdjacencyError::EdgeNotFound(edge_id));
        };
        meta.deleted = true;
        Ok(())
    }

    /// Finalize all CSR matrices. After this, no more edges can be added
    /// but traversal and SpMV operations become available.
    pub fn build(&mut self) {
        for (label, builder) in self.forward.drain() {
            self.forward_csr.insert(label, builder.build());
        }
        for (label, builder) in self.backward.drain() {
            self.backward_csr.insert(label, builder.build());
        }
        self.built = true;
    }

    pub fn is_built(&self) -> bool {
        self.built
    }

    pub fn num_vertices(&self) -> usize {
        self.num_vertices
    }

    pub fn num_edges(&self) -> u64 {
        self.next_edge_id
    }

    pub fn edge_exists(&self, edge_id: EdgeId) -> bool {
        self.edge_meta
            .get(&edge_id.0)
            .is_some_and(|meta| !meta.deleted)
    }

    pub fn edge_meta(&self, edge_id: EdgeId) -> Option<EdgeMeta> {
        self.edge_meta.get(&edge_id.0).copied()
    }

    pub fn incident_edges(&self, vertex: VertexId) -> Vec<EdgeId> {
        let Some(incident) = self.incident_by_vertex.get(vertex.0 as usize) else {
            return Vec::new();
        };
        let mut edges: Vec<_> = incident
            .iter()
            .copied()
            .filter(|&eid| self.edge_exists(EdgeId(eid)))
            .map(EdgeId)
            .collect();
        edges.sort_unstable_by_key(|edge| edge.0);
        edges.dedup_by_key(|edge| edge.0);
        edges
    }

    pub fn neighbors_with_edges_any_label(
        &self,
        vertex: VertexId,
        direction: Direction,
    ) -> Vec<(VertexId, EdgeId)> {
        let Some(incident) = self.incident_by_vertex.get(vertex.0 as usize) else {
            return Vec::new();
        };

        let mut result = Vec::new();
        for &eid in incident {
            let Some(meta) = self.edge_meta.get(&eid) else {
                continue;
            };
            if meta.deleted {
                continue;
            }

            match direction {
                Direction::Outgoing if meta.source == vertex => {
                    result.push((meta.target, EdgeId(eid)));
                }
                Direction::Incoming if meta.target == vertex => {
                    result.push((meta.source, EdgeId(eid)));
                }
                Direction::Both => {
                    if meta.source == vertex {
                        result.push((meta.target, EdgeId(eid)));
                    } else if meta.target == vertex {
                        result.push((meta.source, EdgeId(eid)));
                    }
                }
                _ => {}
            }
        }
        result.sort_unstable_by_key(|(neighbor, edge)| (edge.0, neighbor.0));
        result.dedup_by_key(|(_, edge)| edge.0);
        result
    }

    pub fn incident_degree(&self, vertex: VertexId, direction: Direction) -> usize {
        self.neighbors_with_edges_any_label(vertex, direction).len()
    }

    /// Get the forward CSR matrix for a specific edge label.
    pub fn forward_matrix(&self, label: LabelId) -> Option<&CsrMatrix> {
        self.forward_csr.get(&label)
    }

    /// Get the backward CSR matrix for a specific edge label.
    pub fn backward_matrix(&self, label: LabelId) -> Option<&CsrMatrix> {
        self.backward_csr.get(&label)
    }

    /// Get neighbors of a vertex for a given label and direction.
    ///
    /// Merges results from both the CSR matrices and the delta layer so that
    /// edges added after `build()` are immediately visible.
    pub fn neighbors(
        &self,
        vertex: VertexId,
        label: LabelId,
        direction: Direction,
    ) -> Vec<VertexId> {
        self.neighbors_with_edges(vertex, label, direction)
            .into_iter()
            .map(|(neighbor, _)| neighbor)
            .collect()
    }

    pub fn neighbors_with_edges(
        &self,
        vertex: VertexId,
        label: LabelId,
        direction: Direction,
    ) -> Vec<(VertexId, EdgeId)> {
        let mut result = Vec::new();
        let vid = vertex.0;

        match direction {
            Direction::Outgoing => {
                if let Some(csr) = self.forward_csr.get(&label) {
                    result.extend(
                        csr.neighbors_of(vid)
                            .iter()
                            .zip(csr.edge_ids_of(vid).iter())
                            .filter(|&(_, &eid)| self.edge_exists(EdgeId(eid)))
                            .map(|(&v, &eid)| (VertexId(v), EdgeId(eid))),
                    );
                }
                if let Some(delta) = self.delta_forward.get(&label) {
                    result.extend(
                        delta
                            .iter()
                            .filter(|&&(src, _, eid)| src == vid && self.edge_exists(EdgeId(eid)))
                            .map(|&(_, tgt, eid)| (VertexId(tgt), EdgeId(eid))),
                    );
                }
            }
            Direction::Incoming => {
                if let Some(csr) = self.backward_csr.get(&label) {
                    result.extend(
                        csr.neighbors_of(vid)
                            .iter()
                            .zip(csr.edge_ids_of(vid).iter())
                            .filter(|&(_, &eid)| self.edge_exists(EdgeId(eid)))
                            .map(|(&v, &eid)| (VertexId(v), EdgeId(eid))),
                    );
                }
                if let Some(delta) = self.delta_backward.get(&label) {
                    result.extend(
                        delta
                            .iter()
                            .filter(|&&(src, _, eid)| src == vid && self.edge_exists(EdgeId(eid)))
                            .map(|&(_, tgt, eid)| (VertexId(tgt), EdgeId(eid))),
                    );
                }
            }
            Direction::Both => {
                if let Some(csr) = self.forward_csr.get(&label) {
                    result.extend(
                        csr.neighbors_of(vid)
                            .iter()
                            .zip(csr.edge_ids_of(vid).iter())
                            .filter(|&(_, &eid)| self.edge_exists(EdgeId(eid)))
                            .map(|(&v, &eid)| (VertexId(v), EdgeId(eid))),
                    );
                }
                if let Some(delta) = self.delta_forward.get(&label) {
                    result.extend(
                        delta
                            .iter()
                            .filter(|&&(src, _, eid)| src == vid && self.edge_exists(EdgeId(eid)))
                            .map(|&(_, tgt, eid)| (VertexId(tgt), EdgeId(eid))),
                    );
                }
                if let Some(csr) = self.backward_csr.get(&label) {
                    result.extend(
                        csr.neighbors_of(vid)
                            .iter()
                            .zip(csr.edge_ids_of(vid).iter())
                            .filter(|&(_, &eid)| self.edge_exists(EdgeId(eid)))
                            .map(|(&v, &eid)| (VertexId(v), EdgeId(eid))),
                    );
                }
                if let Some(delta) = self.delta_backward.get(&label) {
                    result.extend(
                        delta
                            .iter()
                            .filter(|&&(src, _, eid)| src == vid && self.edge_exists(EdgeId(eid)))
                            .map(|&(_, tgt, eid)| (VertexId(tgt), EdgeId(eid))),
                    );
                }
            }
        }
        result
    }

    /// All edge labels that have been added (CSR + delta).
    pub fn labels(&self) -> Vec<LabelId> {
        let mut labels: Vec<LabelId> = self.forward_csr.keys().copied().collect();
        for &label in self.forward.keys() {
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        for &label in self.delta_forward.keys() {
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        for meta in self.edge_meta.values() {
            if !meta.deleted && !labels.contains(&meta.label) {
                labels.push(meta.label);
            }
        }
        labels
    }

    pub fn edges_for_label(&self, label: LabelId) -> Vec<(EdgeId, VertexId, VertexId)> {
        let mut edges: Vec<_> = self
            .edge_meta
            .iter()
            .filter(|(_, meta)| meta.label == label && !meta.deleted)
            .map(|(&eid, meta)| (EdgeId(eid), meta.source, meta.target))
            .collect();
        edges.sort_unstable_by_key(|(eid, _, _)| eid.0);
        edges
    }

    /// Update the vertex count. Called by Graph when vertices are added after build.
    pub fn set_num_vertices(&mut self, n: usize) {
        self.num_vertices = n;
        self.incident_by_vertex.resize_with(n, Vec::new);
        for builder in self.forward.values_mut() {
            builder.set_num_vertices(n);
        }
        for builder in self.backward.values_mut() {
            builder.set_num_vertices(n);
        }
    }

    /// Merge the delta layer into fresh CSR matrices.
    ///
    /// Drains all existing CSR data and delta edges into new CsrBuilders,
    /// then builds new CsrMatrices with the current `num_vertices`.
    pub fn rebuild(&mut self) {
        let all_labels: Vec<LabelId> = self.labels();

        let mut new_forward_csr = HashMap::new();
        let mut new_backward_csr = HashMap::new();

        for label in &all_labels {
            let mut fwd_builder = CsrBuilder::new(self.num_vertices);
            let mut bwd_builder = CsrBuilder::new(self.num_vertices);

            let mut label_edges: Vec<_> = self
                .edge_meta
                .iter()
                .filter(|(_, meta)| meta.label == *label && !meta.deleted)
                .collect();
            label_edges.sort_unstable_by_key(|(eid, _)| **eid);

            for (&eid, meta) in label_edges {
                fwd_builder.add_edge(meta.source.0, meta.target.0, eid);
                bwd_builder.add_edge(meta.target.0, meta.source.0, eid);
            }

            new_forward_csr.insert(*label, fwd_builder.build());
            new_backward_csr.insert(*label, bwd_builder.build());
        }

        self.forward_csr = new_forward_csr;
        self.backward_csr = new_backward_csr;
        self.delta_forward.clear();
        self.delta_backward.clear();
        self.edge_meta.retain(|_, meta| !meta.deleted);
        self.rebuild_incident_index();
    }

    fn rebuild_incident_index(&mut self) {
        self.incident_by_vertex = vec![Vec::new(); self.num_vertices];
        let mut live_edges: Vec<_> = self
            .edge_meta
            .iter()
            .filter(|(_, meta)| !meta.deleted)
            .map(|(&eid, meta)| (eid, *meta))
            .collect();
        live_edges.sort_unstable_by_key(|(eid, _)| *eid);
        for (eid, meta) in live_edges {
            self.record_incident_edge(eid, meta.source, meta.target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csr_basic_construction() {
        let mut builder = CsrBuilder::new(4);
        // 0 -> 1, 0 -> 2, 1 -> 3, 2 -> 3
        builder.add_edge(0, 1, 0);
        builder.add_edge(0, 2, 1);
        builder.add_edge(1, 3, 2);
        builder.add_edge(2, 3, 3);

        let csr = builder.build();

        assert_eq!(csr.num_vertices(), 4);
        assert_eq!(csr.num_edges(), 4);
        assert_eq!(csr.degree(0), 2);
        assert_eq!(csr.degree(1), 1);
        assert_eq!(csr.degree(2), 1);
        assert_eq!(csr.degree(3), 0);
        assert_eq!(csr.neighbors_of(0), &[1, 2]);
        assert_eq!(csr.neighbors_of(1), &[3]);
        assert_eq!(csr.neighbors_of(2), &[3]);
        assert_eq!(csr.neighbors_of(3), &[]);
    }

    #[test]
    fn csr_ignores_out_of_range_edges() {
        let mut builder = CsrBuilder::new(3);
        builder.add_edge(0, 1, 0);
        builder.add_edge(1, 2, 1);
        builder.add_edge(99, 0, 2); // out of range source
        builder.add_edge(0, 99, 3); // out of range target

        let csr = builder.build();
        assert_eq!(csr.num_vertices(), 3);
        assert_eq!(csr.degree(0), 1); // 0->1
        assert_eq!(csr.degree(1), 1); // 1->2
        assert_eq!(csr.degree(2), 0);
    }

    #[test]
    fn adjacency_store_bidirectional() {
        let mut store = AdjacencyStore::new(4);
        let label = LabelId(0); // e.g., "KNOWS"

        let e0 = store.add_edge(VertexId(0), VertexId(1), label);
        store.add_edge(VertexId(0), VertexId(2), label);
        store.add_edge(VertexId(1), VertexId(3), label);
        store.build();

        // Forward: outgoing neighbors
        let out = store.neighbors(VertexId(0), label, Direction::Outgoing);
        assert_eq!(out.len(), 2);
        assert!(out.contains(&VertexId(1)));
        assert!(out.contains(&VertexId(2)));

        // Backward: who points to vertex 3?
        let inc = store.neighbors(VertexId(3), label, Direction::Incoming);
        assert_eq!(inc.len(), 1);
        assert!(inc.contains(&VertexId(1)));

        // Both directions for vertex 1
        let both = store.neighbors(VertexId(1), label, Direction::Both);
        assert!(both.contains(&VertexId(3))); // outgoing
        assert!(both.contains(&VertexId(0))); // incoming

        let with_edges = store.neighbors_with_edges(VertexId(0), label, Direction::Outgoing);
        assert!(with_edges.contains(&(VertexId(1), e0)));
    }

    #[test]
    fn adjacency_store_tracks_edge_metadata_and_tombstones() {
        let mut store = AdjacencyStore::new(3);
        let label = LabelId(0);

        let edge = store.add_edge(VertexId(0), VertexId(1), label);
        store.build();

        assert!(store.edge_exists(edge));
        assert_eq!(
            store.edges_for_label(label),
            vec![(edge, VertexId(0), VertexId(1))]
        );

        store.remove_edge(edge).unwrap();

        assert!(!store.edge_exists(edge));
        assert!(
            store
                .neighbors(VertexId(0), label, Direction::Outgoing)
                .is_empty()
        );
        assert!(store.edges_for_label(label).is_empty());
    }

    #[test]
    fn adjacency_store_rejects_out_of_range_edges() {
        let mut store = AdjacencyStore::new(1);
        let result = store.try_add_edge(VertexId(0), VertexId(2), LabelId(0));

        assert!(matches!(
            result,
            Err(AdjacencyError::VertexOutOfRange { .. })
        ));
        assert_eq!(store.num_edges(), 0);
    }

    #[test]
    fn adjacency_store_lists_incident_edges_from_metadata() {
        let mut store = AdjacencyStore::new(4);
        let label = LabelId(0);
        let e0 = store.add_edge(VertexId(0), VertexId(1), label);
        let e1 = store.add_edge(VertexId(2), VertexId(0), label);
        let e2 = store.add_edge(VertexId(2), VertexId(3), label);

        assert_eq!(store.incident_edges(VertexId(0)), vec![e0, e1]);
        assert_eq!(store.incident_edges(VertexId(3)), vec![e2]);
    }

    #[test]
    fn adjacency_store_lists_neighbors_across_all_labels() {
        let mut store = AdjacencyStore::new(4);
        let e0 = store.add_edge(VertexId(0), VertexId(1), LabelId(0));
        let e1 = store.add_edge(VertexId(2), VertexId(0), LabelId(1));
        let e2 = store.add_edge(VertexId(0), VertexId(0), LabelId(2));
        store.build();

        assert_eq!(
            store.neighbors_with_edges_any_label(VertexId(0), Direction::Both),
            vec![(VertexId(1), e0), (VertexId(2), e1), (VertexId(0), e2)]
        );
        assert_eq!(store.incident_degree(VertexId(0), Direction::Both), 3);

        store.remove_edge(e1).unwrap();
        assert_eq!(
            store.neighbors_with_edges_any_label(VertexId(0), Direction::Both),
            vec![(VertexId(1), e0), (VertexId(0), e2)]
        );
    }

    #[test]
    fn rebuild_compacts_tombstoned_edge_metadata() {
        let mut store = AdjacencyStore::new(3);
        let label = LabelId(0);
        let edge = store.add_edge(VertexId(0), VertexId(1), label);
        store.build();
        store.remove_edge(edge).unwrap();

        assert!(store.edge_meta(edge).is_some());

        store.rebuild();

        assert!(store.edge_meta(edge).is_none());
        assert!(
            store
                .neighbors(VertexId(0), label, Direction::Outgoing)
                .is_empty()
        );
    }
}
