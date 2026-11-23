//! Shell builtins that, in a shell of its own process, act on that process:
//! `exec` replaces it, `kill $$` and `kill 0` signal it, `umask` and
//! `ulimit` change it. Here the process is shared by every session, so each
//! is replaced by one that acts on the script alone or says that it cannot.

use std::io::Write;

use brush_core::builtins::{BoxFuture, Registration};
use brush_core::{CommandArg, ExecutionContext, ExecutionControlFlow, ExecutionResult};

use crate::engine::Extensions;
use crate::util::describe;
#[cfg(unix)]
use crate::util::embedding;

/// `exec [COMMAND [ARGUMENT...]]`: without a command, the redirections of
/// the `exec` itself become the shell's; with one, the command runs and the
/// shell exits with its status, which is all a script can see of being
/// replaced.
async fn exec(
    context: ExecutionContext<'_, Extensions>,
    args: Vec<CommandArg>,
) -> Result<ExecutionResult, brush_core::Error> {
    if args.len() < 2 {
        let fds: Vec<_> = context.iter_fds().collect();
        context.shell.replace_open_files(fds.into_iter());
        return Ok(ExecutionResult::success());
    }
    if args[1].to_string().starts_with('-') {
        writeln!(
            context.stderr(),
            "exec: options are not supported by Prime Agent's built-in shell"
        )?;
        return Ok(ExecutionResult::new(2));
    }
    // `command` runs its arguments as a command, past functions and aliases.
    let Some(command) = context.shell.builtins().get("command").cloned() else {
        return brush_core::error::unimp("exec without the command builtin");
    };
    let mut result = (command.execute_func)(context, args).await?;
    result.next_control_flow = ExecutionControlFlow::ExitShell;
    Ok(result)
}

/// `kill [-s SIGNAL | -n NUMBER | -SIGNAL] PID|%JOB...` and `kill -l`.
/// Signalling this process, its process group, or every process ends the
/// script instead: that is what a script means by `kill $$` and `kill 0`.
#[cfg(unix)]
fn kill(
    context: &mut ExecutionContext<'_, Extensions>,
    args: &[CommandArg],
) -> Result<ExecutionResult, brush_core::Error> {
    use nix::sys::signal::Signal;
    use std::str::FromStr as _;

    let words: Vec<String> = args.iter().skip(1).map(ToString::to_string).collect();
    let mut stderr = context.stderr();
    // `Some(None)` is signal 0: only ask whether the process is there.
    let named = |name: &str| -> Option<Option<Signal>> {
        if name == "0" {
            return Some(None);
        }
        if let Ok(number) = name.parse::<i32>() {
            return Signal::try_from(number).ok().map(Some);
        }
        let upper = name.to_ascii_uppercase();
        let full = if upper.starts_with("SIG") {
            upper
        } else {
            format!("SIG{upper}")
        };
        Signal::from_str(&full).ok().map(Some)
    };
    let mut signal = Some(Signal::SIGTERM);
    let mut at = 0;
    if let Some(first) = words.first() {
        let chosen = match first.as_str() {
            "-l" | "-L" => {
                let listed: Vec<&str> = Signal::iterator()
                    .map(|signal| signal.as_str().trim_start_matches("SIG"))
                    .collect();
                writeln!(context.stdout(), "{}", listed.join(" "))?;
                return Ok(ExecutionResult::success());
            }
            "--" => {
                at = 1;
                None
            }
            "-s" | "-n" => {
                at = 2;
                Some(words.get(1).map_or("", String::as_str))
            }
            option if option.starts_with('-') && option.len() > 1 => {
                at = 1;
                Some(&option[1..])
            }
            _ => None,
        };
        if let Some(name) = chosen {
            let Some(parsed) = named(name) else {
                writeln!(stderr, "kill: {name}: invalid signal specification")?;
                return Ok(ExecutionResult::new(1));
            };
            signal = parsed;
        }
    }
    if words.len() <= at {
        writeln!(
            stderr,
            "kill: usage: kill [-s sigspec | -n signum | -sigspec] pid | jobspec ... or kill -l [sigspec]"
        )?;
        return Ok(ExecutionResult::new(2));
    }
    // The script's own id, and the ids that would reach the process the
    // shell lives in.
    let script = embedding(context).and_then(|embedding| i32::try_from(embedding.id).ok());
    let own = nix::unistd::getpid().as_raw();
    let own_group = nix::unistd::getpgrp().as_raw();
    let mut status = 0;
    for target in &words[at..] {
        if target.starts_with('%') {
            let killed = context
                .shell
                .jobs_mut()
                .resolve_job_spec(target)
                .map(|job| job.process_group_id().or_else(|| job.representative_pid()));
            match killed {
                Some(Some(pid)) => {
                    // A job that already ended has nothing left to signal.
                    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal);
                }
                Some(None) => {}
                None => {
                    writeln!(stderr, "kill: {target}: no such job")?;
                    status = 1;
                }
            }
            continue;
        }
        let Ok(pid) = target.parse::<i32>() else {
            writeln!(
                stderr,
                "kill: {target}: arguments must be process or job IDs"
            )?;
            status = 1;
            continue;
        };
        if Some(pid) == script || pid == own || pid == 0 || pid == -1 || pid == -own_group {
            if signal.is_some() {
                if let Some(embedding) = embedding(context) {
                    embedding.stop.request();
                }
                return Ok(ExecutionResult {
                    next_control_flow: ExecutionControlFlow::ExitShell,
                    exit_code: 143.into(),
                });
            }
            continue;
        }
        if let Err(errno) = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal) {
            writeln!(stderr, "kill: ({pid}) - {}", errno.desc())?;
            status = 1;
        }
    }
    Ok(ExecutionResult::new(status))
}

