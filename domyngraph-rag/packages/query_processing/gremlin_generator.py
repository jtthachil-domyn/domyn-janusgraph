"""Gremlin query generator — LLM translates natural language to Gremlin traversals."""

from typing import Dict, Any, Optional, List
import logging
import re

logger = logging.getLogger("domyngraph-rag.query_processing.gremlin")


class GremlinGeneratorConfig:
    """Configuration for Gremlin query generator."""

    def __init__(
        self,
        n_triplet_results: int = 10,
        triplet_search_mode: str = "entity_search",
        enable_triplet_retrieval: bool = True,
    ):
        self.n_triplet_results = n_triplet_results
        self.triplet_search_mode = triplet_search_mode
        self.enable_triplet_retrieval = enable_triplet_retrieval


class GremlinQueryGenerator:
    """
    Generates Gremlin traversals from natural language using an LLM.
    Replaces CypherQueryGenerator from UDA — targets JanusGraph instead of Memgraph.
    """

    def __init__(
        self,
        config: GremlinGeneratorConfig,
        model_client: Any,
        entity_schema: str,
        relationship_schema: str,
        prompts: Dict[str, Any],
        triplet_retriever: Optional[Any] = None,
    ):
        self.config = config
        self.model_client = model_client
        self.entity_schema = entity_schema
        self.relationship_schema = relationship_schema
        self.prompts = prompts
        self.triplet_retriever = triplet_retriever

    def retrieve_triplets(self, query: str, n_results: Optional[int] = None) -> str:
        """Retrieve relevant triplets as context for Gremlin generation.
        Falls back gracefully if vector index is empty or unavailable."""
        if not self.config.enable_triplet_retrieval or not self.triplet_retriever:
            return "No triplets available — using schema only."

        n = n_results or self.config.n_triplet_results
        try:
            results = self.triplet_retriever.search(
                query, n_results=n, mode=self.config.triplet_search_mode
            )
        except Exception as e:
            logger.warning("Triplet retrieval failed (vector index may be empty): %s", e)
            return "No triplets available — using schema only."

        if not results:
            return "No relevant triplets found."

        triplet_lines = []
        for r in results:
            doc = r.get("document", "")
            if doc:
                triplet_lines.append(f"- {doc}")

        return "\n".join(triplet_lines) if triplet_lines else "No relevant triplets found."

    async def generate_gremlin_async(
        self,
        query: str,
        triplet_context: Optional[str] = None,
        business_context: Optional[Dict[str, str]] = None,
        domain_config: Optional[Dict[str, str]] = None,
    ) -> str:
        """Generate a Gremlin traversal from natural language query."""
        if triplet_context is None:
            triplet_context = self.retrieve_triplets(query)

        ctx = business_context or {}
        domain = domain_config or {}

        system_template = self.prompts.get("system_prompt", {}).get("instruction", "")
        user_template = self.prompts.get("user_prompt", {}).get("message", "")

        system_prompt = system_template.format(
            DOMAIN_DESCRIPTION=domain.get("domain_description", "knowledge graphs"),
            DOMAIN_INFO=domain.get("domain_info", "structured information"),
            DOCUMENT_TYPE=domain.get("document_type", "documents"),
            USECASE_INFO=ctx.get("usecase_info", "General knowledge retrieval"),
            USECASE_GOAL=ctx.get("usecase_goal", "Answer user questions accurately"),
            USECASE_APPLICATION=ctx.get("usecase_application", "RAG system"),
            ENDUSER_INFO=ctx.get("enduser_info", "analyst"),
            ENTITY_SCHEMA=self.entity_schema,
            RELATIONSHIP_SCHEMA=self.relationship_schema,
            ENTITY_PROPERTIES=domain.get("entity_properties", ""),
            RELATIONSHIP_PROPERTIES=domain.get("relationship_properties", ""),
            DATASET_SPECIFIC_REQUIREMENTS=domain.get("dataset_specific_requirements", ""),
            SCHEMA_BASED_EXAMPLES=domain.get("schema_based_examples", ""),
        )

        user_prompt = user_template.format(
            USER_QUERY=query,
            TRIPLET_RETRIEVED=triplet_context,
        )

        response = await self.model_client.generate_text_async(
            prompt=user_prompt,
            system_prompt=system_prompt,
            temperature=0.0,
            max_tokens=4096,
        )

        gremlin = self._clean_gremlin(response.content)
        logger.info("Generated Gremlin: %s", gremlin[:200])
        return gremlin

    async def fix_gremlin_async(
        self,
        original_query: str,
        failed_gremlin: str,
        error_message: str,
        triplet_context: str = "",
    ) -> str:
        """Fix a failed Gremlin traversal using LLM error correction."""
        error_prompts = self.prompts.get("error_correction_prompt", {})
        system = error_prompts.get("system_prompt", "").format(
            ENTITY_SCHEMA=self.entity_schema,
            RELATIONSHIP_SCHEMA=self.relationship_schema,
        )
        user = error_prompts.get("user_prompt", "").format(
            USER_QUERY=original_query,
            FAILED_GREMLIN_QUERY=failed_gremlin,
            ERROR_MESSAGE=error_message,
            TRIPLET_RETRIEVED=triplet_context,
        )

        response = await self.model_client.generate_text_async(
            prompt=user, system_prompt=system, temperature=0.0, max_tokens=4096
        )
        return self._clean_gremlin(response.content)

    async def retry_empty_async(
        self,
        original_query: str,
        empty_gremlin: str,
        triplet_context: str = "",
    ) -> str:
        """Generate an optimized Gremlin when the previous query returned empty results."""
        retry_prompts = self.prompts.get("empty_result_retry_prompt", {})
        system = retry_prompts.get("system_prompt", "").format(
            ENTITY_SCHEMA=self.entity_schema,
            RELATIONSHIP_SCHEMA=self.relationship_schema,
        )
        user = retry_prompts.get("user_prompt", "").format(
            USER_QUERY=original_query,
            EMPTY_RESULT_QUERY=empty_gremlin,
            TRIPLET_RETRIEVED=triplet_context,
        )

        response = await self.model_client.generate_text_async(
            prompt=user, system_prompt=system, temperature=0.1, max_tokens=4096
        )
        return self._clean_gremlin(response.content)

    @staticmethod
    def _clean_gremlin(raw: str) -> str:
        """Extract clean Gremlin traversal(s) from LLM output.
        Supports multi-query output separated by '---'."""
        text = raw.strip()

        code_blocks = re.findall(r"```(?:gremlin|groovy)?\s*\n?(.*?)```", text, re.DOTALL)
        if code_blocks:
            text = "\n---\n".join(b.strip() for b in code_blocks)

        if "---" in text:
            parts = []
            for segment in text.split("---"):
                segment = segment.strip()
                if not segment:
                    continue
                for line in segment.split("\n"):
                    stripped = line.strip()
                    if stripped.startswith("g.") or stripped.startswith("graph.traversal()"):
                        parts.append(stripped)
                        break
                else:
                    if segment:
                        parts.append(segment.split("\n")[0].strip())
            return "\n---\n".join(parts) if parts else text

        for line in text.split("\n"):
            stripped = line.strip()
            if stripped.startswith("g.") or stripped.startswith("graph.traversal()"):
                return stripped

        return text.split("\n")[0].strip() if text else text
