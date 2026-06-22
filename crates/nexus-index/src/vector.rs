//! HNSW-style vector index for vertex embeddings (GenAI-native).
//!
//! The index keeps an exact search path as a correctness oracle and uses a
//! deterministic, multi-layer navigable-small-world graph for approximate
//! nearest-neighbor search once the corpus is large enough to benefit from it.

use nexus_core::types::VertexId;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use thiserror::Error;

const DEFAULT_M: usize = 16;
const DEFAULT_EF_CONSTRUCTION: usize = 64;
const DEFAULT_EF_SEARCH: usize = 64;
const EXACT_FALLBACK_LEN: usize = 128;
const VECTOR_SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum VectorPersistenceError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("unsupported vector snapshot version: {0}")]
    UnsupportedVersion(u32),
    #[error(
        "embedding dimension mismatch for vertex {vertex_id}: expected {expected}, got {actual}"
    )]
    DimensionMismatch {
        vertex_id: u64,
        expected: usize,
        actual: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VectorSnapshot {
    version: u32,
    dimension: usize,
    m: usize,
    ef_construction: usize,
    ef_search: usize,
    entries: Vec<VectorSnapshotEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VectorSnapshotEntry {
    vertex_id: u64,
    embedding: Vec<f32>,
}

/// A single vector entry: vertex ID + embedding.
#[derive(Debug, Clone)]
struct VectorEntry {
    vertex_id: u64,
    embedding: Vec<f32>,
    deleted: bool,
}

/// Vector index with an exact oracle and an HNSW-style ANN graph.
#[derive(Debug, Clone)]
pub struct VectorIndex {
    entries: Vec<VectorEntry>,
    links: Vec<Vec<Vec<usize>>>,
    by_vertex: HashMap<u64, usize>,
    dimension: usize,
    active_len: usize,
    entry_point: Option<usize>,
    max_level: usize,
    m: usize,
    ef_construction: usize,
    ef_search: usize,
}

impl VectorIndex {
    pub fn new(dimension: usize) -> Self {
        Self::with_hnsw_params(
            dimension,
            DEFAULT_M,
            DEFAULT_EF_CONSTRUCTION,
            DEFAULT_EF_SEARCH,
        )
    }

    pub fn with_hnsw_params(
        dimension: usize,
        m: usize,
        ef_construction: usize,
        ef_search: usize,
    ) -> Self {
        let m = m.max(2);
        Self {
            entries: Vec::new(),
            links: Vec::new(),
            by_vertex: HashMap::new(),
            dimension,
            active_len: 0,
            entry_point: None,
            max_level: 0,
            m,
            ef_construction: ef_construction.max(m),
            ef_search: ef_search.max(m),
        }
    }

    pub fn dimension(&self) -> usize {
        self.dimension
    }

    pub fn len(&self) -> usize {
        self.active_len
    }

    pub fn is_empty(&self) -> bool {
        self.active_len == 0
    }

    /// Add a vertex embedding to the index.
    pub fn add(&mut self, vertex_id: VertexId, embedding: Vec<f32>) {
        assert_eq!(
            embedding.len(),
            self.dimension,
            "embedding dimension mismatch: expected {}, got {}",
            self.dimension,
            embedding.len()
        );

        if self.by_vertex.contains_key(&vertex_id.0) {
            self.update(vertex_id, embedding);
            return;
        }

        self.insert_new(vertex_id, embedding);
    }

    /// Update an existing embedding, or insert it if it does not exist.
    ///
    /// Existing entries are tombstoned and a fresh HNSW node is inserted. This
    /// keeps query results correct immediately; `compact()` can later rebuild
    /// the graph and purge tombstones.
    pub fn update(&mut self, vertex_id: VertexId, embedding: Vec<f32>) {
        assert_eq!(
            embedding.len(),
            self.dimension,
            "embedding dimension mismatch: expected {}, got {}",
            self.dimension,
            embedding.len()
        );
        self.remove(vertex_id);
        self.insert_new(vertex_id, embedding);
    }

    /// Remove a vertex embedding. Returns true when an active embedding was
    /// present.
    pub fn remove(&mut self, vertex_id: VertexId) -> bool {
        let Some(idx) = self.by_vertex.remove(&vertex_id.0) else {
            return false;
        };
        if self.entries[idx].deleted {
            return false;
        }
        self.entries[idx].deleted = true;
        self.active_len = self.active_len.saturating_sub(1);
        if self.entry_point == Some(idx) {
            self.refresh_entry_point();
        }
        true
    }

    pub fn tombstone_count(&self) -> usize {
        self.entries.len().saturating_sub(self.active_len)
    }

    /// Rebuild the HNSW graph from active entries and purge tombstones.
    pub fn compact(&mut self) {
        if self.tombstone_count() == 0 {
            return;
        }
        let active: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| !entry.deleted)
            .map(|entry| (VertexId(entry.vertex_id), entry.embedding.clone()))
            .collect();
        let mut rebuilt = VectorIndex::with_hnsw_params(
            self.dimension,
            self.m,
            self.ef_construction,
            self.ef_search,
        );
        for (vertex, embedding) in active {
            rebuilt.add(vertex, embedding);
        }
        *self = rebuilt;
    }

    /// Persist active vector entries to an atomically-renamed JSON snapshot.
    ///
    /// Tombstones and stale HNSW links are intentionally omitted. Loading the
    /// snapshot rebuilds the ANN graph from active embeddings only.
    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), VectorPersistenceError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let snapshot = self.to_snapshot();
        let json = serde_json::to_vec_pretty(&snapshot)?;
        let tmp_path = tmp_path_for(path);
        let mut file = File::create(&tmp_path)?;
        file.write_all(&json)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)?;
        Ok(())
    }

    /// Load a vector snapshot and rebuild the HNSW graph.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, VectorPersistenceError> {
        let json = fs::read_to_string(path)?;
        let snapshot: VectorSnapshot = serde_json::from_str(&json)?;
        Self::from_snapshot(snapshot)
    }

    fn to_snapshot(&self) -> VectorSnapshot {
        let mut entries: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| !entry.deleted)
            .map(|entry| VectorSnapshotEntry {
                vertex_id: entry.vertex_id,
                embedding: entry.embedding.clone(),
            })
            .collect();
        entries.sort_by_key(|entry| entry.vertex_id);

        VectorSnapshot {
            version: VECTOR_SNAPSHOT_VERSION,
            dimension: self.dimension,
            m: self.m,
            ef_construction: self.ef_construction,
            ef_search: self.ef_search,
            entries,
        }
    }

    fn from_snapshot(snapshot: VectorSnapshot) -> Result<Self, VectorPersistenceError> {
        if snapshot.version != VECTOR_SNAPSHOT_VERSION {
            return Err(VectorPersistenceError::UnsupportedVersion(snapshot.version));
        }

        let mut index = VectorIndex::with_hnsw_params(
            snapshot.dimension,
            snapshot.m,
            snapshot.ef_construction,
            snapshot.ef_search,
        );
        for entry in snapshot.entries {
            if entry.embedding.len() != snapshot.dimension {
                return Err(VectorPersistenceError::DimensionMismatch {
                    vertex_id: entry.vertex_id,
                    expected: snapshot.dimension,
                    actual: entry.embedding.len(),
                });
            }
            index.add(VertexId(entry.vertex_id), entry.embedding);
        }
        Ok(index)
    }

    fn insert_new(&mut self, vertex_id: VertexId, embedding: Vec<f32>) {
        if self.entry_point.is_none() && !self.entries.is_empty() {
            self.refresh_entry_point();
        }

        let idx = self.entries.len();
        let level = deterministic_level(vertex_id.0, idx as u64);
        self.entries.push(VectorEntry {
            vertex_id: vertex_id.0,
            embedding,
            deleted: false,
        });
        self.links.push(vec![Vec::new(); level + 1]);
        self.by_vertex.insert(vertex_id.0, idx);
        self.active_len += 1;

        let Some(mut entry) = self.entry_point else {
            self.entry_point = Some(idx);
            self.max_level = level;
            return;
        };

        for layer in ((level + 1)..=self.max_level).rev() {
            entry = self.greedy_search_layer(idx, entry, layer);
        }

        for layer in (0..=level.min(self.max_level)).rev() {
            let candidates = self.search_layer_by_index(idx, entry, self.ef_construction, layer);
            let selected = self.select_neighbors_by_index(idx, candidates, self.m);
            for neighbor in selected {
                self.connect(idx, neighbor, layer);
            }
            if let Some(best) = self.best_neighbor_by_index(idx, layer) {
                entry = best;
            }
        }

        if level > self.max_level {
            self.entry_point = Some(idx);
            self.max_level = level;
        }
    }

    /// Find the `k` nearest neighbors. Uses exact search for tiny indexes and
    /// HNSW-style ANN search for larger indexes.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(VertexId, f32)> {
        assert_eq!(query.len(), self.dimension);
        if k == 0 || self.active_len == 0 {
            return Vec::new();
        }
        if self.active_len <= EXACT_FALLBACK_LEN {
            return self.search_exact(query, k);
        }
        self.search_hnsw(query, k)
    }

    /// Exact nearest-neighbor search. Kept as the correctness oracle for tests,
    /// recall validation, and small indexes.
    pub fn search_exact(&self, query: &[f32], k: usize) -> Vec<(VertexId, f32)> {
        assert_eq!(query.len(), self.dimension);

        let mut scored: Vec<(u64, f32)> = self
            .entries
            .iter()
            .filter(|entry| !entry.deleted)
            .map(|entry| {
                let dist = cosine_distance(query, &entry.embedding);
                (entry.vertex_id, dist)
            })
            .collect();

        scored.sort_by(score_vertex_cmp);
        scored.truncate(k);

        scored
            .into_iter()
            .map(|(id, dist)| (VertexId(id), dist))
            .collect()
    }

    pub fn estimated_heap_bytes(&self) -> usize {
        self.entries.capacity() * size_of::<VectorEntry>()
            + self
                .entries
                .iter()
                .map(|entry| entry.embedding.capacity() * size_of::<f32>())
                .sum::<usize>()
            + self.links.capacity() * size_of::<Vec<Vec<usize>>>()
            + self
                .links
                .iter()
                .map(|layers| {
                    layers.capacity() * size_of::<Vec<usize>>()
                        + layers
                            .iter()
                            .map(|neighbors| neighbors.capacity() * size_of::<usize>())
                            .sum::<usize>()
                })
                .sum::<usize>()
            + self.by_vertex.capacity() * size_of::<(u64, usize)>()
    }

    /// HNSW-style approximate nearest-neighbor search.
    pub fn search_hnsw(&self, query: &[f32], k: usize) -> Vec<(VertexId, f32)> {
        assert_eq!(query.len(), self.dimension);
        if k == 0 || self.active_len == 0 {
            return Vec::new();
        }

        let Some(mut entry) = self
            .entry_point
            .filter(|&idx| self.is_active_idx(idx))
            .or_else(|| self.first_active_index())
        else {
            return Vec::new();
        };

        for layer in (1..=self.max_level).rev() {
            entry = self.greedy_search_query(query, entry, layer);
        }

        let candidates = self.search_layer_by_query(query, entry, self.ef_search.max(k), 0);
        let mut scored: Vec<(u64, f32)> = candidates
            .into_iter()
            .filter(|&candidate| self.is_active_idx(candidate))
            .map(|candidate| {
                (
                    self.entries[candidate].vertex_id,
                    cosine_distance(query, &self.entries[candidate].embedding),
                )
            })
            .collect();
        if scored.len() < k.min(self.active_len) {
            return self.search_exact(query, k);
        }
        scored.sort_by(score_vertex_cmp);
        scored.truncate(k);
        scored
            .into_iter()
            .map(|(id, dist)| (VertexId(id), dist))
            .collect()
    }

    /// Find vertices whose embeddings are within `threshold` cosine distance.
    ///
    /// This remains exact: threshold queries have set semantics and should not
    /// silently miss qualifying vertices.
    pub fn search_within(&self, query: &[f32], threshold: f32) -> Vec<(VertexId, f32)> {
        assert_eq!(query.len(), self.dimension);

        self.entries
            .iter()
            .filter(|entry| !entry.deleted)
            .filter_map(|entry| {
                let dist = cosine_distance(query, &entry.embedding);
                if dist <= threshold {
                    Some((VertexId(entry.vertex_id), dist))
                } else {
                    None
                }
            })
            .collect()
    }

    fn connect(&mut self, left: usize, right: usize, layer: usize) {
        self.add_directed_link(left, right, layer);
        self.add_directed_link(right, left, layer);
        self.prune_links(left, layer);
        self.prune_links(right, layer);
    }

    fn add_directed_link(&mut self, from: usize, to: usize, layer: usize) {
        if from == to || layer >= self.links[from].len() {
            return;
        }
        if !self.links[from][layer].contains(&to) {
            self.links[from][layer].push(to);
        }
    }

    fn prune_links(&mut self, node: usize, layer: usize) {
        if layer >= self.links[node].len() || self.links[node][layer].len() <= self.m {
            return;
        }
        let entries = &self.entries;
        self.links[node][layer].retain(|&idx| entries.get(idx).is_some_and(|entry| !entry.deleted));
        let embedding = self.entries[node].embedding.clone();
        self.links[node][layer].sort_by(|&a, &b| {
            let da = cosine_distance(&embedding, &self.entries[a].embedding);
            let db = cosine_distance(&embedding, &self.entries[b].embedding);
            score_index_cmp((a, da), (b, db))
        });
        self.links[node][layer].truncate(self.m);
    }

    fn greedy_search_layer(&self, target: usize, entry: usize, layer: usize) -> usize {
        let target_embedding = &self.entries[target].embedding;
        self.greedy_search_query(target_embedding, entry, layer)
    }

    fn greedy_search_query(&self, query: &[f32], mut current: usize, layer: usize) -> usize {
        let mut current_dist = cosine_distance(query, &self.entries[current].embedding);
        loop {
            let mut improved = false;
            for &neighbor in self.neighbors(current, layer) {
                let dist = cosine_distance(query, &self.entries[neighbor].embedding);
                if dist < current_dist {
                    current = neighbor;
                    current_dist = dist;
                    improved = true;
                }
            }
            if !improved {
                return current;
            }
        }
    }

    fn search_layer_by_index(
        &self,
        target: usize,
        entry: usize,
        ef: usize,
        layer: usize,
    ) -> Vec<usize> {
        let target_embedding = &self.entries[target].embedding;
        self.search_layer_by_query(target_embedding, entry, ef, layer)
            .into_iter()
            .filter(|candidate| *candidate != target && self.is_active_idx(*candidate))
            .collect()
    }

    fn search_layer_by_query(
        &self,
        query: &[f32],
        entry: usize,
        ef: usize,
        layer: usize,
    ) -> Vec<usize> {
        let mut visited = vec![entry];
        let mut candidates = vec![entry];
        let mut result = if self.is_active_idx(entry) {
            vec![entry]
        } else {
            Vec::new()
        };

        while let Some(current) = pop_nearest(query, &self.entries, &mut candidates) {
            let current_dist = cosine_distance(query, &self.entries[current].embedding);
            let worst_dist = worst_distance(query, &self.entries, &result);
            if result.len() >= ef && current_dist > worst_dist {
                break;
            }

            for &neighbor in self.neighbors(current, layer) {
                if visited.contains(&neighbor) {
                    continue;
                }
                visited.push(neighbor);
                candidates.push(neighbor);
                if self.is_active_idx(neighbor) {
                    result.push(neighbor);
                    result.sort_by(|&a, &b| {
                        let da = cosine_distance(query, &self.entries[a].embedding);
                        let db = cosine_distance(query, &self.entries[b].embedding);
                        score_index_cmp((a, da), (b, db))
                    });
                    result.truncate(ef);
                }
            }
        }

        result
    }

    fn select_neighbors_by_index(
        &self,
        target: usize,
        candidates: Vec<usize>,
        limit: usize,
    ) -> Vec<usize> {
        let target_embedding = &self.entries[target].embedding;
        let mut scored = candidates;
        scored.retain(|&idx| self.is_active_idx(idx));
        scored.sort_by(|&a, &b| {
            let da = cosine_distance(target_embedding, &self.entries[a].embedding);
            let db = cosine_distance(target_embedding, &self.entries[b].embedding);
            score_index_cmp((a, da), (b, db))
        });
        scored.dedup();
        scored.truncate(limit);
        scored
    }

    fn best_neighbor_by_index(&self, target: usize, layer: usize) -> Option<usize> {
        let target_embedding = &self.entries[target].embedding;
        self.neighbors(target, layer)
            .iter()
            .copied()
            .filter(|&idx| self.is_active_idx(idx))
            .min_by(|&a, &b| {
                let da = cosine_distance(target_embedding, &self.entries[a].embedding);
                let db = cosine_distance(target_embedding, &self.entries[b].embedding);
                score_index_cmp((a, da), (b, db))
            })
    }

    fn neighbors(&self, node: usize, layer: usize) -> &[usize] {
        self.links
            .get(node)
            .and_then(|layers| layers.get(layer))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn is_active_idx(&self, idx: usize) -> bool {
        self.entries.get(idx).is_some_and(|entry| !entry.deleted)
    }

    fn first_active_index(&self) -> Option<usize> {
        self.entries.iter().position(|entry| !entry.deleted)
    }

    fn refresh_entry_point(&mut self) {
        let Some((idx, level)) = self
            .links
            .iter()
            .enumerate()
            .filter(|(idx, _)| self.is_active_idx(*idx))
            .map(|(idx, layers)| (idx, layers.len().saturating_sub(1)))
            .max_by_key(|(_, level)| *level)
        else {
            self.entry_point = None;
            self.max_level = 0;
            return;
        };
        self.entry_point = Some(idx);
        self.max_level = level;
    }

    #[cfg(test)]
    fn total_links(&self) -> usize {
        self.links
            .iter()
            .flat_map(|layers| layers.iter())
            .map(Vec::len)
            .sum()
    }
}

