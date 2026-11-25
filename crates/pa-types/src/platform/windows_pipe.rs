//! Named-pipe transport for Windows endpoints (`\\.\pipe\` namespace): byte-mode duplex instances,
//! local-only, clients waiting while every instance is busy (Node's `net` on Windows); implements
//! the shared [`TransportListener`] / [`TransportStream`] / [`BlockingTransportStream`] contracts.
//! Endpoint naming lives in `pa-daemon::platform`.

use std::io;
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

/// A listening named-pipe endpoint. Exactly one instance listens at a time (tokio's documented
/// server loop): a client may connect to it before `accept` waits, and `accept` hands that
/// client out. Two listening instances would let a lone client connect to the one no `accept`
/// waits on yet, which is only handed out on the following accept. While the connected
/// instance's replacement is being created a client sees the pipe busy and waits
/// ([`connect`]).
pub(crate) struct NamedPipeListener {
    name: String,
    /// The instance currently listening for the next client.
    pending: AsyncMutex<Option<NamedPipeServer>>,
}

impl NamedPipeListener {
    /// Create the first pipe instance at `name`. `first_pipe_instance`
    /// fails when another process owns the name (a live Unix socket blocking
    /// `bind`); a stale pipe cannot exist - instances die with their process.
    pub(crate) fn bind(name: &str) -> io::Result<Self> {
        let first = ServerOptions::new()
            .first_pipe_instance(true)
            .create(name)?;
        Ok(Self {
            name: name.to_string(),
            pending: AsyncMutex::new(Some(first)),
        })
    }

    /// Hand the next client a connected server instance, then create the one that listens
    /// after it. The listening instance stays in its slot while this call waits, so an accept
    /// dropped mid-wait (a timeout) leaves it listening for the next one. A replacement that
    /// cannot be created leaves the slot empty, and the next accept creates it, failing there.
    pub(crate) async fn accept(&self) -> io::Result<NamedPipeServer> {
        let mut pending = self.pending.lock().await;
        let listening = match pending.take() {
            Some(listening) => listening,
            None => ServerOptions::new().create(&self.name)?,
        };
        if let Err(error) = pending.insert(listening).connect().await {
            // A client that came and went leaves the instance unusable.
            *pending = None;
            return Err(error);
        }
        let replacement = ServerOptions::new().create(&self.name).ok();
        Ok(std::mem::replace(&mut *pending, replacement)
            .expect("the slot holds the instance that just connected"))
    }
}

/// Open a client connection; a busy pipe (`ERROR_PIPE_BUSY`) is retried until the server frees one
/// (Node's clients wait the same way through libuv's connect machinery).
pub(crate) async fn connect(name: &str) -> io::Result<NamedPipeClient> {
    let deadline = tokio::time::Instant::now() + BUSY_RETRY_DEADLINE;
    loop {
        match ClientOptions::new().open(name) {
            Ok(client) => return Ok(client),
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
    client: AsyncMutex<NamedPipeClient>,
    /// Read deadline for pending reads; `None` blocks indefinitely.
    read_timeout: Mutex<Option<Duration>>,
    /// Declared last so the pipe drops before its runtime shuts down.
    runtime: ClientRuntime,
}

/// A blocking client's own runtime. It shuts down in the background when it drops: the client
/// is dropped from threads that drive a runtime too, where tokio refuses the blocking shutdown
/// a dropped `Runtime` performs.
struct ClientRuntime(Option<Runtime>);

impl ClientRuntime {
    fn get(&self) -> &Runtime {
        self.0
            .as_ref()
            .expect("a client's runtime lives until the client drops")
    }
}

impl Drop for ClientRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
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

/// Drive `future` to completion on a blocking client's own runtime. Tokio refuses to block a
/// thread that already drives a runtime (the CLI's async entry points call the blocking client
/// from one, as they call the unix stream, which simply blocks), so on such a thread the
/// client's runtime runs on a scoped thread while this one waits.
fn drive<F>(runtime: &Runtime, future: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    if tokio::runtime::Handle::try_current().is_err() {
        return runtime.block_on(future);
    }
    std::thread::scope(|scope| scope.spawn(|| runtime.block_on(future)).join())
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

impl BlockingPipeClient {
    /// Connect to the pipe at `name`, blocking until connected.
    pub(crate) fn connect(name: &str) -> io::Result<Self> {
        let runtime = ClientRuntime(Some(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        ));
        let client = drive(runtime.get(), connect(name))?;
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
            // Construct the timer only after entering this client's runtime.
            Some(timeout) => match drive(self.state.runtime.get(), async {
                tokio::time::timeout(timeout, read).await
            }) {
                Ok(result) => result,
                // Cancelled reads leave the buffer untouched; callers treat
                // the deadline as an empty poll, like Unix `SO_RCVTIMEO`.
                Err(_elapsed) => Err(io::ErrorKind::TimedOut.into()),
            },
            None => drive(self.state.runtime.get(), read),
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
        drive(self.state.runtime.get(), async {
            self.state.client.lock().await.write(buf).await
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        drive(self.state.runtime.get(), async {
            self.state.client.lock().await.flush().await
        })
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

    /// A client that connects while an accept waits is the one that accept hands out, round
    /// after round: one instance listens, so no client lands on an instance no accept waits on.
    #[tokio::test]
    async fn the_waiting_accept_hands_out_the_client_that_connects_then() {
        use std::future::Future as _;
        let name = test_pipe("one-listening").to_string_lossy().into_owned();
        let listener = super::NamedPipeListener::bind(&name).expect("bind");
        for round in 0..4 {
            let mut accept = std::pin::pin!(listener.accept());
            let first_poll =
                std::future::poll_fn(|cx| std::task::Poll::Ready(accept.as_mut().poll(cx))).await;
            assert!(first_poll.is_pending(), "round {round}: no client yet");
            let _client = super::ClientOptions::new().open(&name).expect("connect");
            tokio::time::timeout(Duration::from_secs(5), accept)
                .await
                .unwrap_or_else(|_| {
                    panic!("round {round}: the waiting accept never got the client")
                })
                .expect("accept");
        }
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

    /// The CLI's async entry points use and drop the blocking client on a thread that drives a
    /// runtime; tokio refuses a nested `block_on` and a blocking runtime shutdown there, which
    /// panicked `--continue`.
    #[test]
    fn a_blocking_client_works_on_a_thread_that_drives_a_runtime() {
        let path = test_pipe("blocking-in-runtime");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let server_path = path.clone();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("server runtime");
            runtime.block_on(async move {
                let listener = bind_transport(&server_path).await.expect("bind");
                ready_tx.send(()).expect("listener ready");
                let server = listener.accept().await.expect("accept");
                let (mut reader, mut writer) = server.split();
                let mut buf = [0u8; 4];
                reader.read_exact(&mut buf).await.expect("server read");
                writer.write_all(&buf).await.expect("server write");
            });
        });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener ready");
        let client_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("client runtime");
        let echoed = client_runtime.block_on(async {
            let mut stream = connect_blocking(&path).expect("blocking connect");
            stream
                .set_read_timeout(Duration::from_secs(5))
                .expect("deadline");
            stream.write_all(b"ping").expect("client write");
            stream.flush().expect("client flush");
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).expect("client read");
            buf
        });
        assert_eq!(&echoed, b"ping");
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
