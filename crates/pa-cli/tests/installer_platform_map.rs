//! The installer's platform map, pinned against the script's own text:
//! the uname rows install-rust.sh resolves (the MSYS/Cygwin/MINGW Windows
//! arm included) and the Windows contract install.ps1 serves. The map is
//! the load-bearing seam the mission names — a user on an unsupported
//! machine must see the honest refusal, and a Windows user (Git Bash,
//! MSYS2, Cygwin) must land on the MSVC build's win32-x64 channel row.
//!
//! The detection block is EXTRACTED from install-rust.sh and driven through
//! `sh` with a stubbed uname (the release-workflow test pattern: the real
//! step code, fixture inputs), so the pin holds the shipped text, not a
//! copy of it.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The repo root (crates/pa-cli -> crates -> root): install-rust.sh and
/// install.ps1 live there.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .expect("worktree root")
}

/// The script's platform-detection block, verbatim: from the uname reads
/// through the `BINARY_NAME` if/else.
fn platform_block(script: &str) -> String {
    let text =
        std::fs::read_to_string(repo_root().join(script)).expect("read the installer script");
    let start = text.find("OS=\"$(uname -s)\"").expect("the uname -s read");
    let name_at = start
        + text[start..]
            .find("BINARY_NAME=")
            .expect("the BINARY_NAME block");
    let fi_at = name_at + text[name_at..].find("\nfi\n").expect("the block's fi");
    text[start..fi_at + "\nfi".len()].to_string()
}

/// Drive the extracted block under `sh` with one fake uname pair; the
/// harness stubs `die` (the full script's function) and prints the
/// resolved map row.
fn detect(os: &str, arch: &str) -> Output {
    let block = platform_block("install-rust.sh");
    let harness = format!(
        "#!/bin/sh\n\
         die() {{ printf '%s\n' \"$1\" >&2; exit 1; }}\n\
         uname() {{\n\
         \x20 case \"$1\" in\n\
         \x20   -s) printf '%s\\n' \"$FAKE_OS\" ;;\n\
         \x20   -m) printf '%s\\n' \"$FAKE_ARCH\" ;;\n\
         \x20   *) printf 'stub\\n' ;;\n\
         \x20 esac\n\
         }}\n\
         {block}\n\
         printf '%s|%s|%s\\n' \"$CHANNEL_PLATFORM\" \"$WINDOWS\" \"$BINARY_NAME\"\n"
    );
    let dir = tempfile::tempdir().expect("scratch dir");
    let path = dir.path().join("harness.sh");
    std::fs::write(&path, harness).expect("write the harness");
    Command::new("sh")
        .arg(&path)
        .env("FAKE_OS", os)
        .env("FAKE_ARCH", arch)
        .output()
        .expect("run the detection harness")
}

