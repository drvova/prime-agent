use super::*;
use crate::keybindings::KeybindingsConfig;
use crate::theme::ColorMode;

fn theme() -> Theme {
    Theme::builtin("prime", ColorMode::TrueColor)
}

/// The overview as the info panel renders it at `width`, trailing padding trimmed.
fn panel_rows(kb: &KeybindingsManager, width: usize, mouse: MouseGestures) -> Vec<String> {
    crate::info_panel::InfoPanel::shortcuts(ShortcutSheet::new(kb, mouse))
        .render(&theme(), width, kb, "  ", 60)
        .iter()
        .map(|row| {
            row.iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// A wide pane lays the groups out in two columns, a narrow one in one; every key is named by
/// its effective binding and the overview ends with the way to the full list.
#[test]
fn the_overview_groups_the_bound_keys_in_one_or_two_columns() {
    let kb = KeybindingsManager::new();
    assert_eq!(
        [
            panel_rows(&kb, 80, MouseGestures::Tracked),
            panel_rows(&kb, 40, MouseGestures::Tracked),
        ],
        [
            vec![
                "\u{2500}".repeat(80),
                "  Shortcuts".to_string(),
                String::new(),
                " Type to start a message \u{b7} /hotkeys lists every key".to_string(),
                String::new(),
                " Composer                                 Transcript".to_string(),
                " Enter      send                          PageUp/PageDown  scroll a page"
                    .to_string(),
                " Alt+Enter  queue a follow-up             Alt+PageUp/Alt+PageDown  jump prompts"
                    .to_string(),
                " /          commands                      Ctrl+Shift+\u{2193}  follow new output"
                    .to_string(),
                " @          attach a file                 Ctrl+O        more or less detail"
                    .to_string(),
                " !          run a shell command           drag          select and copy"
                    .to_string(),
                " Ctrl+R     search past prompts           double-click  copy a word".to_string(),
                " Ctrl+S     stash the draft               triple-click  copy a line".to_string(),
                String::new(),
                " Panels                                   Session".to_string(),
                " Ctrl+L     pick a model                  Esc           stop; twice: rewind"
                    .to_string(),
                " Shift+Tab  cycle the effort              Ctrl+C        clear; twice: exit"
                    .to_string(),
                " Alt+X      context window                Ctrl+D        exit (empty prompt)"
                    .to_string(),
                " Alt+A      focus the activity dock       Ctrl+Z        suspend".to_string(),
                " Alt+T      expand the todo list".to_string(),
                String::new(),
                "  \u{2191}/\u{2193} scroll \u{b7} Esc close".to_string(),
                String::new(),
            ],
            vec![
                "\u{2500}".repeat(40),
                "  Shortcuts".to_string(),
                String::new(),
                " Type to start a message".to_string(),
                " /hotkeys lists every key".to_string(),
                String::new(),
                " Composer".to_string(),
                " Enter         send".to_string(),
                " Alt+Enter     queue a follow-up".to_string(),
                " /             commands".to_string(),
                " @             attach a file".to_string(),
                " !             run a shell command".to_string(),
                " Ctrl+R        search past prompts".to_string(),
                " Ctrl+S        stash the draft".to_string(),
                String::new(),
                " Transcript".to_string(),
                " PageUp/PageDown  scroll a page".to_string(),
                " Alt+PageUp/Alt+PageDown  jump prompts".to_string(),
                " Ctrl+Shift+\u{2193}  follow new output".to_string(),
                " Ctrl+O        more or less detail".to_string(),
                " drag          select and copy".to_string(),
                " double-click  copy a word".to_string(),
                " triple-click  copy a line".to_string(),
                String::new(),
                " Panels".to_string(),
                " Ctrl+L        pick a model".to_string(),
                " Shift+Tab     cycle the effort".to_string(),
                " Alt+X         context window".to_string(),
                " Alt+A         focus the activity dock".to_string(),
                " Alt+T         expand the todo list".to_string(),
                String::new(),
                " Session".to_string(),
                " Esc           stop; twice: rewind".to_string(),
                " Ctrl+C        clear; twice: exit".to_string(),
                " Ctrl+D        exit (empty prompt)".to_string(),
                " Ctrl+Z        suspend".to_string(),
                String::new(),
                "  \u{2191}/\u{2193} scroll \u{b7} Esc close".to_string(),
                String::new(),
            ],
        ]
    );
}

/// A rebound action shows its new key, and an action whose binding is emptied drops its row.
#[test]
fn a_rebound_key_shows_and_an_unbound_action_drops() {
    let mut bindings = KeybindingsConfig::new();
    bindings.insert("app.model.select".to_string(), vec!["ctrl+k".to_string()]);
    bindings.insert("app.prompt.stash".to_string(), Vec::new());
    let sheet = ShortcutSheet::new(
        &KeybindingsManager::with_user_bindings(bindings),
        MouseGestures::Untracked,
    );
    let rows: Vec<(String, &str)> = sheet
        .groups
        .iter()
        .flat_map(|(_, rows)| rows.iter().cloned())
        .filter(|(_, action)| ["pick a model", "stash the draft"].contains(action))
        .collect();
    assert_eq!(rows, [("Ctrl+K".to_string(), "pick a model")]);
}
