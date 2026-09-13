# Packaging mcpls as a plugin

## Problem

The `plugin/` tree registers `mcpls` as an MCP server and wires Claude Code's hook events to `mcpls hook`. Both entries invoke the bare name `mcpls`, so the plugin only works for someone who already installed the binary and put it on `PATH`. Installing the plugin on a clean machine produces a server that fails to spawn and hooks that exit non-zero on every event.

Codex gets nothing at all. There are no Codex manifests, and the hook file uses event names Codex does not have.

Two further problems sit under the first one. The plugin version and the binary version are unrelated, so a plugin can wire hook arguments a binary of a different vintage does not accept. And on Codex the Agent Plugins format forces an MCP server's working directory inside the plugin directory, while mcpls derives its workspace root from the working directory, so a plugin-launched mcpls would index the plugin instead of the project.

## Goals

A user installs the plugin, restarts once, and language server diagnostics reach the agent. No prior mcpls, no `PATH` edit, no config file.

One plugin directory serves Claude Code and Codex. The manifests differ; the skill, the launcher and the binary do not.

The binary the plugin runs matches the version the plugin declares.

A developer working on mcpls can point the plugin at a local build without uninstalling it.

## Non-goals

Installing language servers. mcpls drives whatever `rust-analyzer`, `pyright` or `gopls` the user already has, and a plugin that installed toolchains would be a package manager wearing a plugin's clothes.

Publishing to any third-party plugin registry. The marketplace files here point at this repository.

Cursor. Devkit ships `.cursor-plugin/`; mcpls can add it later by copying the Codex manifests, and nothing in this design blocks that.

## Layout

The plugin stays in `plugin/`, a subdirectory, rather than spreading across the repository root the way devkit does. Devkit is a plugin that happens to contain a Rust workspace. mcpls is a Rust workspace that happens to ship a plugin, and a flat layout would put `hooks/`, `skills/` and `.mcp.json` next to `crates/` and `Cargo.toml`.

Two files live outside `plugin/` because their consumers only look at the repository root:

```
.claude-plugin/marketplace.json     Claude Code's marketplace index
.agents/plugins/marketplace.json    Codex's marketplace index
plugin/.claude-plugin/plugin.json   Claude Code's manifest
plugin/.codex-plugin/plugin.json    Codex's manifest
plugin/.mcp.json                    Claude Code server registration
plugin/bin/mcpls                    launcher, resolves and execs the binary
plugin/hooks/hooks.json             Claude Code hook wiring
plugin/hooks/hooks-codex.json       Codex hook wiring
plugin/skills/mcpls/                the skill, unchanged
```

