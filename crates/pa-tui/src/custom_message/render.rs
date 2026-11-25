//! Custom-message row rendering: each component's row geometry and theme
//! colors. The refinement components live in the sibling `refinement`
//! module; compaction outcomes render through the chat status rows.

use super::{
    AgentMessageDirection, AgentMessageRow, BackgroundCompletionRow, BackgroundOutcome,
    CustomPanelRow, AGENT_MESSAGE_LABEL,
};
use crate::chat::Detail;
use crate::theme::{Theme, ThemeColor};
use crate::width::{pad_line, str_width, truncate_line, wrap_line, wrap_text};
use crate::{Line, Span};
use ratatui::style::Style;

pub(crate) fn spacer() -> Line {
    Vec::new()
}

/// The bold `[<name>]` label shared by the skill card and the generic
/// custom panel.
pub(crate) fn custom_message_label(name: &str, theme: &Theme) -> Span {
    Span::styled(
        format!("[{name}]"),
        theme
            .fg_style(ThemeColor::CustomMessageLabel)
            .add_modifier(ratatui::style::Modifier::BOLD),
    )
}

/// A `Text(spans, 1, 0)` row set: content wrapped at `width - 2`, one margin
/// column, padded to the full width with the default style.
pub(crate) fn text_rows(spans: &Line, width: usize) -> Vec<Line> {
    let content_width = width.saturating_sub(2).max(1);
    let flat: String = spans.iter().map(|s| s.content.as_str()).collect();
    if flat.trim().is_empty() {
        return Vec::new();
    }
    wrap_line(spans, content_width)
        .into_iter()
        .map(|wrapped| {
            let mut row: Line = vec![Span::raw(" ")];
            row.extend(wrapped);
            pad_line(row, width)
        })
        .collect()
}

/// The `✉ <label> · <participant>` summary line, with sanctioned divergences from TS:
/// the `✉` mail envelope (Kevin directive 2026-09-24) renders green (operator directive
/// 2026-09-24), and the participant is the viewer-relative arrow plus the counterpart's
/// name (operator directive 2026-09-25).
pub(crate) fn agent_message_summary_line(
    direction: AgentMessageDirection,
    counterpart: &str,
    theme: &Theme,
) -> Line {
    let arrow = match direction {
        AgentMessageDirection::Received => "\u{2193}",
        AgentMessageDirection::Sent | AgentMessageDirection::Queued => "\u{2191}",
    };
    vec![
        Span::styled("\u{2709}".to_string(), theme.fg_style(ThemeColor::Success)),
        Span::raw(" "),
        Span::styled(
            AGENT_MESSAGE_LABEL.to_string(),
            theme.fg_style(ThemeColor::Muted),
        ),
        Span::styled(" \u{b7} ".to_string(), theme.fg_style(ThemeColor::Dim)),
        Span::styled(
            format!("{arrow} {counterpart}"),
            theme.fg_style(ThemeColor::Dim),
        ),
    ]
}

/// The received agent-message rows: a leading blank (spacing-driven), the summary header
/// (no body preview), and the `╰─`-guttered body when expanded.
pub(crate) fn render_agent_message(
    row: &AgentMessageRow,
    detail: Detail,
    theme: &Theme,
    width: usize,
    leading: bool,
) -> Vec<Line> {
    let mut out = Vec::new();
    if leading {
        out.push(spacer());
    }
    let header = agent_message_summary_line(row.direction, &row.counterpart, theme);
    out.extend(text_rows(&header, width));
    if detail.tool_output_expanded() {
        out.extend(agent_message_body(&row.message, theme, width));
    }
    out
}

