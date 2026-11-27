//! Home-directory resolution shared by every crate that expands `~`: Node `os.homedir()`
//! semantics -
//! `HOME` when set, then on Windows `USERPROFILE`, else `HOMEDRIVE` + `HOMEPATH`. Where
//! `os.homedir()` throws, this port returns `None` and each caller owns its fallback.

use std::path::{Path, PathBuf};

/// The user's home directory, Node `os.homedir()` semantics: `HOME` first (always wins, matching
/// the
/// TS order), then on Windows the `USERPROFILE`/`HOMEDRIVE`+`HOMEPATH` chain; `None` when nothing
/// resolves (the per-call-site fallback replaces the TS throw).
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    if let Some(home) = crate::session_scope::var_os("HOME").filter(|home| !home.is_empty()) {
        return Some(PathBuf::from(home));
    }
    #[cfg(windows)]
    {
        if let Some(profile) = crate::session_scope::var_os("USERPROFILE").filter(|p| !p.is_empty())
        {
            return Some(PathBuf::from(profile));
        }
        if let (Some(drive), Some(path)) = (
            crate::session_scope::var_os("HOMEDRIVE"),
            crate::session_scope::var_os("HOMEPATH"),
        ) {
            let mut home = PathBuf::from(drive);
            home.push(path);
            return Some(home);
        }
    }
    None
}

/// `PRIME_AGENT_CODING_AGENT_DIR`: the agent state directory override
/// (byte-compatible with the TS product).
pub const ENV_AGENT_DIR: &str = "PRIME_AGENT_CODING_AGENT_DIR";

/// The agent state directory name (TS `CONFIG_DIR_NAME`, `.prime/agent`): the official build's
/// per-user agent dir under the home directory, and every build's project config dir under a
/// working directory.
pub const CONFIG_DIR_NAME: &str = ".prime/agent";

/// `PRIME_AGENT_DIST_VARIANT` as this binary was compiled: a distribution build's variant name
/// (its release workflow sets it with the other `PRIME_AGENT_DIST_*` stamps), `None` for the
/// official build.
pub const DIST_VARIANT_STAMP: Option<&str> = option_env!("PRIME_AGENT_DIST_VARIANT");

/// Which per-user state a build owns. The official build keeps the product's shared names
/// (`~/.prime/agent`, the `prime-agent-<uid>` socket dir, the `prime-agent-daemon` pipe), which
/// the TypeScript product reads and writes too. A distribution build owns its own set, named by
/// its variant, so it never lists, resumes, or leases the official products' sessions and never
/// reaches their daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateLayout {
    /// `~/.prime/agent`, the `prime-agent` endpoint names.
    Official,
    /// `~/.prime/agent-<variant>`, the `prime-agent-<variant>` endpoint names.
    Distribution { variant: &'static str },
}

impl StateLayout {
    /// The layout this binary was compiled with.
    pub const BUILD: Self = Self::from_stamp(DIST_VARIANT_STAMP);

    /// The layout a variant stamp names.
    ///
    /// # Panics
    ///
    /// Panics (a compile error for [`Self::BUILD`]) when the variant is empty or holds anything
    /// but `a-z`, `0-9`, and `-` (the installers' own variant rule: it names files, sockets,
    /// and pipes).
    #[must_use]
    pub const fn from_stamp(variant: Option<&'static str>) -> Self {
        let Some(variant) = variant else {
            return Self::Official;
        };
        let bytes = variant.as_bytes();
        assert!(!bytes.is_empty(), "PRIME_AGENT_DIST_VARIANT is empty");
        let mut index = 0;
        while index < bytes.len() {
            assert!(
                matches!(bytes[index], b'a'..=b'z' | b'0'..=b'9' | b'-'),
                "PRIME_AGENT_DIST_VARIANT allows only a-z, 0-9, and -"
            );
            index += 1;
        }
        Self::Distribution { variant }
    }

