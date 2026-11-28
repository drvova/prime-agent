;; MCP servers: the `mcp-*` functions over the session's MCP client. Each
;; sends one operation to the client and returns its reply; a failure raises
;; an error that starts with the function's name. Names starting with % are
;; internal.

(define (%mcp who operation payload)
  (let ([reply (string->jsexpr (%mcp-call operation (value->jsexpr-string payload)))])
    (if (hash-contains? reply 'error)
        (%mcp-raise (string-append who ": " (hash-ref reply 'error)))
        (%exact-integers (hash-ref reply 'value)))))

;; What `server` (or the user's MCP setup) offers, by #:kind:
;; - "tools" (the default): the server's tools, a list of {name,
;;   description, inputSchema}, complete or an error, never a partial list;
;;   with #:tool one tool's {name, description, inputSchema}. With #:search,
;;   live tool names and descriptions matching it: {tools, searched,
;;   unavailable, truncated}, in `server` only (its errors raise) or, without
;;   one, in up to 8 connections the host reports as connected (a failing one
;;   is listed under `unavailable`); at most #:limit (default 20).
;; - "resources": the server's resources; with #:uri one resource's {contents}.
;; - "templates": the server's resource templates.
;; - "prompts": the server's prompts; with #:name one prompt rendered with
;;   #:arguments (a hash of strings; none by default): {description, messages}.
;; - "connections": without `server`, the user's current connections, whose
;;   `connectionId` is the server name the other kinds take. With `server` (a
;;   name or a list of names), open and list each, bounded by #:timeout-ms
;;   (default 10000): a list of {server, tools, error}.
;; - "catalog": the supported-service catalog, one page: {plugins,
;;   nextCursor}; more entries exist while nextCursor is a string, passed back
;;   as #:cursor. #:connection-status filters to "connected" or
;;   "not_connected"; #:limit defaults to 50. With #:search, the entries
;;   matching it (default 10).
;; An option the kind does not take is refused.
(define (mcp-list [server #f]
                  #:kind [kind "tools"]
                  #:tool [tool #f]
                  #:search [query #f]
                  #:limit [limit #f]
                  #:uri [uri #f]
                  #:name [name #f]
                  #:arguments [arguments #f]
                  #:cursor [cursor #f]
                  #:connection-status [connection-status #f]
                  #:timeout-ms [timeout-ms #f])
  (let* ([kind (if (symbol? kind) (symbol->string kind) kind)]
         [takes (cond
                  [(equal? kind "tools") (list "#:tool" "#:search" "#:limit")]
                  [(equal? kind "resources") (list "#:uri")]
                  [(equal? kind "templates") (list)]
                  [(equal? kind "prompts") (list "#:name" "#:arguments")]
                  [(equal? kind "connections") (list "#:timeout-ms")]
                  [(equal? kind "catalog") (list "#:search" "#:limit" "#:cursor" "#:connection-status")]
                  [else
                   (%mcp-raise (string-append "mcp-list: #:kind is \"tools\", \"resources\", \"templates\", "
                                         "\"prompts\", \"connections\" or \"catalog\""))])]
         [given (list (cons "#:tool" tool)
                      (cons "#:search" query)
                      (cons "#:limit" limit)
                      (cons "#:uri" uri)
                      (cons "#:name" name)
                      (cons "#:arguments" arguments)
                      (cons "#:cursor" cursor)
                      (cons "#:connection-status" connection-status)
                      (cons "#:timeout-ms" timeout-ms))]
         [stray (filter (lambda (option) (and (cdr option) (not (member (car option) takes)))) given)])
    (cond
      [(not (null? stray))
       (%mcp-raise (string-append "mcp-list: " (car (car stray)) " does not apply to #:kind \"" kind "\""
                             (if (null? takes)
                                 ", which takes no options"
                                 (string-append ", which takes " (string-join takes " ")))))]
      [(and server (equal? kind "catalog")) (%mcp-raise "mcp-list: #:kind \"catalog\" takes no server")]
      [(and (not server) (member kind (list "resources" "templates" "prompts")))
       (%mcp-raise (string-append "mcp-list: #:kind \"" kind "\" needs a server"))]
      [(equal? kind "tools")
       (cond
         [(and tool query) (error "mcp-list: pass #:tool or #:search, not both")]
         [(and limit (not query)) (error "mcp-list: #:limit bounds a #:search")]
         [query
          (%mcp "mcp-list" "search_tools" (hash 'query query 'connection_id server 'limit (or limit 20)))]
         [(not server) (error "mcp-list: name a server, or pass #:search")]
         [tool (%mcp "mcp-list" "describe_tool" (hash 'server server 'tool tool))]
         [else (%mcp "mcp-list" "list_tools" (hash 'server server))])]
      [(equal? kind "resources")
       (if uri
           (%mcp "mcp-list" "read_resource" (hash 'server server 'uri uri))
           (%mcp "mcp-list" "list_resources" (hash 'server server)))]
      [(equal? kind "templates") (%mcp "mcp-list" "list_resource_templates" (hash 'server server))]
      [(equal? kind "prompts")
       (cond
         [(and arguments (not name)) (error "mcp-list: #:arguments render the prompt #:name names")]
         [name
          (%mcp "mcp-list" "get_prompt" (hash 'server server 'name name 'arguments (or arguments (hash))))]
         [else (%mcp "mcp-list" "list_prompts" (hash 'server server))])]
      [(equal? kind "connections")
       (cond
         [server
          (%mcp "mcp-list"
                "status"
                (hash 'servers (if (string? server) (list server) server) 'timeout_ms (or timeout-ms 10000)))]
         [timeout-ms (error "mcp-list: #:timeout-ms bounds opening the servers named")]
         [else (%mcp "mcp-list" "list_connections" (hash))])]
      [(and query (or connection-status cursor))
       (error "mcp-list: #:search takes no #:connection-status or #:cursor")]
      [query (%mcp "mcp-list" "search_plugins" (hash 'query query 'limit (or limit 10)))]
      [else
       (%mcp "mcp-list"
             "list_plugins"
             (hash 'connection_status connection-status 'limit (or limit 50) 'cursor cursor))])))

;; Call `tool` with `arguments` (a hash; none by default) and return its
;; structured content when it has any, else its text, else its content
;; blocks. A result the server flags as an error raises.
(define (mcp-call server tool [arguments (hash)])
  (%mcp "mcp-call" "call_tool" (hash 'server server 'tool tool 'arguments arguments)))

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

;; Deprecated aliases (`deprecated/aliases.txt`): each takes its old
;; parameters, prints its notice once, and calls `mcp-list`.

(define (mcp-tools [server #f] #:tool [tool #f] #:search [query #f] #:limit [limit #f])
  (%deprecated "mcp-tools")
  (mcp-list server #:kind "tools" #:tool tool #:search query #:limit limit))

(define (mcp-status [servers #f] #:timeout-ms [timeout-ms #f])
  (%deprecated "mcp-status")
  (mcp-list servers #:kind "connections" #:timeout-ms timeout-ms))

(define (mcp-plugins #:search [query #f]
                     #:connection-status [connection-status #f]
                     #:limit [limit #f]
                     #:cursor [cursor #f])
  (%deprecated "mcp-plugins")
  (mcp-list #:kind "catalog"
            #:search query
            #:connection-status connection-status
            #:limit limit
            #:cursor cursor))

(define (mcp-resources server #:uri [uri #f] #:templates [templates #f])
  (%deprecated "mcp-resources")
  (cond
    [(and uri templates) (error "mcp-resources: pass #:uri or #:templates, not both")]
    [templates (mcp-list server #:kind "templates")]
    [else (mcp-list server #:kind "resources" #:uri uri)]))

(define (mcp-prompts server #:name [name #f] #:arguments [arguments #f])
  (%deprecated "mcp-prompts")
  (mcp-list server #:kind "prompts" #:name name #:arguments arguments))
