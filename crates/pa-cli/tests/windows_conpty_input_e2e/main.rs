#![cfg_attr(
    windows,
    expect(
        clippy::too_many_lines,
        reason = "the pseudoconsole scenario is one linear script"
    )
)]
#![cfg(windows)]

//! The interactive TUI's keyboard on a Windows pseudoconsole. The test plays
//! the terminal emulator (`conpty.rs`) through two console hosts, the inbox
//! `ConPTY` and the Windows Terminal host package, each with the console's
//! native input mode and with `ENABLE_VIRTUAL_TERMINAL_INPUT` inherited from
//! a parent: keys act, VT the host relays is decoded or consumed, and no
//! escape text reaches the composer. A last test covers Git Bash's mintty
//! without `ConPTY`, whose MSYS pty stdin the TUI cannot read.

mod conpty;
mod console_windows;
#[path = "../support/mod.rs"]
mod support;

use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use conpty::{Host, PseudoConsole};
use serde_json::{json, Map};

/// Set on the launcher child: the daemon socket the launched TUI attaches to.
const VT_INPUT_LAUNCH_ENV: &str = "PA_CONPTY_VT_INPUT_LAUNCH";
/// `wincon.h` `ENABLE_VIRTUAL_TERMINAL_INPUT`.
const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

mod win32 {
    use std::ffi::c_void;
    pub type Handle = *mut c_void;
    /// `winbase.h` `STD_INPUT_HANDLE`.
    pub const STD_INPUT_HANDLE: i32 = -10;
    /// `winbase.h` `PIPE_ACCESS_DUPLEX`.
    pub const PIPE_ACCESS_DUPLEX: u32 = 0x3;
    /// `winnt.h` `GENERIC_READ | GENERIC_WRITE`.
    pub const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;
    /// `fileapi.h` `OPEN_EXISTING`.
    pub const OPEN_EXISTING: u32 = 3;
    pub const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    extern "system" {
        pub fn GetStdHandle(which: i32) -> Handle;
        pub fn GetConsoleMode(handle: Handle, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(handle: Handle, mode: u32) -> i32;
        pub fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_size: u32,
            in_size: u32,
            timeout: u32,
            attributes: *const c_void,
        ) -> Handle;
        pub fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            attributes: *const c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
    }
}

/// The launcher child: a parent that leaves `ENABLE_VIRTUAL_TERMINAL_INPUT`
/// on in the console it shares (PowerShell, Node CLIs and MSYS programs can),
/// then runs the TUI in that console, as a shell would.
#[test]
fn conpty_vt_input_launcher_child_mode() {
    let Ok(socket) = std::env::var(VT_INPUT_LAUNCH_ENV) else {
        return;
    };
    // SAFETY: a mode read and write on this process's own console input.
    unsafe {
        let input = win32::GetStdHandle(win32::STD_INPUT_HANDLE);
        let mut mode = 0u32;
        assert_ne!(
            win32::GetConsoleMode(input, &raw mut mode),
            0,
            "console input mode"
        );
        assert_ne!(
            win32::SetConsoleMode(input, mode | ENABLE_VIRTUAL_TERMINAL_INPUT),
            0,
            "set VT input"
        );
    }
    let status = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
        .arg("--daemon-socket")
        .arg(socket)
        .env_remove(VT_INPUT_LAUNCH_ENV)
        .status()
        .expect("run prime-agent");
    std::process::exit(status.code().unwrap_or(1));
}

fn hosts() -> Vec<Host> {
    let mut hosts = vec![Host::Inbox];
    if let Ok(dll) = std::env::var("PA_CONPTY_DLL") {
        hosts.push(Host::Package(PathBuf::from(dll)));
    }
    hosts
}

/// The console input mode the TUI starts under.
#[derive(Clone, Copy)]
enum InputMode {
    /// The console's default: native key records.
    Native,
    /// `ENABLE_VIRTUAL_TERMINAL_INPUT` left on by the parent process.
    VtInputInherited,
}

impl InputMode {
    fn label(self) -> &'static str {
        match self {
            InputMode::Native => "native",
            InputMode::VtInputInherited => "vt-input-inherited",
        }
    }
}

fn base_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| !key.starts_with("PRIME_AGENT_") && key != "RUST_LOG")
        .collect()
}

/// The raw stream as a readable escape transcript.
fn visible(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|ch| match ch {
            '\x1b' => "\\e".to_string(),
            '\r' => "\\r".to_string(),
            '\n' => "\\n\n".to_string(),
            c if c.is_control() => format!("\\x{:02x}", c as u32),
            c => c.to_string(),
        })
        .collect()
}

fn dump(name: &str, text: &str) {
    eprintln!("===== {name} =====\n{text}\n===== end {name} =====");
    if let Ok(dir) = std::env::var("WINDOWS_RUNTIME_TRIAGE_DUMP_DIR") {
        let dir = PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(format!("{name}.txt")), text);
    }
}

/// win32-input-mode encoding of one key transition (`CSI Vk;Sc;Uc;Kd;Cs;Rc _`).
fn win32_key(vk: u16, scan: u16, uchar: u16, down: bool, control: u32) -> Vec<u8> {
    format!("\x1b[{vk};{scan};{uchar};{};{control};1_", u8::from(down)).into_bytes()
}

// ---------------------------------------------------------------------------
// The product under the pseudoconsole.

struct Supervisor {
    child: Child,
    socket: PathBuf,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        shutdown_daemon(&self.socket);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Ask the daemon on `socket` to shut down (bounded; a gone daemon is fine).
fn shutdown_daemon(socket: &Path) {
    let socket = socket.to_path_buf();
    let shutdown = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("shutdown runtime");
        runtime.block_on(async move {
            let Ok((client, events)) = pa_tui::daemon_client::DaemonClient::connect(&socket).await
            else {
                return;
            };
            let shutdown = pa_types::daemon::DaemonCommand::Shutdown {
                id: None,
                force: Some(true),
                rest: Map::default(),
            };
            let _ = client.request_with_timeout(shutdown, 1_500).await;
            client.close();
            drop(events);
        });
    });
    let _ = shutdown.join();
}

