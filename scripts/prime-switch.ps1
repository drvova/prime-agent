# prime-switch.ps1 - install, update, and remove the official Prime Agent build
# and the Prime Agent (drvova build), side by side (the Windows twin of
# scripts/prime-switch; the same commands and the same layout under %USERPROFILE%).
#
# THE COMMANDS: the two builds never share a command, so both answer at once and
# nothing needs switching:
#   official  `prime-agent`  $HOME\.local\bin\prime-agent.cmd (share\prime-agent\)
#   fork      `prime`        $HOME\.local\share\prime-agent-fork\bin\prime.cmd
#             (share\prime-agent\, kernel-venv\); its installer puts that bin dir
#             on the user PATH and never touches another `prime` (such as Prime
#             Intellect's own CLI) - whichever comes first on PATH runs
#   switch    $HOME\.local\share\prime-switch\bin   (prime-switch.ps1: this script)
#
# Each build's own launcher pins its daemon pipe and kernel venv, so the two never
# share a live daemon. ~/.prime/agent stays shared. Earlier versions kept a
# prime-agent.cmd shim in the switch dir that ran the "active" build; every command
# retires that shim (marker-checked), so `prime-agent` is the official build again.
#
# Usage: prime-switch.ps1 <status|install fork|official|update [fork|official]|uninstall fork> [-Force]
#   (from a pipe: & ([scriptblock]::Create((irm <url>/prime-switch.ps1))) install fork)
#   `fork` / `official` are kept for old scripts and say there is nothing to switch.
#
# Environment (tests and custom layouts): PRIME_SWITCH_HOME, PRIME_SWITCH_OFFICIAL_PREFIX,
# PRIME_SWITCH_FORK_PREFIX, PRIME_SWITCH_FORK_RELEASES_URL, PRIME_SWITCH_OFFICIAL_INSTALLER_URL
param(
    [Parameter(Position = 0)][string]$Command = 'status',
    [Parameter(Position = 1)][string]$Build = '',
    [switch]$Force
)
$ErrorActionPreference = 'Stop'

# Rendered by scripts/release/fork_release.py render (the fork's releases).
$ForkReleasesUrlDefault = 'https://github.com/drvova/prime-agent/releases/latest/download'
$OfficialInstallerUrlDefault = 'https://app.primeintellect.ai/prime-agent/install.ps1'

function Get-Knob($name, $default) {
    $value = [Environment]::GetEnvironmentVariable($name)
    if ($value) { return $value } else { return $default }
}
$forkReleasesUrl = Get-Knob 'PRIME_SWITCH_FORK_RELEASES_URL' $ForkReleasesUrlDefault
$officialInstallerUrl = Get-Knob 'PRIME_SWITCH_OFFICIAL_INSTALLER_URL' $OfficialInstallerUrlDefault
$prefixes = @{
    official = Get-Knob 'PRIME_SWITCH_OFFICIAL_PREFIX' (Join-Path $HOME '.local')
    fork = Get-Knob 'PRIME_SWITCH_FORK_PREFIX' (Join-Path $HOME '.local\share\prime-agent-fork')
}
$commands = @{ official = 'prime-agent'; fork = 'prime' }
$switchHome = Get-Knob 'PRIME_SWITCH_HOME' (Join-Path $HOME '.local\share\prime-switch')
$switchBin = Join-Path $switchHome 'bin'
$legacyShim = Join-Path $switchBin 'prime-agent.cmd'
$legacyShimMark = 'rem prime-agent - active-build shim written by prime-switch'

function Fail($message) { throw "prime-switch: $message" }
function Changed($message) { Write-Host "changed: $message" }

function Assert-Build($build) {
    if (@('fork', 'official') -notcontains $build) { Fail "unknown build: $build (fork or official)" }
}
# The build's launcher. A fork installed before it had its own command still has
# only bin\prime-agent.cmd: that launcher answers until its next install or update
# replaces it with bin\prime.cmd.
function Get-Launcher($build) {
    $bin = Join-Path $prefixes[$build] 'bin'
    $launcher = Join-Path $bin "$($commands[$build]).cmd"
    $legacy = Join-Path $bin 'prime-agent.cmd'
    if ($build -eq 'fork' -and -not (Test-Path -LiteralPath $launcher -PathType Leaf) -and (Test-Path -LiteralPath $legacy -PathType Leaf)) { return $legacy }
    return $launcher
}
function Get-Marker($build) { Join-Path $prefixes[$build] 'share\prime-agent\.prime-agent-install' }
function Test-Installed($build) {
    (Test-Path -LiteralPath (Get-Launcher $build) -PathType Leaf) -and (Test-Path -LiteralPath (Get-Marker $build) -PathType Leaf)
}
function Get-Version($build) {
    $line = Get-Content -LiteralPath (Get-Marker $build) -ErrorAction SilentlyContinue | Select-Object -Index 1
    if ($line) { $line -replace '^version ', '' } else { '' }
}

