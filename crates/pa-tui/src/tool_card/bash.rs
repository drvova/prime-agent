//! The `bash` tool-call card: the `$ command` call row, the output
//! (collapsed: up to five visual lines whole, longer output as its first
//! and last two around a `⋮ +N lines` mark; expanded: everything), the truncation warning, and the live
//! `Took`/`Elapsed` duration row.

use serde_json::Value;

use super::layout::{panel_content_width, RowOutput};
use super::{ToolCallCard, ToolResultView};
use crate::chat::Detail;
use crate::code_preview::{preview_bash_command, CodePreviewLanguage};
use crate::theme::{Theme, ThemeColor};
use crate::width::{wrap_text, wrapped_text_count};
use crate::{Line, Span};

/// Output up to this many visual lines shows whole when collapsed (TS `BASH_PREVIEW_LINES`).
const BASH_PREVIEW_LINES: usize = 5;
/// Longer collapsed output keeps this many rows at each end around the `⋮ +N lines` mark.
const BASH_EDGE_LINES: usize = 2;

#[must_use]
pub fn render(
    card: &ToolCallCard,
    frame: usize,
    detail: Detail,
    theme: &Theme,
    width: usize,
    show_images: bool,
) -> Vec<Line> {
    let mut rows = RowOutput::paint();
    visit(card, detail, theme, width, &mut rows);
    rows.images(card.result.as_ref(), show_images, theme);
    rows.panel(card, frame, theme, width);
    rows.into_lines()
}

pub(crate) fn count(
    card: &ToolCallCard,
    frame: usize,
    detail: Detail,
    theme: &Theme,
    width: usize,
    show_images: bool,
) -> usize {
    let mut rows = RowOutput::count();
    visit(card, detail, theme, width, &mut rows);
    rows.images(card.result.as_ref(), show_images, theme);
    rows.panel(card, frame, theme, width);
    rows.len()
}

fn visit(card: &ToolCallCard, detail: Detail, theme: &Theme, width: usize, rows: &mut RowOutput) {
    let content_width = panel_content_width(width);
    rows.wrapped_line(&format_bash_call(card, theme), content_width);
    if let Some(result) = &card.result {
        bash_result_rows(
            result,
            detail.tool_output_expanded(),
            theme,
            content_width,
            rows,
        );
    }
    // The live `Took`/`Elapsed` duration row: the shared emitter ticks
    // the running card's elapsed on every repaint (a running call has no
    // result rows yet, so the row is the card's only timing until the
    // streamed output arrives).
    super::duration_row(card, theme, content_width, rows);
}

/// The `$ command` call row (TS `formatBashCall`): dim, with the command
/// preview (a non-bash language prefixes its label) and the timeout suffix.
fn format_bash_call(card: &ToolCallCard, theme: &Theme) -> Line {
    let dim = theme.fg_style(ThemeColor::Dim);
    let error = theme.fg_style(ThemeColor::Error);
    let tool_output = theme.fg_style(ThemeColor::ToolOutput);
    let command = card.args.get("command");
    // `str(value)`: a missing or non-string command is invalid; a null or
    // empty string renders the `...` placeholder.
    let invalid = command.is_some_and(|value| !value.is_string());
    let command = command.and_then(Value::as_str);
    let mut row: Line = vec![Span::styled("$ ".to_string(), dim)];
    if invalid {
        row.push(Span::styled("[invalid arg]".to_string(), error));
    } else if command.unwrap_or_default().is_empty() {
        row.push(Span::styled("...".to_string(), tool_output));
    } else {
        let command = command.unwrap_or_default();
        let preview = preview_bash_command(command);
        let display = if preview.text.is_empty() {
            command.to_string()
        } else {
            match preview.language {
                CodePreviewLanguage::Bash => preview.text,
                CodePreviewLanguage::Python => format!("python: {}", preview.text),
            }
        };
        row.push(Span::styled(display, dim));
    }
    if let Some(timeout) = card.args.get("timeout").and_then(Value::as_f64) {
        row.push(Span::styled(format!(" (timeout {timeout:.0}s)"), dim));
    }
    row
}

