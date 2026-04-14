"""JanusGraph loader for KG benchmark.

Supports three benchmark modes:

  Mode 'fair' (default):
    - Edge creation uses has('external_id') index lookups, directly
      comparable to Neo4j's MATCH by external_id.
    - storage.batch-loading OFF.

  Mode 'optimized' (post-hoc cache):
    - After vertex load, queries all vertices to build an
      external_id -> internal vertex ID cache (separate DB reads).
    - Edge creation uses g.V(internalId) for O(1) direct lookups.
    - ID cache build time is tracked and included in totals.
    - Demonstrates what happens with a naive cache approach.

  Mode 'streamed' (production-optimal):
    - Captures internal vertex IDs during insertion itself.
    - No extra DB round-trips; cache cost is effectively zero.
    - Edge creation uses g.V(internalId) for O(1) direct lookups.
    - This is how a real production system would work.

All modes use parallel batch submission via ThreadPoolExecutor.
Batch sizes are tuned to reasonable production defaults per engine,
not artificially equalized.
"""

from __future__ import annotations

import logging
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from typing import Literal, Sequence

from gremlin_python.driver.client import Client
from gremlin_python.driver.serializer import GraphSONSerializersV3d0

from etl.parser import Edge, ParseResult, TickerData, Vertex

logger = logging.getLogger("benchmark.load_jg")

DEFAULT_URL = "ws://localhost:8182/gremlin"
VERTEX_BATCH = 500
EDGE_BATCH = 200
MAX_WORKERS = 4

REQUIRED_PROPERTY_KEYS = [
    ("entity_type", "String"),
    ("ticker", "String"),
    ("page_id", "String"),
    ("chunk_id", "String"),
    ("source_file", "String"),
    ("triplet_index", "Integer"),
]

Mode = Literal["fair", "optimized", "streamed"]


def _esc(s: str) -> str:
    """Escape a string for embedding in Groovy single-quoted strings."""
    return s.replace("\\", "\\\\").replace("'", "\\'")


