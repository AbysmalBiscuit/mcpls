# Packaging mcpls as a plugin

## Problem

The `plugin/` tree registers `mcpls` as an MCP server and wires Claude Code's hook events to `mcpls hook`. Both entries invoke the bare name `mcpls`, so the plugin only works for someone who already installed the binary and put it on `PATH`. Installing the plugin on a clean machine produces a server that fails to spawn and hooks that exit non-zero on every event.

Codex gets nothing at all. There are no Codex manifests, and the hook file uses event names Codex does not have.

Two further problems sit under the first one. The plugin version and the binary version are unrelated, so a plugin can wire hook arguments a binary of a different vintage does not accept. And on Codex the Agent Plugins format forces an MCP server's working directory inside the plugin directory, while mcpls derives its workspace root from the working directory, so a plugin-launched mcpls would index the plugin instead of the project.

## Goals

A user installs the plugin, restarts once, and language server diagnostics reach the agent. No prior mcpls, no manual install, no config file.

One plugin directory serves Claude Code and Codex on Linux, macOS and Windows. The manifests differ; the skill, the bootstrap and the binary do not.

A binary the plugin installed matches the version the plugin declares.

A developer working on mcpls can run a local build under the installed plugin without uninstalling it.

## Non-goals

Installing language servers. mcpls drives whatever `rust-analyzer`, `pyright` or `gopls` the user already has, and a plugin that installed toolchains would be a package manager wearing a plugin's clothes.

Publishing to any third-party plugin registry. The marketplace files here point at this repository.

Cursor. Devkit ships `.cursor-plugin/`; mcpls can add it later by copying the Codex manifests, and nothing in this design blocks that.

## Layout

The plugin stays in `plugin/`, a subdirectory, rather than spreading across the repository root the way devkit does. Devkit is a plugin that happens to contain a Rust workspace. mcpls is a Rust workspace that happens to ship a plugin, and a flat layout would put `hooks/`, `skills/` and `.mcp.json` next to `crates/` and `Cargo.toml`.

Three files live outside `plugin/` because their consumers only look at the repository root:

```
.claude-plugin/marketplace.json       Claude Code's marketplace index
.agents/plugins/marketplace.json      Codex's marketplace index
dist-workspace.toml                   cargo-dist's release configuration
plugin/.claude-plugin/plugin.json     Claude Code's manifest
plugin/.codex-plugin/plugin.json      Codex's manifest
plugin/.mcp.json                      server registration, both harnesses
plugin/hooks/hooks.json               Claude Code hook wiring
plugin/hooks/hooks-codex.json         Codex hook wiring
plugin/hooks/run-hook.cmd             cmd and bash polyglot that runs a hook script
plugin/hooks/bootstrap-binaries       installs mcpls onto PATH
plugin/hooks/bootstrap-binaries.ps1   its PowerShell twin, for Windows without bash
plugin/skills/mcpls/                  the skill, unchanged
```

