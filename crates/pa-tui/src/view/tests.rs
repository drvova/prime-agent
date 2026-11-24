use super::frame::composite_follow_hint;
use super::*;
use crate::chat::{AssistantMessage, MessageBlock};
use crate::osc133::RowMarkers;
use crate::theme::{ColorMode, Theme, ThemeBg, ThemeColor};
use crate::tool_card::{ToolCallCard, ToolResultView};
use crate::width::str_width;
use crate::Line;
use ratatui::style::Modifier;

fn view() -> AgentView {
    AgentView::new(Theme::builtin("prime", ColorMode::TrueColor))
}

#[test]
fn markdown_link_targets_use_the_owning_view_cwd() {
    let first_path = std::env::temp_dir().join("markdown-first");
    let second_path = std::env::temp_dir().join("markdown-second");
    let first_url = url::Url::from_directory_path(&first_path)
        .unwrap()
        .join("url")
        .unwrap()
        .to_string();
    let second_url = url::Url::from_directory_path(&second_path)
        .unwrap()
        .join("url")
        .unwrap()
        .to_string();
    let mut first = view();
    first.chrome.cwd = first_path.to_string_lossy().into_owned();
    first.push_entry(ChatEntry::User {
        text: "[link](url)".to_string(),
        sent_at: None,
        images: Vec::new(),
    });
    let mut second = view();
    second.chrome.cwd = second_path.to_string_lossy().into_owned();
    second.push_entry(ChatEntry::User {
        text: "[link](url)".to_string(),
        sent_at: None,
        images: Vec::new(),
    });
    let targets =
        |v: &mut AgentView| crate::hyperlinks::frame_link_ranges(&v.render_transcript(80));
    let first_targets = targets(&mut first);
    let second_targets = targets(&mut second);
    assert!(first_targets.iter().any(|link| link.url == first_url));
    assert!(second_targets.iter().any(|link| link.url == second_url));
    assert_eq!(targets(&mut first), first_targets);
    first.chrome.cwd = second_path.to_string_lossy().into_owned();
    assert_eq!(targets(&mut first), second_targets);
    assert!(targets(&mut first).iter().all(|link| link.url != first_url));
}

/// A raw OSC 8 sequence in model, tool, or file text renders as plain text: only the links the
/// renderer adds (here the markdown link) are clickable.
#[test]
fn injected_terminal_links_are_not_clickable() {
    let injected = "\x1b]8;;https://evil.example/\x1b\\safe\x1b]8;;\x1b\\";
    // Removing the inner opener must not join the outer prefix into a new one.
    let nested = "\x1b]8;;\x1b]8;;x\x1b\\file:///evil\x1b\\safe";
    // Markdown decodes `\;` after the source is stripped.
    let escaped = "\x1b]8\\;;file:///evil\x1b\\safe";
    let entries = vec![
        ChatEntry::Assistant(Box::new(AssistantMessage {
            blocks: vec![MessageBlock::Text(format!(
                "{injected} {nested} {escaped} and [docs](https://docs.example/)"
            ))],
            has_tool_calls: true,
            streaming: false,
            error: None,
            aborted: false,
        })),
        ChatEntry::Tool(Box::new(ToolCallCard {
            id: "t1".to_string(),
            name: "python".to_string(),
            args: serde_json::json!({ "code": format!("x = 1  # {injected}") }),
            started: true,
            ..Default::default()
        })),
        ChatEntry::AgentMessage(Box::new(crate::custom_message::AgentMessageRow {
            direction: crate::custom_message::AgentMessageDirection::Received,
            counterpart: "lane".to_string(),
            message: format!("{injected}\n{nested}"),
        })),
        ChatEntry::BackgroundCompletion(Box::new(crate::custom_message::BackgroundCompletionRow {
            outcome: crate::custom_message::BackgroundOutcome::Shell {
                pid: Some(1),
                exit_code: Some(0),
            },
            content: format!("[bash-done] {injected}"),
        })),
        ChatEntry::CompactionSummary {
            summary: injected.to_string(),
            tokens_before: 1,
            custom_instructions: None,
        },
    ];
    for detail in [Detail::Overview, Detail::All] {
        let mut v = view_with(entries.clone());
        v.detail = detail;
        let links = crate::hyperlinks::frame_link_ranges(&v.render_transcript(80));
        assert!(!links.is_empty(), "the markdown link stays clickable");
        assert!(
            links.iter().all(|link| link.url == "https://docs.example/"),
            "{detail:?}: {links:?}"
        );
    }
    let streaming = crate::compaction_row::CompactionState {
        reason: crate::compaction_row::CompactionReason::Manual,
        custom_instructions: None,
        summary: injected.to_string(),
    };
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let rows = crate::compaction_row::render_compaction_stream(&streaming, true, &theme, 80);
    let links = crate::hyperlinks::frame_link_ranges(&rows);
    assert!(!rows.is_empty() && links.is_empty(), "{links:?}");
}

/// The hardware caret never sits over the factory page: the page is an
/// input-less replacement view, so `frame_cursor` suppresses the stale
/// editor dock position while it is open (the settings menu's guard
/// family — Macroscope review finding: the factory page left the caret
/// parked at the previous editor position over the panel).
#[test]
fn the_factory_page_suppresses_the_stale_editor_cursor() {
    let mut v = view();
    v.dock_cursor = Some((10, 3));
    v.factory_view = Some(crate::factory_view::FactoryView::from_reply(
        &serde_json::json!({ "runs": [] }),
        12,
    ));
    assert!(
        v.frame_cursor().is_none(),
        "the open factory page is an input-less overlay"
    );
    // ...and the editor's position reports again once the page closes
    // (`frame_cursor` maps the dock row through the window offset).
    v.factory_view = None;
    assert_eq!(v.frame_cursor(), Some((11, 3)));
}

/// A paste never reaches the editor behind an overlay (the key
/// dispatch's frame owners): the input-bearing pickers take it, the
/// input-less overlays consume it, and only the bare dock's editor
/// sees it.
#[test]
fn a_paste_never_reaches_the_editor_behind_an_overlay() {
    let editor_text = |view: &AgentView| view.editor.get_lines().join("\n");
    // The /effort picker takes it into its search.
    let mut v = view();
    v.choice_picker = Some(crate::choice_picker::ChoicePicker::effort(
        &["high".to_string()],
        None,
    ));
    assert!(v.route_paste("effort"));
    assert_eq!(editor_text(&v), "");
    // The /login provider selector takes it into its search.
    let mut v = view();
    v.provider_auth = Some(crate::provider_auth::ProviderAuthSelector::new(
        crate::provider_auth::AuthSelectorKind::Login,
        Vec::new(),
    ));
    assert!(v.route_paste("login"));
    assert_eq!(editor_text(&v), "");
    // The settings menu takes it into its search.
    let mut v = view();
    v.settings_menu = Some(crate::settings_menu::SettingsMenu::new(Vec::new()));
    assert!(v.route_paste("setting"));
    assert_eq!(editor_text(&v), "");
    // The input-less overlays consume it (the reload box stands in for
    // the whole consume set).
    let mut v = view();
    v.reload_box = Some("reloading".to_string());
    assert!(v.route_paste("never"));
    assert_eq!(editor_text(&v), "");
    // The bare dock: the editor takes it.
    let mut v = view();
    assert!(!v.route_paste("direct"));
    v.editor.handle_paste("direct");
    assert_eq!(editor_text(&v), "direct");
}

fn text_of(line: &Line) -> String {
    line.iter().map(|s| s.content.as_str()).collect::<String>()
}

#[test]
fn a_chat_starts_at_the_collapsed_detail_level() {
    let mut v = view();
    assert_eq!(v.detail, Detail::Overview, "the startup level is overview");
    assert!(!v.detail.show_thinking());
    assert!(!v.detail.tool_output_expanded());
    assert_eq!(v.detail.next(), Detail::Details);
    v.detail = v.detail.next();
    assert_eq!(v.detail.next(), Detail::All);
    v.detail = v.detail.next();
    assert_eq!(v.detail.next(), Detail::Overview);
}

#[test]
fn bang_prompt_renders_in_place_of_the_typed_prefix() {
    let mut v = view();
    v.editor.set_text("!echo hi");
    let frame = v.render_dock(80);
    let joined = frame.iter().map(text_of).collect::<Vec<_>>().join("\n");
    assert!(
        joined.contains("!  echo hi"),
        "the prompt swallows the typed prefix:\n{joined}"
    );
    assert!(
        !joined.contains("> echo hi"),
        "the default prompt does not render for a bang line:\n{joined}"
    );
    let bg = crate::chrome::editor_background(&v.theme);
    let prompt_row = frame
        .iter()
        .find(|line| text_of(line).contains("!  echo hi"))
        .expect("the prompt row");
    assert!(
        prompt_row.contains(&crate::Span::styled(
            "! ",
            bg.patch(v.theme.fg_style(ThemeColor::BashMode))
        )),
        "the bang prompt is tinted bash-mode on the editor background: {prompt_row:?}"
    );

    // The mode shows by colour only: the draft starts in the same column under `>`, `!` and `!!`.
    let draft_column = |text: &str| {
        let mut v = view();
        v.editor.set_text(text);
        let frame = v.render_dock(80);
        frame
            .iter()
            .map(text_of)
            .find_map(|row| row.find("echo quiet"))
            .expect("the draft row")
    };
    assert_eq!(draft_column("!!echo quiet"), draft_column("echo quiet"));
    assert_eq!(draft_column("!echo quiet"), draft_column("echo quiet"));
}

