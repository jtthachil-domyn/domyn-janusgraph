"""Benchmark execution engine.

Runs each benchmark with: 1 cold + 2 warm-up + N measured iterations.
Computes p50/p95/p99/mean/std for measured runs.
"""

from __future__ import annotations

import logging
import random
import time
from dataclasses import asdict, dataclass, field

import numpy as np

from gremlin_python.driver.client import Client
from gremlin_python.driver.serializer import GraphSONSerializersV3d0
from neo4j import GraphDatabase

from benchmark import queries_jg as jg_q
from benchmark import queries_neo4j as neo4j_q
from etl.parser import ParseResult

logger = logging.getLogger("benchmark.runner")


@dataclass
class BenchmarkResult:
    name: str
    engine: str
    category: str
    iterations: int
    warm_up_runs: int
    times_ms: list[float] = field(default_factory=list)
    p50: float = 0.0
    p95: float = 0.0
    p99: float = 0.0
    mean: float = 0.0
    std_dev: float = 0.0
    ops_per_sec: float = 0.0
    records_processed: int = 0
    notes: str = ""

    def compute_stats(self) -> None:
        if not self.times_ms:
            return
        arr = np.array(self.times_ms)
        self.p50 = float(np.percentile(arr, 50))
        self.p95 = float(np.percentile(arr, 95))
        self.p99 = float(np.percentile(arr, 99))
        self.mean = float(np.mean(arr))
        self.std_dev = float(np.std(arr))
        if self.mean > 0 and self.records_processed > 0:
            self.ops_per_sec = self.records_processed / (self.mean / 1000.0)

    def to_dict(self) -> dict:
        return asdict(self)


def _format_plan(plan, depth: int = 0) -> str:
    """Recursively format a Neo4j query plan into a readable string."""
    if plan is None:
        return "no plan"
    parts = []
    indent = "  " * depth
    op = getattr(plan, "operator_type", "?")
    args = getattr(plan, "arguments", {})
    db_hits = args.get("DbHits", "?")
    rows = args.get("Rows", "?")
    details = args.get("Details", "")
    parts.append(f"{indent}{op} (dbHits={db_hits}, rows={rows}) {details}")
    children = getattr(plan, "children", [])
    for child in children:
        parts.append(_format_plan(child, depth + 1))
    return "\n".join(parts)


def _time_ms(fn) -> float:
    t0 = time.perf_counter()
    fn()
    return (time.perf_counter() - t0) * 1000


def _run_measured(fn, iterations: int = 10, cold: int = 1, warm: int = 2) -> list[float]:
    """Run fn with cold + warm discarded runs, then N measured."""
    for _ in range(cold):
        fn()
    for _ in range(warm):
        fn()
    times = []
    for _ in range(iterations):
        times.append(_time_ms(fn))
    return times


