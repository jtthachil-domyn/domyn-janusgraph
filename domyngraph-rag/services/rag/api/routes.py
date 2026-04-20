"""RAG service API routes with SSE streaming."""

from fastapi import APIRouter, Body
from fastapi.responses import StreamingResponse
from typing import Optional, Dict
from pydantic import BaseModel
import logging

router = APIRouter(prefix="/api/v1", tags=["rag"])
logger = logging.getLogger("domyngraph-rag.rag.routes")


class BusinessContext(BaseModel):
    usecase_info: Optional[str] = None
    usecase_goal: Optional[str] = None
    usecase_application: Optional[str] = None
    enduser_info: Optional[str] = None


@router.get("/")
async def root():
    return {"service": "DomynGraph RAG Service", "version": "0.1.0"}


@router.post("/query/graph")
async def graph_rag_query(
    query: str = Body(..., description="Natural language query"),
    n_triplets: int = Body(10, description="Number of triplets for context"),
    business_context: Optional[BusinessContext] = Body(None),
):
    """
    Graph RAG: NL → Gremlin → JanusGraph → LLM answer.
    Returns SSE stream with real-time progress.
    """
    from services.rag.api import get_app_state
    state = get_app_state()
    orchestrator = state["orchestrator"]

    ctx = business_context.model_dump() if business_context else None

    async def stream():
        async for event in orchestrator.graph_rag_generator(
            query=query,
            n_triplets=n_triplets,
            business_context=ctx,
        ):
            yield event

    return StreamingResponse(stream(), media_type="text/event-stream")


@router.post("/query/vector")
async def vector_rag_query(
    query: str = Body(..., description="Natural language query"),
    n_chunks: int = Body(5, description="Number of chunks to retrieve"),
    business_context: Optional[BusinessContext] = Body(None),
):
    """
    Vector RAG: Query → ChromaDB/BM25 → LLM answer.
    Returns SSE stream.
    """
    from services.rag.api import get_app_state
    state = get_app_state()
    orchestrator = state["orchestrator"]

    ctx = business_context.model_dump() if business_context else None

    async def stream():
        async for event in orchestrator.vector_rag_generator(
            query=query,
            n_chunks=n_chunks,
            business_context=ctx,
        ):
            yield event

    return StreamingResponse(stream(), media_type="text/event-stream")


@router.post("/query/hybrid")
async def hybrid_rag_query(
    query: str = Body(..., description="Natural language query"),
    n_chunks: int = Body(5),
    n_triplets: int = Body(10),
    business_context: Optional[BusinessContext] = Body(None),
):
    """
    Hybrid RAG: Graph + Vector combined.
    Returns SSE stream.
    """
    from services.rag.api import get_app_state
    state = get_app_state()
    orchestrator = state["orchestrator"]

    ctx = business_context.model_dump() if business_context else None

    async def stream():
        async for event in orchestrator.hybrid_rag_generator(
            query=query,
            n_chunks=n_chunks,
            n_triplets=n_triplets,
            business_context=ctx,
        ):
            yield event

    return StreamingResponse(stream(), media_type="text/event-stream")
