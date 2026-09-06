# Diagnostics injection

Status: designed, not implemented. Revised after an adversarial review that verified its claims against the source; the review's corrections are folded in.

Supersedes parts 2 through 4 of `2026-09-05-lsp-apply-and-diagnostics-hooks-design.md`. That document's part 1, applying edits, shipped; the rest is replaced by this one.

Target: the `AbysmalBiscuit/mcpls` fork, not upstream. Defaults are tuned for one user, and breaking changes to configuration are acceptable when they buy ergonomics.

Host behaviour was read out of the Claude Code binary rather than its documentation and is version specific. Verified against 2.1.263; the recipe for re-verifying is at the end.

## Problem

mcpls holds warm language servers with full workspace indexes and a live `publishDiagnostics` cache, and none of it reaches the agent unless the agent thinks to ask. The agent edits a file, moves on, and learns what it broke when something unrelated fails later.

Underneath that sits a second problem: mcpls tells its language servers about a file only when a tool call touches it. It accepts `client/registerCapability` and answers `null` (`lsp/client.rs:767`), so a server asking the client to watch files is met with silence.

And a third, found during review: applying an edit currently makes servers forget the files it wrote. `apply_locked` brackets the write with `forget_changed_documents` (`bridge/translator/mod.rs:300,308`), which sends `textDocument/didClose` for every changed path and drops the tracker entry (`mod.rs:361-374`). Nothing in the tree sends `didSave`; the only occurrence is the capability advertisement at `lsp/lifecycle.rs:668`. So the moment mcpls finishes a rename is the moment its servers stop knowing about the renamed files.

## Rejected alternative: plugin `.lsp.json`

Claude Code will spawn a language server itself through a plugin's `.lsp.json`, and its `diagnostics` key defaults to true, documented as controlling whether `publishDiagnostics` is pushed into the agent context after edits. That is this feature, for no code at all, and the design has to justify itself against it:

- It fires on `Edit` and `Write`, which call `changeFile` then `saveFile`. Every other writer produces nothing: `didChangeWatchedFiles` does not appear anywhere in the host bundle, and the file watcher it does have feeds hooks rather than its language servers. An external write never reaches a server it spawned.
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
- Runtime workspace root changes. Roots resolve from config at startup and are fixed for the process lifetime.
- Per-diagnostic deduplication. Per file is the chosen granularity, and the reasoning is in A2.

## Staging

Three stages. Each lands on main in a working state.

- **Stage A** makes configuration merge, then builds the deduplication core and a flush tool. No IPC, no plugin, no host-specific code. What it does not do is deliver anything automatically; see "What stage A actually delivers".
- **Stage B** replaces the forget-on-apply behaviour with a real resync, implements the `workspace/didChangeWatchedFiles` client half, and adds the footer on the tools that write. This is the stage where diagnostics start arriving without being asked for.
- **Stage C** adds the socket, the `mcpls hook` CLI, and the Claude Code plugin, so delivery becomes push rather than pull and covers writers outside the agent entirely.

## Stage A

### A1: configuration entries merge onto the built-ins

Writing any `[[lsp_servers]]` block today replaces the built-in server list wholesale. The test at `config/mod.rs:1003` pins the behaviour: a file with a single rust entry yields exactly one server, and the other built-ins are gone. Reaching one per-server key therefore costs every default server, which is why `handles` and `request_timeout_seconds` are effectively unreachable, and why a per-server diagnostics key would be unreachable too.

Entries merge onto the built-ins, keyed by `LspServerConfig::id()` (`config/server.rs:269`), which is `name` when set and `language_id` otherwise. That method is already the routing identity across `Translator`'s maps, with collision enforcement in `ToolRouter::from_configs`, so the merge introduces no new notion of identity.

Every field becomes individually optional. An absent field keeps the built-in's value. An entry whose id matches no built-in defines a new server, and there the fields with no default, `command` above all, are required; a partial entry matching nothing is a configuration error naming the id it failed to find.

Four semantics are settled explicitly rather than left to discover:

