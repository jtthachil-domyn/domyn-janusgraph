"""Standardized LLM response models."""

from datetime import datetime, timezone
from typing import Any, Dict, Optional
from pydantic import BaseModel, Field


class LLMResponse(BaseModel):
    """Standardized response from any LLM provider."""

    content: str = Field(description="The main response content")
    model: str = Field(description="Model that generated the response")
    timestamp: datetime = Field(default_factory=lambda: datetime.now(timezone.utc))
    usage: Optional[Dict[str, Any]] = Field(default=None)
    finish_reason: Optional[str] = Field(default=None)
    response_time_ms: Optional[float] = Field(default=None)
    provider: str = Field(description="Provider name")
