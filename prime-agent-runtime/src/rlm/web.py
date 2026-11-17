"""Kernel client for Prime Agent's native web search.

``await web.search(query)`` sends the ``web.search`` host request; the host's
native engine runs the provider (or the ``auto`` fallback chain: configured
keyed providers first, then keyless engines such as DuckDuckGo, Startpage,
Mojeek, and Ecosia) and replies with normalized results. API keys stay on the
host: they are configured with ``/mcp`` and never enter the kernel.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from . import host_request

__all__ = ["WebSearchFailure", "WebSearchResult", "WebSearchSource", "search"]

_RECENCIES = ("day", "week", "month", "year")


@dataclass(frozen=True)
class WebSearchSource:
    """One result: ``title``, ``url``, and the optional snippet/date/author."""

    title: str
    url: str
    snippet: str | None = None
    published_date: str | None = None
    author: str | None = None


@dataclass(frozen=True)
class WebSearchFailure:
    """A provider that failed before another answered (fallback order)."""

    provider: str
    label: str
    reason: str
    status: int | None = None


@dataclass(frozen=True)
class WebSearchResult:
    """A finished search. ``str(result)`` is the readable rendering."""

    provider: str
    label: str
    answer: str | None
    sources: list[WebSearchSource]
    citations: list[dict[str, Any]] = field(default_factory=list)
    related_questions: list[str] = field(default_factory=list)
    failures: list[WebSearchFailure] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)
    text: str = ""

    def __str__(self) -> str:
        return self.text


def _result_from_payload(payload: Any) -> WebSearchResult:
    if not isinstance(payload, dict):
        raise RuntimeError(f"web.search returned a non-object result: {payload!r}")
    return WebSearchResult(
        provider=str(payload.get("provider") or ""),
        label=str(payload.get("label") or ""),
        answer=payload.get("answer"),
        sources=[
            WebSearchSource(
                title=str(item.get("title") or item.get("url") or ""),
                url=str(item.get("url") or ""),
                snippet=item.get("snippet"),
                published_date=item.get("publishedDate"),
                author=item.get("author"),
            )
            for item in payload.get("sources") or []
            if isinstance(item, dict)
        ],
        citations=[item for item in payload.get("citations") or [] if isinstance(item, dict)],
        related_questions=[str(q) for q in payload.get("relatedQuestions") or []],
        failures=[
            WebSearchFailure(
                provider=str(item.get("provider") or ""),
                label=str(item.get("label") or ""),
                reason=str(item.get("reason") or ""),
                status=item.get("status"),
            )
            for item in payload.get("failures") or []
            if isinstance(item, dict)
        ],
        notes=[str(note) for note in payload.get("notes") or []],
        text=str(payload.get("text") or ""),
    )


async def search(
    query: str,
    *,
    provider: str | None = None,
    num_results: int | None = None,
    recency: str | None = None,
    timeout: float | None = None,
) -> WebSearchResult:
    """Search the web with the native engine.

    Args:
        query: The search query. Google-style directives (``site:``,
            ``-site:``, ``before:``/``after:``, ``intitle:``, ``filetype:``,
            quoted phrases, ``OR``) are honored natively where the provider
            can, and post-filtered otherwise.
        provider: ``"auto"`` (the fallback chain) or one engine id (for example
            ``"duckduckgo"``, ``"brave"``, ``"hackernews"``); ``None`` uses the
            ``/settings`` choice.
        num_results: Results wanted (clamped per provider).
        recency: ``"day"``, ``"week"``, ``"month"``, or ``"year"``.
        timeout: Per-request timeout in seconds.

    Raises:
        RuntimeError: every candidate provider failed, or the selected
            provider is not set up (the message says how to set it up).
    """
    if not isinstance(query, str) or not query.strip():
        raise TypeError("query must be a non-empty string")
    payload: dict[str, Any] = {"query": query}
    if provider is not None:
        if not isinstance(provider, str) or not provider:
            raise TypeError("provider must be a non-empty string or None")
        payload["provider"] = provider
    if num_results is not None:
        if isinstance(num_results, bool) or not isinstance(num_results, int) or num_results < 1:
            raise ValueError("num_results must be a positive integer")
        payload["numResults"] = num_results
    if recency is not None:
        if recency not in _RECENCIES:
            raise ValueError(f"recency must be one of {', '.join(_RECENCIES)}")
        payload["recency"] = recency
    if timeout is not None:
        if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or timeout <= 0:
            raise ValueError("timeout must be a positive number of seconds")
        payload["timeoutSeconds"] = timeout
    return _result_from_payload(await host_request("web.search", payload))
