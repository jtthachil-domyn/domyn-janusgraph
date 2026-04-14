"""GraphTransformer — converts raw Gremlin results into the canonical GraphResponse.

Handles: valueMap, elementMap, path, project, and mixed result types.
Computes canonical edge IDs, flattens JanusGraph property maps, sets hydration flags.
"""

from __future__ import annotations

from typing import Any

from app.models import GraphEdge, GraphMeta, GraphNode, GraphResponse


def _str_id(raw_id: Any) -> str:
    """Stringify any JanusGraph ID (long, RelationIdentifier, etc.)."""
    return str(raw_id)


def _normalize_keys(d: dict) -> dict[str, Any]:
    """Convert all dict keys to strings — handles TinkerPop T enum keys."""
    result: dict[str, Any] = {}
    for k, v in d.items():
        str_key = str(k)
        if hasattr(k, "name"):
            str_key = k.name
        result[str_key] = v
    return result


def _flatten_properties(props: dict[str, Any]) -> dict[str, Any]:
    """Flatten JanusGraph's nested property format.

    JanusGraph valueMap returns {key: [value]} — unwrap single-element lists.
    elementMap returns {key: value} directly.
    TinkerPop T enum keys (T.id, T.label) are converted to strings first.
    """
    normalized = _normalize_keys(props)
    flat: dict[str, Any] = {}
    for k, v in normalized.items():
        if k in ("id", "T.id", "T.label", "label", "1", "4"):
            continue
        if isinstance(v, list) and len(v) == 1:
            flat[k] = v[0]
        else:
            flat[k] = v
    return flat


def _canonical_edge_id(source: str, label: str, target: str) -> str:
    return f"{source}-{label}-{target}"


def _extract_node_from_map(m: dict[str, Any], hydrated: bool = False) -> GraphNode | None:
    """Extract a GraphNode from an elementMap or valueMap result."""
    nm = _normalize_keys(m)
    raw_id = nm.get("id") or nm.get("T.id")
    if raw_id is None:
        return None

    node_id = _str_id(raw_id)
    label = nm.get("label") or nm.get("T.label") or "unknown"

    props = _flatten_properties(m)
    node_type = props.pop("type", label)
    node_label = props.pop("name", node_type)

    return GraphNode(
        id=node_id,
        label=node_label,
        type=node_type,
        hydrated=hydrated,
        properties=props,
    )


def _extract_edge_from_map(m: dict[str, Any], direction: str = "both") -> GraphEdge | None:
    """Extract a GraphEdge from an elementMap result."""
    nm = _normalize_keys(m)
    raw_id = nm.get("id") or nm.get("T.id")
    edge_label = nm.get("label") or nm.get("T.label") or "RELATED"

    in_v = nm.get("IN") or nm.get("inV")
    out_v = nm.get("OUT") or nm.get("outV")

    if in_v is None or out_v is None:
        return None

    if isinstance(in_v, dict):
        target = _str_id(in_v.get("id") or in_v.get("T.id"))
    else:
        target = _str_id(in_v)

    if isinstance(out_v, dict):
        source = _str_id(out_v.get("id") or out_v.get("T.id"))
    else:
        source = _str_id(out_v)

    props = _flatten_properties(m)
    canonical_id = _canonical_edge_id(source, edge_label, target)

    return GraphEdge(
        id=canonical_id,
        source=source,
        target=target,
        label=edge_label,
        direction=direction,
        properties=props,
    )


def _is_vertex_map(item: dict) -> bool:
    """Heuristic: vertex maps have id/T.id but no IN/OUT keys."""
    nm = _normalize_keys(item)
    has_id = "id" in nm or "T.id" in nm
    has_edge_keys = "IN" in nm or "OUT" in nm or "inV" in nm or "outV" in nm
    return has_id and not has_edge_keys


def _is_edge_map(item: dict) -> bool:
    nm = _normalize_keys(item)
    has_id = "id" in nm or "T.id" in nm
    has_edge_keys = "IN" in nm or "OUT" in nm or "inV" in nm or "outV" in nm
    return has_id and has_edge_keys


