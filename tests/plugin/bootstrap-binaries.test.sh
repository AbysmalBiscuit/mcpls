#!/usr/bin/env bash
# Offline behavioural tests for plugin/hooks/bootstrap-binaries.

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
echo '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"mcpls plugin: restart the session"}}'
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
        bash "${REPO_ROOT}/plugin/hooks/run-hook.cmd" bootstrap-binaries claude-code \
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
run_hook with-mcpls claude-code MCPLS_NO_BOOTSTRAP=1
check "opt-out exits 0" 0 "$last_exit"
check "opt-out touches nothing" NONE "$(stamp)"
check "opt-out says nothing" "" "$(out)"

new_state
run_hook without-mcpls claude-code
check "a missing mcpls installs" 0 "$last_exit"
check "install records the version" "$VERSION" "$(stamp)"
check "install pins the release" "$EXPECTED_CURL" "$(calls)"
check "install tells the agent to restart" yes "$(context_says "restart the session")"
check "install releases its lock" no "$([ -e "$(lock claude-code)" ] && echo yes || echo no)"

# An mcpls the hook did not install must survive untouched.
new_state
run_hook with-mcpls claude-code FAKE_MCPLS_VERSION="$VERSION"
check "an unstamped mcpls is external" external "$(stamp)"
check "an external mcpls skips the network" NONE "$(calls)"
check "a matching external mcpls says nothing" "" "$(out)"

new_state
run_hook with-mcpls claude-code FAKE_MCPLS_VERSION=0.0.1
check "a mismatched external mcpls stays external" external "$(stamp)"
check "a mismatched external mcpls skips the network" NONE "$(calls)"
check "a mismatched external mcpls names both versions" yes \
    "$(context_says "the mcpls on PATH is 0.0.1 but this plugin expects ${VERSION}")"

new_state
set_stamp "$VERSION"
run_hook with-mcpls claude-code
check "the current version is a no-op" NONE "$(calls)"
check "the current version keeps the stamp" "$VERSION" "$(stamp)"
check "the current version says nothing" "" "$(out)"

new_state
set_stamp $'external\r'
run_hook with-mcpls claude-code
check "a CRLF external stamp skips the network" NONE "$(calls)"
check "a CRLF external stamp stays external" external "$(stamp)"

new_state
set_stamp "${VERSION}"$'\r'
run_hook with-mcpls claude-code
check "a CRLF current stamp skips the network" NONE "$(calls)"
check "a CRLF current stamp says nothing" "" "$(out)"

new_state
printf '%s\r\n' "$VERSION" >"${state}/mcpls/bootstrap-failed"
run_hook without-mcpls claude-code
check "a CRLF failure suppresses the retry" NONE "$(calls)"
check "a CRLF failure tells the agent" yes "$(context_says "failed in an earlier session")"

new_state
controls=""
for ((code = 1; code < 32; code++)); do
    printf -v octal '%03o' "$code"
    printf -v char '%b' "\\${octal}"
    controls+="$char"
done
state="${state}/${controls}"
run_hook without-mcpls claude-code CURL_FAIL=1
check "control characters keep context on one line" yes "$(context_says "failed")"
for ((code = 1; code < 32; code++)); do
    printf -v escaped '\\u%04x' "$code"
    check "context escapes control $code" yes "$(context_says "$escaped")"
done

# A plugin update moves plugin.json's version past the stamp.
new_state
set_stamp 0.0.1
run_hook with-mcpls claude-code
check "a stale stamp reinstalls" "$VERSION" "$(stamp)"
check "a reinstall pins the new release" "$EXPECTED_CURL" "$(calls)"
check "a reinstall tells the agent to restart" yes "$(context_says "restart the session")"

new_state
run_hook without-mcpls claude-code CURL_FAIL=1
check "a failed install still exits 0" 0 "$last_exit"
check "a failed install records no version" NONE "$(stamp)"
check "a failed install marks the version" "$VERSION" "$(marker)"
check "a failed install tells the agent" yes "$(context_says "installing mcpls ${VERSION} failed")"

# Having marked a failure, the hook must not retry it every session, but the
# agent still hears about it.
run_hook without-mcpls claude-code
check "a marked failure suppresses the retry" NONE "$(calls)"
check "a marked failure still tells the agent" yes "$(context_says "failed in an earlier session")"

new_state
mkdir "$(lock claude-code)"
run_hook without-mcpls claude-code
check "a held lock skips the install" NONE "$(calls)"
check "a held lock tells the agent" yes "$(context_says "another claude-code session is installing")"

new_state
mkdir "$(lock codex)"
run_hook without-mcpls claude-code
check "the other harness's lock does not block" "$EXPECTED_CURL" "$(calls)"
check "the other harness's lock stays" yes "$([ -d "$(lock codex)" ] && echo yes || echo no)"

new_state
mkdir "$(lock claude-code)"
touch -t 200001010000 "$(lock claude-code)"
run_hook without-mcpls claude-code
check "a stale lock is taken over" "$EXPECTED_CURL" "$(calls)"

new_state
run_hook without-mcpls codex FAKE_UNAME=MINGW64_NT-10.0
check "windows exits 0" 0 "$last_exit"
check "windows delegates to the checksum-verifying PowerShell bootstrap" \
    "powershell -NoProfile -ExecutionPolicy Bypass -File ${REPO_ROOT}/plugin/hooks/bootstrap-binaries.ps1 codex" \
    "$(calls)"
check "windows keeps installer output off stdout" yes "$(context_says "restart the session")"

# The wrapper the hook files invoke, rather than the hook directly.
new_state
run_wrapper
check "run-hook.cmd dispatches on a POSIX shell" external "$(stamp)"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