- `args = []` means empty arguments, not "unspecified". A partial entry distinguishes a present empty list from an absent key, so both are expressible.
- **An entry that overrides `command` inherits neither `args`, `env`, nor `initialization_options`.** Built-ins carry flags that belong to their own binary (`--stdio` for pyright and typescript-language-server, `serve` for gopls, `config/server.rs:317-347`). Swapping the command and inheriting the old flags spawns the new binary with arguments meant for a different program, and it fails silently at spawn time rather than at load time.
- `initialization_options` replaces rather than deep-merging. A deep merge of arbitrary JSON has no single right answer, and nothing needs one.
- An entry that adds a `name` to a built-in's `language_id` is a **new** server, not an override, because `id()` changed. It will collide with the built-in it was meant to modify, so that built-in must be disabled in the same file.

Dropping a built-in was free under replace semantics and needs a spelling under merge: `enabled = false` on an entry removes that server. This is a deliberate breaking change. A config that lists one server in order to suppress the rest must now disable the rest by name.

### A2: the deduplication core

A new module under `bridge`, not an addition to `notifications.rs`, which is already long enough that adding a second responsibility to it would be the wrong shape.

The core holds one record per session: a map from URI to a hash of that file's diagnostics. The hash covers a sorted set of `(range, severity, message)` rather than publish order, because `cap_diagnostics_entry_size` re-sorts survivors by severity when it truncates (`bridge/notifications.rs:218,260`), and an order-sensitive hash would report a change nobody made.

A flush returns every file whose hash differs from the record, then updates the record. A file that transitions to zero diagnostics is reported as a single line saying its problems are gone, which is cheap and is exactly the confirmation an agent wants after a fix.

Deduplication is per file rather than per diagnostic. This needs far less state, and when a file breaks, its full current error list is more useful than a delta against a list that has long since left the context window.

**Three interactions that must be specified together.** The hash is computed over the set that survives the severity floor, so raising a hint on a file whose floor is `error` does not re-report a file with nothing to show. "Problems are gone" means zero diagnostics at or above the floor, not zero diagnostics. And truncation against the volume caps must not advance the record for the files it dropped, or a capped flush silently swallows them forever.

**The baseline cannot be taken on first sight.** The obvious rule, snapshot the cache the first time a session appears and return nothing, does not work: language servers spawn in the background (`lib.rs:652-690`) and rust-analyzer publishes for the whole workspace once its initial analysis finishes, which is well after the first session op on any real workspace. A first-sight baseline is therefore empty, and every publish after it looks new, which is exactly the workspace dump the baseline exists to prevent.

Gate the baseline on servers settling instead. `diagnostics_pump` already receives `$/progress` and discards it (`lib.rs:236`); tracking end-of-progress per server gives the signal, and rust-analyzer names its tokens `rustAnalyzer/Indexing` and `rustAnalyzer/Flycheck`. A session's baseline is the cache as of the moment its servers went quiet.

**Stale publishes.** `diagnostics_pump` (`lib.rs:154`) already receives `p.version` and hands it to `store_diagnostics` (`bridge/notifications.rs:606`). It gains a `DocumentTracker` handle in `PumpShared` (`lib.rs:120-127`) so a publish whose version is below the tracked version is dropped rather than stored. Without that, a late publish describing pre-edit text overwrites the current entry, flips the file's hash, shows the agent errors for text that no longer exists, and flips back on the next publish.

Two rules keep the check from eating what it should pass. A publish carrying no version, or naming a path with no tracked entry, is stored rather than dropped: untracked fanout files are precisely what this feature exists to deliver, and rust-analyzer publishes for them without a version. And the check only bites once stage B stops closing documents on apply, because `close` removes the tracker entry (`bridge/translator/mod.rs:363`) and a reopen restarts at version 1, leaving nothing to compare a late version 7 against. Landing the check in stage A is still right; it is inert until B, and B is where it matters.

`DocumentTracker::documents` is a `StdMutex` held only for short synchronous sections (`bridge/state.rs:280-286`), so a version lookup from the pump cannot stall it.

### A3: the flush tool

Always present, no configuration. The agent calls it and gets everything new since its last flush.

The record is keyed by session id from the start rather than retrofitted in stage C. Claude Code exports `CLAUDE_CODE_SESSION_ID` into the environment of the stdio MCP servers it spawns, confirmed against a running process, so the in-process door and the later hook door key on the same value and share one record. Where that variable is absent, a per-process constant serves, which is correct for one-process-per-client stdio.

### Configuration

```toml
[diagnostics]
severity     = "warning" # off | error | warning | information | hint
max_per_file = 10
max_total    = 50
footer       = false     # stage B; see below

[[lsp_servers]]
language_id          = "rust"
diagnostics_severity = "error"   # overrides the global floor for this server
```

