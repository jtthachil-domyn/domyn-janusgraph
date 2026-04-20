"""ChromaDB + BM25 vector indexer for chunks and triplets."""

from typing import List, Dict, Any, Optional
import json
import logging
from pathlib import Path
import chromadb
from chromadb.config import Settings as ChromaSettings
from rank_bm25 import BM25Okapi
import pickle

logger = logging.getLogger("domyngraph-rag.indexing.vector")


class VectorIndexer:
    """Index chunks and triplets into ChromaDB with BM25 side-index."""

    def __init__(
        self,
        persist_dir: str = "./chroma_data",
        bm25_dir: str = "./bm25_data",
        chunk_collection_name: str = "chunks",
        triplet_collection_name: str = "triplets",
        embedding_function: Optional[Any] = None,
    ):
        self.persist_dir = persist_dir
        self.bm25_dir = Path(bm25_dir)
        self.bm25_dir.mkdir(parents=True, exist_ok=True)

        self.chroma_client = chromadb.Client(
            ChromaSettings(persist_directory=persist_dir, is_persistent=True)
        )
        self.embedding_function = embedding_function

        self.chunk_collection = self.chroma_client.get_or_create_collection(
            name=chunk_collection_name,
            embedding_function=embedding_function,
        )
        self.triplet_collection = self.chroma_client.get_or_create_collection(
            name=triplet_collection_name,
            embedding_function=embedding_function,
        )
        logger.info(
            "VectorIndexer: chroma=%s, chunks=%s, triplets=%s",
            persist_dir,
            chunk_collection_name,
            triplet_collection_name,
        )

    def index_chunks(self, chunks: List[Dict[str, Any]]) -> Dict[str, int]:
        """Index text chunks into ChromaDB + BM25."""
        if not chunks:
            return {"indexed": 0}

        ids = [c["chunk_id"] for c in chunks]
        documents = [c["chunk_text"] for c in chunks]
        metadatas = [
            {
                "source_file": c.get("source_file", ""),
                "page_id": str(c.get("page_id", "")),
                "chunk_index": c.get("chunk_index", 0),
            }
            for c in chunks
        ]

        self.chunk_collection.upsert(ids=ids, documents=documents, metadatas=metadatas)

        tokenized = [doc.lower().split() for doc in documents]
        bm25 = BM25Okapi(tokenized)
        bm25_path = self.bm25_dir / "chunks_bm25.pkl"
        with open(bm25_path, "wb") as f:
            pickle.dump({"bm25": bm25, "ids": ids, "documents": documents}, f)

        logger.info("Indexed %d chunks into ChromaDB + BM25", len(chunks))
        return {"indexed": len(chunks)}

    def index_triplets(self, triplets: List[Dict[str, Any]]) -> Dict[str, int]:
        """Index triplets as searchable documents into ChromaDB + BM25."""
        if not triplets:
            return {"indexed": 0}

        ids = []
        documents = []
        metadatas = []

        for i, t in enumerate(triplets):
            head = t.get("head", "")
            tail = t.get("tail", "")
            relation = t.get("relation_name", t.get("relation", ""))
            triplet_text = f"{head} {relation} {tail}"

            tid = f"triplet_{i}_{hash(triplet_text) & 0xFFFFFFFF:08x}"
            ids.append(tid)
            documents.append(triplet_text)
            metadatas.append({
                "head": head,
                "tail": tail,
                "relation": relation,
                "head_type": t.get("head_type", ""),
                "tail_type": t.get("tail_type", ""),
                "source_file": t.get("properties", {}).get("source_file", ""),
                "page_id": str(t.get("properties", {}).get("page_id", "")),
            })

        self.triplet_collection.upsert(ids=ids, documents=documents, metadatas=metadatas)

        tokenized = [doc.lower().split() for doc in documents]
        bm25 = BM25Okapi(tokenized)
        bm25_path = self.bm25_dir / "triplets_bm25.pkl"
        with open(bm25_path, "wb") as f:
            pickle.dump({"bm25": bm25, "ids": ids, "documents": documents}, f)

        logger.info("Indexed %d triplets into ChromaDB + BM25", len(triplets))
        return {"indexed": len(triplets)}
