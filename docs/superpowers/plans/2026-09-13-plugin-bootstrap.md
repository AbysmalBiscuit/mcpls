# mcpls plugin PATH bootstrap implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The mcpls plugin works on Linux, macOS and Windows in Claude Code and Codex: a `SessionStart` hook installs `mcpls` onto `PATH` from the matching cargo-dist release, tells the agent whenever the binary and the plugin disagree, and every MCP and hook entry runs the bare name `mcpls`.

**Architecture:** This replaces the launcher from `2026-09-13-plugin-packaging.md` (its Tasks 2, 3 and 5 wired every entry through `plugin/bin/mcpls`). cargo-dist builds releases and publishes `mcpls-installer.sh` and `mcpls-installer.ps1`. `plugin/hooks/bootstrap-binaries`, its PowerShell twin and the `run-hook.cmd` polyglot are ported from devkit, extended with a per-harness install lock and a `SessionStart` context note, and run that installer when `mcpls` is missing or older than the plugin. `plugin/.mcp.json` serves both harnesses, and Codex's manifest names it by path.

**Tech Stack:** Bash, PowerShell, cmd, Rust 2024 (serde_json, toml), GitHub Actions, cargo-dist 0.32, release-please.

**Spec:** `docs/superpowers/specs/2026-09-13-plugin-packaging-design.md`. The devkit originals live at `/home/lev/Git/lev/devkit/hooks/` and `/home/lev/Git/lev/devkit/tests/hooks/bootstrap-binaries.test.sh`.

## Global constraints

