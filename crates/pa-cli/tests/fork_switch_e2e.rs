//! The fork distribution end to end: `scripts/prime-switch` installs the
//! official build and the Prime Agent (drvova build) side by side from a
//! fake loopback release (the official R2 layout and the fork's GitHub
//! Releases layout, both built with `scripts/release/fork_release.py`, the
//! fork workflow's own publication script, and installed by the real
//! `install-rust.sh`), switches between them both ways, stops the other
//! build's daemon through its own launcher, refuses a mid-turn daemon
//! without `--force`, and never touches the official install. The payloads
//! are stub scripts that log each call with the launcher's daemon-socket
//! and kernel-venv pins, so the pins are observed, not assumed.
#![cfg(unix)]
// too_many_lines: the switch walkthrough is one ordered scenario (each step
// depends on the state the previous one left), the sibling e2e files' style.
#![allow(clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest as _, Sha256};

const OFFICIAL_VERSION: &str = "0.10.4";
const FORK_VERSION: &str = "0.10.3-drvova.1";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .expect("worktree root")
}

fn python3() -> &'static str {
    [
        "/usr/bin/python3",
        "/usr/local/bin/python3",
        "/opt/homebrew/bin/python3",
    ]
    .into_iter()
    .find(|path| Path::new(path).is_file())
    .expect("a python3 for the installer and the release script")
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    std::fs::set_permissions(path, permissions).expect("chmod");
}

fn write_file(path: &Path, body: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
    std::fs::write(path, body).expect("write file");
}

fn assert_ok(output: &Output, what: &str) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{what} succeeds:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// Serve `root` over loopback HTTP for the rest of the test process: one
/// thread per connection, a 404 for anything missing.
fn serve_dir(root: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("local address");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let root = root.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    return;
                }
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                    line.clear();
                }
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().trim_start_matches('/');
                let file = root.join(path);
                let body = (!path.contains("..") && file.is_file())
                    .then(|| std::fs::read(&file).ok())
                    .flatten();
                let mut stream = stream;
                let (status, body) = match body {
                    Some(body) => ("200 OK", body),
                    None => ("404 Not Found", Vec::new()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                if method != "HEAD" {
                    let _ = stream.write_all(&body);
                }
            });
        }
    });
    format!("http://{address}")
}

/// The stub payload: it logs `version|args|socket pin|venv pin` and plays
/// the daemon commands the switch drives (`list` answers only while
/// `$HOME/daemon-<version>` exists; `busy-<version>` marks a mid-turn
/// session; `shutdown` stops it).
fn stub_payload(version: &str) -> String {
    format!(
        r#"#!/bin/sh
printf '%s|%s|%s|%s\n' '{version}' "$*" "${{PRIME_AGENT_DAEMON_SOCKET:-}}" "${{PRIME_AGENT_KERNEL_VENV:-}}" >> "$HOME/calls.log"
case "$1" in
  list)
    [ -f "$HOME/daemon-{version}" ] || {{ echo 'Error: no daemon' >&2; exit 1; }}
    if [ -f "$HOME/busy-{version}" ]; then echo '[{{"isStreaming": true}}]'; else echo '[]'; fi ;;
  shutdown) rm -f "$HOME/daemon-{version}"; echo '{{}}' ;;
  *) echo '{version}' ;;
esac
"#
    )
}

/// One release: the stub tarball, its SHA256SUMS, and the channel files
/// `fork_release.py channel` emits.
fn publish_release(scratch: &Path, channel_dir: &Path, assets_dir: &Path, version: &str) {
    let platform = pa_core::update::install::current_platform_alias();
    let payload = scratch.join(format!("payload-{version}"));
    write_file(
        &payload.join("prime-agent"),
        stub_payload(version).as_bytes(),
    );
    make_executable(&payload.join("prime-agent"));
    let file = format!("prime-agent-{version}-{platform}.tar.gz");
    std::fs::create_dir_all(assets_dir).expect("assets dir");
    let tarball = assets_dir.join(&file);
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(&payload)
        .arg("prime-agent")
        .status()
        .expect("run tar");
    assert!(status.success(), "tar the stub release");
    let sha = format!(
        "{:x}",
        Sha256::digest(std::fs::read(&tarball).expect("tarball"))
    );
    write_file(
        &assets_dir.join("SHA256SUMS"),
        format!("{sha}  {file}\n").as_bytes(),
    );
    let manifest = scratch.join(format!("manifest-{version}.json"));
    let document = serde_json::json!({
        "version": version,
        "binaries": [{"platform": platform, "file": file, "sha256": sha}],
    });
    write_file(&manifest, document.to_string().as_bytes());
    let output = Command::new(python3())
        .arg(repo_root().join("scripts/release/fork_release.py"))
        .args(["channel", "--manifest"])
        .arg(&manifest)
        .args(["--version", version, "--platforms", platform, "--out-dir"])
        .arg(channel_dir)
        .output()
        .expect("run fork_release.py channel");
    assert_ok(&output, "fork_release.py channel");
}

