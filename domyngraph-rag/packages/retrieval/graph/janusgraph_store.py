"""JanusGraph graph store for retrieval operations via Gremlin.

Uses a persistent connection pool and parameterized queries to avoid:
1. Client-per-query connection churn (WebSocket + serializer allocation)
2. Groovy script compilation leak (unique script strings = new classes in JVM heap)
3. Event-loop conflicts with FastAPI (blocking calls in ThreadPoolExecutor)
"""

from typing import List, Dict, Any, Optional
import asyncio
import time
import re
import logging
import threading
import concurrent.futures

from gremlin_python.driver.client import Client
from gremlin_python.driver.serializer import GraphSONSerializersV3d0

logger = logging.getLogger("domyngraph-rag.retrieval.janusgraph")

_executor = concurrent.futures.ThreadPoolExecutor(
    max_workers=4, thread_name_prefix="gremlin"
)


def _normalize_gremlin(query: str) -> str:
    """Convert g.V() style to graph.traversal().V() for JanusGraph server compatibility."""
    q = query.strip()
    if q.startswith("g."):
        q = "graph.traversal()." + q[2:]
    return q


_TEXT_PRED_PATTERN = re.compile(
    r"\.has\(\s*'([^']+)'\s*,\s*"
    r"(textContains|textContainsPrefix|textContainsRegex|textContainsFuzzy)"
    r"\(\s*'([^']*)'\s*\)\s*\)"
)
_EXACT_HAS_PATTERN = re.compile(
    r"\.has\(\s*'([^']+)'\s*,\s*'([^']*)'\s*\)"
)
_LIMIT_PATTERN = re.compile(r"\.limit\(\s*(\d+)\s*\)")


def _parameterize_query(query: str):
    """Extract literal values from Gremlin into bindings so JanusGraph can cache
    the compiled script template. Returns (parameterized_query, bindings_dict).

    Example:
        .has('name', textContains('Revenue')).has('tenant_id', 'NVDA').limit(20)
      becomes:
        .has('name', textContains(p0)).has('tenant_id', p1).limit(p2)
      with bindings {'p0': 'Revenue', 'p1': 'NVDA', 'p2': 20}
    """
    bindings = {}
    counter = [0]

    def _replace_text_pred(m):
        prop_name = m.group(1)
        pred_func = m.group(2)
        value = m.group(3)
        key = f"p{counter[0]}"
        counter[0] += 1
        bindings[key] = value
        return f".has('{prop_name}', {pred_func}({key}))"

    def _replace_exact(m):
        prop_name = m.group(1)
        value = m.group(2)
        key = f"p{counter[0]}"
        counter[0] += 1
        bindings[key] = value
        return f".has('{prop_name}', {key})"

    def _replace_limit(m):
        value = int(m.group(1))
        key = f"p{counter[0]}"
        counter[0] += 1
        bindings[key] = value
        return f".limit({key})"

    parameterized = _TEXT_PRED_PATTERN.sub(_replace_text_pred, query)
    parameterized = _EXACT_HAS_PATTERN.sub(_replace_exact, parameterized)
    parameterized = _LIMIT_PATTERN.sub(_replace_limit, parameterized)

    return parameterized, bindings


class _ConnectionPool:
    """Thread-safe pool of persistent gremlin-python Client connections.

    On any connection error, refreshes ALL clients (the server likely
    restarted, so all sockets are dead) and retries once before raising.
    """

    def __init__(self, url: str, alias: str, pool_size: int = 2):
        self._url = url
        self._alias = alias
        self._pool_size = pool_size
        self._clients: list = []
        self._index = 0
        self._lock = threading.Lock()
        self._init_pool()

    def _make_client(self) -> Client:
        return Client(
            self._url,
            self._alias,
            message_serializer=GraphSONSerializersV3d0(),
        )

    def _init_pool(self):
        for _ in range(self._pool_size):
            self._clients.append(self._make_client())
        logger.info("Gremlin connection pool: %d clients → %s", self._pool_size, self._url)

    def submit(self, query: str, bindings: Optional[dict] = None) -> list:
        """Round-robin a query across pooled clients. Auto-reconnects on failure."""
        with self._lock:
            client = self._clients[self._index % self._pool_size]
            self._index += 1

        try:
            return self._do_submit(client, query, bindings)
        except Exception as e:
            err_str = str(e).lower()
            is_conn_error = any(k in err_str for k in (
                "connection was already closed", "connection reset",
                "connection refused", "broken pipe", "eof",
            ))
            if not is_conn_error:
                raise

            logger.warning("Connection error, refreshing all pool clients: %s", e)
            self._refresh_all()

            with self._lock:
                fresh_client = self._clients[0]
            return self._do_submit(fresh_client, query, bindings)

    @staticmethod
    def _do_submit(client: Client, query: str, bindings: Optional[dict]) -> list:
        if bindings:
            rs = client.submit(query, bindings)
        else:
            rs = client.submit(query)
        return rs.all().result()

    def _refresh_all(self):
        """Close all existing clients and create fresh ones."""
        with self._lock:
            for i, old in enumerate(self._clients):
                try:
                    old.close()
                except Exception:
                    pass
                self._clients[i] = self._make_client()
            self._index = 0
            logger.info("Refreshed all %d Gremlin pool clients", self._pool_size)

    def close_all(self):
        for c in self._clients:
            try:
                c.close()
            except Exception:
                pass
        self._clients.clear()


_pools: Dict[str, _ConnectionPool] = {}
_pools_lock = threading.Lock()


