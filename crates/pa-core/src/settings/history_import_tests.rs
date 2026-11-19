use super::*;
use std::collections::BTreeMap;

const FORK: StateLayout = StateLayout::Distribution { variant: "fork" };

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// Every file under `dir`, relative (`/`-joined), with its content.
fn tree(dir: &Path) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(dir).unwrap();
                let key = relative
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                files.insert(key, std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    files
}

/// The RLM child's session file under an agent dir, native separators (the
/// form a registry records).
fn child_path(agent_dir: &Path) -> PathBuf {
    ["session-artifacts", "s1", "rlm", "c1", "session.jsonl"]
        .iter()
        .fold(agent_dir.to_path_buf(), |path, part| path.join(part))
}

/// One RLM registry line naming `child` by absolute path.
fn registry_line(child: &Path) -> String {
    let entry = serde_json::json!({
        "childId": "c1",
        "sessionFile": child.display().to_string(),
        "status": "completed",
    });
    format!("{entry}\n")
}

fn pending(home: &Path, run: HistoryImportRun) -> HistoryImportPlan {
    match plan_official_history_import(FORK, home, &FORK.home_agent_dir(home), run).unwrap() {
        HistoryImportCheck::Pending(plan) => plan,
        other => panic!("expected a pending import, got {other:?}"),
    }
}

/// The official dir of a long-time user: chats in two folders (one still
/// being appended to), an RLM child and its registry naming the child by
/// absolute path, harness memory with its lock, configuration, and the
/// runtime state that must stay behind.
fn official_dir(home: &Path) -> PathBuf {
    let official = home.join(".prime/agent");
    let child = child_path(&official);
    let registry = registry_line(&child);
    for (name, body) in [
        (
            "sessions/--repo--/2026-01-01_s1.jsonl",
            "{\"id\":\"s1\"}\n{\"m\":1}\n",
        ),
        (
            "sessions/--repo--/2026-01-02_s2.jsonl",
            "{\"id\":\"s2\"}\n{\"m\":\"torn",
        ),
        (
            "sessions/--other--/2026-01-03_s3.jsonl",
            "{\"id\":\"s3\"}\n",
        ),
        (
            "sessions-archive/2025-12-01_old.jsonl",
            "{\"id\":\"old\"}\n",
        ),
        (
            "session-artifacts/s1/rlm/c1/session.jsonl",
            "{\"id\":\"c1\"}\n",
        ),
        (
            "session-artifacts/s1/rlm-subagents.jsonl",
            registry.as_str(),
        ),
        ("harness/harness_state.json", "{\"entries\":[]}"),
        ("harness/harness_state.json.lock/owner", "1 token"),
        ("settings.json", "{\"defaultModel\":\"official\"}"),
        ("models.json", "{\"providers\":{}}"),
        ("mcp-services.json", "{\"services\":[]}"),
        ("session-leases/s1.lock", "1"),
        ("daemon-workers/w.json", "{}"),
        ("persistent-shells/p.json", "{}"),
        ("logs/agent.jsonl", "{}\n"),
        ("kernel-venv/bin/python", ""),
        ("telemetry.json", "{\"version\":1}"),
    ] {
        write(&official.join(name), body);
    }
    official
}

/// A user who ran the first fork release already has the fork dir: its own
/// settings and a chat. The import merges the history in, rewrites the
/// registry's child path to the copy, cuts a torn line, fills in only the
/// missing configuration, never overwrites a fork file, leaves the official
/// dir untouched, and never runs twice.
#[test]
fn the_history_merges_into_an_existing_dir_once() {
    let home = tempfile::tempdir().unwrap();
    let official = official_dir(home.path());
    let before = tree(&official);
    let fork = FORK.home_agent_dir(home.path());
    write(&fork.join("settings.json"), "{\"defaultModel\":\"fork\"}");
    write(
        &fork.join("sessions/--repo--/2026-01-03_mine.jsonl"),
        "{\"id\":\"mine\"}\n",
    );
    write(
        &fork.join("sessions/--other--/2026-01-03_s3.jsonl"),
        "{\"id\":\"fork s3\"}\n",
    );

    let plan = pending(home.path(), HistoryImportRun::Once);
    assert_eq!(
        (plan.chats(), plan.config_entries().to_vec()),
        (4, vec!["models.json", "mcp-services.json"])
    );
    let mut visited = Vec::new();
    let report = plan
        .run(&mut |relative| visited.push(relative.to_path_buf()))
        .unwrap();

    assert_eq!(
        report,
        HistoryImportReport {
            chats_imported: 3,
            files_imported: 6,
            bytes_imported: report.bytes_imported,
            kept: vec![Path::new("sessions")
                .join("--other--")
                .join("2026-01-03_s3.jsonl")
                .to_string_lossy()
                .into_owned()],
            config_added: vec!["models.json".to_string(), "mcp-services.json".to_string()],
            config_skipped: Vec::new(),
        }
    );
    assert_eq!(visited.len(), 7);
    let child = child_path(&fork);
    let expected: BTreeMap<String, String> = [
        ("settings.json", "{\"defaultModel\":\"fork\"}".to_string()),
        ("models.json", "{\"providers\":{}}".to_string()),
        ("mcp-services.json", "{\"services\":[]}".to_string()),
        ("sessions/--repo--/2026-01-03_mine.jsonl", "{\"id\":\"mine\"}\n".to_string()),
        ("sessions/--other--/2026-01-03_s3.jsonl", "{\"id\":\"fork s3\"}\n".to_string()),
        ("sessions/--repo--/2026-01-01_s1.jsonl", "{\"id\":\"s1\"}\n{\"m\":1}\n".to_string()),
        ("sessions/--repo--/2026-01-02_s2.jsonl", "{\"id\":\"s2\"}\n".to_string()),
        ("sessions-archive/2025-12-01_old.jsonl", "{\"id\":\"old\"}\n".to_string()),
        ("session-artifacts/s1/rlm/c1/session.jsonl", "{\"id\":\"c1\"}\n".to_string()),
        ("session-artifacts/s1/rlm-subagents.jsonl", registry_line(&child)),
        ("harness/harness_state.json", "{\"entries\":[]}".to_string()),
        (
            ".imported-sessions-v1",
            "{\"version\":1,\"chatsImported\":3,\"filesImported\":6,\"kept\":1,\"configAdded\":[\"models.json\",\"mcp-services.json\"]}\n"
                .to_string(),
        ),
        (".imported-sessions-v1.lock", String::new()),
    ]
    .into_iter()
    .map(|(name, body)| (name.to_string(), body))
    .collect();
    assert_eq!(tree(&fork), expected);
    assert_eq!(tree(&official), before, "the official dir is untouched");
    // The resume list orders by modification time: the copy keeps the source's.
    let modified = |path: &Path| std::fs::metadata(path).unwrap().modified().unwrap();
    assert_eq!(
        modified(&fork.join("sessions/--repo--/2026-01-01_s1.jsonl")),
        modified(&official.join("sessions/--repo--/2026-01-01_s1.jsonl"))
    );

    // The marker blocks the automatic run, even after the official dir grows.
    write(
        &official.join("sessions/--repo--/2026-02-01_new.jsonl"),
        "{}\n",
    );
    assert!(matches!(
        plan_official_history_import(FORK, home.path(), &fork, HistoryImportRun::Once).unwrap(),
        HistoryImportCheck::AlreadyImported
    ));
    assert_eq!(tree(&fork), expected);
}

