;; Rizin: the `rz-*` functions over the kernel's rizin sessions. Each sends
;; one operation and returns its reply; a failure raises an error that
;; starts with the function's name. Names starting with % are internal.

(define (%rz who operation options)
  (let ([reply (%rz-call operation options)])
    (if (hash-contains? reply 'error)
        (%native-raise (string-append who ": " (hash-ref reply 'error)))
        (hash-ref reply 'value))))

;; The id in a session (the hash rz-open returned), or the id itself.
(define (%rz-id session)
  (if (hash? session) (hash-ref session 'session) session))

;; Start a rizin on `target` (a path, a URI such as "malloc://512" or
;; "gdb://host:port", a pid with #:debug #t, or "--" for no file) and return
;; the session: {session, target, pid, version, debug, write, alive, reason,
;; output}. #:write opens for writing, #:debug starts the target under the
;; debugger with #:args (a list of strings) and its stdin read from #:stdin
;; (a file; nothing by default), #:arch, #:bits and #:base override what the
;; file says, #:raw loads no binary info, #:analyze runs `aaa` first, and
;; #:flags (a list of strings) are further rizin options.
(define (rz-open target
                 #:write [write #f]
                 #:debug [debug #f]
                 #:args [args (list)]
                 #:arch [arch #f]
                 #:bits [bits #f]
                 #:base [base #f]
                 #:raw [raw #f]
                 #:analyze [analyze #f]
                 #:stdin [stdin #f]
                 #:flags [flags (list)]
                 #:timeout-ms [timeout-ms 120000])
  (%rz "rz-open"
       "open"
       (hash 'target target
             'write write
             'debug debug
             'args args
             'arch arch
             'bits bits
             'base base
             'raw raw
             'analyze analyze
             'stdin stdin
             'flags flags
             'timeout-ms timeout-ms)))

;; Run one rizin command line and return what it printed, errors included.
(define (rz-cmd session command #:timeout-ms [timeout-ms 120000])
  (%rz "rz-cmd"
       "cmd"
       (hash 'session (%rz-id session) 'command command 'timeout-ms timeout-ms)))

;; Run a command's JSON form and return the parsed value: hashes keyed by
;; rizin's own names, lists, #f for null and for no output.
(define (rz-cmdj session command #:timeout-ms [timeout-ms 120000])
  (%rz "rz-cmdj"
       "cmdj"
       (hash 'session (%rz-id session) 'command command 'timeout-ms timeout-ms)))

;; This kernel's sessions: each {session, target, pid, version, debug,
;; write, alive, reason}.
(define (rz-sessions)
  (%rz "rz-sessions" "sessions" (hash)))

;; Kill a session's rizin and whatever it started: #t, or #f when there is
;; no such session.
(define (rz-close session)
  (%rz "rz-close" "close" (hash 'session (%rz-id session))))
