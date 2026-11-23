//! The utilities the shell runs itself: what each is given, how one that
//! cannot do what it was asked hands over to a program, and how a failure
//! inside one stays that command's failure.
//!
//! A utility is a function of an [`Invocation`]: its arguments, the shell's
//! streams for this command, and a [`View`] of the shell. It resolves every
//! path against the shell's working directory ([`View::path`]), reads and
//! writes only those streams, and has no access to the process's own
//! working directory, environment, or standard streams. It parses all of its
//! arguments before it writes anything, so an option it does not implement
//! is answered with [`Failure::Unsupported`] while handing over is still
//! possible.

mod files;
mod find;
mod grep;
mod ls;
mod pattern;
mod sed;
mod system;
mod text;
mod xargs;

use std::io::{BufRead, BufWriter, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use brush_core::builtins::{BoxFuture, ContentOptions, ContentType, Registration};
use brush_core::{CommandArg, ExecutionContext, ExecutionResult};

use crate::engine::{Embedding, Extensions};
use crate::{Stop, INTERNAL_ERROR_EXIT_CODE, STOPPED_EXIT_CODE};

/// Why a utility ended without an exit code of its own.
pub(crate) enum Failure {
    /// It does not implement what was asked. Nothing was written yet: the
    /// command goes to the program of the same name.
    Unsupported(String),
    /// A read or a write failed. A closed pipe ends the command quietly,
    /// as SIGPIPE ends a program; anything else is reported.
    Io(std::io::Error),
    /// The script was stopped.
    Stopped,
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Failure::Io(error)
    }
}

/// A utility's exit code, or why it has none.
pub(crate) type Outcome = Result<i32, Failure>;

/// One run of one utility: its arguments, its streams, and its view of the
/// shell. The streams are separate fields so a utility reads one while it
/// writes another.
pub(crate) struct Invocation<'a> {
    /// The name it was called by.
    pub(crate) name: &'a str,
    /// Its arguments, without the name.
    pub(crate) args: &'a [String],
    pub(crate) stdin: Box<dyn BufRead + 'a>,
    pub(crate) stdout: Box<dyn Write + 'a>,
    pub(crate) stderr: Box<dyn Write + 'a>,
    pub(crate) shell: View<'a>,
}

/// What a utility sees of the shell that runs it.
#[derive(Clone, Copy)]
pub(crate) struct View<'a> {
    cwd: &'a Path,
    stop: &'a Stop,
    /// A variable of the shell's environment.
    pub(crate) env: &'a dyn Fn(&str) -> Option<String>,
    /// The shell's exported variables.
    pub(crate) environment: &'a dyn Fn() -> Vec<(String, String)>,
    /// A program of this name on the shell's `PATH`.
    pub(crate) program: &'a dyn Fn(&str) -> Option<PathBuf>,
    /// One of the command's open descriptors, for reading.
    descriptor: &'a dyn Fn(i32) -> Option<Box<dyn std::io::Read>>,
}

/// The descriptor of the command that `path` names: `/dev/stdin` and
/// `/dev/fd/N` (what a process substitution expands to). In a shell of its
/// own process these are files; here they would be the embedding process's.
fn descriptor_named(path: &str) -> Option<i32> {
    if path == "/dev/stdin" {
        Some(0)
    } else {
        path.strip_prefix("/dev/fd/")?.parse().ok()
    }
}

/// `name` quoted as GNU coreutils quotes a file name in a message.
pub(crate) fn quoted(name: &str) -> String {
    format!("'{}'", name.replace('\'', "'\\''"))
}

