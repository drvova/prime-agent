//! The panel's rows from a transcript the view holds: one test per behaviour, comparing the
//! pane's whole rows as text.

use proptest::prelude::*;
use serde_json::{json, Value};

use super::{ChangesKey, ChangesPanel, LoadedHistory};
use crate::keybindings::{KeybindingsConfig, KeybindingsManager};
use crate::session::TranscriptItem;
use crate::theme::{ColorMode, Theme};
use crate::view::AgentView;
use crate::Line;

const CWD: &str = "/w";
const HOST: &str = "/w/crates/pa-daemon/src/rlm_children/host.rs";
const LIFECYCLE: &str = "/w/crates/pa-daemon/src/rlm_children/lifecycle.rs";
/// The pane's row budget in these tests: room for every row they list.
const ROWS: usize = 16;

fn text(line: &Line) -> String {
    line.iter()
        .map(|span| span.content.as_str())
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn rows(rows: &[&str]) -> Vec<String> {
    rows.iter().map(ToString::to_string).collect()
}

fn view() -> AgentView {
    AgentView::new(Theme::builtin("prime", ColorMode::TrueColor))
}

/// One edit's diff as the edit tool numbers it: `removed` old rows, then `added` new ones.
fn diff(line: usize, removed: usize, added: usize) -> String {
    std::iter::once(format!(" {} context\n", line.saturating_sub(1)))
        .chain(std::iter::repeat_n(format!("-{line} old row\n"), removed))
        .chain(std::iter::repeat_n(format!("+{line} new row\n"), added))
        .collect()
}

fn prompt(view: &mut AgentView, text: &str) {
    view.push(TranscriptItem::UserMessage {
        text: text.to_string(),
    });
}

/// One settled kernel cell with these card `details`.
fn cell(view: &mut AgentView, details: Value) {
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
        details,
        is_error: false,
    });
}

/// A cell whose card shows these `(path, diff)` edits.
fn edits(view: &mut AgentView, edits: &[(&str, String)]) {
    let diffs: Vec<Value> = edits
        .iter()
        .map(|(path, diff)| json!({ "path": path, "diff": diff }))
        .collect();
    cell(view, json!({ "status": "ok", "diffs": diffs }));
}

/// A cell that waited for one test run and printed the kernel's summary line.
fn test_run(view: &mut AgentView, failed: u64) {
    cell(
        view,
        json!({
            "status": "ok",
            "stdout": format!("[tests: cargo \u{b7} 3 passed \u{b7} {failed} failed]\n"),
            "bashCommands": { "first": "cargo test -p pa-daemon", "count": 1, "lines": 1 },
        }),
    );
}

fn render(view: &mut AgentView, panel: &mut ChangesPanel, width: usize) -> Vec<String> {
    let kb = KeybindingsManager::new();
    render_with(view, panel, width, &kb)
}

fn render_with(
    view: &mut AgentView,
    panel: &mut ChangesPanel,
    width: usize,
    kb: &KeybindingsManager,
) -> Vec<String> {
    panel
        .render(
            &mut view.change_ledger,
            &view.chat,
            &view.theme,
            width,
            kb,
            CWD,
            ROWS,
        )
        .iter()
        .map(text)
        .collect()
}

fn key(view: &mut AgentView, panel: &mut ChangesPanel, id: &str) -> ChangesKey {
    panel.handle_key(
        id,
        &KeybindingsManager::new(),
        &mut view.change_ledger,
        &view.chat,
    )
}

