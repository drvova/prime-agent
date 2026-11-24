//! Named-pipe transport for Windows endpoints (`\\.\pipe\` namespace): byte-mode duplex instances,
//! local-only, clients waiting while every instance is busy (Node's `net` on Windows); implements
//! the shared [`TransportListener`] / [`TransportStream`] / [`BlockingTransportStream`] contracts.
//! Endpoint naming lives in `pa-daemon::platform`.
//!
//! An elevated process's pipes are its elevated daemon's: each instance is
//! created with [`super::elevation::pipe_security_descriptor`]'s access list
//! (only `SYSTEM` and elevated administrators, at High integrity), the first
//! one with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a name another program took
//! first fails the bind instead of being shared; and an elevated client
//! refuses a pipe whose serving process is not elevated.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::runtime::Runtime;
use tokio::sync::Mutex as AsyncMutex;

use super::transport::BlockingTransportStream;

/// `winerror.h` `ERROR_PIPE_BUSY`: the pipe name exists but no instance is
/// listening (pinned constant, no windows-sys dependency).
const ERROR_PIPE_BUSY: i32 = 231;
/// Busy-pipe poll interval; Node's clients wait the same way.
const BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(50);
/// Overall cap on busy-pipe waiting, so a wedged server surfaces as an
/// error instead of hanging the caller.
const BUSY_RETRY_DEADLINE: Duration = Duration::from_secs(10);
/// `sddl.h` `SDDL_REVISION_1`.
const SDDL_REVISION_1: u32 = 1;

/// `SECURITY_ATTRIBUTES`.
#[repr(C)]
#[allow(non_snake_case)] // The Win32 fields keep their documented names.
struct SecurityAttributes {
    nLength: u32,
    lpSecurityDescriptor: *mut c_void,
    bInheritHandle: i32,
}

#[link(name = "advapi32")]
extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
    fn GetNamedPipeServerProcessId(pipe: *mut c_void, process_id: *mut u32) -> i32;
}

/// A self-relative security descriptor converted from SDDL, freed on drop.
struct PipeSecurity(*mut c_void);

// SAFETY: the descriptor is memory this value alone owns and nothing writes
// after the conversion; Windows only reads it.
unsafe impl Send for PipeSecurity {}
// SAFETY: as above.
unsafe impl Sync for PipeSecurity {}

impl PipeSecurity {
    /// The descriptor `sddl` describes.
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let text: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: a NUL-terminated string and live out-pointers.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PipeSecurity(descriptor))
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by the conversion and is freed
        // once, here.
        unsafe { LocalFree(self.0) };
    }
}

/// A listening named-pipe endpoint. One instance waits for the next client at all times; `accept`
/// connects that instance and has already created its replacement, so the pipe name stays
/// connectable - the named-pipe equivalent of a backlog.
pub(crate) struct NamedPipeListener {
    name: String,
    /// The access list every instance is created with: an elevated daemon's
    /// own; `None` keeps Windows' default.
    security: Option<PipeSecurity>,
    /// The instance currently listening for the next client.
    pending: AsyncMutex<Option<NamedPipeServer>>,
}

impl NamedPipeListener {
    /// Create the first pipe instance at `name`. `first_pipe_instance`
    /// fails when another process owns the name (a live Unix socket blocking
    /// `bind`); a stale pipe cannot exist - instances die with their process.
    ///
    /// For an elevated process the name is its elevated daemon's: a pipe of
    /// that name that already exists was made by another program, and the
    /// bind fails saying so rather than share it.
    pub(crate) fn bind(name: &str) -> io::Result<Self> {
        let elevation = super::elevation::process_elevation();
        let security = super::elevation::pipe_security_descriptor(elevation)
            .map(PipeSecurity::from_sddl)
            .transpose()?;
        let mut listener = Self {
            name: name.to_string(),
            security,
            pending: AsyncMutex::new(None),
        };
        let first = listener
            .create(ServerOptions::new().first_pipe_instance(true))
            .map_err(|error| match elevation {
                super::elevation::Elevation::Elevated => io::Error::new(
                    error.kind(),
                    format!(
                        "the elevated daemon's pipe {name} could not be created ({error}): \
                         another program may hold its name; Prime Agent does not share it"
                    ),
                ),
                super::elevation::Elevation::Standard => error,
            })?;
        *listener.pending.get_mut() = Some(first);
        Ok(listener)
    }

