## Scheduled jobs

Schedule a prompt to run in this session later ("check CI in 30 minutes", "rerun the benchmark nightly"). Jobs persist with the session and the user sees them in `prime-agent schedule list`; you list and cancel only the jobs you created. At most 10 live jobs per session; recurring jobs fire at most every 5 minutes. Without the prime-agent daemon (headless runs) the calls raise `RuntimeError` and nothing is scheduled.

- `schedule.add(prompt: str, *, at: str | datetime | timedelta | float | None = None, every: str | timedelta | float | None = None, cron: str | None = None, label: str | None = None) -> ScheduledJob`: pass exactly one of `at` (one shot: a datetime, a delay in seconds or `timedelta`, an ISO-8601 string, or `"in 30m"`), `every` (`"30m"`, `"2h"`, `"1d"`), or `cron` (five-field UTC expression or `@daily`); the job runs `prompt` as a new turn when it fires
- `schedule.list(*, include_inactive: bool = False) -> list[ScheduledJob]`: soonest first; `ScheduledJob(id, prompt, kind, schedule, status, next_run_at, last_run_at, run_count, label, last_error, created_at)`, `kind` is `once`/`interval`/`cron`
- `schedule.cancel(id: str) -> ScheduledJob`: cancel one of your jobs
