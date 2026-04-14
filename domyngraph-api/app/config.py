"""Configuration for DomynGraph Lens API — loaded from environment variables."""

from __future__ import annotations

import os


class Settings:
    GREMLIN_HOST: str = os.getenv("GREMLIN_HOST", "localhost")
    GREMLIN_PORT: int = int(os.getenv("GREMLIN_PORT", "8182"))
    GREMLIN_URL: str = os.getenv(
        "GREMLIN_URL",
        f"ws://{os.getenv('GREMLIN_HOST', 'localhost')}:{os.getenv('GREMLIN_PORT', '8182')}/gremlin",
    )
    GRAPH_ALIAS: str = os.getenv("GRAPH_ALIAS", "graph")

    CORS_ORIGIN: str = os.getenv("CORS_ORIGIN", "http://localhost:3000")
    QUERY_TIMEOUT_S: int = int(os.getenv("QUERY_TIMEOUT_S", "10"))
    PROCEDURE_TIMEOUT_S: int = int(os.getenv("PROCEDURE_TIMEOUT_S", "30"))
    MAX_EXPAND_NODES: int = int(os.getenv("MAX_EXPAND_NODES", "100"))
    MAX_EXPAND_DEPTH: int = int(os.getenv("MAX_EXPAND_DEPTH", "3"))

    CACHE_TTL_S: int = int(os.getenv("CACHE_TTL_S", "30"))
    CACHE_MAX_SIZE: int = int(os.getenv("CACHE_MAX_SIZE", "500"))

    RATE_LIMIT_PER_S: int = int(os.getenv("RATE_LIMIT_PER_S", "100"))


settings = Settings()
