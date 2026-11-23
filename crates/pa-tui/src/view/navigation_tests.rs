use super::{follow_pill_label, pinned_prompt_row, PromptJump};
use crate::chat::MessageBlock;
use crate::keybindings::format_key_text;
use crate::menu_chrome::{Footer, HintRank};
use crate::session::TranscriptItem;
use crate::theme::{ColorMode, Theme};
use crate::view::AgentView;
use crate::width::str_width;
use crate::Line;
use proptest::prelude::*;

const FIRST_PROMPT: &str = "fix the flaky collect test and tell me what caused it";
const SECOND_PROMPT: &str = "now bound the wait and add a regression test";
const THIRD_PROMPT: &str = "run the whole suite once more";

/// The frame height the tests render: the top bar, a 9-row transcript window, the dock.
const HEIGHT: usize = 16;
const WINDOW: std::ops::RangeInclusive<usize> = 1..=9;

fn row_text(line: &Line) -> String {
    line.iter().map(|span| span.content.as_str()).collect()
}

/// The transcript window's rows as text, trailing padding trimmed.
fn window(view: &mut AgentView, width: usize) -> Vec<String> {
    crate::app::render_frame_text(
        view,
        u16::try_from(width).expect("width"),
        u16::try_from(HEIGHT).expect("height"),
    )[WINDOW]
        .iter()
        .map(|row| row.trim_end().to_string())
        .collect()
}

fn turn(view: &mut AgentView, prompt: &str) {
    view.push(TranscriptItem::UserMessage {
        text: prompt.to_string(),
    });
    let word = prompt.split(' ').next().unwrap_or_default();
    let reply: Vec<String> = (1..=9).map(|row| format!("{word} step {row}")).collect();
    view.push(TranscriptItem::Assistant {
        blocks: vec![MessageBlock::Text(reply.join("\n\n"))],
        has_tool_calls: false,
    });
}

/// Three turns, each taller than the transcript window.
fn long_run(prompts: [&str; 3]) -> AgentView {
    let mut view = AgentView::new(Theme::builtin("prime", ColorMode::TrueColor));
    view.chrome.version = "0.0.0".to_string();
    view.chrome.cwd = "/w".to_string();
    view.chrome.chat_name = "w".to_string();
    view.onboarding = None;
    view.splash_suppressed = true;
    for prompt in prompts {
        turn(&mut view, prompt);
    }
    view
}

fn note(view: &mut AgentView) {
    view.push(TranscriptItem::SystemNote {
        text: "note".to_string(),
    });
}

fn rows(rows: &[&str]) -> Vec<String> {
    rows.iter().map(ToString::to_string).collect()
}

/// The pill's key label on this host.
fn follow_key() -> String {
    format_key_text("ctrl+shift+down")
}

/// The pinned row at 80 columns: the prompt, then the jump hints flush right.
fn pinned_80(prompt: &str) -> String {
    let hints = format!(
        "{} prev \u{b7} {} next",
        format_key_text("alt+pageUp"),
        format_key_text("alt+pageDown")
    );
    let left = format!("\u{276f} {prompt}");
    format!(
        "{left}{}{hints}",
        " ".repeat(80 - str_width(&left) - str_width(&hints))
    )
}

/// The pill centered over `row` at `width` columns.
fn with_pill(row: &str, pill: &str, width: usize) -> String {
    let col = (width - str_width(pill)) / 2;
    let head: String = format!("{row:<col$}").chars().take(col).collect();
    format!("{head}{pill}").trim_end().to_string()
}

/// The prompt row is pinned while the window sits below its prompt, goes while the prompt's own
/// rows hold the first row, and goes at the bottom.
#[test]
fn the_pinned_prompt_row_shows_only_below_its_prompt() {
    let mut view = long_run([FIRST_PROMPT, SECOND_PROMPT, THIRD_PROMPT]);
    let bottom = rows(&[
        " run step 5",
        "",
        " run step 6",
        "",
        " run step 7",
        "",
        " run step 8",
        "",
        " run step 9",
    ]);
    assert_eq!(window(&mut view, 80), bottom);
    view.scroll_by(-6);
    let pill = format!(" \u{2193} {} to follow ", follow_key());
    assert_eq!(
        window(&mut view, 80),
        vec![
            pinned_80(THIRD_PROMPT),
            String::new(),
            " run step 3".to_string(),
            String::new(),
            " run step 4".to_string(),
            String::new(),
            " run step 5".to_string(),
            String::new(),
            with_pill(" run step 6", &pill, 80),
        ]
    );
    // Six more rows up, the third prompt's own rows hold the first row.
    view.scroll_by(-6);
    assert_eq!(
        window(&mut view, 80),
        vec![
            String::new(),
            "  run the whole suite once more".to_string(),
            String::new(),
            String::new(),
            " run step 1".to_string(),
            String::new(),
            " run step 2".to_string(),
            String::new(),
            with_pill(" run step 3", &pill, 80),
        ]
    );
    view.scroll_to_bottom();
    assert_eq!(window(&mut view, 80), bottom);
}