`off` mutes a server entirely, which is how a language is excluded. One scale does both jobs, so there is no second place to look when a language goes quiet.

The severity floor is the one knob that genuinely varies by server: rust-analyzer's clippy warnings, marksman's prose hints, and ty's inference notes are not the same signal, and wanting errors only from some of them is normal. A1 is what makes that per-server key reachable without abandoning the built-ins.

The caps stay global because they are one shared context budget. Per-server caps let three servers each spend the whole thing. `footer` stays global because it shapes a tool result, and a result belongs to a call rather than to a server.

Truncation against the caps is stated in the output rather than applied silently.

Stage C adds `[diagnostics.hooks]` under the same table.

### What stage A actually delivers

Not goal 1. The flush tool is `get_cached_diagnostics` with deduplication, and it fires only when the agent calls it. Stage A is worth landing for A1, which is useful on its own and unblocks every per-server key the config already has, and for the core plus the version fix, which everything after it needs. The spec says so rather than implying more.

The one place stage A could have delivered something automatic is rust-analyzer, the only configured server that watches the filesystem for itself. It does not. This was measured rather than assumed, and the result is in "What rust-analyzer does with an external write" below: its analysis stays current, but no compiler diagnostic is ever published. So stage A covers external writes to no language at all.

### Testing

The core is pure, so it takes unit tests directly: a changed file is returned once and not twice, a cleared file reports once, a publish below the tracked version is dropped while one with no version or no tracked entry is kept, the hash ignores sub-floor diagnostics, and caps truncate, say so, and leave the dropped files' records unadvanced.

A1 gets its own: a partial entry keeps the built-in's other fields, an entry overriding `command` drops the built-in's `args`, an entry with an unmatched id and no command is rejected naming the id, `enabled = false` removes a built-in, `args = []` produces empty arguments rather than the built-in's, and adding a `name` to a built-in `language_id` collides the way `ToolRouter::from_configs` already enforces.

End to end, `tests/ra_e2e.rs` already drives a real rust-analyzer against `tests/fixtures/rust_workspace`. It gains a test that breaks a caller and asserts the fanout file appears in a later flush.

## Stage B

### B1: stop forgetting the files an apply wrote

`apply_locked` closes every changed document and drops it from the tracker (`bridge/translator/mod.rs:300-374`). Replace that with a content-compared resync.

A path already tracked is re-read through `disk_phase` (`bridge/state.rs:594`), and if the content is unchanged, nothing is sent. This matters more than it looks: every `didSave` restarts rust-analyzer's flycheck, cancelling the `cargo check` in progress, so an unconditional resync after a twenty-file apply produces twenty cancel-and-restart cycles before any diagnostics land. When content did change, the document version bumps and every server holding it receives `didChange` then `didSave`, under the path lock (`bridge/state.rs:492`).

This is what makes A2's version check meaningful, and it is the precondition for the footer.

### B2: the `didChangeWatchedFiles` client half

Only some servers need this. Ten were checked against their own source at the commits `docm` resolves:

| Server | Watches for itself | Needs the client to watch |
|---|---|---|
| rust-analyzer | yes | no |
| gopls | opt-in only, off by default | yes |
| tsgo | Windows and macOS only | yes on Linux |
| typescript-language-server | yes, through tsserver | no |
| vtsls | yes, through tsserver | no |
| lua-language-server | yes | no |
| pyrefly | no | yes |
| ty | no | yes |
| taplo | no | ignores the notification |
| marksman | no | ignores the notification |

- **gopls** builds its watcher registrations in `registerWatchedDirectoriesLocked` (`gopls/internal/server/general.go:596`), whose first statement returns `nil` when `DynamicWatchedFilesSupported` is false. It has a server-side watcher behind the `fileWatcher` setting, off by default, so at default settings a client that does not watch leaves gopls seeing only the documents it is told about. It also unregisters the previous registration after registering the new one (`general.go:510-522`), so registrations replace rather than accumulate.
- **tsgo** chooses among three branches at init (`internal/lsp/server.go:1694-1713`): client-side watching when the client advertises `didChangeWatchedFiles.dynamicRegistration`; otherwise an in-process watcher, which its own comment limits to Windows and FSEvents; otherwise file watching disabled. On Linux and WSL2 that is the third branch.
- **typescript-language-server** gates client watching on `tsserver.useClientFileWatcher`, which defaults to false (`src/lsp-server.ts:183`, `docs/configuration.md:109`), and falls back to tsserver's own watching when the client cannot oblige.
- **vtsls** has no `didChangeWatchedFiles` registration or handler anywhere in its packages, so client watching cannot reach it; tsserver watches.
- **lua-language-server** watches through `bee.filewatch` (`script/filewatch.lua`) and never handles the notification.
- **pyrefly** registers `FileSystemWatcher` patterns with the client (`pyrefly/lib/lsp/non_wasm/server.rs:5811`); the `notify`-based watcher elsewhere in that codebase belongs to the CLI `check` command. **ty** gates registration on the same client capability (`crates/ty_server/src/session.rs:955-975`, inside ruff's checkout rather than ty's).
- **taplo** and **marksman** handle no watched-files notification at all. Nothing here changes that, and nothing can.

