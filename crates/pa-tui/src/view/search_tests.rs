use super::HistoryScope;
use crate::chat::MessageBlock;
use crate::session::TranscriptItem;
use crate::theme::{ColorMode, Theme, ThemeBg, ThemeColor};
use crate::view::expansion::tests::finished_tool_card;
use crate::view::AgentView;
use crate::width::str_width;
use proptest::prelude::*;
use ratatui::style::Modifier;

/// The frame height the tests render: the top bar, the transcript window, the dock.
const HEIGHT: usize = 16;

fn view() -> AgentView {
    let mut view = AgentView::new(Theme::builtin("prime", ColorMode::TrueColor));
    view.chrome.version = "0.0.0".to_string();
    view.chrome.cwd = "/w".to_string();
    view.chrome.chat_name = "w".to_string();
    view.onboarding = None;
    view.splash_suppressed = true;
    view
}

fn prompt(view: &mut AgentView, text: &str) {
    view.push(TranscriptItem::UserMessage {
        text: text.to_string(),
    });
}

fn reply(view: &mut AgentView, lines: &[&str]) {
    view.push(TranscriptItem::Assistant {
        blocks: vec![MessageBlock::Text(lines.join("\n\n"))],
        has_tool_calls: false,
    });
}

/// Two turns: each reply names `wait` on some rows, and the second turn's reply runs taller
/// than the window.
fn two_turns() -> AgentView {
    let mut view = view();
    prompt(&mut view, "why does collect hang");
    reply(
        &mut view,
        &[
            "the wait in collect is unbounded",
            "it returns late",
            "fix the wait",
        ],
    );
    prompt(&mut view, "bound it");
    let steps: Vec<String> = (1..=6).map(|step| format!("step {step}")).collect();
    let mut lines: Vec<&str> = steps.iter().map(String::as_str).collect();
    lines.extend(["then wait once", "the Wait is bounded now"]);
    reply(&mut view, &lines);
    view
}

/// One key into the open bar (a space is the `space` key id).
fn key(view: &mut AgentView, id: &str) {
    view.transcript_search_key(id).expect("the bar is open");
}

/// Type `text` into the bar and let the index finish.
fn typed(view: &mut AgentView, text: &str) {
    for c in text.chars() {
        let id = if c == ' ' {
            "space".to_string()
        } else {
            c.to_string()
        };
        key(view, &id);
    }
    while view.transcript_search_pending() {
        view.continue_transcript_search();
    }
}

/// The frame's rows as text, trailing padding trimmed, with the lit matches marked: `[..]` for
/// a match and `{..}` for the current one (plain text loses the colours).
fn marked(view: &mut AgentView, width: usize) -> Vec<String> {
    let current_fg = view.theme.fg_style(ThemeColor::Warning).fg;
    let match_bg = view.theme.bg_style(ThemeBg::SelectedBg).bg;
    view.render_frame(width, HEIGHT)
        .into_iter()
        .map(|mut row| {
            crate::osc133::strip(&mut row);
            crate::hyperlinks::strip_osc8(&mut row);
            let mut out = String::new();
            let mut open: Option<char> = None;
            for span in &row {
                let current = span.style.add_modifier.contains(Modifier::REVERSED)
                    && span.style.fg == current_fg;
                let lit = !current && match_bg.is_some() && span.style.bg == match_bg;
                let mark = if current {
                    Some('{')
                } else if lit {
                    Some('[')
                } else {
                    None
                };
                if mark != open {
                    if let Some(close) = open {
                        out.push(if close == '{' { '}' } else { ']' });
                    }
                    if let Some(mark) = mark {
                        out.push(mark);
                    }
                    open = mark;
                }
                out.push_str(&span.content);
            }
            if let Some(close) = open {
                out.push(if close == '{' { '}' } else { ']' });
            }
            out.trim_end().to_string()
        })
        .collect()
}

fn rows(rows: &[&str]) -> Vec<String> {
    rows.iter().map(ToString::to_string).collect()
}

/// The bar row of the last frame.
fn bar(view: &mut AgentView, width: usize) -> String {
    marked(view, width).pop().unwrap_or_default()
}