fn map_row(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The five published platforms resolve to their channel rows; the Windows
/// uname family (Git Bash, MSYS2, Cygwin) lands on the MSVC build with the
/// `.exe` binary name.
#[test]
fn the_uname_map_resolves_the_published_platforms() {
    let cases = [
        ("Darwin", "arm64", "darwin-arm64|no|prime-agent"),
        ("Darwin", "x86_64", "darwin-x64|no|prime-agent"),
        ("Linux", "x86_64", "linux-x64|no|prime-agent"),
        ("Linux", "aarch64", "linux-arm64|no|prime-agent"),
        (
            "MINGW64_NT-10.0-19045",
            "x86_64",
            "win32-x64|yes|prime-agent.exe",
        ),
        (
            "MSYS_NT-10.0-19045",
            "x86_64",
            "win32-x64|yes|prime-agent.exe",
        ),
        (
            "CYGWIN_NT-10.0-19045",
            "x86_64",
            "win32-x64|yes|prime-agent.exe",
        ),
    ];
    for (os, arch, expected) in cases {
        let output = detect(os, arch);
        assert!(
            output.status.success(),
            "the {os}:{arch} row must resolve: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(map_row(&output), expected, "the {os}:{arch} row");
    }
}

/// An unsupported machine gets the honest refusal: the full matrix — the
/// MSVC build named — plus the Windows entry points, so the die tells the
/// user exactly what ships.
#[test]
fn the_uname_map_refuses_unsupported_machines_with_the_matrix() {
    for (os, arch) in [
        ("Windows_NT", "x86_64"),
        ("MINGW32_NT-10.0-19045", "i686"),
        ("FreeBSD", "amd64"),
    ] {
        let output = detect(os, arch);
        assert!(!output.status.success(), "the {os}:{arch} pair must refuse");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no rust build is published for"),
            "the {os}:{arch} refusal names the gap: {stderr}"
        );
        assert!(
            stderr.contains("x86_64-pc-windows-msvc"),
            "the refusal names the Windows build: {stderr}"
        );
        assert!(
            stderr.contains("install.ps1"),
            "the refusal names the Windows-native installer: {stderr}"
        );
    }
}

/// install.ps1's Windows contract: the win32-x64 channel row (the TS
/// `NATIVE_PLATFORMS` spelling the manifest reader keeps), the ARM64
/// refusal, and the two render lines the release pipeline's publish step
/// stamps (the sed + grep contract in release.yml's promote job).
#[test]
fn install_ps1_carries_the_windows_contract() {
    let text = std::fs::read_to_string(repo_root().join("install.ps1")).expect("read install.ps1");
    // The channel row.
    assert!(
        text.contains("$platform = 'win32-x64'"),
        "the ps1 serves win32-x64"
    );
    assert!(
        text.contains("prime-agent-$version-$platform.tar.gz"),
        "the ps1 validates the channel-named artifact row"
    );
    // The arch refusal: an ARM64 Windows machine fails loudly.
    assert!(
        text.contains("PROCESSOR_ARCHITECTURE") && text.contains("build only"),
        "the ps1 refuses non-x86_64 machines loudly"
    );
    // The publish render's stamp lines (release.yml's render_installer_ps1
    // sed + grep targets): both must exist verbatim at line starts.
    assert!(
        text.contains("$DownloadBaseUrlDefault = '"),
        "the base-URL default line the publish stamps must exist"
    );
    assert!(
        text.contains("$ReleaseChannelDefault = '"),
        "the channel-default line the publish stamps must exist"
    );
    // The same layout as install-rust.sh (share\prime-agent + bin):
    // the two routes must interoperate on one install.
    assert!(
        text.contains("share\\prime-agent") && text.contains("prime-agent.exe"),
        "the ps1 publishes the same payload layout with the .exe name"
    );
    // The marker write: the same ownership proof the sh installer writes
    // (the update funnel's channel-stickiness read keys on its shape).
    assert!(
        text.contains("install-rust.sh channel"),
        "the ps1 writes the funnel-readable install marker"
    );
}

/// install.ps1's download discipline: Invoke-WebRequest under the default
/// Progress/Verbose preferences leaks the raw .NET transfer records onto
/// the caller's console (the progress bar and the "Writing web request /
/// Writing request stream..." verbose chatter), and under `irm | iex` the
/// script shares the caller's session - the operator's real-machine report
/// (2026-10-06). The installer pins both preferences to `SilentlyContinue`
/// around its download calls, prints its own one-line progress instead
/// (the file name plus the size, install-rust.sh's discipline), and
/// restores the caller's values after: the quiet zone is the download,
/// never the session.
#[test]
fn install_ps1_pins_the_download_preferences_around_the_downloads() {
    let text = std::fs::read_to_string(repo_root().join("install.ps1")).expect("read install.ps1");
    // The download block: the two staged file names through the checksum
    // verify that follows it.
    let block_start = text
        .find("$tarball = Join-Path $download $expectedFile")
        .expect("the download block stages the tarball");
    let block_end = text
        .find("$sumsLine = Get-Content")
        .expect("the checksum verify follows the download");
    let block = &text[block_start..block_end];
    let markers: [(&str, &str); 8] = [
        (
            "save of the caller's progress preference",
            "$callerProgressPreference = $ProgressPreference",
        ),
        (
            "save of the caller's verbose preference",
            "$callerVerbosePreference = $VerbosePreference",
        ),
        ("progress pin", "$ProgressPreference = 'SilentlyContinue'"),
        ("verbose pin", "$VerbosePreference = 'SilentlyContinue'"),
        (
            "payload download",
            "Save-Release -Uri \"$releaseAssetsUrl/$releasePrefix/$expectedFile\" -OutFile $tarball",
        ),
        ("restore on the failure path too", "} finally {"),
        (
            "restore of the caller's progress preference",
            "$ProgressPreference = $callerProgressPreference",
        ),
        (
            "restore of the caller's verbose preference",
            "$VerbosePreference = $callerVerbosePreference",
        ),
    ];
    let mut positions: Vec<usize> = Vec::new();
    for (what, marker) in markers {
        positions.push(
            block.find(marker).unwrap_or_else(|| {
                panic!("the download block must carry its {what} line: {marker}")
            }),
        );
    }
    for pair in positions.windows(2) {
        assert!(
            pair[0] < pair[1],
            "the download block's discipline is ordered save -> pin -> download -> restore"
        );
    }
    // The installer's own progress: the download's name before it starts,
    // the name plus the size after it lands (install-rust.sh's
    // step_start/step_ok "Downloaded <MB>" discipline).
    let download_at = positions[4];
    let announcing = block
        .find("Write-Host \"downloading $expectedFile\"")
        .expect("the installer announces the download by name");
    assert!(
        announcing < download_at,
        "the installer's own progress line prints before the download starts"
    );
    assert!(
        block.contains("downloaded: $expectedFile") && block.contains("1MB"),
        "the installer reports the downloaded file's name and size"
    );
}

/// The installer's console output is ASCII-only: the Windows console's
/// default codepage renders non-ASCII bytes as mojibake (the operator's
/// 2026-10-06 report: an em-dash printed as an a-circumflex), so every
/// string the installer EMITS - Write-Host, Write-Warning, and Fail's
/// thrown message - must be plain ASCII. The script's comments and the
/// launcher files it writes are out of scope: neither lands on the console.
#[test]
fn install_ps1_emits_only_ascii_output() {
    let text = std::fs::read_to_string(repo_root().join("install.ps1")).expect("read install.ps1");
    let mut offenders: Vec<(usize, &str)> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        let emits = line.contains("Write-Host")
            || line.contains("Write-Warning")
            || line.contains("Write-Error")
            || line.contains("WriteErrorLine")
            || trimmed.starts_with("Fail ")
            || trimmed.starts_with("throw ");
        if emits && !line.is_ascii() {
            offenders.push((index + 1, trimmed));
        }
    }
    assert!(
        offenders.is_empty(),
        "install.ps1 emits non-ASCII output (the Windows console mojibakes it): {offenders:?}"
    );
}

