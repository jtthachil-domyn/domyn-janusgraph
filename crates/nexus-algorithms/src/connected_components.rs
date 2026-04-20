//! Connected components via min-min semiring.
//!
//! Each vertex starts with its own ID as its component label.
//! Iterative SpMV propagates the minimum label to neighbors until convergence.

use nexus_core::csr::CsrMatrix;

pub struct ComponentsResult {
    /// (vertex_id, component_id) sorted by component_id.
    pub assignments: Vec<(u64, u64)>,
    pub num_components: usize,
    pub iterations: usize,
}

/// Find connected components using label propagation.
/// Treats the graph as undirected by using the forward CSR matrix.
/// For proper undirected behavior, pass a matrix that includes both directions.
pub fn connected_components(matrix: &CsrMatrix, max_iterations: usize) -> ComponentsResult {
    let n = matrix.num_vertices();
    if n == 0 {
        return ComponentsResult {
            assignments: Vec::new(),
            num_components: 0,
            iterations: 0,
        };
    }

    let mut labels: Vec<u64> = (0..n as u64).collect();
    let mut iterations = 0;

    for iter in 0..max_iterations {
        let mut changed = false;

        for v in 0..n {
            let my_label = labels[v];
            for &neighbor in matrix.neighbors_of(v as u64) {
                let neighbor_label = labels[neighbor as usize];
                if neighbor_label < my_label {
                    labels[v] = neighbor_label;
                    changed = true;
                }
            }
        }

        // Also propagate backwards (for directed graphs treated as undirected)
        for v in 0..n {
            for &neighbor in matrix.neighbors_of(v as u64) {
                let my_label = labels[v];
                let neighbor_idx = neighbor as usize;
                if my_label < labels[neighbor_idx] {
                    labels[neighbor_idx] = my_label;
                    changed = true;
                }
            }
        }

        iterations = iter + 1;
        if !changed {
            break;
        }
    }

    let mut unique_labels: Vec<u64> = labels.clone();
    unique_labels.sort_unstable();
    unique_labels.dedup();
    let num_components = unique_labels.len();

    let mut assignments: Vec<(u64, u64)> = labels
        .into_iter()
        .enumerate()
        .map(|(v, c)| (v as u64, c))
        .collect();
    assignments.sort_by_key(|&(_, c)| c);

    ComponentsResult {
        assignments,
        num_components,
        iterations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::csr::CsrBuilder;

    #[test]
    fn two_components() {
        // Component 1: 0 -> 1, 1 -> 2
        // Component 2: 3 -> 4
        let mut b = CsrBuilder::new(5);
        b.add_edge(0, 1, 0);
        b.add_edge(1, 2, 1);
        b.add_edge(3, 4, 2);
        let m = b.build();

        let result = connected_components(&m, 100);
        assert_eq!(result.num_components, 2);

        let comp_of = |v: u64| -> u64 {
            result
                .assignments
                .iter()
                .find(|&&(id, _)| id == v)
                .unwrap()
                .1
        };
        assert_eq!(comp_of(0), comp_of(1));
        assert_eq!(comp_of(0), comp_of(2));
        assert_eq!(comp_of(3), comp_of(4));
        assert_ne!(comp_of(0), comp_of(3));
    }

    #[test]
    fn single_component() {
        // 0 -> 1 -> 2 -> 0
        let mut b = CsrBuilder::new(3);
        b.add_edge(0, 1, 0);
        b.add_edge(1, 2, 1);
        b.add_edge(2, 0, 2);
        let m = b.build();

        let result = connected_components(&m, 100);
        assert_eq!(result.num_components, 1);
    }
}
