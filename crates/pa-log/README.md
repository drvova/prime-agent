# pa-log

Prime Agent's SQLite store: the event log, the session transcripts, the runtime's task lanes and
the agent's other state (schedules, the event bus, the harness store, the crash log), in one
database file.
It is the only crate that links SQLite (`rusqlite`, built from its bundled source).

It is being built beside today's file stores as part of the move onto one event log. The
transcript store in `pa-core` (`session::store`, its SQLite store) keeps every session transcript
in it. `pa-runtime` keeps its events, task lanes and state in it (`pa_runtime::LogEventStore`,
`LogTaskStore`, `LogStateStore`).

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
  - `events_since` and `events_before` read every event above a sequence number (oldest first)
    or below one (newest first), as `pa-runtime`'s `EventStore` does; one scan serves all three.
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

- **Transcripts by locator, byte for byte** (the transcript store's tables, schema migration 2).
  A transcript is the exact bytes of a JSONL session file, keyed by its canonical locator (the
  `<sessions dir>/<id>.jsonl` path the caller resolves) and stored as its lines, each with its
  newline and the byte offset it starts at, so whatever the bytes hold (a torn last write, a
  line that is not JSON or not UTF-8) comes back unchanged.
  - `put_transcript` stores a whole transcript, replacing one (`PutMode::Replace`, a new row id)
    or only where none is (`PutMode::IfAbsent`, the import); `append_transcript` appends,
    ending a torn last line first (`AppendMode::NewLine`) or finishing it (`AppendMode::AsIs`).
  - `transcript_bytes` reads from any byte position: the offset index finds the line holding it,
    so a read of a long transcript's tail never reads its start.
  - A transcript's row id is its identity: appends keep it, every replace takes a new one, and
    ids are never reused. With `store_id` (a random number minted with the database) and the
    length, it names the bytes exactly: the store's generations are built from them.
  - `move_transcript`, `remove_transcript`, `transcript`, `transcript_by_id`, `transcripts_in`,
    `all_transcripts`; `meta`/`set_meta` keep the store's own facts (the import markers).

- **The agent's other state, by partition** (schema migration 3). A partition is named by a
  scope the caller chooses (one per file it replaces) and counts once the `partitions` table
  holds it; from then on its rows are the truth.
  - `sources` (timer, bus, watch and webhook sources, with a timer's claimed run) and
    `subscriptions`: `replace_sources` stores a partition's whole `SourceSet` in order, `sources`
    reads it. A kind is stored as text in a column with no constraint, so the watch and webhook
    kinds (`pa-runtime`'s sources) came with no migration; a build that does not know a kind
    refuses only the partition that holds it.
  - `kv`: `put_value` and `value`, one value per key in a partition.
  - `crashes`: `append_crashes` adds records in order, `crashes` reads them; records are never
    rewritten.
  - `Origin::Written` always stores and holds the partition; `Origin::Imported` (the file and
    its SHA-256) stores only while the partition is not held, so a second import or one racing
    a write changes nothing. Each write is one transaction: a partition is never half-imported.
  - `partition` and `partitions` (by scope prefix) say what is held and where it came from.

- **Task lanes** (schema migration 4, `pa_types::event_log::Task`). `save_task` upserts a task
  by id; its first save fixes its place in insertion order (`seq`), and `created`/`updated` record
  its first and last save. `unfinished_tasks(session)` returns one session's `queued` and
  `running` tasks in that order, which is what `pa-runtime`'s `TaskQueue::restore` replays when
  that session opens (another process may be running the database's other sessions).

## Non-goals

- What the lines mean: entry types, the branch tree, compaction. Those stay with the session
  code that reads them.
- Which events are stored and which are live-only, and what a subscription does with an event.
  Those belong to the runtime.
- What a task's states mean, and what a restart does with a running one: the runtime's.
- What a partition's rows mean (a job, a schedule, a harness entry), and which file a partition
  replaces. Those stay with the code that owns each store.
- Settings, auth, models, skills, the spill store and checkpoints, which stay files.

## Public API

- `Log::open` and `Log::path`.
- Events: `append_event`, `events`, `events_since`, `events_before`, `prune_events`; `EventRetention`,
  `DEFAULT_EVENT_RETENTION`.
- Sessions: `import_session_file`, `export_session`, `append_entry`, `sessions`, `entries`;
  `ImportOutcome`, `SessionRow`, `EntryRow`.
- Transcripts: `put_transcript`, `append_transcript`, `transcript_bytes`, `move_transcript`,
  `remove_transcript`, `transcript`, `transcript_by_id`, `transcripts_in`, `all_transcripts`,
  `store_id`, `meta`, `set_meta`; `TranscriptRow`, `TranscriptChunk`, `NewTranscript`,
  `PutMode`, `AppendMode`, `MoveOutcome`.
- Partitions: `partition`, `partitions`, `sources`, `replace_sources`, `value`, `put_value`,
  `crashes`, `append_crashes`; `Partition`, `Origin`, `SourceSet`, `SourceRow`, `SourceKind`,
  `SubscriptionRow`, `CrashRow`.
- Tasks: `save_task`, `unfinished_tasks`.
- `Receipt`: `wait` and `wait_blocking` (which parks the thread, and so may run on a runtime's
  thread, as a blocking file write would).

Dependencies: `pa-types` only, among workspace crates.