- Every MCP server and `mcpls hook` entry runs the bare name `mcpls`. Nothing references `plugin/bin/`.
- The bootstrap takes one argument, `claude` or `codex`, and exits 0 on every path.
- The bootstrap's stdout is empty or exactly one line: `{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"mcpls plugin: ..."}}`. Installer output goes to stderr.
- No file named `plugin/plugin.json` may exist.
- Release repository: `https://github.com/AbysmalBiscuit/mcpls`. Tag: `v<version>`. Installers: `releases/download/v<version>/mcpls-installer.sh` and `mcpls-installer.ps1`.
- Bootstrap state, under `${XDG_STATE_HOME:-$HOME/.local/state}/mcpls/`: `bootstrap-version`, `bootstrap-failed`, and the lock directories `install-claude.lock` and `install-codex.lock`. A lock older than 10 minutes is stale. Opt-out: `MCPLS_NO_BOOTSTRAP=1`.
- `mcpls --version` prints `mcpls <version>` (clap's `#[command(version)]` in `crates/mcpls-cli/src/args.rs`).
- One version string, equal to `Cargo.toml`'s `workspace.package.version`, appears in `plugin/.claude-plugin/plugin.json` (`$.version`), `plugin/.codex-plugin/plugin.json` (`$.version`) and `.claude-plugin/marketplace.json` (`$.plugins[0].version`).
- `.github/workflows/release.yml` is generated. Change `dist-workspace.toml` and run `dist generate`; never edit the workflow by hand. The dist version in `dist-workspace.toml` and in CI's install step must match.
- `dist` shells out to cargo. On a machine whose `~/.cargo/config.toml` sets a `rustc-wrapper` that can fail, run it as `env RUSTC_WRAPPER= dist ...`.
- Codex behaviour is pinned to `rust-v0.154.0`. Re-resolve with `docm info codex` and read the cited paths under `codex-rs/`.
- Rust: MSRV 1.88, edition 2024, clippy `all`, `pedantic` and `nursery` denied in CI. Integration test files open with `#![allow(clippy::unwrap_used)]`.
- Commands: `devrun task fmt`, `devrun task lint`, `devrun task test`, `devrun task verify`. A single Rust test: `cargo nextest run -p mcpls <name>`.
- Commits: Conventional Commits, subject at most 50 characters, ending with the executing agent's `Co-Authored-By` trailer per `AGENTS.md`. The commands below show Claude Opus 5's; swap in your own model.

## File map

- Create `dist-workspace.toml`; modify `Cargo.toml` (`[profile.dist]`) and `.github/workflows/release.yml` (generated). Already produced by `dist init`; Task 1 commits them with a CI drift check in `.github/workflows/ci.yml`.
- Create `plugin/hooks/bootstrap-binaries`, `plugin/hooks/bootstrap-binaries.ps1`, `plugin/hooks/run-hook.cmd`, `tests/plugin/bootstrap-binaries.test.sh`. Task 2.
- Modify `plugin/.mcp.json`, `plugin/.codex-plugin/plugin.json`, `plugin/hooks/hooks.json`, `plugin/hooks/hooks-codex.json`, `crates/mcpls-cli/tests/plugin_manifests.rs`, `.gitattributes`, `.github/workflows/ci.yml`. Delete `plugin/bin/mcpls`, `tests/plugin/launcher.test.sh`. Task 3.
- Modify `plugin/README.md`. Task 4.

---

### Task 1: Commit the spec, this plan and the cargo-dist setup

**Files:**
- Modify: `docs/superpowers/specs/2026-09-13-plugin-packaging-design.md`, `docs/superpowers/plans/2026-09-13-plugin-packaging.md`
- Create: `docs/superpowers/plans/2026-09-13-plugin-bootstrap.md`, `dist-workspace.toml`
- Modify: `Cargo.toml`, `.github/workflows/release.yml`, `.github/workflows/ci.yml`

**Interfaces:**
- Produces: release assets `mcpls-installer.sh` and `mcpls-installer.ps1` on every `v<version>` release, which Task 2's bootstrap fetches.

- [x] **Step 1: Confirm the dist config**

`dist-workspace.toml` must read:

```toml
[workspace]
members = ["cargo:."]

# Config for 'dist'
[dist]
# The preferred dist version to use in CI (Cargo.toml SemVer syntax)
cargo-dist-version = "0.32.0"
# CI backends to support
ci = "github"
# The installers to generate for each app
installers = ["shell", "powershell"]
# Target platforms to build apps for (Rust target-triple syntax)
targets = ["aarch64-apple-darwin", "aarch64-unknown-linux-gnu", "aarch64-pc-windows-msvc", "x86_64-apple-darwin", "x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl", "x86_64-pc-windows-msvc"]
# Path that installers should place binaries in
install-path = "CARGO_HOME"
# Whether to install an updater program
install-updater = true
# Whether dist should create a Github Release or use an existing draft
create-release = false
```

`Cargo.toml` must end with this, and no `lto` override, so dist builds inherit `[profile.release]`'s full LTO:

```toml
# The profile that 'dist' will build with
[profile.dist]
inherits = "release"
```

- [x] **Step 2: Check the plan and the generated workflow**

Run: `env RUSTC_WRAPPER= dist plan`
Expected: `mcpls-installer.sh`, `mcpls-installer.ps1`, and one archive per target in `dist-workspace.toml`, each holding only the `mcpls` binary.

Run: `env RUSTC_WRAPPER= dist generate --check`
Expected: exit 0.

- [x] **Step 3: Fail CI when release.yml drifts from the config**

In `.github/workflows/ci.yml`, in the `detect-changes` job:
- Under `outputs:`, after `plugin: ${{ steps.filter.outputs.plugin }}`, add `release: ${{ steps.filter.outputs.release }}`.
- Under the path filter's `filters: |`, after the `plugin:` filter's entries, add:

```yaml
            release:
              - 'dist-workspace.toml'
              - 'Cargo.toml'
              - '.github/workflows/release.yml'
              - '.github/workflows/ci.yml'
```

After the `shellcheck` job, add:

```yaml
  dist-check:
    name: Release workflow matches dist config
    needs: detect-changes
    if: needs.detect-changes.outputs.release == 'true'
    runs-on: ubuntu-latest
    timeout-minutes: 5
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v6
      - name: Install dist
        shell: bash
        run: "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.sh | sh"
      - name: Check release.yml
        run: dist generate --check
```

In the `ci-gate` job, add `dist-check` to `needs`, after `shellcheck`, and add `"${{ needs.dist-check.result }}"` to `results`, after `"${{ needs.shellcheck.result }}"`.

Run: `yq '.jobs.ci-gate.needs' .github/workflows/ci.yml`
Expected: the list includes `dist-check`.

- [x] **Step 4: Commit the docs**

```bash
git add docs/superpowers/specs/2026-09-13-plugin-packaging-design.md docs/superpowers/plans/2026-09-13-plugin-packaging.md docs/superpowers/plans/2026-09-13-plugin-bootstrap.md
git commit -m "docs(plugin): install mcpls onto PATH at startup" -m "The launcher cannot be an MCP command on Windows. A SessionStart
hook ported from devkit installs the cargo-dist release instead, and
every entry runs the bare name.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

- [x] **Step 5: Commit the release build**

```bash
git add dist-workspace.toml Cargo.toml .github/workflows/release.yml .github/workflows/ci.yml
git commit -m "build(release): build releases with cargo-dist" -m "dist publishes shell and PowerShell installers alongside the archives,
which the plugin's bootstrap hook runs. release-please still creates
the release; create-release = false makes dist upload to it. CI fails
when release.yml no longer matches dist-workspace.toml.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: The bootstrap hook

**Files:**
- Create: `tests/plugin/bootstrap-binaries.test.sh`
- Create: `plugin/hooks/bootstrap-binaries`
- Create: `plugin/hooks/bootstrap-binaries.ps1`
- Create: `plugin/hooks/run-hook.cmd`
- Modify: `.gitattributes`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: the installer asset names from Task 1.
- Produces: `plugin/hooks/run-hook.cmd bootstrap-binaries <claude|codex>`, the command Task 3's hook files register.

- [x] **Step 1: Write the failing test**

Create `tests/plugin/bootstrap-binaries.test.sh`:

```bash
#!/usr/bin/env bash
# Behavioural tests for plugin/hooks/bootstrap-binaries.
#
# The hook installs mcpls from a GitHub release, so every case runs it against
# stubbed curl, uname and powershell.exe on a stubbed PATH. Nothing is
# downloaded and nothing outside the test's temporary directory is touched.
#
# Run: bash tests/plugin/bootstrap-binaries.test.sh

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HOOK="${REPO_ROOT}/plugin/hooks/bootstrap-binaries"
MANIFEST="${REPO_ROOT}/plugin/.claude-plugin/plugin.json"

VERSION=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$MANIFEST" | head -n 1)
if [ -z "$VERSION" ]; then
    echo "could not read a version from ${MANIFEST}" >&2
    exit 1
fi

RELEASE="https://github.com/AbysmalBiscuit/mcpls/releases/download/v${VERSION}"
EXPECTED_CURL="curl --proto =https --tlsv1.2 -LsSf --connect-timeout 10 --max-time 300 ${RELEASE}/mcpls-installer.sh
installer-ran"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

STUB="${WORK}/stub"
BIN="${WORK}/bin"
mkdir -p "$STUB" "$BIN"
CALLS="${WORK}/calls"

# The "installer" curl emits prints to stdout the way a real one does, so a
# case can catch the hook leaking installer output onto its own stdout.
cat >"${STUB}/curl" <<'EOF'
#!/usr/bin/env bash
echo "curl $*" >>"$CURL_LOG"
[ "${CURL_FAIL:-0}" = "1" ] && exit 1
echo "echo installer-ran >>\"$CURL_LOG\"; echo installer-progress"
EOF

cat >"${STUB}/powershell.exe" <<'EOF'
#!/usr/bin/env bash
echo "powershell $*" >>"$CURL_LOG"
echo powershell-progress
EOF

cat >"${STUB}/uname" <<'EOF'
#!/usr/bin/env bash
echo "${FAKE_UNAME:-Linux}"
EOF

# The stand-in mcpls reports FAKE_MCPLS_VERSION the way clap prints a version.
cat >"${BIN}/mcpls" <<'EOF'
#!/usr/bin/env bash
[ "${1:-}" = "--version" ] && echo "mcpls ${FAKE_MCPLS_VERSION:-0.0.1}"
EOF

chmod +x "${STUB}/curl" "${STUB}/powershell.exe" "${STUB}/uname" "${BIN}/mcpls"

pass=0
fail=0
state=""
state_seq=0
last_exit=0

check() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        printf 'FAIL %s\n  expected: %s\n  actual:   %s\n' "$label" "$expected" "$actual" >&2
    fi
}

new_state() {
    state_seq=$((state_seq + 1))
    state="${WORK}/state-${state_seq}"
    mkdir -p "$state/mcpls"
}

# run_hook <with-mcpls|without-mcpls> <harness> [VAR=value...]
# env -i keeps the caller's real mcpls, MCPLS_* and XDG_* out of every case.
run_hook() {
    local binary="$1" harness="$2"
    shift 2
    local path="$STUB"
    [ "$binary" = with-mcpls ] && path="${BIN}:${STUB}"
    rm -f "$CALLS"
    env -i HOME="$WORK" PATH="${path}:/usr/bin:/bin" XDG_STATE_HOME="$state" \
        CURL_LOG="$CALLS" "$@" bash "$HOOK" "$harness" >"${WORK}/stdout" 2>"${WORK}/stderr"
    last_exit=$?
}

run_wrapper() {
    rm -f "$CALLS"
    env -i HOME="$WORK" PATH="${BIN}:${STUB}:/usr/bin:/bin" XDG_STATE_HOME="$state" \
        CURL_LOG="$CALLS" FAKE_MCPLS_VERSION="$VERSION" \
        bash "${REPO_ROOT}/plugin/hooks/run-hook.cmd" bootstrap-binaries claude \
        >"${WORK}/stdout" 2>"${WORK}/stderr"
    last_exit=$?
}

stamp() { cat "${state}/mcpls/bootstrap-version" 2>/dev/null || echo NONE; }
marker() { cat "${state}/mcpls/bootstrap-failed" 2>/dev/null || echo NONE; }
calls() { cat "$CALLS" 2>/dev/null || echo NONE; }
out() { cat "${WORK}/stdout"; }
set_stamp() { printf '%s\n' "$1" >"${state}/mcpls/bootstrap-version"; }
lock() { printf '%s' "${state}/mcpls/install-$1.lock"; }

# yes when stdout is exactly one SessionStart context object containing $1.
context_says() {
    local o
    o=$(out)
    if [[ "$o" == '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"mcpls plugin: '*'"}}' &&
        "$o" == *"$1"* && "$o" != *$'\n'* ]]; then
        echo yes
    else
        echo no
    fi
}

echo "testing plugin/hooks/bootstrap-binaries against plugin version ${VERSION}"

new_state
run_hook with-mcpls claude MCPLS_NO_BOOTSTRAP=1
check "opt-out exits 0" 0 "$last_exit"
check "opt-out touches nothing" NONE "$(stamp)"
check "opt-out says nothing" "" "$(out)"

new_state
run_hook without-mcpls claude
check "a missing mcpls installs" 0 "$last_exit"
check "install records the version" "$VERSION" "$(stamp)"
check "install pins the release" "$EXPECTED_CURL" "$(calls)"
check "install tells the agent to restart" yes "$(context_says "restart the session")"
check "install releases its lock" no "$([ -e "$(lock claude)" ] && echo yes || echo no)"

# An mcpls the hook did not install must survive untouched.
new_state
run_hook with-mcpls claude FAKE_MCPLS_VERSION="$VERSION"
check "an unstamped mcpls is external" external "$(stamp)"
check "an external mcpls skips the network" NONE "$(calls)"
check "a matching external mcpls says nothing" "" "$(out)"

new_state
run_hook with-mcpls claude FAKE_MCPLS_VERSION=0.0.1
check "a mismatched external mcpls stays external" external "$(stamp)"
check "a mismatched external mcpls skips the network" NONE "$(calls)"
check "a mismatched external mcpls names both versions" yes \
    "$(context_says "the mcpls on PATH is 0.0.1 but this plugin expects ${VERSION}")"

new_state
set_stamp "$VERSION"
run_hook with-mcpls claude
check "the current version is a no-op" NONE "$(calls)"
check "the current version keeps the stamp" "$VERSION" "$(stamp)"
check "the current version says nothing" "" "$(out)"

# A plugin update moves plugin.json's version past the stamp.
new_state
set_stamp 0.0.1
run_hook with-mcpls claude
check "a stale stamp reinstalls" "$VERSION" "$(stamp)"
check "a reinstall pins the new release" "$EXPECTED_CURL" "$(calls)"
check "a reinstall tells the agent to restart" yes "$(context_says "restart the session")"

new_state
run_hook without-mcpls claude CURL_FAIL=1
check "a failed install still exits 0" 0 "$last_exit"
check "a failed install records no version" NONE "$(stamp)"
check "a failed install marks the version" "$VERSION" "$(marker)"
check "a failed install tells the agent" yes "$(context_says "installing mcpls ${VERSION} failed")"

# Having marked a failure, the hook must not retry it every session, but the
# agent still hears about it.
run_hook without-mcpls claude
check "a marked failure suppresses the retry" NONE "$(calls)"
check "a marked failure still tells the agent" yes "$(context_says "failed in an earlier session")"

new_state
mkdir "$(lock claude)"
run_hook without-mcpls claude
check "a held lock skips the install" NONE "$(calls)"
check "a held lock tells the agent" yes "$(context_says "another claude session is installing")"

new_state
mkdir "$(lock codex)"
run_hook without-mcpls claude
check "the other harness's lock does not block" "$EXPECTED_CURL" "$(calls)"
check "the other harness's lock stays" yes "$([ -d "$(lock codex)" ] && echo yes || echo no)"

new_state
mkdir "$(lock claude)"
touch -t 200001010000 "$(lock claude)"
run_hook without-mcpls claude
check "a stale lock is taken over" "$EXPECTED_CURL" "$(calls)"

new_state
run_hook without-mcpls codex FAKE_UNAME=MINGW64_NT-10.0
check "windows exits 0" 0 "$last_exit"
check "windows runs the PowerShell installer" \
    "powershell -NoProfile -ExecutionPolicy Bypass -Command irm ${RELEASE}/mcpls-installer.ps1 | iex" \
    "$(calls)"
check "windows keeps installer output off stdout" yes "$(context_says "restart the session")"

# The wrapper the hook files invoke, rather than the hook directly.
new_state
run_wrapper
check "run-hook.cmd dispatches on a POSIX shell" external "$(stamp)"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
```

- [x] **Step 2: Run it to verify it fails**

Run: `bash tests/plugin/bootstrap-binaries.test.sh`
Expected: FAIL lines starting with `opt-out exits 0` (bash exits 127 because `plugin/hooks/bootstrap-binaries` does not exist), and a non-zero exit.

- [x] **Step 3: Write the bash bootstrap**

Create `plugin/hooks/bootstrap-binaries`:

```bash
#!/usr/bin/env bash
# SessionStart hook: ensure the mcpls this plugin drives is on PATH and matches
# the plugin's version, installing it from the matching GitHub release.
#
# Usage: bootstrap-binaries <claude|codex>
#
# The MCP server and every other hook run `mcpls` from PATH, and a plugin
# install cannot place it there, so this runs at session start instead.
#
# Every path exits 0: a session must start even with no network. Both
# harnesses read a SessionStart hook's stdout, so installer output goes to
# stderr and stdout carries at most one context object telling the agent
# when the binary does not match the plugin.

set -uo pipefail

[ "${MCPLS_NO_BOOTSTRAP:-}" = "1" ] && exit 0

REPO_URL="https://github.com/AbysmalBiscuit/mcpls"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PLUGIN_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
MANIFEST="${PLUGIN_ROOT}/.claude-plugin/plugin.json"

# Installs are serialised per harness, so the lock is named after it.
case "${1:-}" in
    claude | codex) harness="$1" ;;
    *) harness=unknown ;;
esac

warn() { printf 'mcpls plugin: %s\n' "$1" >&2; }

# Ends the hook, handing the agent a note when there is one.
finish() {
    if [ -n "${1:-}" ]; then
        local note="${1//\\/\\\\}"
        note="${note//\"/\\\"}"
        printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"mcpls plugin: %s"}}\n' "$note"
    fi
    exit 0
}

version=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$MANIFEST" 2>/dev/null | head -n 1)
if [ -z "$version" ]; then
    warn "could not read a version from ${MANIFEST}; skipping the mcpls install"
    exit 0
fi

release="${REPO_URL}/releases/tag/v${version}"
state_dir="${XDG_STATE_HOME:-${HOME}/.local/state}/mcpls"
stamp="${state_dir}/bootstrap-version"
failed="${state_dir}/bootstrap-failed"
lock="${state_dir}/install-${harness}.lock"

installed="mcpls is not on PATH"
if command -v mcpls >/dev/null 2>&1; then
    recorded=$(cat "$stamp" 2>/dev/null || true)
    [ "$recorded" = "$version" ] && exit 0
    current=$(mcpls --version 2>/dev/null | sed -n 's/^mcpls \([^[:space:]]*\).*/\1/p' | head -n 1)
    installed="the mcpls on PATH is ${current:-an unknown version}"
    # An mcpls this hook did not install (cargo, a package manager, a source
    # build). Record that and never overwrite it.
    if [ -z "$recorded" ] || [ "$recorded" = external ]; then
        mkdir -p "$state_dir" 2>/dev/null && printf 'external\n' >"$stamp" 2>/dev/null
        [ "$current" = "$version" ] && exit 0
        finish "${installed} but this plugin expects ${version}. The plugin did not install it and will not replace it. Tell the user to update mcpls with the installer from ${release}, or update the plugin, then restart the session."
    fi
    action="updating mcpls from ${current:-an unknown version} to ${version}"
else
    action="installing mcpls ${version}"
fi

if [ "$(cat "$failed" 2>/dev/null || true)" = "$version" ]; then
    warn "install of ${version} failed previously; remove ${failed} to retry"
    finish "installing mcpls ${version} failed in an earlier session, and ${installed}. Tell the user to run the installer from ${release}, or delete ${failed} and restart the session to retry."
fi

mkdir -p "$state_dir" 2>/dev/null
# A lock older than the hook's timeout belongs to a session the harness killed.
if [ -n "$(find "$lock" -maxdepth 0 -mmin +10 2>/dev/null)" ]; then
    rmdir "$lock" 2>/dev/null
fi
if mkdir "$lock" 2>/dev/null; then
    trap 'rmdir "$lock" 2>/dev/null' EXIT
elif [ -d "$lock" ]; then
    finish "another ${harness} session is installing mcpls ${version}, and ${installed}. Tell the user to restart the session once that install finishes."
else
    warn "could not create ${lock}; installing without a lock"
fi

warn "$action"

case "$(uname -s 2>/dev/null || echo unknown)" in
    MINGW* | MSYS* | CYGWIN*)
        installer="${REPO_URL}/releases/download/v${version}/mcpls-installer.ps1"
        powershell.exe -NoProfile -ExecutionPolicy Bypass \
            -Command "irm ${installer} | iex" >&2
        ;;
    *)
        installer="${REPO_URL}/releases/download/v${version}/mcpls-installer.sh"
        curl --proto '=https' --tlsv1.2 -LsSf \
            --connect-timeout 10 --max-time 300 "$installer" | sh >&2
        ;;
esac
status=$?

if [ "$status" -ne 0 ]; then
    printf '%s\n' "$version" >"$failed" 2>/dev/null
    warn "installer exited ${status}"
    finish "installing mcpls ${version} failed, and ${installed}. Tell the user to run the installer from ${release}, or delete ${failed} and restart the session to retry."
fi

printf '%s\n' "$version" >"$stamp" 2>/dev/null
rm -f "$failed" 2>/dev/null
finish "mcpls ${version} was just installed, but this session started without it, so the MCP server and hooks are not using it yet. Tell the user to restart the session."
```

- [x] **Step 4: Write the PowerShell twin**

Create `plugin/hooks/bootstrap-binaries.ps1`. PowerShell sends `Write-Host` and an installer's output to the host's stdout under `-File`, so the install pipes every stream to stderr:

```powershell
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
```

- [x] **Step 5: Write the polyglot wrapper**

Create `plugin/hooks/run-hook.cmd`, copied from devkit with only its last comment changed:

```cmd
: << 'CMDBLOCK'
@echo off
REM Cross-platform polyglot wrapper for hook scripts.
REM On Windows: cmd.exe runs the batch portion, which finds and calls bash.
REM On Unix: the shell interprets this as a script (: is a no-op in bash).
REM
REM Hook scripts use extensionless filenames so Windows .sh auto-detection
REM doesn't interfere.
REM
REM Usage: run-hook.cmd <script-name> [args...]

if "%~1"=="" (
    echo run-hook.cmd: missing script name >&2
    exit /b 1
)

set "HOOK_DIR=%~dp0"

REM Try Git for Windows bash in standard locations
if exist "C:\Program Files\Git\bin\bash.exe" (
    "C:\Program Files\Git\bin\bash.exe" "%HOOK_DIR%%~1" %2 %3 %4 %5 %6 %7 %8 %9
    exit /b %ERRORLEVEL%
)
if exist "C:\Program Files (x86)\Git\bin\bash.exe" (
    "C:\Program Files (x86)\Git\bin\bash.exe" "%HOOK_DIR%%~1" %2 %3 %4 %5 %6 %7 %8 %9
    exit /b %ERRORLEVEL%
)

REM Try bash on PATH (user-installed Git Bash, MSYS2, Cygwin)
where bash >nul 2>nul
if %ERRORLEVEL% equ 0 (
    bash "%HOOK_DIR%%~1" %2 %3 %4 %5 %6 %7 %8 %9
    exit /b %ERRORLEVEL%
)

REM No bash. Fall back to a PowerShell twin of the hook where one exists, so a
REM Windows host without Git for Windows still gets hooks that must run.
if exist "%HOOK_DIR%%~1.ps1" (
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%HOOK_DIR%%~1.ps1" %2 %3 %4 %5 %6 %7 %8 %9
    exit /b %ERRORLEVEL%
)

REM Bash-only hook with no bash: exit silently rather than error, so the
REM session still starts. Every hook this plugin ships has a .ps1 twin.
exit /b 0
CMDBLOCK

# Unix: run the named script directly
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SCRIPT_NAME="$1"
shift
exec bash "${SCRIPT_DIR}/${SCRIPT_NAME}" "$@"
```

Then mark both scripts executable:

```bash
chmod +x plugin/hooks/bootstrap-binaries plugin/hooks/run-hook.cmd
```

- [x] **Step 6: Keep LF line endings**

In `.gitattributes`, after `plugin/bin/* text eol=lf`, add:

```
plugin/hooks/bootstrap-binaries text eol=lf
plugin/hooks/run-hook.cmd text eol=lf
```

- [x] **Step 7: Run the test to verify it passes**

Run: `bash tests/plugin/bootstrap-binaries.test.sh`
Expected: `0 failed` on the last line, exit 0.

Run: `shellcheck plugin/hooks/bootstrap-binaries tests/plugin/bootstrap-binaries.test.sh`
Expected: no output, exit 0. Without a local `shellcheck`, `bunx --bun shellcheck` runs the same check.

- [x] **Step 8: Run the test in CI**

In `.github/workflows/ci.yml`, replace the shellcheck job's last step:

```yaml
      - name: Shellcheck plugin launcher
        run: shellcheck plugin/bin/mcpls tests/plugin/launcher.test.sh
```

with:

```yaml
      - name: Shellcheck plugin scripts
        run: shellcheck plugin/bin/mcpls tests/plugin/launcher.test.sh plugin/hooks/bootstrap-binaries tests/plugin/bootstrap-binaries.test.sh
```

In the `plugin-launcher` job, after the `Launcher tests` step, add:

```yaml
      - name: Bootstrap tests
        shell: bash
        run: bash tests/plugin/bootstrap-binaries.test.sh
```

- [x] **Step 9: Commit**

```bash
git add tests/plugin/bootstrap-binaries.test.sh plugin/hooks/bootstrap-binaries plugin/hooks/bootstrap-binaries.ps1 plugin/hooks/run-hook.cmd .gitattributes .github/workflows/ci.yml
git commit -m "feat(plugin): add a hook that installs mcpls" -m "Ported from devkit. It runs the release's cargo-dist installer when
mcpls is missing or older than the plugin, one session per harness at
a time, and never overwrites an mcpls it did not install. When the
binary and the plugin disagree it tells the agent through SessionStart
additionalContext, so the user hears what to update.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: Every entry runs mcpls from PATH

**Files:**
- Modify: `crates/mcpls-cli/tests/plugin_manifests.rs`
- Modify: `plugin/.mcp.json`, `plugin/.codex-plugin/plugin.json`, `plugin/hooks/hooks.json`, `plugin/hooks/hooks-codex.json`
- Modify: `.gitattributes`, `.github/workflows/ci.yml`
- Delete: `plugin/bin/mcpls`, `tests/plugin/launcher.test.sh`

**Interfaces:**
- Consumes: `plugin/hooks/run-hook.cmd bootstrap-binaries <claude|codex>` from Task 2.
- Produces: the entry shapes Task 4's README describes.

- [x] **Step 1: Write the failing test**

In `crates/mcpls-cli/tests/plugin_manifests.rs`, replace the module doc comment (lines 1-4) with:

```rust
//! The plugin's manifests are JSON that other programs read, so these check
//! the invariants that break without an error: the version every manifest
//! pins, the `PATH` lookup every entry runs, and the manifest file Codex must
//! never find.
```

Replace the doc comment on `test_every_manifest_pins_the_workspace_version` with:

```rust
/// The bootstrap installs the release named by the Claude Code manifest's
/// version, so every file repeating that version must agree with the
/// workspace the release is built from.
```

Replace `test_every_entry_runs_the_launcher_for_its_harness` and its doc comment with:

```rust
/// Every entry a harness runs names `mcpls` on `PATH`, which the bootstrap
/// installs at session start, and each harness's hook file names only events
/// that harness has.
#[test]
fn test_every_entry_runs_mcpls_from_path() {
    assert_eq!(
        json("plugin/.mcp.json")["mcpServers"]["mcpls"],
        serde_json::json!({ "command": "mcpls" })
    );

    let codex = json("plugin/.codex-plugin/plugin.json");
    assert_eq!(codex["hooks"], "./hooks/hooks-codex.json");
    assert_eq!(
        codex["mcpServers"], "./.mcp.json",
        "an inline object would replace the shared registration"
    );

    let harnesses: [(&str, &str, &str, &[&str]); 2] = [
        (
            "plugin/hooks/hooks.json",
            "mcpls hook",
            "\"${CLAUDE_PLUGIN_ROOT}/hooks/run-hook.cmd\" bootstrap-binaries claude",
            &[
                "FileChanged",
                "PostToolBatch",
                "SessionEnd",
                "SessionStart",
                "UserPromptSubmit",
            ],
        ),
        (
            "plugin/hooks/hooks-codex.json",
            "mcpls hook --host codex",
            "\"${PLUGIN_ROOT}/hooks/run-hook.cmd\" bootstrap-binaries codex",
            &[
                "PostToolUse",
                "SessionEnd",
                "SessionStart",
                "SubagentStop",
                "UserPromptSubmit",
            ],
        ),
    ];
    for (file, hook, bootstrap, events) in harnesses {
        let hooks = json(file);
        let hooks = hooks["hooks"].as_object().unwrap();
        let mut names: Vec<&str> = hooks.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, events, "{file}");

        let mut bootstraps = Vec::new();
        for (event, groups) in hooks {
            let groups = groups.as_array().unwrap();
            assert!(!groups.is_empty(), "{file}: {event}");
            for group in groups {
                let registrations = group["hooks"].as_array().unwrap();
                assert!(!registrations.is_empty(), "{file}: {event}");
                for registration in registrations {
                    assert_eq!(registration["type"], "command", "{file}: {event}");
                    if registration["command"] == bootstrap {
                        bootstraps.push(event.as_str());
                    } else {
                        assert_eq!(registration["command"], hook, "{file}: {event}");
                    }
                }
            }
        }
        assert_eq!(
            bootstraps,
            ["SessionStart"],
            "{file}: the bootstrap runs once, at session start"
        );
    }

    assert_eq!(
        json("plugin/hooks/hooks-codex.json")["hooks"]["SessionStart"][0]["hooks"][0]
            ["commandWindows"],
        "& \"${PLUGIN_ROOT}/hooks/run-hook.cmd\" bootstrap-binaries codex",
        "Codex runs Windows hooks through PowerShell, where a quoted path needs the call operator"
    );
}
```

- [x] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p mcpls test_every_entry_runs_mcpls_from_path`
Expected: FAIL on the first assertion, because `plugin/.mcp.json` still names `${CLAUDE_PLUGIN_ROOT}/bin/mcpls`.

