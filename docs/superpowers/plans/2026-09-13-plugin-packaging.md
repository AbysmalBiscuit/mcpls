# Self-contained mcpls plugin implementation plan

> **Superseded in part.** The launcher in Tasks 2, 3 and 5 and the release matrix in Task 6 are replaced by `2026-09-13-plugin-bootstrap.md`, which installs mcpls onto `PATH` with cargo-dist. Task 4 (`mcpls hook --host codex`) and the release-please setup in Task 6 still stand. The spec describes the current design.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Installing the mcpls plugin in Claude Code or Codex, with no mcpls on the machine, yields a working MCP server and hooks after one restart.

**Architecture:** A bash launcher at `plugin/bin/mcpls` resolves a version-keyed binary under a per-user store, downloading the matching GitHub release on a miss, and every MCP entry and hook runs through it. Claude Code reads `plugin/.mcp.json` and `plugin/hooks/hooks.json`; Codex reads `plugin/.codex-plugin/plugin.json`, whose inline `mcpServers` object and `hooks-codex.json` never touch the Claude files. `mcpls hook --host codex` reads Codex's payload shape. release-please writes one version into `Cargo.toml` and every manifest, and the tag it creates triggers the existing binary build.

**Tech Stack:** Bash, Rust 2024 (clap, serde_json, tokio, assert_cmd), GitHub Actions, release-please.

**Spec:** `docs/superpowers/specs/2026-09-13-plugin-packaging-design.md` (brought onto this branch by Task 1). Where this plan and the spec's first draft disagree, Task 1 corrects the spec and the section "Corrections to the spec" below says why.

## Global constraints

- The launcher writes nothing to stdout before its final `exec`. Every message goes to stderr.
- The launcher exits non-zero, with the reason on stderr, whenever it cannot exec a binary.
- No file named `plugin/plugin.json` may exist.
- Release repository: `https://github.com/AbysmalBiscuit/mcpls`. Tag: `v<version>`. Assets: `mcpls-<target>.tar.gz` (Linux, macOS) and `mcpls-<target>.zip` (Windows), each holding `mcpls` or `mcpls.exe` at the archive root (`.github/workflows/release.yml`, `build-binaries`).
- Targets built: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`.
- Binary store: `${MCPLS_HOME:-$HOME/.local/share/mcpls}/bin/mcpls-<version>`, with `.exe` appended on Windows. `XDG_DATA_HOME` is deliberately not read.
- One version string, equal to `Cargo.toml`'s `workspace.package.version`, appears in `plugin/.claude-plugin/plugin.json` (`$.version`), `plugin/.codex-plugin/plugin.json` (`$.version`) and `.claude-plugin/marketplace.json` (`$.plugins[0].version`).
- Codex behaviour is pinned to `rust-v0.154.0`. Re-resolve with `docm info codex` and read the cited paths under `codex-rs/`.
- Rust: MSRV 1.88, edition 2024, clippy `all`, `pedantic` and `nursery` denied in CI. Integration test files open with `#![allow(clippy::unwrap_used)]`.
- Commands: `devrun task fmt`, `devrun task lint`, `devrun task test`, `devrun task verify`. A single Rust test: `cargo nextest run -p mcpls <name>`.
- Commits: Conventional Commits, subject at most 50 characters, ending with the executing agent's `Co-Authored-By` trailer per `AGENTS.md`. The commands below show Claude Opus 5's; swap in your own model. The `commit-msg` hook enforces the format (`devrun task hooks` installs it).

## Corrections to the spec

Each was read from Codex `rust-v0.154.0` or Claude Code's docs while planning. Task 1 writes them into the spec.

1. **Store path ignores `XDG_DATA_HOME`.** Codex starts a stdio MCP server with only `HOME`, `LOGNAME`, `PATH`, `SHELL`, `USER`, `LANG`, `LC_ALL`, `TERM`, `TMPDIR`, `TZ` and `__CF_USER_TEXT_ENCODING` (`rmcp-client/src/utils.rs`, `DEFAULT_ENV_VARS`), but hooks replay Codex's whole process environment minus a few auth-token variables (`hooks/src/registry.rs`, `Hooks::new`; `protocol/src/shell_environment.rs`, `NON_INHERITABLE_ENV_VARS`). A store keyed on `XDG_DATA_HOME` would split into two stores on any machine that sets it.
2. **The Codex MCP entry is an inline object in `.codex-plugin/plugin.json`, not `.mcp.json`.** An inline `mcpServers` object skips `.mcp.json` discovery entirely (`core-plugins/src/loader.rs`, `load_plugin_mcp_servers_from_manifest_with_format`), so Claude's `${CLAUDE_PLUGIN_ROOT}` entry never reaches Codex. `$CODEX_HOME` is not in the default environment either, so the entry lists it in `env_vars`. Host plugins resolve a relative `cwd` against the plugin root but never a relative `command` (`codex-mcp/src/plugin_config.rs`), so the `sh -c` stays. Only hooks have `commandWindows`; MCP server configs do not (`config/src/mcp_types.rs`, `RawMcpServerConfig`). Codex gives an MCP server 30 seconds to start (`codex-mcp/src/rmcp_client.rs`, `DEFAULT_STARTUP_TIMEOUT`) and kills a first start still downloading, so the entry sets `startup_timeout_sec` to the launcher's 300-second download limit.
3. **`mcpls hook --host codex` prints nothing on `SessionStart`.** Every Codex hook output struct is `deny_unknown_fields` (`hooks/src/schema.rs`), and JSON stdout that fails to parse marks the hook failed (`hooks/src/events/session_start.rs`). Claude's `watchPaths` would fail every Codex session start, and Codex has no `FileChanged` to consume watch paths anyway.
4. **A Codex subagent reports under `session_id/agent_id`, and `SubagentStop` ends that session.** This keeps a subagent's deliveries apart from its parent's without changing the socket protocol.
5. **Only Claude Code's marketplace entry carries a version.** Claude Code pins updates to the marketplace entry's `version`. Codex's marketplace format has no version field.
6. **The marketplace `source` question is answered.** Claude Code resolves `./plugin` against the marketplace root, the directory holding `.claude-plugin/` (code.claude.com/docs/en/plugin-marketplaces).
7. **The Windows twin (`bin/mcpls.cmd`) is deferred.** Neither harness has a per-platform MCP command field that could select it. Hooks and the launcher run under Git Bash on Windows, and CI runs the launcher test there. See unresolved questions.
8. **Checksum verification and a download lock are left out.** `curl -f` with `--max-time` already fails a truncated transfer, and the temporary-file rename makes a concurrent cold start safe. Both stay open questions.

## File map

```
docs/superpowers/specs/2026-09-13-plugin-packaging-design.md   spec, copied and corrected (Task 1)
.gitattributes                                                  LF for scripts on Windows checkouts (Task 2)
plugin/bin/mcpls                                                launcher (Task 2)
tests/plugin/launcher.test.sh                                   launcher and Codex entry tests (Tasks 2, 5)
.github/workflows/ci.yml                                        launcher job, filters, shellcheck (Tasks 2, 3)
plugin/.claude-plugin/plugin.json                               Claude manifest (Task 3)
plugin/.mcp.json                                                Claude MCP entry (Task 3)
plugin/hooks/hooks.json                                         Claude hooks via launcher (Task 3)
.claude-plugin/marketplace.json                                 Claude marketplace (Task 3)
crates/mcpls-cli/tests/plugin_manifests.rs                      manifest invariants (Tasks 3, 5)
crates/mcpls-cli/src/hook.rs                                    Host, project_dir, dispatch by host (Task 4)
crates/mcpls-cli/src/hook/codex.rs                              Codex payload dispatch (Task 4)
crates/mcpls-cli/src/args.rs                                    --host flag (Task 4)
crates/mcpls-cli/src/main.rs                                    wire --host (Task 4)
crates/mcpls-cli/tests/cli_integration.rs                       Codex CLI test (Task 4)
plugin/.codex-plugin/plugin.json                                Codex manifest with inline MCP entry (Task 5)
plugin/hooks/hooks-codex.json                                   Codex hooks (Task 5)
.agents/plugins/marketplace.json                                Codex marketplace (Task 5)
release-please-config.json, .release-please-manifest.json       versioning (Task 6)
.github/workflows/release-please.yml                            release PR and tag (Task 6)
.github/workflows/release.yml                                   drop create-release and publish-crates (Task 6)
plugin/README.md                                                install and troubleshooting (Task 7)
```

---

### Task 1: Bring the spec onto the branch and correct it

**Files:**
- Create: `docs/superpowers/specs/2026-09-13-plugin-packaging-design.md`

**Interfaces:**
- Consumes: the local branch `plugin-packaging-spec` (commit `2f43338` or later).
- Produces: the spec every later task cites.

- [ ] **Step 1: Copy the spec**

```bash
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for show plugin-packaging-spec:docs/superpowers/specs/2026-09-13-plugin-packaging-design.md > /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/docs/superpowers/specs/2026-09-13-plugin-packaging-design.md
```

