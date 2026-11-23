//! `ls [-a] [-A] [-l] [-1] [-d] [-h] [-F] [-p] [-R] [-t] [-S] [-r] [FILE...]`:
//! one name a line, as GNU `ls` writes to anything but a terminal.
//!
//! Names sort by their bytes. Under a locale that sorts otherwise, and with
//! a shell `TZ` this process cannot read the zone data for, the program
//! takes over when the machine has one.

use std::fs::Metadata;
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use super::{quoted, reason, Arg, Failure, Invocation, Options, Outcome, View};

#[derive(Clone, Copy)]
enum Order {
    Name,
    Time,
    Size,
}

/// Which trailing mark a name gets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Marks {
    None,
    /// `-p`: a slash after a directory.
    Directories,
    /// `-F`: also `*`, `@`, `|` and `=`.
    All,
}

struct Listing {
    all: bool,
    almost_all: bool,
    long: bool,
    human: bool,
    marks: Marks,
    order: Order,
    reverse: bool,
    utc: bool,
}

struct Entry {
    name: String,
    /// The entry's own metadata: a link's, not its target's.
    metadata: Metadata,
    /// Where a link points, for the long format.
    target: Option<String>,
    /// Whether a link leads to a directory (`-F` and plain listing follow it).
    directory: bool,
}

impl Entry {
    fn read(name: String, path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        let link = metadata.file_type().is_symlink();
        Ok(Entry {
            name,
            target: link
                .then(|| std::fs::read_link(path).ok())
                .flatten()
                .map(|target| target.to_string_lossy().into_owned()),
            directory: if link {
                std::fs::metadata(path).is_ok_and(|followed| followed.is_dir())
            } else {
                metadata.is_dir()
            },
            metadata,
        })
    }
}

/// A size as `ls -h` prints it: powers of 1024, rounded up, one decimal
/// below ten.
fn human(bytes: u64) -> String {
    if bytes < 1024 {
        return bytes.to_string();
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < 6 {
        value /= 1024.0;
        unit += 1;
    }
    let suffix = ['B', 'K', 'M', 'G', 'T', 'P', 'E'][unit];
    if value < 10.0 {
        let tenths = (value * 10.0).ceil() / 10.0;
        if tenths < 10.0 {
            return format!("{tenths:.1}{suffix}");
        }
    }
    format!("{}{suffix}", value.ceil())
}

#[cfg(unix)]
fn mode_text(metadata: &Metadata) -> String {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let kind = metadata.file_type();
    let mode = metadata.permissions().mode();
    let mut text = String::with_capacity(10);
    text.push(if kind.is_dir() {
        'd'
    } else if kind.is_symlink() {
        'l'
    } else if kind.is_fifo() {
        'p'
    } else if kind.is_socket() {
        's'
    } else if kind.is_char_device() {
        'c'
    } else if kind.is_block_device() {
        'b'
    } else {
        '-'
    });
    // Per class: read, write, then execute merged with its special bit.
    for (shift, special, set, unset) in [
        (6, 0o4000, 's', 'S'),
        (3, 0o2000, 's', 'S'),
        (0, 0o1000, 't', 'T'),
    ] {
        let bits = mode >> shift;
        text.push(if bits & 4 != 0 { 'r' } else { '-' });
        text.push(if bits & 2 != 0 { 'w' } else { '-' });
        text.push(match (bits & 1 != 0, mode & special != 0) {
            (true, false) => 'x',
            (false, false) => '-',
            (true, true) => set,
            (false, true) => unset,
        });
    }
    text
}

#[cfg(not(unix))]
fn mode_text(metadata: &Metadata) -> String {
    let write = if metadata.permissions().readonly() {
        '-'
    } else {
        'w'
    };
    if metadata.is_dir() {
        format!("dr{write}xr-xr-x")
    } else {
        format!("-r{write}-r--r--")
    }
}

/// Link count, owner, group, size column and 1K blocks of an entry.
#[cfg(unix)]
fn long_fields(
    listing: &Listing,
    _shell: View<'_>,
    metadata: &Metadata,
) -> (u64, String, String, String, u64) {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let owner = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(metadata.uid()))
        .ok()
        .flatten()
        .map_or_else(|| metadata.uid().to_string(), |user| user.name);
    let group = nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(metadata.gid()))
        .ok()
        .flatten()
        .map_or_else(|| metadata.gid().to_string(), |group| group.name);
    let kind = metadata.file_type();
    let size = if kind.is_char_device() || kind.is_block_device() {
        let device = metadata.rdev();
        format!(
            "{}, {}",
            ((device >> 32) & 0xffff_f000) | ((device >> 8) & 0xfff),
            ((device >> 12) & 0xffff_ff00) | (device & 0xff)
        )
    } else if listing.human {
        human(metadata.len())
    } else {
        metadata.len().to_string()
    };
    (metadata.nlink(), owner, group, size, metadata.blocks() / 2)
}

