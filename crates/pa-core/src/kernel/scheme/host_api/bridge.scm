;; Shared plumbing for the host API: every function below it is a thin
;; wrapper that builds a payload, sends one typed host request, and shapes
;; the reply. Names starting with % are internal.

;; JSON numbers arrive as floats; whole ones become exact integers again, so
;; a count prints as 3 and can be sent back where the host wants an integer.
(define (%exact-integers value)
  (cond
    [(and (number? value) (inexact? value) (integer? value)) (exact value)]
    [(list? value) (map %exact-integers value)]
    [(hash? value)
     (foldl (lambda (entry rebuilt)
              (hash-insert rebuilt (car entry) (%exact-integers (cdr entry))))
            (hash)
            (hash->list value))]
    [else value]))

;; Send `payload` as the host request `type` and return its reply. The type
;; rides in the payload too (some handlers read it there), set last so a
;; payload key cannot reroute the request.
(define (%host-call type payload)
  (%exact-integers (%host-request type (hash-insert payload 'type type))))

;; `payload` plus each `key value` pair whose value was given: #f and void
;; (an omitted keyword argument) add nothing.
(define (%options payload . entries)
  (let loop ([payload payload] [entries entries])
    (if (null? entries)
        payload
        (loop (let ([value (cadr entries)])
                (if (or (void? value) (not value))
                    payload
                    (hash-insert payload (car entries) value)))
              (cddr entries)))))

;; `value` as the exact integer the host expects (8.0 becomes 8), or an error
;; naming the argument.
(define (%integer who name value)
  (if (and (number? value) (integer? value))
      (exact value)
      (error (string-append who ": " name " must be an integer"))))

;; The host selector of a family member: a name or id string, or a row that
;; carries one (a spawn handle, a subagent row, a collect result).
(define (%child-selector who target)
  (cond
    [(string? target) (trim target)]
    [(and (hash? target) (hash-contains? target 'rlm_child_id)) (hash-ref target 'rlm_child_id)]
    [else
     (error (string-append who ": the target must be a name, an id, a spawn handle, or a subagent row"))]))