/// The composer's `>` prompt carries the session's thinking level (oh-my-pi's effort-tinted
/// editor edge): dim through low, muted at medium, accent at high, warm above it; plain with no
/// level, and the bash prompt keeps its own colour over the tint.
#[test]
fn the_prompt_is_tinted_by_the_thinking_level() {
    let prompt_style = |suffix: Option<&str>, text: &str| {
        let mut v = view();
        v.chrome.thinking_suffix = suffix.map(str::to_string);
        v.editor.set_text(text);
        let frame = v.render_dock(80);
        let row = frame
            .iter()
            .find(|line| text_of(line).contains("draft"))
            .expect("the prompt row");
        let prompt = row
            .iter()
            .find(|span| span.content == "> " || span.content == "! ")
            .expect("the prompt span");
        (prompt.style, text_of(row).find("draft"))
    };
    let reference = view();
    let theme = &reference.theme;
    let bg = crate::chrome::editor_background(theme);
    let tinted = ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        .map(|level| prompt_style(Some(level), "draft"));
    let draft_column = prompt_style(None, "draft").1;
    let expected = [
        ThemeColor::Dim,
        ThemeColor::Dim,
        ThemeColor::Dim,
        ThemeColor::Muted,
        ThemeColor::Accent,
        ThemeColor::Warning,
        ThemeColor::Warning,
    ]
    .map(|color| (bg.patch(theme.fg_style(color)), draft_column));
    assert_eq!(
        tinted, expected,
        "one tint per level, the draft never moves"
    );
    assert_eq!(prompt_style(None, "draft").0, bg, "no level, no tint");
    assert_eq!(
        prompt_style(Some("high"), "!draft").0,
        bg.patch(theme.fg_style(ThemeColor::BashMode)),
        "bash mode owns the prompt over the thinking tint"
    );
}

/// A large paste lands as one chip (nc/omp): the marker renders on the chip background, the
/// cursor steps over it and Backspace deletes it as one unit, and submit expands it in full.
#[test]
fn a_large_paste_is_one_chip_that_moves_deletes_and_expands_whole() {
    let mut v = view();
    let pasted = (1..=12)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    v.editor.handle_input("a");
    let _ = v.editor.handle_paste(&pasted);
    let marker = "[paste #1 +13 lines]";
    assert_eq!(v.editor.get_text(), format!("a{marker}"));
    let chip = v
        .theme
        .soft_selection_style()
        .patch(v.theme.fg_style(ThemeColor::Accent));
    let dock = v.render_dock(80);
    assert!(
        dock.iter()
            .flatten()
            .any(|span| *span == crate::Span::styled(marker, chip)),
        "the marker renders as one chip span: {dock:?}"
    );
    assert_eq!(v.editor.get_expanded_text(), format!("a{pasted}"));
    v.editor.handle_input("left");
    assert_eq!(
        v.editor.get_cursor(),
        (0, 1),
        "one Left steps over the whole chip"
    );
    v.editor.handle_input("right");
    v.editor.handle_input("backspace");
    assert_eq!(
        v.editor.get_text(),
        "a",
        "one Backspace deletes the whole chip"
    );
}

/// A draft taller than the box names what it hides in the box's own padding rows, dim and flush
/// right (nc): `+N lines above` on the top row, `+N lines below` on the bottom one.
#[test]
fn the_composer_counts_the_lines_it_hides() {
    let mut v = view();
    let text: Vec<String> = (1..=12).map(|n| format!("draft line {n}")).collect();
    v.editor.set_text(&text.join("\n"));
    let bg = crate::chrome::editor_background(&v.theme);
    let dim = bg.patch(v.theme.fg_style(ThemeColor::Dim));
    let counter = |label: &str| -> Line {
        vec![
            crate::Span::styled(" ".repeat(80 - label.len() - 2), bg),
            crate::Span::styled(label.to_string(), dim),
            crate::Span::styled("  ", bg),
        ]
    };
    // The box shows 7 of the 12 lines (30% of the 24-row terminal); the cursor sits at the end.
    let dock = v.render_dock(80);
    assert!(
        dock.contains(&counter("+5 lines above")),
        "the top padding row counts the hidden head: {dock:?}"
    );
    v.editor.move_to_doc_start();
    let dock = v.render_dock(80);
    assert!(
        dock.contains(&counter("+5 lines below")),
        "the bottom padding row counts the hidden tail: {dock:?}"
    );
    assert!(!dock.contains(&counter("+5 lines above")));
}

/// The empty composer's text row carries a transient status (dim, after the cursor) and the
/// self-retiring key hint (dim, flush right); typing hides both.
#[test]
fn the_empty_composer_row_hosts_the_status_and_the_hint() {
    let mut v = view();
    v.composer_status = Some("Copied last agent message to clipboard".to_string());
    let hint = "\u{21e7}\u{21e5} thinking effort";
    v.composer_hint = Some(hint.to_string());
    let bg = crate::chrome::editor_background(&v.theme);
    let dim = bg.patch(v.theme.fg_style(ThemeColor::Dim));
    let status = "Copied last agent message to clipboard";
    // ` > ` + the cursor cell (5), the status, the fill, the hint, the right padding (2).
    let fill = 80 - 5 - status.len() - crate::width::str_width(hint) - 2;
    let expected: Line = vec![
        crate::Span::styled(" ", bg),
        crate::Span::styled("> ", bg),
        crate::Span::styled(" ", bg),
        crate::Span::styled(" ", bg.add_modifier(Modifier::REVERSED)),
        crate::Span::styled(status, dim),
        crate::Span::styled(" ".repeat(fill), bg),
        crate::Span::styled(hint, dim),
        crate::Span::styled("  ", bg),
    ];
    assert!(
        v.render_dock(80).contains(&expected),
        "{:?}",
        v.render_dock(80)
    );
    v.editor.handle_input("x");
    let joined: String = v.render_dock(80).iter().map(text_of).collect();
    assert!(!joined.contains(status) && !joined.contains(hint));
}

/// A draft typed into a running turn takes the steering composer (nc): every box row on the warm
/// surface behind a warning-colored `▐` rail. An empty draft or an idle session keeps the plain
/// editor surface.
#[test]
fn a_draft_during_a_turn_takes_the_steering_composer() {
    let mut v = view();
    let steering = v.theme.steering_surface_style();
    assert_ne!(
        steering.bg,
        crate::chrome::editor_background(&v.theme).bg,
        "the warm surface differs from the editor surface"
    );
    let rail = crate::Span::styled(
        "\u{2590}",
        steering.patch(v.theme.fg_style(ThemeColor::Warning)),
    );
    let railed = |frame: &[Line]| -> Vec<Line> {
        frame
            .iter()
            .filter(|row| row.first() == Some(&rail))
            .cloned()
            .collect()
    };
    v.working = Some(WorkingState {
        activity: "Writing",
        message: None,
        download: true,
        tokens: 0,
        elapsed_secs: 2,
        thought: None,
        quiet_secs: 0,
        tool_label: None,
    });
    // A thinking level is set, but steering drops its tint from the prompt.
    v.chrome.thinking_suffix = Some("high".to_string());
    v.editor.set_text("also cover the tests");
    let rows = railed(&v.render_dock(80));
    assert_eq!(rows.len(), 3, "top pad, the draft row, bottom pad");
    assert_eq!(
        rows[0],
        vec![rail.clone(), crate::Span::styled(" ".repeat(79), steering)]
    );
    assert_eq!(
        text_of(&rows[1]).trim_end(),
        "\u{2590}>  also cover the tests"
    );
    assert_eq!(
        rows[1]
            .iter()
            .find(|span| span.content == "> ")
            .map(|span| span.style),
        Some(steering),
        "the steering prompt is plain on the warm surface, never thinking-tinted"
    );
    assert!(
        rows.iter()
            .flatten()
            .all(|span| span.style.bg == steering.bg),
        "every box span sits on the warm surface: {rows:?}"
    );

    v.editor.set_text("");
    assert!(
        railed(&v.render_dock(80)).is_empty(),
        "an empty draft reverts"
    );
    v.editor.set_text("also cover the tests");
    v.working = None;
    assert!(
        railed(&v.render_dock(80)).is_empty(),
        "an idle session reverts"
    );
}

#[test]
fn autocomplete_dropdown_rows_carry_the_popup_background() {
    // Every span of a menu row must carry a background, so an
    // unselected row does not blend into the transcript behind.
    let mut v = view();
    v.editor.handle_input("/");
    v.editor.handle_input("m");
    v.editor.materialize_autocomplete();
    assert!(v.editor.is_showing_autocomplete(), "the dropdown opens");
    let frame = v.render_dock(80);
    let marker_row = frame
        .iter()
        .find(|line| text_of(line).contains("\u{203a}"))
        .expect("the dropdown renders its marker row");
    assert!(
        marker_row.iter().all(|span| span.style.bg.is_some()),
        "dropdown row spans the popup background: {marker_row:?}"
    );
}

