//! Async pipe reading utilities for non-Unix platforms.
//!
//! Uses `spawn_blocking` internally for the I/O operation only,
//! not for the entire subshell execution.

use std::io::{self, Read};

pub(crate) struct AsyncPipeReader {
    inner: Option<std::io::PipeReader>,
}

impl AsyncPipeReader {
    pub(crate) fn new(fd: std::io::PipeReader) -> io::Result<Self> {
        Ok(Self { inner: Some(fd) })
    }

    /// Prime Agent patch: read to the end, keeping at most `limit` bytes.
    /// `None` when there were more: the rest is read and dropped, so the
    /// writer runs to its end as it would have.
    pub(crate) async fn read_to_string(&mut self, limit: usize) -> io::Result<Option<String>> {
        let Some(mut reader) = self.inner.take() else {
            return Ok(Some(String::new()));
        };

        tokio::task::spawn_blocking(move || {
            let mut kept = Some(Vec::new());
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let read = reader.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                kept = kept.filter(|kept| kept.len() + read <= limit).map(|mut kept| {
                    kept.extend_from_slice(&chunk[..read]);
                    kept
                });
            }
            kept.map(|kept| {
                String::from_utf8(kept)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            })
            .transpose()
        })
        .await
        .map_err(io::Error::other)?
    }
}