/// The pill counts the entries that arrived since the window paused, the follow key's jump
/// shows them, and the next pause starts from zero.
#[test]
fn the_follow_pill_counts_what_arrived_and_the_jump_clears_it() {
    let mut view = long_run([FIRST_PROMPT, SECOND_PROMPT, THIRD_PROMPT]);
    window(&mut view, 80);
    view.scroll_by(-6);
    let quiet = format!(" \u{2193} {} to follow ", follow_key());
    assert_eq!(
        window(&mut view, 80)[8],
        with_pill(" run step 6", &quiet, 80)
    );
    for _ in 0..4 {
        note(&mut view);
    }
    let counted = format!(" \u{2193} 4 new \u{b7} {} to follow ", follow_key());
    assert_eq!(
        window(&mut view, 80)[8],
        with_pill(" run step 6", &counted, 80)
    );
    assert_eq!(view.unseen_entries(), 4);
    view.scroll_to_bottom();
    assert_eq!(
        window(&mut view, 80),
        rows(&[
            " run step 9",
            "",
            " note",
            "",
            " note",
            "",
            " note",
            "",
            " note",
        ])
    );
    view.scroll_by(-6);
    assert_eq!(window(&mut view, 80)[8], with_pill(" note", &quiet, 80));
}

/// Each jump puts the prompt's block at the first row: previous walks up prompt by prompt and
/// stops at the first, next walks down and returns to the bottom past the last.
#[test]
fn prompt_jumps_land_on_the_previous_and_next_prompt() {
    let mut view = long_run([FIRST_PROMPT, SECOND_PROMPT, THIRD_PROMPT]);
    window(&mut view, 80);
    let landed = |view: &mut AgentView| window(view, 80)[..6].to_vec();
    let on = |prompt: &str, word: &str| {
        vec![
            String::new(),
            String::new(),
            format!("  {prompt}"),
            String::new(),
            String::new(),
            format!(" {word} step 1"),
        ]
    };
    // The first prompt leads the transcript: no spacing row above its block.
    let on_first = vec![
        String::new(),
        format!("  {FIRST_PROMPT}"),
        String::new(),
        String::new(),
        " fix step 1".to_string(),
        String::new(),
    ];
    let mut moves = Vec::new();
    let mut tops = Vec::new();
    for jump in [
        PromptJump::Previous,
        PromptJump::Previous,
        PromptJump::Previous,
        PromptJump::Previous,
        PromptJump::Next,
        PromptJump::Next,
    ] {
        moves.push(view.jump_to_prompt(jump));
        tops.push(landed(&mut view));
    }
    assert_eq!(moves, [true, true, true, false, true, true]);
    assert_eq!(
        tops,
        [
            on(THIRD_PROMPT, "run"),
            on(SECOND_PROMPT, "now"),
            on_first.clone(),
            on_first,
            on(SECOND_PROMPT, "now"),
            on(THIRD_PROMPT, "run"),
        ]
    );
    assert!(!view.is_following());
    // Past the last prompt the window returns to the bottom and follows; there it has no next.
    assert!(view.jump_to_prompt(PromptJump::Next));
    assert!(view.is_following());
    assert_eq!(window(&mut view, 80)[8], " run step 9");
    assert!(!view.jump_to_prompt(PromptJump::Next));
}

/// Output that arrives while the window is scrolled up changes the pill only: every other
/// window row stays where the reader left it.
#[test]
fn new_output_never_moves_a_scrolled_up_window() {
    let mut view = long_run([FIRST_PROMPT, SECOND_PROMPT, THIRD_PROMPT]);
    window(&mut view, 80);
    view.scroll_by(-6);
    let before = window(&mut view, 80);
    turn(&mut view, "one more prompt while reading");
    for _ in 0..3 {
        note(&mut view);
    }
    let after = window(&mut view, 80);
    assert!(!view.is_following());
    assert_eq!(after[..8], before[..8]);
    assert_eq!(
        after[8],
        with_pill(
            " run step 6",
            &format!(" \u{2193} 5 new \u{b7} {} to follow ", follow_key()),
            80
        )
    );
}