/// The post-install smoke test's DLL-not-found class must not lie: a
/// launcher that fails --version with 0xC0000135 (`STATUS_DLL_NOT_FOUND`,
/// exit -1073741515) never started its process - a runtime DLL the binary
/// links is missing - and re-running cannot fix a missing DLL. With the
/// static-runtime build the class should not occur at all; if it does,
/// the message must say what actually happened.
#[test]
fn install_ps1_names_the_dll_not_found_failure_honestly() {
    let text = std::fs::read_to_string(repo_root().join("install.ps1")).expect("read install.ps1");
    let verify_at = text
        .find("# --- verify: the launcher must answer --version")
        .expect("the smoke test section exists");
    let dll_exit = text.find("-1073741515").expect(
        "the smoke test must branch on the DLL-not-found exit code (0xC0000135, -1073741515)",
    );
    assert!(
        dll_exit > verify_at,
        "the DLL-not-found branch belongs to the smoke test"
    );
    let warning_at = text[dll_exit..]
        .find("Write-Warning")
        .expect("the DLL-not-found branch prints a warning");
    let line_end = text[dll_exit + warning_at..]
        .find('\n')
        .map_or(text.len(), |at| dll_exit + warning_at + at);
    let dll_warning = &text[dll_exit..line_end];
    assert!(
        dll_warning.contains("0xC0000135") && dll_warning.contains("runtime"),
        "the DLL-not-found warning must name the missing runtime dependency:\n{dll_warning}"
    );
}

