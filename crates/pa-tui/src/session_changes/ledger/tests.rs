use serde_json::json;

use super::{check_kind, CheckKind, Scope};
use crate::chat::ChatEntry;
use crate::session::TranscriptItem;
use crate::theme::{ColorMode, Theme};
use crate::view::AgentView;

/// The rule's matches and misses: wrappers, assignments, chains and paths reach the program; a
/// look-alike word, a script that is neither test nor build, or a test name in an argument do
/// not count.
#[test]
fn the_check_rule_reads_the_program_and_its_subcommand() {
    let commands = [
        "cargo test -p pa-tui",
        "cd crates/pa-tui && RUST_LOG=off timeout -s KILL 600 cargo +1.98.1 test",
        "cargo nextest run",
        "make check",
        "pnpm run test -- --watch=false",
        "/usr/bin/python3 -m pytest tests/",
        "npx vitest run",
        "go vet ./... && go test ./...",
        "cargo clippy --workspace -- -D warnings",
        "yarn build",
        "git diff && cargo build 2>&1 | tail -5",
        "cargo fmt --all --check",
        "npm run lint",
        "echo cargo test",
        "grep -rn pytest .",
        "make",
    ];
    assert_eq!(
        commands.map(check_kind),
        [
            Some(CheckKind::Test),
            Some(CheckKind::Test),
            Some(CheckKind::Test),
            Some(CheckKind::Test),
            Some(CheckKind::Test),
            Some(CheckKind::Test),
            Some(CheckKind::Test),
            Some(CheckKind::Test),
            Some(CheckKind::Build),
            Some(CheckKind::Build),
            Some(CheckKind::Build),
            None,
            None,
            None,
            None,
            None,
        ]
    );
}

fn edit_cell(view: &mut AgentView, path: &str) {
    let id = format!("c{}", view.chat_len());
    view.push(TranscriptItem::ToolCall {
        id: id.clone(),
        name: "eval".to_string(),
        arguments: json!({ "code": "(edit ...)" }).to_string(),
    });
    view.push(TranscriptItem::ToolResult {
        tool_call_id: id,
        tool_name: "eval".to_string(),
        text: "ok".to_string(),
        content: Vec::new(),
        details: json!({ "diffs": [{ "path": path, "diff": "-1 a\n+1 b\n" }] }),
        is_error: false,
    });
}

/// The ledger reads each entry once: after the first summary an append reads only the new
/// entry, an entry changed in place reads only itself again, and a second ask at the same
/// revision reads nothing.
#[test]
fn the_summary_reads_only_the_entries_that_changed() {
    let mut view = AgentView::new(Theme::builtin("prime", ColorMode::TrueColor));
    view.push(TranscriptItem::UserMessage {
        text: "go".to_string(),
    });
    for file in 0..200 {
        edit_cell(&mut view, &format!("/w/{}.rs", file % 7));
    }
    let mut reads = Vec::new();
    let mut ask = |view: &mut AgentView| {
        let before = view.change_ledger.reads;
        let files = view
            .change_ledger
            .summary(&view.chat, Scope::Session)
            .rows
            .len();
        reads.push((view.change_ledger.reads - before, files));
    };
    ask(&mut view);
    edit_cell(&mut view, "/w/new.rs");
    ask(&mut view);
    ask(&mut view);
    let last = view.chat_len() - 1;
    view.prepare_entry_mutation(last);
    view.mark_entry_stale(last);
    ask(&mut view);
    view.push_entry(ChatEntry::Status {
        text: "note".to_string(),
        kind: crate::chat::StatusKind::Info,
    });
    ask(&mut view);
    assert_eq!(reads, vec![(201, 7), (1, 8), (0, 8), (1, 8), (1, 8)]);
}
