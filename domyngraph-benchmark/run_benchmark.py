#!/usr/bin/env python3
"""DomynGraph Benchmark CLI.

Compares JanusGraph (DomynGraph Engine) vs Neo4j CE on real 10-K KG data.

Usage:
  python run_benchmark.py --mode full                      # ingest + query benchmarks
  python run_benchmark.py --mode query-only                # query benchmarks only
  python run_benchmark.py --mode parse-only                # just parse and print stats

JanusGraph Ingestion Modes:
  python run_benchmark.py --mode full --jg-mode fair       # external_id lookups only
  python run_benchmark.py --mode full --jg-mode optimized  # post-hoc ID cache
  python run_benchmark.py --mode full --jg-mode streamed   # capture IDs during insert (production-optimal)
  python run_benchmark.py --mode full --jg-mode all        # run all 3 modes sequentially (default)

Options:
  --jg-url           JanusGraph websocket URL  (default: ws://localhost:8182/gremlin)
  --neo4j-uri        Neo4j bolt URI            (default: neo4j://127.0.0.1:7687)
  --neo4j-user       Neo4j username            (default: neo4j)
  --neo4j-pass       Neo4j password            (default: neo4jtest123)
  --triplet-dir      Path to triplet JSON root (default: env TRIPLET_DIR)
  --iterations       Measured runs per bench   (default: 10)
  --output-dir       Results output directory  (default: results/)
  --single-ticker    Ticker for single-load B1 (default: first ticker found)
  --jg-mode          JanusGraph ingestion mode (default: all)
"""

from __future__ import annotations

import argparse
import logging
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from benchmark.report import generate_charts, save_results
from benchmark.runner import BenchmarkResult, BenchmarkRunner
from etl.load_janusgraph import JanusGraphLoader
from etl.load_neo4j import Neo4jLoader
from etl.parser import parse_all

logger = logging.getLogger("benchmark")


def setup_logging(verbose: bool = False) -> None:
    level = logging.DEBUG if verbose else logging.INFO
    logging.basicConfig(
        level=level,
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )


def _run_jg_single(jg_loader, single_td, mode_label):
    """Run B1 single-company load for a JanusGraph mode. Returns BenchmarkResult."""
    jg_loader.drop_tenant(single_td.ticker)
    t0 = time.perf_counter()
    jg_vt, jg_et, jg_cache_t = jg_loader.load_ticker(single_td)
    jg_total = time.perf_counter() - t0

    jg_loader.drop_tenant(single_td.ticker)

    total_records = len(single_td.vertices) + len(single_td.edges)
    is_direct = jg_loader.mode in ("optimized", "streamed")
    edge_lookup = "internal_id" if is_direct else "external_id"
    lookups = 0 if is_direct else 2
    tpe = (jg_et * 1000) / len(single_td.edges) if single_td.edges else 0
    cache_strategy = {
        "fair": "none",
        "optimized": "post-hoc query",
        "streamed": "captured during insert",
    }.get(jg_loader.mode, "unknown")

    notes = (
        f"ticker={single_td.ticker}, V={len(single_td.vertices)}, "
        f"E={len(single_td.edges)}, V_time={jg_vt:.2f}s, E_time={jg_et:.2f}s, "
        f"id_cache_time={jg_cache_t:.2f}s, "
        f"edge_lookup={edge_lookup}, lookups_per_edge={lookups}, "
        f"time_per_edge_ms={tpe:.2f}, "
        f"cache_strategy={cache_strategy}"
    )

    result = BenchmarkResult(
        f"B1: Single Load ({mode_label})", "janusgraph", "write", 1, 0,
        times_ms=[jg_total * 1000],
        records_processed=total_records,
        notes=notes,
    )
    result.compute_stats()

    logger.info("B1 JanusGraph [%s]: %.1fs (V=%.1fs, E=%.1fs, cache=%.1fs)",
                mode_label, jg_total, jg_vt, jg_et, jg_cache_t)
    return result


def _run_jg_bulk(jg_loader, parsed, mode_label):
    """Run B2 bulk load for a JanusGraph mode. Returns BenchmarkResult."""
    t0 = time.perf_counter()
    jg_stats = jg_loader.load_all(parsed)
    jg_bulk = time.perf_counter() - t0

    notes = (
        f"V={jg_stats['vertices']}, E={jg_stats['edges']}, "
        f"V_time={jg_stats['vertex_time_s']:.2f}s, "
        f"E_time={jg_stats['edge_time_s']:.2f}s, "
        f"id_cache_time={jg_stats['id_cache_time_s']:.2f}s, "
        f"edge_lookup={jg_stats['edge_lookup']}, "
        f"lookups_per_edge={jg_stats['lookups_per_edge']}, "
        f"time_per_edge_ms={jg_stats['time_per_edge_ms']:.2f}, "
        f"batch_loading={'ON' if jg_stats['batch_loading'] else 'OFF'}"
    )

    result = BenchmarkResult(
        f"B2: Bulk Load ({mode_label})", "janusgraph", "write", 1, 0,
        times_ms=[jg_bulk * 1000],
        records_processed=parsed.vertex_count + parsed.edge_count,
        notes=notes,
    )
    result.compute_stats()

    logger.info("B2 JanusGraph [%s]: %.1fs", mode_label, jg_bulk)
    return result