/// Each source line wraps at `width - 4`: the first rendered line carries
/// the `╰─ ` gutter, the rest three spaces, all in `customMessageText`.
pub(crate) fn agent_message_body(message: &str, theme: &Theme, width: usize) -> Vec<Line> {
    let safe_width = width.max(1);
    let text_width = super::geometry::agent_body_width(width);
    let body = theme.fg_style(ThemeColor::CustomMessageText);
    let mut lines: Vec<Line> = Vec::new();
    for source in message.split('\n') {
        let wrapped = wrap_text(source, text_width);
        for mut line in wrapped {
            crate::hyperlinks::strip_osc8(&mut line);
            lines.push(line);
        }
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    let dim = theme.fg_style(ThemeColor::Dim);
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            // The first rendered line carries the dim `╰─ ` gutter,
            // continuation lines three unstyled spaces.
            let mut row: Line = vec![Span::raw(" ")];
            if index == 0 {
                row.push(Span::styled("\u{2570}\u{2500} ".to_string(), dim));
            } else {
                row.push(Span::raw("   "));
            }
            for span in line {
                row.push(Span::styled(span.content, body));
            }
            truncate_line(&row, safe_width, "")
        })
        .collect()
}

/// Plain-text truncate with an explicit ellipsis.
pub(crate) fn truncate_text(text: &str, width: usize, ellipsis: &str) -> String {
    let line: Line = vec![Span::raw(text.to_string())];
    let truncated = truncate_line(&line, width, ellipsis);
    truncated
        .iter()
        .map(|s| s.content.as_str())
        .collect::<String>()
}

/// One background-job notice row, standalone form: the header mark, then the
/// raw content under the branch gutter when expanded. A quiet notice names
/// its first command between its label and its silence, cut to fit; a row
/// too narrow for a useful piece of it keeps the label and the silence.
pub(crate) fn render_background_completion(
    row: &BackgroundCompletionRow,
    detail: Detail,
    theme: &Theme,
    width: usize,
    leading: bool,
) -> Vec<Line> {
    /// The fewest columns of a quiet notice's command worth showing.
    const QUIET_COMMAND_MIN: usize = 10;
    let (tone, mark, label) = match &row.outcome {
        BackgroundOutcome::Shell {
            exit_code: Some(code),
            ..
        } if *code != 0 => (
            ThemeColor::Error,
            "\u{2717}",
            format!("Background shell command failed \u{b7} exit {code}"),
        ),
        BackgroundOutcome::Shell { .. } => (
            ThemeColor::Muted,
            "\u{2713}",
            "Background shell command finished".to_string(),
        ),
        BackgroundOutcome::Cell { status } => match status.as_deref() {
            Some("error") => (
                ThemeColor::Error,
                "\u{2717}",
                "Background cell failed".to_string(),
            ),
            Some("interrupted") => (
                ThemeColor::Error,
                "\u{2717}",
                "Background cell interrupted".to_string(),
            ),
            Some(_) | None => (
                ThemeColor::Muted,
                "\u{2713}",
                "Background cell finished".to_string(),
            ),
        },
        BackgroundOutcome::Quiet {
            command,
            others,
            quiet_ms,
        } => {
            let head = if *others == 0 {
                "Background command quiet"
            } else {
                "Background commands quiet"
            };
            let more = if *others == 0 {
                String::new()
            } else {
                format!(" +{others} more")
            };
            let silence = quiet_ms.map(crate::duration_text::millis);
            let quiet = silence
                .as_deref()
                .map_or_default(|silence| format!(" \u{b7} quiet {silence}"));
            // The columns left for the command: the mark's, the label's and
            // the rest's taken.
            let room =
                width.saturating_sub(str_width(&format!(" \u{25f7} {head} \u{b7} {more}{quiet}")));
            let label = match command {
                Some(command) if room >= QUIET_COMMAND_MIN.min(str_width(command)) => {
                    let command = crate::width::truncate_to_width(
                        command,
                        room,
                        crate::menu_chrome::ELLIPSIS,
                    );
                    format!("{head} \u{b7} {command}{more}{quiet}")
                }
                Some(_) | None => match silence {
                    Some(silence) => format!("{head} \u{b7} {silence}"),
                    None => head.to_string(),
                },
            };
            (ThemeColor::Warning, "\u{25f7}", label)
        }
    };
    let header = truncate_line(
        &vec![Span::styled(
            format!(" {mark} {label}"),
            theme.fg_style(tone),
        )],
        width,
        crate::menu_chrome::ELLIPSIS,
    );
    let mut out = Vec::new();
    if leading {
        out.push(spacer());
    }
    out.push(header);
    if detail.tool_output_expanded() {
        let mut body =
            crate::branch::branch_block(&vec![Span::raw(row.content.clone())], theme, width);
        body.iter_mut().for_each(crate::hyperlinks::strip_osc8);
        out.extend(body);
    }
    out
}

