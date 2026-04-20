"""Pydantic configuration models for LLM clients."""

from typing import Dict, Any, Optional
from pydantic import BaseModel, Field, field_validator


class BaseModelConfig(BaseModel):
    """Base configuration for all model clients."""

    default_model: str = Field(description="Default model to use")
    temperature: float = Field(default=0.1, ge=0.0, le=2.0)
    max_tokens: int = Field(default=32768, gt=0)
    timeout: float = Field(default=240, gt=0)
    max_retries: int = Field(default=2, ge=0)
    max_rpm: int = Field(default=60, gt=0)
    max_tpm: int = Field(default=15000, gt=0)


class OpenAICompatibleConfig(BaseModelConfig):
    """Configuration for OpenAI-compatible APIs (Crystal gateway, vLLM, etc.)."""

    api_key: str = Field(default="EMPTY", description="API key")
    base_url: str = Field(description="API base URL (e.g. https://gateway-dev.llm.crystal.ai/v1)")
    default_model: str = Field(default="Qwen/Qwen3-32B")
    max_rpm: int = Field(default=40, gt=0)
    max_tpm: int = Field(default=30000, gt=0)

    @field_validator("base_url")
    @classmethod
    def validate_base_url(cls, v: str) -> str:
        if not v or not v.startswith(("http://", "https://")):
            raise ValueError("Base URL must start with http:// or https://")
        return v

    @property
    def is_local_model(self) -> bool:
        return self.api_key == "EMPTY"


class EmbeddingConfig(BaseModel):
    """Configuration for embedding models."""

    model_name: str = Field(default="Qwen/Qwen3-Embedding")
    base_url: str = Field(default="https://gateway-dev.llm.crystal.ai/v1")
    api_key: str = Field(default="EMPTY")
    dimension: int = Field(default=1024)
    batch_size: int = Field(default=32)