# The pre-`prime` shim goes: only the file this script wrote (its marker).
function Remove-LegacyShim {
    if ((Test-Path -LiteralPath $legacyShim -PathType Leaf) -and ((Get-Content -LiteralPath $legacyShim) -contains $legacyShimMark)) {
        Remove-Item -LiteralPath $legacyShim -Force
        Changed "removed the old $legacyShim shim (``prime-agent`` runs the official build again; the fork is ``prime``)"
    }
}

# The busy check and the clean stop through the build's OWN launcher (its pipe pin).
function Stop-DaemonOf($build) {
    if (-not (Test-Installed $build)) { return }
    $launcher = Get-Launcher $build
    # Windows PowerShell 5.1 turns a native command's stderr into a
    # terminating error under 'Stop'; the exit code is the answer here.
    $callerPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $sessions = & $launcher list --json 2>$null
        $listed = $LASTEXITCODE -eq 0
    } finally {
        $ErrorActionPreference = $callerPreference
    }
    if (-not $listed) { Write-Host "no $build daemon is running"; return }
    $text = ($sessions | Out-String)
    if ($text -match '"isStreaming"\s*:\s*true' -or $text -match '"activity"\s*:\s*"working"') {
        if (-not $Force) { Fail "the $build daemon has sessions mid-turn; let them finish ($launcher list) or re-run with -Force" }
        Write-Host "-Force: stopping the $build daemon although sessions are mid-turn"
    }
    $ErrorActionPreference = 'Continue'
    try {
        & $launcher shutdown --force --json *> $null
        $stopped = $LASTEXITCODE -eq 0
    } finally {
        $ErrorActionPreference = $callerPreference
    }
    if (-not $stopped) { Fail "the $build daemon did not stop ($launcher shutdown --force); stop it by hand and re-run" }
    Changed "stopped the $build daemon ($launcher shutdown --force)"
}

# Which `prime-agent` and `prime` a new shell runs, and whether that is the build the
# name belongs to.
function Write-PathHint {
    foreach ($build in 'official', 'fork') {
        if (-not (Test-Installed $build)) { continue }
        $name = $commands[$build]
        $launcher = Get-Launcher $build
        $resolved = Get-Command $name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if (-not $resolved) {
            Write-Host "note: ``$name`` is not on PATH; add $(Split-Path -Parent $launcher) to the user PATH (or run $launcher)"
        } elseif ((Split-Path -Parent $resolved.Source) -ine (Split-Path -Parent $launcher)) {
            Write-Host "note: ``$name`` on PATH is $($resolved.Source), not the $build build."
            Write-Host "  move $(Split-Path -Parent $launcher) before $(Split-Path -Parent $resolved.Source) in the user PATH, or run $launcher"
        }
    }
}

function Write-ModelsWarning {
    $agentDir = if ($env:PRIME_AGENT_CODING_AGENT_DIR) { $env:PRIME_AGENT_CODING_AGENT_DIR } else { Join-Path $HOME '.prime\agent' }
    $models = Join-Path $agentDir 'models.json'
    if ((Test-Path -LiteralPath $models) -and (Select-String -LiteralPath $models -Pattern 'token-exchange' -Quiet)) {
        Write-Warning "$models declares a token-exchange provider (a fork-only auth block); the official build rejects that models.json, so its custom models will not load."
    }
}

function Install-Self {
    New-Item -ItemType Directory -Path $switchBin -Force | Out-Null
    $dest = Join-Path $switchBin 'prime-switch.ps1'
    if ($PSCommandPath) {
        if ($PSCommandPath -ne $dest) { Copy-Item -LiteralPath $PSCommandPath -Destination $dest -Force; Changed "installed $dest" }
    } else {
        Invoke-WebRequest -Uri "$forkReleasesUrl/prime-switch.ps1" -OutFile $dest -UseBasicParsing
        Changed "installed $dest"
    }
}

