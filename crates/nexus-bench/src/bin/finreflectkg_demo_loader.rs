//! Build a durable Domyn Nexus demo data directory from FinReflectKG triplets.
//!
//! This is a product-demo wrapper around the benchmark loader. It writes a
//! graph snapshot plus a small named vector snapshot so the server, Docker
//! Compose demo, or Azure single-node demo can mount a ready-to-query data dir.

use nexus_bench::real_data_loader::{
    build_graph_from_parsed, load_real_data, parse_all_tickers, parse_single_ticker,
};
use nexus_core::types::VertexId;
use nexus_index::vector::VectorIndex;
use nexus_storage::persistence::NexusStore;
use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_VECTOR_INDEX: &str = "entities";
const DEFAULT_VECTOR_DIM: usize = 8;
const DEFAULT_VECTOR_ENTRIES: usize = 256;

#[derive(Debug)]
struct Args {
    triplet_dir: Option<String>,
    data_dir: PathBuf,
    ticker: Option<String>,
    vector_index: String,
    vector_dim: usize,
    vector_entries: usize,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("finreflectkg_demo_loader: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args(env::args().skip(1).collect())?;

    if args.data_dir.exists() {
        return Err(format!(
            "data directory already exists: {}. Refusing to overwrite.",
            args.data_dir.display()
        ));
    }

    let (graph, vertices, edges, tickers, build_ms) = if let Some(ticker) = &args.ticker {
        let parsed = parse_single_ticker(args.triplet_dir.as_deref(), ticker)
            .ok_or_else(|| format!("ticker not found or has no triplets: {ticker}"))?;
        let vertices = parsed.vertices.len();
        let edges = parsed.edges.len();
        let (graph, build_ms) = build_graph_from_parsed(&parsed.vertices, &parsed.edges);
        (graph, vertices, edges, 1, build_ms)
    } else if args.triplet_dir.is_some() {
        let parsed = parse_all_tickers(args.triplet_dir.as_deref());
        if parsed.is_empty() {
            return Err("no ticker triplets found".into());
        }
        let mut vertices = Vec::new();
        let mut edges = Vec::new();
        for ticker in parsed {
            vertices.extend(ticker.vertices);
            edges.extend(ticker.edges);
        }
        let ticker_count = vertices
            .iter()
            .map(|(_, _, _, ticker, _)| ticker.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len();
        let (graph, build_ms) = build_graph_from_parsed(&vertices, &edges);
        (graph, vertices.len(), edges.len(), ticker_count, build_ms)
    } else {
        let loaded = load_real_data(None);
        let vertices = loaded.stats.vertices;
        let edges = loaded.stats.edges;
        let tickers = loaded.stats.tickers;
        let build_ms = loaded.stats.build_time_ms;
        (loaded.graph, vertices, edges, tickers, build_ms)
    };

    let mut store = NexusStore::open(&args.data_dir).map_err(|err| err.to_string())?;
    store
        .save_snapshot(&graph)
        .map_err(|err| format!("failed to save graph snapshot: {err}"))?;

    let vector_entries = args.vector_entries.min(graph.num_vertices());
    let mut vectors = VectorIndex::new(args.vector_dim);
    for raw in 0..vector_entries {
        vectors.add(
            VertexId(raw as u64),
            deterministic_embedding(raw as u64, args.vector_dim),
        );
    }
    store
        .save_vector_index(&args.vector_index, &vectors)
        .map_err(|err| format!("failed to save vector snapshot: {err}"))?;

    let metrics = store
        .metrics()
        .map_err(|err| format!("failed to read storage metrics: {err}"))?;

    println!("finreflectkg_demo_loader: ok");
    println!("data_dir={}", args.data_dir.display());
    println!("vertices={vertices}");
    println!("edges={edges}");
    println!("tickers={tickers}");
    println!("graph_build_ms={build_ms:.1}");
    println!("vector_index={}", args.vector_index);
    println!("vector_dim={}", args.vector_dim);
    println!("vector_entries={vector_entries}");
    println!("snapshot_bytes={}", metrics.snapshot_bytes);
    println!("snapshot_archive_count={}", metrics.snapshot_archive_count);
    println!("vector_snapshot_count={}", metrics.vector_snapshot_count);
    println!("vector_snapshot_bytes={}", metrics.vector_snapshot_bytes);

    Ok(())
}

fn parse_args(raw: Vec<String>) -> Result<Args, String> {
    let mut args = Args {
        triplet_dir: None,
        data_dir: PathBuf::from("/tmp/domyn-nexus-finreflect-demo"),
        ticker: None,
        vector_index: DEFAULT_VECTOR_INDEX.into(),
        vector_dim: DEFAULT_VECTOR_DIM,
        vector_entries: DEFAULT_VECTOR_ENTRIES,
    };

    let mut iter = raw.into_iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--triplet-dir" => {
                args.triplet_dir = Some(next_arg(&mut iter, "--triplet-dir")?);
            }
            "--data-dir" => {
                args.data_dir = PathBuf::from(next_arg(&mut iter, "--data-dir")?);
            }
            "--ticker" => {
                args.ticker = Some(next_arg(&mut iter, "--ticker")?);
            }
            "--vector-index" => {
                args.vector_index = next_arg(&mut iter, "--vector-index")?;
            }
            "--vector-dim" => {
                args.vector_dim = next_arg(&mut iter, "--vector-dim")?
                    .parse()
                    .map_err(|_| "--vector-dim must be a positive integer".to_string())?;
            }
            "--vector-entries" => {
                args.vector_entries = next_arg(&mut iter, "--vector-entries")?
                    .parse()
                    .map_err(|_| "--vector-entries must be a non-negative integer".to_string())?;
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if args.vector_dim == 0 {
        return Err("--vector-dim must be greater than zero".into());
    }

    Ok(args)
}

fn next_arg(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn deterministic_embedding(vertex_id: u64, dimension: usize) -> Vec<f32> {
    (0..dimension)
        .map(|i| {
            let mixed = vertex_id
                .wrapping_mul(1_103_515_245)
                .wrapping_add((i as u64 + 1).wrapping_mul(12_345));
            ((mixed % 10_000) as f32 / 10_000.0) - 0.5
        })
        .collect()
}

fn print_help() {
    println!(
        "Usage: cargo run -p nexus-bench --bin finreflectkg_demo_loader -- \\
         [--triplet-dir <dir>] [--data-dir <dir>] [--ticker <ticker>] \\
         [--vector-index <name>] [--vector-dim <n>] [--vector-entries <n>]\n\
\n\
Builds a durable Nexus data directory from FinReflectKG triplet JSON files.\n\
If --ticker is omitted, all tickers under --triplet-dir are loaded.\n\
If --triplet-dir is omitted, the benchmark loader's local default path is used."
    );
}
