use std::path::Path;

use serde_json::json;

use super::*;

fn context(cwd: &Path, limit: u32) -> ReadContext {
    ReadContext {
        cwd: cwd.to_path_buf(),
        budget: Arc::new(ReadBudget::new(limit)),
        progress: Arc::new(|_| {}),
    }
}

#[test]
fn git_commands_parse_without_a_shell_and_carry_the_hardening() {
    assert_eq!(
        git_arguments("git log --oneline -n 5 -- 'src/a b.rs'"),
        Ok([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "diff.autoRefreshIndex=false",
            "--no-optional-locks",
            "--no-pager",
            "-c",
            "core.pager=cat",
            "log",
            "--no-ext-diff",
            "--no-textconv",
            "--oneline",
            "-n",
            "5",
            "--",
            "src/a b.rs",
        ]
        .map(String::from)
        .to_vec())
    );
    for (command, reason) in [
        ("rm -rf src", "`rm` is not read-only git"),
        (
            "git checkout .",
            "`git checkout` is not one of the read-only subcommands",
        ),
        (
            "git -c core.pager=sh log",
            "`git -c` is not one of the read-only subcommands",
        ),
        (
            "git log > out.txt",
            "`>` is a shell operator and is not accepted",
        ),
        (
            "git log | sh",
            "`|` is a shell operator and is not accepted",
        ),
        (
            "git log $(touch x)",
            "`$` is a shell operator and is not accepted",
        ),
        (
            "git diff --output=x",
            "`--output=x` can write a file or start a program",
        ),
        (
            "git grep -Ovim foo",
            "`-Ovim` can write a file or start a program",
        ),
    ] {
        assert_eq!(git_arguments(command), Err(reason.to_string()), "{command}");
    }
}

/// Every write path the helper's tools could reach is refused at the host, and the file on
/// disk is untouched; a read through the same tools still works.
#[tokio::test]
async fn every_write_the_helper_attempts_is_refused_and_nothing_changes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("parser.rs");
    std::fs::write(&file, "fn parse() {}\n").unwrap();
    let parent: Vec<Arc<dyn AgentTool>> = ["edit", "bash", pa_types::ai::KERNEL_TOOL_NAME]
        .into_iter()
        .map(|name| named_tool(name))
        .collect();
    let tools = read_only_tools(&parent, &context(dir.path(), 10));
    let run = |index: usize, params: Value| {
        let tool = Arc::clone(&tools[index]);
        async move {
            let result = tool
                .execute(
                    "call".to_string(),
                    params,
                    AbortSignal::never(),
                    Arc::new(|_| {}),
                )
                .await
                .unwrap();
            serde_json::to_value(&result.content).unwrap()[0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let refusals = [
        run(0, json!({ "path": "parser.rs", "edits": [] })).await,
        run(1, json!({ "command": "echo hi > parser.rs" })).await,
        run(1, json!({ "command": "rm parser.rs" })).await,
        run(2, json!({ "code": "(fs-write \"parser.rs\" \"\")" })).await,
        run(
            2,
            json!({ "code": "(fs-edit \"parser.rs\" (hash 'append \"x\"))" }),
        )
        .await,
        run(2, json!({ "code": "(bash \"rm parser.rs\")" })).await,
    ];
    assert_eq!(
        refusals
            .each_ref()
            .map(|text| text.starts_with("Refused: ")),
        [true; 6],
        "{refusals:#?}"
    );
    assert_eq!(refusals[0], EDIT_REFUSED);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn parse() {}\n");
    let read = run(
        2,
        json!({ "code": "(displayln (hash-ref (fs-read \"parser.rs\") 'text))" }),
    )
    .await;
    assert!(read.contains("|fn parse() {}"), "{read}");
}

#[tokio::test]
async fn reads_past_the_budget_are_refused_with_how_to_answer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    let context = context(dir.path(), 1);
    let text =
        scheme_reads::run_eval_cell("(fs-read \"a.txt\")\n(fs-read \"a.txt\")", &context).await;
    let sections: Vec<&str> = text.split("\n\n").collect();
    assert_eq!(sections.len(), 2, "{text}");
    assert!(sections[0].contains("|a"), "{text}");
    assert_eq!(sections[1], context.budget_spent());
    assert_eq!(context.budget.used(), 1);
}

/// A stand-in parent tool: only its definition matters (running it would mean the helper reached
/// the session's real tool).
pub(in crate::session_engine::better) fn named_tool(name: &'static str) -> Arc<dyn AgentTool> {
    struct Named(&'static str, Value);
    impl AgentTool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &'static str {
            "parent tool"
        }
        fn parameters(&self) -> &Value {
            &self.1
        }
        fn execute(
            self: Arc<Self>,
            _tool_call_id: String,
            _params: Value,
            _signal: AbortSignal,
            _on_update: AgentToolUpdateCallback,
        ) -> BoxFut<'static, anyhow::Result<AgentToolResult>> {
            Box::pin(async { panic!("the parent tool never runs inside the helper") })
        }
    }
    Arc::new(Named(name, json!({ "type": "object" })))
}
