# Diagnostics injection

Status: designed, not implemented.

Supersedes parts 2 through 4 of `2026-09-05-lsp-apply-and-diagnostics-hooks-design.md`. That document's part 1, applying edits, shipped; the rest is replaced by this one.

Target: the `AbysmalBiscuit/mcpls` fork, not upstream. Defaults are tuned for one user, and breaking changes to configuration are acceptable when they buy ergonomics.

Host behaviour was read out of the Claude Code binary rather than its documentation and is version specific. Verified against 2.1.263; the recipe for re-verifying is at the end.

## Problem

mcpls holds warm language servers with full workspace indexes and a live `publishDiagnostics` cache, and none of it reaches the agent unless the agent thinks to ask. The agent edits a file, moves on, and learns what it broke when something unrelated fails later.

Underneath that sits a second problem: mcpls tells its language servers about a file only when a tool call touches it. It accepts `client/registerCapability` and answers `null` (`lsp/client.rs:767`), so a server asking the client to watch files is met with silence. A write from `cargo fmt`, `sed -i`, `git checkout`, or another editor is invisible to every server that does not run its own watcher, which is most of them.

## Rejected alternative: plugin `.lsp.json`

Claude Code will spawn a language server itself through a plugin's `.lsp.json`, and its `forwardDiagnostics` key defaults to true, documented as pushing `publishDiagnostics` into the agent context after edits. That is this feature, for no code at all, and the design has to justify itself against it:

- It fires on `Edit` and `Write`. Every other writer produces nothing.
- It spawns a second language server beside the one mcpls already runs, so two rust-analyzer indexes per project.
- It needs a parallel server list maintained in `.lsp.json` rather than the routing, heuristics, and respawn logic already in `mcpls.toml`.
- It is Claude Code only.

Anyone who wants only the first bullet's coverage should use `.lsp.json` and skip all of this.

## Goals

1. New diagnostics reach the agent without the agent asking.
2. Every writer counts, not only the agent's edit tools.
3. One warm set of servers, the ones mcpls already owns, configured in one place.
4. Any MCP host benefits. Claude Code gets more, because it can offer more.

## Non-goals

- Upstreaming.
- A standalone mcpls daemon surviving host restarts. The MCP process keeps owning the language servers.
- Runtime workspace root changes. Roots resolve from config at startup, as today.
- Per-diagnostic deduplication. Per file is the chosen granularity, and the reasoning is in stage A.

## Staging

Three stages. Each lands on main in a working state and is useful without the ones after it.

- **Stage A** makes configuration merge, then builds the delivery core and the two host-agnostic ways to reach it. No IPC, no plugin, no host-specific code.
- **Stage B** implements the `workspace/didChangeWatchedFiles` client half, which is what makes writes from outside the agent visible to the servers. It improves stage A without changing it.
- **Stage C** adds the socket, the `mcpls hook` CLI, and the Claude Code plugin, so delivery becomes push rather than pull.

## Stage A

### A1: configuration entries merge onto the built-ins

Writing any `[[lsp_servers]]` block today replaces the built-in server list wholesale. The test at `config/mod.rs:1003` pins the behaviour: a file with a single rust entry yields exactly one server, and the other built-ins are gone. Reaching one per-server key therefore costs every default server, which is why `handles` and `request_timeout_seconds` are effectively unreachable, and why a per-server diagnostics key would be unreachable too.

Entries merge onto the built-ins, keyed by `LspServerConfig::id()` (`config/server.rs:270`), which is `name` when set and `language_id` otherwise. That method is already the routing identity across `Translator`'s maps, with collision enforcement in `ToolRouter::from_configs`, so the merge introduces no new notion of identity.

Every field becomes individually optional. An absent field keeps the built-in's value. An entry whose id matches no built-in defines a new server, and there the fields with no default, `command` above all, are required; a partial entry matching nothing is a configuration error naming the id it failed to find.

Two semantics are settled explicitly rather than left to discover:

- `args = []` means empty arguments, not "unspecified". A partial entry distinguishes a present empty list from an absent key, so both are expressible.
- `initialization_options` replaces rather than deep-merging. A deep merge of arbitrary JSON has no single right answer, and nothing needs one.

Dropping a built-in was free under replace semantics and needs a spelling under merge: `enabled = false` on an entry removes that server. This is a deliberate breaking change. A config that lists one server in order to suppress the rest must now disable the rest by name.

### A2: the delivery core

