"""Websearch skill implementation: a thin wrapper over the native engine.

The search runs on the host (``web.search``, the same engine as the kernel's
``web.search(...)``); this module keeps the skill's historical call signature
and output shape.
"""

from __future__ import annotations

import os

from rlm import host_request


def _env_int(name: str, default: int) -> int:
    """Read an int from the environment, falling back to default on bad values."""
    try:
        return int(os.environ[name])
    except (KeyError, ValueError):
        return default


async def run(
    query: str,
    *,
    max_output: int = 8192,
    timeout: int | None = None,
    num_results: int | None = None,
) -> str:
    """Run a web search with the provider chosen in /settings and return formatted results.

    Args:
        query: Web search query.
        max_output: Truncate output to this many chars.
        timeout: HTTP timeout in seconds.
        num_results: Organic results to return.

    Returns:
        Formatted search results.
    """
    if timeout is None:
        timeout = _env_int("PRIME_AGENT_WEBSEARCH_TIMEOUT", 45)
    if num_results is None:
        num_results = _env_int("PRIME_AGENT_WEBSEARCH_NUM_RESULTS", 5)

    payload = {"query": query, "numResults": max(1, num_results), "timeoutSeconds": max(1, timeout)}
    try:
        result = (await host_request("web.search", payload)).get("text") or ""
    except RuntimeError as e:
        message = str(e)
        # The host's setup guidance (a selected provider without its key) is
        # the whole answer, as before; other failures read as the result.
        if message.startswith("Web search is not set up"):
            return message
        result = f"Error searching for '{query}': {message}"
    output = f'Results for query "{query}":\n\n{result}'

    if len(output) > max_output:
        total = len(output)
        marker = f"\n... [output truncated, {total} chars total] ...\n"
        # Reserve room for the marker so the result stays within max_output.
        half = max(0, (max_output - len(marker)) // 2)
        output = output[:half] + marker + output[len(output) - half:]
        if len(output) > max_output:  # marker alone exceeds the budget
            output = output[:max_output]

    return output
