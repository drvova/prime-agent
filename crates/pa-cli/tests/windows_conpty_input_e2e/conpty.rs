//! A minimal pseudoconsole driver: the test plays the terminal emulator.
//!
//! The child runs attached to a `ConPTY` whose input pipe this driver writes
//! (the bytes a terminal sends for keystrokes) and whose output pipe it
//! drains (the VT the console host renders). The host is either the
//! system's inbox console host (`kernel32!CreatePseudoConsole`) or a
//! side-loaded Windows Terminal build (`conpty.dll` + `x64\OpenConsole.exe`
//! from the `Microsoft.Windows.Console.ConPTY` package), the host Windows
//! Terminal, `WezTerm`, VS Code and Warp ship. Hand-declared kernel32 externs,
//! the repo's no-windows-sys policy (see `pa_types::platform`).

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

type Handle = *mut c_void;
type Hpcon = *mut c_void;

/// `wincon.h` `COORD`, passed by value.
#[repr(C)]
#[derive(Clone, Copy)]
struct Coord {
    x: i16,
    y: i16,
}

#[repr(C)]
struct StartupInfoW {
    cb: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_count_chars: u32,
    y_count_chars: u32,
    fill_attribute: u32,
    flags: u32,
    show_window: u16,
    reserved2_count: u16,
    reserved2: *mut u8,
    std_input: Handle,
    std_output: Handle,
    std_error: Handle,
}

#[repr(C)]
struct StartupInfoExW {
    startup_info: StartupInfoW,
    attribute_list: *mut c_void,
}

#[repr(C)]
struct ProcessInformation {
    process: Handle,
    thread: Handle,
    process_id: u32,
    thread_id: u32,
}

/// `processthreadsapi.h` `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`.
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
/// `winbase.h` `EXTENDED_STARTUPINFO_PRESENT`.
const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
/// `winbase.h` `CREATE_UNICODE_ENVIRONMENT`.
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
/// `winbase.h` `STARTF_USESTDHANDLES`: with invalid handles, so the child
/// takes the pseudoconsole's handles instead of the runner's redirected ones.
const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