/// A command typed out with its separator ghosts its argument hint dim after the cursor.
#[test]
fn a_typed_command_ghosts_its_argument_hint_after_the_cursor() {
    let mut v = view();
    for ch in "/tier ".chars() {
        v.editor.handle_input(&ch.to_string());
    }
    let frame = v.render_dock(80);
    let ghost = frame
        .iter()
        .flatten()
        .find(|span| span.content == "[default|flex|priority|auto]")
        .expect("the hint ghosts after the cursor");
    let bg = crate::chrome::editor_background(&v.theme);
    assert_eq!(ghost.style, bg.patch(v.theme.fg_style(ThemeColor::Dim)));
    v.editor.handle_input("f");
    let frame = v.render_dock(80);
    assert!(
        !frame.iter().any(|line| text_of(line).contains("[default|")),
        "the first argument character clears the ghost"
    );
}

/// The dropdown opens with its top border (operator directive 2026-09-26).
#[test]
fn autocomplete_panel_opens_with_the_top_border_rule() {
    let mut v = view();
    v.editor.handle_input("/");
    v.editor.materialize_autocomplete();
    assert!(v.editor.is_showing_autocomplete(), "the dropdown opens");
    let frame = v.render_dock(80);
    let marker_row = frame
        .iter()
        .position(|line| text_of(line).trim_start().starts_with('\u{203a}'))
        .expect("the dropdown renders its selected marker row");
    let rule = frame
        .get(marker_row.checked_sub(1).expect("a row above the menu"))
        .expect("the top border row");
    assert_eq!(
        text_of(rule),
        "\u{2500}".repeat(80),
        "the panel opens with the full-width rule:\n{}",
        frame.iter().map(text_of).collect::<Vec<_>>().join("\n")
    );
    let rule_style = v
        .theme
        .fg_style(ThemeColor::BorderMuted)
        .patch(v.theme.bg_style(ThemeBg::ToolPanelBg));
    assert!(
        rule.iter().all(|span| span.style == rule_style),
        "the rule draws in the muted border color on the popup background: {rule:?}"
    );
}

/// The selected row's wash spans the panel's full width (operator directive 2026-09-26).
#[test]
fn autocomplete_selected_row_washes_the_full_panel_width() {
    let mut v = view();
    v.editor.handle_input("/");
    v.editor.materialize_autocomplete();
    assert!(v.editor.is_showing_autocomplete(), "the dropdown opens");
    let frame = v.render_dock(80);
    let selection_bg = v.theme.soft_selection_style().bg;
    let marker_row = frame
        .iter()
        .position(|line| text_of(line).trim_start().starts_with('\u{203a}'))
        .expect("the dropdown renders its selected marker row");
    let selected = &frame[marker_row];
    assert_eq!(
        str_width(&text_of(selected)),
        80,
        "the selected row spans the full panel width"
    );
    assert!(
        selected.iter().all(|span| span.style.bg == selection_bg),
        "the selection wash covers every span, both edges included: {selected:?}"
    );
    let unselected = frame
        .get(marker_row + 1)
        .expect("an unselected menu row follows");
    let panel_bg = v.theme.bg_style(ThemeBg::ToolPanelBg).bg;
    assert!(
        unselected.iter().all(|span| span.style.bg == panel_bg),
        "an unselected menu row keeps the popup background: {unselected:?}"
    );
}

#[test]
fn hint_rows_carry_the_platform_alt_label() {
    let mut v = view();
    v.queue_selected = Some(crate::queued::QueueSelectionItem {
        lane: crate::queued::QueueLane::Steering,
        index: 0,
        position: 0,
        text: "turn right".to_string(),
    });
    let frame = v.render_frame(80, 24);
    let joined = frame.iter().map(text_of).collect::<Vec<_>>().join("\n");
    assert!(
        joined.contains("browse"),
        "the queue browse header renders: {joined}"
    );
    if std::env::consts::OS == "macos" {
        assert!(
            joined.contains("Option+\u{2191}"),
            "macOS hint row: {joined}"
        );
    } else {
        assert!(joined.contains("Alt+\u{2191}"), "hint row: {joined}");
    }
}

#[test]
fn frame_is_exactly_height_rows() {
    let mut v = view();
    v.chrome.version = "0.0.0".to_string();
    v.chrome.cwd = "/tmp/project".to_string();
    v.chrome.chat_name = "project".to_string();
    let frame = v.render_frame(80, 24);
    assert_eq!(frame.len(), 24);
    assert!(frame.iter().all(|l| str_width(&text_of(l)) <= 80));
    let joined = frame.iter().map(text_of).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("prime agent v0.0.0"));
    assert!(joined.contains("Collapsed mode (Ctrl+O to expand)"));
    assert!(joined.contains('>'));
}

/// Fill the transcript past one window so there is scrollable history.
fn filled(mut v: AgentView, turns: usize) -> AgentView {
    v.chrome.version = "0.0.0".to_string();
    v.chrome.cwd = "/w".to_string();
    v.chrome.chat_name = "w".to_string();
    v.onboarding = None;
    for index in 0..turns {
        v.push(TranscriptItem::UserMessage {
            text: format!("user line {index}"),
        });
        v.push(TranscriptItem::Assistant {
            blocks: vec![crate::chat::MessageBlock::Text(format!(
                "assistant reply {index}"
            ))],
            has_tool_calls: false,
        });
    }
    v
}

fn row_text(line: &Line) -> String {
    line.iter().map(|s| s.content.as_str()).collect::<String>()
}

#[test]
fn scroll_pages_from_tail_and_resumes_at_bottom() {
    let mut v = filled(view(), 30);
    let frame = v.render_frame(80, 24);
    assert!(v.is_following());
    let info = v.scroll_info();
    assert_eq!(info.lines_below, 0);
    assert!(frame.iter().any(|l| row_text(l).contains("reply 29")));

    // PageUp pauses following and moves the window up a page: `page`
    // rows remain below the window.
    let page = v.page_size();
    v.scroll_by(-(page as isize));
    assert!(!v.is_following());
    assert_eq!(v.scroll_info().lines_below, page);

    v.scroll_by(page as isize);
    assert!(v.is_following());
    assert_eq!(v.scroll_info().lines_below, 0);
}

#[test]
fn scroll_offset_is_visible_in_frames() {
    let mut v = filled(view(), 30);
    let following = v.render_frame(80, 24);
    v.scroll_to_top();
    let top = v.render_frame(80, 24);
    // The top frame shows the earliest history the following frame
    // scrolled past.
    assert!(top.iter().any(|l| row_text(l).contains("reply 0")));
    assert!(!following.iter().any(|l| row_text(l).contains("reply 0")));
    assert!(following.iter().any(|l| row_text(l).contains("reply 29")));
    assert!(!top.iter().any(|l| row_text(l).contains("reply 29")));
}

/// The stall the adoption counter records is the one on screen: a quiet loader under a retry
/// countdown is hidden, so the layout reports no stall until the countdown leaves.
#[test]
fn a_stall_counts_as_shown_only_when_the_working_loader_owns_the_status_area() {
    let mut v = view();
    let now = std::time::Instant::now();
    v.working = Some(WorkingState {
        activity: "Waiting",
        message: None,
        download: false,
        tokens: 0,
        elapsed_secs: 0,
        thought: None,
        quiet_secs: 0,
        tool_label: None,
    });
    v.working_since = now.checked_sub(std::time::Duration::from_secs(10));
    v.working_heard = now.checked_sub(std::time::Duration::from_secs(5));
    v.retry = Some(crate::chat::RetryState {
        attempt: 1,
        max_attempts: 3,
        ends_at: now + std::time::Duration::from_secs(30),
        error_message: "429 Rate limit exceeded".to_string(),
        provider: None,
    });
    v.render_frame(80, 24);
    assert!(!v.stall_shown, "the retry countdown hides the stall row");
    v.retry = None;
    let frame = v.render_frame(80, 24);
    assert!(
        frame
            .iter()
            .any(|row| row_text(row).contains("Waiting on the model")),
        "the stall row is on screen"
    );
    assert!(v.stall_shown, "the painted stall row is reported");
}

#[test]
fn compaction_loader_replaces_the_working_loader() {
    // TS `startCompactionLoader`: the compaction loader owns the status
    // area while a compaction runs, working loader hidden.
    let mut v = view();
    v.working = Some(WorkingState {
        activity: "Waiting",
        message: None,
        download: false,
        tokens: 0,
        elapsed_secs: 0,
        thought: None,
        quiet_secs: 0,
        tool_label: None,
    });
    v.compaction = Some(crate::chat::CompactionState {
        reason: crate::chat::CompactionReason::Manual,
        custom_instructions: None,
        summary: String::new(),
    });
    let frame = v.render_frame(80, 24);
    let flat: Vec<String> = frame.iter().map(row_text).collect();
    assert!(
        flat.iter()
            .any(|l| l.contains("Compacting context... (Ctrl+C to cancel)")),
        "{flat:?}"
    );
    assert!(
        !flat.iter().any(|l| l.contains("Waiting")),
        "the working loader is hidden during compaction: {flat:?}"
    );
    v.compaction = None;
    v.push_entry(crate::chat::ChatEntry::CompactionSummary {
        summary: "the story so far".to_string(),
        tokens_before: 1234,
        custom_instructions: None,
    });
    let frame = v.render_frame(80, 24);
    let flat: Vec<String> = frame.iter().map(row_text).collect();
    assert!(
        flat.iter()
            .any(|l| l.trim() == "\u{25c6} Context compacted"),
        "{flat:?}"
    );
    assert!(
        flat.iter().any(|l| l.trim() == "the story so far"),
        "{flat:?}"
    );
}

