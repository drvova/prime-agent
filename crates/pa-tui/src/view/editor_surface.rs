//! The editor box: the shared renderer for every editor-bearing surface (the chat's
//! prompt dock and the agents view's action composers); the surfaces compose it with
//! their own headers, placeholders, and click regions. Row shape: the top bg row (the
//! `+N lines above` counter once content hides above), the header block's two rows, the
//! content rows, and the trailing bg row (`+N lines below`); a placeholder replaces the first
//! content row while the editor is empty.

use super::chunk_selection;
use super::flush::split_at_chars;
use super::frame::pad_row;
use crate::editor::Editor;
use crate::prompt_highlight::{
    command_token, editor_chunk_highlights, editor_text_spans, find_arg_tokens, ArgTokenSpan,
};
use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::width::{str_width, truncate_to_width};
use crate::{Line, Span};
use pa_types::slash_commands::SlashCommandRegistry;
use ratatui::style::Modifier;

pub(crate) struct EditorBox {
    /// The box's rows, top bg row first.
    pub(crate) rows: Vec<Line>,
    /// The cursor's row and column within the box; `None` while the window
    /// shows no cursor.
    pub(crate) cursor: Option<(usize, usize)>,
    /// The rows between the top row and the first content row (a header block
    /// inserts two).
    pub(crate) content_offset: usize,
    /// The rendered prompt's visible width (`> `, `! `, `!! `).
    pub(crate) prompt_width: usize,
    /// The width the editor's layout wrapped at.
    pub(crate) content_width: usize,
    pub(crate) visible_rows: usize,
}

/// What colours the `> ` prompt (the composer's gutter) when no mode owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptTint {
    /// The plain prompt (the agents view's composers, a model without reasoning).
    Plain,
    /// The session's thinking level, oh-my-pi's effort-tinted editor edge
    /// (`updateEditorBorderColor`): dim through low, muted at medium, accent at high, warm
    /// above it.
    Thinking(pa_types::ai::ModelThinkingLevel),
}

/// The editor box's surface tone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceTone {
    /// The plain editor surface, its `> ` prompt in the given tint.
    Idle(PromptTint),
    /// A draft typed into a running turn (nc's steering composer): the warm surface and a
    /// warning-colored `▐` rail down the left edge. The prompt drops the thinking tint, so the
    /// steering cue reads alone.
    Steering,
}

/// What the first content row shows while the editor is empty.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EmptyRow<'a> {
    /// Dim text after the cursor (a placeholder, or a transient status).
    pub(crate) placeholder: Option<&'a str>,
    /// A dim key hint, flush right, when it fits beside the placeholder.
    pub(crate) hint: Option<&'a str>,
}

