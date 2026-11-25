//! The `ipython` tool-call card: a fixed collapsed summary line (marker,
//! language, preview, line counts, duration, error name), per-file edit
//! summaries (with diff rows when edit diffs expand), plus in expanded
//! mode the full cell source and output. The top line never changes with
//! expansion, so toggling detail never shifts the layout.

pub(crate) mod diffs;
mod output;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use diffs::render_diffs;
use output::render_output;
use serde_json::Value;

use super::ipython_details::{
    format_duration, is_agent_message_receipt, is_edit_confirmation, parse_sent_agent_message,
    read_background_shell, transcript_reads_summary, BackgroundShell, BashCommands, IpythonDetails,
    IpythonError,
};
use super::layout::RowOutput;
use super::scheme_value::{BashOutcome, PrintedHash};
use super::{highlight, ToolCallCard};
use crate::chat::Detail;
use crate::code_preview::{
    parse_ipython_bash_cell, preview_bash_command, preview_ipython_code, python_statement_lines,
    scheme_outline, CodePreviewLanguage, SchemeOutline,
};
use crate::custom_message::AgentMessageDirection;
use crate::error_summary::{normalize_error_details, summarize_error_details};
use crate::theme::{Theme, ThemeColor};
use crate::width::{str_width, truncate_line, wrap_line};
use crate::{Line, Span};

/// The output continuation indent (TS `OUTPUT_INDENT`).
const OUTPUT_INDENT: &str = "   ";

/// The card status (TS `statusKind`).
enum CardStatus {
    Queued,
    Running,
    Done,
    Error,
    Aborted,
    /// The cell ended and what it started goes on: the cell itself, moved
    /// to the background past its yield window, or the command it left
    /// running.
    Background,
}

impl CardStatus {
    fn of(
        card: &ToolCallCard,
        details: &IpythonDetails,
        command: Option<&BashOutcome>,
    ) -> CardStatus {
        // An interrupted cell never renders as a success: a settled
        // straggler's kept partial can still carry a background shell
        // whose exit code reads Done, and the aborted stamp is the
        // truth the exit code never saw.
        if card.aborted {
            return CardStatus::Aborted;
        }
        let result = card.result.as_ref();
        let is_error = result.is_some_and(|r| r.is_error);
        if !card.result_partial {
            if let Some(background) =
                result.and_then(|r| read_background_shell(cell_code(card), &r.details))
            {
                return match background.exit_code {
                    Some(0) => CardStatus::Done,
                    Some(_) => CardStatus::Error,
                    None => CardStatus::Running,
                };
            }
        }
        let status = details.status.as_deref();
        if is_error || status == Some("error") {
            return CardStatus::Error;
        }
        if let (false, Some(command)) = (card.result_partial, command) {
            if command.running {
                return CardStatus::Background;
            }
            if command.timed_out || command.exit_code.is_some_and(|code| code != 0) {
                return CardStatus::Error;
            }
        }
        if !card.result_partial && status == Some("backgrounded") {
            return CardStatus::Background;
        }
        if status == Some("aborted") {
            return CardStatus::Aborted;
        }
        let has_result = result.is_some_and(|r| {
            details.stdout.is_some()
                || details.stderr.is_some()
                || details.result.is_some()
                || details.error.is_some()
                || !details.diffs.is_empty()
                || !details.sent_agent_messages.is_empty()
                || !r.content.is_empty()
        });
        if !card.result_partial && (status.is_some() || card.started || has_result) {
            return CardStatus::Done;
        }
        if card.result_partial || card.started {
            CardStatus::Running
        } else {
            CardStatus::Queued
        }
    }
}

/// Whether the cell's final result carries a still-running background
/// shell: the summary line keeps animating — the card's rows must not
/// cache.
pub(crate) fn background_shell_running(card: &ToolCallCard) -> bool {
    if card.result_partial {
        return false;
    }
    card.result
        .as_ref()
        .and_then(|result| read_background_shell(cell_code(card), &result.details))
        .is_some_and(|background| background.exit_code.is_none())
}