Each marketplace entry names `./plugin` as its source. Claude Code resolves that relative path against the marketplace root, the directory containing `.claude-plugin/` ([plugin marketplaces](https://code.claude.com/docs/en/plugin-marketplaces)).

## Getting the binary onto PATH

Every entry the plugin declares runs the bare name `mcpls`: the MCP server and every hook. A `SessionStart` hook, copied from devkit's, puts it there by running the cargo-dist installer from the release that matches the plugin's version.

The bare name is what makes Windows work. Neither Claude Code's `.mcp.json` nor Codex's MCP config has a per-platform command field (`config/src/mcp_types.rs` has no `commandWindows`), and an MCP server is spawned directly rather than through a shell. Its command has to be something all three platforms can execute, and `mcpls` is: it resolves to `mcpls.exe` on Windows and the native binary elsewhere. A script inside the plugin cannot be that command everywhere.

The bootstrap takes the harness name, `claude` or `codex`, as its argument, and does this, in order:

1. Exits at once when `MCPLS_NO_BOOTSTRAP=1`.
2. Reads the version from `plugin/.claude-plugin/plugin.json`.
3. When `mcpls` is on `PATH`, reads the stamp at `${XDG_STATE_HOME:-$HOME/.local/state}/mcpls/bootstrap-version`. A stamp equal to the version means nothing to do. No stamp, or `external`, means the binary came from cargo, a package manager or a source build: the hook writes `external` and never replaces it, because upgrading it is the user's call, and tells the agent when `mcpls --version` reports a different version. Any other stamp means the plugin installed an older version, so it installs this one.
4. When `mcpls` is missing, installs.
5. Skips the install when `bootstrap-failed` in the same directory already names this version, so a broken release or a machine with no network does not retry every session. It tells the agent what is on `PATH` and how to retry.
6. Takes the harness's install lock, `install-<harness>.lock` in the same directory. When another session of the same harness holds it, the hook skips its own install and tells the agent to restart once that one finishes.
7. Installs by piping `releases/download/v<version>/mcpls-installer.sh` into `sh`, or running `mcpls-installer.ps1` through PowerShell on Windows. The installer verifies the archive's checksum, places `mcpls` in `$CARGO_HOME/bin`, and adds that directory to `PATH` for new shells.
8. Writes the version to the stamp on success, or to `bootstrap-failed` on failure, releases the lock, and tells the agent either way.

Every path exits 0, because a session must start with no network. Installer output goes to stderr, because both harnesses read a `SessionStart` hook's stdout. Stdout carries at most one JSON object, `{"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": "..."}}`, which both harnesses add to the model's context (Codex: `hooks/src/schema.rs`, `SessionStartHookSpecificOutputWire`). That note is how a user learns the binary does not match the plugin: the agent reads the installed and expected versions and what to do about them, and relays it.

The lock is a directory, because `mkdir` either creates it or fails, atomically, on every platform the hook runs on. A lock older than the hook's timeout belongs to a session the harness killed, and the next session removes it. Locks are per harness: two Claude Code sessions never install at once, and neither do two Codex sessions, while one of each can. Those two still write the same `$CARGO_HOME/bin/mcpls` and the same stamp, a race this design accepts.

`run-hook.cmd` is the entry point on every platform. cmd.exe runs its batch half, which runs the bash script under Git for Windows' bash, or the `.ps1` twin when no bash exists. A POSIX shell reads the batch half as a heredoc handed to `:` and execs the bash script. Codex runs a hook through the session's detected shell, falling back to `%COMSPEC%` on Windows only when it has none (`hooks/src/engine/command_runner.rs`, `default_shell_command`; `core/src/session/mod.rs`). On Windows that shell is normally PowerShell, where a quoted path is a string rather than a command, so the Codex entry adds a `commandWindows` that prefixes PowerShell's `&` call operator.

To run a local build, put it ahead of the installed binary on `PATH` and set `MCPLS_NO_BOOTSTRAP=1`. Without the variable, a plugin upgrade finds a stale stamp and installs the release into `$CARGO_HOME/bin`, which is also where `cargo install --path` puts a local build.

## Registering the server

`plugin/.mcp.json` registers the server for both harnesses:

```json
{
  "mcpServers": {
    "mcpls": { "command": "mcpls" }
  }
}
```

Claude Code spawns it with the session's working directory, which is the project.

Codex is the interesting one, and the choice of manifest format decides whether the plugin works at all.

The Agent Plugins MCP schema expands `${PLUGIN_ROOT}` and `${PLUGIN_DATA}`. It also requires an stdio server's `cwd` to be a contained path under one of those two roots, defaulting to `${PLUGIN_ROOT}` (`codex-mcp/src/agent_plugin_config.rs`, the `parse_agent_plugin_cwd` check). An mcpls started that way sees the plugin directory as its working directory, and since mcpls resolves its workspace root from the working directory when `workspace.roots` is empty, it would index the plugin. Codex offers no project placeholder to fill a `--root` flag with, and does not answer `roots/list`, so there is nothing to correct it with afterwards.

The legacy `.codex-plugin/plugin.json` format, which is also the only format Codex loads plugin hooks from (see Hooks), passes no `cwd` at all. The stdio launcher then falls back to Codex's own process working directory (`LocalStdioServerLauncher::new(runtime_context.local_process_cwd())` in `codex-mcp/src/rmcp_client.rs`), which is the project. That is the format to use, and the constraint is worth writing down because it reads like a step backwards: the legacy format is correct here precisely because it declines to set a working directory.

Which format a plugin gets is not a field it declares. `find_plugin_manifest_path` in `utils/plugins/src/plugin_namespace.rs` looks for `plugin.json` at the plugin root and treats the plugin as Agent Plugins format only if that file exists and carries an `$schema` under `https://agent-plugins.org/schemas/`. Anything else falls through to `.codex-plugin/plugin.json`, `.claude-plugin/plugin.json` or `.cursor-plugin/plugin.json`, and is loaded as legacy. The cwd-constrained parse runs behind a `manifest_format == AgentPlugin` check in `core-plugins/src/loader.rs`, so it is skipped entirely.

The practical rule this hands the implementer is a negative one: do not put a `plugin.json` at `plugin/`'s root. A schema-bearing one there silently switches the format, and the symptom is an mcpls indexing the plugin cache directory.

The Codex manifest names the shared file with `"mcpServers": "./.mcp.json"`. A path string loads exactly that file (`core-plugins/src/loader.rs`, `plugin_mcp_config_paths`), where an inline object would replace it. `PATH` is in the environment Codex gives every stdio MCP server (`rmcp-client/src/utils.rs`, `DEFAULT_ENV_VARS`), so the bare name resolves without an `env_vars` entry.

## Hooks

Claude Code's `hooks.json` keeps its events, and every command is `mcpls hook`. Its `SessionStart` group also runs `"${CLAUDE_PLUGIN_ROOT}/hooks/run-hook.cmd" bootstrap-binaries claude`.

Codex's `hooks-codex.json` is a different file, not a copy with substitutions, because the event vocabularies differ. Codex has `PreToolUse`, `PostToolUse`, `PreCompact`, `PostCompact`, `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `SubagentStart`, `SubagentStop`, `Stop`, `PermissionRequest` and `Interrupt` (`hooks/src/schema.rs`). It has no `FileChanged` and no `PostToolBatch`. Its commands are `mcpls hook --host codex`, and its `SessionStart` group runs only the bootstrap, as `"${PLUGIN_ROOT}/hooks/run-hook.cmd" bootstrap-binaries codex` with the `commandWindows` described above.

Both bootstrap entries set an explicit timeout sized for a slow download rather than relying on each harness's default. A killed install writes no failure marker and its lock expires, so a later session retries it.

Codex loads plugin hooks only from legacy-format plugins. The loader returns an empty hook list for any plugin whose manifest format is Agent Plugins (`core-plugins/src/loader.rs`, the `load_plugin_hooks` call), even though an Agent Plugins manifest can name a hooks file through its Codex extension (`core-plugins/src/agent_plugin_manifest.rs`). That is a second, independent reason the plugin has to stay on the legacy format: switching would lose every hook, not only the working directory.

Legacy hook commands expand placeholders. Hook discovery sets `PLUGIN_ROOT`, `PLUGIN_DATA`, `CLAUDE_PLUGIN_ROOT` and `CLAUDE_PLUGIN_DATA` in the hook's environment and substitutes `${KEY}` for each in the command string (`hooks/src/engine/discovery.rs`), which is how the bootstrap entry finds `run-hook.cmd`.

`PostToolBatch` maps to `PostToolUse`, which fires per tool call rather than per batch, so the same flush arrives more often and carries less.

`FileChanged` has no counterpart. Edits Codex makes through its own tools are visible through `PostToolUse`, but an edit from a terminal, another agent, or a `git checkout` reaches mcpls only through a watcher. This makes the backend watcher from issue #20 the thing that makes Codex work at all, not a nice-to-have, and it is the reason this document depends on that one.

Codex rejects unknown hook output fields, and JSON stdout that does not match the event's output schema marks the hook failed. Claude Code's `watchPaths` response is invalid for Codex and Codex has no `FileChanged` event to use it, so `mcpls hook --host codex` prints nothing for `SessionStart`.

Every Codex hook payload carries `cwd`, `session_id`, and for a subagent an `agent_id` and `agent_type`. Claude Code's payloads carry `session_id` and the `CLAUDE_PROJECT_DIR` environment variable instead. `mcpls hook` reads `CLAUDE_PROJECT_DIR` and falls back to `.`, which on Codex would resolve to whatever directory the hook process inherits.

So `mcpls hook` takes a `--host` flag, defaulting to `claude`. With `--host codex` it reads the project directory from the payload's `cwd` field. A subagent uses `session_id/agent_id` as its session key, which keeps its deliveries separate from the parent's without changing the socket protocol, and `SubagentStop` ends that session. The flag is explicit rather than sniffed from the payload shape, because a silent misidentification here produces a socket path for the wrong project and the symptom is an empty workspace, which is indistinguishable from a healthy one.

Codex's edit tool is `apply_patch`, whose `tool_input` holds a patch body rather than a file path. Extracting the touched paths needs a parser for the `*** Update File:` envelope. devkit already has one; port it with tests over the exact key names rather than re-deriving it.

## Version, release, and what the plugin points at

release-please owns the version. Its `extra-files` list carries every file that repeats the version string: both plugin manifests and Claude Code's marketplace file. Codex's marketplace format has no version field. One release commit moves the versioned files together with `Cargo.toml`, so nobody edits a version by hand and the manifests cannot drift from the tag.

cargo-dist builds the release. `dist-workspace.toml` lists the targets and the two installers, and `.github/workflows/release.yml` is generated from it with `dist generate`, never edited by hand. CI runs `dist generate --check`, so a hand edit, or a config change without regenerating, fails the pull request. release-please creates the tag and the GitHub Release, and `create-release = false` makes dist upload its archives, checksums and installers to that release instead of creating a second one. release-please pushes the tag with a personal access token in `RELEASE_PLEASE_TOKEN`, because a tag pushed with `GITHUB_TOKEN` does not trigger other workflows and the release build would never run.

The upstream `publish-crates` job is gone. This repository is a fork that will not be upstreamed, the crate names belong to upstream on crates.io, and the job's version check is redundant once release-please writes the version.

## Restarting once

Neither harness guarantees `SessionStart` hooks finish before it spawns the MCP server, and Codex reviews a plugin's hook trust on first load. On a first install, the first session starts before `mcpls` exists: its MCP server fails to spawn, and its other hooks fail with `mcpls` not found while the bootstrap installs it.

The honest answer is to document it: install the plugin, restart the harness once, and it works from then on. A machine whose `$CARGO_HOME/bin` was not already on `PATH` needs the harness started from a new shell, since the installer's `PATH` change only reaches new processes. Making the first session work anyway would mean shipping binaries inside the plugin or putting a script between the harness and the binary, and the section below rejects both.

`mcpls hook doctor` is what turns a failed install into a readable report, and the plugin README should send people there first. It names the socket, the project directory both sides computed, whether an owner answered, and which `mcpls` is on `PATH`.

## Testing

The bootstrap is a shell script that installs from the network, which is exactly the shape of thing that goes untested and then breaks in front of a new user. devkit's `tests/hooks/bootstrap-binaries.test.sh` is the pattern to copy: stub `curl`, `uname` and `powershell.exe` on a stubbed `PATH` and assert what the hook asked for.

What the test has to cover:

- `MCPLS_NO_BOOTSTRAP=1` touches nothing.
- A missing `mcpls` installs from the release matching the manifest version, records the stamp, and tells the agent to restart.
- An `mcpls` on `PATH` with no stamp is recorded as `external` and never installed over. It is silent when its version matches, and tells the agent both versions when it does not.
- A current stamp does nothing, and a stale stamp installs the new version.
- A failed install exits 0, records the failure, tells the agent, and suppresses the retry next session while still telling the agent.
- A lock held by the same harness skips the install and tells the agent. A lock held by the other harness does not block, a stale lock is taken over, and the lock is gone after an install.
- A Windows `uname` runs the PowerShell installer.
- `run-hook.cmd` dispatches to the script on a POSIX shell.
- Stdout is empty or exactly one `SessionStart` context object, never installer output.

A Rust test over the manifests asserts that every MCP and hook entry runs bare `mcpls`, that each hook file runs the bootstrap exactly once and only on `SessionStart`, that every file repeating the version agrees with `Cargo.toml`, and that `plugin/plugin.json` does not exist.

The hook side needs its own tests: a Codex `SessionStart` payload prints nothing, a Codex `PostToolUse` payload for `apply_patch` yields the right file list, and a Claude payload keeps behaving as it does now. Assert on exact key names, since the whole interface is untyped JSON from another project.

CI runs the bootstrap test on Linux, macOS and Windows. The Windows leg runs under Git Bash, the path Windows users with Git for Windows take.

## Verification

Beyond the tests, the things that need a real install to confirm:

- Claude Code loads the plugin from a marketplace entry pointing at `./plugin`, the first session installs `mcpls`, and after a restart the MCP server appears.
- A Codex session started in a project directory produces an mcpls whose `hook doctor` reports the project, not the plugin cache directory.
- On Windows, both harnesses run the bootstrap and, after a restart, start `mcpls.exe` as the MCP server.
- An `mcpls` installed with `cargo install` before the plugin is left in place and marked `external`.
- A plugin upgrade installs the new release, and `hook doctor` in a restarted session reports the new binary.

## Rejected alternatives

**A launcher script inside the plugin.** An earlier draft ran every entry through `plugin/bin/mcpls`, which downloaded a version-keyed binary into a per-user store on first use and exec'd it. It kept the plugin off `PATH` entirely and closed the first-session gap. It cannot be an MCP command on Windows, which has no per-platform command field to swap in a `.cmd` twin. It also put a network download inside every hook's cold start, including Codex's `SessionEnd`, whose timeout is a few seconds. And the Codex MCP entry needed an `sh -c` that searched `$CODEX_HOME` for the plugin cache, plus a startup timeout long enough to block Codex on the download. Restarting once is a smaller price than all three.

**Writing hooks and the MCP registration into user config.** An `mcpls` subcommand could merge entries into each harness's settings after a manual install. The plugin systems already register, update and remove exactly these entries, and a config writer would own merging and uninstalling in two formats that change without notice.

**Shipping binaries inside the plugin.** A release binary for every target in a git repository, to save a one-time download. No.

**One hooks file with per-harness matchers.** The event names do not overlap enough. A merged file would carry entries each harness ignores, and the first person to debug a missing event would have to work out which half applied.

## Sources

Codex publishes no prose documentation for its plugin manifests; the manifest and marketplace schemas are described in a reference file shipped inside its own plugin-creator skill, and everything else here was read from the source. Links are pinned to the tag these claims were checked against. To re-resolve against whatever version this workspace tracks now, run `docm info codex` for the ref and read the same paths under its checkout.

- [Plugin and marketplace JSON spec](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/skills/src/assets/samples/plugin-creator/references/plugin-json-spec.md), the closest thing to a manifest reference, including the field guide and the validator's rules.
- [Installing and updating](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/skills/src/assets/samples/plugin-creator/references/installing-and-updating.md), the companion file covering install paths and the cache.
- [`plugin_namespace.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/utils/plugins/src/plugin_namespace.rs), where `find_plugin_manifest_path` decides which manifest file wins and therefore which format applies.
- [`core-plugins/src/loader.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/core-plugins/src/loader.rs), which loads a manifest's MCP servers from an inline object or a named file, gates the Agent Plugins parse on the format, and skips hook loading for Agent Plugins plugins.
- [`core-plugins/src/agent_plugin_manifest.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/core-plugins/src/agent_plugin_manifest.rs), where an Agent Plugins manifest takes a hooks path from its Codex extension that the loader then ignores.
- [`hooks/src/engine/discovery.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/hooks/src/engine/discovery.rs), which sets the plugin root and data variables for plugin hooks, expands them in hook commands, and selects `commandWindows` on Windows.
- [`hooks/src/engine/command_runner.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/hooks/src/engine/command_runner.rs), which runs a hook through the session shell or falls back to `%COMSPEC%` or `$SHELL`.
- [`core/src/session/mod.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/core/src/session/mod.rs), where the session's detected shell becomes the hook shell.
- [`codex-mcp/src/agent_plugin_config.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/codex-mcp/src/agent_plugin_config.rs), the Agent Plugins MCP schema: placeholder expansion and the `cwd` containment rule.
- [`codex-mcp/src/rmcp_client.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/codex-mcp/src/rmcp_client.rs), where an stdio server with no declared `cwd` inherits Codex's own.
- [`hooks/src/schema.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/hooks/src/schema.rs), the hook event names and the payload fields each one carries.
- [`rmcp-client/src/utils.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/rmcp-client/src/utils.rs), which defines the default environment passed to stdio MCP servers.
- [`config/src/mcp_types.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/config/src/mcp_types.rs), the MCP server configuration fields, which have no `commandWindows`.
- [`hooks/src/events/session_start.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/hooks/src/events/session_start.rs), which parses SessionStart hook output and fails invalid JSON output.
- [Agent Plugins v1 schema](https://github.com/agentplugins/agent-plugins-spec/blob/main/schemas/1.0.0/plugin.schema.json), the cross-vendor spec whose `$schema` URI is what switches a plugin into the newer format.
- [cargo-dist](https://opensource.axo.dev/cargo-dist/), the release builder, including `create-release` and the shell and PowerShell installers.
- devkit's `hooks/bootstrap-binaries`, `hooks/bootstrap-binaries.ps1`, `hooks/run-hook.cmd` and `tests/hooks/bootstrap-binaries.test.sh`, the bootstrap this design copies.
