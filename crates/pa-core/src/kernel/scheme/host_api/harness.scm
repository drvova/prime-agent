;; The continual harness store: memories, prompt notes, skills, subagent
;; specs, and refinement events that outlive the turn. Entries go to this
;; session's store unless #:global #t selects the cross-session one; an id
;; written "global:<id>" or "local:<id>", as the overview shows it, is
;; accepted as is. An entry is {id, kind, title, content, path, scope,
;; reference, arguments, metadata, source, created_at, updated_at, version};
;; version starts at 1 and grows with every update. Everything raises while
;; the user has harness refinements turned off in /settings.

(define (%harness who verb payload)
  (%kernel-reply (%harness-call (string-append verb ":" who) (value->jsexpr-string payload))))

;; Create (`id` may be #f: the entry is then named after its title) or
;; update one entry. An update keeps every optional field it omits.
(define (%harness-write who action kind id title content path reference arguments metadata global)
  (%harness who
            "write"
            (%options (hash 'action action
                            'kind kind
                            'title title
                            'content content
                            'global (if global #t #f))
                      'id id
                      'path path
                      'reference reference
                      'arguments arguments
                      'metadata metadata)))

(define (%harness-delete who kind id global)
  (%harness who "delete" (hash 'kind kind 'id id 'global (if global #t #f))))

;; Create a memory; the entry. #:path groups it (default "general").
(define (harness-create-memory title content
                               #:id [id #f] #:path [path "general"] #:metadata [metadata #f] #:global [global #f])
  (%harness-write "harness-create-memory" "create" "memory" id title content path #f #f metadata global))

;; Replace a memory's title and content; the entry.
(define (harness-update-memory id title content
                               #:path [path #f] #:metadata [metadata #f] #:global [global #f])
  (%harness-write "harness-update-memory" "update" "memory" id title content path #f #f metadata global))

;; Delete a memory; whether it existed.
(define (harness-delete-memory id #:global [global #f])
  (%harness-delete "harness-delete-memory" "memory" id global))

;; Create a prompt note (standing guidance); the entry. #:path defaults to
;; "policy".
(define (harness-create-prompt-note title content
                                    #:id [id #f] #:path [path "policy"] #:metadata [metadata #f] #:global [global #f])
  (%harness-write "harness-create-prompt-note" "create" "prompt" id title content path #f #f metadata global))

;; Replace a prompt note's title and content; the entry.
(define (harness-update-prompt-note id title content
                                    #:path [path #f] #:metadata [metadata #f] #:global [global #f])
  (%harness-write "harness-update-prompt-note" "update" "prompt" id title content path #f #f metadata global))

;; Delete a prompt note; whether it existed.
(define (harness-delete-prompt-note id #:global [global #f])
  (%harness-delete "harness-delete-prompt-note" "prompt" id global))

;; Create a skill entry; the entry. A skill entry describes a Scheme
;; function of an installed skill: #:reference is required, a hash with
;; 'type "scheme", 'skill (the installed skill whose skill.scm or
;; scheme/*.scm defines the function), and a 'callable (the function's name)
;; or 'call_pattern (a call of it); #:arguments describes its parameters.
(define (harness-create-skill title content
                              #:id [id #f] #:path [path "general"]
                              #:reference [reference #f] #:arguments [arguments #f]
                              #:metadata [metadata #f] #:global [global #f])
  (%harness-write "harness-create-skill" "create" "skill" id title content path reference arguments metadata global))

;; Replace a skill entry's title and content; the entry. An omitted
;; #:reference or #:arguments keeps the stored one.
(define (harness-update-skill id title content
                              #:path [path #f]
                              #:reference [reference #f] #:arguments [arguments #f]
                              #:metadata [metadata #f] #:global [global #f])
  (%harness-write "harness-update-skill" "update" "skill" id title content path reference arguments metadata global))

;; Delete a skill entry; whether it existed.
(define (harness-delete-skill id #:global [global #f])
  (%harness-delete "harness-delete-skill" "skill" id global))

;; Create a subagent spec (a reusable brief for `rlm-spawn`); the entry.
(define (harness-create-subagent title content
                                 #:id [id #f] #:path [path "general"] #:metadata [metadata #f] #:global [global #f])
  (%harness-write "harness-create-subagent" "create" "subagent" id title content path #f #f metadata global))

;; Replace a subagent spec's title and content; the entry.
(define (harness-update-subagent id title content
                                 #:path [path #f] #:metadata [metadata #f] #:global [global #f])
  (%harness-write "harness-update-subagent" "update" "subagent" id title content path #f #f metadata global))

;; Delete a subagent spec; whether it existed.
(define (harness-delete-subagent id #:global [global #f])
  (%harness-delete "harness-delete-subagent" "subagent" id global))

;; The `arguments` of a factory entry: its spec under 'dag or 'machine,
;; exactly one of them, or #f when neither was given.
(define (%harness-factory-arguments who dag machine)
  (cond
    [(and dag machine) (error (string-append who ": pass either #:dag or #:machine, not both"))]
    [machine (hash 'machine machine)]
    [dag (hash 'dag dag)]
    [else #f]))

;; Create a factory entry (a state machine of subagent states that
;; `factory-run` runs by its id); the entry. The spec is #:machine (states
;; and transitions) or #:dag (nodes; sugar that compiles to a machine),
;; exactly one, and it is validated first: an invalid spec is never stored
;; (`factory-validate` lists its problems). Refused while the factory is
;; disabled.
(define (harness-create-factory title content
                                #:id [id #f] #:path [path "general"]
                                #:dag [dag #f] #:machine [machine #f]
                                #:metadata [metadata #f] #:global [global #f])
  (%harness-write "harness-create-factory" "create" "factory" id title content path #f
                  (%harness-factory-arguments "harness-create-factory" dag machine)
                  metadata global))

;; Replace a factory entry's title and content, and its spec when #:dag or
;; #:machine is given (validated like a new one); the entry. Refused while
;; the factory is disabled.
(define (harness-update-factory id title content
                                #:path [path #f]
                                #:dag [dag #f] #:machine [machine #f]
                                #:metadata [metadata #f] #:global [global #f])
  (%harness-write "harness-update-factory" "update" "factory" id title content path #f
                  (%harness-factory-arguments "harness-update-factory" dag machine)
                  metadata global))

;; Delete a factory entry; whether it existed. Allowed while the factory is
;; disabled: clearing a stale entry is not authoring.
(define (harness-delete-factory id #:global [global #f])
  (%harness-delete "harness-delete-factory" "factory" id global))

;; Record one refinement pass: {id, trigger, changes, evidence, outcome,
;; created_at}. `trigger` says what caused it; `changes` is one string or a
;; list of them.
(define (harness-record-refinement trigger changes
                                   #:evidence [evidence ""] #:outcome [outcome ""]
                                   #:id [id #f] #:global [global #f])
  (%harness "harness-record-refinement"
            "record-refinement"
            (%options (hash 'trigger trigger
                            'changes changes
                            'evidence evidence
                            'outcome outcome
                            'global (if global #t #f))
                      'id id)))

;; A suggested diagnose, update, validate plan for one observation: a list
;; of steps. Nothing is stored.
(define (harness-plan-refinement observation
                                 #:failing-component [failing-component ""]
                                 #:next-step [next-step ""])
  (append
   (list (string-append "Diagnose the repeated failure or opportunity"
                        (if (equal? failing-component "") "" (string-append " for " failing-component))
                        ": "
                        observation)
         "Update the smallest useful prompt note, memory item, skill, or subagent spec."
         "Run the next action with the changed harness state, then record the outcome.")
   (if (equal? next-step "") '() (list (string-append "Immediate validation step: " next-step)))))

;; A readable summary of one store: its file, each kind's count and first
;; #:max-entries-per-kind entries, and the last refinements.
(define (harness-overview #:max-entries-per-kind [max-entries-per-kind 20] #:global [global #f])
  (%harness "harness-overview"
            "overview"
            (hash 'max_entries_per_kind
                  (%integer "harness-overview" "#:max-entries-per-kind" max-entries-per-kind)
                  'global (if global #t #f))))

;; Entries ranked by how well they match the words of `query` (title,
;; content, path, and id; rare words count more), best first, at most
;; #:limit. #:kind narrows to "prompt", "memory", "skill", "subagent", or
;; "factory".
(define (harness-search query #:kind [kind #f] #:limit [limit 10] #:global [global #f])
  (%harness "harness-search"
            "search"
            (%options (hash 'query query
                            'limit (%integer "harness-search" "#:limit" limit)
                            'global (if global #t #f))
                      'kind kind)))

;; One store in full: {scope, file_path, entries, refinements}, entries
;; keyed by kind and then id. #:state-dir reads another agent's store from
;; its harness directory.
(define (harness-state #:state-dir [state-dir #f] #:global [global #f])
  (%harness "harness-state"
            "state"
            (%options (hash 'global (if global #t #f)) 'state_dir state-dir)))

;; How each installed skill and global harness entry has been reused: one
;; hash per entry, with the sessions other than its creator's that were
;; offered and used it, its uses there and in its own session, its last
;; use, and what it costs each prompt that carries it. Read-only, and
;; answered while harness refinements are off.
(define (reuse-report) (%reuse-report))