- [ ] **Step 2: Apply the corrections**

Edit the copied spec so each numbered item in this plan's "Corrections to the spec" is true of it:

- "The launcher", step 2: the store default becomes `$HOME/.local/share/mcpls`, with a sentence naming Codex's MCP environment whitelist as the reason.
- "Registering the server": replace the paragraph beginning "What the legacy format costs" with the inline-object design from correction 2, including `env_vars: ["CODEX_HOME", "MCPLS_BIN", "MCPLS_HOME"]`, `startup_timeout_sec: 300`, and the fact that MCP configs have no `commandWindows`. Where the spec says hooks inherit the full environment, cite `hooks/src/registry.rs` and `protocol/src/shell_environment.rs`.
- "Hooks": add correction 3 (Codex rejects unknown output fields, so `--host codex` prints nothing on `SessionStart`) and correction 4 (subagent session key and `SubagentStop`).
- "Version, release": the Codex marketplace file carries no version (correction 5).
- "Layout": delete the sentence saying the `./plugin` source must be confirmed; state correction 6 instead. Remove `plugin/bin/mcpls.cmd` from the layout block (correction 7).
- "Open questions": delete the marketplace question. Keep checksum and lock. Add the Windows MCP entry question.
- "Sources": add `rmcp-client/src/utils.rs`, `codex-mcp/src/plugin_config.rs`, `codex-mcp/src/rmcp_client.rs`, `config/src/mcp_types.rs`, `hooks/src/events/session_start.rs`, `hooks/src/registry.rs`, `protocol/src/shell_environment.rs` and `core-plugins/src/store.rs`, linked at `rust-v0.154.0` in the same style as the existing entries.

- [ ] **Step 3: Commit**

```bash
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for add docs/superpowers/specs/2026-09-13-plugin-packaging-design.md docs/superpowers/plans/2026-09-13-plugin-packaging.md
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for commit -m "docs: add the plugin packaging spec and plan" -m "Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: The launcher

**Files:**
- Create: `plugin/bin/mcpls`
- Create: `tests/plugin/launcher.test.sh`
- Create: `.gitattributes`
- Modify: `.github/workflows/ci.yml` (`detect-changes`, `shellcheck`, new `plugin-launcher` job, `ci-gate`)

**Interfaces:**
- Consumes: `plugin/.claude-plugin/plugin.json` `$.version` (exists today at `0.1.0`; Task 3 sets it to the workspace version).
- Produces: `plugin/bin/mcpls [ARGS...]`, which execs the resolved binary with `ARGS`. Env inputs: `MCPLS_BIN`, `MCPLS_HOME`, `HOME`.

- [ ] **Step 1: Write the failing test**

Create `tests/plugin/launcher.test.sh`:

```bash
#!/usr/bin/env bash
# Behavioural tests for plugin/bin/mcpls.
#
# Every case runs the launcher against stubbed curl, uname, cygpath and
# powershell.exe on a stubbed PATH, so nothing is downloaded and nothing
# outside the test's temporary directory is touched.
#
# Run: bash tests/plugin/launcher.test.sh

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LAUNCHER="${REPO_ROOT}/plugin/bin/mcpls"
MANIFEST="${REPO_ROOT}/plugin/.claude-plugin/plugin.json"

VERSION=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$MANIFEST" | head -n 1)
if [ -z "$VERSION" ]; then
    echo "could not read a version from ${MANIFEST}" >&2
    exit 1
fi
RELEASE_URL="https://github.com/AbysmalBiscuit/mcpls/releases/download/v${VERSION}"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

STUB="${WORK}/stub"
RELEASE="${WORK}/release"
mkdir -p "$STUB" "$RELEASE"
CALLS="${WORK}/calls"

# The fake binary names itself and its arguments on stdout, so a case can
# tell which binary ran and that the launcher printed nothing of its own.
cat >"${RELEASE}/mcpls" <<'EOF'
#!/usr/bin/env bash
echo "fake-mcpls $*"
EOF
chmod +x "${RELEASE}/mcpls"
cp "${RELEASE}/mcpls" "${RELEASE}/mcpls.exe"
tar -czf "${RELEASE}/archive.tar.gz" -C "$RELEASE" mcpls

# curl logs the URL it was asked for and writes the fake archive to -o.
cat >"${STUB}/curl" <<'EOF'
#!/usr/bin/env bash
echo "curl ${*: -1}" >>"$CALLS"
[ "${CURL_FAIL:-0}" = "1" ] && exit 22
out=""
while [ $# -gt 0 ]; do
    [ "$1" = "-o" ] && out="$2"
    shift
done
case "$out" in
    *.tar.gz) cp "${FAKE_RELEASE}/archive.tar.gz" "$out" ;;
    *.zip) : >"$out" ;;
esac
EOF

cat >"${STUB}/uname" <<'EOF'
#!/usr/bin/env bash
case "$1" in
    -s) echo "${FAKE_UNAME_S:-Linux}" ;;
    -m) echo "${FAKE_UNAME_M:-x86_64}" ;;
esac
EOF

cat >"${STUB}/cygpath" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "${@: -1}"
EOF

# Expand-Archive stand-in: copy the fake exe to the -DestinationPath.
cat >"${STUB}/powershell.exe" <<'EOF'
#!/usr/bin/env bash
echo "powershell" >>"$CALLS"
dest=$(printf '%s' "$*" | sed -n "s/.*-DestinationPath '\([^']*\)'.*/\1/p")
cp "${FAKE_RELEASE}/mcpls.exe" "${dest}/mcpls.exe"
EOF

chmod +x "${STUB}/curl" "${STUB}/uname" "${STUB}/cygpath" "${STUB}/powershell.exe"

pass=0
fail=0
last_exit=0
case_seq=0
store=""

check() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        printf 'FAIL %s\n  expected: %s\n  actual:   %s\n' "$label" "$expected" "$actual" >&2
    fi
}

new_case() {
    case_seq=$((case_seq + 1))
    store="${WORK}/case-${case_seq}/store"
    mkdir -p "${WORK}/case-${case_seq}/home"
}

# env -i keeps the caller's real mcpls, MCPLS_* and XDG_* out of every case.
run_launcher() {
    rm -f "$CALLS"
    env -i HOME="${WORK}/case-${case_seq}/home" PATH="${STUB}:/usr/bin:/bin" \
        ${SYSTEMROOT:+SYSTEMROOT="$SYSTEMROOT"} \
        CALLS="$CALLS" FAKE_RELEASE="$RELEASE" "$@" \
        bash "$LAUNCHER" serve --flag >"${WORK}/out" 2>"${WORK}/err"
    last_exit=$?
}

out() { cat "${WORK}/out"; }
calls() { cat "$CALLS" 2>/dev/null || echo NONE; }
stderr_has() { grep -q "$1" "${WORK}/err" && echo yes || echo no; }

echo "testing plugin/bin/mcpls against plugin version ${VERSION}"

new_case
run_launcher MCPLS_HOME="$store"
check "cold start exits 0" 0 "$last_exit"
check "cold start execs the binary with the caller's arguments" "fake-mcpls serve --flag" "$(out)"
check "cold start downloads the pinned release" "curl ${RELEASE_URL}/mcpls-x86_64-unknown-linux-gnu.tar.gz" "$(calls)"
check "cold start installs under the version" yes "$([ -x "${store}/bin/mcpls-${VERSION}" ] && echo yes || echo no)"
check "cold start leaves no download directory" no "$(compgen -G "${store}/bin/.download.*" >/dev/null && echo yes || echo no)"

run_launcher MCPLS_HOME="$store"
check "warm start skips the network" NONE "$(calls)"
check "warm start execs the installed binary" "fake-mcpls serve --flag" "$(out)"

new_case
mkdir -p "${store}/bin"
printf '#!/usr/bin/env bash\necho old\n' >"${store}/bin/mcpls-0.0.1"
chmod +x "${store}/bin/mcpls-0.0.1"
run_launcher MCPLS_HOME="$store"
check "a new version downloads" "curl ${RELEASE_URL}/mcpls-x86_64-unknown-linux-gnu.tar.gz" "$(calls)"
check "a new version runs the new binary" "fake-mcpls serve --flag" "$(out)"
check "a new version keeps the old binary" yes "$([ -x "${store}/bin/mcpls-0.0.1" ] && echo yes || echo no)"

new_case
run_launcher MCPLS_BIN="${RELEASE}/mcpls" MCPLS_HOME="$store"
check "MCPLS_BIN runs that binary" "fake-mcpls serve --flag" "$(out)"
check "MCPLS_BIN skips the network" NONE "$(calls)"
check "MCPLS_BIN leaves the store alone" no "$([ -e "$store" ] && echo yes || echo no)"

new_case
run_launcher MCPLS_BIN="${WORK}/missing" MCPLS_HOME="$store"
check "a bad MCPLS_BIN exits non-zero" 1 "$last_exit"
check "a bad MCPLS_BIN says why" yes "$(stderr_has MCPLS_BIN)"
check "a bad MCPLS_BIN prints nothing on stdout" "" "$(out)"