fn spawn_supervisor(agent_dir: &Path) -> Supervisor {
    support::scrub_ambient_provider_credentials();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or_default(|stamp| stamp.as_nanos());
    let socket = PathBuf::from(format!(
        r"\\.\pipe\prime-agent-conpty-{}-{unique}",
        std::process::id()
    ));
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        // The supervisor (selected by name: the runtime daemon is the default), which this
        // suite drives on the old wire.
        .env("PRIME_AGENT_RUNTIME_DAEMON", "0")
        .arg(&socket)
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("PI_OFFLINE", "1")
        .env("PA_DAEMON_WORKER_CONNECT_TIMEOUT_MS", "90000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for var in [
        pa_daemon::worker::WORKER_ROLE_ENV,
        pa_daemon::worker::WORKER_TOKEN_ENV,
        pa_daemon::worker::WORKER_ACTIVE_SESSION_ID_ENV,
        pa_daemon::worker::WORKER_RECOVERY_JOURNAL_ENV,
        pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV,
        pa_daemon::worker::WORKER_SOCKET_ENV,
        pa_daemon::worker::WORKER_INSTANCE_ID_ENV,
        pa_daemon::worker::WORKER_SCRIPT_ENV,
    ] {
        command.env_remove(var);
    }
    let child = command.spawn().expect("spawn prime-agent --mode daemon");
    let supervisor = Supervisor { child, socket };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("readiness runtime");
    runtime.block_on(async {
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if let Ok((client, events)) =
                pa_tui::daemon_client::DaemonClient::connect(&supervisor.socket).await
            {
                client.close();
                drop(events);
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the supervisor never answered a client connect");
    });
    supervisor
}

/// The rendered screen: the console host repaints with cursor moves, so
/// needles are matched against the grid, never the raw stream.
struct Screen {
    rows: usize,
    cols: usize,
    grid: Vec<Vec<char>>,
    row: usize,
    col: usize,
    pending: Vec<u8>,
}

impl Screen {
    fn new(cols: usize, rows: usize) -> Screen {
        Screen {
            rows,
            cols,
            grid: vec![vec![' '; cols]; rows],
            row: 0,
            col: 0,
            pending: Vec::new(),
        }
    }

    fn put(&mut self, ch: char) {
        if self.col >= self.cols {
            self.col = 0;
            self.row = (self.row + 1).min(self.rows - 1);
        }
        self.grid[self.row][self.col] = ch;
        self.col += 1;
    }

    fn feed(&mut self, data: &[u8]) {
        self.pending.extend_from_slice(data);
        let bytes = std::mem::take(&mut self.pending);
        let mut i = 0;
        while i < bytes.len() {
            let byte = bytes[i];
            if byte == 0x1b {
                match bytes.get(i + 1) {
                    Some(b'[') => {
                        let mut j = i + 2;
                        while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                            j += 1;
                        }
                        if j >= bytes.len() {
                            self.pending = bytes[i..].to_vec();
                            return;
                        }
                        let params = String::from_utf8_lossy(&bytes[i + 2..j]).to_string();
                        self.csi(&params, bytes[j]);
                        i = j + 1;
                    }
                    Some(b']' | b'P' | b'_') => {
                        let mut j = i + 2;
                        let mut end = None;
                        while j < bytes.len() {
                            if bytes[j] == 0x07 {
                                end = Some(j + 1);
                                break;
                            }
                            if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                                end = Some(j + 2);
                                break;
                            }
                            j += 1;
                        }
                        if let Some(end) = end {
                            i = end;
                        } else {
                            self.pending = bytes[i..].to_vec();
                            return;
                        }
                    }
                    Some(_) => i += 2,
                    None => {
                        self.pending = bytes[i..].to_vec();
                        return;
                    }
                }
                continue;
            }
            match byte {
                b'\n' => {
                    self.row = (self.row + 1).min(self.rows - 1);
                    i += 1;
                }
                b'\r' => {
                    self.col = 0;
                    i += 1;
                }
                0x08 => {
                    self.col = self.col.saturating_sub(1);
                    i += 1;
                }
                byte if byte < 0x20 => i += 1,
                byte if byte < 0x80 => {
                    self.put(byte as char);
                    i += 1;
                }
                _ => {
                    let len = if byte < 0xe0 {
                        2
                    } else if byte < 0xf0 {
                        3
                    } else {
                        4
                    };
                    if i + len > bytes.len() {
                        self.pending = bytes[i..].to_vec();
                        return;
                    }
                    if let Some(ch) = std::str::from_utf8(&bytes[i..i + len])
                        .ok()
                        .and_then(|text| text.chars().next())
                    {
                        self.put(ch);
                    }
                    i += len;
                }
            }
        }
    }

    fn csi(&mut self, params: &str, final_byte: u8) {
        if params.starts_with(['?', '>', '<', '=']) {
            return;
        }
        let number = |index: usize, default: usize| -> usize {
            params
                .split(';')
                .nth(index)
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| *value != 0)
                .unwrap_or(default)
        };
        match final_byte {
            b'H' | b'f' => {
                self.row = number(0, 1).saturating_sub(1).min(self.rows - 1);
                self.col = number(1, 1).saturating_sub(1).min(self.cols - 1);
            }
            b'A' => self.row = self.row.saturating_sub(number(0, 1)),
            b'B' => self.row = (self.row + number(0, 1)).min(self.rows - 1),
            b'C' => self.col = (self.col + number(0, 1)).min(self.cols - 1),
            b'D' => self.col = self.col.saturating_sub(number(0, 1)),
            b'G' => self.col = number(0, 1).saturating_sub(1).min(self.cols - 1),
            b'd' => self.row = number(0, 1).saturating_sub(1).min(self.rows - 1),
            b'X' => {
                let count = number(0, 1);
                for col in self.col..(self.col + count).min(self.cols) {
                    self.grid[self.row][col] = ' ';
                }
            }
            b'K' => {
                let mode = params.parse::<usize>().unwrap_or(0);
                let range = match mode {
                    1 => 0..(self.col + 1).min(self.cols),
                    2 => 0..self.cols,
                    _ => self.col.min(self.cols)..self.cols,
                };
                for col in range {
                    self.grid[self.row][col] = ' ';
                }
            }
            b'J' => {
                let mode = params.parse::<usize>().unwrap_or(0);
                match mode {
                    2 | 3 => self.grid = vec![vec![' '; self.cols]; self.rows],
                    _ => {
                        for col in self.col.min(self.cols)..self.cols {
                            self.grid[self.row][col] = ' ';
                        }
                        for row in self.row + 1..self.rows {
                            self.grid[row] = vec![' '; self.cols];
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn text(&self) -> String {
        self.grid
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The emulated terminal: tracks the modes the host asked for and answers
/// device queries the way a real terminal does.
struct Terminal {
    pty: PseudoConsole,
    screen: Screen,
    stream: Vec<u8>,
    scanned: usize,
    win32_input: bool,
    bracketed_paste: bool,
    kitty_depth: usize,
    queries_answered: Vec<String>,
}

impl Terminal {
    fn new(pty: PseudoConsole, cols: usize, rows: usize) -> Terminal {
        Terminal {
            pty,
            screen: Screen::new(cols, rows),
            stream: Vec::new(),
            scanned: 0,
            win32_input: false,
            bracketed_paste: false,
            kitty_depth: 0,
            queries_answered: Vec::new(),
        }
    }

    fn pump(&mut self, wait: Duration) {
        if let Some(chunk) = self.pty.recv(wait) {
            self.stream.extend_from_slice(&chunk);
            self.screen.feed(&chunk);
            self.scan_modes();
        }
    }

    fn scan_modes(&mut self) {
        let text = String::from_utf8_lossy(&self.stream[self.scanned..]).to_string();
        let mut consumed = 0;
        let mut rest = text.as_str();
        while let Some(at) = rest.find("\x1b[") {
            let tail = &rest[at + 2..];
            let Some(end) = tail.find(|c: char| ('\x40'..='\x7e').contains(&c)) else {
                break;
            };
            let params = &tail[..end];
            let final_char = tail[end..].chars().next().unwrap_or(' ');
            match (params, final_char) {
                ("?9001", 'h') => self.win32_input = true,
                ("?9001", 'l') => self.win32_input = false,
                ("?2004", 'h') => self.bracketed_paste = true,
                ("?2004", 'l') => self.bracketed_paste = false,
                ("" | "0", 'c') => {
                    self.queries_answered.push("DA1".to_string());
                    self.pty.send(b"\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c");
                }
                ("6", 'n') => {
                    self.queries_answered.push("CPR".to_string());
                    let reply = format!("\x1b[{};{}R", self.screen.row + 1, self.screen.col + 1);
                    self.pty.send(reply.as_bytes());
                }
                (flags, 'u') if flags.starts_with('>') => self.kitty_depth += 1,
                (_, 'u') if params.starts_with('<') => {
                    self.kitty_depth = self.kitty_depth.saturating_sub(1);
                }
                _ => {}
            }
            let advance = at + 2 + end + final_char.len_utf8();
            consumed += advance;
            rest = &rest[advance..];
        }
        self.scanned += consumed.min(self.stream.len() - self.scanned);
    }

    fn wait_for(&mut self, needle: &str, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            if self.screen.text().contains(needle) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            self.pump(Duration::from_millis(50));
        }
    }

    fn wait_gone(&mut self, needle: &str, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            if !self.screen.text().contains(needle) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            self.pump(Duration::from_millis(50));
        }
    }

    fn settle(&mut self, quiet: Duration) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.pty.recv(quiet) {
                Some(chunk) => {
                    self.stream.extend_from_slice(&chunk);
                    self.screen.feed(&chunk);
                    self.scan_modes();
                }
                None => return,
            }
        }
    }

    fn key(&mut self, key: Key) {
        let bytes = if self.win32_input {
            key.win32()
        } else {
            key.vt()
        };
        self.pty.send(&bytes);
    }

    fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.key(Key::Char(ch));
        }
    }

    fn paste(&mut self, text: &str) {
        if self.bracketed_paste {
            self.pty
                .send(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.pty.send(text.as_bytes());
        }
    }
}

#[derive(Clone, Copy)]
enum Key {
    Char(char),
    Left,
    ShiftEnter,
    Enter,
    Escape,
    CtrlC,
    CtrlV,
}

impl Key {
    fn vt(self) -> Vec<u8> {
        match self {
            Key::Char(ch) => ch.to_string().into_bytes(),
            Key::Left => b"\x1b[D".to_vec(),
            // A legacy VT terminal has no distinct Shift+Enter; the kitty
            // form is what an enhanced terminal sends.
            Key::ShiftEnter => b"\x1b[13;2u".to_vec(),
            Key::Enter => b"\r".to_vec(),
            Key::Escape => b"\x1b".to_vec(),
            Key::CtrlC => b"\x03".to_vec(),
            Key::CtrlV => b"\x16".to_vec(),
        }
    }

    fn win32(self) -> Vec<u8> {
        const SHIFT: u32 = 0x10;
        const LEFT_CTRL: u32 = 0x08;
        const ENHANCED: u32 = 0x100;
        match self {
            Key::Char(ch) => {
                let upper = ch.to_ascii_uppercase();
                let vk = upper as u16;
                let shift = if ch.is_ascii_uppercase() { SHIFT } else { 0 };
                let unit = ch as u16;
                let mut out = Vec::new();
                if shift != 0 {
                    out.extend(win32_key(0x10, 0x2a, 0, true, SHIFT));
                }
                out.extend(win32_key(vk, 0, unit, true, shift));
                out.extend(win32_key(vk, 0, unit, false, shift));
                if shift != 0 {
                    out.extend(win32_key(0x10, 0x2a, 0, false, 0));
                }
                out
            }
            Key::Left => [
                win32_key(0x25, 0x4b, 0, true, ENHANCED),
                win32_key(0x25, 0x4b, 0, false, ENHANCED),
            ]
            .concat(),
            Key::ShiftEnter => [
                win32_key(0x10, 0x2a, 0, true, SHIFT),
                win32_key(0x0d, 0x1c, 0x0d, true, SHIFT),
                win32_key(0x0d, 0x1c, 0x0d, false, SHIFT),
                win32_key(0x10, 0x2a, 0, false, 0),
            ]
            .concat(),
            Key::Enter => [
                win32_key(0x0d, 0x1c, 0x0d, true, 0),
                win32_key(0x0d, 0x1c, 0x0d, false, 0),
            ]
            .concat(),
            Key::Escape => [
                win32_key(0x1b, 0x01, 0x1b, true, 0),
                win32_key(0x1b, 0x01, 0x1b, false, 0),
            ]
            .concat(),
            Key::CtrlC => [
                win32_key(0x11, 0x1d, 0, true, LEFT_CTRL),
                win32_key(0x43, 0x2e, 0x03, true, LEFT_CTRL),
                win32_key(0x43, 0x2e, 0x03, false, LEFT_CTRL),
                win32_key(0x11, 0x1d, 0, false, 0),
            ]
            .concat(),
            Key::CtrlV => [
                win32_key(0x11, 0x1d, 0, true, LEFT_CTRL),
                win32_key(0x56, 0x2f, 0x16, true, LEFT_CTRL),
                win32_key(0x56, 0x2f, 0x16, false, LEFT_CTRL),
                win32_key(0x11, 0x1d, 0, false, 0),
            ]
            .concat(),
        }
    }
}

/// What a leaked VT sequence looks like once its ESC is gone: the arrow, the
/// CSI-u and modifyOtherKeys keys, paste markers, the win32-input-mode tail,
/// mouse reports and device-attribute replies. The chrome draws `←` itself
/// (`← manage`), so the arrow glyph is no evidence of a leak.
const ESCAPE_TEXT: &[&str] = &[
    "[D", "[?", "[<", "200~", "201~", ";2u", "13;2", ";1_", ";0_", "[27;",
];

#[test]
fn prime_tui_keys_act_on_conpty() {
    let mut failures = Vec::new();
    for host in hosts() {
        for input_mode in [InputMode::Native, InputMode::VtInputInherited] {
            let scenario = format!("{}-{}", host.label(), input_mode.label());
            failures.extend(run_tui_scenario(&host, input_mode, &scenario));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// One TUI run on `host` under `input_mode`; returns the failed checks.
fn run_tui_scenario(host: &Host, input_mode: InputMode, scenario: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let dir = tempfile::TempDir::new().expect("temp dir");
    let root = dir.path().to_path_buf();
    let agent_dir = root.join("agent");
    let session_dir = agent_dir.join("sessions");
    let cwd = root.join("project");
    std::fs::create_dir_all(&session_dir).expect("sessions dir");
    std::fs::create_dir_all(&cwd).expect("project dir");
    pa_core::settings::SettingsManager::create(&cwd, &agent_dir)
        .set_onboarding_shown(true)
        .expect("onboarding flag");
    let script = root.join("script.json");
    std::fs::write(
        &script,
        json!({ "responses": ["conpty scripted reply"] }).to_string(),
    )
    .expect("script");
    let supervisor = spawn_supervisor(&agent_dir);
    let mut env = base_env();
    for (key, value) in [
        (
            "PRIME_AGENT_CODING_AGENT_DIR",
            agent_dir.display().to_string(),
        ),
        ("PRIME_AGENT_SESSION_DIR", session_dir.display().to_string()),
        ("PRIME_AGENT_FAUX_SCRIPT", script.display().to_string()),
        ("PI_OFFLINE", "1".to_string()),
        ("DO_NOT_TRACK", "1".to_string()),
        // The TUI talks to the supervisor this test spawned, on the old wire.
        ("PRIME_AGENT_RUNTIME_DAEMON", "0".to_string()),
    ] {
        env.push((key.to_string(), value));
    }
    let command = match input_mode {
        InputMode::Native => format!(
            "\"{}\" --daemon-socket \"{}\"",
            env!("CARGO_BIN_EXE_prime-agent"),
            supervisor.socket.display()
        ),
        InputMode::VtInputInherited => {
            env.push((
                VT_INPUT_LAUNCH_ENV.to_string(),
                supervisor.socket.display().to_string(),
            ));
            format!(
                "\"{}\" --exact conpty_vt_input_launcher_child_mode --nocapture --test-threads=1",
                std::env::current_exe().expect("test binary").display()
            )
        }
    };
    let pty = PseudoConsole::spawn(host, &command, &env, &cwd, 100, 30);
    let mut term = Terminal::new(pty, 100, 30);
    let mut log = Vec::new();
    let mut check = |term: &mut Terminal, label: &str, ok: bool| {
        log.push(format!("{label}: {}", if ok { "ok" } else { "FAILED" }));
        if !ok {
            failures.push(format!("[{scenario}] {label}\n{}", term.screen.text()));
        }
    };
    let started = term.wait_for("prime", Duration::from_secs(60));
    check(&mut term, "startup chrome", started);
    term.settle(Duration::from_millis(500));
    term.type_text("qzx");
    let typed = term.wait_for("qzx", Duration::from_secs(30));
    check(&mut term, "typing echoes", typed);
    term.key(Key::Left);
    term.key(Key::Left);
    term.type_text("K");
    let moved = term.wait_for("qKzx", Duration::from_secs(10));
    check(&mut term, "left arrow moves the caret", moved);
    term.key(Key::ShiftEnter);
    term.type_text("yy");
    let newline = term.wait_for("yy", Duration::from_secs(10));
    check(&mut term, "shift+enter then text", newline);
    // A terminal without win32-input-mode sends Shift+Enter as CSI-u; the
    // package host relays what it cannot translate one vk=0 record per
    // character (the inbox host drops it). Either way it must not type.
    term.pty.send(b"\x1b[13;2u");
    term.type_text("ww");
    let relayed = term.wait_for("ww", Duration::from_secs(10));
    check(&mut term, "a relayed CSI-u key then text", relayed);
    // The session enables SGR mouse tracking; a hover report is decoded or
    // consumed, never typed.
    term.pty.send(b"\x1b[<35;5;5M");
    term.type_text("mm");
    let hovered = term.wait_for("mm", Duration::from_secs(10));
    check(&mut term, "a mouse report then text", hovered);
    term.paste("pa\r\nste");
    let pasted = term.wait_for("ste", Duration::from_secs(10));
    check(&mut term, "paste lands", pasted);
    term.settle(Duration::from_millis(500));
    let screen = term.screen.text();
    let leaked: Vec<&str> = ESCAPE_TEXT
        .iter()
        .copied()
        .filter(|needle| screen.contains(needle))
        .collect();
    check(
        &mut term,
        &format!("no escape text ({leaked:?})"),
        leaked.is_empty(),
    );
    // Ctrl+C clears an idle draft. The draft is multi-line by now and the
    // composer shows its tail, so the pasted tail before the caret (`ste` +
    // `zx`) is the line that must go; `qKzx` may be scrolled out of view.
    term.key(Key::CtrlC);
    let ctrl_c = term.wait_gone("stezx", Duration::from_secs(10));
    check(&mut term, "ctrl+c clears the draft", ctrl_c);
    // A paste never submits: a copied line carries its line break, and a
    // multi-line paste its CRLFs and a trailing one. Both stay draft text
    // (a submit would leave them in the transcript past the Ctrl+C, and
    // add them to the session's prompts checked at the end).
    term.paste("pasted-alpha\r\n");
    let line = term.wait_for("pasted-alpha", Duration::from_secs(10));
    term.paste("pasted-beta\r\npasted-omega\r\n");
    let lines = term.wait_for("pasted-omega", Duration::from_secs(10));
    check(&mut term, "pastes with line breaks land", line && lines);
    term.settle(Duration::from_millis(500));
    term.key(Key::CtrlC);
    let unsent = term.wait_gone("pasted-alpha", Duration::from_secs(10))
        && term.wait_gone("pasted-omega", Duration::from_secs(10));
    check(&mut term, "pasted line breaks never submit", unsent);
    // A lone Esc acts where it is observable: it closes the command menu (on a
    // draft, one Esc only arms the double-Esc clear). The keys typed after it
    // must stay plain text, never an Alt chord with the held Esc.
    term.type_text("/");
    let menu = term.wait_for("Open settings menu", Duration::from_secs(10));
    check(&mut term, "slash opens the command menu", menu);
    term.key(Key::Escape);
    let closed = term.wait_gone("Open settings menu", Duration::from_secs(10));
    check(&mut term, "escape closes the menu", closed);
    term.type_text("abc");
    let retyped = term.wait_for("no command matches /abc", Duration::from_secs(10));
    check(&mut term, "typing after escape stays text", retyped);
    term.key(Key::Escape);
    let dismissed = term.wait_gone("no command matches", Duration::from_secs(10));
    check(&mut term, "escape dismisses the no-match row", dismissed);
    term.key(Key::CtrlC);
    let cleared = term.wait_gone("/abc", Duration::from_secs(10));
    check(&mut term, "ctrl+c clears the command", cleared);
    // The typed prompt settles before Enter, as a person's would: a key
    // burst and an Enter right behind it read as one paste.
    term.type_text("hello");
    let echoed = term.wait_for("hello", Duration::from_secs(10));
    check(&mut term, "the prompt echoes", echoed);
    term.settle(Duration::from_millis(500));
    term.key(Key::Enter);
    let replied = term.wait_for("conpty scripted reply", Duration::from_secs(60));
    check(&mut term, "enter submits", replied);
    let prompts = submitted_prompts(&session_dir);
    check(
        &mut term,
        &format!("only the typed prompt was submitted ({prompts:?})"),
        prompts == ["hello"],
    );
    // Image paste is Ctrl+V (the user's spec; there is no Alt+V binding). A
    // Ctrl+V the terminal passes on reads the clipboard's bitmap (converted
    // to PNG) into an `[image #N]` marker; with no image, Ctrl+V pastes the
    // clipboard's text, and an empty clipboard says so; a pasted image file
    // path attaches the file. None of it submits.
    let bitmap = set_clipboard_bitmap();
    check(&mut term, "a bitmap lands on the clipboard", bitmap.is_ok());
    term.key(Key::CtrlV);
    let marker = term.wait_for("[image #1]", Duration::from_secs(30));
    check(&mut term, "ctrl+v pastes the clipboard image", marker);
    // A terminal that intercepts Ctrl+V over an image-only clipboard sends an
    // empty bracketed paste. Both console hosts consume the marker pair before
    // the app reads a record (run 44467269683: no host delivered anything), so
    // on the console it must leave the draft as it was; a VT terminal's empty
    // paste attaching the image is pinned by pa-tui's image_paste_headless.
    let composer = |term: &Terminal| {
        term.screen
            .text()
            .lines()
            .find(|line| line.contains("[image #1]"))
            .map(str::to_string)
    };
    let draft = composer(&term);
    term.pty.send(b"\x1b[200~\x1b[201~");
    term.settle(Duration::from_millis(500));
    let unchanged = draft.is_some() && composer(&term) == draft;
    check(
        &mut term,
        "an empty bracketed paste leaves the draft as it was",
        unchanged,
    );
    let text = set_clipboard_text("clip-text-fallback");
    check(&mut term, "text lands on the clipboard", text.is_ok());
    term.key(Key::CtrlV);
    let fallback = term.wait_for("clip-text-fallback", Duration::from_secs(30));
    check(
        &mut term,
        "ctrl+v without an image pastes the text",
        fallback,
    );
    let cleared = clear_clipboard();
    check(&mut term, "the clipboard clears", cleared.is_ok());
    term.key(Key::CtrlV);
    let empty = term.wait_for("Clipboard has no image", Duration::from_secs(30));
    check(&mut term, "an empty clipboard says so", empty);
    let image_file = root.join("dropped shot.png");
    std::fs::write(&image_file, ONE_PIXEL_PNG).expect("write the dropped image");
    term.paste(&format!("\"{}\"", image_file.display()));
    let dropped = term.wait_for("[image #2]", Duration::from_secs(30));
    check(&mut term, "a pasted image path attaches the file", dropped);
    term.settle(Duration::from_millis(500));
    let prompts = submitted_prompts(&session_dir);
    check(
        &mut term,
        &format!("image paste never submits ({prompts:?})"),
        prompts == ["hello"],
    );
    dump(
        &format!("tui-{scenario}"),
        &format!(
            "win32_input={} bracketed_paste={} kitty_depth={} answered={:?}\n{}\n--- screen\n{}\n--- host output\n{}",
            term.win32_input,
            term.bracketed_paste,
            term.kitty_depth,
            term.queries_answered,
            log.join("\n"),
            term.screen.text(),
            visible(&term.stream)
        ),
    );
    drop(term);
    drop(supervisor);
    failures
}

/// A 1x1 opaque PNG.
const ONE_PIXEL_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x70, 0x6b, 0xda, 0x02,
    0x00, 0x02, 0x8e, 0x01, 0x7d, 0xc8, 0x73, 0x8d, 0x1c, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];

/// Run one PowerShell clipboard script (STA: the clipboard is an OLE object).
fn clipboard_script(script: &str) -> Result<(), String> {
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", script])
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

/// Put a 16x8 bitmap on the clipboard the way a screenshot tool does (`SetImage`: the system
/// offers it as `CF_BITMAP`, `CF_DIB` and `CF_DIBV5`, no PNG).
fn set_clipboard_bitmap() -> Result<(), String> {
    clipboard_script(
        "Add-Type -AssemblyName System.Windows.Forms, System.Drawing; \
         $bitmap = New-Object System.Drawing.Bitmap 16, 8; \
         $graphics = [System.Drawing.Graphics]::FromImage($bitmap); \
         $graphics.Clear([System.Drawing.Color]::SteelBlue); \
         [System.Windows.Forms.Clipboard]::SetImage($bitmap)",
    )
}

fn set_clipboard_text(text: &str) -> Result<(), String> {
    clipboard_script(&format!(
        "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Clipboard]::SetText('{text}')"
    ))
}

fn clear_clipboard() -> Result<(), String> {
    clipboard_script(
        "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Clipboard]::Clear()",
    )
}

/// The user prompts every session file under `dir` stored, in file order.
fn submitted_prompts(dir: &Path) -> Vec<String> {
    let mut prompts = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return prompts;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    // The sessions live in the agent dir's database.
    for (_, text) in support::session_texts(dir) {
        for line in text.lines() {
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let message = &entry["message"];
            if entry["type"] != "message" || message["role"] != "user" {
                continue;
            }
            let content = &message["content"];
            let prompt = match content.as_str() {
                Some(text) => text.to_string(),
                None => content
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part["text"].as_str())
                    .collect(),
            };
            prompts.push(prompt);
        }
    }
    for dir in dirs {
        prompts.extend(submitted_prompts(&dir));
    }
    prompts
}

/// Git Bash's mintty without `ConPTY` hands the program an MSYS pty pipe, which
/// `IsTerminal` reports as a terminal while the console the TUI reads keys
/// from never receives one. The interactive mode refuses with the remedy
/// instead of starting a TUI that cannot be typed into.
#[test]
fn git_bash_without_conpty_gets_a_clear_message() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    // The pipe name shape msys-2.0.dll gives a pty's slave side.
    let name: Vec<u16> = format!(
        r"\\.\pipe\msys-0123456789abcdef-pty0-from-master-{}",
        std::process::id()
    )
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect();
    // SAFETY: a named pipe and its client end, both owned below.
    let (server, client) = unsafe {
        let server = win32::CreateNamedPipeW(
            name.as_ptr(),
            win32::PIPE_ACCESS_DUPLEX,
            0,
            1,
            4096,
            4096,
            0,
            std::ptr::null(),
        );
        assert_ne!(server, win32::INVALID_HANDLE_VALUE, "create the pty pipe");
        let client = win32::CreateFileW(
            name.as_ptr(),
            win32::GENERIC_READ_WRITE,
            0,
            std::ptr::null(),
            win32::OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        assert_ne!(client, win32::INVALID_HANDLE_VALUE, "open the pty pipe");
        (
            OwnedHandle::from_raw_handle(server),
            OwnedHandle::from_raw_handle(client),
        )
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
        .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
        .env("PI_OFFLINE", "1")
        .stdin(Stdio::from(client))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run prime-agent");
    let mut stderr_pipe = child.stderr.take().expect("stderr pipe");
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr_pipe, &mut text);
        let _ = stderr_tx.send(text);
    });
    // Bounded: a build without the refusal starts a TUI that never exits.
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll prime-agent") {
            break Some(status);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::park_timeout(Duration::from_millis(50));
    };
    drop(server);
    // A process the TUI spawned can hold the pipe open past the kill.
    let stderr = stderr_rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|_| "<stderr still open>".to_string());
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(1),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("winpty prime-agent") && stderr.contains("MSYS=enable_pcon"),
        "stderr: {stderr}"
    );
}

