//! Tool-call card rendering: the card model mirrors the TS component
//! state; each tool renders through its own shell (ipython self-renders;
//! bash and the generic fallback use `ToolPanel`).

pub mod bash;
pub mod generic;
pub mod highlight;
pub mod ipython;
pub mod ipython_details;
mod layout;

use std::time::Instant;

use serde_json::Value;

use crate::chat::Detail;
use crate::theme::{Theme, ThemeColor};
use crate::{Line, Span};

/// One tool call and its execution state, minus the renderer caches.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolCallCard {
    pub id: String,
    pub name: String,
    pub args: Value,
    /// `tool_execution_start` seen (replayed cards infer it from the
    /// result).
    pub started: bool,
    /// When the execution started, when seen live (drives the header duration).
    pub started_at: Option<Instant>,
    /// When the final result landed (replay sets start and end together).
    pub ended_at: Option<Instant>,
    /// The result so far; `None` until the first result frame.
    pub result: Option<ToolResultView>,
    /// `result` is a partial streaming frame.
    pub result_partial: bool,
    /// A failed final frame settled this still-pending card with the
    /// run's error text; late result frames are dropped.
    pub aborted: bool,
}

/// One (partial or final) tool result.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolResultView {
    pub content: Vec<Value>,
    pub details: Value,
    pub is_error: bool,
}

impl ToolResultView {
    /// The joined text of the result's text blocks, ANSI stripped, with
    /// hidden image blocks appended as `[Image: ...]` fallback text.
    pub fn text_output(&self, show_images: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        for block in &self.content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => parts.push(
                    crate::error_summary::normalize_error_details(
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
                    .replace('\r', ""),
                ),
                Some("image") if !show_images => parts.push(hidden_image_text(block)),
                _ => {}
            }
        }
        parts.join("\n")
    }
}

/// The `[Image: ...]` text standing in for one hidden image block (TS
/// `imageFallback` with `includeImageDimensions: false`); the payload
/// is never decoded: the text is the mime alone.
fn hidden_image_text(block: &Value) -> String {
    let mime = block
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("image/unknown");
    crate::terminal_image::image_fallback(mime, None, None)
}

/// The image block's rendered size segment: the elided payload's byte
/// count when the transcript load replaced the data, the payload's own
/// length otherwise.
pub(crate) fn image_block_size_text(block: &Value) -> String {
    format_size(image_block_bytes(block))
}

/// The payload's size in bytes, from the elision marker when present
/// (the transcript load's `elidedBytes`), else the payload's own length.
pub(crate) fn image_block_bytes(block: &Value) -> usize {
    block
        .get("elidedBytes")
        .and_then(Value::as_u64)
        .map_or_else(
            || {
                block
                    .get("data")
                    .and_then(Value::as_str)
                    .map_or(0, str::len)
            },
            |bytes| bytes as usize,
        )
}

/// The animated working icon glyph.
#[must_use]
pub fn working_icon(frame: usize) -> &'static str {
    crate::chat::working_icon_frame(frame)
}

/// Render one tool-call card through its tool shell. `detail` is the card's own level (a click
/// flips it); `level` is the conversation level, which the python card's edit diffs and sent
/// agent messages follow. `show_images` (the `terminal.showImages` setting) renders metadata
/// rows or `[Image: ...]` placeholders.
#[must_use]
pub fn render_tool_card(
    card: &ToolCallCard,
    frame: usize,
    detail: Detail,
    level: Detail,
    theme: &Theme,
    width: usize,
    show_images: bool,
) -> Vec<Line> {
    let mut rows = match card.name.as_str() {
        name if pa_types::ai::is_kernel_tool_name(name) => {
            ipython::render(card, frame, detail, level, theme, width, show_images)
        }
        "bash" => bash::render(card, frame, detail, theme, width, show_images),
        _ => generic::render(card, frame, detail, theme, width, show_images),
    };
    // Cards show model-written arguments and file contents and add no links of their own.
    rows.iter_mut().for_each(crate::hyperlinks::strip_osc8);
    rows
}