So the payoff is gopls at default settings, tsgo on Linux, pyrefly, and ty. That is narrower than it first looked and still worth building, since two of the four are the fork owner's likely second and third languages.

The servers configured beyond these ten (bash, yaml, json, css, dockerfile, latex, nushell) were not checked. Check them before claiming this stage covers them.

The implementation:

1. `client/registerCapability` for `workspace/didChangeWatchedFiles` stops being answered with a bare `null` (`lsp/client.rs:767`). The registration's `watchers` array, a glob pattern plus a change-kind bitmask, is stored per server and keyed by registration id. `client/unregisterCapability` drops it.
2. `workspace.did_change_watched_files.dynamic_registration = true` is advertised, which also moves gopls and tsgo out of their do-nothing branches.
3. When mcpls learns a path changed, it notifies every server whose registered globs match that path and whose bitmask includes the event kind.

**The tripwire is already in the tree.** `test_client_capabilities_do_not_claim_dynamic_file_watching` (`lsp/lifecycle.rs:933`) asserts that mcpls does not advertise this capability, with the reason inline: advertising without sending the notification blinds gopls and tsgo. Stage B is the change that flips it, and the advertisement and the notification must land in the same commit.

Before stage C there is no watcher, so B's inputs are the paths mcpls already learns about: the targets of an apply, and the files a tool call touches. That alone is what tells gopls about the files a rename rewrote.

### B3: the footer

`rename_symbol`, `format_document`, and `apply_code_action` append new diagnostics to their own results, because those calls just changed the tree and the answer is wanted immediately. Off by default. Read tools never carry a footer: an agent asking `get_hover` what type something is should not receive forty lines of unrelated compiler errors.

The footer belongs here rather than in stage A because before B1 an apply ends by telling every server to forget the files it wrote, so a footer would have nothing to report.

A footer fires immediately after a resync, and the diagnostics for that resync have not arrived yet. It therefore waits, bounded and briefly, for each changed path to publish at or above its new tracked version, and reports what it has when the bound expires. Without the wait the footer reports the previous edit's diagnostics, which is worse than reporting none.

## Stage C

### Socket identity

The socket path derives from a canonicalized directory hash: `$XDG_RUNTIME_DIR/mcpls/<hash>.sock` on Linux with a `/tmp/mcpls-<uid>` fallback where that variable is unset, `$TMPDIR` on macOS, `\\.\pipe\mcpls-<hash>` on Windows. `tokio` is already on `features = ["full"]`, so both transports exist without a new dependency.

The two sides reach that directory differently, and this is checked against a live process rather than assumed. A running mcpls spawned as a stdio MCP server has `CLAUDE_CODE_SESSION_ID` in its environment but **not** `CLAUDE_PROJECT_DIR`; its working directory is the project directory. The hook has `CLAUDE_PROJECT_DIR`. So mcpls hashes its own canonicalized startup working directory and the hook hashes `CLAUDE_PROJECT_DIR`, and the two agree because the host spawns stdio MCP servers in the project directory.

That agreement is a property of the host, not a guarantee, which is why `mcpls hook doctor` prints both hashes: a config whose roots point at a subdirectory, a multi-root config, or a symlinked checkout would otherwise produce a permanent silent no-op with nothing to look at. mcpls's own working directory is fixed at spawn, so a `cd` in the agent's shell cannot move it.

The first mcpls instance in a project binds. A later instance becomes passive and retries every 5 seconds, so an owner exiting does not strand it. A connect attempt arbitrates: it succeeds when a listener holds the socket, and fails with `ECONNREFUSED` on a stale file. Two passive instances can race to unlink and rebind a stale path, so binding goes to a temporary name and moves into place with `rename(2)`, which is atomic, rather than unlink-then-bind, which is not.