/// Pad a rendered line to the full width with a base style.
pub(crate) fn pad_with(mut line: Line, width: usize, base: Style) -> Line {
    let used: usize = line.iter().map(|s| str_width(&s.content)).sum();
    if used < width {
        line.push(Span::styled(" ".repeat(width - used), base));
    }
    line
}

/// One generic custom row: a leading blank, the bold `[<customType>]` label, then the
/// always-shown markdown body in `customMessageText` under the branch gutter.
pub(crate) fn render_custom_panel(row: &CustomPanelRow, theme: &Theme, width: usize) -> Vec<Line> {
    let md = super::geometry::markdown_style(ThemeColor::CustomMessageText, theme);
    let mut out = vec![spacer()];
    out.extend(text_rows(
        &vec![custom_message_label(&row.custom_type, theme)],
        width,
    ));
    out.extend(crate::branch::branch_markdown(
        &row.content,
        &md,
        theme,
        width,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Detail;
    use crate::theme::{ColorMode, Theme};
    use crate::Span;

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    fn flat(row: &Line) -> String {
        row.iter().map(|s| s.content.as_str()).collect()
    }

    #[test]
    fn agent_message_header_shape() {
        let row = AgentMessageRow {
            direction: AgentMessageDirection::Received,
            counterpart: "model-probe".to_string(),
            message: "ready".to_string(),
        };
        let rows = render_agent_message(&row, Detail::Overview, &theme(), 60, true);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].is_empty());
        let header = flat(&rows[1]);
        assert_eq!(
            header.trim_end(),
            " \u{2709} Agent message \u{b7} \u{2193} model-probe"
        );
        assert!(!header.contains("ready"), "no preview: {header:?}");
        // Colors: green envelope, muted label, dim viewer-relative arrow
        // plus name.
        let green = theme().fg_style(ThemeColor::Success);
        let muted = theme().fg_style(ThemeColor::Muted);
        let dim = theme().fg_style(ThemeColor::Dim);
        assert_eq!(rows[1][0], Span::styled(" ".to_string(), Style::default()));
        assert_eq!(rows[1][1], Span::styled("\u{2709}".to_string(), green));
        assert_eq!(rows[1][3], Span::styled("Agent message".to_string(), muted));
        assert_eq!(rows[1][4], Span::styled(" \u{b7} ".to_string(), dim));
        assert_eq!(
            rows[1][5],
            Span::styled("\u{2193} model-probe".to_string(), dim)
        );
    }

    #[test]
    fn agent_message_header_never_carries_the_body() {
        // An empty body and a long body render the SAME collapsed header.
        for message in ["  \n  ".to_string(), format!("{} end", "word ".repeat(20))] {
            let row = AgentMessageRow {
                direction: AgentMessageDirection::Received,
                counterpart: "root".to_string(),
                message,
            };
            let rows = render_agent_message(&row, Detail::Overview, &theme(), 60, false);
            assert_eq!(rows.len(), 1, "one header row: {rows:?}");
            let header = flat(&rows[0]).trim_end().to_string();
            assert_eq!(header, " \u{2709} Agent message \u{b7} \u{2193} root");
            assert!(!header.contains("word"), "no preview: {header:?}");
            assert!(!header.contains("\u{2026}"), "no ellipsis: {header:?}");
        }
    }

    /// The arrow comes from the row's actual direction, never parsed out of a string.
    #[test]
    fn agent_message_arrows_follow_the_row_direction() {
        for (direction, arrow) in [
            (AgentMessageDirection::Received, "\u{2193}"),
            (AgentMessageDirection::Sent, "\u{2191}"),
            (AgentMessageDirection::Queued, "\u{2191}"),
        ] {
            let row = AgentMessageRow {
                direction,
                counterpart: "worker".to_string(),
                message: "ping".to_string(),
            };
            let rows = render_agent_message(&row, Detail::Overview, &theme(), 80, false);
            let header = flat(&rows[0]);
            assert!(
                header.contains(&format!("\u{2709} Agent message \u{b7} {arrow} worker")),
                "{direction:?} header: {header}"
            );
            assert!(
                !header.contains("ping"),
                "the body never leaks into the collapsed row: {header}"
            );
        }
    }

    #[test]
    fn agent_message_body_gutter_when_expanded() {
        let row = AgentMessageRow {
            direction: AgentMessageDirection::Received,
            counterpart: "root".to_string(),
            message: "line one\nline two".to_string(),
        };
        let rows = render_agent_message(&row, Detail::All, &theme(), 60, false);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(flat(&rows[1]), " \u{2570}\u{2500} line one");
        assert_eq!(flat(&rows[2]), "    line two");
        let dim = theme().fg_style(ThemeColor::Dim);
        let body = theme().fg_style(ThemeColor::CustomMessageText);
        // The first rendered line carries the dim gutter, continuation
        // rows three unstyled spaces.
        assert_eq!(
            rows[1][1],
            Span::styled("\u{2570}\u{2500} ".to_string(), dim)
        );
        assert_eq!(rows[2][1], Span::raw("   "));
        assert!(rows[1]
            .iter()
            .any(|span| span.content == "line one" && span.style == body));
    }

    #[test]
    fn cell_completion_rows() {
        let header = |status: Option<&str>| {
            let row = BackgroundCompletionRow {
                outcome: BackgroundOutcome::Cell {
                    status: status.map(str::to_string),
                },
                content: "[cell-done cell:c1 status:ok]".to_string(),
            };
            let rows = render_background_completion(&row, Detail::Overview, &theme(), 60, false);
            (flat(&rows[0]), rows[0][0].style)
        };
        let muted = theme().fg_style(ThemeColor::Muted);
        let error = theme().fg_style(ThemeColor::Error);
        assert_eq!(
            [
                header(Some("ok")),
                header(Some("error")),
                header(Some("interrupted")),
                header(None)
            ],
            [
                (" \u{2713} Background cell finished".to_string(), muted),
                (" \u{2717} Background cell failed".to_string(), error),
                (" \u{2717} Background cell interrupted".to_string(), error),
                (" \u{2713} Background cell finished".to_string(), muted),
            ]
        );
    }

    #[test]
    fn shell_completion_rows() {
        let ok = BackgroundCompletionRow {
            outcome: crate::custom_message::BackgroundOutcome::Shell {
                pid: Some(4371),
                exit_code: Some(0),
            },
            content: "[bash-done pid:4371 exit:0]".to_string(),
        };
        let rows = render_background_completion(&ok, Detail::Overview, &theme(), 60, true);
        assert!(rows[0].is_empty());
        assert_eq!(
            flat(&rows[1]),
            " \u{2713} Background shell command finished"
        );
        assert_eq!(
            rows[1][0].style,
            theme().fg_style(ThemeColor::Muted),
            "muted when exit 0"
        );
        let failed = BackgroundCompletionRow {
            outcome: crate::custom_message::BackgroundOutcome::Shell {
                pid: Some(11),
                exit_code: Some(2),
            },
            content: "[bash-done pid:11 exit:2]".to_string(),
        };
        let rows = render_background_completion(&failed, Detail::Overview, &theme(), 60, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            flat(&rows[0]),
            " \u{2717} Background shell command failed \u{b7} exit 2"
        );
        assert_eq!(
            rows[0][0].style,
            theme().fg_style(ThemeColor::Error),
            "error when failed"
        );
        // The expanded body is the raw content under the branch gutter: the gutter on
        // the first content row, the four-column continuation on the blank source line.
        let row = BackgroundCompletionRow {
            outcome: crate::custom_message::BackgroundOutcome::Shell {
                pid: Some(99),
                exit_code: Some(0),
            },
            content: "[bash-done pid:99 exit:0]\n\nCommand: \"printf done\"".to_string(),
        };
        let rows = render_background_completion(&row, Detail::All, &theme(), 60, true);
        let trimmed: Vec<String> = rows
            .iter()
            .map(|row| flat(row).trim_end().to_string())
            .collect();
        assert_eq!(
            trimmed,
            vec![
                "",
                " \u{2713} Background shell command finished",
                " \u{2570}\u{2500} [bash-done pid:99 exit:0]",
                "",
                "    Command: \"printf done\"",
            ],
            "{rows:?}"
        );
    }

    /// A `[bash-quiet]` notice is one compact warning row: its first command between the label
    /// and the silence, cut to fit with the one cut mark; the other commands it names as a count;
    /// on a row too narrow for a useful piece of the command, the label and the silence alone.
    #[test]
    fn quiet_notice_rows() {
        let quiet = |jobs: serde_json::Value| {
            let message = serde_json::json!({
                "role": "custom",
                "customType": "async_bash_quiet",
                "content": "[bash-quiet] background commands still running",
                "display": true,
                "details": { "jobs": jobs },
            });
            match &crate::custom_message::custom_message_entries(&message)[..] {
                [crate::chat::ChatEntry::BackgroundCompletion(row)] => (**row).clone(),
                other => panic!("{other:?}"),
            }
        };
        let job = |command: &str, quiet_ms: u64| {
            serde_json::json!({
                "job": "a1",
                "pid": 7,
                "command": command,
                "elapsedMs": quiet_ms + 5_000,
                "quietMs": quiet_ms,
            })
        };
        let long = quiet(serde_json::json!([job(
            "cargo test --workspace   --all-features\n  -- --nocapture --test-threads 1",
            180_000
        )]));
        let short = quiet(serde_json::json!([job("npm run dev", 750_000)]));
        let two = quiet(serde_json::json!([
            job("npm run dev", 750_000),
            job("tail -f log", 200_000)
        ]));
        let warning = theme().fg_style(ThemeColor::Warning);
        let row = |text: &str| vec![Span::styled(text.to_string(), warning)];
        assert_eq!(
            [
                (&long, 80),
                (&long, 40),
                (&short, 80),
                (&short, 40),
                (&two, 80),
                (&two, 40),
            ]
            .map(|(row, width)| {
                render_background_completion(row, Detail::Overview, &theme(), width, false)
            }),
            [
                vec![row(" \u{25f7} Background command quiet \u{b7} cargo test --workspace --all-featu\u{2026} \u{b7} quiet 3m 00s")],
                vec![row(" \u{25f7} Background command quiet \u{b7} 3m 00s")],
                vec![row(" \u{25f7} Background command quiet \u{b7} npm run dev \u{b7} quiet 12m 30s")],
                vec![row(" \u{25f7} Background command quiet \u{b7} 12m 30s")],
                vec![row(" \u{25f7} Background commands quiet \u{b7} npm run dev +1 more \u{b7} quiet 3m 20s")],
                vec![row(" \u{25f7} Background commands quiet \u{b7} 3m 20s")],
            ]
        );
    }

    /// The un-boxed panel's label and body carry their shared spans with no box background.
    #[test]
    fn custom_panel_guttered_shape() {
        let row = CustomPanelRow {
            custom_type: "autonomous_status".to_string(),
            content: "[autonomous-status: on]".to_string(),
        };
        let rows = render_custom_panel(&row, &theme(), 40);
        // No box background anywhere on the row.
        assert_eq!(
            rows[1][1],
            custom_message_label("autonomous_status", &theme())
        );
        assert!(rows[1].iter().all(|span| span.style.bg.is_none()));
        let body = rows[2]
            .iter()
            .find(|span| span.content.contains("[autonomous-status: on]"))
            .expect("the body text renders");
        assert_eq!(
            body.style.fg,
            theme().fg_style(ThemeColor::CustomMessageText).fg
        );
    }
}