/// The generic panel status: an interrupt settles first, then error wins
/// even while streaming; `running` animates until then.
pub(crate) enum PanelStatus {
    Queued,
    Running,
    Done,
    Error,
    /// The run ended (Esc, turn boundary) before the call returned.
    Interrupted,
}

pub(crate) fn panel_status(card: &ToolCallCard) -> PanelStatus {
    if card.aborted {
        return PanelStatus::Interrupted;
    }
    if let Some(result) = &card.result {
        if !card.result_partial {
            return if result.is_error {
                PanelStatus::Error
            } else {
                PanelStatus::Done
            };
        }
        if result.is_error {
            return PanelStatus::Error;
        }
    }
    if card.started {
        PanelStatus::Running
    } else {
        PanelStatus::Queued
    }
}

/// The panel header row: `name · status · 3.4s`. The tool name carries the card state by
/// colour (accent while running, muted once done or queued, error on failure or interrupt), and
/// the call's duration rides the same row: while the call runs it is recomputed on every paint
/// (the loader spinner's 80ms phase repaints tick it, #3296's wake, so it adds no wake of its
/// own), and once the final result lands it is the static run time. A card that never saw its
/// execution start, or a replayed card whose timing collapsed to one rebuild instant, shows no
/// duration.
pub(crate) fn panel_header(card: &ToolCallCard, frame: usize, theme: &Theme) -> Line {
    use crate::theme::ThemeColor::{Accent, BashMode, Dim, Error, Muted, Success};
    let dim = theme.fg_style(Dim);
    let error = theme.fg_style(Error);
    let (name_color, status) = match panel_status(card) {
        PanelStatus::Error => (Error, Span::styled("error".to_string(), error)),
        PanelStatus::Interrupted => (Error, Span::styled("interrupted".to_string(), error)),
        PanelStatus::Done => (
            Muted,
            Span::styled("done".to_string(), theme.fg_style(Success)),
        ),
        PanelStatus::Running => (
            Accent,
            Span::styled(
                format!("{} running", working_icon(frame)),
                theme.fg_style(BashMode),
            ),
        ),
        PanelStatus::Queued => (
            Muted,
            Span::styled("queued".to_string(), theme.fg_style(Muted)),
        ),
    };
    let separator = || Span::styled(" \u{00b7} ".to_string(), dim);
    let mut header: Line = vec![
        Span::styled(card.name.clone(), theme.fg_style(name_color)),
        separator(),
        status,
    ];
    let elapsed = card.started_at.and_then(|started| match card.ended_at {
        Some(ended) if ended == started => None,
        Some(ended) => Some(ended - started),
        None => Some(started.elapsed()),
    });
    if let Some(elapsed) = elapsed {
        header.push(separator());
        header.push(Span::styled(format_bash_duration(elapsed.as_millis()), dim));
    }
    header
}

/// One tool-panel row: content indented by 2, padded to the full width
/// on the panel background.
pub(crate) fn panel_line(content: Line, bg: ratatui::style::Style, width: usize) -> Line {
    let padding = 2usize;
    let content_width = layout::panel_content_width(width);
    let mut line: Line = vec![Span::styled(" ".repeat(padding), bg)];
    let used: usize = content
        .iter()
        .map(|s| crate::width::str_width(&s.content))
        .sum();
    for span in content {
        line.push(Span::styled(span.content, bg.patch(span.style)));
    }
    if used > content_width {
        return crate::width::truncate_line(&line, width, "");
    }
    line.push(Span::styled(" ".repeat(content_width - used), bg));
    line.push(Span::styled(" ".repeat(padding), bg));
    line
}