A passive instance's flush tool forwards to the owner over the socket rather than reading its own record, so one session has one record no matter which process the agent is talking to.

### Protocol

Newline-delimited JSON. A connection carries one or more requests and closes when the client is done.

```
{"op":"changed","session":"<id>","paths":["/abs/src/x.rs"],"event":"change"}
{"op":"flush","session":"<id>"}
{"op":"status"}
```

`changed` runs the filters below and B1's resync, and does not flush. `flush` drains and does not resync. `PostToolBatch` sends `changed` then `flush` on one connection. `status` serves `doctor`.

Every op answers within a deadline of a second or two, whether or not it has finished the work behind it, because a flush can queue behind a path lock held by a long apply and the host's default hook timeout is 600 seconds. The deadline is the hook's protection, not the host's.

### Bounding the watched set

Claude Code has a `FileChanged` hook event backed by a real file watcher, delivering `{file_path, event}` where event is `change`, `add`, or `unlink`. That covers every writer, which is the point.

Its watcher passes no ignore list, so the watched set has to be constrained where it is declared, by returning `watchPaths` from `SessionStart`. mcpls builds that list by walking each workspace root to depth one with the `ignore` crate, already a dependency, and returning the non-ignored subdirectories plus root-level files. `target/`, `node_modules/`, and `.git/` are therefore never watched.

`FileChanged` may also return a revised `watchPaths`, but this design does not use that. Workspace roots are fixed for the process lifetime, so there is nothing for a revision to track: recomputing after a `CwdChanged` would either name paths outside the roots, which filter 1 drops anyway, or narrow the set and lose coverage of the rest of the root.

Two filters then run inside mcpls, in order:

1. The path resolves under a configured root and is not ignored by `.gitignore`.
2. The path matches at least one registered watcher glob, or has a routable extension.

A path passing neither is dropped without touching the tracker. This is what keeps a `cargo check` from filling `DocumentTracker` to its document ceiling with build artifacts, after which every real tool call would fail with `DocumentLimitExceeded`.

### What a change does

Changes arrive in bursts, so they are collected rather than acted on one at a time. A path enters a pending set and the sweep runs once the set has been quiet briefly. This is not an optimisation: every `didSave` restarts rust-analyzer's flycheck and cancels the check in flight, so a `cargo fmt` forwarded one path at a time produces a run of cancelled checks and no diagnostics at all.

The sweep then treats a path by what its server needs:

- **A tracked path** takes B1's resync, whose content comparison drops the paths that did not really change.
- **An untracked path routed to a server whose diagnostics come from a build** must be opened and saved, because naming it is not enough. rust-analyzer already knows what the file says and still will not check it without a `didSave`. This costs a tracker slot, which is what the filters above exist to protect.
- **An untracked path routed to a server that wanted the notification** is reported through `workspace/didChangeWatchedFiles` and costs no slot.

Paths arriving from tool inputs are bounded by what the agent actually edited, so they are opened if routable and a first-touch file still produces diagnostics.

`add` maps to a watched-file create event, `unlink` to a delete event plus `didClose` if the path was tracked.

### Hook set

| Event | mcpls does | Injects context |
|---|---|---|
| `SessionStart` | returns `watchPaths` | no |
| `FileChanged` | `changed` for one path | no |
| `PostToolBatch` | `changed` for the batch's paths, then `flush` | yes |
| `UserPromptSubmit` | `flush` | yes |
| `SessionEnd` | drops the session record | no |

`SessionStart` no longer snapshots the baseline: A2 gates that on servers settling, which is a signal mcpls owns and the hook cannot observe. This also removes a race, since the hook and the MCP server start concurrently and the hook can reach a socket that is not yet bound.

`Stop` is deliberately absent. Its output schema states that `additionalContext` is non-error feedback delivered to the model and the conversation continues so the model can act on it, so a `Stop` flush would turn every new warning into a keep-working signal and produce a warnings-driven auto-continue loop. The next `UserPromptSubmit` flush delivers the same diagnostics anyway.

`PostToolBatch` rather than `PostToolUse`: it fires exactly once per batch, where `PostToolUse` runs concurrently for parallel tool calls and would put several hook processes on the socket at once. It also carries file paths, so it remains a working fallback if `FileChanged` is unavailable.