- [x] **Step 3: Point the registrations at mcpls**

Replace `plugin/.mcp.json` with:

```json
{
  "mcpServers": {
    "mcpls": { "command": "mcpls" }
  }
}
```

In `plugin/.codex-plugin/plugin.json`, replace the whole `"mcpServers": { ... }` object with:

```json
  "mcpServers": "./.mcp.json",
```

Replace `plugin/hooks/hooks.json` with:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "\"${CLAUDE_PLUGIN_ROOT}/hooks/run-hook.cmd\" bootstrap-binaries claude",
            "timeout": 300
          },
          { "type": "command", "command": "mcpls hook" }
        ]
      }
    ],
    "FileChanged": [{ "hooks": [{ "type": "command", "command": "mcpls hook" }] }],
    "PostToolBatch": [{ "hooks": [{ "type": "command", "command": "mcpls hook" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "mcpls hook" }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "command": "mcpls hook" }] }]
  }
}
```

Replace `plugin/hooks/hooks-codex.json` with:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "\"${PLUGIN_ROOT}/hooks/run-hook.cmd\" bootstrap-binaries codex",
            "commandWindows": "& \"${PLUGIN_ROOT}/hooks/run-hook.cmd\" bootstrap-binaries codex",
            "timeout": 300
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "apply_patch",
        "hooks": [{ "type": "command", "command": "mcpls hook --host codex" }]
      }
    ],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "mcpls hook --host codex" }] }],
    "SubagentStop": [{ "hooks": [{ "type": "command", "command": "mcpls hook --host codex" }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "command": "mcpls hook --host codex" }] }]
  }
}
```

