//! One text file as `fs.read`/`fs.edit` see it: decoded lines with their own
//! line terminators, the encoding to write back with, and the atomic
//! replace that lands an edit.
//!
//! Text is UTF-8 (with or without a byte-order mark) or UTF-16 with a mark.
//! A NUL byte, invalid UTF-8, or a broken UTF-16 stream refuses the file as
//! binary or undecodable instead of guessing an encoding: an edit must write
//! back exactly the bytes it did not change.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};

/// Largest file `fs.read`/`fs.edit` load (bytes).
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
const UTF16LE_BOM: &[u8] = &[0xFF, 0xFE];
const UTF16BE_BOM: &[u8] = &[0xFE, 0xFF];

/// The encoding a file decoded from and writes back with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf8Bom => "utf-8-bom",
            Self::Utf16Le => "utf-16le",
            Self::Utf16Be => "utf-16be",
        }
    }
}

/// One line's terminator. Only a file's last line can have none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Eol {
    Lf,
    CrLf,
    None,
}

impl Eol {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::None => "",
        }
    }
}

/// How a file ends its lines, as `fs.read` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineEnds {
    Lf,
    CrLf,
    Mixed,
    /// No line has a terminator (empty, or one line without a final newline).
    Unterminated,
}

impl LineEnds {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Lf => "lf",
            Self::CrLf => "crlf",
            Self::Mixed => "mixed",
            Self::Unterminated => "none",
        }
    }
}

/// One line: its text (without the terminator) and its terminator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) text: String,
    pub(crate) eol: Eol,
}

/// A decoded text file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Document {
    pub(crate) encoding: Encoding,
    pub(crate) lines: Vec<Line>,
}

/// Why bytes are not editable text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotText {
    /// A NUL byte (or NUL character in UTF-16): a binary file.
    Binary,
    /// Neither valid UTF-8 nor UTF-16 with a byte-order mark.
    Undecodable,
}

impl Document {
    /// Decode file bytes.
    ///
    /// # Errors
    ///
    /// [`NotText`] when the bytes are binary or in no supported encoding.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, NotText> {
        let (encoding, text) = if let Some(body) = bytes.strip_prefix(UTF16LE_BOM) {
            (Encoding::Utf16Le, decode_utf16(body, u16::from_le_bytes)?)
        } else if let Some(body) = bytes.strip_prefix(UTF16BE_BOM) {
            (Encoding::Utf16Be, decode_utf16(body, u16::from_be_bytes)?)
        } else {
            let (encoding, body) = match bytes.strip_prefix(UTF8_BOM) {
                Some(body) => (Encoding::Utf8Bom, body),
                None => (Encoding::Utf8, bytes),
            };
            if body.contains(&0) {
                return Err(NotText::Binary);
            }
            let text = std::str::from_utf8(body).map_err(|_| NotText::Undecodable)?;
            (encoding, text.to_string())
        };
        if text.contains('\0') {
            return Err(NotText::Binary);
        }
        let lines = text
            .split_inclusive('\n')
            .map(|segment| match segment.strip_suffix('\n') {
                Some(body) => match body.strip_suffix('\r') {
                    Some(body) => Line {
                        text: body.to_string(),
                        eol: Eol::CrLf,
                    },
                    None => Line {
                        text: body.to_string(),
                        eol: Eol::Lf,
                    },
                },
                None => Line {
                    text: segment.to_string(),
                    eol: Eol::None,
                },
            })
            .collect();
        Ok(Self { encoding, lines })
    }

    /// The bytes this document writes: its mark, then every line and its own
    /// terminator, in its encoding.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut text = String::new();
        for line in &self.lines {
            text.push_str(&line.text);
            text.push_str(line.eol.as_str());
        }
        match self.encoding {
            Encoding::Utf8 => text.into_bytes(),
            Encoding::Utf8Bom => [UTF8_BOM, text.as_bytes()].concat(),
            Encoding::Utf16Le => UTF16LE_BOM
                .iter()
                .copied()
                .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
                .collect(),
            Encoding::Utf16Be => UTF16BE_BOM
                .iter()
                .copied()
                .chain(text.encode_utf16().flat_map(u16::to_be_bytes))
                .collect(),
        }
    }

    /// How the file ends its lines.
    pub(crate) fn line_ends(&self) -> LineEnds {
        let crlf = self
            .lines
            .iter()
            .filter(|line| line.eol == Eol::CrLf)
            .count();
        let lf = self.lines.iter().filter(|line| line.eol == Eol::Lf).count();
        match (lf, crlf) {
            (0, 0) => LineEnds::Unterminated,
            (_, 0) => LineEnds::Lf,
            (0, _) => LineEnds::CrLf,
            (_, _) => LineEnds::Mixed,
        }
    }

    /// The terminator new lines take: the file's majority (LF on a tie or in
    /// a file with none).
    pub(crate) fn majority_eol(&self) -> Eol {
        let crlf = self
            .lines
            .iter()
            .filter(|line| line.eol == Eol::CrLf)
            .count();
        let lf = self.lines.iter().filter(|line| line.eol == Eol::Lf).count();
        if crlf > lf {
            Eol::CrLf
        } else {
            Eol::Lf
        }
    }

    /// The file ends with a line terminator (an empty file counts as ending
    /// with one: a first line written into it gets one).
    pub(crate) fn final_newline(&self) -> bool {
        self.lines.last().is_none_or(|line| line.eol != Eol::None)
    }
}