/// install.ps1's PATH parity (the operator's smoothness ask, 2026-10-06):
/// a Linux box almost always carries ~/.local/bin on PATH already, so the
/// sh installer only prints a profile note - but a Windows box has no
/// %USERPROFILE%\\.local\\bin, and a printed-only note leaves the fresh
/// install unusable until the user edits their environment by hand.
/// Windows HAS a canonical user-PATH setting (the registry's User scope,
/// no admin needed), so the installer ADDS the launcher's bin dir there
/// itself: idempotent (appended only when the entry is absent, never
/// duplicated), mirrored into the CURRENT session so `prime-agent` works
/// in the same console immediately, and the printed hint says a NEW
/// terminal gets it automatically instead of telling the user to go edit
/// the registry by hand.
#[test]
fn install_ps1_adds_bin_to_the_user_path_and_the_session_path() {
    let text = std::fs::read_to_string(repo_root().join("install.ps1")).expect("read install.ps1");
    let section_start = text
        .find("# --- the PATH add")
        .expect("the PATH-add section exists");
    let section_end = text
        .find("# --- verify: the launcher must answer --version")
        .expect("the smoke test section exists");
    let section = &text[section_start..section_end];
    let markers: [(&str, &str); 12] = [
        (
            "the PATH-add target: the launcher's bin dir",
            "$pathDirs = @($bin)",
        ),
        (
            "the any-append flag initialized OUTSIDE the loop (a write on any iteration must broadcast; a later no-op iteration must not suppress it)",
            "$pathAdded = $false",
        ),
        ("the session PATH add", "$env:PATH = \"$env:PATH;$binDir\""),
        (
            "the registry key open (writable: the append needs it anyway)",
            "[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)",
        ),
        (
            "the missing-key fallback (OpenSubKey returns null when HKCU\\Environment does not exist - a stripped profile - and the null would only print manual guidance; the key is created instead)",
            "[Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')",
        ),
        (
            "the RAW user PATH read (never [Environment]::GetEnvironmentVariable: it returns the EXPANDED value, and the flattened write-back would freeze %VAR%-style entries at this run's expansion)",
            "[Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames",
        ),
        (
            "the kind read (a REG_EXPAND_SZ Path must stay REG_EXPAND_SZ: SetEnvironmentVariable flattens it to plain REG_SZ, dotnet/runtime#1442)",
            "$envKey.GetValueKind('Path')",
        ),
        (
            "the expanded copy for membership (a %USERPROFILE%\\..local\\bin-style RAW entry spells the same directory as the expanded $bin - the dedup must see it, or the idempotent add appends the directory twice)",
            "[Environment]::ExpandEnvironmentVariables($rawUserPath)",
        ),
        (
            "the idempotent absent check (a whole-entry compare on BOTH spellings, never a substring: a sibling entry like C:\\Tools\\bin-old must not mask C:\\Tools\\bin)",
            "if (-not (Test-PathEntry $rawUserPath $binDir) -and -not (Test-PathEntry $expandedUserPath $binDir)) {",
        ),
        (
            "the kind-preserving user PATH write",
            "$envKey.SetValue('Path', $newUserPath, $rawUserKind)",
        ),
        ("the change broadcast", "SendMessageTimeout([IntPtr]0xffff"),
        ("the new-terminal hint", "picks them up automatically"),
    ];
    let mut positions: Vec<usize> = Vec::new();
    for (what, marker) in markers {
        positions.push(
            section
                .find(marker)
                .unwrap_or_else(|| panic!("the PATH section must carry its {what} line: {marker}")),
        );
    }
    for pair in positions.windows(2) {
        assert!(
            pair[0] < pair[1],
            "the PATH add's discipline is session first, then the durable user write: {markers:?}"
        );
    }
    // The printed note never tells the user to edit their own PATH by hand
    // anymore: the installer did it.
    assert!(
        !section.contains("add it for the prime-agent command"),
        "the PATH note must not tell the user to edit their own PATH by hand"
    );
    // The flattened write route is gone: SetEnvironmentVariable turns a
    // REG_EXPAND_SZ Path into plain REG_SZ with this run's expansion frozen
    // in (the macroscope finding: %JAVA_HOME%-style entries stop following
    // their variables).
    assert!(
        !section.contains("[Environment]::SetEnvironmentVariable('Path'"),
        "the user PATH write must preserve the registry value's kind (never the flattening SetEnvironmentVariable route)"
    );
    // The durable add is best-effort (the macroscope finding): the session
    // PATH already works and the payload is published, so a registry
    // failure must degrade to the honest manual note - never abort the
    // finished install.
    assert!(
        section.contains("could not persist the PATH entry to the user environment")
            && section.contains("put this entry on the user PATH"),
        "a registry failure in the PATH add must degrade to the honest manual note instead of failing the install"
    );
    // The broadcast reflects ANY append (the reviewer's finding): the flag
    // initializes before the loop and never resets - a later no-op
    // iteration must not suppress the WM_SETTINGCHANGE of an earlier one.
    let flag_at = section
        .find("$pathAdded = $false")
        .expect("the any-append flag exists");
    let loop_at = section
        .find("foreach ($binDir in $pathDirs)")
        .expect("the PATH-add loop");
    assert!(
        flag_at < loop_at,
        "the any-append flag must initialize OUTSIDE (before) the loop"
    );
    assert!(
        !section.contains("\n    $pathAdded = $false"),
        "the flag must never reset inside an iteration (an earlier append's broadcast would be lost)"
    );
}

