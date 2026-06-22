//! Synthetic GraphRAG retrieval benchmark.
//!
//! This is the NornicDB-style hybrid lane: vector search followed by bounded
//! graph expansion in the same process. It complements `real_data_bench`, which
//! is focused on the B1-B10 graph-only workload.
//!
//! Fast default:
//!   cargo run -p nexus-bench --bin graphrag_retrieval_bench
//!   cargo run -p nexus-bench --bin graphrag_retrieval_bench -- --assert-internal-beta
//!
//! Larger release run:
//!   GRAPH_RAG_CHUNKS=8192 GRAPH_RAG_ENTITIES=1024 GRAPH_RAG_DIM=64 \
//!   GRAPH_RAG_QUERIES=64 GRAPH_RAG_ITERATIONS=20 \
//!   cargo run -p nexus-bench --release --bin graphrag_retrieval_bench

use nexus_core::graph::Graph;
use nexus_core::properties::PropertyType;
use nexus_core::types::{Direction, Value};
use nexus_index::vector::VectorIndex;
use std::collections::HashSet;
use std::env;
use std::hint::black_box;
use std::process::ExitCode;
use std::time::Instant;

const DEFAULT_MIN_RECALL_AT_K: f64 = 0.50;

#[derive(Debug, Clone)]
struct BenchConfig {
    chunks: usize,
    entities: usize,
    dim: usize,
    k: usize,
    queries: usize,
    iterations: usize,
    warm_runs: usize,
    hnsw_m: usize,
    hnsw_ef_construction: usize,
    hnsw_ef_search: usize,
}

impl BenchConfig {
    fn from_env() -> Self {
        Self {
            chunks: env_usize("GRAPH_RAG_CHUNKS", 1_024).max(1),
            entities: env_usize("GRAPH_RAG_ENTITIES", 256).max(1),
            dim: env_usize("GRAPH_RAG_DIM", 32).max(1),
            k: env_usize("GRAPH_RAG_K", 10).max(1),
            queries: env_usize("GRAPH_RAG_QUERIES", 16).max(1),
            iterations: env_usize("GRAPH_RAG_ITERATIONS", 5).max(1),
            warm_runs: env_usize("GRAPH_RAG_WARM_RUNS", 1),
            hnsw_m: env_usize("GRAPH_RAG_HNSW_M", 8).max(2),
            hnsw_ef_construction: env_usize("GRAPH_RAG_HNSW_EF_CONSTRUCTION", 32).max(2),
            hnsw_ef_search: env_usize("GRAPH_RAG_HNSW_EF_SEARCH", 48).max(2),
        }
    }
}

struct BenchResult {
    name: &'static str,
    p50_us: f64,
    p95_us: f64,
    mean_us: f64,
    notes: String,
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (pct / 100.0 * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn stats(times_us: &[f64]) -> (f64, f64, f64) {
    let mut sorted = times_us.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&sorted, 50.0);
    let p95 = percentile(&sorted, 95.0);
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
    (p50, p95, mean)
}

fn run_measured<F: FnMut()>(cfg: &BenchConfig, mut f: F) -> Vec<f64> {
    for _ in 0..cfg.warm_runs {
        f();
    }
    (0..cfg.iterations)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64() * 1_000_000.0
        })
        .collect()
}

fn embedding(seed: usize, dim: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(dim);
    for i in 0..dim {
        let raw = ((seed as u64)
            .wrapping_mul(1_103_515_245)
            .wrapping_add((i as u64 + 1) * 12_345)
            % 10_000) as f32;
        out.push((raw / 10_000.0) - 0.5);
    }
    let norm = out.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
    for v in &mut out {
        *v /= norm;
    }
    out
}