/// The result rows (TS `rebuildBashResultRenderComponent`): output rows,
/// the truncation warning, the duration row.
fn bash_result_rows(
    result: &ToolResultView,
    expanded: bool,
    theme: &Theme,
    content_width: usize,
    rows: &mut RowOutput,
) {
    let tool_output = theme.fg_style(ThemeColor::ToolOutput);
    let dim = theme.fg_style(ThemeColor::Dim);
    let output = result.text_output(true);
    let output = output.trim();
    // The output sits behind a two-column gutter: `⎿ ` marks the first row, the rest align
    // under it, and the elided count takes `⋮` in the same column.
    let gutter = |first: bool, text: String, style| {
        let mark = if first { "\u{23bf} " } else { "  " };
        vec![
            Span::styled(mark.to_string(), dim),
            Span::styled(text, style),
        ]
    };
    let output_width = content_width.saturating_sub(2).max(1);
    if !output.is_empty() {
        rows.blank();
        if expanded {
            for (index, line) in output.split('\n').enumerate() {
                rows.push(|| gutter(index == 0, line.to_string(), tool_output));
            }
        } else {
            let total: usize = output
                .split('\n')
                .map(|line| wrapped_text_count(line, output_width).max(1))
                .sum();
            // Output past the preview shows its first and last rows around a `⋮ +N lines` mark.
            let windowed = total > BASH_PREVIEW_LINES;
            if rows.is_counting() {
                rows.add_count(if windowed {
                    BASH_EDGE_LINES * 2 + 1
                } else {
                    total
                });
            } else {
                let visual: Vec<String> = output
                    .split('\n')
                    .flat_map(|line| {
                        let wrapped = wrap_text(line, output_width);
                        let joined = wrapped
                            .iter()
                            .map(|row| row.iter().map(|span| span.content.as_str()).collect());
                        let joined: Vec<String> = joined.collect();
                        if joined.is_empty() {
                            vec![String::new()]
                        } else {
                            joined
                        }
                    })
                    .collect();
                for (index, row) in visual.iter().enumerate() {
                    if windowed && index == BASH_EDGE_LINES {
                        let hidden = total - BASH_EDGE_LINES * 2;
                        let mark = format!("\u{22ee} +{hidden} lines");
                        rows.push(|| vec![Span::styled(mark.clone(), dim)]);
                    }
                    if !windowed || index < BASH_EDGE_LINES || index >= total - BASH_EDGE_LINES {
                        rows.push(|| gutter(index == 0, row.clone(), tool_output));
                    }
                }
            }
        }
    }
    if let Some(warning) = truncation_warning(result) {
        rows.blank();
        rows.wrapped_text(&warning, theme.fg_style(ThemeColor::Warning), content_width);
    }
}