extern "system" {
    fn CreatePipe(
        read: *mut Handle,
        write: *mut Handle,
        attributes: *const c_void,
        size: u32,
    ) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
    fn ReadFile(
        handle: Handle,
        buffer: *mut u8,
        len: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn WriteFile(
        handle: Handle,
        buffer: *const u8,
        len: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn InitializeProcThreadAttributeList(
        list: *mut c_void,
        count: u32,
        flags: u32,
        size: *mut usize,
    ) -> i32;
    fn UpdateProcThreadAttribute(
        list: *mut c_void,
        flags: u32,
        attribute: usize,
        value: *const c_void,
        size: usize,
        previous: *mut c_void,
        returned: *mut usize,
    ) -> i32;
    fn DeleteProcThreadAttributeList(list: *mut c_void);
    fn CreateProcessW(
        application: *const u16,
        command_line: *mut u16,
        process_attributes: *const c_void,
        thread_attributes: *const c_void,
        inherit_handles: i32,
        flags: u32,
        environment: *const c_void,
        cwd: *const u16,
        startup_info: *const StartupInfoExW,
        process_information: *mut ProcessInformation,
    ) -> i32;
    fn TerminateProcess(process: Handle, code: u32) -> i32;
    fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
    fn LoadLibraryW(name: *const u16) -> Handle;
    fn GetProcAddress(module: Handle, name: *const u8) -> *const c_void;
}

type CreatePseudoConsoleFn =
    unsafe extern "system" fn(Coord, Handle, Handle, u32, *mut Hpcon) -> i32;
type ClosePseudoConsoleFn = unsafe extern "system" fn(Hpcon);

fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// Which console host serves the pseudoconsole.
#[derive(Debug, Clone)]
pub enum Host {
    /// The OS's own `kernel32!CreatePseudoConsole`.
    Inbox,
    /// A side-loaded Windows Terminal `conpty.dll` (its `OpenConsole.exe`
    /// sits in `x64\` beside it).
    Package(std::path::PathBuf),
}

impl Host {
    pub fn label(&self) -> &'static str {
        match self {
            Host::Inbox => "inbox",
            Host::Package(_) => "terminal-package",
        }
    }

    fn entry_points(&self) -> (CreatePseudoConsoleFn, ClosePseudoConsoleFn) {
        let (module, create, close): (Vec<u16>, &[u8], &[u8]) = match self {
            Host::Inbox => (
                wide(std::ffi::OsStr::new("kernel32.dll")),
                b"CreatePseudoConsole\0",
                b"ClosePseudoConsole\0",
            ),
            Host::Package(dll) => (
                wide(dll.as_os_str()),
                b"ConptyCreatePseudoConsole\0",
                b"ConptyClosePseudoConsole\0",
            ),
        };
        // SAFETY: plain module load and symbol lookups; the symbols carry the
        // documented pseudoconsole signatures.
        unsafe {
            let handle = LoadLibraryW(module.as_ptr());
            assert!(!handle.is_null(), "load the console host module {self:?}");
            let create = GetProcAddress(handle, create.as_ptr());
            let close = GetProcAddress(handle, close.as_ptr());
            assert!(
                !create.is_null() && !close.is_null(),
                "the pseudoconsole entry points of {self:?}"
            );
            (
                std::mem::transmute::<*const c_void, CreatePseudoConsoleFn>(create),
                std::mem::transmute::<*const c_void, ClosePseudoConsoleFn>(close),
            )
        }
    }
}

/// A raw handle the reader thread owns.
struct SendHandle(Handle);
// SAFETY: a pipe handle is usable from any thread; only the reader thread touches it.
unsafe impl Send for SendHandle {}

/// One child process on a pseudoconsole.
pub struct PseudoConsole {
    hpcon: Hpcon,
    close: ClosePseudoConsoleFn,
    input: Handle,
    process: Handle,
    thread: Handle,
    output: mpsc::Receiver<Vec<u8>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl PseudoConsole {
    /// Spawn `command_line` on a fresh `cols`x`rows` pseudoconsole served by
    /// `host`, with `env` as its whole environment.
    pub fn spawn(
        host: &Host,
        command_line: &str,
        env: &[(String, String)],
        cwd: &Path,
        cols: i16,
        rows: i16,
    ) -> PseudoConsole {
        let (create, close) = host.entry_points();
        // SAFETY: the documented ConPTY bring-up sequence; every out-pointer
        // is a live local, and every failure panics before a handle is used.
        unsafe {
            let mut input_read = std::ptr::null_mut();
            let mut input_write = std::ptr::null_mut();
            let mut output_read = std::ptr::null_mut();
            let mut output_write = std::ptr::null_mut();
            assert_ne!(
                CreatePipe(
                    &raw mut input_read,
                    &raw mut input_write,
                    std::ptr::null(),
                    0
                ),
                0
            );
            assert_ne!(
                CreatePipe(
                    &raw mut output_read,
                    &raw mut output_write,
                    std::ptr::null(),
                    0
                ),
                0
            );
            let mut hpcon = std::ptr::null_mut();
            let result = create(
                Coord { x: cols, y: rows },
                input_read,
                output_write,
                0,
                &raw mut hpcon,
            );
            assert_eq!(result, 0, "CreatePseudoConsole on {host:?}: {result:#x}");
            // The host holds its own duplicates of the child-side ends.
            CloseHandle(input_read);
            CloseHandle(output_write);

            let mut size = 0usize;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &raw mut size);
            let mut list = vec![0u8; size];
            let list_ptr = list.as_mut_ptr().cast::<c_void>();
            assert_ne!(
                InitializeProcThreadAttributeList(list_ptr, 1, 0, &raw mut size),
                0
            );
            assert_ne!(
                UpdateProcThreadAttribute(
                    list_ptr,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                    hpcon,
                    std::mem::size_of::<Hpcon>(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                0
            );
            let startup = StartupInfoExW {
                startup_info: StartupInfoW {
                    cb: u32::try_from(std::mem::size_of::<StartupInfoExW>()).expect("size"),
                    reserved: std::ptr::null_mut(),
                    desktop: std::ptr::null_mut(),
                    title: std::ptr::null_mut(),
                    x: 0,
                    y: 0,
                    x_size: 0,
                    y_size: 0,
                    x_count_chars: 0,
                    y_count_chars: 0,
                    fill_attribute: 0,
                    flags: STARTF_USESTDHANDLES,
                    show_window: 0,
                    reserved2_count: 0,
                    reserved2: std::ptr::null_mut(),
                    std_input: INVALID_HANDLE_VALUE,
                    std_output: INVALID_HANDLE_VALUE,
                    std_error: INVALID_HANDLE_VALUE,
                },
                attribute_list: list_ptr,
            };
            let mut block: Vec<u16> = Vec::new();
            for (key, value) in env {
                block.extend(std::ffi::OsStr::new(&format!("{key}={value}")).encode_wide());
                block.push(0);
            }
            block.push(0);
            let mut command = wide(std::ffi::OsStr::new(command_line));
            let cwd = wide(cwd.as_os_str());
            let mut info = ProcessInformation {
                process: std::ptr::null_mut(),
                thread: std::ptr::null_mut(),
                process_id: 0,
                thread_id: 0,
            };
            let created = CreateProcessW(
                std::ptr::null(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                block.as_ptr().cast::<c_void>(),
                cwd.as_ptr(),
                &raw const startup,
                &raw mut info,
            );
            assert_ne!(
                created,
                0,
                "CreateProcessW {command_line}: {}",
                std::io::Error::last_os_error()
            );
            DeleteProcThreadAttributeList(list_ptr);

            let (tx, rx) = mpsc::channel();
            let output = SendHandle(output_read);
            let reader = std::thread::spawn(move || {
                let output = output;
                let mut buffer = vec![0u8; 16 * 1024];
                loop {
                    let mut read = 0u32;
                    let ok = ReadFile(
                        output.0,
                        buffer.as_mut_ptr(),
                        u32::try_from(buffer.len()).expect("buffer size"),
                        &raw mut read,
                        std::ptr::null_mut(),
                    );
                    if ok == 0 || read == 0 {
                        break;
                    }
                    if tx.send(buffer[..read as usize].to_vec()).is_err() {
                        break;
                    }
                }
                CloseHandle(output.0);
            });
            PseudoConsole {
                hpcon,
                close,
                input: input_write,
                process: info.process,
                thread: info.thread,
                output: rx,
                reader: Some(reader),
            }
        }
    }

    /// Send the bytes a terminal emulator would send for input.
    pub fn send(&self, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() {
            let mut written = 0u32;
            // SAFETY: the input pipe's write end stays open for `self`'s life.
            let ok = unsafe {
                WriteFile(
                    self.input,
                    bytes[offset..].as_ptr(),
                    u32::try_from(bytes.len() - offset).expect("input size"),
                    &raw mut written,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(ok, 0, "write the pseudoconsole input pipe");
            offset += written as usize;
        }
    }

    /// The next output chunk the host rendered, waiting at most `wait`.
    pub fn recv(&self, wait: Duration) -> Option<Vec<u8>> {
        self.output.recv_timeout(wait).ok()
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        // SAFETY: every handle was created by `spawn` and is closed once here.
        unsafe {
            TerminateProcess(self.process, 1);
            WaitForSingleObject(self.process, 5_000);
            CloseHandle(self.input);
            (self.close)(self.hpcon);
            CloseHandle(self.process);
            CloseHandle(self.thread);
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
