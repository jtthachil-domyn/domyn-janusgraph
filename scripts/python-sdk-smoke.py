#!/usr/bin/env python3
"""Smoke test for the packaged Domyn Nexus Python SDK wheel."""

from __future__ import annotations

import shutil
import tempfile
from pathlib import Path

import domyn_nexus as nx


def assert_true(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def main() -> None:
    for name in [
        "NexusError",
        "CypherError",
        "StorageError",
        "SchemaError",
        "VectorError",
    ]:
        assert_true(hasattr(nx, name), f"missing exception class: {name}")

    graph = nx.Graph(vertex_capacity=16, edge_capacity=32)
    graph.register_vertex_property("name", "string", indexed=True)
    apple = graph.add_vertex("Entity")
    graph.set_property(apple, "name", "Apple")
    graph.build()

    rows = graph.cypher(
        "MATCH (n:Entity) WHERE n.name = $name RETURN n.name AS name",
        {"name": "Apple"},
    ).to_dicts()
    assert_true(rows == [{"name": "Apple"}], f"unexpected in-memory rows: {rows}")

    list_rows = graph.cypher(
        "UNWIND $names AS name RETURN name",
        {"names": ["Apple", "Beta"]},
    ).to_dicts()
    assert_true(
        list_rows == [{"name": "Apple"}, {"name": "Beta"}],
        f"unexpected list-param rows: {list_rows}",
    )

    try:
        graph.cypher("MATCH (n) RETURN n CALL db.labels()")
    except nx.CypherError:
        pass
    else:
        raise AssertionError("unsupported Cypher should raise CypherError")

    try:
        graph.save_snapshot()
    except nx.StorageError:
        pass
    else:
        raise AssertionError("in-memory save_snapshot should raise StorageError")

    root = Path(tempfile.mkdtemp(prefix="domyn-nexus-sdk-smoke-"))
    try:
        data_dir = root / "data"
        backup_dir = root / "backup"
        persistent = nx.Graph.open(str(data_dir))
        result = persistent.cypher(
            "CREATE (n:Entity {name: $name}) RETURN n.name AS name",
            {"name": "Beta"},
        ).to_dicts()
        assert_true(result == [{"name": "Beta"}], f"unexpected create result: {result}")

        chunk = persistent.cypher(
            "CREATE (c:Chunk {id: 'chunk-1'}) RETURN c",
        ).to_dicts()[0]["c"]
        named_vectors = persistent.vector_index("chunks", 3)
        named_vectors.upsert(chunk, [0.2, 0.3, 0.4])
        named_hits = named_vectors.search([0.2, 0.3, 0.4], k=1)
        assert_true(
            named_hits and named_hits[0][0] == chunk,
            f"unexpected named vector result: {named_hits}",
        )
        persistent.save_snapshot()

        reopened = nx.Graph.open(str(data_dir))
        rows = reopened.cypher(
            "MATCH (n:Entity) WHERE n.name = $name RETURN n.name AS name",
            {"name": "Beta"},
        ).to_dicts()
        assert_true(rows == [{"name": "Beta"}], f"unexpected reopened rows: {rows}")

        reopened_vectors = reopened.vector_index("chunks", 3)
        reopened_hits = reopened_vectors.search([0.2, 0.3, 0.4], k=1)
        assert_true(
            reopened_hits and reopened_hits[0][0] == chunk,
            f"unexpected reopened vector result: {reopened_hits}",
        )

        manifest = reopened.backup(str(backup_dir))
        assert_true("files" in manifest, f"backup manifest missing files: {manifest}")
        assert_true(
            (backup_dir / "backup-manifest.json").exists(),
            "backup manifest file was not written",
        )
    finally:
        shutil.rmtree(root, ignore_errors=True)

    vectors = nx.VectorIndex(dimension=3)
    vectors.add(apple, [0.1, 0.2, 0.3])
    nearest = vectors.search([0.1, 0.2, 0.3], k=1)
    assert_true(nearest and nearest[0][0] == apple, f"unexpected vector result: {nearest}")

    try:
        vectors.search([0.1, 0.2], k=1)
    except nx.VectorError:
        pass
    else:
        raise AssertionError("dimension mismatch should raise VectorError")

    print("python sdk smoke passed")


if __name__ == "__main__":
    main()
