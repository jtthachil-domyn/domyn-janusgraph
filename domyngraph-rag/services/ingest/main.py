"""DomynGraph Ingest Service — KG construction + indexing for JanusGraph GraphRAG."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent.parent))

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
import logging

from services.ingest.config.settings import settings

logging.basicConfig(level=logging.INFO, format="%(asctime)s | %(name)s | %(levelname)s | %(message)s")
logger = logging.getLogger("domyngraph-rag.ingest")

app = FastAPI(
    title="DomynGraph Ingest Service",
    description="KG construction and indexing for DomynGraph GraphRAG",
    version="0.1.0",
)

app.add_middleware(
    CORSMiddleware,
    allow_origins=[settings.CORS_ORIGIN, "http://localhost:3000", "*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)


@app.on_event("startup")
async def startup():
    from packages.core.models import OpenAICompatibleConfig, OpenAICompatibleClient
    from packages.core.models import EmbeddingConfig, EmbeddingClient, ChromaEmbeddingFunction
    from packages.indexing.vector.chroma_indexer import VectorIndexer

    llm_config = OpenAICompatibleConfig(
        base_url=settings.LLM_BASE_URL,
        api_key=settings.LLM_API_KEY,
        default_model=settings.LLM_MODEL,
        temperature=settings.LLM_TEMPERATURE,
    )
    llm_client = OpenAICompatibleClient(llm_config)

    embed_config = EmbeddingConfig(
        model_name=settings.EMBEDDING_MODEL,
        base_url=settings.EMBEDDING_BASE_URL,
        api_key=settings.EMBEDDING_API_KEY,
    )
    embed_client = EmbeddingClient(embed_config)
    chroma_ef = ChromaEmbeddingFunction(embed_client)

    vector_indexer = VectorIndexer(
        persist_dir=settings.CHROMA_PERSIST_DIR,
        bm25_dir=settings.BM25_PERSIST_DIR,
        chunk_collection_name=settings.CHUNK_COLLECTION,
        triplet_collection_name=settings.TRIPLET_COLLECTION,
        embedding_function=chroma_ef,
    )

    from services.ingest.api import set_app_state

    set_app_state({
        "llm_client": llm_client,
        "embed_client": embed_client,
        "vector_indexer": vector_indexer,
    })

    logger.info("Ingest service started — LLM=%s, Embedding=%s", settings.LLM_MODEL, settings.EMBEDDING_MODEL)


from services.ingest.api.routes import router
app.include_router(router)


@app.get("/health")
async def health():
    return {"status": "healthy", "service": "domyngraph-ingest"}


if __name__ == "__main__":
    import uvicorn
    uvicorn.run("services.ingest.main:app", host="0.0.0.0", port=8081, reload=True)
