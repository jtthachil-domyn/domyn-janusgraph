"""Shared app state for RAG service."""

_app_state = {}


def set_app_state(state: dict):
    global _app_state
    _app_state = state


def get_app_state() -> dict:
    return _app_state