/// Typing filters as it goes: the matches on screen are lit, the nearest one above the bottom
/// is current, and the bar counts them.
#[test]
fn typing_lights_the_matches_and_counts_them() {
    let mut view = two_turns();
    marked(&mut view, 80);
    view.open_transcript_search(HistoryScope::Complete);
    typed(&mut view, "wait");
    // The window rows are the same at both widths; the bar drops the previous-match hint first.
    let body = rows(&[
        "",
        " step 2",
        "",
        " step 3",
        "",
        " step 4",
        "",
        " step 5",
        "",
        " step 6",
        "",
        " then [wait] once",
        "",
        " the {Wait} is bounded now",
    ]);
    let framed = |top: usize, bar: &str| {
        [
            vec![format!("{}w", " ".repeat(top))],
            body.clone(),
            vec![bar.to_string()],
        ]
        .concat()
    };
    assert_eq!(
        [marked(&mut view, 80), marked(&mut view, 40)],
        [
            framed(
                39,
                " / wait                                    4 of 4 \u{b7} \u{2193} next \u{b7} \u{2191} prev \u{b7} Esc close"
            ),
            framed(39 - 20, " / wait     4 of 4 \u{b7} \u{2193} next \u{b7} Esc close"),
        ]
    );
}

/// The step keys walk the matches in transcript order and wrap at both ends; a match already in
/// view does not move the window, one out of view lands a third of the way down.
#[test]
fn next_and_previous_land_on_the_match_row_and_wrap() {
    let mut view = two_turns();
    marked(&mut view, 80);
    view.open_transcript_search(HistoryScope::Complete);
    typed(&mut view, "wait");
    let mut landings = Vec::new();
    for id in ["up", "up", "up", "up", "down"] {
        key(&mut view, id);
        let frame = marked(&mut view, 80);
        let row = frame.iter().position(|row| row.contains('{'));
        landings.push((
            frame.last().cloned().unwrap_or_default(),
            row,
            row.map(|row| frame[row].clone()),
        ));
    }
    let landed = |ordinal: usize, row: usize, text: &str| {
        (
            format!(
                " / wait                                    {ordinal} of 4 \u{b7} \u{2193} next \u{b7} \u{2191} prev \u{b7} Esc close"
            ),
            Some(row),
            Some(text.to_string()),
        )
    };
    assert_eq!(
        landings,
        vec![
            // In view already: the window stays at the bottom.
            landed(3, 12, " then {wait} once"),
            // Above the window: the match lands a third of the way down.
            landed(2, 5, " fix the {wait}"),
            // On the window's first row, under the pinned prompt: it lands again.
            landed(1, 5, " the {wait} in collect is unbounded"),
            // Previous from the first wraps to the last, at the bottom.
            landed(4, 14, " the {Wait} is bounded now"),
            // Next from the last wraps to the first.
            landed(1, 5, " the {wait} in collect is unbounded"),
        ]
    );
}

/// A match in rows a collapsed card hides opens the card while it is current, and the card
/// closes again when the current match moves to another entry.
#[test]
fn a_match_in_a_collapsed_card_opens_it_while_current() {
    let mut view = view();
    prompt(&mut view, "run the probe");
    view.push_entry(finished_tool_card("c0", "alpha"));
    reply(&mut view, &["alpha 4 was the culprit"]);
    marked(&mut view, 80);
    view.open_transcript_search(HistoryScope::Complete);
    typed(&mut view, "alpha 4");
    let on_reply = marked(&mut view, 80);
    key(&mut view, "up");
    let in_card = marked(&mut view, 80);
    key(&mut view, "down");
    let back = marked(&mut view, 80);
    let bar = |ordinal: usize| {
        format!(
            " / alpha 4                                 {ordinal} of 2 \u{b7} \u{2193} next \u{b7} \u{2191} prev \u{b7} Esc close"
        )
    };
    let top = format!("{}w", " ".repeat(39));
    assert_eq!(
        (on_reply.clone(), in_card),
        (
            [
                rows(&[
                    &top,
                    "  run the probe",
                    "",
                    "",
                    "  bash \u{b7} done \u{b7} 0.0s",
                    "",
                    "  $ echo hi",
                    "",
                    "  \u{23bf} alpha 1",
                    "    alpha 2",
                    "  \u{22ee} +4 lines",
                    "    alpha 7",
                    "    alpha 8",
                    "",
                    " {alpha 4} was the culprit",
                ]),
                vec![bar(2)],
            ]
            .concat(),
            [
                rows(&[
                    &top,
                    "  bash \u{b7} done \u{b7} 0.0s",
                    "",
                    "  $ echo hi",
                    "",
                    "  \u{23bf} alpha 1",
                    "    alpha 2",
                    "    alpha 3",
                    "    {alpha 4}",
                    "    alpha 5",
                    "    alpha 6",
                    "    alpha 7",
                    "    alpha 8",
                    "",
                    " [alpha 4] was the culprit",
                ]),
                vec![bar(1)],
            ]
            .concat(),
        )
    );
    // Leaving the card for the reply's match closes it again.
    assert_eq!(back, on_reply);
}