/// The sorted file names in `dir`.
fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    base: String,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("pa-fork-switch-")
            .tempdir()
            .expect("world root");
        let root = dir.path().canonicalize().expect("canonical root");
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".prime/agent/sessions")).expect("home");
        let www = root.join("www");
        let base = serve_dir(www.clone());
        // The official channel in its R2 layout, its installer rendered the
        // way release.yml's promote job renders install.sh.
        publish_release(
            &root,
            &www.join("official"),
            &www.join(format!("official/releases/v{OFFICIAL_VERSION}")),
            OFFICIAL_VERSION,
        );
        let installer =
            std::fs::read_to_string(repo_root().join("install-rust.sh")).expect("install-rust.sh");
        let official_installer = installer.replacen(
            "DOWNLOAD_BASE_URL_DEFAULT=\"https://pub-728493de92a943e2a9b2d17b4719f318.r2.dev\"",
            &format!("DOWNLOAD_BASE_URL_DEFAULT=\"{base}/official\""),
            1,
        );
        assert_ne!(official_installer, installer, "the official render applied");
        write_file(
            &www.join("official/install.sh"),
            official_installer.as_bytes(),
        );
        // The fork channel in the GitHub Releases layout, the installers
        // rendered by the fork workflow's own render step.
        let latest = www.join("gh/releases/latest/download");
        publish_release(
            &root,
            &latest,
            &www.join(format!("gh/releases/download/v{FORK_VERSION}")),
            FORK_VERSION,
        );
        let output = Command::new(python3())
            .arg(repo_root().join("scripts/release/fork_release.py"))
            .arg("render")
            .arg("--repo-root")
            .arg(repo_root())
            .args([
                "--download-base",
                &format!("{base}/gh/releases/latest/download"),
            ])
            .args(["--assets-url", &format!("{base}/gh/releases/download")])
            .args(["--home-prefix", ".local/share/prime-agent-fork"])
            .args(["--variant", "fork", "--command", "prime", "--out-dir"])
            .arg(&latest)
            // The Windows build job renders under the runner's non-UTF-8
            // locale (cp1252): render under a non-UTF-8 locale here too, so a
            // locale-dependent read of the UTF-8 installers fails on Linux.
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env("PYTHONUTF8", "0")
            .output()
            .expect("run fork_release.py render");
        assert_ok(&output, "fork_release.py render");
        // The installer's hermetic tools (the installer_update_e2e shims):
        // uv answers only `python find`, npm reports an empty global root.
        let shims = root.join("shims");
        write_file(
            &shims.join("uv"),
            format!(
                "#!/bin/sh\n[ \"$1 $2\" = \"python find\" ] && echo {} && exit 0\nexit 1\n",
                python3()
            )
            .as_bytes(),
        );
        write_file(
            &shims.join("npm"),
            b"#!/bin/sh\n[ \"$1\" = root ] && echo /nonexistent\nexit 0\n",
        );
        make_executable(&shims.join("uv"));
        make_executable(&shims.join("npm"));
        std::fs::create_dir_all(root.join("tmp")).expect("tmp");
        Self {
            _dir: dir,
            root,
            home,
            base,
        }
    }

    fn switch_bin(&self) -> PathBuf {
        self.home.join(".local/share/prime-switch/bin")
    }

    fn fork_prefix(&self) -> PathBuf {
        self.home.join(".local/share/prime-agent-fork")
    }

    fn run(&self, program: &Path, args: &[&str]) -> Output {
        Command::new(program)
            .args(args)
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:{}:/usr/bin:/bin",
                    self.switch_bin().display(),
                    self.root.join("shims").display()
                ),
            )
            .env("HOME", &self.home)
            .env("TMPDIR", self.root.join("tmp"))
            .env("PRIME_AGENT_ALLOW_HTTP", "1")
            .env(
                "PRIME_SWITCH_OFFICIAL_INSTALLER_URL",
                format!("{}/official/install.sh", self.base),
            )
            .env(
                "PRIME_SWITCH_FORK_RELEASES_URL",
                format!("{}/gh/releases/latest/download", self.base),
            )
            .current_dir(&self.root)
            .output()
            .expect("run")
    }

    fn switch(&self, args: &[&str]) -> Output {
        self.run(&repo_root().join("scripts/prime-switch"), args)
    }

    /// Every file of the official install, by relative path.
    fn official_tree(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        let mut pending = vec![self.home.join(".local/share/prime-agent")];
        files.insert(
            PathBuf::from("bin/prime-agent"),
            std::fs::read(self.home.join(".local/bin/prime-agent")).expect("official launcher"),
        );
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("read dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let relative = path.strip_prefix(&self.home).expect("under home");
                    files.insert(relative.to_path_buf(), std::fs::read(&path).expect("file"));
                }
            }
        }
        files
    }

    fn switch_bin_entries(&self) -> Vec<String> {
        dir_entries(&self.switch_bin())
    }

    /// The logged calls, `version|args|socket|venv` rows.
    fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.home.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.split('|').map(str::to_string).collect())
            .collect()
    }
}