/// Two files edited across three cells: one row each, the most recently changed first, with the
/// summed rows, the edit count, and the paths relative to the session's directory; on a 40-column
/// pane the edits column drops and the path is cut from the left so its file name stays.
#[test]
fn several_edits_to_two_files_make_one_row_per_file() {
    let mut view = view();
    prompt(&mut view, "move the roster read into host.rs");
    edits(&mut view, &[(HOST, diff(4, 1, 1)), (HOST, diff(9, 1, 2))]);
    edits(&mut view, &[(LIFECYCLE, diff(2, 0, 1))]);
    edits(&mut view, &[(HOST, diff(12, 1, 1))]);
    let mut panel = ChangesPanel::new(LoadedHistory::Whole);
    assert_eq!(
        [render(&mut view, &mut panel, 80), render(&mut view, &mut panel, 40)],
        [
            rows(&[
                "  Changes \u{b7} this turn",
                "  2 files \u{b7} +5 -3",
                "",
                format!(
                    "\u{203a} {:<46}  +4 -3  3 edits  no test after",
                    "crates/pa-daemon/src/rlm_children/host.rs"
                )
                .as_str(),
                "  crates/pa-daemon/src/rlm_children/lifecycle.rs  +1 -0  1 edit   no test after",
                "",
                "  \u{2191}/\u{2193} navigate \u{b7} A attach \u{b7} T session \u{b7} Enter/\u{2192} diff \u{b7} \u{2190}/Esc close",
                "",
            ]),
            rows(&[
                "  Changes \u{b7} this turn",
                "  2 files \u{b7} +5 -3",
                "",
                "\u{203a} \u{2026}hildren/host.rs  +4 -3  no test after",
                "  \u{2026}en/lifecycle.rs  +1 -0  no test after",
                "",
                "  Enter/\u{2192} diff \u{b7} \u{2190}/Esc close",
                "",
            ]),
        ]
    );
}

/// The user's own `!` command.
fn user_command(view: &mut AgentView, command: &str, exit_code: i64) {
    view.push(TranscriptItem::BashExecution {
        command: command.to_string(),
        output: String::new(),
        exit_code: Some(exit_code),
        cancelled: false,
        truncated: false,
        full_output_path: None,
        excluded: false,
    });
}

/// A background command's `[bash-done]` row.
fn bash_done(view: &mut AgentView, command: &str, exit_code: i64) {
    view.push(TranscriptItem::CustomRow {
        entry: crate::chat::ChatEntry::BackgroundCompletion(Box::new(
            crate::custom_message::BackgroundCompletionRow {
                outcome: crate::custom_message::BackgroundOutcome::Shell {
                    pid: Some(7),
                    exit_code: Some(exit_code),
                },
                content: format!(
                    "[bash-done pid:7 exit:{exit_code}]\n\nCommand: {}\n\nOutput: (none)",
                    json!(command)
                ),
            },
        )),
    });
}

/// The evidence column reads the checks after the file's last edit: the kernel's passing test
/// summary, the user's failing `!` test, no test when the only run came before the last edit,
/// a `[bash-done]` build, and a failed test that a later passing build does not hide.
#[test]
fn the_evidence_column_reads_the_checks_after_the_last_edit() {
    let evidence = |checks: &dyn Fn(&mut AgentView)| {
        let mut view = view();
        prompt(&mut view, "fix it");
        edits(&mut view, &[("/w/x.rs", diff(1, 1, 1))]);
        checks(&mut view);
        render(&mut view, &mut ChangesPanel::new(LoadedHistory::Whole), 80)[3].clone()
    };
    assert_eq!(
        [
            evidence(&|view| test_run(view, 0)),
            evidence(&|view| user_command(view, "timeout 600 cargo test", 101)),
            evidence(&|view| {
                test_run(view, 0);
                edits(view, &[("/w/x.rs", diff(7, 0, 1))]);
            }),
            evidence(&|view| bash_done(view, "cd crates && cargo build", 0)),
            evidence(&|view| {
                user_command(view, "cargo test", 101);
                bash_done(view, "cargo build", 0);
            }),
        ],
        [
            "\u{203a} x.rs  +1 -1  1 edit  tests ran after \u{2713}",
            "\u{203a} x.rs  +1 -1  1 edit  tests failed after \u{2717}",
            "\u{203a} x.rs  +2 -1  2 edits  no test after",
            "\u{203a} x.rs  +1 -1  1 edit  built after \u{2713}",
            "\u{203a} x.rs  +1 -1  1 edit  tests failed after \u{2717}",
        ]
        .map(str::to_string)
    );
}

