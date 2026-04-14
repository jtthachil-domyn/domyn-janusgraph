"""Shared triplet parser for KG benchmark.

Reads 10-K triplet JSON files, normalizes quintuples into Vertex/Edge
dataclasses, deduplicates vertices by external_id, and extracts unique
predicates for schema pre-registration.
"""

from __future__ import annotations

import json
import logging
import os
import re
from dataclasses import dataclass, field
from pathlib import Path

logger = logging.getLogger("benchmark.parser")

TRIPLET_DIR = Path(
    os.environ.get(
        "TRIPLET_DIR",
        "/Users/josephthomasthachil/Desktop/Domyn/uda/KG_VIEWS/"
        "Qwen2.5-72B-Instruct/reflection",
    )
)


@dataclass
class Vertex:
    external_id: str
    name: str
    entity_type: str
    ticker: str
    tenant_id: str


@dataclass
class Edge:
    source_id: str
    target_id: str
    predicate: str
    page_id: str
    chunk_id: str
    source_file: str
    triplet_index: int


@dataclass
class TickerData:
    ticker: str
    vertices: list[Vertex] = field(default_factory=list)
    edges: list[Edge] = field(default_factory=list)


@dataclass
class ParseResult:
    tickers: list[TickerData] = field(default_factory=list)
    all_vertices: list[Vertex] = field(default_factory=list)
    all_edges: list[Edge] = field(default_factory=list)
    unique_predicates: set[str] = field(default_factory=set)
    vertex_count: int = 0
    edge_count: int = 0
    skipped_empty: int = 0
    skipped_malformed: int = 0


def _normalize_predicate(pred: str) -> str:
    """Capitalize first letter of each word, replace spaces with underscores."""
    pred = pred.strip()
    if not pred:
        return "RELATED"
    parts = re.split(r"[\s_]+", pred)
    return "_".join(p.capitalize() for p in parts if p)


def _make_external_id(tenant_id: str, name: str, entity_type: str) -> str:
    return f"{tenant_id}:{name}:{entity_type}"


def parse_ticker(ticker_dir: Path) -> TickerData | None:
    """Parse a single ticker directory, returning TickerData or None."""
    json_files = list(ticker_dir.rglob("*_triplets_*.json"))
    if not json_files:
        return None

    json_file = json_files[0]
    ticker = ticker_dir.name

    logger.info("Parsing %s from %s", ticker, json_file.name)

    with open(json_file, "r", encoding="utf-8") as f:
        chunks = json.load(f)

    seen_vertices: dict[str, Vertex] = {}
    edges: list[Edge] = []
    skipped_empty = 0
    skipped_malformed = 0
    predicates: set[str] = set()

    for chunk in chunks:
        page_id = chunk.get("page_id", "")
        chunk_id = chunk.get("chunk_id", "")
        source_file = chunk.get("source_file", "")
        chunk_ticker = chunk.get("ticker", ticker)

        triplet_map = chunk.get("chunk_triplet", {})
        for key, triplet in triplet_map.items():
            if not triplet or not isinstance(triplet, list):
                skipped_empty += 1
                continue

            if len(triplet) < 5:
                skipped_malformed += 1
                logger.debug(
                    "Malformed triplet in %s %s %s: %s (len=%d)",
                    ticker, page_id, key, triplet, len(triplet),
                )
                continue

            subj_name, subj_type, predicate, obj_name, obj_type = (
                str(triplet[0]).strip(),
                str(triplet[1]).strip(),
                str(triplet[2]).strip(),
                str(triplet[3]).strip(),
                str(triplet[4]).strip(),
            )

            if not subj_name or not obj_name or not predicate:
                skipped_malformed += 1
                continue

            pred_normalized = _normalize_predicate(predicate)
            predicates.add(pred_normalized)

            tenant_id = chunk_ticker

            subj_ext_id = _make_external_id(tenant_id, subj_name, subj_type)
            obj_ext_id = _make_external_id(tenant_id, obj_name, obj_type)

            if subj_ext_id not in seen_vertices:
                seen_vertices[subj_ext_id] = Vertex(
                    external_id=subj_ext_id,
                    name=subj_name,
                    entity_type=subj_type,
                    ticker=chunk_ticker,
                    tenant_id=tenant_id,
                )
            if obj_ext_id not in seen_vertices:
                seen_vertices[obj_ext_id] = Vertex(
                    external_id=obj_ext_id,
                    name=obj_name,
                    entity_type=obj_type,
                    ticker=chunk_ticker,
                    tenant_id=tenant_id,
                )

            idx_match = re.search(r"\d+", key)
            triplet_index = int(idx_match.group()) if idx_match else 0

            edges.append(Edge(
                source_id=subj_ext_id,
                target_id=obj_ext_id,
                predicate=pred_normalized,
                page_id=page_id,
                chunk_id=chunk_id,
                source_file=source_file,
                triplet_index=triplet_index,
            ))

    td = TickerData(
        ticker=ticker,
        vertices=list(seen_vertices.values()),
        edges=edges,
    )

    logger.info(
        "%s: %d vertices, %d edges, %d skipped_empty, %d malformed",
        ticker, len(td.vertices), len(td.edges), skipped_empty, skipped_malformed,
    )

    return td


def parse_all(triplet_dir: Path | None = None) -> ParseResult:
    """Parse all ticker directories under the triplet root."""
    root = triplet_dir or TRIPLET_DIR
    result = ParseResult()

    ticker_dirs = sorted(
        [d for d in root.iterdir() if d.is_dir()],
        key=lambda d: d.name,
    )

    for td_path in ticker_dirs:
        td = parse_ticker(td_path)
        if td is None:
            continue
        result.tickers.append(td)
        result.all_vertices.extend(td.vertices)
        result.all_edges.extend(td.edges)
        result.unique_predicates.update(
            e.predicate for e in td.edges
        )

    global_seen: set[str] = set()
    deduped: list[Vertex] = []
    for v in result.all_vertices:
        if v.external_id not in global_seen:
            global_seen.add(v.external_id)
            deduped.append(v)
    result.all_vertices = deduped
    result.vertex_count = len(deduped)
    result.edge_count = len(result.all_edges)
    result.skipped_empty = sum(1 for _ in [])  # computed per-ticker above

    logger.info(
        "Total: %d tickers, %d vertices, %d edges, %d unique predicates",
        len(result.tickers), result.vertex_count, result.edge_count,
        len(result.unique_predicates),
    )
    logger.info("Predicates: %s", sorted(result.unique_predicates))

    return result


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO, format="%(message)s")
    r = parse_all()
    print(f"\nSummary:")
    print(f"  Tickers:    {len(r.tickers)}")
    print(f"  Vertices:   {r.vertex_count}")
    print(f"  Edges:      {r.edge_count}")
    print(f"  Predicates: {sorted(r.unique_predicates)}")
    for td in r.tickers:
        print(f"  {td.ticker}: {len(td.vertices)} V, {len(td.edges)} E")