/// The operator's "stream the compacted summary" feature: the delta renders under the loader row.
#[test]
fn compaction_streams_the_summary_under_the_loader_in_expanded_detail() {
    let mut v = view();
    v.detail = crate::chat::Detail::All;
    v.compaction = Some(crate::chat::CompactionState {
        reason: crate::chat::CompactionReason::Threshold,
        custom_instructions: None,
        summary: "The session covered the fleet work.".to_string(),
    });
    let frame = v.render_frame(80, 24);
    let flat: Vec<String> = frame.iter().map(row_text).collect();
    let loader = flat
        .iter()
        .position(|l| l.contains("Auto-compacting..."))
        .expect("the loader row renders");
    let gutter = &flat[loader + 1];
    assert!(
        gutter
            .trim_start()
            .starts_with(crate::branch::BRANCH_GUTTER),
        "the live block nests under the loader: {flat:?}"
    );
    assert!(
        gutter.contains("The session covered the fleet work."),
        "{flat:?}"
    );
    // TS keeps the loader plain outside `all`.
    v.detail = crate::chat::Detail::Overview;
    let frame = v.render_frame(80, 24);
    let flat: Vec<String> = frame.iter().map(row_text).collect();
    assert!(
        flat.iter().any(|l| l.contains("Auto-compacting...")),
        "{flat:?}"
    );
    assert!(
        !flat
            .iter()
            .any(|l| l.contains("The session covered the fleet work.")),
        "no streamed block outside the expanded detail: {flat:?}"
    );
    v.detail = crate::chat::Detail::All;
    v.compaction = None;
    v.push_entry(crate::chat::ChatEntry::CompactionSummary {
        summary: "The session covered the fleet work.".to_string(),
        tokens_before: 1234,
        custom_instructions: None,
    });
    let frame = v.render_frame(80, 24);
    let flat: Vec<String> = frame.iter().map(row_text).collect();
    assert!(
        !flat.iter().any(|l| l.contains("Auto-compacting...")),
        "the loader cleared: {flat:?}"
    );
    assert!(
        flat.iter()
            .any(|l| l.trim() == "\u{25c6} Context compacted \u{b7} Compacted from 1,234 tokens"),
        "the durable summary row replaced the streamed block: {flat:?}"
    );
}

#[test]
fn follow_hint_shows_when_paused_and_hides_when_following() {
    let mut v = filled(view(), 30);
    let following_frame = v.render_frame(80, 24);
    assert!(!following_frame
        .iter()
        .any(|l| row_text(l).contains("to follow")));
    v.scroll_by(-(v.page_size() as isize));
    let paused_frame = v.render_frame(80, 24);
    let hint = format!(
        "{} to follow",
        crate::keybindings::format_key_text("ctrl+shift+down")
    );
    assert!(paused_frame.iter().any(|l| row_text(l).contains(&hint)));
    v.scroll_to_bottom();
    let resumed_frame = v.render_frame(80, 24);
    assert!(v.is_following());
    assert!(!resumed_frame
        .iter()
        .any(|l| row_text(l).contains("to follow")));
    // TS shows the hint for every non-following window, even at the very
    // top.
    v.scroll_to_top();
    let top_frame = v.render_frame(80, 24);
    assert!(!v.is_following());
    assert!(top_frame.iter().any(|l| row_text(l).contains("to follow")));
}

/// A transcript with thinking blocks: long in `all`/`details`, short in `overview` (thinking
/// hidden).
fn thinking_filled(v: AgentView, turns: usize) -> AgentView {
    let mut v = filled(v, 0);
    for index in 0..turns {
        v.push(TranscriptItem::UserMessage {
            text: format!("user line {index}"),
        });
        v.push(TranscriptItem::Assistant {
            blocks: vec![crate::chat::MessageBlock::Thinking(format!(
                "thinking {index} {} the expanded mode dwarfs the collapsed one",
                "reasoning word ".repeat(56)
            ))],
            has_tool_calls: false,
        });
    }
    v
}

/// The mode-exit follow recompute (operator directive 2026-09-26).
#[test]
fn collapse_resumes_following_when_the_tail_is_in_view() {
    let mut v = thinking_filled(view(), 1);
    v.detail = Detail::All;
    v.render_frame(80, 24);
    assert!(v.is_following());
    v.scroll_by(-(v.page_size() as isize));
    assert!(!v.is_following());
    v.detail = Detail::Overview;
    let collapsed = v.render_frame(80, 24);
    assert!(v.is_following(), "the collapse re-derived the follow state");
    assert!(
        !collapsed.iter().any(|l| row_text(l).contains("to follow")),
        "the tail-in-view window carries no follow hint: {collapsed:?}"
    );
    assert_eq!(v.scroll_info().lines_below, 0);
}

#[test]
fn a_window_ending_exactly_at_the_tail_shows_no_hint() {
    let mut v = thinking_filled(view(), 8);
    v.render_frame(80, 24);
    // The scroll distance to the exact bottom: a following window sits
    // at `last_max_scroll`, which `lines_above` reports.
    let bottom = v.scroll_info().lines_above;
    v.scroll_to_top();
    // Each row step keeps the walk (the sparse path), so the final
    // window is a walked Top anchor sitting exactly on the bottom.
    v.render_frame(80, 24);
    for _ in 0..bottom {
        v.scroll_by(1);
    }
    let frame = v.render_frame(80, 24);
    assert_eq!(
        v.scroll_info().lines_below,
        0,
        "the window sits exactly at the bottom"
    );
    assert!(
        !frame.iter().any(|l| row_text(l).contains("to follow")),
        "the at-the-bottom window carries no follow hint (following \
         would scroll nothing)"
    );
}

#[test]
fn collapse_keeps_the_hint_when_rows_remain_below() {
    let mut v = filled(view(), 30);
    v.render_frame(80, 24);
    v.scroll_by(-2);
    assert!(!v.is_following());
    v.detail = Detail::Overview;
    let collapsed = v.render_frame(80, 24);
    assert!(!v.is_following());
    assert!(
        collapsed.iter().any(|l| row_text(l).contains(&format!(
            "{} to follow",
            crate::keybindings::format_key_text("ctrl+shift+down")
        ))),
        "the paused window with rows below keeps the hint"
    );
}

/// The hover affordance (operator directive 2026-09-26): Muted spans
/// brighten to the theme's foreground, Dim to Muted.
#[test]
fn the_hover_affordance_brightens_the_hovered_card_row() {
    // A settled tool card paints its header row Muted (the tool
    // name) with the separator Dim — the affordance's exact input.
    let mut view = collapsed_view(settled_cards(1));
    let plain = view.render_frame(80, 24);
    let muted = view.theme.fg_style(crate::theme::ThemeColor::Muted).fg;
    let text = view.theme.fg_style(crate::theme::ThemeColor::Text).fg;
    // The card's header row (the "bash \u{b7} done" row), not the
    // entry's leading spacer: the affordance's visible surface.
    let row_text = |row: usize| -> String {
        plain[row]
            .iter()
            .map(|span| span.content.as_str())
            .collect()
    };
    let card_row = (0..plain.len())
        .find(|&row| row_text(row).contains("bash \u{b7} done"))
        .expect("the card's header row renders");
    assert!(
        plain[card_row].iter().any(|span| span.style.fg == muted),
        "the card's header row paints muted: {:?}",
        plain[card_row]
    );
    assert!(view.note_hover(card_row, 2));
    assert_eq!(view.hover_pos, Some((card_row, 2)));
    let hovered = view.render_frame(80, 24);
    let dim = view.theme.fg_style(crate::theme::ThemeColor::Dim).fg;
    let bright = view.theme.fg_style(crate::theme::ThemeColor::Muted);
    for (hovered_span, plain_span) in hovered[card_row].iter().zip(&plain[card_row]) {
        if plain_span.style.fg == muted {
            assert_eq!(
                hovered_span.style.fg, text,
                "the hovered row's muted span brightened to the theme fg"
            );
        } else if plain_span.style.fg == dim {
            assert_eq!(
                hovered_span.style.fg, bright.fg,
                "the hovered row's dim span stepped up to muted"
            );
        }
    }
    // A motion across the same row changes nothing: the affordance is
    // row-level.
    assert!(!view.note_hover(card_row, 5));
    assert_eq!(view.hover_pos, Some((card_row, 5)));
    assert!(view.note_hover(0, 2));
    assert_eq!(view.hover_pos, None);
    let restored = view.render_frame(80, 24);
    assert!(
        restored[card_row].iter().any(|span| span.style.fg == muted),
        "the card's muted paint returns with the hover"
    );
}

