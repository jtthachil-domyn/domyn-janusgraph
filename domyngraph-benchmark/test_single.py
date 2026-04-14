#!/usr/bin/env python3
"""Quick test: load one ticker (AAPL) into both engines, run queries, verify, clean up."""

import logging
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from etl.parser import parse_all
from etl.load_janusgraph import JanusGraphLoader
from etl.load_neo4j import Neo4jLoader

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(message)s", datefmt="%H:%M:%S")
logger = logging.getLogger("test")

JG_URL = "ws://localhost:8182/gremlin"
NEO4J_URI = "neo4j://127.0.0.1:7687"
NEO4J_USER = "neo4j"
NEO4J_PASS = "neo4jtest123"


def main():
    logger.info("=== Parsing triplets ===")
    parsed = parse_all()
    aapl = next(t for t in parsed.tickers if t.ticker == "AAPL")
    logger.info("AAPL: %d vertices, %d edges", len(aapl.vertices), len(aapl.edges))

    predicates_aapl = set(e.predicate for e in aapl.edges)
    logger.info("AAPL predicates: %d unique", len(predicates_aapl))

    # --- JanusGraph ---
    logger.info("\n=== JanusGraph: Load AAPL ===")
    jg = JanusGraphLoader(JG_URL)
    jg.connect()

    jg.pre_register_schema(predicates_aapl)
    vt, et = jg.load_ticker(aapl)
    logger.info("JG load done: V=%.1fs, E=%.1fs", vt, et)

    jg_vc = jg.vertex_count("AAPL")
    jg_ec = jg.edge_count("AAPL")
    logger.info("JG counts: %d vertices, %d edges", jg_vc, jg_ec)

    # Quick query test
    r = jg._submit(
        "graph.traversal().V().has('tenant_id','AAPL').limit(3).elementMap().toList()"
    )
    logger.info("JG sample vertices:")
    for v in r:
        logger.info("  %s", v)

    r = jg._submit(
        "graph.traversal().V().has('tenant_id','AAPL')"
        ".groupCount().by('entity_type')"
    )
    logger.info("JG entity types: %s", r)

    r = jg._submit(
        "graph.traversal().V().has('external_id','AAPL:AAPL:ORG')"
        ".both().dedup().limit(5).elementMap().toList()"
    )
    logger.info("JG 1-hop from AAPL:AAPL:ORG (%d results):", len(r))
    for v in r[:3]:
        logger.info("  %s", v)

    # --- Neo4j ---
    logger.info("\n=== Neo4j: Load AAPL ===")
    neo = Neo4jLoader(NEO4J_URI, NEO4J_USER, NEO4J_PASS)
    neo.connect()

    vt, et = neo.load_ticker(aapl)
    logger.info("Neo4j load done: V=%.1fs, E=%.1fs", vt, et)

    neo_vc = neo.vertex_count("AAPL")
    neo_ec = neo.edge_count("AAPL")
    logger.info("Neo4j counts: %d vertices, %d edges", neo_vc, neo_ec)

    # Quick query test
    from neo4j import GraphDatabase
    driver = GraphDatabase.driver(NEO4J_URI, auth=(NEO4J_USER, NEO4J_PASS))
    with driver.session() as s:
        rows = s.run(
            "MATCH (e:Entity {tenant_id: 'AAPL'}) RETURN e LIMIT 3"
        )
        logger.info("Neo4j sample vertices:")
        for row in rows:
            node = row["e"]
            logger.info("  %s %s", dict(node), list(node.labels))

        rows = s.run(
            "MATCH (e:Entity {tenant_id: 'AAPL'}) "
            "RETURN e.entity_type AS type, count(*) AS cnt ORDER BY cnt DESC"
        )
        logger.info("Neo4j entity types:")
        for row in rows:
            logger.info("  %s: %d", row["type"], row["cnt"])

        rows = s.run(
            "MATCH (s:Entity {external_id: 'AAPL:AAPL:ORG'})-[]-(n) "
            "RETURN DISTINCT n LIMIT 5"
        )
        results = list(rows)
        logger.info("Neo4j 1-hop from AAPL:AAPL:ORG (%d results):", len(results))
        for row in results[:3]:
            logger.info("  %s", dict(row["n"]))

    driver.close()

    # --- Verify counts match ---
    logger.info("\n=== Verification ===")
    logger.info("JG  vertices: %d  | Neo4j vertices: %d  | Match: %s",
                jg_vc, neo_vc, jg_vc == neo_vc)
    logger.info("JG  edges:    %d  | Neo4j edges:    %d",
                jg_ec, neo_ec)

    # --- Cleanup ---
    logger.info("\n=== Cleanup ===")
    jg.drop_tenant("AAPL")
    neo.drop_tenant("AAPL")

    jg_after = jg.vertex_count("AAPL")
    neo_after = neo.vertex_count("AAPL")
    logger.info("After cleanup: JG AAPL vertices=%d, Neo4j AAPL vertices=%d", jg_after, neo_after)

    jg.close()
    neo.close()

    logger.info("\n=== DONE ===")
    if jg_vc == neo_vc and jg_after == 0 and neo_after == 0:
        logger.info("All checks PASSED")
    else:
        logger.warning("Some checks FAILED -- review output above")


if __name__ == "__main__":
    main()
