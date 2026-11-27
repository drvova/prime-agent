# pa-log

Prime Agent's SQLite store: the event log and the session transcripts, in one database file.
It is the only crate that links SQLite (`rusqlite`, built from its bundled source).

It is being built beside today's file stores as part of the move onto one event log. Nothing
reads from it or writes to it yet: the storage cutover moves the transcript writers and readers
onto it, together with the first-start import.

## Scope

- **One writer.** A dedicated thread owns the read-write connection. It runs every change
  submitted to it, committing whatever is queued (up to 256 changes) in one transaction. Each
  change runs inside its own savepoint, so a failing change undoes only itself. A `Receipt`
  answers each change once its transaction commits. Nothing that paints may wait on a receipt.
- **Pooled reads.** Reads take a read-only connection from a pool. The database runs in WAL
  mode, so a read never waits for the writer.
- **Durability.** `synchronous=NORMAL`: a power loss can drop the last commits, but never leaves
  the file inconsistent.
- **Schema.** Numbered migrations, recorded in `PRAGMA user_version` and applied at open
  (`schema.rs`). A database a newer build migrated further is refused.
- **Event log** (`pa_types::event_log`):
  - `append_event` gives an event its id, sequence number and time, and sets when its retention
    ends (`EventRetention`, 30 days by default).
  - `events` reads by topic filter: after a sequence number, or the last few, oldest first. The
    filter's literal prefix narrows the scan in SQL, and `TopicFilter::matches` decides.
  - `prune_events` deletes expired events.
- **Session transcripts, line for line.** A JSONL session file is stored as its lines. Each is
  indexed by the `type`, `id` and `parentId` it names; a line that is not a JSON object, such as
  a torn last write, is kept as it is.
  - `import_session_file` is idempotent:
    - a new file is imported;
    - an unchanged one is left alone;
    - one that changed since its import replaces its own lines;
    - a file that is not UTF-8, has no session header, or holds a session id already stored
      from another file is skipped, whole, with the reason.
  - `export_session` writes the same bytes back.
  - `append_entry` adds a line.
  - `sessions` and `entries` read them.

## Non-goals

- What the lines mean: entry types, the branch tree, compaction. Those stay with the session
  code that reads them.
- Which events are stored and which are live-only, and what a subscription does with an event.
  Those belong to the runtime.
- Settings, auth, models, skills, the spill store and checkpoints, which stay files.

## Public API

- `Log::open` and `Log::path`.
- Events: `append_event`, `events`, `prune_events`; `EventRetention`,
  `DEFAULT_EVENT_RETENTION`.
- Sessions: `import_session_file`, `export_session`, `append_entry`, `sessions`, `entries`;
  `ImportOutcome`, `SessionRow`, `EntryRow`.
- `Receipt`: `wait` and `wait_blocking`.

Dependencies: `pa-types` only, among workspace crates.
