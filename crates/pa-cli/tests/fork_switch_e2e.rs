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
            .args(["--variant", "fork", "--out-dir"])
            .arg(&latest)
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
        let mut names: Vec<String> = std::fs::read_dir(self.switch_bin())
            .expect("switch bin")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
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
fn prime_switch_installs_and_switches_side_by_side_builds() {
    let world = World::new();
    assert_ok(&world.switch(&["install", "official"]), "install official");
    let official = world.official_tree();

    let installed = assert_ok(&world.switch(&["install", "fork"]), "install fork");
    assert!(
        installed.contains(&format!(
            "changed: installed the fork build {FORK_VERSION} at {}",
            world.fork_prefix().display()
        )),
        "{installed}"
    );
    assert_eq!(
        world.official_tree(),
        official,
        "the fork install leaves the official tree"
    );
    let fork_launcher = std::fs::read_to_string(world.fork_prefix().join("bin/prime-agent"))
        .expect("fork launcher");
    assert!(
        fork_launcher.contains("prime-agent-fork-$(id -u)/daemon.sock")
            && !fork_launcher.contains("prime-agent-rust-"),
        "the fork launcher pins its own daemon socket:\n{fork_launcher}"
    );

    let shim = world.switch_bin().join("prime-agent");
    let status = assert_ok(&world.switch(&["status"]), "status");
    assert_eq!(
        status.lines().take(3).collect::<Vec<_>>(),
        vec![
            format!("active:   none ({})", shim.display()),
            format!(
                "official: {OFFICIAL_VERSION}  {}",
                world.home.join(".local/bin/prime-agent").display()
            ),
            format!(
                "fork:     {FORK_VERSION}  {}",
                world.fork_prefix().join("bin/prime-agent").display()
            ),
        ]
    );

    // Switch to the fork while the official daemon runs: it is stopped
    // through the official launcher (its own socket pin), then the shim
    // runs the fork with the fork's socket and venv pins.
    write_file(&world.home.join(format!("daemon-{OFFICIAL_VERSION}")), b"");
    let switched = assert_ok(&world.switch(&["fork"]), "switch to fork");
    assert!(
        switched.contains("changed: stopped the official daemon"),
        "{switched}"
    );
    let shutdown: Vec<_> = world
        .calls()
        .into_iter()
        .filter(|call| call[1] == "shutdown --force --json")
        .collect();
    assert_eq!(shutdown.len(), 1, "one daemon stop: {shutdown:?}");
    assert_eq!(shutdown[0][0], OFFICIAL_VERSION);
    assert!(
        shutdown[0][2].contains("/prime-agent-rust-"),
        "{shutdown:?}"
    );
    let version = assert_ok(&world.run(&shim, &["--version"]), "the shim runs the fork");
    assert_eq!(version.trim(), FORK_VERSION);
    let last = world.calls().pop().expect("the shim's call");
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

    // Idempotent: a repeat changes nothing and leaves no stray files.
    let shim_bytes = std::fs::read(&shim).expect("shim");
    let again = assert_ok(&world.switch(&["fork"]), "repeat switch");
    assert!(
        again.starts_with("unchanged: fork is already active"),
        "{again}"
    );
    assert_eq!(std::fs::read(&shim).expect("shim"), shim_bytes);
    assert_eq!(
        world.switch_bin_entries(),
        vec!["prime-agent", "prime-switch"]
    );

    // A mid-turn fork daemon refuses the switch back unless --force.
    write_file(&world.home.join(format!("daemon-{FORK_VERSION}")), b"");
    write_file(&world.home.join(format!("busy-{FORK_VERSION}")), b"");
    let refused = world.switch(&["official"]);
    assert!(
        !refused.status.success(),
        "a mid-turn daemon refuses the switch"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("sessions mid-turn"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        std::fs::read(&shim).expect("shim"),
        shim_bytes,
        "the refusal changed nothing"
    );
    let forced = assert_ok(&world.switch(&["official", "--force"]), "forced switch");
    assert!(
        forced.contains("changed: stopped the fork daemon"),
        "{forced}"
    );
    let version = assert_ok(&world.run(&shim, &["--version"]), "the shim runs official");
    assert_eq!(version.trim(), OFFICIAL_VERSION);

    // update runs the active build's own `update`.
    assert_ok(&world.switch(&["update"]), "update");
    let update = world.calls().pop().expect("the update call");
    assert_eq!(
        (update[0].as_str(), update[1].as_str()),
        (OFFICIAL_VERSION, "update")
    );

    let removed = assert_ok(&world.switch(&["uninstall", "fork"]), "uninstall fork");
    assert!(
        removed.contains("changed: removed the fork build"),
        "{removed}"
    );
    assert!(!world.fork_prefix().exists(), "the fork prefix is gone");
    assert_eq!(
        world.switch_bin_entries(),
        vec!["prime-agent", "prime-switch"]
    );
    assert_eq!(
        world.official_tree(),
        official,
        "the official install was never touched"
    );
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
