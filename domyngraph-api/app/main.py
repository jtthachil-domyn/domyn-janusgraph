"""DomynGraph Lens — FastAPI Application.

The middleware layer between the React UI and JanusGraph/DomynGraph Engine.
"""

from __future__ import annotations

import logging
import time
from typing import Any, Optional

from fastapi import FastAPI, HTTPException, Query, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse
from pydantic import BaseModel

from app.cache import cache_get, cache_set, cache_stats, invalidate_tenant
from app.config import settings
from app.gremlin_service import GremlinError, gremlin_service
from app.jobs import JobStatus, get_job, list_jobs, run_algorithm_job
from app.models import GraphResponse
from app.queries import (
    _tenant_filter,
    algorithm_bfs_query,
    algorithm_cc_query,
    algorithm_pagerank_query,
    edge_count_query,
    expand_center_query,
    expand_edges_query,
    expand_neighbors_query,
    health_check_query,
    introspect_procedure_query,
    list_procedures_query,
    run_procedure_query,
    schema_status_query,
    schema_version_query,
    overview_edges_query,
    overview_nodes_query,
    search_query,
    tenant_list_query,
    vertex_count_query,
    vertex_detail_query,
)
from app.transformer import gremlin_to_g6

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(name)s %(levelname)s %(message)s")
logger = logging.getLogger("domyngraph.api")

app = FastAPI(
    title="DomynGraph Lens API",
    description="Middleware API for DomynGraph Lens — graph exploration, procedures, algorithms, admin.",
    version="0.1.0",
)

