"""Structured logger for DomynGraph RAG pipeline."""

import logging
import sys
from typing import Optional


class PipelineLogger:
    """Lightweight structured logger wrapping stdlib logging."""

    def __init__(self, name: str = "domyngraph-rag", level: int = logging.INFO):
        self.logger = logging.getLogger(name)
        if not self.logger.handlers:
            handler = logging.StreamHandler(sys.stdout)
            handler.setFormatter(
                logging.Formatter(
                    "%(asctime)s | %(name)s | %(levelname)s | %(message)s",
                    datefmt="%Y-%m-%d %H:%M:%S",
                )
            )
            self.logger.addHandler(handler)
            self.logger.setLevel(level)

    def info(self, msg: str, **kw):
        self.logger.info(msg, **kw)

    def warning(self, msg: str, **kw):
        self.logger.warning(msg, **kw)

    def error(self, msg: str, **kw):
        self.logger.error(msg, **kw)

    def debug(self, msg: str, **kw):
        self.logger.debug(msg, **kw)
