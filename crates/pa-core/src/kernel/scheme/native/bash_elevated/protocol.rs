//! The wire between a session and its elevated helper (Windows): framing,
//! messages, and the checks each side makes before it trusts the other.
//!
//! A frame is a 4-byte little-endian length, then that many bytes of one
//! JSON [`Message`]. A frame over [`MAX_FRAME`] is refused, so a peer cannot
//! make the other side allocate without bound.
//!
//! The handshake, in order:
//!
//! 1. The session creates the pipe and starts the helper through UAC with
//!    the pipe's name, the session process's id and a one-time token.
//! 2. The helper connects and asks Windows which process serves the pipe;
//!    one that is not the process it was told of is refused
//!    ([`check_server`]).
//! 3. The helper sends [`Message::Hello`] with the token and its own id.
//! 4. The session asks Windows which process connected; it must be the
//!    process the UAC launch started, and the hello must name that process
//!    and carry the token ([`check_hello`]). Anything else closes the pipe.
//! 5. The session sends [`Message::Welcome`]; from then on it sends
//!    [`Message::Run`] and [`Message::Cancel`], and the helper answers each
//!    run with [`Message::Started`], [`Message::Output`] and one
//!    [`Message::Exit`] or [`Message::Failed`].
//!
//! This module is the same on every platform, so it is tested everywhere
//! (off Windows only the tests use it: its declaration allows the dead code).

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The version both sides speak; a hello with another is refused.
pub(super) const PROTOCOL: u32 = 1;
/// The largest frame either side accepts.
pub(super) const MAX_FRAME: usize = 4 * 1024 * 1024;
/// The most output bytes one [`Message::Output`] carries.
pub(super) const OUTPUT_CHUNK: usize = 64 * 1024;

/// One message on the pipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Message {
    /// Helper to session, first: who it is and the token it was given.
    Hello {
        protocol: u32,
        token: String,
        pid: u32,
    },
    /// Session to helper: the hello was accepted.
    Welcome { protocol: u32 },
    /// Session to helper: run `script` in `cwd` with exactly `env`, on the
    /// shell program `shell` (`<shell> -c <script>`), or on the built-in
    /// shell without one.
    Run {
        id: u64,
        script: String,
        shell: Option<String>,
        cwd: String,
        env: Vec<(String, String)>,
    },
    /// Session to helper: end run `id` and everything it started.
    Cancel { id: u64 },
    /// Helper to session: run `id` is running.
    Started { id: u64 },
    /// Helper to session: output of run `id`, stdout and stderr together,
    /// base64.
    Output { id: u64, data: String },
    /// Helper to session: run `id` ended with `code`.
    Exit { id: u64, code: i32 },
    /// Helper to session: run `id` could not run.
    Failed { id: u64, error: String },
}

impl Message {
    /// An output message carrying `bytes`.
    pub(super) fn output(id: u64, bytes: &[u8]) -> Self {
        Message::Output {
            id,
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }
}

/// Why one side refused the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Refusal {
    /// The first message was not a hello of this protocol.
    NotHello,
    /// The token was not the one this session gave.
    WrongToken,
    /// The process on the other end is not the one expected.
    WrongProcess { expected: u32, found: u32 },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotHello => formatter.write_str("the peer did not open with a hello"),
            Refusal::WrongToken => formatter.write_str("the peer's token is wrong"),
            Refusal::WrongProcess { expected, found } => write!(
                formatter,
                "the peer is process {found}, not process {expected}"
            ),
        }
    }
}

/// The session's check of a helper's first message: `hello` must be a hello
/// of [`PROTOCOL`] carrying `token` and naming `client_pid`, the process
/// Windows says is on the pipe, which must be `launched`, the process the
/// UAC launch started.
///
/// # Errors
///
/// The [`Refusal`] that closes the pipe.
pub(super) fn check_hello(
    hello: &Message,
    token: &str,
    launched: u32,
    client_pid: u32,
) -> Result<(), Refusal> {
    let Message::Hello {
        protocol: PROTOCOL,
        token: offered,
        pid,
    } = hello
    else {
        return Err(Refusal::NotHello);
    };
    if client_pid != launched {
        return Err(Refusal::WrongProcess {
            expected: launched,
            found: client_pid,
        });
    }
    if *pid != client_pid {
        return Err(Refusal::WrongProcess {
            expected: client_pid,
            found: *pid,
        });
    }
    // Compared in full whatever differs, so the time taken says nothing of
    // where.
    let matches = offered.len() == token.len()
        && offered
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |differs, (left, right)| differs | (left ^ right))
            == 0;
    if matches {
        Ok(())
    } else {
        Err(Refusal::WrongToken)
    }
}

