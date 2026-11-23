//! The shell through its one entry point: scripts with their exact output,
//! and what a process that runs many of them relies on.

use std::io::Read;
use std::path::Path;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Duration;

use crate::{start, Children, Ended, Running, Script, Stop};

/// How long any one step may take before its test fails instead of hanging.
const STEP: Duration = Duration::from_secs(60);

/// Starts each program as asked; a program ends when the script that waits
/// for it is dropped.
struct Direct;

impl Children for Direct {
    fn spawn(&self, command: std::process::Command) -> std::io::Result<tokio::process::Child> {
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        command.spawn()
    }
}

/// A started script, its output as it arrives, and the request to stop it.
struct Started {
    running: Running,
    /// Each chunk the script writes; the channel closes when everything
    /// that held the script's output has let go of it.
    output: Receiver<Vec<u8>>,
    #[cfg_attr(
        not(unix),
        expect(
            dead_code,
            reason = "read by the test of stopping, which needs a program"
        )
    )]
    stop: Stop,
}

fn begin(source: &str, cwd: &Path, env: &[(&str, &str)]) -> Started {
    let (mut reading, writing) = std::io::pipe().unwrap();
    let (sent, output) = channel();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(read) = reading.read(&mut chunk) {
            // A test that has ended no longer listens.
            if read == 0 || sent.send(chunk[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    let stop = Stop::default();
    let running = start(
        Script {
            source: source.to_string(),
            id: 0x2000_0000,
            cwd: cwd.to_path_buf(),
            env: env
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
            output: writing,
        },
        stop.clone(),
        Arc::new(Direct),
    )
    .unwrap();
    Started {
        running,
        output,
        stop,
    }
}

impl Started {
    /// The script's result and everything it wrote, once all of it is gone.
    async fn finish(mut self) -> (Ended, String) {
        let ended = tokio::time::timeout(STEP, self.running.ended())
            .await
            .unwrap();
        tokio::time::timeout(STEP, self.running.gone())
            .await
            .unwrap();
        let mut output = Vec::new();
        while let Ok(chunk) = self.output.recv_timeout(STEP) {
            output.extend(chunk);
        }
        (ended, String::from_utf8_lossy(&output).into_owned())
    }
}

/// Every case of the corpus, on a machine with no programs at all: the
/// shell's own builtins and utilities are all a case can reach.
///
/// The corpus holds, per case, a `### name` line, the script, a `--- N` line
/// with the exit code, and the exact output (a final line ending in `\`
/// stands for output without a trailing newline). The expectations are what
/// GNU bash 5.2 with coreutils 9.4, grep 3.11, sed 4.9 and findutils 4.9
/// printed for the same script in an empty directory, under `LC_ALL=C` and
/// `TZ=UTC`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn every_corpus_script_prints_what_bash_and_the_gnu_tools_print() {
    let mut differing = Vec::new();
    for case in include_str!("corpus.txt")
        .split("\n### ")
        .map(|case| case.trim_start_matches("### "))
    {
        let (name, rest) = case.split_once('\n').unwrap();
        let (script, expected) = rest.split_once("\n--- ").unwrap();
        let (code, output) = expected.split_once('\n').unwrap_or((expected, ""));
        let expected_output = match output.trim_end_matches('\n').strip_suffix('\\') {
            Some(unterminated) => unterminated.to_string(),
            None if output.is_empty() => String::new(),
            None => format!("{}\n", output.trim_end_matches('\n')),
        };
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_string_lossy().into_owned();
        let (ended, printed) = begin(
            script,
            dir.path(),
            &[
                ("PATH", "/nonexistent-bin"),
                ("LC_ALL", "C"),
                ("TZ", "UTC"),
                ("HOME", &home),
            ],
        )
        .finish()
        .await;
        if (ended.exit_code, printed.as_str())
            != (
                code.trim().parse::<i32>().unwrap(),
                expected_output.as_str(),
            )
        {
            differing.push(format!(
                "{name}: exit {} and {printed:?}, expected exit {code} and {expected_output:?}",
                ended.exit_code
            ));
        }
    }
    assert_eq!(differing, Vec::<String>::new());
}

/// Two scripts running at once each keep their own working directory,
/// variables and environment, and neither touches the process's.
#[tokio::test(flavor = "multi_thread")]
async fn two_scripts_in_one_process_share_no_directory_and_no_environment() {
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let before = (
        std::env::current_dir().unwrap(),
        std::env::var_os("PA_SHELL_TEST_OWN"),
        std::env::var_os("PA_SHELL_TEST_GIVEN"),
    );
    // Each waits for the other's file, so both are mid-script together.
    let script = |own: &str, other: &str| {
        format!(
            "mkdir sub; cd sub; export PA_SHELL_TEST_OWN={own}; touch ../{own}.started; \
             until [ -e \"$OTHER/{other}.started\" ]; do sleep 0.01; done; \
             echo \"$PA_SHELL_TEST_GIVEN $PA_SHELL_TEST_OWN ${{PWD##*/}} $(ls ..)\""
        )
    };
    let paths: Vec<String> = dirs
        .iter()
        .map(|dir| dir.path().to_string_lossy().into_owned())
        .collect();
    let first = begin(
        &script("one", "two"),
        dirs[0].path(),
        &[("PA_SHELL_TEST_GIVEN", "first"), ("OTHER", &paths[1])],
    );
    let second = begin(
        &script("two", "one"),
        dirs[1].path(),
        &[("PA_SHELL_TEST_GIVEN", "second"), ("OTHER", &paths[0])],
    );
    let (first, second) = (first.finish().await, second.finish().await);
    let after = (
        std::env::current_dir().unwrap(),
        std::env::var_os("PA_SHELL_TEST_OWN"),
        std::env::var_os("PA_SHELL_TEST_GIVEN"),
    );
    assert_eq!(
        (first.1.as_str(), second.1.as_str(), after),
        (
            "first one sub one.started\nsub\n",
            "second two sub sub\ntwo.started\n",
            before
        )
    );
}

/// A stopped script ends at once wherever it is: in the interpreter's own
/// loop, in a utility that would run on, in a utility that waits for one,
/// or waiting for a program, which is ended with it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_script_ends_in_the_interpreter_in_a_utility_and_in_a_program() {
    let dir = tempfile::tempdir().unwrap();
    let mut ends = Vec::new();
    for script in [
        "echo started; while :; do :; done; echo not-reached",
        "echo started; sleep 1000; echo not-reached",
        "echo started; sleep 1000 | cat; echo not-reached",
        "/bin/sh -c 'echo started; exec /bin/sleep 1000'; echo not-reached",
    ] {
        let started = begin(script, dir.path(), &[("PATH", "/nonexistent-bin")]);
        let first = started.output.recv_timeout(STEP).unwrap();
        started.stop.request();
        // The output closes only when the program that held it is gone too.
        let (ended, rest) = started.finish().await;
        ends.push((
            String::from_utf8_lossy(&first).into_owned(),
            ended.exit_code,
            rest,
        ));
    }
    assert_eq!(
        ends,
        vec![
            (
                "started\n".to_string(),
                crate::STOPPED_EXIT_CODE,
                String::new()
            );
            4
        ]
    );
}

/// A utility that panics fails as a command: the script goes on, and so
/// does the process.
#[tokio::test(flavor = "multi_thread")]
async fn a_panic_in_a_utility_is_that_commands_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (ended, printed) = begin(
        "pa-test-panic; echo \"rc=$?\"; pa-test-panic | cat; echo after",
        dir.path(),
        &[],
    )
    .finish()
    .await;
    assert_eq!(
        (ended.exit_code, printed.as_str()),
        (
            0,
            "pa-test-panic: Prime Agent's built-in pa-test-panic failed on an internal error: \
             a utility panicked\nrc=70\n\
             pa-test-panic: Prime Agent's built-in pa-test-panic failed on an internal error: \
             a utility panicked\nafter\n"
        )
    );
}

