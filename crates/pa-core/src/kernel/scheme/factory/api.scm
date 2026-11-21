;; The factory API. Every function sends one request to the session's
;; factory through `%factory-request-json` and returns its reply; a refusal
;; raises with the factory's own sentence. Names starting with % are
;; internal.

(define (%factory operation payload)
  (let ([reply (string->jsexpr (%factory-request-json operation (value->jsexpr-string payload)))])
    (if (hash-contains? reply 'error)
        (error (hash-ref reply 'error))
        (%exact-integers (hash-ref reply 'value)))))

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
