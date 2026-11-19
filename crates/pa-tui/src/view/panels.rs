//! The panel assembly: the dock — prompt-context rows, the queued-input
//! strip, the autocomplete overlay, the editor surface, the tray, the
//! subagent summary box (TS `SubagentSummaryLine`) — plus the share
//! loader and reload-box panels.

use super::click::{ClickAction, DockClickRegion, EditorClickSurface};
use super::editor_surface;
use super::{AgentView, ShareLoader};
use crate::chrome::{render_prompt_context, render_tray_with_hint};
use crate::theme::ThemeColor;
use crate::{Line, Span};
use ratatui::style::Style;

impl AgentView {
    pub fn render_dock(&mut self, width: usize) -> Vec<Line> {
        // The queued-input strip sits directly above the prompt dock rows
        // (TS `queuedMessagesContainer` above the editor).
        let keys = {
            let kb = self.editor.keybindings();
            let first = |id: &str| {
                kb.first_key(id)
                    .map(|key| crate::keybindings::format_key_text(&key))
                    .unwrap_or_default()
            };
            crate::queued::PendingStripKeys {
                dispatch: first("app.input.clear"),
                edit: first("app.message.navigateOlder"),
            }
        };
        // The todo tray tops the dock, above the queued-input strip.
        let mut lines = crate::todo_panel::render_todo_panel(&self.todos, &self.theme, width);
        lines.extend(crate::queued::render_queue(
            &self.theme,
            &self.queued,
            &keys,
            width,
        ));
        lines.extend(render_prompt_context(
            &self.detail_label(),
            &self.theme,
            width,
        ));
        let context_rows = lines.len();
        let overlay_rows = self.render_autocomplete_overlay(width);
        lines.extend(overlay_rows);
        let overlay_count = lines.len() - context_rows;
        let (editor_rows, cursor) = self.render_editor_surface(width, context_rows + overlay_count);
        self.dock_cursor = cursor.map(|(row, col)| (context_rows + overlay_count + row, col));
        lines.extend(editor_rows);
        // The tray's `← manage` hint is a click region (operator directive 2026-09-29): its own
        // cells, never the depth label, perform the handoff.
        let tray_row = lines.len();
        let tray = render_tray_with_hint(&self.chrome, &self.theme, width);
        lines.push(tray.line);
        if let Some(hint) = tray.manage.filter(|hint| hint.start < width) {
            self.click.record_dock_region(DockClickRegion {
                dock_row: tray_row,
                cols: hint.start..hint.end.min(width),
                action: ClickAction::OpenAgentsView,
            });
        }
        // The context gauge and its label open the context window (oh-my-pi's clickable meter).
        if let Some(context) = tray.context {
            self.click.record_dock_region(DockClickRegion {
                dock_row: tray_row,
                cols: context,
                action: ClickAction::OpenContextView,
            });
        }
        // The activity dock's group segments are click regions too: the
        // dock frame starts right under the tray.
        if let Some(dock) = &self.chrome.activity {
            let (frame, segments) =
                crate::chrome::render_activity_dock_segments(dock, &self.theme, width);
            for segment in segments {
                self.click.record_dock_region(DockClickRegion {
                    dock_row: tray_row + 1 + segment.row,
                    cols: segment.cols,
                    action: ClickAction::OpenDockGroup(segment.group),
                });
            }
            lines.extend(frame);
        }
        // The `/speed` footer (TS `footerSlot`): a dim row only while
        // the display is on with a sample.
        if let Some(speed) = &self.chrome.speed_text {
            lines.push(crate::chrome::render_speed_footer(
                speed,
                &self.theme,
                width,
            ));
        }
        lines
    }