/// `winbase.h` `CREATE_NEW_CONSOLE`: the detector's canary opens a visible
/// console window on purpose.
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

/// Launching Prime opens no console windows and keeps its own terminal
/// title. The TUI spawns its daemon the way a real launch does. The
/// session's worker probes git for the header and the turn checkpoint, a
/// user shell command runs git, and a turn has the kernel run a command.
/// None of their console children may show a window (a detached parent
/// leaves them none to share, so Windows opens one titled with the program
/// path, such as `git.exe` or the shell's own).
/// None may retitle the terminal either.
#[test]
fn background_children_open_no_console_windows() {
    use std::os::windows::process::CommandExt;

    let watch = console_windows::ConsoleWatch::start();
    // The detector must see a console window the tree does open: a green
    // run is only evidence when the canary is caught.
    let mut canary = Command::new("cmd.exe")
        .args(["/d", "/c", "ping -n 3 127.0.0.1 >nul"])
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .expect("spawn the canary console");
    let canary_pid = canary.id();
    let _ = canary.wait();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !watch
        .shown_by_tree()
        .iter()
        .any(|window| window.pid == canary_pid)
    {
        assert!(
            Instant::now() < deadline,
            "the detector never saw the canary console (pid {canary_pid}); shown: {:?}; job pids: {:?}",
            console_windows::shown(),
            watch.pids()
        );
        std::thread::park_timeout(Duration::from_millis(50));
    }

    support::scrub_ambient_provider_credentials();
    let dir = tempfile::TempDir::new().expect("temp dir");
    let root = dir.path().to_path_buf();
    let agent_dir = root.join("agent");
    let session_dir = agent_dir.join("sessions");
    let cwd = root.join("project");
    std::fs::create_dir_all(&session_dir).expect("sessions dir");
    std::fs::create_dir_all(&cwd).expect("project dir");
    pa_core::settings::SettingsManager::create(&cwd, &agent_dir)
        .set_onboarding_shown(true)
        .expect("onboarding flag");
    let git_init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&cwd)
        .status()
        .expect("run git init");
    assert!(git_init.success(), "git init the project");
    std::fs::write(cwd.join("notes.txt"), "console windows\n").expect("project file");
    let receipt = root.join("kernel-receipt.txt");
    // The kernel's command is a console child of the worker; its output is
    // the receipt that the cell ran.
    let cell = format!(
        "(define ran (bash \"git --version && git status --short\"))\n(fs-write {receipt:?} (hash-ref ran 'output))\n(displayln \"KERNEL_BOOTED\")",
        receipt = receipt.display().to_string(),
    );
    let script = root.join("script.json");
    std::fs::write(
        &script,
        // The `faux` engine runs the real agent loop, so the tool call
        // reaches the kernel (the default scripted engine only replays).
        json!({
            "engine": "faux",
            "responses": [
                { "content": [ { "type": "toolCall", "name": "eval", "arguments": {
                    "code": cell,
                } } ] },
                { "text": "kernel turn done" },
            ],
        })
        .to_string(),
    )
    .expect("script");
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or_default(|stamp| stamp.as_nanos());
    let socket = PathBuf::from(format!(
        r"\\.\pipe\prime-agent-console-windows-{}-{unique}",
        std::process::id()
    ));
    // `base_env` drops every `PRIME_AGENT_*` variable, the worker handshake
    // included; `PI_PACKAGE_DIR` would point the session at another package dir.
    let mut env: Vec<(String, String)> = base_env()
        .into_iter()
        .filter(|(key, _)| key != "PI_PACKAGE_DIR")
        .collect();
    for (key, value) in [
        (
            "PRIME_AGENT_CODING_AGENT_DIR",
            agent_dir.display().to_string(),
        ),
        ("PRIME_AGENT_SESSION_DIR", session_dir.display().to_string()),
        ("PRIME_AGENT_FAUX_SCRIPT", script.display().to_string()),
        ("PA_DAEMON_WORKER_CONNECT_TIMEOUT_MS", "90000".to_string()),
        ("DO_NOT_TRACK", "1".to_string()),
    ] {
        env.push((key.to_string(), value));
    }
    // No `--mode daemon` here: the TUI spawns its own daemon on the socket,
    // the product's launch path.
    let command = format!(
        "\"{}\" --daemon-socket \"{}\"",
        env!("CARGO_BIN_EXE_prime-agent"),
        socket.display()
    );
    let pty = PseudoConsole::spawn(&Host::Inbox, &command, &env, &cwd, 100, 30);
    let mut term = Terminal::new(pty, 100, 30);
    let started = term.wait_for("prime", Duration::from_secs(120));
    term.settle(Duration::from_millis(500));
    // Each typed line echoes and settles before its Enter, as a person's
    // would: on Windows a key burst and an Enter right behind it read as
    // one paste, so the Enter would land as a line break, never a submit.
    term.type_text("!git --version");
    let shell_typed = term.wait_for("git --version", Duration::from_secs(30));
    term.settle(Duration::from_millis(500));
    term.key(Key::Enter);
    let shell_ran = term.wait_for("git version", Duration::from_secs(60));
    term.settle(Duration::from_millis(500));
    term.type_text("boot the kernel");
    let prompt_typed = term.wait_for("boot the kernel", Duration::from_secs(30));
    term.settle(Duration::from_millis(500));
    term.key(Key::Enter);
    let turn_done = term.wait_for("kernel turn done", Duration::from_mins(5));
    term.settle(Duration::from_secs(2));
    let kernel_command = std::fs::read_to_string(&receipt).unwrap_or_default();
    let screen = term.screen.text();
    let stream = String::from_utf8_lossy(&term.stream).to_string();
    shutdown_daemon(&socket);
    drop(term);
    // The titles the terminal was asked to show (OSC 0 and OSC 2): the TUI's
    // own, and the console host's for the program it started.
    let mut titles = Vec::new();
    let mut rest = stream.as_str();
    while let Some(at) = rest.find("\x1b]") {
        let tail = &rest[at + 2..];
        let end = tail.find(['\x07', '\x1b']).unwrap_or(tail.len());
        if let Some(title) = tail[..end]
            .strip_prefix("0;")
            .or_else(|| tail[..end].strip_prefix("2;"))
        {
            titles.push(title.to_string());
        }
        rest = &tail[end..];
    }
    let pids = watch.pids();
    let opened: Vec<String> = watch
        .shown_by_tree()
        .into_iter()
        .filter(|window| window.pid != canary_pid)
        .map(|window| {
            format!(
                "pid {} ({}) titled {:?}",
                window.pid,
                pids.get(&window.pid).map_or("", String::as_str),
                window.title
            )
        })
        .collect();
    let foreign_titles: Vec<&String> = titles
        .iter()
        .filter(|title| {
            let title = title.to_ascii_lowercase();
            title.contains("git.exe") || title.contains("bash.exe") || title.contains("sh.exe")
        })
        .collect();
    dump(
        "console-windows",
        &format!(
            "started={started} shell_typed={shell_typed} shell_ran={shell_ran} prompt_typed={prompt_typed} turn_done={turn_done} kernel_command={kernel_command:?}\nopened={opened:#?}\ntitles={titles:#?}\nall shown={:#?}\njob pids={pids:#?}\n--- screen\n{screen}",
            console_windows::shown()
        ),
    );
    assert!(started, "the TUI started:\n{screen}");
    assert!(shell_typed, "the shell command echoed:\n{screen}");
    assert!(shell_ran, "the user shell command ran git:\n{screen}");
    assert!(prompt_typed, "the prompt echoed:\n{screen}");
    assert!(
        turn_done && kernel_command.contains("git version"),
        "the turn's kernel command ran git:\n{screen}"
    );
    assert!(
        opened.is_empty(),
        "Prime's process tree opened console windows:\n{}",
        opened.join("\n")
    );
    assert!(
        foreign_titles.is_empty(),
        "a child retitled the terminal: {foreign_titles:?}"
    );
    assert!(
        titles.last().is_some_and(|title| title.ends_with("prime")),
        "the terminal keeps Prime's own title: {titles:?}"
    );
}