def _process_path(
    path_obj: Any,
    nodes: dict[str, GraphNode],
    edges: dict[str, GraphEdge],
    direction: str,
) -> None:
    """Process a Gremlin Path result — pairwise (v, e, v) iteration.

    Path objects contain .objects list: [v1, e1, v2, e2, v3, ...]
    We iterate deterministically in pairs to extract vertices and edges.
    """
    objects = getattr(path_obj, "objects", None)
    if objects is None:
        if isinstance(path_obj, (list, tuple)):
            objects = list(path_obj)
        else:
            return

    prev_vertex_id: str | None = None
    for item in objects:
        if isinstance(item, dict):
            if _is_edge_map(item):
                edge = _extract_edge_from_map(item, direction)
                if edge and edge.id not in edges:
                    edges[edge.id] = edge
            elif _is_vertex_map(item):
                node = _extract_node_from_map(item)
                if node and node.id not in nodes:
                    nodes[node.id] = node
                if node:
                    prev_vertex_id = node.id
            else:
                node = _extract_node_from_map(item)
                if node and node.id not in nodes:
                    nodes[node.id] = node
                if node:
                    prev_vertex_id = node.id
        elif hasattr(item, "id") and hasattr(item, "label"):
            item_id = _str_id(item.id)
            if hasattr(item, "inVertex") or hasattr(item, "outVertex"):
                src = _str_id(item.outVertex.id) if hasattr(item, "outVertex") else prev_vertex_id
                tgt = _str_id(item.inVertex.id) if hasattr(item, "inVertex") else None
                if src and tgt:
                    cid = _canonical_edge_id(src, item.label, tgt)
                    if cid not in edges:
                        edges[cid] = GraphEdge(
                            id=cid,
                            source=src,
                            target=tgt,
                            label=item.label,
                            direction=direction,
                            properties={},
                        )
            else:
                if item_id not in nodes:
                    lbl = item.label if hasattr(item, "label") else "unknown"
                    nodes[item_id] = GraphNode(
                        id=item_id,
                        label=lbl,
                        type=lbl,
                        hydrated=False,
                        properties={},
                    )
                prev_vertex_id = item_id


def _process_project(
    item: dict[str, Any],
    nodes: dict[str, GraphNode],
) -> None:
    """Process a project() result — typically {name: X, type: Y, ...}."""
    nm = _normalize_keys(item)
    if "name" in nm and ("type" in nm or "label" in nm):
        node_id = _str_id(nm.get("id", nm.get("name", "")))
        if node_id and node_id not in nodes:
            props = {k: v for k, v in nm.items() if k not in ("id", "name", "type", "label")}
            nodes[node_id] = GraphNode(
                id=node_id,
                label=nm.get("name", ""),
                type=nm.get("type", nm.get("label", "unknown")),
                hydrated=False,
                properties=props,
            )


def gremlin_to_g6(
    results: list[Any],
    *,
    hydrated: bool = False,
    direction: str = "both",
    limit: int | None = None,
) -> GraphResponse:
    """Transform raw Gremlin results into the canonical GraphResponse.

    Args:
        results: Raw list from client.submit(...).all().result()
        hydrated: Whether these results should be marked as fully hydrated
        direction: Default edge direction ("out", "in", "both")
        limit: Max nodes to include (explosion guard)
    """
    nodes: dict[str, GraphNode] = {}
    edges: dict[str, GraphEdge] = {}

    for item in results:
        if hasattr(item, "objects"):
            _process_path(item, nodes, edges, direction)
        elif isinstance(item, dict):
            if _is_edge_map(item):
                edge = _extract_edge_from_map(item, direction)
                if edge and edge.id not in edges:
                    edges[edge.id] = edge
            elif _is_vertex_map(item):
                node = _extract_node_from_map(item, hydrated=hydrated)
                if node and node.id not in nodes:
                    nodes[node.id] = node
            elif "name" in _normalize_keys(item):
                _process_project(item, nodes)
            else:
                node = _extract_node_from_map(item, hydrated=hydrated)
                if node:
                    if node.id not in nodes:
                        nodes[node.id] = node
        elif isinstance(item, (list, tuple)):
            _process_path(item, nodes, edges, direction)
        elif hasattr(item, "id") and hasattr(item, "label"):
            item_id = _str_id(item.id)
            if item_id not in nodes:
                lbl = item.label if hasattr(item, "label") else "unknown"
                nodes[item_id] = GraphNode(
                    id=item_id,
                    label=lbl,
                    type=lbl,
                    hydrated=hydrated,
                    properties={},
                )

    truncated = False
    if limit and len(nodes) > limit:
        truncated = True
        node_ids = list(nodes.keys())[:limit]
        node_set = set(node_ids)
        nodes = {nid: nodes[nid] for nid in node_ids}
        edges = {
            eid: e
            for eid, e in edges.items()
            if e.source in node_set and e.target in node_set
        }

    return GraphResponse(
        nodes=list(nodes.values()),
        edges=list(edges.values()),
        meta=GraphMeta(
            total_nodes=len(nodes),
            total_edges=len(edges),
            truncated=truncated,
        ),
    )
