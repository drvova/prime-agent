//! One script on one shell: the thread and runtime it runs on, what the
//! shell is made of, and how it ends.
//!
//! The interpreter does blocking reads and writes on the threads of its
//! runtime (a builtin reading a pipe another stage of the pipeline writes),
//! so every script has a small multi-threaded runtime of its own. A stopped
//! script's runtime is abandoned rather than joined: a thread that never
//! returns from a read holds nothing but its own pipe.

use std::collections::HashMap;
use std::io::Write as _;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use brush_core::extensions::{ErrorFormatter, ShellExtensionsImpl};
use brush_core::openfiles::OpenFile;
use brush_core::{ErrorKind, Shell, ShellVariable};

use crate::{
    Children, Counts, Ended, Running, Script, Stop, INTERNAL_ERROR_EXIT_CODE, STACK_BYTES,
    STOPPED_EXIT_CODE,
};

/// The shell's worker threads: enough for a blocking reader, the stage that
/// feeds it, and two process substitutions beside them.
const WORKERS: usize = 2;
/// How deep shell functions may call each other before the call fails. The
/// interpreter recurses on its thread's stack with every call, and a stack
/// overflow would end the whole process, not the script.
const FUNCTION_DEPTH: usize = 512;
/// The longest script the shell takes.
const SOURCE_BYTES: usize = 1024 * 1024;
/// How deeply a script's brackets may nest: the parser recurses with them.
const NESTING: usize = 3000;
/// What one expansion may cost the process the shell shares: a refused
/// allocation is not a panic, so it ends the process and every session in
/// it. A command past one of these fails; the script goes on.
const LIMITS: brush_core::host::Limits = brush_core::host::Limits {
    // `{1..1000000}`: about 7 MiB of words.
    brace_words: 1_000_000,
    // The longest value a variable is given by one assignment, and the
    // longest argument a command is passed.
    word_bytes: 32 * 1024 * 1024,
    substitution_bytes: 32 * 1024 * 1024,
};

/// The shell's extensions: errors worded as a shell words them.
pub(crate) type Extensions = ShellExtensionsImpl<ShellErrors>;

#[derive(Clone, Default)]
pub(crate) struct ShellErrors;

impl ErrorFormatter for ShellErrors {
    fn format_error(
        &self,
        error: &brush_core::Error,
        shell: &Shell<impl brush_core::ShellExtensions>,
    ) -> String {
        if let ErrorKind::CommandNotFound(name) = error.kind() {
            let line = shell.env_str("LINENO").unwrap_or_default();
            return format!("bash: line {line}: {name}: command not found\n");
        }
        if shell.host().is_some_and(|host| host.interrupted()) {
            // A stopped script says nothing more: what fails from here on
            // fails because it was stopped.
            return String::new();
        }
        format!("bash: {error:#}\n")
    }
}

/// One script's hold on its shell and every subshell of it.
pub(crate) struct Embedding {
    pub(crate) stop: Stop,
    /// What the script sees as the shell's process id.
    pub(crate) id: u32,
    children: Arc<dyn Children>,
    pub(crate) in_process: AtomicU64,
    spawned: AtomicU64,
    pub(crate) handed_over: AtomicU64,
    pub(crate) unavailable: AtomicU64,
}