    /// Create one instance of this listener's pipe with `options` and its
    /// access list.
    fn create(&self, options: &ServerOptions) -> io::Result<NamedPipeServer> {
        let Some(security) = &self.security else {
            return options.create(&self.name);
        };
        let mut attributes = SecurityAttributes {
            nLength: u32::try_from(std::mem::size_of::<SecurityAttributes>())
                .map_err(io::Error::other)?,
            lpSecurityDescriptor: security.0,
            bInheritHandle: 0,
        };
        // SAFETY: `attributes` is a live `SECURITY_ATTRIBUTES` whose
        // descriptor outlives the call.
        unsafe {
            options.create_with_security_attributes_raw(&self.name, (&raw mut attributes).cast())
        }
    }

    /// Hand the next client a connected server instance. The next instance is created before
    /// waiting, so a creation failure surfaces first; two instances listen while this call waits,
    /// keeping clients off the busy path.
    pub(crate) async fn accept(&self) -> io::Result<NamedPipeServer> {
        let mut pending = self.pending.lock().await;
        let next = self.create(&ServerOptions::new())?;
        let server = pending
            .replace(next)
            .expect("a listening instance is always kept ready");
        server.connect().await?;
        Ok(server)
    }
}

/// Open a client connection; a busy pipe (`ERROR_PIPE_BUSY`) is retried until the server frees one
/// (Node's clients wait the same way through libuv's connect machinery).
///
/// An elevated client uses the pipe only when the process serving it is
/// elevated too: any program of the user can create a pipe of the elevated
/// daemon's name before the daemon does.
pub(crate) async fn connect(name: &str) -> io::Result<NamedPipeClient> {
    let deadline = tokio::time::Instant::now() + BUSY_RETRY_DEADLINE;
    loop {
        match ClientOptions::new().open(name) {
            Ok(client) => {
                let client_elevation = super::elevation::process_elevation();
                if client_elevation == super::elevation::Elevation::Standard {
                    return Ok(client);
                }
                let mut server = 0;
                // SAFETY: the client's pipe handle is open, and `server` a
                // live DWORD.
                if unsafe { GetNamedPipeServerProcessId(client.as_raw_handle(), &raw mut server) }
                    == 0
                {
                    return Err(io::Error::last_os_error());
                }
                let server_elevation = super::elevation::process_elevation_of(server);
                if super::elevation::trusts_pipe_server(client_elevation, server_elevation) {
                    return Ok(client);
                }
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "pipe {name} is served by process {server}, which is not elevated: an \
                         elevated Prime Agent uses only an elevated daemon (another program may \
                         have taken the elevated daemon's pipe name)"
                    ),
                ));
            }
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {}
            Err(error) => return Err(error),
        }
        if tokio::time::Instant::now() + BUSY_RETRY_INTERVAL > deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("named pipe {name} stayed busy for more than {BUSY_RETRY_DEADLINE:?}"),
            ));
        }
        tokio::time::sleep(BUSY_RETRY_INTERVAL).await;
    }
}

/// State shared by the read and write halves of a blocking client: one pipe handle driven by a
/// dedicated single-threaded runtime, because std exposes no read deadline for named pipes; the
/// runtime gives `set_read_timeout` a real bounded wait.
struct BlockingPipeState {
    runtime: Runtime,
    client: AsyncMutex<NamedPipeClient>,
    /// Read deadline for pending reads; `None` blocks indefinitely.
    read_timeout: Mutex<Option<Duration>>,
}

/// A blocking named-pipe client for one-shot command surfaces (the CLI
/// daemon client). Cloning shares the underlying connection.
pub(crate) struct BlockingPipeClient {
    name: String,
    state: Arc<BlockingPipeState>,
}

impl std::fmt::Debug for BlockingPipeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockingPipeClient")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl BlockingPipeClient {
    /// Connect to the pipe at `name`, blocking until connected.
    pub(crate) fn connect(name: &str) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let client = runtime.block_on(connect(name))?;
        Ok(Self {
            name: name.to_string(),
            state: Arc::new(BlockingPipeState {
                runtime,
                client: AsyncMutex::new(client),
                read_timeout: Mutex::new(None),
            }),
        })
    }

    /// Duplicate this handle so reads and writes can proceed on separate
    /// owned halves of the same connection.
    pub(crate) fn try_clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            state: Arc::clone(&self.state),
        }
    }

    /// One blocking read, bounded by the configured deadline when present.
    fn read_bounded(&self, buf: &mut [u8]) -> io::Result<usize> {
        let deadline = self
            .state
            .read_timeout
            .lock()
            .map_err(|_| io::Error::other("read deadline lock poisoned"))?;
        let read = async { self.state.client.lock().await.read(buf).await };
        match *deadline {
            Some(timeout) => match self
                .state
                .runtime
                // Construct the timer only after entering this client's runtime.
                .block_on(async { tokio::time::timeout(timeout, read).await })
            {
                Ok(result) => result,
                // Cancelled reads leave the buffer untouched; callers treat
                // the deadline as an empty poll, like Unix `SO_RCVTIMEO`.
                Err(_elapsed) => Err(io::ErrorKind::TimedOut.into()),
            },
            None => self.state.runtime.block_on(read),
        }
    }
}