fn pop_nearest(
    query: &[f32],
    entries: &[VectorEntry],
    candidates: &mut Vec<usize>,
) -> Option<usize> {
    let best_pos = candidates
        .iter()
        .enumerate()
        .min_by(|left, right| {
            let a = *left.1;
            let b = *right.1;
            let da = cosine_distance(query, &entries[a].embedding);
            let db = cosine_distance(query, &entries[b].embedding);
            score_index_cmp((a, da), (b, db))
        })
        .map(|(pos, _)| pos)?;
    Some(candidates.swap_remove(best_pos))
}

fn worst_distance(query: &[f32], entries: &[VectorEntry], candidates: &[usize]) -> f32 {
    if candidates.is_empty() {
        return f32::INFINITY;
    }
    candidates
        .iter()
        .map(|&candidate| cosine_distance(query, &entries[candidate].embedding))
        .fold(f32::NEG_INFINITY, f32::max)
}

fn deterministic_level(vertex_id: u64, idx: u64) -> usize {
    let mut x = vertex_id
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(idx.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;

    let mut level = 0usize;
    while level < 16 && ((x >> (level * 4)) & 0xF) == 0 {
        level += 1;
    }
    level
}

fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn magnitude(v: &[f32]) -> f32 {
    dot_product(v, v).sqrt()
}

/// Cosine distance: 1 - cosine_similarity. Range [0, 2]. 0 = identical.
fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let dot = dot_product(a, b);
    let mag_a = magnitude(a);
    let mag_b = magnitude(b);
    if mag_a == 0.0 || mag_b == 0.0 {
        return 1.0;
    }
    1.0 - (dot / (mag_a * mag_b))
}

