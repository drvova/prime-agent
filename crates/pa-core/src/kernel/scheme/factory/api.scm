;; The factory API. Every function sends one request to the session's
;; factory through `%factory-request-json` and returns its reply; a refusal
;; raises with the factory's own sentence. Names starting with % are
;; internal. `(factory-help)` is the full guide.

(define (%factory operation payload)
  (let ([reply (string->jsexpr (%factory-request-json operation (value->jsexpr-string payload)))])
    (if (hash-contains? reply 'error)
        (error (hash-ref reply 'error))
        (%exact-integers (hash-ref reply 'value)))))

;; `payload` plus `key value` when the value was given (#f and void add
;; nothing).
(define (%factory-with payload key value)
  (if (or (void? value) (not value))
      payload
      (hash-insert payload key value)))

;; Start a run of a stored factory entry, or of the library machine of that
;; name when no entry has the id. Returns at once; the run continues after
;; the cell and the turn end: {run_id, spec_id, name, nodes, max_parallel,
;; started, pending}, plus {machine, machine_path} for a library machine.
;; #:name labels the run in status and the TUI.
(define (factory-run spec-id #:name [name #f])
  (%factory "run" (%factory-with (hash 'spec_id spec-id) 'name name)))

;; A run's state: {run_id, spec_id, name, state, nodes, events, elapsed_ms,
;; usage}, plus needs_verify when an entry keeps a provisional answer, and
;; pause_reason, last_error and remedy while paused. `state` is running,
;; stopping, paused, done, failed or stopped.
(define (factory-status run-id)
  (%factory "status" (hash 'run_id run-id)))

;; Cancel every running child of the run: {run_id, state, cancelled}.
;; Stopping twice is harmless.
(define (factory-stop run-id)
  (%factory "stop" (hash 'run_id run-id)))

;; Continue a paused run: {run_id, state, started, pending}. After an
;; escalate pause the failed entry is admitted again first. Raises when the
;; run is not paused.
(define (factory-resume run-id)
  (%factory "resume" (hash 'run_id run-id)))

;; A machine's structure fused with live state: for a run id {run_id,
;; spec_id, name, state, pause_reason, elapsed_ms, machine, nodes,
;; active_nodes, last_fired, events, usage, budget}; for a stored spec id
;; the static structure; with no argument {runs}, every live run and the
;; newest finished ones.
(define (factory-graph [reference #f])
  (%factory "graph" (%factory-with (hash) 'ref reference)))

;; The graph of a run plus `changed`, once the run's state or instance
;; shape changes or `seconds` (at most 60) have passed.
(define (factory-watch run-id [seconds 0])
  (%factory "watch" (hash 'run_id run-id 'timeout seconds)))

;; The factory guide: authoring rules, guards, joins, cycles, foreach,
;; budgets, and this API with worked examples.
(define (factory-help)
  (%factory "help" (hash)))

;; The validation sentences of a factory spec in either form (machine, or
;; dag sugar): an empty list means the spec is valid.
(define (factory-validate spec)
  (%factory "validate" (hash 'spec spec)))

;; The canonical machine form of a spec, every default applied:
;; {run, states, transitions}. Raises the joined validation sentences.
(define (factory-canonicalize spec)
  (%factory "canonicalize" (hash 'spec spec)))

;; A dag-form spec compiled to machine form: {machine, errors}, the machine
;; void when the dag has errors.
(define (factory-compile-dag dag)
  (%factory "compile-dag" (hash 'dag dag)))

;; Node ids of a dag-form node list in dependency order (input order among
;; ready nodes). Raises on a duplicate id, an unknown dependency, or a cycle.
(define (factory-topological-order nodes)
  (%factory "topological-order" (hash 'nodes nodes)))

;; The machine library's usable machines, sorted by name:
;; a list of {name, description, version, author, source, path}.
(define (factory-list-machines)
  (%factory "list-machines" (hash)))

;; Validate a MACHINE.md file and store it in the personal machine library:
;; {name, path, created}. An invalid machine is never stored.
(define (factory-import-machine path)
  (%factory "import-machine" (hash 'path path)))

;; Write a stored factory entry's spec, a run's machine, or a library
;; machine as MACHINE.md at `out`: {name, path, source}. An existing file
;; is refused unless #:overwrite #t.
(define (factory-export-machine target out #:overwrite [overwrite #f])
  (%factory "export-machine" (hash 'target target 'out out 'overwrite (if overwrite #t #f))))
