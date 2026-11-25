//! `export-jsonl`: write transcripts back out as JSONL session files (the v3
//! format), byte for byte what the store holds, so a transcript imported from
//! a file comes back as that file's bytes.

use std::io;
use std::path::{Component, Path, PathBuf, Prefix};

use super::{read_first_line_bounded, store, stored_locators, TranscriptStore};

/// The longest header line read to find a transcript's session id.
const HEADER_BYTES: usize = 64 * 1024;

/// Which transcripts [`export_jsonl`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportSelection<'a> {
    /// The transcripts whose header names this session id (or an id it is
    /// the only prefix of), each to `<out>/<file name>`.
    Session(&'a str),
    /// Every transcript, to `<out>/<its path under the agent dir>` (one
    /// outside it to `<out>/other/<its absolute path>`).
    All,
}

/// What [`export_jsonl`] wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// Files written.
    pub written: Vec<PathBuf>,
    /// Files already there with the same bytes.
    pub unchanged: Vec<PathBuf>,
    /// Transcripts not written, and why.
    pub failed: Vec<(PathBuf, String)>,
}

/// A transcript found to export: its locator (as listed, and canonical)
/// and its header's session id.
struct Found {
    locator: PathBuf,
    canonical: PathBuf,
    session: String,
}

/// The transcripts under `roots` (their trees, through the store's listing)
/// and every one the database holds, with their session ids; files that do
/// not start with a session header are not transcripts.
fn transcripts(
    store: &dyn TranscriptStore,
    mut locators: Vec<PathBuf>,
    roots: &[PathBuf],
) -> Vec<Found> {
    let mut dirs: Vec<PathBuf> = roots.to_vec();
    while let Some(dir) = dirs.pop() {
        let Ok(listing) = store.list(&dir) else {
            continue;
        };
        dirs.extend(listing.directories);
        locators.extend(
            listing
                .transcripts
                .into_iter()
                .filter(|listed| listed.listed_as_transcript)
                .map(|listed| listed.locator),
        );
    }
    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();
    for locator in locators {
        let canonical =
            pa_types::platform::canonicalize(locator.parent().unwrap_or(Path::new(".")))
                .map_or_else(
                    |_| locator.clone(),
                    |dir| dir.join(locator.file_name().unwrap_or_default()),
                );
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let Ok(mut reader) = store.open(&locator) else {
            continue;
        };
        let Some(header) = read_first_line_bounded(&mut *reader, HEADER_BYTES) else {
            continue;
        };
        let Ok(serde_json::Value::Object(header)) = serde_json::from_slice(&header) else {
            continue;
        };
        let (Some("session"), Some(id)) = (
            header.get("type").and_then(serde_json::Value::as_str),
            header.get("id").and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        found.push(Found {
            locator,
            canonical,
            session: id.to_string(),
        });
    }
    found.sort_by(|left, right| left.locator.cmp(&right.locator));
    found
}

/// Where an exported transcript goes under `out`.
fn destination(locator: &Path, base: &Path, out: &Path, selection: ExportSelection<'_>) -> PathBuf {
    match selection {
        ExportSelection::Session(_) => out.join(locator.file_name().unwrap_or_default()),
        ExportSelection::All => match locator.strip_prefix(base) {
            Ok(relative) => out.join(relative),
            Err(_) => out.join("other").join(
                locator
                    .components()
                    .filter_map(|component| match component {
                        Component::Normal(part) => Some(part.to_os_string()),
                        // A Windows drive is kept as a directory of its letter.
                        Component::Prefix(prefix) => match prefix.kind() {
                            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                                Some(char::from(letter).to_string().into())
                            }
                            Prefix::Verbatim(_)
                            | Prefix::VerbatimUNC(..)
                            | Prefix::DeviceNS(_)
                            | Prefix::UNC(..) => None,
                        },
                        Component::RootDir | Component::CurDir | Component::ParentDir => None,
                    })
                    .collect::<PathBuf>(),
            ),
        },
    }
}

/// Export the selected transcripts found under `roots` (the session trees,
/// e.g. `<agent dir>/sessions` and `sessions-archive`) or held by the
/// database, as JSONL files under `out`. `base` (the agent dir) is the root
/// whose layout [`ExportSelection::All`] keeps. A file already at a
/// destination is never overwritten: with the same bytes it counts as
/// unchanged, with other bytes it is a failure.
///
/// # Errors
///
/// The database could not be listed, or a session id matches no transcript
/// or more than one session.
pub fn export_jsonl(
    roots: &[PathBuf],
    base: &Path,
    selection: ExportSelection<'_>,
    out: &Path,
) -> io::Result<ExportReport> {
    export_from(store(), stored_locators()?, roots, base, selection, out)
}

/// [`export_jsonl`] from `store`, whose database holds `stored`.
pub(super) fn export_from(
    store: &dyn TranscriptStore,
    stored: Vec<PathBuf>,
    roots: &[PathBuf],
    base: &Path,
    selection: ExportSelection<'_>,
    out: &Path,
) -> io::Result<ExportReport> {
    let mut found = transcripts(store, stored, roots);
    if let ExportSelection::Session(wanted) = selection {
        let exact = found.iter().any(|transcript| transcript.session == wanted);
        found.retain(|transcript| {
            if exact {
                transcript.session == wanted
            } else {
                transcript.session.starts_with(wanted)
            }
        });
        let mut sessions: Vec<&str> = found.iter().map(|found| found.session.as_str()).collect();
        sessions.sort_unstable();
        sessions.dedup();
        match sessions.as_slice() {
            [] => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no saved session has the id {wanted}"),
                ))
            }
            [_] => {}
            [..] => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{wanted} is the start of several session ids: {}",
                        sessions.join(", ")
                    ),
                ))
            }
        }
    }
    let base = pa_types::platform::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    let mut report = ExportReport::default();
    for transcript in found {
        let target = destination(&transcript.canonical, &base, out, selection);
        let written = store.read(&transcript.locator).and_then(|bytes| {
            match std::fs::read(&target) {
                Ok(existing) if existing == bytes => return Ok(false),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists with other content", target.display()),
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, bytes).map(|()| true)
        });
        match written {
            Ok(true) => report.written.push(target),
            Ok(false) => report.unchanged.push(target),
            Err(error) => report.failed.push((transcript.locator, error.to_string())),
        }
    }
    Ok(report)
}
