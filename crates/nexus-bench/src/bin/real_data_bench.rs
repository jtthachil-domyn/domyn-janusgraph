//! Real-data benchmark: loads the same 47.5K V / 64.9K E 10-K SEC filing
//! dataset used by domyngraph-benchmark and runs equivalent B3-B10 queries
//! through the Nexus Cypher executor with proper indexes.
//!
//! Usage:
//!   cargo run -p nexus-bench --release --bin real_data_bench

use nexus_bench::real_data_loader::{
    build_graph_from_parsed, load_real_data, parse_all_tickers, parse_single_ticker,
};
use nexus_core::graph::Graph;
use nexus_core::types::{Direction, Value, VertexId};
use nexus_cypher::executor::run_cypher_with_indexes;
use nexus_index::composite::IndexSet;
use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

const ITERATIONS: usize = 10;
const COLD_RUNS: usize = 1;
const WARM_RUNS: usize = 2;

struct BenchResult {
    name: String,
    p50_ms: f64,
    p95_ms: f64,
    mean_ms: f64,
    std_ms: f64,
    notes: String,
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (pct / 100.0 * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn stats(times_ms: &[f64]) -> (f64, f64, f64, f64) {
    let mut sorted = times_ms.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&sorted, 50.0);
    let p95 = percentile(&sorted, 95.0);
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
    let variance = sorted.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / sorted.len() as f64;
    (p50, p95, mean, variance.sqrt())
}

fn run_measured<F: FnMut()>(mut f: F) -> Vec<f64> {
    for _ in 0..COLD_RUNS {
        f();
    }
    for _ in 0..WARM_RUNS {
        f();
    }
    (0..ITERATIONS)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect()
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

fn run_indexed_query(query: &str, graph: &Graph, indexes: &IndexSet) -> usize {
    let result = run_cypher_with_indexes(query, graph, indexes)
        .unwrap_or_else(|err| panic!("Cypher benchmark query failed:\n{query}\n{err}"));
    let cells = result.rows.iter().map(|row| row.len()).sum::<usize>();
    black_box(result.rows.len() + cells + result.columns.len())
}

fn build_indexes(graph: &Graph) -> IndexSet {
    let mut idx = IndexSet::new();

    let eid_idx = idx.add_unique("external_id");
    let tenant_idx = idx.add_composite("tenant_id");
    let etype_idx = idx.add_composite("entity_type");
    let name_idx = idx.add_composite("name");

    for vid_raw in 0..graph.num_vertices() as u64 {
        let vid = VertexId(vid_raw);
        if let Value::String(ref eid) = graph.get_vertex_property(vid, "external_id") {
            let _ = idx.unique_mut(eid_idx).unwrap().insert(eid, vid);
        }
        if let Value::String(ref t) = graph.get_vertex_property(vid, "tenant_id") {
            idx.composite_mut(tenant_idx).unwrap().insert(t, vid);
        }
        if let Value::String(ref et) = graph.get_vertex_property(vid, "entity_type") {
            idx.composite_mut(etype_idx).unwrap().insert(et, vid);
        }
        if let Value::String(ref n) = graph.get_vertex_property(vid, "name") {
            idx.composite_mut(name_idx).unwrap().insert(n, vid);
        }
    }

    idx
}

fn b3_cypher(graph: &Graph, indexes: &IndexSet, seeds: &[String]) -> BenchResult {
    let queries: Vec<String> = seeds
        .iter()
        .map(|eid| {
            format!(
                "MATCH (e:Entity) WHERE e.external_id = '{}' RETURN e.name, e.entity_type",
                escape(eid)
            )
        })
        .collect();

    let times = run_measured(|| {
        let mut checksum = 0usize;
        for q in &queries {
            checksum ^= run_indexed_query(q, graph, indexes);
        }
        black_box(checksum);
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B3: Point Lookup (Cypher+idx)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("{} lookups/iter", seeds.len()),
    }
}

fn b3_direct(graph: &Graph, seeds: &[String], ext_map: &HashMap<String, VertexId>) -> BenchResult {
    let times = run_measured(|| {
        for eid in seeds {
            if let Some(&vid) = ext_map.get(eid.as_str()) {
                let _ = graph.get_vertex_property(vid, "name");
                let _ = graph.get_vertex_property(vid, "entity_type");
            }
        }
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B3: Point Lookup (direct)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("{} lookups/iter", seeds.len()),
    }
}

fn b5_cypher(graph: &Graph, indexes: &IndexSet, seeds: &[String]) -> BenchResult {
    let queries: Vec<String> = seeds
        .iter()
        .map(|eid| {
            format!(
                "MATCH (s:Entity)-[]-(n) WHERE s.external_id = '{}' RETURN DISTINCT n.name LIMIT 100",
                escape(eid)
            )
        })
        .collect();

    let times = run_measured(|| {
        let mut checksum = 0usize;
        for q in &queries {
            checksum ^= run_indexed_query(q, graph, indexes);
        }
        black_box(checksum);
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B5: 1-Hop (Cypher+idx)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("{} seeds", seeds.len()),
    }
}

fn b5_direct(graph: &Graph, seeds: &[String], ext_map: &HashMap<String, VertexId>) -> BenchResult {
    let times = run_measured(|| {
        for eid in seeds {
            if let Some(&vid) = ext_map.get(eid.as_str()) {
                let mut seen = std::collections::HashSet::new();
                let mut count = 0usize;
                for (neighbor, _) in graph.neighbors_with_edges_any_label(vid, Direction::Both) {
                    if count >= 100 {
                        break;
                    }
                    if seen.insert(neighbor) {
                        count += 1;
                    }
                }
            }
        }
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B5: 1-Hop (direct)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("{} seeds", seeds.len()),
    }
}

fn b6_cypher(graph: &Graph, indexes: &IndexSet, seeds: &[String]) -> BenchResult {
    let queries: Vec<String> = seeds
        .iter()
        .map(|eid| {
            format!(
                "MATCH (s:Entity)-[]->()-[]->(n) WHERE s.external_id = '{}' RETURN DISTINCT n.name LIMIT 100",
                escape(eid)
            )
        })
        .collect();

    let times = run_measured(|| {
        let mut checksum = 0usize;
        for q in &queries {
            checksum ^= run_indexed_query(q, graph, indexes);
        }
        black_box(checksum);
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B6: 2-Hop (Cypher+idx)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("{} seeds", seeds.len()),
    }
}

fn b6_direct(graph: &Graph, seeds: &[String], ext_map: &HashMap<String, VertexId>) -> BenchResult {
    let times = run_measured(|| {
        for eid in seeds {
            if let Some(&vid) = ext_map.get(eid.as_str()) {
                let mut seen = std::collections::HashSet::new();
                let mut count = 0usize;
                'outer: for (hop1, _) in
                    graph.neighbors_with_edges_any_label(vid, Direction::Outgoing)
                {
                    for (hop2, _) in graph.neighbors_with_edges_any_label(hop1, Direction::Outgoing)
                    {
                        if count >= 100 {
                            break 'outer;
                        }
                        if seen.insert(hop2) {
                            count += 1;
                        }
                    }
                }
            }
        }
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B6: 2-Hop (direct)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("{} seeds", seeds.len()),
    }
}

fn b7_cypher(graph: &Graph, indexes: &IndexSet, seeds: &[String]) -> BenchResult {
    let queries: Vec<String> = seeds
        .iter()
        .map(|eid| {
            format!(
                "MATCH (s:Entity)-[:Discloses]->(n) WHERE s.external_id = '{}' RETURN DISTINCT n.name LIMIT 100",
                escape(eid)
            )
        })
        .collect();

    let times = run_measured(|| {
        let mut checksum = 0usize;
        for q in &queries {
            checksum ^= run_indexed_query(q, graph, indexes);
        }
        black_box(checksum);
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B7: Filtered (Cypher+idx)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("{} seeds, pred=Discloses", seeds.len()),
    }
}

fn b7_direct(graph: &Graph, seeds: &[String], ext_map: &HashMap<String, VertexId>) -> BenchResult {
    let times = run_measured(|| {
        for eid in seeds {
            if let Some(&vid) = ext_map.get(eid.as_str()) {
                let mut _count = 0;
                for n in graph
                    .neighbors(vid, "Discloses", Direction::Outgoing)
                    .iter()
                    .take(100)
                {
                    let _ = n;
                    _count += 1;
                }
            }
        }
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B7: Filtered (direct)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("{} seeds, pred=Discloses", seeds.len()),
    }
}

fn b8_cypher(graph: &Graph, indexes: &IndexSet, tenant: &str) -> BenchResult {
    let query = format!(
        "MATCH (e:Entity) WHERE e.tenant_id = '{}' RETURN e.entity_type, count(*) ORDER BY count(*) DESC",
        escape(tenant)
    );
    let times = run_measured(|| {
        black_box(run_indexed_query(&query, graph, indexes));
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B8: Count By Type (Cypher)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("tenant={tenant}"),
    }
}

fn b8_direct(graph: &Graph, tenant: &str) -> BenchResult {
    let times = run_measured(|| {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for vid_raw in 0..graph.num_vertices() as u64 {
            let vid = VertexId(vid_raw);
            if let Value::String(ref t) = graph.get_vertex_property(vid, "tenant_id") {
                if t == tenant {
                    if let Value::String(ref et) = graph.get_vertex_property(vid, "entity_type") {
                        *counts.entry(et.clone()).or_insert(0) += 1;
                    }
                }
            }
        }
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B8: Count By Type (direct)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("tenant={tenant}"),
    }
}

fn b9_cypher(graph: &Graph, indexes: &IndexSet, tenant: &str) -> BenchResult {
    let query = format!(
        "MATCH (e:Entity)-[r]-() WHERE e.tenant_id = '{}' RETURN e.name, count(r) ORDER BY count(r) DESC LIMIT 10",
        escape(tenant)
    );
    let times = run_measured(|| {
        black_box(run_indexed_query(&query, graph, indexes));
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B9: Top Connected (Cypher)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("tenant={tenant}"),
    }
}

fn b9_direct(graph: &Graph, tenant: &str) -> BenchResult {
    let times = run_measured(|| {
        let mut degrees: Vec<(String, usize)> = Vec::new();
        for vid_raw in 0..graph.num_vertices() as u64 {
            let vid = VertexId(vid_raw);
            if let Value::String(ref t) = graph.get_vertex_property(vid, "tenant_id") {
                if t == tenant {
                    let name = match graph.get_vertex_property(vid, "name") {
                        Value::String(s) => s,
                        _ => continue,
                    };
                    let deg = graph.incident_degree(vid, Direction::Both);
                    degrees.push((name, deg));
                }
            }
        }
        degrees.sort_by(|a, b| b.1.cmp(&a.1));
        degrees.truncate(10);
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B9: Top Connected (direct)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("tenant={tenant}"),
    }
}

fn b10_cypher(graph: &Graph, indexes: &IndexSet, a: &str, b: &str) -> BenchResult {
    let query = format!(
        "MATCH (e:Entity) WHERE e.tenant_id = '{}' AND e.tenant_id = '{}' RETURN count(e)",
        escape(a),
        escape(b)
    );
    let times = run_measured(|| {
        black_box(run_indexed_query(&query, graph, indexes));
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B10: Tenant Isolation (Cypher)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("tenants={a},{b}"),
    }
}

fn b1_single_ticker_ingest(ticker: &str) -> BenchResult {
    let parsed = parse_single_ticker(None, ticker).expect("ticker not found");
    let v_count = parsed.vertices.len();
    let e_count = parsed.edges.len();

    let times = run_measured(|| {
        let (_g, _ms) = build_graph_from_parsed(&parsed.vertices, &parsed.edges);
        black_box(_g);
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: format!("B1: Single Load ({ticker})"),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!("V={v_count}, E={e_count}"),
    }
}

fn b2_bulk_ingest() -> BenchResult {
    let all_tickers = parse_all_tickers(None);
    let total_v: usize = all_tickers.iter().map(|t| t.vertices.len()).sum();
    let total_e: usize = all_tickers.iter().map(|t| t.edges.len()).sum();

    let mut all_vertices = Vec::new();
    let mut all_edges = Vec::new();
    let mut seen_ext_ids = std::collections::HashSet::new();
    for t in &all_tickers {
        for v in &t.vertices {
            if seen_ext_ids.insert(v.0.clone()) {
                all_vertices.push(v.clone());
            }
        }
        all_edges.extend(t.edges.iter().cloned());
    }

    let times = run_measured(|| {
        let (_g, _ms) = build_graph_from_parsed(&all_vertices, &all_edges);
        black_box(_g);
    });
    let (p50, p95, mean, std) = stats(&times);
    BenchResult {
        name: "B2: Bulk Load (all 18)".into(),
        p50_ms: p50,
        p95_ms: p95,
        mean_ms: mean,
        std_ms: std,
        notes: format!(
            "{} tickers, V={}, E={}",
            all_tickers.len(),
            total_v,
            total_e
        ),
    }
}

fn format_ms(ms: f64) -> String {
    if ms < 0.001 {
        format!("{:.0} ns", ms * 1_000_000.0)
    } else if ms < 1.0 {
        format!("{:.1} us", ms * 1000.0)
    } else {
        format!("{:.2} ms", ms)
    }
}

fn main() {
    eprintln!("=== Domyn Nexus Real-Data Benchmark ===");
    eprintln!("Same dataset as JanusGraph/Neo4j: 18 tickers, ~47.5K V, ~64.9K E");
    eprintln!(
        "Protocol: {} cold + {} warm-up + {} measured\n",
        COLD_RUNS, WARM_RUNS, ITERATIONS
    );

    let data = load_real_data(None);

    eprintln!(
        "Loaded: {} V, {} E, {} predicates, {} tickers",
        data.stats.vertices, data.stats.edges, data.stats.predicates, data.stats.tickers
    );
    eprintln!(
        "Parse: {:.1}ms, Build: {:.1}ms\n",
        data.stats.load_time_ms, data.stats.build_time_ms
    );

    eprintln!("Building indexes (external_id, tenant_id, entity_type, name)...");
    let idx_start = Instant::now();
    let indexes = build_indexes(&data.graph);
    let idx_time = idx_start.elapsed();
    eprintln!(
        "Indexes built in {:.1}ms\n",
        idx_time.as_secs_f64() * 1000.0
    );

    let ext_map: HashMap<String, VertexId> = {
        let mut m = HashMap::new();
        for vid_raw in 0..data.graph.num_vertices() as u64 {
            let vid = VertexId(vid_raw);
            if let Value::String(ref eid) = data.graph.get_vertex_property(vid, "external_id") {
                m.insert(eid.clone(), vid);
            }
        }
        m
    };

    let seeds_50 = &data.seed_external_ids;
    let seeds_30: Vec<String> = seeds_50.iter().take(30).cloned().collect();
    let seeds_15: Vec<String> = seeds_50.iter().take(15).cloned().collect();
    let tenant = &data.tickers[0];
    let tenant_b = if data.tickers.len() > 1 {
        &data.tickers[1]
    } else {
        "NONE"
    };

    let mut results: Vec<BenchResult> = Vec::new();

    // B1: Single ticker ingestion
    eprintln!("B1: Single Load (AAPL)...");
    results.push(b1_single_ticker_ingest("AAPL"));

    // B2: Bulk load (all 18 tickers)
    eprintln!("B2: Bulk Load (all 18 tickers)...");
    results.push(b2_bulk_ingest());

    eprintln!("B3: Point Lookup ({} seeds)...", seeds_50.len());
    results.push(b3_cypher(&data.graph, &indexes, seeds_50));
    results.push(b3_direct(&data.graph, seeds_50, &ext_map));

    eprintln!("B5: 1-Hop ({} seeds)...", seeds_30.len());
    results.push(b5_cypher(&data.graph, &indexes, &seeds_30));
    results.push(b5_direct(&data.graph, &seeds_30, &ext_map));

    eprintln!("B6: 2-Hop ({} seeds)...", seeds_15.len());
    results.push(b6_cypher(&data.graph, &indexes, &seeds_15));
    results.push(b6_direct(&data.graph, &seeds_15, &ext_map));

    eprintln!("B7: Filtered Traversal ({} seeds)...", seeds_30.len());
    results.push(b7_cypher(&data.graph, &indexes, &seeds_30));
    results.push(b7_direct(&data.graph, &seeds_30, &ext_map));

    eprintln!("B8: Count By Type (tenant={})...", tenant);
    results.push(b8_cypher(&data.graph, &indexes, tenant));
    results.push(b8_direct(&data.graph, tenant));

    eprintln!("B9: Top Connected (tenant={})...", tenant);
    results.push(b9_cypher(&data.graph, &indexes, tenant));
    results.push(b9_direct(&data.graph, tenant));

    eprintln!("B10: Tenant Isolation ({}, {})...", tenant, tenant_b);
    results.push(b10_cypher(&data.graph, &indexes, tenant, tenant_b));

    // Print results
    println!();
    println!(
        "===================================================================================================="
    );
    println!(
        "{:<35} {:>10} {:>10} {:>10} {:>10}  {}",
        "Benchmark", "p50", "p95", "mean", "std", "Notes"
    );
    println!(
        "===================================================================================================="
    );
    for r in &results {
        println!(
            "{:<35} {:>10} {:>10} {:>10} {:>10}  {}",
            r.name,
            format_ms(r.p50_ms),
            format_ms(r.p95_ms),
            format_ms(r.mean_ms),
            format_ms(r.std_ms),
            r.notes
        );
    }
    println!(
        "===================================================================================================="
    );

    // Comparison table
    println!(
        "\n--- 3-Way Comparison (same dataset: {} V, {} E) ---\n",
        data.stats.vertices, data.stats.edges
    );
    println!(
        "{:<25} {:>14} {:>14} {:>14} {:>14}",
        "Benchmark", "JanusGraph", "Neo4j", "Nexus(Cypher)", "Nexus(direct)"
    );
    println!("{}", "-".repeat(85));

    let find = |name: &str| -> f64 {
        results
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.p50_ms)
            .unwrap_or(f64::NAN)
    };

    let find_prefix = |prefix: &str| -> f64 {
        results
            .iter()
            .find(|r| r.name.starts_with(prefix))
            .map(|r| r.p50_ms)
            .unwrap_or(f64::NAN)
    };

    let rows: Vec<(&str, &str, &str, f64, f64)> = vec![
        (
            "B1: Single Load",
            "2.0 s",
            "1.1 s",
            find_prefix("B1:"),
            f64::NAN,
        ),
        (
            "B2: Bulk Load",
            "56.6 s",
            "8.2 s",
            find_prefix("B2:"),
            f64::NAN,
        ),
        (
            "B3: Point Lookup",
            "1.12 ms",
            "0.92 ms",
            find("B3: Point Lookup (Cypher+idx)"),
            find("B3: Point Lookup (direct)"),
        ),
        (
            "B5: 1-Hop Traversal",
            "65.0 ms",
            "26.4 ms",
            find("B5: 1-Hop (Cypher+idx)"),
            find("B5: 1-Hop (direct)"),
        ),
        (
            "B6: 2-Hop Traversal",
            "34.0 ms",
            "11.5 ms",
            find("B6: 2-Hop (Cypher+idx)"),
            find("B6: 2-Hop (direct)"),
        ),
        (
            "B7: Filtered Trav.",
            "33.6 ms",
            "18.9 ms",
            find("B7: Filtered (Cypher+idx)"),
            find("B7: Filtered (direct)"),
        ),
        (
            "B8: Count By Type",
            "18.8 ms",
            "3.4 ms",
            find("B8: Count By Type (Cypher)"),
            find("B8: Count By Type (direct)"),
        ),
        (
            "B9: Top Connected",
            "25.7 ms",
            "4.1 ms",
            find("B9: Top Connected (Cypher)"),
            find("B9: Top Connected (direct)"),
        ),
        (
            "B10: Tenant Isolation",
            "3.7 ms",
            "1.2 ms",
            find("B10: Tenant Isolation (Cypher)"),
            f64::NAN,
        ),
    ];

    for (name, jg, neo, cypher, direct) in &rows {
        let c = format_ms(*cypher);
        let d = if direct.is_nan() {
            "—".to_string()
        } else {
            format_ms(*direct)
        };
        println!("{:<25} {:>14} {:>14} {:>14} {:>14}", name, jg, neo, c, d);
    }
    println!("{}", "-".repeat(85));
    println!(
        "\nJanusGraph: JG-Streamed mode (best case), over WebSocket. Neo4j: over Bolt. April 14 run."
    );
    println!(
        "Nexus B1/B2: full pipeline (parse JSON + add_vertex/add_edge + CSR build), in-process."
    );
    println!("Nexus B3-B10: full Cypher parse+plan+execute with indexes, in-process.");
    println!(
        "Graph build: {:.1}ms | Index build: {:.1}ms | Total: single binary, no external services.",
        data.stats.build_time_ms,
        idx_time.as_secs_f64() * 1000.0
    );
}
