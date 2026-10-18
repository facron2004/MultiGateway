"""Shared safety limits for model completion requests."""

from __future__ import annotations

import os
from typing import Any, Optional


# Trae SOLO CN agent-remote models cap single responses at 64K tokens
# (report: solo_agent_remote max_tokens=64000). Keep the local clamp below
# that ceiling so a client asking for 131072 cannot push an upstream 4xx.
DEFAULT_MAX_COMPLETION_TOKENS = 64000

_AGENT_MODEL_MAX_TOKENS = 64000
_AGENT_MAX_MODELS = {
    "glm-5.1",
    "glm-5.2",
    "glm-5.3",
    "deepseek-v4-pro",
    "deepseek-v4-flash",
    "deepseek-v4-pro-official",
    "deepseek-v4-flash-official",
}

# Vision-capable config names taken from the live Trae catalog
# (``multimodal: true`` in /api/remote/v1/models).  A text-only model cannot
# read an attached image, so an image-bearing request is re-pointed at one of
# these instead of being answered with the image silently dropped.
MULTIMODAL_MODELS = {
    "doubao-seed-2.0-code",
    "doubao-seed-2.1-pro",
    "doubao-seed-2.1-turbo",
    "doubao-seed-code",
    "doubao-seed-evolving",
    "custom_openai_compatible//mimo-v2.5",
    "custom_openai_compatible//qwen36-35b",
    "glm-5v-turbo",
    "kimi-k2.5",
    "kimi-k2.6",
    "kimi-k2.7-code",
    "kimi-k3",
    "minimax-m3",
    "qwen-3.5",
    "qwen-3.6-plus",
    "qwen-3.7-plus",
    "qwen3.8-max",
}

# Preference order for the automatic multimodal re-point.  The first entry the
# account's catalog actually exposes wins.
MULTIMODAL_FALLBACK_ORDER = (
    "kimi-k2.6",
    "Doubao-Seed-2.1-Pro",
    "qwen3.8-max",
    "glm-5v-turbo",
    "minimax-m3",
    "kimi-k3",
    "Doubao-Seed-Code",
)


def _normalize_model_name(model: Optional[str]) -> str:
    normalized = str(model or "").strip().lower()
    if normalized.startswith("trae/"):
        normalized = normalized[5:]
    return normalized


def is_multimodal_model(model: Optional[str]) -> bool:
    return _normalize_model_name(model) in MULTIMODAL_MODELS


def configured_multimodal_model() -> str:
    return str(os.environ.get("TRAE_MULTIMODAL_MODEL", "")).strip()


def _configured_limit(model: Optional[str] = None) -> int:
    raw = os.environ.get("TRAE_MAX_COMPLETION_TOKENS", "")
    try:
        value = int(raw)
    except (TypeError, ValueError):
        value = DEFAULT_MAX_COMPLETION_TOKENS
    if value <= 0:
        value = DEFAULT_MAX_COMPLETION_TOKENS
    normalized = str(model or "").strip().lower()
    if normalized.startswith("trae/"):
        normalized = normalized[5:]
    if normalized in _AGENT_MAX_MODELS:
        return min(value, _AGENT_MODEL_MAX_TOKENS)
    return value


def clamp_max_completion_tokens(value: Any, model: Optional[str] = None) -> Any:
    """Clamp positive completion-token values to the upstream-safe limit."""

    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return value
    if value <= 0:
        return value
    return min(int(value), _configured_limit(model))