new_case
run_launcher MCPLS_HOME="$store" CURL_FAIL=1
check "a failed download exits non-zero" 1 "$last_exit"
check "a failed download names the URL" yes "$(stderr_has "download failed")"
check "a failed download prints nothing on stdout" "" "$(out)"
check "a failed download installs nothing" "" "$(ls -A "${store}/bin" 2>/dev/null)"

new_case
run_launcher MCPLS_HOME="$store" FAKE_UNAME_S=SunOS
check "an unbuilt OS exits non-zero" 1 "$last_exit"
check "an unbuilt OS prints nothing on stdout" "" "$(out)"

new_case
run_launcher MCPLS_HOME="$store" FAKE_UNAME_S=Darwin FAKE_UNAME_M=arm64
check "Apple silicon downloads the aarch64 darwin build" "curl ${RELEASE_URL}/mcpls-aarch64-apple-darwin.tar.gz" "$(calls)"

new_case
run_launcher MCPLS_HOME="$store" FAKE_UNAME_S=MINGW64_NT-10.0
check "Windows exits 0" 0 "$last_exit"
check "Windows downloads the zip" "curl ${RELEASE_URL}/mcpls-x86_64-pc-windows-msvc.zip
powershell" "$(calls)"
check "Windows installs an exe" yes "$([ -x "${store}/bin/mcpls-${VERSION}.exe" ] && echo yes || echo no)"
check "Windows execs the installed exe" "fake-mcpls serve --flag" "$(out)"

# Codex gives an MCP server no XDG_DATA_HOME while its hooks inherit one, so
# the default store must not depend on it or the two would diverge.
new_case
run_launcher XDG_DATA_HOME="${WORK}/xdg"
check "the default store lives under HOME" yes \
    "$([ -x "${WORK}/case-${case_seq}/home/.local/share/mcpls/bin/mcpls-${VERSION}" ] && echo yes || echo no)"
check "the default store ignores XDG_DATA_HOME" no "$([ -e "${WORK}/xdg" ] && echo yes || echo no)"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `bash /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/tests/plugin/launcher.test.sh`
Expected: exit 1, with `FAIL` lines for every check that needs the launcher to run (the `exits 0`, `execs`, `downloads` and `installs` checks). The checks that only assert silence on stdout pass vacuously, since bash prints its `No such file or directory` to stderr.

- [ ] **Step 3: Write the launcher**

Create `plugin/bin/mcpls`:

```bash
#!/usr/bin/env bash
# Resolves the mcpls binary this plugin version pins and execs it with the
# caller's arguments. The MCP server entry and every hook run through here.
#
# Stdout belongs to the process exec'd at the end, the MCP JSON-RPC stream or
# hook JSON, so everything this script says goes to stderr.

set -euo pipefail

REPO_URL="https://github.com/AbysmalBiscuit/mcpls"

die() {
    printf 'mcpls plugin: %s\n' "$1" >&2
    exit 1
}

if [ -n "${MCPLS_BIN:-}" ]; then
    if [ ! -f "$MCPLS_BIN" ] || [ ! -x "$MCPLS_BIN" ]; then
        die "MCPLS_BIN=${MCPLS_BIN} is not an executable file"
    fi
    exec "$MCPLS_BIN" "$@"
fi

plugin_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="${plugin_root}/.claude-plugin/plugin.json"
version=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$manifest" 2>/dev/null | head -n 1 || true)
[ -n "$version" ] || die "could not read a version from ${manifest}"

system=$(uname -s)
machine=$(uname -m)
case "$system" in
    Linux) os=unknown-linux-gnu archive=tar.gz exe=mcpls suffix="" ;;
    Darwin) os=apple-darwin archive=tar.gz exe=mcpls suffix="" ;;
    MINGW* | MSYS* | CYGWIN*) os=pc-windows-msvc archive=zip exe=mcpls.exe suffix=.exe ;;
    *) die "no mcpls release is built for ${system}" ;;
esac
case "$machine" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "no mcpls release is built for ${machine}" ;;
esac
target="${arch}-${os}"

# Codex starts an MCP server without XDG_DATA_HOME but runs hooks with it, so
# the store is keyed on HOME alone to keep both on one copy.
store="${MCPLS_HOME:-${HOME}/.local/share/mcpls}/bin"
binary="${store}/mcpls-${version}${suffix}"

if [ ! -x "$binary" ]; then
    asset="mcpls-${target}.${archive}"
    url="${REPO_URL}/releases/download/v${version}/${asset}"
    mkdir -p "$store" || die "could not create ${store}"
    # A download directory beside the target makes the final mv a rename on
    # one filesystem, so a concurrent start never execs a partial file.
    work=$(mktemp -d "${store}/.download.XXXXXX") || die "could not create a download directory in ${store}"
    trap 'rm -rf "$work"' EXIT

    printf 'mcpls plugin: downloading mcpls %s for %s\n' "$version" "$target" >&2
    curl --proto '=https' --tlsv1.2 -fsSL --connect-timeout 10 --max-time 300 \
        -o "${work}/${asset}" "$url" >&2 || die "download failed: ${url}"

    case "$archive" in
        tar.gz)
            tar -xzf "${work}/${asset}" -C "$work" >&2 || die "could not unpack ${asset}"
            ;;
        zip)
            powershell.exe -NoProfile -Command \
                "Expand-Archive -LiteralPath '$(cygpath -w "${work}/${asset}")' -DestinationPath '$(cygpath -w "$work")'" \
                >&2 || die "could not unpack ${asset}"
            ;;
    esac
    [ -f "${work}/${exe}" ] || die "${asset} does not contain ${exe}"
    chmod +x "${work}/${exe}"
    mv -f "${work}/${exe}" "$binary" || die "could not install ${binary}"

    # exec replaces this process, so the EXIT trap would never clean up.
    rm -rf "$work"
    trap - EXIT
fi

exec "$binary" "$@"
```

- [ ] **Step 4: Mark it executable and keep LF endings**

```bash
chmod +x /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/plugin/bin/mcpls /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/tests/plugin/launcher.test.sh
```

Create `.gitattributes`:

```
# Bash reads a CRLF checkout as commands ending in \r, which Windows runners
# with core.autocrlf would otherwise produce.
plugin/bin/* text eol=lf
tests/plugin/*.sh text eol=lf
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `bash /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/tests/plugin/launcher.test.sh`
Expected: `N passed, 0 failed` and exit 0.

Run: `shellcheck /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/plugin/bin/mcpls /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/tests/plugin/launcher.test.sh`
Expected: no output.

- [ ] **Step 6: Run the launcher in CI on all three OSes**

In `.github/workflows/ci.yml`:

1. In `detect-changes`, add an output and a filter:

```yaml
    outputs:
      run-full-ci: ${{ steps.classify.outputs.run-full-ci }}
      scripts: ${{ steps.filter.outputs.scripts }}
      plugin: ${{ steps.filter.outputs.plugin }}
```

```yaml
            plugin:
              - 'plugin/**'
              - 'tests/plugin/**'
              - '.github/workflows/ci.yml'
```

2. Add the job after `shellcheck`:

```yaml
  plugin-launcher:
    name: Plugin launcher (${{ matrix.os }})
    needs: detect-changes
    if: needs.detect-changes.outputs.plugin == 'true'
    runs-on: ${{ matrix.os }}
    timeout-minutes: 5
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v6
      - name: Launcher tests
        shell: bash
        run: bash tests/plugin/launcher.test.sh
```

3. In `shellcheck`, change its `if` to run on either output and add a step:

```yaml
    if: needs.detect-changes.outputs.scripts == 'true' || needs.detect-changes.outputs.plugin == 'true'
```

```yaml
      - name: Shellcheck plugin launcher
        run: shellcheck plugin/bin/mcpls tests/plugin/launcher.test.sh
```

4. In `ci-gate`, add `plugin-launcher` to `needs` and `"${{ needs.plugin-launcher.result }}"` to `results`.

- [ ] **Step 7: Commit**

```bash
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for add plugin/bin/mcpls tests/plugin/launcher.test.sh .gitattributes .github/workflows/ci.yml
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for commit -m "feat(plugin): add a launcher that fetches mcpls" -m "The plugin ran a bare mcpls and did nothing on a machine without one.
The launcher resolves a version-keyed binary under a per-user store and
downloads the matching release on a miss." -m "Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: Claude Code wiring and marketplace

**Files:**
- Modify: `plugin/.claude-plugin/plugin.json`
- Modify: `plugin/.mcp.json`
- Modify: `plugin/hooks/hooks.json`
- Create: `.claude-plugin/marketplace.json`
- Create: `crates/mcpls-cli/tests/plugin_manifests.rs`
- Modify: `.github/workflows/ci.yml` (`code` filter)

**Interfaces:**
- Consumes: `plugin/bin/mcpls` from Task 2.
- Produces: the Claude entry strings `${CLAUDE_PLUGIN_ROOT}/bin/mcpls` and `"${CLAUDE_PLUGIN_ROOT}/bin/mcpls" hook`; `repo_root()` and `json(relative)` helpers in `plugin_manifests.rs`, which Task 5 extends.

