"""Canonical data models for DomynGraph Lens API."""

from __future__ import annotations

from typing import Any, Optional

from pydantic import BaseModel, Field, model_validator


def _coerce_props(props: dict[str, Any]) -> dict[str, Any]:
    """Force all property values into JSON-safe types."""
    safe: dict[str, Any] = {}
    for k, v in props.items():
        if v is None or isinstance(v, (str, int, float, bool)):
            safe[k] = v
        elif isinstance(v, (list, tuple)):
            safe[k] = [_coerce_props({"_": i})["_"] if isinstance(i, dict) else _coerce_val(i) for i in v]
        elif isinstance(v, dict):
            safe[k] = _coerce_props(v)
        else:
            safe[k] = _coerce_val(v)
    return safe


def _coerce_val(v: Any) -> Any:
    if v is None or isinstance(v, (str, int, float, bool)):
        return v
    try:
        return float(str(v))
    except (ValueError, TypeError):
        return str(v)


class GraphNode(BaseModel):
    id: str
    label: str
    type: str
    hydrated: bool = False
    properties: dict[str, Any] = Field(default_factory=dict)

    @model_validator(mode="before")
    @classmethod
    def _sanitize_props(cls, values: Any) -> Any:
        if isinstance(values, dict) and "properties" in values:
            values["properties"] = _coerce_props(values["properties"])
        return values


class GraphEdge(BaseModel):
    id: str
    source: str
    target: str
    label: str
    direction: str = "both"
    properties: dict[str, Any] = Field(default_factory=dict)

    @model_validator(mode="before")
    @classmethod
    def _sanitize_props(cls, values: Any) -> Any:
        if isinstance(values, dict) and "properties" in values:
            values["properties"] = _coerce_props(values["properties"])
        return values


class GraphMeta(BaseModel):
    total_nodes: int = 0
    total_edges: int = 0
    truncated: bool = False
    cursor: Optional[str] = None


class GraphResponse(BaseModel):
    nodes: list[GraphNode] = Field(default_factory=list)
    edges: list[GraphEdge] = Field(default_factory=list)
    meta: GraphMeta = Field(default_factory=GraphMeta)