/// The bash panel duration: tenths of a second.
pub(crate) fn format_bash_duration(ms: u128) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// The bash tool's output byte budget, used when the spill carries no
/// `maxBytes`.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// `formatSize`: `512B`, `50.0KB`, `1.2MB`.
#[must_use]
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Image result blocks render below the card: the metadata row, then the image itself inline
/// (its half-block cells, with the terminal's pixel protocol drawn over them when it has one;
/// see [`crate::inline_image`]) sized to `max_cols`. Blocks without data or a mime type, and
/// every image while `show_images` is false, render nothing here; an elided payload (an
/// attach snapshot's marker) or an unreadable header renders the metadata row alone. The rows
/// are built from the block's metadata only (a bounded prefix read supplies the dimensions);
/// the decode runs off the paint path.
pub(crate) fn image_rows(
    result: Option<&ToolResultView>,
    show_images: bool,
    theme: &Theme,
    max_cols: usize,
) -> Vec<Line> {
    let fallback = theme.fg_style(ThemeColor::ToolOutput);
    let mut rows: Vec<Line> = Vec::new();
    for block in eligible_image_blocks(result, show_images) {
        rows.push(vec![Span::styled(
            format!("    \u{2570}\u{2500} {}", image_block_row_text(block)),
            fallback,
        )]);
        if let (Some(data), Some(mime)) = (
            block.get("data").and_then(Value::as_str),
            block.get("mimeType").and_then(Value::as_str),
        ) {
            rows.extend(
                crate::inline_image::image_rows(
                    data,
                    mime,
                    max_cols.saturating_sub(IMAGE_INDENT.len()),
                    ratatui::style::Style::default(),
                )
                .into_iter()
                .map(|row| {
                    let mut line = vec![Span::raw(IMAGE_INDENT)];
                    line.extend(row);
                    line
                }),
            );
        }
    }
    rows
}

/// The inline image's indent under its metadata row.
const IMAGE_INDENT: &str = "    ";

/// The metadata-row text for one shown image block: `[mime · WxH]`, or
/// `[mime · 140.1KB omitted]` when the dimensions are not available.
pub(crate) fn image_block_row_text(block: &Value) -> String {
    let mime = block
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("image/unknown");
    let dimensions = image_block_dimensions(block);
    match dimensions {
        Some(dimensions) => format!(
            "[{mime} \u{b7} {}\u{d7}{}]",
            dimensions.width_px, dimensions.height_px
        ),
        None => format!("[{mime} \u{b7} {} omitted]", image_block_size_text(block)),
    }
}

/// The image block's pixel dimensions, without a payload decode: the
/// elision marker's `widthPx`/`heightPx`, else the bounded-prefix read.
pub(crate) fn image_block_dimensions(
    block: &Value,
) -> Option<crate::terminal_image::ImageDimensions> {
    if let (Some(width), Some(height)) = (
        block.get("widthPx").and_then(Value::as_u64),
        block.get("heightPx").and_then(Value::as_u64),
    ) {
        return Some(crate::terminal_image::ImageDimensions {
            width_px: width as u32,
            height_px: height as u32,
        });
    }
    match (
        block.get("data").and_then(Value::as_str),
        block.get("mimeType").and_then(Value::as_str),
    ) {
        (Some(data), Some(mime)) => crate::terminal_image::get_image_dimensions_prefix(
            data,
            mime,
            crate::terminal_image::IMAGE_DIMENSIONS_PREFIX_BYTES,
        ),
        _ => None,
    }
}

fn eligible_image_blocks(
    result: Option<&ToolResultView>,
    show_images: bool,
) -> impl Iterator<Item = &Value> {
    result
        .into_iter()
        .flat_map(|result| result.content.iter())
        .filter(move |block| {
            // The paint and the geometry count share this eligibility: a `data: null` block
            // renders no row on either path, so cached card heights cannot diverge.
            show_images
                && block.get("type").and_then(Value::as_str) == Some("image")
                && block.get("data").and_then(Value::as_str).is_some()
                && block.get("mimeType").and_then(Value::as_str).is_some()
        })
}