- [ ] **Step 1: Write the failing test**

Create `crates/mcpls-cli/tests/plugin_manifests.rs`:

```rust
//! The plugin's manifests are JSON that other programs read, so these check
//! the invariants that break without an error: the version every manifest
//! pins, the launcher every entry runs, and the manifest file Codex must
//! never find.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn json(relative: &str) -> Value {
    let path = repo_root().join(relative);
    let text = fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// A schema-bearing `plugin.json` at the plugin root switches Codex to the
/// Agent Plugins format, which starts the MCP server inside the plugin
/// directory and loads no hooks at all.
#[test]
fn test_the_plugin_root_has_no_plugin_json() {
    assert!(!repo_root().join("plugin/plugin.json").exists());
}

/// The launcher downloads the release named by the Claude Code manifest's
/// version, so every file repeating that version must agree with the
/// workspace the release is built from.
#[test]
fn test_every_manifest_pins_the_workspace_version() {
    let cargo: toml::Table = fs::read_to_string(repo_root().join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let workspace = cargo["workspace"]["package"]["version"].as_str().unwrap();

    let pinned = [
        (
            "plugin/.claude-plugin/plugin.json",
            json("plugin/.claude-plugin/plugin.json")["version"].clone(),
        ),
        (
            ".claude-plugin/marketplace.json",
            json(".claude-plugin/marketplace.json")["plugins"][0]["version"].clone(),
        ),
    ];
    for (file, version) in pinned {
        assert_eq!(version.as_str(), Some(workspace), "{file}");
    }
}

/// Nothing puts `mcpls` itself on `PATH`, so every entry a harness runs goes
/// through the launcher, and each harness's hook file names only events
/// that harness has.
#[test]
fn test_every_entry_runs_the_launcher_for_its_harness() {
    assert_eq!(
        json("plugin/.mcp.json")["mcpServers"]["mcpls"]["command"],
        "${CLAUDE_PLUGIN_ROOT}/bin/mcpls"
    );

    let harnesses: [(&str, &str, &[&str]); 1] = [(
        "plugin/hooks/hooks.json",
        "\"${CLAUDE_PLUGIN_ROOT}/bin/mcpls\" hook",
        &["FileChanged", "PostToolBatch", "SessionEnd", "SessionStart", "UserPromptSubmit"],
    )];
    for (file, command, events) in harnesses {
        let hooks = json(file);
        let hooks = hooks["hooks"].as_object().unwrap();
        let mut names: Vec<&str> = hooks.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, events, "{file}");
        for group in hooks.values().flat_map(|groups| groups.as_array().unwrap()) {
            for hook in group["hooks"].as_array().unwrap() {
                assert_eq!(hook["command"], command, "{file}");
            }
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p mcpls --test plugin_manifests`
Expected: `test_every_manifest_pins_the_workspace_version` FAILS (`.claude-plugin/marketplace.json: No such file or directory`), `test_every_entry_runs_the_launcher_for_its_harness` FAILS (command is `"mcpls"`), `test_the_plugin_root_has_no_plugin_json` passes.

- [ ] **Step 3: Write the Claude files**

`plugin/.claude-plugin/plugin.json`:

```json
{
  "name": "mcpls",
  "description": "Language server intelligence and push diagnostics through mcpls",
  "version": "0.3.9",
  "author": { "name": "Lev Velykoivanenko" },
  "homepage": "https://github.com/AbysmalBiscuit/mcpls",
  "repository": "https://github.com/AbysmalBiscuit/mcpls",
  "license": "MIT OR Apache-2.0"
}
```

`plugin/.mcp.json`:

```json
{
  "mcpServers": {
    "mcpls": {
      "command": "${CLAUDE_PLUGIN_ROOT}/bin/mcpls",
      "args": []
    }
  }
}
```

