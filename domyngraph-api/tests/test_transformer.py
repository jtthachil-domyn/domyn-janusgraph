"""Tests for GraphTransformer — covers all Gremlin result types."""

import sys
import os

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from app.transformer import gremlin_to_g6


class FakeVertex:
    def __init__(self, vid, label):
        self.id = vid
        self.label = label


class FakeEdge:
    def __init__(self, eid, label, out_v, in_v):
        self.id = eid
        self.label = label
        self.outVertex = out_v
        self.inVertex = in_v


class FakePath:
    def __init__(self, objects):
        self.objects = objects


def test_element_map_vertices():
    results = [
        {"id": 123, "label": "Entity", "name": "Apple", "type": "Company", "tenant_id": "demo"},
        {"id": 456, "label": "Entity", "name": "Google", "type": "Company", "tenant_id": "demo"},
    ]
    resp = gremlin_to_g6(results)
    assert resp.meta.total_nodes == 2
    assert resp.meta.total_edges == 0
    ids = {n.id for n in resp.nodes}
    assert "123" in ids
    assert "456" in ids
    apple = next(n for n in resp.nodes if n.id == "123")
    assert apple.label == "Apple"
    assert apple.type == "Company"


def test_element_map_edges():
    results = [
        {
            "id": "e-1",
            "label": "REFERENCES",
            "IN": {"id": 456, "label": "Concept"},
            "OUT": {"id": 123, "label": "Entity"},
            "weight": 0.9,
        }
    ]
    resp = gremlin_to_g6(results, direction="out")
    assert resp.meta.total_edges == 1
    edge = resp.edges[0]
    assert edge.source == "123"
    assert edge.target == "456"
    assert edge.label == "REFERENCES"
    assert edge.direction == "out"
    assert edge.id == "123-REFERENCES-456"


def test_value_map_unwraps_lists():
    results = [
        {"id": 100, "label": "Entity", "name": ["NVIDIA"], "type": ["Company"], "tenant_id": ["demo"]},
    ]
    resp = gremlin_to_g6(results)
    assert resp.meta.total_nodes == 1
    node = resp.nodes[0]
    assert node.label == "NVIDIA"
    assert node.type == "Company"
    assert node.properties["tenant_id"] == "demo"


def test_path_results_pairwise():
    v1 = {"id": 1, "label": "Entity", "name": "A", "type": "Person"}
    e1 = {"id": "e1", "label": "KNOWS", "IN": {"id": 2}, "OUT": {"id": 1}}
    v2 = {"id": 2, "label": "Entity", "name": "B", "type": "Person"}

    path = FakePath([v1, e1, v2])
    resp = gremlin_to_g6([path])
    assert resp.meta.total_nodes == 2
    assert resp.meta.total_edges == 1
    edge = resp.edges[0]
    assert edge.source == "1"
    assert edge.target == "2"
    assert edge.label == "KNOWS"


def test_dedup_nodes():
    results = [
        {"id": 1, "label": "Entity", "name": "Apple", "type": "Company"},
        {"id": 1, "label": "Entity", "name": "Apple", "type": "Company"},
        {"id": 2, "label": "Entity", "name": "Google", "type": "Company"},
    ]
    resp = gremlin_to_g6(results)
    assert resp.meta.total_nodes == 2


def test_dedup_edges():
    results = [
        {"id": "e1", "label": "REF", "IN": {"id": 2}, "OUT": {"id": 1}, "weight": 0.5},
        {"id": "e2", "label": "REF", "IN": {"id": 2}, "OUT": {"id": 1}, "weight": 0.5},
    ]
    resp = gremlin_to_g6(results)
    assert resp.meta.total_edges == 1
    assert resp.edges[0].id == "1-REF-2"


def test_limit_truncates():
    results = [
        {"id": i, "label": "Entity", "name": f"Node{i}", "type": "Thing"} for i in range(200)
    ]
    resp = gremlin_to_g6(results, limit=50)
    assert resp.meta.total_nodes == 50
    assert resp.meta.truncated is True


def test_hydrated_flag():
    results = [{"id": 1, "label": "Entity", "name": "X", "type": "Y"}]
    resp_light = gremlin_to_g6(results, hydrated=False)
    resp_full = gremlin_to_g6(results, hydrated=True)
    assert resp_light.nodes[0].hydrated is False
    assert resp_full.nodes[0].hydrated is True


def test_canonical_edge_id_format():
    results = [
        {"id": "x", "label": "OWNS", "IN": {"id": 99}, "OUT": {"id": 42}},
    ]
    resp = gremlin_to_g6(results)
    assert resp.edges[0].id == "42-OWNS-99"


def test_direction_preserved():
    results = [
        {"id": "x", "label": "WORKS_AT", "IN": {"id": 2}, "OUT": {"id": 1}},
    ]
    resp_out = gremlin_to_g6(results, direction="out")
    resp_in = gremlin_to_g6(results, direction="in")
    assert resp_out.edges[0].direction == "out"
    assert resp_in.edges[0].direction == "in"


def test_empty_results():
    resp = gremlin_to_g6([])
    assert resp.meta.total_nodes == 0
    assert resp.meta.total_edges == 0
    assert resp.nodes == []
    assert resp.edges == []


def test_project_results():
    results = [
        {"name": "Alice", "type": "Person", "age": 30},
        {"name": "Bob", "type": "Person", "age": 25},
    ]
    resp = gremlin_to_g6(results)
    assert resp.meta.total_nodes == 2
    names = {n.label for n in resp.nodes}
    assert "Alice" in names
    assert "Bob" in names


def test_fake_vertex_objects():
    v1 = FakeVertex(10, "Entity")
    v2 = FakeVertex(20, "Concept")
    resp = gremlin_to_g6([v1, v2])
    assert resp.meta.total_nodes == 2
    ids = {n.id for n in resp.nodes}
    assert "10" in ids
    assert "20" in ids


def test_mixed_results():
    results = [
        {"id": 1, "label": "Entity", "name": "A", "type": "X"},
        {"id": "e1", "label": "LINK", "IN": {"id": 2}, "OUT": {"id": 1}},
        {"id": 2, "label": "Entity", "name": "B", "type": "Y"},
    ]
    resp = gremlin_to_g6(results)
    assert resp.meta.total_nodes == 2
    assert resp.meta.total_edges == 1
