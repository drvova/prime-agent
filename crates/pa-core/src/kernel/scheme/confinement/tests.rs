//! A session's cells against Steel's own ways out of the kernel, through a
//! real session kernel.

use crate::kernel::scheme::SchemeKernelProvisioner;
use crate::kernel::shared::HostRequestHandlers;
use crate::tools::kernel_cell::{KernelExecuteOptions, KernelProvisioner as _};
use steel::steel_vm::engine::Engine;

/// What `code` did as one cell: what it printed, or the error it raised.
async fn cell(provisioner: &SchemeKernelProvisioner, code: &str) -> Result<String, String> {
    let result = provisioner
        .ensure(None, None)
        .await
        .unwrap()
        .execute(
            code,
            KernelExecuteOptions {
                signal: None,
                timeout_ms: None,
                on_stream: None,
                on_late_sent_agent_message: None,
            },
        )
        .await
        .unwrap();
    match result.error {
        None => Ok(result.stdout),
        Some(error) => Err(error.evalue),
    }
}

/// Every way Steel itself offers to read or write a file, list or change a
/// directory, start a process, load a file as code, read the environment, or
/// build an engine without these limits fails in a cell, and leaves no
/// trace; the kernel's own functions do the same work.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_reaches_files_and_processes_only_through_the_kernel_functions() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "the secret\n").unwrap();
    let code = dir.path().join("code.scm");
    std::fs::write(&code, "(define loaded-from-a-file 1)\n").unwrap();
    let made = dir.path().join("made");
    let (secret, code, made) = (
        secret.to_string_lossy().into_owned(),
        code.to_string_lossy().into_owned(),
        made.to_string_lossy().into_owned(),
    );
    let provisioner = SchemeKernelProvisioner::new(HostRequestHandlers::new(), dir.path());
    let before = std::env::current_dir().unwrap();
    let mut open = Vec::new();
    for attempt in [
        // Files and directories.
        format!("(read-port-to-string (open-input-file {secret:?}))"),
        format!("(#%prim.open-input-file {secret:?})"),
        format!("(call-with-input-file {secret:?} read-line)"),
        format!("(with-input-from-file {secret:?} read-line)"),
        format!("(open-output-file {made:?})"),
        format!("(call-with-output-file {made:?} (lambda (port) (write-string \"x\" port)))"),
        format!("(read-dir {:?})", dir.path()),
        format!("(path-exists? {secret:?})"),
        format!("(is-file? {secret:?})"),
        format!("(create-directory! {made:?})"),
        format!("(delete-file! {secret:?})"),
        format!("(begin (require-builtin steel/filesystem as fs.) (fs.read-dir {:?}))", dir.path()),
        format!("(begin (require-builtin steel/ports as ports.) (ports.open-input-file {secret:?}))"),
        "(current-directory)".to_string(),
        format!("(change-current-directory! {:?})", dir.path()),
        format!("(set-current-dir! (command \"true\" (list)) {:?})", dir.path()),
        // Processes.
        format!("(spawn-process (command \"touch\" (list {made:?})))"),
        format!("(#%prim.spawn-process (#%prim.command \"touch\" (list {made:?})))"),
        format!(
            "(begin (require-builtin steel/process as process.)
                    (process.spawn-process (process.command \"touch\" (list {made:?}))))"
        ),
        format!(
            "((%module-get% %-builtin-module-steel/process \"spawn-process\")
              ((%module-get% %-builtin-module-steel/process \"command\") \"touch\" (list {made:?})))"
        ),
        "(which \"sh\")".to_string(),
        format!("(begin (require-builtin steel/git as git.) (git.git-clone \"https://example.invalid/x\" {made:?}))"),
        "(begin (require-builtin steel/tcp as tcp.) (tcp.tcp-connect \"127.0.0.1:9\"))".to_string(),
        // Files as code, and engines without these limits.
        format!("(load {code:?})"),
        format!("(#%prim.load {code:?})"),
        format!("(require {code:?})"),
        format!("(eval '(require {code:?}))"),
        format!("(eval-string \"(load \\\"{code}\\\")\")"),
        format!("(run! (Engine::new) \"(spawn-process (command \\\"touch\\\" (list \\\"{made}\\\")))\")"),
        format!(
            "(run! ((%module-get% %-builtin-module-steel/meta \"Engine::new\")) \"(read-dir \\\"{}\\\")\")",
            dir.path().display()
        ),
        "((%module-get% %-builtin-module-steel/base \"Engine::new\"))".to_string(),
        // The process's environment and streams.
        "(env-var \"HOME\")".to_string(),
        "(maybe-get-env-var \"HOME\")".to_string(),
        "(set-env-var! \"PA_CONFINEMENT_TEST\" \"1\")".to_string(),
        "(write-string \"x\" (stdout))".to_string(),
        "(write-string \"x\" (#%default-output-port))".to_string(),
        "(write-string \"x\" (#%default-error-port))".to_string(),
        "(write-string \"x\" (#%prim.#%default-output-port))".to_string(),
        // Builtins that print with the process's own `print!`.
        "(stdout-simple-displayln \"x\")".to_string(),
        "(%string->render-markdown \"x\")".to_string(),
        "(inspect car)".to_string(),
        "(debug-globals)".to_string(),
        "(#%lookup-doc car)".to_string(),
        "(#%native-fn-ptr-doc car)".to_string(),
        "(read-line (stdin))".to_string(),
    ] {
        if cell(&provisioner, &attempt).await.is_ok() {
            open.push(attempt);
        }
    }
    let defined = cell(&provisioner, "(display loaded-from-a-file)").await;
    let through_the_kernel = cell(
        &provisioner,
        "(fs-write \"made\" \"by the kernel\\n\")
         (display (list (hash-ref (bash \"cat secret.txt\") 'output)
                        (length (hash-ref (fs-read \"made\") 'lines))
                        (eval '(+ 1 2))
                        (with-output-to-string (lambda () (display \"captured\")))))",
    )
    .await;
    assert_eq!(
        (
            open,
            defined.is_err(),
            std::env::current_dir().unwrap(),
            std::env::var_os("PA_CONFINEMENT_TEST"),
            through_the_kernel,
        ),
        (
            Vec::new(),
            true,
            before,
            None,
            Ok("(the secret\n 1 3 captured)".to_string()),
        )
    );
}

/// Steel runs the body of a `defmacro`, of a `syntax-case` transformer and
/// of `begin-for-syntax` in a second engine its compiler keeps. That engine
/// is closed like the session's own: the same attempts fail there and leave
/// no trace, what a body prints is the cell's output, and macros (Steel's
/// own `struct` among them) still expand.
#[tokio::test(flavor = "multi_thread")]
async fn a_macro_body_reaches_no_more_than_its_cell_does() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "the secret\n").unwrap();
    let code = dir.path().join("code.scm");
    std::fs::write(&code, "(define loaded-from-a-file 1)\n").unwrap();
    let made = dir.path().join("made");
    let (secret, code, made) = (
        secret.to_string_lossy().into_owned(),
        code.to_string_lossy().into_owned(),
        made.to_string_lossy().into_owned(),
    );
    let provisioner = SchemeKernelProvisioner::new(HostRequestHandlers::new(), dir.path());
    let mut open = Vec::new();
    for (number, attempt) in [
        format!("(spawn-process (command \"touch\" (list {made:?})))"),
        format!(
            "((%module-get% %-builtin-module-steel/process \"spawn-process\")
              ((%module-get% %-builtin-module-steel/process \"command\") \"touch\" (list {made:?})))"
        ),
        format!("(read-port-to-string (open-input-file {secret:?}))"),
        format!("(run! (Engine::new) \"(read-dir \\\"{}\\\")\")", dir.path().display()),
        "(env-var \"HOME\")".to_string(),
        "(set-env-var! \"PA_CONFINEMENT_MACRO_TEST\" \"1\")".to_string(),
        "(write-string \"x\" (#%default-output-port))".to_string(),
        "(stdout-simple-displayln \"x\")".to_string(),
        "(spawn-native-thread (lambda () 1))".to_string(),
        "(eval '(env-var \"HOME\"))".to_string(),
        format!("(eval '(require {code:?}))"),
    ]
    .into_iter()
    .enumerate()
    {
        for form in [
            format!("(defmacro (escape-{number} stx) {attempt} ''1) (escape-{number} 1)"),
            format!(
                "(define-syntax (escape-case-{number} stx)
                   (syntax-case stx () [(_ x) (begin {attempt} #'x)]))
                 (escape-case-{number} 1)"
            ),
            format!(
                "(begin-for-syntax (define (escape-helper-{number}) {attempt}))
                 (defmacro (escape-for-syntax-{number} stx) (escape-helper-{number}) ''1)
                 (escape-for-syntax-{number} 1)"
            ),
        ] {
            if cell(&provisioner, &form).await.is_ok() {
                open.push(form);
            }
        }
    }
    let expanding = cell(
        &provisioner,
        "(defmacro (twice stx)
           (displayln \"expanding\")
           (let ([form (cadr (syntax->datum stx))]) `(begin ,form ,form)))
         (define-syntax (plus-one stx) (syntax-case stx () [(_ x) #'(+ x 1)]))
         (struct point (x y))
         (twice (display (plus-one (point-y (point 1 2)))))",
    )
    .await;
    assert_eq!(
        (
            open,
            std::path::Path::new(&made).exists(),
            std::env::var_os("PA_CONFINEMENT_MACRO_TEST"),
            expanding,
        ),
        (Vec::new(), false, None, Ok("expanding\n33".to_string()))
    );
}

/// A macro body that loops is stopped with its cell: the stop reaches the
/// engine the body runs in, not only the session's own.
#[tokio::test(flavor = "multi_thread")]
async fn a_looping_macro_body_stops_with_its_cell() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), dir.path())
        .ensure(None, None)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    // The loop ends by itself after a minute, so a stop that does not reach
    // it fails this test instead of hanging it.
    let result = kernel
        .execute(
            "(defmacro (spin stx)
               (let ([until (+ (current-milliseconds) 60000)])
                 (let loop () (when (< (current-milliseconds) until) (loop))))
               ''done)
             (spin 1)",
            KernelExecuteOptions {
                signal: None,
                timeout_ms: Some(200),
                on_stream: None,
                on_late_sent_agent_message: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (
            result.error.map(|error| error.evalue),
            started.elapsed() < std::time::Duration::from_secs(20),
        ),
        (
            Some("the cell was interrupted after 200 ms".to_string()),
            true
        )
    );
}

/// Everything a cell can name in a fresh session engine or in the engine
/// its macro bodies run in: each global, and each function of each builtin
/// module (reached through `require-builtin` or a module lookup), leaving
/// out the ones bound to the refusal. Names Steel numbers as it compiles its
/// own library are listed without the number.
fn reachable() -> std::collections::BTreeSet<String> {
    let mut engine = super::engine().unwrap();
    let open = |value: &steel::rvals::SteelVal| match value {
        steel::rvals::SteelVal::FuncV(function) => {
            !std::ptr::fn_addr_eq(*function, super::refusal as steel::rvals::FunctionSignature)
        }
        _ => true,
    };
    let reachable_in = |engine: &Engine| {
        let mut names = std::collections::BTreeSet::new();
        for global in engine.globals().iter() {
            let global = global.resolve().to_string();
            if engine
                .extract_value(&global)
                .is_ok_and(|value| open(&value))
            {
                let numbered = ["##gensym", "##__lifted_pure_function"]
                    .into_iter()
                    .find_map(|prefix| global.find(prefix).map(|at| at + prefix.len()))
                    .filter(|at| global[*at..].chars().all(|digit| digit.is_ascii_digit()));
                names.insert(match numbered {
                    Some(at) => format!("{}N", &global[..at]),
                    None => global,
                });
            }
        }
        for module in super::STEEL_MODULES {
            if let Some(functions) = engine.builtin_modules().get(module) {
                for (name, value) in functions.inner_map().iter() {
                    if open(value) {
                        names.insert(format!("{module} {name}"));
                    }
                }
            }
        }
        names
    };
    let mut names = reachable_in(&engine);
    names.extend(
        engine
            .with_macro_engine(|macros| reachable_in(macros))
            .unwrap(),
    );
    names
}

/// The names a cell can reach are exactly the ones `open_globals.txt` lists:
/// a Steel upgrade that adds a builtin, a module, or a way around a closed
/// one fails here until the new name is closed in `confinement.rs` or, once
/// it is known to open nothing, added to the list.
#[test]
fn a_cell_can_reach_only_the_names_on_the_open_list() {
    let listed: std::collections::BTreeSet<String> = include_str!("open_globals.txt")
        .lines()
        .map(str::to_string)
        .collect();
    let reachable = reachable();
    assert_eq!(
        (
            reachable.difference(&listed).collect::<Vec<_>>(),
            listed.difference(&reachable).collect::<Vec<_>>(),
        ),
        (Vec::new(), Vec::new()),
        "first: reachable but not listed (close it, or list it); second: listed but gone"
    );
}
