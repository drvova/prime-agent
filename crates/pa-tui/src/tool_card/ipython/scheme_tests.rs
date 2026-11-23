//! The kernel card for Scheme (`eval`) cells: what a cell's printed value
//! becomes on the card, what its title says of how it ended, and the
//! properties every render holds for any cell, result and width.

use super::*;
use crate::theme::{ColorMode, Theme};
use proptest::prelude::*;
use serde_json::json;

fn theme() -> Theme {
    Theme::builtin("prime", ColorMode::TrueColor)
}

/// A settled `eval` cell.
fn cell(code: &str, details: Value) -> ToolCallCard {
    ToolCallCard {
        id: "toolu_1".into(),
        name: "eval".into(),
        args: json!({ "code": code }),
        started: true,
        result: Some(crate::tool_card::ToolResultView {
            content: vec![],
            details,
            is_error: false,
        }),
        ..Default::default()
    }
}

/// The card's rows as text, trailing padding dropped.
fn rows(card: &ToolCallCard, detail: Detail, width: usize) -> Vec<String> {
    crate::tool_card::render_tool_card(card, 0, detail, detail, &theme(), width, true)
        .iter()
        .map(|row| {
            let text: String = row.iter().map(|span| span.content.as_str()).collect();
            text.trim_end().to_string()
        })
        .collect()
}

/// A hash as the kernel prints it: one `'key: value,` row per field.
fn printed_hash(fields: &[(&str, &str)]) -> String {
    let rows: Vec<String> = fields
        .iter()
        .map(|(key, value)| format!("    '{key}: {value},\n"))
        .collect();
    format!("#<hashmap {{\n{}}}>", rows.concat())
}

/// A cell that ends in a command shows what the command printed and how it
/// exited, not the reply's printed hash: the output was one escaped string
/// inside the dump, and a failing command's card read as a success.
#[test]
fn a_command_cell_shows_its_output_and_its_exit_code() {
    let card = cell(
        "(bash \"cargo test -p pa-tui\")",
        json!({
            "status": "ok",
            "durationMs": 5300,
            "result": printed_hash(&[
                ("running", "#false"),
                ("exit-code", "101"),
                ("output", "\"running 2 tests\\ntest a ... FAILED\\n\""),
                ("timed-out", "#false"),
                ("job", "\"j3\""),
                ("pid", "4242"),
            ]),
        }),
    );
    assert_eq!(
        rows(&card, Detail::All, 80),
        vec![
            " \u{2717} bash \u{b7} cargo test -p pa-tui \u{b7} \u{2191} 1 \u{2193} 2 lines \u{b7} 5.3s \u{b7} exit 101",
            " \u{2570}\u{2500} (bash \"cargo test -p pa-tui\")",
            "",
            "  \u{203a} running 2 tests",
            "    test a ... FAILED",
        ]
    );
}

/// A reply the card already shows is not printed under it again: an edit's
/// confirmation beside its diff, a sent message's receipt beside its row.
/// The Python card left both out by their Python reprs, which a printed
/// Scheme hash never matched.
#[test]
fn a_value_the_card_already_shows_is_not_printed_again() {
    let edit = cell(
        "(edit \"src/app.rs\" \"x = 1\" \"x = 2\")",
        json!({
            "status": "ok",
            "durationMs": 5,
            "result": printed_hash(&[
                ("path", "\"/work/src/app.rs\""),
                ("start-line", "10"),
                ("text", "\"Edited /work/src/app.rs\""),
            ]),
            "diffs": [{ "path": "/work/src/app.rs", "diff": "-10 x = 1\n+10 x = 2" }],
        }),
    );
    let sent = cell(
        "(agent-message-send \"Ping.\" #:receiver-role \"child\" #:receiver-name \"worker\")",
        json!({
            "status": "ok",
            "durationMs": 3,
            "result": printed_hash(&[
                ("id", "\"agentmsg_1\""),
                ("delivery-status", "\"delivered\""),
            ]),
            "sentAgentMessages": [{
                "id": "agentmsg_1",
                "message": "Ping.",
                "deliveryStatus": "delivered",
                "target": { "activeSessionId": "a", "sessionId": "s", "sessionName": "worker" },
            }],
        }),
    );
    assert_eq!(
        (rows(&edit, Detail::All, 60), rows(&sent, Detail::All, 60)),
        (
            vec![
                " \u{2713} scheme \u{b7} edit src/app.rs \u{b7} \u{2191} 1 lines \u{b7} 5ms".to_string(),
                " \u{2570}\u{2500} (edit \"src/app.rs\" \"x = 1\" \"x = 2\")".to_string(),
                String::new(),
                "    \u{2570}\u{2500} /work/src/app.rs +1 -1".to_string(),
                "  10 - x = 1".to_string(),
                "  10 + x = 2".to_string(),
            ],
            vec![
                " \u{2713} scheme \u{b7} agent-message-send Ping. \u{b7} \u{2191} 1 lines \u{b7} 3ms".to_string(),
                " \u{2570}\u{2500} (agent-message-send \"Ping.\" #:receiver-role \"child\"".to_string(),
                "    #:receiver-name \"worker\")".to_string(),
                String::new(),
                " \u{2709} Agent message \u{b7} \u{2191} worker".to_string(),
                " \u{2570}\u{2500} Ping.".to_string(),
            ],
        )
    );
}

