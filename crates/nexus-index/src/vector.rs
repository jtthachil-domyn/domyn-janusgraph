//! HNSW vector index for vertex embeddings (GenAI-native).
//!
//! Stores dense embedding vectors on vertices and supports approximate
//! nearest-neighbor search. Replaces the need for an external vector DB
//! in the GraphRAG pipeline.
//!
//! Implementation uses a flat brute-force index initially (correct baseline),
//! with HNSW to be added for production-scale ANN queries.

use nexus_core::types::VertexId;

/// A single vector entry: vertex ID + embedding.
#[derive(Debug, Clone)]
struct VectorEntry {
    vertex_id: u64,
    embedding: Vec<f32>,
}

/// Brute-force vector index (exact nearest-neighbor).
/// Serves as the correct baseline before HNSW optimization.
pub struct VectorIndex {
    entries: Vec<VectorEntry>,
    dimension: usize,
}

impl VectorIndex {
    pub fn new(dimension: usize) -> Self {
        Self {
            entries: Vec::new(),
            dimension,
        }
    }

    pub fn dimension(&self) -> usize {
        self.dimension
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
        self.entries.push(VectorEntry {
            vertex_id: vertex_id.0,
            embedding,
        });
    }

    /// Find the `k` nearest neighbors to the query vector (brute-force).
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(VertexId, f32)> {
        assert_eq!(query.len(), self.dimension);

        let mut scored: Vec<(u64, f32)> = self
            .entries
            .iter()
            .map(|e| {
                let dist = cosine_distance(query, &e.embedding);
                (e.vertex_id, dist)
            })
            .collect();

        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);

        scored
            .into_iter()
            .map(|(id, dist)| (VertexId(id), dist))
            .collect()
    }

    /// Find vertices whose embeddings are within `threshold` cosine distance.
    pub fn search_within(&self, query: &[f32], threshold: f32) -> Vec<(VertexId, f32)> {
        assert_eq!(query.len(), self.dimension);

        self.entries
            .iter()
            .filter_map(|e| {
                let dist = cosine_distance(query, &e.embedding);
                if dist <= threshold {
                    Some((VertexId(e.vertex_id), dist))
                } else {
                    None
                }
            })
            .collect()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_index_basic_search() {
        let mut idx = VectorIndex::new(3);

        idx.add(VertexId(0), vec![1.0, 0.0, 0.0]);
        idx.add(VertexId(1), vec![0.0, 1.0, 0.0]);
        idx.add(VertexId(2), vec![0.9, 0.1, 0.0]); // close to vertex 0
        idx.add(VertexId(3), vec![0.0, 0.0, 1.0]);

        let results = idx.search(&[1.0, 0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, VertexId(0)); // exact match first
        assert_eq!(results[1].0, VertexId(2)); // closest second
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
        idx.add(VertexId(1), vec![0.95, 0.05]); // very close
        idx.add(VertexId(2), vec![0.0, 1.0]); // far away

        let results = idx.search_within(&[1.0, 0.0], 0.01);
        assert!(results.iter().any(|(v, _)| *v == VertexId(0)));
        assert!(!results.iter().any(|(v, _)| *v == VertexId(2)));
    }
}
