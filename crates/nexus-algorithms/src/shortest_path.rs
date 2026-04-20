//! Shortest path via tropical semiring (min, plus).
//!
//! The tropical semiring accumulates edge weights along paths (plus)
//! and selects the minimum across alternative paths (min).

use nexus_core::csr::CsrMatrix;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

pub struct ShortestPathResult {
    /// Distances from source to each reachable vertex.
    pub distances: Vec<(u64, f64)>,
    /// Predecessor map for path reconstruction.
    pub predecessors: Vec<(u64, Option<u64>)>,
}

/// Dijkstra's shortest path (unweighted -- all edges have cost 1).
/// For weighted graphs, edge weights would come from the edge property store.
pub fn shortest_path_unweighted(matrix: &CsrMatrix, source: u64) -> ShortestPathResult {
    let n = matrix.num_vertices();
    let mut dist = vec![f64::INFINITY; n];
    let mut pred: Vec<Option<u64>> = vec![None; n];

    if (source as usize) >= n {
        return ShortestPathResult {
            distances: Vec::new(),
            predecessors: Vec::new(),
        };
    }

    dist[source as usize] = 0.0;

    // BinaryHeap with Reverse for min-heap behavior
    let mut heap: BinaryHeap<Reverse<(u64, u64)>> = BinaryHeap::new();
    heap.push(Reverse((0, source)));

    while let Some(Reverse((cost_bits, u))) = heap.pop() {
        let cost = f64::from_bits(cost_bits);
        let u_idx = u as usize;

        if cost > dist[u_idx] {
            continue;
        }

        for &neighbor in matrix.neighbors_of(u) {
            let n_idx = neighbor as usize;
            let new_cost = cost + 1.0;
            if new_cost < dist[n_idx] {
                dist[n_idx] = new_cost;
                pred[n_idx] = Some(u);
                heap.push(Reverse((new_cost.to_bits(), neighbor)));
            }
        }
    }

    let distances: Vec<(u64, f64)> = dist
        .iter()
        .enumerate()
        .filter(|(_, d)| d.is_finite())
        .map(|(i, &d)| (i as u64, d))
        .collect();

    let predecessors: Vec<(u64, Option<u64>)> = pred
        .into_iter()
        .enumerate()
        .filter(|(i, _)| dist[*i].is_finite())
        .map(|(i, p)| (i as u64, p))
        .collect();

    ShortestPathResult {
        distances,
        predecessors,
    }
}

/// Reconstruct the path from source to target using the predecessor map.
pub fn reconstruct_path(
    predecessors: &[(u64, Option<u64>)],
    source: u64,
    target: u64,
) -> Option<Vec<u64>> {
    let pred_map: std::collections::HashMap<u64, Option<u64>> =
        predecessors.iter().map(|&(v, p)| (v, p)).collect();

    if !pred_map.contains_key(&target) {
        return None;
    }

    let mut path = vec![target];
    let mut current = target;

    while current != source {
        match pred_map.get(&current) {
            Some(Some(prev)) => {
                path.push(*prev);
                current = *prev;
            }
            _ => return None,
        }
    }

    path.reverse();
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::csr::CsrBuilder;

    #[test]
    fn shortest_path_chain() {
        // 0 -> 1 -> 2 -> 3
        let mut b = CsrBuilder::new(4);
        b.add_edge(0, 1, 0);
        b.add_edge(1, 2, 1);
        b.add_edge(2, 3, 2);
        let m = b.build();

        let result = shortest_path_unweighted(&m, 0);

        let dist_of =
            |v: u64| -> f64 { result.distances.iter().find(|&&(id, _)| id == v).unwrap().1 };

        assert_eq!(dist_of(0), 0.0);
        assert_eq!(dist_of(1), 1.0);
        assert_eq!(dist_of(2), 2.0);
        assert_eq!(dist_of(3), 3.0);

        let path = reconstruct_path(&result.predecessors, 0, 3).unwrap();
        assert_eq!(path, vec![0, 1, 2, 3]);
    }

    #[test]
    fn shortest_path_diamond() {
        // 0 -> 1 -> 3 (cost 2)
        // 0 -> 2 -> 3 (cost 2)
        let mut b = CsrBuilder::new(4);
        b.add_edge(0, 1, 0);
        b.add_edge(0, 2, 1);
        b.add_edge(1, 3, 2);
        b.add_edge(2, 3, 3);
        let m = b.build();

        let result = shortest_path_unweighted(&m, 0);
        let dist_of =
            |v: u64| -> f64 { result.distances.iter().find(|&&(id, _)| id == v).unwrap().1 };

        assert_eq!(dist_of(3), 2.0);
    }

    #[test]
    fn unreachable_vertex() {
        // 0 -> 1, vertex 2 is disconnected
        let mut b = CsrBuilder::new(3);
        b.add_edge(0, 1, 0);
        let m = b.build();

        let result = shortest_path_unweighted(&m, 0);
        assert!(!result.distances.iter().any(|&(id, _)| id == 2));
    }
}
