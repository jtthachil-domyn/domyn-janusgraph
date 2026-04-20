"""Context builder — processes JanusGraph query results into LLM-ready context.

Follows UDA's pattern: extract page_id/source_file from graph edges,
look up actual document chunk text from the chunk_text CSV, and build
context from the real 10-K filing content for the LLM to answer from.
"""

from typing import List, Dict, Any, Tuple, Optional, Set
import pandas as pd
from pathlib import Path
import logging

logger = logging.getLogger("domyngraph-rag.query_processing.context")


class ContextBuilderConfig:
    def __init__(
        self,
        max_context_length: int = 60000,
        chunk_separator: str = "\n\n---\n\n",
        dataset_type: str = "sec",
    ):
        self.max_context_length = max_context_length
        self.chunk_separator = chunk_separator
        self.dataset_type = dataset_type


class KGContextBuilder:
    """
    Builds LLM-ready context from JanusGraph Gremlin query results.

    For relationship results: extracts page_id/source_file from edges,
    looks up actual document chunk text from the CSV mapping, and builds
    markdown context from real 10-K filing content.

    For simple results: formats entity metadata directly.
    """

    def __init__(
        self,
        config: ContextBuilderConfig,
        graph_chunk_mapping: Optional[pd.DataFrame] = None,
    ):
        self.config = config
        self.graph_chunk_mapping = graph_chunk_mapping

    @classmethod
    def with_csv(cls, config: ContextBuilderConfig, csv_path: str) -> "KGContextBuilder":
        mapping = None
        path = Path(csv_path)
        if path.exists():
            mapping = pd.read_csv(path)
            logger.info("Loaded chunk mapping CSV: %s (%d rows)", csv_path, len(mapping))
        else:
            logger.warning("Chunk mapping CSV not found: %s", csv_path)
        return cls(config=config, graph_chunk_mapping=mapping)

    def build_context(
        self,
        gremlin_results: List[Any],
        user_query: str,
    ) -> Tuple[str, str]:
        if not gremlin_results:
            return "", "empty"

        relationships, simple_values = self._classify_results(gremlin_results)

        if relationships:
            context = self._build_relationship_context(relationships)
            if context.strip():
                return context, "relationship"

        if simple_values:
            context = self._build_simple_context(simple_values)
            return context, "simple_value"

        context = str(gremlin_results)[:self.config.max_context_length]
        return context, "simple_value"

    def _classify_results(self, results: List[Any]) -> Tuple[List[Dict], List[Any]]:
        relationships = []
        simple_values = []

        for item in results:
            if isinstance(item, dict):
                if self._is_relationship_result(item):
                    relationships.append(item)
                else:
                    simple_values.append(item)
            elif isinstance(item, list):
                for sub in item:
                    if isinstance(sub, dict) and self._is_relationship_result(sub):
                        relationships.append(sub)
                    else:
                        simple_values.append(sub)
            else:
                simple_values.append(item)

        return relationships, simple_values

    @staticmethod
    def _is_relationship_result(item: Dict) -> bool:
        keys = set(item.keys())
        if len(keys) >= 2 and sum(1 for v in item.values() if isinstance(v, dict)) >= 2:
            return True
        edge_keys = {"e", "edge", "r", "rel", "relation"}
        vertex_keys = {"v", "vertex", "neighbor", "target", "source", "metric",
                       "contributor", "factor", "risk", "impacted", "company",
                       "dependency", "related", "discloser", "revenue"}
        if keys & edge_keys and keys & vertex_keys:
            return True
        return False

    def _build_relationship_context(self, relationships: List[Dict]) -> str:
        """Extract chunk references from edges, look up document text, build context."""
        unique_chunks: Set[Tuple[str, str, str]] = set()
        relationship_summaries = []

        for rel in relationships:
            summary = self._format_relationship_summary(rel)
            if summary:
                relationship_summaries.append(summary)

            for key, val in rel.items():
                if not isinstance(val, dict):
                    continue
                source_file = val.get("source_file", "")
                page_id = val.get("page_id", "")
                chunk_id = val.get("chunk_id", "")
                if source_file and page_id:
                    if isinstance(source_file, list):
                        source_file = source_file[0]
                    if isinstance(page_id, list):
                        page_id = page_id[0]
                    sf = str(source_file).split("/")[-1]
                    unique_chunks.add((str(chunk_id), sf, str(page_id)))

        logger.info("Found %d unique chunks from %d relationships", len(unique_chunks), len(relationships))

        context_parts = []

        if relationship_summaries:
            context_parts.append("=== Graph Relationships Found ===")
            context_parts.extend(relationship_summaries[:30])

        if self.graph_chunk_mapping is not None and unique_chunks:
            chunk_context = self._build_context_from_chunks(unique_chunks)
            if chunk_context:
                context_parts.append("\n=== Source Document Context ===")
                context_parts.append(chunk_context)

        context = "\n".join(context_parts)
        return context[:self.config.max_context_length]

    def _build_context_from_chunks(self, unique_chunks: Set[Tuple[str, str, str]]) -> str:
        """Look up actual document text from chunk_text CSV (UDA pattern)."""
        if self.graph_chunk_mapping is None:
            return ""

        context_by_entity: Dict[str, Dict[str, str]] = {}

        for chunk_id, source_file, page_id in unique_chunks:
            filtered = self.graph_chunk_mapping[
                (self.graph_chunk_mapping["source_file"] == source_file) &
                (self.graph_chunk_mapping["page_id"] == page_id)
            ]

            if filtered.empty:
                continue

            if "chunk_text" not in filtered.columns:
                continue

            grouped = (
                filtered.groupby("chunk_id")["chunk_text"]
                .agg("\n".join)
                .reset_index()
            )
            final_text = grouped["chunk_text"].str.cat(sep="\n\n")

            entity_name = self._extract_entity_name(source_file)

            if entity_name not in context_by_entity:
                context_by_entity[entity_name] = {}
            context_by_entity[entity_name][page_id] = final_text

        parts = []
        for entity_name, pages in context_by_entity.items():
            for page_id, text in pages.items():
                parts.append(f"[{entity_name} — {page_id}]:\n{text}")

        return self.config.chunk_separator.join(parts)

    @staticmethod
    def _extract_entity_name(source_file: str) -> str:
        """Extract ticker/entity name from source filename like 'NVDA_10k_2024.pdf'."""
        name = source_file.replace(".pdf", "").replace(".PDF", "")
        parts = name.split("_")
        return parts[0] if parts else name

    def _build_simple_context(self, values: List[Any]) -> str:
        skip_keys = {"id", "T.id", "T.label", "domyn.pageRank.rank", "domyn.pageRank.edgeCount"}
        lines = []
        for v in values:
            if isinstance(v, dict):
                parts = []
                for k, val in v.items():
                    if k in skip_keys:
                        continue
                    display_val = val[0] if isinstance(val, list) and len(val) == 1 else val
                    parts.append(f"{k}: {display_val}")
                if parts:
                    lines.append(" | ".join(parts))
            else:
                lines.append(str(v))
        return "\n".join(lines)[:self.config.max_context_length]

    @staticmethod
    def _format_relationship_summary(rel: Dict) -> str:
        """Format a relationship result as a human-readable summary line."""
        def _name(d: dict) -> str:
            name = d.get("name", "")
            if isinstance(name, list):
                name = name[0] if name else ""
            etype = d.get("entity_type", d.get("T.label", ""))
            if isinstance(etype, list):
                etype = etype[0] if etype else ""
            tenant = d.get("tenant_id", "")
            if isinstance(tenant, list):
                tenant = tenant[0] if tenant else ""
            parts = [str(name)]
            if etype and etype != "Entity":
                parts.append(f"({etype})")
            if tenant:
                parts.append(f"[{tenant}]")
            return " ".join(parts) if name else ""

        def _edge_label(d: dict) -> str:
            return str(d.get("T.label", d.get("label", "related_to")))

        dict_items = [(k, v) for k, v in rel.items() if isinstance(v, dict)]

        source_keys = {"source", "contributor", "factor", "discloser", "company", "risk"}
        edge_keys = {"e", "edge", "r", "rel", "relation"}
        target_keys = {"target", "neighbor", "metric", "impacted", "dependency", "related", "revenue"}

        source = edge = target = ""
        for k, v in dict_items:
            if k in source_keys and not source:
                source = _name(v)
            elif k in edge_keys and not edge:
                edge = _edge_label(v)
            elif k in target_keys and not target:
                target = _name(v)

        if not source or not target:
            names = [_name(v) for _, v in dict_items if _name(v)]
            edges = [_edge_label(v) for k, v in dict_items if "T.label" in v and v.get("T.label") != "Entity"]
            if len(names) >= 2 and edges:
                return f"{names[0]} --[{edges[0]}]--> {names[1]}"
            elif len(names) >= 2:
                return f"{names[0]} --> {names[1]}"

        if source and edge and target:
            return f"{source} --[{edge}]--> {target}"
        if source and target:
            return f"{source} --> {target}"
        return ""