fn decode_utf16(body: &[u8], unit: fn([u8; 2]) -> u16) -> Result<String, NotText> {
    let (pairs, rest) = body.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(NotText::Undecodable);
    }
    let units: Vec<u16> = pairs.iter().map(|pair| unit(*pair)).collect();
    String::from_utf16(&units).map_err(|_| NotText::Undecodable)
}

/// A file loaded for reading or editing.
#[derive(Debug)]
pub(crate) struct Loaded {
    /// The bytes on disk, compared again just before an edit lands.
    pub(crate) bytes: Vec<u8>,
    pub(crate) document: Document,
}

/// Load `path` (shown as `shown` in errors) as text.
///
/// # Errors
///
/// A missing path, a directory or other non-regular file, a file over the
/// size cap, a binary file, or one in no supported encoding.
pub(crate) fn load(path: &Path, shown: &str) -> anyhow::Result<Loaded> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("{shown} does not exist")
        }
        Err(error) => return Err(anyhow!(error).context(format!("{shown}: cannot stat"))),
    };
    if metadata.is_dir() {
        bail!("{shown} is a directory");
    }
    if !metadata.is_file() {
        bail!("{shown} is not a regular file");
    }
    if metadata.len() > MAX_FILE_BYTES {
        bail!(
            "{shown} is {} bytes; files over {MAX_FILE_BYTES} bytes are not read or edited",
            metadata.len()
        );
    }
    let bytes = std::fs::read(path).with_context(|| format!("{shown}: cannot read"))?;
    let document = Document::decode(&bytes).map_err(|reason| match reason {
        NotText::Binary => anyhow!("{shown} is a binary file"),
        NotText::Undecodable => {
            anyhow!("{shown} is not UTF-8 text (or UTF-16 with a byte-order mark)")
        }
    })?;
    Ok(Loaded { bytes, document })
}

/// Replace `path` with `bytes` atomically: write a temp file beside the real
/// file (through a symlink, so the link survives), carry its permissions
/// over, flush it, and rename it onto the file. When the file no longer holds
/// `expected` (another writer got there between the check and the write),
/// nothing is replaced.
///
/// # Errors
///
/// The file changed since it was loaded, or an I/O failure (the temp file is
/// removed).
pub(crate) fn replace_atomically(
    path: &Path,
    shown: &str,
    expected: &[u8],
    bytes: &[u8],
) -> anyhow::Result<()> {
    let target = std::fs::canonicalize(path).with_context(|| format!("{shown}: cannot resolve"))?;
    let permissions = std::fs::metadata(&target)
        .with_context(|| format!("{shown}: cannot stat"))?
        .permissions();
    let name = target
        .file_name()
        .ok_or_else(|| anyhow!("{shown} has no file name"))?
        .to_string_lossy()
        .into_owned();
    let temp: PathBuf = target.with_file_name(format!(
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
        file.set_permissions(permissions)
            .with_context(|| format!("{shown}: cannot copy permissions"))?;
        file.sync_all()
            .with_context(|| format!("{shown}: cannot flush the temp file"))?;
        drop(file);
        let current = std::fs::read(&target).with_context(|| format!("{shown}: cannot re-read"))?;
        if current != expected {
            bail!("{shown} changed while the edit was being applied; nothing was written");
        }
        crate::platform::rename_onto(&temp, &target)
            .with_context(|| format!("{shown}: cannot replace the file"))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}