/// Esc closes the bar and puts back the exact window it opened over, the entries it opened
/// closed again.
#[test]
fn esc_restores_the_exact_prior_window() {
    let mut view = two_turns();
    view.push_entry(finished_tool_card("c0", "wait"));
    reply(&mut view, &["done"]);
    marked(&mut view, 80);
    view.scroll_by(-5);
    let before = marked(&mut view, 80);
    view.open_transcript_search(HistoryScope::Complete);
    typed(&mut view, "wait");
    for id in ["up", "up", "up"] {
        key(&mut view, id);
    }
    let moved = marked(&mut view, 80);
    key(&mut view, "escape");
    assert!(view.transcript_search.is_none());
    assert_ne!(moved, before);
    assert_eq!(marked(&mut view, 80), before);
}

/// A query that matches nothing says so, and says it searched only the loaded rows when older
/// history lives only in the daemon.
#[test]
fn a_query_without_a_match_says_so() {
    let mut view = two_turns();
    marked(&mut view, 80);
    view.open_transcript_search(HistoryScope::Complete);
    typed(&mut view, "zebra");
    let complete = [bar(&mut view, 80), bar(&mut view, 40)];
    view.set_transcript_search_history(HistoryScope::LoadedOnly);
    let loaded = [bar(&mut view, 80), bar(&mut view, 40)];
    assert_eq!(
        (complete, loaded),
        (
            [
                format!(" / zebra{}no match \u{b7} Esc close", " ".repeat(51)),
                format!(" / zebra{}no match \u{b7} Esc close", " ".repeat(11)),
            ],
            // The scope note keeps its place; the close hint drops first.
            [
                format!(
                    " / zebra{}no match in loaded history \u{b7} Esc close",
                    " ".repeat(33)
                ),
                format!(" / zebra{}no match in loaded history", " ".repeat(5)),
            ]
        )
    );
}

fn wide_text() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z \u{754c}\u{6570}\u{636e}\u{1f642}\u{1f469}\u{200d}\u{1f4bb}e\u{301}\t]{1,80}",
        Just("\u{6570}\u{636e}".repeat(60)),
        Just("\u{1f469}\u{200d}\u{1f4bb}".repeat(40)),
        Just("e\u{301}".repeat(50)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    /// With the bar open over wide, joined and combining text, no frame row is wider than the
    /// frame at 20 to 200 columns, the bar row is exactly as wide, and every lit span starts and
    /// ends on a cluster boundary of its row.
    #[test]
    fn search_rows_never_exceed_the_frame_width(
        text in wide_text(),
        query in prop_oneof![
            Just("\u{6570}".to_string()),
            Just("\u{1f469}".to_string()),
            Just("e".to_string()),
            Just("a".to_string()),
            "[a-z\u{754c}\u{636e}]{1,3}",
        ],
        width in 20usize..=200,
    ) {
        let mut view = view();
        prompt(&mut view, &text);
        reply(&mut view, &[text.as_str(), "plain", text.as_str()]);
        view.push_entry(finished_tool_card("c0", &text));
        view.render_frame(width, HEIGHT);
        view.open_transcript_search(HistoryScope::LoadedOnly);
        view.transcript_search_paste(&query);
        while view.transcript_search_pending() {
            view.continue_transcript_search();
        }
        key(&mut view, "up");
        let match_bg = view.theme.bg_style(ThemeBg::SelectedBg).bg;
        let current_fg = view.theme.fg_style(ThemeColor::Warning).fg;
        let frame = view.render_frame(width, HEIGHT);
        let bar_width = crate::width::spans_width(frame.last().expect("the bar row"));
        prop_assert_eq!(bar_width, width);
        for mut row in frame {
            crate::osc133::strip(&mut row);
            let text: String = row.iter().map(|span| span.content.as_str()).collect();
            prop_assert!(crate::width::spans_width(&row) <= width, "{:?}", row);
            prop_assert!(str_width(&text) <= width, "{:?}", row);
            let boundaries: Vec<usize> =
                unicode_segmentation::UnicodeSegmentation::grapheme_indices(text.as_str(), true)
                    .map(|(at, _)| at)
                    .chain([text.len()])
                    .collect();
            let mut at = 0;
            for span in &row {
                let lit = (match_bg.is_some() && span.style.bg == match_bg)
                    || (span.style.add_modifier.contains(Modifier::REVERSED)
                        && span.style.fg == current_fg);
                let end = at + span.content.len();
                if lit {
                    prop_assert!(
                        boundaries.contains(&at) && boundaries.contains(&end),
                        "{:?}",
                        row
                    );
                }
                at = end;
            }
        }
    }
}
