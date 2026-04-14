"""Neo4j loader for KG benchmark.

Optimized pipeline:
  1. Vertices loaded via UNWIND/MERGE in batches of 1000.
  2. Edges grouped by predicate (Neo4j cannot parameterize rel types),
     then predicate groups processed in parallel via ThreadPoolExecutor.
  3. Each thread opens its own session for transaction isolation.
"""

from __future__ import annotations

import logging
import time
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor, as_completed
from typing import Sequence

from neo4j import GraphDatabase

from etl.parser import Edge, ParseResult, TickerData, Vertex

logger = logging.getLogger("benchmark.load_neo4j")

VERTEX_BATCH = 1000
EDGE_BATCH = 500
MAX_WORKERS = 4

VERTEX_MERGE = """
UNWIND $batch AS row
MERGE (e:Entity {external_id: row.external_id})
SET e.name = row.name,
    e.entity_type = row.entity_type,
    e.tenant_id = row.tenant_id,
    e.ticker = row.ticker
"""


class Neo4jLoader:
    def __init__(self, uri: str, user: str, password: str, workers: int = MAX_WORKERS):
        self.uri = uri
        self.user = user
        self.password = password
        self.workers = workers
        self._driver = None

    def connect(self) -> None:
        logger.info("Connecting to Neo4j at %s", self.uri)
        self._driver = GraphDatabase.driver(self.uri, auth=(self.user, self.password))
        self._driver.verify_connectivity()
        with self._driver.session() as s:
            r = s.run("MATCH (n) RETURN count(n) AS c").single()
            logger.info("Connected. Current node count: %d", r["c"])

    def close(self) -> None:
        if self._driver:
            self._driver.close()
            self._driver = None

    def drop_tenant(self, tenant_id: str) -> None:
        logger.info("Dropping tenant %s...", tenant_id)
        with self._driver.session() as s:
            s.run(
                "MATCH (e:Entity {tenant_id: $tid}) DETACH DELETE e",
                tid=tenant_id,
            )

    def load_vertices(self, vertices: Sequence[Vertex]) -> int:
        total = 0
        with self._driver.session() as s:
            for i in range(0, len(vertices), VERTEX_BATCH):
                batch = vertices[i : i + VERTEX_BATCH]
                rows = [
                    {
                        "external_id": v.external_id,
                        "name": v.name,
                        "entity_type": v.entity_type,
                        "tenant_id": v.tenant_id,
                        "ticker": v.ticker,
                    }
                    for v in batch
                ]
                s.run(VERTEX_MERGE, batch=rows)
                total += len(batch)
        return total

    def _load_predicate_group(self, predicate: str, rows: list[dict]) -> int:
        """Load all edges for a single predicate. Runs in its own session/thread.

        Retries individual batches up to 3 times on deadlock (TransientError).
        """
        safe_pred = predicate.replace("`", "``")
        query = f"""
UNWIND $batch AS row
MATCH (src:Entity {{external_id: row.src}})
MATCH (tgt:Entity {{external_id: row.tgt}})
MERGE (src)-[r:`{safe_pred}`]->(tgt)
SET r.page_id = row.page_id,
    r.chunk_id = row.chunk_id,
    r.source_file = row.source_file,
    r.triplet_index = row.triplet_index
"""
        loaded = 0
        for i in range(0, len(rows), EDGE_BATCH):
            batch = rows[i : i + EDGE_BATCH]
            for attempt in range(3):
                try:
                    with self._driver.session() as s:
                        s.run(query, batch=batch)
                    loaded += len(batch)
                    break
                except Exception as e:
                    if attempt < 2 and "DeadlockDetected" in str(e):
                        import time
                        time.sleep(0.1 * (attempt + 1))
                        continue
                    raise
        return loaded

    def load_edges(self, edges: Sequence[Edge]) -> int:
        grouped: dict[str, list[dict]] = defaultdict(list)
        for e in edges:
            grouped[e.predicate].append({
                "src": e.source_id,
                "tgt": e.target_id,
                "page_id": e.page_id,
                "chunk_id": e.chunk_id,
                "source_file": e.source_file,
                "triplet_index": e.triplet_index,
            })

        total = 0
        with ThreadPoolExecutor(max_workers=self.workers) as pool:
            futures = {
                pool.submit(self._load_predicate_group, pred, rows): pred
                for pred, rows in grouped.items()
            }
            for fut in as_completed(futures):
                pred = futures[fut]
                try:
                    count = fut.result()
                    total += count
                except Exception:
                    logger.error("Predicate group '%s' failed", pred, exc_info=True)

        return total

    def load_ticker(self, td: TickerData) -> tuple[float, float]:
        t0 = time.perf_counter()
        self.load_vertices(td.vertices)
        vt = time.perf_counter() - t0

        t0 = time.perf_counter()
        self.load_edges(td.edges)
        et = time.perf_counter() - t0

        logger.info(
            "Neo4j %s: %d V in %.1fs, %d E in %.1fs",
            td.ticker, len(td.vertices), vt, len(td.edges), et,
        )
        return vt, et

    def load_all(self, parsed: ParseResult) -> dict:
        t0 = time.perf_counter()
        v_total = self.load_vertices(parsed.all_vertices)
        v_time = time.perf_counter() - t0

        t0 = time.perf_counter()
        e_total = self.load_edges(parsed.all_edges)
        e_time = time.perf_counter() - t0

        logger.info(
            "Neo4j total: %d V in %.1fs, %d E in %.1fs",
            v_total, v_time, e_total, e_time,
        )
        return {
            "vertices": v_total,
            "edges": e_total,
            "vertex_time_s": v_time,
            "edge_time_s": e_time,
        }

    def vertex_count(self, tenant_id: str | None = None) -> int:
        with self._driver.session() as s:
            if tenant_id:
                r = s.run(
                    "MATCH (e:Entity {tenant_id: $tid}) RETURN count(e) AS c",
                    tid=tenant_id,
                ).single()
            else:
                r = s.run("MATCH (e:Entity) RETURN count(e) AS c").single()
            return r["c"]

    def edge_count(self, tenant_id: str | None = None) -> int:
        with self._driver.session() as s:
            if tenant_id:
                r = s.run(
                    "MATCH (e:Entity {tenant_id: $tid})-[r]-() RETURN count(r) AS c",
                    tid=tenant_id,
                ).single()
            else:
                r = s.run("MATCH ()-[r]->() RETURN count(r) AS c").single()
            return r["c"]
