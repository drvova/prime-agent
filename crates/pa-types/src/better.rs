//! `/better` vocabulary the TUI and the session share: the side-run kind the TUI asks the
//! daemon for, and how the user treated an inserted rewrite (the TUI judges it, the session
//! counts it).

use serde::{Deserialize, Serialize};

/// What one `start_side_question` run is: a `/btw` side question (the default an absent `kind`
/// means), or a `/better` prompt rewrite.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideRunKind {
    #[default]
    Question,
    Better,
}

/// What the user did with a rewrite `/better` put in the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerOutcome {
    /// Sent exactly as inserted.
    SentAsIs,
    /// Sent after changes that kept most of the rewrite.
    Edited,
    /// Never sent, or sent with most of the rewrite gone.
    Discarded,
}

/// Judge one inserted rewrite: `composer` is the whole composer text right after the insertion
/// (the rewrite above any draft), `submitted` what the user then sent (`None`: nothing was sent
/// before the rewrite left the composer). An edit keeps at least half of the rewrite's
/// non-blank lines; fewer means the user wrote their own prompt instead.
#[must_use]
pub fn classify_composer_outcome(
    rewrite: &str,
    composer: &str,
    submitted: Option<&str>,
) -> ComposerOutcome {
    let Some(submitted) = submitted else {
        return ComposerOutcome::Discarded;
    };
    if submitted.trim() == composer.trim() {
        return ComposerOutcome::SentAsIs;
    }
    let lines: Vec<&str> = rewrite
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let sent: std::collections::HashSet<&str> = submitted.lines().map(str::trim).collect();
    let kept = lines.iter().filter(|line| sent.contains(*line)).count();
    if !lines.is_empty() && kept * 2 >= lines.len() {
        ComposerOutcome::Edited
    } else {
        ComposerOutcome::Discarded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_follow_how_much_of_the_rewrite_was_sent() {
        let rewrite = "Fix `parse_expr` in `src/parser.rs`.\nRun `cargo test -p parser`.\nKeep the API.";
        let composer = format!("{rewrite}\n\nmy draft");
        let cases = [
            (Some(composer.as_str()), ComposerOutcome::SentAsIs),
            (
                Some("Fix `parse_expr` in `src/parser.rs`.\nRun `cargo test -p parser`.\nAlso the lexer."),
                ComposerOutcome::Edited,
            ),
            (Some("just fix it"), ComposerOutcome::Discarded),
            (None, ComposerOutcome::Discarded),
        ];
        assert_eq!(
            cases.map(|(submitted, _)| classify_composer_outcome(rewrite, &composer, submitted)),
            cases.map(|(_, expected)| expected)
        );
    }
}