#[test]
fn the_fork_installs_as_prime_beside_the_official_prime_agent() {
    let world = World::new();
    assert_ok(&world.switch(&["install", "official"]), "install official");
    let official = world.official_tree();

    // A pre-`prime` prime-switch left its `prime-agent` shim in the switch
    // dir: any command retires it, so `prime-agent` falls back to the
    // official build.
    let legacy_shim = world.switch_bin().join("prime-agent");
    write_file(
        &legacy_shim,
        b"#!/bin/sh\n# prime-agent - active-build shim written by prime-switch\n# build: fork\nexec '/nonexistent/bin/prime-agent' \"$@\"\n",
    );
    make_executable(&legacy_shim);

    let installed = assert_ok(&world.switch(&["install", "fork"]), "install fork");
    assert!(
        installed.contains(&format!(
            "changed: installed the fork build {FORK_VERSION} at {}",
            world.fork_prefix().display()
        )) && installed.contains(&format!(
            "changed: removed the old {} shim",
            legacy_shim.display()
        )),
        "{installed}"
    );
    assert_eq!(world.switch_bin_entries(), vec!["prime-switch"]);
    assert_eq!(
        world.official_tree(),
        official,
        "the fork install leaves the official tree"
    );
    // The fork's launcher is `prime`, never a second `prime-agent`.
    assert_eq!(dir_entries(&world.fork_prefix().join("bin")), vec!["prime"]);
    let fork_launcher =
        std::fs::read_to_string(world.fork_prefix().join("bin/prime")).expect("fork launcher");
    assert!(
        fork_launcher.contains("prime-agent-fork-$(id -u)/daemon.sock")
            && !fork_launcher.contains("prime-agent-rust-"),
        "the fork launcher pins its own daemon socket:\n{fork_launcher}"
    );
    // The free ~/.local/bin/prime slot gets the fork's public command.
    assert_eq!(
        dir_entries(&world.home.join(".local/bin")),
        vec!["prime", "prime-agent"]
    );
    let prime = world.home.join(".local/bin/prime");
    let version = assert_ok(&world.run(&prime, &["--version"]), "prime --version");
    assert_eq!(version.trim(), FORK_VERSION);
    let last = world.calls().pop().expect("the fork's call");
    let venv = world
        .fork_prefix()
        .join("kernel-venv")
        .display()
        .to_string();
    assert_eq!(
        (last[0].as_str(), last[1].as_str(), last[3].as_str()),
        (FORK_VERSION, "--version", venv.as_str())
    );
    assert!(
        last[2].contains("/prime-agent-fork-"),
        "fork socket: {last:?}"
    );
    let official_launcher = world.home.join(".local/bin/prime-agent");
    let version = assert_ok(
        &world.run(&official_launcher, &["--version"]),
        "prime-agent --version",
    );
    assert_eq!(version.trim(), OFFICIAL_VERSION);

    let status = assert_ok(&world.switch(&["status"]), "status");
    assert_eq!(
        status.lines().take(2).collect::<Vec<_>>(),
        vec![
            format!(
                "official: {OFFICIAL_VERSION}  prime-agent  {}",
                official_launcher.display()
            ),
            format!(
                "fork:     {FORK_VERSION}  prime  {}",
                world.fork_prefix().join("bin/prime").display()
            ),
        ]
    );
    // Nothing to switch: both commands answer at once.
    let switched = assert_ok(&world.switch(&["fork"]), "prime-switch fork");
    assert!(switched.contains("nothing to switch"), "{switched}");

    // update runs each installed build's own `update`.
    assert_ok(&world.switch(&["update"]), "update");
    let updates: Vec<_> = world
        .calls()
        .into_iter()
        .filter(|call| call[1] == "update")
        .map(|call| call[0].clone())
        .collect();
    assert_eq!(updates, vec![OFFICIAL_VERSION, FORK_VERSION]);

    // A mid-turn fork daemon refuses the uninstall unless --force; the stop
    // goes through the fork's own launcher (its socket pin).
    write_file(&world.home.join(format!("daemon-{FORK_VERSION}")), b"");
    write_file(&world.home.join(format!("busy-{FORK_VERSION}")), b"");
    let refused = world.switch(&["uninstall", "fork"]);
    assert!(
        !refused.status.success()
            && String::from_utf8_lossy(&refused.stderr).contains("sessions mid-turn"),
        "a mid-turn daemon refuses the uninstall: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(world.fork_prefix().join("bin/prime").is_file());
    let removed = assert_ok(
        &world.switch(&["uninstall", "fork", "--force"]),
        "uninstall fork",
    );
    assert!(
        removed.contains("changed: stopped the fork daemon")
            && removed.contains(&format!("changed: removed {}", prime.display()))
            && removed.contains("changed: removed the fork build"),
        "{removed}"
    );
    let shutdown = world
        .calls()
        .into_iter()
        .find(|call| call[1] == "shutdown --force --json")
        .expect("the fork daemon stop");
    assert!(
        shutdown[0] == FORK_VERSION && shutdown[2].contains("/prime-agent-fork-"),
        "{shutdown:?}"
    );
    assert!(!world.fork_prefix().exists(), "the fork prefix is gone");
    assert_eq!(
        dir_entries(&world.home.join(".local/bin")),
        vec!["prime-agent"]
    );
    assert_eq!(
        world.official_tree(),
        official,
        "the official install was never touched"
    );
}

/// Prime Intellect's own CLI is also `prime`, often in ~/.local/bin: the
/// fork install never replaces or removes a `prime` it did not write. Its
/// own `prime` stays in the fork prefix's bin dir, and the install says
/// which `prime` wins on PATH.
#[test]
fn a_foreign_prime_command_is_never_replaced() {
    let world = World::new();
    let foreign = world.home.join(".local/bin/prime");
    let foreign_bytes = b"#!/bin/sh\necho 'the prime CLI'\n";
    write_file(&foreign, foreign_bytes);
    make_executable(&foreign);

    let output = world.switch(&["install", "fork"]);
    assert_ok(&output, "install fork");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("~/.local/bin/prime already exists and was not written by this installer")
            && stderr.contains(&format!(
                "export PATH=\"{}:$PATH\"",
                world.fork_prefix().join("bin").display()
            )),
        "the install explains the collision:\n{stderr}"
    );
    assert_eq!(
        std::fs::read(&foreign).expect("foreign prime"),
        foreign_bytes
    );
    let fork_prime = world.fork_prefix().join("bin/prime");
    let version = assert_ok(&world.run(&fork_prime, &["--version"]), "prime --version");
    assert_eq!(version.trim(), FORK_VERSION);

    // A re-install (the update path) over a fork installed before it had
    // its own command retires that era's `prime-agent` launcher and still
    // leaves the foreign `prime` alone.
    let old_launcher = world.fork_prefix().join("bin/prime-agent");
    write_file(
        &old_launcher,
        b"#!/bin/sh\n# prime-agent \xe2\x80\x94 launcher written by install-rust.sh.\nexit 1\n",
    );
    make_executable(&old_launcher);
    let installer = world
        .root
        .join("www/gh/releases/latest/download/install.sh");
    assert_ok(
        &world.run(Path::new("/bin/sh"), &[installer.to_str().expect("utf-8")]),
        "re-run the fork installer",
    );
    assert_eq!(dir_entries(&world.fork_prefix().join("bin")), vec!["prime"]);

    let removed = assert_ok(&world.switch(&["uninstall", "fork"]), "uninstall fork");
    assert!(
        removed.contains("changed: removed the fork build"),
        "{removed}"
    );
    assert_eq!(
        std::fs::read(&foreign).expect("foreign prime"),
        foreign_bytes
    );
    assert_eq!(dir_entries(&world.home.join(".local/bin")), vec!["prime"]);
}

