//! The shortcut overview `?` opens on an empty prompt: the keys most worth knowing, grouped
//! (composer, transcript, panels, session) and named by the EFFECTIVE bindings, so a rebound key
//! shows its new key and an unbound action drops its row. It mounts as the read-only info panel
//! (its frame, scrolling and close keys); `/hotkeys` stays the full reference. Any printable key
//! closes the overview and types into the prompt, so a message can still start with `?`: press
//! it twice.

use crate::keybindings::{format_key_text, KeybindingsManager};
use crate::menu_chrome::ELLIPSIS;
use crate::theme::{Theme, ThemeColor};
use crate::width::{str_width, truncate_to_width};
use crate::{Line, Span};

/// Whether the transcript's mouse gestures are on (the `terminal.fullscreenMouse` tracking): the
/// overview lists them only when the terminal reports the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseGestures {
    Tracked,
    Untracked,
}

/// What a row's key names: the first key of each binding (joined with `/`), a key the surface
/// matches literally (`/`, `@`), or a mouse gesture (listed only while the mouse is tracked).
enum RowKey {
    Bound(&'static [&'static str]),
    Literal(&'static str),
    Mouse(&'static str),
}

/// The overview's groups in reading order: `(title, rows)`, each row `(key, action)`.
const GROUPS: &[(&str, &[(RowKey, &str)])] = &[
    (
        "Composer",
        &[
            (RowKey::Bound(&["tui.input.submit"]), "send"),
            (
                RowKey::Bound(&["app.message.followUp"]),
                "queue a follow-up",
            ),
            (RowKey::Literal("/"), "commands"),
            (RowKey::Literal("@"), "attach a file"),
            (RowKey::Literal("!"), "run a shell command"),
            (
                RowKey::Bound(&["app.history.search"]),
                "search past prompts",
            ),
            (RowKey::Bound(&["app.prompt.stash"]), "stash the draft"),
        ],
    ),
    (
        "Transcript",
        &[
            (
                RowKey::Bound(&["tui.viewport.pageUp", "tui.viewport.pageDown"]),
                "scroll a page",
            ),
            (
                RowKey::Bound(&["tui.viewport.previousPrompt", "tui.viewport.nextPrompt"]),
                "jump prompts",
            ),
            (RowKey::Bound(&["tui.viewport.follow"]), "follow new output"),
            (RowKey::Bound(&["app.tools.expand"]), "more or less detail"),
            (RowKey::Mouse("drag"), "select and copy"),
            (RowKey::Mouse("double-click"), "copy a word"),
            (RowKey::Mouse("triple-click"), "copy a line"),
        ],
    ),
    (
        "Panels",
        &[
            (RowKey::Bound(&["app.model.select"]), "pick a model"),
            (RowKey::Bound(&["app.thinking.cycle"]), "cycle the effort"),
            (RowKey::Bound(&["app.context.open"]), "context window"),
            (
                RowKey::Bound(&["app.subagents.focus"]),
                "focus the activity dock",
            ),
            (RowKey::Bound(&["app.todos.toggle"]), "expand the todo list"),
        ],
    ),
    (
        "Session",
        &[
            (RowKey::Bound(&["app.input.clear"]), "stop; twice: rewind"),
            (RowKey::Bound(&["app.clear"]), "clear; twice: exit"),
            (RowKey::Bound(&["app.exit"]), "exit (empty prompt)"),
            (RowKey::Bound(&["app.suspend"]), "suspend"),
        ],
    ),
];

/// The closing line's two parts: how to leave the overview by typing, and where the full list
/// lives. A pane too narrow for both on one row gives each its own.
const CLOSING_PARTS: [&str; 2] = ["Type to start a message", "/hotkeys lists every key"];

/// The narrowest width that lays the groups out in two columns.
const TWO_COLUMN_MIN_WIDTH: usize = 76;
/// The gap between the two columns.
const COLUMN_GAP: usize = 4;
/// The gap between a row's key and its action.
const KEY_GAP: usize = 2;
/// The widest key the key column aligns to; a longer key (a pair of chords) pushes only its own
/// row's action along.
const KEY_COLUMN_MAX: usize = 14;

/// The overview's content resolved against the bindings at open time: the groups' rows with
/// their key labels. An action whose bindings are all unbound has no row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShortcutSheet {
    groups: Vec<Group>,
}

