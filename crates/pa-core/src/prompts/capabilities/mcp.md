# mcp: external MCP services

One generic client for every MCP server; tool schemas are discovered at runtime. `server` is a connection id from `mcp-list-connections`. Reply keys are the server's own (`'inputSchema`); a JSON null is `void`. A failure raises an error that starts with the function's name: an unknown tool or connection, a tool the user's `enabledTools`/`disabledTools` policy excludes (do not retry; tell the user), missing credentials (ask the user to connect through `/plugins` or `/mcp login <service>`, never through environment variables), or a result the server flags as an error (do not blindly retry a write). Credentials never appear in results or errors. A call blocks its cell and ends with the server's answer, its call timeout (60 s by default; `callTimeoutMs` in the server's `mcpServers` entry), or the cell.

- `(mcp-list-connections)`: the user's current connections; each row's `connectionId` is the `server` the other functions take
- `(mcp-list-tools server)`: the server's tools, each `{name, description, inputSchema}`; complete or an error, never a partial list
- `(mcp-describe-tool server tool)`: one tool's `{name, description, inputSchema}`. Read the schema before calling: do not assume tool names or arguments
- `(mcp-call-tool server tool [arguments])`: call `tool` with `arguments` (a hash; none by default): its structured content when it has any, else its text, else its content blocks. Arguments go straight to the server and the result enters context only when printed, so build large or exact payloads with code and print a summary
- `(mcp-search-tools query #:connection-id #:limit)`: search live tool names and descriptions: `{tools, searched, unavailable, truncated}`, each tool `{connectionId, name, description}`. With `#:connection-id` only that connection is searched and its errors raise; without it, up to 8 connected connections are searched and a failing one is listed under `unavailable`. `#:limit` defaults to 20; when `truncated`, narrow the query or pick a connection
- `(mcp-list-plugins #:connection-status #:limit #:cursor)`: one page of the supported-service catalog: `{plugins, nextCursor}`. More entries exist while `nextCursor` is a string; pass it back as `#:cursor`. `#:connection-status` filters to `"connected"` or `"not_connected"`; `#:limit` defaults to 50. Entries carry `connectionStatus` (`connected`, `pending`, `not_connected`, `setup_required`, `disabled`, `error`), `connectable`, `connectionIds`, and sometimes `setupHint`
- `(mcp-search-plugins query #:limit)`: catalog entries matching `query` (default 10): `{plugins, nextCursor}`
- `(mcp-status servers #:timeout-ms)`: open and list each of `servers` (a list of names), each bounded by `#:timeout-ms` (default 10000): a list of `{server, tools, error}`; a server that failed has no tools and says why
- `(mcp-login server)`: start the user's interactive login (completed in their browser) and wait: `{server, status, message}`; `status` is `"logged_in"` (usable now), `"unavailable"` (this run cannot show a login: ask the user to run `/mcp login <server>`), or `"failed"`
- `(mcp-reload [server])`: close the connection to `server`, or every connection when none is named; the next call opens it again with the current configuration
- `(mcp-list-resources server)`: the server's resources, as it describes them
- `(mcp-list-resource-templates server)`: the server's resource templates
- `(mcp-read-resource server uri)`: one resource's `{contents}`
- `(mcp-list-prompts server)`: the server's prompts
- `(mcp-get-prompt server name [arguments])`: one prompt rendered with `arguments` (a hash of strings): `{description, messages}`

Recommend, never connect: when a task would benefit from an unconnected service, name the real catalog entry and tell the user to open `/plugins` or run `/mcp login <service>`. A connection the user makes mid-task works on the next call.
