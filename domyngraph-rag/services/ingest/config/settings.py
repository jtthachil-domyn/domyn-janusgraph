"""Ingest service configuration — loaded from environment variables."""

import os


class IngestSettings:
    LLM_BASE_URL: str = os.getenv("LLM_BASE_URL", "https://gateway-dev.llm.crystal.ai/v1")
    LLM_API_KEY: str = os.getenv("LLM_API_KEY", "sk-v10TyzbmfF6Kf7KvjgrLNg")
    LLM_MODEL: str = os.getenv("LLM_MODEL", "Qwen/Qwen3-32B")
    LLM_TEMPERATURE: float = float(os.getenv("LLM_TEMPERATURE", "0.1"))

    EMBEDDING_BASE_URL: str = os.getenv("EMBEDDING_BASE_URL", "https://gateway-dev.llm.crystal.ai/v1")
    EMBEDDING_MODEL: str = os.getenv("EMBEDDING_MODEL", "Qwen/Qwen3-Embedding-0.6B")
    EMBEDDING_API_KEY: str = os.getenv("EMBEDDING_API_KEY", "sk-v10TyzbmfF6Kf7KvjgrLNg")

    GREMLIN_URL: str = os.getenv("GREMLIN_URL", "ws://localhost:8182/gremlin")
    GRAPH_ALIAS: str = os.getenv("GRAPH_ALIAS", "graph")
    DEFAULT_TENANT: str = os.getenv("DEFAULT_TENANT", "default")

    CHROMA_PERSIST_DIR: str = os.getenv("CHROMA_PERSIST_DIR", "./data/chroma")
    BM25_PERSIST_DIR: str = os.getenv("BM25_PERSIST_DIR", "./data/bm25")
    CHUNK_COLLECTION: str = os.getenv("CHUNK_COLLECTION", "domyngraph_chunks")
    TRIPLET_COLLECTION: str = os.getenv("TRIPLET_COLLECTION", "domyngraph_triplets")
    GRAPH_RAG_OUTPUT_DIR: str = os.getenv("GRAPH_RAG_OUTPUT_DIR", "./data/graph_rag_output")

    UPLOAD_DIR: str = os.getenv("UPLOAD_DIR", "./data/uploads")
    KG_OUTPUT_DIR: str = os.getenv("KG_OUTPUT_DIR", "./data/kg_output")

    CHUNK_SIZE: int = int(os.getenv("CHUNK_SIZE", "1000"))
    CHUNK_OVERLAP: int = int(os.getenv("CHUNK_OVERLAP", "200"))

    CORS_ORIGIN: str = os.getenv("CORS_ORIGIN", "http://localhost:5173")


settings = IngestSettings()