/// The hover is a screen coordinate: a scroll that brings content onto the hovered row clears it.
#[test]
fn a_scrolled_layout_revalidates_the_hover() {
    // A transcript TALLER than the window: the revalidation is exercised by a real layout move.
    let mut entries = settled_cards(3);
    for index in 0..10 {
        entries.push(crate::chat::ChatEntry::User {
            text: format!("later user line {index}"),
            sent_at: None,
            images: Vec::new(),
        });
    }
    let mut view = collapsed_view(entries);
    view.render_frame(80, 24);
    view.scroll_to_top();
    let plain = view.render_frame(80, 24);
    let muted = view.theme.fg_style(crate::theme::ThemeColor::Muted).fg;
    let row_text = |row: usize| -> String {
        plain[row]
            .iter()
            .map(|span| span.content.as_str())
            .collect()
    };
    let card_row = (0..plain.len())
        .find(|&row| row_text(row).contains("bash \u{b7} done"))
        .expect("the card's header row renders");
    assert!(view.note_hover(card_row, 2));
    let text = view.theme.fg_style(crate::theme::ThemeColor::Text).fg;
    let hovered = view.render_frame(80, 24);
    for (hovered_span, plain_span) in hovered[card_row].iter().zip(&plain[card_row]) {
        if plain_span.style.fg == muted {
            assert_eq!(
                hovered_span.style.fg, text,
                "the muted spans brightened on the hovered row"
            );
        }
    }
    // Scroll down past every card's span: user rows land on the
    // recorded screen row, and a stale coordinate never brightens them.
    view.scroll_by(24);
    let scrolled = view.render_frame(80, 24);
    assert_eq!(
        view.hover_pos, None,
        "the scroll cleared the stale hover coordinate"
    );
    let scrolled_text: String = scrolled[card_row]
        .iter()
        .map(|span| span.content.as_str())
        .collect();
    assert!(
        !scrolled_text.contains("bash \u{b7} done"),
        "the card's rows vacated the recorded position: {:?}",
        scrolled[card_row]
    );
    assert!(
        (0..scrolled.len()).any(|row| scrolled[row]
            .iter()
            .any(|span| span.content.contains("user line"))),
        "the user rows moved into the window with the scroll"
    );
}

#[test]
fn follow_hint_keeps_zone_markers_on_the_composited_row() {
    // A marked row composited with the hint keeps its zone flags and
    // the visible text around the centered label.
    let mut row = vec![crate::Span::raw("x".repeat(80))];
    crate::osc133::mark_end(&mut row);
    let mut out = composite_follow_hint(&row, " ctrl+shift+down to follow ", 80);
    let markers = crate::osc133::row_markers(&out);
    assert!(markers.end && !markers.start);
    assert!(row_text(&out).contains("to follow"));
    assert_eq!(str_width(&row_text(&out)), 80);
    crate::osc133::strip(&mut out);
    assert!(row_text(&out).contains("to follow"));
    let plain = vec![crate::Span::raw(" ".repeat(80))];
    let out = composite_follow_hint(&plain, " ctrl+shift+down to follow ", 80);
    assert_eq!(crate::osc133::row_markers(&out), RowMarkers::default());
}

/// Flush the inline frame into a byte sink, returning the exact bytes
/// the terminal would receive.
fn flush_bytes(v: &mut AgentView, width: usize, screen_height: usize) -> Vec<u8> {
    let mut sink: Vec<u8> = Vec::new();
    v.stream_flush_to(&mut sink, width, screen_height)
        .expect("the flush streams");
    sink
}

#[test]
fn flush_streams_append_then_repaints_the_changed_tail() {
    let mut v = view();
    v.chrome.version = "0.0.0".to_string();
    v.chrome.cwd = "/w".to_string();
    v.chrome.chat_name = "w".to_string();
    v.push(TranscriptItem::UserMessage {
        text: "first turn".to_string(),
    });
    // The first flush appends the whole inline frame with the zero-width zone markers
    // embedded — they must survive into scrollback for shell-integration jumps.
    let first = flush_bytes(&mut v, 80, 24);
    let joined = String::from_utf8_lossy(&first);
    // The encoded bytes carry SGR styling between spans: assert on
    // single-span fragments, not strings spanning a style boundary.
    assert!(joined.contains("0.0.0"));
    assert!(joined.contains("first turn"));
    assert!(first
        .windows(crate::osc133::ZONE_START.len())
        .any(|w| w == crate::osc133::ZONE_START.as_bytes()));
    assert!(first.starts_with(b"\r") && first.ends_with(b"\r\n"));

    assert!(flush_bytes(&mut v, 80, 24).is_empty());

    // New transcript rows land ABOVE the flushed dock: the changed
    // region is rewritten, not appended below the stale dock.
    v.push(TranscriptItem::UserMessage {
        text: "second turn".to_string(),
    });
    let repaint = flush_bytes(&mut v, 80, 24);
    assert!(
        repaint.starts_with(b"\x1b[2J\x1b[H"),
        "a repaint erases first"
    );
    let joined = String::from_utf8_lossy(&repaint);
    assert!(joined.contains("second turn"));
    assert!(joined.contains("first turn"));
    // The repaint covers at most one screenful.
    let mut long = filled(view(), 30);
    let appended = flush_bytes(&mut long, 80, 10);
    assert!(
        !appended.is_empty() && !appended.starts_with(b"\x1b[2J"),
        "first flush appends"
    );
    long.push(TranscriptItem::UserMessage {
        text: "late turn".to_string(),
    });
    let repaint = flush_bytes(&mut long, 80, 10);
    assert!(repaint.starts_with(b"\x1b[2J\x1b[H"));
    // One screenful of rows: at most `screen_height` CRLFs; the SIMD
    // bytecount dependency would be pointless here.
    #[allow(clippy::naive_bytecount)]
    let newline_rows = repaint.iter().filter(|b| **b == b'\n').count();
    assert!(newline_rows <= 10);
    let joined = String::from_utf8_lossy(&repaint);
    assert!(joined.contains("late turn"));
    assert!(!joined.contains("reply 0"));

    // A shrinking rebuild never rewinds into a rewrite of scrollback.
    v.clear_chat();
    let repaint = flush_bytes(&mut v, 80, 24);
    assert!(repaint.starts_with(b"\x1b[2J\x1b[H"));
    let joined = String::from_utf8_lossy(&repaint);
    assert!(!joined.contains("second turn"));
}

#[test]
fn inline_frame_is_transcript_plus_dock_without_padding() {
    let mut v = filled(view(), 30);
    // The inline layout is the unpinned frame: every transcript row is
    // present (no window slicing) and no height padding rows follow.
    let frame = v.render_frame(80, 24);
    let inline = v.render_inline_frame(80);
    assert!(inline.iter().any(|l| text_of(l).contains("reply 0")));
    assert!(inline.iter().any(|l| text_of(l).contains("reply 29")));
    assert!(frame.len() == 24 && inline.len() != frame.len());
    let joined = inline.iter().map(text_of).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("Collapsed mode"));
}

#[test]
fn dock_pads_window_between_splash_and_editor() {
    let mut v = view();
    v.chrome.version = "0.0.0".to_string();
    v.chrome.cwd = "/w".to_string();
    v.chrome.chat_name = "w".to_string();
    let frame = v.render_frame(60, 40);
    assert_eq!(frame.len(), 40);
    let joined = frame.iter().map(text_of).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("Collapsed mode"));
}

fn view_with(entries: Vec<ChatEntry>) -> AgentView {
    let mut view = AgentView::new(crate::theme::Theme::builtin(
        "prime",
        crate::theme::ColorMode::Color256,
    ));
    for entry in entries {
        view.push_entry(entry);
    }
    view
}

fn settled_tool_card(id: &str) -> ChatEntry {
    // A fixed run time: two `Instant::now()` reads can be equal (Windows'
    // clock), and a card whose run took no time shows none.
    let started = std::time::Instant::now();
    ChatEntry::Tool(Box::new(ToolCallCard {
        id: id.to_string(),
        name: "bash".to_string(),
        args: serde_json::json!({"command": "echo done"}),
        started: true,
        started_at: Some(started),
        ended_at: Some(started + std::time::Duration::from_millis(1)),
        result: Some(ToolResultView {
            content: vec![serde_json::json!({"type": "text", "text": "done"})],
            details: serde_json::Value::Null,
            is_error: false,
        }),
        result_partial: false,
        aborted: false,
    }))
}

fn transcript_text(view: &mut AgentView, width: usize) -> String {
    let rows = view.render_transcript(width);
    rows.iter()
        .map(|line| line.iter().map(|span| span.content.as_str()).collect())
        .collect::<Vec<String>>()
        .join("\n")
}

/// A prompt's send time sits flush right on its first text row; a row too full for it keeps
/// its text, and the clock never changes the entry's height.
#[cfg(unix)] // Windows renders no clock: pa_types::platform::local_clock answers None there
#[test]
fn a_prompt_shows_its_send_time_flush_right() {
    let sent_at = 1_763_300_000_000;
    let (hour, minute) = pa_types::platform::local_clock(sent_at).unwrap();
    let clock = format!("{hour:02}:{minute:02}");
    let rows_of = |text: &str, sent_at: Option<i64>| {
        let mut view = view_with(vec![ChatEntry::User {
            text: text.to_string(),
            sent_at,
            images: Vec::new(),
        }]);
        let text = transcript_text(&mut view, 40);
        text.lines().map(str::to_string).collect::<Vec<_>>()
    };
    let stamped = rows_of("hello", Some(sent_at));
    let row = stamped.iter().find(|row| row.contains("hello")).unwrap();
    assert!(row.ends_with(&format!("{clock}  ")), "{row:?}");
    assert_eq!(stamped.len(), rows_of("hello", None).len());
    let full = "x".repeat(34);
    let crowded = rows_of(&full, Some(sent_at));
    assert!(
        !crowded.iter().any(|row| row.contains(&clock)),
        "{crowded:?}"
    );
}