### Configuration

```toml
[diagnostics.hooks]
enabled = true
```

Defaulting on is safe here in a way it is not for the footer, because reaching this configuration at all means installing the plugin, and installing the plugin is the opt-in. With `enabled = false` the listener never binds and every hook exits 0 without output. There is no separate socket switch and no configurable socket path: the hook process does not read mcpls's config, since `MCPLS_CONFIG` lives in the MCP server's environment rather than the hook's, so a path it could not discover would be unusable.

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

The socket gets integration tests over a temporary directory: bind, a second instance deferring to a live owner, a second instance taking over a stale socket file, two instances racing for a stale path with exactly one winner, a passive instance acquiring the socket after the owner exits, a passive instance's flush reaching the owner's record, and an op answering within its deadline while the work behind it is still running.

The watcher filters get unit tests: a `target/` artifact dropped, a gitignored file dropped, a path matching a registered glob forwarded to that server and not to a server that did not register it.

Windows named pipes have no CI coverage, but Windows is a supported target, so the listener sits behind a small trait with the logic tested once and the transport verified by hand.

## Multiple agents in one project directory

Each session spawns its own mcpls over stdio. The first binds the socket and serves every session's hooks; the rest stay passive and retry. Per-session records are keyed by `CLAUDE_CODE_SESSION_ID`, which both the hook payload and the MCP server's environment carry, so two agents get independent deduplication state from one warm set of servers and neither sees the other's already-delivered diagnostics.

If the worktree-per-session habit holds, the passive path is rarely exercised, and a plain bind-or-log would do. It is specified because getting it wrong is silent.

Apply is the exception, inherited from the shipped part 1 rather than introduced here. The global apply mutex is per process, so two mcpls processes applying edits to the same file simultaneously can both pass phase one and the second wins. Two agents editing one file at the same moment is already unrecoverable regardless of mcpls, so this is documented rather than solved.

## What rust-analyzer does with an external write

Measured against rust-analyzer 1.98.1 with a minimal LSP client advertising the same capabilities mcpls does today, driving a two-file scratch crate. The method is a script, not a tool call: spawn the server, let it settle, write the file with a plain shell redirect, send nothing, and watch.

**Its watcher sees the change.** A function appended to a file with no notification of any kind was returned by `workspace/symbol` 35 seconds later, having been absent before the write. rust-analyzer's virtual file system is current without help.

**It does not check the change.** Sixty seconds after an external write introducing an `E0308`, there were zero `publishDiagnostics` and `rust-analyzer/flycheck/0` never began. The control run proves the setup: `didOpen` for the same erroneous text published an empty list, and the following `didSave` began flycheck and delivered the three `E0308` diagnostics 90 milliseconds later.

Two consequences the design has to carry:

- **A watched-files notification would not help, even if rust-analyzer accepted one.** It already knows. What it wants is a `didSave`. So for Rust, and for any server whose diagnostics come from a build rather than from resident analysis, an externally changed file needs mcpls to open it and save it, not merely to name it. That costs a tracker slot per file, which is why stage C's filters matter.
- **A burst of external writes must be debounced into one save.** Every `didSave` restarts flycheck and cancels the `cargo check` in flight, so forwarding a `cargo fmt` across fifty files as fifty saves produces fifty cancelled checks and no diagnostics. One save after the burst settles, per changed file, with a short quiet period before the sweep.

The same measurement should be repeated for any server later added to the routing table whose diagnostics come from a build step rather than from resident analysis.

## Open questions

- **Does a range in the hash make edits above a diagnostic re-report it?** Hashing `(range, severity, message)` re-reports a file whenever an edit shifts an unrelated diagnostic's line. Hashing `(severity, code, message, line text)` would suppress that at the cost of conflating two identical messages on different lines. Native injection appears to have the same property, so start with the range and measure.
- **HTTP transport.** `transport-http` exists behind a default-off feature. One process per client holds for stdio only; the record would need rmcp's session id there.

## Risks