/// A cell whose work goes on after it returned says so on its title: the
/// cell itself moved to the background, or the command it left running.
/// Both read as a finished success before.
#[test]
fn a_cell_whose_work_goes_on_is_not_titled_done() {
    let moved = cell(
        "(rlm-collect workers #:timeout-ms 600000)",
        json!({ "status": "backgrounded", "backgroundCell": "ab12", "durationMs": 30000 }),
    );
    let job = cell(
        "(bash \"npm run dev\" #:background #t)",
        json!({
            "status": "ok",
            "durationMs": 12,
            "result": printed_hash(&[
                ("job", "\"j4\""),
                ("pid", "5151"),
                ("command", "\"npm run dev\""),
                ("running", "#true"),
            ]),
        }),
    );
    assert_eq!(
        (
            rows(&moved, Detail::Overview, 90),
            rows(&job, Detail::Overview, 90)
        ),
        (
            vec![" \u{21bb} scheme \u{b7} rlm-collect workers \u{b7} \u{2191} 1 lines \u{b7} 30.0s \u{b7} in background".to_string()],
            vec![" \u{21bb} bash \u{b7} npm dev \u{b7} \u{2191} 1 lines \u{b7} 12ms \u{b7} command still running".to_string()],
        )
    );
}

/// A failed Scheme cell's title says what failed. Every Scheme failure has
/// the one error name, which is all the title said; and the preview gives
/// way so the duration and the error stay on the row, where a long first
/// line used to push them off the edge.
#[test]
fn a_failed_cell_is_titled_by_its_message_and_keeps_its_duration() {
    let message = "Error: FreeIdentifier: Cannot reference an identifier before its definition: fs-raed\n  --> cell:1:2";
    let card = cell(
        "(fs-raed \"crates/pa-tui/src/tool_card/ipython.rs\" (hash 'start 1 'end 400))",
        json!({
            "status": "error",
            "durationMs": 2,
            "error": { "ename": "SchemeError", "evalue": message, "traceback": [message] },
        }),
    );
    assert_eq!(
        rows(&card, Detail::Overview, 80),
        vec![" \u{2717} scheme \u{b7} fs-raed crates\u{2026} \u{b7} \u{2191} 1 lines \u{b7} 2ms \u{b7} FreeIdentifier: Cannot referenc\u{2026}"]
    );
}

/// An edit's rows are labelled by what they are and show what changed: a
/// carriage return where the edit changes line endings, the whitespace a
/// changed row ends in, and no row at all for the diff text's own end. A
/// line-ending or trailing-space edit used to read as two identical rows.
#[test]
fn a_diff_shows_whitespace_and_line_ending_changes() {
    let card = cell(
        "(fs-edit \"notes.txt\" edits)",
        json!({
            "status": "ok",
            "diffs": [
                { "path": "notes.txt", "diff": " 1 title\r\n-2 body\r\n+2 body\n" },
                { "path": "notes.txt", "diff": "-7 done \t\n+7 done" },
                { "path": "same.txt", "diff": "" },
            ],
        }),
    );
    assert_eq!(
        rows(&card, Detail::Details, 40)[1..],
        [
            "    \u{2570}\u{2500} notes.txt +2 -2",
            "  1   title",
            "  2 - body\u{240d}",
            "  2 + body",
            "  \u{22ee}",
            "  7 - done\u{b7}\u{2192}",
            "  7 + done",
            "    \u{2570}\u{2500} same.txt +0 -0",
        ]
    );
}

/// Source and output text for the properties: Scheme fragments, wide and
/// combining characters, emoji joined by ZWJ, tabs, carriage returns,
/// escape sequences, control characters, and very long words.
fn soup() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        6 => "[a-z0-9 ()\\[\\]\"';#|:.\\-\\\\]{0,24}",
        2 => prop::sample::select(vec![
            "(bash \"cargo test\")", "(fs-read \"a.rs\")", "(define x ", "#| block", "\"open",
            "数据数据", "e\u{301}", "🙂", "👨\u{200d}👩\u{200d}👧", "🇺🇸", "\t", "\r", "\n", "\n\n",
            "\u{1b}[31mred\u{1b}[0m", "\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}", "\u{7}\u{0}\u{8}",
            "\u{200b}", "#<hashmap {\n    'output: \"a\\nb\",\n    'job: #false,\n    'pid: 0,\n    'running: #false,\n}>",
        ]).prop_map(str::to_owned),
        1 => "[a-z数]{30,90}",
    ];
    prop::collection::vec(piece, 0..10).prop_map(|pieces| pieces.concat())
}

