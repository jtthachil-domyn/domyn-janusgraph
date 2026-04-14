"""GremlinService — singleton managing the gremlinpython connection to JanusGraph.

Handles: connection pooling, per-request timeout, error normalization, reconnection.
"""

from __future__ import annotations

import asyncio
import logging
import time
from typing import Any

from gremlin_python.driver.client import Client
from gremlin_python.driver.serializer import GraphSONSerializersV3d0

from app.config import settings

logger = logging.getLogger("domyngraph.gremlin")


class GremlinError(Exception):
    """Normalized Gremlin error with a clean message and optional status code."""

    def __init__(self, message: str, status_code: int = 500, raw: str | None = None):
        super().__init__(message)
        self.status_code = status_code
        self.raw = raw


class GremlinService:
    """Singleton Gremlin client wrapper."""

    _instance: GremlinService | None = None
    _client: Client | None = None

    def __new__(cls) -> GremlinService:
        if cls._instance is None:
            cls._instance = super().__new__(cls)
        return cls._instance

    @property
    def client(self) -> Client:
        if self._client is None:
            self._connect()
        return self._client  # type: ignore[return-value]

    def _connect(self) -> None:
        logger.info("Connecting to Gremlin Server at %s (alias=%s)", settings.GREMLIN_URL, settings.GRAPH_ALIAS)
        try:
            self._client = Client(
                settings.GREMLIN_URL,
                settings.GRAPH_ALIAS,
                message_serializer=GraphSONSerializersV3d0(),
            )
            logger.info("Gremlin connection established")
        except Exception as exc:
            logger.error("Failed to connect to Gremlin Server: %s", exc)
            self._client = None
            raise GremlinError(
                f"Cannot connect to DomynGraph: {exc}",
                status_code=503,
                raw=str(exc),
            ) from exc

    def reconnect(self) -> None:
        """Force reconnection (e.g., after a connection drop)."""
        if self._client is not None:
            try:
                self._client.close()
            except Exception:
                pass
            self._client = None
        self._connect()

    def submit(self, query: str, timeout_s: int | None = None) -> list[Any]:
        """Submit a Gremlin query and return the raw result list.

        Applies per-request timeout. Normalizes errors into GremlinError.
        """
        effective_timeout = timeout_s or settings.QUERY_TIMEOUT_S
        start = time.monotonic()

        try:
            result_set = self.client.submit(query)
            results = result_set.all().result()
            elapsed = int((time.monotonic() - start) * 1000)
            logger.debug("Query completed in %dms: %s", elapsed, query[:120])
            return results
        except Exception as exc:
            elapsed = int((time.monotonic() - start) * 1000)
            error_str = str(exc)

            if "Could not alias" in error_str or "not in the Graph" in error_str:
                raise GremlinError(
                    "Graph binding not found. Is the DomynGraph cluster running?",
                    status_code=503,
                    raw=error_str,
                ) from exc

            if "timed out" in error_str.lower() or "timeout" in error_str.lower():
                raise GremlinError(
                    f"Query timed out after {elapsed}ms",
                    status_code=504,
                    raw=error_str,
                ) from exc

            if "599" in error_str or "serializ" in error_str.lower():
                raise GremlinError(
                    "Server error during query execution",
                    status_code=500,
                    raw=error_str,
                ) from exc

            status = 500
            if "4" in error_str[:5]:
                status = 400

            raise GremlinError(
                f"Gremlin query failed ({elapsed}ms): {error_str[:200]}",
                status_code=status,
                raw=error_str,
            ) from exc

    async def submit_async(self, query: str, timeout_s: int | None = None) -> list[Any]:
        """Async wrapper — runs the blocking submit in a thread executor."""
        loop = asyncio.get_event_loop()
        return await loop.run_in_executor(None, self.submit, query, timeout_s)

    def close(self) -> None:
        if self._client is not None:
            try:
                self._client.close()
            except Exception:
                pass
            self._client = None

    def is_connected(self) -> bool:
        try:
            self.submit("1+1", timeout_s=5)
            return True
        except Exception:
            return False


gremlin_service = GremlinService()
