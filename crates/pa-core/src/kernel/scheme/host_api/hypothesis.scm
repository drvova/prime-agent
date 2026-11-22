;; Parallel hypotheses: try several approaches to one task at once, each in
;; its own worktree with its own child.

(define (%hypotheses operation payload)
  (%kernel-reply (%hypotheses-call operation (value->jsexpr-string payload))))

;; One row with the verdict of a procedure check on its checkout: an integer
;; is an exit code (0 passes), anything else passes unless it is #f, and a
;; check that raises fails its approach with the error as its tail.
(define (%hypothesis-checked row check)
  (let ([outcome (with-handler (lambda (raised) (list 'raised (to-string raised)))
                               (check (hash-ref (hash-ref row 'worktree) 'path)))])
    (cond
      [(and (list? outcome) (= (length outcome) 2) (eq? (car outcome) 'raised))
       (hash-insert (hash-insert row 'passed #f) 'check_tail (cadr outcome))]
      [(integer? outcome)
       (hash-insert (hash-insert row 'passed (= outcome 0)) 'check_exit (exact outcome))]
      [else
       (hash-insert (hash-insert row 'passed (if outcome #t #f)) 'check_exit (if outcome 0 1))])))

;; Try 2 to 6 distinct `approaches` to `task`, one child per approach in its
;; own worktree; once the children settle, run #:check in each checkout and
;; return the ranked table {task, rows, winner, text}: passing approaches
;; first, then those whose child finished, then the smaller diff. #:check is
;; required: a shell command run from each checkout's root (exit 0 passes),
;; or a procedure taking the checkout path (an integer is an exit code,
;; anything else passes unless #f). Each row is {rank, approach, name,
;; passed, child_status, check_exit, check_tail, answer_preview, diff,
;; worktree}; diff is {name, files, lines, text, label} with the whole patch
;; in text. Every worktree but the winner's is removed (worktree is #f on
;; those rows); winner is the first row when it passed, else #f. Bring the
;; winner home with (worktree-merge (hash-ref winner 'worktree)), then
;; `worktree-remove` it. Children still running after #:timeout-ms are
;; aborted and their approaches fail. #:better #t first checks each child's
;; brief (see `better`). With a shell check, a cell stopped midway still
;; aborts the children and removes the worktrees; with a procedure check,
;; stopping the cell while the checks run leaves the worktrees in place.
(define (rlm-hypotheses task approaches
                        #:check [check #f]
                        #:timeout-ms [timeout-ms 1800000]
                        #:better [better #f])
  (unless (or (string? check) (function? check))
    (error "rlm-hypotheses: #:check must be a shell command string or a procedure taking the checkout path"))
  (let ([request (hash 'task task
                       'approaches approaches
                       'timeout_ms (%integer "rlm-hypotheses" "#:timeout-ms" timeout-ms)
                       'better (if better #t #f))])
    (if (string? check)
        (%hypotheses "run" (hash-insert request 'check check))
        (%hypotheses "rank"
                     (hash 'task task
                           'rows (map (lambda (row)
                                        (if (equal? (hash-ref row 'child_status) "timeout")
                                            row
                                            (%hypothesis-checked row check)))
                                      (hash-ref (%hypotheses "run" request) 'rows)))))))