/// Compose one editor box. `header` renders as the two-row block under the top
/// row; while the editor is empty, the [`EmptyRow`]'s placeholder (dim, after the cursor) and
/// hint (dim, flush right) take the first content row.
pub(crate) fn render(
    editor: &mut Editor,
    theme: &Theme,
    width: usize,
    terminal_rows: u16,
    header: Option<Line>,
    EmptyRow { placeholder, hint }: EmptyRow<'_>,
    tone: SurfaceTone,
) -> EditorBox {
    let bg = match tone {
        SurfaceTone::Idle(_) => crate::chrome::editor_background(theme),
        SurfaceTone::Steering => theme.steering_surface_style(),
    };
    let padding_x = 2usize;
    let content_width = width.saturating_sub(padding_x * 2).max(1);
    // A bang first line swaps the `> ` for the same-width `! `/`!!` prompt.
    let bash_prompt = editor.bash_prompt_prefix();
    let prompt = bash_prompt.unwrap_or("> ");
    // The prompt's colour, highest claim first: bash mode owns it (by colour on the editor
    // background, never by width: `!` in the bash-mode tint, the `!` card's; `!!` dim, its
    // context-excluded card's), then steering (the plain prompt on the warm surface), then the
    // thinking tint. Only the colour moves, so no row shifts width.
    let prompt_style = match (bash_prompt, tone) {
        (Some("!!"), _) => bg.patch(theme.fg_style(ThemeColor::Dim)),
        (Some(_), _) => bg.patch(theme.fg_style(ThemeColor::BashMode)),
        (None, SurfaceTone::Steering | SurfaceTone::Idle(PromptTint::Plain)) => bg,
        (None, SurfaceTone::Idle(PromptTint::Thinking(level))) => {
            use pa_types::ai::ModelThinkingLevel as L;
            let color = match level {
                L::Off | L::Minimal | L::Low => ThemeColor::Dim,
                L::Medium => ThemeColor::Muted,
                L::High => ThemeColor::Accent,
                L::Xhigh | L::Max => ThemeColor::Warning,
            };
            bg.patch(theme.fg_style(color))
        }
    };
    let prompt_width = str_width(prompt);
    let input_width = content_width.saturating_sub(prompt_width).max(1);
    let layout_width = input_width;
    let (visible, scroll_offset, _hidden_above, hidden_below) =
        editor.visible_window(layout_width, terminal_rows);
    let content_offset = usize::from(header.is_some()) * 2;
    // The overflow counters (nc): a draft taller than the box names the rows it hides, dim and
    // flush right in the box's own padding rows, so the box never grows a row for them.
    let overflow_row = |count: usize, side: &str| -> Line {
        let label = format!("+{count} line{} {side}", if count == 1 { "" } else { "s" });
        let label = truncate_to_width(&label, width.saturating_sub(padding_x), "");
        let used = str_width(&label) + padding_x;
        vec![
            Span::styled(" ".repeat(width.saturating_sub(used)), bg),
            Span::styled(label, bg.patch(theme.fg_style(ThemeColor::Dim))),
            Span::styled(" ".repeat(padding_x.min(width)), bg),
        ]
    };
    let mut rows: Vec<Line> = Vec::new();
    if scroll_offset > 0 {
        rows.push(overflow_row(scroll_offset, "above"));
    } else {
        rows.push(vec![Span::styled(" ".repeat(width), bg)]);
    }
    // The header block: the caller's line on the editor background plus its
    // empty companion row — the box grows by two rows while a header shows, and
    // the click regions shift with it.
    if let Some(header) = header {
        let mut row: Line = vec![Span::styled(" ".repeat(padding_x), bg)];
        row.extend(crate::width::truncate_line(
            &header,
            content_width,
            crate::menu_chrome::ELLIPSIS,
        ));
        let used = crate::width::line_width(&row);
        row.push(Span::styled(" ".repeat(width.saturating_sub(used)), bg));
        rows.push(row);
        rows.push(vec![Span::styled(" ".repeat(width), bg)]);
    }
    // A bare `--` separator highlights only while the first line opens with an
    // argument-taking slash command.
    let selection = editor.selection_range();
    let editor_lines = editor.get_lines();
    let registry = SlashCommandRegistry::builtin_cached();
    let include_bare_separator = editor_lines
        .first()
        .and_then(|first| command_token(first))
        .is_some_and(|token| registry.takes_argument(&token.name));
    let arg_token_spans: Vec<Vec<ArgTokenSpan>> = editor_lines
        .iter()
        .map(|line| find_arg_tokens(line, 0, include_bare_separator))
        .collect();
    let mut cursor: Option<(usize, usize)> = None;
    // The empty row: while the editor is empty, the first content row is the cursor cell
    // followed by the dim placeholder, with the dim hint flush right when both fit.
    let empty_row = (placeholder.is_some() || hint.is_some()) && editor.get_text().is_empty();
    let ghost = editor.slash_argument_ghost();
    for (index, line) in visible.iter().enumerate() {
        if index == 0 && empty_row {
            let dim = bg.patch(theme.fg_style(ThemeColor::Dim));
            let mut row: Line = vec![Span::styled(" ".to_string(), bg)];
            row.push(Span::styled(prompt.to_string(), prompt_style));
            row.push(Span::styled(" ".to_string(), bg));
            let placeholder_width = input_width.saturating_sub(1);
            let text = truncate_to_width(placeholder.unwrap_or(""), placeholder_width, "");
            // The hint keeps a two-cell gap from the placeholder and drops when it would not fit.
            let hint =
                hint.filter(|hint| str_width(&text) + 2 + str_width(hint) <= placeholder_width);
            let hint_width = hint.map_or(0, str_width);
            let fill = " ".repeat(placeholder_width.saturating_sub(str_width(&text) + hint_width));
            row.push(Span::styled(
                " ".to_string(),
                bg.add_modifier(Modifier::REVERSED),
            ));
            row.push(Span::styled(text, dim));
            row.push(Span::styled(fill, bg));
            if let Some(hint) = hint {
                row.push(Span::styled(hint.to_string(), dim));
            }
            row.push(Span::styled(" ".repeat(padding_x), bg));
            rows.push(row);
            cursor = Some((1 + content_offset, prompt_width + 2));
            continue;
        }
        let mut row: Line = vec![Span::styled(" ".to_string(), bg)];
        if index == 0 {
            row.push(Span::styled(prompt.to_string(), prompt_style));
        } else {
            row.push(Span::styled(" ".repeat(prompt_width), bg));
        }
        row.push(Span::styled(" ".to_string(), bg));
        let text: &str = &line.text;
        let cursor_pos = line
            .has_cursor
            .then(|| line.cursor_pos.min(text.chars().count()));
        // The prompt-highlight spans of this chunk: argument tokens, and the
        // command token of the first layout line in accent unless the cursor
        // sits inside it.
        let command = (scroll_offset + index == 0)
            .then(|| command_token(text))
            .flatten();
        let highlights = editor_chunk_highlights(
            text,
            arg_token_spans
                .get(line.source_line)
                .map_or(&[][..], |spans| spans),
            line.source_start,
            command.as_ref(),
            cursor_pos,
        );
        // Collapsed pastes and completed `@file` mentions render as chips; the editor moves
        // over and deletes each as one unit.
        let chips = editor.chip_ranges(text);
        row.extend(editor_text_spans(
            theme,
            text,
            &highlights,
            &chips,
            chunk_selection(selection, line.source_line, line.source_start, text),
            cursor_pos,
            bg,
        ));
        let mut used = str_width(text);
        if cursor_pos == Some(text.chars().count()) {
            // The end-of-line cursor appends one reversed cell.
            used += 1;
            // The typed command's argument hint ghosts dim after the cursor.
            if let Some(hint) = ghost.as_deref() {
                let hint = truncate_to_width(hint, input_width.saturating_sub(used), "\u{2026}");
                used += str_width(&hint);
                row.push(Span::styled(
                    hint,
                    bg.patch(theme.fg_style(ThemeColor::Dim)),
                ));
            }
        }
        if let Some(position) = cursor_pos {
            let head = split_at_chars(text, position).0;
            cursor = Some((
                1 + content_offset + index,
                str_width(head) + prompt_width + 2,
            ));
        }
        row.push(Span::styled(
            " ".repeat(input_width.saturating_sub(used)),
            bg,
        ));
        row.push(Span::styled(" ".repeat(padding_x), bg));
        rows.push(row);
    }
    if hidden_below > 0 {
        rows.push(overflow_row(hidden_below, "below"));
    } else {
        rows.push(vec![Span::styled(" ".repeat(width), bg)]);
    }
    match tone {
        SurfaceTone::Idle(_) => {}
        // The rail takes every row's first cell (always a padding space), so the cursor and
        // click columns keep their places.
        SurfaceTone::Steering => {
            let rail = bg.patch(theme.fg_style(ThemeColor::Warning));
            for row in &mut rows {
                if let Some(first) = row.first_mut() {
                    first.content = first.content.chars().skip(1).collect::<String>();
                }
                row.insert(0, Span::styled("\u{2590}", rail));
            }
        }
    }
    EditorBox {
        rows,
        cursor,
        content_offset,
        prompt_width,
        content_width: layout_width,
        visible_rows: visible.len(),
    }
}

