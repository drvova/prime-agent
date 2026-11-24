//! Atomic file replacement: the one way a write of persistent state reaches its file.
//!
//! The new bytes go to a temp file created beside the target (same directory, so the rename
//! stays on one filesystem), are flushed (and synced when the writer asks), and the temp file is
//! renamed onto the target with [`rename_onto`] (the bounded win32 destination-busy retry). A
//! reader, or a crash at any point, sees the old file or the new one, never part of either; a
//! failed write removes its temp file and leaves the old file as it was.
//!
//! pa-types holds this because it is the one crate every writer can reach (pa-shell and pa-tui
//! depend on pa-types alone). pa-telemetry depends on no workspace crate and keeps its own
//! install-id rename for that reason.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::rename::rename_onto;

/// How much of a replacement survives a hard crash or power loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// The rename alone: a crash may lose the write (the old file stays), never tear it. The
    /// default of writers whose state is rebuilt or re-learned when lost.
    Unsynced,
    /// The temp file is synced before the rename, so the file a crash leaves is never an empty
    /// rename of unwritten data.
    SyncFile,
    /// [`Durability::SyncFile`], and the directory is synced after the rename so the rename
    /// itself survives a crash (POSIX; Windows has no directory sync).
    SyncFileAndDirectory,
}

/// The permissions the replaced file ends with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// Owner-only (`0600`) on Unix, whatever the target had: credentials and per-user state.
    /// Windows: the directory's inherited ACLs apply.
    Private,
    /// The target's own permissions when it exists (a user's file keeps its mode, an
    /// executable stays executable); a new file gets the default a plain create gives it, as
    /// `std::fs::write` would.
    KeepTarget,
}

/// What a symlink at the target path means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Symlinks {
    /// Replace the file the link points to, so the link survives (what `std::fs::write` does to
    /// a link); a dangling link or a missing file is replaced at the path as given.
    FollowToTarget,
    /// Replace the path itself: a symlink there becomes a regular file.
    ReplaceLink,
}

/// How [`replace_file`] and [`AtomicFile`] replace their target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaceOptions {
    pub durability: Durability,
    pub mode: FileMode,
    pub symlinks: Symlinks,
}

/// Makes temp names unique between the threads (and sessions) of one process; the pid makes
/// them unique between processes sharing a directory.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A replacement being written: bytes go to a temp file beside the target, and
/// [`AtomicFile::commit`] renames it onto the target. Dropped without a commit (an error, a
/// check that failed between the write and the commit), it removes its temp file and the
/// target is untouched.
#[derive(Debug)]
pub struct AtomicFile {
    target: PathBuf,
    temp: PathBuf,
    file: Option<File>,
    permissions: Option<Permissions>,
    durability: Durability,
}

impl AtomicFile {
    /// Start replacing `path`: resolve the target per `options.symlinks`, read the permissions it
    /// keeps, and create a fresh temp file beside it.
    ///
    /// # Errors
    ///
    /// The path has no file name, the target cannot be resolved or statted (other than not
    /// existing), or the temp file cannot be created.
    pub fn create(path: &Path, options: ReplaceOptions) -> io::Result<Self> {
        let target = match options.symlinks {
            Symlinks::ReplaceLink => path.to_path_buf(),
            Symlinks::FollowToTarget => match fs::canonicalize(path) {
                Ok(target) => target,
                Err(error) if error.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
                Err(error) => return Err(error),
            },
        };
        let permissions = match options.mode {
            FileMode::Private => None,
            FileMode::KeepTarget => match fs::metadata(&target) {
                Ok(metadata) => Some(metadata.permissions()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            },
        };
        let name = target.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} has no file name", target.display()),
            )
        })?;
        let name = name.to_string_lossy();
        let directory = match target.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            Some(_) | None => PathBuf::from("."),
        };
        loop {
            let temp = directory.join(format!(
                ".{name}.{}-{}.tmp",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let mut open = OpenOptions::new();
            open.write(true).create_new(true);
            #[cfg(unix)]
            if options.mode == FileMode::Private {
                use std::os::unix::fs::OpenOptionsExt;
                open.mode(0o600);
            }
            match open.open(&temp) {
                Ok(file) => {
                    return Ok(Self {
                        target,
                        temp,
                        file: Some(file),
                        permissions,
                        durability: options.durability,
                    });
                }
                // A temp file a crashed writer left with this name: take the next one.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// The file the commit replaces (the symlink's target under
    /// [`Symlinks::FollowToTarget`]).
    #[must_use]
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Finish the replacement: give the temp file its permissions, sync it per the durability,
    /// and rename it onto the target.
    ///
    /// # Errors
    ///
    /// Setting the permissions, the sync, or the rename failed; the temp file is removed and the
    /// target keeps its old content.
    pub fn commit(mut self) -> io::Result<()> {
        let Some(file) = self.file.take() else {
            return Err(io::Error::other("an atomic file was committed twice"));
        };
        if let Some(permissions) = self.permissions.take() {
            file.set_permissions(permissions)?;
        }
        match self.durability {
            Durability::Unsynced => {}
            Durability::SyncFile | Durability::SyncFileAndDirectory => file.sync_all()?,
        }
        // Windows renames only a closed file.
        drop(file);
        rename_onto(&self.temp, &self.target)?;
        // Renamed: nothing is left for the drop to remove.
        self.temp = PathBuf::new();
        match self.durability {
            Durability::Unsynced | Durability::SyncFile => Ok(()),
            Durability::SyncFileAndDirectory => sync_directory(&self.target),
        }
    }

    fn file(&mut self) -> io::Result<&mut File> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("an atomic file was written after its commit"))
    }
}

impl Write for AtomicFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file()?.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file()?.flush()
    }
}