def _get_pool(url: str, alias: str) -> _ConnectionPool:
    key = f"{url}|{alias}"
    with _pools_lock:
        if key not in _pools:
            _pools[key] = _ConnectionPool(url, alias)
        return _pools[key]


def _run_gremlin(url: str, alias: str, query: str) -> list:
    """Execute a Gremlin query using the persistent connection pool with parameterized bindings."""
    query = _normalize_gremlin(query)
    parameterized, bindings = _parameterize_query(query)
    pool = _get_pool(url, alias)

    if bindings:
        logger.debug("Parameterized: %s  bindings=%s", parameterized[:120], bindings)
        return pool.submit(parameterized, bindings)
    else:
        return pool.submit(query)


class JanusGraphStore:
    """
    JanusGraph graph store for executing Gremlin queries in the RAG pipeline.
    Query-only — all writes are handled by the indexing package.

    Uses persistent connection pooling and parameterized queries to avoid
    JVM heap pressure from Groovy script compilation.
    """

    def __init__(
        self,
        gremlin_url: str = "ws://localhost:8182/gremlin",
        graph_alias: str = "graph",
        max_retries: int = 3,
        retry_delay: float = 0.5,
    ):
        self.gremlin_url = gremlin_url
        self.graph_alias = graph_alias
        self.max_retries = max_retries
        self.retry_delay = retry_delay
        _get_pool(gremlin_url, graph_alias)
        logger.info("JanusGraphStore → %s alias=%s (pooled, parameterized)", gremlin_url, graph_alias)

    async def execute_query_async(self, query: str) -> List[Any]:
        """Execute a Gremlin traversal — safe to call from async context."""
        loop = asyncio.get_running_loop()

        for attempt in range(1, self.max_retries + 1):
            try:
                start = time.monotonic()
                raw_results = await loop.run_in_executor(
                    _executor, _run_gremlin, self.gremlin_url, self.graph_alias, query
                )
                elapsed = int((time.monotonic() - start) * 1000)
                logger.info("Gremlin OK (%dms, %d rows): %s", elapsed, len(raw_results), query[:120])
                return self._serialize_results(raw_results)

            except Exception as e:
                if attempt < self.max_retries:
                    delay = self.retry_delay * (2 ** (attempt - 1))
                    logger.warning("Attempt %d failed: %s — retrying in %.1fs", attempt, e, delay)
                    await asyncio.sleep(delay)
                else:
                    raise RuntimeError(
                        f"Gremlin query failed after {self.max_retries} attempts: {query[:200]}\nError: {e}"
                    )

    def execute_query(self, query: str) -> List[Any]:
        """Sync wrapper for non-async contexts (e.g. indexing)."""
        for attempt in range(1, self.max_retries + 1):
            try:
                start = time.monotonic()
                raw = _run_gremlin(self.gremlin_url, self.graph_alias, query)
                elapsed = int((time.monotonic() - start) * 1000)
                logger.info("Gremlin OK (%dms, %d rows): %s", elapsed, len(raw), query[:120])
                return self._serialize_results(raw)
            except Exception as e:
                if attempt < self.max_retries:
                    delay = self.retry_delay * (2 ** (attempt - 1))
                    logger.warning("Attempt %d failed: %s — retrying in %.1fs", attempt, e, delay)
                    time.sleep(delay)
                else:
                    raise RuntimeError(
                        f"Gremlin query failed after {self.max_retries} attempts: {query[:200]}\nError: {e}"
                    )

    def _serialize_results(self, results: list) -> List[Any]:
        """Convert Gremlin result objects to plain Python types."""
        serialized = []
        for item in results:
            serialized.append(self._serialize_value(item))
        return serialized

    def _serialize_value(self, value: Any) -> Any:
        if isinstance(value, dict):
            return {str(k): self._serialize_value(v) for k, v in value.items()}
        elif isinstance(value, (list, tuple)):
            return [self._serialize_value(v) for v in value]
        elif hasattr(value, "keys"):
            return {str(k): self._serialize_value(value[k]) for k in value.keys()}
        elif hasattr(value, "__iter__") and not isinstance(value, (str, bytes)):
            return [self._serialize_value(v) for v in value]
        return value

    async def check_connection_async(self) -> bool:
        try:
            await self.execute_query_async("g.V().count()")
            return True
        except Exception:
            return False

    def search_entities(
        self,
        keyword: str,
        vertex_label: str = "Entity",
        tenant_id: Optional[str] = None,
        limit: int = 20,
    ) -> List[Dict]:
        escaped = keyword.replace("'", "\\'")
        tenant_filter = f".has('tenant_id', '{tenant_id}')" if tenant_id else ""
        query = (
            f"g.V().hasLabel('{vertex_label}'){tenant_filter}"
            f".has('name', textContains('{escaped}'))"
            f".elementMap().limit({limit}).toList()"
        )
        return self.execute_query(query)

    def get_neighbors(
        self,
        vertex_id: str,
        edge_label: Optional[str] = None,
        limit: int = 50,
    ) -> Dict[str, List]:
        edge_step = f"bothE('{edge_label}')" if edge_label else "bothE()"
        query = (
            f"g.V({vertex_id}).{edge_step}.limit({limit})"
            f".project('edge','neighbor')"
            f".by(elementMap())"
            f".by(otherV().elementMap())"
            f".toList()"
        )
        return self.execute_query(query)

    def close(self):
        key = f"{self.gremlin_url}|{self.graph_alias}"
        with _pools_lock:
            pool = _pools.pop(key, None)
        if pool:
            pool.close_all()