#[test]
fn cached_transcript_rows_match_fresh_render() {
    let mut view = view_with(vec![
        ChatEntry::User {
            text: "hello".to_string(),
            sent_at: None,
            images: Vec::new(),
        },
        ChatEntry::Assistant(Box::new(AssistantMessage {
            blocks: vec![MessageBlock::Text("world".to_string())],
            has_tool_calls: false,
            streaming: false,
            error: None,
            aborted: false,
        })),
        settled_tool_card("call_1"),
    ]);
    let fresh = transcript_text(&mut view, 80);
    let cached = transcript_text(&mut view, 80);
    assert_eq!(fresh, cached);
}

#[test]
fn stale_entry_re_renders_new_content() {
    let mut view = view_with(vec![ChatEntry::Assistant(Box::new(AssistantMessage {
        blocks: vec![MessageBlock::Text("part one".to_string())],
        has_tool_calls: false,
        streaming: false,
        error: None,
        aborted: false,
    }))]);
    let before = transcript_text(&mut view, 80);
    if let Some(ChatEntry::Assistant(open)) = view.chat.get_mut(0) {
        open.blocks = vec![MessageBlock::Text("part one part two".to_string())];
    }
    view.mark_entry_stale(0);
    let after = transcript_text(&mut view, 80);
    assert!(before.contains("part one"));
    assert!(!before.contains("part two"));
    assert!(after.contains("part one part two"));
}

/// A settled assistant message keeps no markdown block cache, while a
/// streaming message keeps its settled blocks cached.
#[test]
fn settled_messages_render_once_streaming_keeps_block_cache() {
    let settled_rows = {
        let mut view = view_with(vec![ChatEntry::Assistant(Box::new(AssistantMessage {
            blocks: vec![MessageBlock::Text("settled body".to_string())],
            has_tool_calls: false,
            streaming: false,
            error: None,
            aborted: false,
        }))]);
        let text = transcript_text(&mut view, 80);
        assert!(
            view.md_caches.borrow().is_empty(),
            "a settled message keeps no duplicate block-cache copy"
        );
        text
    };
    let mut view = view_with(vec![ChatEntry::Assistant(Box::new(AssistantMessage {
        blocks: vec![MessageBlock::Text("streaming body".to_string())],
        has_tool_calls: false,
        streaming: true,
        error: None,
        aborted: false,
    }))]);
    let streaming_text = transcript_text(&mut view, 80);
    assert!(
        !view.md_caches.borrow().is_empty(),
        "a streaming message keeps its block cache for per-frame replays"
    );
    assert!(
        settled_rows.contains("settled body") && streaming_text.contains("streaming body"),
        "both render their bodies identically through their own paths"
    );
}

/// A running card's rows must not be cached: the spinner frame
/// advances.
#[test]
fn running_card_is_not_cached_and_settled_card_is() {
    let running = ChatEntry::Tool(Box::new(ToolCallCard {
        id: "call_r".to_string(),
        name: "bash".to_string(),
        args: serde_json::json!({"command": "sleep 1"}),
        started: true,
        started_at: Some(std::time::Instant::now()),
        ended_at: None,
        result: None,
        result_partial: false,
        aborted: false,
    }));
    let mut view = view_with(vec![running, settled_tool_card("call_d")]);
    view.pulse_frame = 0;
    let frame0 = transcript_text(&mut view, 80);
    view.pulse_frame = 1;
    let frame1 = transcript_text(&mut view, 80);
    assert_ne!(frame0, frame1, "the running spinner must animate");

    // With only a settled card, the pulse frame cannot change rows.
    let mut settled_view = view_with(vec![settled_tool_card("call_d")]);
    settled_view.pulse_frame = 0;
    let s0 = transcript_text(&mut settled_view, 80);
    settled_view.pulse_frame = 7;
    let s7 = transcript_text(&mut settled_view, 80);
    assert_eq!(s0, s7);
}

#[test]
fn detail_change_invalidates_cached_rows() {
    let mut view = view_with(vec![ChatEntry::Assistant(Box::new(AssistantMessage {
        blocks: vec![
            MessageBlock::Thinking("thinking body".to_string()),
            MessageBlock::Text("answer".to_string()),
        ],
        has_tool_calls: false,
        streaming: false,
        error: None,
        aborted: false,
    }))]);
    view.detail = Detail::Overview;
    let overview = transcript_text(&mut view, 80);
    view.detail = view.detail.next();
    let details = transcript_text(&mut view, 80);
    assert!(!overview.contains("thinking body"));
    assert!(details.contains("thinking body"));
}

#[test]
fn compaction_summary_block_toggles_with_the_detail_cycle() {
    let summary = "## Summary\nthe session story, first line\nand a second line that wraps";
    let mut view = view_with(vec![ChatEntry::CompactionSummary {
        summary: summary.to_string(),
        tokens_before: 12345,
        custom_instructions: Some("the goal".to_string()),
    }]);
    let collapsed = transcript_text(&mut view, 80);
    assert!(collapsed.contains("\u{25c6} Context compacted"));
    assert!(collapsed.contains("## Summary the session story, first line"));
    assert!(!collapsed.contains("Compacted from"));
    // The row is cached from the first render; a detail change must
    // re-flow it or the block would stay collapsed forever.
    view.detail = view.detail.next();
    let at_details = transcript_text(&mut view, 80);
    assert_eq!(view.detail, Detail::Details);
    assert!(
        !at_details.contains("Compacted from"),
        "the middle `details` level keeps the block collapsed too: {at_details}"
    );
    view.detail = view.detail.next();
    let expanded = transcript_text(&mut view, 80);
    assert!(
        expanded.contains("Compacted from 12,345 tokens \u{b7} focus: the goal"),
        "the expanded metadata row renders: {expanded}"
    );
    // The expanded body is markdown, not the EventSummary collapse.
    assert!(
        expanded.contains("Summary"),
        "the expanded markdown body renders: {expanded}"
    );
    view.detail = view.detail.next();
    let collapsed_again = transcript_text(&mut view, 80);
    assert_eq!(view.detail, Detail::Overview);
    assert!(
        !collapsed_again.contains("Compacted from"),
        "the cycle back to `overview` collapses the block: {collapsed_again}"
    );
}

fn user_row() -> ChatEntry {
    ChatEntry::User {
        text: "hello".to_string(),
        sent_at: None,
        images: Vec::new(),
    }
}

/// A tool-carrying assistant whose only body is a thinking block (TS `hideThinkingBlock`).
fn thinking_tool_assistant() -> ChatEntry {
    ChatEntry::Assistant(Box::new(AssistantMessage {
        blocks: vec![MessageBlock::Thinking("thinking body".to_string())],
        has_tool_calls: true,
        streaming: false,
        error: None,
        aborted: false,
    }))
}

fn agent_message_row() -> ChatEntry {
    ChatEntry::AgentMessage(Box::new(crate::custom_message::AgentMessageRow {
        direction: crate::custom_message::AgentMessageDirection::Received,
        counterpart: "lane".to_string(),
        message: "hi".to_string(),
    }))
}

fn shell_completion_row() -> ChatEntry {
    ChatEntry::BackgroundCompletion(Box::new(crate::custom_message::BackgroundCompletionRow {
        outcome: crate::custom_message::BackgroundOutcome::Shell {
            pid: Some(1),
            exit_code: Some(0),
        },
        content: "[bash-done]".to_string(),
    }))
}

/// TS `UserMessageComponent` is a Box(2,1): its vertical padding row
/// is the first of two blanks before a tool card.
#[test]
fn tool_card_after_user_message_keeps_ts_two_blank_seam() {
    let mut view = view_with(vec![
        ChatEntry::User {
            text: "run the cell".to_string(),
            sent_at: None,
            images: Vec::new(),
        },
        settled_tool_card("t1"),
    ]);
    let rows = view.render_transcript(120);
    let flat: Vec<String> = rows
        .iter()
        .map(|l| l.iter().map(|s| s.content.as_str()).collect())
        .collect();
    let user = flat
        .iter()
        .position(|r| r.contains("run the cell"))
        .expect("user row");
    // The box padding row carries the OSC 133 zone-end markers behind
    // its background spaces; both seam rows are visually empty.
    let empty = |row: &str| crate::width::str_width(row.trim()) == 0;
    assert!(empty(&flat[user + 1]), "box bottom padding row");
    assert!(empty(&flat[user + 2]), "tool leading spacer row");
    assert!(
        flat[user + 3].trim().starts_with("bash"),
        "card after the two blanks: {:?}",
        &flat[user + 3..]
    );
}

