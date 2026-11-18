// too_many_lines: the scenario is one linear script. cast_*: the pipe and
// console APIs take 32-bit sizes the test's buffers never exceed.
#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
#![cfg(windows)]

//! DIAGNOSTIC (temporary): what the console delivers on a pseudoconsole.

mod conpty;
#[path = "../support/mod.rs"]
mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use conpty::{Host, PseudoConsole};
use serde_json::{json, Map};

const PROBE_LOG_ENV: &str = "PA_CONPTY_PROBE_LOG";
const PROBE_VT_INPUT_ENV: &str = "PA_CONPTY_PROBE_VT_INPUT";

mod console {
    #![allow(non_snake_case)]
    use std::ffi::c_void;
    pub type Handle = *mut c_void;
    extern "system" {
        pub fn GetStdHandle(which: i32) -> Handle;
        pub fn GetConsoleMode(handle: Handle, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(handle: Handle, mode: u32) -> i32;
        pub fn ReadConsoleInputW(
            handle: Handle,
            buffer: *mut InputRecord,
            len: u32,
            read: *mut u32,
        ) -> i32;
        pub fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct InputRecord {
        pub event_type: u16,
        pub _pad: u16,
        pub key_down: i32,
        pub repeat: u16,
        pub vk: u16,
        pub scan: u16,
        pub uchar: u16,
        pub control: u32,
    }
}

/// The probe child: raw console records, logged one per line.
#[test]
fn conpty_probe_child_mode() {
    let Ok(log) = std::env::var(PROBE_LOG_ENV) else {
        return;
    };
    let mut log = std::fs::File::create(log).expect("probe log");
    // SAFETY: console handle queries and mode writes on this process's own console.
    unsafe {
        let input = console::GetStdHandle(-10);
        let output = console::GetStdHandle(-11);
        let mut mode = 0u32;
        console::GetConsoleMode(input, &raw mut mode);
        writeln!(log, "initial input mode {mode:#x}").ok();
        let mut out_mode = 0u32;
        console::GetConsoleMode(output, &raw mut out_mode);
        writeln!(log, "initial output mode {out_mode:#x}").ok();
        if std::env::var(PROBE_VT_INPUT_ENV).is_ok() {
            mode |= 0x200;
        }
        // crossterm's raw mode: line, echo and processed input off.
        mode &= !(0x1 | 0x2 | 0x4);
        console::SetConsoleMode(input, mode);
        console::SetConsoleMode(output, out_mode | 0x4);
        let mut now_mode = 0u32;
        console::GetConsoleMode(input, &raw mut now_mode);
        writeln!(log, "raw input mode {now_mode:#x}").ok();
        let mut stdout = std::io::stdout();
        // The modes the session surface writes on Windows today.
        stdout
            .write_all(b"\x1b[?1049h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?2004h\x1b[>4;0m")
            .ok();
        stdout.write_all(b"PROBE-READY\r\n").ok();
        stdout.flush().ok();
        let mut phase = 0;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if console::WaitForSingleObject(input, 200) != 0 {
                continue;
            }
            let mut records = [console::InputRecord {
                event_type: 0,
                _pad: 0,
                key_down: 0,
                repeat: 0,
                vk: 0,
                scan: 0,
                uchar: 0,
                control: 0,
            }; 64];
            let mut read = 0u32;
            console::ReadConsoleInputW(input, records.as_mut_ptr(), 64, &raw mut read);
            for record in &records[..read as usize] {
                if record.event_type == 1 {
                    let ch = char::from_u32(u32::from(record.uchar)).unwrap_or('?');
                    writeln!(
                        log,
                        "KEY down={} vk={:#x} scan={:#x} uchar={:#x} {:?} ctrl={:#x}",
                        record.key_down, record.vk, record.scan, record.uchar, ch, record.control
                    )
                    .ok();
                    if record.key_down != 0 && record.uchar == u16::from(b'Z') {
                        phase += 1;
                        if phase == 1 {
                            writeln!(log, "--- queries: DA1, kitty, CPR, DECRQM 2026").ok();
                            stdout.write_all(b"\x1b[c\x1b[?u\x1b[6n\x1b[?2026$p").ok();
                            stdout.write_all(b"PROBE-QUERIED\r\n").ok();
                            stdout.flush().ok();
                        }
                    }
                } else {
                    writeln!(log, "EVENT type={}", record.event_type).ok();
                }
            }
            if phase >= 2 {
                break;
            }
        }
        stdout.write_all(b"PROBE-DONE\r\n").ok();
        stdout.flush().ok();
    }
}

fn hosts() -> Vec<Host> {
    let mut hosts = vec![Host::Inbox];
    if let Ok(dll) = std::env::var("PA_CONPTY_DLL") {
        hosts.push(Host::Package(PathBuf::from(dll)));
    }
    hosts
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

fn drain_until(pty: &PseudoConsole, stream: &mut Vec<u8>, needle: &[u8], wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        if stream.windows(needle.len()).any(|window| window == needle) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        if let Some(chunk) = pty.recv(Duration::from_millis(50)) {
            stream.extend_from_slice(&chunk);
        }
    }
}

/// win32-input-mode encoding of one key transition (`CSI Vk;Sc;Uc;Kd;Cs;Rc _`).
fn win32_key(vk: u16, scan: u16, uchar: u16, down: bool, control: u32) -> Vec<u8> {
    format!("\x1b[{vk};{scan};{uchar};{};{control};1_", u8::from(down)).into_bytes()
}

#[test]
fn diagnose_console_records() {
    for host in hosts() {
        for vt_input in [false, true] {
            let dir = tempfile::TempDir::new().expect("temp dir");
            let log = dir.path().join("probe.log");
            let mut env = base_env();
            env.push((PROBE_LOG_ENV.to_string(), log.display().to_string()));
            if vt_input {
                env.push((PROBE_VT_INPUT_ENV.to_string(), "1".to_string()));
            }
            let exe = std::env::current_exe().expect("test binary");
            let command = format!(
                "\"{}\" --exact conpty_probe_child_mode --nocapture --test-threads=1",
                exe.display()
            );
            let pty = PseudoConsole::spawn(&host, &command, &env, dir.path(), 100, 30);
            let mut stream = Vec::new();
            let ready = drain_until(&pty, &mut stream, b"PROBE-READY", Duration::from_secs(30));
            let mut sent = Vec::new();
            if ready {
                let inputs: Vec<(&str, Vec<u8>)> = vec![
                    ("plain a", b"a".to_vec()),
                    ("vt left", b"\x1b[D".to_vec()),
                    ("vt shift+enter csi-u", b"\x1b[13;2u".to_vec()),
                    ("vt shift+enter modifyOtherKeys", b"\x1b[27;2;13~".to_vec()),
                    ("bracketed paste", b"\x1b[200~hi\x1b[201~".to_vec()),
                    ("sgr mouse", b"\x1b[<35;5;5M".to_vec()),
                    ("cr", b"\r".to_vec()),
                    ("ctrl+c", b"\x03".to_vec()),
                    (
                        "win32 b",
                        [
                            win32_key(0x42, 0x30, 0x62, true, 0),
                            win32_key(0x42, 0x30, 0x62, false, 0),
                        ]
                        .concat(),
                    ),
                    (
                        "win32 shift+enter",
                        [
                            win32_key(0x10, 0x2a, 0, true, 0x10),
                            win32_key(0x0d, 0x1c, 0x0d, true, 0x10),
                            win32_key(0x0d, 0x1c, 0x0d, false, 0x10),
                            win32_key(0x10, 0x2a, 0, false, 0),
                        ]
                        .concat(),
                    ),
                    ("lone esc", b"\x1b".to_vec()),
                    ("end marker", b"Z".to_vec()),
                ];
                for (label, bytes) in inputs {
                    sent.push(label);
                    pty.send(&bytes);
                    // Diagnostic pacing only: each input lands as its own write.
                    std::thread::sleep(Duration::from_millis(150));
                }
                let queried =
                    drain_until(&pty, &mut stream, b"PROBE-QUERIED", Duration::from_secs(10));
                if queried {
                    // A real terminal answers DA1; the host may answer itself.
                    pty.send(b"\x1b[?61;4c");
                    std::thread::sleep(Duration::from_millis(300));
                    pty.send(b"Z");
                }
                drain_until(&pty, &mut stream, b"PROBE-DONE", Duration::from_secs(10));
                pty.wait_exit(Duration::from_secs(10));
            }
            let records = std::fs::read_to_string(&log).unwrap_or_else(|e| format!("no log: {e}"));
            let name = format!(
                "probe-{}-{}",
                host.label(),
                if vt_input { "vtinput" } else { "native" }
            );
            dump(
                &name,
                &format!(
                    "ready={ready} sent={sent:?}\n--- records\n{records}\n--- host output\n{}",
                    visible(&stream)
                ),
            );
        }
    }
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

const ESCAPE_TEXT: &[&str] = &[
    "[D", "[?", "[<", "200~", "201~", ";2u", "13;2", ";1_", ";0_", "[27;", "\u{2190}",
];

#[test]
fn prime_tui_keys_act_on_conpty() {
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
        let command = format!(
            "\"{}\" --daemon-socket \"{}\"",
            env!("CARGO_BIN_EXE_prime-agent"),
            supervisor.socket.display()
        );
        let pty = PseudoConsole::spawn(&host, &command, &env, &cwd, 100, 30);
        let mut term = Terminal::new(pty, 100, 30);
        let mut log = Vec::new();
        let mut check = |term: &mut Terminal, label: &str, ok: bool| {
            log.push(format!("{label}: {}", if ok { "ok" } else { "FAILED" }));
            if !ok {
                failures.push(format!(
                    "[{}] {label}\n{}",
                    host.label(),
                    term.screen.text()
                ));
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
        let name = format!("tui-{}", host.label());
        dump(
            &name,
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
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
