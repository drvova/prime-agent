//! The status line for a `history_import` broadcast: a distribution build
//! copying the official build's chats into its own agent dir.

use std::fmt::Write as _;

/// The notice for one `history_import` frame: the start (what is coming, and
/// that `/resume` lists it once done), the finish (what landed, what was kept
/// as it was, which configuration entries were added), or the early stop.
pub(crate) fn history_import_notice(
    finished: bool,
    sessions: u64,
    kept: u64,
    config: &[String],
    error: Option<&str>,
) -> String {
    let chats = |count: u64| {
        if count == 1 {
            "1 chat".to_string()
        } else {
            format!("{count} chats")
        }
    };
    if let Some(error) = error {
        return format!(
            "The chat import from Prime Agent stopped ({error}); it picks up where it left off the next time the daemon starts."
        );
    }
    let mut notice = match (finished, sessions) {
        (true, 0) => "No new chats to import from Prime Agent.".to_string(),
        (true, _) => format!(
            "Imported {} from Prime Agent; /resume lists them.",
            chats(sessions)
        ),
        (false, 0) => "Importing from Prime Agent in the background.".to_string(),
        (false, _) => format!(
            "Importing {} from Prime Agent in the background; /resume lists them when it's done.",
            chats(sessions)
        ),
    };
    if finished && kept > 0 {
        let _ = write!(
            notice,
            " {kept} file(s) already here were kept as they are."
        );
    }
    if !config.is_empty() {
        let _ = write!(
            notice,
            " {} configuration: {}.",
            if finished { "Added" } else { "Also copying" },
            config.join(", ")
        );
    }
    notice
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_names_what_the_import_does() {
        let config = ["models.json".to_string(), "mcp-services.json".to_string()];
        assert_eq!(
            [
                history_import_notice(false, 12, 0, &config, None),
                history_import_notice(true, 1, 2, &config, None),
                history_import_notice(true, 0, 0, &[], Some("disk full")),
                history_import_notice(true, 0, 0, &config, None),
            ],
            [
                "Importing 12 chats from Prime Agent in the background; /resume lists them when it's done. Also copying configuration: models.json, mcp-services.json.",
                "Imported 1 chat from Prime Agent; /resume lists them. 2 file(s) already here were kept as they are. Added configuration: models.json, mcp-services.json.",
                "The chat import from Prime Agent stopped (disk full); it picks up where it left off the next time the daemon starts.",
                "No new chats to import from Prime Agent. Added configuration: models.json, mcp-services.json.",
            ]
        );
    }
}
