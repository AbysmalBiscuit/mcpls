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
plugin/.mcp.json                    server registration, both harnesses
plugin/bin/mcpls                    launcher, resolves and execs the binary
plugin/bin/mcpls.cmd                Windows twin of the launcher
plugin/hooks/hooks.json             Claude Code hook wiring
plugin/hooks/hooks-codex.json       Codex hook wiring
plugin/skills/mcpls/                the skill, unchanged
```

Each marketplace entry names `./plugin` as its source. Whether Claude Code's `source` field accepts a subdirectory for a local path source has to be confirmed against a real install before the layout is settled; if it does not, the marketplace file moves into `plugin/.claude-plugin/` and the repository root keeps a pointer.

## The launcher

Everything the plugin declares runs through `plugin/bin/mcpls`, a shell script that resolves a real binary and `exec`s it. The MCP registration and every hook command call the launcher, never a bare `mcpls`.

The launcher resolves in this order:

1. `$MCPLS_BIN`, if set and executable. This is the developer escape hatch: point it at `target/debug/mcpls` and the installed plugin drives the working tree.
2. `$MCPLS_HOME/bin/mcpls-<version>`, where `MCPLS_HOME` defaults to `${XDG_DATA_HOME:-$HOME/.local/share}/mcpls`, and `<version>` is read out of `plugin/.claude-plugin/plugin.json`.
3. A download of that version from this repository's release for the matching tag, into the path in step 2.

The version-keyed filename is what keeps the binary and the manifest in lockstep. Upgrading the plugin changes the version string, which changes the path, which misses and downloads. Downgrading finds the old file still there. Nothing has to compare versions or record state.

The store lives under the user's data directory rather than inside the plugin, for three reasons. Codex unpacks plugins into a versioned cache directory under `$CODEX_HOME` that should be treated as read-only. Two harnesses with the same plugin installed would otherwise hold two copies of the same binary. And a per-user path is one the launcher computes from environment variables it can read on either harness, which sidesteps placeholder expansion entirely.

Three properties the launcher has to hold:

**Never write to stdout.** The launcher is the MCP server's own process before the `exec`, and a single stray byte on stdout corrupts the JSON-RPC stream. Progress, warnings and failures go to stderr. The download command gets `-sS`, not a progress bar.

**Survive a concurrent start.** Many sessions in one project start at once, which is the whole reason issue #20 exists. Each downloads to a temporary file beside the target and renames it into place, so the worst case is wasted bandwidth rather than a half-written binary being executed. A lock around the download turns that waste into a wait, and is worth adding only if the wasted bandwidth shows up in practice.

**Fail loudly and exit non-zero.** This is the opposite of devkit's bootstrap hook, which exits 0 on every path because a session must start even with no network. The launcher is not a hook; it stands in for the binary. A launcher that exits 0 after failing to find a binary tells the harness the MCP server started and then closed its pipes, which is reported as a protocol error nobody can read. Exiting non-zero with the reason on stderr is what makes the failure legible.

The Windows twin follows devkit's `run-hook.cmd` pattern, a polyglot file cmd.exe and bash both accept, downloading the `.zip` release asset and using PowerShell when no bash is present.

## Registering the server

`plugin/.mcp.json` is shared:

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

The legacy `.codex-plugin/plugin.json` format, which is also the format that loads hooks today, passes no `cwd` at all. The stdio launcher then falls back to Codex's own process working directory (`LocalStdioServerLauncher::new(runtime_context.local_process_cwd())` in `codex-mcp/src/rmcp_client.rs`), which is the project. That is the format to use, and the constraint is worth writing down because it reads like a step backwards: the legacy format is correct here precisely because it declines to set a working directory.

What the legacy format costs is placeholder expansion. `${PLUGIN_ROOT}` in a legacy `.mcp.json` stays a literal string. The launcher has to be located some other way, and the plugin's own unpacked location under `$CODEX_HOME/plugins/cache/<marketplace>/<plugin>/<version>/` is derivable from environment Codex does export. The entry becomes a `sh -c` that computes the path and execs the launcher, with the same expression in `commandWindows` form for PowerShell. This is the ugliest part of the design and the first thing to delete if a future Codex release expands placeholders in the legacy path or relaxes the `cwd` rule in the new one.

## Hooks

Claude Code's `hooks.json` keeps its current events and changes every command from `mcpls hook` to `"${CLAUDE_PLUGIN_ROOT}/bin/mcpls" hook`.

Codex's `hooks-codex.json` is a different file, not a copy with substitutions, because the event vocabularies differ. Codex has `PreToolUse`, `PostToolUse`, `PreCompact`, `PostCompact`, `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `SubagentStart`, `SubagentStop`, `Stop`, `PermissionRequest` and `Interrupt` (`hooks/src/schema.rs`). It has no `FileChanged` and no `PostToolBatch`.

`PostToolBatch` maps to `PostToolUse`, which fires per tool call rather than per batch, so the same flush arrives more often and carries less.

`FileChanged` has no counterpart. Edits Codex makes through its own tools are visible through `PostToolUse`, but an edit from a terminal, another agent, or a `git checkout` reaches mcpls only through a watcher. This makes the backend watcher from issue #20 the thing that makes Codex work at all, not a nice-to-have, and it is the reason this document depends on that one.

Every Codex hook payload carries `cwd`, `session_id`, and for a subagent an `agent_id` and `agent_type`. Claude Code's payloads carry `session_id` and the `CLAUDE_PROJECT_DIR` environment variable instead. `mcpls hook` today reads `CLAUDE_PROJECT_DIR` and falls back to `.`, which on Codex would resolve to whatever directory the hook process inherits.

So `mcpls hook` takes a `--host` flag, defaulting to `claude`. With `--host codex` it reads the project directory from the payload's `cwd` field, and carries `agent_id` through as the agent identity the delivery records in #20 key on. The flag is explicit rather than sniffed from the payload shape, because a silent misidentification here produces a socket path for the wrong project and the symptom is an empty workspace, which is indistinguishable from a healthy one.

Codex's edit tool is `apply_patch`, whose `tool_input` holds a patch body rather than a file path. Extracting the touched paths needs a parser for the `*** Update File:` envelope. devkit already has one; port it with tests over the exact key names rather than re-deriving it.

## Version, release, and what the plugin points at

release-please owns the version. Its `extra-files` list carries every file that repeats the version string: both plugin manifests and both marketplace files. One release commit moves all of them together with `Cargo.toml`, and the tag it pushes is the tag the launcher downloads from. A plugin manifest claiming a version with no matching release is then impossible to commit by hand, which is the failure this arrangement exists to prevent.

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

CI runs the launcher test on Linux, macOS and Windows. The Windows leg is not optional; the polyglot launcher is the part most likely to be silently wrong.

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

Does Claude Code's marketplace `source` accept `./plugin`, or does the marketplace file have to sit beside the manifest?

Should the launcher verify the downloaded archive's checksum against the published `.sha256` asset? It is a few lines and the assets already exist, but it is also downloading the checksum from the same host over the same TLS connection, so it catches truncation rather than tampering.

Is a lock around the download worth it, or is the wasted bandwidth from a simultaneous cold start of several sessions acceptable, given it happens once per version?