/// The autocomplete dropdown, mounted just above the editor surface. The panel
/// opens with the one full-width muted rule every inline menu panel opens with (the
/// operator's 2026-09-26 top-border directive), its rows float on the popup
/// background, and the selected row's wash spans the panel's full width. `live`
/// answers a command row's current state (see `AutocompleteState::render`).
pub(crate) fn overlay(
    editor: &Editor,
    theme: &Theme,
    width: usize,
    live: &dyn Fn(&str) -> Option<String>,
) -> Vec<Line> {
    let Some(state) = editor.autocomplete_state() else {
        return Vec::new();
    };
    let bg = theme.bg_style(ThemeBg::ToolPanelBg);
    let selection = theme.soft_selection_style();
    let padding_x = 2usize;
    // The overlay anchors against the live prompt prefix (the `!`/`!!` prompts
    // included).
    let prompt_width = str_width(editor.bash_prompt_prefix().unwrap_or("> "));
    let content_width = width.saturating_sub(padding_x * 2).max(1);
    let input_width = content_width.saturating_sub(prompt_width).max(1);
    let border = theme.fg_style(ThemeColor::BorderMuted).patch(bg);
    let mut rows: Vec<Line> = vec![vec![Span::styled("\u{2500}".repeat(width.max(1)), border)]];
    let mut overlay = Vec::new();
    overlay.extend(state.render(theme, input_width, live));
    overlay.push(Vec::new());
    for mut line in overlay {
        // The shared menu rows pad with unstyled spans, so the popup background
        // must ride on every span the row left unstyled. The selected row's edge
        // padding washes with the selection too, so the band spans the panel's full
        // width.
        let selected = line.iter().any(|span| span.style.bg.is_some());
        for span in &mut line {
            if span.style.bg.is_none() {
                span.style = span.style.patch(bg);
            }
        }
        let used: usize = line.iter().map(|s| str_width(&s.content)).sum();
        let edge = if selected { bg.patch(selection) } else { bg };
        let mut row: Line = vec![Span::styled(" ".repeat(padding_x + prompt_width), edge)];
        row.extend(line);
        row.push(Span::styled(
            " ".repeat(input_width.saturating_sub(used)),
            edge,
        ));
        row.push(Span::styled(" ".repeat(padding_x), edge));
        rows.push(pad_row(row, width));
    }
    rows
}