def run_ingest_benchmarks(
    jg_url: str,
    neo_loader: Neo4jLoader,
    parsed,
    single_ticker: str | None,
    jg_mode: str,
) -> list[BenchmarkResult]:
    """B1: Single company load, B2: Bulk load.

    Runs JanusGraph in fair, optimized, or both modes.
    Neo4j runs once; its results are shared across modes.
    """
    results = []

    tickers = parsed.tickers
    if not tickers:
        logger.error("No tickers found in parsed data")
        return results

    single_td = None
    if single_ticker:
        single_td = next((t for t in tickers if t.ticker == single_ticker), None)
    if not single_td:
        single_td = tickers[0]

    modes_to_run = []
    if jg_mode in ("fair", "all"):
        modes_to_run.append("fair")
    if jg_mode in ("optimized", "all"):
        modes_to_run.append("optimized")
    if jg_mode in ("streamed", "all"):
        modes_to_run.append("streamed")

    # -- Neo4j single load (run once) -------------------------------------
    logger.info("=" * 60)
    logger.info("B1: Single company load (%s)", single_td.ticker)
    logger.info("=" * 60)

    neo_loader.drop_tenant(single_td.ticker)
    t0 = time.perf_counter()
    neo_vt, neo_et = neo_loader.load_ticker(single_td)
    neo_total = time.perf_counter() - t0
    neo_loader.drop_tenant(single_td.ticker)

    b1_neo = BenchmarkResult(
        "B1: Single Load", "neo4j", "write", 1, 0,
        times_ms=[neo_total * 1000],
        records_processed=len(single_td.vertices) + len(single_td.edges),
        notes=(
            f"ticker={single_td.ticker}, V={len(single_td.vertices)}, "
            f"E={len(single_td.edges)}, V_time={neo_vt:.2f}s, E_time={neo_et:.2f}s"
        ),
    )
    b1_neo.compute_stats()
    results.append(b1_neo)
    logger.info("B1 Neo4j: %.1fs (V=%.1fs, E=%.1fs)", neo_total, neo_vt, neo_et)

    # -- JanusGraph single load (per mode) --------------------------------
    for mode in modes_to_run:
        label = f"jg-{mode}"
        jg_loader = JanusGraphLoader(jg_url, mode=mode)
        jg_loader.connect()
        jg_loader.pre_register_schema(parsed.unique_predicates)

        b1_jg = _run_jg_single(jg_loader, single_td, label)
        results.append(b1_jg)
        jg_loader.close()

    # -- Bulk load B2 -----------------------------------------------------
    logger.info("=" * 60)
    logger.info("B2: Bulk load (all %d tickers)", len(tickers))
    logger.info("=" * 60)

    # Neo4j bulk (run once)
    t0 = time.perf_counter()
    neo_stats = neo_loader.load_all(parsed)
    neo_bulk = time.perf_counter() - t0

    b2_neo = BenchmarkResult(
        "B2: Bulk Load", "neo4j", "write", 1, 0,
        times_ms=[neo_bulk * 1000],
        records_processed=parsed.vertex_count + parsed.edge_count,
        notes=f"V={neo_stats['vertices']}, E={neo_stats['edges']}",
    )
    b2_neo.compute_stats()
    results.append(b2_neo)
    logger.info("B2 Neo4j: %.1fs", neo_bulk)

    # JanusGraph bulk (per mode -- requires clean graph between modes)
    for mode in modes_to_run:
        label = f"jg-{mode}"
        jg_loader = JanusGraphLoader(jg_url, mode=mode)
        jg_loader.connect()

        b2_jg = _run_jg_bulk(jg_loader, parsed, label)
        results.append(b2_jg)

        if mode != modes_to_run[-1]:
            logger.info("Cleaning JanusGraph for next mode...")
            jg_loader.drop_all()

        jg_loader.close()

    return results


