;; The continual harness store: memories, prompt notes, skills, subagent
;; specs, factories, and refinement events that outlive the turn. Entries go
;; to this session's store unless #:global #t selects the cross-session one;
;; a name written "global:<name>" or "local:<name>", as the overview shows
;; it, is accepted as is. An entry is {id, kind, title, content, path, scope,
;; reference, arguments, metadata, source, created_at, updated_at, version};
;; version starts at 1 and grows with every update. Everything raises while
;; the user has harness refinements turned off in /settings.

(define (%harness who verb payload)
  (%kernel-reply (%harness-call (string-append verb ":" who) (value->jsexpr-string payload))))

;; The `arguments` of a factory entry: its spec under 'dag or 'machine,
;; exactly one of them, or `arguments` itself when neither was given.
(define (%harness-arguments who arguments dag machine)
  (cond
    [(and dag machine) (error (string-append who ": pass either #:dag or #:machine, not both"))]
    [(and arguments (or dag machine))
     (error (string-append who ": #:dag and #:machine are a factory's arguments; pass them alone"))]
    [machine (hash 'machine machine)]
    [dag (hash 'dag dag)]
    [else arguments]))

;; Create or update the entry `name` of `kind` ("memory", "prompt",
;; "skill", "subagent", or "factory"); the entry. A new entry is titled
;; `name` unless #:title says otherwise, and filed under #:path ("policy"
;; for a prompt note, "general" otherwise); an update keeps every field it
;; omits. A skill entry describes a Scheme function of an installed skill:
;; its first write needs #:reference, a hash with 'type "scheme", 'skill,
;; and a 'callable or 'call_pattern. A factory entry is a state machine
;; `factory-run` runs by its name: its first write needs #:machine or
;; #:dag, validated first, and it is refused while the factory is disabled.
(define (harness-put kind name content
                     #:title [title #f]
                     #:path [path #f]
                     #:reference [reference #f]
                     #:arguments [arguments #f]
                     #:dag [dag #f]
                     #:machine [machine #f]
                     #:metadata [metadata #f]
                     #:global [global #f])
  (%harness "harness-put"
            "write"
            (%options (hash 'kind kind 'id name 'content content 'global (if global #t #f))
                      'title title
                      'path path
                      'reference reference
                      'arguments (%harness-arguments "harness-put" arguments dag machine)
                      'metadata metadata)))

;; Delete the entry `name` of `kind`; whether it existed. Allowed while the
;; factory is disabled: clearing a stale entry is not authoring.
(define (harness-delete kind name #:global [global #f])
  (%harness "harness-delete" "delete" (hash 'kind kind 'id name 'global (if global #t #f))))

;; Read one store. With `query`, entries ranked by how well they match its
;; words (title, content, path, and id; rare words count more), best first,
;; at most #:limit (default 10), #:kind narrowing to one kind. Without it,
;; #:view picks: "overview" (the default: a readable summary with each
;; kind's count and first #:limit entries, default 20, and the last
;; refinements), "state" (the store in full, {scope, file_path, entries,
;; refinements}; #:state-dir reads another agent's), or "reuse" (how each
;; installed skill and global entry has been reused; answered while
;; harness refinements are off).
(define (harness-query [query #f]
                       #:view [view #f]
                       #:kind [kind #f]
                       #:limit [limit #f]
                       #:state-dir [state-dir #f]
                       #:global [global #f])
  (let ([view (or view (if query "search" "overview"))]
        [global (if global #t #f)])
    (when (and query (not (equal? view "search")))
      (error "harness-query: a query searches; it takes no #:view"))
    (when (and kind (not (equal? view "search")))
      (error "harness-query: #:kind narrows a search; pass a query"))
    (when (and state-dir (not (equal? view "state")))
      (error "harness-query: #:state-dir reads another store in full; pass #:view \"state\""))
    (cond
      [(equal? view "search")
       (unless (string? query)
         (error "harness-query: #:view \"search\" needs a query"))
       (%harness "harness-query"
                 "search"
                 (%options (hash 'query query
                                 'limit (%integer "harness-query" "#:limit" (or limit 10))
                                 'global global)
                           'kind kind))]
      [(equal? view "overview")
       (%harness "harness-query"
                 "overview"
                 (hash 'max_entries_per_kind (%integer "harness-query" "#:limit" (or limit 20))
                       'global global))]
      [(equal? view "state")
       (%harness "harness-query" "state" (%options (hash 'global global) 'state_dir state-dir))]
      [(equal? view "reuse") (%reuse-report)]
      [else
       (error (string-append "harness-query: unknown #:view " (to-string view)
                             "; expected \"overview\", \"state\", \"reuse\", or a query"))])))

;; Record one refinement pass: {id, trigger, changes, evidence, outcome,
;; created_at}. `trigger` says what caused it; `changes` is one string or a
;; list of them.
(define (refine-record trigger changes
                       #:evidence [evidence ""] #:outcome [outcome ""]
                       #:id [id #f] #:global [global #f])
  (%harness "refine-record"
            "record-refinement"
            (%options (hash 'trigger trigger
                            'changes changes
                            'evidence evidence
                            'outcome outcome
                            'global (if global #t #f))
                      'id id)))
