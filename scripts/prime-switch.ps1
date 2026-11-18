# prime-switch.ps1 - install and switch between the official Prime Agent build
# and the Prime Agent (drvova build), side by side (the Windows twin of
# scripts/prime-switch; the same commands and the same layout under %USERPROFILE%).
#
#   official  $HOME\.local                          (bin\prime-agent.cmd, share\prime-agent\)
#   fork      $HOME\.local\share\prime-agent-fork   (bin\prime-agent.cmd, share\prime-agent\, kernel-venv\)
#   switch    $HOME\.local\share\prime-switch\bin   (prime-agent.cmd: the ACTIVE build's shim;
#             prime-switch.ps1: this script). The first switch puts this dir FIRST on the
#             user PATH, so `prime-agent` runs the active build in new terminals.
#
# Each build's own launcher pins its daemon pipe and kernel venv, so the two never share
# a live daemon and switching never rebuilds a venv. ~/.prime/agent stays shared.
#
# Usage: prime-switch.ps1 <status|fork|official|install fork|official|update|uninstall fork> [-Force]
#   (from a pipe: & ([scriptblock]::Create((irm <url>/prime-switch.ps1))) install fork)
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
$switchHome = Get-Knob 'PRIME_SWITCH_HOME' (Join-Path $HOME '.local\share\prime-switch')
$switchBin = Join-Path $switchHome 'bin'
$shim = Join-Path $switchBin 'prime-agent.cmd'
$shimMark = 'rem prime-agent - active-build shim written by prime-switch'

function Fail($message) { throw "prime-switch: $message" }
function Changed($message) { Write-Host "changed: $message" }

function Assert-Build($build) {
    if (@('fork', 'official') -notcontains $build) { Fail "unknown build: $build (fork or official)" }
}
function Get-Other($build) { if ($build -eq 'fork') { 'official' } else { 'fork' } }
function Get-Launcher($build) { Join-Path (Join-Path $prefixes[$build] 'bin') 'prime-agent.cmd' }
function Get-Marker($build) { Join-Path $prefixes[$build] 'share\prime-agent\.prime-agent-install' }
function Test-Installed($build) {
    (Test-Path -LiteralPath (Get-Launcher $build) -PathType Leaf) -and (Test-Path -LiteralPath (Get-Marker $build) -PathType Leaf)
}
function Get-Version($build) {
    $line = Get-Content -LiteralPath (Get-Marker $build) -ErrorAction SilentlyContinue | Select-Object -Index 1
    if ($line) { $line -replace '^version ', '' } else { '' }
}
function Get-Active {
    if (-not (Test-Path -LiteralPath $shim)) { return '' }
    $line = Get-Content -LiteralPath $shim | Where-Object { $_ -match '^rem build: ' } | Select-Object -First 1
    if ($line) { $line -replace '^rem build: ', '' } else { '' }
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

# The shim is written to a temp file in the same dir, then renamed over the old one.
function Write-Shim($build) {
    New-Item -ItemType Directory -Path $switchBin -Force | Out-Null
    $body = "@echo off`r`n$shimMark`r`nrem build: $build`r`n`"$(Get-Launcher $build)`" %*`r`n"
    if ((Test-Path -LiteralPath $shim) -and ([IO.File]::ReadAllText($shim) -eq $body)) { return }
    $tmp = Join-Path $switchBin (".prime-agent-{0}.cmd" -f [Guid]::NewGuid().ToString('N'))
    [IO.File]::WriteAllText($tmp, $body)
    Move-Item -LiteralPath $tmp -Destination $shim -Force
}

# The first switch puts the shim dir first on the user PATH (kept in the registry kind
# the value already has, the install.ps1 discipline) and in this session.
function Set-ShimPathFirst {
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    try {
        $raw = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $entries = @($raw -split ';' | Where-Object { $_ })
        if ($entries.Count -gt 0 -and $entries[0].TrimEnd('\') -ieq $switchBin.TrimEnd('\')) { return }
        $kind = if ($raw) { $key.GetValueKind('Path') } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
        $rest = @($entries | Where-Object { $_.TrimEnd('\') -ine $switchBin.TrimEnd('\') })
        $key.SetValue('Path', (@($switchBin) + $rest) -join ';', $kind)
        $env:PATH = "$switchBin;$env:PATH"
        Changed "put $switchBin first on the user PATH (open a new terminal to pick it up)"
        # Tell Explorer the environment changed (install.ps1's broadcast), so
        # new terminals see the PATH without a re-login; best-effort.
        try {
            if (-not ('prime_switch.Win32SendMessage' -as [type])) {
                Add-Type -Namespace 'prime_switch' -Name 'Win32SendMessage' -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
            }
            [prime_switch.Win32SendMessage]::SendMessageTimeout([IntPtr]0xffff, 0x001A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]([UIntPtr]::Zero)) | Out-Null
        } catch {
            Write-Host 'note: open a new terminal (or sign out and in) to pick up the PATH change'
        }
    } finally {
        $key.Dispose()
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

switch ($Command) {
    'status' {
        $active = Get-Active
        Write-Host ("active:   {0} ({1})" -f $(if ($active) { $active } else { 'none' }), $shim)
        foreach ($build in 'official', 'fork') {
            if (Test-Installed $build) {
                Write-Host ("{0,-9} {1}  {2}" -f "${build}:", (Get-Version $build), (Get-Launcher $build))
            } else {
                Write-Host ("{0,-9} not installed  (prime-switch.ps1 install {1})" -f "${build}:", $build)
            }
        }
    }
    { $_ -in 'fork', 'official' } {
        $build = $Command
        if (-not (Test-Installed $build)) { Fail "the $build build is not installed: run prime-switch.ps1 install $build" }
        if ((Get-Active) -eq $build) {
            Write-Host "unchanged: $build is already active ($(Get-Version $build))"
        } else {
            Stop-DaemonOf (Get-Other $build)
            Write-Shim $build
            Changed "$shim now runs the $build build ($(Get-Version $build), $(Get-Launcher $build))"
            if ($build -eq 'official') { Write-ModelsWarning }
        }
        Set-ShimPathFirst
    }
    'install' {
        Assert-Build $Build
        Install-Self
        if (Test-Installed $Build) {
            Write-Host "unchanged: the $Build build is already installed ($(Get-Version $Build)); prime-switch.ps1 update updates the active build"
        } else {
            Invoke-Installer $Build
            if (-not (Test-Installed $Build)) { Fail "the $Build installer finished but $(Get-Launcher $Build) is missing" }
            Changed "installed the $Build build $(Get-Version $Build) at $($prefixes[$Build])"
            Write-Host "run ``prime-switch.ps1 $Build`` to make it the active prime-agent"
        }
    }
    'update' {
        $active = Get-Active
        if (-not $active) { Fail 'no build is active: run prime-switch.ps1 fork or prime-switch.ps1 official first' }
        Write-Host "updating the $active build: $(Get-Launcher $active) update"
        & (Get-Launcher $active) update
        exit $LASTEXITCODE
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
            if ((Get-Active) -eq 'fork') {
                if (Test-Installed 'official') {
                    Write-Shim 'official'
                    Changed "$shim now runs the official build ($(Get-Version 'official'))"
                } else {
                    Remove-Item -LiteralPath $shim -Force
                    Changed "removed $shim (no build is active)"
                }
            }
            Remove-Item -LiteralPath $forkReal -Recurse -Force
            Changed "removed the fork build at $forkReal"
        }
    }
    default { Fail "unknown command: $Command (status, fork, official, install, update, uninstall)" }
}
