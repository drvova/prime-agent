//! `(bash command (hash 'persistent #t))`: a shell that outlives its kernel.
//!
//! A persistent shell survives kernel, worker, and daemon restarts and its
//! session's delete or archive; only its own exit or an explicit kill ends
//! it. Every session's shells live in one home under the agent directory,
//! `<agent dir>/persistent-shells/` (the layout the daemon reads): per shell a
//! `<id>.json` record assigned to the session that started it, its output in
//! `<id>.out`, and its exit status in `<id>.rc`. It is not in the orphan
//! journal and holds no job slot.
//!
//! A kernel that starts re-attaches its session's live shells and lists once
//! each shell that finished while no kernel ran. A re-attached shell is not
//! this process's child, so its end is polled. A shell is only ever signalled
//! while its pid provably still names it: a recycled pid, or a process group
//! whose leader was not seen to exit, is left alone and its files are kept.
//! POSIX only.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;
use serde_json::json;

use super::job::{end_group, lock, Finished, Job, CHILD_ENV, HEAD_CAP, TAIL_CAP};
use super::Session;
use crate::kernel::orphan_journal::get_process_start_id;
use crate::platform;

/// The directory under the agent directory that holds every session's shells.
const HOME_DIR: &str = "persistent-shells";
/// How often a re-attached shell is checked for its end.
const POLL: Duration = Duration::from_secs(1);
/// Runs nothing until a line arrives on stdin (the shell is registered by
/// then), then runs the command in a shell of its own, so `exit`, `exec`, or
/// `set -e` in it cannot skip the atomic write of its status.
const WRAPPER: &str = "read -r _prime_agent_gate || exit 127\n\
                       exec 0</dev/null\n\
                       \"$1\" -c \"$2\"\n\
                       __prime_status=$?\n\
                       printf '%s\\n' \"$__prime_status\" > \"$3.tmp\" && mv -f \"$3.tmp\" \"$3\"\n";

/// One shell's record, as the daemon reads it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    id: String,
    pid: u32,
    start_id: String,
    command: String,
    started_at: String,
    session_id: String,
}

fn shell_file(home: &Path, id: &str, kind: &str) -> PathBuf {
    home.join(format!("{id}.{kind}"))
}

/// Remove a finished shell's files. The record goes last: until then the next
/// kernel still finds the shell and removes the rest.
fn remove_files(home: &Path, id: &str) {
    for kind in ["rc", "out", "json"] {
        // Already gone is the state this wants.
        let _ = std::fs::remove_file(shell_file(home, id, kind));
    }
}

/// What makes a job a persistent shell: where its files are and which
/// process it is.
pub(super) struct Lasting {
    /// The pid-reuse identity its record holds.
    start_id: String,
    home: PathBuf,
    /// Its whole output, held once it has finished so the file can go.
    kept: Mutex<Option<String>>,
}

impl Lasting {
    /// The shell's output file as the kernel shows it, and whether it lost
    /// its middle: its first [`HEAD_CAP`] and last [`TAIL_CAP`] bytes.
    fn read(&self, id: &str) -> std::io::Result<(String, bool)> {
        let mut file = match std::fs::File::open(shell_file(&self.home, id, "out")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((String::new(), false))
            }
            Err(error) => return Err(error),
        };
        let size = usize::try_from(file.metadata()?.len()).unwrap_or(usize::MAX);
        if size <= HEAD_CAP + TAIL_CAP {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            return Ok((String::from_utf8_lossy(&bytes).into_owned(), false));
        }
        let mut head = vec![0u8; HEAD_CAP];
        file.read_exact(&mut head)?;
        file.seek(SeekFrom::End(-(TAIL_CAP as i64)))?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail)?;
        Ok((
            format!(
                "{}\n... [{} bytes dropped] ...\n{}",
                String::from_utf8_lossy(&head),
                size - HEAD_CAP - TAIL_CAP,
                String::from_utf8_lossy(&tail)
            ),
            true,
        ))
    }

    /// Everything the shell has written so far.
    ///
    /// # Errors
    ///
    /// The output file exists and could not be read.
    pub(super) fn text(&self, id: &str) -> anyhow::Result<String> {
        if let Some(kept) = lock(&self.kept).as_ref() {
            return Ok(kept.clone());
        }
        let (text, _) = self
            .read(id)
            .context("could not read the persistent shell's output")?;
        Ok(text)
    }

    /// Whether `pid` still names this shell: true while it does, false once
    /// it is provably gone (dead, or recycled under another start identity),
    /// `None` when neither can be proven.
    pub(super) fn identity(&self, pid: u32) -> Option<bool> {
        // An unknown liveness reads as alive: a recycled group is never claimed.
        if !pa_types::platform::process::is_process_alive(pid).unwrap_or(true) {
            return Some(false);
        }
        get_process_start_id(pid as i32).map(|current| current == self.start_id)
    }

    /// Kill what is left of the shell's group, without ever signalling a
    /// live recycled or unverified pid. True when nothing of it remains.
    pub(super) fn reap_group(&self, pid: u32, leader_exit_witnessed: bool) -> bool {
        if !platform::pid_exists(pid) {
            if leader_exit_witnessed {
                return end_group(pid);
            }
            // A durable record can outlive its group: one that exists without
            // the recorded leader may be a reused id, so it is kept, not killed.
            return matches!(
                platform::process::process_group_exists(pid as i32),
                Ok(false)
            );
        }
        // A zombie can still own an unrelated group: it passes the same
        // identity check as a live pid.
        match get_process_start_id(pid as i32) {
            Some(current) if current == self.start_id => end_group(pid),
            Some(_) => true,
            None => false,
        }
    }
}

