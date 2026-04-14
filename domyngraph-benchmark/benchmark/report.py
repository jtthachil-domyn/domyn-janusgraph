"""Benchmark reporting: JSON, CSV, and matplotlib charts."""

from __future__ import annotations

import csv
import json
import logging
from datetime import datetime
from pathlib import Path

import matplotlib.pyplot as plt
import numpy as np

from benchmark.runner import BenchmarkResult

logger = logging.getLogger("benchmark.report")


def save_results(results: list[BenchmarkResult], out_dir: Path) -> str:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")

    json_path = out_dir / f"{ts}_raw.json"
    with open(json_path, "w") as f:
        json.dump([r.to_dict() for r in results], f, indent=2)
    logger.info("Raw results: %s", json_path)

    csv_path = out_dir / f"{ts}_summary.csv"
    with open(csv_path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["benchmark", "engine", "category", "iterations",
                     "p50_ms", "p95_ms", "p99_ms", "mean_ms", "std_ms",
                     "ops_per_sec", "notes"])
        for r in results:
            w.writerow([
                r.name, r.engine, r.category, r.iterations,
                f"{r.p50:.2f}", f"{r.p95:.2f}", f"{r.p99:.2f}",
                f"{r.mean:.2f}", f"{r.std_dev:.2f}",
                f"{r.ops_per_sec:.1f}", r.notes,
            ])
    logger.info("Summary CSV: %s", csv_path)

    return ts


def generate_charts(results: list[BenchmarkResult], out_dir: Path, ts: str) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)

    read_results = [r for r in results if r.category in ("read", "traversal", "aggregation", "correctness")]
    if not read_results:
        logger.warning("No read benchmarks to chart")
        return

    benchmarks = sorted(set(r.name for r in read_results))
    jg_p50 = []
    neo_p50 = []
    jg_p95 = []
    neo_p95 = []
    jg_p99 = []
    neo_p99 = []
    labels = []

    for bname in benchmarks:
        jg = next((r for r in read_results if r.name == bname and r.engine == "janusgraph"), None)
        neo = next((r for r in read_results if r.name == bname and r.engine == "neo4j"), None)
        if not jg or not neo:
            continue
        short = bname.split(":")[0].strip() if ":" in bname else bname
        labels.append(short)
        jg_p50.append(jg.p50)
        neo_p50.append(neo.p50)
        jg_p95.append(jg.p95)
        neo_p95.append(neo.p95)
        jg_p99.append(jg.p99)
        neo_p99.append(neo.p99)

    if not labels:
        return

    x = np.arange(len(labels))
    width = 0.35

    # 1. P50 latency bar chart
    fig, ax = plt.subplots(figsize=(12, 6))
    ax.bar(x - width / 2, jg_p50, width, label="JanusGraph", color="#2196F3")
    ax.bar(x + width / 2, neo_p50, width, label="Neo4j", color="#FF9800")
    ax.set_ylabel("p50 Latency (ms)")
    ax.set_title("Benchmark Comparison: p50 Latency")
    ax.set_xticks(x)
    ax.set_xticklabels(labels, rotation=30, ha="right")
    ax.legend()
    ax.grid(axis="y", alpha=0.3)
    fig.tight_layout()
    fig.savefig(out_dir / f"{ts}_latency_bar.png", dpi=150)
    plt.close(fig)

    # 2. Percentile grouped bar chart
    fig, axes = plt.subplots(1, 3, figsize=(18, 6), sharey=True)
    for i, (pvals_jg, pvals_neo, pname) in enumerate([
        (jg_p50, neo_p50, "p50"),
        (jg_p95, neo_p95, "p95"),
        (jg_p99, neo_p99, "p99"),
    ]):
        ax = axes[i]
        ax.bar(x - width / 2, pvals_jg, width, label="JanusGraph", color="#2196F3")
        ax.bar(x + width / 2, pvals_neo, width, label="Neo4j", color="#FF9800")
        ax.set_title(f"{pname} Latency (ms)")
        ax.set_xticks(x)
        ax.set_xticklabels(labels, rotation=30, ha="right")
        ax.legend()
        ax.grid(axis="y", alpha=0.3)
    fig.suptitle("Percentile Comparison", fontsize=14)
    fig.tight_layout()
    fig.savefig(out_dir / f"{ts}_percentiles.png", dpi=150)
    plt.close(fig)

    # 3. Box plot of latency distributions
    fig, ax = plt.subplots(figsize=(14, 6))
    positions = []
    data_list = []
    tick_labels_full = []
    for i, bname in enumerate(benchmarks):
        jg = next((r for r in read_results if r.name == bname and r.engine == "janusgraph"), None)
        neo = next((r for r in read_results if r.name == bname and r.engine == "neo4j"), None)
        if not jg or not neo:
            continue
        short = bname.split(":")[0].strip() if ":" in bname else bname
        pos_jg = i * 3
        pos_neo = i * 3 + 1
        positions.extend([pos_jg, pos_neo])
        data_list.extend([jg.times_ms, neo.times_ms])
        tick_labels_full.extend([f"{short}\nJG", f"{short}\nNeo4j"])

    if data_list:
        bp = ax.boxplot(data_list, positions=list(range(len(data_list))),
                        widths=0.6, patch_artist=True)
        colors = ["#2196F3", "#FF9800"] * (len(data_list) // 2 + 1)
        for patch, color in zip(bp["boxes"], colors):
            patch.set_facecolor(color)
            patch.set_alpha(0.7)
        ax.set_xticks(list(range(len(data_list))))
        ax.set_xticklabels(tick_labels_full, rotation=45, ha="right", fontsize=8)
        ax.set_ylabel("Latency (ms)")
        ax.set_title("Latency Distribution by Benchmark")
        ax.grid(axis="y", alpha=0.3)
    fig.tight_layout()
    fig.savefig(out_dir / f"{ts}_latency_box.png", dpi=150)
    plt.close(fig)

    # 4. Throughput chart (for write benchmarks B1/B2)
    write_results = [r for r in results if r.category == "write" and r.ops_per_sec > 0]
    if write_results:
        fig, ax = plt.subplots(figsize=(10, 5))
        w_labels = sorted(set(r.name for r in write_results))
        jg_ops = []
        neo_ops = []
        for wl in w_labels:
            jg = next((r for r in write_results if r.name == wl and r.engine == "janusgraph"), None)
            neo = next((r for r in write_results if r.name == wl and r.engine == "neo4j"), None)
            jg_ops.append(jg.ops_per_sec if jg else 0)
            neo_ops.append(neo.ops_per_sec if neo else 0)

        wx = np.arange(len(w_labels))
        ax.bar(wx - width / 2, jg_ops, width, label="JanusGraph", color="#2196F3")
        ax.bar(wx + width / 2, neo_ops, width, label="Neo4j", color="#FF9800")
        ax.set_ylabel("Records / sec")
        ax.set_title("Write Throughput")
        ax.set_xticks(wx)
        ax.set_xticklabels(w_labels, rotation=30, ha="right")
        ax.legend()
        ax.grid(axis="y", alpha=0.3)
        fig.tight_layout()
        fig.savefig(out_dir / f"{ts}_throughput.png", dpi=150)
        plt.close(fig)

    logger.info("Charts saved to %s", out_dir)