`plugin/hooks/hooks.json`:

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "\"${CLAUDE_PLUGIN_ROOT}/bin/mcpls\" hook" }] }],
    "FileChanged": [{ "hooks": [{ "type": "command", "command": "\"${CLAUDE_PLUGIN_ROOT}/bin/mcpls\" hook" }] }],
    "PostToolBatch": [{ "hooks": [{ "type": "command", "command": "\"${CLAUDE_PLUGIN_ROOT}/bin/mcpls\" hook" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "\"${CLAUDE_PLUGIN_ROOT}/bin/mcpls\" hook" }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "command": "\"${CLAUDE_PLUGIN_ROOT}/bin/mcpls\" hook" }] }]
  }
}
```

`.claude-plugin/marketplace.json`:

```json
{
  "name": "mcpls",
  "owner": { "name": "Lev Velykoivanenko" },
  "plugins": [
    {
      "name": "mcpls",
      "source": "./plugin",
      "description": "Language server intelligence and push diagnostics through mcpls",
      "version": "0.3.9"
    }
  ]
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls --test plugin_manifests`
Expected: 3 passed.

Run: `bash /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/tests/plugin/launcher.test.sh`
Expected: `0 failed` (the launcher test reads the version that just changed).

- [ ] **Step 5: Run the Rust tests when only plugin files change**

In `.github/workflows/ci.yml`, `detect-changes`, extend the `code` filter:

```yaml
            code:
              - 'crates/**'
              - 'Cargo.toml'
              - 'Cargo.lock'
              - 'plugin/**'
              - '.claude-plugin/**'
              - '.agents/**'
```

- [ ] **Step 6: Commit**

```bash
devrun task fmt
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for add plugin/.claude-plugin/plugin.json plugin/.mcp.json plugin/hooks/hooks.json .claude-plugin/marketplace.json crates/mcpls-cli/tests/plugin_manifests.rs .github/workflows/ci.yml
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for commit -m "feat(plugin): route claude entries via launcher" -m "Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: `mcpls hook --host codex`

**Files:**
- Modify: `crates/mcpls-cli/src/hook.rs:1-70` (module docs, `mod codex`, `Host`, `project_dir`, `dispatch_payload`) and its `tests` module (helpers at `:815-835`, callers at `:1437` and `:1459`, new tests)
- Create: `crates/mcpls-cli/src/hook/codex.rs`
- Modify: `crates/mcpls-cli/src/args.rs:137-150` (`Command::Hook`)
- Modify: `crates/mcpls-cli/src/main.rs:34-60`
- Modify: `crates/mcpls-cli/tests/cli_integration.rs` (new test after `test_hook_context_outputs_name_the_triggering_event_through_cli`)

**Interfaces:**
- Consumes: `mcpls_core::hooks::{ChangeEvent, Request, SocketIdentity, send, send_and_acknowledge}`; `hook.rs` private items `SOCKET_TIMEOUT`, `FLUSH_SOCKET_TIMEOUT`, `flush_context(Response) -> Option<String>`, `additional_context_output(&str, Option<String>) -> String`.
- Produces:
  - `pub enum hook::Host { Claude, Codex }` deriving `clap::ValueEnum`.
  - `pub fn hook::project_dir(host: Host, stdin: &str) -> PathBuf`.
  - `pub async fn hook::dispatch_payload(host: Host, stdin: &str, project_dir: &Path, identity: Option<&SocketIdentity>) -> String`.
  - CLI: `mcpls hook [--host claude|codex]`, default `claude`.

- [ ] **Step 1: Write the failing CLI test**

Append to `crates/mcpls-cli/tests/cli_integration.rs`, directly after `test_hook_context_outputs_name_the_triggering_event_through_cli`:

```rust
/// A Codex hook names its project in the payload's `cwd` rather than in
/// `CLAUDE_PROJECT_DIR`, and Codex spawns it from its own working directory,
/// so the flush has to reach the owner of the payload's project.
#[tokio::test]
async fn test_codex_hook_reaches_the_owner_named_by_the_payload_cwd() {
    use mcpls_core::hooks::{HookListener, Request, Response};
    let project = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    #[cfg(windows)]
    let identity = mcpls_core::hooks::identity_for(project.path()).unwrap();
    // The child derives its socket from the temporary directory set below,
    // which `identity_for` in this process would not read.
    #[cfg(not(windows))]
    let identity = {
        let hash = mcpls_core::hooks::identity_hash(project.path()).unwrap();
        mcpls_core::hooks::SocketIdentity {
            socket: runtime
                .path()
                .join("mcpls-mcpls-test")
                .join(format!("{hash}.sock")),
            lock: runtime
                .path()
                .join("mcpls-mcpls-test")
                .join(format!("{hash}.lock")),
            hash,
        }
    };
    let listener = HookListener::acquire(&identity).await.unwrap().unwrap();
    let (cancel, rx) = tokio::sync::watch::channel(false);
    let owner = tokio::spawn(listener.serve(
        |request| {
            Box::pin(async move {
                match request {
                    Request::Changed { .. } => Response::Changed { queued: 0 },
                    Request::Flush { .. } => Response::Flush {
                        context: Some("diagnostic".into()),
                        token: None,
                    },
                    _ => unreachable!(),
                }
            })
        },
        Duration::from_secs(1),
        rx,
    ));
    tokio::task::spawn_blocking(move || {
        for event in ["UserPromptSubmit", "PostToolUse"] {
            let mut cmd = Command::cargo_bin("mcpls").unwrap();
            clear_ambient_env(&mut cmd);
            let output = assert_cmd::Command::from_std(cmd)
                .env_remove("CLAUDE_PROJECT_DIR")
                .current_dir(elsewhere.path())
                .env_remove("XDG_RUNTIME_DIR")
                .env("TMPDIR", runtime.path())
                .env("USER", "mcpls-test")
                .args(["hook", "--host", "codex"])
                .write_stdin(
                    serde_json::json!({
                        "hook_event_name": event,
                        "session_id": "test",
                        "cwd": project.path(),
                        "tool_name": "apply_patch",
                        "tool_input": {
                            "command": "*** Begin Patch\n*** Update File: a.rs\n*** End Patch\n"
                        }
                    })
                    .to_string(),
                )
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
            let parsed: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(
                parsed,
                serde_json::json!({"hookSpecificOutput": {
                    "hookEventName": event, "additionalContext": "diagnostic"
                }})
            );
        }
    })
    .await
    .unwrap();
    cancel.send(true).unwrap();
    owner.await.unwrap();
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p mcpls test_codex_hook_reaches_the_owner_named_by_the_payload_cwd`
Expected: FAIL. The child exits with status 2 and stderr `error: unexpected argument '--host' found`.

- [ ] **Step 3: Write the failing dispatch tests**

In `crates/mcpls-cli/src/hook.rs`'s `tests` module, update the existing helpers and direct callers to pass the host, then add the Codex tests.

Replace `dispatch_raw` and `dispatch_against` (`:819-835`):

```rust
    /// The same, from raw stdin bytes, so a payload that is not JSON at all
    /// goes through the same path.
    async fn dispatch_raw(stdin: &str, project_dir: &Path) -> String {
        let identity = mcpls_core::hooks::identity_for(project_dir).expect("identity");
        super::dispatch_payload(Host::Claude, stdin, project_dir, Some(&identity)).await
    }

    /// Run the dispatcher against a listener that records the requests it
    /// gets.
    async fn dispatch_against(payload: &serde_json::Value, recorder: &RecordingOwner) -> String {
        dispatch_as(Host::Claude, payload, recorder).await
    }

    /// The same, for a hook spawned by `host`.
    async fn dispatch_as(
        host: Host,
        payload: &serde_json::Value,
        recorder: &RecordingOwner,
    ) -> String {
        super::dispatch_payload(
            host,
            &payload.to_string(),
            recorder.project_dir(),
            Some(&recorder.identity),
        )
        .await
    }
```

In `test_a_missing_identity_still_lets_session_start_answer` and `test_a_missing_identity_produces_no_output_for_socket_using_arms`, change each `super::dispatch_payload(` call to pass `Host::Claude,` as its first argument.

Add at the end of the `tests` module:

```rust
    #[tokio::test]
    async fn test_codex_post_tool_use_reports_patched_files_under_the_subagent_session() {
        let recorder = RecordingOwner::start_with_flush(Some(DEFAULT_FLUSH_TEXT.to_string()));
        let cwd = recorder.project_dir().join("crates");
        let out = dispatch_as(
            Host::Codex,
            &json!({
                "hook_event_name": "PostToolUse",
                "session_id": "s1",
                "agent_id": "a1",
                "cwd": cwd.display().to_string(),
                "tool_name": "apply_patch",
                "tool_input": {
                    "command": "*** Begin Patch\n*** Update File: src/a.rs\n*** End Patch\n"
                }
            }),
            &recorder,
        )
        .await;

        assert_eq!(
            out,
            additional_context_output("PostToolUse", Some(DEFAULT_FLUSH_TEXT.to_string()))
        );
        let requests = recorder.requests();
        let Request::Changed {
            session,
            paths,
            event,
        } = &requests[0]
        else {
            panic!("expected a changed request: {:?}", requests[0]);
        };
        assert_eq!(session.as_str(), "s1/a1");
        assert_eq!(
            *paths,
            vec![cwd.join("src/a.rs")],
            "apply_patch paths are relative to the session's cwd"
        );
        assert_eq!(*event, ChangeEvent::Change);
        let Request::Flush { session } = &requests[1] else {
            panic!("expected a flush request: {:?}", requests[1]);
        };
        assert_eq!(session.as_str(), "s1/a1");
    }

    #[tokio::test]
    async fn test_codex_session_start_prints_nothing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");

        let out = super::dispatch_payload(
            Host::Codex,
            &json!({ "hook_event_name": "SessionStart", "session_id": "s1" }).to_string(),
            dir.path(),
            None,
        )
        .await;

        assert_eq!(
            out, "",
            "Codex fails a SessionStart hook whose JSON carries a field it does \
             not know, and watchPaths is one"
        );
    }

    #[tokio::test]
    async fn test_codex_subagent_stop_ends_the_subagent_session() {
        let recorder = RecordingOwner::start();
        let out = dispatch_as(
            Host::Codex,
            &json!({ "hook_event_name": "SubagentStop", "session_id": "s1", "agent_id": "a1" }),
            &recorder,
        )
        .await;

        assert_eq!(out, "");
        let requests = recorder.requests();
        let Request::EndSession { session } = &requests[0] else {
            panic!("expected an end-session request: {:?}", requests[0]);
        };
        assert_eq!(session.as_str(), "s1/a1");
    }

    #[test]
    fn test_codex_project_dir_is_the_payload_cwd() {
        assert_eq!(
            project_dir(Host::Codex, r#"{"hook_event_name":"Stop","cwd":"/work/project"}"#),
            PathBuf::from("/work/project")
        );
        assert_eq!(
            project_dir(Host::Codex, "not json"),
            PathBuf::from("."),
            "an unreadable payload falls back the way a missing CLAUDE_PROJECT_DIR does"
        );
    }
```

- [ ] **Step 4: Run them to verify they fail**

Run: `cargo nextest run -p mcpls hook::tests`
Expected: compile error `cannot find type Host in this scope`.

- [ ] **Step 5: Implement `Host`, `project_dir` and host dispatch in `hook.rs`**

Replace the module doc comment and the `use` block at the top of `crates/mcpls-cli/src/hook.rs` so it reads:

```rust
//! Dispatching one agent hook invocation.
//!
//! A hook registration spawns `mcpls hook`, writes one JSON payload to its
//! stdin, and reads one JSON payload back from its stdout. Routing on the
//! payload's own `hook_event_name` here, in one binary, means there is no
//! shell script translating hook names into subcommands, and the same
//! registrations work unmodified on Windows. Claude Code and Codex send
//! different payload shapes, so `--host` picks which dispatcher reads it.

mod codex;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use mcpls_core::hooks::filters::WatchPaths;
use mcpls_core::hooks::{
    ChangeEvent, ProbeOutcome, Request, Response, SocketIdentity, probe, send,
    send_and_acknowledge, watch_paths,
};
use serde::Deserialize;
```

Add after `FLUSH_SOCKET_TIMEOUT`:

```rust
/// The agent harness that spawned a hook, which decides where its project
/// directory and session identity come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Host {
    /// Claude Code, which names the project in `CLAUDE_PROJECT_DIR`.
    Claude,
    /// Codex, which names the project in every payload's `cwd`.
    Codex,
}

/// The directory a hook invocation names as its project, before
/// canonicalization, or `.` when it names none.
pub fn project_dir(host: Host, stdin: &str) -> PathBuf {
    let named = match host {
        Host::Claude => std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from),
        Host::Codex => serde_json::from_str::<serde_json::Value>(stdin)
            .ok()
            .and_then(|payload| payload.get("cwd")?.as_str().map(PathBuf::from)),
    };
    named.unwrap_or_else(|| PathBuf::from("."))
}
```

Replace `dispatch_payload`:

```rust
/// Return hook JSON, or an empty answer on payload or socket failure.
/// Claude Code's `SessionStart` works without an identity and reports local
/// watch-scan failures.
pub async fn dispatch_payload(
    host: Host,
    stdin: &str,
    project_dir: &Path,
    identity: Option<&SocketIdentity>,
) -> String {
    match host {
        Host::Claude => silently(run(stdin, project_dir, identity)).await,
        Host::Codex => silently(codex::run(stdin, project_dir, identity)).await,
    }
}
```

- [ ] **Step 6: Implement the Codex dispatcher**

Create `crates/mcpls-cli/src/hook/codex.rs`:

```rust
//! Dispatching one Codex hook invocation.
//!
//! Codex's payloads differ from Claude Code's in the fields read here: a
//! subagent carries its own `agent_id`, and the `apply_patch` edit tool names
//! its files inside a patch envelope rather than in a `file_path`. Codex
//! fails a hook whose JSON output carries a field it does not know, so
//! nothing here emits Claude Code's `watchPaths`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use mcpls_core::hooks::{ChangeEvent, Request, SocketIdentity, send, send_and_acknowledge};
use serde::Deserialize;
use serde_json::Value;

use super::{FLUSH_SOCKET_TIMEOUT, SOCKET_TIMEOUT, additional_context_output, flush_context};

/// Envelope headers that name a file. `Move to` is a rename's destination,
/// so a rename reports both ends.
const PATCH_VERBS: [&str; 4] = ["Add File", "Update File", "Delete File", "Move to"];

/// The Codex hook payload, keeping only the fields the dispatch below reads.
/// Every field but the event name is defaulted, since which ones are present
/// depends on the event.
#[derive(Debug, Deserialize)]
struct CodexPayload {
    hook_event_name: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_input: Value,
}

impl CodexPayload {
    /// The session a request is filed under. A subagent files under
    /// `session/agent`, so its diagnostics reach it rather than its parent.
    fn session(&self) -> String {
        match self.agent_id.as_deref() {
            Some(agent) if !agent.is_empty() => format!("{}/{agent}", self.session_id),
            _ => self.session_id.clone(),
        }
    }

    /// The files an `apply_patch` call touched, resolved against the
    /// session's `cwd`, which the envelope's relative paths are written
    /// against.
    fn patched_paths(&self, project_dir: &Path) -> Vec<PathBuf> {
        if self.tool_name != "apply_patch" {
            return Vec::new();
        }
        let cwd = self.cwd.as_deref().unwrap_or(project_dir);
        self.tool_input
            .get("command")
            .and_then(Value::as_str)
            .map(apply_patch_paths)
            .unwrap_or_default()
            .into_iter()
            .map(|path| cwd.join(path))
            .collect()
    }
}

/// Every file path an `apply_patch` envelope names, in order, verbatim.
fn apply_patch_paths(envelope: &str) -> Vec<&str> {
    envelope
        .lines()
        .filter_map(|line| {
            let (verb, path) = line.trim().strip_prefix("*** ")?.split_once(": ")?;
            let path = path.trim();
            (PATCH_VERBS.contains(&verb) && !path.is_empty()).then_some(path)
        })
        .collect()
}

pub(super) async fn run(
    stdin: &str,
    project_dir: &Path,
    identity: Option<&SocketIdentity>,
) -> Result<String> {
    let payload: CodexPayload = serde_json::from_str(stdin)?;
    let Some(identity) = identity else {
        return Ok(String::new());
    };
    let session = payload.session();

    match payload.hook_event_name.as_str() {
        "PostToolUse" => {
            let requests = [
                Request::Changed {
                    session: session.clone(),
                    paths: payload.patched_paths(project_dir),
                    event: ChangeEvent::Change,
                },
                Request::Flush { session },
            ];
            let responses = send_and_acknowledge(identity, &requests, FLUSH_SOCKET_TIMEOUT).await?;
            let context = responses.into_iter().nth(1).and_then(flush_context);
            Ok(additional_context_output("PostToolUse", context))
        }

        "UserPromptSubmit" => {
            let responses =
                send_and_acknowledge(identity, &[Request::Flush { session }], FLUSH_SOCKET_TIMEOUT)
                    .await?;
            let context = responses.into_iter().next().and_then(flush_context);
            Ok(additional_context_output("UserPromptSubmit", context))
        }

        "SubagentStop" | "SessionEnd" => {
            send(identity, &Request::EndSession { session }, SOCKET_TIMEOUT).await?;
            Ok(String::new())
        }

        _ => Ok(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_patch_paths_names_every_verb_and_both_ends_of_a_rename() {
        let patch = "*** Begin Patch\n\
                     *** Add File: src/new.rs\n\
                     +*** Update File: not/a/header.rs\n\
                     *** Update File: src/old.rs\n\
                     *** Move to: src/renamed.rs\n\
                     @@\n\
                     -a\n\
                     +b\n\
                     *** Delete File: src/gone.rs\n\
                     *** End Patch\n";
        assert_eq!(
            apply_patch_paths(patch),
            vec!["src/new.rs", "src/old.rs", "src/renamed.rs", "src/gone.rs"]
        );
    }
}
```

- [ ] **Step 7: Add the flag and wire it**

In `crates/mcpls-cli/src/args.rs`, add `use crate::hook::Host;` beside `use crate::completions::Shell;`, and replace the `Hook` variant:

```rust
    /// Serve one agent hook invocation
    ///
    /// With no argument, reads the hook payload from stdin and writes hook
    /// JSON to stdout, dispatching on the payload's own `hook_event_name`.
    /// One subcommand rather than one per event means no shell script and the
    /// same registrations work on Windows.
    Hook {
        /// The harness that spawned this hook, which decides where the
        /// project directory comes from
        #[arg(long, value_enum, default_value_t = Host::Claude)]
        host: Host,

        /// What to do instead of reading a hook payload from stdin
        #[command(subcommand)]
        action: Option<HookAction>,
    },
```

In `crates/mcpls-cli/src/main.rs`, change the match head and the `None` arm's project directory and dispatch call:

```rust
    if let Some(Command::Hook { host, action }) = &args.command {
        match action {
            None => {
                use std::io::{Read as _, Write as _};
                let mut stdin = String::new();
                let _ = std::io::stdin().read_to_string(&mut stdin);
                let raw_project_dir = hook::project_dir(*host, &stdin);
```

```rust
                let out = hook::dispatch_payload(*host, &stdin, &root, identity.as_ref()).await;
```

Leave the canonicalization, identity and stdout-writing lines between them, and the whole `Doctor` arm, unchanged.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls hook`
Expected: every `hook::tests` and `hook::codex::tests` test passes, including the four new ones.

Run: `cargo nextest run -p mcpls test_codex_hook_reaches_the_owner_named_by_the_payload_cwd test_hook_context_outputs_name_the_triggering_event_through_cli`
Expected: 2 passed.

Run: `devrun task lint`
Expected: no warnings. The parameter of `apply_patch_paths` is `envelope`, not `patch`, because pedantic `similar_names` rejects `patch` beside `path`.

- [ ] **Step 9: Commit**

```bash
devrun task fmt
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for add crates/mcpls-cli/src/hook.rs crates/mcpls-cli/src/hook/codex.rs crates/mcpls-cli/src/args.rs crates/mcpls-cli/src/main.rs crates/mcpls-cli/tests/cli_integration.rs
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for commit -m "feat(hooks): read codex payloads with --host codex" -m "Codex names the project in each payload's cwd, not in
CLAUDE_PROJECT_DIR, names apply_patch targets inside a patch envelope,
and fails a hook whose JSON output carries a field it does not know.
A subagent reports under session/agent and SubagentStop ends it." -m "Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: Codex manifest, hooks and marketplace

**Files:**
- Create: `plugin/.codex-plugin/plugin.json`
- Create: `plugin/hooks/hooks-codex.json`
- Create: `.agents/plugins/marketplace.json`
- Modify: `crates/mcpls-cli/tests/plugin_manifests.rs`
- Modify: `tests/plugin/launcher.test.sh`

**Interfaces:**
- Consumes: `plugin/bin/mcpls` (Task 2), `mcpls hook --host codex` (Task 4), `json()` and `repo_root()` (Task 3).
- Produces: the Codex MCP entry `sh -c <locate-and-exec>` with `env_vars ["CODEX_HOME", "MCPLS_BIN", "MCPLS_HOME"]` and `startup_timeout_sec: 300`; hook command `"${PLUGIN_ROOT}/bin/mcpls" hook --host codex`.

- [ ] **Step 1: Write the failing tests**

In `crates/mcpls-cli/tests/plugin_manifests.rs`, replace `pinned` in `test_every_manifest_pins_the_workspace_version`:

```rust
    let pinned = [
        (
            "plugin/.claude-plugin/plugin.json",
            json("plugin/.claude-plugin/plugin.json")["version"].clone(),
        ),
        (
            "plugin/.codex-plugin/plugin.json",
            json("plugin/.codex-plugin/plugin.json")["version"].clone(),
        ),
        (
            ".claude-plugin/marketplace.json",
            json(".claude-plugin/marketplace.json")["plugins"][0]["version"].clone(),
        ),
    ];
```

and replace `harnesses` in `test_every_entry_runs_the_launcher_for_its_harness`:

```rust
    let harnesses: [(&str, &str, &[&str]); 2] = [
        (
            "plugin/hooks/hooks.json",
            "\"${CLAUDE_PLUGIN_ROOT}/bin/mcpls\" hook",
            &["FileChanged", "PostToolBatch", "SessionEnd", "SessionStart", "UserPromptSubmit"],
        ),
        (
            "plugin/hooks/hooks-codex.json",
            "\"${PLUGIN_ROOT}/bin/mcpls\" hook --host codex",
            &["PostToolUse", "SessionEnd", "SubagentStop", "UserPromptSubmit"],
        ),
    ];
```

Add to the same test, after the `.mcp.json` assertion:

```rust
    let codex = json("plugin/.codex-plugin/plugin.json");
    assert_eq!(codex["hooks"], "./hooks/hooks-codex.json");
    assert_eq!(codex["mcpServers"]["mcpls"]["command"], "sh");
    assert_eq!(
        codex["mcpServers"]["mcpls"]["env_vars"],
        serde_json::json!(["CODEX_HOME", "MCPLS_BIN", "MCPLS_HOME"]),
        "Codex starts an MCP server with none of these unless the entry names them"
    );
    assert_eq!(
        codex["mcpServers"]["mcpls"]["startup_timeout_sec"],
        300,
        "Codex kills a server still starting after 30 seconds, and a cold start downloads for up to 300"
    );
```

In `tests/plugin/launcher.test.sh`, insert before the final `printf` summary:

```bash
# The Codex MCP entry cannot use a plugin-root placeholder, so it finds the
# installed copy of the plugin under CODEX_HOME and execs its launcher. Codex
# keeps one version of a plugin in its cache and deletes the old one on
# upgrade (core-plugins/src/store.rs), so the fixture installs one.
entry=$(jq -r '.mcpServers.mcpls.args[1]' "${REPO_ROOT}/plugin/.codex-plugin/plugin.json" 2>/dev/null)
check "the Codex entry is readable from the manifest" yes "$([ -n "$entry" ] && echo yes || echo no)"

codex_home="${WORK}/codex-home"
installed="${codex_home}/plugins/cache/mcpls/mcpls/${VERSION}"
mkdir -p "${installed}/bin"
printf '#!/usr/bin/env bash\necho installed-plugin "$@"\n' >"${installed}/bin/mcpls"
chmod +x "${installed}/bin/mcpls"

codex_out=$(env -i HOME="${WORK}/nohome" PATH="/usr/bin:/bin" CODEX_HOME="$codex_home" sh -c "$entry" 2>/dev/null)
check "the Codex entry runs the installed plugin" "installed-plugin" "$codex_out"

mkdir -p "${WORK}/home-default/.codex"
cp -R "${codex_home}/plugins" "${WORK}/home-default/.codex/plugins"
codex_out=$(env -i HOME="${WORK}/home-default" PATH="/usr/bin:/bin" sh -c "$entry" 2>/dev/null)
check "the Codex entry defaults CODEX_HOME to ~/.codex" "installed-plugin" "$codex_out"

codex_out=$(env -i HOME="${WORK}/nohome" PATH="/usr/bin:/bin" CODEX_HOME="${WORK}/empty" sh -c "$entry" 2>"${WORK}/err")
codex_exit=$?
check "the Codex entry fails with no installed plugin" 1 "$codex_exit"
check "the Codex entry prints nothing on stdout when it fails" "" "$codex_out"
check "the Codex entry says where it looked" yes "$(stderr_has "plugins/cache")"
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p mcpls --test plugin_manifests`
Expected: FAIL with `plugin/.codex-plugin/plugin.json: No such file or directory`.

Run: `bash /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/tests/plugin/launcher.test.sh`
Expected: FAIL on `the Codex entry is readable from the manifest` and every case after it.

- [ ] **Step 3: Write the Codex files**

`plugin/.codex-plugin/plugin.json`:

```json
{
  "name": "mcpls",
  "version": "0.3.9",
  "description": "Language server intelligence and push diagnostics through mcpls",
  "author": { "name": "Lev Velykoivanenko" },
  "homepage": "https://github.com/AbysmalBiscuit/mcpls",
  "repository": "https://github.com/AbysmalBiscuit/mcpls",
  "license": "MIT OR Apache-2.0",
  "keywords": ["mcp", "lsp", "language-server", "diagnostics"],
  "skills": "./skills/",
  "hooks": "./hooks/hooks-codex.json",
  "mcpServers": {
    "mcpls": {
      "command": "sh",
      "args": ["-c", "root=$(ls -td \"${CODEX_HOME:-$HOME/.codex}\"/plugins/cache/*/mcpls/*/ 2>/dev/null | head -n 1); if [ -z \"$root\" ]; then echo \"mcpls plugin: no installed mcpls plugin under ${CODEX_HOME:-$HOME/.codex}/plugins/cache\" >&2; exit 1; fi; exec \"${root}bin/mcpls\""],
      "env_vars": ["CODEX_HOME", "MCPLS_BIN", "MCPLS_HOME"],
      "startup_timeout_sec": 300
    }
  },
  "interface": {
    "displayName": "mcpls",
    "shortDescription": "Language server diagnostics and code intelligence through MCP",
    "category": "Coding",
    "capabilities": ["Read"]
  }
}
```

`plugin/hooks/hooks-codex.json`:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "apply_patch",
        "hooks": [{ "type": "command", "command": "\"${PLUGIN_ROOT}/bin/mcpls\" hook --host codex" }]
      }
    ],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "\"${PLUGIN_ROOT}/bin/mcpls\" hook --host codex" }] }],
    "SubagentStop": [{ "hooks": [{ "type": "command", "command": "\"${PLUGIN_ROOT}/bin/mcpls\" hook --host codex" }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "command": "\"${PLUGIN_ROOT}/bin/mcpls\" hook --host codex" }] }]
  }
}
```

`.agents/plugins/marketplace.json`:

```json
{
  "name": "mcpls",
  "interface": { "displayName": "mcpls" },
  "plugins": [
    {
      "name": "mcpls",
      "description": "Language server intelligence and push diagnostics through mcpls",
      "source": { "source": "local", "path": "./plugin" },
      "policy": { "installation": "AVAILABLE", "authentication": "ON_INSTALL" },
      "category": "Coding"
    }
  ]
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls --test plugin_manifests`
Expected: 3 passed.

Run: `bash /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/tests/plugin/launcher.test.sh`
Expected: `0 failed`.

- [ ] **Step 5: Commit**

```bash
devrun task fmt
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for add plugin/.codex-plugin/plugin.json plugin/hooks/hooks-codex.json .agents/plugins/marketplace.json crates/mcpls-cli/tests/plugin_manifests.rs tests/plugin/launcher.test.sh
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for commit -m "feat(plugin): ship codex manifests and hooks" -m "The MCP entry is an inline object so Codex never reads the Claude
.mcp.json, and it locates the installed plugin from CODEX_HOME because
legacy-format plugins get no plugin-root placeholder in MCP configs." -m "Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: One version, one release pipeline

**Files:**
- Create: `release-please-config.json`
- Create: `.release-please-manifest.json`
- Create: `.github/workflows/release-please.yml`
- Modify: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: the three version fields listed in Global constraints; `Cargo.toml` `workspace.package.version` and `workspace.dependencies.mcpls-core.version`; the `mcpls` and `mcpls-core` entries in `Cargo.lock`.
- Produces: a release PR that bumps every version field, `Cargo.lock` and `CHANGELOG.md`; on merge, tag `v<version>` and a GitHub Release that `release.yml` uploads assets to.

- [ ] **Step 1: Write the release-please config**

The config uses the `simple` release type with explicit `extra-files`, not the `rust` type with the `cargo-workspace` plugin. release-please's Cargo updaters reject this workspace: the root `Cargo.toml` has no `[package]` table, and the crates declare `version.workspace = true`, which `cargo-toml.ts` and `cargo-workspace.ts` both throw on.

The `Cargo.lock` jsonpath compares `@.name.value`, not `@.name`. `GenericToml` parses with a position-tagging parser that wraps every string in `{start, end, value}`, so `@.name=='mcpls'` matches nothing and leaves the lock stale. A stale lock fails every `--locked` build on `main` once the release PR merges.

`release-please-config.json`:

```json
{
  "$schema": "https://raw.githubusercontent.com/googleapis/release-please/main/schemas/config.json",
  "include-component-in-tag": false,
  "bootstrap-sha": "216dbb84567aabc8b4b8d79a59302a2bd8214345",
  "packages": {
    ".": {
      "release-type": "simple",
      "package-name": "mcpls",
      "bump-minor-pre-major": true,
      "bump-patch-for-minor-pre-major": true,
      "extra-files": [
        { "type": "toml", "path": "Cargo.toml", "jsonpath": "$.workspace.package.version" },
        { "type": "toml", "path": "Cargo.toml", "jsonpath": "$.workspace.dependencies['mcpls-core'].version" },
        { "type": "toml", "path": "Cargo.lock", "jsonpath": "$.package[?(@.name.value=='mcpls' || @.name.value=='mcpls-core')].version" },
        { "type": "json", "path": "plugin/.claude-plugin/plugin.json", "jsonpath": "$.version" },
        { "type": "json", "path": "plugin/.codex-plugin/plugin.json", "jsonpath": "$.version" },
        { "type": "json", "path": ".claude-plugin/marketplace.json", "jsonpath": "$.plugins[0].version" }
      ]
    }
  }
}
```

`.release-please-manifest.json`:

```json
{
  ".": "0.3.9"
}
```

- [ ] **Step 2: Add the release-please workflow**

`.github/workflows/release-please.yml`:

```yaml
name: release-please

on:
  push:
    branches: [main]

permissions:
  contents: write
  pull-requests: write

jobs:
  # Maintains the release PR and, on merge, creates the tag and GitHub
  # Release. The token is a PAT rather than GITHUB_TOKEN because a tag
  # created with GITHUB_TOKEN does not trigger release.yml, which builds the
  # binaries the plugin launcher downloads.
  release-please:
    name: release-please
    runs-on: ubuntu-latest
    steps:
      - uses: googleapis/release-please-action@5c625bfb5d1ff62eadeeb3772007f7f66fdcf071 # v4
        with:
          token: ${{ secrets.RELEASE_PLEASE_TOKEN }}
          config-file: release-please-config.json
          manifest-file: .release-please-manifest.json
```

- [ ] **Step 3: Strip `release.yml` to building and uploading**

In `.github/workflows/release.yml`:

1. Delete the `create-release` job (from `# Create GitHub Release` through its last step). release-please creates the release, and `softprops/action-gh-release` in `build-binaries` uploads to the existing release for the pushed tag.
2. In `build-binaries`, delete the line `needs: create-release`.
3. Delete the `publish-crates` job (from `# Publish to crates.io using Trusted Publishing (OIDC)` through its last step). The crate names belong to upstream.
4. In `update-release-notes`, change `needs: [create-release, build-binaries, publish-crates]` to `needs: [build-binaries]`.
5. In `update-release-notes`'s `body`, replace the `### Cargo Installation` and `### Claude Code Integration` sections, including their code blocks, with:

```yaml
            ### Claude Code and Codex plugin

            The plugin downloads this release on first use. See the [plugin README](https://github.com/AbysmalBiscuit/mcpls/tree/v${{ steps.version.outputs.version }}/plugin#install).
```

- [ ] **Step 4: Validate the workflow files**

Run: `yq '.jobs | keys' /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/.github/workflows/release.yml`
Expected: `- build-binaries` and `- update-release-notes`, nothing else.

Run: `jq . /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/release-please-config.json`
Expected: the config, pretty-printed, no parse error.

- [ ] **Step 5: Commit**

```bash
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for add release-please-config.json .release-please-manifest.json .github/workflows/release-please.yml .github/workflows/release.yml
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for commit -m "ci(release): adopt release-please" -m "The launcher downloads the release its manifest names, so one release
commit has to move Cargo.toml and every plugin manifest together. The
crates.io publish job goes: the crate names belong to upstream." -m "Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

- [ ] **Step 6: Dry-run release-please against the pushed branch (ask Lev before pushing)**

release-please reads its config from GitHub, not the working tree, so this needs the branch on `origin`. Ask before pushing.

```bash
bunx --bun release-please release-pr --dry-run --token="$(gh auth token)" --repo-url=AbysmalBiscuit/mcpls --target-branch=package-mcpls-as-a-self-contained-plugin-for
```

Expected: a proposed release PR whose file list includes `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, `plugin/.claude-plugin/plugin.json`, `plugin/.codex-plugin/plugin.json` and `.claude-plugin/marketplace.json`, all at one new version. In the `Cargo.lock` diff, exactly the `mcpls` and `mcpls-core` version lines change.

If a file is missing or the lock diff touches other packages, fix its jsonpath in `release-please-config.json` and commit with `fix(release): <what changed>`.

---

### Task 7: Plugin README

**Files:**
- Modify: `plugin/README.md` ("Install" section, the `mcpls on PATH:` bullet, "What's included")

**Interfaces:**
- Consumes: install commands verified with `claude plugin install --help` and `codex plugin marketplace add --help`; `MCPLS_BIN` and `MCPLS_HOME` from Task 2.
- Produces: user-facing install and troubleshooting text.

- [ ] **Step 1: Replace the "Install" section**

````markdown
## Install

The plugin downloads the mcpls release matching its own version on first use, into `~/.local/share/mcpls/bin` (set `MCPLS_HOME` to move it). Language servers such as `rust-analyzer` are not installed; mcpls drives whichever ones are already on `PATH`.

Claude Code:

```fish
claude plugin marketplace add AbysmalBiscuit/mcpls
claude plugin install mcpls@mcpls
```

Codex:

```fish
codex plugin marketplace add AbysmalBiscuit/mcpls
codex plugin add mcpls@mcpls
```

Restart the session once after installing. The first session can start the MCP server while the binary is still downloading, and Codex asks you to trust the plugin's hooks on first load.

Codex waits for the first download to finish before giving up on the server. If Claude Code gives up on the server during the first download, raise its MCP startup timeout (`MCP_TIMEOUT`, in milliseconds) or restart once the download finishes.

On Codex, edits made outside Codex's own `apply_patch` tool (a terminal, another agent, a `git checkout`) reach mcpls only once the shared backend's file watcher lands (#23).

To run a local build instead of the release, point `MCPLS_BIN` at it:

```fish
set -gx MCPLS_BIN /path/to/mcpls/target/debug/mcpls
```
````

- [ ] **Step 2: Update the doctor's `mcpls on PATH:` bullet**

Replace the bullet's last two sentences ("Hooks invoke `mcpls` by name, so check the `PATH` the hook process inherits, not just your interactive shell's.") with:

```markdown
The plugin's hooks and MCP server run the launcher at `bin/mcpls` inside the plugin, not a `PATH` lookup, so this line only matters for an mcpls you run yourself. Claude Code puts the plugin's `bin/` on the Bash tool's `PATH`, so there it finds the launcher.
```

- [ ] **Step 3: Replace "What's included"**

```markdown
## What's included

- `bin/mcpls` resolves the mcpls binary this plugin version pins, downloading it on first use, and every entry below runs through it.
- `.mcp.json` and `hooks/hooks.json` register the server and hooks with Claude Code.
- `.codex-plugin/plugin.json` and `hooks/hooks-codex.json` register them with Codex. There is deliberately no `plugin.json` at this directory's root: Codex would load one as an Agent Plugins manifest, start mcpls inside the plugin cache instead of the project, and load no hooks.
- [`skills/mcpls`](skills/mcpls/) is the agent skill that explains the tools.
```

- [ ] **Step 4: Commit**

```bash
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for add plugin/README.md
git -C /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for commit -m "docs(plugin): document marketplace installs" -m "Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 8: Verify

**Files:** none changed unless a check fails.

- [ ] **Step 1: Full local verification**

Run: `devrun task verify`
Expected: formatting, clippy, nextest and doctests all pass.

Run: `bash /home/lev/Git/lev/mcpls_worktrees/package-mcpls-as-a-self-contained-plugin-for/tests/plugin/launcher.test.sh`
Expected: `0 failed`.

- [ ] **Step 2: Claude Code load check without a release**

With `MCPLS_BIN` pointing at a local build, so no release is needed yet:

```fish
cargo build -p mcpls
set -gx MCPLS_BIN (pwd)/target/debug/mcpls
claude --plugin-dir ./plugin
```

Expected: `/mcp` lists `mcpls` as connected. Editing a Rust file produces diagnostics context on the next prompt. `mcpls hook doctor` from the Bash tool reports the checkout root.

- [ ] **Step 3: Codex load check without a release**

```fish
codex plugin marketplace add (pwd)
codex plugin add mcpls@mcpls
```

Then start `codex` in this checkout from the same shell, so it inherits the exported `MCPLS_BIN`. The manifest's `env_vars` passes that through to the MCP server, and hooks inherit it directly.

Expected: the mcpls MCP server starts, and `mcpls hook doctor` run from a Codex shell reports this checkout as `root:`, not a path under `~/.codex/plugins/cache`. The plugin sits at `~/.codex/plugins/cache/mcpls/mcpls/<version>/`, the only version directory there, matching the glob in the MCP entry (`core-plugins/src/store.rs`).

- [ ] **Step 4: Real release checks (after the first release exists)**

These need a published release, so they run after the release PR from Task 6 merges and `release.yml` finishes. Unset `MCPLS_BIN` first.

- A clean `~/.local/share/mcpls` plus a Claude Code install downloads `mcpls-<version>` on first start.
- A Codex install on the same machine reuses that file without downloading.
- Codex with `CODEX_HOME` set to a non-default path still starts the server.
- A plugin upgrade leaves the old `mcpls-<version>` in the store and running sessions keep using it until they restart.

---

## Unresolved questions

1. **First release.** The fork has no releases or tags, so the plugin cannot download anything until the Task 6 release PR merges. `RELEASE_PLEASE_TOKEN`, a PAT with `contents: write` and `pull-requests: write` on the fork, has to be added as a repository secret first. Do you want that token set up before this lands, or release from a manual tag once?
2. **Windows MCP entry.** Claude Code's `.mcp.json` has one `command` for every platform, and Codex MCP configs have no `commandWindows`, so neither can point Windows at a `.cmd` twin. Is Windows MCP support in scope for this issue, or a follow-up once someone runs it there?
3. **Checksum verification.** Worth adding a `.sha256` check against the release asset, knowing it catches corruption but not tampering from the same host?
4. **Download lock.** Accept duplicated downloads when several sessions cold-start at once, or add a lock now?