def _print_summary(all_results: list[BenchmarkResult]) -> None:
    print("\n" + "=" * 88)
    print(f"{'Benchmark':<35} {'Engine':<12} {'p50ms':>8} {'p95ms':>8} {'meanms':>8} {'notes (abbrev)':<30}")
    print("=" * 88)
    for r in all_results:
        if r.category == "meta":
            continue
        abbrev = r.notes[:40] + "..." if len(r.notes) > 40 else r.notes
        print(
            f"{r.name:<35} {r.engine:<12} {r.p50:>8.1f} {r.p95:>8.1f} {r.mean:>8.1f} {abbrev}"
        )
    print("=" * 88)

    write_results = [r for r in all_results if r.category == "write"]
    if write_results:
        print("\n--- Ingestion Detail ---")
        for r in write_results:
            print(f"  {r.name:<35} {r.engine:<12}  {r.mean:.0f}ms  | {r.notes}")
        print()

    jg_advanced = [r for r in write_results
                   if "optimized" in r.name.lower() or "streamed" in r.name.lower()]
    if jg_advanced:
        print("Caveats:")
        print("  - 'fair': external_id index lookups, directly comparable to Neo4j MATCH.")
        print("  - 'optimized': post-hoc query to build ID cache after vertex load.")
        print("    ID cache build time is included in totals (cold pipeline cost).")
        print("  - 'streamed': captures internal IDs during vertex insertion itself.")
        print("    No extra DB round-trip; this is the production-optimal pattern.")
        print("  - Batch sizes tuned to reasonable production defaults per engine.")
        print("  - JanusGraph throughput is also constrained by Cassandra write")
        print("    throughput and compaction behavior.")
        print()


def main():
    parser = argparse.ArgumentParser(description="DomynGraph Benchmark CLI")
    parser.add_argument(
        "--mode",
        choices=["full", "query-only", "parse-only"],
        default="full",
    )
    parser.add_argument("--jg-url", default="ws://localhost:8182/gremlin")
    parser.add_argument("--neo4j-uri", default="neo4j://127.0.0.1:7687")
    parser.add_argument("--neo4j-user", default="neo4j")
    parser.add_argument("--neo4j-pass", default="neo4jtest123")
    parser.add_argument("--triplet-dir", default=None)
    parser.add_argument("--iterations", type=int, default=10)
    parser.add_argument("--output-dir", default="results")
    parser.add_argument("--single-ticker", default=None)
    parser.add_argument(
        "--jg-mode",
        choices=["fair", "optimized", "streamed", "all"],
        default="all",
        help="JanusGraph ingestion mode: fair (external_id lookups), "
             "optimized (post-hoc ID cache), streamed (capture IDs during "
             "insert), or all (default: run all 3).",
    )
    parser.add_argument("--verbose", action="store_true")

    args = parser.parse_args()
    setup_logging(args.verbose)

    logger.info("Parsing triplet data...")
    triplet_dir = Path(args.triplet_dir) if args.triplet_dir else None
    parsed = parse_all(triplet_dir)

    logger.info(
        "Parsed: %d tickers, %d vertices, %d edges, %d predicates",
        len(parsed.tickers), parsed.vertex_count, parsed.edge_count,
        len(parsed.unique_predicates),
    )

    if args.mode == "parse-only":
        print("\nParse Summary:")
        print(f"  Tickers:    {len(parsed.tickers)}")
        print(f"  Vertices:   {parsed.vertex_count}")
        print(f"  Edges:      {parsed.edge_count}")
        print(f"  Predicates: {sorted(parsed.unique_predicates)}")
        for td in parsed.tickers:
            print(f"  {td.ticker}: {len(td.vertices)} V, {len(td.edges)} E")
        return

    out_dir = Path(args.output_dir)
    all_results: list[BenchmarkResult] = []

    if args.mode == "full":
        neo_loader = Neo4jLoader(args.neo4j_uri, args.neo4j_user, args.neo4j_pass)
        neo_loader.connect()

        ingest_results = run_ingest_benchmarks(
            args.jg_url, neo_loader, parsed, args.single_ticker, args.jg_mode,
        )
        all_results.extend(ingest_results)
        neo_loader.close()

    runner = BenchmarkRunner(
        jg_url=args.jg_url,
        neo4j_uri=args.neo4j_uri,
        neo4j_user=args.neo4j_user,
        neo4j_pass=args.neo4j_pass,
        parsed=parsed,
        iterations=args.iterations,
    )
    runner.connect()
    read_results = runner.run_all_read_benchmarks()
    all_results.extend(read_results)
    runner.close()

    ts = save_results(all_results, out_dir)
    generate_charts(all_results, out_dir, ts)

    logger.info("=" * 60)
    logger.info("BENCHMARK COMPLETE")
    logger.info("Results in: %s", out_dir)
    logger.info("=" * 60)

    _print_summary(all_results)


if __name__ == "__main__":
    main()
