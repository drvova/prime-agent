"""Kernel client for Prime Agent's native web search and URL reading.

``await web.search(query)`` sends the ``web.search`` host request; the host's
native engine runs the provider (or the ``auto`` fallback chain: configured
keyed providers first, then keyless engines such as DuckDuckGo, Startpage,
Mojeek, and Ecosia) and replies with normalized results. API keys stay on the
host: they are configured with ``/mcp`` and never enter the kernel.

``await web.read(url_or_source)`` sends the ``web.read`` host request: the
host fetches the URL (arXiv, GitHub and Stack Exchange through their
structured sources) and replies with markdown, the outline of heading
anchors, and the links found. A search source reads directly.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from . import host_request

__all__ = [
    "WebHeading",
    "WebLink",
    "WebPage",
    "WebSearchFailure",
    "WebSearchResult",
    "WebSearchSource",
    "read",
    "search",
]

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


@dataclass(frozen=True)
class WebHeading:
    """One outline entry: ``section=anchor`` reads its span; ``url`` cites it."""

    level: int
    text: str
    anchor: str
    url: str | None = None


@dataclass(frozen=True)
class WebLink:
    """A link found on the page (absolute URL)."""

    text: str
    url: str


@dataclass(frozen=True)
class WebPage:
    """A read URL. ``str(page)`` is the readable rendering."""

    url: str
    final_url: str
    title: str | None
    content_type: str
    method: str
    markdown: str
    outline: list[WebHeading] = field(default_factory=list)
    links: list[WebLink] = field(default_factory=list)
    section: str | None = None
    truncated: bool = False
    total_chars: int = 0
    notes: list[str] = field(default_factory=list)
    text: str = ""

    def __str__(self) -> str:
        return self.text


def _page_from_payload(payload: Any) -> WebPage:
    if not isinstance(payload, dict):
        raise RuntimeError(f"web.read returned a non-object result: {payload!r}")
    return WebPage(
        url=str(payload.get("url") or ""),
        final_url=str(payload.get("finalUrl") or payload.get("url") or ""),
        title=payload.get("title"),
        content_type=str(payload.get("contentType") or ""),
        method=str(payload.get("method") or ""),
        markdown=str(payload.get("markdown") or ""),
        outline=[
            WebHeading(
                level=int(item.get("level") or 0),
                text=str(item.get("text") or ""),
                anchor=str(item.get("anchor") or ""),
                url=item.get("url"),
            )
            for item in payload.get("outline") or []
            if isinstance(item, dict)
        ],
        links=[
            WebLink(text=str(item.get("text") or ""), url=str(item.get("url") or ""))
            for item in payload.get("links") or []
            if isinstance(item, dict)
        ],
        section=payload.get("section"),
        truncated=bool(payload.get("truncated")),
        total_chars=int(payload.get("totalChars") or 0),
        notes=[str(note) for note in payload.get("notes") or []],
        text=str(payload.get("text") or ""),
    )


async def read(
    source: str | WebSearchSource | WebLink | WebHeading | dict[str, Any],
    *,
    max_chars: int | None = None,
    section: str | None = None,
    timeout: float | None = None,
) -> WebPage:
    """Read a URL as markdown with its outline and links.

    Args:
        source: A URL, or anything carrying one: a ``web.search`` source, a
            ``page.links`` entry, an outline heading, or a citation dict.
        max_chars: Markdown characters returned (default 20000); a longer
            page is cut at a line and says so in ``page.notes``.
        section: An outline anchor (``"installation"``, ``"page=3"``) or a
            heading's text: only that heading's span returns. For a code view
            (a GitHub file, plain text, JSON) a line range such as
            ``"L10-L40"``. The URL's own ``#fragment`` applies when omitted.
        timeout: Whole-read deadline in seconds (default 30).

    Raises:
        RuntimeError: the fetch failed, the content cannot be rendered, or
            ``section`` names no heading (the message lists the anchors).
    """
    url = source if isinstance(source, str) else None
    if url is None and isinstance(source, dict):
        url = source.get("url")
    if url is None:
        url = getattr(source, "url", None)
    if not isinstance(url, str) or not url.strip():
        raise TypeError("source must be a URL string or carry a non-empty url")
    payload: dict[str, Any] = {"url": url.strip()}
    if max_chars is not None:
        if isinstance(max_chars, bool) or not isinstance(max_chars, int) or max_chars < 1:
            raise ValueError("max_chars must be a positive integer")
        payload["maxChars"] = max_chars
    if section is not None:
        if not isinstance(section, str) or not section.strip():
            raise TypeError("section must be a non-empty string or None")
        payload["section"] = section
    if timeout is not None:
        if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or timeout <= 0:
            raise ValueError("timeout must be a positive number of seconds")
        payload["timeoutSeconds"] = timeout
    return _page_from_payload(await host_request("web.read", payload))
