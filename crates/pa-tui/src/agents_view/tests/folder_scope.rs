//! The resume picker's folder scope: a tree lists by its root session's folder, the toggle key
//! widens and narrows it, and the global view keeps every folder.

use super::*;

/// Two folders' sessions: root `a` lives in `/a` with a subagent whose own cwd is `/b`, root `b`
/// lives in `/b`, and the saved catalog holds one more session per folder.
fn two_folder_mode(folder_scope: Option<FolderScope>) -> AgentsViewMode {
    let mut mode = AgentsViewMode::new(AgentsViewOptions {
        socket_path: PathBuf::from("/tmp/agents-view-test.sock"),
        cwd: PathBuf::from("/a"),
        session_dir: None,
        theme: "prime".to_string(),
        version: "0.0.0".to_string(),
        anchor_session_id: None,
        scope: None,
        folder_scope,
        query: None,
        expanded_ancestors: Vec::new(),
        selected_row_identity: None,
        selected_key: None,
        status_message: None,
        keybindings: crate::keybindings::KeybindingsManager::new(),
        show_hardware_cursor: false,
        incident_notice_state: None,
        create_config: serde_json::json!({}),
    });
    let in_folder = |mut summary: serde_json::Value, cwd: &str| {
        summary["cwd"] = serde_json::json!(cwd);
        summary
    };
    mode.roster = vec![
        roster_entry("a", "idle", &in_folder(parent_summary("a"), "/a")),
        roster_entry(
            "c",
            "idle",
            &in_folder(child_summary("c", "a", "worker one"), "/b"),
        ),
        roster_entry("b", "idle", &in_folder(parent_summary("b"), "/b")),
    ];
    mode.saved = vec![
        in_folder(saved_catalog_row("/x/sa.jsonl", "sa", "saved in a"), "/a"),
        in_folder(saved_catalog_row("/x/sb.jsonl", "sb", "saved in b"), "/b"),
    ];
    mode.rebuild_rows();
    // Open every subagents line, so a listed child shows as its own row.
    let summaries: Vec<AgentsViewRow> = mode
        .rows
        .iter()
        .filter(|row| row.kind == RowKind::SubagentSummary)
        .cloned()
        .collect();
    for row in &summaries {
        mode.toggle_subagent_list(row);
    }
    mode
}

fn picker(folder: &str) -> Option<FolderScope> {
    Some(FolderScope {
        folder: PathBuf::from(folder),
        filter: FolderFilter::CurrentFolder,
    })
}

fn listed(mode: &AgentsViewMode) -> Vec<(RowKind, String)> {
    mode.rows
        .iter()
        .map(|row| (row.kind, row.title.clone()))
        .collect()
}

fn row(kind: RowKind, title: &str) -> (RowKind, String) {
    (kind, title.to_string())
}

/// `/resume` from folder A lists A's trees only (the subagent rides its root even though its own
/// cwd is B), the toggle shows both folders, and the global agents view shows both.
#[test]
fn the_resume_picker_lists_the_current_folder_and_the_toggle_widens_it() {
    let folder_a = vec![
        row(RowKind::Agent, "a name"),
        row(RowKind::SubagentSummary, "1 subagent \u{b7} 0 running"),
        row(RowKind::Subagent, "worker one"),
        row(RowKind::Agent, "saved in a"),
    ];
    let every_folder = vec![
        row(RowKind::Agent, "a name"),
        row(RowKind::SubagentSummary, "1 subagent \u{b7} 0 running"),
        row(RowKind::Subagent, "worker one"),
        row(RowKind::Agent, "b name"),
        row(RowKind::Agent, "saved in a"),
        row(RowKind::Agent, "saved in b"),
    ];
    let mut mode = two_folder_mode(picker("/a"));
    assert_eq!(listed(&mode), folder_a);
    assert_eq!(mode.folder_hidden, 2);

    mode.handle_key("tab");
    assert_eq!(listed(&mode), every_folder);
    assert_eq!(
        (mode.folder_scope.clone(), mode.folder_toggles),
        (
            Some(FolderScope {
                folder: PathBuf::from("/a"),
                filter: FolderFilter::AllFolders,
            }),
            1
        )
    );
    mode.handle_key("tab");
    assert_eq!(listed(&mode), folder_a);

    // The global agents view (no folder scope) lists every folder, and its tab does nothing.
    let mut global = two_folder_mode(None);
    assert_eq!(listed(&global), every_folder);
    global.handle_key("tab");
    assert_eq!(listed(&global), every_folder);
}

/// From folder B the subagent whose own cwd is B never lists without its root in A.
#[test]
fn a_subagent_follows_its_root_folder_not_its_own() {
    let mode = two_folder_mode(picker("/b"));
    assert_eq!(
        listed(&mode),
        vec![
            row(RowKind::Agent, "b name"),
            row(RowKind::Agent, "saved in b")
        ]
    );
}

/// The folder match tolerates a trailing separator, and a folder without sessions shows an empty
/// state that names the toggle instead of widening on its own.
#[test]
fn an_empty_folder_says_so_and_offers_the_toggle() {
    let mode = two_folder_mode(picker("/a/"));
    assert_eq!(listed(&mode).len(), 4);

    let mut empty = two_folder_mode(picker("/elsewhere"));
    assert_eq!(listed(&empty), Vec::new());
    let list = empty.render_list(80, 10, 0);
    assert_eq!(
        list.iter().map(flat).collect::<Vec<_>>(),
        vec!["No sessions in this folder yet. Press Tab to show all folders.".to_string()]
    );
}

/// A subtree scope entered from a folder-scoped list lists the scope root's subtree (the folder
/// filter waits for the unscoped list), and the toggle is inert there.
#[test]
fn a_subtree_scope_composes_with_the_folder_scope() {
    let mut mode = AgentsViewMode::new(AgentsViewOptions {
        scope: Some(AgentsViewScope {
            session_id: Some("p".to_string()),
            active_session_id: Some("p-live".to_string()),
            session_name: Some("p name".to_string()),
        }),
        ..two_folder_mode(picker("/elsewhere")).options
    });
    mode.roster = vec![
        roster_entry("p", "idle", &parent_summary("p")),
        roster_entry("c", "idle", &child_summary("c", "p", "worker one")),
    ];
    mode.rebuild_rows();
    assert_eq!(listed(&mode), vec![row(RowKind::Agent, "worker one")]);
    mode.handle_key("tab");
    assert_eq!(
        (listed(&mode), mode.folder_toggles),
        (vec![row(RowKind::Agent, "worker one")], 0)
    );
}

/// A narrow view cuts its scope line and search query with the shared mark: neither stops
/// mid-word as if it were the whole text (the section headings share the same cut).
#[test]
fn a_narrow_scope_line_and_query_end_in_the_cut_mark() {
    let mut mode = two_folder_mode(picker("/a"));
    mode.query = "saved in a long query that runs past the frame".to_string();
    let (frame, _) = mode.render_frame(18, 24);
    let rows: Vec<String> = frame
        .iter()
        .map(flat)
        .map(|row| row.trim_end().to_string())
        .filter(|row| row.starts_with("Sessions") || row.starts_with(" >"))
        .collect();
    assert_eq!(
        rows,
        [
            "Sessions \u{b7} / \u{b7} +2\u{2026}",
            " >  saved in a l\u{2026}",
        ]
    );
}
