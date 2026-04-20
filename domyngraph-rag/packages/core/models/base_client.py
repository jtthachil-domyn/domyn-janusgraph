"""Base model client with async/sync interfaces and rate limiting."""

from abc import ABC, abstractmethod
from typing import Dict, List, Optional, Any
from concurrent.futures import ThreadPoolExecutor
import time
import asyncio
import logging

from .response_models import LLMResponse
from .config_models import BaseModelConfig

logger = logging.getLogger("domyngraph-rag.models")


class BaseModelClient(ABC):
    """Base class for LLM clients with rate limiting and retry logic."""

    def __init__(self, config: BaseModelConfig):
        self.config = config
        self.requests_per_minute = config.max_rpm
        self.tokens_per_minute = config.max_tpm
        self.temperature = config.temperature
        self.max_tokens = config.max_tokens
        self.timeout = config.timeout
        self.max_retries = config.max_retries
        self.default_model = config.default_model
        self.request_count = 0
        self.token_count = 0
        self.start_time = time.time()

    @abstractmethod
    async def generate_text_async(
        self,
        prompt: str,
        system_prompt: Optional[str] = None,
        context: Optional[List[Dict[str, str]]] = None,
        temperature: Optional[float] = None,
        max_tokens: Optional[int] = None,
        model_name: Optional[str] = None,
        **kwargs,
    ) -> LLMResponse:
        pass

    def generate_text(
        self,
        prompt: str,
        system_prompt: Optional[str] = None,
        context: Optional[List[Dict[str, str]]] = None,
        temperature: Optional[float] = None,
        max_tokens: Optional[int] = None,
        model_name: Optional[str] = None,
        **kwargs,
    ) -> LLMResponse:
        """Synchronous wrapper around async text generation."""

        def _run():
            return asyncio.run(
                self.generate_text_async(
                    prompt=prompt,
                    system_prompt=system_prompt,
                    context=context,
                    temperature=temperature,
                    max_tokens=max_tokens,
                    model_name=model_name,
                    **kwargs,
                )
            )

        with ThreadPoolExecutor(max_workers=1) as pool:
            return pool.submit(_run).result()

    def _build_messages(
        self,
        prompt: str,
        system_prompt: Optional[str] = None,
        context: Optional[List[Dict[str, str]]] = None,
    ) -> List[Dict[str, str]]:
        messages = []
        if system_prompt:
            messages.append({"role": "system", "content": system_prompt})
        if context:
            messages.extend(context)
        messages.append({"role": "user", "content": prompt})
        return messages

    def _reset_limits_if_needed(self):
        if time.time() - self.start_time > 60:
            self.request_count = 0
            self.token_count = 0
            self.start_time = time.time()

    async def rate_limit_check(self):
        self._reset_limits_if_needed()
        if self.request_count >= self.requests_per_minute:
            wait = 60 - (time.time() - self.start_time)
            logger.info("Request limit reached, waiting %.1fs", wait)
            await asyncio.sleep(max(wait, 0))
            self._reset_limits_if_needed()

    def track_usage(self, usage_info: Optional[Dict[str, Any]]):
        if usage_info and "total_tokens" in usage_info:
            self.token_count += usage_info["total_tokens"]
        self.request_count += 1

    @property
    @abstractmethod
    def provider_name(self) -> str:
        pass
