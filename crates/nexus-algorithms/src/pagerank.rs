//! PageRank via arithmetic semiring (plus, times) over the adjacency matrix.
//!
//! Instead of Pregel-style vertex programs (JanusGraph's FulgoraGraphComputer),
//! this runs iterative SpMV: rank_new = damping * (A^T * (rank / degree)) + (1 - damping) / N

use nexus_core::csr::CsrMatrix;

pub struct PageRankResult {
    pub ranks: Vec<(u64, f64)>,
    pub iterations: usize,
    pub converged: bool,
}

/// Run PageRank on a CSR matrix.
///
/// Uses the arithmetic semiring: rank contributions are multiplied along edges
/// and summed across incoming paths.
pub fn pagerank(
    matrix: &CsrMatrix,
    damping: f64,
    max_iterations: usize,
    tolerance: f64,
) -> PageRankResult {
    let n = matrix.num_vertices();
    if n == 0 {
        return PageRankResult {
            ranks: Vec::new(),
            iterations: 0,
            converged: true,
        };
    }

    let base_rank = (1.0 - damping) / n as f64;
    let mut ranks = vec![1.0 / n as f64; n];
    let mut converged = false;
    let mut iterations = 0;

    for iter in 0..max_iterations {
        let mut new_ranks = vec![base_rank; n];

        // For each vertex, distribute its rank equally to neighbors
        for src in 0..n {
            let neighbors = matrix.neighbors_of(src as u64);
            let degree = neighbors.len();
            if degree == 0 {
                // Dangling node: distribute rank equally to all vertices
                let contribution = damping * ranks[src] / n as f64;
                for r in new_ranks.iter_mut() {
                    *r += contribution;
                }
            } else {
                let contribution = damping * ranks[src] / degree as f64;
                for &neighbor in neighbors {
                    new_ranks[neighbor as usize] += contribution;
                }
            }
        }

        // Check convergence (L1 norm of difference)
        let diff: f64 = ranks
            .iter()
            .zip(new_ranks.iter())
            .map(|(old, new)| (old - new).abs())
            .sum();

        ranks = new_ranks;
        iterations = iter + 1;

        if diff < tolerance {
            converged = true;
            break;
        }
    }

    let mut result: Vec<(u64, f64)> = ranks
        .into_iter()
        .enumerate()
        .map(|(i, r)| (i as u64, r))
        .collect();
    result.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    PageRankResult {
        ranks: result,
        iterations,
        converged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::csr::CsrBuilder;

    #[test]
    fn pagerank_simple_chain() {
        // 0 -> 1 -> 2 -> 3
        let mut b = CsrBuilder::new(4);
        b.add_edge(0, 1, 0);
        b.add_edge(1, 2, 1);
        b.add_edge(2, 3, 2);
        let m = b.build();

        let result = pagerank(&m, 0.85, 100, 1e-6);
        assert!(result.converged);
        // Vertex 3 (end of chain) should have highest rank
        assert_eq!(result.ranks[0].0, 3);
    }

    #[test]
    fn pagerank_cycle() {
        // 0 -> 1 -> 2 -> 0 (cycle: all should have equal rank)
        let mut b = CsrBuilder::new(3);
        b.add_edge(0, 1, 0);
        b.add_edge(1, 2, 1);
        b.add_edge(2, 0, 2);
        let m = b.build();

        let result = pagerank(&m, 0.85, 100, 1e-6);
        assert!(result.converged);
        let expected = 1.0 / 3.0;
        for &(_, rank) in &result.ranks {
            assert!((rank - expected).abs() < 0.01);
        }
    }
}