/// install-rust.sh obtains a Python for its own scripting steps through
/// uv, and the uv target honors a knob (the reviewer's finding: the e2e
/// routes must be isolatable - the sh route's Git Bash run would otherwise
/// write the shared ~/.local/bin): the operator-set dir wins, the Windows
/// C:\ spelling normalizes through cygpath exactly like
/// `PRIME_AGENT_RUST_PREFIX`, and the knob is documented in the header's
/// Configuration block. The product itself needs no uv (the kernel is
/// built into the binary).
#[test]
fn install_rust_sh_honors_the_uv_bin_dir_knob() {
    let text =
        std::fs::read_to_string(repo_root().join("install-rust.sh")).expect("read install-rust.sh");
    assert!(
        text.contains("[ -n \"${PRIME_AGENT_UV_BIN_DIR:-}\" ]"),
        "install-rust.sh must honor PRIME_AGENT_UV_BIN_DIR (both e2e routes isolate)"
    );
    assert!(
        text.contains("cygpath -u \"$uv_bin_dir\""),
        "the Windows spelling of the knob must normalize exactly like PRIME_AGENT_RUST_PREFIX"
    );
    assert!(
        text.contains("#   PRIME_AGENT_UV_BIN_DIR"),
        "the knob must be documented in the header's Configuration block"
    );
    assert!(
        text.contains("elif [ -x \"${HOME}/.local/bin/${uv_name}\" ] && [ \"$uv_under_store\" = \"no\" ]"),
        "the sh's uv discovery must fall back to the canonical ~/.local/bin uv when the knob redirects the target (no duplicate install) - but NEVER in the store-alias shape (a uv inside the shared store must not carry uv's python writes back into it)"
    );
    assert!(
        text.contains("uv_name=\"uv\"") && text.contains("uv_name=\"uv.exe\""),
        "the sh's uv probes must use the platform executable name (uv.exe on Windows - the Git Bash route probes real Windows files)"
    );
    assert!(
        !text.contains("[ -x \"${uv_bin_dir}/uv\" ]"),
        "every target probe must use the platform uv name, never a bare /uv (uv.exe misses on Windows)"
    );
    assert!(
        text.contains("case \"$uv_bin_dir\" in\n    /*) ;;\n    *) die \"PRIME_AGENT_UV_BIN_DIR must be an absolute path: ${PRIME_AGENT_UV_BIN_DIR}\" ;;\n  esac"),
        "a relative knob must be refused outright (a relative target in PATH guidance breaks every later shell)"
    );
    assert!(
        text.contains("PRIME_AGENT_UV_BIN_DIR must not contain a colon"),
        "the knob must refuse the PATH separator (a target with a colon corrupts every PATH export and prepend)"
    );
    assert!(
        text.contains("uv_on_path()") && text.contains("uv_on_path_entry") && text.contains("[ -f \"${uv_on_path_entry}/${uv_on_path_name}\" ]"),
        "the sh's uv-on-PATH check must SCAN PATH for an executable file itself: command -v also reports exported shell FUNCTIONS (and a bare-name result can hit a cwd decoy), which no child process can resolve"
    );
    assert!(
        text.contains("uv_path_exts=\" .com .exe .bat .cmd\"")
            && text.contains("uv_pathtext_rest=\"${PATHEXT:-}\"")
            && text.contains("uv_pathtext_entry=\"${uv_pathtext_rest%%;*}\"")
            && text.contains("tr 'A-Z' 'a-z'"),
        "the Windows candidate list must be BUILT from the effective supported PATHEXT parsed as literal semicolon-delimited entries with END-ONLY trimming (the child's windows_executable_candidates: str::trim cuts the ends, whitespace inside an entry stays part of it), in PATHEXT order, defaults only as fallback - no word splitting or glob expansion anywhere"
    );
    assert!(
        text.contains("uv_path_candidates=\"uv\""),
        "the bare name rides first (the child tries it first)"
    );
    assert!(
        text.contains("for uv_on_path_name in $uv_path_candidates"),
        "the scan iterates the candidate names within each PATH entry, mirroring the child's search"
    );
    assert!(
        !text.contains("for uv_on_path_entry in $PATH"),
        "the PATH split must be literal: an unquoted for-loop GLOB-expands a PATH entry like /tmp/glob-* into a different directory the child never searches"
    );
    assert!(
        text.contains("uv_on_path_entry=\"${uv_on_path_rest%%:*}\""),
        "the PATH entries must be split by parameter expansion alone (never globbed)"
    );
    assert!(
        text.contains("uv_on_path_rest=\"${PATH}:\"")
            && text.contains("[ -z \"${PATH+x}\" ]"),
        "the lookup must DISTINGUISH an UNSET PATH (the child resolves nothing: no match, no abort) from a SET-EMPTY one (the child resolves the cwd: the sentinel colon visits it), and every component including a trailing or sole empty one gets its own iteration"
    );
    assert!(
        text.contains("[ -n \"$uv_on_path_entry\" ] || uv_on_path_entry=\".\""),
        "an EMPTY PATH component means the current directory in the child's resolution semantics - the scan must search it, not skip it"
    );
    assert!(
        text.contains("uv_bin=\"$uv_on_path_bin\""),
        "the sh's discovery must use the RESOLVED executable from the PATH scan, never command -v's bare-name output"
    );
    assert!(
        !text.contains("uv_bin=\"$(command -v uv)\""),
        "no bare command -v result may become uv_bin"
    );
    assert!(
        text.contains("incoming_path=\"${PATH:-}\"") && text.contains("case \":$incoming_path:\" in"),
        "the incoming-PATH capture must survive an unset PATH under set -u, and the PATH guidance compares against it"
    );
}

