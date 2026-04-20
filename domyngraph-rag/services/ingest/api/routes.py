"""Ingest service API routes — schema generation, KG construction, indexing."""

from fastapi import APIRouter, UploadFile, File, Form, Body, HTTPException
from typing import Optional, List
import json
import shutil
from pathlib import Path
import logging

router = APIRouter(prefix="/api/v1", tags=["ingest"])
logger = logging.getLogger("domyngraph-rag.ingest.routes")


@router.get("/")
async def root():
    return {"service": "DomynGraph Ingest Service", "version": "0.1.0"}


@router.post("/schema/generate")
async def generate_schema(
    text: str = Body(..., description="Sample text to analyze for schema generation"),
):
    """Auto-generate entity/relationship schema from sample text."""
    from services.ingest.api import get_app_state
    state = get_app_state()

    from packages.prompts.loader import load_prompt, format_prompt
    prompts = load_prompt("kg_construction_prompt")

    schema_prompts = prompts.get("schema_generation_prompt", {})
    system = schema_prompts.get("system_prompt", "")
    user = schema_prompts.get("user_prompt", "").replace("{SAMPLE_TEXT}", text[:8000])

    response = await state["llm_client"].generate_text_async(
        prompt=user, system_prompt=system, temperature=0.2, max_tokens=32768
    )

    try:
        schema = json.loads(response.content)
    except json.JSONDecodeError:
        schema = {"raw_response": response.content}

    return {"schema": schema}


@router.post("/upload")
async def upload_documents(
    files: List[UploadFile] = File(...),
    tenant_id: Optional[str] = Form("default"),
):
    """Upload PDF documents for processing."""
    from services.ingest.config.settings import settings

    upload_dir = Path(settings.UPLOAD_DIR) / tenant_id
    upload_dir.mkdir(parents=True, exist_ok=True)

    uploaded = []
    for file in files:
        dest = upload_dir / file.filename
        with open(dest, "wb") as f:
            shutil.copyfileobj(file.file, f)
        uploaded.append({"filename": file.filename, "path": str(dest)})
        logger.info("Uploaded: %s → %s", file.filename, dest)

    return {"uploaded": uploaded, "count": len(uploaded)}


@router.post("/kg/build")
async def build_knowledge_graph(
    tenant_id: str = Body("default"),
    entity_types: Optional[str] = Body(None, description="Comma-separated entity types"),
):
    """Extract triplets from uploaded documents using LLM."""
    from services.ingest.config.settings import settings
    from services.ingest.api import get_app_state

    state = get_app_state()
    upload_dir = Path(settings.UPLOAD_DIR) / tenant_id
    if not upload_dir.exists():
        raise HTTPException(404, f"No uploads found for tenant '{tenant_id}'")

    from packages.processing.pdf.processor import PDFProcessor
    from packages.processing.text.chunker import TextChunker
    from packages.prompts.loader import load_prompt

    pdf_proc = PDFProcessor()
    chunker = TextChunker(chunk_size=settings.CHUNK_SIZE, chunk_overlap=settings.CHUNK_OVERLAP)
    prompts = load_prompt("kg_construction_prompt")

    entity_type_list = entity_types if entity_types else "Entity, Concept"

    all_triplets = []
    all_chunks = []

    for pdf_file in upload_dir.glob("*.pdf"):
        pages = pdf_proc.extract(str(pdf_file))
        chunks = chunker.chunk_pages(pages)
        all_chunks.extend(chunks)

        for chunk in chunks:
            system = prompts["system_prompt"]["instruction"].replace("{ENTITY_TYPES}", entity_type_list)
            user = prompts["user_prompt"]["message"].format(
                SOURCE_FILE=chunk["source_file"],
                PAGE_ID=chunk["page_id"],
                CHUNK_ID=chunk["chunk_id"],
                CHUNK_TEXT=chunk["chunk_text"][:3000],
            )

            response = await state["llm_client"].generate_text_async(
                prompt=user,
                system_prompt=system,
                temperature=0.1,
                max_tokens=32768,
                enable_thinking=False,
            )

            try:
                raw = response.content.strip()
                if raw.startswith("```"):
                    raw = raw.split("```")[1]
                    if raw.startswith("json"):
                        raw = raw[4:]
                triplets = json.loads(raw)
                if isinstance(triplets, list):
                    all_triplets.extend(triplets)
            except (json.JSONDecodeError, IndexError) as e:
                logger.warning("Failed to parse triplets from chunk %s: %s", chunk["chunk_id"], e)

    output_dir = Path(settings.KG_OUTPUT_DIR) / tenant_id
    output_dir.mkdir(parents=True, exist_ok=True)
    triplets_file = output_dir / "triplets.json"
    triplets_file.write_text(json.dumps(all_triplets, indent=2))

    chunks_file = output_dir / "chunks.json"
    chunks_file.write_text(json.dumps(all_chunks, indent=2))

    logger.info("KG build complete: %d triplets from %d chunks", len(all_triplets), len(all_chunks))

    return {
        "triplets_count": len(all_triplets),
        "chunks_count": len(all_chunks),
        "output_dir": str(output_dir),
    }