`timeout` is the key both harnesses read, in seconds (Codex: `config/src/hook_config.rs`, `#[serde(rename = "timeout")]`). It must stay below the bootstrap's 10-minute stale-lock age, so a killed install's lock is stale by the time another session checks.

- [x] **Step 4: Run the test to verify it passes**

Run: `cargo nextest run -p mcpls --test plugin_manifests`
Expected: 3 tests pass.

- [x] **Step 5: Remove the launcher**

```bash
git rm plugin/bin/mcpls tests/plugin/launcher.test.sh
```

In `.gitattributes`, delete the line `plugin/bin/* text eol=lf`.

In `.github/workflows/ci.yml`:
- The shellcheck step's command becomes `shellcheck plugin/hooks/bootstrap-binaries tests/plugin/bootstrap-binaries.test.sh`.
- Delete the `Launcher tests` step from the `plugin-launcher` job.
- Rename the job id `plugin-launcher` to `plugin-bootstrap` and its `name` to `Plugin bootstrap (${{ matrix.os }})`, then update both references in the `ci-gate` job: the `needs` list and `"${{ needs.plugin-launcher.result }}"`.

Run: `rg -n "plugin-launcher|launcher.test|bin/mcpls|MCPLS_BIN|MCPLS_HOME" .github .gitattributes plugin crates tests`
Expected: no matches.