/// The install ledger + the streaming download (the operator's
/// flow-parity ask, 2026-10-06): install-rust.sh runs a real progress UI -
/// `step_ok` / `step_fail` lines plus a download bar fed by the fetched
/// bytes - while the ps1's download was one silent web-cmdlet call. The
/// ps1's mirror, as far as PowerShell sensibly allows: an ASCII step
/// ledger around the phases, and the payload streams through a
/// `HttpClient` copy loop that repaints one short line in place (the
/// file's name, the landed bytes, the percent) with three attempts - the
/// .NET transfer records stay silent the whole time (the download-block
/// pins test covers the silencing).
#[test]
fn install_ps1_runs_a_step_ledger_and_its_own_download_progress() {
    let text = std::fs::read_to_string(repo_root().join("install.ps1")).expect("read install.ps1");
    // The ledger functions exist.
    assert!(
        text.contains("function Step-Ok"),
        "the installer must define Step-Ok (install-rust.sh's step_ok)"
    );
    assert!(
        text.contains("function Step-Fail"),
        "the installer must define Step-Fail (install-rust.sh's step_fail)"
    );
    // The streaming helper: a `HttpClient` copy loop with the installer's
    // own in-place progress line and the sh installer's three-attempt
    // retry.
    let helper_start = text
        .find("function Save-Release")
        .expect("the streaming download helper must exist");
    let helper = &text[helper_start..];
    let helper = &helper[..helper.find("\n}\n").map_or(helper.len(), |at| at + 3)];
    assert!(
        helper.contains("System.Net.Http.HttpClient"),
        "the helper streams with HttpClient, which emits no cmdlet progress records"
    );
    assert!(
        helper.contains("`r"),
        "the helper repaints its progress line in place (a carriage return, never a newline)"
    );
    assert!(
        helper.contains("%)"),
        "the progress line carries the percent"
    );
    assert!(
        helper.contains("-NoNewline"),
        "the progress line does not scroll"
    );
    assert!(
        helper.contains("attempt"),
        "the helper retries like the sh installer's fetch (three attempts)"
    );
    // The payload download goes through the helper; the raw web cmdlet for
    // the payload is gone.
    let block_start = text
        .find("$tarball = Join-Path $download $expectedFile")
        .expect("the download block stages the tarball");
    let block_end = text
        .find("$sumsLine = Get-Content")
        .expect("the checksum verify follows");
    let block = &text[block_start..block_end];
    assert!(
        block.contains(
            "Save-Release -Uri \"$releaseAssetsUrl/$releasePrefix/$expectedFile\" -OutFile $tarball"
        ),
        "the payload download must stream through the helper"
    );
    assert!(
        !block
            .contains("Invoke-WebRequest -Uri \"$releaseAssetsUrl/$releasePrefix/$expectedFile\""),
        "the payload download must not be the noisy web cmdlet anymore"
    );
    // The ledger marks the phases (install-rust.sh's step_ok lines).
    for (phase, marker) in [
        ("checksum", "Step-Ok 'checksum verified'"),
        ("extract", "Step-Ok 'extracted'"),
        ("publish", "Step-Ok 'published'"),
    ] {
        assert!(
            text.contains(marker),
            "the ledger must mark the {phase} phase: {marker}"
        );
    }
}

