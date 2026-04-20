"""DomynGraph RAG Service — Graph/Vector/Hybrid RAG over JanusGraph."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent.parent))

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
import logging

from services.rag.config.settings import settings

logging.basicConfig(level=logging.INFO, format="%(asctime)s | %(name)s | %(levelname)s | %(message)s")
logger = logging.getLogger("domyngraph-rag.rag")

app = FastAPI(
    title="DomynGraph RAG Service",
    description="Graph/Vector/Hybrid RAG over JanusGraph + ChromaDB",
    version="0.1.0",
)

app.add_middleware(
    CORSMiddleware,
    allow_origins=[settings.CORS_ORIGIN, "http://localhost:3000", "http://localhost:5173", "*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)


@app.on_event("startup")
async def startup():
    from packages.core.models import OpenAICompatibleConfig, OpenAICompatibleClient
    from packages.core.models import LocalSentenceTransformerEF
    from packages.retrieval.graph.janusgraph_store import JanusGraphStore
    from packages.retrieval.vector.chroma_retriever import ChromaRetriever
    from packages.retrieval.hybrid.ensemble import EnsembleRetriever
    from packages.query_processing import KGContextBuilder, ContextBuilderConfig
    from packages.prompts.loader import load_prompt
    from services.rag.services.rag_orchestrator import RAGOrchestrator

    llm_config = OpenAICompatibleConfig(
        base_url=settings.LLM_BASE_URL,
        api_key=settings.LLM_API_KEY,
        default_model=settings.LLM_MODEL,
    )
    llm_client = OpenAICompatibleClient(llm_config)

    local_ef = LocalSentenceTransformerEF(model_name=settings.LOCAL_EMBEDDING_MODEL)

    graph_store = JanusGraphStore(
        gremlin_url=settings.GREMLIN_URL,
        graph_alias=settings.GRAPH_ALIAS,
    )

    chunk_retriever = ChromaRetriever(
        persist_dir=settings.CHROMA_PERSIST_DIR,
        collection_name=settings.CHUNK_COLLECTION,
        embedding_function=local_ef,
    )
    triplet_retriever_chroma = ChromaRetriever(
        persist_dir=settings.CHROMA_PERSIST_DIR,
        collection_name=settings.TRIPLET_COLLECTION,
        embedding_function=local_ef,
    )

    bm25_triplets_path = str(Path(settings.BM25_PERSIST_DIR) / settings.BM25_TRIPLETS_FILE)
    triplet_ensemble = EnsembleRetriever(
        chroma_retriever=triplet_retriever_chroma,
        bm25_path=bm25_triplets_path,
        bm25_weight=settings.BM25_WEIGHT,
    )

    bm25_chunks_path = str(Path(settings.BM25_PERSIST_DIR) / settings.BM25_CHUNKS_FILE)
    chunk_ensemble = EnsembleRetriever(
        chroma_retriever=chunk_retriever,
        bm25_path=bm25_chunks_path,
        bm25_weight=settings.BM25_WEIGHT,
    )

    context_builder = KGContextBuilder.with_csv(
        config=ContextBuilderConfig(max_context_length=settings.MAX_CONTEXT_LENGTH),
        csv_path=settings.GRAPH_CHUNK_MAPPING_CSV,
    )

    prompts = load_prompt("gremlin_rag_prompt")

    domain_config = {
        "domain_description": settings.DOMAIN_DESCRIPTION,
        "domain_info": settings.DOMAIN_INFO,
        "document_type": settings.DOCUMENT_TYPE,
        "entity_properties": "name (str), entity_type (str: COMP, FIN_METRIC, PRODUCT, CONCEPT, ORG, GPE, PERSON, SEGMENT, SECTOR, RISK_FACTOR, MACRO_CONDITION, FIN_INST, ECON_IND, etc.), tenant_id (str: stock ticker like NVDA, AAPL, MSFT, AMZN, GOOG, etc.), ticker (str: same as tenant_id), external_id (str: TICKER:Name:Type)",
        "relationship_properties": "page_id (str), chunk_id (str), source_file (str), triplet_index (int)",
        "dataset_specific_requirements": """CRITICAL RULES:
- All vertices have label 'Entity'. Use .hasLabel('Entity') always.
- The property 'entity_type' (NOT 'type') stores the entity category (COMP, FIN_METRIC, PRODUCT, etc.)
- The property 'tenant_id' stores the stock ticker (NVDA, AAPL, MSFT, etc.)
- Available tickers: AAPL, ACN, ADBE, AMD, AVGO, CRM, CSCO, IBM, INTC, INTU, MSFT, NOW, NVDA, ORCL, PLTR, QCOM, SBUX, TXN
- There is NO tenant_id='ALL'. If the user wants ALL companies or doesn't specify one, OMIT the tenant_id filter.
- Edge labels are relationship names like: Contributes_To, Related_To, Positively_Impacts, Increases, Has_Value, Depends_On, Includes, Introduces, etc.
- Use textContains() for flexible name matching.
- When searching for a specific company, filter by tenant_id for that company's ticker OR search by name.
- Example: For NVIDIA revenue, use .has('tenant_id','NVDA') and .has('name',textContains('Revenue'))
- Example: For revenue across ALL companies, just use .has('name',textContains('Revenue')) without tenant_id""",
        "schema_based_examples": """Example queries:
1. Find all NVIDIA entities: graph.traversal().V().hasLabel('Entity').has('tenant_id','NVDA').limit(20).elementMap().toList()
2. Find revenue for NVIDIA: graph.traversal().V().hasLabel('Entity').has('name','Revenue').has('tenant_id','NVDA').bothE().otherV().elementMap().toList()
3. Find what contributes to revenue: graph.traversal().V().hasLabel('Entity').has('name',textContains('Revenue')).has('tenant_id','NVDA').inE('Contributes_To').outV().elementMap().toList()
4. Find entities related to a concept: graph.traversal().V().hasLabel('Entity').has('name',textContains('AI')).has('tenant_id','NVDA').bothE().otherV().elementMap().limit(20).toList()
5. Compare entities across companies: graph.traversal().V().hasLabel('Entity').has('name',textContains('Revenue')).has('entity_type','FIN_METRIC').valueMap('name','tenant_id','entity_type').limit(20).toList()""",
    }

    orchestrator = RAGOrchestrator(
        llm_client=llm_client,
        graph_store=graph_store,
        chunk_retriever=chunk_retriever,
        triplet_retriever=triplet_ensemble,
        chunk_ensemble=chunk_ensemble,
        context_builder=context_builder,
        prompts=prompts,
        entity_schema=settings.ENTITY_SCHEMA,
        relationship_schema=settings.RELATIONSHIP_SCHEMA,
        domain_config=domain_config,
    )

    from services.rag.api import set_app_state

    set_app_state({"orchestrator": orchestrator})

    logger.info(
        "RAG service started — LLM=%s, Graph=%s, Chroma=%s (%s / %s), BM25=%s",
        settings.LLM_MODEL,
        settings.GREMLIN_URL,
        settings.CHROMA_PERSIST_DIR,
        settings.CHUNK_COLLECTION,
        settings.TRIPLET_COLLECTION,
        settings.BM25_PERSIST_DIR,
    )


from services.rag.api.routes import router
app.include_router(router)


@app.get("/health")
async def health():
    return {"status": "healthy", "service": "domyngraph-rag"}


if __name__ == "__main__":
    import uvicorn
    uvicorn.run("services.rag.main:app", host="0.0.0.0", port=8083, reload=True)
