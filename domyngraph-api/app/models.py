"""Canonical data models for DomynGraph Lens API."""

from __future__ import annotations

from typing import Any, Optional

from pydantic import BaseModel, Field


class GraphNode(BaseModel):
    id: str
    label: str
    type: str
    hydrated: bool = False
    properties: dict[str, Any] = Field(default_factory=dict)


class GraphEdge(BaseModel):
    id: str
    source: str
    target: str
    label: str
    direction: str = "both"
    properties: dict[str, Any] = Field(default_factory=dict)


class GraphMeta(BaseModel):
    total_nodes: int = 0
    total_edges: int = 0
    truncated: bool = False
    cursor: Optional[str] = None


class GraphResponse(BaseModel):
    nodes: list[GraphNode] = Field(default_factory=list)
    edges: list[GraphEdge] = Field(default_factory=list)
    meta: GraphMeta = Field(default_factory=GraphMeta)