/// The Windows install e2e must exercise the SHIPPED link mode (the
/// reviewer's pre-bar finding): the harness builds its own artifact, so a
/// plain `cargo build --release` there produces the DYNAMIC binary the
/// workflow stopped shipping - and the runner image carries the VC
/// runtime, so that smoke passes even on a machine that would never run
/// the shipped one. The e2e builds with the workflow's step-scoped
/// `crt-static` flag, rejects any vcruntime import in the built PE (the
/// stock-box 0xC0000135 regression guard), and restores the User PATH it
/// dirtied.
#[test]
fn windows_install_e2e_builds_the_shipped_link_mode() {
    let harness = std::fs::read_to_string(
        repo_root()
            .join("scripts")
            .join("release")
            .join("test_windows_install.ps1"),
    )
    .expect("read test_windows_install.ps1");
    assert!(
        harness.contains("target-feature=+crt-static"),
        "the e2e must build its artifact with the workflow's step-scoped crt-static flag"
    );
    assert!(
        harness.contains("vcruntime"),
        "the e2e must reject a vcruntime import in the built release binary (the runner's own VC runtime would mask the stock-box regression)"
    );
    assert!(
        harness.contains(".Trim().StartsWith($scratch, [System.StringComparison]::OrdinalIgnoreCase)"),
        "the e2e must strip exactly its own scratch-prefixed PATH entries from the CURRENT user PATH (never a stale snapshot restore)"
    );
}

/// The harnesses' User PATH cleanup strips ONLY the entries the test itself
/// added (the macroscope finding): restoring a whole stale snapshot would
/// clobber any PATH change made outside the test while it ran, and the
/// flatten-on-write [Environment] route would destroy the registry value's
/// `REG_EXPAND_SZ` kind. Each harness reads the RAW value with its kind,
/// drops only the entries under its own scratch dir, and writes back with
/// the kind preserved - a Path the test created from nothing is deleted
/// again; a Path untouched by the test is not rewritten at all.
#[test]
fn windows_e2e_harnesses_strip_only_their_own_path_entries() {
    for harness_name in [
        "test_windows_install.ps1",
        "test_windows_channel_fallback.ps1",
    ] {
        let harness = std::fs::read_to_string(
            repo_root()
                .join("scripts")
                .join("release")
                .join(harness_name),
        )
        .unwrap_or_else(|_| panic!("read {harness_name}"));
        for (what, marker) in [
            (
                "the raw read",
                "[Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames",
            ),
            ("the kind read", "$envKey.GetValueKind('Path')"),
            (
                "the own-entries filter",
                ".Trim().StartsWith($scratch, [System.StringComparison]::OrdinalIgnoreCase)",
            ),
            (
                "the kind-preserving write",
                "$envKey.SetValue('Path', ($keptEntries -join ';'), $rawUserKind)",
            ),
            (
                "the absent-Path edge (the test created the value from nothing)",
                "$envKey.DeleteValue('Path', $false)",
            ),
        ] {
            assert!(
                harness.contains(marker),
                "{harness_name} must carry its {what} line: {marker}"
            );
        }
        assert!(
            !harness.contains("[Environment]::SetEnvironmentVariable('Path', $userPathBefore, 'User')"),
            "{harness_name}'s cleanup must not restore a stale whole-value snapshot (it clobbers external changes and flattens the kind)"
        );
    }
}

