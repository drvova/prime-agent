---
name: websearch
description: Search the web with the provider chosen in /settings (auto by default; keyed providers such as Serper, You.com Eco, Brave, Tavily, or Exa when their API key is set via /mcp). Takes one query and returns titles, URLs, snippets, and any answer the provider synthesizes.
---

# Web Search

Search the web with the provider chosen in `/settings` → **Agents** → **Web search provider**.
The default, `auto`, uses any keyed provider whose API key is configured and otherwise
searches keyless engines (DuckDuckGo, Startpage, Mojeek, Ecosia), so it works with no key.
The search runs in Prime Agent's native engine, the same one as `web.search(...)` in the
Python kernel.

## Setup

None is needed for `auto`. For a keyed provider (better ranking, synthesized answers), get
an API key from it (for example Serper: https://serper.dev, You.com Eco:
https://you.com/platform), then run `/mcp` in Prime Agent and choose its
**(web search)** row to paste it. The key stays in Prime Agent.

If web search reports a missing key for a selected provider, walk the user through those
two steps; don't ask them to set environment variables.

Optional overrides (environment variables):

- `PRIME_AGENT_WEBSEARCH_TIMEOUT` - HTTP timeout in seconds (default 45).
- `PRIME_AGENT_WEBSEARCH_NUM_RESULTS` - number of organic results to return (default 5; You.com Eco returns at most 10).

## Usage

Call the prepared `websearch` import directly in the Python kernel:

```python
print(await websearch("latest Prime Agent release"))
```
