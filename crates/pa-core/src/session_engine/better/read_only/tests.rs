use super::*;

fn context(cwd: &Path, limit: u32) -> ReadContext {
    ReadContext {
        cwd: cwd.to_path_buf(),
        budget: Arc::new(ReadBudget::new(limit)),
        progress: Arc::new(|_| {}),
    }
}

#[test]
fn a_cell_of_literal_fs_calls_parses_to_its_calls() {
    let calls = parse_repl_cell(
        "# look around\nprint(await fs.read('src/lib.rs', start=1, end=40))\n\
         fs.grep(r\"fn \\w+\", \"src\", glob=[\"*.rs\", \"!target/**\"],\n    case='smart'); \
         str(await fs.find(name=None, kind=\"file\", max_depth=2))",
    )
    .unwrap();
    assert_eq!(
        calls,
        vec![
            FsCall {
                op: FsOp::Read,
                arguments: vec![
                    ("path", Literal::Str("src/lib.rs".to_string())),
                    ("start", Literal::Int(1)),
                    ("end", Literal::Int(40)),
                ],
            },
            FsCall {
                op: FsOp::Grep,
                arguments: vec![
                    ("pattern", Literal::Str("fn \\w+".to_string())),
                    ("path", Literal::Str("src".to_string())),
                    (
                        "glob",
                        Literal::List(vec![
                            Literal::Str("*.rs".to_string()),
                            Literal::Str("!target/**".to_string()),
                        ])
                    ),
                    ("case", Literal::Str("smart".to_string())),
                ],
            },
            FsCall {
                op: FsOp::Find,
                arguments: vec![
                    ("name", Literal::None),
                    ("kind", Literal::Str("file".to_string())),
                    ("max_depth", Literal::Int(2)),
                ],
            },
        ]
    );
}

#[test]
fn anything_but_a_literal_fs_read_is_refused_by_the_parser() {
    for (code, reason) in [
        ("x = await fs.read('a')", "each line must be one fs.read/grep/glob/find call"),
        ("open('a', 'w').write('x')", "each line must be one fs.read/grep/glob/find call"),
        ("import os", "each line must be one fs.read/grep/glob/find call"),
        ("await fs.edit('a', [])", "fs.edit is not a read; only read/grep/glob/find"),
        ("fs.read(path)", "`path` is a name, and only literal arguments are accepted"),
        ("fs.read('a', mode='w')", "fs.read() has no parameter `mode`"),
        ("fs.read('a') + fs.read('b')", "`+` is not accepted here"),
        ("", "the cell holds no fs call"),
    ] {
        assert_eq!(parse_repl_cell(code), Err(reason.to_string()), "{code}");
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
        ("git checkout .", "`git checkout` is not one of the read-only subcommands"),
        ("git -c core.pager=sh log", "`git -c` is not one of the read-only subcommands"),
        ("git log > out.txt", "`>` is a shell operator and is not accepted"),
        ("git log | sh", "`|` is a shell operator and is not accepted"),
        ("git log $(touch x)", "`$` is a shell operator and is not accepted"),
        ("git diff --output=x", "`--output=x` can write a file or start a program"),
        ("git grep -Ovim foo", "`-Ovim` can write a file or start a program"),
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
    let parent: Vec<Arc<dyn AgentTool>> = ["edit", "bash", pa_types::ai::PYTHON_TOOL_NAME]
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
        run(2, json!({ "code": "open('parser.rs', 'w').write('')" })).await,
        run(2, json!({ "code": "await fs.edit('parser.rs', {'append': 'x'})" })).await,
    ];
    assert_eq!(
        refusals.each_ref().map(|text| text.starts_with("Refused: ")),
        [true; 5],
        "{refusals:#?}"
    );
    assert_eq!(refusals[0], EDIT_REFUSED);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn parse() {}\n");
    let read = run(2, json!({ "code": "print(await fs.read('parser.rs'))" })).await;
    assert!(read.contains("|fn parse() {}"), "{read}");
}

#[tokio::test]
async fn reads_past_the_budget_are_refused_with_how_to_answer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    let context = context(dir.path(), 1);
    let text = run_repl_cell("fs.read('a.txt')\nfs.read('a.txt')", &context).await;
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