- [x] **Step 6: Commit**

```bash
git add plugin/.mcp.json plugin/.codex-plugin/plugin.json plugin/hooks/hooks.json plugin/hooks/hooks-codex.json crates/mcpls-cli/tests/plugin_manifests.rs .gitattributes .github/workflows/ci.yml
git commit -m "feat(plugin): run mcpls from PATH" -m "A script cannot be an MCP command on Windows, and neither harness has
a per-platform command field. Every entry runs the bare name the
bootstrap installs, and Codex reads the shared .mcp.json by path.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: Plugin README

**Files:**
- Modify: `plugin/README.md`

**Interfaces:**
- Consumes: the bootstrap behaviour from Task 2 and the entries from Task 3.

- [x] **Step 1: Rewrite the install section**

Replace the paragraph beginning "The plugin downloads the mcpls release" with:

```markdown
The plugin installs the mcpls release matching its own version when a session starts, using that release's installer, which places `mcpls` in `$CARGO_HOME/bin` (`~/.cargo/bin` by default) and adds it to `PATH`. An `mcpls` already on `PATH` that the plugin did not install is left alone. Language servers such as `rust-analyzer` are not installed; mcpls drives whichever ones are already on `PATH`.
```

Replace the two paragraphs beginning "Restart the session once after installing." and "Codex waits for the first download" with:

```markdown
Restart the session once after installing, from a new terminal if `~/.cargo/bin` was not already on your `PATH`. The first session starts before `mcpls` exists, so its MCP server fails to start and its hooks report `mcpls` as not found. Codex also asks you to trust the plugin's hooks on first load.