#[cfg(not(unix))]
fn long_fields(
    listing: &Listing,
    shell: View<'_>,
    metadata: &Metadata,
) -> (u64, String, String, String, u64) {
    let owner = (shell.env)("USERNAME").unwrap_or_else(|| "user".to_string());
    let size = if listing.human {
        human(metadata.len())
    } else {
        metadata.len().to_string()
    };
    (1, owner.clone(), owner, size, metadata.len().div_ceil(1024))
}

/// One entry's columns in the long format.
struct Row {
    mode: String,
    links: u64,
    owner: String,
    group: String,
    size: String,
    /// What the entry holds on disk, in 1K blocks, for the `total` line.
    blocks: u64,
}

/// Print `entries` (already selected) in the listing's order.
fn print(
    listing: &Listing,
    shell: View<'_>,
    mut entries: Vec<Entry>,
    total: bool,
    stdout: &mut dyn Write,
) -> Result<(), Failure> {
    let modified = |entry: &Entry| entry.metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    entries.sort_by(|left, right| {
        let ordering = match listing.order {
            Order::Name => left.name.as_bytes().cmp(right.name.as_bytes()),
            Order::Time => modified(right)
                .cmp(&modified(left))
                .then_with(|| left.name.as_bytes().cmp(right.name.as_bytes())),
            Order::Size => right
                .metadata
                .len()
                .cmp(&left.metadata.len())
                .then_with(|| left.name.as_bytes().cmp(right.name.as_bytes())),
        };
        if listing.reverse {
            ordering.reverse()
        } else {
            ordering
        }
    });
    let mark = |entry: &Entry| -> &'static str {
        let kind = entry.metadata.file_type();
        match listing.marks {
            Marks::Directories | Marks::All if kind.is_dir() => "/",
            Marks::None | Marks::Directories => "",
            Marks::All if kind.is_symlink() => "@",
            Marks::All => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
                    if kind.is_fifo() {
                        "|"
                    } else if kind.is_socket() {
                        "="
                    } else if entry.metadata.permissions().mode() & 0o111 != 0 {
                        "*"
                    } else {
                        ""
                    }
                }
                #[cfg(not(unix))]
                ""
            }
        }
    };
    if !listing.long {
        for entry in &entries {
            shell.alive()?;
            writeln!(stdout, "{}{}", entry.name, mark(entry))?;
        }
        return Ok(());
    }
    let rows: Vec<Row> = entries
        .iter()
        .map(|entry| {
            let (links, owner, group, size, blocks) = long_fields(listing, shell, &entry.metadata);
            Row {
                mode: mode_text(&entry.metadata),
                links,
                owner,
                group,
                size,
                blocks,
            }
        })
        .collect();
    if total {
        let blocks: u64 = rows.iter().map(|row| row.blocks).sum();
        if listing.human {
            writeln!(stdout, "total {}", human(blocks * 1024))?;
        } else {
            writeln!(stdout, "total {blocks}")?;
        }
    }
    let width = |column: fn(&Row) -> usize| rows.iter().map(column).max().unwrap_or(0);
    let (links, owners, groups, sizes) = (
        width(|row| row.links.to_string().len()),
        width(|row| row.owner.len()),
        width(|row| row.group.len()),
        width(|row| row.size.len()),
    );
    // A file from the last six months shows its time of day, any other its year.
    let now = chrono::Utc::now();
    let recent = now - chrono::Duration::seconds(15_778_476);
    for (entry, row) in entries.iter().zip(&rows) {
        let Row {
            mode,
            links: link_count,
            owner,
            group,
            size,
            blocks: _,
        } = row;
        shell.alive()?;
        let stamp: chrono::DateTime<chrono::Utc> = modified(entry).into();
        let format = if stamp > recent && stamp <= now {
            "%b %e %H:%M"
        } else {
            "%b %e  %Y"
        };
        let date = if listing.utc {
            stamp.format(format).to_string()
        } else {
            stamp
                .with_timezone(&chrono::Local)
                .format(format)
                .to_string()
        };
        write!(
            stdout,
            "{mode} {link_count:>links$} {owner:<owners$} {group:<groups$} {size:>sizes$} {date} {}",
            entry.name
        )?;
        match &entry.target {
            Some(target) => writeln!(stdout, " -> {target}")?,
            None => writeln!(stdout, "{}", mark(entry))?,
        }
    }
    Ok(())
}