    /// The autocomplete dropdown, mounted just above the editor surface (TS anchors the overlay
    /// above the cursor row). The panel opens with the full-width muted rule (the operator's
    /// 2026-09-26 top-border directive).
    fn render_autocomplete_overlay(&mut self, width: usize) -> Vec<Line> {
        // The command rows' live state, read from the chrome the tray already renders: the menu
        // doubles as a status read-out without a daemon round-trip.
        let chrome = &self.chrome;
        let live = |command: &str| match command {
            "model" => {
                chrome
                    .model_id
                    .as_ref()
                    .map(|model| match chrome.thinking_suffix.as_deref() {
                        Some(level) => format!("{model} \u{b7} {level}"),
                        None => model.clone(),
                    })
            }
            "effort" => chrome.thinking_suffix.clone(),
            "fast" => Some(
                if chrome.service_tier.as_deref() == Some("priority") {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
            ),
            "tier" => Some(
                chrome
                    .service_tier
                    .clone()
                    .unwrap_or_else(|| "default".to_string()),
            ),
            _ => None,
        };
        editor_surface::overlay(&self.editor, &self.theme, width, &live)
    }

    /// The editor surface (TS `Editor.render` with a background): a
    /// blank bg row, content rows with the `> ` prompt and a reverse-video
    /// cursor, and a trailing bg row.
    fn render_editor_surface(
        &mut self,
        width: usize,
        dock_row: usize,
    ) -> (Vec<Line>, Option<(usize, usize)>) {
        // The queue-browse header (while a parked message is selected): the dim
        // browse text on the editor background, from the shared box's header block.
        let header = self.queue_selected.as_ref().map(|selected| {
            let keys = {
                let kb = self.editor.keybindings();
                let display =
                    |id: &str| crate::keybindings::format_key_text(&kb.get_keys(id).join("/"));
                crate::queued::QueueBrowseKeys {
                    navigate_older: display("app.message.navigateOlder"),
                    navigate_newer: display("app.message.navigateNewer"),
                    move_earlier: display("app.message.moveEarlier"),
                    move_later: display("app.message.moveLater"),
                    follow_up: display("app.message.followUp"),
                }
            };
            let dim = crate::chrome::editor_background(&self.theme)
                .patch(self.theme.fg_style(ThemeColor::Dim));
            vec![Span::styled(
                crate::queued::browse_header_text(selected, &keys),
                dim,
            )]
        });
        // The prompt carries the session's thinking level (the tray glyph's level), plain while
        // the model has none.
        let tint = self
            .chrome
            .thinking_suffix
            .as_deref()
            .and_then(pa_types::ai::thinking_level_from_str)
            .map_or(
                editor_surface::PromptTint::Plain,
                editor_surface::PromptTint::Thinking,
            );
        // A draft typed while a turn runs is a steering draft (Enter steers, the follow-up key
        // queues): the warm surface says so until the draft empties or the turn ends, and the
        // prompt drops the thinking tint meanwhile. Browsing a parked message keeps the plain
        // surface under its own header.
        let tone = if self.working.is_some()
            && self.queue_selected.is_none()
            && !self.editor.get_text().trim().is_empty()
        {
            editor_surface::SurfaceTone::Steering
        } else {
            editor_surface::SurfaceTone::Idle(tint)
        };
        let surface = editor_surface::render(
            &mut self.editor,
            &self.theme,
            width,
            self.terminal_rows,
            header,
            // A transient status (a slash result, a copy) rides the empty composer's text row
            // until the next keystroke, beside the self-retiring key hint.
            editor_surface::EmptyRow {
                placeholder: self.composer_status.as_deref(),
                hint: self.composer_hint.as_deref(),
            },
            tone,
        );
        // The content rows' click surface: one region over the visible content rows,
        // shifted by the header block's rows.
        self.click.record_editor(EditorClickSurface {
            dock_row,
            rows: surface.visible_rows,
            content_offset: surface.content_offset,
            prompt_width: surface.prompt_width,
            content_width: surface.content_width,
        });
        (surface.rows, surface.cursor)
    }

    /// The `/share` loader rows (TS `BorderedLoader`): border,
    /// spinner + message, cancel hint, border — replacing the editor while
    /// `gh gist create` runs.
    pub(super) fn render_share_loader(&self, loader: &ShareLoader, width: usize) -> Vec<Line> {
        let border = self.theme.fg_style(ThemeColor::Border);
        let muted = self.theme.fg_style(ThemeColor::Muted);
        let dim = self.theme.fg_style(ThemeColor::Dim);
        let spinner =
            crate::chat::LOADER_FRAMES[self.pulse_frame % crate::chat::LOADER_FRAMES.len()];
        let mut rows: Vec<Line> = Vec::with_capacity(7);
        rows.push(vec![Span::styled("─".repeat(width.max(1)), border)]);
        let mut row: Line = vec![Span::styled(" ".to_string(), Style::default())];
        // TS `BorderedLoader` wraps a `Loader` with the muted spinner and message color fns;
        // the gap between them is unstyled plain space (see `chat::render_loader`).
        row.push(Span::styled(spinner.to_string(), muted));
        row.push(Span::raw(" ".to_string()));
        row.push(Span::styled(loader.message.clone(), muted));
        rows.push(row);
        rows.push(vec![Span::raw(String::new())]);
        // TS `keyHint("tui.select.cancel", "cancel")`: every key of the
        // binding, first letter capitalized, then the description.
        let key_text = self.editor.keybindings().key_text("tui.select.cancel");
        let mut hint: Line = vec![Span::styled(" ".to_string(), Style::default())];
        hint.push(Span::styled(key_text, dim));
        hint.push(Span::styled(" cancel".to_string(), muted));
        rows.push(hint);
        rows.push(vec![Span::raw(String::new())]);
        rows.push(vec![Span::styled("─".repeat(width.max(1)), border)]);
        rows
    }

    /// The `/reload` box (TS `handleReloadCommand`): `DynamicBorder`, blank, the muted message,
    /// blank, `DynamicBorder` — the editor replacement while the reload runs.
    pub(super) fn render_reload_box(&self, message: &str, width: usize) -> Vec<Line> {
        let border = self.theme.fg_style(ThemeColor::Border);
        let muted = self.theme.fg_style(ThemeColor::Muted);
        let rule = "─".repeat(width.max(1));
        let rows: Vec<Line> = vec![
            vec![Span::styled(rule.clone(), border)],
            vec![Span::raw(String::new())],
            vec![
                Span::raw(" ".to_string()),
                Span::styled(message.to_string(), muted),
            ],
            vec![Span::raw(String::new())],
            vec![Span::styled(rule, border)],
        ];
        rows
    }
}
