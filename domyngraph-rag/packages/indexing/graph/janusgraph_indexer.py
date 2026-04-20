"""JanusGraph graph indexer — writes KG triplets into JanusGraph via Gremlin."""

from typing import List, Dict, Any, Optional
from pathlib import Path
import json
import hashlib
import logging
import pandas as pd

from gremlin_python.driver.client import Client
from gremlin_python.driver.serializer import GraphSONSerializersV3d0

logger = logging.getLogger("domyngraph-rag.indexing.janusgraph")


class JanusGraphIndexer:
    """Index knowledge graph triplets into JanusGraph using Gremlin WebSocket."""

    def __init__(
        self,
        gremlin_url: str = "ws://localhost:8182/gremlin",
        graph_alias: str = "graph",
        tenant_id: str = "default",
        csv_output_dir: str = "graph_rag_output",
    ):
        self.gremlin_url = gremlin_url
        self.graph_alias = graph_alias
        self.tenant_id = tenant_id
        self.csv_output_dir = Path(csv_output_dir)
        self.csv_output_dir.mkdir(parents=True, exist_ok=True)
        self.csv_file_path = self.csv_output_dir / "chunk_text.csv"

        self._client: Optional[Client] = None
        self._csv_data: List[Dict[str, str]] = []

    @property
    def client(self) -> Client:
        if self._client is None:
            self._client = Client(
                self.gremlin_url,
                self.graph_alias,
                message_serializer=GraphSONSerializersV3d0(),
            )
        return self._client

    def _submit(self, query: str) -> list:
        try:
            rs = self.client.submit(query)
            return rs.all().result()
        except Exception as e:
            logger.error("Gremlin query failed: %s\nQuery: %s", e, query[:200])
            raise

    @staticmethod
    def _escape(value: str) -> str:
        return value.replace("\\", "\\\\").replace("'", "\\'").replace("\n", " ").replace("\r", "")

    def _get_or_create_vertex(self, name: str, vertex_label: str, entity_type: str, extra_props: Optional[Dict] = None) -> str:
        """Upsert a vertex by external_id, return the query to get its id."""
        eid = hashlib.sha256(f"{name}:{vertex_label}:{self.tenant_id}".encode()).hexdigest()[:20]
        safe_name = self._escape(name.lower())
        safe_type = self._escape(entity_type)
        safe_eid = self._escape(eid)

        query = (
            f"graph.traversal().V().has('external_id', '{safe_eid}').fold()"
            f".coalesce("
            f"  unfold(),"
            f"  addV('{vertex_label}')"
            f"    .property('name', '{safe_name}')"
            f"    .property('type', '{safe_type}')"
            f"    .property('external_id', '{safe_eid}')"
            f"    .property('tenant_id', '{self._escape(self.tenant_id)}')"
            f"    .property('created_at', {self._now_epoch()})"
        )

        if extra_props:
            for k, v in extra_props.items():
                if isinstance(v, str):
                    query += f"    .property('{self._escape(k)}', '{self._escape(v)}')"
                elif isinstance(v, (int, float)):
                    query += f"    .property('{self._escape(k)}', {v})"

        query += ").id().next()"
        return query

    def _create_edge(self, src_query: str, tgt_query: str, edge_label: str, relation_name: str, props: Optional[Dict] = None) -> str:
        safe_rel = self._escape(relation_name)
        query = (
            f"srcId = {src_query}\n"
            f"tgtId = {tgt_query}\n"
            f"graph.traversal().V(srcId).outE('{edge_label}').where(inV().hasId(tgtId)).fold()"
            f".coalesce("
            f"  unfold(),"
            f"  graph.traversal().V(srcId).addE('{edge_label}').to(graph.traversal().V(tgtId).next())"
            f"    .property('name', '{safe_rel}')"
            f"    .property('weight', 1.0)"
        )

        if props:
            for k, v in props.items():
                if isinstance(v, str):
                    query += f"    .property('{self._escape(k)}', '{self._escape(str(v))}')"

        query += ").next()"
        return query

    @staticmethod
    def _now_epoch() -> int:
        import time
        return int(time.time() * 1000)

    def index_triplets(self, triplets: List[Dict[str, Any]], chunks: Optional[List[Dict]] = None):
        """
        Index a list of triplets into JanusGraph.

        Each triplet dict: head, head_type, relation, relation_name, tail, tail_type, properties
        """
        logger.info("Indexing %d triplets into JanusGraph (tenant=%s)", len(triplets), self.tenant_id)
        success = 0
        errors = 0

        for triplet in triplets:
            try:
                head = triplet["head"]
                tail = triplet["tail"]
                head_type = triplet.get("head_type", "Entity")
                tail_type = triplet.get("tail_type", "Entity")
                relation = triplet.get("relation", "RELATION")
                relation_name = triplet.get("relation_name", relation)
                props = triplet.get("properties", {})

                head_label = self._map_label(head_type)
                tail_label = self._map_label(tail_type)
                edge_label = self._map_edge_label(relation)

                src_q = self._get_or_create_vertex(head, head_label, head_type, props)
                tgt_q = self._get_or_create_vertex(tail, tail_label, tail_type, props)

                src_id = self._submit(src_q)
                tgt_id = self._submit(tgt_q)

                if src_id and tgt_id:
                    edge_src_q = f"graph.traversal().V().has('external_id', '{self._escape(hashlib.sha256(f'{head}:{head_label}:{self.tenant_id}'.encode()).hexdigest()[:20])}').id().next()"
                    edge_tgt_q = f"graph.traversal().V().has('external_id', '{self._escape(hashlib.sha256(f'{tail}:{tail_label}:{self.tenant_id}'.encode()).hexdigest()[:20])}').id().next()"
                    edge_q = self._create_edge(edge_src_q, edge_tgt_q, edge_label, relation_name, props)
                    self._submit(edge_q)

                if props.get("chunk_id") and props.get("source_file"):
                    self._csv_data.append({
                        "chunk_id": props["chunk_id"],
                        "source_file": props["source_file"],
                        "page_id": str(props.get("page_id", "")),
                        "entity_name": head.lower(),
                    })

                success += 1

            except Exception as e:
                errors += 1
                logger.warning("Failed to index triplet: %s — %s", triplet.get("head", "?"), e)

        if self._csv_data:
            self._write_chunk_csv(chunks)

        logger.info("Indexed %d/%d triplets (%d errors)", success, len(triplets), errors)
        return {"indexed": success, "errors": errors, "total": len(triplets)}

    def _write_chunk_csv(self, chunks: Optional[List[Dict]] = None):
        """Write chunk-to-graph mapping CSV for RAG context enrichment."""
        chunk_map = {}
        if chunks:
            for c in chunks:
                chunk_map[c["chunk_id"]] = c.get("chunk_text", "")

        rows = []
        for entry in self._csv_data:
            rows.append({
                "chunk_id": entry["chunk_id"],
                "source_file": entry["source_file"],
                "page_id": entry["page_id"],
                "entity_name": entry["entity_name"],
                "chunk_text": chunk_map.get(entry["chunk_id"], ""),
            })

        df = pd.DataFrame(rows).drop_duplicates(subset=["chunk_id", "entity_name"])
        df.to_csv(self.csv_file_path, index=False)
        logger.info("Wrote chunk mapping CSV: %s (%d rows)", self.csv_file_path, len(df))

    @staticmethod
    def _map_label(entity_type: str) -> str:
        """Map entity type to JanusGraph vertex label."""
        label_map = {
            "chunk": "Chunk",
            "document": "Document",
            "concept": "Concept",
        }
        return label_map.get(entity_type.lower(), "Entity")

    @staticmethod
    def _map_edge_label(relation: str) -> str:
        """Map relation to JanusGraph edge label."""
        label_map = {
            "contains": "CONTAINS",
            "references": "REFERENCES",
            "similar_to": "SIMILAR_TO",
        }
        return label_map.get(relation.lower(), "RELATION")

    def index_from_jsons(self, json_dir: str, chunks: Optional[List[Dict]] = None):
        """Load triplets from a directory of JSON files and index them."""
        json_path = Path(json_dir)
        all_triplets = []

        for f in sorted(json_path.glob("*.json")):
            try:
                data = json.loads(f.read_text())
                if isinstance(data, list):
                    all_triplets.extend(data)
                elif isinstance(data, dict) and "triplets" in data:
                    all_triplets.extend(data["triplets"])
            except Exception as e:
                logger.warning("Failed to load %s: %s", f.name, e)

        logger.info("Loaded %d triplets from %s", len(all_triplets), json_dir)
        return self.index_triplets(all_triplets, chunks)

    def close(self):
        if self._client:
            try:
                self._client.close()
            except Exception:
                pass
            self._client = None
