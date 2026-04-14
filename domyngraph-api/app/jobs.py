"""Async job queue for long-running algorithm execution.

POST /algorithms/run -> returns job_id immediately.
GET /algorithms/{job_id} -> poll for status + result.
"""

from __future__ import annotations

import asyncio
import logging
import time
import uuid
from dataclasses import dataclass, field
from enum import Enum
from typing import Any

logger = logging.getLogger("domyngraph.jobs")


class JobStatus(str, Enum):
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"
    TIMEOUT = "timeout"


@dataclass
class AlgorithmJob:
    job_id: str
    algorithm: str
    status: JobStatus = JobStatus.RUNNING
    progress: str = ""
    result: Any = None
    error: str | None = None
    started_at: float = field(default_factory=time.monotonic)
    elapsed_ms: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "job_id": self.job_id,
            "algorithm": self.algorithm,
            "status": self.status.value,
            "progress": self.progress,
            "result": self.result,
            "error": self.error,
            "elapsed_ms": self.elapsed_ms,
        }


_jobs: dict[str, AlgorithmJob] = {}


def get_job(job_id: str) -> AlgorithmJob | None:
    return _jobs.get(job_id)


def list_jobs(limit: int = 20) -> list[dict[str, Any]]:
    jobs = sorted(_jobs.values(), key=lambda j: j.started_at, reverse=True)
    return [j.to_dict() for j in jobs[:limit]]


async def run_algorithm_job(
    algorithm: str,
    gremlin_query: str,
    submit_fn,
    transform_fn,
) -> str:
    """Launch an algorithm job in the background. Returns job_id immediately."""
    job_id = str(uuid.uuid4())
    job = AlgorithmJob(job_id=job_id, algorithm=algorithm)
    _jobs[job_id] = job

    logger.info("Algorithm job %s started: %s", job_id, algorithm)

    async def _execute():
        try:
            job.progress = "executing"
            results = await submit_fn(gremlin_query, timeout_s=120)
            job.progress = "transforming"
            response = transform_fn(results)
            job.status = JobStatus.COMPLETED
            job.result = response
            job.elapsed_ms = int((time.monotonic() - job.started_at) * 1000)
            logger.info("Algorithm job %s completed in %dms", job_id, job.elapsed_ms)
        except Exception as exc:
            job.elapsed_ms = int((time.monotonic() - job.started_at) * 1000)
            error_str = str(exc)
            if "timeout" in error_str.lower() or "timed out" in error_str.lower():
                job.status = JobStatus.TIMEOUT
                job.error = f"Algorithm timed out after {job.elapsed_ms}ms"
            else:
                job.status = JobStatus.FAILED
                job.error = error_str[:500]
            logger.error("Algorithm job %s failed: %s", job_id, job.error)

    asyncio.create_task(_execute())
    return job_id