/// What a utility does not implement goes to the program of the same name
/// when the machine has one, and is refused in so many words when it has
/// none; both are counted.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_unimplemented_option_goes_to_the_program_or_is_refused() {
    use crate::Counts;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let programs = dir.path().join("bin");
    std::fs::create_dir(&programs).unwrap();
    let program = programs.join("ls");
    std::fs::write(&program, "#!/bin/sh\necho \"the program got: $*\"\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = "touch here; ls; ls --zero; echo \"rc=$?\"";
    let with_program = begin(script, dir.path(), &[("PATH", &programs.to_string_lossy())])
        .finish()
        .await;
    let without = begin(script, dir.path(), &[("PATH", "/nonexistent-bin")])
        .finish()
        .await;
    assert_eq!(
        (
            (with_program.1.as_str(), with_program.0.counts),
            (without.1.as_str(), without.0.counts),
        ),
        (
            (
                "bin\nhere\nthe program got: --zero\nrc=0\n",
                Counts {
                    in_process: 2,
                    spawned: 1,
                    handed_over: 1,
                    unavailable: 0,
                },
            ),
            (
                "bin\nhere\nls: option --zero is not supported by Prime Agent's built-in ls, \
                 and no ls program is installed on this machine\nrc=2\n",
                Counts {
                    in_process: 2,
                    spawned: 0,
                    handed_over: 0,
                    unavailable: 1,
                },
            ),
        )
    );
}

