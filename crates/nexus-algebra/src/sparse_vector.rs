//! Sparse vector representation for graph traversal.
//!
//! A sparse vector represents a set of "active" vertices with associated values.
//! For boolean reachability, the value is just `true`. For BFS, it's the distance.
//! For PageRank, it's the rank contribution.

use std::collections::HashMap;

/// A sparse vector: only stores entries for non-zero (active) vertices.
#[derive(Debug, Clone)]
pub struct SparseVector<V: Clone> {
    /// Map from vertex index to value.
    entries: HashMap<u64, V>,
    /// Total dimension (number of vertices in the graph).
    dimension: usize,
}

impl<V: Clone> SparseVector<V> {
    pub fn new(dimension: usize) -> Self {
        Self {
            entries: HashMap::new(),
            dimension,
        }
    }

    pub fn with_capacity(dimension: usize, capacity: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity),
            dimension,
        }
    }

    /// Create a sparse vector with a single active vertex.
    pub fn singleton(dimension: usize, index: u64, value: V) -> Self {
        let mut v = Self::new(dimension);
        v.set(index, value);
        v
    }

    pub fn set(&mut self, index: u64, value: V) {
        self.entries.insert(index, value);
    }

    pub fn get(&self, index: u64) -> Option<&V> {
        self.entries.get(&index)
    }

    pub fn contains(&self, index: u64) -> bool {
        self.entries.contains_key(&index)
    }

    pub fn remove(&mut self, index: u64) {
        self.entries.remove(&index);
    }

    pub fn nnz(&self) -> usize {
        self.entries.len()
    }

    pub fn dimension(&self) -> usize {
        self.dimension
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&u64, &V)> {
        self.entries.iter()
    }

    pub fn indices(&self) -> impl Iterator<Item = u64> + '_ {
        self.entries.keys().copied()
    }

    pub fn into_entries(self) -> HashMap<u64, V> {
        self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_vector_basic() {
        let mut v = SparseVector::<bool>::new(100);
        assert!(v.is_empty());

        v.set(5, true);
        v.set(42, true);

        assert_eq!(v.nnz(), 2);
        assert!(v.contains(5));
        assert!(v.contains(42));
        assert!(!v.contains(0));
    }

    #[test]
    fn singleton() {
        let v = SparseVector::singleton(1000, 7, 1.0f64);
        assert_eq!(v.nnz(), 1);
        assert_eq!(v.get(7), Some(&1.0));
    }
}
