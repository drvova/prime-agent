;; Frida: the `frida-*` functions over the session's Frida registry. Each
;; sends one operation and returns its reply; a failure raises an error that
;; starts with the function's name and carries Frida's own message. Names
;; starting with % are internal.

(define (%frida who operation payload)
  (let ([reply (string->jsexpr (%frida-call operation (value->jsexpr-string payload)))])
    (if (hash-contains? reply 'error)
        (%frida-raise (string-append who ": " (hash-ref reply 'error)))
        (%exact-integers (hash-ref reply 'value)))))

;; The id in a session or script handle (the hash a function returned), or
;; the id itself.
(define (%frida-id handle key)
  (if (hash? handle) (hash-ref handle key) handle))

;; The devices Frida sees: a list of {id, name, type}.
(define (frida-devices)
  (%frida "frida-devices" "devices" (hash)))

;; Connect a frida-server at `address` ("host[:port]"): its {id, name, type}.
;; Pass the id as #:device.
(define (frida-add-remote address
                          #:certificate [certificate #f]
                          #:origin [origin #f]
                          #:token [token #f]
                          #:keepalive-interval [keepalive-interval #f])
  (%frida "frida-add-remote"
          "add-remote"
          (hash 'address address
                'certificate certificate
                'origin origin
                'token token
                'keepalive_interval keepalive-interval)))

;; Running processes, each {pid, name}; with #:applications #t the installed
;; applications, each {identifier, name, pid} (pid 0 when not running).
(define (frida-ps #:device [device #f] #:applications [applications #f])
  (%frida "frida-ps" "ps" (hash 'device device 'applications applications)))

;; Kill a process by pid or name.
(define (frida-kill target #:device [device #f])
  (%frida "frida-kill" "kill" (hash 'target target 'device device)))

;; Attach to a pid or process name, or with #:spawn #t start a program path
;; or application identifier suspended with #:args and attach to it: the
;; session {session, pid, spawned}.
(define (frida-attach target
                      #:device [device #f]
                      #:spawn [spawn #f]
                      #:args [args (list)]
                      #:pause [pause #f])
  (%frida "frida-attach"
          "attach"
          (hash 'target target 'device device 'spawn spawn 'args args 'pause pause)))

;; This kernel's sessions: each {session, pid, spawned, detached-reason,
;; scripts}.
(define (frida-sessions)
  (%frida "frida-sessions" "sessions" (hash)))

;; Let a spawned target run (once; a no-op for an attached process).
(define (frida-resume session)
  (%frida "frida-resume" "resume" (hash 'session (%frida-id session 'session))))

;; Detach; a target the session spawned is killed.
(define (frida-detach session)
  (%frida "frida-detach" "detach" (hash 'session (%frida-id session 'session))))

;; Create and load a script from JavaScript source: {script, session, name}.
(define (frida-script-load session source #:name [name #f] #:runtime [runtime #f])
  (%frida "frida-script-load"
          "script-load"
          (hash 'session (%frida-id session 'session) 'source source 'name name 'runtime runtime)))

;; The script's buffered messages, oldest first (the newest `n` when given):
;; {messages, dropped}.
(define (frida-script-messages script [n #f])
  (%frida "frida-script-messages"
          "script-messages"
          (hash 'script (%frida-id script 'script) 'n n)))

;; The newest `n` messages as text, one line each.
(define (frida-script-tail script [n 20])
  (%frida "frida-script-tail"
          "script-messages"
          (hash 'script (%frida-id script 'script) 'n n 'tail #t)))

;; The next message after the last one a wait returned that `predicate`
;; accepts (any, by default), waiting up to #:timeout-ms: the message, or #f
;; on timeout or once the script is gone. Messages the predicate rejects are
;; skipped for good.
(define (frida-script-wait script #:predicate [predicate #f] #:timeout-ms [timeout-ms 30000])
  (let ([id (%frida-id script 'script)])
    (let loop ([left timeout-ms])
      (let* ([reply (%frida "frida-script-wait" "script-wait" (hash 'script id 'timeout_ms left))]
             [message (hash-ref reply 'message)])
        (cond
          [(not message) #f]
          [(or (not predicate) (predicate message)) message]
          [else (loop (hash-ref reply 'remaining-ms))])))))

;; Call the script's `rpc.exports` function `export` with `args` (a list)
;; and return its result.
(define (frida-script-call script export [args (list)] #:timeout-ms [timeout-ms 30000])
  (%frida "frida-script-call"
          "script-call"
          (hash 'script (%frida-id script 'script)
                'export export
                'args args
                'timeout_ms timeout-ms)))

;; Send a JSON message (and optionally base64 binary data) to the script's
;; `recv()`.
(define (frida-script-post script message #:data-base64 [data-base64 #f])
  (%frida "frida-script-post"
          "script-post"
          (hash 'script (%frida-id script 'script) 'message message 'data_base64 data-base64)))

;; Unload the script.
(define (frida-script-unload script)
  (%frida "frida-script-unload" "script-unload" (hash 'script (%frida-id script 'script))))

;; The tracer's ApiResolver queries for a list of names, each marked `mark`:
;; a name that holds "!" is "module!function", any other is completed by
;; `complete`.
(define (%frida-trace-steps mark kind names complete)
  (map (lambda (name)
         (list mark
               (string-append kind
                              ":"
                              (if (string-contains? name "!") name (complete name)))))
       names))

;; Trace calls of the functions matching the globs: #:include / #:exclude
;; take function names, #:include-modules / #:exclude-modules module names,
;; #:include-imports the modules whose imports to trace. Returns a script
;; whose messages are the calls.
(define (frida-trace session
                     #:include [include (list)]
                     #:exclude [exclude (list)]
                     #:include-modules [include-modules (list)]
                     #:exclude-modules [exclude-modules (list)]
                     #:include-imports [include-imports (list)])
  (%frida "frida-trace"
          "trace"
          (hash 'session (%frida-id session 'session)
                'name "trace"
                'spec (append
                       (%frida-trace-steps "+" "exports" include-modules
                                           (lambda (name) (string-append name "!*")))
                       (%frida-trace-steps "+" "imports" include-imports
                                           (lambda (name) (string-append name "!*")))
                       (%frida-trace-steps "+" "exports" include
                                           (lambda (name) (string-append "*!" name)))
                       (%frida-trace-steps "-" "exports" exclude-modules
                                           (lambda (name) (string-append name "!*")))
                       (%frida-trace-steps "-" "exports" exclude
                                           (lambda (name) (string-append "*!" name)))))))

;; Bundle the script project at `entrypoint` (TypeScript or JavaScript) with
;; Frida's compiler and return the bundle's text, ready for
;; frida-script-load.
(define (frida-compile entrypoint #:project-root [project-root #f])
  (%frida "frida-compile" "compile" (hash 'entrypoint entrypoint 'project_root project-root)))