/// The row count [`image_rows`] paints (the count-only geometry pass).
fn image_rows_count(result: Option<&ToolResultView>, show_images: bool, max_cols: usize) -> usize {
    eligible_image_blocks(result, show_images)
        .map(|block| {
            let inline = match (
                block.get("data").and_then(Value::as_str),
                block.get("mimeType").and_then(Value::as_str),
            ) {
                (Some(data), Some(mime)) => crate::inline_image::image_row_count(
                    data,
                    mime,
                    max_cols.saturating_sub(IMAGE_INDENT.len()),
                ),
                _ => 0,
            };
            1 + inline
        })
        .sum()
}

/// Exact row geometry for every tool shell without painting output rows.
pub(crate) fn count_tool_card(
    card: &ToolCallCard,
    frame: usize,
    detail: Detail,
    level: Detail,
    theme: &Theme,
    width: usize,
    show_images: bool,
) -> usize {
    match card.name.as_str() {
        name if pa_types::ai::is_kernel_tool_name(name) => {
            ipython::count(card, frame, detail, level, theme, width, show_images)
        }
        "bash" => bash::count(card, frame, detail, theme, width, show_images),
        _ => generic::count(card, frame, detail, theme, width, show_images),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> crate::theme::Theme {
        use crate::theme::{ColorMode, Theme};
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    fn text_of(line: &Line) -> String {
        line.iter().map(|s| s.content.as_str()).collect()
    }

    fn tiny_png(width: u32, height: u32) -> String {
        use base64::Engine;
        let mut bytes = vec![0x89, b'P', b'N', b'G'];
        bytes.extend(vec![0u8; 12]); // length + IHDR tag
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn image_result(data: &str, mime: &str) -> Option<ToolResultView> {
        Some(ToolResultView {
            content: vec![
                serde_json::json!({ "type": "text", "text": "done" }),
                serde_json::json!({ "type": "image", "data": data, "mimeType": mime }),
            ],
            details: serde_json::Value::Null,
            is_error: false,
        })
    }

    #[test]
    fn shown_image_rows_render_metadata_without_materializing_the_payload() {
        // The tail past the bounded prefix is invalid base64: the row
        // still renders dimensions, proving the payload was never decoded
        // in full.
        let payload = format!(
            "{}{}{}",
            tiny_png(64, 32),
            "A".repeat(crate::terminal_image::IMAGE_DIMENSIONS_PREFIX_BYTES * 2),
            "!".repeat(64)
        );
        let result = image_result(&payload, "image/png");
        let rows = image_rows(result.as_ref(), true, &theme(), 100);
        let flat: Vec<String> = rows.iter().map(text_of).collect();
        // The metadata row, then the image's reserved 8 x 2 cells (64 px wide, 32 tall).
        assert_eq!(
            flat[0],
            "    \u{2570}\u{2500} [image/png \u{b7} 64\u{d7}32]".to_string()
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(image_rows_count(result.as_ref(), true, 100), rows.len());
        assert!(flat.iter().all(|row| !row.contains("AAAA")));
    }

    #[test]
    fn shown_image_rows_render_the_size_placeholder_when_the_header_does_not_parse() {
        let payload = "x".repeat(186_328);
        let result = image_result(&payload, "image/jpeg");
        let rows = image_rows(result.as_ref(), true, &theme(), 100);
        let flat: Vec<String> = rows.iter().map(text_of).collect();
        assert_eq!(
            flat,
            vec!["    \u{2570}\u{2500} [image/jpeg \u{b7} 182.0KB omitted]".to_string()]
        );
    }

    #[test]
    fn elided_payloads_render_the_size_placeholder_from_the_marker() {
        let result = Some(ToolResultView {
            content: vec![serde_json::json!({
                "type": "image",
                "data": "",
                "mimeType": "image/jpeg",
                "elidedBytes": 186_328
            })],
            details: serde_json::Value::Null,
            is_error: false,
        });
        let rows = image_rows(result.as_ref(), true, &theme(), 100);
        let flat: Vec<String> = rows.iter().map(text_of).collect();
        assert_eq!(
            flat,
            vec!["    \u{2570}\u{2500} [image/jpeg \u{b7} 182.0KB omitted]".to_string()]
        );
    }

    #[test]
    fn hidden_image_text_never_parses_the_payload_or_the_marker() {
        let view = ToolResultView {
            content: vec![serde_json::json!({
                "type": "image",
                "data": tiny_png(64, 32),
                "mimeType": "image/png"
            })],
            ..Default::default()
        };
        assert_eq!(view.text_output(false), "[Image: [image/png]]");

        let elided = ToolResultView {
            content: vec![serde_json::json!({
                "type": "image",
                "data": "",
                "mimeType": "image/jpeg",
                "elidedBytes": 186_328
            })],
            ..Default::default()
        };
        assert_eq!(elided.text_output(false), "[Image: [image/jpeg]]");
    }

    #[test]
    fn non_string_data_blocks_render_no_rows_on_either_path() {
        let result = Some(ToolResultView {
            content: vec![serde_json::json!({
                "type": "image",
                "data": null,
                "mimeType": "image/png"
            })],
            details: serde_json::Value::Null,
            is_error: false,
        });
        assert!(image_rows(result.as_ref(), true, &theme(), 100).is_empty());
        assert_eq!(image_rows_count(result.as_ref(), true, 100), 0);
        let elided = Some(ToolResultView {
            content: vec![serde_json::json!({
                "type": "image",
                "data": "",
                "mimeType": "image/png",
                "elidedBytes": 500 * 1024
            })],
            details: serde_json::Value::Null,
            is_error: false,
        });
        assert_eq!(image_rows(elided.as_ref(), true, &theme(), 100).len(), 1);
        assert_eq!(image_rows_count(elided.as_ref(), true, 100), 1);
    }

    #[test]
    fn the_panel_title_carries_the_card_state_by_colour() {
        use crate::theme::ThemeColor::{Accent, Error, Muted};
        let theme = theme();
        let running = ToolCallCard {
            name: "bash".into(),
            started: true,
            ..Default::default()
        };
        let done = ToolCallCard {
            result: Some(ToolResultView::default()),
            ..running.clone()
        };
        let failed = ToolCallCard {
            result: Some(ToolResultView {
                is_error: true,
                ..Default::default()
            }),
            ..running.clone()
        };
        let interrupted = ToolCallCard {
            aborted: true,
            ..failed.clone()
        };
        let headers: Vec<(String, Option<ratatui::style::Color>)> =
            [&running, &done, &failed, &interrupted]
                .into_iter()
                .map(|card| {
                    let header = panel_header(card, 0, &theme);
                    (text_of(&header), header[0].style.fg)
                })
                .collect();
        let fg = |color| theme.fg_style(color).fg;
        assert_eq!(
            headers,
            vec![
                ("bash \u{00b7} \u{25c7} running".to_string(), fg(Accent)),
                ("bash \u{00b7} done".to_string(), fg(Muted)),
                ("bash \u{00b7} error".to_string(), fg(Error)),
                ("bash \u{00b7} interrupted".to_string(), fg(Error)),
            ]
        );
    }

    #[test]
    fn panel_status_settles_on_error_even_while_partial() {
        let mut card = ToolCallCard {
            name: "bash".into(),
            started: true,
            ..Default::default()
        };
        card.result = Some(ToolResultView {
            is_error: true,
            ..Default::default()
        });
        card.result_partial = true;
        assert!(matches!(panel_status(&card), PanelStatus::Error));
        card.result_partial = false;
        assert!(matches!(panel_status(&card), PanelStatus::Error));
        card.result = Some(ToolResultView::default());
        assert!(matches!(panel_status(&card), PanelStatus::Done));
        card.result = None;
        assert!(matches!(panel_status(&card), PanelStatus::Running));
        card.started = false;
        assert!(matches!(panel_status(&card), PanelStatus::Queued));
    }
}