fn cell_code(card: &ToolCallCard) -> &str {
    card.args
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Render the ipython cell card: the fixed summary line, the edit
/// summaries, then the expanded code and output rows when the card's
/// `detail` is `all`. Edit diff rows and the sent agent messages' bodies
/// follow the conversation `level`, never a card's own click toggle.
#[must_use]
pub fn render(
    card: &ToolCallCard,
    frame: usize,
    detail: Detail,
    level: Detail,
    theme: &Theme,
    width: usize,
    show_images: bool,
) -> Vec<Line> {
    let mut lines = RowOutput::paint();
    layout(
        card,
        frame,
        detail,
        level,
        theme,
        width,
        show_images,
        &mut lines,
    );
    // No row is wider than the terminal, whatever the width and the text:
    // a row that still is (a wide character in a one-column terminal, a
    // fixed gutter in a narrower one) is cut to fit, by whole characters.
    let mut rows = lines.into_lines();
    for row in &mut rows {
        if crate::width::line_width(row) > width {
            *row = crate::width::slice_line_by_column_strict(row, 0, width, /*strict*/ true);
        }
    }
    rows
}

pub(super) fn count(
    card: &ToolCallCard,
    frame: usize,
    detail: Detail,
    level: Detail,
    theme: &Theme,
    width: usize,
    show_images: bool,
) -> usize {
    let mut lines = RowOutput::count();
    layout(
        card,
        frame,
        detail,
        level,
        theme,
        width,
        show_images,
        &mut lines,
    );
    lines.len()
}

#[expect(
    clippy::too_many_arguments,
    reason = "the paint/count sink rides beside the render inputs so both traversals share one layout"
)]
fn layout(
    card: &ToolCallCard,
    frame: usize,
    detail: Detail,
    level: Detail,
    theme: &Theme,
    width: usize,
    show_images: bool,
    lines: &mut RowOutput,
) {
    let code = cell_code(card).trim_end();
    let details = card.result.as_ref().map_or_else(
        || IpythonDetails::parse(&Value::Null),
        |result| IpythonDetails::parse(&result.details),
    );
    let background = card
        .result
        .as_ref()
        .and_then(|result| read_background_shell(code, &result.details));

    lines.push(|| {
        collapsed_line(
            card,
            &details,
            background.as_ref(),
            frame,
            theme,
            width,
            code,
        )
    });
    // Edit summaries, sent-message receipts, and transcript reads show even
    // when the cell is collapsed; code and output open up only when
    // expanded. The sent messages are agent messages of their own, so their
    // bodies follow the conversation level, not a click on the card.
    let expanded = detail.tool_output_expanded();
    let has_code = expanded
        && render_code(
            lines,
            code,
            pa_types::ai::kernel_tool_language(&card.name),
            theme,
            width,
        );
    render_diffs(
        lines,
        &details.diffs,
        has_code,
        level.edit_diffs_expanded(),
        theme,
        width,
    );
    render_sent_agent_messages(lines, &details, level.tool_output_expanded(), theme, width);
    render_transcript_reads(lines, &details, expanded, theme, width);
    render_todo_list(lines, &details, theme, width);
    render_questions(lines, &details, theme, width);
    if expanded {
        render_output(card, &details, lines, has_code, show_images, theme, width);
    }
    // Image blocks (an `attach_image` cell's pictures) render below the card when shown, the
    // collapsed card included: the picture is the cell's visible result (TS adds these rows for
    // every tool shell).
    lines.images(card.result.as_ref(), show_images, theme, width);
}

/// A Scheme cell's source as its card reads it: the outline its title is
/// made from and, once the card has been expanded, its highlighted rows with
/// the palette that coloured them.
struct SchemeSource {
    outline: SchemeOutline,
    rows: RefCell<Option<(highlight::SyntaxPalette, Rc<Vec<Line>>)>>,
}

/// How many cells' sources stay read.
const SCHEME_SOURCES_KEPT: usize = 8;