    /// The last component of the per-user agent dir under `~/.prime`: `agent`, or
    /// `agent-<variant>`.
    #[must_use]
    pub fn agent_dir_leaf(self) -> String {
        match self {
            Self::Official => "agent".to_string(),
            Self::Distribution { variant } => format!("agent-{variant}"),
        }
    }

    /// The per-user agent dir under `home` (no env override): `<home>/.prime/agent` (the
    /// official spelling, joined exactly as it always was), or `<home>/.prime/agent-<variant>`.
    #[must_use]
    pub fn home_agent_dir(self, home: &Path) -> PathBuf {
        match self {
            Self::Official => home.join(CONFIG_DIR_NAME),
            Self::Distribution { .. } => home.join(".prime").join(self.agent_dir_leaf()),
        }
    }

    /// The prefix of the daemon endpoint names (the Unix socket dir `<prefix>-<uid>`, the
    /// Windows pipe `<prefix>-daemon`): `prime-agent`, or `prime-agent-<variant>` (the
    /// installers' distribution launchers pin the same names).
    #[must_use]
    pub fn endpoint_prefix(self) -> String {
        match self {
            Self::Official => "prime-agent".to_string(),
            Self::Distribution { variant } => format!("prime-agent-{variant}"),
        }
    }
}

/// The agent state directory: the env override with a leading `~`/`~/` expanded, else this
/// build's per-user dir ([`StateLayout::home_agent_dir`]); `None` when the home directory does
/// not resolve (each caller owns its fallback).
#[must_use]
pub fn agent_dir() -> Option<PathBuf> {
    if let Some(dir) = crate::session_scope::var_os(ENV_AGENT_DIR).filter(|dir| !dir.is_empty()) {
        return Some(expand_tilde(&dir.to_string_lossy()));
    }
    home_dir().map(|home| StateLayout::BUILD.home_agent_dir(&home))
}