/// GitHub's tag-filter glob (`*` no slash, `+` one or more of the previous
/// character or class, `[..]` a class, the rest literal) as a regex.
fn tag_filter_regex(pattern: &str) -> String {
    let mut regex = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '*' => regex.push_str("[^/]*"),
            '+' => regex.push('+'),
            '[' => {
                regex.push('[');
                for class in chars.by_ref() {
                    regex.push(class);
                    if class == ']' {
                        break;
                    }
                }
            }
            other => regex.push_str(&regex_escape(other)),
        }
    }
    regex.push('$');
    regex
}

fn regex_escape(character: char) -> String {
    if character.is_ascii_alphanumeric() || character == '-' {
        character.to_string()
    } else {
        format!("\\{character}")
    }
}

fn workflow_tags(name: &str) -> Vec<String> {
    let path = repo_root().join(".github/workflows").join(name);
    let text = std::fs::read_to_string(&path).expect("read the workflow");
    let workflow: serde_yaml::Value = serde_yaml::from_str(&text).expect("the workflow parses");
    workflow["on"]["push"]["tags"]
        .as_sequence()
        .expect("push tag filters")
        .iter()
        .map(|tag| tag.as_str().expect("a tag pattern").to_string())
        .collect()
}

fn matches_any(filters: &[String], tag: &str) -> bool {
    filters.iter().any(|filter| {
        let pattern = tag_filter_regex(filter);
        python_regex_match(&pattern, tag)
    })
}

