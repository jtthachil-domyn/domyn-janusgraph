#!/usr/bin/env python3
"""Official Neo4j Python-driver smoke test for Nexus Bolt compatibility.

Usage:
    pip install neo4j
    NEXUS_BOLT_URI=bolt://127.0.0.1:7687 \
    NEXUS_BOLT_USER=neo4j \
    NEXUS_BOLT_PASSWORD=<token-if-configured> \
    python3 scripts/neo4j-driver-smoke.py

This intentionally tests auto-commit writes only. Nexus currently rejects
explicit write transactions instead of pretending rollback is supported.
"""

from __future__ import annotations

import os
import sys


def main() -> int:
    try:
        from neo4j import GraphDatabase, basic_auth
    except ImportError:
        print("neo4j Python driver is not installed. Run: pip install neo4j", file=sys.stderr)
        return 2

    uri = os.environ.get("NEXUS_BOLT_URI", "bolt://127.0.0.1:7687")
    user = os.environ.get("NEXUS_BOLT_USER", "neo4j")
    password = os.environ.get("NEXUS_BOLT_PASSWORD", "")
    database = os.environ.get("NEXUS_BOLT_DATABASE")

    auth = basic_auth(user, password) if password else None
    driver = GraphDatabase.driver(uri, auth=auth)

    try:
        driver.verify_connectivity()
        session_kwargs = {"database": database} if database else {}
        with driver.session(**session_kwargs) as session:
            record = session.run(
                "CREATE (n:DriverSmoke {name: $name}) RETURN n.name AS name",
                name="neo4j-driver-smoke",
            ).single(strict=True)
            assert record["name"] == "neo4j-driver-smoke"

            count_record = session.run(
                "MATCH (n:DriverSmoke) WHERE n.name = $name RETURN count(n) AS count",
                name="neo4j-driver-smoke",
            ).single(strict=True)
            assert count_record["count"] >= 1

            rel_record = session.run(
                """
                MATCH (a:DriverSmoke), (b:DriverSmoke)
                WHERE a.name = $name AND b.name = $name
                CREATE (a)-[:SMOKE_REL]->(b)
                RETURN count(*) AS count
                """,
                name="neo4j-driver-smoke",
            ).single(strict=True)
            assert rel_record["count"] >= 1
    finally:
        driver.close()

    print("neo4j-driver smoke: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
