"""Kernel client for the session's model facts.

``await model.info()`` reports the model this session runs on: its id and
provider, the input it accepts, its context window, and the thinking level the
next call runs at.
"""

from __future__ import annotations

from dataclasses import dataclass

from . import host_request

__all__ = ["ModelInfo", "info"]


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