impl View<'_> {
    /// `path` as the shell sees it: relative to its working directory.
    pub(crate) fn path(&self, path: &str) -> PathBuf {
        let given = Path::new(path);
        if descriptor_named(path).is_some() {
            // Only ever read, through `file`: as a path it leads nowhere.
            Path::new("/dev/null").join("descriptor")
        } else if given.is_absolute() {
            given.to_path_buf()
        } else {
            self.cwd.join(given)
        }
    }

    /// Fail with [`Failure::Stopped`] once the script is stopped: called
    /// between the lines or files of anything that can run long.
    pub(crate) fn alive(&self) -> Result<(), Failure> {
        if self.stop.requested() {
            Err(Failure::Stopped)
        } else {
            Ok(())
        }
    }

    /// Fail with [`Failure::Unsupported`] when text does not sort by its
    /// bytes in the shell's locale and the machine has `program` to sort it
    /// the locale's way (the collation is the C library's). Without the
    /// program a utility sorts by bytes whatever the locale.
    pub(crate) fn sorts_by_bytes(&self, program: &str) -> Result<(), Failure> {
        let by_bytes = ["LC_ALL", "LC_COLLATE", "LANG"]
            .into_iter()
            .find_map(|variable| (self.env)(variable).filter(|value| !value.is_empty()))
            .is_none_or(|locale| {
                matches!(
                    locale.to_ascii_uppercase().as_str(),
                    "C" | "POSIX" | "C.UTF-8" | "C.UTF8"
                )
            });
        if by_bytes || (self.program)(program).is_none() {
            Ok(())
        } else {
            Err(Failure::Unsupported("the locale's sort order".to_string()))
        }
    }

    /// Open `path` for reading, or `stdin` for `-`.
    pub(crate) fn open<'r>(
        &self,
        stdin: &'r mut dyn BufRead,
        path: &str,
    ) -> Result<Box<dyn BufRead + 'r>, Failure> {
        if path == "-" {
            Ok(Box::new(stdin))
        } else {
            self.file(path)
        }
    }

    /// Open the file at `path` for reading. Only regular files are opened
    /// here: a read of a FIFO or a device cannot be stopped, so those go to
    /// the program instead.
    pub(crate) fn file<'r>(&self, path: &str) -> Result<Box<dyn BufRead + 'r>, Failure> {
        if path == "/dev/null" {
            return Ok(Box::new(std::io::empty()));
        }
        if let Some(descriptor) = descriptor_named(path) {
            return match (self.descriptor)(descriptor) {
                Some(stream) => Ok(Box::new(std::io::BufReader::new(stream))),
                None => Err(Failure::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Bad file descriptor",
                ))),
            };
        }
        let resolved = self.path(path);
        let metadata = std::fs::metadata(&resolved)?;
        if metadata.is_dir() {
            return Err(Failure::Io(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                "Is a directory",
            )));
        }
        if !metadata.is_file() {
            return Err(Failure::Unsupported(format!(
                "reading {path}, which is not a regular file,"
            )));
        }
        Ok(Box::new(std::io::BufReader::new(std::fs::File::open(
            resolved,
        )?)))
    }
}

/// An operating-system error as the GNU tools word it: without Rust's
/// `(os error N)` suffix.
pub(crate) fn reason(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.find(" (os error") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

/// The arguments of a utility, split the way `getopt` splits them: clustered
/// short options, `--long` and `--long=value`, `--` ending the options.
pub(crate) struct Options<'a> {
    args: std::slice::Iter<'a, String>,
    /// The rest of a cluster of short options (`-abc`).
    cluster: std::str::Chars<'a>,
    /// A value attached to the long option just returned (`--name=value`).
    attached: Option<&'a str>,
    operands_only: bool,
}

/// One parsed argument.
pub(crate) enum Arg<'a> {
    Short(char),
    Long(&'a str),
    Operand(&'a str),
}

impl<'a> Options<'a> {
    pub(crate) fn new(args: &'a [String]) -> Self {
        Options {
            args: args.iter(),
            cluster: "".chars(),
            attached: None,
            operands_only: false,
        }
    }

    /// The next option or operand. A lone `-` is an operand.
    pub(crate) fn next(&mut self) -> Option<Arg<'a>> {
        if let Some(short) = self.cluster.next() {
            return Some(Arg::Short(short));
        }
        self.attached = None;
        let arg = self.args.next()?.as_str();
        if self.operands_only || arg == "-" || !arg.starts_with('-') {
            return Some(Arg::Operand(arg));
        }
        if arg == "--" {
            self.operands_only = true;
            return self.next();
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            self.attached = value;
            return Some(Arg::Long(name));
        }
        self.cluster = arg[1..].chars();
        self.cluster.next().map(Arg::Short)
    }

    /// The value of the option just returned: the rest of its cluster
    /// (`-n5`), what follows `=` (`--lines=5`), or the next argument.
    pub(crate) fn value(&mut self) -> Result<&'a str, Failure> {
        let rest = self.cluster.as_str();
        if !rest.is_empty() {
            self.cluster = "".chars();
            return Ok(rest);
        }
        if let Some(attached) = self.attached.take() {
            return Ok(attached);
        }
        self.args
            .next()
            .map(String::as_str)
            .ok_or_else(|| Failure::Unsupported("an option without its value".to_string()))
    }

    /// The value attached to the option just returned (`-i.bak`,
    /// `--in-place=.bak`), if it has one: the next argument is never taken.
    pub(crate) fn attached(&mut self) -> Option<&'a str> {
        let rest = self.cluster.as_str();
        if rest.is_empty() {
            self.attached.take()
        } else {
            self.cluster = "".chars();
            Some(rest)
        }
    }

    /// An option this utility does not implement.
    pub(crate) fn unsupported(arg: &Arg<'_>) -> Failure {
        Failure::Unsupported(match arg {
            Arg::Short(short) => format!("option -{short}"),
            Arg::Long(long) => format!("option --{long}"),
            Arg::Operand(operand) => format!("operand {operand}"),
        })
    }
}