class BenchmarkRunner:
    def __init__(
        self,
        jg_url: str,
        neo4j_uri: str,
        neo4j_user: str,
        neo4j_pass: str,
        parsed: ParseResult,
        iterations: int = 10,
    ):
        self.jg_url = jg_url
        self.neo4j_uri = neo4j_uri
        self.neo4j_user = neo4j_user
        self.neo4j_pass = neo4j_pass
        self.parsed = parsed
        self.iterations = iterations
        self.results: list[BenchmarkResult] = []

        self._jg: Client | None = None
        self._neo4j_driver = None

        self._seed_vertices: list[str] = []
        self._seed_names: list[str] = []
        self._tickers: list[str] = []

    def connect(self) -> None:
        logger.info("Connecting to JanusGraph...")
        self._jg = Client(
            self.jg_url, "graph",
            message_serializer=GraphSONSerializersV3d0(),
        )
        logger.info("Connecting to Neo4j...")
        self._neo4j_driver = GraphDatabase.driver(
            self.neo4j_uri, auth=(self.neo4j_user, self.neo4j_pass),
        )
        self._neo4j_driver.verify_connectivity()

        all_verts = self.parsed.all_vertices
        self._tickers = [td.ticker for td in self.parsed.tickers]

        sample_size = min(50, len(all_verts))
        sampled = random.sample(all_verts, sample_size)
        self._seed_vertices = [v.external_id for v in sampled]
        self._seed_names = [v.name for v in sampled]

    def close(self) -> None:
        if self._jg:
            self._jg.close()
        if self._neo4j_driver:
            self._neo4j_driver.close()

    def _jg_submit(self, script: str) -> list:
        return self._jg.submit(script).all().result()

    def _neo4j_run(self, cypher: str, **params) -> list:
        with self._neo4j_driver.session() as s:
            return [r.data() for r in s.run(cypher, parameters=params)]

    def _add(self, result: BenchmarkResult) -> None:
        result.compute_stats()
        self.results.append(result)
        logger.info(
            "  %-40s %-10s  p50=%.1fms  p95=%.1fms  mean=%.1fms  std=%.1fms",
            result.name, result.engine, result.p50, result.p95, result.mean, result.std_dev,
        )

    def run_b3_point_lookup(self) -> None:
        logger.info("B3: Point lookup by external_id")
        seeds = self._seed_vertices[:100]

        # JanusGraph
        jg_times = []
        for _ in range(self.iterations):
            batch_times = []
            for eid in seeds:
                batch_times.append(_time_ms(lambda e=eid: self._jg_submit(jg_q.point_lookup(e))))
            jg_times.append(np.mean(batch_times))

        r = BenchmarkResult("B3: Point Lookup", "janusgraph", "read", self.iterations, 3,
                            notes=f"{len(seeds)} lookups per iteration")
        r.times_ms = jg_times
        r.records_processed = len(seeds)
        self._add(r)

        # Neo4j
        neo_times = []
        for _ in range(self.iterations):
            batch_times = []
            for eid in seeds:
                batch_times.append(
                    _time_ms(lambda e=eid: self._neo4j_run(neo4j_q.POINT_LOOKUP, eid=e))
                )
            neo_times.append(np.mean(batch_times))

        r = BenchmarkResult("B3: Point Lookup", "neo4j", "read", self.iterations, 3,
                            notes=f"{len(seeds)} lookups per iteration")
        r.times_ms = neo_times
        r.records_processed = len(seeds)
        self._add(r)

    def run_b4_fulltext(self) -> None:
        logger.info("B4: Full-text search by name")
        names = self._seed_names[:20]
        tenant = self._tickers[0] if self._tickers else "AAPL"

        jg_times = _run_measured(
            lambda: [self._jg_submit(jg_q.fulltext_search(n, tenant)) for n in names],
            self.iterations,
        )
        r = BenchmarkResult("B4: Fulltext Search", "janusgraph", "read", self.iterations, 3,
                            notes=f"{len(names)} searches per iteration")
        r.times_ms = jg_times
        r.records_processed = len(names)
        self._add(r)

        neo_times = _run_measured(
            lambda: [self._neo4j_run(neo4j_q.FULLTEXT_SEARCH, query=n, tenant=tenant, lim=10)
                     for n in names],
            self.iterations,
        )
        r = BenchmarkResult("B4: Fulltext Search", "neo4j", "read", self.iterations, 3,
                            notes=f"{len(names)} searches per iteration")
        r.times_ms = neo_times
        r.records_processed = len(names)
        self._add(r)

    def run_b5_one_hop(self) -> None:
        logger.info("B5: 1-hop traversal")
        seeds = self._seed_vertices[:30]

        jg_times = _run_measured(
            lambda: [self._jg_submit(jg_q.one_hop(s)) for s in seeds],
            self.iterations,
        )
        r = BenchmarkResult("B5: 1-Hop Traversal", "janusgraph", "traversal", self.iterations, 3,
                            notes=f"{len(seeds)} seeds")
        r.times_ms = jg_times
        self._add(r)

        neo_times = _run_measured(
            lambda: [self._neo4j_run(neo4j_q.ONE_HOP, eid=s, lim=100) for s in seeds],
            self.iterations,
        )
        r = BenchmarkResult("B5: 1-Hop Traversal", "neo4j", "traversal", self.iterations, 3,
                            notes=f"{len(seeds)} seeds")
        r.times_ms = neo_times
        self._add(r)

    def run_b6_two_hop(self) -> None:
        logger.info("B6: 2-hop traversal")
        seeds = self._seed_vertices[:15]

        jg_times = _run_measured(
            lambda: [self._jg_submit(jg_q.two_hop(s)) for s in seeds],
            self.iterations,
        )
        r = BenchmarkResult("B6: 2-Hop Traversal", "janusgraph", "traversal", self.iterations, 3,
                            notes=f"{len(seeds)} seeds")
        r.times_ms = jg_times
        self._add(r)

        neo_times = _run_measured(
            lambda: [self._neo4j_run(neo4j_q.TWO_HOP, eid=s, lim=100) for s in seeds],
            self.iterations,
        )
        r = BenchmarkResult("B6: 2-Hop Traversal", "neo4j", "traversal", self.iterations, 3,
                            notes=f"{len(seeds)} seeds")
        r.times_ms = neo_times
        self._add(r)

    def run_b7_filtered(self) -> None:
        logger.info("B7: Filtered traversal (Discloses)")
        seeds = self._seed_vertices[:30]
        pred = "Discloses"

        jg_times = _run_measured(
            lambda: [self._jg_submit(jg_q.filtered_traversal(s, pred)) for s in seeds],
            self.iterations,
        )
        r = BenchmarkResult("B7: Filtered Traversal", "janusgraph", "traversal", self.iterations, 3,
                            notes=f"{len(seeds)} seeds, predicate={pred}")
        r.times_ms = jg_times
        self._add(r)

        neo_query = neo4j_q.filtered_traversal_query(pred)
        neo_times = _run_measured(
            lambda: [self._neo4j_run(neo_query, eid=s, lim=100) for s in seeds],
            self.iterations,
        )
        r = BenchmarkResult("B7: Filtered Traversal", "neo4j", "traversal", self.iterations, 3,
                            notes=f"{len(seeds)} seeds, predicate={pred}")
        r.times_ms = neo_times
        self._add(r)

    def run_b8_count_by_type(self) -> None:
        logger.info("B8: Count by entity_type")
        tenant = self._tickers[0] if self._tickers else "AAPL"

        jg_times = _run_measured(
            lambda: self._jg_submit(jg_q.count_by_type(tenant)),
            self.iterations,
        )
        r = BenchmarkResult("B8: Count By Type", "janusgraph", "aggregation", self.iterations, 3,
                            notes=f"tenant={tenant}")
        r.times_ms = jg_times
        self._add(r)

        neo_times = _run_measured(
            lambda: self._neo4j_run(neo4j_q.COUNT_BY_TYPE, tenant=tenant),
            self.iterations,
        )
        r = BenchmarkResult("B8: Count By Type", "neo4j", "aggregation", self.iterations, 3,
                            notes=f"tenant={tenant}")
        r.times_ms = neo_times
        self._add(r)

    def run_b9_top_connected(self) -> None:
        logger.info("B9: Top-10 most connected")
        tenant = self._tickers[0] if self._tickers else "AAPL"

        jg_times = _run_measured(
            lambda: self._jg_submit(jg_q.top_connected(tenant)),
            self.iterations,
        )
        r = BenchmarkResult("B9: Top Connected", "janusgraph", "aggregation", self.iterations, 3,
                            notes=f"tenant={tenant}")
        r.times_ms = jg_times
        self._add(r)

        neo_times = _run_measured(
            lambda: self._neo4j_run(neo4j_q.TOP_CONNECTED, tenant=tenant, lim=10),
            self.iterations,
        )
        r = BenchmarkResult("B9: Top Connected", "neo4j", "aggregation", self.iterations, 3,
                            notes=f"tenant={tenant}")
        r.times_ms = neo_times
        self._add(r)

    def run_b10_isolation(self) -> None:
        logger.info("B10: Cross-tenant isolation")
        if len(self._tickers) < 2:
            logger.warning("Need at least 2 tickers for B10, skipping")
            return
        a, b = self._tickers[0], self._tickers[1]

        jg_times = _run_measured(
            lambda: self._jg_submit(jg_q.tenant_isolation(a, b)),
            self.iterations,
        )
        r = BenchmarkResult("B10: Tenant Isolation", "janusgraph", "correctness", self.iterations, 3,
                            notes=f"tenants={a},{b}")
        r.times_ms = jg_times
        self._add(r)

        neo_times = _run_measured(
            lambda: self._neo4j_run(neo4j_q.TENANT_ISOLATION, tenant_a=a, tenant_b=b),
            self.iterations,
        )
        r = BenchmarkResult("B10: Tenant Isolation", "neo4j", "correctness", self.iterations, 3,
                            notes=f"tenants={a},{b}")
        r.times_ms = neo_times
        self._add(r)

    def run_b11_memory(self) -> None:
        logger.info("B11: Memory footprint")

        # JanusGraph -- JVM Runtime heap stats via Groovy
        try:
            jg_mem = self._jg_submit(jg_q.memory_stats())
            mem = jg_mem[0] if jg_mem else {}
            heap_max = mem.get("heapMax", 0)
            heap_used = mem.get("heapUsed", 0)
            heap_free = mem.get("heapFree", 0)
            jg_notes = (
                f"heapMax={heap_max / (1024**2):.0f}MB, "
                f"heapUsed={heap_used / (1024**2):.0f}MB, "
                f"heapFree={heap_free / (1024**2):.0f}MB"
            )
            logger.info("  JanusGraph: %s", jg_notes)
        except Exception as e:
            jg_notes = f"error: {e}"
            logger.warning("  JanusGraph memory stats failed: %s", e)

        r = BenchmarkResult("B11: Memory Footprint", "janusgraph", "meta", 1, 0,
                            notes=jg_notes)
        r.times_ms = [0]
        self._add(r)

        # Neo4j -- JMX memory stats
        try:
            neo_mem = self._neo4j_run(neo4j_q.MEMORY_STATS)
            neo_notes_parts = []
            for row in neo_mem:
                attrs = row.get("attributes", {})
                for key, val in attrs.items():
                    if isinstance(val, dict) and "value" in val:
                        inner = val["value"]
                        if isinstance(inner, dict):
                            used = inner.get("used", 0)
                            max_v = inner.get("max", 0)
                            neo_notes_parts.append(
                                f"{key}: used={used / (1024**2):.0f}MB, "
                                f"max={max_v / (1024**2):.0f}MB"
                            )
            neo_notes = "; ".join(neo_notes_parts) if neo_notes_parts else "raw: " + str(neo_mem)[:200]
            logger.info("  Neo4j: %s", neo_notes)
        except Exception as e:
            neo_notes = f"error: {e}"
            logger.warning("  Neo4j memory stats failed: %s", e)

        r = BenchmarkResult("B11: Memory Footprint", "neo4j", "meta", 1, 0,
                            notes=neo_notes)
        r.times_ms = [0]
        self._add(r)

    def run_b12_index_verification(self) -> None:
        logger.info("B12: Index hit verification")
        seed = self._seed_vertices[0] if self._seed_vertices else "AAPL:AAPL:ORG"
        tenant = self._tickers[0] if self._tickers else "AAPL"

        # JanusGraph -- .profile() shows step-level timings and index usage
        try:
            jg_profile = self._jg_submit(jg_q.profile_point_lookup(seed))
            jg_prof_str = str(jg_profile)[:500]
            jg_notes = f"external_id lookup profile: {jg_prof_str}"

            jg_tenant_profile = self._jg_submit(jg_q.profile_tenant_scan(tenant))
            jg_tenant_str = str(jg_tenant_profile)[:500]
            jg_notes += f" | tenant scan profile: {jg_tenant_str}"
            logger.info("  JanusGraph index profile captured")
        except Exception as e:
            jg_notes = f"error: {e}"
            logger.warning("  JanusGraph profile failed: %s", e)

        r = BenchmarkResult("B12: Index Verification", "janusgraph", "meta", 1, 0,
                            notes=jg_notes)
        r.times_ms = [0]
        self._add(r)

        # Neo4j -- PROFILE shows query plan with index hits
        try:
            with self._neo4j_driver.session() as s:
                result = s.run(neo4j_q.PROFILE_POINT_LOOKUP, eid=seed)
                summary = result.consume()
                plan = summary.profile if summary.profile else summary.plan
                neo_notes = f"point_lookup plan: {_format_plan(plan)}"

                result2 = s.run(neo4j_q.PROFILE_TENANT_SCAN, tenant=tenant)
                summary2 = result2.consume()
                plan2 = summary2.profile if summary2.profile else summary2.plan
                neo_notes += f" | tenant_scan plan: {_format_plan(plan2)}"

            logger.info("  Neo4j index profile captured")
        except Exception as e:
            neo_notes = f"error: {e}"
            logger.warning("  Neo4j profile failed: %s", e)

        r = BenchmarkResult("B12: Index Verification", "neo4j", "meta", 1, 0,
                            notes=neo_notes)
        r.times_ms = [0]
        self._add(r)

    def run_b13_stats(self) -> None:
        logger.info("B13: Graph statistics")
        jg_vc = self._jg_submit(jg_q.vertex_count())[0]
        try:
            jg_ec = self._jg_submit(jg_q.edge_count())[0]
        except Exception:
            logger.warning("  JG E().count() timed out, using V().bothE().count()/2 approximation")
            raw = self._jg_submit("graph.traversal().V().limit(5000).bothE().count()")[0]
            ratio = raw / min(jg_vc, 5000) if jg_vc > 0 else 0
            jg_ec = int(ratio * jg_vc / 2)
        jg_deg = (jg_ec * 2) / jg_vc if jg_vc else 0

        with self._neo4j_driver.session() as s:
            neo_vc = s.run(neo4j_q.VERTEX_COUNT_ALL).single()["cnt"]
            neo_ec = s.run(neo4j_q.EDGE_COUNT_ALL).single()["cnt"]
        neo_deg = (neo_ec * 2) / neo_vc if neo_vc else 0

        logger.info("  JanusGraph: %d V, %d E, avg_degree=%.2f", jg_vc, jg_ec, jg_deg)
        logger.info("  Neo4j:      %d V, %d E, avg_degree=%.2f", neo_vc, neo_ec, neo_deg)

        r = BenchmarkResult("B13: Graph Stats", "janusgraph", "meta", 1, 0,
                            notes=f"V={jg_vc}, E={jg_ec}, avg_deg={jg_deg:.2f}")
        r.times_ms = [0]
        self._add(r)

        r = BenchmarkResult("B13: Graph Stats", "neo4j", "meta", 1, 0,
                            notes=f"V={neo_vc}, E={neo_ec}, avg_deg={neo_deg:.2f}")
        r.times_ms = [0]
        self._add(r)

    def run_all_read_benchmarks(self) -> list[BenchmarkResult]:
        self.run_b3_point_lookup()
        self.run_b4_fulltext()
        self.run_b5_one_hop()
        self.run_b6_two_hop()
        self.run_b7_filtered()
        self.run_b8_count_by_type()
        self.run_b9_top_connected()
        self.run_b10_isolation()
        self.run_b11_memory()
        self.run_b12_index_verification()
        self.run_b13_stats()
        return self.results