fn build_graph_and_vectors(cfg: &BenchConfig) -> (Graph, VectorIndex, Vec<Vec<f32>>) {
    let mut graph = Graph::new(cfg.chunks + cfg.entities, cfg.chunks * 2);
    graph.register_vertex_property("kind", PropertyType::String, true, false);
    graph.register_vertex_property("name", PropertyType::String, true, false);
    graph.register_vertex_property("tenant_id", PropertyType::String, true, false);
    graph.register_edge_property("rank", PropertyType::Int64, false, false);

    let mut chunks = Vec::with_capacity(cfg.chunks);
    for i in 0..cfg.chunks {
        let v = graph.add_vertex("Chunk");
        graph.set_vertex_property(v, "kind", Value::String("chunk".into()));
        graph.set_vertex_property(v, "name", Value::String(format!("chunk-{i}")));
        graph.set_vertex_property(v, "tenant_id", Value::String(format!("T{}", i % 8)));
        chunks.push(v);
    }

    let mut entities = Vec::with_capacity(cfg.entities);
    for i in 0..cfg.entities {
        let v = graph.add_vertex("Entity");
        graph.set_vertex_property(v, "kind", Value::String("entity".into()));
        graph.set_vertex_property(v, "name", Value::String(format!("entity-{i}")));
        graph.set_vertex_property(v, "tenant_id", Value::String(format!("T{}", i % 8)));
        entities.push(v);
    }

    for i in 0..cfg.chunks {
        let first = entities[(i * 17) % cfg.entities];
        let second = entities[(i * 31 + 7) % cfg.entities];
        let e1 = graph.add_edge(chunks[i], first, "MENTIONS");
        graph.set_edge_property(e1, "rank", Value::Int64(1));
        let e2 = graph.add_edge(chunks[i], second, "MENTIONS");
        graph.set_edge_property(e2, "rank", Value::Int64(2));
    }
    graph.build();

    let mut index = VectorIndex::with_hnsw_params(
        cfg.dim,
        cfg.hnsw_m,
        cfg.hnsw_ef_construction,
        cfg.hnsw_ef_search,
    );
    for (i, chunk) in chunks.iter().enumerate() {
        index.add(*chunk, embedding(i, cfg.dim));
    }

    let queries = (0..cfg.queries)
        .map(|i| embedding((i * 97) % cfg.chunks, cfg.dim))
        .collect();

    (graph, index, queries)
}

fn recall_at_k(index: &VectorIndex, queries: &[Vec<f32>], k: usize) -> f64 {
    let mut total = 0.0;
    for query in queries {
        let ann: HashSet<_> = index.search(query, k).into_iter().map(|(v, _)| v).collect();
        let exact: HashSet<_> = index
            .search_exact(query, k)
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        let overlap = ann.intersection(&exact).count();
        total += overlap as f64 / k as f64;
    }
    total / queries.len() as f64
}

fn bench_vector_only(index: &VectorIndex, queries: &[Vec<f32>], cfg: &BenchConfig) -> BenchResult {
    let times = run_measured(cfg, || {
        let mut checksum = 0usize;
        for query in queries {
            checksum ^= index.search(query, cfg.k).len();
        }
        black_box(checksum);
    });
    let (p50, p95, mean) = stats(&times);
    BenchResult {
        name: "Vector only",
        p50_us: p50,
        p95_us: p95,
        mean_us: mean,
        notes: format!("{} queries, k={}", queries.len(), cfg.k),
    }
}

fn bench_vector_plus_one_hop(
    graph: &Graph,
    index: &VectorIndex,
    queries: &[Vec<f32>],
    cfg: &BenchConfig,
) -> BenchResult {
    let times = run_measured(cfg, || {
        let mut checksum = 0usize;
        for query in queries {
            for (chunk, _) in index.search(query, cfg.k) {
                let neighbors = graph.neighbors(chunk, "MENTIONS", Direction::Outgoing);
                checksum ^= neighbors.len();
                for entity in neighbors {
                    if let Value::String(name) = graph.get_vertex_property(entity, "name") {
                        checksum ^= name.len();
                    }
                }
            }
        }
        black_box(checksum);
    });
    let (p50, p95, mean) = stats(&times);
    BenchResult {
        name: "Vector + 1 hop",
        p50_us: p50,
        p95_us: p95,
        mean_us: mean,
        notes: format!("{} queries, k={}", queries.len(), cfg.k),
    }
}

