"""RAG service configuration."""

import os


UDA_ROOT = os.getenv("UDA_ROOT", "/Users/josephthomasthachil/Desktop/Domyn/uda")


class RAGSettings:
    LLM_BASE_URL: str = os.getenv("LLM_BASE_URL", "https://gateway-dev.llm.crystal.ai/v1")
    LLM_API_KEY: str = os.getenv("LLM_API_KEY", "sk-v10TyzbmfF6Kf7KvjgrLNg")
    LLM_MODEL: str = os.getenv("LLM_MODEL", "Qwen/Qwen3-32B")

    EMBEDDING_BASE_URL: str = os.getenv("EMBEDDING_BASE_URL", "https://gateway-dev.llm.crystal.ai/v1")
    EMBEDDING_MODEL: str = os.getenv("EMBEDDING_MODEL", "Qwen/Qwen3-Embedding-0.6B")
    EMBEDDING_API_KEY: str = os.getenv("EMBEDDING_API_KEY", "sk-v10TyzbmfF6Kf7KvjgrLNg")

    LOCAL_EMBEDDING_MODEL: str = os.getenv("LOCAL_EMBEDDING_MODEL", "all-MiniLM-L6-v2")

    GREMLIN_URL: str = os.getenv("GREMLIN_URL", "ws://localhost:8182/gremlin")
    GRAPH_ALIAS: str = os.getenv("GRAPH_ALIAS", "graph")

    CHROMA_PERSIST_DIR: str = os.getenv("CHROMA_PERSIST_DIR", os.path.join(UDA_ROOT, ".chromadb"))
    BM25_PERSIST_DIR: str = os.getenv("BM25_PERSIST_DIR", os.path.join(UDA_ROOT, ".bm25_collections"))
    CHUNK_COLLECTION: str = os.getenv("CHUNK_COLLECTION", "financial_documents_test")
    TRIPLET_COLLECTION: str = os.getenv("TRIPLET_COLLECTION", "financial_document_triplets_test")

    BM25_CHUNKS_FILE: str = os.getenv("BM25_CHUNKS_FILE", "financial_documents_bm25_test.json")
    BM25_TRIPLETS_FILE: str = os.getenv("BM25_TRIPLETS_FILE", "financial_document_triplets_bm25_test.json")

    GRAPH_CHUNK_MAPPING_CSV: str = os.getenv("GRAPH_CHUNK_MAPPING_CSV", os.path.join(UDA_ROOT, "graph_rag_output/chunk_text.csv"))

    N_TRIPLETS: int = int(os.getenv("N_TRIPLETS", "10"))
    N_CHUNKS: int = int(os.getenv("N_CHUNKS", "5"))
    BM25_WEIGHT: float = float(os.getenv("BM25_WEIGHT", "0.75"))
    MAX_CONTEXT_LENGTH: int = int(os.getenv("MAX_CONTEXT_LENGTH", "60000"))

    ENTITY_SCHEMA: str = os.getenv("ENTITY_SCHEMA", "Entity (vertex label: 'Entity', with properties: name, entity_type, tenant_id, ticker, external_id)")
    RELATIONSHIP_SCHEMA: str = os.getenv("RELATIONSHIP_SCHEMA", "Contributes_To, Related_To, Positively_Impacts, Increases, Decreases, Discloses, Has_Value, Depends_On, Includes, Introduces, Negatively_Impacts, Restricts, Impacts, Has, Served_As, Serves_As, Has_Stake_In, Holds_Position, Involved_In, Has_Fair_Value")

    DOMAIN_DESCRIPTION: str = os.getenv("DOMAIN_DESCRIPTION", "financial document analysis and knowledge graphs")
    DOMAIN_INFO: str = os.getenv("DOMAIN_INFO", "financial data, SEC filings, company information")
    DOCUMENT_TYPE: str = os.getenv("DOCUMENT_TYPE", "10-K SEC filings")

    CORS_ORIGIN: str = os.getenv("CORS_ORIGIN", "http://localhost:5173")


settings = RAGSettings()