thread_local! {
    /// The sources read last, the newest at the back. A running card
    /// repaints on every frame and a cell's source does not change under its
    /// card, so a source is lexed when it is first seen, not once a frame.
    static SCHEME_SOURCES: RefCell<VecDeque<(String, Rc<SchemeSource>)>> =
        const { RefCell::new(VecDeque::new()) };
}

/// The reading of the Scheme cell `code`, made on its first use.
fn scheme_source(code: &str) -> Rc<SchemeSource> {
    SCHEME_SOURCES.with_borrow_mut(|sources| {
        if let Some((_, source)) = sources.iter().find(|(read, _)| read == code) {
            return Rc::clone(source);
        }
        if sources.len() == SCHEME_SOURCES_KEPT {
            sources.pop_front();
        }
        let source = Rc::new(SchemeSource {
            outline: scheme_outline(code),
            rows: RefCell::new(None),
        });
        sources.push_back((code.to_string(), Rc::clone(&source)));
        source
    })
}

/// The widest a Scheme error's message runs on the summary line, and the
/// share of a narrower row it may take: the preview keeps the rest.
const ERROR_SUMMARY_WIDTH: usize = 48;

/// The fixed marker + summary line (TS `collapsedLine`).
fn collapsed_line(
    card: &ToolCallCard,
    details: &IpythonDetails,
    background: Option<&BackgroundShell>,
    frame: usize,
    theme: &Theme,
    width: usize,
    code: &str,
) -> Line {
    let muted = theme.fg_style(ThemeColor::Muted);
    let dim = theme.fg_style(ThemeColor::Dim);
    let error = theme.fg_style(ThemeColor::Error);
    let success = theme.fg_style(ThemeColor::Success);
    let bash_mode = theme.fg_style(ThemeColor::BashMode);
    let accent = theme.fg_style(ThemeColor::Accent);

    // A cell's own label and preview follow its kernel's language; a saved
    // session's Python cells keep theirs. A Scheme cell is titled from its
    // source alone: the kernel call it makes and that call's main argument,
    // the same while it waits, runs and settles.
    let source = match pa_types::ai::kernel_tool_language(&card.name) {
        Some(pa_types::ai::KernelLanguage::Scheme) => Some(scheme_source(code)),
        Some(pa_types::ai::KernelLanguage::Python) | None => None,
    };
    let outline = source.as_ref().map(|source| &source.outline);
    // (label, preview, further calls, titled as bash)
    let (language_label, preview, more, bash_titled): (&str, Line, usize, bool) =
        if let Some(outline) = outline {
            let preview = outline.preview();
            let mut part: Line = Vec::new();
            if !preview.head.is_empty() {
                part.push(Span::styled(
                    preview.head,
                    theme.fg_style(ThemeColor::SyntaxFunction),
                ));
            }
            if !preview.text.is_empty() {
                let gap = if part.is_empty() { "" } else { " " };
                part.push(Span::styled(format!("{gap}{}", preview.text), dim));
            }
            let label = if preview.bash { "bash" } else { "scheme" };
            (label, part, preview.more, preview.bash)
        } else {
            let preview = preview_ipython_code(code);
            let own_label = match (parse_ipython_bash_cell(code).is_some(), &preview.language) {
                (true, CodePreviewLanguage::Python) => "bash \u{00b7} python",
                (true | false, CodePreviewLanguage::Bash) => "bash",
                (false, CodePreviewLanguage::Python) => "python",
            };
            match dominant_bash(None, details, code) {
                Some(bash) => {
                    // A command that is not a literal previews as the cell's own first line.
                    let text = Some(preview_bash_command(&bash.first).text)
                        .filter(|text| !text.is_empty())
                        .unwrap_or(preview.text);
                    let more = bash.count.saturating_sub(1);
                    ("bash", vec![Span::styled(text, dim)], more, true)
                }
                None => (own_label, vec![Span::styled(preview.text, dim)], 0, false),
            }
        };
    // What a Scheme cell's printed value says of the command it ended on.
    let command = outline
        .and(details.result.as_deref())
        .and_then(PrintedHash::parse)
        .and_then(|hash| hash.bash_outcome())
        .filter(|_| !card.result_partial);
    let status = CardStatus::of(card, details, command.as_ref());
    let in_background = matches!(status, CardStatus::Background);

    // The language label is the card's title: it carries the state by colour (accent while
    // running, muted once settled or queued, error on failure or interrupt) beside the marker.
    let (marker, title): (Line, _) = match status {
        CardStatus::Error | CardStatus::Aborted => {
            (vec![Span::styled("\u{2717}".to_string(), error)], error)
        }
        CardStatus::Done => (vec![Span::styled("\u{2713}".to_string(), success)], muted),
        CardStatus::Running => (
            vec![Span::styled(
                super::working_icon(frame).to_string(),
                bash_mode,
            )],
            accent,
        ),
        CardStatus::Queued => (vec![Span::styled("\u{25c7}".to_string(), muted)], muted),
        CardStatus::Background => (vec![Span::styled("\u{21bb}".to_string(), bash_mode)], muted),
    };

    let mut parts: Vec<Line> = Vec::new();
    let mut preview_index: Option<usize> = None;
    let mut marker = marker;
    marker.push(Span::raw(" "));
    marker.push(Span::styled(language_label.to_string(), title));
    // A command run with the machine's highest rights says so, from its call
    // (while it waits for the user's consent too) or from its result.
    let elevated = bash_titled
        && (outline
            .and_then(SchemeOutline::dominant_bash)
            .is_some_and(|bash| bash.elevated)
            || details
                .bash_commands
                .as_ref()
                .is_some_and(|bash| bash.elevated));
    if elevated {
        marker.push(Span::raw(" "));
        marker.push(Span::styled(
            if cfg!(windows) { "admin" } else { "root" }.to_string(),
            theme.fg_style(ThemeColor::Warning),
        ));
    }
    parts.push(marker);
    if preview.iter().any(|span| !span.content.is_empty()) {
        preview_index = Some(parts.len());
        parts.push(preview);
    } else if !bash_titled && !card.started {
        parts.push(vec![Span::styled("waiting for code".to_string(), dim)]);
    }
    if more > 0 {
        parts.push(vec![Span::styled(format!("+{more} more"), dim)]);
    }
    if let Some(counts) = line_counts(card, details, code) {
        parts.push(vec![Span::styled(counts, dim)]);
    }
    let mut live_timer = false;
    if let Some(duration) = details.duration_ms {
        let label = if background.is_some() {
            format!("cell {}", format_duration(duration))
        } else {
            format_duration(duration)
        };
        parts.push(vec![Span::styled(label, dim)]);
    } else if let Some(started) = card
        .started_at
        .filter(|_| card.ended_at.is_none() || card.aborted)
    {
        let elapsed = card.ended_at.unwrap_or_else(std::time::Instant::now) - started;
        parts.push(vec![Span::styled(
            format_duration(elapsed.as_millis() as f64),
            dim,
        )]);
        live_timer = card.ended_at.is_none();
    }
    if !card.result_partial {
        // Every Scheme failure carries the one error name, so its card says
        // what failed: the first line of the message.
        let said = details
            .error
            .as_ref()
            .filter(|e| outline.is_some() && e.ename == "SchemeError")
            .and_then(|e| {
                let message = normalize_error_details(&e.evalue);
                let first = message.lines().map(str::trim).find(|l| !l.is_empty())?;
                Some(crate::width::truncate_to_width(
                    first.strip_prefix("Error: ").unwrap_or(first),
                    ERROR_SUMMARY_WIDTH.min(width * 2 / 5),
                    "\u{2026}",
                ))
            });
        let error_name = said
            .or_else(|| details.error.as_ref().map(|e| e.ename.clone()))
            .or_else(|| details.error_ename.clone());
        if let Some(ename) = error_name {
            parts.push(vec![Span::styled(ename, error)]);
        }
    }
    let exit_code = background
        .and_then(|shell| shell.exit_code)
        .or(command.as_ref().and_then(|command| command.exit_code));
    if let Some(exit_code) = exit_code.filter(|code| *code != 0) {
        parts.push(vec![Span::styled(format!("exit {exit_code}"), error)]);
    }
    if command.as_ref().is_some_and(|command| command.timed_out) {
        parts.push(vec![Span::styled("timed out".to_string(), error)]);
    }
    if in_background {
        let what = if command.is_some() {
            "command still running"
        } else {
            "in background"
        };
        parts.push(vec![Span::styled(what.to_string(), bash_mode)]);
    }

    // The live timer is only useful on screen, but the summary clips
    // from the right and the duration slot clips first: while the cell
    // runs, the code preview gives the ticking duration room by
    // shortening (or, if it cannot fit at all, dropping) itself. The
    // settled card keeps its established layout — `durationMs` lands
    // in the same slot, clipped exactly as before.
    // A Scheme cell's preview always gives way: its duration and what
    // became of it are the parts a settled card is read for.
    if live_timer || outline.is_some() {
        let separator_width = str_width(" \u{00b7} ");
        let used = parts
            .iter()
            .map(|p| crate::width::line_width(p))
            .sum::<usize>()
            + separator_width * parts.len().saturating_sub(1)
            + 1;
        if used > width {
            let excess = used - width;
            match preview_index {
                Some(index) if crate::width::line_width(&parts[index]) > excess => {
                    let part = &mut parts[index];
                    let target = crate::width::line_width(part) - excess;
                    if outline.is_some() {
                        *part = truncate_line(part, target, "\u{2026}");
                    } else if let Some(span) = part.first_mut() {
                        span.content = crate::width::truncate_to_width(&span.content, target, "");
                    }
                }
                Some(index) => {
                    parts.remove(index);
                }
                None => {}
            }
        }
    }

    let mut row: Line = vec![Span::raw(" ")];
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            row.push(Span::styled(" \u{00b7} ".to_string(), dim));
        }
        row.extend(part.iter().cloned());
    }
    truncate_line(&row, width, "")
}

