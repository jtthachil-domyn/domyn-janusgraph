"""ChromaDB retriever for vector similarity search."""

from typing import List, Dict, Any, Optional
import logging
import chromadb
from chromadb.config import Settings as ChromaSettings

logger = logging.getLogger("domyngraph-rag.retrieval.vector")


class ChromaRetriever:
    """Retrieve chunks or triplets from ChromaDB collections."""

    def __init__(
        self,
        persist_dir: str,
        collection_name: str,
        embedding_function: Optional[Any] = None,
    ):
        self.client = chromadb.Client(
            ChromaSettings(persist_directory=persist_dir, is_persistent=True)
        )
        self.collection = self.client.get_or_create_collection(
            name=collection_name,
            embedding_function=embedding_function,
        )
        logger.info("ChromaRetriever: dir=%s collection=%s", persist_dir, collection_name)

    def search(
        self,
        query: str,
        n_results: int = 10,
        where: Optional[Dict] = None,
    ) -> List[Dict[str, Any]]:
        """Search for similar documents."""
        kwargs: Dict[str, Any] = {"query_texts": [query], "n_results": n_results}
        if where:
            kwargs["where"] = where

        results = self.collection.query(**kwargs)

        docs = []
        if results and results["documents"]:
            for i, doc in enumerate(results["documents"][0]):
                entry = {
                    "id": results["ids"][0][i] if results["ids"] else "",
                    "document": doc,
                    "distance": results["distances"][0][i] if results.get("distances") else None,
                }
                if results.get("metadatas") and results["metadatas"][0]:
                    entry["metadata"] = results["metadatas"][0][i]
                docs.append(entry)

        return docs
