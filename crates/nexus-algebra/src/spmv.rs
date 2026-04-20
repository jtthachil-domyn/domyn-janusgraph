//! Sparse Matrix-Vector Multiply (SpMV) -- the core traversal primitive.
//!
//! A single SpMV over a CSR matrix is equivalent to a 1-hop graph traversal.
//! Chaining SpMV operations gives multi-hop traversal with linear cost growth.
//!
//! `result = A * v`
//!
//! Where:
//! - A is a CSR adjacency matrix (one per edge label)
//! - v is a sparse vector of active vertices
//! - result is the new frontier (reachable vertices after one hop)
//!
//! The semiring determines how values combine:
//! - Boolean (OR, AND): reachability
//! - Tropical (min, plus): shortest path
//! - Arithmetic (plus, times): PageRank
//!
//! Design influence: FalkorDB's GraphBLAS execution model.

use crate::semiring::Semiring;
use crate::sparse_vector::SparseVector;
use nexus_core::csr::CsrMatrix;

/// Perform SpMV: `result = A * input` using the given semiring.
///
/// For each active vertex in `input`, we iterate its neighbors in the CSR matrix
/// and combine values using the semiring's multiply (along edges) and add (across paths).
pub fn spmv<S: Semiring>(
    matrix: &CsrMatrix,
    input: &SparseVector<S::Value>,
    semiring: &S,
) -> SparseVector<S::Value> {
    let mut result = SparseVector::with_capacity(input.dimension(), input.nnz() * 4);

    for (&src, src_val) in input.iter() {
        let neighbors = matrix.neighbors_of(src);
        for &neighbor in neighbors {
            let edge_contribution = semiring.multiply(src_val, &semiring.one());

            if let Some(existing) = result.get(neighbor) {
                let combined = semiring.add(existing, &edge_contribution);
                result.set(neighbor, combined);
            } else {
                result.set(neighbor, edge_contribution);
            }
        }
    }

    result
}

/// Masked SpMV: only produces output for vertices NOT in the mask.
/// Used for BFS to avoid revisiting already-visited vertices.
pub fn spmv_masked<S: Semiring>(
    matrix: &CsrMatrix,
    input: &SparseVector<S::Value>,
    mask: &SparseVector<S::Value>,
    semiring: &S,
) -> SparseVector<S::Value> {
    let mut result = SparseVector::with_capacity(input.dimension(), input.nnz() * 4);

    for (&src, src_val) in input.iter() {
        let neighbors = matrix.neighbors_of(src);
        for &neighbor in neighbors {
            if mask.contains(neighbor) {
                continue;
            }

            let edge_contribution = semiring.multiply(src_val, &semiring.one());

            if let Some(existing) = result.get(neighbor) {
                let combined = semiring.add(existing, &edge_contribution);
                result.set(neighbor, combined);
            } else {
                result.set(neighbor, edge_contribution);
            }
        }
    }

    result
}

/// Multi-hop traversal: chain `hops` SpMV operations.
/// Returns the frontier after the final hop.
///
/// This is the key insight from FalkorDB: k-hop traversal is k SpMV operations
/// with linear cost growth, not exponential pointer-chasing.
pub fn multi_hop<S: Semiring>(
    matrix: &CsrMatrix,
    start: &SparseVector<S::Value>,
    hops: usize,
    semiring: &S,
) -> SparseVector<S::Value> {
    let mut current = start.clone();
    for _ in 0..hops {
        current = spmv(matrix, &current, semiring);
        if current.is_empty() {
            break;
        }
    }
    current
}

/// BFS using masked SpMV. Returns all reachable vertices within `max_depth` hops
/// with their discovery depth.
pub fn bfs(matrix: &CsrMatrix, start_vertex: u64, max_depth: u32) -> SparseVector<u32> {
    let n = matrix.num_vertices();
    let mut visited = SparseVector::<u32>::new(n);
    visited.set(start_vertex, 0);

    let mut frontier = SparseVector::<bool>::singleton(n, start_vertex, true);

    for depth in 1..=max_depth {
        let mut next_frontier = SparseVector::<bool>::with_capacity(n, frontier.nnz() * 4);

        for (&src, _) in frontier.iter() {
            for &neighbor in matrix.neighbors_of(src) {
                if !visited.contains(neighbor) {
                    visited.set(neighbor, depth);
                    next_frontier.set(neighbor, true);
                }
            }
        }

        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }

    visited
}

/// Collect all vertices reachable within `max_depth` hops, respecting a node budget.
pub fn bounded_traversal(
    matrix: &CsrMatrix,
    start_vertex: u64,
    max_depth: u32,
    max_nodes: usize,
) -> Vec<u64> {
    let mut visited = Vec::with_capacity(max_nodes);
    let mut seen = SparseVector::<bool>::new(matrix.num_vertices());

    seen.set(start_vertex, true);
    visited.push(start_vertex);

    let mut frontier = vec![start_vertex];

    for _depth in 0..max_depth {
        if visited.len() >= max_nodes || frontier.is_empty() {
            break;
        }

        let mut next_frontier = Vec::new();

        for &src in &frontier {
            for &neighbor in matrix.neighbors_of(src) {
                if visited.len() >= max_nodes {
                    break;
                }
                if !seen.contains(neighbor) {
                    seen.set(neighbor, true);
                    visited.push(neighbor);
                    next_frontier.push(neighbor);
                }
            }
        }

        frontier = next_frontier;
    }

    visited
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semiring::BooleanSemiring;
    use nexus_core::csr::CsrBuilder;

    fn diamond_graph() -> CsrMatrix {
        //   0 -> 1
        //   0 -> 2
        //   1 -> 3
        //   2 -> 3
        let mut b = CsrBuilder::new(4);
        b.add_edge(0, 1, 0);
        b.add_edge(0, 2, 1);
        b.add_edge(1, 3, 2);
        b.add_edge(2, 3, 3);
        b.build()
    }

    #[test]
    fn spmv_one_hop_reachability() {
        let m = diamond_graph();
        let sr = BooleanSemiring;
        let input = SparseVector::singleton(4, 0, true);

        let result = spmv(&m, &input, &sr);
        assert_eq!(result.nnz(), 2);
        assert!(result.contains(1));
        assert!(result.contains(2));
        assert!(!result.contains(3));
    }

    #[test]
    fn spmv_two_hop_reachability() {
        let m = diamond_graph();
        let sr = BooleanSemiring;
        let start = SparseVector::singleton(4, 0, true);

        let result = multi_hop(&m, &start, 2, &sr);
        assert!(result.contains(3));
    }

    #[test]
    fn bfs_diamond() {
        let m = diamond_graph();
        let result = bfs(&m, 0, 3);

        assert_eq!(result.get(0), Some(&0)); // start
        assert_eq!(result.get(1), Some(&1)); // 1 hop
        assert_eq!(result.get(2), Some(&1)); // 1 hop
        assert_eq!(result.get(3), Some(&2)); // 2 hops
    }

    #[test]
    fn bounded_traversal_respects_budget() {
        let m = diamond_graph();
        let result = bounded_traversal(&m, 0, 10, 3);
        assert_eq!(result.len(), 3);
        assert!(result.contains(&0));
    }
}