impl ShortcutSheet {
    #[must_use]
    pub(crate) fn new(kb: &KeybindingsManager, mouse: MouseGestures) -> Self {
        let groups = GROUPS
            .iter()
            .map(|(title, rows)| {
                let resolved: Vec<(String, &'static str)> = rows
                    .iter()
                    .filter_map(|(key, action)| {
                        let label = match key {
                            RowKey::Bound(bindings) => bindings
                                .iter()
                                .filter_map(|binding| kb.first_key(binding))
                                .map(|key| format_key_text(&key))
                                .collect::<Vec<_>>()
                                .join("/"),
                            RowKey::Literal(key) => (*key).to_string(),
                            RowKey::Mouse(gesture) => match mouse {
                                MouseGestures::Tracked => (*gesture).to_string(),
                                MouseGestures::Untracked => String::new(),
                            },
                        };
                        (!label.is_empty()).then_some((label, *action))
                    })
                    .collect();
                (*title, resolved)
            })
            .filter(|(_, rows)| !rows.is_empty())
            .collect();
        Self { groups }
    }

    /// The overview's rows at `width`: two columns of groups on a wide pane (composer and panels
    /// on the left, transcript and session on the right, so the columns run about as long), one
    /// column in reading order otherwise. Every row fits the width; a cut action ends in the
    /// shared cut mark.
    #[must_use]
    pub(crate) fn layout(&self, theme: &Theme, width: usize) -> Vec<Line> {
        // One leading cell, like the info panel's other content rows.
        let inner = width.saturating_sub(1);
        // The way out and the way to the full list lead, so a short pane that scrolls the
        // groups still shows them.
        let closing = CLOSING_PARTS.join(crate::menu_chrome::SEPARATOR);
        let closing = if str_width(&closing) <= inner {
            vec![closing]
        } else {
            CLOSING_PARTS.map(str::to_string).to_vec()
        };
        let mut rows: Vec<Line> = closing
            .into_iter()
            .map(|part| {
                vec![theme.fg_span(ThemeColor::Dim, truncate_to_width(&part, inner, ELLIPSIS))]
            })
            .collect();
        rows.push(Vec::new());
        rows.extend(if inner >= TWO_COLUMN_MIN_WIDTH {
            let column = (inner - COLUMN_GAP) / 2;
            let left: Vec<&Group> = self.groups.iter().step_by(2).collect();
            let right: Vec<&Group> = self.groups.iter().skip(1).step_by(2).collect();
            let left = column_rows(theme, &left, column);
            let right = column_rows(theme, &right, column);
            (0..left.len().max(right.len()))
                .map(|index| {
                    let mut row = left.get(index).cloned().unwrap_or_default();
                    let used = crate::width::line_width(&row);
                    row.push(Span::raw(" ".repeat(column + COLUMN_GAP - used)));
                    row.extend(right.get(index).cloned().unwrap_or_default());
                    row
                })
                .collect::<Vec<Line>>()
        } else {
            let groups: Vec<&Group> = self.groups.iter().collect();
            column_rows(theme, &groups, inner)
        });
        // One blank above the panel's footer.
        rows.push(Vec::new());
        rows.into_iter()
            .map(|row| {
                let mut line = vec![Span::raw(" ")];
                line.extend(row);
                line
            })
            .collect()
    }
}

/// One group: its title and its `(key, action)` rows.
type Group = (&'static str, Vec<(String, &'static str)>);

/// One column of groups at `width`: each group's title, then its rows with the keys in one
/// column as wide as the widest key that fits [`KEY_COLUMN_MAX`] (a longer key pushes only its
/// own action along) and the actions beside them, cut to the column with the shared mark.
fn column_rows(theme: &Theme, groups: &[&Group], width: usize) -> Vec<Line> {
    let key_width = groups
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|(key, _)| str_width(key)))
        .filter(|key_width| *key_width <= KEY_COLUMN_MAX)
        .max()
        .unwrap_or(0);
    let mut rows: Vec<Line> = Vec::new();
    for (index, (title, group)) in groups.iter().enumerate() {
        if index > 0 {
            rows.push(Vec::new());
        }
        rows.push(vec![theme.fg_span(
            ThemeColor::Accent,
            truncate_to_width(title, width, ELLIPSIS),
        )]);
        for (key, action) in group {
            let key = truncate_to_width(key, width, ELLIPSIS);
            let pad = key_width.max(str_width(&key)) - str_width(&key) + KEY_GAP;
            let action_width = width.saturating_sub(str_width(&key) + pad);
            rows.push(vec![
                theme.fg_span(ThemeColor::Text, key),
                Span::raw(" ".repeat(pad)),
                theme.fg_span(
                    ThemeColor::Muted,
                    truncate_to_width(action, action_width, ELLIPSIS),
                ),
            ]);
        }
    }
    rows
}

/// Hand the overview's open to the telemetry seam and return at once: the key path never waits
/// on the delivery.
pub(crate) fn report_opened(
    telemetry: Option<&std::sync::Arc<dyn crate::interactive::InteractionTelemetry>>,
) {
    if let Some(telemetry) = telemetry.cloned() {
        tokio::spawn(async move {
            telemetry.shortcut_overlay_opened().await;
        });
    }
}

#[cfg(test)]
mod tests;
