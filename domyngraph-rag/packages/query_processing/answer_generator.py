"""QA chain for generating final answers from retrieved context."""

from typing import Dict, Any, Optional, List
import logging

logger = logging.getLogger("domyngraph-rag.query_processing.qa")


class AnswerGeneratorConfig:
    def __init__(self, max_context_length: int = 14000):
        self.max_context_length = max_context_length


class QAChain:
    """Generate answers from context using LLM."""

    def __init__(self, model_client: Any, prompts: Optional[Dict[str, Any]] = None):
        self.model_client = model_client
        self.prompts = prompts or {}

    async def answer_async(
        self,
        query: str,
        context: str,
        result_type: str = "simple_value",
        business_context: Optional[Dict[str, str]] = None,
        domain_config: Optional[Dict[str, str]] = None,
    ) -> str:
        """Generate an answer from context + query."""
        ctx = business_context or {}
        domain = domain_config or {}

        if result_type == "simple_value":
            prompts = self.prompts.get("simple_value_prompt", {})
            system = prompts.get("simple_value_system_prompt", "You are a helpful analyst.").format(
                ENDUSER_INFO=ctx.get("enduser_info", "analyst"),
                DOMAIN_INFO=domain.get("domain_info", "knowledge"),
            )
            user = prompts.get("simple_value_context_prompt", "Data: {GREMLIN_RESULT}\n\nQuestion: {USER_QUERY}").format(
                GREMLIN_RESULT=context,
                USER_QUERY=query,
            )
        else:
            prompts_section = self.prompts.get("relationship_based_prompts", {})
            system = prompts_section.get(
                "relationship_system_message",
                "You are a helpful analyst expert in answering questions based on the provided context.",
            )
            user_template = prompts_section.get(
                "relationship_user_prompt",
                "Context:\n{CONTEXT}\n\nAnswer: {USER_QUERY}",
            )
            user = f"Context:\n{context}\n\n" + user_template.format(USER_QUERY=query)

        response = await self.model_client.generate_text_async(
            prompt=user,
            system_prompt=system,
            temperature=0.2,
            max_tokens=8192,
        )
        return response.content

    async def answer_from_chunks_async(
        self,
        query: str,
        chunks: List[Dict[str, Any]],
    ) -> str:
        """Generate answer from vector-retrieved chunks."""
        context_parts = []
        for c in chunks:
            doc = c.get("document", "")
            meta = c.get("metadata", {})
            source = meta.get("source_file", "")
            page = meta.get("page_id", "")
            header = f"[{source} p.{page}]" if source else ""
            context_parts.append(f"{header}\n{doc}")

        context = "\n\n".join(context_parts)

        system = "You are a helpful analyst. Answer questions based on the provided document context. Be specific and cite sources when possible."

        response = await self.model_client.generate_text_async(
            prompt=f"Context:\n{context}\n\nQuestion: {query}",
            system_prompt=system,
            temperature=0.2,
            max_tokens=8192,
        )
        return response.content
