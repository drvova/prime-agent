//! Terminal UI for Prime Agent.
//!
//! Rust port of the TS reference TUI (packages/tui) plus the interactive agent view
//! from `coding-agent/src/modes/interactive`. Components render styled lines;
//! the terminal layer paints them with crossterm + ratatui diffing.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "casts narrow structurally bounded values (screen coordinates, byte counts, timestamps)"
)]
#![expect(
    clippy::too_many_lines,
    reason = "render routes are flat tables; splitting adds indirection without changing the flow"
)]
#![expect(
    clippy::struct_excessive_bools,
    clippy::fn_params_excessive_bools,
    reason = "widget state carries independent flag bits; a nested struct adds indirection"
)]
#![expect(
    clippy::large_futures,
    reason = "the futures live as long as the surface; boxing adds an allocation to the steady-state loop"
)]
#![expect(
    clippy::unnecessary_wraps,
    reason = "the wrappers keep a uniform Result-returning API surface"
)]

pub mod agents_view;
pub mod agents_view_forest;
pub mod agents_view_search;
pub mod agents_view_state;
pub mod altscreen;
pub mod ansi;
pub mod app;
pub mod auth_panel;
pub mod autocomplete;
mod autolink;
pub mod bash_bang;
pub mod bash_card;
pub mod bash_view;
pub(crate) mod branch;
pub(crate) mod browser;
pub mod chat;
pub mod chat_slash;
mod choice_picker;
pub mod chrome;
mod click_dispatch;
pub mod client_auth;
pub mod client_settings;
mod clipboard;
mod clipboard_image;
pub mod code_preview;
mod compaction_row;
pub mod config_selector;
pub mod confirm;
pub mod context_view;
mod crash_notice;
pub mod custom_message;
pub mod daemon_client;
pub mod daemon_reconnect;
pub mod direct_transport;
mod duration_text;
pub mod editor;
pub mod effort_picker;
mod enhanced_keys;
pub mod error_summary;
pub mod exit_guard;
pub(crate) mod exit_restore;
pub mod export_share;
mod external_editor;
pub mod factory_view;
mod frame_output;
pub mod fuzzy;
pub mod goal_surface;
pub mod heartbeats_picker;
mod history_import_notice;
mod history_search;
pub mod hotkeys;
pub mod hyperlinks;
mod image_component;
pub mod image_load;
mod image_markers;
#[cfg(debug_assertions)]
mod inbound_recording;
pub mod incident_notices;
pub mod info_commands;
pub mod info_panel;
mod inline_image;
mod input;
pub mod interactive;
mod keybinding_notice;
pub mod keybindings;
pub mod keys;
pub mod markdown;
pub mod markdown_table;
pub mod mcp_view;
mod menu_chrome;
mod menu_panel;
pub mod model_picker;
pub(crate) mod mouse;
pub(crate) mod mouse_tracking;
mod new_session_command;
pub mod onboarding;
pub mod onboarding_choice;
pub mod onboarding_flow;
pub mod osc133;
pub mod osc52;
pub(crate) mod pasted_image_path;
pub(crate) mod prompt_highlight;
pub mod prompt_stash;
pub mod provider_auth;
mod question_prompt;
pub mod queued;
pub(crate) mod scroll_region;
pub(crate) mod search_input;
pub mod selection;
mod selection_gesture;
mod sequence_guard;
pub mod session;
mod session_changes;
pub mod session_open_error;
pub mod session_ui;
pub mod settings_menu;
mod shortcut_overlay;
pub mod side_question;
pub mod snapshot;
pub mod subagents;
mod suspend;
mod terminal_attention;
mod terminal_image;
mod terminal_title;
pub mod theme;
pub mod toast;
mod todo_panel;
pub mod tool_card;
pub mod traces;
pub mod tree_display;
pub mod tree_list;
pub mod tree_nodes;
pub mod tree_selector;
pub mod update_command;
pub(crate) mod update_restart_wait;
pub mod usage_page;
pub mod user_message_selector;
pub mod view;
pub mod width;
// The TUI's commands and events on wire2: the `daemon_client` facade's wire2 link carries them.
mod wire2_compat;
#[doc(hidden)]
pub use wire2_compat::{
    history_import_reply_frame as wire2_history_import_reply_frame,
    history_import_request as wire2_history_import_request, live_frames as wire2_live_frames,
    request_frames as wire2_request_frames, response_frame as wire2_response_frame,
};

use ratatui::style::Style;

/// A styled run of text. `style` applies to the whole `content`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub style: Style,
    pub content: String,
}

impl Span {
    pub fn raw(content: impl Into<String>) -> Self {
        Span {
            style: Style::default(),
            content: content.into(),
        }
    }

    pub fn styled(content: impl Into<String>, style: Style) -> Self {
        Span {
            style,
            content: content.into(),
        }
    }
}

/// A rendered line: styled spans laid out left to right.
pub type Line = Vec<Span>;

/// Render trait shared by all components.
pub trait Component {
    /// Render to lines for the given viewport width. Lines must not exceed `width` visible columns.
    fn render(&self, width: u16) -> Vec<Line>;
}