/// The scope key switches between the turn's files and the session's, and back.
#[test]
fn the_scope_key_switches_between_the_turn_and_the_session() {
    let mut view = view();
    prompt(&mut view, "first");
    edits(&mut view, &[("/w/old.rs", diff(1, 1, 1))]);
    prompt(&mut view, "second");
    edits(&mut view, &[("/w/new.rs", diff(1, 0, 2))]);
    let mut panel = ChangesPanel::new(LoadedHistory::Whole);
    let turn = render(&mut view, &mut panel, 60);
    let switched = key(&mut view, &mut panel, "t");
    let session = render(&mut view, &mut panel, 60);
    key(&mut view, &mut panel, "t");
    assert_eq!(
        (
            turn[..4].to_vec(),
            switched,
            session[..5].to_vec(),
            render(&mut view, &mut panel, 60),
        ),
        (
            rows(&[
                "  Changes \u{b7} this turn",
                "  1 file \u{b7} +2 -0",
                "",
                "\u{203a} new.rs  +2 -0  1 edit  no test after",
            ]),
            ChangesKey::Scoped,
            rows(&[
                "  Changes \u{b7} whole session",
                "  2 files \u{b7} +3 -1",
                "",
                "\u{203a} new.rs  +2 -0  1 edit  no test after",
                "  old.rs  +1 -1  1 edit  no test after",
            ]),
            turn,
        )
    );
}

/// Enter shows the file's edits through the tool cards' own diff renderer, under the file's
/// title and counts.
#[test]
fn enter_shows_the_files_diff_through_the_card_renderer() {
    let mut view = view();
    prompt(&mut view, "edit it twice");
    edits(&mut view, &[(HOST, diff(4, 1, 1))]);
    edits(&mut view, &[("/w/other.rs", diff(1, 1, 1))]);
    edits(&mut view, &[(HOST, diff(9, 1, 2))]);
    let mut panel = ChangesPanel::new(LoadedHistory::Whole);
    let opened = key(&mut view, &mut panel, "enter");
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let mut card = crate::tool_card::layout::RowOutput::paint();
    crate::tool_card::ipython::diffs::render_diffs(
        &mut card,
        &[
            json!({ "path": HOST, "diff": diff(4, 1, 1) }),
            json!({ "path": HOST, "diff": diff(9, 1, 2) }),
        ],
        /*has_code*/ false,
        /*expanded*/ true,
        &theme,
        80,
    );
    let mut expected = rows(&[
        "  Changes \u{203a} crates/pa-daemon/src/rlm_children/host.rs",
        "  +3 -2 \u{b7} 2 edits \u{b7} no test after",
        "",
    ]);
    expected.extend(card.into_lines().iter().map(text));
    expected.extend(rows(&[
        "",
        "  \u{2191}/\u{2193} scroll \u{b7} A attach \u{b7} \u{2190} back \u{b7} Esc close",
        "",
    ]));
    assert_eq!(
        (opened, render(&mut view, &mut panel, 80)),
        (ChangesKey::OpenedDiff, expected)
    );
}

/// With no edits the pane says so, and offers only the keys that do something: the scope
/// switch and the close keys.
#[test]
fn an_empty_scope_offers_only_the_scope_switch_and_close() {
    let mut view = view();
    prompt(&mut view, "just answer");
    let mut panel = ChangesPanel::new(LoadedHistory::Whole);
    assert_eq!(
        (
            render(&mut view, &mut panel, 80),
            key(&mut view, &mut panel, "enter"),
            key(&mut view, &mut panel, "a"),
        ),
        (
            rows(&[
                "  Changes \u{b7} this turn",
                "",
                "",
                "  No file edits in this turn",
                "",
                "  T session \u{b7} \u{2190}/Esc close",
                "",
            ]),
            ChangesKey::Handled,
            ChangesKey::Handled,
        )
    );
}