impl brush_core::host::Host for Embedding {
    fn spawn(&self, command: std::process::Command) -> std::io::Result<tokio::process::Child> {
        if self.stop.requested() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "the script was stopped",
            ));
        }
        self.spawned.fetch_add(1, Ordering::Relaxed);
        self.children.spawn(command)
    }

    fn interrupted(&self) -> bool {
        self.stop.requested()
    }

    fn process_id(&self) -> u32 {
        self.id
    }

    fn limits(&self) -> brush_core::host::Limits {
        LIMITS
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Start `script` on a shell of its own, on a thread of its own.
///
/// A panic inside the shell or one of its utilities ends that command or
/// that script with a message in its output; it never unwinds into the
/// caller. The shell's threads carry no session scope
/// (`pa_types::session_scope`): everything they need is in `script`.
///
/// # Errors
///
/// The script's thread could not be started.
pub fn start(script: Script, stop: Stop, children: Arc<dyn Children>) -> std::io::Result<Running> {
    let (report, ended) = tokio::sync::oneshot::channel();
    let (finished, gone) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("pa-shell".to_string())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            let embedding = Arc::new(Embedding {
                stop,
                id: script.id,
                children,
                in_process: AtomicU64::new(0),
                spawned: AtomicU64::new(0),
                handed_over: AtomicU64::new(0),
                unavailable: AtomicU64::new(0),
            });
            let counts = |embedding: &Embedding| Counts {
                in_process: embedding.in_process.load(Ordering::Relaxed),
                spawned: embedding.spawned.load(Ordering::Relaxed),
                handed_over: embedding.handed_over.load(Ordering::Relaxed),
                unavailable: embedding.unavailable.load(Ordering::Relaxed),
            };
            let Script {
                source,
                id: _,
                cwd,
                env,
                output,
            } = script;
            // The shell's own messages, and those about it, go where the
            // script's output goes.
            let say = |message: &str| {
                // A closed output has no reader left to tell.
                let _ = (&output).write_all(message.as_bytes());
            };
            let mut report = Some(report);
            let nesting = source
                .bytes()
                .fold((0usize, 0usize), |(depth, deepest), byte| match byte {
                    b'(' | b'{' | b'[' => (depth + 1, deepest.max(depth + 1)),
                    b')' | b'}' | b']' => (depth.saturating_sub(1), deepest),
                    _ => (depth, deepest),
                })
                .1;
            let refused = if source.len() > SOURCE_BYTES {
                Some(format!(
                    "bash: the script is {} bytes long; the built-in shell takes at most {SOURCE_BYTES}\n",
                    source.len()
                ))
            } else if nesting > NESTING {
                Some(format!(
                    "bash: the script nests brackets {nesting} deep; the built-in shell takes at most {NESTING}\n"
                ))
            } else {
                None
            };
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(WORKERS)
                .thread_name("pa-shell-worker")
                .thread_stack_size(STACK_BYTES)
                .enable_all()
                .build();
            match (refused, runtime) {
                (Some(reason), _) => {
                    say(&reason);
                    // Unheard is fine: the embedder may have stopped listening.
                    let _ = report.take().map(|report| {
                        report.send(Ended {
                            exit_code: 2,
                            counts: Counts::default(),
                        })
                    });
                }
                (None, Err(error)) => {
                    say(&format!("bash: the shell could not start: {error}\n"));
                    let _ = report.take().map(|report| {
                        report.send(Ended {
                            exit_code: 126,
                            counts: Counts::default(),
                        })
                    });
                }
                (None, Ok(runtime)) => {
                    let body = async {
                        let stdio = |stream: &std::io::PipeWriter| {
                            stream.try_clone().map(OpenFile::PipeWriter)
                        };
                        let fds = (|| {
                            Ok::<_, brush_core::Error>(HashMap::from([
                                (0, brush_core::openfiles::null()?),
                                (1, stdio(&output)?),
                                (2, stdio(&output)?),
                            ]))
                        })();
                        let shell = match fds {
                            Ok(fds) => {
                                let mut builder = Shell::builder_with_extensions::<Extensions>()
                                    .builtins(brush_builtins::default_builtins(
                                        brush_builtins::BuiltinSet::BashMode,
                                    ))
                                    .builtins(crate::shims::registrations())
                                    .builtins(crate::util::registrations())
                                    .profile(brush_core::ProfileLoadBehavior::Skip)
                                    .rc(brush_core::RcLoadBehavior::Skip)
                                    .do_not_inherit_env(true)
                                    .working_dir(cwd)
                                    .fds(fds)
                                    .shell_name("bash".to_string())
                                    .max_function_call_depth(FUNCTION_DEPTH)
                                    .host(Arc::clone(&embedding) as Arc<dyn brush_core::host::Host>);
                                for (name, value) in env {
                                    let mut variable = ShellVariable::new(value);
                                    variable.export();
                                    builder = builder.var(name, variable);
                                }
                                builder.build().await
                            }
                            Err(error) => Err(error),
                        };
                        let mut shell = match shell {
                            Ok(shell) => shell,
                            Err(error) => {
                                say(&format!("bash: the shell could not start: {error}\n"));
                                let _ = report.take().map(|report| {
                                    report.send(Ended {
                                        exit_code: 126,
                                        counts: Counts::default(),
                                    })
                                });
                                return;
                            }
                        };
                        let ran = tokio::select! {
                            biased;
                            () = embedding.stop.wait() => None,
                            ran = shell.run_dash_c_command(source) => Some(ran),
                        };
                        let exit_code = match ran {
                            None => STOPPED_EXIT_CODE,
                            Some(_) if embedding.stop.requested() => STOPPED_EXIT_CODE,
                            Some(Ok(result)) => i32::from(u8::from(result.exit_code)),
                            Some(Err(error)) => {
                                say(&ShellErrors.format_error(&error, &shell));
                                i32::from(u8::from(error.into_result(&shell).exit_code))
                            }
                        };
                        let _ = report.take().map(|report| {
                            report.send(Ended {
                                exit_code,
                                counts: counts(&embedding),
                            })
                        });
                        // What the script left in the background runs on, as
                        // under a shell that waits for its children.
                        tokio::select! {
                            biased;
                            () = embedding.stop.wait() => {}
                            // A job that failed has already said so.
                            _ = shell.jobs_mut().wait_all() => {}
                        }
                    };
                    let outcome = pa_types::session_scope::catching_panics(AssertUnwindSafe(|| {
                        runtime.block_on(body);
                    }));
                    if let Err(panic) = outcome {
                        let message = panic
                            .downcast_ref::<&str>()
                            .map(|text| (*text).to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "panic with a non-text payload".to_string());
                        say(&format!(
                            "bash: the built-in shell stopped on an internal error: {message}\n"
                        ));
                        let _ = report.take().map(|report| {
                            report.send(Ended {
                                exit_code: INTERNAL_ERROR_EXIT_CODE,
                                counts: counts(&embedding),
                            })
                        });
                    }
                    // Not joined: a worker stuck in a read ends with its pipe.
                    runtime.shutdown_background();
                }
            }
            drop(output);
            // Unheard is fine: the embedder may have stopped listening.
            let _ = finished.send(());
        })?;
    Ok(Running { ended, gone })
}
