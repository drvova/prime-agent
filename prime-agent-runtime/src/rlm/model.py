"""Kernel client for the session's model facts.

``await model.info()`` reports the model this session runs on: its id and
provider, the input it accepts, its context window, and the thinking level the
next call runs at. ``await model.set_effort(level)`` changes this session's
thinking level, only where the user allowed it (``replCanChangeEffort``).
"""

from __future__ import annotations

from dataclasses import dataclass

from . import host_request

__all__ = ["THINKING_LEVELS", "ModelInfo", "info", "set_effort"]

THINKING_LEVELS = ("off", "minimal", "low", "medium", "high", "xhigh", "max")


@dataclass(frozen=True)
class ModelInfo:
    """The session model. ``selector`` (``provider/id``) is what ``rlm.spawn(model=...)`` takes."""

    id: str
    provider: str
    input: list[str]
    context_window: int | None
    thinking_level: str | None

    @property
    def selector(self) -> str:
        return f"{self.provider}/{self.id}"

    @property
    def image_input(self) -> bool:
        return "image" in self.input

    def __str__(self) -> str:
        return (
            f"{self.selector}: input {', '.join(self.input) or 'text'}, "
            f"context window {self.context_window}, thinking {self.thinking_level}"
        )


async def info() -> ModelInfo:
    """The session model's facts (see the module docs)."""
    reply = await host_request("model.info", {})
    if not isinstance(reply, dict):
        raise RuntimeError(f"model.info returned an unexpected result: {reply!r}")
    return ModelInfo(
        id=str(reply.get("id", "")),
        provider=str(reply.get("provider", "")),
        input=[str(item) for item in reply.get("input", [])],
        context_window=reply.get("contextWindow"),
        thinking_level=reply.get("thinkingLevel"),
    )


async def set_effort(level: str) -> str:
    """Change this session's thinking level from the next model call; the effective level.

    It changes what every later call costs, so it works only when the user set
    ``"replCanChangeEffort": true`` in the global settings; otherwise it raises
    ``RuntimeError`` saying how to enable it. The level may be clamped to what
    the model supports. Children pick their own level with
    ``rlm.spawn(thinking=...)``.
    """
    if level not in THINKING_LEVELS:
        raise ValueError(f"level must be one of {', '.join(THINKING_LEVELS)}, got {level!r}")
    reply = await host_request("model.set_effort", {"level": level})
    if not isinstance(reply, dict) or not isinstance(reply.get("thinkingLevel"), str):
        raise RuntimeError(f"model.set_effort returned an unexpected result: {reply!r}")
    return reply["thinkingLevel"]