@router.post("/index/vector")
async def index_vector(
    tenant_id: str = Body("default"),
):
    """Index chunks and triplets into ChromaDB + BM25."""
    from services.ingest.config.settings import settings
    from services.ingest.api import get_app_state

    state = get_app_state()
    output_dir = Path(settings.KG_OUTPUT_DIR) / tenant_id

    chunks_file = output_dir / "chunks.json"
    triplets_file = output_dir / "triplets.json"

    if not chunks_file.exists():
        raise HTTPException(404, "No chunks found. Run /kg/build first.")

    chunks = json.loads(chunks_file.read_text())
    triplets = json.loads(triplets_file.read_text()) if triplets_file.exists() else []

    indexer = state["vector_indexer"]
    chunk_result = indexer.index_chunks(chunks)
    triplet_result = indexer.index_triplets(triplets)

    return {
        "chunks_indexed": chunk_result["indexed"],
        "triplets_indexed": triplet_result["indexed"],
    }


@router.post("/index/graph")
async def index_graph(
    tenant_id: str = Body("default"),
):
    """Index triplets into JanusGraph."""
    from services.ingest.config.settings import settings
    from services.ingest.api import get_app_state

    state = get_app_state()
    output_dir = Path(settings.KG_OUTPUT_DIR) / tenant_id

    triplets_file = output_dir / "triplets.json"
    chunks_file = output_dir / "chunks.json"

    if not triplets_file.exists():
        raise HTTPException(404, "No triplets found. Run /kg/build first.")

    triplets = json.loads(triplets_file.read_text())
    chunks = json.loads(chunks_file.read_text()) if chunks_file.exists() else None

    from packages.indexing.graph.janusgraph_indexer import JanusGraphIndexer

    graph_indexer = JanusGraphIndexer(
        gremlin_url=settings.GREMLIN_URL,
        graph_alias=settings.GRAPH_ALIAS,
        tenant_id=tenant_id,
        csv_output_dir=settings.GRAPH_RAG_OUTPUT_DIR,
    )

    result = graph_indexer.index_triplets(triplets, chunks)
    graph_indexer.close()

    return result


@router.get("/status/{tenant_id}")
async def get_status(tenant_id: str):
    """Check what data exists for a tenant."""
    from services.ingest.config.settings import settings

    upload_dir = Path(settings.UPLOAD_DIR) / tenant_id
    output_dir = Path(settings.KG_OUTPUT_DIR) / tenant_id

    pdfs = list(upload_dir.glob("*.pdf")) if upload_dir.exists() else []
    triplets_file = output_dir / "triplets.json"
    chunks_file = output_dir / "chunks.json"

    triplet_count = 0
    chunk_count = 0
    if triplets_file.exists():
        triplet_count = len(json.loads(triplets_file.read_text()))
    if chunks_file.exists():
        chunk_count = len(json.loads(chunks_file.read_text()))

    return {
        "tenant_id": tenant_id,
        "uploaded_pdfs": len(pdfs),
        "pdf_files": [p.name for p in pdfs],
        "triplets": triplet_count,
        "chunks": chunk_count,
    }
