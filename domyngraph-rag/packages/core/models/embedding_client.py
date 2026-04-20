"""Embedding client for vector operations via OpenAI-compatible API."""

from typing import List, Optional
import logging
import httpx
import numpy as np

from .config_models import EmbeddingConfig

logger = logging.getLogger("domyngraph-rag.models.embedding")


class EmbeddingClient:
    """Generate embeddings via the Crystal AI gateway (OpenAI-compatible)."""

    def __init__(self, config: EmbeddingConfig):
        self.model = config.model_name
        self.base_url = config.base_url.rstrip("/")
        self.api_key = config.api_key
        self.dimension = config.dimension
        self.batch_size = config.batch_size

        self._client = httpx.AsyncClient(
            timeout=httpx.Timeout(60.0, connect=10.0),
            limits=httpx.Limits(max_connections=50, max_keepalive_connections=20),
            http2=True,
        )
        logger.info("Embedding client → %s model=%s dim=%d", self.base_url, self.model, self.dimension)

    async def embed_async(self, texts: List[str], model: Optional[str] = None) -> List[List[float]]:
        """Embed a list of texts, batching if necessary."""
        all_embeddings: List[List[float]] = []
        use_model = model or self.model

        for i in range(0, len(texts), self.batch_size):
            batch = texts[i : i + self.batch_size]
            embeddings = await self._embed_batch(batch, use_model)
            all_embeddings.extend(embeddings)

        return all_embeddings

    async def _embed_batch(self, texts: List[str], model: str) -> List[List[float]]:
        headers = {"Content-Type": "application/json"}
        if self.api_key and self.api_key != "EMPTY":
            headers["Authorization"] = f"Bearer {self.api_key}"

        payload = {"model": model, "input": texts}
        url = f"{self.base_url}/embeddings"

        resp = await self._client.post(url, headers=headers, json=payload)
        if resp.status_code != 200:
            raise RuntimeError(f"Embedding API {resp.status_code}: {resp.text[:300]}")

        data = resp.json()
        sorted_data = sorted(data["data"], key=lambda x: x["index"])
        return [item["embedding"] for item in sorted_data]

    def embed_sync(self, texts: List[str]) -> List[List[float]]:
        """Synchronous embedding for ChromaDB compatibility."""
        import asyncio

        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            loop = None

        if loop and loop.is_running():
            import concurrent.futures
            with concurrent.futures.ThreadPoolExecutor() as pool:
                return pool.submit(lambda: asyncio.run(self.embed_async(texts))).result()
        return asyncio.run(self.embed_async(texts))

    async def close(self):
        await self._client.aclose()


class ChromaEmbeddingFunction:
    """Adapter wrapping EmbeddingClient for ChromaDB >= 1.5 embedding_function interface."""

    def __init__(self, client: EmbeddingClient):
        self._client = client

    def __call__(self, input: List[str]) -> List[List[float]]:
        return self._client.embed_sync(input)

    def embed_documents(self, input: List[str]) -> List[List[float]]:
        return self._client.embed_sync(input)

    def embed_query(self, input: List[str]) -> List[List[float]]:
        return self._client.embed_sync(input)

    @staticmethod
    def name() -> str:
        return "domyngraph_gateway_embedding"


class LocalSentenceTransformerEF:
    """ChromaDB embedding function using a local SentenceTransformer model."""

    def __init__(self, model_name: str = "all-MiniLM-L6-v2"):
        from sentence_transformers import SentenceTransformer
        self._model = SentenceTransformer(model_name)
        dim = self._model.get_embedding_dimension() if hasattr(self._model, 'get_embedding_dimension') else self._model.get_sentence_embedding_dimension()
        logger.info("Loaded local SentenceTransformer: %s (dim=%d)", model_name, dim)

    def __call__(self, input: List[str]) -> List[List[float]]:
        return self._model.encode(input).tolist()

    def embed_documents(self, input: List[str]) -> List[List[float]]:
        return self._model.encode(input).tolist()

    def embed_query(self, input: List[str]) -> List[List[float]]:
        return self._model.encode(input).tolist()

    @staticmethod
    def name() -> str:
        return "sentence_transformer_local"