impl Drop for AtomicFile {
    fn drop(&mut self) {
        if !self.temp.as_os_str().is_empty() {
            // Close before removing (Windows); a temp file that cannot be removed is left for
            // the next writer to step over, and the target is untouched either way.
            drop(self.file.take());
            let _ = fs::remove_file(&self.temp);
        }
    }
}

/// Replace `path` with `bytes` in one atomic step (see the module docs).
///
/// # Errors
///
/// The temp file cannot be created, written, synced or renamed onto the target; the target
/// keeps its old content and no temp file is left behind.
pub fn replace_file(path: &Path, bytes: &[u8], options: ReplaceOptions) -> io::Result<()> {
    let mut file = AtomicFile::create(path, options)?;
    file.write_all(bytes)?;
    file.commit()
}

#[cfg(unix)]
fn sync_directory(target: &Path) -> io::Result<()> {
    let directory = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        Some(_) | None => Path::new("."),
    };
    File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // the Unix arm's signature: Windows has no directory sync
fn sync_directory(_target: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    const KEEP: ReplaceOptions = ReplaceOptions {
        durability: Durability::SyncFile,
        mode: FileMode::KeepTarget,
        symlinks: Symlinks::FollowToTarget,
    };

    /// A replacement never rewrites the file in place: a hard link made before it keeps the old
    /// bytes, the target's own mode carries over, and no temp file is left.
    #[cfg(unix)]
    #[test]
    fn a_replacement_keeps_the_target_mode_and_never_rewrites_it_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("run.sh");
        fs::write(&target, "old\n").unwrap();
        fs::set_permissions(&target, Permissions::from_mode(0o751)).unwrap();
        let link = dir.path().join("link");
        fs::hard_link(&target, &link).unwrap();

        replace_file(&target, b"new\n", KEEP).unwrap();

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            (
                fs::read_to_string(&target).unwrap(),
                mode(&target),
                fs::read_to_string(&link).unwrap(),
                names(dir.path()),
            ),
            (
                "new\n".to_string(),
                0o751,
                "old\n".to_string(),
                vec!["link".to_string(), "run.sh".to_string()],
            )
        );
    }

    /// A private file ends owner-only even when the file it replaces was readable by others.
    #[cfg(unix)]
    #[test]
    fn a_private_replacement_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("auth.json");
        fs::write(&target, "{}").unwrap();
        fs::set_permissions(&target, Permissions::from_mode(0o644)).unwrap();
        replace_file(
            &target,
            b"{\"token\":1}",
            ReplaceOptions {
                durability: Durability::SyncFileAndDirectory,
                mode: FileMode::Private,
                symlinks: Symlinks::ReplaceLink,
            },
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// A replacement dropped before its commit leaves the target and the directory as they were.
    #[test]
    fn an_uncommitted_replacement_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.json");
        fs::write(&target, "old").unwrap();
        let mut file = AtomicFile::create(&target, KEEP).unwrap();
        file.write_all(b"new").unwrap();
        drop(file);
        assert_eq!(
            (fs::read_to_string(&target).unwrap(), names(dir.path())),
            ("old".to_string(), vec!["state.json".to_string()])
        );
    }

    /// Following a symlink replaces the file it points to and the link survives.
    #[cfg(unix)]
    #[test]
    fn a_followed_symlink_survives_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        fs::write(&real, "old").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        replace_file(&link, b"new", KEEP).unwrap();
        assert_eq!(
            (
                fs::read_to_string(&real).unwrap(),
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
            ),
            ("new".to_string(), true)
        );
    }
}
