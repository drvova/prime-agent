fn main() {
    // Allocator tuning before any thread spawns: the session-load and
    // attach-snapshot phases are large transient bursts, and glibc's
    // per-thread arenas otherwise keep each burst's high-water pages
    // resident for the process lifetime.
    pa_types::memory_release::cap_thread_arenas();
    // Before anything spawns: a daemon or program started from here must not
    // hold the caller's stdio pipes open past this process (Windows).
    pa_core::platform::process::withhold_standard_handles_from_children();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // A session's elevated helper (Windows), started through UAC: it serves
    // that session's elevated commands and nothing else of the CLI runs.
    if args.first().map(String::as_str) == Some("--elevation-helper") {
        std::process::exit(pa_core::kernel::run_elevation_helper(&args[1..]));
    }
    // A fresh top-level invocation sheds an inherited worker identity before
    // any thread exists (edition 2021 makes `remove_var` safe, but it must
    // still run single-threaded): everything but the supervisor-spawned
    // `worker` subcommand.
    if args.first().map(String::as_str) != Some("worker") {
        pa_daemon::worker::strip_inherited_worker_env();
    }
    // Still single-threaded: a kept daemon launch splits into the daemon and its restart
    // owner here (a launch written any other way is kept all the same, one program start
    // later); any other process sheds the owner's hand-off.
    pa_daemon::keeper::enter(
        args.windows(2)
            .any(|pair| pair[0] == "--mode" && pair[1] == "daemon"),
    );
    // Still single-threaded: the Scheme interpreter's own directory moves
    // under the agent directory before anything can build an engine.
    pa_core::kernel::pin_scheme_home();
    let code = pa_cli::main_with_runtime(&args, &pa_cli::PrintRuntime);
    // Restore the Windows VT bit before process exit.
    pa_types::platform::console_restore();
    std::process::exit(code);
}
