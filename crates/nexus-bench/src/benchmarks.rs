//! B1-B13 benchmark implementations for Domyn Nexus.
//!
//! Each benchmark measures a specific workload pattern:
//!   B1:  Single-ticker ingestion
//!   B2:  Bulk multi-ticker ingestion
//!   B3:  Point lookup by external_id
//!   B4:  Full-text search
//!   B5:  1-hop traversal (DISCLOSES)
//!   B6:  2-hop traversal (DISCLOSES -> HAS_COMPONENT)
//!   B7:  Multi-hop variable-length path
//!   B8:  Aggregation (count, sum)
//!   B9:  Filtered traversal with predicate
//!   B10: Bounded subgraph extraction
//!   B11: Memory footprint measurement
//!   B12: Index verification
//!   B13: Graph statistics

use crate::graph_builder::BenchGraph;
use nexus_core::types::{Direction, VertexId};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct BenchResult {
    pub name: String,
    pub duration_ms: f64,
    pub rows_returned: usize,
    pub passed: bool,
    pub notes: String,
}

impl BenchResult {
    fn ok(name: &str, duration: std::time::Duration, rows: usize) -> Self {
        Self {
            name: name.into(),
            duration_ms: duration.as_secs_f64() * 1000.0,
            rows_returned: rows,
            passed: true,
            notes: String::new(),
        }
    }

    fn fail(name: &str, msg: &str) -> Self {
        Self {
            name: name.into(),
            duration_ms: 0.0,
            rows_returned: 0,
            passed: false,
            notes: msg.into(),
        }
    }
}

/// Run all benchmarks against a pre-built graph.
pub fn run_all(bg: &BenchGraph) -> Vec<BenchResult> {
    vec![
        b3_point_lookup(bg),
        b5_one_hop_traversal(bg),
        b6_two_hop_traversal(bg),
        b7_variable_length_path(bg),
        b8_aggregation(bg),
        b9_filtered_traversal(bg),
        b10_bounded_subgraph(bg),
        b11_memory_footprint(bg),
        b12_index_verification(bg),
        b13_graph_statistics(bg),
    ]
}

/// B3: Point lookup -- find a vertex by property value.
pub fn b3_point_lookup(bg: &BenchGraph) -> BenchResult {
    let start = Instant::now();
    let mut found = 0;

    for vid_raw in 0..bg.graph.num_vertices() as u64 {
        let vid = VertexId(vid_raw);
        if let Some(ticker) = bg.graph.get_vertex_property(vid, "ticker").as_str() {
            if ticker == "AAPL" {
                found += 1;
            }
        }
    }

    BenchResult::ok("B3: Point lookup (ticker=AAPL)", start.elapsed(), found)
}

/// B5: 1-hop traversal -- follow DISCLOSES edges from companies.
pub fn b5_one_hop_traversal(bg: &BenchGraph) -> BenchResult {
    let start = Instant::now();
    let mut total_neighbors = 0;

    for &company in &bg.companies {
        let neighbors = bg
            .graph
            .neighbors(company, "DISCLOSES", Direction::Outgoing);
        total_neighbors += neighbors.len();
    }

    BenchResult::ok(
        "B5: 1-hop traversal (DISCLOSES)",
        start.elapsed(),
        total_neighbors,
    )
}

/// B6: 2-hop traversal -- DISCLOSES -> HAS_COMPONENT.
pub fn b6_two_hop_traversal(bg: &BenchGraph) -> BenchResult {
    let start = Instant::now();
    let mut total = 0;

    for &company in &bg.companies {
        let metrics = bg
            .graph
            .neighbors(company, "DISCLOSES", Direction::Outgoing);
        for metric in metrics {
            let components = bg
                .graph
                .neighbors(metric, "HAS_COMPONENT", Direction::Outgoing);
            total += components.len();
        }
    }

    BenchResult::ok(
        "B6: 2-hop traversal (DISCLOSES->HAS_COMPONENT)",
        start.elapsed(),
        total,
    )
}

/// B7: Variable-length path -- explore up to 3 hops.
pub fn b7_variable_length_path(bg: &BenchGraph) -> BenchResult {
    let start = Instant::now();
    let mut total = 0;

    for &company in bg.companies.iter().take(10) {
        let mut frontier = vec![company];
        let mut visited = std::collections::HashSet::new();
        visited.insert(company);

        for _ in 0..3 {
            let mut next_frontier = Vec::new();
            for &v in &frontier {
                for label in bg.graph.edge_label_names() {
                    for n in bg.graph.neighbors(v, &label, Direction::Outgoing) {
                        if visited.insert(n) {
                            next_frontier.push(n);
                        }
                    }
                }
            }
            frontier = next_frontier;
        }
        total += visited.len();
    }

    BenchResult::ok("B7: Variable-length path (3 hops)", start.elapsed(), total)
}

/// B8: Aggregation -- count vertices by label.
pub fn b8_aggregation(bg: &BenchGraph) -> BenchResult {
    let start = Instant::now();
    let mut label_counts = std::collections::HashMap::new();

    for vid_raw in 0..bg.graph.num_vertices() as u64 {
        if let Some(label) = bg.graph.vertex_label(VertexId(vid_raw)) {
            *label_counts.entry(label.to_string()).or_insert(0usize) += 1;
        }
    }

    let total: usize = label_counts.values().sum();
    BenchResult::ok("B8: Aggregation (count by label)", start.elapsed(), total)
}