/// Evaluate the converted filter with python's `re` (no regex crate here).
fn python_regex_match(pattern: &str, tag: &str) -> bool {
    Command::new(python3())
        .args([
            "-c",
            "import re, sys; sys.exit(0 if re.fullmatch(sys.argv[1], sys.argv[2]) else 1)",
            pattern,
            tag,
        ])
        .status()
        .expect("run python3")
        .success()
}

/// The fork tag scheme and the official release filters never overlap: a
/// fork tag can never trigger release.yml (its R2 publish), and an
/// official tag synced into the fork never cuts a fork release.
#[test]
fn fork_tags_never_match_the_official_release_filters() {
    let official = workflow_tags("release.yml");
    let fork = workflow_tags("fork-release.yml");
    for tag in ["v0.10.3", "v0.10.4-beta.12"] {
        assert!(
            matches_any(&official, tag),
            "{tag} is an official release tag"
        );
        assert!(
            !matches_any(&fork, tag),
            "{tag} must not cut a fork release"
        );
    }
    for tag in ["v0.10.3-drvova.1", "v1.20.300-drvova.12"] {
        assert!(matches_any(&fork, tag), "{tag} cuts a fork release");
        assert!(
            !matches_any(&official, tag),
            "{tag} must not run release.yml"
        );
    }
}

