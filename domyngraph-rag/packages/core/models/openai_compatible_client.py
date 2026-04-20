"""Client for OpenAI-compatible APIs (Crystal AI gateway, vLLM, Ollama, etc.)."""

from typing import Dict, List, Optional, Any
import time
import logging
import httpx

from .base_client import BaseModelClient
from .response_models import LLMResponse
from .config_models import OpenAICompatibleConfig

logger = logging.getLogger("domyngraph-rag.models.openai_compat")


class OpenAICompatibleClient(BaseModelClient):
    """
    Client for any OpenAI-compatible endpoint.
    Targets the Crystal AI gateway at https://gateway-dev.llm.crystal.ai
    """

    def __init__(self, config: OpenAICompatibleConfig):
        super().__init__(config)
        self.base_url = config.base_url.rstrip("/")
        self.api_key = config.api_key

        self._client = httpx.AsyncClient(
            timeout=httpx.Timeout(self.timeout, connect=10.0),
            limits=httpx.Limits(
                max_keepalive_connections=100,
                max_connections=200,
                keepalive_expiry=30.0,
            ),
            http2=True,
        )
        logger.info("OpenAI-compatible client → %s model=%s", self.base_url, config.default_model)

    async def generate_text_async(
        self,
        prompt: str,
        system_prompt: Optional[str] = None,
        context: Optional[List[Dict[str, str]]] = None,
        temperature: Optional[float] = None,
        max_tokens: Optional[int] = None,
        model_name: Optional[str] = None,
        enable_thinking: Optional[bool] = None,
        **kwargs,
    ) -> LLMResponse:
        await self.rate_limit_check()

        messages = self._build_messages(prompt, system_prompt, context)
        model = model_name or self.default_model
        temp = temperature if temperature is not None else self.temperature
        tokens = max_tokens if max_tokens is not None else self.max_tokens

        headers = {"Content-Type": "application/json"}
        if self.api_key and self.api_key != "EMPTY":
            headers["Authorization"] = f"Bearer {self.api_key}"

        payload: Dict[str, Any] = {
            "model": model,
            "messages": messages,
            "max_tokens": tokens,
            "temperature": temp,
        }

        if enable_thinking is not None:
            payload["chat_template_kwargs"] = {"enable_thinking": enable_thinking}

        start = time.time()

        for attempt in range(self.max_retries + 1):
            try:
                url = f"{self.base_url}/chat/completions"
                resp = await self._client.post(url, headers=headers, json=payload)

                if resp.status_code != 200:
                    raise Exception(f"API {resp.status_code}: {resp.text[:300]}")

                data = resp.json()
                elapsed = (time.time() - start) * 1000

                raw_content = data["choices"][0]["message"]["content"]

                if not enable_thinking:
                    parts = raw_content.split("</think>")
                    final_content = parts[-1].strip() if len(parts) > 1 else raw_content
                else:
                    final_content = raw_content

                usage = None
                if "usage" in data:
                    u = data["usage"]
                    usage = {
                        "prompt_tokens": u.get("prompt_tokens", 0),
                        "completion_tokens": u.get("completion_tokens", 0),
                        "total_tokens": u.get("total_tokens", 0),
                    }
                self.track_usage(usage)

                return LLMResponse(
                    content=final_content,
                    model=model,
                    usage=usage,
                    finish_reason=data["choices"][0].get("finish_reason"),
                    response_time_ms=elapsed,
                    provider=self.provider_name,
                )

            except Exception as e:
                if attempt < self.max_retries:
                    logger.warning("Attempt %d failed: %s — retrying", attempt + 1, e)
                    continue
                logger.error("All %d attempts failed: %s", self.max_retries + 1, e)
                return LLMResponse(
                    content=f"Error: {e}",
                    model=model,
                    provider=self.provider_name,
                    finish_reason="error",
                    response_time_ms=(time.time() - start) * 1000,
                )

    @property
    def provider_name(self) -> str:
        return "openai_compatible"

    async def close(self):
        if hasattr(self, "_client"):
            await self._client.aclose()