/// What the registration of a utility or a replaced builtin answers `help`
/// with.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature is the shell's for a registration"
)]
pub(crate) fn describe(
    name: &str,
    _content: ContentType,
    _options: &ContentOptions,
) -> Result<String, brush_core::Error> {
    Ok(format!("{name}: built into Prime Agent's shell\n"))
}

/// The script's hold on the shell that runs `context`, when it has one.
pub(crate) fn embedding<'a>(
    context: &'a ExecutionContext<'_, Extensions>,
) -> Option<&'a Embedding> {
    context
        .shell
        .host()
        .and_then(|host| host.as_any().downcast_ref::<Embedding>())
}

/// What a panic carried, as text.
pub(crate) fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic with a non-text payload".to_string())
}

/// Give the command `name words` to the program of that name: `what` is not
/// something the built-in implements. Without such a program the command
/// fails and says so.
pub(crate) async fn hand_over(
    context: &ExecutionContext<'_, Extensions>,
    name: &str,
    words: &[String],
    what: &str,
) -> Result<ExecutionResult, brush_core::Error> {
    let mut stderr = context.stderr();
    let program = context.shell.find_first_executable_in_path(name);
    let exit_code = if let (Some(program), Some(host)) = (program, context.shell.host().cloned()) {
        if let Some(embedding) = embedding(context) {
            embedding.handed_over.fetch_add(1, Ordering::Relaxed);
        }
        let command = brush_core::commands::compose_std_command(
            context,
            &program.to_string_lossy(),
            name,
            words,
            /*empty_env*/ false,
        )?;
        match host.spawn(command) {
            Ok(mut child) => child
                .wait()
                .await
                .ok()
                .and_then(|status| status.code())
                // Ended by a signal, as a stopped script's programs are.
                .unwrap_or(STOPPED_EXIT_CODE),
            Err(error) => {
                writeln!(stderr, "{name}: {}", reason(&error))?;
                126
            }
        }
    } else {
        if let Some(embedding) = embedding(context) {
            embedding.unavailable.fetch_add(1, Ordering::Relaxed);
        }
        writeln!(
            stderr,
            "{name}: {what} is not supported by Prime Agent's built-in {name}, and no {name} program is installed on this machine"
        )?;
        2
    };
    Ok(ExecutionResult::new(
        u8::try_from(exit_code.rem_euclid(256)).unwrap_or(1),
    ))
}

/// Run utility `run` as the shell's command `context`: in the process when
/// it implements what was asked, else as the program of the same name.
async fn dispatch(
    run: fn(&mut Invocation<'_>) -> Outcome,
    context: ExecutionContext<'_, Extensions>,
    args: Vec<CommandArg>,
) -> Result<ExecutionResult, brush_core::Error> {
    let words: Vec<String> = args.iter().skip(1).map(ToString::to_string).collect();
    let name = context.command_name.clone();
    // A device or `/proc` path means the process that opens it. Here that
    // is the embedding process, so such a command is the program's, where
    // the path means the script's own child.
    let process_path = |path: &str| {
        (path.starts_with("/dev/") && path != "/dev/null" && descriptor_named(path).is_none())
            || path.starts_with("/proc/")
    };
    if let Some(path) = words
        .iter()
        .map(String::as_str)
        .chain(context.shell.working_dir().to_str())
        .find(|path| process_path(path))
    {
        return hand_over(&context, &name, &words, &format!("the path {path}")).await;
    }
    let unstoppable = Stop::default();
    let mut stderr = context.stderr();
    let outcome = {
        let shell = &*context.shell;
        let mut invocation = Invocation {
            name: &name,
            args: &words,
            stdin: Box::new(std::io::BufReader::new(context.stdin())),
            stdout: Box::new(BufWriter::with_capacity(64 * 1024, context.stdout())),
            stderr: Box::new(context.stderr()),
            shell: View {
                cwd: shell.working_dir(),
                stop: embedding(&context).map_or(&unstoppable, |embedding| &embedding.stop),
                env: &|variable| shell.env_str(variable).map(std::borrow::Cow::into_owned),
                environment: &|| {
                    shell
                        .env()
                        .iter_exported()
                        .filter(|(_, variable)| variable.value().is_set())
                        .map(|(name, variable)| {
                            (
                                name.clone(),
                                variable.value().to_cow_str(shell).into_owned(),
                            )
                        })
                        .collect()
                },
                program: &|program| shell.find_first_executable_in_path(program),
                descriptor: &|descriptor| {
                    context
                        .try_fd(descriptor)
                        .map(|file| Box::new(file) as Box<dyn std::io::Read>)
                },
            },
        };
        // A utility's panic is that command's failure: it must not unwind
        // through the interpreter.
        pa_types::session_scope::catching_panics(AssertUnwindSafe(|| {
            let outcome = run(&mut invocation);
            match invocation.stdout.flush() {
                Err(error) if outcome.is_ok() => Err(Failure::Io(error)),
                Ok(()) | Err(_) => outcome,
            }
        }))
    };
    let count = |counter: fn(&Embedding) -> &AtomicU64| {
        if let Some(embedding) = embedding(&context) {
            counter(embedding).fetch_add(1, Ordering::Relaxed);
        }
    };
    let exit_code = match outcome {
        Ok(Ok(code)) => {
            count(|embedding| &embedding.in_process);
            code
        }
        Ok(Err(Failure::Stopped)) => STOPPED_EXIT_CODE,
        Ok(Err(Failure::Io(error))) => {
            count(|embedding| &embedding.in_process);
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                // 128 + SIGPIPE, as the shell reports a program its reader left.
                141
            } else {
                writeln!(stderr, "{name}: {}", reason(&error))?;
                1
            }
        }
        Ok(Err(Failure::Unsupported(what))) => {
            return hand_over(&context, &name, &words, &what).await;
        }
        Err(panic) => {
            writeln!(
                stderr,
                "{name}: Prime Agent's built-in {name} failed on an internal error: {}",
                panic_text(&*panic)
            )?;
            INTERNAL_ERROR_EXIT_CODE
        }
    };
    Ok(ExecutionResult::new(
        u8::try_from(exit_code.rem_euclid(256)).unwrap_or(1),
    ))
}