/// The truncation notice (`[Full output: ... . Truncated: ...]`, warning
/// color) when the engine cut the output or spilled it to a file.
fn truncation_warning(result: &ToolResultView) -> Option<String> {
    let details = result.details.as_object();
    let truncation = details.and_then(|d| d.get("truncation"));
    let full_output_path = details
        .and_then(|d| d.get("fullOutputPath"))
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    let truncated =
        truncation.is_some_and(|t| t.get("truncated").and_then(Value::as_bool).unwrap_or(false));
    if !truncated && full_output_path.is_none() {
        return None;
    }
    let mut warnings: Vec<String> = Vec::new();
    if let Some(path) = full_output_path {
        warnings.push(format!("Full output: {path}"));
    }
    if truncated {
        let truncation = truncation.expect("checked");
        let output_lines = truncation.get("outputLines").and_then(Value::as_u64);
        let total_lines = truncation.get("totalLines").and_then(Value::as_u64);
        let truncated_by_lines = truncation
            .get("truncatedBy")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "lines");
        match (output_lines, total_lines) {
            (Some(output), Some(total)) if truncated_by_lines => {
                warnings.push(format!("Truncated: showing {output} of {total} lines"));
            }
            (Some(output), Some(_)) => {
                let max_bytes = truncation
                    .get("maxBytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(super::DEFAULT_MAX_BYTES as u64)
                    as usize;
                warnings.push(format!(
                    "Truncated: {output} lines shown ({} limit)",
                    super::format_size(max_bytes)
                ));
            }
            _ => {}
        }
    }
    if warnings.is_empty() {
        return None;
    }
    Some(format!("[{}]", warnings.join(". ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorMode, Theme};
    use serde_json::json;

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    /// The instant `ms` milliseconds ago: the live timer's start.
    fn started_ms_ago(ms: u64) -> std::time::Instant {
        std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(ms))
            .expect("the clock is past the start")
    }

    fn done_card(command: &str, output: &str) -> ToolCallCard {
        ToolCallCard {
            id: "toolu_1".into(),
            name: "bash".into(),
            args: json!({ "command": command }),
            started: true,
            started_at: Some(std::time::Instant::now()),
            ended_at: Some(std::time::Instant::now()),
            result: Some(ToolResultView {
                content: vec![json!({ "type": "text", "text": output })],
                details: json!({}),
                is_error: false,
            }),
            result_partial: false,
            aborted: false,
        }
    }

    fn text_of(line: &Line) -> String {
        line.iter().map(|s| s.content.as_str()).collect()
    }

    #[test]
    fn collapsed_preview_truncates_from_the_end() {
        let output = (1..=12)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let card = done_card("seq 1 12", &output);
        let rows = render(&card, 0, Detail::Overview, &theme(), 120, true);
        let flat: Vec<String> = rows.iter().map(text_of).collect();
        assert!(
            flat.iter().any(|r| r.contains("bash \u{00b7} done")),
            "got: {flat:?}"
        );
        assert!(
            flat.iter().any(|r| r.contains("$ seq 1 12")),
            "got: {flat:?}"
        );
        // The window: the first and last two rows around the hidden count.
        let mark = flat
            .iter()
            .position(|r| r.contains("\u{22ee} +8 lines"))
            .expect("the mark renders");
        let window: Vec<&str> = flat[mark - 2..=mark + 2]
            .iter()
            .map(|r| r.trim_end())
            .collect();
        // The gutter column: `⎿` on the first row, `⋮` on the elided count, the rest aligned.
        assert_eq!(
            window,
            [
                "  \u{23bf} 1",
                "    2",
                "  \u{22ee} +8 lines",
                "    11",
                "    12"
            ],
            "got: {flat:?}"
        );
    }

    #[test]
    fn expanded_shows_all_output() {
        let output = (1..=12)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let card = done_card("seq 1 12", &output);
        let rows = render(&card, 0, Detail::All, &theme(), 120, true);
        let flat: Vec<String> = rows.iter().map(text_of).collect();
        assert!(
            flat.iter().any(|r| r.trim_end().ends_with('1')),
            "got: {flat:?}"
        );
        assert!(
            !flat.iter().any(|r| r.contains('\u{22ee}')),
            "got: {flat:?}"
        );
    }

    #[test]
    fn duration_row_rendered() {
        let card = done_card("echo hi", "hi");
        let rows = render(&card, 0, Detail::Overview, &theme(), 120, true);
        let flat: Vec<String> = rows.iter().map(text_of).collect();
        assert!(flat.iter().any(|r| r.contains("Took ")), "got: {flat:?}");
    }

    #[test]
    fn truncation_warning_rendered() {
        let mut card = done_card("cat big.txt", "line1\nline2");
        card.result = Some(ToolResultView {
            content: vec![json!({ "type": "text", "text": "line1\nline2" })],
            details: json!({
                "truncation": {
                    "truncated": true,
                    "truncatedBy": "lines",
                    "outputLines": 2,
                    "totalLines": 10,
                },
                "fullOutputPath": "/tmp/pi-bash-abc",
            }),
            is_error: false,
        });
        let rows = render(&card, 0, Detail::Overview, &theme(), 120, true);
        let flat: Vec<String> = rows.iter().map(text_of).collect();
        assert!(
            flat.iter()
                .any(|r| r
                    .contains("[Full output: /tmp/pi-bash-abc. Truncated: showing 2 of 10 lines]")),
            "got: {flat:?}"
        );
    }

    #[test]
    fn byte_truncation_warning_shape() {
        let mut card = done_card("cat big.txt", "x");
        card.result = Some(ToolResultView {
            content: vec![json!({ "type": "text", "text": "x" })],
            details: json!({
                "truncation": {
                    "truncated": true,
                    "truncatedBy": "bytes",
                    "outputLines": 3,
                    "totalLines": 9,
                },
            }),
            is_error: false,
        });
        let rows = render(&card, 0, Detail::Overview, &theme(), 120, true);
        let flat: Vec<String> = rows.iter().map(text_of).collect();
        assert!(
            flat.iter()
                .any(|r| r.contains("Truncated: 3 lines shown (50.0KB limit)")),
            "got: {flat:?}"
        );
    }

    #[test]
    fn running_header_animates() {
        // The bash tool emits an empty partial result the moment it starts,
        // so a running card carries an (empty) partial result.
        let card = ToolCallCard {
            id: "t".into(),
            name: "bash".into(),
            args: json!({ "command": "sleep 1" }),
            started: true,
            started_at: Some(std::time::Instant::now()),
            result: Some(ToolResultView::default()),
            result_partial: true,
            ..Default::default()
        };
        let rows = render(&card, 3, Detail::Overview, &theme(), 120, true);
        let flat: Vec<String> = rows.iter().map(text_of).collect();
        assert!(flat.iter().any(|r| r.contains("running")), "got: {flat:?}");
        assert!(flat.iter().any(|r| r.contains("Elapsed ")), "got: {flat:?}");
    }

    #[test]
    fn running_card_renders_live_elapsed_before_any_result() {
        // The quiet window between `tool_execution_start` and the first
        // streamed frame: the card has a start and no result at all, and
        // the live timer is the only timing it shows. The value follows
        // the execution start (the 80ms repaints recompute it) — the
        // headless timer e2e asserts that tracking across frames, so
        // this test pins only the row's presence.
        let card = ToolCallCard {
            id: "t".into(),
            name: "bash".into(),
            args: json!({ "command": "sleep 1" }),
            started: true,
            started_at: Some(started_ms_ago(300)),
            ..Default::default()
        };
        let rows = render(&card, 3, Detail::Overview, &theme(), 120, true);
        let flat: Vec<String> = rows.iter().map(text_of).collect();
        assert!(flat.iter().any(|r| r.contains("Elapsed ")), "got: {flat:?}");
        assert!(!flat.iter().any(|r| r.contains("Took ")), "got: {flat:?}");
    }

    #[test]
    fn invalid_and_missing_command_shapes() {
        let card = ToolCallCard {
            id: "t".into(),
            name: "bash".into(),
            args: json!({ "command": 5 }),
            started: false,
            ..Default::default()
        };
        let rows = render(&card, 0, Detail::Overview, &theme(), 120, true);
        let flat: Vec<String> = rows.iter().map(text_of).collect();
        assert!(
            flat.iter().any(|r| r.contains("$ [invalid arg]")),
            "got: {flat:?}"
        );

        let card = ToolCallCard {
            id: "t".into(),
            name: "bash".into(),
            args: json!({ "command": "" }),
            started: false,
            ..Default::default()
        };
        let rows = render(&card, 0, Detail::Overview, &theme(), 120, true);
        let flat: Vec<String> = rows.iter().map(text_of).collect();
        assert!(flat.iter().any(|r| r.contains("$ ...")), "got: {flat:?}");
    }
}
