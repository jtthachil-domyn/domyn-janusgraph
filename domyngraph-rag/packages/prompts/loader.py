"""TOML prompt template loader."""

from pathlib import Path
from typing import Dict, Any, Optional
import toml
import logging

logger = logging.getLogger("domyngraph-rag.prompts")

TEMPLATES_DIR = Path(__file__).parent / "templates"


def load_prompt(name: str, directory: Optional[str] = None) -> Dict[str, Any]:
    """Load a TOML prompt template by name (without .toml extension)."""
    base = Path(directory) if directory else TEMPLATES_DIR
    path = base / f"{name}.toml"
    if not path.exists():
        raise FileNotFoundError(f"Prompt template not found: {path}")
    return toml.load(path)


def format_prompt(template: str, **kwargs) -> str:
    """Format a prompt template with keyword arguments."""
    result = template
    for key, value in kwargs.items():
        result = result.replace(f"{{{key}}}", str(value))
    return result