/// TS `createConversationSpacing.shouldAddLeadingSpace` for one
/// spacing-driven row.
#[test]
fn conversation_leading_matches_ts_spacing_rules() {
    let visible_assistant = || {
        ChatEntry::Assistant(Box::new(AssistantMessage {
            blocks: vec![MessageBlock::Text("done".to_string())],
            has_tool_calls: true,
            streaming: false,
            error: None,
            aborted: false,
        }))
    };
    let tool_only_assistant = || {
        ChatEntry::Assistant(Box::new(AssistantMessage {
            blocks: Vec::new(),
            has_tool_calls: true,
            streaming: false,
            error: None,
            aborted: false,
        }))
    };
    let user = || ChatEntry::User {
        text: "hello".to_string(),
        sent_at: None,
        images: Vec::new(),
    };

    // Nothing preceding: the collapsed form leads with a blank, the
    // expanded form sits flush against the top of the chat.
    let view = view_with(vec![agent_message_row()]);
    assert!(view.conversation_leading(0, false));
    assert!(!view.conversation_leading(0, true));

    // A user row is never a compact neighbor: both forms lead.
    let view = view_with(vec![user(), agent_message_row()]);
    assert!(view.conversation_leading(1, false));
    assert!(view.conversation_leading(1, true));

    // A visible assistant with tool calls carries the trailing space.
    let view = view_with(vec![visible_assistant(), agent_message_row()]);
    assert!(!view.conversation_leading(1, false));
    assert!(!view.conversation_leading(1, true));

    // A compact neighbor (tool card, shell completion, agent message):
    // flush collapsed, blank expanded.
    for neighbor in [
        settled_tool_card("c1"),
        shell_completion_row(),
        agent_message_row(),
    ] {
        let view = view_with(vec![neighbor, agent_message_row()]);
        assert!(!view.conversation_leading(1, false), "flush collapsed");
        assert!(view.conversation_leading(1, true), "blank expanded");
    }

    // A tool-only assistant (no visible body) is a separator.
    let view = view_with(vec![tool_only_assistant(), agent_message_row()]);
    assert!(!view.conversation_leading(1, false));
    assert!(!view.conversation_leading(1, true));

    // A user row NEWER than the separator ends the scan, so the
    // agent message leads.
    let view = view_with(vec![tool_only_assistant(), user(), agent_message_row()]);
    assert!(view.conversation_leading(2, false));
    assert!(view.conversation_leading(2, true));
    // With the separator NEWER than the non-compact row, the separator
    // dominates (TS returns the tool separator with a trailing space).
    let view = view_with(vec![user(), tool_only_assistant(), agent_message_row()]);
    assert!(!view.conversation_leading(2, false));
    assert!(!view.conversation_leading(2, true));
    // A compact row older than the separator ends the scan WITHOUT it
    // (TS falls through the `toolSeparator` branch to the compact row).
    let view = view_with(vec![
        settled_tool_card("c2"),
        tool_only_assistant(),
        agent_message_row(),
    ]);
    assert!(!view.conversation_leading(2, false));
    assert!(view.conversation_leading(2, true));

    // A hidden thinking-only assistant contributes nothing to spacing:
    // the scan skips it to the user row.
    let hidden_assistant = || {
        ChatEntry::Assistant(Box::new(AssistantMessage {
            blocks: vec![MessageBlock::Thinking("quiet".to_string())],
            has_tool_calls: false,
            streaming: false,
            error: None,
            aborted: false,
        }))
    };
    let mut view = view_with(vec![user(), hidden_assistant(), agent_message_row()]);
    view.detail = Detail::Overview;
    assert!(view.conversation_leading(2, false));
}

/// Collapsed thinking folds to one row between spacers: the body stays hidden, the fold
/// names it with its token estimate.
#[test]
fn collapsed_thinking_folds_to_one_row() {
    let mut view = view_with(vec![
        user_row(),
        agent_message_row(),
        thinking_tool_assistant(),
        settled_tool_card("c1"),
    ]);
    view.detail = Detail::Overview;
    let text = transcript_text(&mut view, 80);
    assert!(!text.contains("thinking body"), "collapsed hides the body");
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let header_row = lines
        .iter()
        .position(|line| line.contains("bash \u{b7} done"))
        .expect("the tool card header renders");
    assert_eq!(
        lines[header_row - 4..header_row],
        [
            " \u{2709} Agent message \u{b7} \u{2193} lane",
            "",
            " \u{2022} Thought \u{b7} ~4 tokens",
            ""
        ],
        "{text}"
    );
}

#[test]
fn a_visible_assistant_after_an_agent_message_keeps_its_spacers() {
    let mut view = view_with(vec![
        user_row(),
        agent_message_row(),
        ChatEntry::Assistant(Box::new(AssistantMessage {
            blocks: vec![MessageBlock::Text("answer body".to_string())],
            has_tool_calls: true,
            streaming: false,
            error: None,
            aborted: false,
        })),
        settled_tool_card("c1"),
    ]);
    view.detail = Detail::Overview;
    let text = transcript_text(&mut view, 80);
    let lines: Vec<&str> = text.lines().collect();
    let agent_row = lines
        .iter()
        .position(|line| line.contains("Agent message \u{b7} \u{2193} lane"))
        .expect("the agent-message row renders");
    // In overview the agent message renders its header alone; the visible assistant body keeps
    // the tool separator (TS `hasTrailingSpace`).
    assert!(lines[agent_row + 1].trim().is_empty(), "{text}");
    assert!(lines[agent_row + 2].contains("answer body"), "{text}");
    assert!(lines[agent_row + 3].trim().is_empty(), "{text}");
    assert!(lines[agent_row + 4].contains("bash \u{b7} done"), "{text}");
}

#[test]
fn custom_rows_render_in_the_transcript() {
    let mut view = view_with(vec![agent_message_row(), shell_completion_row()]);
    view.detail = Detail::All;
    let text = transcript_text(&mut view, 80);
    assert!(text.contains("Agent message \u{b7} \u{2193} lane"));
    assert!(text.contains("\u{2570}\u{2500} hi"));
    assert!(text.contains("Background shell command finished"));
    assert!(text.contains("[bash-done]"));
}

#[test]
fn streaming_assistant_updates_across_frames() {
    let mut view = view_with(vec![ChatEntry::Assistant(Box::new(AssistantMessage {
        blocks: vec![MessageBlock::Text("so far".to_string())],
        has_tool_calls: false,
        streaming: true,
        error: None,
        aborted: false,
    }))]);
    let frame0 = transcript_text(&mut view, 80);
    assert!(frame0.contains("so far"));
    if let Some(ChatEntry::Assistant(open)) = view.chat.get_mut(0) {
        open.blocks = vec![MessageBlock::Text("so far, and more".to_string())];
    }
    view.mark_entry_stale(0);
    let frame1 = transcript_text(&mut view, 80);
    assert!(frame1.contains("and more"));
}

/// A confirmation posted while a panel replaces the editor dock (so the empty composer's row is
/// off screen) shows as the overlay toast, not in the hidden composer row.
#[test]
fn a_toast_under_a_panel_that_replaces_the_editor_stays_visible() {
    let mut v = view();
    v.reload_box = Some("reloading".to_string());
    v.toast("Factory run r1 is now paused");
    let frame = v.render_frame(80, 24);
    assert!(
        frame
            .iter()
            .any(|row| row_text(row).contains("Factory run r1 is now paused")),
        "the confirmation is on screen: {:?}",
        frame.iter().map(row_text).collect::<Vec<_>>()
    );
}

