# SessionStart hook: ensure the mcpls this plugin drives is on PATH and matches
# the plugin's version, installing it from the matching GitHub release.
#
# Usage: bootstrap-binaries.ps1 <claude|codex>
#
# PowerShell twin of `bootstrap-binaries`, for Windows hosts with no bash.
# Both resolve the same state paths and locks, so a machine that later gains
# Git Bash does not reinstall.
#
# Every path exits 0: a session must start even with no network. Both
# harnesses read a SessionStart hook's stdout, so installer output goes to
# stderr and stdout carries at most one context object telling the agent
# when the binary does not match the plugin.

$ErrorActionPreference = 'Continue'

if ($env:MCPLS_NO_BOOTSTRAP -eq '1') { exit 0 }

$repoUrl = 'https://github.com/AbysmalBiscuit/mcpls'
$pluginRoot = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $pluginRoot '.claude-plugin/plugin.json'

# Installs are serialised per harness, so the lock is named after it.
$harness = 'unknown'
if ($args.Count -gt 0 -and $args[0] -in @('claude', 'codex')) { $harness = $args[0] }

function Write-Note($message) { [Console]::Error.WriteLine("mcpls plugin: $message") }

# Ends the hook, handing the agent a note when there is one.
function Complete-Hook($note) {
    if ($note) {
        $output = @{ hookSpecificOutput = @{ hookEventName = 'SessionStart'; additionalContext = "mcpls plugin: $note" } }
        [Console]::Out.WriteLine(($output | ConvertTo-Json -Compress -Depth 3))
    }
    exit 0
}

$version = $null
try {
    $version = (Get-Content -Raw -ErrorAction Stop $manifest | ConvertFrom-Json).version
} catch {}
if ([string]::IsNullOrWhiteSpace($version)) {
    Write-Note "could not read a version from ${manifest}; skipping the mcpls install"
    exit 0
}

$release = "$repoUrl/releases/tag/v$version"
$stateRoot = if ($env:XDG_STATE_HOME) { $env:XDG_STATE_HOME } else { Join-Path $HOME '.local/state' }
$stateDir = Join-Path $stateRoot 'mcpls'
$stamp = Join-Path $stateDir 'bootstrap-version'
$failed = Join-Path $stateDir 'bootstrap-failed'
$lock = Join-Path $stateDir "install-$harness.lock"

function Read-Marker($path) {
    try { (Get-Content -Raw -ErrorAction Stop $path).Trim() } catch { '' }
}

$installed = 'mcpls is not on PATH'
if (Get-Command mcpls -ErrorAction SilentlyContinue) {
    $recorded = Read-Marker $stamp
    if ($recorded -eq $version) { exit 0 }
    $current = ''
    try {
        $current = ((& mcpls --version 2>$null) | Select-Object -First 1) -replace '^mcpls\s+(\S+).*$', '$1'
    } catch {}
    $shown = if ($current) { $current } else { 'an unknown version' }
    $installed = "the mcpls on PATH is $shown"
    # An mcpls this hook did not install (cargo, a source build). Record that
    # and never overwrite it.
    if ($recorded -eq '' -or $recorded -eq 'external') {
        New-Item -ItemType Directory -Force -Path $stateDir -ErrorAction SilentlyContinue | Out-Null
        Set-Content -Path $stamp -Value 'external' -ErrorAction SilentlyContinue
        if ($current -eq $version) { exit 0 }
        Complete-Hook "$installed but this plugin expects $version. The plugin did not install it and will not replace it. Tell the user to update mcpls with the installer from $release, or update the plugin, then restart the session."
    }
    $action = "updating mcpls from $shown to $version"
} else {
    $action = "installing mcpls $version"
}

if ((Read-Marker $failed) -eq $version) {
    Write-Note "install of $version failed previously; remove $failed to retry"
    Complete-Hook "installing mcpls $version failed in an earlier session, and $installed. Tell the user to run the installer from $release, or delete $failed and restart the session to retry."
}

New-Item -ItemType Directory -Force -Path $stateDir -ErrorAction SilentlyContinue | Out-Null
# A lock older than the hook's timeout belongs to a session the harness killed.
$held = Get-Item -LiteralPath $lock -ErrorAction SilentlyContinue
if ($held -and $held.LastWriteTime -lt (Get-Date).AddMinutes(-10)) {
    Remove-Item -LiteralPath $lock -Recurse -Force -ErrorAction SilentlyContinue
}
$ownLock = $false
try {
    New-Item -ItemType Directory -Path $lock -ErrorAction Stop | Out-Null
    $ownLock = $true
} catch {
    if (Test-Path -LiteralPath $lock) {
        Complete-Hook "another $harness session is installing mcpls $version, and $installed. Tell the user to restart the session once that install finishes."
    }
    Write-Note "could not create ${lock}; installing without a lock"
}

Write-Note $action

$status = 0
try {
    $installer = "$repoUrl/releases/download/v$version/mcpls-installer.ps1"
    $script = Invoke-RestMethod -Uri $installer -TimeoutSec 300 -ErrorAction Stop
    Invoke-Expression $script *>&1 | ForEach-Object { [Console]::Error.WriteLine("$_") }
} catch {
    $status = 1
    Write-Note "installer failed: $($_.Exception.Message)"
}

if ($status -eq 0) {
    Set-Content -Path $stamp -Value $version -ErrorAction SilentlyContinue
    Remove-Item -Path $failed -Force -ErrorAction SilentlyContinue
} else {
    Set-Content -Path $failed -Value $version -ErrorAction SilentlyContinue
}
if ($ownLock) { Remove-Item -LiteralPath $lock -Recurse -Force -ErrorAction SilentlyContinue }

if ($status -ne 0) {
    Complete-Hook "installing mcpls $version failed, and $installed. Tell the user to run the installer from $release, or delete $failed and restart the session to retry."
}
Complete-Hook "mcpls $version was just installed, but this session started without it, so the MCP server and hooks are not using it yet. Tell the user to restart the session."
