# pa-sources

Prime Agent's event sources: timers, folder watches and webhooks. A source turns something
outside a session (a time, a file change, an HTTP request) into an event on the log.

It is being built beside today's scheduler as part of the move onto one event log. `pa-runtime`
starts a timer or watch at its first subscription and stops it at its last, keeping that count
itself (`pa_runtime::Runtime::define_source`); webhooks are not started until the daemon hosts
them.

## Scope

- **The contract.** A source publishes through a `Publisher` the runtime implements: a
  synchronous hand-off that never blocks, so it works from Tokio tasks and from a file watcher's
  own thread. `Source::start` validates the options and installs the listener (the watch, the
  bound socket, the first deadline) before it returns, so nothing after the return is missed.
  The `SourceHandle` it returns stops the source promptly, explicitly (`stop`, which reports how
  its work ended) or on drop. Every wait observes the handle's cancellation, and nothing a source
  starts outlives its handle.
- **Options.** Each kind has typed serde options that refuse unknown fields, and a JSON Schema of
  them (`options_schema`) for the runtime's validation and docs. `SourceKind` starts a kind from
  JSON options.
- **`schedule`** is the one schedule grammar and evaluator, shared with pa-core's scheduled
  jobs. A `Schedule` fires every interval, on a cron expression, or once at a time, and answers
  `next_after` and `latest_due`. Durations are `<count><unit>` parts (`30s`, `1h30m`,
  `2 hours`); cron is five fields or `@hourly`-style, read in UTC, a minute firing when every
  field matches, day-of-month and weekday included; times are ISO (`parse_instant`). A scheduled
  job's sentence (`Schedule::parse`: `in 10m`, `every 5m`, `at <ISO time>`, cron) reads the same
  forms. The shortest interval is the caller's: a timer takes 1 s, the scheduled-jobs store 10 s.
- **`timer`** publishes `timer/<name>` with `{ "scheduledFor", "firedAt" }` (Unix ms), with Owner
  trust. It takes exactly one of `every` (`30s`, `1h30m`), `cron` (five fields or `@daily`-style)
  and `at` (an ISO time), read by `schedule`; `tz` accepts only `"UTC"`. Catch-up is the
  scheduled-jobs store's rule: a start resumes from the `firedAt` of the last fire the runtime
  passes in (`StartContext::last_fired_ms`). Missed fires collapse into one: the timer fires once,
  at once, with `scheduledFor` the latest missed time, and every next fire is counted from when
  the last one fired. The time comes from a `Clock`, so tests move it by hand.
- **`watch`** publishes `source/<name>/changed` with `{ "paths", "kinds" }`, built on `notify`.
  Changes are coalesced over a window that the first change opens (`debounce_ms`, 200 by default).
  It takes an optional `glob`: without `/` the glob matches a file name at any depth, and with `/`
  it matches the path relative to the folder. The watch is recursive unless the options say
  otherwise. Reads are not reported. A platform watcher error goes to `source/<name>/error`, and
  the watch continues. Trust comes from the options, Owner by default.
- **`webhook`** is a hyper HTTP/1 listener. It publishes each authenticated `POST` to its path on
  `source/<name>/received`, always with Untrusted trust. A JSON body is published as
  `{ "body", "headers" }`, the headers being only an allowlist of descriptive ones; any other body
  as `{ "body": <text> }`.
  - **Listening.** It binds `127.0.0.1:0` by default; any other address needs
    `allow_remote: true`.
  - **Auth.** A bearer `token`, an HMAC-SHA256 `hmac_secret` (`X-Signature-256: sha256=<hex>`),
    or both are required, and every one configured must pass.
  - **Limits.** A body over `max_body_bytes` (1 MiB by default) gets 413, refused from its
    declared length before it is read. At most 64 connections are served at once. Headers have a
    10 s timeout, and the body 30 s.
  - **Address.** The handle exposes the bound address.

## Non-goals

- Reference counting, persistence of the last fire, and the subscriptions that start a source:
  the runtime owns all three.
- Where a webhook's token comes from (`auth.json`), and adoption telemetry: the runtime wires
  both when it starts sources.
- Importing today's scheduled jobs and heartbeats into timers.
- Time zones other than UTC.

## Public API

- `Publisher`, `PublisherClosed`, `Source`, `StartContext`, `SourceKind`, `SourceHandle`
  (`stop`, `local_addr`).
- `schedule`: `Schedule` (`parse`, `every`, `at`, `next_after`, `latest_due`), `ParsedSchedule`,
  `Cron` (`parse`, `expression`), `parse_duration` with `DurationUnits`, `parse_instant`,
  `ScheduleError`.
- `timer`: `TimerSource` (`new`, `with_clock`), `TimerOptions`, `Clock`, `SystemClock`.
- `watch`: `WatchSource`, `WatchOptions`.
- `webhook`: `WebhookSource`, `WebhookOptions`, `Secret`, and the limits `REPORTED_HEADERS`,
  `MAX_CONNECTIONS`, `HEADER_TIMEOUT` and `BODY_TIMEOUT`.

Dependencies: `pa-types` only, among workspace crates. Dependents: `pa-core` (its scheduled jobs
read their schedules with `schedule`).
