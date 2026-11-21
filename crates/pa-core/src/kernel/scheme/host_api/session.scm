;; The session's own state: context, harness refinement, the goal, the
;; model, settings, usage, paged-out output, checkpoints, and schedules.

;; Context usage: {tokens, context_window, percent, scheduled}.
(define (compact-status)
  (%host-call "compact.status" (hash)))

;; Schedule context compaction for when the turn ends: {scheduled: #t}, or
;; {scheduled: #f, reason}. `instructions` focus the summary.
(define (compact-run [instructions #f])
  (%host-call "compact.run" (%options (hash) 'instructions instructions)))

;; Refinement state: {pending, in_flight}.
(define (refine-status)
  (%host-call "refine.status" (hash)))

;; Schedule harness refinement for when the turn ends: {scheduled: #t}, or
;; {scheduled: #f, reason}. #:global #t targets the cross-session store.
(define (refine-run [instructions #f] #:global [global #f])
  (%host-call "refine.run" (%options (hash) 'instructions instructions 'global global)))

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

;; The session model: {id, provider, input, contextWindow, thinkingLevel}.
(define (model-info)
  (%host-call "model.info" (hash)))

;; Change this session's thinking level from the next model call ("off",
;; "minimal", "low", "medium", "high", "xhigh", "max"); the effective level.
;; Raises unless the user allowed it.
(define (model-set-effort level)
  (hash-ref (%host-call "model.set_effort" (hash 'level level)) 'thinkingLevel))

;; The session's effective settings: {session, host}, each as the host
;; reports it.
(define (settings-get)
  (hash 'session (%host-call "settings.session" (hash))
        'host (%host-call "settings.host" (hash))))

;; This session's cost and tokens and the provider quota windows:
;; {session, quotas}. #:refresh #t re-reads every provider's quota now.
(define (usage-get #:refresh [refresh #f])
  (hash 'session (%host-call "usage.session" (hash))
        'quotas (%host-call "usage.quotas" (hash 'refresh refresh))))

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
(define (ctx-page handle #:start [start 0] #:lines [lines 200])
  (%ctx-read "ctx-page"
             "ctx.page"
             (hash 'handle handle
                   'start (%integer "ctx-page" "#:start" start)
                   'lines (%integer "ctx-page" "#:lines" lines))))

;; The last `n` lines of a paged-out output.
(define (ctx-tail handle [n 200])
  (%ctx-read "ctx-tail" "ctx.tail" (hash 'handle handle 'n (%integer "ctx-tail" "n" n))))

;; Lines of a paged-out output matching the regex `pattern`, with #:context
;; lines around each: {matches, shown, text, truncated, ...}.
(define (ctx-grep handle pattern #:context [context 2] #:max-matches [max-matches 50])
  (%ctx-read "ctx-grep"
             "ctx.grep"
             (hash 'handle handle
                   'pattern pattern
                   'context (%integer "ctx-grep" "#:context" context)
                   'max_matches (%integer "ctx-grep" "#:max-matches" max-matches))))

;; A handle's state and counts; never raises for a handle without stored
;; text.
(define (ctx-info handle)
  (%host-call "ctx.info" (hash 'handle handle)))

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

;; Schedule `prompt` to run in this session later; the job. Pass exactly one
;; of #:at (an ISO-8601 time, "in 30m", or a delay in seconds), #:every
;; ("30m", "2h", or seconds), or #:cron (five fields, UTC). #:label names it
;; in the list.
(define (schedule-add prompt #:at [at #f] #:every [every #f] #:cron [cron #f] #:label [label #f])
  (hash-ref (%host-call "schedule.add"
                        (%options (hash 'prompt prompt)
                                  'at (cond
                                        [(not (number? at)) at]
                                        [(> at 0) (%utc-after (exact->inexact at))]
                                        [else (error "schedule-add: a numeric #:at is a delay in seconds and must be positive")])
                                  'every (if (number? every)
                                             (string-append (number->string (exact (round every))) "s")
                                             every)
                                  'cron cron
                                  'label label))
            'job))

;; This session's scheduled jobs, soonest first; #:include-inactive #t adds
;; finished and cancelled ones.
(define (schedule-list #:include-inactive [include-inactive #f])
  (hash-ref (%host-call "schedule.list" (hash 'include_inactive include-inactive)) 'jobs))

;; Cancel one of this session's jobs by id; the cancelled job.
(define (schedule-cancel id)
  (hash-ref (%host-call "schedule.cancel" (hash 'id id)) 'job))

;; This session's internal recurring heartbeats.
(define (rlm-heartbeat-list #:include-inactive [include-inactive #f])
  (%host-call "rlm_heartbeat.list" (hash 'include_inactive include-inactive)))

;; Create a recurring heartbeat that prompts this session with
;; `instruction`. #:delivery-mode is "steer" (interrupt a busy turn, the
;; default) or "follow_up" (wait for it).
(define (rlm-heartbeat-create instruction
                              #:interval [interval #f]
                              #:label [label #f]
                              #:delivery-mode [delivery-mode #f])
  (%host-call "rlm_heartbeat.create"
              (%options (hash 'instruction instruction)
                        'interval interval
                        'label label
                        'delivery_mode delivery-mode)))

;; Update one heartbeat; #:status is "pause" or "resume".
(define (rlm-heartbeat-update id
                              #:instruction [instruction #f]
                              #:interval [interval #f]
                              #:label [label #f]
                              #:status [status #f]
                              #:delivery-mode [delivery-mode #f])
  (%host-call "rlm_heartbeat.update"
              (%options (hash 'id id)
                        'instruction instruction
                        'interval interval
                        'label label
                        'status status
                        'delivery_mode delivery-mode)))

;; Cancel one heartbeat.
(define (rlm-heartbeat-delete id)
  (%host-call "rlm_heartbeat.delete" (hash 'id id)))