/// `wait [PID|%JOB...]`: wait for the named background jobs (a job is named
/// by the pid `$!` held, or by its job number), or for all of them, and
/// report the last one's exit status.
async fn wait(
    context: ExecutionContext<'_, Extensions>,
    args: Vec<CommandArg>,
) -> Result<ExecutionResult, brush_core::Error> {
    let names: Vec<String> = args.iter().skip(1).map(ToString::to_string).collect();
    if names.is_empty() {
        context.shell.jobs_mut().wait_all().await?;
        return Ok(ExecutionResult::success());
    }
    let mut result = ExecutionResult::success();
    for name in names {
        if let Some(job) = context.shell.jobs_mut().resolve_job_spec(&name) {
            result = job.wait().await?;
        } else {
            writeln!(
                context.stderr(),
                "wait: pid {name} is not a child of this shell"
            )?;
            result = ExecutionResult::new(127);
        }
    }
    Ok(result)
}

/// What cannot be done to one script of a shared process.
fn refuse(
    context: &ExecutionContext<'_, Extensions>,
) -> Result<ExecutionResult, brush_core::Error> {
    writeln!(
        context.stderr(),
        "{}: not available in Prime Agent's built-in shell: it would change the process every session shares (run the script under a system shell with `bash -c` or `sh -c` instead)",
        context.command_name
    )?;
    Ok(ExecutionResult::new(1))
}

/// The replacements, by the name each is called by.
pub(crate) fn registrations() -> Vec<(String, Registration<Extensions>)> {
    let builtin = |execute_func| Registration {
        execute_func,
        content_func: describe,
        disabled: false,
        special_builtin: false,
        declaration_builtin: false,
    };
    let mut shims = vec![
        (
            "exec".to_string(),
            builtin(|context, args| -> BoxFuture<'_, _> { Box::pin(exec(context, args)) })
                .special(),
        ),
        (
            "umask".to_string(),
            builtin(|context, _args| -> BoxFuture<'_, _> {
                Box::pin(std::future::ready(refuse(&context)))
            }),
        ),
        (
            "ulimit".to_string(),
            builtin(|context, _args| -> BoxFuture<'_, _> {
                Box::pin(std::future::ready(refuse(&context)))
            }),
        ),
        (
            "suspend".to_string(),
            builtin(|context, _args| -> BoxFuture<'_, _> {
                Box::pin(std::future::ready(refuse(&context)))
            }),
        ),
    ];
    shims.push((
        "wait".to_string(),
        builtin(|context, args| -> BoxFuture<'_, _> { Box::pin(wait(context, args)) }),
    ));
    #[cfg(unix)]
    shims.push((
        "kill".to_string(),
        builtin(|mut context, args| -> BoxFuture<'_, _> {
            Box::pin(std::future::ready(kill(&mut context, &args)))
        }),
    ));
    shims
}