A new module under `bridge`, not an addition to `notifications.rs`, which is already long enough that adding a second responsibility to it would be the wrong shape.

The core holds one record per session: a map from URI to a hash of that file's diagnostics. The hash covers a sorted set of `(range, severity, message)` rather than publish order, because `store_diagnostics` (`bridge/notifications.rs:606`) re-sorts survivors by severity when it truncates, and an order-sensitive hash would report a change that nobody made.

A session seen for the first time snapshots the current cache as its baseline and returns nothing. Without that, the first flush of every session dumps the workspace's entire pre-existing warning set, and so does the first flush after a restart.

A flush returns every file whose hash differs from the record, then updates the record. A file that transitions to zero diagnostics is reported as a single line saying its problems are gone, which is cheap and is exactly the confirmation an agent wants after a fix.

Deduplication is per file rather than per diagnostic. This needs far less state, and when a file breaks, its full current error list is more useful than a delta against a list that has long since left the context window.

Stdio means one mcpls process per client, so in stage A the record is per process. It is still keyed by session id from the start, with stage A passing a constant, so stage C's multi-session owner is not a retrofit.

**Stale publishes.** `diagnostics_pump` (`lib.rs:154`) already receives `p.version` and hands it to `store_diagnostics`. It gains the document tracker handle so a publish whose version is below the tracked version is dropped rather than stored. Without that, a late publish describing pre-edit text overwrites the current entry, flips the file's hash, shows the agent errors for text that no longer exists, and flips back on the next publish. Claude Code drops these natively for the same reason.

### A3: two ways to reach it

**A flush tool.** Always present, no configuration. The agent calls it and gets everything new since its last flush. Zero noise, works on every host, and it is the only path that exists before stage C.

**A footer on the tools that write.** `rename_symbol`, `format_document`, and `apply_code_action` append new diagnostics to their own results, because those calls just changed the tree and the answer is wanted immediately. Off by default. Read tools never carry a footer: an agent asking `get_hover` what type something is should not receive forty lines of unrelated compiler errors.

### Configuration

```toml
[diagnostics]
footer       = false     # append to the results of the tools that write
severity     = "warning" # off | error | warning | information | hint
max_per_file = 10
max_total    = 50

[[lsp_servers]]
language_id          = "rust"
diagnostics_severity = "error"   # overrides the global floor for this server
```

`off` mutes a server entirely, which is how a language is excluded. One scale does both jobs, so there is no second place to look when a language goes quiet.

The severity floor is the one knob that genuinely varies by server: rust-analyzer's clippy warnings, marksman's prose hints, and ty's inference notes are not the same signal, and wanting errors only from some of them is normal. A1 is what makes that per-server key reachable without abandoning the built-ins.

The caps stay global because they are one shared context budget. Per-server caps let three servers each spend the whole thing. `footer` stays global because it shapes a tool result, and a result belongs to a call rather than to a server.

Truncation against the caps is stated in the output rather than applied silently.

Stage C adds `[diagnostics.hooks]` under the same table.

### Testing

The core is pure, so it takes unit tests directly: a first-sight session returns nothing, a changed file is returned once and not twice, a cleared file reports once, a publish below the tracked version is dropped, and caps truncate and say so in the output.

A1 gets its own: a partial entry keeps the built-in's other fields, an entry with an unmatched id and no command is rejected naming the id, `enabled = false` removes a built-in, `args = []` produces empty arguments rather than the built-in's, and two entries sharing an id still collide the way `ToolRouter::from_configs` already enforces.

End to end, `tests/ra_e2e.rs` already drives a real rust-analyzer against `tests/fixtures/rust_workspace`. It gains a test that breaks a caller and asserts the fanout file appears in a later flush.

## Stage B: file watching

### Why mcpls needs it

Every configured language server was checked against its own source. Only one of them watches the filesystem for itself.

| Server | Watches for itself | Needs the client to watch | Sends `workspace/applyEdit` |
|---|---|---|---|
| rust-analyzer | yes | no | not observed |
| gopls | no | yes | yes |
| tsgo | Windows and macOS only | yes on Linux | protocol types only |
| vtsls | no | yes | yes |
| typescript-language-server | no | yes | yes |
| pyrefly | no | yes | no |
| ty | no | yes | no |
| lua-language-server | no | yes | yes |
| taplo | no | ignores the notification | no |
| marksman | no | ignores the notification | no |

The two that decide at runtime both land badly under mcpls as it stands:

- **gopls** builds its watcher registrations in `registerWatchedDirectoriesLocked` (`gopls/internal/server/general.go:596`), whose first statement returns `nil` when `DynamicWatchedFilesSupported` is false. There is no in-process fallback. A client that does not watch leaves gopls seeing only the documents it is told about.
- **tsgo** chooses among three branches at init (`internal/lsp/server.go:1694-1713`): client-side watching when the client advertises `didChangeWatchedFiles.dynamicRegistration`; otherwise an in-process watcher, which its own comment limits to Windows and FSEvents; otherwise file watching disabled. On Linux and WSL2 that is the third branch.

ty and typescript-language-server both gate registration on the same client capability (`crates/ty_server/src/session.rs:958`, `src/lsp-server.ts:187`). pyrefly registers `FileSystemWatcher` patterns with the client (`pyrefly/lib/lsp/non_wasm/server.rs:5811`); the `notify`-based watcher elsewhere in that codebase belongs to the CLI `check` command.

taplo and marksman handle no watched-files notification at all, so they see only what document synchronization tells them. Nothing here changes that, and nothing can.

### The client half

1. `client/registerCapability` for `workspace/didChangeWatchedFiles` stops being answered with a bare `null` (`lsp/client.rs:767`). The registration's `watchers` array, a glob pattern plus a change-kind bitmask, is stored per server and keyed by registration id. `client/unregisterCapability` drops it. gopls re-registers as its watched directory set grows, incrementing its own registration id, so registrations accumulate per server rather than replacing one another.
2. `workspace.did_change_watched_files.dynamic_registration = true` is advertised, which also moves gopls and tsgo out of their do-nothing branches.
3. When mcpls learns a path changed, it notifies every server whose registered globs match that path and whose bitmask includes the event kind.

**The tripwire is already in the tree.** `test_client_capabilities_do_not_claim_dynamic_file_watching` (`lsp/lifecycle.rs:933`) asserts that mcpls does not advertise this capability, with the reason inline: advertising without sending the notification blinds gopls and tsgo. Stage B is the change that flips it, and the advertisement and the notification must land in the same commit. An incomplete stage B leaves both servers worse off than today.

### Where change events come from

Before stage C there is no watcher, so stage B's inputs are the paths mcpls already learns about: the targets of an apply, and the files a tool call touches. That alone is worth having, because it is what tells gopls about the files a rename rewrote.

Stage C adds the real event source.

### What a change does

A path already tracked is re-read through `disk_phase` (`bridge/state.rs`), and if the content is unchanged, nothing is sent. This matters more than it looks: every `didSave` restarts rust-analyzer's flycheck, cancelling the `cargo check` in progress, so an unconditional resync after a twenty-file apply produces twenty cancel-and-restart cycles before any diagnostics land. When content did change, the document version bumps and every server holding it receives `didChange` then `didSave`, under the path lock.

A path not tracked is never opened by the watcher path. It is reported through `workspace/didChangeWatchedFiles` to the servers that asked for it, which is the mechanism those servers wanted, and it costs no tracker slot. Paths arriving from tool inputs are different: they are bounded by what the agent actually edited, and they are opened if routable, so a first-touch file still produces diagnostics.

`add` maps to a watched-file create event, `unlink` to a delete event plus `didClose` if the path was tracked.

### Settle before building

Does rust-analyzer run flycheck for a change it learned about from its own watcher rather than from `didSave`? If it does not, an external write to a Rust file yields no compiler diagnostics until a tool call opens the file, which removes most of stage B's value for the language the fork's owner uses most. Measure this against a live server before building on the assumption.

## Stage C: socket, hooks, plugin

### Socket ownership

The socket path derives from a hash of mcpls's canonicalized startup working directory: `$XDG_RUNTIME_DIR/mcpls/<hash>.sock` on Linux with a `/tmp/mcpls-<uid>` fallback where that variable is unset, `$TMPDIR` on macOS, `\\.\pipe\mcpls-<hash>` on Windows. `tokio` is already on `features = ["full"]`, so both transports exist without a new dependency.

The startup working directory, not `workspace.roots[0]`, because the hook hashes `CLAUDE_PROJECT_DIR` and the two must agree. A config whose roots point at a subdirectory, a multi-root config, or a symlinked checkout would otherwise produce a permanent silent no-op. Claude Code spawns stdio MCP servers in the project directory, so the two match. `mcpls hook doctor` prints both hashes.