fn diff_text() -> impl Strategy<Value = String> {
    let row = (
        prop::sample::select(vec!["+", "-", " ", "", "\\", "@"]),
        prop::sample::select(vec!["1", " 12", "   ", "", "007"]),
        prop::sample::select(vec![" ", "", "  "]),
        soup(),
    )
        .prop_map(|(sign, number, gap, text)| format!("{sign}{number}{gap}{text}"));
    prop::collection::vec(row, 0..8).prop_map(|rows| rows.join("\n"))
}

fn details() -> impl Strategy<Value = Value> {
    (
        prop::sample::select(vec!["ok", "error", "aborted", "backgrounded", "starting"]),
        prop::option::of(soup()),
        prop::option::of(soup()),
        prop::option::of(soup()),
        prop::collection::vec((soup(), diff_text()), 0..3),
        prop::option::of((soup(), soup())),
        prop::option::of(0u32..100_000),
        prop::option::of((soup(), 0u32..4, 0u32..6)),
    )
        .prop_map(
            |(status, stdout, stderr, result, diffs, error, duration, bash)| {
                let mut details = json!({ "status": status });
                for (key, value) in [("stdout", stdout), ("stderr", stderr), ("result", result)] {
                    if let Some(value) = value {
                        details[key] = json!(value);
                    }
                }
                if !diffs.is_empty() {
                    details["diffs"] = diffs
                        .into_iter()
                        .map(|(path, diff)| json!({ "path": path, "diff": diff }))
                        .collect();
                }
                if let Some((ename, evalue)) = error {
                    details["error"] =
                        json!({ "ename": ename, "evalue": evalue, "traceback": [evalue] });
                }
                if let Some(duration) = duration {
                    details["durationMs"] = json!(duration);
                }
                if let Some((first, count, lines)) = bash {
                    details["bashCommands"] =
                        json!({ "first": first, "count": count, "lines": lines });
                }
                details
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 384,
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x5343_4845),
        ..ProptestConfig::default()
    })]

    /// Whatever the cell (a saved Python one too), its result and the terminal: the render does not
    /// panic, no row is wider than the terminal, the geometry pass counts
    /// the rows the paint draws, and the same card paints the same rows.
    #[test]
    fn any_cell_renders_within_the_terminal(
        name in prop::sample::select(vec!["eval", "ipython"]),
        code in soup(),
        details in details(),
        text in soup(),
        (is_error, partial, aborted, started) in any::<(bool, bool, bool, bool)>(),
        detail in prop_oneof![Just(Detail::Overview), Just(Detail::Details), Just(Detail::All)],
        width in prop_oneof![0usize..=20, 0usize..=220],
        color in prop::sample::select(vec![ColorMode::TrueColor, ColorMode::Color256]),
    ) {
        let theme = Theme::builtin("prime", color);
        let card = ToolCallCard {
            name: name.into(),
            started,
            aborted,
            result_partial: partial,
            result: Some(crate::tool_card::ToolResultView {
                content: vec![json!({ "type": "text", "text": text })],
                details,
                is_error,
            }),
            ..cell(&code, Value::Null)
        };
        let painted = crate::tool_card::render_tool_card(&card, 1, detail, detail, &theme, width, true);
        let widest = painted.iter().map(|row| crate::width::line_width(row)).max().unwrap_or(0);
        prop_assert!(widest <= width, "a row is {widest} columns in a {width}-column terminal");
        prop_assert_eq!(
            crate::tool_card::count_tool_card(&card, 1, detail, detail, &theme, width, true),
            painted.len()
        );
        prop_assert_eq!(
            crate::tool_card::render_tool_card(&card, 1, detail, detail, &theme, width, true),
            painted
        );
    }

    /// A card does not move when its cell settles on what it was already
    /// showing: streaming a cell's output and then ending with that output
    /// keeps the title's label and preview and the row count. (From 30
    /// columns: narrower, the `waiting for output...` and `no output`
    /// placeholders of a silent cell wrap to different heights.)
    #[test]
    fn a_card_keeps_its_shape_when_its_cell_settles(
        code in soup(),
        output in soup(),
        detail in prop_oneof![Just(Detail::Overview), Just(Detail::Details), Just(Detail::All)],
        width in 30usize..=160,
    ) {
        let running = ToolCallCard {
            result_partial: true,
            result: Some(crate::tool_card::ToolResultView {
                content: vec![json!({ "type": "text", "text": output })],
                details: json!({ "status": "ok" }),
                is_error: false,
            }),
            ..cell(&code, Value::Null)
        };
        let settled = cell(&code, json!({ "status": "ok", "stdout": output }));
        // The title up to its line counts: the marker is the state and differs.
        let title = |card: &ToolCallCard| {
            let row = rows(card, detail, 400).swap_remove(0);
            let named = row.split(" \u{b7} \u{2191}").next().unwrap_or_default().to_string();
            named.chars().skip(2).collect::<String>()
        };
        prop_assert_eq!(title(&running), title(&settled));
        prop_assert_eq!(rows(&running, detail, width).len(), rows(&settled, detail, width).len());
    }
}
