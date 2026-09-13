param([Parameter(Mandatory = $true)][string]$DistInstaller)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$version = (Get-Content -Raw (Join-Path $repo 'plugin/.claude-plugin/plugin.json') | ConvertFrom-Json).version
$engine = (Get-Process -Id $PID).Path
$work = Join-Path ([IO.Path]::GetTempPath()) "mcpls-bootstrap-test-$([guid]::NewGuid())"
New-Item -ItemType Directory $work | Out-Null

function Check($condition, $message) {
    if (-not $condition) { throw $message }
}

try {
    $temp = Join-Path $work 'temp'
    New-Item -ItemType Directory $temp | Out-Null
    $env:TEMP = $temp
    $env:TMP = $temp
    $env:TMPDIR = $temp
    $tokens = $null
    $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($DistInstaller, [ref]$tokens, [ref]$errors)
    Check ($errors.Count -eq 0) 'generated installer must parse'
    $functions = foreach ($name in @('Download', 'Invoke-DownloadFile', 'Get-ExceptionMessage')) {
        $function = $ast.Find({ param($node)
            $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
        }, $true)
        Check ($null -ne $function) "generated installer must define $name"
        $function.Extent.Text
    }
    $download = 'Invoke-DownloadFile -client $wc -url $url -path $dir_path'
    Check (([regex]::Matches(($functions -join "`n"), [regex]::Escape($download))).Count -eq 1) 'pinned archive download call must be unique'

    Set-Content -LiteralPath (Join-Path $work 'mcpls.exe') -Value 'archive fixture'
    Compress-Archive -LiteralPath (Join-Path $work 'mcpls.exe') -DestinationPath (Join-Path $work 'archive.zip')
    $fixture = @'
$ErrorActionPreference = 'Stop'
$InformationPreference = 'Continue'
$app_name = 'mcpls'
$install_updater = $false
$auth_token = $null
function New-Temp-Dir { Join-Path $env:MCPLS_TEST_STATE 'unpacked' }
function WebProxyFromEnvironment { $null }
function New-Object($TypeName, [object[]]$ArgumentList) {
    if ($TypeName -ne 'Net.Webclient') {
        return Microsoft.PowerShell.Utility\New-Object -TypeName $TypeName -ArgumentList $ArgumentList
    }
    $client = [pscustomobject]@{}
    $client | Add-Member ScriptMethod DownloadFile {
        param($url, $path)
        Add-Content -LiteralPath (Join-Path $env:MCPLS_TEST_STATE 'downloads') -Value $url
        Copy-Item -LiteralPath (Join-Path $env:MCPLS_TEST_WORK 'archive.zip') -Destination $path
    }
    $client | Add-Member ScriptMethod DownloadString {
        param($url)
        Add-Content -LiteralPath (Join-Path $env:MCPLS_TEST_STATE 'downloads') -Value $url
        if ($env:MCPLS_TEST_MODE -eq 'missing-checksum') { throw 'checksum unavailable' }
        if ($env:MCPLS_TEST_MODE -eq 'malformed-checksum') { return 'not a checksum' }
        $hash = (Get-FileHash -LiteralPath (Join-Path $env:MCPLS_TEST_WORK 'archive.zip') -Algorithm SHA256).Hash
        if ($env:MCPLS_TEST_MODE -eq 'mismatch') { $hash = '0' * 64 }
        $name = "mcpls-$env:MCPLS_TEST_TARGET.zip"
        if ($env:MCPLS_TEST_MODE -eq 'wrong-filename') { $name = 'another.zip' }
        return "$hash *$name`r`n"
    }
    return $client
}
'@
    $fixture += "`n" + ($functions -join "`n") + "`n" + @'
New-Item -ItemType Directory -Path (New-Temp-Dir) | Out-Null
$platforms = @{}
$platforms[$env:MCPLS_TEST_TARGET] = @{
    zip_ext = '.zip'; bins = @('mcpls.exe'); libs = @(); staticlibs = @()
    artifact_name = "mcpls-$env:MCPLS_TEST_TARGET.zip"
}
Write-Output 'installer stdout diagnostic'
[Console]::Error.WriteLine('installer stderr diagnostic')
try {
    Download -download_url $env:MCPLS_TEST_RELEASE -platforms $platforms -arch $env:MCPLS_TEST_TARGET | Out-Null
    if ($env:MCPLS_TEST_MODE -eq 'exit-failure') { exit 23 }
} catch {
    Write-Information $_
    exit 1
}
'@
    Set-Content -LiteralPath (Join-Path $work 'installer.ps1') -Value $fixture
    $driver = @'
function Get-Command($Name) {
    if ($Name -eq 'mcpls') { return $null }
    Microsoft.PowerShell.Core\Get-Command $Name
}
function Invoke-RestMethod($Uri, $TimeoutSec) {
    if ($Uri -ne "$env:MCPLS_TEST_RELEASE/mcpls-installer.ps1") { throw "wrong installer URL: $Uri" }
    if ($env:MCPLS_TEST_MODE -eq 'network-failure') { throw 'installer unavailable' }
    $script = Get-Content -Raw -LiteralPath (Join-Path $env:MCPLS_TEST_WORK 'installer.ps1')
    if ($env:MCPLS_TEST_MODE -eq 'template-drift') {
        $script = $script.Replace('Invoke-DownloadFile -client $wc -url $url -path $dir_path', 'throw "changed template"')
    }
    $script
}
& $env:MCPLS_TEST_BOOTSTRAP codex
'@
    Set-Content -LiteralPath (Join-Path $work 'driver.ps1') -Value $driver
    $env:MCPLS_TEST_WORK = $work
    $env:MCPLS_TEST_BOOTSTRAP = Join-Path $repo 'plugin/hooks/bootstrap-binaries.ps1'
    $env:MCPLS_TEST_RELEASE = "https://github.com/AbysmalBiscuit/mcpls/releases/download/v$version"
    $env:MCPLS_NO_BOOTSTRAP = $null
    $passed = 0
    foreach ($target in @('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')) {
        Check ((Get-Content -Raw $DistInstaller).Contains('"artifact_name" = "mcpls-' + $target + '.zip"')) "dist must publish the $target ZIP"
        $env:MCPLS_TEST_TARGET = $target
        foreach ($mode in @('exit-failure', 'success', 'mismatch', 'missing-checksum', 'malformed-checksum', 'wrong-filename', 'network-failure', 'template-drift')) {
            $env:MCPLS_TEST_MODE = $mode
            $env:XDG_STATE_HOME = Join-Path $work "$target-$mode"
            $env:MCPLS_TEST_STATE = $env:XDG_STATE_HOME
            New-Item -ItemType Directory $env:XDG_STATE_HOME | Out-Null
            $stdout = Join-Path $work 'stdout'
            $stderr = Join-Path $work 'stderr'
            $ErrorActionPreference = 'Continue'
            & $engine -NoProfile -ExecutionPolicy Bypass -File (Join-Path $work 'driver.ps1') >$stdout 2>$stderr
            $exitCode = $LASTEXITCODE
            $ErrorActionPreference = 'Stop'
            Check ($exitCode -eq 0) "$target/$mode must exit zero, got $exitCode"
            $lines = @(Get-Content -LiteralPath $stdout)
            Check ($lines.Count -eq 1) "$target/$mode must emit exactly one context line"
            $context = $lines[0] | ConvertFrom-Json
            Check ($context.hookSpecificOutput.hookEventName -eq 'SessionStart') "$target/$mode must emit SessionStart"
            $note = $context.hookSpecificOutput.additionalContext
            Check ($note.StartsWith('mcpls plugin: ')) "$target/$mode must prefix context"
            Check (-not (Test-Path (Join-Path $env:XDG_STATE_HOME 'mcpls/install-codex.lock'))) "$target/$mode must release its lock"
            Check (@(Get-ChildItem -LiteralPath $temp -Filter 'mcpls-installer-*.ps1').Count -eq 0) "$target/$mode must remove its temporary installer"
            if ($mode -eq 'success') {
                Check ((Get-Content (Join-Path $env:XDG_STATE_HOME 'mcpls/bootstrap-version')) -eq $version) 'success must stamp the version'
                Check ($note.Contains('restart the session')) 'success must request restart'
                $urls = @(Get-Content (Join-Path $env:XDG_STATE_HOME 'downloads'))
                Check ($urls.Count -eq 2) 'success must fetch archive and checksum'
                Check ($urls[1] -eq "$env:MCPLS_TEST_RELEASE/mcpls-$target.zip.sha256") 'checksum must match the fork archive URL'
            } else {
                Check ((Get-Content (Join-Path $env:XDG_STATE_HOME 'mcpls/bootstrap-failed')) -eq $version) "$mode must mark failure"
                Check (-not (Test-Path (Join-Path $env:XDG_STATE_HOME 'mcpls/bootstrap-version'))) "$mode must not stamp success"
                Check ($note.Contains('failed')) "$mode must report failure"
            }
            if ($mode -in @('success', 'exit-failure')) {
                $diagnostics = Get-Content -Raw -LiteralPath $stderr
                Check ($diagnostics.Contains('installer stdout diagnostic') -and $diagnostics.Contains('installer stderr diagnostic')) 'installer diagnostics must reach stderr'
                Check (Test-Path (Join-Path $env:XDG_STATE_HOME 'unpacked/mcpls.exe')) "valid archive must be extracted: $diagnostics"
            } else {
                Check (-not (Test-Path (Join-Path $env:XDG_STATE_HOME 'unpacked/mcpls.exe'))) "$mode must fail before extraction"
            }
            $passed++
        }
    }
    Write-Output "$passed Windows bootstrap cases passed"

    if ([Environment]::OSVersion.Platform -eq 'Win32NT') {
        $wrapper = Get-Content -Raw (Join-Path $repo 'plugin/hooks/run-hook.cmd')
        # Relocate the fixed Git probes so the real cmd fallback is reachable on CI hosts with Git.
        $wrapper = $wrapper.Replace('C:\Program Files\Git\bin\bash.exe', "$work\missing-git\bash.exe")
        $wrapper = $wrapper.Replace('C:\Program Files (x86)\Git\bin\bash.exe', "$work\missing-git-x86\bash.exe")
        Set-Content -LiteralPath (Join-Path $work 'run-hook.cmd') -Value $wrapper
        Set-Content -LiteralPath (Join-Path $work 'probe.ps1') -Value @'
if ($args.Count -ne 1 -or $args[0] -ne 'codex') { exit 91 }
exit ([int]$env:MCPLS_TEST_EXIT)
'@
        Copy-Item -LiteralPath "$env:SystemRoot\System32\where.exe" -Destination $work
        $env:PATH = "$work;$env:SystemRoot\System32\WindowsPowerShell\v1.0"
        foreach ($status in @(0, 23)) {
            $env:MCPLS_TEST_EXIT = "$status"
            & "$env:SystemRoot\System32\cmd.exe" /d /c "`"$work\run-hook.cmd`" probe codex"
            Check ($LASTEXITCODE -eq $status) "cmd fallback must return PowerShell exit $status, got $LASTEXITCODE"
        }
        Write-Output '2 native cmd fallback cases passed'
    } else {
        Write-Output 'SKIP native cmd fallback cases: requires Windows'
    }
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force
}
