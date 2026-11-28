use std::sync::{Arc, Mutex};

use serde_json::json;

use super::{CallKind, CallPolicy, CallRefused, NativeFamily};
use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::tools::kernel_cell::{KernelExecuteOptions, KernelProvisioner};

/// Refuses `bash` and the `echo` request, admits the rest, and keeps what
/// it was asked.
#[derive(Default)]
struct RefusesBashAndEcho {
    asked: Mutex<Vec<String>>,
}

impl CallPolicy for RefusesBashAndEcho {
    fn admit(&self, call: CallKind<'_>) -> Result<(), CallRefused> {
        let (name, refused) = match call {
            CallKind::HostRequest { kind, .. } => (kind, kind == "echo"),
            CallKind::Native { family, name } => (name, family == NativeFamily::Bash),
        };
        self.asked.lock().unwrap().push(name.to_string());
        if refused {
            Err(CallRefused {
                call: name.to_string(),
            })
        } else {
            Ok(())
        }
    }
}

/// A refused call is its cell's error and never runs: a command is not
/// started, a handler is not reached, and a macro body, which runs in the
/// engine Steel's compiler keeps, reaches no native at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_call_is_the_cells_error_and_never_runs() {
    let dir = tempfile::tempdir().unwrap();
    let made = dir.path().join("made");
    let answered = Arc::new(Mutex::new(0));
    let mut handlers = HostRequestHandlers::new();
    handlers.register("echo", {
        let answered = Arc::clone(&answered);
        host_handler(move |_payload| {
            *answered.lock().unwrap() += 1;
            async { Ok(json!("echoed")) }
        })
    });
    let policy = Arc::new(RefusesBashAndEcho::default());
    let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        host_handlers: handlers,
        cwd: dir.path().to_path_buf(),
        call_policy: Some(Arc::clone(&policy) as Arc<dyn CallPolicy>),
        ..SchemeKernelOptions::default()
    })
    .ensure(None, None)
    .await
    .unwrap();
    let mut errors = Vec::new();
    for code in [
        "(bash \"touch made\")".to_string(),
        "(%host-request \"echo\" (hash))".to_string(),
        "(defmacro (escape stx) (bash \"touch made\") ''1) (escape 1)".to_string(),
    ] {
        let result = kernel
            .execute(
                &code,
                KernelExecuteOptions {
                    signal: None,
                    timeout_ms: None,
                    on_stream: None,
                    on_late_sent_agent_message: None,
                    tool_call_id: None,
                },
            )
            .await
            .unwrap();
        errors.push(result.error.map(|error| error.evalue));
    }
    let refused = |text: &Option<String>, call: &str| {
        text.as_deref()
            .is_some_and(|text| text.contains(&format!("refused for an untrusted turn: {call}")))
    };
    let asked = policy.asked.lock().unwrap().clone();
    assert_eq!(
        (
            refused(&errors[0], "bash"),
            refused(&errors[1], "echo"),
            errors[2].is_some(),
            made.exists(),
            *answered.lock().unwrap(),
            asked.iter().filter(|name| *name == "bash").count(),
            asked.contains(&"echo".to_string()),
        ),
        (true, true, true, false, 0, 1, true),
        "errors: {errors:?}, asked: {asked:?}"
    );
}
