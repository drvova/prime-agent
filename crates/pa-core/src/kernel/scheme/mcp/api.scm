;; MCP servers: the `mcp-*` functions over the session's MCP client. Each
;; sends one operation to the client and returns its reply; a failure raises
;; an error that starts with the function's name. Names starting with % are
;; internal.

(define (%mcp who operation payload)
  (let ([reply (string->jsexpr (%mcp-call operation (value->jsexpr-string payload)))])
    (if (hash-contains? reply 'error)
        (%mcp-raise (string-append who ": " (hash-ref reply 'error)))
        (%exact-integers (hash-ref reply 'value)))))

;; The server's tools: a list of {name, description, inputSchema}. Complete
;; or an error, never a partial list.
(define (mcp-list-tools server)
  (%mcp "mcp-list-tools" "list_tools" (hash 'server server)))

;; One tool's {name, description, inputSchema}.
(define (mcp-describe-tool server tool)
  (%mcp "mcp-describe-tool" "describe_tool" (hash 'server server 'tool tool)))

;; Call `tool` with `arguments` (a hash; none by default) and return its
;; structured content when it has any, else its text, else its content
;; blocks. A result the server flags as an error raises.
(define (mcp-call-tool server tool [arguments (hash)])
  (%mcp "mcp-call-tool" "call_tool" (hash 'server server 'tool tool 'arguments arguments)))

;; Search live tool names and descriptions: {tools, searched, unavailable,
;; truncated}. With #:connection-id only that connection is searched and its
;; errors raise; without it, up to 8 connections the host reports as
;; connected are searched and a failing one is listed under `unavailable`.
(define (mcp-search-tools query #:connection-id [connection-id #f] #:limit [limit 20])
  (%mcp "mcp-search-tools"
        "search_tools"
        (hash 'query query 'connection_id connection-id 'limit limit)))

;; The user's current connections; `connectionId` is the server name the
;; other functions take.
(define (mcp-list-connections)
  (%mcp "mcp-list-connections" "list_connections" (hash)))

;; One page of the supported-service catalog: {plugins, nextCursor}. More
;; entries exist while nextCursor is a string; pass it back as #:cursor.
;; #:connection-status filters to "connected" or "not_connected".
(define (mcp-list-plugins #:connection-status [connection-status #f]
                          #:limit [limit 50]
                          #:cursor [cursor #f])
  (%mcp "mcp-list-plugins"
        "list_plugins"
        (hash 'connection_status connection-status 'limit limit 'cursor cursor)))

;; Catalog entries matching `query`: {plugins, nextCursor}.
(define (mcp-search-plugins query #:limit [limit 10])
  (%mcp "mcp-search-plugins" "search_plugins" (hash 'query query 'limit limit)))

;; Open and list each of `servers` (a list of names), each bounded by
;; #:timeout-ms: a list of {server, tools, error}, where a server that failed
;; has no tools and says why.
(define (mcp-status servers #:timeout-ms [timeout-ms 10000])
  (%mcp "mcp-status" "status" (hash 'servers servers 'timeout_ms timeout-ms)))

;; Start the user's interactive login to `server` and wait for it:
;; {server, status, message}, status "logged_in" (usable now), "unavailable"
;; (this run cannot show a login: ask the user to run /mcp login <server>),
;; or "failed". No credential is returned.
(define (mcp-login server)
  (%mcp "mcp-login" "login" (hash 'server server)))

;; Close the connection to `server`, or every connection when none is named;
;; the next call opens it again with the current configuration.
(define (mcp-reload [server #f])
  (%mcp "mcp-reload" "reload" (hash 'server server)))

;; The server's resources, as it describes them.
(define (mcp-list-resources server)
  (%mcp "mcp-list-resources" "list_resources" (hash 'server server)))

;; The server's resource templates, as it describes them.
(define (mcp-list-resource-templates server)
  (%mcp "mcp-list-resource-templates" "list_resource_templates" (hash 'server server)))

;; One resource's {contents}.
(define (mcp-read-resource server uri)
  (%mcp "mcp-read-resource" "read_resource" (hash 'server server 'uri uri)))

;; The server's prompts, as it describes them.
(define (mcp-list-prompts server)
  (%mcp "mcp-list-prompts" "list_prompts" (hash 'server server)))

;; One prompt rendered with `arguments` (a hash of strings; none by
;; default): {description, messages}.
(define (mcp-get-prompt server name [arguments (hash)])
  (%mcp "mcp-get-prompt" "get_prompt" (hash 'server server 'name name 'arguments arguments)))