/// Expand a leading `~`/`~/` - and on Windows `~\` - against [`home_dir`]; other values pass
/// through (`~foo` is not an expansion).
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest));
    }
    #[cfg(windows)]
    if let Some(rest) = path.strip_prefix("~\\") {
        return home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest));
    }
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_scope::{self, SessionScope};

    /// Run `body` in a session scope that sets the named variables (`None`
    /// hides them). The resolvers read through `session_scope`, so the
    /// process environment stays untouched and the tests run in parallel.
    fn with_env(names: &[(&str, Option<&str>)], body: impl FnOnce()) {
        let env = names
            .iter()
            .filter_map(|(name, value)| value.map(|value| ((*name).to_string(), value.to_string())))
            .collect();
        let hidden = names
            .iter()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| (*name).to_string())
            .collect();
        let _scope = session_scope::enter(Some(SessionScope::new(
            "dirs-test".to_string(),
            PathBuf::new(),
            env,
            hidden,
        )));
        body();
    }

    #[test]
    #[cfg(unix)]
    fn agent_dir_env_override_expands_tilde() {
        with_env(
            &[
                ("HOME", Some("/home/tester")),
                ("PRIME_AGENT_CODING_AGENT_DIR", Some("~/state")),
            ],
            || {
                assert_eq!(agent_dir(), Some(PathBuf::from("/home/tester/state")));
            },
        );
    }

    #[test]
    #[cfg(unix)]
    fn agent_dir_env_override_passthrough() {
        with_env(
            &[
                ("HOME", None),
                ("PRIME_AGENT_CODING_AGENT_DIR", Some("/opt/state")),
            ],
            || {
                assert_eq!(agent_dir(), Some(PathBuf::from("/opt/state")));
            },
        );
    }

    #[test]
    #[cfg(unix)]
    fn agent_dir_default_under_home() {
        with_env(
            &[
                ("HOME", Some("/home/tester")),
                ("PRIME_AGENT_CODING_AGENT_DIR", None),
            ],
            || {
                assert_eq!(
                    agent_dir(),
                    Some(PathBuf::from("/home/tester/.prime/agent"))
                );
            },
        );
    }

    /// A distribution stamp moves the per-user agent dir and the daemon endpoints off the
    /// official products' shared names; no stamp keeps them.
    #[test]
    #[cfg(unix)]
    fn a_distribution_layout_owns_its_agent_dir_and_endpoints() {
        let home = Path::new("/home/tester");
        let resolved = [None, Some("fork")].map(|stamp| {
            let layout = StateLayout::from_stamp(stamp);
            (
                layout,
                layout.home_agent_dir(home),
                layout.endpoint_prefix(),
            )
        });
        assert_eq!(
            resolved,
            [
                (
                    StateLayout::Official,
                    PathBuf::from("/home/tester/.prime/agent"),
                    "prime-agent".to_string()
                ),
                (
                    StateLayout::Distribution { variant: "fork" },
                    PathBuf::from("/home/tester/.prime/agent-fork"),
                    "prime-agent-fork".to_string()
                ),
            ]
        );
    }

    #[test]
    #[cfg(unix)]
    fn agent_dir_without_home_is_none() {
        with_env(
            &[("HOME", None), ("PRIME_AGENT_CODING_AGENT_DIR", None)],
            || {
                assert_eq!(agent_dir(), None);
            },
        );
    }

    #[test]
    #[cfg(unix)]
    fn home_dir_follows_home() {
        with_env(&[("HOME", Some("/home/tester"))], || {
            assert_eq!(home_dir(), Some(PathBuf::from("/home/tester")));
        });
    }

    #[test]
    #[cfg(unix)]
    fn home_dir_unset_is_none() {
        with_env(&[("HOME", None)], || {
            assert_eq!(home_dir(), None);
        });
    }

    #[test]
    #[cfg(unix)]
    fn home_dir_empty_is_none() {
        with_env(&[("HOME", Some(""))], || {
            assert_eq!(home_dir(), None);
        });
    }

    #[test]
    #[cfg(windows)]
    fn agent_dir_env_override_expands_the_win32_backslash_tilde() {
        // The TS `expandTildePath` win32 arm: a `~\`-prefixed override expands exactly like the
        // `~/`
        // form (a CLI-only expansion would split the state dir between processes).
        with_env(
            &[
                ("HOME", None),
                ("USERPROFILE", Some(r"C:\Users\tester")),
                ("PRIME_AGENT_CODING_AGENT_DIR", Some(r"~\state")),
            ],
            || {
                assert_eq!(agent_dir(), Some(PathBuf::from(r"C:\Users\tester\state")));
            },
        );
    }

    #[test]
    #[cfg(windows)]
    fn home_dir_prefers_home_over_userprofile() {
        with_env(
            &[
                ("HOME", Some("/home/tester")),
                ("USERPROFILE", Some(r"C:\Users\tester")),
            ],
            || {
                assert_eq!(home_dir(), Some(PathBuf::from("/home/tester")));
            },
        );
    }

    #[test]
    #[cfg(windows)]
    fn home_dir_falls_back_to_userprofile() {
        with_env(
            &[("HOME", None), ("USERPROFILE", Some(r"C:\Users\tester"))],
            || {
                assert_eq!(home_dir(), Some(PathBuf::from(r"C:\Users\tester")));
            },
        );
    }

    #[test]
    #[cfg(windows)]
    fn home_dir_falls_back_to_homedrive_homepath() {
        with_env(
            &[
                ("HOME", None),
                ("USERPROFILE", None),
                ("HOMEDRIVE", Some("C:")),
                ("HOMEPATH", Some(r"\Users\tester")),
            ],
            || {
                assert_eq!(home_dir(), Some(PathBuf::from(r"C:\Users\tester")));
            },
        );
    }

    #[test]
    #[cfg(windows)]
    fn home_dir_unset_is_none() {
        with_env(
            &[
                ("HOME", None),
                ("USERPROFILE", None),
                ("HOMEDRIVE", None),
                ("HOMEPATH", None),
            ],
            || {
                assert_eq!(home_dir(), None);
            },
        );
    }
}