/// The session scope of a transcript whose start the window has not loaded says its rows come
/// from the loaded history.
#[test]
fn a_partly_loaded_session_says_its_rows_are_from_loaded_history() {
    let mut view = view();
    edits(&mut view, &[("/w/a.rs", diff(1, 1, 1))]);
    let mut panel = ChangesPanel::new(LoadedHistory::Partial);
    key(&mut view, &mut panel, "t");
    assert_eq!(
        render(&mut view, &mut panel, 80)[..2].to_vec(),
        rows(&[
            "  Changes \u{b7} whole session",
            "  1 file \u{b7} +1 -1 \u{b7} from loaded history",
        ])
    );
}

/// A rebound scope key moves both the handler and the footer's hint: the default no longer
/// switches, the new key does, and the footer names it.
#[test]
fn a_rebound_scope_key_moves_its_handler_and_hint() {
    let mut bindings = KeybindingsConfig::new();
    bindings.insert(super::SCOPE_BINDING.to_string(), vec!["s".to_string()]);
    let kb = KeybindingsManager::with_user_bindings(bindings);
    let mut view = view();
    prompt(&mut view, "nothing yet");
    let mut panel = ChangesPanel::new(LoadedHistory::Whole);
    let press = |view: &mut AgentView, panel: &mut ChangesPanel, id: &str| {
        panel.handle_key(id, &kb, &mut view.change_ledger, &view.chat)
    };
    let old = press(&mut view, &mut panel, "t");
    let new = press(&mut view, &mut panel, "s");
    assert_eq!(
        (
            old,
            new,
            render_with(&mut view, &mut panel, 80, &kb)[5].clone()
        ),
        (
            ChangesKey::Handled,
            ChangesKey::Scoped,
            "  S turn \u{b7} \u{2190}/Esc close".to_string(),
        )
    );
}

proptest! {
    /// No row of the list or of a file's diff is wider than the pane, for paths of wide,
    /// combining and joined characters at 40 to 200 columns.
    #[test]
    fn no_row_is_wider_than_the_pane(
        width in 40usize..=200,
        segments in proptest::collection::vec(
            proptest::sample::select(vec![
                "src", "数据库", "e\u{301}cole", "👩\u{200d}💻", "very_long_directory_name", "ü",
            ]),
            1..12,
        ),
        added in 0usize..300,
        failed in proptest::bool::ANY,
    ) {
        let path = format!("/w/{}.rs", segments.join("/"));
        let mut view = view();
        prompt(&mut view, "go");
        edits(&mut view, &[(path.as_str(), diff(3, 1, added % 40)), ("/w/b.rs", diff(1, added, 0))]);
        test_run(&mut view, u64::from(failed));
        edits(&mut view, &[("/w/b.rs", diff(1, 1, 1))]);
        let mut panel = ChangesPanel::new(LoadedHistory::Partial);
        let kb = KeybindingsManager::new();
        let mut widths: Vec<usize> = Vec::new();
        for id in ["t", "down", "enter"] {
            let rows = panel.render(&mut view.change_ledger, &view.chat, &view.theme, width, &kb, CWD, ROWS);
            widths.extend(rows.iter().map(|row| crate::width::line_width(row)));
            panel.handle_key(id, &kb, &mut view.change_ledger, &view.chat);
        }
        let rows = panel.render(&mut view.change_ledger, &view.chat, &view.theme, width, &kb, CWD, ROWS);
        widths.extend(rows.iter().map(|row| crate::width::line_width(row)));
        prop_assert!(widths.iter().all(|row| *row <= width), "{widths:?} at {width}");
    }
}