/// Save-Release's body copy must survive a stalled body (the macroscope
/// finding): `HttpClient.Timeout` under `ResponseHeadersRead` bounds only the
/// wait for the headers, so a server that answers the headers and then
/// parks the tarball body would hang the synchronous `$stream.Read` loop
/// forever - the three-attempt retry the helper promises never gets its
/// chance. Each chunk read is a bounded task: a body that delivers nothing
/// within the stall window throws and takes the retry path, while a
/// slow-but-flowing body never trips it (a read returns as soon as data
/// arrives, the window restarts per chunk).
#[test]
fn install_ps1_download_survives_a_stalled_body() {
    let text = std::fs::read_to_string(repo_root().join("install.ps1")).expect("read install.ps1");
    let helper_start = text
        .find("function Save-Release")
        .expect("the streaming download helper must exist");
    let helper = &text[helper_start..];
    let helper = &helper[..helper.find("\n}\n").map_or(helper.len(), |at| at + 3)];
    let markers: [(&str, &str); 4] = [
        (
            "the per-chunk stall window",
            "$readTimeout = [TimeSpan]::FromSeconds(60)",
        ),
        (
            "the bounded chunk read",
            "$readTask = $stream.ReadAsync($buffer, 0, $buffer.Length)",
        ),
        (
            "the wait against the window",
            "if (-not $readTask.Wait($readTimeout)) {",
        ),
        ("the honest stall message", "the body of $Name stalled"),
    ];
    let mut positions: Vec<usize> = Vec::new();
    for (what, marker) in markers {
        positions.push(
            helper.find(marker).unwrap_or_else(|| {
                panic!("the download helper must carry its {what} line: {marker}")
            }),
        );
    }
    for pair in positions.windows(2) {
        assert!(
            pair[0] < pair[1],
            "the stall discipline is ordered window -> read -> wait -> throw: {markers:?}"
        );
    }
}

/// The Windows install e2e keeps uv out of the real profile and proves the
/// ps1 route needs none. The kernel is built into the binary, so
/// install.ps1 installs no uv: the harness fails if its scratch uv target
/// exists after the ps1 route. install-rust.sh (the Git Bash route, run
/// afterwards) still installs uv for its own Python when the machine has
/// none, and ownership of a file in the SHARED ~/.local/bin can never be
/// proven (a concurrent external install races the attribution) - so the
/// harness points the `PRIME_AGENT_UV_BIN_DIR` knob, uv's cache, and uv's
/// pythons at its own scratch dir and restores the caller's values.
#[test]
fn windows_install_e2e_keeps_uv_to_the_sh_route_and_in_scratch() {
    let harness = std::fs::read_to_string(
        repo_root()
            .join("scripts")
            .join("release")
            .join("test_windows_install.ps1"),
    )
    .expect("read test_windows_install.ps1");
    let ps1_route = harness
        .find("& pwsh -File (Join-Path $repo 'install.ps1')")
        .expect("the ps1 route runs install.ps1");
    let no_uv_gate = harness
        .find("if (Test-Path (Join-Path $scratch 'uv-bin')) { throw 'install.ps1 installed uv")
        .expect("the harness must fail when install.ps1 installs uv (the kernel needs none)");
    let sh_route = harness
        .find("# 6. Route B: install-rust.sh under Git Bash")
        .expect("the sh route follows");
    assert!(
        ps1_route < no_uv_gate && no_uv_gate < sh_route,
        "the no-uv gate must read the uv target after the ps1 route and before the sh route (which may install uv for its own Python)"
    );
    for (what, set, saved, restored) in [
        (
            "uv target",
            "$env:PRIME_AGENT_UV_BIN_DIR = Join-Path $scratch 'uv-bin'",
            "$callerUvBinDir = $env:PRIME_AGENT_UV_BIN_DIR",
            "$env:PRIME_AGENT_UV_BIN_DIR = $callerUvBinDir",
        ),
        (
            "uv cache",
            "$env:UV_CACHE_DIR = Join-Path $scratch 'uv-cache'",
            "$callerUvCacheDir = $env:UV_CACHE_DIR",
            "$env:UV_CACHE_DIR = $callerUvCacheDir",
        ),
        (
            "uv python dir",
            "$env:UV_PYTHON_INSTALL_DIR = Join-Path $scratch 'uv-python'",
            "$callerUvPythonDir = $env:UV_PYTHON_INSTALL_DIR",
            "$env:UV_PYTHON_INSTALL_DIR = $callerUvPythonDir",
        ),
    ] {
        assert!(
            harness.contains(set) && harness.contains(saved) && harness.contains(restored),
            "the harness must steer the sh route's {what} into its scratch dir and restore the caller's value"
        );
    }
    // The cleanup never touches the user's uv binaries: the install never
    // lands in the shared dir under the knob.
    let finally_at = harness
        .rfind("\n} finally {")
        .expect("the cleanup finally exists");
    assert!(
        !harness[finally_at..].contains(".local\\bin"),
        "the cleanup must not touch ~/.local/bin (the install never lands there under the knob)"
    );
}