app.add_middleware(
    CORSMiddleware,
    allow_origins=[settings.CORS_ORIGIN, "http://localhost:5173"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)


@app.exception_handler(GremlinError)
async def gremlin_error_handler(request: Request, exc: GremlinError):
    return JSONResponse(
        status_code=exc.status_code,
        content={"error": str(exc), "detail": exc.raw[:300] if exc.raw else None},
    )


# ──────────────────────────────────────────────────
#  Graph Exploration
# ──────────────────────────────────────────────────


@app.get("/api/graph/expand", response_model=GraphResponse)
async def graph_expand(
    vertex_id: str = Query(..., description="JanusGraph vertex ID"),
    tenant: str = Query(..., min_length=1, description="Tenant ID"),
    depth: int = Query(1, ge=1, le=settings.MAX_EXPAND_DEPTH),
    limit: int = Query(50, ge=1, le=settings.MAX_EXPAND_NODES),
    edge_types: Optional[str] = Query(None, description="Comma-separated edge labels to filter"),
):
    cached = cache_get(
        tenant, op="expand", vertex_id=vertex_id, depth=depth, limit=limit, edge_types=edge_types
    )
    if cached is not None:
        return cached

    from app.models import GraphNode as GN, GraphEdge as GE, GraphMeta as GM

    center_result = await gremlin_service.submit_async(
        expand_center_query(vertex_id, tenant), timeout_s=settings.QUERY_TIMEOUT_S
    )
    edge_result = await gremlin_service.submit_async(
        expand_edges_query(vertex_id, tenant, limit=limit, edge_types=edge_types),
        timeout_s=settings.QUERY_TIMEOUT_S,
    )
    neighbor_result = await gremlin_service.submit_async(
        expand_neighbors_query(vertex_id, tenant, limit=limit, edge_types=edge_types),
        timeout_s=settings.QUERY_TIMEOUT_S,
    )

    center_resp = gremlin_to_g6(
        center_result if isinstance(center_result, list) else [center_result],
        direction="both",
    )
    neighbor_resp = gremlin_to_g6(neighbor_result, direction="both")

    edges_list: list[GE] = []
    for ed in edge_result:
        if isinstance(ed, dict):
            src = str(ed.get("sourceId", ""))
            tgt = str(ed.get("targetId", ""))
            lbl = ed.get("edgeLabel", "RELATED")
            raw_wt = ed.get("weight", 1.0)
            wt = float(raw_wt) if hasattr(raw_wt, "__float__") else raw_wt
            canonical_id = f"{src}-{lbl}-{tgt}"
            edges_list.append(GE(
                id=canonical_id, source=src, target=tgt,
                label=lbl, direction="out", properties={"weight": wt},
            ))

    seen_nodes: dict[str, GN] = {}
    for n in center_resp.nodes + neighbor_resp.nodes:
        if n.id not in seen_nodes:
            seen_nodes[n.id] = n
    seen_edges: dict[str, GE] = {}
    for e in edges_list:
        if e.id not in seen_edges:
            seen_edges[e.id] = e

    truncated = len(seen_nodes) > limit
    if truncated:
        ids = list(seen_nodes.keys())[:limit]
        seen_nodes = {i: seen_nodes[i] for i in ids}

    response = GraphResponse(
        nodes=list(seen_nodes.values()),
        edges=list(seen_edges.values()),
        meta=GM(
            total_nodes=len(seen_nodes),
            total_edges=len(seen_edges),
            truncated=truncated,
        ),
    )

    cache_set(
        tenant, response, op="expand", vertex_id=vertex_id, depth=depth, limit=limit, edge_types=edge_types
    )
    return response


@app.get("/api/graph/search", response_model=GraphResponse)
async def graph_search(
    q: str = Query(..., min_length=2, description="Search query"),
    tenant: str = Query(..., min_length=1),
    limit: int = Query(20, ge=1, le=100),
    cursor: Optional[str] = Query(None),
):
    cached = cache_get(tenant, op="search", q=q, limit=limit, cursor=cursor)
    if cached is not None:
        return cached

    query = search_query(q, tenant, limit=limit)
    results = await gremlin_service.submit_async(query, timeout_s=settings.QUERY_TIMEOUT_S)
    response = gremlin_to_g6(results, hydrated=False, limit=limit)

    cache_set(tenant, response, op="search", q=q, limit=limit, cursor=cursor)
    return response


@app.get("/api/graph/vertex/{vertex_id}")
async def graph_vertex_detail(
    vertex_id: str,
    tenant: str = Query(..., min_length=1),
):
    query = vertex_detail_query(vertex_id, tenant)
    results = await gremlin_service.submit_async(query, timeout_s=settings.QUERY_TIMEOUT_S)
    if not results:
        raise HTTPException(status_code=404, detail="Vertex not found")
    response = gremlin_to_g6(results if isinstance(results, list) else [results], hydrated=True)
    if not response.nodes:
        raise HTTPException(status_code=404, detail="Vertex not found for this tenant")
    return response.nodes[0]


@app.get("/api/graph/overview", response_model=GraphResponse)
async def graph_overview(
    tenant: str = Query(..., min_length=1),
    limit: int = Query(5000, ge=1, le=50000),
):
    """Load an overview of the graph — all vertices + edges for a tenant (up to limit)."""
    from app.models import GraphEdge as GE, GraphMeta as GM

    effective_timeout = max(settings.QUERY_TIMEOUT_S, 60)
    node_results = await gremlin_service.submit_async(
        overview_nodes_query(tenant, limit), timeout_s=effective_timeout
    )
    node_resp = gremlin_to_g6(node_results, direction="both")
    node_ids = {n.id for n in node_resp.nodes}

    if not node_ids:
        return GraphResponse(nodes=[], edges=[], meta=GM())

    tf = _tenant_filter(tenant)
    edge_limit = min(limit * 4, 200000)
    edge_query = (
        f"graph.traversal().V(){tf}.outE().limit({edge_limit})"
        f".project('edgeLabel','sourceId','targetId','weight')"
        f".by(label()).by(outV().id()).by(inV().id())"
        f".by(coalesce(values('weight'), constant(1.0))).toList()"
    )
    edge_results = await gremlin_service.submit_async(
        edge_query, timeout_s=max(settings.QUERY_TIMEOUT_S, 60)
    )

    edges_list: list[GE] = []
    for ed in edge_results:
        if isinstance(ed, dict):
            src = str(ed.get("sourceId", ""))
            tgt = str(ed.get("targetId", ""))
            if src not in node_ids or tgt not in node_ids:
                continue
            lbl = ed.get("edgeLabel", "RELATED")
            raw_wt = ed.get("weight", 1.0)
            wt = float(raw_wt) if hasattr(raw_wt, "__float__") else raw_wt
            canonical_id = f"{src}-{lbl}-{tgt}"
            edges_list.append(GE(
                id=canonical_id, source=src, target=tgt,
                label=lbl, direction="out", properties={"weight": wt},
            ))

    return GraphResponse(
        nodes=node_resp.nodes,
        edges=edges_list,
        meta=GM(total_nodes=len(node_resp.nodes), total_edges=len(edges_list)),
    )


# ──────────────────────────────────────────────────
#  Procedures
# ──────────────────────────────────────────────────


@app.get("/api/procedures")
async def list_procedures_endpoint():
    results = await gremlin_service.submit_async(list_procedures_query())
    return {"procedures": results}


class ProcedureRunRequest(BaseModel):
    name: str
    params: dict[str, Any] = {}
    tenant: str


@app.post("/api/procedures/run")
async def run_procedure(req: ProcedureRunRequest):
    query = run_procedure_query(req.name, req.tenant, req.params)
    results = await gremlin_service.submit_async(query, timeout_s=settings.PROCEDURE_TIMEOUT_S)
    return {"procedure": req.name, "results": results}


# ──────────────────────────────────────────────────
#  Algorithms (Async)
# ──────────────────────────────────────────────────


class AlgorithmRunRequest(BaseModel):
    algorithm: str
    params: dict[str, Any] = {}
    tenant: str
    config: dict[str, Any] = {}


@app.post("/api/algorithms/run")
async def run_algorithm(req: AlgorithmRunRequest):
    timeout_ms = req.config.get("timeout_ms", 60000)
    max_iter = req.config.get("max_iterations", 20)

    if req.algorithm == "bfs":
        vertex_id = req.params.get("vertex_id")
        if not vertex_id:
            raise HTTPException(400, "BFS requires params.vertex_id")
        max_depth = req.params.get("max_depth", 5)
        query = algorithm_bfs_query(vertex_id, max_depth=max_depth, timeout_ms=timeout_ms)
    elif req.algorithm == "pagerank":
        query = algorithm_pagerank_query(max_iterations=max_iter, timeout_ms=timeout_ms)
    elif req.algorithm == "connected_components":
        query = algorithm_cc_query(max_iterations=max_iter, timeout_ms=timeout_ms)
    else:
        raise HTTPException(400, f"Unknown algorithm: {req.algorithm}")

    def transform(results):
        raw = results[0] if isinstance(results, list) and results else results
        enriched: dict[str, Any] = {"algorithm": req.algorithm}
        if isinstance(raw, dict):
            enriched.update(raw)
        elif isinstance(raw, str):
            enriched["output"] = raw
        else:
            enriched["output"] = str(raw)
        enriched.setdefault("executionTimeMs", None)
        enriched.setdefault("verticesProcessed", None)
        return enriched

    job_id = await run_algorithm_job(
        algorithm=req.algorithm,
        gremlin_query=query,
        submit_fn=gremlin_service.submit_async,
        transform_fn=transform,
    )
    return {"job_id": job_id, "status": "running"}


@app.get("/api/algorithms/{job_id}")
async def get_algorithm_status(job_id: str):
    job = get_job(job_id)
    if job is None:
        raise HTTPException(404, "Job not found")
    return job.to_dict()


@app.get("/api/algorithms")
async def list_algorithm_jobs():
    return {"jobs": list_jobs()}


# ──────────────────────────────────────────────────
#  Schema
# ──────────────────────────────────────────────────


@app.get("/api/schema/status")
async def schema_status():
    index_results = await gremlin_service.submit_async(schema_status_query())
    version_results = await gremlin_service.submit_async(schema_version_query())
    return {
        "indexes": index_results,
        "schema_version": version_results[0] if version_results else None,
    }


# ──────────────────────────────────────────────────
#  Tenants
# ──────────────────────────────────────────────────


@app.get("/api/tenants")
async def list_tenants():
    from app.queries import ALL_TENANT

    tenant_ids = await gremlin_service.submit_async(tenant_list_query())
    tenants = []

    total_vc = 0
    total_ec = 0
    for tid in (tenant_ids if isinstance(tenant_ids, list) else []):
        try:
            vc = await gremlin_service.submit_async(vertex_count_query(str(tid)))
            ec = await gremlin_service.submit_async(edge_count_query(str(tid)))
            v = vc[0] if vc else 0
            e = ec[0] if ec else 0
            total_vc += v
            total_ec += e
            tenants.append({"id": str(tid), "vertex_count": v, "edge_count": e})
        except Exception:
            tenants.append({"id": str(tid), "vertex_count": 0, "edge_count": 0})

    tenants.insert(0, {"id": ALL_TENANT, "vertex_count": total_vc, "edge_count": total_ec})
    return {"tenants": tenants}


@app.get("/api/tenants/{tenant_id}/stats")
async def tenant_stats(tenant_id: str):
    vc = await gremlin_service.submit_async(vertex_count_query(tenant_id))
    ec = await gremlin_service.submit_async(edge_count_query(tenant_id))
    return {
        "id": tenant_id,
        "vertex_count": vc[0] if vc else 0,
        "edge_count": ec[0] if ec else 0,
    }


# ──────────────────────────────────────────────────
#  Health + Metrics
# ──────────────────────────────────────────────────


@app.get("/api/health")
async def health():
    checks = {"api": "ok", "gremlin": "unknown", "cache": cache_stats()}
    try:
        result = await gremlin_service.submit_async(health_check_query(), timeout_s=5)
        checks["gremlin"] = "ok"
        checks["vertex_count"] = result[0] if result else 0
    except Exception as exc:
        checks["gremlin"] = f"error: {str(exc)[:100]}"
    return checks


@app.get("/api/cache/stats")
async def get_cache_stats():
    return cache_stats()


# ──────────────────────────────────────────────────
#  Gremlin Query Console
# ──────────────────────────────────────────────────


class GremlinQueryRequest(BaseModel):
    query: str
    timeout_s: int = 30


@app.post("/api/gremlin/query")
async def gremlin_query(req: GremlinQueryRequest):
    """Execute a raw Gremlin query and return the results.

    Restricted to read-only traversals in practice -- the UI should warn
    users about mutations, but we don't enforce it server-side to keep the
    console useful for debugging.
    """
    if not req.query.strip():
        raise HTTPException(status_code=400, detail="Query cannot be empty")
    if len(req.query) > 5000:
        raise HTTPException(status_code=400, detail="Query too long (max 5000 chars)")

    query = req.query.strip()
    if query.startswith("g."):
        query = "graph.traversal()." + query[2:]

    t0 = time.time()
    try:
        result = await gremlin_service.submit_async(
            query, timeout_s=min(req.timeout_s, 60)
        )
        elapsed = round((time.time() - t0) * 1000, 1)
        serializable = _make_serializable(result)
        return {
            "result": serializable,
            "count": len(serializable) if isinstance(serializable, list) else 1,
            "elapsed_ms": elapsed,
        }
    except Exception as exc:
        elapsed = round((time.time() - t0) * 1000, 1)
        raise HTTPException(
            status_code=400,
            detail={"error": str(exc)[:500], "elapsed_ms": elapsed},
        )


def _make_serializable(obj: Any) -> Any:
    """Convert Gremlin result objects into JSON-safe structures."""
    if obj is None:
        return None
    if isinstance(obj, (str, int, float, bool)):
        return obj
    if isinstance(obj, dict):
        return {str(k): _make_serializable(v) for k, v in obj.items()}
    if isinstance(obj, (list, tuple)):
        return [_make_serializable(item) for item in obj]
    return str(obj)