When the `mcpls` on `PATH` does not match the plugin's version, the plugin tells the agent at session start, including which version to install and where to get it. That covers an `mcpls` you installed yourself, an install that failed, and an install still running in another session. A failed install is not retried every session: run the installer from the release page, or delete `~/.local/state/mcpls/bootstrap-failed` and restart.
```

Replace the paragraph beginning "To run a local build instead of the release" and its fish block with:

````markdown
To run a local build instead of the release, put it first on `PATH` and stop the plugin from installing over it on upgrade:

```fish
set -gx MCPLS_NO_BOOTSTRAP 1
set -gx PATH /path/to/mcpls/target/debug $PATH
```
````

- [x] **Step 2: Update the doctor's PATH line and the file list**

Replace the `mcpls on PATH:` bullet with:

```markdown
- **`mcpls on PATH:`** the absolute path of a candidate found on `PATH`, or `not found`. The doctor does not execute it: a text file named `mcpls.exe` on Windows is still a candidate, not a verified installation. The plugin's MCP server and hooks run this same `PATH` lookup, so this is the binary they run.
```

Replace the first two bullets under "What's included" (the `bin/mcpls` bullet and the `.mcp.json` bullet) with:

```markdown
- `hooks/bootstrap-binaries` installs the mcpls release this plugin version pins when a session starts, and tells the agent when the `mcpls` on `PATH` does not match. `hooks/run-hook.cmd` runs it through bash, or through its PowerShell twin on Windows without Git Bash.
- `.mcp.json` registers the server with both harnesses, and `hooks/hooks.json` wires Claude Code's hooks.
```

Replace the `.codex-plugin/plugin.json` bullet's first sentence with "`.codex-plugin/plugin.json` and `hooks/hooks-codex.json` register the plugin with Codex."

Run: `rg -n "launcher|MCPLS_BIN|MCPLS_HOME|bin/mcpls|MCP_TIMEOUT|downloads the mcpls" plugin/README.md`
Expected: no matches.

- [x] **Step 3: Commit**

```bash
git add plugin/README.md
git commit -m "docs(plugin): describe the PATH bootstrap" -m "Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: Verify

