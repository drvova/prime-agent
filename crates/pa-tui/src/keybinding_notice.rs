//! How a user binding that took another action's default key reads: one line per taken key for
//! `/hotkeys`, the one notice row a startup or `/reload` shows, and the startup telemetry
//! hand-off.

use crate::keybindings::{format_key_text, KeybindingTakeover, KeybindingsManager};
use std::collections::BTreeMap;

/// One taken default key: its label (`F2`) and what it now does
/// (`F2 → Resume a session (yours); Switch to the next recently used model has no key`).
pub(crate) struct TakeoverLine {
    pub(crate) key: String,
    pub(crate) text: String,
}

/// The lines for `takeovers`, one per default key and the user bindings that took it, in key
/// order. An action that lost the key says whether it still has another one.
pub(crate) fn takeover_lines(
    kb: &KeybindingsManager,
    takeovers: &[KeybindingTakeover],
) -> Vec<TakeoverLine> {
    let describe = |id: &str| {
        kb.get_definition(id).map_or_else(
            || id.to_string(),
            |definition| definition.description.to_string(),
        )
    };
    let mut by_key: BTreeMap<(&str, &[String]), Vec<&str>> = BTreeMap::new();
    for takeover in takeovers {
        by_key
            .entry((takeover.key.as_str(), takeover.taken_by.as_slice()))
            .or_default()
            .push(takeover.action.as_str());
    }
    by_key
        .into_iter()
        .map(|((key, taken_by), actions)| {
            let key = format_key_text(key);
            let yours = taken_by
                .iter()
                .map(|id| describe(id))
                .collect::<Vec<_>>()
                .join(" and ");
            let lost = actions
                .iter()
                .map(|action| match kb.get_keys(action).as_slice() {
                    [] => format!("{} has no key", describe(action)),
                    remaining => format!(
                        "{} keeps {}",
                        describe(action),
                        format_key_text(&remaining.join("/"))
                    ),
                })
                .collect::<Vec<_>>()
                .join("; ");
            TakeoverLine {
                text: format!("{key} \u{2192} {yours} (yours); {lost}"),
                key,
            }
        })
        .collect()
}

/// The notice row for takeovers a run has not announced yet: the one line itself, or the keys
/// with a pointer to `/hotkeys` when there are several. `None` when there is nothing to say.
pub(crate) fn takeover_notice(
    kb: &KeybindingsManager,
    takeovers: &[KeybindingTakeover],
) -> Option<String> {
    let lines = takeover_lines(kb, takeovers);
    match lines.as_slice() {
        [] => None,
        [line] => Some(format!(
            "Your keybindings.json takes a default key: {}",
            line.text
        )),
        several => Some(format!(
            "Your keybindings.json takes {} default keys ({}); /hotkeys lists what each replaced",
            several.len(),
            several
                .iter()
                .map(|line| line.key.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Hand the startup takeover count to the telemetry seam and return at once: the startup path
/// never waits on the delivery.
pub(crate) fn report_takeovers(
    telemetry: Option<&std::sync::Arc<dyn crate::interactive::InteractionTelemetry>>,
    takeovers: u64,
) {
    if let Some(telemetry) = telemetry.cloned() {
        tokio::spawn(async move {
            telemetry.keybinding_takeovers(takeovers).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::KeybindingsConfig;

    fn manager(bindings: &[(&str, &[&str])]) -> KeybindingsManager {
        let config: KeybindingsConfig = bindings
            .iter()
            .map(|(id, keys)| {
                (
                    id.to_string(),
                    keys.iter().map(ToString::to_string).collect(),
                )
            })
            .collect();
        KeybindingsManager::with_user_bindings(config)
    }

    /// One taken key names the user's action and the default's loser; several name their keys
    /// and point at `/hotkeys`; a loser with another key says which one it keeps.
    #[test]
    fn the_notice_names_what_each_binding_took() {
        let one = manager(&[("app.session.resume", &["f2"])]);
        let several = manager(&[
            ("app.session.resume", &["f2"]),
            ("app.session.fork", &["ctrl+n"]),
            ("app.session.new", &["?"]),
        ]);
        let list = manager(&[("tui.select.pageDown", &["ctrl+n"])]);
        assert_eq!(
            [
                takeover_notice(&one, one.get_takeovers()),
                takeover_notice(&several, several.get_takeovers()),
                takeover_notice(&KeybindingsManager::new(), &[]),
            ],
            [
                Some("Your keybindings.json takes a default key: F2 \u{2192} Resume a session (yours); Switch to the next recently used model has no key".to_string()),
                Some("Your keybindings.json takes 2 default keys (?, F2); /hotkeys lists what each replaced".to_string()),
                None,
            ]
        );
        assert_eq!(
            takeover_lines(&list, list.get_takeovers())
                .into_iter()
                .map(|line| line.text)
                .collect::<Vec<_>>(),
            vec![
                "Ctrl+N \u{2192} Selection page down (yours); Start a new session from the agents view has no key; Transcript search: next match keeps \u{2193}"
                    .to_string()
            ]
        );
    }
}