/// A cell renders as bash when its executed `bash()` lines are at least
/// `1/BASH_DOMINANCE_DIVISOR` of the cell's non-blank lines. 2 means the bash
/// lines are at least as many as the remaining Python lines (B >= P - B):
/// literal commands also appear in the cell source, so P - B estimates
/// the non-bash Python.
const BASH_DOMINANCE_DIVISOR: usize = 2;

fn input_line_count(code: &str) -> usize {
    let body = parse_ipython_bash_cell(code).map_or_else(|| code.to_string(), |cell| cell.body);
    body.lines().filter(|line| !line.trim().is_empty()).count()
}

/// The `bash` commands a cell is titled by, when they make up most of it.
/// A Python cell's are the ones it ran, known once its result arrives. A
/// Scheme cell's are the `(bash …)` calls of its `outline`, read from its
/// source: its title is then the same while it waits, runs and settles,
/// where a title taken from the result changed from `scheme` to `bash` under
/// the reader.
fn dominant_bash(
    outline: Option<&SchemeOutline>,
    details: &IpythonDetails,
    code: &str,
) -> Option<BashCommands> {
    match outline {
        Some(outline) => outline.dominant_bash().map(|calls| BashCommands {
            first: calls.first.clone(),
            count: calls.count,
            lines: calls.lines,
            elevated: calls.elevated,
        }),
        None => details
            .bash_commands
            .clone()
            .filter(|bash| bash.lines * BASH_DOMINANCE_DIVISOR >= input_line_count(code)),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BashCellStats {
    pub(crate) bash_lines: usize,
    pub(crate) cell_lines: usize,
    pub(crate) count: usize,
}

/// The bash stats of a settled ipython card that renders as bash.
pub(crate) fn bash_dominated_stats(card: &ToolCallCard) -> Option<BashCellStats> {
    let code = cell_code(card).trim_end();
    let details = IpythonDetails::parse(&card.result.as_ref()?.details);
    let source = match pa_types::ai::kernel_tool_language(&card.name) {
        Some(pa_types::ai::KernelLanguage::Scheme) => Some(scheme_source(code)),
        Some(pa_types::ai::KernelLanguage::Python) | None => None,
    };
    let outline = source.as_ref().map(|source| &source.outline);
    let bash = dominant_bash(outline, &details, code)?;
    Some(BashCellStats {
        bash_lines: bash.lines,
        cell_lines: input_line_count(code),
        count: bash.count,
    })
}

/// `\u{2191}in \u{2193}out lines` (TS `lineCounts`): non-empty input
/// lines; output from the structured fields (edits show the diff, so
/// count zero).
fn line_counts(card: &ToolCallCard, details: &IpythonDetails, code: &str) -> Option<String> {
    let input = input_line_count(code);
    let has_diffs = !details.diffs.is_empty();

    let language = pa_types::ai::kernel_tool_language(&card.name);
    let result = details.shown_result(language);
    let structured = [
        details.stdout.as_deref(),
        details.stderr.as_deref(),
        result.as_deref(),
        details.background_output.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|text| !text.trim().is_empty())
    .collect::<Vec<_>>()
    .join("\n");
    let blocks_text = text_from_blocks(card);
    // The blocks repeat a value a row above already says: a Python receipt
    // by its repr, a Scheme value by the print the card left out.
    let said = is_agent_message_receipt(Some(blocks_text.as_str()), &details.sent_agent_messages)
        || (language == Some(pa_types::ai::KernelLanguage::Scheme) && details.result.is_some());
    let fallback = if said { String::new() } else { blocks_text };
    let output_text = if structured.trim().is_empty() {
        fallback
    } else {
        structured
    };
    let output = if has_diffs || output_text.trim().is_empty() {
        0
    } else {
        output_text.trim().lines().count()
    };

    let mut segments: Vec<String> = Vec::new();
    if input > 0 {
        segments.push(format!("\u{2191} {input}"));
    }
    if output > 0 {
        segments.push(format!("\u{2193} {output}"));
    }
    if segments.is_empty() {
        None
    } else {
        Some(format!("{} lines", segments.join(" ")))
    }
}

/// The text of the result's text blocks (TS `textFromBlocks`).
fn text_from_blocks(card: &ToolCallCard) -> String {
    card.result
        .as_ref()
        .map(|result| {
            result
                .content
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// `True` when the line is a magic `!command` line (TS `MAGIC_LINE_PATTERN`).
fn is_magic_line(statement: &str) -> bool {
    statement.trim_start().starts_with('!')
}

/// The expanded source rows (TS `renderCode`): the first line guttered
/// `\u{2570}\u{2500}`, continuation lines indented; bash cells and magic
/// lines render in bashMode, python lines syntax highlighted. A Scheme
/// cell has no magic lines: all of it is highlighted as Scheme.
fn render_code(
    lines: &mut RowOutput,
    code: &str,
    language: Option<pa_types::ai::KernelLanguage>,
    theme: &Theme,
    width: usize,
) -> bool {
    if code.is_empty() {
        add_wrapped(
            lines,
            &vec![Span::styled(
                "\u{2570}\u{2500} ".to_string(),
                theme.fg_style(ThemeColor::Dim),
            )],
            &vec![Span::styled(
                "waiting for code".to_string(),
                theme.fg_style(ThemeColor::Muted),
            )],
            width,
        );
        return false;
    }
    let scheme = language == Some(pa_types::ai::KernelLanguage::Scheme);
    let is_bash_cell = !scheme && parse_ipython_bash_cell(code).is_some();
    let raw_lines: Vec<&str> = code.split('\n').collect();
    let palette = highlight::SyntaxPalette::from_theme(theme);
    let highlighted = if is_bash_cell {
        Rc::new(Vec::new())
    } else if scheme {
        // Coloured once for this source and palette, then shared.
        let source = scheme_source(code);
        let mut rows = source.rows.borrow_mut();
        match rows.as_ref() {
            Some((coloured, rows)) if *coloured == palette => Rc::clone(rows),
            Some(_) | None => {
                let fresh = Rc::new(highlight::highlight_scheme(code, &palette));
                *rows = Some((palette, Rc::clone(&fresh)));
                fresh
            }
        }
    } else {
        Rc::new(highlight::highlight_python(code, &palette))
    };
    let statements = if scheme {
        Vec::new()
    } else {
        python_statement_lines(code)
    };
    for (index, raw_line) in raw_lines.iter().enumerate() {
        let prefix: Line = if index == 0 {
            vec![Span::styled(
                "\u{2570}\u{2500} ".to_string(),
                theme.fg_style(ThemeColor::Dim),
            )]
        } else {
            vec![Span::raw(OUTPUT_INDENT)]
        };
        let statement = statements.get(index).cloned().unwrap_or_default();
        let magic = is_magic_line(&statement) || parse_ipython_bash_cell(&statement).is_some();
        let body: Line = if is_bash_cell || magic {
            vec![Span::styled(
                (*raw_line).to_string(),
                theme.fg_style(ThemeColor::BashMode),
            )]
        } else {
            highlighted.get(index).cloned().unwrap_or_else(|| {
                vec![Span::styled(
                    (*raw_line).to_string(),
                    theme.fg_style(ThemeColor::MdCodeBlock),
                )]
            })
        };
        let body = if body.is_empty() {
            vec![Span::raw(" ")]
        } else {
            body
        };
        add_wrapped(lines, &prefix, &body, width);
    }
    true
}

/// TS `renderSentAgentMessages`: one summary row per sent receipt, the
/// `╰─`-guttered body only in the expanded view (the TS sent rows are
/// the receipt summary alone).
fn render_sent_agent_messages(
    lines: &mut RowOutput,
    details: &IpythonDetails,
    expanded: bool,
    theme: &Theme,
    width: usize,
) {
    for sent in &details.sent_agent_messages {
        let Some(sent) = parse_sent_agent_message(sent) else {
            continue;
        };
        if expanded {
            lines.blank();
        }
        let direction = if sent.delivered {
            AgentMessageDirection::Sent
        } else {
            AgentMessageDirection::Queued
        };
        // TS: truncateToWidth(summary, max(1, width - 1), "…") then the
        // one-space `addPlain` margin.
        lines.push(|| {
            let summary = crate::custom_message::render::agent_message_summary_line(
                direction,
                &sent.counterpart,
                theme,
            );
            let mut row: Line = vec![Span::raw(" ")];
            row.extend(truncate_line(
                &summary,
                width.saturating_sub(1).max(1),
                "\u{2026}",
            ));
            row
        });
        if expanded {
            if lines.is_counting() {
                lines.add_count(crate::custom_message::agent_message_body_count(
                    &sent.message,
                    width,
                ));
            } else {
                for row in
                    crate::custom_message::render::agent_message_body(&sent.message, theme, width)
                {
                    lines.push(|| row);
                }
            }
        }
    }
}

/// One summary row for the cell's `rlm.session.messages` reads, shaped like
/// the sent agent-message rows; the expanded view sets it off from the code.
fn render_transcript_reads(
    lines: &mut RowOutput,
    details: &IpythonDetails,
    expanded: bool,
    theme: &Theme,
    width: usize,
) {
    if let Some(summary) = transcript_reads_summary(&details.transcript_reads) {
        if expanded {
            lines.blank();
        }
        lines.push(|| {
            let row: Line = vec![
                Span::raw(" "),
                Span::styled(
                    "\u{2261} transcript".to_string(),
                    theme.fg_style(ThemeColor::Muted),
                ),
                Span::styled(" \u{b7} ".to_string(), theme.fg_style(ThemeColor::Dim)),
                Span::styled(summary, theme.fg_style(ThemeColor::Dim)),
            ];
            truncate_line(&row, width, "\u{2026}")
        });
    }
}

/// One row for the todo list the cell's `todo.*` calls left, shown collapsed too: the
/// transcript's record of each plan change (the live list is the dock's tray).
fn render_todo_list(lines: &mut RowOutput, details: &IpythonDetails, theme: &Theme, width: usize) {
    let Some(summary) = details
        .todo_list
        .as_ref()
        .and_then(super::ipython_details::todo_list_summary)
    else {
        return;
    };
    lines.push(|| {
        let row: Line = vec![
            Span::raw(" "),
            Span::styled(
                "\u{2630} todos".to_string(),
                theme.fg_style(ThemeColor::Muted),
            ),
            Span::styled(" \u{b7} ".to_string(), theme.fg_style(ThemeColor::Dim)),
            Span::styled(summary, theme.fg_style(ThemeColor::Dim)),
        ];
        truncate_line(&row, width, "\u{2026}")
    });
}

/// One row per settled `ask(...)`, shown collapsed too: the transcript's record of the
/// question and how the user answered (the live panel is the composer's).
fn render_questions(lines: &mut RowOutput, details: &IpythonDetails, theme: &Theme, width: usize) {
    for asked in &details.questions {
        let answer = asked.answer.join(", ");
        let (outcome, color) = match asked.status.as_str() {
            "answered" => (answer, ThemeColor::Success),
            "timed_out" if !answer.is_empty() => {
                (format!("timed out \u{b7} {answer}"), ThemeColor::Warning)
            }
            "timed_out" => ("timed out".to_string(), ThemeColor::Warning),
            "no_user" if !answer.is_empty() => {
                (format!("no one to ask \u{b7} {answer}"), ThemeColor::Dim)
            }
            "no_user" => ("no one to ask".to_string(), ThemeColor::Dim),
            other => (other.to_string(), ThemeColor::Warning),
        };
        lines.push(|| {
            // The answer stays whole; the question gives way when the row runs out.
            let tail = format!(" \u{2192} {outcome}");
            let room = width.saturating_sub(3 + str_width(&tail));
            let text = if room >= 12 {
                crate::width::truncate_to_width(&asked.text, room, "\u{2026}")
            } else {
                asked.text.clone()
            };
            let row: Line = vec![
                Span::raw(" "),
                Span::styled("? ".to_string(), theme.fg_style(ThemeColor::Accent)),
                Span::styled(text, theme.fg_style(ThemeColor::Text)),
                Span::styled(" \u{2192} ".to_string(), theme.fg_style(ThemeColor::Dim)),
                Span::styled(outcome, theme.fg_style(color)),
            ];
            truncate_line(&row, width, "\u{2026}")
        });
    }
}

/// One indented card row (TS `addWrapped`): the first wrapped row carries
/// `prefix`, continuation rows the matching indent; each row truncates
/// to the width.
fn add_wrapped(lines: &mut RowOutput, prefix: &Line, body: &Line, width: usize) {
    let prefix_width: usize = prefix.iter().map(|s| str_width(&s.content)).sum();
    let available = width.saturating_sub(1 + prefix_width).max(1);
    if lines.is_counting() {
        lines.add_count(crate::width::wrapped_line_count(body, available).max(1));
        return;
    }
    let wrapped = wrap_line(body, available);
    let mut rows: Vec<Line> = Vec::new();
    if wrapped.is_empty() {
        rows.push(Vec::new());
    } else {
        rows.extend(wrapped);
    }
    for (index, mut row) in rows.into_iter().enumerate() {
        let mut line: Line = vec![Span::raw(" ")];
        if index == 0 {
            line.extend(prefix.iter().cloned());
        } else {
            line.push(Span::raw(" ".repeat(prefix_width)));
        }
        line.append(&mut row);
        lines.push(|| truncate_line(&line, width, ""));
    }
}

#[cfg(test)]
mod scheme_tests;
#[cfg(test)]
mod tests;
