# schedule: scheduled jobs and heartbeats

Scheduled jobs run a prompt in this session later ("check CI in 30 minutes", "rerun the benchmark nightly"). They persist with the session and the user sees them in `prime-agent schedule list`; you list and cancel only the jobs you created. At most 10 live jobs per session; recurring jobs fire at most every 5 minutes. Without the prime-agent daemon (headless runs) the calls raise and nothing is scheduled.

- `(schedule-add prompt #:at #:every #:cron #:label)`: schedule `prompt` to run as a new turn; the job. Pass exactly one of `#:at` (one shot: an ISO-8601 time, `"in 30m"`, or a positive delay in seconds), `#:every` (`"30m"`, `"2h"`, `"1d"`, or seconds), or `#:cron` (a five-field UTC expression or `@daily`). `#:label` names it in the list. A job is `{id, prompt, kind, schedule, status, next_run_at, last_run_at, run_count, label, last_error, created_at}`; `kind` is `"once"`, `"interval"`, or `"cron"`
- `(schedule-list #:include-inactive)`: this session's jobs, soonest first; `#:include-inactive #t` adds finished and cancelled ones
- `(schedule-cancel id)`: cancel one of your jobs; the cancelled job

Heartbeats are internal recurring prompts of this session, separate from the user's `/heartbeat` (which these functions cannot read or change). Use them for your own recurring checks and long-running coordination; keep each instruction specific. Several may run at once. They never fire in a session without a daemon.

- `(rlm-heartbeat-create instruction #:interval #:label #:delivery-mode)`: create a recurring heartbeat that prompts this session with `instruction`. `#:interval` is a schedule string such as `"5m"` (default: every 5 minutes). `#:delivery-mode` is `"steer"` (the default: interrupt a busy turn) or `"follow_up"` (wait for the turn to finish)
- `(rlm-heartbeat-list #:include-inactive)`: this session's heartbeats, active and paused ones by default
- `(rlm-heartbeat-update id #:instruction #:interval #:label #:status #:delivery-mode)`: change one heartbeat; `#:status` is `"pause"` or `"resume"`
- `(rlm-heartbeat-delete id)`: cancel one heartbeat
