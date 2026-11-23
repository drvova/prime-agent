//! Async pipe reading utilities for Unix.

use std::io;
use std::os::unix::io::OwnedFd;

use tokio::net::unix::pipe;

pub(crate) struct AsyncPipeReader(pipe::Receiver);

impl AsyncPipeReader {
    pub(crate) fn new(reader: std::io::PipeReader) -> io::Result<Self> {
        Ok(Self(pipe::Receiver::from_file(std::fs::File::from(
            OwnedFd::from(reader),
        ))?))
    }

    /// Prime Agent patch: read to the end, keeping at most `limit` bytes.
    /// `None` when there were more: the rest is read and dropped, so the
    /// writer runs to its end as it would have.
    pub(crate) async fn read_to_string(&mut self, limit: usize) -> io::Result<Option<String>> {
        use tokio::io::AsyncReadExt;
        let mut kept = Some(Vec::new());
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let read = self.0.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            kept = kept.filter(|kept| kept.len() + read <= limit).map(|mut kept| {
                kept.extend_from_slice(&chunk[..read]);
                kept
            });
        }
        kept.map(|kept| {
            String::from_utf8(kept).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        })
        .transpose()
    }
}
