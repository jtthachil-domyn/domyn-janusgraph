"""PDF extraction using PyMuPDF (fitz)."""

from pathlib import Path
from typing import List, Dict, Any
import logging

logger = logging.getLogger("domyngraph-rag.processing.pdf")


class PDFProcessor:
    """Extract text from PDF documents page by page."""

    def extract(self, file_path: str) -> List[Dict[str, Any]]:
        """
        Extract pages from a PDF file.

        Returns list of dicts with keys: page_id, text, source_file, metadata
        """
        import fitz

        path = Path(file_path)
        if not path.exists():
            raise FileNotFoundError(f"PDF not found: {file_path}")

        doc = fitz.open(str(path))
        pages = []

        for page_num in range(len(doc)):
            page = doc.load_page(page_num)
            text = page.get_text("text").strip()
            if not text:
                continue

            pages.append(
                {
                    "page_id": page_num + 1,
                    "text": text,
                    "source_file": path.name,
                    "metadata": {
                        "page_count": len(doc),
                        "file_path": str(path),
                    },
                }
            )

        doc.close()
        logger.info("Extracted %d pages from %s", len(pages), path.name)
        return pages