/// Start `command` as a persistent shell assigned to the session.
///
/// # Errors
///
/// Not POSIX, no saved session to assign it to, no usable shell or working
/// directory, a failed spawn, or a record that could not be written (the
/// shell then exits without running anything).
pub(super) fn spawn(session: &Arc<Session>, command: &str) -> anyhow::Result<Arc<Job>> {
    if !cfg!(unix) {
        bail!("persistent shells are only supported on POSIX");
    }
    let settings = &session.host.settings;
    let (Some(session_id), Some(agent_dir)) = (&settings.session_id, &settings.agent_dir) else {
        bail!(
            "a persistent shell needs a saved session: the shell is assigned to the session that started it"
        );
    };
    if lock(&session.table).closed {
        bail!("the kernel was shut down");
    }
    if !session.cwd.is_dir() {
        bail!(
            "the working directory does not exist: {}",
            session.cwd.display()
        );
    }
    let shell = platform::resolve_kernel_bash_shell(settings.shell.as_deref())?
        .ok_or_else(|| anyhow!("no POSIX shell was found"))?;
    let command_line = match &settings.command_prefix {
        Some(prefix) => format!("{prefix}\n{command}"),
        None => command.to_string(),
    };
    let home = agent_dir.join(HOME_DIR);
    platform::perms::create_dir_all_private(&home)
        .context("could not create the persistent shells directory")?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mut private = std::fs::OpenOptions::new();
    private.write(true).create_new(true);
    platform::set_private_mode(&mut private);
    let output = private
        .open(shell_file(&home, &id, "out"))
        .context("could not create the shell's output file")?;
    let (gate_end, mut gate) = std::io::pipe().context("could not open the start gate")?;
    let mut process = pa_types::session_scope::tokio_command(&shell);
    process
        .arg("-c")
        .arg(WRAPPER)
        .arg("prime-agent-persistent")
        .arg(&shell)
        .arg(format!(
            "{{\n{command_line}\n}}\n__prime_status=$?\nwait\nexit \"$__prime_status\"\n"
        ))
        .arg(shell_file(&home, &id, "rc"))
        .current_dir(&session.cwd)
        .envs(settings.env.iter().map(|(name, value)| (name, value)))
        .envs(CHILD_ENV)
        .stdin(gate_end)
        .stdout(
            output
                .try_clone()
                .context("could not open the shell's output file")?,
        )
        .stderr(output);
    #[cfg(unix)]
    platform::process::set_new_session(process.as_std_mut());
    let context = session.runtime.enter();
    let child = process
        .spawn()
        .with_context(|| format!("could not start {shell}"))?;
    drop(context);
    drop(process);
    // The wrapper runs nothing until its gate's line arrives: every failure
    // from here closes the gate instead, and the shell exits.
    let pid = child
        .id()
        .ok_or_else(|| anyhow!("the shell ended before it had a pid"))?;
    let start_id = get_process_start_id(pid as i32)
        .ok_or_else(|| anyhow!("the shell's start identity is unreadable"))?;
    let job = Job::new(
        session,
        command,
        pid,
        id.clone(),
        Some(Lasting {
            start_id: start_id.clone(),
            home: home.clone(),
            kept: Mutex::new(None),
        }),
    );
    let record = json!({
        "id": id,
        "pid": pid,
        "startId": start_id,
        "command": command,
        "startedAt": job.started_at,
        "sessionId": session_id,
    });
    let staged = shell_file(&home, &id, "json.tmp");
    private
        .open(&staged)
        .and_then(|mut file| file.write_all(record.to_string().as_bytes()))
        .and_then(|()| platform::rename_onto(&staged, &shell_file(&home, &id, "json")))
        .context("could not record the persistent shell")?;
    gate.write_all(b"\n")
        .context("the persistent shell ended before it started")?;
    drop(gate);
    let job = Arc::new(job);
    {
        let mut table = lock(&session.table);
        table.jobs.insert(job.id.clone(), Arc::clone(&job));
        table.order.push(job.id.clone());
    }
    session.runtime.spawn(watch(
        Arc::clone(session),
        Arc::clone(&job),
        Some(child),
        /*running*/ true,
    ));
    Ok(job)
}

