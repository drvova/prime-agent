"""Kernel client for the session's effective settings (read-only).

``await settings.get()`` reports the limits this session runs with: the model,
its context window and current thinking level, the ``python_repl`` cell time
limit, auto-compaction, the RLM depth and its bound, the image model, and
autonomous mode with its budget. No credential is ever part of the reply.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from . import host_request

__all__ = ["Autonomous", "Compaction", "Settings", "get"]


@dataclass(frozen=True)
class Compaction:
    """Auto-compaction: whether it runs, the context share that triggers it, the tokens kept."""

    enabled: bool
    threshold_percent: float
    keep_recent_tokens: int
    agent_callable: bool


@dataclass(frozen=True)
class Autonomous:
    """Autonomous mode: ``enabled`` with what it has used against its ``limits``.

    ``limits`` maps ``max_continuations``, ``max_turns``, ``max_tokens`` and
    ``timeout_ms`` to their values (``0`` means unlimited).
    """

    enabled: bool
    continuations_used: int
    turns_used: int
    tokens_used: int
    limits: dict[str, int]


@dataclass(frozen=True)
class Settings:
    """``settings.get()``'s reply. ``autonomous`` is ``None`` when no autonomous runner serves the session."""

    model: str
    context_window: int | None
    thinking_level: str
    cell_timeout_ms: int | None
    compaction: Compaction
    rlm_depth: int
    rlm_max_depth: int
    rlm_max_depth_source: str
    image_model: str | None
    web_search_provider: str
    cache_retention: str
    daemonless: bool
    autonomous: Autonomous | None

    def __str__(self) -> str:
        timeout = "none" if self.cell_timeout_ms is None else f"{self.cell_timeout_ms} ms"
        compaction = (
            f"on at {self.compaction.threshold_percent:g}% (keeps {self.compaction.keep_recent_tokens} tokens)"
            if self.compaction.enabled
            else "off"
        )
        lines = [
            f"model: {self.model} (context window {self.context_window}, thinking {self.thinking_level})",
            f"cell timeout: {timeout}",
            f"auto-compaction: {compaction}",
            f"rlm depth: {self.rlm_depth} of max {self.rlm_max_depth} ({self.rlm_max_depth_source})",
            f"image model: {self.image_model or 'none'}",
            f"web search: {self.web_search_provider}",
        ]
        if self.autonomous is None:
            lines.append("autonomous: unavailable")
        elif self.autonomous.enabled:
            lines.append(
                f"autonomous: on ({self.autonomous.turns_used} turns, {self.autonomous.tokens_used} tokens, "
                f"{self.autonomous.continuations_used} continuations used; limits {self.autonomous.limits})"
            )
        else:
            lines.append("autonomous: off")
        return "\n".join(lines)


def _autonomous(payload: Any) -> Autonomous | None:
    if not isinstance(payload, dict):
        return None
    limits = payload.get("limits") or {}
    return Autonomous(
        enabled=bool(payload.get("enabled")),
        continuations_used=int(payload.get("continuationsUsed", 0)),
        turns_used=int(payload.get("turnsUsed", 0)),
        tokens_used=int(payload.get("tokensUsed", 0)),
        limits={
            "max_continuations": int(limits.get("maxContinuations", 0)),
            "max_turns": int(limits.get("maxTurns", 0)),
            "max_tokens": int(limits.get("maxTokens", 0)),
            "timeout_ms": int(limits.get("timeoutMs", 0)),
        },
    )


async def get() -> Settings:
    """The session's effective settings (see the module docs)."""
    session = await host_request("settings.session", {})
    host = await host_request("settings.host", {})
    if not isinstance(session, dict) or not isinstance(host, dict):
        raise RuntimeError(f"settings request returned an unexpected result: {session!r}, {host!r}")
    model = session.get("model") or {}
    compaction = session.get("compaction") or {}
    depth = host.get("rlmMaxDepth") or {}
    return Settings(
        model=f"{model.get('provider', '')}/{model.get('id', '')}",
        context_window=model.get("contextWindow"),
        thinking_level=str(model.get("thinkingLevel", "off")),
        cell_timeout_ms=session.get("cellTimeoutMs"),
        compaction=Compaction(
            enabled=bool(compaction.get("enabled")),
            threshold_percent=float(compaction.get("thresholdPercent", 0)),
            keep_recent_tokens=int(compaction.get("keepRecentTokens", 0)),
            agent_callable=bool(compaction.get("agentCallable")),
        ),
        rlm_depth=int(session.get("rlmDepth", 0)),
        rlm_max_depth=int(depth.get("maxDepth", 0)),
        rlm_max_depth_source=str(depth.get("source", "default")),
        image_model=session.get("imageModel"),
        web_search_provider=str(session.get("webSearchProvider", "auto")),
        cache_retention=str(session.get("cacheRetention", "short")),
        daemonless=bool(session.get("daemonless")),
        autonomous=_autonomous(host.get("autonomous")),
    )