class JanusGraphLoader:
    def __init__(
        self,
        url: str = DEFAULT_URL,
        workers: int = MAX_WORKERS,
        mode: Mode = "fair",
    ):
        self.url = url
        self.workers = workers
        self.mode: Mode = mode
        self._client: Client | None = None
        self._id_cache: dict[str, int] = {}

    def connect(self) -> None:
        logger.info("Connecting to JanusGraph at %s (mode=%s)", self.url, self.mode)
        self._client = Client(
            self.url, "graph",
            message_serializer=GraphSONSerializersV3d0(),
        )
        count = self._submit("graph.traversal().V().count()")[0]
        logger.info("Connected. Current vertex count: %d", count)

    def close(self) -> None:
        if self._client:
            self._client.close()
            self._client = None

    def _submit(self, script: str) -> list:
        assert self._client is not None
        return self._client.submit(script).all().result()

    def _submit_fire_and_collect(self, script: str) -> list:
        assert self._client is not None
        return self._client.submit(script).all().result()

    # -- batch-loading toggle (optimized modes only) ----------------------

    def _enable_batch_loading(self) -> None:
        logger.info("Enabling storage.batch-loading (disables locking)")
        try:
            self._submit(
                "mgmt = graph.openManagement(); "
                "mgmt.set('storage.batch-loading', true); "
                "mgmt.commit(); 'batch_on'"
            )
            self._batch_loading_active = True
        except Exception as e:
            if "local configuration option" in str(e):
                logger.warning(
                    "Cannot toggle storage.batch-loading at runtime "
                    "(local-only config). ID cache optimization still active."
                )
                self._batch_loading_active = False
            else:
                raise

    def _disable_batch_loading(self) -> None:
        if not getattr(self, "_batch_loading_active", False):
            return
        logger.info("Disabling storage.batch-loading")
        try:
            self._submit(
                "mgmt = graph.openManagement(); "
                "mgmt.set('storage.batch-loading', false); "
                "mgmt.commit(); 'batch_off'"
            )
        except Exception:
            logger.warning("Could not disable storage.batch-loading")

    # -- schema registration ----------------------------------------------

    def pre_register_schema(self, predicates: set[str]) -> None:
        """Register property keys and edge labels in chunked transactions."""
        pk_lines = ["mgmt = graph.openManagement()"]
        for pk_name, pk_type in REQUIRED_PROPERTY_KEYS:
            pk_lines.append(
                f"if (!mgmt.containsPropertyKey('{pk_name}')) "
                f"mgmt.makePropertyKey('{pk_name}').dataType({pk_type}.class)"
                f".cardinality(org.janusgraph.core.Cardinality.SINGLE).make()"
            )
        pk_lines.append("mgmt.commit()")
        pk_lines.append("'pk_done'")
        logger.info("Pre-registering %d property keys...", len(REQUIRED_PROPERTY_KEYS))
        self._submit("\n".join(pk_lines))

        sorted_preds = sorted(predicates)
        chunk_size = 200
        chunks = [
            sorted_preds[i : i + chunk_size]
            for i in range(0, len(sorted_preds), chunk_size)
        ]
        logger.info("Pre-registering %d edge labels in %d chunks...",
                     len(predicates), len(chunks))

        for ci, chunk in enumerate(chunks):
            lines = ["mgmt = graph.openManagement()"]
            for p in chunk:
                safe = _esc(p)
                lines.append(
                    f"if (!mgmt.containsEdgeLabel('{safe}')) "
                    f"mgmt.makeEdgeLabel('{safe}').multiplicity(MULTI).make()"
                )
            lines.append("mgmt.commit()")
            lines.append(f"'chunk_{ci}'")
            self._submit("\n".join(lines))
            logger.debug("Edge label chunk %d/%d done", ci + 1, len(chunks))

        logger.info("Schema registered.")

    # -- data cleanup -----------------------------------------------------

    def drop_tenant(self, tenant_id: str) -> None:
        """Drop all vertices for a tenant, in batches to avoid eval timeout."""
        safe = _esc(tenant_id)
        logger.info("Dropping tenant %s...", tenant_id)
        while True:
            result = self._submit(
                f"g = graph.traversal(); "
                f"cnt = g.V().has('tenant_id','{safe}').limit(2000).drop().iterate(); "
                f"graph.tx().commit(); "
                f"g.V().has('tenant_id','{safe}').count().next()"
            )
            remaining = result[0] if result else 0
            if remaining == 0:
                break
            logger.debug("  %d remaining for tenant %s", remaining, tenant_id)

    def drop_all(self) -> None:
        """Drop all vertices in batches."""
        logger.info("Dropping all vertices...")
        while True:
            result = self._submit(
                "g = graph.traversal(); "
                "g.V().limit(5000).drop().iterate(); "
                "graph.tx().commit(); "
                "g.V().count().next()"
            )
            remaining = result[0] if result else 0
            if remaining == 0:
                break
            logger.debug("  %d remaining", remaining)

    # -- ID cache: post-hoc query (optimized mode) ------------------------

    def build_id_cache(self, tickers: list[str]) -> float:
        """Query internal vertex IDs per-tenant. Returns build time in seconds."""
        t0 = time.perf_counter()
        self._id_cache.clear()
        for ticker in tickers:
            safe = _esc(ticker)
            rows = self._submit(
                f"graph.traversal().V().has('tenant_id','{safe}')"
                f".project('eid','vid').by('external_id').by(id).toList()"
            )
            for row in rows:
                self._id_cache[row["eid"]] = row["vid"]
        elapsed = time.perf_counter() - t0
        logger.info("ID cache built (post-hoc query): %d entries in %.1fs",
                     len(self._id_cache), elapsed)
        return elapsed

    # -- vertex scripts ---------------------------------------------------

    def _build_vertex_script(self, batch: Sequence[Vertex]) -> str:
        """Standard vertex insert -- .iterate() returns nothing."""
        lines = ["g = graph.traversal()"]
        for v in batch:
            lines.append(
                f"g.addV('Entity')"
                f".property('name','{_esc(v.name)}')"
                f".property('entity_type','{_esc(v.entity_type)}')"
                f".property('tenant_id','{_esc(v.tenant_id)}')"
                f".property('external_id','{_esc(v.external_id)}')"
                f".property('ticker','{_esc(v.ticker)}')"
                f".iterate()"
            )
        lines.append("graph.tx().commit()")
        lines.append(f"'{len(batch)}'")
        return "\n".join(lines)

    def _build_vertex_script_streamed(self, batch: Sequence[Vertex]) -> str:
        """Vertex insert that captures internal IDs during creation.

        Uses .next() to get the vertex object, then collects
        [external_id, internal_id] pairs in a result list.
        No extra DB round-trip needed -- IDs are available immediately.
        """
        lines = [
            "g = graph.traversal()",
            "results = []",
        ]
        for v in batch:
            lines.append(
                f"v = g.addV('Entity')"
                f".property('name','{_esc(v.name)}')"
                f".property('entity_type','{_esc(v.entity_type)}')"
                f".property('tenant_id','{_esc(v.tenant_id)}')"
                f".property('external_id','{_esc(v.external_id)}')"
                f".property('ticker','{_esc(v.ticker)}')"
                f".next()"
            )
            lines.append(
                f"results.add(['{_esc(v.external_id)}', v.id()])"
            )
        lines.append("graph.tx().commit()")
        lines.append("results")
        return "\n".join(lines)

    # -- edge scripts (mode-dependent) ------------------------------------

    def _build_edge_script_fair(self, batch: Sequence[Edge]) -> str:
        """Edge creation via has('external_id') index lookup. lookups_per_edge = 2."""
        lines = ["g = graph.traversal()"]
        for e in batch:
            lines.append(
                f"g.V().has('external_id','{_esc(e.source_id)}')"
                f".addE('{_esc(e.predicate)}')"
                f".to(__.V().has('external_id','{_esc(e.target_id)}'))"
                f".property('page_id','{_esc(e.page_id)}')"
                f".property('chunk_id','{_esc(e.chunk_id)}')"
                f".property('source_file','{_esc(e.source_file)}')"
                f".property('triplet_index',{e.triplet_index})"
                f".iterate()"
            )
        lines.append("graph.tx().commit()")
        lines.append(f"'{len(batch)}'")
        return "\n".join(lines)

    def _build_edge_script_fast(self, batch: Sequence[Edge]) -> str:
        """Edge creation via g.V(internalId) -- O(1) direct lookup. lookups_per_edge = 0."""
        lines = ["g = graph.traversal()"]
        for e in batch:
            src_id = self._id_cache.get(e.source_id)
            tgt_id = self._id_cache.get(e.target_id)
            if src_id is None or tgt_id is None:
                logger.debug(
                    "Skipping edge %s->%s: vertex not in ID cache",
                    e.source_id, e.target_id,
                )
                continue
            lines.append(
                f"g.V({src_id}L).addE('{_esc(e.predicate)}')"
                f".to(__.V({tgt_id}L))"
                f".property('page_id','{_esc(e.page_id)}')"
                f".property('chunk_id','{_esc(e.chunk_id)}')"
                f".property('source_file','{_esc(e.source_file)}')"
                f".property('triplet_index',{e.triplet_index})"
                f".iterate()"
            )
        lines.append("graph.tx().commit()")
        lines.append(f"'{len(batch)}'")
        return "\n".join(lines)

    def _build_edge_script(self, batch: Sequence[Edge]) -> str:
        if self.mode in ("optimized", "streamed") and self._id_cache:
            return self._build_edge_script_fast(batch)
        return self._build_edge_script_fair(batch)

    # -- parallel batch submission ----------------------------------------

    def _submit_parallel(self, scripts: list[str]) -> list[list]:
        """Submit scripts in parallel. Returns list of results per script."""
        all_results: list[list] = [[] for _ in scripts]
        failed = 0
        with ThreadPoolExecutor(max_workers=self.workers) as pool:
            futures = {
                pool.submit(self._submit_fire_and_collect, s): i
                for i, s in enumerate(scripts)
            }
            for fut in as_completed(futures):
                idx = futures[fut]
                try:
                    all_results[idx] = fut.result()
                except Exception:
                    failed += 1
                    logger.error("Batch %d failed", idx, exc_info=True)

        if failed:
            logger.warning("%d / %d batches failed", failed, len(scripts))
        return all_results

    # -- load_vertices: standard (fair / optimized) -----------------------

    def load_vertices(self, vertices: Sequence[Vertex]) -> int:
        batches = [
            vertices[i : i + VERTEX_BATCH]
            for i in range(0, len(vertices), VERTEX_BATCH)
        ]
        scripts = [self._build_vertex_script(b) for b in batches]
        self._submit_parallel(scripts)
        total = len(vertices)
        logger.info("Loaded %d vertices in %d batches", total, len(batches))
        return total

    # -- load_vertices_streamed: captures IDs during insert ---------------

    def load_vertices_streamed(self, vertices: Sequence[Vertex]) -> int:
        """Load vertices and capture internal IDs in _id_cache simultaneously.

        Each batch script uses .next() and returns [external_id, internal_id]
        pairs. No separate query needed -- IDs are available at insert time.
        """
        batches = [
            vertices[i : i + VERTEX_BATCH]
            for i in range(0, len(vertices), VERTEX_BATCH)
        ]
        scripts = [self._build_vertex_script_streamed(b) for b in batches]
        batch_results = self._submit_parallel(scripts)

        captured = 0
        for result_list in batch_results:
            for item in result_list:
                if isinstance(item, (list, tuple)) and len(item) == 2:
                    self._id_cache[item[0]] = item[1]
                    captured += 1

        total = len(vertices)
        logger.info("Loaded %d vertices in %d batches, captured %d IDs in-flight",
                     total, len(batches), captured)
        return total

    # -- load_edges -------------------------------------------------------

    def load_edges(self, edges: Sequence[Edge]) -> int:
        batches = [
            edges[i : i + EDGE_BATCH]
            for i in range(0, len(edges), EDGE_BATCH)
        ]
        scripts = [self._build_edge_script(b) for b in batches]
        self._submit_parallel(scripts)
        total = len(edges)
        logger.info("Loaded %d edges in %d batches", total, len(batches))
        return total

    # -- high-level load methods ------------------------------------------

    def load_ticker(self, td: TickerData) -> tuple[float, float, float]:
        """Load a single ticker. Returns (vertex_time, edge_time, id_cache_time).

        id_cache_time is 0.0 in fair and streamed modes (streamed has zero
        extra cost because IDs are captured during vertex insertion).
        """
        self._id_cache.clear()

        t0 = time.perf_counter()
        if self.mode == "streamed":
            self.load_vertices_streamed(td.vertices)
        else:
            self.load_vertices(td.vertices)
        vt = time.perf_counter() - t0

        cache_t = 0.0
        if self.mode == "optimized":
            cache_t = self.build_id_cache([td.ticker])

        t0 = time.perf_counter()
        self.load_edges(td.edges)
        et = time.perf_counter() - t0

        total = vt + cache_t + et
        logger.info(
            "JG %s [%s]: %d V in %.1fs, %d E in %.1fs, cache=%.1fs, total=%.1fs",
            td.ticker, self.mode, len(td.vertices), vt, len(td.edges), et, cache_t, total,
        )
        return vt, et, cache_t

    def load_all(self, parsed: ParseResult) -> dict:
        """Load all tickers. Returns stats dict with timing breakdown."""
        self.pre_register_schema(parsed.unique_predicates)
        self._id_cache.clear()

        if self.mode in ("optimized", "streamed"):
            self._enable_batch_loading()

        try:
            t0 = time.perf_counter()
            if self.mode == "streamed":
                v_total = self.load_vertices_streamed(parsed.all_vertices)
            else:
                v_total = self.load_vertices(parsed.all_vertices)
            v_time = time.perf_counter() - t0

            cache_time = 0.0
            if self.mode == "optimized":
                ticker_list = [td.ticker for td in parsed.tickers]
                cache_time = self.build_id_cache(ticker_list)

            t0 = time.perf_counter()
            e_total = self.load_edges(parsed.all_edges)
            e_time = time.perf_counter() - t0
        finally:
            if self.mode in ("optimized", "streamed"):
                self._disable_batch_loading()

        total = v_time + cache_time + e_time
        is_direct = self.mode in ("optimized", "streamed")
        edge_lookup = "internal_id" if is_direct else "external_id"
        lookups_per_edge = 0 if is_direct else 2
        batch_loading_on = getattr(self, "_batch_loading_active", False)

        logger.info(
            "JG total [%s]: %d V in %.1fs, %d E in %.1fs, cache=%.1fs, total=%.1fs",
            self.mode, v_total, v_time, e_total, e_time, cache_time, total,
        )

        return {
            "vertices": v_total,
            "edges": e_total,
            "vertex_time_s": v_time,
            "edge_time_s": e_time,
            "id_cache_time_s": cache_time,
            "total_time_s": total,
            "mode": self.mode,
            "edge_lookup": edge_lookup,
            "lookups_per_edge": lookups_per_edge,
            "batch_loading": batch_loading_on,
            "time_per_edge_ms": (e_time * 1000) / e_total if e_total else 0,
            "id_cache_entries": len(self._id_cache),
        }

    # -- convenience counts -----------------------------------------------

    def vertex_count(self, tenant_id: str | None = None) -> int:
        if tenant_id:
            safe = _esc(tenant_id)
            r = self._submit(f"graph.traversal().V().has('tenant_id','{safe}').count()")
        else:
            r = self._submit("graph.traversal().V().count()")
        return r[0]

    def edge_count(self, tenant_id: str | None = None) -> int:
        if tenant_id:
            safe = _esc(tenant_id)
            r = self._submit(
                f"graph.traversal().V().has('tenant_id','{safe}').bothE().dedup().count()"
            )
        else:
            r = self._submit("graph.traversal().E().count()")
        return r[0]
