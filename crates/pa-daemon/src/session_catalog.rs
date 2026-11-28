//! Saved-session catalog resolve: pa-core's `resolve_saved_session`, which
//! both daemons share; the supervisor uses it when a client command addresses
//! a session that is not resident. Its tests stay here, beside the session
//! file fixtures they write.

pub(crate) use pa_core::session::catalog::resolve_saved_session;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::{session_file_name, SessionFile};
    use std::path::{Path, PathBuf};

    fn temp_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        (tmp, dir)
    }

    fn write_session(dir: &Path, cwd: &str, name: Option<&str>) -> (String, PathBuf) {
        let mut session = SessionFile::create(cwd, None, 0);
        if let Some(name) = name {
            session.append_session_info(name);
        }
        session.append_message(&serde_json::json!({
            "role": "user", "content": "hi", "timestamp": 1u64
        }));
        let path = dir.join(session_file_name(session.session_id()));
        session.set_path(path.clone());
        session.rewrite().unwrap();
        (session.session_id().to_string(), path)
    }

    #[test]
    fn resolves_by_id_prefix_and_exact_name() {
        let (_tmp, dir) = temp_dir();
        let (id, _path) = write_session(&dir, "/work/a", Some("alpha"));
        let found = resolve_saved_session(&dir, &dir.join("archive"), &id[..8], "/work/a")
            .unwrap()
            .expect("id prefix resolves");
        assert_eq!(found.id, id);
        let found = resolve_saved_session(&dir, &dir.join("archive"), "alpha", "/work/a")
            .unwrap()
            .expect("name resolves");
        assert_eq!(found.id, id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_scope_wins_and_global_is_the_fallback() {
        let (_tmp, dir) = temp_dir();
        let (id, _path) = write_session(&dir, "/work/a", Some("alpha"));
        // A different cwd still resolves through the global pass.
        let found = resolve_saved_session(&dir, &dir.join("archive"), "alpha", "/work/other")
            .unwrap()
            .expect("global fallback");
        assert_eq!(found.id, id);
        // The local pass serves the session's own cwd.
        let found = resolve_saved_session(&dir, &dir.join("archive"), &id[..8], "/work/a")
            .unwrap()
            .expect("local pass");
        assert_eq!(found.cwd, "/work/a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ambiguous_selectors_carry_the_ts_error() {
        let (_tmp, dir) = temp_dir();
        write_session(&dir, "/work/a", Some("alpha"));
        write_session(&dir, "/work/a", Some("alpha"));
        let error =
            resolve_saved_session(&dir, &dir.join("archive"), "alpha", "/work/a").unwrap_err();
        assert_eq!(error.to_string(), "Ambiguous session selector \"alpha\"");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archived_roots_restore_by_id_prefix_or_name_and_resolve_again() {
        for selector_index in 0..3 {
            let root = tempfile::TempDir::new().unwrap();
            let sessions = root.path().join("sessions");
            let archive = root.path().join("sessions-archive");
            std::fs::create_dir_all(&archive).unwrap();
            let (id, path) = write_session(&archive, "/work/a", Some("alpha"));
            let bytes = pa_core::session::store::store().read(&path).unwrap();
            let modified = pa_core::session::store::store()
                .stat(&path)
                .unwrap()
                .modified
                .unwrap();
            let mut expected = pa_core::session::catalog::read_session_info(&path).unwrap();
            expected.path = sessions.join(session_file_name(&id));
            let selector = [id.as_str(), &id[..8], "alpha"][selector_index];
            // A global lookup must still recover a root created before upgrade.
            let restored = resolve_saved_session(&sessions, &archive, selector, "/other")
                .unwrap()
                .expect("archived selector resolves");
            assert_eq!(restored, expected);
            assert_eq!(
                pa_core::session::store::store()
                    .read(&restored.path)
                    .unwrap(),
                bytes
            );
            assert_eq!(
                pa_core::session::store::store()
                    .stat(&restored.path)
                    .unwrap()
                    .modified
                    .unwrap(),
                modified
            );
            assert!(
                !pa_core::session::store::store().exists(&path),
                "restored file leaves the archive"
            );
            let again = resolve_saved_session(&sessions, &archive, selector, "/other")
                .unwrap()
                .expect("restored session resolves again");
            assert_eq!(again.id, id);
            assert_eq!(again.path, restored.path);
            assert_eq!(
                pa_core::session::store::store().read(&again.path).unwrap(),
                bytes
            );
        }
    }

    #[test]
    fn a_restore_never_clobbers_an_unreadable_live_session() {
        let root = tempfile::TempDir::new().unwrap();
        let sessions = root.path().join("sessions");
        let archive = root.path().join("sessions-archive");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&archive).unwrap();
        let (id, path) = write_session(&archive, "/work/a", Some("alpha"));
        let bytes = pa_core::session::store::store().read(&path).unwrap();
        let live_path = sessions.join(session_file_name(&id));
        let live_bytes = b"unparseable live session bytes\n";
        std::fs::write(&live_path, live_bytes).unwrap();
        for _ in 0..2 {
            assert!(resolve_saved_session(&sessions, &archive, "alpha", "/work/a").is_err());
            assert_eq!(
                pa_core::session::store::store().read(&live_path).unwrap(),
                live_bytes
            );
            assert_eq!(pa_core::session::store::store().read(&path).unwrap(), bytes);
            let remaining: Vec<_> = std::fs::read_dir(&sessions)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(remaining, vec![live_path.clone()]);
        }
    }

    #[test]
    fn live_matches_win_over_archived_ones() {
        let root = tempfile::TempDir::new().unwrap();
        let sessions = root.path().join("sessions");
        let archive = root.path().join("sessions-archive");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&archive).unwrap();
        let (live_id, _) = write_session(&sessions, "/other", Some("alpha"));
        let (_, archived_path) = write_session(&archive, "/work/a", Some("alpha"));
        let bytes = pa_core::session::store::store()
            .read(&archived_path)
            .unwrap();
        let found = resolve_saved_session(&sessions, &archive, "alpha", "/work/a")
            .unwrap()
            .expect("live global match wins over archived local match");
        assert_eq!(found.id, live_id);
        assert_eq!(
            pa_core::session::store::store()
                .read(&archived_path)
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn archived_matches_keep_local_scope_and_ambiguity_rules() {
        let root = tempfile::TempDir::new().unwrap();
        let sessions = root.path().join("sessions");
        let archive = root.path().join("sessions-archive");
        std::fs::create_dir_all(&archive).unwrap();
        let (local_id, _) = write_session(&archive, "/work/a", Some("alpha"));
        let (_, other_path) = write_session(&archive, "/other", Some("alpha"));
        let error = resolve_saved_session(&sessions, &archive, "alpha", "/third").unwrap_err();
        assert_eq!(error.to_string(), "Ambiguous session selector \"alpha\"");
        let found = resolve_saved_session(&sessions, &archive, "alpha", "/work/a")
            .unwrap()
            .expect("local archived match wins");
        assert_eq!(found.id, local_id);
        assert!(
            pa_core::session::store::store().exists(&other_path),
            "unselected archives stay in place"
        );
    }

    #[test]
    fn a_miss_is_none_not_an_error() {
        let (_tmp, dir) = temp_dir();
        write_session(&dir, "/work/a", Some("alpha"));
        let found = resolve_saved_session(&dir, &dir.join("archive"), "ghost", "/work/a").unwrap();
        assert!(found.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