fn bench_vector_plus_two_hop(
    graph: &Graph,
    index: &VectorIndex,
    queries: &[Vec<f32>],
    cfg: &BenchConfig,
) -> BenchResult {
    let times = run_measured(cfg, || {
        let mut checksum = 0usize;
        for query in queries {
            let mut seen = HashSet::new();
            for (chunk, _) in index.search(query, cfg.k) {
                for entity in graph.neighbors(chunk, "MENTIONS", Direction::Outgoing) {
                    for (related_chunk, _) in
                        graph.neighbors_with_edges_any_label(entity, Direction::Incoming)
                    {
                        if seen.len() >= cfg.k * 4 {
                            break;
                        }
                        if related_chunk != chunk && seen.insert(related_chunk) {
                            checksum ^= related_chunk.0 as usize;
                        }
                    }
                }
            }
        }
        black_box(checksum);
    });
    let (p50, p95, mean) = stats(&times);
    BenchResult {
        name: "Vector + 2 hop",
        p50_us: p50,
        p95_us: p95,
        mean_us: mean,
        notes: format!("{} queries, bounded expansion", queries.len()),
    }
}

fn print_result(result: &BenchResult) {
    println!(
        "{:<18} {:>10.1} {:>10.1} {:>10.1}  {}",
        result.name, result.p50_us, result.p95_us, result.mean_us, result.notes
    );
}

fn main() -> ExitCode {
    let args = BenchArgs::parse();
    let cfg = BenchConfig::from_env();
    eprintln!(
        "Building synthetic GraphRAG graph: {} chunks, {} entities, dim={}",
        cfg.chunks, cfg.entities, cfg.dim
    );
    let started = Instant::now();
    let (graph, index, queries) = build_graph_and_vectors(&cfg);
    let build_ms = started.elapsed().as_secs_f64() * 1000.0;
    let recall = recall_at_k(&index, &queries, cfg.k);

    let vector_only = bench_vector_only(&index, &queries, &cfg);
    let vector_one = bench_vector_plus_one_hop(&graph, &index, &queries, &cfg);
    let vector_two = bench_vector_plus_two_hop(&graph, &index, &queries, &cfg);

    println!();
    println!("Synthetic GraphRAG retrieval benchmark");
    println!(
        "Build: {:.1}ms | vertices={} | edges={} | vector_recall@{}={:.3}",
        build_ms,
        graph.num_vertices(),
        graph.num_edges(),
        cfg.k,
        recall
    );
    println!(
        "{:<18} {:>10} {:>10} {:>10}  Notes",
        "Benchmark", "p50 us", "p95 us", "mean us"
    );
    println!("{}", "-".repeat(76));
    print_result(&vector_only);
    print_result(&vector_one);
    print_result(&vector_two);

    if args.assert_internal_beta {
        let min_recall = env_f64("GRAPH_RAG_MIN_RECALL_AT_K", DEFAULT_MIN_RECALL_AT_K);
        if recall < min_recall {
            eprintln!(
                "Internal Beta GraphRAG threshold: FAIL\n- vector_recall@{} {:.3} below minimum {:.3}",
                cfg.k, recall, min_recall
            );
            return ExitCode::FAILURE;
        }
        if ![&vector_only, &vector_one, &vector_two]
            .iter()
            .all(|result| result.p50_us.is_finite() && result.p95_us.is_finite())
        {
            eprintln!(
                "Internal Beta GraphRAG threshold: FAIL\n- benchmark produced non-finite latency"
            );
            return ExitCode::FAILURE;
        }
        println!(
            "\nInternal Beta GraphRAG thresholds: PASS (vector_recall@{} {:.3} >= {:.3})",
            cfg.k, recall, min_recall
        );
    }

    ExitCode::SUCCESS
}

#[derive(Debug, Default)]
struct BenchArgs {
    assert_internal_beta: bool,
}

impl BenchArgs {
    fn parse() -> Self {
        let mut args = Self::default();
        for arg in env::args().skip(1) {
            match arg.as_str() {
                "--assert-internal-beta" => args.assert_internal_beta = true,
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    print_help();
                    std::process::exit(2);
                }
            }
        }
        args
    }
}

fn env_f64(name: &str, default: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn print_help() {
    println!(
        "Usage: graphrag_retrieval_bench [--assert-internal-beta]\n\n\
         --assert-internal-beta  fail if recall/latency sanity checks regress\n\n\
         Environment:\n\
           GRAPH_RAG_MIN_RECALL_AT_K  minimum recall for assertion mode, default {DEFAULT_MIN_RECALL_AT_K}"
    );
}