The first mcpls instance in a workspace binds. A later instance gets `EADDRINUSE` and becomes passive, retrying the bind every 5 seconds so that an owner exiting does not strand it for the rest of its session. A connect attempt arbitrates on its own: it succeeds when a listener holds the socket, and fails with `ECONNREFUSED` on a stale file, which is then unlinked.

### Protocol

Newline-delimited JSON, one request per connection, connection closes after the response.

```
{"op":"changed","session":"<id>","paths":["/abs/src/x.rs"],"event":"change"}
{"op":"flush","session":"<id>"}
{"op":"status"}
```

`changed` runs stage B's filters and resync and does not flush. `flush` drains and does not resync. `PostToolBatch` sends `changed` then `flush` on one connection. `status` serves `doctor`.

### Bounding the watched set

Claude Code has a `FileChanged` hook event backed by a real file watcher, delivering `{file_path, event}` where event is `change`, `add`, or `unlink`. That covers every writer, which is the point.

Its watcher passes no ignore list and rejects glob patterns, so the watched set has to be constrained where it is declared, by returning `watchPaths`. mcpls builds that list by walking each workspace root to depth one with the `ignore` crate, already a dependency, and returning the non-ignored subdirectories plus root-level files. `target/`, `node_modules/`, and `.git/` are therefore never watched.

`SessionStart` is no longer the only chance to declare it. As of 2.1.263 `FileChanged` itself may return `watchPaths`, and `CwdChanged` and `DirectoryAdded` both exist and carry the same field, so the watched set can follow the agent instead of being fixed for the session. Recompute on those events rather than only at session start.

Two filters then run inside mcpls, in order:

1. The path resolves under a configured root and is not ignored by `.gitignore`.
2. The path matches at least one registered watcher glob, or has a routable extension.

A path passing neither is dropped without touching the tracker. This is what keeps a `cargo check` from filling `DocumentTracker` to its document ceiling with build artifacts, after which every real tool call would fail with `DocumentLimitExceeded`.

### Hook set

| Event | mcpls does | Injects context |
|---|---|---|
| `SessionStart` | returns `watchPaths`, snapshots the session baseline | no |
| `FileChanged` | `changed` for one path, may return revised `watchPaths` | no |
| `PostToolBatch` | `changed` for the batch's paths, then `flush` | yes |
| `UserPromptSubmit` | `flush` | yes |
| `SessionEnd` | drops the session record | no |

`Stop` is deliberately absent. Its output schema states that `additionalContext` is delivered to the model and the conversation continues so the model can act on it, so a `Stop` flush would turn every new warning into a keep-working signal and produce a warnings-driven auto-continue loop. Native injection never does this, and the next `UserPromptSubmit` flush delivers the same diagnostics anyway.

`PostToolBatch` rather than `PostToolUse`: it fires exactly once per batch, where `PostToolUse` runs concurrently for parallel tool calls and would put several hook processes on the socket at once. It also carries file paths, so it remains a working fallback if `FileChanged` is unavailable.

### Configuration

```toml
[diagnostics.hooks]
enabled = true
```

Defaulting on is safe here in a way it is not for the footer, because reaching this configuration at all means installing the plugin, and installing the plugin is the opt-in. With `enabled = false` the listener never binds and every hook exits 0 without output. There is no separate socket switch and no configurable socket path: the hook process does not read mcpls's config, since `MCPLS_CONFIG` lives in the MCP server's environment rather than the hook's, so a path it could not discover would be unusable.

### Working directory

The hook resolves its socket from `CLAUDE_PROJECT_DIR`, which is exported into both plugin and regular hook environments. Deriving it from the hook's own working directory would break as soon as the agent moves into a subdirectory.

mcpls's own working directory is set at spawn and never changes; a `cd` in the Bash tool happens in a different process. Workspace roots resolve and canonicalize at startup and are fixed for the process lifetime, so agent navigation cannot move them. A file outside every configured root gets no route, no diagnostics, and is refused by apply confinement.

### Failure is silent

No socket, a connect timeout of 50 ms, a malformed response, or any other fault: the hook exits 0 having printed nothing. An edit must never fail because diagnostics were unavailable.

### CLI and plugin layout

One hook subcommand, dispatching on the `hook_event_name` the payload already carries. Reading the payload from stdin and writing hook JSON to stdout means no shell script, and the same definitions work on Windows.

```
mcpls hook            # dispatches on hook_event_name from stdin
mcpls hook doctor     # socket path, both hashes, owner pid, liveness, PATH check
```

