//! The old daemon itself (the `pa-daemon` supervisor binary built beside
//! `prime-agent`), which goes with the supervisor (phase 10, track C):
//!
//! - [`Bed::old_daemon`], which only a blessing of the frozen old-daemon
//!   answers starts ([`super::old_reference`]);
//! - ownership: with the old daemon started on the same agent dir after the
//!   runtime daemon (its boot reap spares the runtime daemon), a session
//!   either one holds is refused by the other (the session lease). Two
//!   daemons exist only while the supervisor does.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use super::{ok, refused, run_turn, support, Bed, Client, Daemon};
use pa_types::wire2::Since;

impl Bed {
    /// The old daemon (the `pa-daemon` supervisor binary built beside
    /// `prime-agent`), listening on `daemon.sock`.
    pub(crate) fn old_daemon(&self) -> Daemon {
        self.old_daemon_with(&[])
    }

    /// The old daemon with `env` over the bed's, listening.
    pub(crate) fn old_daemon_with(&self, env: &[(&str, &str)]) -> Daemon {
        let binary = Path::new(env!("CARGO_BIN_EXE_prime-agent"))
            .parent()
            .expect("the profile dir")
            .join("pa-daemon");
        assert!(
            binary.exists(),
            "pa-daemon not built at {}: run `cargo build -p pa-daemon` first",
            binary.display()
        );
        let mut command = Command::new(binary);
        self.env(&mut command, "old-daemon.log");
        command
            .arg("supervisor")
            .arg("--socket")
            .arg(self.old_socket())
            .arg("--agent-dir")
            .arg(self.agent_dir())
            .envs(env.iter().copied());
        let mut child = command.spawn().expect("spawn the old daemon");
        support::await_listening(&self.old_socket(), &mut child);
        Daemon(child)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_old_and_the_runtime_daemon_never_hold_one_session() {
    let bed = Bed::new();
    let script = bed.dir.path().join("old-script.json");
    std::fs::write(&script, json!({ "responses": ["old answer"] }).to_string()).unwrap();
    // The runtime daemon first: the old daemon's boot reap, which takes a
    // command line naming its socket for a predecessor, must spare it.
    let _runtime = bed.runtime_daemon(&json!({ "responses": ["runtime answer"] }));
    let _old = bed.old_daemon();

    // The old daemon holds a session: the runtime daemon cannot open it.
    let old_file = {
        let socket = bed.old_socket();
        let (cwd, session_dir, script) = (
            bed.dir.path().to_path_buf(),
            bed.agent_dir().join("sessions"),
            script.clone(),
        );
        tokio::task::spawn_blocking(move || {
            old_wire::create(&socket, "old", &cwd, &session_dir, &script, None)
        })
        .await
        .unwrap()
        .expect("the old daemon creates a session")
    };
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let refusal = refused(
        client
            .call("session.create", json!({ "path": old_file }))
            .await,
    );
    assert!(
        refusal.contains("already active"),
        "the lease refuses the runtime daemon: {refusal}"
    );

    // The runtime daemon holds a session: the old daemon cannot open it.
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    let runtime_file = PathBuf::from(created["path"].as_str().unwrap());
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "write the file").await;
    let old_refusal = {
        let socket = bed.old_socket();
        let (cwd, session_dir) = (
            bed.dir.path().to_path_buf(),
            bed.agent_dir().join("sessions"),
        );
        tokio::task::spawn_blocking(move || {
            old_wire::create(
                &socket,
                "reopen",
                &cwd,
                &session_dir,
                &script,
                Some(&runtime_file),
            )
        })
        .await
        .unwrap()
        .expect_err("the old daemon is refused")
    };
    assert!(
        old_refusal.contains("already active") || old_refusal.contains("session_already_active"),
        "the lease refuses the old daemon: {old_refusal}"
    );
}

/// The old daemon's line protocol, enough to create a session.
mod old_wire {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::{json, Value};

    /// Creates the session `name` on the scripted engine (opening
    /// `session_path` when given); its session file, or the refusal's text.
    pub(super) fn create(
        socket: &Path,
        name: &str,
        cwd: &Path,
        session_dir: &Path,
        script: &Path,
        session_path: Option<&Path>,
    ) -> Result<PathBuf, String> {
        let stream = UnixStream::connect(socket).expect("connect to the old daemon");
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let config = json!({
            "cwd": cwd,
            "sessionDir": session_dir,
            "script": script,
        });
        let mut command = json!({ "type": "create", "name": name, "config": config });
        if let Some(path) = session_path {
            command["sessionPath"] = json!(path);
        }
        let envelope = json!({
            "type": "command",
            "id": "create",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        writer
            .write_all(format!("{envelope}\n").as_bytes())
            .unwrap();
        let deadline = Instant::now() + Duration::from_mins(1);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => panic!("the old daemon closed the connection"),
                Ok(_) => {
                    let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                        continue;
                    };
                    if frame["id"] != "create" {
                        continue;
                    }
                    if frame["success"] != true {
                        return Err(frame.to_string());
                    }
                    let file = frame["data"]["sessionFile"]
                        .as_str()
                        .or_else(|| frame["data"]["session"]["sessionFile"].as_str())
                        .unwrap_or_else(|| panic!("no session file in {frame}"));
                    return Ok(PathBuf::from(file));
                }
                Err(_) => assert!(Instant::now() < deadline, "the old daemon never replied"),
            }
        }
    }
}