- Advertising `didChangeWatchedFiles.dynamic_registration` changes what gopls and tsgo do at init. Both abandon their current behaviour on the strength of the advertisement. Land the advertisement and the notification together, in one commit.
- Replacing forget-on-apply with a resync changes what servers see after every apply, which is the shipped feature's most-used path. The content comparison is what keeps it from cancelling flycheck repeatedly, and it needs the e2e test to prove it.
- Claude Code's `FileChanged` watcher spawns a hook process per event and passes no ignore list. The `watchPaths` computation is the only thing bounding it, so it needs measuring on a repository mid-build rather than at rest.
- Opening externally changed files to get build diagnostics consumes tracker slots against `workspace.max_documents`, and a wide external change can consume many at once. The filters and the debounce bound it; a sweep that would exceed the ceiling should report that rather than fail the next unrelated tool call with `DocumentLimitExceeded`.
- A1 changes what servers spawn for any config that already lists `[[lsp_servers]]`. That is intended, and it is the one change here that can surprise an existing config rather than only adding to it.
- Two rust-analyzer processes still exist if the host separately spawns one through a plugin `.lsp.json`. Setting `diagnostics: false` on such a server suppresses duplicate injection but not the duplicate process; removing the `.lsp.json` is the actual fix.

## Verification of host behaviour

The hook events, payload shapes, and native diagnostics behaviour described here were read out of the Claude Code binary rather than its documentation, and are version specific. To re-verify against a different version:

```fish
strings -n 4 ~/.local/share/claude/versions/<version> > /tmp/cc.strings
rg -o 'hook_event_name:C\("FileChanged"\).{0,300}' /tmp/cc.strings
rg -o 'hook_event_name:C\("PostToolBatch"\).{0,300}' /tmp/cc.strings
rg -o 'Hook-specific output for the Stop event.{0,200}' /tmp/cc.strings
rg -o '.{0,60}Whether to push publishDiagnostics.{0,200}' /tmp/cc.strings
rg -o 'CLAUDE_CODE_SESSION_ID.{0,120}' /tmp/cc.strings
```

What a spawned MCP server actually receives is a different question from what the bundle mentions, and only the first one matters here. Ask the process:

```fish
for pid in (pgrep -f 'bin/mcpls')
    tr '\0' '\n' < /proc/$pid/environ | rg 'SESSION_ID|PROJECT_DIR'
    readlink /proc/$pid/cwd
end
```

Verified against 2.1.263:

- `FileChanged` carries `{file_path, event}` where event is `change`, `add`, or `unlink`, and its output schema accepts `watchPaths`. `CwdChanged` accepts it too. `DirectoryAdded` exists but has no output schema and no `watchPaths`.
- `PostToolBatch` carries `tool_calls` and describes itself as firing exactly once after every tool call in a batch, where `PostToolUse` fires per tool and may run concurrently. Its output accepts `additionalContext`.
- `SessionStart` accepts `watchPaths`.
- `Stop`'s `additionalContext` is documented as non-error feedback delivered to the model, after which the conversation continues so the model can act on it.
- `CLAUDE_PROJECT_DIR` is exported into hook environments, plugin and regular alike.
- A live stdio MCP server's `/proc/<pid>/environ` carries `CLAUDE_CODE_SESSION_ID` matching the session that spawned it, and does **not** carry `CLAUDE_PROJECT_DIR`. Its working directory is the project directory. Re-check this the same way rather than from the bundle, since it is what the socket path and the session key both rest on.
- Native injection is intact: a passive `textDocument/publishDiagnostics` handler accumulates publishes and flushes them into the next model request wrapped in `<new-diagnostics>`.
- Plugin `.lsp.json` is still supported. Its key is `diagnostics`, not `forwardDiagnostics`; the consumer tests `config.diagnostics === false`. `forwardDiagnostics` exists in the bundle as an unrelated cloud-runner method and is not this setting.
- `didChangeWatchedFiles` does not appear in the bundle at all, so the host's own language servers never learn about external writes.
- The hook file watcher does not reject glob patterns; nothing validates the matcher beyond dropping UNC paths. `watchPaths` is still the only mechanism bounding it.

Language server behaviour was read from source checkouts resolved by `docm`, not from documentation: gopls at `gopls/internal/server/general.go:510-522,596` and its `fileWatcher` setting; typescript-go at `internal/lsp/server.go:1694-1713`; typescript-language-server at `src/lsp-server.ts:183` and `docs/configuration.md:109`; vtsls by the absence of any `didChangeWatchedFiles` match under its packages; pyrefly at `pyrefly/lib/lsp/non_wasm/server.rs:5811`; ty at `crates/ty_server/src/session.rs:955-975` inside ruff's checkout; lua-language-server at `script/filewatch.lua`; taplo and marksman by the absence of any match for the watched-files notification.
