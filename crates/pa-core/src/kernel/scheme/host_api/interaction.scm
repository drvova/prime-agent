;; Talking to the user: questions, the todo list, and attention pings.

;; Ask the user `question` and wait for the answer: {status, answer}.
;; `choices` (a list of at least two strings) makes it a choice question;
;; without them the user types free text. #:multi #t lets the user pick any
;; number of choices (the answer is then a list). #:default is the answer on
;; a timeout or when nobody can answer; #:timeout is in seconds. status is
;; "answered", "declined", "timed_out", "no_user", or "cancelled"; answer is
;; a string, a list for #:multi, or #f when there is none.
(define (ask question [choices #f] #:multi [multi #f] #:default [default #f] #:timeout [timeout #f])
  (when (and multi (not choices))
    (error "ask: #:multi needs choices"))
  (let* ([reply (%host-call "question.ask"
                            (%options (hash 'id (%fresh-id)
                                            'text question
                                            'kind (cond [(not choices) "text"] [multi "multi"] [else "single"]))
                                      'choices choices
                                      'default (and default (if (list? default) default (list default)))
                                      'timeoutSeconds (and timeout (max 1 (exact (round timeout))))))]
         [status (hash-ref reply 'status)]
         [answer (hash-ref reply 'answer)])
    (hash 'status status
          'answer (cond
                    [(null? answer) (and multi (equal? status "answered") answer)]
                    [multi answer]
                    [else (car answer)]))))

;; Create or replace the session todo list; the whole list, {items}. Each
;; item is its text, or a hash with 'text and optional 'id, 'status
;; ("pending", "in_progress", "done") and 'note. An empty list clears it.
(define (todo-set items)
  (%host-call "todo.set"
              (hash 'items (map (lambda (item) (if (string? item) (hash 'text item) item)) items))))

;; Change one item by id: its #:status, #:text, or #:note ("" clears the
;; note); the whole list.
(define (todo-update id #:status [status #f] #:text [text #f] #:note [note #f])
  (unless (or status text note)
    (error "todo-update: pass at least one of #:status, #:text, #:note"))
  (%host-call "todo.update" (%options (hash 'id id) 'status status 'text text 'note note)))

;; The current session todo list, {items} (empty when none was set).
(define (todo-get)
  (%host-call "todo.get" (hash)))

;; Ping the user with `message` without waiting; whether a user was attached
;; to see it. #:level is "info", "success", "warning", or "error".
(define (notify message #:level [level "info"])
  (hash-ref (%host-call "notify.send" (hash 'message message 'level level)) 'delivered))