macro_rules! utilities {
    ($($name:literal => $run:path,)*) => {
        /// The names of the utilities the shell runs inside the process.
        pub const UTILITIES: &[&str] = &[$($name,)* "xargs"];

        /// The utilities as the shell's builtins.
        pub(crate) fn registrations() -> Vec<(String, Registration<Extensions>)> {
            #[allow(unused_mut, reason = "only a test build adds to it")]
            let mut utilities = vec![
                $((
                    $name.to_string(),
                    Registration {
                        execute_func: |context, args| -> BoxFuture<'_, _> {
                            Box::pin(dispatch($run, context, args))
                        },
                        content_func: describe,
                        disabled: false,
                        special_builtin: false,
                        declaration_builtin: false,
                    },
                ),)*
                (
                    "xargs".to_string(),
                    Registration {
                        execute_func: |context, args| -> BoxFuture<'_, _> {
                            Box::pin(xargs::xargs(context, args))
                        },
                        content_func: describe,
                        disabled: false,
                        special_builtin: false,
                        declaration_builtin: false,
                    },
                ),
            ];
            // A fault-injection seam for the tests: a utility that panics.
            #[cfg(test)]
            utilities.push((
                "pa-test-panic".to_string(),
                Registration {
                    execute_func: |context, args| -> BoxFuture<'_, _> {
                        Box::pin(dispatch(|_| panic!("a utility panicked"), context, args))
                    },
                    content_func: describe,
                    disabled: false,
                    special_builtin: false,
                    declaration_builtin: false,
                },
            ));
            utilities
        }
    };
}

utilities! {
    "basename" => files::basename,
    "cat" => files::cat,
    "chmod" => files::chmod,
    "cp" => files::cp,
    "cut" => text::cut,
    "date" => system::date,
    "dirname" => files::dirname,
    "env" => system::env,
    "find" => find::find,
    "grep" => grep::grep,
    "head" => files::head,
    "ls" => ls::ls,
    "mkdir" => files::mkdir,
    "mv" => files::mv,
    "printenv" => system::env,
    "realpath" => files::realpath,
    "rm" => files::rm,
    "rmdir" => files::rmdir,
    "sed" => sed::sed,
    "seq" => text::seq,
    "sleep" => system::sleep,
    "sort" => text::sort,
    "tac" => text::tac,
    "tail" => files::tail,
    "tee" => files::tee,
    "touch" => files::touch,
    "tr" => text::tr,
    "uname" => system::uname,
    "uniq" => text::uniq,
    "wc" => files::wc,
    "which" => system::which,
    "whoami" => system::whoami,
    "yes" => text::yes,
}

/// Read `reader` to its end, giving `line` each line without its newline
/// and whether it had one, until `line` answers false. A stop is noticed
/// between lines.
pub(crate) fn each_line(
    reader: &mut dyn BufRead,
    shell: View<'_>,
    mut line: impl FnMut(&[u8], bool) -> Result<bool, Failure>,
) -> Result<(), Failure> {
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            return Ok(());
        }
        shell.alive()?;
        let terminated = buffer.last() == Some(&b'\n');
        if terminated {
            buffer.pop();
        }
        if !line(&buffer, terminated)? {
            return Ok(());
        }
    }
}
