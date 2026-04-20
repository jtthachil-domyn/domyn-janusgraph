"""RAG Orchestrator — coordinates Graph, Vector, and Hybrid RAG for JanusGraph.

Replaces the Memgraph/Cypher-based orchestrator from UDA with
JanusGraph/Gremlin traversals via the Crystal AI gateway LLMs.
"""

from typing import Dict, Any, Optional, AsyncIterator
import json
import logging

from packages.query_processing import (
    GremlinQueryGenerator,
    GremlinGeneratorConfig,
    KGContextBuilder,
    ContextBuilderConfig,
    QAChain,
)
from packages.retrieval.graph.janusgraph_store import JanusGraphStore
from packages.retrieval.vector.chroma_retriever import ChromaRetriever
from packages.retrieval.hybrid.ensemble import EnsembleRetriever

logger = logging.getLogger("domyngraph-rag.rag.orchestrator")


class RAGOrchestrator:
    """
    Orchestrates Graph RAG, Vector RAG, and Hybrid RAG.

    Graph RAG flow:
      1. Retrieve relevant triplets (dense + BM25 hybrid)
      2. LLM generates Gremlin traversal from NL query + triplet context
      3. Execute Gremlin on JanusGraph
      4. Build context from results (with optional chunk enrichment)
      5. LLM generates final answer

    Vector RAG flow:
      1. Retrieve relevant chunks (dense + BM25)
      2. LLM generates answer from chunks

    Hybrid RAG flow:
      1. Parallel: Graph RAG context + Vector RAG chunks
      2. Merge contexts
      3. LLM generates answer
    """

    def __init__(
        self,
        llm_client: Any,
        graph_store: JanusGraphStore,
        chunk_retriever: ChromaRetriever,
        triplet_retriever: EnsembleRetriever,
        chunk_ensemble: EnsembleRetriever,
        context_builder: KGContextBuilder,
        prompts: Dict[str, Any],
        entity_schema: str,
        relationship_schema: str,
        domain_config: Optional[Dict[str, str]] = None,
    ):
        self.llm_client = llm_client
        self.graph_store = graph_store
        self.chunk_retriever = chunk_retriever
        self.context_builder = context_builder
        self.domain_config = domain_config or {}

        self.gremlin_generator = GremlinQueryGenerator(
            config=GremlinGeneratorConfig(n_triplet_results=10),
            model_client=llm_client,
            entity_schema=entity_schema,
            relationship_schema=relationship_schema,
            prompts=prompts,
            triplet_retriever=triplet_retriever,
        )

        self.triplet_retriever = triplet_retriever
        self.chunk_ensemble = chunk_ensemble

        self.qa_chain = QAChain(model_client=llm_client, prompts=prompts)

    async def graph_rag_generator(
        self,
        query: str,
        n_triplets: int = 10,
        business_context: Optional[Dict[str, str]] = None,
    ) -> AsyncIterator[str]:
        """Stream Graph RAG execution as SSE events."""
        yield self._sse("status", "Retrieving relevant triplets...")

        triplet_context = self.gremlin_generator.retrieve_triplets(query, n_results=n_triplets)
        yield self._sse("triplets", triplet_context[:500])

        yield self._sse("status", "Generating Gremlin traversal...")
        gremlin_raw = await self.gremlin_generator.generate_gremlin_async(
            query=query,
            triplet_context=triplet_context,
            business_context=business_context,
            domain_config=self.domain_config,
        )

        queries = self._split_multi_query(gremlin_raw)
        yield self._sse("gremlin_query", gremlin_raw)

        yield self._sse("status", f"Executing {len(queries)} Gremlin query(ies) on JanusGraph...")
        all_results = []

        for i, gq in enumerate(queries):
            gq = gq.strip()
            if not gq:
                continue
            try:
                partial = await self.graph_store.execute_query_async(gq)
                if partial:
                    all_results.extend(partial)
                    logger.info("Query %d/%d returned %d results", i + 1, len(queries), len(partial))
            except Exception as e:
                yield self._sse("gremlin_error", f"Query {i+1} failed: {e}")
                yield self._sse("status", f"Fixing query {i+1}...")
                try:
                    fixed = await self.gremlin_generator.fix_gremlin_async(
                        query, gq, str(e), triplet_context
                    )
                    yield self._sse("gremlin_query_retry", fixed)
                    for fq in self._split_multi_query(fixed):
                        fq = fq.strip()
                        if not fq:
                            continue
                        try:
                            partial = await self.graph_store.execute_query_async(fq)
                            if partial:
                                all_results.extend(partial)
                        except Exception:
                            pass
                except Exception as e2:
                    yield self._sse("warning", f"Fix for query {i+1} also failed: {e2}")

        if not all_results:
            yield self._sse("status", "Empty results — retrying with broader query...")
            try:
                broader = await self.gremlin_generator.retry_empty_async(
                    query, gremlin_raw, triplet_context
                )
                yield self._sse("gremlin_query_retry", broader)
                for bq in self._split_multi_query(broader):
                    bq = bq.strip()
                    if not bq:
                        continue
                    try:
                        partial = await self.graph_store.execute_query_async(bq)
                        if partial:
                            all_results.extend(partial)
                    except Exception:
                        pass
            except Exception as e3:
                yield self._sse("warning", f"Retry also empty: {e3}")

        if not all_results:
            yield self._sse("answer", "No results found in the knowledge graph for this query.")
            yield self._sse("done", "")
            return

        yield self._sse("status", "Building context from graph results...")
        yield self._sse("result_count", str(len(all_results)))

        context, result_type = self.context_builder.build_context(all_results, query)

        if result_type == "simple_value" and all_results:
            yield self._sse("status", "Enriching with document context...")
            enriched_context = await self._enrich_with_edges(all_results, query)
            if enriched_context:
                context = enriched_context + "\n\n--- Entity Summary ---\n" + context
                result_type = "relationship"

        if triplet_context and triplet_context not in ("No triplets available — using schema only.", "No relevant triplets found."):
            context += f"\n\n--- Relevant Knowledge Graph Triplets ---\n{triplet_context}"

        yield self._sse("context_type", result_type)

        yield self._sse("status", "Generating answer...")
        answer = await self.qa_chain.answer_async(
            query=query,
            context=context,
            result_type=result_type,
            business_context=business_context,
            domain_config=self.domain_config,
        )

        yield self._sse("answer", answer)
        yield self._sse("done", "")

    async def vector_rag_generator(
        self,
        query: str,
        n_chunks: int = 5,
        business_context: Optional[Dict[str, str]] = None,
    ) -> AsyncIterator[str]:
        """Stream Vector RAG execution as SSE events."""
        yield self._sse("status", "Retrieving relevant chunks...")

        chunks = self.chunk_ensemble.search(query, n_results=n_chunks, mode="chunk_search")
        yield self._sse("chunks_found", str(len(chunks)))

        if not chunks:
            yield self._sse("answer", "No relevant document chunks found.")
            yield self._sse("done", "")
            return

        yield self._sse("status", "Generating answer from document context...")
        answer = await self.qa_chain.answer_from_chunks_async(query=query, chunks=chunks)

        yield self._sse("answer", answer)
        yield self._sse("done", "")

    async def hybrid_rag_generator(
        self,
        query: str,
        n_chunks: int = 5,
        n_triplets: int = 10,
        business_context: Optional[Dict[str, str]] = None,
    ) -> AsyncIterator[str]:
        """Stream Hybrid RAG (Graph + Vector) as SSE events."""
        yield self._sse("status", "Running hybrid retrieval (graph + vector)...")

        chunks = self.chunk_ensemble.search(query, n_results=n_chunks, mode="chunk_search")
        yield self._sse("chunks_found", str(len(chunks)))

        triplet_context = self.gremlin_generator.retrieve_triplets(query, n_results=n_triplets)

        gremlin = await self.gremlin_generator.generate_gremlin_async(
            query=query,
            triplet_context=triplet_context,
            business_context=business_context,
            domain_config=self.domain_config,
        )
        yield self._sse("gremlin_query", gremlin)

        graph_results = []
        try:
            graph_results = await self.graph_store.execute_query_async(gremlin)
        except Exception as e:
            yield self._sse("gremlin_error", str(e))

        graph_context = ""
        if graph_results:
            graph_context, _ = self.context_builder.build_context(graph_results, query)

        chunk_context = ""
        if chunks:
            chunk_parts = []
            for c in chunks:
                doc = c.get("document", "")
                meta = c.get("metadata", {})
                source = meta.get("source_file", "")
                chunk_parts.append(f"[{source}]: {doc}")
            chunk_context = "\n\n".join(chunk_parts)

        combined = ""
        if graph_context:
            combined += f"=== Knowledge Graph Context ===\n{graph_context}\n\n"
        if chunk_context:
            combined += f"=== Document Context ===\n{chunk_context}"

        if not combined.strip():
            yield self._sse("answer", "No relevant information found from either graph or documents.")
            yield self._sse("done", "")
            return

        yield self._sse("status", "Generating answer from combined context...")

        system = "You are a helpful analyst. Answer questions using both knowledge graph and document context. Synthesize information from both sources."
        response = await self.llm_client.generate_text_async(
            prompt=f"Context:\n{combined}\n\nQuestion: {query}",
            system_prompt=system,
            temperature=0.2,
            max_tokens=32768,
        )

        yield self._sse("answer", response.content)
        yield self._sse("done", "")

    @staticmethod
    def _split_multi_query(gremlin_raw: str) -> list:
        """Split a `---`-separated multi-query into individual Gremlin traversals."""
        parts = [p.strip() for p in gremlin_raw.split("---") if p.strip()]
        if not parts:
            return [gremlin_raw]
        return parts

    async def _enrich_with_edges(self, simple_results: list, query: str) -> str:
        """When a Gremlin query returned only entity metadata (no edges),
        follow up by traversing edges from those entities to fetch
        page_id / source_file from edges, then look up chunk text."""
        vertex_ids = []
        for item in simple_results:
            if isinstance(item, dict):
                vid = item.get("T.id")
                if vid:
                    vertex_ids.append(vid)
        if not vertex_ids:
            return ""

        ids_str = ",".join(str(v) for v in vertex_ids[:5])
        enrich_q = (
            f"g.V({ids_str})"
            f".as('source').bothE().as('edge').otherV().as('target')"
            f".select('source','edge','target').by(elementMap())"
            f".limit(30).toList()"
        )
        try:
            edge_results = await self.graph_store.execute_query_async(enrich_q)
            if edge_results:
                enriched, rtype = self.context_builder.build_context(edge_results, query)
                if enriched.strip():
                    logger.info("Enriched %d simple results with %d edge traversals", len(simple_results), len(edge_results))
                    return enriched
        except Exception as e:
            logger.warning("Edge enrichment failed: %s", e)
        return ""

    @staticmethod
    def _sse(event: str, data: str) -> str:
        """Format a Server-Sent Event."""
        payload = json.dumps({"event": event, "data": data})
        return f"data: {payload}\n\n"