/// The fork release cuts itself after a green `ci` run on a push to main,
/// and builds the commit that run tested (workflow_run.head_sha, carried as
/// tag-check's `sha`), never main's tip at the time the release starts.
#[test]
fn the_fork_workflow_releases_after_green_ci_on_main() {
    let path = repo_root().join(".github/workflows/fork-release.yml");
    let text = std::fs::read_to_string(&path).expect("read the workflow");
    let workflow: serde_yaml::Value = serde_yaml::from_str(&text).expect("the workflow parses");
    let trigger: serde_yaml::Value =
        serde_yaml::from_str("{workflows: [ci], types: [completed], branches: [main]}")
            .expect("the expected trigger parses");
    assert_eq!(workflow["on"]["workflow_run"], trigger);
    let guard = workflow["jobs"]["tag-check"]["if"]
        .as_str()
        .expect("tag-check guard");
    for condition in [
        "github.repository == 'drvova/prime-agent'",
        "github.event.workflow_run.conclusion == 'success'",
        "github.event.workflow_run.event == 'push'",
    ] {
        assert!(guard.contains(condition), "tag-check requires {condition}");
    }
    let mut checkouts = Vec::new();
    for job in ["catalog-ref", "build-gnu", "build-windows", "publish"] {
        for step in workflow["jobs"][job]["steps"]
            .as_sequence()
            .expect("job steps")
        {
            if step["uses"]
                .as_str()
                .is_some_and(|uses| uses.starts_with("actions/checkout@"))
            {
                checkouts.push((job, step["with"]["ref"].as_str().map(str::to_string)));
            }
        }
    }
    let sha = Some("${{ needs.tag-check.outputs.sha }}".to_string());
    assert_eq!(
        checkouts,
        vec![
            ("catalog-ref", sha.clone()),
            ("build-gnu", sha.clone()),
            ("build-windows", sha.clone()),
            ("publish", sha)
        ]
    );
}

/// A fork binary bundles install-rust.sh for `update --rollback` and
/// `--archive`: every build job renders the fork knobs into it BEFORE it
/// compiles, with the same distribution arguments as the published
/// install.sh, so a fork rollback never writes an unpinned `prime-agent`
/// launcher.
#[test]
fn the_fork_workflow_bundles_the_published_installer_render() {
    let path = repo_root().join(".github/workflows/fork-release.yml");
    let text = std::fs::read_to_string(&path).expect("read the workflow");
    let workflow: serde_yaml::Value = serde_yaml::from_str(&text).expect("the workflow parses");
    let distribution = [
        "--download-base \"${PRIME_AGENT_DIST_DOWNLOAD_BASE_URL}\"",
        "--assets-url \"${FORK_ASSETS_URL}\"",
        "--home-prefix \"${PRIME_AGENT_DIST_HOME_PREFIX}\"",
        "--variant \"${FORK_VARIANT}\"",
        "--command \"${PRIME_AGENT_DIST_COMMAND}\"",
    ];
    assert_eq!(
        workflow["env"]["PRIME_AGENT_DIST_COMMAND"].as_str(),
        Some("prime")
    );
    let mut renders = Vec::new();
    for job in ["build-gnu", "build-windows", "publish"] {
        let steps = workflow["jobs"][job]["steps"]
            .as_sequence()
            .expect("job steps");
        let runs: Vec<&str> = steps
            .iter()
            .map(|step| step["run"].as_str().unwrap_or_default())
            .collect();
        let render = runs
            .iter()
            .position(|run| run.contains("fork_release.py render"))
            .unwrap_or_else(|| panic!("{job} renders the installers"));
        let joined = runs[render].replace("\\\n", " ");
        let flattened = joined.split_whitespace().collect::<Vec<_>>().join(" ");
        renders.push((
            job,
            distribution
                .iter()
                .all(|argument| flattened.contains(argument)),
        ));
        if job != "publish" {
            let build = runs
                .iter()
                .position(|run| run.starts_with("cargo build --release"))
                .unwrap_or_else(|| panic!("{job} builds"));
            assert!(
                render < build && runs[render].contains("install-rust.sh"),
                "{job} bundles the rendered installer before it compiles"
            );
        }
    }
    assert_eq!(
        renders,
        vec![
            ("build-gnu", true),
            ("build-windows", true),
            ("publish", true)
        ]
    );
}

/// The shipped shell scripts parse (`sh -n`), and shellcheck passes where installed.
#[test]
fn the_switch_and_installer_scripts_parse() {
    for script in ["scripts/prime-switch", "install-rust.sh"] {
        let path = repo_root().join(script);
        let output = Command::new("sh")
            .arg("-n")
            .arg(&path)
            .output()
            .expect("sh -n");
        assert_ok(&output, &format!("sh -n {script}"));
        match Command::new("shellcheck")
            .arg("-S")
            .arg("error")
            .arg(&path)
            .output()
        {
            Ok(output) => {
                assert_ok(&output, &format!("shellcheck {script}"));
            }
            Err(error) => eprintln!("shellcheck not run for {script}: {error}"),
        }
    }
}
