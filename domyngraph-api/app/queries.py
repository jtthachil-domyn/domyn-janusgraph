"""Gremlin query builders for DomynGraph Lens API.

Tenant is ALWAYS enforced inside the Gremlin traversal, not just at the API layer.
"""

from __future__ import annotations


def expand_query(
    vertex_id: str,
    tenant: str,
    depth: int = 1,
    limit: int = 50,
    edge_types: str | None = None,
) -> str:
    """Build a Gremlin query that expands neighbors from a vertex.

    Returns elementMap of center vertex + neighbors + connecting edges.
    """
    edge_filter = ""
    if edge_types:
        types = ",".join(f"'{t.strip()}'" for t in edge_types.split(","))
        edge_filter = f".hasLabel({types})"

    return f"""
t = graph.traversal()
center = t.V({vertex_id}).has('tenant_id', '{tenant}').next()
neighbors = t.V(center).bothE(){edge_filter}.otherV().has('tenant_id', '{tenant}').dedup().limit({limit}).toList()
edges = t.V(center).bothE(){edge_filter}.has('tenant_id', '{tenant}').or().where(__.otherV().has('tenant_id', '{tenant}')).dedup().limit({limit * 2}).elementMap().toList()
nodes = neighbors.collect {{ it -> t.V(it).elementMap().next() }}
centerMap = t.V(center).elementMap().next()
nodes.add(0, centerMap)
[nodes: nodes, edges: edges]
"""


def expand_center_query(vertex_id: str, tenant: str) -> str:
    return f"graph.traversal().V({vertex_id}).has('tenant_id', '{tenant}').elementMap().toList()"


def expand_edges_query(
    vertex_id: str,
    tenant: str,
    limit: int = 50,
    edge_types: str | None = None,
) -> str:
    edge_step = "bothE()"
    if edge_types:
        types = ",".join(f"'{t.strip()}'" for t in edge_types.split(","))
        edge_step = f"bothE({types})"
    return f"graph.traversal().V({vertex_id}).has('tenant_id', '{tenant}').{edge_step}.limit({limit * 2}).project('edgeLabel','sourceId','targetId','weight').by(label()).by(outV().id()).by(inV().id()).by(coalesce(values('weight'), constant(1.0))).toList()"


def expand_neighbors_query(
    vertex_id: str,
    tenant: str,
    limit: int = 50,
    edge_types: str | None = None,
) -> str:
    edge_step = "bothE()"
    if edge_types:
        types = ",".join(f"'{t.strip()}'" for t in edge_types.split(","))
        edge_step = f"bothE({types})"
    return f"graph.traversal().V({vertex_id}).has('tenant_id', '{tenant}').{edge_step}.limit({limit * 2}).otherV().has('tenant_id', '{tenant}').dedup().limit({limit}).elementMap().toList()"


def search_query(q: str, tenant: str, limit: int = 20) -> str:
    """Build a full-text search query using the ES mixed index."""
    escaped = q.replace("'", "\\\\'")
    return f"""
t = graph.traversal()
t.V().has('tenant_id', '{tenant}').has('name', textContains('{escaped}')).limit({limit}).elementMap().toList()
"""


def vertex_detail_query(vertex_id: str, tenant: str) -> str:
    """Full vertex detail — all properties."""
    return f"graph.traversal().V({vertex_id}).has('tenant_id', '{tenant}').elementMap().toList()"


def vertex_count_query(tenant: str) -> str:
    return f"graph.traversal().V().has('tenant_id', '{tenant}').count()"


def edge_count_query(tenant: str) -> str:
    return f"graph.traversal().V().has('tenant_id', '{tenant}').bothE().dedup().count()"


def list_procedures_query() -> str:
    return "org.janusgraph.domyn.procedures.ProcedureRegistry.getInstance().listNames()"


def introspect_procedure_query(name: str) -> str:
    escaped = name.replace("'", "\\\\'")
    return f"org.janusgraph.domyn.procedures.ProcedureRegistry.getInstance().introspect('{escaped}')"


def run_procedure_query(name: str, tenant: str, params: dict) -> str:
    escaped_name = name.replace("'", "\\\\'")
    escaped_tenant = tenant.replace("'", "\\\\'")
    args_entries = []
    for k, v in params.items():
        ek = k.replace("'", "\\\\'")
        if isinstance(v, str):
            ev = v.replace("'", "\\\\'")
            args_entries.append(f"'{ek}': '{ev}'")
        elif isinstance(v, bool):
            args_entries.append(f"'{ek}': {'true' if v else 'false'}")
        elif isinstance(v, (int, float)):
            args_entries.append(f"'{ek}': {v}")
        else:
            ev = str(v).replace("'", "\\\\'")
            args_entries.append(f"'{ek}': '{ev}'")
    args_map = "[" + ", ".join(args_entries) + "]" if args_entries else "[:]"
    return f"""
import org.janusgraph.domyn.procedures.*
ctx = ProcedureContext.create(graph, '{escaped_tenant}')
ProcedureRegistry.getInstance().call('{escaped_name}', ctx, {args_map})
"""


def schema_status_query() -> str:
    return "org.janusgraph.domyn.procedures.SchemaProcedures.indexStatus(graph)"


def schema_version_query() -> str:
    return 'graph.traversal().V().has("__type", "__schema_meta").values("schema_version")'


def health_check_query() -> str:
    return "graph.traversal().V().count()"


def algorithm_bfs_query(vertex_id: str, max_depth: int = 5, timeout_ms: int = 60000) -> str:
    return f"""
import org.janusgraph.domyn.algorithms.*
seedId = graph.traversal().V({vertex_id}).id().next()
program = BFSVertexProgram.build().seed((long) seedId).maxDepth({max_depth}).create(graph)
config = AlgorithmConfig.builder().timeoutMs({timeout_ms}).workerThreads(2).memoryLimitMb(512).build()
AlgorithmResourceManager.getInstance().execute(graph, program, config).toMap()
"""


def algorithm_pagerank_query(max_iterations: int = 20, timeout_ms: int = 60000) -> str:
    return f"""
import org.janusgraph.domyn.algorithms.*
program = DomynPageRankVertexProgram.build().iterations({max_iterations}).create(graph)
config = AlgorithmConfig.builder().timeoutMs({timeout_ms}).workerThreads(2).memoryLimitMb(512).build()
AlgorithmResourceManager.getInstance().execute(graph, program, config).toMap()
"""


def algorithm_cc_query(max_iterations: int = 20, timeout_ms: int = 60000) -> str:
    return f"""
import org.janusgraph.domyn.algorithms.*
program = ConnectedComponentsVertexProgram.build().maxIterations({max_iterations}).create(graph)
config = AlgorithmConfig.builder().timeoutMs({timeout_ms}).workerThreads(2).memoryLimitMb(512).build()
AlgorithmResourceManager.getInstance().execute(graph, program, config).toMap()
"""


def overview_nodes_query(tenant: str, limit: int = 50) -> str:
    return f"graph.traversal().V().has('tenant_id', '{tenant}').limit({limit}).elementMap().toList()"


def overview_edges_query(tenant: str, limit: int = 200) -> str:
    return f"graph.traversal().V().has('tenant_id', '{tenant}').outE().limit({limit}).project('edgeLabel','sourceId','targetId','weight').by(label()).by(outV().id()).by(inV().id()).by(coalesce(values('weight'), constant(1.0))).toList()"


def tenant_list_query() -> str:
    return "graph.traversal().V().has('tenant_id').values('tenant_id').dedup().toList()"
