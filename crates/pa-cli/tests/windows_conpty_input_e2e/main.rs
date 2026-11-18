// too_many_lines: the scenario is one linear script. cast_*: the pipe and
// console APIs take 32-bit sizes the test's buffers never exceed.
#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
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
    #![allow(non_snake_case)]
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
        let socket = self.socket.clone();
        let shutdown = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("shutdown runtime");
            runtime.block_on(async move {
                let Ok((client, events)) =
                    pa_tui::daemon_client::DaemonClient::connect(&socket).await
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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// The supervisor child is killed and reaped by `Supervisor`'s Drop.
#[allow(clippy::zombie_processes)]
fn spawn_supervisor(agent_dir: &Path) -> Supervisor {
    support::scrub_ambient_provider_credentials();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|stamp| stamp.as_nanos())
        .unwrap_or_default();
    let socket = PathBuf::from(format!(
        r"\\.\pipe\prime-agent-conpty-{}-{unique}",
        std::process::id()
    ));
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .args(["--mode", "daemon", "--daemon-socket"])
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
        }
    }
}

/// What a leaked VT sequence looks like once its ESC is gone: the arrow, the
/// CSI-u and modifyOtherKeys keys, paste markers, the win32-input-mode tail,
/// mouse reports and device-attribute replies.
const ESCAPE_TEXT: &[&str] = &[
    "[D", "[?", "[<", "200~", "201~", ";2u", "13;2", ";1_", ";0_", "[27;", "\u{2190}",
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
    term.key(Key::Escape);
    let cleared = term.wait_gone("qKzx", Duration::from_secs(10));
    check(&mut term, "escape clears the draft", cleared);
    term.type_text("abc");
    let retyped = term.wait_for("abc", Duration::from_secs(10));
    check(&mut term, "retype", retyped);
    term.key(Key::CtrlC);
    let ctrl_c = term.wait_gone("abc", Duration::from_secs(10));
    check(&mut term, "ctrl+c clears the draft", ctrl_c);
    term.type_text("hello");
    term.key(Key::Enter);
    let replied = term.wait_for("conpty scripted reply", Duration::from_secs(60));
    check(&mut term, "enter submits", replied);
    term.settle(Duration::from_millis(500));
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
