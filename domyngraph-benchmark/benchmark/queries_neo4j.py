"""Cypher query templates for Neo4j benchmarks."""

from __future__ import annotations


POINT_LOOKUP = """
MATCH (e:Entity {external_id: $eid})
RETURN e
"""

FULLTEXT_SEARCH = """
CALL db.index.fulltext.queryNodes("entity_search", $query)
YIELD node, score
WHERE node.tenant_id = $tenant
RETURN node LIMIT $lim
"""

ONE_HOP = """
MATCH (s:Entity {external_id: $eid})-[]-(n)
RETURN DISTINCT n LIMIT $lim
"""

TWO_HOP = """
MATCH (s:Entity {external_id: $eid})-[]->()-[]->(n)
RETURN DISTINCT n LIMIT $lim
"""

FILTERED_TRAVERSAL_TEMPLATE = """
MATCH (s:Entity {{external_id: $eid}})-[:`{predicate}`]->(n)
RETURN DISTINCT n LIMIT $lim
"""

COUNT_BY_TYPE = """
MATCH (e:Entity {tenant_id: $tenant})
RETURN e.entity_type AS type, count(*) AS cnt
ORDER BY cnt DESC
"""

TOP_CONNECTED = """
MATCH (e:Entity {tenant_id: $tenant})-[r]-()
RETURN e.name AS name, count(r) AS degree
ORDER BY degree DESC
LIMIT $lim
"""

TENANT_ISOLATION = """
MATCH (e:Entity)
WHERE e.tenant_id = $tenant_a AND e.tenant_id = $tenant_b
RETURN count(e) AS cnt
"""

VERTEX_COUNT = """
MATCH (e:Entity {tenant_id: $tenant})
RETURN count(e) AS cnt
"""

VERTEX_COUNT_ALL = """
MATCH (e:Entity) RETURN count(e) AS cnt
"""

EDGE_COUNT_ALL = """
MATCH ()-[r]->() RETURN count(r) AS cnt
"""

MEMORY_STATS = """
CALL dbms.queryJmx('java.lang:type=Memory')
YIELD name, attributes
RETURN name, attributes
"""

PROFILE_POINT_LOOKUP = """
PROFILE MATCH (e:Entity {external_id: $eid})
RETURN e
"""

PROFILE_TENANT_SCAN = """
PROFILE MATCH (e:Entity {tenant_id: $tenant})
RETURN e LIMIT 1
"""


def filtered_traversal_query(predicate: str) -> str:
    safe = predicate.replace("`", "``")
    return FILTERED_TRAVERSAL_TEMPLATE.format(predicate=safe)
