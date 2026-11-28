;; The session's own state: context, harness refinement, the goal, the
;; model, paged-out output, and checkpoints.

;; Schedule context compaction for when the turn ends: {scheduled: #t}, or
;; {scheduled: #f, reason}. `instructions` focus the summary. With
;; #:status #t it schedules nothing and returns the context usage:
;; {tokens, context_window, percent, scheduled}.
(define (compact [instructions #f] #:status [status #f])
  (cond
    [(and status instructions) (error "compact: #:status takes no instructions")]
    [status (%host-call "compact.status" (hash))]
    [else (%host-call "compact.run" (%options (hash) 'instructions instructions))]))

;; Refinement state: {pending, in_flight}.
(define (refine-status)
  (%host-call "refine.status" (hash)))

;; Schedule harness refinement for when the turn ends: {scheduled: #t}, or
;; {scheduled: #f, reason}. #:global #t targets the cross-session store.
;; With #:plan-only #t nothing is scheduled: the reply is a suggested
;; diagnose, update, validate plan for the observation `instructions`, a
;; list of steps.
(define (refine-run [instructions #f] #:global [global #f] #:plan-only [plan-only #f])
  (cond
    [(and plan-only global) (error "refine-run: #:plan-only stores nothing, so it takes no #:global")]
    [plan-only
     (unless (string? instructions)
       (error "refine-run: #:plan-only needs the observation as its instructions"))
     (list (string-append "Diagnose the repeated failure or opportunity: " instructions)
           "Update the smallest useful prompt note, memory item, skill, or subagent spec."
           "Run the next action with the changed harness state, then record the outcome.")]
    [else (%host-call "refine.run" (%options (hash) 'instructions instructions 'global global))]))

;; The thread goal: {goal, remaining_tokens, completion_budget_report}.
(define (goal-get)
  (%host-call "goal.get" (hash)))

;; Start a new active thread goal. #:check is a shell command that must exit
;; 0 before the goal completes; #:verify "judge" adds a read-only judge call;
;; #:max-cost-usd caps the goal's spend.
(define (goal-create objective
                     #:token-budget [token-budget #f]
                     #:check [check #f]
                     #:verify [verify #f]
                     #:max-cost-usd [max-cost-usd #f])
  (%host-call "goal.create"
              (%options (hash 'objective objective)
                        'token_budget (and token-budget (%integer "goal-create" "#:token-budget" token-budget))
                        'check check
                        'verify verify
                        'max_cost_usd max-cost-usd)))

;; Mark the thread goal achieved; on a checked goal `verification` says
;; whether the claim passed and what remains.
(define (goal-complete)
  (%host-call "goal.complete" (hash)))

;; Change this session's thinking level from the next model call ("off",
;; "minimal", "low", "medium", "high", "xhigh", "max"); the effective level.
;; Raises unless the user allowed it.
(define (model-set-effort level)
  (hash-ref (%host-call "model.set_effort" (hash 'level level)) 'thinkingLevel))

;; One paged-out output read, or an error saying why the handle has no
;; stored text.
(define (%ctx-read who type payload)
  (let ([reply (%host-call type payload)])
    (if (equal? (hash-ref reply 'state) "stored")
        reply
        (error (string-append who ": " (hash-ref reply 'handle) " is " (hash-ref reply 'state)
                              (if (string? (hash-try-get reply 'reason))
                                  (string-append " (" (hash-ref reply 'reason) ")")
                                  "")
                              "; re-run the command to see its output")))))

;; Lines #:start onward (0-based), at most #:lines, of a paged-out output:
;; {handle, start, end, storedLines, text, truncated, ...}. Read on from end.
;; #:tail n reads its last n lines instead; #:info #t its state and counts,
;; which never raises for a handle without stored text.
(define (output-read handle #:start [start #f] #:lines [lines #f] #:tail [tail #f] #:info [info #f])
  (cond
    [(and info (or start lines tail))
     (error "output-read: #:info takes no #:start, #:lines or #:tail")]
    [(and tail (or start lines))
     (error "output-read: #:tail takes no #:start or #:lines")]
    [info (%host-call "ctx.info" (hash 'handle handle))]
    [tail
     (%ctx-read "output-read" "ctx.tail" (hash 'handle handle 'n (%integer "output-read" "#:tail" tail)))]
    [else
     (%ctx-read "output-read"
                "ctx.page"
                (hash 'handle handle
                      'start (%integer "output-read" "#:start" (or start 0))
                      'lines (%integer "output-read" "#:lines" (or lines 200))))]))

;; Lines of a paged-out output matching the regex `pattern`, with #:context
;; lines around each: {matches, shown, text, truncated, ...}.
(define (output-grep handle pattern #:context [context 2] #:max-matches [max-matches 50])
  (%ctx-read "output-grep"
             "ctx.grep"
             (hash 'handle handle
                   'pattern pattern
                   'context (%integer "output-grep" "#:context" context)
                   'max_matches (%integer "output-grep" "#:max-matches" max-matches))))

;; Snapshot the workspace now: {id, label, created_at}.
(define (checkpoint-create [label ""])
  (hash-ref (%host-call "checkpoint.create" (hash 'label label)) 'checkpoint))

;; This session's checkpoints, oldest first.
(define (checkpoint-list)
  (hash-ref (%host-call "checkpoint.list" (hash)) 'checkpoints))

;; Put the workspace back to checkpoint `id` after the user confirms:
;; {status, restored} with status "restored", "declined", or "no_user".
;; Raises while the user has restoring turned off.
(define (checkpoint-restore id)
  (let ([reply (%host-call "checkpoint.restore" (hash 'id id 'question_id (%fresh-id)))])
    (if (equal? (hash-ref reply 'status) "disabled")
        (error (hash-ref reply 'message))
        reply)))