function Invoke-Installer($build) {
    $url = if ($build -eq 'official') { $officialInstallerUrl } else { "$forkReleasesUrl/install.ps1" }
    $script = Join-Path ([IO.Path]::GetTempPath()) ("prime-switch-{0}.ps1" -f [Guid]::NewGuid().ToString('N'))
    Invoke-WebRequest -Uri $url -OutFile $script -UseBasicParsing
    Write-Host "running the $build installer from $url (prefix $($prefixes[$build]))"
    # The other build's knobs must not leak in: each installer reads only its own
    # rendered defaults plus the prefix named here.
    $saved = @{}
    foreach ($name in 'PRIME_AGENT_DOWNLOAD_BASE_URL', 'PRIME_AGENT_RELEASE_ASSETS_URL', 'PRIME_AGENT_VERSION', 'PRIME_AGENT_DAEMON_SOCKET', 'PRIME_AGENT_KERNEL_VENV', 'PRIME_AGENT_RUST_PREFIX') {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name)
        [Environment]::SetEnvironmentVariable($name, $null)
    }
    $env:PRIME_AGENT_RUST_PREFIX = $prefixes[$build]
    try {
        & powershell -NoProfile -ExecutionPolicy Bypass -File $script
        if ($LASTEXITCODE -ne 0) { Fail "the $build installer failed (exit $LASTEXITCODE)" }
    } finally {
        foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name]) }
        Remove-Item -LiteralPath $script -Force -ErrorAction SilentlyContinue
    }
}

if (@('status', 'fork', 'official', 'install', 'update', 'uninstall') -notcontains $Command) {
    Fail "unknown command: $Command (status, install, update, uninstall)"
}
Remove-LegacyShim

switch ($Command) {
    'status' {
        foreach ($build in 'official', 'fork') {
            if (Test-Installed $build) {
                Write-Host ("{0,-9} {1}  {2}  {3}" -f "${build}:", (Get-Version $build), $commands[$build], (Get-Launcher $build))
            } else {
                Write-Host ("{0,-9} not installed  (prime-switch.ps1 install {1})" -f "${build}:", $build)
            }
        }
        Write-PathHint
    }
    { $_ -in 'fork', 'official' } {
        Write-Host 'nothing to switch: the fork build is `prime` and the official build is `prime-agent`;'
        Write-Host '  both answer at once (prime-switch.ps1 status shows where each one lives)'
    }
    'install' {
        Assert-Build $Build
        Install-Self
        if (Test-Installed $Build) {
            Write-Host "unchanged: the $Build build is already installed ($(Get-Version $Build)); prime-switch.ps1 update $Build updates it"
        } else {
            Invoke-Installer $Build
            if (-not (Test-Installed $Build)) { Fail "the $Build installer finished but $(Get-Launcher $Build) is missing" }
            Changed "installed the $Build build $(Get-Version $Build) at $($prefixes[$Build])"
            Write-Host "run ``$($commands[$Build])`` to start the $Build build (open a new terminal if the installer changed the user PATH)"
            if ($Build -eq 'official') { Write-ModelsWarning }
            Write-PathHint
        }
    }
    'update' {
        if ($Build) { Assert-Build $Build }
        $updated = $false
        $failed = 0
        foreach ($build in 'official', 'fork') {
            if ($Build -and $Build -ne $build) { continue }
            if (-not (Test-Installed $build)) {
                if ($Build) { Fail "the $build build is not installed: run prime-switch.ps1 install $build" }
                continue
            }
            Write-Host "updating the $build build: $(Get-Launcher $build) update"
            & (Get-Launcher $build) update
            if ($LASTEXITCODE -ne 0) { $failed = $LASTEXITCODE }
            $updated = $true
        }
        if (-not $updated) { Fail 'no build is installed: run prime-switch.ps1 install fork|official' }
        if ($failed -ne 0) { Fail "an update failed (exit $failed)" }
    }
    'uninstall' {
        if ($Build -ne 'fork') { Fail 'only the fork build is uninstalled here; the official build keeps its own installer''s layout' }
        if (-not (Test-Installed 'fork')) {
            Write-Host 'unchanged: the fork build is not installed'
        } else {
            # The removal guard: only a marked fork prefix that contains neither $HOME nor
            # the official prefix is ever removed.
            $forkReal = [IO.Path]::GetFullPath($prefixes.fork).TrimEnd('\')
            foreach ($protected in @([IO.Path]::GetFullPath($prefixes.official), [IO.Path]::GetFullPath($HOME))) {
                if (($protected.TrimEnd('\') + '\').StartsWith($forkReal + '\', [StringComparison]::OrdinalIgnoreCase)) {
                    Fail "refusing to remove ${forkReal}: it contains $protected"
                }
            }
            if (-not (Select-String -LiteralPath (Get-Marker 'fork') -Pattern '^install-rust\.sh channel ' -Quiet)) {
                Fail "refusing to remove ${forkReal}: it carries no installer marker"
            }
            Stop-DaemonOf 'fork'
            Remove-Item -LiteralPath $forkReal -Recurse -Force
            Changed "removed the fork build at $forkReal (its bin dir stays on the user PATH until you remove it there)"
        }
    }
}