- [x] **Step 1: Full local checks**

Run: `devrun task verify`
Expected: fmt-check, lint and tests pass.

Run: `bash tests/plugin/bootstrap-binaries.test.sh`
Expected: `0 failed`.

Run: `env RUSTC_WRAPPER= dist generate --check`
Expected: exit 0.

- [ ] **Step 2: Live checks after the first release**

These need a published release, so they run after release-please's first release PR merges and dist's workflow finishes:

- Claude Code on Linux: `claude plugin marketplace add AbysmalBiscuit/mcpls`, `claude plugin install mcpls@mcpls`, start a session. Expected: the agent is told mcpls was just installed and to restart. After a restart, `/mcp` lists `mcpls` as connected, and `mcpls hook doctor` from the Bash tool reports the checkout root.
- Codex on Linux: `codex plugin marketplace add AbysmalBiscuit/mcpls`, `codex plugin add mcpls@mcpls`, start a session in this checkout, restart it. Expected: the MCP server starts, and `mcpls hook doctor` run from this checkout reports it as `root:`, not a path under `~/.codex/plugins/cache`.
- Windows, both harnesses: the first session writes `%USERPROFILE%\.local\state\mcpls\bootstrap-version`, and after a restart from a new terminal the MCP server runs `mcpls.exe`.
- A machine with an older mcpls installed by hand: the bootstrap writes `external`, runs no installer, and the agent's first reply relays both versions.

## Unresolved questions

None.
