"""Ensemble retriever combining dense (ChromaDB) + sparse (BM25) retrieval."""

from typing import List, Dict, Any, Optional
from pathlib import Path
import json
import pickle
import logging
import numpy as np

logger = logging.getLogger("domyngraph-rag.retrieval.hybrid")


class EnsembleRetriever:
    """Hybrid retrieval combining ChromaDB dense search with BM25 sparse search."""

    def __init__(
        self,
        chroma_retriever: Any,
        bm25_path: str,
        bm25_weight: float = 0.75,
    ):
        self.chroma_retriever = chroma_retriever
        self.bm25_weight = bm25_weight
        self.dense_weight = 1.0 - bm25_weight
        self._bm25_data = None

        bm25_file = Path(bm25_path)
        if bm25_file.exists():
            self._bm25_data = self._load_bm25(bm25_file)
            if self._bm25_data:
                logger.info("Loaded BM25 index from %s (%d docs)", bm25_path, len(self._bm25_data.get("documents", [])))

    @staticmethod
    def _load_bm25(path: Path) -> Optional[Dict]:
        """Load BM25 index from either pickle (.pkl) or JSON (.json) format."""
        try:
            if path.suffix == ".json":
                with open(path, "r") as f:
                    data = json.load(f)
                from rank_bm25 import BM25Okapi
                tokenized = data.get("tokenized_docs", [])
                if not tokenized:
                    return None
                bm25 = BM25Okapi(tokenized)
                ids = [f"doc_{i}" for i in range(len(data["documents"]))]
                return {
                    "bm25": bm25,
                    "ids": ids,
                    "documents": data["documents"],
                    "metadata": data.get("metadata", []),
                }
            else:
                with open(path, "rb") as f:
                    return pickle.load(f)
        except Exception as e:
            logger.warning("Failed to load BM25 from %s: %s", path, e)
            return None

    def search(
        self,
        query: str,
        n_results: int = 10,
        mode: str = "entity_search",
    ) -> List[Dict[str, Any]]:
        """
        Hybrid search combining dense + sparse results.

        Modes:
        - entity_search: search for entity-related triplets
        - chunk_search: search for text chunks
        """
        dense_results = self.chroma_retriever.search(query, n_results=n_results * 2)

        bm25_results = self._bm25_search(query, n_results * 2) if self._bm25_data else []

        merged = self._reciprocal_rank_fusion(dense_results, bm25_results, n_results)
        return merged

    def _bm25_search(self, query: str, n_results: int) -> List[Dict[str, Any]]:
        if not self._bm25_data:
            return []

        bm25 = self._bm25_data["bm25"]
        ids = self._bm25_data["ids"]
        documents = self._bm25_data["documents"]
        metadata_list = self._bm25_data.get("metadata", [])

        tokenized_query = query.lower().split()
        scores = bm25.get_scores(tokenized_query)

        top_indices = np.argsort(scores)[::-1][:n_results]

        results = []
        for idx in top_indices:
            if scores[idx] > 0:
                entry = {
                    "id": ids[idx],
                    "document": documents[idx],
                    "bm25_score": float(scores[idx]),
                }
                if idx < len(metadata_list):
                    entry["metadata"] = metadata_list[idx]
                results.append(entry)
        return results

    def _reciprocal_rank_fusion(
        self,
        dense: List[Dict],
        sparse: List[Dict],
        n_results: int,
        k: int = 60,
    ) -> List[Dict[str, Any]]:
        """Merge results using Reciprocal Rank Fusion (RRF)."""
        scores: Dict[str, float] = {}
        doc_map: Dict[str, Dict] = {}

        for rank, doc in enumerate(dense):
            doc_id = doc.get("id", doc.get("document", ""))
            scores[doc_id] = scores.get(doc_id, 0) + self.dense_weight / (k + rank + 1)
            doc_map[doc_id] = doc

        for rank, doc in enumerate(sparse):
            doc_id = doc.get("id", doc.get("document", ""))
            scores[doc_id] = scores.get(doc_id, 0) + self.bm25_weight / (k + rank + 1)
            if doc_id not in doc_map:
                doc_map[doc_id] = doc

        sorted_ids = sorted(scores.keys(), key=lambda x: scores[x], reverse=True)

        results = []
        for doc_id in sorted_ids[:n_results]:
            entry = doc_map[doc_id].copy()
            entry["rrf_score"] = scores[doc_id]
            results.append(entry)

        return results
