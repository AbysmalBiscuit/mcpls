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

# The Codex MCP entry cannot use a plugin-root placeholder, so it finds the
# installed copy of the plugin under CODEX_HOME and execs its launcher. Codex
# keeps one version of a plugin in its cache and deletes the old one on
# upgrade (core-plugins/src/store.rs), so the fixture installs one.
codex_command=$(jq -r '.mcpServers.mcpls.command' "${REPO_ROOT}/plugin/.codex-plugin/plugin.json" 2>/dev/null)
check "the Codex entry uses sh" sh "$codex_command"
codex_arg0=$(jq -r '.mcpServers.mcpls.args[0]' "${REPO_ROOT}/plugin/.codex-plugin/plugin.json" 2>/dev/null)
check "the Codex entry invokes sh with -c" -c "$codex_arg0"
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

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
