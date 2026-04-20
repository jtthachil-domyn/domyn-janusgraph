"""Text chunking with configurable overlap."""

from typing import List, Dict, Any
import hashlib
import logging

logger = logging.getLogger("domyngraph-rag.processing.text")


class TextChunker:
    """Split text into overlapping chunks for indexing."""

    def __init__(self, chunk_size: int = 1000, chunk_overlap: int = 200):
        self.chunk_size = chunk_size
        self.chunk_overlap = chunk_overlap

    def chunk_pages(self, pages: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
        """
        Chunk a list of extracted pages.
        Each page dict must have: page_id, text, source_file
        """
        all_chunks = []
        for page in pages:
            chunks = self._split_text(page["text"])
            for idx, chunk_text in enumerate(chunks):
                chunk_id = self._generate_chunk_id(page["source_file"], page["page_id"], idx)
                all_chunks.append(
                    {
                        "chunk_id": chunk_id,
                        "chunk_text": chunk_text,
                        "page_id": page["page_id"],
                        "source_file": page["source_file"],
                        "chunk_index": idx,
                    }
                )

        logger.info("Created %d chunks from %d pages", len(all_chunks), len(pages))
        return all_chunks

    def _split_text(self, text: str) -> List[str]:
        chunks = []
        start = 0
        while start < len(text):
            end = start + self.chunk_size
            chunk = text[start:end]
            if chunk.strip():
                chunks.append(chunk.strip())
            start += self.chunk_size - self.chunk_overlap
        return chunks

    @staticmethod
    def _generate_chunk_id(source_file: str, page_id: int, chunk_index: int) -> str:
        raw = f"{source_file}_{page_id}_{chunk_index}"
        return hashlib.sha256(raw.encode()).hexdigest()[:16]