/// `$$` is the number the embedder gave, never this process's id, and
/// signalling it (or the whole process group) ends the script alone.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_script_that_kills_itself_ends_only_itself() {
    let dir = tempfile::tempdir().unwrap();
    let mut ends = Vec::new();
    for script in [
        "echo $$ $PPID; kill $$; echo not-reached",
        "kill 0; echo not-reached",
        "exec true; echo not-reached",
    ] {
        let (ended, printed) = begin(script, dir.path(), &[("PATH", "/bin:/usr/bin")])
            .finish()
            .await;
        ends.push((printed, ended.exit_code != 0 || script.starts_with("exec")));
    }
    assert_eq!(
        ends,
        [
            ("536870912 536870912\n".to_string(), true),
            (String::new(), true),
            (String::new(), true),
        ]
    );
}

/// An expansion that would take more than the shell allows fails the line
/// it is on, as any failed expansion does, and the script goes on with the
/// next one: a count of words, a word that doubles itself, and the output of
/// a command substitution.
#[tokio::test(flavor = "multi_thread")]
async fn an_expansion_past_the_limits_fails_its_line_and_the_script_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let (ended, printed) = begin(
        "echo {1..2000000}; echo not-reached\n\
         echo \"rc=$?\"\n\
         x=0123456789abcdef; while :; do x=$x$x; done\n\
         echo \"rc=$? length ${#x}\"\n\
         y=$(yes | head -c 40000000)\n\
         echo \"rc=$? length ${#y}\"\n\
         z=$(yes | head -c 1000000); echo \"rc=$? length ${#z}\"",
        dir.path(),
        &[("PATH", "/nonexistent-bin")],
    )
    .finish()
    .await;
    assert_eq!(
        (ended.exit_code, printed.as_str()),
        (
            0,
            "bash: brace expansion: 2000000 words is more than this shell expands \
             (at most 1000000 words and 33554432 bytes)\n\
             rc=1\n\
             bash: expansion: a word of 67108864 bytes is longer than this shell expands \
             (at most 33554432 bytes)\n\
             rc=1 length 33554432\n\
             bash: command substitution: the command wrote more than this shell captures \
             (at most 33554432 bytes)\n\
             rc=1 length 0\n\
             rc=0 length 999999\n"
        )
    );
}
