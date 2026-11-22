---
name: websearch
description: Search the web with the provider chosen in /settings (auto by default; keyed providers such as Serper, You.com Eco, Brave, Tavily, or Exa when their API key is set via /mcp). Takes one query and returns titles, URLs, snippets, and any answer the provider synthesizes.
---

# Web Search

Search the web with the provider chosen in `/settings` → **Agents** → **Web search provider**.
The default, `auto`, uses any keyed provider whose API key is configured and otherwise
searches keyless engines (DuckDuckGo, Startpage, Mojeek, Ecosia), so it works with no key.
The search runs in Prime Agent's native engine, through `(web-search ...)` in the Scheme kernel.

## Setup

None is needed for `auto`. For a keyed provider (better ranking, synthesized answers), get
an API key from it (for example Serper: https://serper.dev, You.com Eco:
https://you.com/platform), then run `/mcp` in Prime Agent and choose its
**(web search)** row to paste it. The key stays in Prime Agent.

If web search reports a missing key for a selected provider, walk the user through those
two steps; don't ask them to set environment variables.

## Usage

```scheme
(define found (web-search "latest Prime Agent release"))
(displayln (hash-ref found 'text))

;; Options are keywords: results, recency, one provider, seconds per request.
(web-search "tokio select cancel safety" #:num-results 8 #:recency "year")

;; Read a result as markdown.
(displayln (hash-ref (web-read (car (hash-ref found 'sources))) 'text))
```

`text` is the readable numbered list; `sources` holds `{title, url, snippet,
publishedDate, author}` hashes; `answer` is set by answer-synthesizing providers.
`(api-help "web")` prints the full reference of `web-search` and `web-read`.
