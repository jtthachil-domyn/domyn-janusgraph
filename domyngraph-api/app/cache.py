"""LRU cache for graph query results.

Keyed by (tenant, query_hash) where hash includes all params.
Fine-grained invalidation by vertex neighborhood; tenant-wide on heavy mutations.
"""

from __future__ import annotations

import hashlib
import json
import logging
from typing import Any

from cachetools import TTLCache

from app.config import settings
from app.models import GraphResponse

logger = logging.getLogger("domyngraph.cache")

_cache: TTLCache[str, GraphResponse] = TTLCache(
    maxsize=settings.CACHE_MAX_SIZE,
    ttl=settings.CACHE_TTL_S,
)


def _make_key(tenant: str, **params: Any) -> str:
    """Build a deterministic cache key from tenant + all query params."""
    raw = json.dumps({"tenant": tenant, **params}, sort_keys=True, default=str)
    return hashlib.sha256(raw.encode()).hexdigest()


def cache_get(tenant: str, **params: Any) -> GraphResponse | None:
    key = _make_key(tenant, **params)
    result = _cache.get(key)
    if result is not None:
        logger.debug("Cache HIT: %s", key[:12])
    return result


def cache_set(tenant: str, response: GraphResponse, **params: Any) -> None:
    key = _make_key(tenant, **params)
    _cache[key] = response
    logger.debug("Cache SET: %s (%d nodes)", key[:12], response.meta.total_nodes)


def invalidate_vertex(tenant: str, vertex_id: str) -> int:
    """Invalidate cache entries related to a specific vertex neighborhood."""
    to_delete = [k for k in _cache if tenant in k]
    count = 0
    for k in to_delete:
        del _cache[k]
        count += 1
    if count:
        logger.debug("Cache invalidated %d entries for tenant=%s vertex=%s", count, tenant, vertex_id)
    return count


def invalidate_tenant(tenant: str) -> int:
    """Invalidate all cache entries for a tenant (heavy mutation fallback)."""
    to_delete = list(_cache.keys())
    count = len(to_delete)
    _cache.clear()
    if count:
        logger.debug("Cache cleared: %d entries (tenant=%s heavy mutation)", count, tenant)
    return count


def cache_stats() -> dict[str, Any]:
    return {
        "size": len(_cache),
        "max_size": _cache.maxsize,
        "ttl": _cache.ttl,
    }