fn score_vertex_cmp(a: &(u64, f32), b: &(u64, f32)) -> Ordering {
    a.1.partial_cmp(&b.1)
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.0.cmp(&b.0))
}

fn score_index_cmp(a: (usize, f32), b: (usize, f32)) -> Ordering {
    a.1.partial_cmp(&b.1)
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.0.cmp(&b.0))
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("vectors.json");
    path.with_file_name(format!("{file_name}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_index_basic_search() {
        let mut idx = VectorIndex::new(3);
        let empty = idx.estimated_heap_bytes();

        idx.add(VertexId(0), vec![1.0, 0.0, 0.0]);
        idx.add(VertexId(1), vec![0.0, 1.0, 0.0]);
        idx.add(VertexId(2), vec![0.9, 0.1, 0.0]);
        idx.add(VertexId(3), vec![0.0, 0.0, 1.0]);

        let results = idx.search(&[1.0, 0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, VertexId(0));
        assert_eq!(results[1].0, VertexId(2));
        assert!(idx.estimated_heap_bytes() > empty);
    }

    #[test]
    fn vector_index_exact_search_is_available_as_oracle() {
        let mut idx = VectorIndex::with_hnsw_params(2, 8, 32, 32);
        for i in 0..256u64 {
            let angle = i as f32 * 0.01;
            idx.add(VertexId(i), vec![angle.cos(), angle.sin()]);
        }

        let exact = idx.search_exact(&[1.0, 0.0], 5);
        let ann = idx.search_hnsw(&[1.0, 0.0], 5);
        assert_eq!(exact[0].0, ann[0].0);
        assert_eq!(exact[0].0, VertexId(0));
    }

    #[test]
    fn vector_index_builds_hnsw_links() {
        let mut idx = VectorIndex::with_hnsw_params(2, 4, 16, 16);
        for i in 0..200u64 {
            idx.add(VertexId(i), vec![i as f32, 1.0]);
        }

        assert_eq!(idx.len(), 200);
        assert!(idx.total_links() > 0);
        assert!(idx.entry_point.is_some());
    }

    #[test]
    fn vector_index_hnsw_recall_matches_exact_on_clustered_data() {
        let mut idx = VectorIndex::with_hnsw_params(4, 12, 48, 96);
        for i in 0..512u64 {
            let cluster = (i % 8) as f32;
            let offset = (i / 8) as f32 * 0.0001;
            idx.add(
                VertexId(i),
                vec![
                    cluster + offset,
                    1.0 - offset,
                    (cluster * 0.5).sin(),
                    (cluster * 0.5).cos(),
                ],
            );
        }

        let query = [3.0, 1.0, 1.5_f32.sin(), 1.5_f32.cos()];
        let exact = idx.search_exact(&query, 10);
        let ann = idx.search(&query, 10);
        let exact_top: Vec<_> = exact.iter().take(3).map(|(id, _)| *id).collect();
        let ann_top: Vec<_> = ann.iter().take(3).map(|(id, _)| *id).collect();

        assert!(
            ann_top.iter().any(|id| exact_top.contains(id)),
            "ANN top results should overlap exact top results; exact={exact_top:?}, ann={ann_top:?}"
        );
    }

    #[test]
    fn vector_index_cosine_distance() {
        let dist = cosine_distance(&[1.0, 0.0], &[1.0, 0.0]);
        assert!(
            dist.abs() < 1e-6,
            "identical vectors should have distance ~0"
        );

        let dist = cosine_distance(&[1.0, 0.0], &[0.0, 1.0]);
        assert!(
            (dist - 1.0).abs() < 1e-6,
            "orthogonal vectors should have distance ~1"
        );

        let dist = cosine_distance(&[1.0, 0.0], &[-1.0, 0.0]);
        assert!(
            (dist - 2.0).abs() < 1e-6,
            "opposite vectors should have distance ~2"
        );
    }

    #[test]
    fn vector_index_search_within_threshold() {
        let mut idx = VectorIndex::new(2);
        idx.add(VertexId(0), vec![1.0, 0.0]);
        idx.add(VertexId(1), vec![0.95, 0.05]);
        idx.add(VertexId(2), vec![0.0, 1.0]);

        let results = idx.search_within(&[1.0, 0.0], 0.01);
        assert!(results.iter().any(|(v, _)| *v == VertexId(0)));
        assert!(!results.iter().any(|(v, _)| *v == VertexId(2)));
    }

    #[test]
    fn vector_index_update_replaces_existing_embedding() {
        let mut idx = VectorIndex::new(2);
        idx.add(VertexId(7), vec![1.0, 0.0]);
        idx.add(VertexId(8), vec![0.0, 1.0]);

        idx.update(VertexId(7), vec![0.0, 0.99]);

        assert_eq!(idx.len(), 2);
        assert_eq!(idx.tombstone_count(), 1);
        let results = idx.search_exact(&[0.0, 1.0], 2);
        assert_eq!(results[0].0, VertexId(7));
        assert_eq!(results[1].0, VertexId(8));
    }

    #[test]
    fn vector_index_remove_hides_embedding_from_all_search_paths() {
        let mut idx = VectorIndex::new(2);
        idx.add(VertexId(1), vec![1.0, 0.0]);
        idx.add(VertexId(2), vec![0.9, 0.1]);
        idx.add(VertexId(3), vec![0.0, 1.0]);

        assert!(idx.remove(VertexId(1)));
        assert!(!idx.remove(VertexId(1)));
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.tombstone_count(), 1);

        assert!(
            !idx.search(&[1.0, 0.0], 3)
                .iter()
                .any(|(id, _)| *id == VertexId(1))
        );
        assert!(
            !idx.search_exact(&[1.0, 0.0], 3)
                .iter()
                .any(|(id, _)| *id == VertexId(1))
        );
        assert!(
            !idx.search_within(&[1.0, 0.0], 0.01)
                .iter()
                .any(|(id, _)| *id == VertexId(1))
        );
    }

    #[test]
    fn vector_index_compact_purges_tombstones_and_preserves_results() {
        let mut idx = VectorIndex::new(2);
        idx.add(VertexId(1), vec![1.0, 0.0]);
        idx.add(VertexId(2), vec![0.9, 0.1]);
        idx.add(VertexId(3), vec![0.0, 1.0]);
        idx.remove(VertexId(1));

        let before = idx.search_exact(&[1.0, 0.0], 2);
        idx.compact();
        let after = idx.search_exact(&[1.0, 0.0], 2);

        assert_eq!(idx.len(), 2);
        assert_eq!(idx.tombstone_count(), 0);
        assert_eq!(before, after);
        assert_eq!(after[0].0, VertexId(2));
    }

    #[test]
    fn vector_index_hnsw_search_filters_deleted_nearest_neighbor() {
        let mut idx = VectorIndex::with_hnsw_params(2, 8, 32, 64);
        for i in 0..256u64 {
            let value = i as f32 / 255.0;
            idx.add(VertexId(i), vec![value, 1.0 - value]);
        }

        assert!(idx.remove(VertexId(0)));

        let results = idx.search(&[0.0, 1.0], 5);
        assert!(!results.iter().any(|(id, _)| *id == VertexId(0)));
        assert_eq!(results[0].0, VertexId(1));
    }

    #[test]
    fn vector_index_snapshot_roundtrip_rebuilds_hnsw_from_active_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.json");
        let mut idx = VectorIndex::with_hnsw_params(2, 8, 32, 64);
        for i in 0..256u64 {
            let value = i as f32 / 255.0;
            idx.add(VertexId(i), vec![value, 1.0 - value]);
        }
        idx.update(VertexId(12), vec![0.0, 1.0]);
        idx.remove(VertexId(0));
        idx.save_to_path(&path).unwrap();

        let loaded = VectorIndex::load_from_path(&path).unwrap();

        assert_eq!(loaded.dimension(), 2);
        assert_eq!(loaded.len(), 255);
        assert_eq!(loaded.tombstone_count(), 0);
        assert!(loaded.entry_point.is_some());
        assert!(loaded.total_links() > 0);
        assert!(
            !loaded
                .search_exact(&[0.0, 1.0], 10)
                .iter()
                .any(|(id, _)| *id == VertexId(0))
        );
        assert_eq!(loaded.search(&[0.0, 1.0], 1)[0].0, VertexId(12));
    }

    #[test]
    fn vector_index_snapshot_load_rejects_wrong_dimension_entry() {
        let snapshot = VectorSnapshot {
            version: VECTOR_SNAPSHOT_VERSION,
            dimension: 3,
            m: 8,
            ef_construction: 32,
            ef_search: 64,
            entries: vec![VectorSnapshotEntry {
                vertex_id: 99,
                embedding: vec![1.0, 0.0],
            }],
        };

        let err = VectorIndex::from_snapshot(snapshot).unwrap_err();

        assert!(matches!(
            err,
            VectorPersistenceError::DimensionMismatch {
                vertex_id: 99,
                expected: 3,
                actual: 2
            }
        ));
    }
}