/// B9: Filtered traversal -- find metrics with value > threshold.
pub fn b9_filtered_traversal(bg: &BenchGraph) -> BenchResult {
    let start = Instant::now();
    let threshold = 200.0;
    let mut found = 0;

    for &company in &bg.companies {
        for metric in bg
            .graph
            .neighbors(company, "DISCLOSES", Direction::Outgoing)
        {
            if let Some(val) = bg.graph.get_vertex_property(metric, "value").as_f64() {
                if val > threshold {
                    found += 1;
                }
            }
        }
    }

    BenchResult::ok(
        "B9: Filtered traversal (value > 200)",
        start.elapsed(),
        found,
    )
}

/// B10: Bounded subgraph extraction -- extract up to N nodes from a start vertex.
pub fn b10_bounded_subgraph(bg: &BenchGraph) -> BenchResult {
    if bg.companies.is_empty() {
        return BenchResult::fail("B10: Bounded subgraph", "no companies");
    }

    let start = Instant::now();
    let budget = 50;
    let root = bg.companies[0];

    let mut visited = std::collections::HashSet::new();
    visited.insert(root);
    let mut frontier = vec![root];

    while visited.len() < budget && !frontier.is_empty() {
        let mut next = Vec::new();
        for &v in &frontier {
            for label in bg.graph.edge_label_names() {
                for n in bg.graph.neighbors(v, &label, Direction::Both) {
                    if visited.len() >= budget {
                        break;
                    }
                    if visited.insert(n) {
                        next.push(n);
                    }
                }
            }
        }
        frontier = next;
    }

    BenchResult::ok(
        "B10: Bounded subgraph (budget=50)",
        start.elapsed(),
        visited.len(),
    )
}

/// B11: Memory footprint measurement.
pub fn b11_memory_footprint(bg: &BenchGraph) -> BenchResult {
    let vertices = bg.graph.num_vertices();
    let edges = bg.graph.num_edges();

    let est_bytes = vertices * 64 + (edges as usize) * 24;

    BenchResult {
        name: "B11: Memory footprint".into(),
        duration_ms: 0.0,
        rows_returned: est_bytes,
        passed: true,
        notes: format!(
            "~{} bytes for {} vertices, {} edges",
            est_bytes, vertices, edges
        ),
    }
}

/// B12: Index verification -- ensure all companies have external_id set.
pub fn b12_index_verification(bg: &BenchGraph) -> BenchResult {
    let start = Instant::now();
    let mut all_have_id = true;
    let mut checked = 0;

    for &company in &bg.companies {
        checked += 1;
        if bg
            .graph
            .get_vertex_property(company, "external_id")
            .is_null()
        {
            all_have_id = false;
            break;
        }
    }

    BenchResult {
        name: "B12: Index verification".into(),
        duration_ms: start.elapsed().as_secs_f64() * 1000.0,
        rows_returned: checked,
        passed: all_have_id,
        notes: if all_have_id {
            "all companies have external_id".into()
        } else {
            "some companies missing external_id".into()
        },
    }
}

/// B13: Graph statistics.
pub fn b13_graph_statistics(bg: &BenchGraph) -> BenchResult {
    let start = Instant::now();

    let vertices = bg.graph.num_vertices();
    let edges = bg.graph.num_edges();
    let vertex_labels = bg.graph.vertex_label_names();
    let edge_labels = bg.graph.edge_label_names();

    BenchResult {
        name: "B13: Graph statistics".into(),
        duration_ms: start.elapsed().as_secs_f64() * 1000.0,
        rows_returned: vertices,
        passed: true,
        notes: format!(
            "V={}, E={}, vertex_labels={:?}, edge_labels={:?}",
            vertices, edges, vertex_labels, edge_labels
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_builder::build_bench_graph;

    fn bench_graph() -> BenchGraph {
        build_bench_graph(20, 5, 2, 5, 10)
    }

    #[test]
    fn run_all_benchmarks() {
        let bg = bench_graph();
        let results = run_all(&bg);

        for r in &results {
            println!(
                "{}: {:.2}ms, rows={}, passed={} {}",
                r.name, r.duration_ms, r.rows_returned, r.passed, r.notes
            );
            assert!(r.passed, "{} failed: {}", r.name, r.notes);
        }
    }

    #[test]
    fn b3_finds_aapl() {
        let bg = bench_graph();
        let r = b3_point_lookup(&bg);
        assert!(r.passed);
        assert!(r.rows_returned > 0);
    }

    #[test]
    fn b5_traversal_nonzero() {
        let bg = bench_graph();
        let r = b5_one_hop_traversal(&bg);
        assert!(r.passed);
        assert!(r.rows_returned > 0);
    }

    #[test]
    fn b6_two_hop_nonzero() {
        let bg = bench_graph();
        let r = b6_two_hop_traversal(&bg);
        assert!(r.passed);
        assert!(r.rows_returned > 0);
    }

    #[test]
    fn b13_stats_valid() {
        let bg = bench_graph();
        let r = b13_graph_statistics(&bg);
        assert!(r.passed);
        assert!(r.rows_returned > 0);
    }
}