impl std::io::Read for BlockingPipeClient {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_bounded(buf)
    }
}

impl std::io::Write for BlockingPipeClient {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.state
            .runtime
            .block_on(async { self.state.client.lock().await.write(buf).await })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.state
            .runtime
            .block_on(async { self.state.client.lock().await.flush().await })
    }
}

impl BlockingTransportStream for BlockingPipeClient {
    fn try_clone_box(&self) -> io::Result<Box<dyn BlockingTransportStream>> {
        Ok(Box::new(self.try_clone()))
    }

    fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        *self
            .state
            .read_timeout
            .lock()
            .map_err(|_| io::Error::other("read deadline lock poisoned"))? = Some(timeout);
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tests {
    //! Round-trips through the public transport API. Windows-only; the sandbox verifies compilation
    //! for the cross target, a real Windows runner executes them.

    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::super::transport::{bind_transport, connect_blocking, connect_transport};

    /// Unique pipe name per test process (parallel binaries must not
    /// collide in the global `\.\pipe\` namespace).
    fn test_pipe(tag: &str) -> PathBuf {
        PathBuf::from(format!(
            r"\\.\pipe\prime-agent-pa-types-test-{}-{tag}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn pipe_roundtrip_through_public_api() {
        let path = test_pipe("async-roundtrip");
        let listener = bind_transport(&path).await.expect("bind");
        let connect_path = path.clone();
        let client =
            tokio::spawn(async move { connect_transport(&connect_path).await.expect("connect") });
        let server = listener.accept().await.expect("accept");
        let client = client.await.expect("client task");
        let (mut client_read, mut client_write) = client.split();
        let (mut server_read, mut server_write) = server.split();
        server_write.write_all(b"ping").await.expect("server write");
        let mut buf = [0u8; 4];
        client_read.read_exact(&mut buf).await.expect("client read");
        assert_eq!(&buf, b"ping");
        client_write.write_all(b"pong").await.expect("client write");
        let mut buf = [0u8; 4];
        server_read.read_exact(&mut buf).await.expect("server read");
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn second_bind_on_a_live_pipe_name_fails() {
        let path = test_pipe("bind-conflict");
        let _listener = bind_transport(&path).await.expect("first bind");
        let error = bind_transport(&path)
            .await
            .err()
            .expect("a live pipe name must not be rebindable");
        assert!(
            error.to_string().to_lowercase().contains("denied"),
            "first-instance conflict, got: {error}"
        );
    }

    #[test]
    fn blocking_client_deadline_then_roundtrip() {
        let path = test_pipe("blocking-roundtrip");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let server_path = path.clone();

        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("server runtime");
            runtime.block_on(async move {
                let listener = bind_transport(&server_path).await.expect("bind");
                ready_tx.send(()).expect("listener ready");
                let server = listener.accept().await.expect("accept");
                let (mut reader, mut writer) = server.split();
                reply_rx.await.expect("release reply");
                writer.write_all(b"hello").await.expect("server write");
                let mut buf = [0u8; 4];
                reader.read_exact(&mut buf).await.expect("server read");
                assert_eq!(&buf, b"echo");
            });
        });

        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener ready");
        let mut stream = connect_blocking(&path).expect("blocking connect");
        stream
            .set_read_timeout(Duration::from_millis(50))
            .expect("deadline");
        let mut buf = [0u8; 5];
        assert_eq!(
            stream
                .read(&mut buf)
                .expect_err("reply is held back")
                .kind(),
            std::io::ErrorKind::TimedOut
        );
        reply_tx.send(()).expect("release reply");
        stream
            .set_read_timeout(Duration::from_secs(5))
            .expect("roundtrip deadline");
        stream.read_exact(&mut buf).expect("client read");
        assert_eq!(&buf, b"hello");
        stream.write_all(b"echo").expect("client write");
        stream.flush().expect("client flush");
        server.join().expect("server thread");
    }

    #[test]
    fn blocking_connect_to_missing_pipe_reports_not_found() {
        let path = test_pipe("missing");
        let error = connect_blocking(&path).expect_err("no listener owns this pipe name");
        // The CLI maps NotFound to Node's `connect ENOENT <path>` error text.
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}
