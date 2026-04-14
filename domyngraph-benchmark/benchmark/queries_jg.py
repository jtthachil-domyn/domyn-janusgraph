"""Gremlin query templates for JanusGraph benchmarks."""

from __future__ import annotations


def _esc(s: str) -> str:
    """Escape a string for embedding in Groovy single-quoted strings."""
    return s.replace("\\", "\\\\").replace("'", "\\'")


def point_lookup(external_id: str) -> str:
    eid = _esc(external_id)
    return f"graph.traversal().V().has('external_id','{eid}').elementMap().toList()"


def fulltext_search(name: str, tenant_id: str, limit: int = 10) -> str:
    n = _esc(name)
    t = _esc(tenant_id)
    return (
        f"graph.traversal().V().has('tenant_id','{t}')"
        f".has('name', textContains('{n}')).limit({limit}).elementMap().toList()"
    )


def one_hop(external_id: str, limit: int = 100) -> str:
    eid = _esc(external_id)
    return (
        f"graph.traversal().V().has('external_id','{eid}')"
        f".both().dedup().limit({limit}).elementMap().toList()"
    )


def two_hop(external_id: str, limit: int = 100) -> str:
    eid = _esc(external_id)
    return (
        f"graph.traversal().V().has('external_id','{eid}')"
        f".out().out().dedup().limit({limit}).elementMap().toList()"
    )


def filtered_traversal(external_id: str, predicate: str, limit: int = 100) -> str:
    eid = _esc(external_id)
    p = _esc(predicate)
    return (
        f"graph.traversal().V().has('external_id','{eid}')"
        f".outE('{p}').inV().dedup().limit({limit}).elementMap().toList()"
    )


def count_by_type(tenant_id: str) -> str:
    t = _esc(tenant_id)
    return (
        f"graph.traversal().V().has('tenant_id','{t}')"
        f".groupCount().by('entity_type')"
    )


def top_connected(tenant_id: str, limit: int = 10) -> str:
    t = _esc(tenant_id)
    return (
        f"graph.traversal().V().has('tenant_id','{t}')"
        f".project('name','degree').by('name').by(bothE().count())"
        f".order().by(select('degree'), desc).limit({limit}).toList()"
    )


def tenant_isolation(tenant_a: str, tenant_b: str) -> str:
    a = _esc(tenant_a)
    b = _esc(tenant_b)
    return (
        f"graph.traversal().V().has('tenant_id','{a}')"
        f".has('tenant_id','{b}').count()"
    )


def vertex_count(tenant_id: str | None = None) -> str:
    if tenant_id:
        t = _esc(tenant_id)
        return f"graph.traversal().V().has('tenant_id','{t}').count()"
    return "graph.traversal().V().count()"


def edge_count() -> str:
    return "graph.traversal().E().count()"


def memory_stats() -> str:
    return (
        "rt = Runtime.getRuntime();"
        "[heapMax: rt.maxMemory(), heapUsed: rt.totalMemory() - rt.freeMemory(),"
        " heapFree: rt.freeMemory()]"
    )


def profile_point_lookup(external_id: str) -> str:
    """Return a profile()-enabled point lookup to verify index usage."""
    eid = _esc(external_id)
    return (
        f"graph.traversal().V().has('external_id','{eid}')"
        f".profile().toList()"
    )


def profile_tenant_scan(tenant_id: str) -> str:
    eid = _esc(tenant_id)
    return (
        f"graph.traversal().V().has('tenant_id','{eid}')"
        f".limit(1).profile().toList()"
    )