pub(super) fn ls(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut listing = Listing {
        all: false,
        almost_all: false,
        long: false,
        human: false,
        marks: Marks::None,
        order: Order::Name,
        reverse: false,
        utc: false,
    };
    let (mut recursive, mut directories_as_files) = (false, false);
    let mut operands = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('a') | Arg::Long("all") => listing.all = true,
            Arg::Short('A') | Arg::Long("almost-all") => listing.almost_all = true,
            Arg::Short('l') => listing.long = true,
            Arg::Short('1') => {}
            Arg::Short('h') | Arg::Long("human-readable") => listing.human = true,
            Arg::Short('F') | Arg::Long("classify") => listing.marks = Marks::All,
            Arg::Short('p') => listing.marks = Marks::Directories,
            Arg::Short('R') | Arg::Long("recursive") => recursive = true,
            Arg::Short('d') | Arg::Long("directory") => directories_as_files = true,
            Arg::Short('t') => listing.order = Order::Time,
            Arg::Short('S') => listing.order = Order::Size,
            Arg::Short('r') | Arg::Long("reverse") => listing.reverse = true,
            Arg::Long("color" | "colour") => {
                if !matches!(options.value()?, "never" | "auto") {
                    return Err(Failure::Unsupported("--color=always".to_string()));
                }
            }
            Arg::Operand(operand) => operands.push(operand),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    let shell = invocation.shell;
    shell.sorts_by_bytes("ls")?;
    if listing.long {
        match (shell.env)("TZ").as_deref() {
            None | Some("") => {}
            Some("UTC" | "UTC0" | "GMT" | "Etc/UTC") => listing.utc = true,
            Some(zone) => return Err(Failure::Unsupported(format!("TZ={zone}"))),
        }
    }
    if operands.is_empty() {
        operands.push(".");
    }
    let Invocation { stdout, stderr, .. } = invocation;
    let mut status = 0;
    let (mut files, mut directories) = (Vec::new(), Vec::new());
    let mut sorted = operands.clone();
    sorted.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    for operand in sorted {
        match Entry::read(operand.to_string(), &shell.path(operand)) {
            Ok(entry) if entry.directory && !directories_as_files => {
                // A link to a directory is listed as a link in the long format.
                if listing.long && entry.target.is_some() && !operand.ends_with('/') {
                    files.push(entry);
                } else {
                    directories.push(operand.to_string());
                }
            }
            Ok(entry) => files.push(entry),
            Err(error) => {
                writeln!(
                    stderr,
                    "ls: cannot access {}: {}",
                    quoted(operand),
                    reason(&error)
                )?;
                status = 2;
            }
        }
    }
    let mut first = files.is_empty();
    let headers = recursive || operands.len() > 1;
    print(&listing, shell, files, /*total*/ false, stdout)?;
    // Depth first, each directory's subdirectories right after it.
    directories.reverse();
    while let Some(directory) = directories.pop() {
        shell.alive()?;
        let path = shell.path(&directory);
        let read = std::fs::read_dir(&path).and_then(|entries| {
            entries
                .map(|entry| {
                    let entry = entry?;
                    Entry::read(
                        entry.file_name().to_string_lossy().into_owned(),
                        &entry.path(),
                    )
                })
                .collect::<std::io::Result<Vec<Entry>>>()
        });
        let mut entries = match read {
            Ok(entries) => entries,
            Err(error) => {
                stdout.flush()?;
                writeln!(
                    stderr,
                    "ls: cannot open directory {}: {}",
                    quoted(&directory),
                    reason(&error)
                )?;
                status = status.max(1);
                continue;
            }
        };
        entries.retain(|entry| listing.all || listing.almost_all || !entry.name.starts_with('.'));
        if listing.all {
            for special in [".", ".."] {
                if let Ok(entry) = Entry::read(special.to_string(), &path.join(special)) {
                    entries.push(entry);
                }
            }
        }
        if headers {
            writeln!(stdout, "{}{directory}:", if first { "" } else { "\n" })?;
        }
        first = false;
        if recursive {
            let mut below: Vec<String> = entries
                .iter()
                .filter(|entry| {
                    entry.metadata.is_dir() && !matches!(entry.name.as_str(), "." | "..")
                })
                .map(|entry| format!("{}/{}", directory.trim_end_matches('/'), entry.name))
                .collect();
            below.sort_by(|left, right| right.as_bytes().cmp(left.as_bytes()));
            directories.extend(below);
        }
        print(&listing, shell, entries, /*total*/ true, stdout)?;
    }
    Ok(status)
}
