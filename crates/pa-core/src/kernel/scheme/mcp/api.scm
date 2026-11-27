;; MCP servers: the `mcp-*` functions over the session's MCP client. Each
;; sends one operation to the client and returns its reply; a failure raises
;; an error that starts with the function's name. Names starting with % are
;; internal.

(define (%mcp who operation payload)
  (let ([reply (string->jsexpr (%mcp-call operation (value->jsexpr-string payload)))])
    (if (hash-contains? reply 'error)
        (%mcp-raise (string-append who ": " (hash-ref reply 'error)))
        (%exact-integers (hash-ref reply 'value)))))

;; A server's tools: a list of {name, description, inputSchema}, complete
;; or an error, never a partial list; with #:tool one tool's {name,
;; description, inputSchema}. With #:search, live tool names and
;; descriptions matching it: {tools, searched, unavailable, truncated}, in
;; `server` only (its errors raise) or, without one, in up to 8 connections
;; the host reports as connected (a failing one is listed under
;; `unavailable`); at most #:limit (default 20).
(define (mcp-tools [server #f] #:tool [tool #f] #:search [query #f] #:limit [limit #f])
  (cond
    [(and tool query) (error "mcp-tools: pass #:tool or #:search, not both")]
    [(and limit (not query)) (error "mcp-tools: #:limit bounds a #:search")]
    [query
     (%mcp "mcp-tools" "search_tools" (hash 'query query 'connection_id server 'limit (or limit 20)))]
    [(not server) (error "mcp-tools: name a server, or pass #:search")]
    [tool (%mcp "mcp-tools" "describe_tool" (hash 'server server 'tool tool))]
    [else (%mcp "mcp-tools" "list_tools" (hash 'server server))]))

;; Call `tool` with `arguments` (a hash; none by default) and return its
;; structured content when it has any, else its text, else its content
;; blocks. A result the server flags as an error raises.
(define (mcp-call server tool [arguments (hash)])
  (%mcp "mcp-call" "call_tool" (hash 'server server 'tool tool 'arguments arguments)))

;; Without `servers`, the user's current connections; `connectionId` is the
;; server name the other functions take. With `servers` (a list of names),
;; open and list each, bounded by #:timeout-ms (default 10000): a list of
;; {server, tools, error}, where a server that failed has no tools and says
;; why.
(define (mcp-status [servers #f] #:timeout-ms [timeout-ms #f])
  (cond
    [servers (%mcp "mcp-status" "status" (hash 'servers servers 'timeout_ms (or timeout-ms 10000)))]
    [timeout-ms (error "mcp-status: #:timeout-ms bounds opening the servers named")]
    [else (%mcp "mcp-status" "list_connections" (hash))]))

;; The supported-service catalog: one page, {plugins, nextCursor}; more
;; entries exist while nextCursor is a string, passed back as #:cursor.
;; #:connection-status filters to "connected" or "not_connected"; #:limit
;; defaults to 50. With #:search, the entries matching it (default 10).
(define (mcp-plugins #:search [query #f]
                     #:connection-status [connection-status #f]
                     #:limit [limit #f]
                     #:cursor [cursor #f])
  (cond
    [(and query (or connection-status cursor))
     (error "mcp-plugins: #:search takes no #:connection-status or #:cursor")]
    [query (%mcp "mcp-plugins" "search_plugins" (hash 'query query 'limit (or limit 10)))]
    [else
     (%mcp "mcp-plugins"
           "list_plugins"
           (hash 'connection_status connection-status 'limit (or limit 50) 'cursor cursor))]))

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

;; The server's resources, as it describes them; with #:templates #t its
;; resource templates; with #:uri one resource's {contents}.
(define (mcp-resources server #:uri [uri #f] #:templates [templates #f])
  (cond
    [(and uri templates) (error "mcp-resources: pass #:uri or #:templates, not both")]
    [uri (%mcp "mcp-resources" "read_resource" (hash 'server server 'uri uri))]
    [templates (%mcp "mcp-resources" "list_resource_templates" (hash 'server server))]
    [else (%mcp "mcp-resources" "list_resources" (hash 'server server))]))

;; The server's prompts, as it describes them; with #:name one prompt
;; rendered with #:arguments (a hash of strings; none by default):
;; {description, messages}.
(define (mcp-prompts server #:name [name #f] #:arguments [arguments #f])
  (cond
    [(and arguments (not name)) (error "mcp-prompts: #:arguments render the prompt #:name names")]
    [name (%mcp "mcp-prompts" "get_prompt" (hash 'server server 'name name 'arguments (or arguments (hash))))]
    [else (%mcp "mcp-prompts" "list_prompts" (hash 'server server))]))