```
plugin/
  .claude-plugin/plugin.json
  .mcp.json                 registers mcpls as the MCP server
  hooks/hooks.json          the registrations above
  skills/mcpls/SKILL.md     moved from the repository's top-level skills/
```

Hooks invoke `mcpls` from `PATH`. If the hook process environment lacks the install directory, every hook silently does nothing, which the silent-failure rule makes invisible. `mcpls hook doctor` exists to answer that, and the plugin README leads with it.

### Testing

The socket gets integration tests over a temporary directory: bind, a second instance deferring to a live owner, a second instance taking over a stale socket file, a passive instance acquiring the socket after the owner exits, an unknown session's first flush returning nothing, and a changed file returned once and not twice.

The watcher filters get unit tests: a `target/` artifact dropped, a gitignored file dropped, a path matching a registered glob forwarded to that server and not to a server that did not register it.

Windows named pipes have no CI coverage, but Windows is a supported target, so the listener sits behind a small trait with the logic tested once and the transport verified by hand.

## Multiple agents in one project directory

Each session spawns its own mcpls over stdio. The first binds the socket and serves every session's hooks; the rest stay passive and retry. Per-session diagnostic records are keyed by the session id that every hook payload carries, so two agents get independent deduplication state from one warm set of servers, and neither sees the other's already-delivered diagnostics.

Apply is the exception, and it is inherited from the shipped part 1 rather than introduced here. The global apply mutex is per process, so two mcpls processes applying edits to the same file simultaneously can both pass phase one and the second wins. The staleness check narrows the window but does not close it. Two agents editing one file at the same moment is already unrecoverable regardless of mcpls, so this is documented rather than solved.

## Risks

- Advertising `didChangeWatchedFiles.dynamic_registration` changes what gopls and tsgo do at init. Both abandon their current behaviour on the strength of the advertisement. Land the advertisement and the notification together, in one commit.
- Claude Code's `FileChanged` watcher spawns a hook process per event and passes no ignore list. The `watchPaths` computation is the only thing bounding it, so it needs measuring on a repository mid-build rather than at rest.
- A1 changes what servers spawn for any config that already lists `[[lsp_servers]]`. That is intended, and it is the one change here that can surprise an existing config rather than only adding to it.
- Two rust-analyzer processes still exist if the host separately spawns one through a plugin `.lsp.json`. Setting `forwardDiagnostics: false` on such a server suppresses duplicate injection but not the duplicate process; removing the `.lsp.json` is the actual fix.

## Verification of host behaviour

The hook events, payload shapes, and native diagnostics behaviour described here were read out of the Claude Code binary rather than its documentation, and are version specific. To re-verify against a different version:

```fish
strings -n 4 ~/.local/share/claude/versions/<version> > /tmp/cc.strings
rg -o 'hook_event_name:C\("FileChanged"\).{0,300}' /tmp/cc.strings
rg -o 'hook_event_name:C\("PostToolBatch"\).{0,300}' /tmp/cc.strings
rg -o 'Hook-specific output for the Stop event.{0,200}' /tmp/cc.strings
rg -o 'Whether to push publishDiagnostics.{0,260}' /tmp/cc.strings
```

Verified against 2.1.263:

- `FileChanged` carries `{file_path, event}` where event is `change`, `add`, or `unlink`, and its output schema accepts `watchPaths`. `CwdChanged` and `DirectoryAdded` also carry `watchPaths`.
- `PostToolBatch` carries `tool_calls` and describes itself as firing exactly once after every tool call in a batch, where `PostToolUse` fires per tool and may run concurrently.
- `SessionStart` accepts `watchPaths`.
- Native injection is intact: a passive `textDocument/publishDiagnostics` handler accumulates publishes and flushes them into the next model request wrapped in `<new-diagnostics>`.
- Plugin `.lsp.json` is still supported, and `forwardDiagnostics` is documented as controlling whether `publishDiagnostics` is pushed into the agent context after edits, defaulting to true.

Language server behaviour was read from source checkouts rather than documentation: gopls at `gopls/internal/server/general.go:596`; typescript-go at `internal/lsp/server.go:1694-1713`; typescript-language-server at `src/lsp-server.ts:187`; vtsls at `packages/service/src/service/delegate.ts:49`; pyrefly 1.2.0 at `pyrefly/lib/lsp/non_wasm/server.rs:5811`; ty at `crates/ty_server/src/session.rs:901-962`; lua-language-server at `script/client.lua:595`; taplo and marksman by the absence of any match for the watched-files notification in `crates/taplo-lsp/src` and `Marksman/`.