/// The action toast renders as a compact right-aligned pill (the covered row keeps its own
/// content).
#[test]
fn action_toasts_render_as_a_pill_coalesce_and_auto_dismiss() {
    // A transcript taller than the window puts real content on the
    // window's top row, so the pill lands over a covered row.
    let mut view = view_with(
        (0..40)
            .map(|index| ChatEntry::Status {
                text: format!("covered line {index}"),
                kind: crate::chat::StatusKind::Info,
            })
            .collect(),
    );
    view.toasts.push("Copied to clipboard");
    let frame = view.render_frame(60, 24);
    let rows: Vec<String> = frame
        .iter()
        .map(|line| line.iter().map(|span| span.content.as_str()).collect())
        .collect();
    let toast_row = rows
        .iter()
        .position(|row| row.contains("Copied to clipboard"))
        .expect("the toast renders");
    // Right-aligned: the top bar stays above the overlay (fullscreen:
    // row 0).
    assert!(toast_row >= 1, "the toast sits below the top bar");
    assert!(
        rows[toast_row].contains("covered line"),
        "the covered row keeps its content: {:?}",
        rows[toast_row]
    );
    let frame = view.render_frame(60, 24);
    let pill = frame
        .iter()
        .flatten()
        .find(|span| span.content.contains("Copied to clipboard"))
        .expect("the pill renders");
    assert!(
        pill.style.add_modifier.contains(Modifier::REVERSED),
        "the pill carries the reversed-chip style: {:?}",
        pill.style
    );
    // Regression (the operator's brand-purple directive): the pill's
    // color is the theme's Accent token, never the Success green.
    assert_eq!(
        pill.style.fg,
        view.theme.fg_style(ThemeColor::Accent).fg,
        "the pill carries the brand-purple Accent style: {:?}",
        pill.style
    );
    assert_ne!(
        pill.style.fg,
        view.theme.fg_style(ThemeColor::Success).fg,
        "the pill must not carry the action green: {:?}",
        pill.style
    );
    view.toasts.push("Copied to clipboard");
    view.toasts.push("Copied to clipboard");
    let frame = view.render_frame(60, 24);
    let joined: String = frame
        .iter()
        .map(|line| {
            line.iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let toast_rows = joined
        .lines()
        .filter(|row| row.contains("Copied to clipboard"))
        .count();
    assert_eq!(
        toast_rows, 1,
        "one coalesced toast row, not stacked: {joined}"
    );
    assert!(
        joined.contains("Copied to clipboard (x3)"),
        "the count bump acknowledges every copy: {joined}"
    );
    view.toasts
        .age_by(crate::toast::TOAST_TTL + std::time::Duration::from_millis(1));
    let frame = view.render_frame(60, 24);
    let joined: String = frame
        .iter()
        .map(|line| {
            line.iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !joined.contains("Copied to clipboard"),
        "the expired toast is gone: {joined}"
    );
}

/// The browse header inserts BELOW the editor's top row with an empty
/// companion row (TS `CustomEditor.render`'s two header rows).
#[test]
fn browse_header_pair_sits_below_the_editor_top_row() {
    let mut v = view();
    v.queue_selected = Some(crate::queued::QueueSelectionItem {
        lane: crate::queued::QueueLane::Steering,
        index: 0,
        position: 0,
        text: "turn right".to_string(),
    });
    let frame = v.render_frame(80, 24);
    let joined: Vec<String> = frame.iter().map(text_of).collect();
    // The header truncates at the content width; `browse` sits inside
    // the visible prefix (the queue is empty here).
    let header_row = joined
        .iter()
        .position(|row| row.contains("browse"))
        .expect("the queue browse header renders");
    assert!(
        joined[header_row - 1].trim().is_empty(),
        "the editor top row stays above the header: {:?}",
        joined[header_row - 1]
    );
    assert!(
        joined[header_row + 1].trim().is_empty(),
        "the empty companion row follows the header: {:?}",
        joined[header_row + 1]
    );
    assert!(
        joined[header_row + 2].contains("> "),
        "the content rows shift below the header pair: {:?}",
        joined[header_row + 2]
    );
}
// The collapsed view (operator directive 2026-09-28): "collapse mode should just be details mode,
// but WITHOUT THINKING BLOCKS".

fn collapsed_view(entries: Vec<ChatEntry>) -> AgentView {
    let mut view = view_with(entries);
    view.detail = Detail::Overview;
    view
}

fn thinking_only() -> ChatEntry {
    ChatEntry::Assistant(Box::new(crate::chat::AssistantMessage {
        blocks: vec![crate::chat::MessageBlock::Thinking("hmm".to_string())],
        has_tool_calls: true,
        streaming: false,
        error: None,
        aborted: false,
    }))
}

fn settled_cards(count: usize) -> Vec<ChatEntry> {
    (0..count)
        .map(|index| settled_tool_card(&format!("card{index}")))
        .collect()
}

#[test]
fn the_collapsed_view_renders_every_activity_item_as_details_does() {
    let mut entries = settled_cards(3);
    entries.push(thinking_only());
    entries.push(agent_message_row());
    entries.push(crate::chat::ChatEntry::User {
        text: "go".to_string(),
        sent_at: None,
        images: Vec::new(),
    });
    entries.extend(settled_cards(2));
    let mut view = collapsed_view(entries);
    let text = transcript_text(&mut view, 80);
    assert!(
        text.matches("bash \u{b7} done").count() == 5,
        "every tool card renders its own panel rows: {text}"
    );
    assert!(
        text.contains("Agent message \u{b7} \u{2193} lane"),
        "the agent-message notice keeps its own row: {text}"
    );
    assert!(
        !text.contains("hi"),
        "the collapsed notice row carries no body preview: {text}"
    );
    assert!(
        !text.contains("tool calls"),
        "no condensed-run summary block renders: {text}"
    );
    assert!(
        !text.contains("hmm"),
        "the collapsed view hides the thinking: {text}"
    );
    view.detail = view.detail.next();
    assert_eq!(view.detail, Detail::Details);
    let details = transcript_text(&mut view, 80);
    assert!(
        details.matches("bash \u{b7} done").count() == 5,
        "every card keeps its own rows at details: {details}"
    );
    assert!(
        details.contains("Agent message \u{b7} \u{2193} lane"),
        "the notice keeps its own row at details: {details}"
    );
    assert!(
        details.contains("hmm"),
        "the thinking is visible at details"
    );
}

#[test]
fn a_running_card_updates_between_frames() {
    let mut view = collapsed_view(Vec::new());
    view.push_entry(crate::chat::ChatEntry::User {
        text: "go".to_string(),
        sent_at: None,
        images: Vec::new(),
    });
    view.push_entry(ChatEntry::Tool(Box::new(ToolCallCard {
        id: "run_r".to_string(),
        name: "bash".to_string(),
        args: serde_json::json!({"command": "sleep 1"}),
        started: true,
        started_at: Some(std::time::Instant::now()),
        ended_at: None,
        result: None,
        result_partial: false,
        ..Default::default()
    })));
    view.pulse_frame = 0;
    let frame0 = transcript_text(&mut view, 80);
    assert!(
        frame0.contains("running"),
        "the running card renders its own rows: {frame0}"
    );
    view.pulse_frame = 1;
    let frame1 = transcript_text(&mut view, 80);
    assert_ne!(
        frame0, frame1,
        "the working icon animates with the pulse frame"
    );
    assert!(
        !frame1.contains("tool calls"),
        "no condensed block renders while the run is live: {frame1}"
    );
}

#[test]
fn streamed_cards_render_their_own_rows_immediately() {
    let streamed = |id: &str| {
        ChatEntry::Tool(Box::new(ToolCallCard {
            id: id.to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({"command": "ls"}),
            started: true,
            started_at: Some(std::time::Instant::now()),
            ..Default::default()
        }))
    };
    let mut view = collapsed_view(Vec::new());
    view.push_entry(crate::chat::ChatEntry::User {
        text: "go".to_string(),
        sent_at: None,
        images: Vec::new(),
    });
    view.push_entry(streamed("c0"));
    let one = transcript_text(&mut view, 80);
    assert!(
        one.contains("running"),
        "the first streamed card renders at once: {one}"
    );
    view.push_entry(streamed("c1"));
    view.push_entry(streamed("c2"));
    let three = transcript_text(&mut view, 80);
    assert_eq!(
        three.matches("running").count(),
        3,
        "every streamed card keeps its own row: {three}"
    );
    assert!(
        !three.contains("tool calls"),
        "no summary block ever forms: {three}"
    );
}

#[test]
fn a_landing_result_with_receipts_settles_the_card() {
    let mut view = collapsed_view(Vec::new());
    view.push_entry(crate::chat::ChatEntry::User {
        text: "go".to_string(),
        sent_at: None,
        images: Vec::new(),
    });
    view.push_entry(ChatEntry::Tool(Box::new(ToolCallCard {
        id: "cell".to_string(),
        name: "ipython".to_string(),
        args: serde_json::json!({"code": "print(1)"}),
        started: true,
        started_at: Some(std::time::Instant::now()),
        ..Default::default()
    })));
    let index = view.chat.len() - 1;
    view.prepare_entry_mutation(index);
    if let Some(ChatEntry::Tool(card)) = view.chat.get_mut(index) {
        card.result = Some(ToolResultView {
            content: vec![serde_json::json!({"type": "text", "text": "done"})],
            details: serde_json::json!({
                "sentAgentMessages": [
                    { "id": "m1", "message": "a", "deliveryStatus": "delivered", "receiverRole": "parent" }
                ]
            }),
            is_error: false,
        });
        // The final result settles the call (the pipeline stamps
        // `ended_at` with every settle path): the live timer stops.
        card.ended_at = Some(std::time::Instant::now());
    }
    view.mark_entry_stale(index);
    let text = transcript_text(&mut view, 80);
    assert!(
        text.contains("python \u{b7} print(1)"),
        "the settled cell renders its own rows: {text}"
    );
    assert!(
        text.contains("Agent message"),
        "the landed receipt's notice row renders: {text}"
    );
    assert!(
        !text.contains("tool calls"),
        "no condensed block forms on the landing: {text}"
    );
    // The same rows render at `details` (this transcript has no
    // thinking).
    view.detail = Detail::Details;
    let details = transcript_text(&mut view, 80);
    let collapsed_body = text.replace("Collapsed mode (Ctrl+O to expand)", "MODE");
    let details_body = details.replace("Details mode (Ctrl+O to expand)", "MODE");
    assert_eq!(
        collapsed_body, details_body,
        "the collapsed view renders the activity exactly as details does"
    );
}

#[test]
fn settled_rows_survive_a_cache_roundtrip() {
    let mut view = collapsed_view(settled_cards(3));
    let first = transcript_text(&mut view, 80);
    let second = transcript_text(&mut view, 80);
    assert_eq!(first, second, "the cached card rows are stable");
    let mut fresh = collapsed_view(settled_cards(3));
    let fresh_text = transcript_text(&mut fresh, 80);
    assert_eq!(
        first.replace("0s", "").replace("0.0s", ""),
        fresh_text.replace("0s", "").replace("0.0s", ""),
        "a fresh view renders the same rows (the Took clock may move)"
    );
}
