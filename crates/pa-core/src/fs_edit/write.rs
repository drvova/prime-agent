//! `fs.write`: create a file, or replace a whole file, with the text given.
//!
//! The text is written as it is (UTF-8, its own line endings). Missing parent
//! directories are created. The write is atomic: a temp file beside the
//! target, flushed, then renamed onto it, so a reader never sees half a file
//! and an existing file keeps its permissions.

use std::io::Write as _;

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};

use super::document::{self, Document};
use super::patch::Rewrite;
use super::{parse_target, EditFact, Target};

/// One `fs.write` to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriteRequest {
    pub(crate) target: Target,
    pub(crate) text: String,
}

/// Parse the `fs.write` payload.
///
/// # Errors
///
/// A missing or relative path, or `text` that is not a string.
pub(crate) fn parse_write(data: &Value) -> anyhow::Result<WriteRequest> {
    let target = parse_target(data)?;
    let text = data
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("text must be a string"))?
        .to_string();
    Ok(WriteRequest { target, text })
}

/// Run one `fs.write`: the reply, its telemetry fact, and, when a text file
/// was replaced or created, its lines before (none for a new file) and after
/// (the seen-files record and the cell's card).
///
/// # Errors
///
/// The target is a directory or not a regular file, or an I/O failure:
/// nothing is left half-written.
pub(crate) fn write(request: &WriteRequest) -> anyhow::Result<(Value, EditFact, Option<Rewrite>)> {
    let shown = &request.target.shown;
    let path = &request.target.path;
    let bytes = request.text.as_bytes();
    let after = Document::decode(bytes)
        .map_err(|_| anyhow!("text holds a NUL character; {shown} was not written"))?;
    let lines = after.lines.len();
    let after: Vec<String> = after.lines.into_iter().map(|line| line.text).collect();
    let existing = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => bail!("{shown} is a directory"),
        Ok(metadata) if !metadata.is_file() => bail!("{shown} is not a regular file"),
        Ok(_) => Some(std::fs::read(path).with_context(|| format!("{shown}: cannot read"))?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(anyhow!(error).context(format!("{shown}: cannot stat"))),
    };
    let rewrite = if let Some(before) = &existing {
        document::replace_atomically(path, shown, before, bytes)?;
        // A file that was not text had no seen version to bring forward.
        Document::decode(before).ok().map(|before| Rewrite {
            before: before.lines.into_iter().map(|line| line.text).collect(),
            after,
        })
    } else {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("{shown} has no parent directory"))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("{shown}: cannot create its directory"))?;
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("{shown} has no file name"))?
            .to_string_lossy()
            .into_owned();
        let temp = path.with_file_name(format!(
            ".{name}.{}.pa-edit.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let written = (|| -> anyhow::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .with_context(|| format!("{shown}: cannot create a temp file beside it"))?;
            file.write_all(bytes)
                .with_context(|| format!("{shown}: cannot write the temp file"))?;
            file.sync_all()
                .with_context(|| format!("{shown}: cannot flush the temp file"))?;
            drop(file);
            // Another writer may have created the file meanwhile: it is not replaced.
            if path.exists() {
                bail!("{shown} appeared while it was being created; nothing was written");
            }
            crate::platform::rename_onto(&temp, path)
                .with_context(|| format!("{shown}: cannot move the file into place"))
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        written?;
        // From no lines: only a seen version of an empty file, since
        // deleted, matches that and moves forward to what was written.
        Some(Rewrite {
            before: Vec::new(),
            after,
        })
    };
    let created = existing.is_none();
    let text = format!(
        "{shown}: {}, {lines} line{}, {} bytes",
        if created { "created" } else { "overwritten" },
        if lines == 1 { "" } else { "s" },
        bytes.len()
    );
    Ok((
        json!({
            "path": shown,
            "created": created,
            "totalLines": lines,
            "bytes": bytes.len(),
            "text": text,
        }),
        EditFact::Written { created },
        rewrite,
    ))
}