/// An import cut off mid-way (a crash, a kill) left some files published, a
/// partial temp file, and no marker: the next run finishes the import, keeps
/// what the first one published, and leaves no temp file behind.
#[test]
fn an_interrupted_import_resumes_cleanly() {
    let home = tempfile::tempdir().unwrap();
    let official = official_dir(home.path());
    let fork = FORK.home_agent_dir(home.path());
    let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut seen = 0;
        pending(home.path(), HistoryImportRun::Once).run(&mut |_| {
            seen += 1;
            assert!(seen < 3, "the import is cut off at its third file");
        })
    }));
    assert!(interrupted.is_err());
    assert!(!fork.join(HISTORY_IMPORT_MARKER).exists());
    write(
        &fork.join("sessions/--repo--/.2026-01-02_s2.jsonl.history-import"),
        "partial",
    );

    let report = pending(home.path(), HistoryImportRun::Once)
        .run(&mut |_| {})
        .unwrap();

    assert_eq!(
        (report.files_imported, report.kept.len()),
        (5, 2),
        "the second run copies the rest and keeps the first run's copies: {report:?}"
    );
    let is_json = |name: &str| Path::new(name).extension().is_some_and(|ext| ext == "json");
    let mut copied = tree(&fork);
    copied.retain(|name, _| !name.starts_with(".imported-sessions-v1") && !is_json(name));
    let mut source = tree(&official);
    source.retain(|name, _| {
        HISTORY_TREES
            .iter()
            .any(|tree| name.starts_with(&format!("{tree}/")))
            && !name.contains(".lock")
            && !is_json(name)
    });
    source.insert(
        "sessions/--repo--/2026-01-02_s2.jsonl".to_string(),
        "{\"id\":\"s2\"}\n".to_string(),
    );
    source.insert(
        "session-artifacts/s1/rlm-subagents.jsonl".to_string(),
        registry_line(&child_path(&fork)),
    );
    assert_eq!(copied, source);
}

/// The official build and an overridden dir never import; a home with no
/// history creates nothing; a held import makes a second one wait; `--skip`
/// writes the marker, and an `import-sessions` run goes again past it.
#[test]
fn only_a_distribution_builds_own_dir_imports_and_the_marker_gates_it() {
    let home = tempfile::tempdir().unwrap();
    official_dir(home.path());
    let fork = FORK.home_agent_dir(home.path());
    let empty = tempfile::tempdir().unwrap();
    let outcomes = [
        plan_official_history_import(
            StateLayout::Official,
            home.path(),
            &StateLayout::Official.home_agent_dir(home.path()),
            HistoryImportRun::Again,
        )
        .unwrap(),
        plan_official_history_import(
            FORK,
            home.path(),
            &home.path().join("elsewhere"),
            HistoryImportRun::Again,
        )
        .unwrap(),
        plan_official_history_import(
            FORK,
            empty.path(),
            &FORK.home_agent_dir(empty.path()),
            HistoryImportRun::Once,
        )
        .unwrap(),
    ];
    assert!(
        matches!(
            outcomes,
            [
                HistoryImportCheck::NotApplicable,
                HistoryImportCheck::NotApplicable,
                HistoryImportCheck::NothingToImport
            ]
        ),
        "{outcomes:?}"
    );
    assert!(!fork.exists() && !FORK.home_agent_dir(empty.path()).exists());

    let held = pending(home.path(), HistoryImportRun::Once);
    assert!(matches!(
        plan_official_history_import(FORK, home.path(), &fork, HistoryImportRun::Again).unwrap(),
        HistoryImportCheck::Busy
    ));
    drop(held);

    skip_official_history_import(&fork).unwrap();
    assert!(matches!(
        plan_official_history_import(FORK, home.path(), &fork, HistoryImportRun::Once).unwrap(),
        HistoryImportCheck::AlreadyImported
    ));
    assert!(!fork.join("sessions").exists(), "a skip copies nothing");
    let again = pending(home.path(), HistoryImportRun::Again);
    assert_eq!(again.chats(), 4);
}