Each marketplace entry names `./plugin` as its source. Claude Code resolves that relative path against the marketplace root, the directory containing `.claude-plugin/` ([plugin marketplaces](https://code.claude.com/docs/en/plugin-marketplaces)).

## The launcher

Everything the plugin declares runs through `plugin/bin/mcpls`, a shell script that resolves a real binary and `exec`s it. The MCP registration and every hook command call the launcher, never a bare `mcpls`.

The launcher resolves in this order:

1. `$MCPLS_BIN`, if set and executable. This is the developer escape hatch: point it at `target/debug/mcpls` and the installed plugin drives the working tree.
2. `$MCPLS_HOME/bin/mcpls-<version>`, where `MCPLS_HOME` defaults to `$HOME/.local/share/mcpls`, and `<version>` is read out of `plugin/.claude-plugin/plugin.json`. The default does not use `XDG_DATA_HOME` because Codex's MCP environment whitelist does not include it.
3. A download of that version from this repository's release for the matching tag, into the path in step 2.

The version-keyed filename is what keeps the binary and the manifest in lockstep. Upgrading the plugin changes the version string, which changes the path, which misses and downloads. Downgrading finds the old file still there. Nothing has to compare versions or record state.

The store lives under the user's data directory rather than inside the plugin, for three reasons. Codex unpacks plugins into a versioned cache directory under `$CODEX_HOME` that should be treated as read-only. Two harnesses with the same plugin installed would otherwise hold two copies of the same binary. And a per-user path is one the launcher computes from environment variables it can read on either harness, which sidesteps placeholder expansion entirely. Codex's MCP process receives a small environment whitelist, while hooks inherit the full process environment except non-inheritable authentication variables (`hooks/src/registry.rs`, `Hooks::new`; `protocol/src/shell_environment.rs`, `NON_INHERITABLE_ENV_VARS`). Ignoring `XDG_DATA_HOME` keeps both paths on the same store.

Three properties the launcher has to hold:

**Never write to stdout.** The launcher is the MCP server's own process before the `exec`, and a single stray byte on stdout corrupts the JSON-RPC stream. Progress, warnings and failures go to stderr. The download command gets `-sS`, not a progress bar.

**Survive a concurrent start.** Many sessions in one project start at once, which is the whole reason issue #20 exists. Each downloads to a temporary file beside the target and renames it into place, so the worst case is wasted bandwidth rather than a half-written binary being executed. A lock around the download turns that waste into a wait, and is worth adding only if the wasted bandwidth shows up in practice.

**Fail loudly and exit non-zero.** This is the opposite of devkit's bootstrap hook, which exits 0 on every path because a session must start even with no network. The launcher is not a hook; it stands in for the binary. A launcher that exits 0 after failing to find a binary tells the harness the MCP server started and then closed its pipes, which is reported as a protocol error nobody can read. Exiting non-zero with the reason on stderr is what makes the failure legible.

A Windows `bin/mcpls.cmd` twin is deferred. Neither harness has a per-platform MCP command field that could select it. Hooks and the launcher run under Git Bash on Windows, and CI runs the launcher test there.

## Registering the server

`plugin/.mcp.json` registers the server for Claude Code:

```json
{
  "mcpServers": {
    "mcpls": { "command": "${CLAUDE_PLUGIN_ROOT}/bin/mcpls", "args": [] }
  }
}
```

Claude Code expands `${CLAUDE_PLUGIN_ROOT}` and spawns the launcher with the session's working directory, which is the project. That case is finished.

Codex is the interesting one, and the choice of manifest format decides whether the plugin works at all.

The Agent Plugins MCP schema expands `${PLUGIN_ROOT}` and `${PLUGIN_DATA}`, which is exactly the convenience this file wants. It also requires an stdio server's `cwd` to be a contained path under one of those two roots, defaulting to `${PLUGIN_ROOT}` (`codex-mcp/src/agent_plugin_config.rs`, the `parse_agent_plugin_cwd` check). An mcpls started that way sees the plugin directory as its working directory, and since mcpls resolves its workspace root from the working directory when `workspace.roots` is empty, it would index the plugin. Codex offers no project placeholder to fill a `--root` flag with, and does not answer `roots/list`, so there is nothing to correct it with afterwards.

The legacy `.codex-plugin/plugin.json` format, which is also the only format Codex loads plugin hooks from (see Hooks), passes no `cwd` at all. The stdio launcher then falls back to Codex's own process working directory (`LocalStdioServerLauncher::new(runtime_context.local_process_cwd())` in `codex-mcp/src/rmcp_client.rs`), which is the project. That is the format to use, and the constraint is worth writing down because it reads like a step backwards: the legacy format is correct here precisely because it declines to set a working directory.

Which format a plugin gets is not a field it declares. `find_plugin_manifest_path` in `utils/plugins/src/plugin_namespace.rs` looks for `plugin.json` at the plugin root and treats the plugin as Agent Plugins format only if that file exists and carries an `$schema` under `https://agent-plugins.org/schemas/`. Anything else falls through to `.codex-plugin/plugin.json`, `.claude-plugin/plugin.json` or `.cursor-plugin/plugin.json`, and is loaded as legacy. The cwd-constrained parse runs behind a `manifest_format == AgentPlugin` check in `core-plugins/src/loader.rs`, so it is skipped entirely.

The practical rule this hands the implementer is a negative one: do not put a `plugin.json` at `plugin/`'s root. A schema-bearing one there silently switches the format, and the symptom is an mcpls indexing the plugin cache directory.

The Codex manifest declares its MCP server as an inline `mcpServers` object. That makes the loader skip `.mcp.json` discovery entirely, so Claude Code's `${CLAUDE_PLUGIN_ROOT}` entry never reaches Codex. The inline entry is:

```json
{
  "mcpServers": {
    "mcpls": {
      "command": "sh",
      "args": ["-c", "root=$(ls -td \"${CODEX_HOME:-$HOME/.codex}\"/plugins/cache/*/mcpls/*/ 2>/dev/null | head -n 1); if [ -z \"$root\" ]; then echo \"mcpls plugin: no installed mcpls plugin under ${CODEX_HOME:-$HOME/.codex}/plugins/cache\" >&2; exit 1; fi; exec \"${root}bin/mcpls\""],
      "env_vars": ["CODEX_HOME", "MCPLS_BIN", "MCPLS_HOME"],
      "startup_timeout_sec": 300
    }
  }
}
```

`CODEX_HOME` is not in Codex's default MCP environment, so the entry requests it alongside the launcher overrides. Host plugins resolve a relative `cwd` against the plugin root, but they do not resolve a relative `command`, so `sh -c` remains necessary. MCP server configs have no `commandWindows` field. The 300-second startup timeout matches the launcher's download limit instead of Codex's 30-second default, which can kill a first start while the binary is still downloading.

## Hooks

Claude Code's `hooks.json` keeps its current events and changes every command from `mcpls hook` to `"${CLAUDE_PLUGIN_ROOT}/bin/mcpls" hook`.

Codex's `hooks-codex.json` is a different file, not a copy with substitutions, because the event vocabularies differ. Codex has `PreToolUse`, `PostToolUse`, `PreCompact`, `PostCompact`, `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `SubagentStart`, `SubagentStop`, `Stop`, `PermissionRequest` and `Interrupt` (`hooks/src/schema.rs`). It has no `FileChanged` and no `PostToolBatch`.

Codex loads plugin hooks only from legacy-format plugins. The loader returns an empty hook list for any plugin whose manifest format is Agent Plugins (`core-plugins/src/loader.rs`, the `load_plugin_hooks` call), even though an Agent Plugins manifest can name a hooks file through its Codex extension (`core-plugins/src/agent_plugin_manifest.rs`). That is a second, independent reason the plugin has to stay on the legacy format: switching would lose every hook, not only the working directory.

Legacy hook commands do expand placeholders. Hook discovery sets `PLUGIN_ROOT`, `PLUGIN_DATA`, `CLAUDE_PLUGIN_ROOT` and `CLAUDE_PLUGIN_DATA` in the hook's environment and substitutes `${KEY}` for each in the command string (`hooks/src/engine/discovery.rs`). So `hooks-codex.json` calls `"${PLUGIN_ROOT}/bin/mcpls" hook --host codex` directly, the same way devkit's `hooks-codex.json` calls its `run-hook.cmd`, and needs none of the `sh -c` path computation the server entry does.

`PostToolBatch` maps to `PostToolUse`, which fires per tool call rather than per batch, so the same flush arrives more often and carries less.

`FileChanged` has no counterpart. Edits Codex makes through its own tools are visible through `PostToolUse`, but an edit from a terminal, another agent, or a `git checkout` reaches mcpls only through a watcher. This makes the backend watcher from issue #20 the thing that makes Codex work at all, not a nice-to-have, and it is the reason this document depends on that one.

Codex rejects unknown hook output fields, and JSON stdout that does not match the event's output schema marks the hook failed. Claude Code's `watchPaths` response is invalid for Codex and Codex has no `FileChanged` event to use it, so `mcpls hook --host codex` prints nothing for `SessionStart`.

Every Codex hook payload carries `cwd`, `session_id`, and for a subagent an `agent_id` and `agent_type`. Claude Code's payloads carry `session_id` and the `CLAUDE_PROJECT_DIR` environment variable instead. `mcpls hook` today reads `CLAUDE_PROJECT_DIR` and falls back to `.`, which on Codex would resolve to whatever directory the hook process inherits.

So `mcpls hook` takes a `--host` flag, defaulting to `claude`. With `--host codex` it reads the project directory from the payload's `cwd` field. A subagent uses `session_id/agent_id` as its session key, which keeps its deliveries separate from the parent's without changing the socket protocol, and `SubagentStop` ends that session. The flag is explicit rather than sniffed from the payload shape, because a silent misidentification here produces a socket path for the wrong project and the symptom is an empty workspace, which is indistinguishable from a healthy one.

Codex's edit tool is `apply_patch`, whose `tool_input` holds a patch body rather than a file path. Extracting the touched paths needs a parser for the `*** Update File:` envelope. devkit already has one; port it with tests over the exact key names rather than re-deriving it.

## Version, release, and what the plugin points at

release-please owns the version. Its `extra-files` list carries every file that repeats the version string: both plugin manifests and Claude Code's marketplace file. Codex's marketplace format has no version field. One release commit moves the versioned files together with `Cargo.toml`, and the tag it pushes is the tag the launcher downloads from. A plugin manifest claiming a version with no matching release is then impossible to commit by hand, which is the failure this arrangement exists to prevent.

The release workflow's `publish-crates` job goes away. This repository is a fork that will not be upstreamed, the crate names belong to upstream on crates.io, and the job's version check is redundant once release-please writes the version. The binary-building matrix stays as is; it already produces the archives and checksums the launcher fetches.

## Restarting once

Codex reviews a plugin's hook trust on first load and caches the result, and it may spawn the MCP server before the SessionStart hook runs. On a first install that means the server can start while the launcher is still downloading, slow enough to trip the startup timeout, and the hooks for that session may not be trusted yet.

The honest answer is to document it: install the plugin, restart the harness once, and it works from then on. Attempting to make the first session work anyway would mean either blocking startup on a network download or shipping binaries inside the plugin for six targets, and neither is worth it for a one-time cost the user pays knowingly.

`mcpls hook doctor` is what turns a failed install into a readable report, and the plugin README should send people there first. It already names the socket, the project directory both sides computed, and whether an owner answered.

## Testing

The launcher is a shell script that downloads from the network, which is exactly the shape of thing that goes untested and then breaks in front of a new user. devkit's `tests/hooks/bootstrap-binaries.test.sh` is the pattern to copy: a test that stubs the download command and asserts behaviour against a fake release.

What the test has to cover:

- A cold install writes the binary and execs it.
- A second run with the binary present does not download.
- A version bump downloads again, under a different name, leaving the old one.
- `MCPLS_BIN` wins over everything and skips the store.
- A failed download exits non-zero with the reason on stderr.
- No path writes anything to stdout, which is the invariant that protects the JSON-RPC stream.

The hook side needs its own tests: a Codex `SessionStart` payload resolves the project from `cwd`, a Codex `PostToolUse` payload for `apply_patch` yields the right file list, and a Claude payload keeps behaving as it does now. Assert on exact key names, since the whole interface is untyped JSON from another project.

CI runs the launcher test on Linux, macOS and Windows. The Windows leg is not optional; the Git Bash path is the part most likely to be silently wrong.

## Verification

Beyond the tests, the things that need a real install to confirm:

- Claude Code loads the plugin from a marketplace entry pointing at `./plugin`, and the MCP server appears.
- A Codex session started in a project directory produces an mcpls whose `hook doctor` reports the project, not the plugin cache directory.
- The Codex `sh -c` entry resolves the launcher on a machine where `CODEX_HOME` is set to a non-default path.
- Two harnesses installed side by side share one binary in the store.
- A plugin upgrade downloads the new version and the old sessions keep running against the old binary until they restart.

## Rejected alternatives

**Installing to `PATH` the way devkit does.** devkit's bootstrap hook runs the project's own installer, which puts binaries on `PATH` through `CARGO_HOME`. That works for devkit because devkit's commands are things a user types. mcpls is invoked by the harness and nothing else, so putting it on `PATH` buys nothing and costs an installer script, a state directory, a stamp file, and a failure mode where the plugin's version and the `PATH` binary's version disagree.

**Shipping binaries inside the plugin.** Six targets of release binary in a git repository, to save a one-time download. No.

**Bootstrapping from a SessionStart hook instead of the launcher.** This is devkit's shape, and it has a race the launcher does not: the MCP server can be spawned before hooks run, so the bootstrap has to finish before the thing it bootstraps is needed, and it cannot guarantee that. Resolving inside the launcher makes the download happen exactly when something first needs the binary.

**One hooks file with per-harness matchers.** The event names do not overlap enough. A merged file would carry entries each harness ignores, and the first person to debug a missing event would have to work out which half applied.

## Open questions

Neither Claude Code's `.mcp.json` nor Codex's MCP config has a per-platform command field. Should Windows MCP support use another entry design, or remain deferred until it can be tested on Windows?

Should the launcher verify the downloaded archive's checksum against the published `.sha256` asset? It is a few lines and the assets already exist, but it is also downloading the checksum from the same host over the same TLS connection, so it catches truncation rather than tampering.

Is a lock around the download worth it, or is the wasted bandwidth from a simultaneous cold start of several sessions acceptable, given it happens once per version?

## Sources

Codex publishes no prose documentation for its plugin manifests; the manifest and marketplace schemas are described in a reference file shipped inside its own plugin-creator skill, and everything else here was read from the source. Links are pinned to the tag these claims were checked against. To re-resolve against whatever version this workspace tracks now, run `docm info codex` for the ref and read the same paths under its checkout.

- [Plugin and marketplace JSON spec](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/skills/src/assets/samples/plugin-creator/references/plugin-json-spec.md), the closest thing to a manifest reference, including the field guide and the validator's rules.
- [Installing and updating](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/skills/src/assets/samples/plugin-creator/references/installing-and-updating.md), the companion file covering install paths and the cache.
- [`plugin_namespace.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/utils/plugins/src/plugin_namespace.rs), where `find_plugin_manifest_path` decides which manifest file wins and therefore which format applies.
- [`core-plugins/src/loader.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/core-plugins/src/loader.rs), which loads a manifest's MCP servers, gates the Agent Plugins parse on the format, and skips hook loading for Agent Plugins plugins.
- [`core-plugins/src/agent_plugin_manifest.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/core-plugins/src/agent_plugin_manifest.rs), where an Agent Plugins manifest takes a hooks path from its Codex extension that the loader then ignores.
- [`hooks/src/engine/discovery.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/hooks/src/engine/discovery.rs), which sets the plugin root and data variables for plugin hooks and expands them in hook commands.
- [`codex-mcp/src/agent_plugin_config.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/codex-mcp/src/agent_plugin_config.rs), the Agent Plugins MCP schema: placeholder expansion and the `cwd` containment rule.
- [`codex-mcp/src/rmcp_client.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/codex-mcp/src/rmcp_client.rs), where an stdio server with no declared `cwd` inherits Codex's own.
- [`hooks/src/schema.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/hooks/src/schema.rs), the hook event names and the payload fields each one carries.
- [`rmcp-client/src/utils.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/rmcp-client/src/utils.rs), which defines the default environment passed to stdio MCP servers.
- [`codex-mcp/src/plugin_config.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/codex-mcp/src/plugin_config.rs), which resolves host-plugin working directories but leaves commands unchanged.
- [`config/src/mcp_types.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/config/src/mcp_types.rs), the MCP server configuration fields, which have no `commandWindows`.
- [`hooks/src/events/session_start.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/hooks/src/events/session_start.rs), which parses SessionStart hook output and fails invalid JSON output.
- [`hooks/src/registry.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/hooks/src/registry.rs), where hooks capture the Codex process environment.
- [`protocol/src/shell_environment.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/protocol/src/shell_environment.rs), which defines the authentication variables that hooks do not inherit.
- [`core-plugins/src/store.rs`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/core-plugins/src/store.rs), which manages installed plugin versions in Codex's cache.
- [Agent Plugins v1 schema](https://github.com/agentplugins/agent-plugins-spec/blob/main/schemas/1.0.0/plugin.schema.json), the cross-vendor spec whose `$schema` URI is what switches a plugin into the newer format.