/// What an inline-image sequence looks like printed as text: the kitty APC, the iTerm2 OSC,
/// a sixel raster header, and the transcript's own row sentinel.
const IMAGE_ESCAPE_TEXT: &[&str] = &["_Ga=", "1337;File", "\"1;1;", "pa-img"];

/// A pasted image renders inline in the user's row on a Windows pseudoconsole and never as
/// escape text: through the inbox host (no pixel protocol: the half-block cells) and through
/// the Windows Terminal host package with `WT_SESSION` set, as Windows Terminal sets it (sixel,
/// which that host passes through to the terminal).
#[test]
fn a_pasted_image_renders_inline_without_escape_text_on_conpty() {
    const LEFT_CTRL: u32 = 0x08;
    let mut failures = Vec::new();
    for host in hosts() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let root = dir.path().to_path_buf();
        let agent_dir = root.join("agent");
        let session_dir = agent_dir.join("sessions");
        let cwd = root.join("project");
        std::fs::create_dir_all(&session_dir).expect("sessions dir");
        std::fs::create_dir_all(&cwd).expect("project dir");
        pa_core::settings::SettingsManager::create(&cwd, &agent_dir)
            .set_onboarding_shown(true)
            .expect("onboarding flag");
        let script = root.join("script.json");
        std::fs::write(
            &script,
            json!({ "responses": ["conpty image reply"] }).to_string(),
        )
        .expect("script");
        let image = root.join("clipboard.png");
        std::fs::write(&image, ONE_PIXEL_PNG).expect("write the clipboard image");
        let supervisor = spawn_supervisor(&agent_dir);
        let mut env = base_env();
        env.retain(|(key, _)| key != "WT_SESSION");
        for (key, value) in [
            (
                "PRIME_AGENT_CODING_AGENT_DIR",
                agent_dir.display().to_string(),
            ),
            ("PRIME_AGENT_SESSION_DIR", session_dir.display().to_string()),
            ("PRIME_AGENT_FAUX_SCRIPT", script.display().to_string()),
            (
                "PRIME_AGENT_TEST_CLIPBOARD_IMAGE",
                image.display().to_string(),
            ),
            ("PI_OFFLINE", "1".to_string()),
            ("DO_NOT_TRACK", "1".to_string()),
            // The TUI talks to the supervisor this test spawned, on the old wire.
            ("PRIME_AGENT_RUNTIME_DAEMON", "0".to_string()),
        ] {
            env.push((key.to_string(), value));
        }
        let windows_terminal = matches!(host, Host::Package(_));
        if windows_terminal {
            env.push((
                "WT_SESSION".to_string(),
                "0f1d2c3b-4a59-6877-8695-a4b3c2d1e0f9".to_string(),
            ));
        }
        let command = format!(
            "\"{}\" --daemon-socket \"{}\"",
            env!("CARGO_BIN_EXE_prime-agent"),
            supervisor.socket.display()
        );
        let pty = PseudoConsole::spawn(&host, &command, &env, &cwd, 100, 30);
        let mut term = Terminal::new(pty, 100, 30);
        let scenario = format!("image-{}", host.label());
        let mut check = |term: &mut Terminal, label: &str, ok: bool| {
            if !ok {
                failures.push(format!("[{scenario}] {label}\n{}", term.screen.text()));
            }
        };
        let started = term.wait_for("prime", Duration::from_secs(60));
        check(&mut term, "startup chrome", started);
        term.settle(Duration::from_millis(500));
        // Ctrl+V reads the clipboard image (the fixture stands in for the clipboard).
        let ctrl_v = if term.win32_input {
            [
                win32_key(0x11, 0x1d, 0, true, LEFT_CTRL),
                win32_key(0x56, 0x2f, 0x16, true, LEFT_CTRL),
                win32_key(0x56, 0x2f, 0x16, false, LEFT_CTRL),
                win32_key(0x11, 0x1d, 0, false, 0),
            ]
            .concat()
        } else {
            vec![0x16]
        };
        term.pty.send(&ctrl_v);
        let marker = term.wait_for("[image #1]", Duration::from_secs(30));
        check(&mut term, "ctrl+v pastes the clipboard image", marker);
        term.type_text(" describe");
        let typed = term.wait_for("describe", Duration::from_secs(10));
        check(&mut term, "the prompt echoes", typed);
        term.settle(Duration::from_millis(500));
        term.key(Key::Enter);
        let replied = term.wait_for("conpty image reply", Duration::from_secs(60));
        check(&mut term, "enter submits", replied);
        // The 1x1 image is one half-block cell in the user's row.
        let picture = term.wait_for("\u{2580}", Duration::from_secs(30));
        check(&mut term, "the image renders in the user's row", picture);
        term.settle(Duration::from_millis(500));
        let screen = term.screen.text();
        let leaked: Vec<&str> = ESCAPE_TEXT
            .iter()
            .chain(IMAGE_ESCAPE_TEXT)
            .copied()
            .filter(|needle| screen.contains(needle))
            .collect();
        check(
            &mut term,
            &format!("no escape text ({leaked:?})"),
            leaked.is_empty(),
        );
        let stream = String::from_utf8_lossy(&term.stream).to_string();
        let sixel = stream.contains("\x1bP0;1;0q");
        check(
            &mut term,
            &format!(
                "sixel reaches the terminal exactly under Windows Terminal (sixel={sixel}, wt={windows_terminal})"
            ),
            sixel == windows_terminal,
        );
        check(
            &mut term,
            "no kitty or iTerm2 sequence is ever sent",
            !stream.contains("\x1b_G") && !stream.contains("\x1b]1337;"),
        );
        dump(
            &format!("image-{}", host.label()),
            &format!(
                "--- screen\n{}\n--- host output\n{}",
                term.screen.text(),
                visible(&term.stream)
            ),
        );
        drop(term);
        drop(supervisor);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
