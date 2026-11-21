---
name: mcp
description: Use external MCP services generically from the Scheme kernel - search the supported-service catalog, inspect the user's connections, discover live tool schemas, and call tools on any connection (Notion, Linear, Slack, and the rest of the catalog) without per-service packages.
---

# MCP Services

Every external service reachable over MCP uses the same `mcp-*` functions of
the Scheme kernel. Adding a service is data (a catalog entry plus a user
login), never a new package, and there are no per-service skills to read.
`(api-help "mcp")` prints the reference of every function.

## Inventory: what can I use?

```scheme
;; Supported catalog, one bounded page. More pages exist while nextCursor is a string.
(define page (mcp-list-plugins #:connection-status "not_connected" #:limit 50))
(define results (mcp-search-plugins "notion"))     ; bounded search
(define first-page (mcp-list-plugins))             ; default: first 50 entries

;; The user's actual connections. connectionId is the server name everywhere below.
(define connections (mcp-list-connections))
```

Catalog entries carry `connectionStatus` (`connected`, `pending`,
`not_connected`, `setup_required`, `disabled`, `error`), `connectable`, and
when connected the `connectionIds` you dispatch on. `connected` means a
verified MCP handshake, not just stored credentials; `pending` means
credentials exist but verification hasn't run or succeeded yet. Entries may
also carry `setupHint` (why a service isn't connectable yet), `unverified`
(imported but not vetted), `verifiedAt`/`toolCount`, and `connectionIds`
(empty when not dispatchable). Entries with `setup_required` need a developer
app or API key first — say so honestly instead of promising one-click.

## Recommend, never connect

If a task would benefit from an unconnected service, name the real catalog
entry (e.g. "Connecting Notion would let me search your workspace directly")
and tell the user to open `/plugins` (or run `/mcp login <service>`) to connect.
Recommendations and catalog lookups never install anything, open a browser,
use credentials, or request extra scopes. When a server needs the user to sign
in, `(mcp-login server)` starts their interactive login and waits; if the user
connects mid-task, the connection works on the next call without a reload.

## Discover, then call

```scheme
;; Schemas come from the server.
(define found (mcp-search-tools "search workspace documents" #:connection-id "notion"))
(define tool (mcp-describe-tool "notion" "notion-search"))   ; {name, description, inputSchema}
(define tools (mcp-list-tools "notion"))                     ; full tool list with schemas

(define result (mcp-call-tool "notion" "notion-search" (hash 'query "roadmap")))
```

- Discover before calling: don't assume tool names or arguments; read the
  `inputSchema` with `mcp-describe-tool` and construct matching arguments.
- `mcp-list-tools` returns the complete inventory or raises — never a silent
  partial list.
- `mcp-search-tools` without `#:connection-id` searches at most 8 connections
  the host reports as connected and reports its scope: `searched`,
  `unavailable` (per-connection failures as fixed, redaction-safe summaries),
  and `truncated` (narrow the query or pick a connection when it is set — the
  result never claims to be exhaustive).
- Prefer narrow catalog searches over paging the whole catalog; don't dump all
  schemas into context.
- Results are already-parsed data (a hash for structured output, otherwise a
  string). Keys are the server's own, as symbols (`'inputSchema`).

## Large or integrity-critical payloads

`mcp-call-tool` runs the MCP client inside the agent. Arguments go from the
kernel straight to the server and never pass through model output; results
enter context only if you print them. For bulk writes or data that must stay
exact (financial figures, generated rows), keep the payload in code:

```scheme
;; /tmp/updates.json was written by code, never typed by the model.
;; open-output-file creates the result file and raises when it already exists.
(define args (string->jsexpr (hash-ref (bash "cat /tmp/updates.json") 'output)))
(define result (mcp-call-tool "sheets" "update_spreadsheet" args))
(let ([port (open-output-file "/tmp/updates.result.json")])
  (write-string (value->jsexpr-string result) port)
  (close-output-port port))
(displayln (length (hash-ref args 'requests)))
```

- Build or load arguments with code (a file a script wrote, a loop). Never
  paste a payload into a cell or ask the model to retype one.
- Write results to a file and check them (counts, hashes, a readback compared
  field by field). Print the summary, not the payload.
- Each call times out after 60 s by default. For large writes, raise
  `callTimeoutMs` in that server's `mcpServers` entry.

## Errors and permissions

Every failure raises an error that starts with the function's name; catch one
with `(with-handler (lambda (e) (to-string e)) ...)` to read it.

- The tool is excluded by the user's `enabledTools` / `disabledTools` policy:
  don't retry; tell the user.
- The tool or connection doesn't exist: re-check with `mcp-list-tools` /
  `mcp-list-connections`; tools can change across sessions.
- The server's tool listing couldn't complete honestly (broken pagination): no
  partial inventory is published; re-run, and ask the user to reconnect if it
  persists.
- Credentials are not available: ask the user to connect via `/plugins` or
  `/mcp login <service>`, never via environment variables.
- The server flagged the call as an error: surface the message; don't blindly
  retry a write that may have already happened.

Credentials never appear in inventory results, tool schemas, or errors.
