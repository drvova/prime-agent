//! The runtime's task lanes (schema migration 4): every task a session's
//! lane queued, in the state it last saved, so queued work survives a
//! restart.
//!
//! A task's first save fixes its place in insertion order; a later save of
//! the same id replaces everything else about it. What the states mean, and
//! what a restart does with a running task, stay with `pa-runtime`.

use anyhow::{Context, Result};
use pa_types::event_log::{EventId, Task, TaskState};
use rusqlite::{params, Row};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::{now_ms, Log, Receipt};

/// The word a unit enum serializes to (`queued`, `prompt`, `owner`).
fn word<T: Serialize>(value: T) -> Result<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(word) => Ok(word),
        other => anyhow::bail!("{other} is not a word"),
    }
}

/// The unit enum `word` names.
fn parse<T: DeserializeOwned>(word: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(word.to_string()))
        .with_context(|| format!("a stored task holds {word:?}"))
}

fn decode(row: &Row<'_>) -> rusqlite::Result<Result<Task>> {
    let id: String = row.get("id")?;
    let session: String = row.get("session")?;
    let priority: String = row.get("priority")?;
    let state: String = row.get("state")?;
    let trigger: String = row.get("trigger")?;
    let prompt: String = row.get("prompt")?;
    let trust: String = row.get("trust")?;
    let attempts: i64 = row.get("attempts")?;
    let error: Option<String> = row.get("error")?;
    Ok((|| -> Result<Task> {
        Ok(Task {
            id,
            session,
            priority: parse(&priority)?,
            state: parse(&state)?,
            trigger: EventId(trigger),
            prompt,
            trust: parse(&trust)?,
            attempts: u32::try_from(attempts)?,
            error,
        })
    })())
}

impl Log {
    /// Store `task`, replacing the one with its id; the first save of an id
    /// fixes its place in [`Log::unfinished_tasks`]'s order.
    pub fn save_task(&self, task: Task) -> Receipt<()> {
        let words = (|| -> Result<[String; 3]> {
            Ok([word(task.priority)?, word(task.state)?, word(task.trust)?])
        })();
        let [priority, state, trust] = match words {
            Ok(words) => words,
            Err(error) => return Receipt::ready(Err(error)),
        };
        self.writer.submit(move |connection| {
            let now = i64::try_from(now_ms())?;
            connection
                .prepare_cached(
                    "INSERT INTO tasks (id, session, priority, state, trigger, prompt, trust, \
                     attempts, error, created, updated) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10) \
                     ON CONFLICT (id) DO UPDATE SET session = excluded.session, \
                     priority = excluded.priority, state = excluded.state, \
                     trigger = excluded.trigger, prompt = excluded.prompt, \
                     trust = excluded.trust, attempts = excluded.attempts, \
                     error = excluded.error, updated = excluded.updated",
                )?
                .execute(params![
                    task.id,
                    task.session,
                    priority,
                    state,
                    task.trigger.0,
                    task.prompt,
                    trust,
                    task.attempts,
                    task.error,
                    now,
                ])
                .context("store the task")?;
            Ok(())
        })
    }

    /// Every queued and running task, in the order their ids were first
    /// saved.
    ///
    /// # Errors
    ///
    /// The database could not be read, or a stored row does not decode.
    pub fn unfinished_tasks(&self) -> Result<Vec<Task>> {
        let unfinished = [word(TaskState::Queued)?, word(TaskState::Running)?];
        self.read(|connection| {
            connection
                .prepare_cached(
                    "SELECT id, session, priority, state, trigger, prompt, trust, attempts, error \
                     FROM tasks WHERE state IN (?1, ?2) ORDER BY seq",
                )?
                .query_map(params![unfinished[0], unfinished[1]], decode)?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .collect()
        })
    }
}