/// Kernel start: re-attach the session's live shells, list the ones that
/// finished while no kernel ran, and remove its dead ones. A record whose
/// identity cannot be proven, and every other session's records, stay
/// untouched. Returns the jobs it added.
pub(super) fn rehydrate(session: &Arc<Session>) -> Vec<Arc<Job>> {
    let settings = &session.host.settings;
    let (Some(session_id), Some(agent_dir), true) =
        (&settings.session_id, &settings.agent_dir, cfg!(unix))
    else {
        return Vec::new();
    };
    let home = agent_dir.join(HOME_DIR);
    // No home yet: no session has started a persistent shell.
    let Ok(entries) = std::fs::read_dir(&home) else {
        return Vec::new();
    };
    // A record that vanished or does not parse belongs to a shell another
    // kernel is finishing, or to no shell at all.
    let mut records: Vec<Record> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| std::fs::read(path).ok())
        .filter_map(|raw| serde_json::from_slice::<Record>(&raw).ok())
        .filter(|record| record.session_id == *session_id)
        .collect();
    records.sort_by(|first, second| first.started_at.cmp(&second.started_at));
    let mut attached = Vec::new();
    for record in records {
        let lasting = Lasting {
            start_id: record.start_id,
            home: home.clone(),
            kept: Mutex::new(None),
        };
        let Some(running) = lasting.identity(record.pid) else {
            continue;
        };
        if running || shell_file(&home, &record.id, "rc").exists() {
            let mut job = Job::new(
                session,
                &record.command,
                record.pid,
                record.id,
                Some(lasting),
            );
            let age = crate::cron::parse_iso_millis(&record.started_at)
                .map(|started| crate::autonomous::now_millis().saturating_sub(started))
                .unwrap_or_default();
            job.started = Instant::now()
                .checked_sub(Duration::from_millis(age))
                .unwrap_or(job.started);
            job.started_at = record.started_at;
            let job = Arc::new(job);
            {
                let mut table = lock(&session.table);
                table.jobs.insert(job.id.clone(), Arc::clone(&job));
                table.order.push(job.id.clone());
            }
            session.runtime.spawn(watch(
                Arc::clone(session),
                Arc::clone(&job),
                /*child*/ None,
                running,
            ));
            attached.push(job);
        } else if lasting.reap_group(record.pid, /*leader_exit_witnessed*/ false) {
            remove_files(&home, &record.id);
        }
    }
    attached
}

/// Watch one persistent shell to its end and settle its result from its
/// files. `running` false means it finished while no kernel ran: its pid may
/// name another process by now, so it is never polled or signalled.
async fn watch(
    session: Arc<Session>,
    job: Arc<Job>,
    child: Option<tokio::process::Child>,
    running: bool,
) {
    let Some(lasting) = &job.lasting else {
        return;
    };
    let status_file = shell_file(&lasting.home, &job.id, "rc");
    let leader_exit_witnessed = child.is_some() || running;
    if let Some(mut child) = child {
        // Only its end matters: the status is read from the shell's own file.
        let _ = child.wait().await;
    } else if running {
        while !status_file.exists() && lasting.identity(job.pid) != Some(false) {
            tokio::time::sleep(POLL).await;
        }
    }
    let mut failure = None;
    let exit_code = match std::fs::read_to_string(&status_file) {
        Ok(text) => text.trim().parse().unwrap_or_else(|_| {
            failure = Some(format!("its exit status is unreadable: {text:?}"));
            -1
        }),
        // Killed before the wrapper recorded a status.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => -1,
        Err(error) => {
            failure = Some(format!("its exit status could not be read: {error}"));
            -1
        }
    };
    let reaped = lasting.reap_group(job.pid, leader_exit_witnessed);
    let (output, truncated) = lasting.read(&job.id).unwrap_or_else(|error| {
        failure = Some(format!("its output could not be read: {error}"));
        (String::new(), false)
    });
    *lock(&lasting.kept) = Some(output.clone());
    // Unread files stay recoverable, and so do those of a group that could
    // not be verified gone.
    if failure.is_none() && reaped {
        remove_files(&lasting.home, &job.id);
    }
    job.update(|state| {
        state.reaped = true;
        state.failure = failure;
        state.finished = Some(Finished {
            exit_code,
            output,
            truncated,
            duration: job.started.elapsed(),
            handle: None,
        });
    });
    lock(&session.table).retire(&job);
}
