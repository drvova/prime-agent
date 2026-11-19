//! Image file paths in pasted text: a terminal's drag-and-drop types the dropped file's path, and
//! a "copy as path" pastes it, so a paste that is exactly one image path attaches the image (omp's
//! `extractImagePathFromText`). The check here is textual — the caller loads the file and keeps
//! the paste as text when it is not a readable image.

use std::path::PathBuf;

/// The extensions a pasted path must carry (the formats the attachment path sends).
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// Which platform's path forms a paste is read with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathFlavor {
    /// Unix and macOS: a leading `/` is the filesystem root.
    Posix,
    /// Windows, where Git Bash (MSYS) and Cygwin spell `C:\` as `/c/` and `/cygdrive/c/`.
    Windows,
}

impl PathFlavor {
    /// The flavor of the platform this build runs on.
    pub(crate) const CURRENT: PathFlavor = if cfg!(windows) {
        PathFlavor::Windows
    } else {
        PathFlavor::Posix
    };
}

/// The one image file path `text` is, or `None` for any other text. Accepted forms, quoted with
/// `"` or `'` or not: an absolute POSIX path (`/…`, with `\ ` shell escapes undone), a home path
/// (`~/…`), a Windows drive (`C:\…`, `C:/…`) or UNC (`\\server\…`) path, a `file://` URL, and on
/// Windows the MSYS / Cygwin drive forms (`/c/…`, `/cygdrive/c/…`).
pub(crate) fn pasted_image_path(text: &str, flavor: PathFlavor) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains(['\n', '\r']) {
        return None;
    }
    let unquoted = ['"', '\'']
        .iter()
        .find_map(|quote| {
            trimmed
                .strip_prefix(*quote)
                .and_then(|rest| rest.strip_suffix(*quote))
        })
        .unwrap_or(trimmed);
    if unquoted.is_empty() || unquoted.contains(['"', '\0']) {
        return None;
    }
    let path = if unquoted.starts_with("file://") {
        url::Url::parse(unquoted).ok()?.to_file_path().ok()?
    } else if is_windows_path(unquoted) {
        PathBuf::from(unquoted)
    } else if let Some(rest) = unquoted.strip_prefix("~/") {
        pa_types::platform::home_dir()?.join(unescape_shell(rest))
    } else if unquoted.starts_with('/') {
        let posix = unescape_shell(unquoted);
        match flavor {
            PathFlavor::Posix => PathBuf::from(posix),
            PathFlavor::Windows => {
                let msys = posix.strip_prefix("/cygdrive").unwrap_or(&posix);
                let bytes = msys.as_bytes();
                if bytes.len() > 3
                    && bytes[0] == b'/'
                    && bytes[1].is_ascii_alphabetic()
                    && bytes[2] == b'/'
                {
                    let drive = char::from(bytes[1].to_ascii_uppercase());
                    PathBuf::from(format!("{drive}:\\{}", msys[3..].replace('/', "\\")))
                } else {
                    PathBuf::from(posix)
                }
            }
        }
    } else {
        return None;
    };
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    IMAGE_EXTENSIONS
        .contains(&extension.as_str())
        .then_some(path)
}

/// A drive-letter (`C:\`, `C:/`) or UNC (`\\server\share`) path.
fn is_windows_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.starts_with(r"\\")
        || (bytes.len() > 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

/// Undo a POSIX terminal's drag-and-drop escaping (`My\ Shot.png`): a backslash keeps the
/// character after it.
fn unescape_shell(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_image_paths_in_every_drop_form_resolve() {
        let cases: Vec<(&str, PathFlavor, Option<PathBuf>)> = vec![
            (
                "/tmp/shot.png",
                PathFlavor::Posix,
                Some(PathBuf::from("/tmp/shot.png")),
            ),
            (
                "  /tmp/shot.PNG\n",
                PathFlavor::Posix,
                Some(PathBuf::from("/tmp/shot.PNG")),
            ),
            (
                "'/tmp/My Shot.jpeg'",
                PathFlavor::Posix,
                Some(PathBuf::from("/tmp/My Shot.jpeg")),
            ),
            (
                "/tmp/My\\ Shot\\ (2).webp",
                PathFlavor::Posix,
                Some(PathBuf::from("/tmp/My Shot (2).webp")),
            ),
            // On a POSIX host `/c/…` is an ordinary directory under the root.
            (
                "/c/Users/me/a.png",
                PathFlavor::Posix,
                Some(PathBuf::from("/c/Users/me/a.png")),
            ),
            (
                r#""C:\Users\me\Pictures\shot 1.png""#,
                PathFlavor::Windows,
                Some(PathBuf::from(r"C:\Users\me\Pictures\shot 1.png")),
            ),
            (
                r"C:/Users/me/a.gif",
                PathFlavor::Windows,
                Some(PathBuf::from(r"C:/Users/me/a.gif")),
            ),
            (
                r"\\server\share\a.jpg",
                PathFlavor::Windows,
                Some(PathBuf::from(r"\\server\share\a.jpg")),
            ),
            (
                "'/c/Users/me/My Shot.png'",
                PathFlavor::Windows,
                Some(PathBuf::from(r"C:\Users\me\My Shot.png")),
            ),
            (
                "/d/shots/My\\ Shot.JPG",
                PathFlavor::Windows,
                Some(PathBuf::from(r"D:\shots\My Shot.JPG")),
            ),
            (
                "/cygdrive/c/Users/me/a.webp",
                PathFlavor::Windows,
                Some(PathBuf::from(r"C:\Users\me\a.webp")),
            ),
            #[cfg(not(windows))]
            (
                "file:///tmp/a%20b.png",
                PathFlavor::Posix,
                Some(PathBuf::from("/tmp/a b.png")),
            ),
            #[cfg(windows)]
            (
                "file:///C:/Users/me/a%20b.png",
                PathFlavor::Windows,
                Some(PathBuf::from(r"C:\Users\me\a b.png")),
            ),
        ];
        let resolved: Vec<(&str, PathFlavor, Option<PathBuf>)> = cases
            .iter()
            .map(|(text, flavor, _)| (*text, *flavor, pasted_image_path(text, *flavor)))
            .collect();
        assert_eq!(resolved, cases);
    }

    #[test]
    fn text_that_is_not_one_image_path_stays_text() {
        let texts = [
            "",
            "look at shot.png",
            "shot.png",
            "./shot.png",
            "/tmp/notes.txt",
            "/c/Users/me/notes.txt",
            "/tmp/a.png\n/tmp/b.png",
            "https://example.com/a.png",
            "/tmp/archive.png.zip",
            "C:shot.png",
        ];
        let resolved: Vec<[Option<PathBuf>; 2]> = texts
            .iter()
            .map(|text| {
                [PathFlavor::Posix, PathFlavor::Windows]
                    .map(|flavor| pasted_image_path(text, flavor))
            })
            .collect();
        assert_eq!(resolved, vec![[None, None]; texts.len()]);
    }

    #[test]
    fn a_home_path_expands() {
        let Some(home) = pa_types::platform::home_dir() else {
            return;
        };
        assert_eq!(
            pasted_image_path("~/Desktop/shot.png", PathFlavor::CURRENT),
            Some(home.join("Desktop/shot.png"))
        );
    }
}