/// The pill names the bound key, drops what a narrow frame cannot hold, and says nothing when
/// there is nothing to say.
#[test]
fn the_follow_pill_shortens_to_fit() {
    let labels = [
        follow_pill_label(4, Some("End"), 80),
        follow_pill_label(4, Some("End"), 20),
        follow_pill_label(4, Some("End"), 14),
        follow_pill_label(4, Some("End"), 8),
        follow_pill_label(0, Some("End"), 80),
        follow_pill_label(0, Some("End"), 10),
        follow_pill_label(0, Some("End"), 6),
        follow_pill_label(4, None, 80),
        follow_pill_label(0, None, 80),
    ];
    assert_eq!(
        labels.map(Option::unwrap_or_default),
        [
            " \u{2193} 4 new \u{b7} End to follow ",
            " \u{2193} 4 new \u{b7} End ",
            " \u{2193} 4 new ",
            "",
            " \u{2193} End to follow ",
            " \u{2193} End ",
            "",
            " \u{2193} 4 new ",
            "",
        ]
    );
}

/// The prompt comes before the jump hints: at 80 columns a long prompt keeps its whole text
/// beside the one hint that still fits, and at 40 it is cut with the shared mark and carries
/// none.
#[test]
fn a_long_prompt_keeps_its_text_before_the_jump_hints() {
    let mut view = long_run([SECOND_PROMPT, THIRD_PROMPT, FIRST_PROMPT]);
    window(&mut view, 40);
    view.scroll_by(-6);
    for _ in 0..4 {
        note(&mut view);
    }
    let narrow = window(&mut view, 40);
    let wide = window(&mut view, 80);
    let left = format!("\u{276f} {FIRST_PROMPT}");
    let hint = format!("{} prev", format_key_text("alt+pageUp"));
    assert_eq!(
        [narrow[0].clone(), narrow[8].clone(), wide[0].clone()],
        [
            "\u{276f} fix the flaky collect test and tell m\u{2026}".to_string(),
            with_pill(
                " fix step 6",
                &format!(" \u{2193} 4 new \u{b7} {} to follow ", follow_key()),
                40
            ),
            format!(
                "{left}{}{hint}",
                " ".repeat(80 - str_width(&left) - str_width(&hint))
            ),
        ]
    );
}

fn prompt_text() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z \u{754c}\u{6570}\u{636e}\u{1f642}\u{1f469}\u{200d}\u{1f4bb}e\u{301}\t]{0,120}",
        Just(FIRST_PROMPT.to_string()),
        Just("\u{6570}\u{636e}".repeat(60)),
        Just("\u{1f469}\u{200d}\u{1f4bb}".repeat(40)),
        Just("\n\n  second line is the first with text\nthird".to_string()),
        Just("\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{7} tail".to_string()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    /// The pinned row is exactly as wide as its frame and the pill never wider, for any prompt
    /// (wide, combining and joined characters included) at every width from 20 to 200.
    #[test]
    fn the_new_rows_never_exceed_the_frame_width(
        prompt in prompt_text(),
        width in 20usize..=200,
        unseen in prop_oneof![Just(0usize), 1usize..=99_999],
        rebound in any::<bool>(),
    ) {
        let theme = Theme::builtin("prime", ColorMode::TrueColor);
        let jumps = Footer::new()
            .keys(["alt+pageUp"], "prev", HintRank::Extra)
            .keys(["alt+pageDown"], "next", HintRank::Extra);
        let pinned = pinned_prompt_row(&prompt, &jumps, &theme, width);
        prop_assert_eq!(str_width(&row_text(&pinned)), width);
        let key = if rebound { "Ctrl+Shift+Alt+PageDown" } else { "End" };
        let pill = follow_pill_label(unseen, Some(key), width).unwrap_or_default();
        prop_assert!(str_width(&pill) <= width);

        // The composed frame: the window's first and last rows carry the pinned prompt and the
        // pill, and every row is exactly the frame's width.
        let mut view = long_run([FIRST_PROMPT, prompt.as_str(), THIRD_PROMPT]);
        view.render_frame(width, HEIGHT);
        view.scroll_by(-6);
        for _ in 0..unseen.min(3) {
            note(&mut view);
        }
        for mut row in view.render_frame(width, HEIGHT) {
            crate::osc133::strip(&mut row);
            prop_assert_eq!(str_width(&row_text(&row)), width);
        }
    }
}