/// The helper's check of the pipe it connected to: the process Windows says
/// serves it must be the session's (`expected`), or another program created
/// the name first.
///
/// # Errors
///
/// [`Refusal::WrongProcess`].
pub(super) fn check_server(server_pid: u32, expected: u32) -> Result<(), Refusal> {
    if server_pid == expected {
        Ok(())
    } else {
        Err(Refusal::WrongProcess {
            expected,
            found: server_pid,
        })
    }
}

/// Decode an output message's bytes.
///
/// # Errors
///
/// The data is not base64.
pub(super) fn output_bytes(data: &str) -> std::io::Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(std::io::Error::other)
}

/// Write one frame.
///
/// # Errors
///
/// The message is over [`MAX_FRAME`], or the write failed.
pub(super) async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &Message,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    let length = u32::try_from(body.len())
        .ok()
        .filter(|length| *length as usize <= MAX_FRAME)
        .ok_or_else(|| std::io::Error::other("frame too large"))?;
    let mut frame = length.to_le_bytes().to_vec();
    frame.extend_from_slice(&body);
    writer.write_all(&frame).await?;
    writer.flush().await
}

/// Read one frame: `None` when the pipe closed between frames.
///
/// # Errors
///
/// The pipe closed inside a frame, a frame is over [`MAX_FRAME`], or its
/// body is not a message.
pub(super) async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<Option<Message>> {
    let mut length = [0u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(std::io::Error::other(format!(
            "a frame of {length} bytes is over the limit of {MAX_FRAME}"
        )));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every message survives the wire as it was sent, a closed pipe between
    /// frames reads as the end, and an oversized or cut-off frame is an error.
    #[tokio::test]
    async fn frames_carry_messages_and_refuse_oversized_or_cut_ones() {
        let messages = vec![
            Message::Hello {
                protocol: PROTOCOL,
                token: "t".repeat(64),
                pid: 7,
            },
            Message::Welcome { protocol: PROTOCOL },
            Message::Run {
                id: 1,
                script: "netsh advfirewall show allprofiles".to_string(),
                shell: None,
                cwd: r"C:\work".to_string(),
                env: vec![("PATH".to_string(), r"C:\Windows".to_string())],
            },
            Message::Cancel { id: 1 },
            Message::Started { id: 1 },
            Message::output(1, b"ok\n\xff"),
            Message::Exit { id: 1, code: 3 },
            Message::Failed {
                id: 2,
                error: "no".to_string(),
            },
        ];
        let mut wire = Vec::new();
        for message in &messages {
            write_frame(&mut wire, message).await.unwrap();
        }
        let mut reader = wire.as_slice();
        let mut read = Vec::new();
        while let Some(message) = read_frame(&mut reader).await.unwrap() {
            read.push(message);
        }
        let Some(Message::Output { data, .. }) = read.get(5) else {
            panic!("the output message is missing: {read:?}");
        };
        let oversized = u32::try_from(MAX_FRAME + 1).unwrap().to_le_bytes();
        let cut = &wire[..wire.len() - 1];
        assert_eq!(
            (
                read == messages,
                output_bytes(data).unwrap(),
                read_frame(&mut oversized.as_slice()).await.is_err(),
                read_frame(&mut &cut[cut.len() - 10..]).await.is_err(),
            ),
            (true, b"ok\n\xff".to_vec(), true, true)
        );
    }

    /// The session accepts only a hello of this protocol, from the process
    /// its UAC launch started, naming itself, with the session's token; the
    /// helper accepts only the session's own process as the pipe's server.
    #[test]
    fn a_wrong_token_or_process_is_refused() {
        let token = "a".repeat(64);
        let hello = |token: &str, pid| Message::Hello {
            protocol: PROTOCOL,
            token: token.to_string(),
            pid,
        };
        assert_eq!(
            [
                check_hello(&hello(&token, 40), &token, 40, 40),
                check_hello(&hello(&"b".repeat(64), 40), &token, 40, 40),
                check_hello(&hello("a", 40), &token, 40, 40),
                check_hello(&hello(&token, 40), &token, 40, 41),
                check_hello(&hello(&token, 41), &token, 40, 40),
                check_hello(
                    &Message::Hello {
                        protocol: PROTOCOL + 1,
                        token: token.clone(),
                        pid: 40,
                    },
                    &token,
                    40,
                    40
                ),
                check_hello(&Message::Started { id: 1 }, &token, 40, 40),
                check_server(9, 9),
                check_server(10, 9),
            ],
            [
                Ok(()),
                Err(Refusal::WrongToken),
                Err(Refusal::WrongToken),
                Err(Refusal::WrongProcess {
                    expected: 40,
                    found: 41
                }),
                Err(Refusal::WrongProcess {
                    expected: 40,
                    found: 41
                }),
                Err(Refusal::NotHello),
                Err(Refusal::NotHello),
                Ok(()),
                Err(Refusal::WrongProcess {
                    expected: 9,
                    found: 10
                }),
            ]
        );
    }
}
