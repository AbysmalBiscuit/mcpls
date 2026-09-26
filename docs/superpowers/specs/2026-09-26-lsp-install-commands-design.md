# Custom language server install commands

Status: approved design, not yet implemented.

Target: the `AbysmalBiscuit/mcpls` fork, issue #71.

## Problem

A fresh checkout needs its language servers installed before mcpls can use them. `mcpls doctor` reports each missing binary as `not installed: X is not on PATH`, but the fix lives in a README or someone's memory. A project that already pins its servers in `mcpls.toml` has nowhere to say how to get them, so bootstrapping a checkout takes a trip through each server's install docs, once per platform.

## Goals

1. A `[[lsp_servers]]` entry can carry the command that installs its binary, as one string for every OS or as separate Unix and Windows variants.
2. One command installs every missing language server that applies to a checkout.
3. `mcpls doctor` points at that command when a server with an install command is missing.

## Non-goals

- Installing automatically. mcpls never runs an install command unless someone runs `mcpls lsp install`.
- Default install commands for the built-in servers. The right package manager varies by machine, so only configured servers get one.
- Upgrading installed servers. A server whose binary resolves is skipped, and upgrades stay with the package manager.
- Starting a backend. After installing, the CLI tells a backend that already answers to start the new servers, and does nothing when none answers.
- Running installs in parallel.

## Configuration

`install` joins `LspServerConfig` and `PartialLspServerConfig`:

```toml
[[lsp_servers]]
language_id = "python"
command = "pyrefly"
args = ["lsp"]
install = "uv tool install pyrefly"

[[lsp_servers]]
language_id = "zig"
[lsp_servers.install]
unix = "brew install zls"
windows = "winget install zigtools.zls"
```

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum InstallCommand {
    /// One shell command for every OS.
    Any(String),
    /// A shell command per OS family; a missing key means no command there.
    PerOs(PerOsInstall),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PerOsInstall {
    pub unix: Option<String>,
    pub windows: Option<String>,
}
```

- `InstallCommand::for_this_os() -> Option<&str>` picks the command for the running OS. `unix` covers Linux and macOS. A blank command counts as none.
- `LspServerConfig::install` is `Option<InstallCommand>`, absent by default, and skipped when serializing a `None`.
- Overlay rule, same as `args` and `env`: an overlay inherits the built-in's `install` unless it replaces `command`, in which case an omitted value means none. Built-ins carry no install command.
- A table with an unknown key, such as `macos`, is a config error.
- `devrun task schema` regenerates `schema/mcpls-config.json`.

## Trust

Install commands come from whichever config tier wins for the directory, resolved the way `mcpls config` resolves it and honoring `--config` and `--trust-project-config`. A checkout-root `mcpls.toml` loads only with `--trust-project-config`, the gate that already guards `command`, so an untrusted checkout cannot make `mcpls lsp install` run anything. When the resolution skipped a checkout-root `mcpls.toml`, the report says so with the line `mcpls doctor` prints, because a fresh checkout's own config is the likeliest home for install commands.

## CLI

```
mcpls lsp install <SERVER>... | --all  [--dir DIR] [--dry-run]
```

- `SERVER`, `--all` and `--dir` reuse `LspTargets` and mean what they mean for `mcpls lsp start`. `--all` covers every server whose project markers apply to the checkout, the test `mcpls doctor` uses.
- A named id that does not apply to the checkout fails with the applicable ids listed, matching the other `lsp` verbs.
- The install runs in the CLI process. It needs no backend.

For each target, in configuration order:

1. If the server's binary resolves (`resolve_program`), it is reported `already installed` and skipped.
2. If it has no install command for this OS, it is reported `no install command`.
3. Otherwise the command runs.

Each command runs to completion before the next starts, because package managers commonly hold a global lock. Before running, the CLI prints a header naming the server and the command:

```
==> python: uv tool install pyrefly
```

The command runs with inherited stdio, so installer output and prompts reach the terminal. Unix runs `sh -c <command>`. Windows runs `powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand <base64 of the UTF-16LE command>`. The command is encoded because Windows argument quoting mangles double quotes on their way into `-Command`, and the execution policy is bypassed because package managers such as npm install `.ps1` shims the default policy refuses. That shell is Windows PowerShell 5.1, which has no `&&`, and its `;` runs the next command even after a failure, so the docs tell a Windows command to put the installer last or stop with `if (-not $?) { exit 1 }`. The working directory is the checkout root and the environment is the CLI's own. The server's `env` table does not apply, since it describes the server process.

After a command exits 0, the CLI resolves the binary again. When it still does not resolve, the installer most likely changed `PATH` only for new shells, as winget and rustup do. The report says so and suggests opening a new shell.

`--dry-run` prints the header for each command that would run and runs nothing.

### Backend nudge

When the installs finish, the CLI probes the checkout's backend. If one answers, it sends `Request::Lsp { action: Start, servers }` naming each server this run installed, and does not wait for them to settle. A server reported `already installed` is left alone, so the nudge never restarts a server someone stopped with `mcpls lsp stop`. The backend lifecycle already allows `not installed` to `starting`. When no backend answers, the CLI skips the nudge silently.

### Report and exit status

The run ends with one line per target, id then outcome, in the shape `mcpls lsp status` prints:

```
python  installed
zig  no install command
rust  already installed
```

Outcomes: `installed`, `already installed`, `no install command`, `failed (exit N)`, `installed, but <command> is still not on PATH; open a new shell`, and under `--dry-run`, `would run`.

Exit status is 1 when:

- an install command exits non-zero or cannot be started;
- a binary still does not resolve after its command succeeded;
- a server named on the command line has no install command for this OS;
- a named id does not apply to the checkout.

Otherwise it is 0. Under `--all`, a server with no install command is informational and does not fail the run.

## Doctor

A `not installed` server line gains a hint when that server has an install command for this OS:

```
not installed: zls is not on PATH; run `mcpls lsp install zig`
```

## Testing

- Config unit tests, beside the existing ones in `config/server.rs` and `config/mod.rs`: both forms parse, an unknown per-OS key is rejected, `for_this_os` picks the right variant, and the overlay rule inherits or drops `install` as described.
- CLI integration tests in `crates/mcpls-cli/tests/`, driving the built binary against a temp checkout and config passed with `--config`:
  - an install command that writes a fake server executable into a directory on the test's `PATH`, reported `installed`, exit 0;
  - a command that exits non-zero, reported `failed`, exit 1;
  - `--dry-run` prints the header, and the fake executable is never created;
  - a server whose binary already resolves is `already installed` and its command does not run;
  - a named server with no install command exits 1; the same server under `--all` does not.
- Test install commands are written per OS through the `unix`/`windows` table, which exercises the feature itself.
- A doctor test asserts the hint appears for a missing server with an install command and is absent without one.

## Documentation

- `docs/user-guide/configuration.md`: the `install` key.
- `plugin/skills/setup-mcpls/references/configuration.md` and `cli.md`: the key and the verb.
- `schema/mcpls-config.json`, regenerated.
