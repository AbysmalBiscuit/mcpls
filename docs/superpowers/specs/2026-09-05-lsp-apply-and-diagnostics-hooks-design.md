# LSP edit application and diagnostics injection

Status: part 1 (applying edits) shipped in a63cc14..f7d8ffa. Parts 2 through 4 are
superseded by `2026-09-06-diagnostics-injection-design.md` and are kept here only as
the record they were designed from.

Target: `AbysmalBiscuit/mcpls` fork, not upstream

## Problem

mcpls exposes LSP capabilities as MCP tools but never writes anything. `rename_symbol`, `format_document`, and `get_code_actions` return descriptions of edits, and the agent reproduces those edits by hand with `Edit`. That discards the reason to use LSP at all: rust-analyzer's rename is scope-aware, while an agent working from `rg` output hits shadowed bindings, string literals, comments, and unrelated same-named symbols in other modules. The deterministic answer exists and is being thrown away.

Separately, mcpls holds warm language servers with full workspace indexes and a live `publishDiagnostics` cache, and none of it reaches the agent unless the agent thinks to ask. Claude Code injects diagnostics natively after edits, but only for language servers it spawns itself, which means running a second copy of every server mcpls already owns.

Underneath both sits a third problem, found while designing the second: mcpls tells its language servers about a file only when a tool call touches it. It accepts `client/registerCapability` and then ignores it (`lsp/client.rs:663`), so a server that asks the client to watch files is answered with silence.

## Goals

1. Let configured tools apply their edits to the working tree, defaulting to read-only.
2. Keep language servers informed of file changes they would otherwise miss.
3. Push diagnostics into the agent's context after any file change, from the warm servers mcpls already owns, without a second set of servers and without stalling edits.

## Non-goals

- Upstreaming. This is fork-only; defaults are tuned for one user.
- A standalone mcpls daemon surviving Claude Code restarts. The MCP process keeps owning the language servers. The `mcpls hook` CLI surface is designed so this stays possible later without changing the plugin.
- Runtime workspace root changes (`/add-dir`, `DirectoryAdded`). Roots resolve from config at startup, as today.
- Cross-process serialization of concurrent applies. See "Multiple agents".

## Part 1: applying edits

### Configuration

New `[apply]` table, absent by default, so existing configs behave exactly as before.

```toml
[apply]
rename              = true    # default false
format_document     = true    # default false
code_actions        = false   # default false
allow_file_deletion = false   # gates delete operations for all of the above
```

Named fields rather than `HashMap<ToolKind, Permission>`: only three of the fifteen `ToolKind` variants can mutate anything, so a map would need validation purely to reject meaningless keys like `hover = "apply"`, and `deny_unknown_fields` (already set on `ServerConfig`) gives better typo errors on a struct than on a map. Booleans rather than a `"read_only" | "apply"` enum because no third state exists.

### Client capabilities

`lsp/lifecycle.rs:460` currently advertises `WorkspaceClientCapabilities` with `workspace_folders: Some(true)` and nothing else. Applying requires three more, declared together:

- `workspace.apply_edit = true`
- `workspace.workspace_edit.document_changes = true`
- `workspace.workspace_edit.resource_operations = [Create, Rename, Delete]`

plus `text_document.synchronization.did_save = true`, since the resync path sends `didSave` and nothing in the current capabilities says so.

Advertising `resource_operations` changes read-only behavior too. rust-analyzer gates file-system edits on it, so a module rename that returns text-only today starts returning `DocumentChangeOperation::Op` entries. `handle_rename` currently filters those out (`bridge/translator/edits.rs:226`), which would show a preview missing the file rename the apply then performs. `WorkspaceEditDescription` therefore gains a resource-operations field, and preview and apply read the same normalized structure.

### Tool surface

`rename_symbol` and `format_document` gain an optional `apply: bool`, default `false`. With `apply: true` and the matching config key `false`, the call fails with an error naming the config key rather than silently previewing.

`get_code_actions` returns a list, so `apply: true` alone is ambiguous. Applying a code action is a separate call taking a selector:

```
apply_code_action(file_path, range, action: <index | exact title>)
```

It re-issues `textDocument/codeAction` for the same range, selects by the selector, `codeAction/resolve`s the chosen action, and applies the resulting edit. Stateless, so no pending-edit cache and no staleness rules of its own. `data_support` and `resolve_support.properties = ["edit"]` are already advertised (`lsp/lifecycle.rs:431-435`); the `CodeAction` DTO gains the `data` field that resolve requires, which it currently drops.

Some servers deliver an assist as a command rather than an edit, requiring `workspace/executeCommand` and answering through an inbound `workspace/applyEdit`. That path is specified below and is not optional: gopls, vtsls, typescript-language-server, and lua-language-server all send it, so four of the configured servers reach code actions this way.

### The applier

New module `bridge/apply.rs`. One entry point takes an `lsp_types::WorkspaceEdit` and returns a summary of what it wrote. All apply-enabled calls serialize on a single process-wide `tokio::Mutex`, which closes the phase-one/phase-two race between two concurrent applies and defines the window in which an inbound `applyEdit` is honored.

**Normalization.** A `WorkspaceEdit` arrives as `changes` (a plain URI map), as `document_changes: Edits`, or as `document_changes: Operations` with resource operations interleaved among text edits. All three collapse into one ordered operation list. Resource operations run in array order, as the spec requires. Text edits within a single file apply bottom-up, because every range refers to the original document. Two edits with overlapping ranges in one file are a hard error, never a merge.

**Encoding.** `bridge/encoding.rs:192` already provides `EncodingConverter::character_to_byte_offset` for UTF-8, UTF-16, and UTF-32 with boundary checks, and `EncodingCtx::to_lsp` is the inward direction. The applier reuses both. The genuinely new piece is a document-level line table, built by scanning for `\n` rather than by `str::lines()`, which drops a trailing empty line and strips `\r`. Lines keep their `\r`, and a position on the line after the final terminator is valid.

**Staleness.** Tracked and untracked targets differ and the distinction is explicit. `prepare_gated_document` opens only the queried file, so a rename's other targets are usually untracked and the server read them from disk. A tracked target requires disk content to equal what `DocumentTracker` holds; drift refuses the whole edit. An untracked target applies against current disk content. To make refusals rare, every tracked document belonging to the routed server is stat-swept through `disk_phase` before the LSP request is issued, so the server computes against fresh text.

**Confinement.** Reuse `validate_path_against_roots` (`bridge/translator/routing.rs:30`), which canonicalizes and prefix-checks against the configured roots. A create operation targets a path that does not exist yet, so it canonicalizes the parent and joins the file name. Delete operations additionally require `allow_file_deletion`.

**Atomicity.** Two phases. Phase one walks the operation list in array order against an overlay of what the tree holds at each point, seeded from disk on first touch, and emits a journal of reversible steps: write a file, move a path, or move a path aside. Nothing touches disk until the whole list has validated. The overlay is what makes ordering meaningful: an edit to a file an earlier operation in the same plan created reads the overlay rather than the disk, which is the shape rust-analyzer's "create module" assist emits. Phase two performs the journal in order.

A write goes through a temp file plus rename. The temp file copies the original's mode bits, is named so it matches no source-file glob (a dot-prefixed name with a non-source suffix), and the rename targets the symlink-resolved path so a symlinked file is not replaced by a regular file. A delete is a rename to a sibling trash path, purged only once every step has succeeded, so rollback can restore a whole directory tree without ever having held it in memory. A move refuses a destination that already exists, because `std::fs::rename` replaces it silently on Unix and fails on Windows; a plan that means to replace one emits a trash step for the destination first.

If a step fails, the completed steps reverse in order. The error names every file in each of three exhaustive states: still holding its new content, restored to its original, or sitting at neither location with that location spelled out. A failed restore is itself a temp-and-rename, so it changed nothing and the file belongs in the first group, not in a vaguer "unknown" one.

**Tracker updates.** The applier holds `DocumentTracker::lock_path` (`bridge/state.rs:483`) for each path across its write, because `update` is documented as racing `ensure_open` (`bridge/state.rs:381`). A renamed document is closed under its old path and left untracked under the new one, to be re-tracked by the next `ensure_open`; its `NotificationCache` entry under the old URI is dropped. A deleted document is closed and its cache entry dropped.

**Resync.** After a successful write, every server holding the document receives `didChange` followed by `didSave`. `didSave` is what triggers rust-analyzer's flycheck, so applying an edit feeds the diagnostics queue in part 3.

**Return value.** Per-file edit counts and the resource operations performed, and a note that the agent's cached view of those files is now stale. No unified diff: diffs are expensive in tokens and the agent re-reads what it needs.

### Inbound `workspace/applyEdit`

`lsp/client.rs:672` answers this request with `{"applied": false}` from `server_request_result`, a synchronous function with no access to translator state.

Awaiting an applier response inline in `message_loop_inner` would deadlock: that loop is a single `select!` over the command channel and the transport, and the applier's own resync sends notifications through that same command channel, which has capacity 100. A rename touching 51 files fills it, the applier blocks, and the loop that would drain it is parked awaiting the applier.

So the loop never awaits. A new `ClientCommand::SendResponse` lets a spawned task answer later, and the request is handed to that task while the loop keeps polling. The task forwards the edit to the applier through a sink:

```rust
type ApplySink = mpsc::Sender<(WorkspaceEdit, oneshot::Sender<bool>)>;
```

The sink's `Sender` is installed only while the global apply mutex is held by a code-action apply, which is what makes the in-flight window rule fall out for free: outside that window there is no sink and the answer stays `{"applied": false}`. Without that rule a language server could write to the working tree at a moment of its own choosing. The response is sent after the write and the tracker update, before the resync.

## Part 2: file watching

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

The two that decide it at runtime deserve their reasoning spelled out, because both land badly under mcpls as it stands:

- **gopls** builds its watcher registrations in `registerWatchedDirectoriesLocked` (`gopls/internal/server/general.go:596`), whose first statement returns `nil` when `DynamicWatchedFilesSupported` is false. There is no in-process fallback. A client that does not watch leaves gopls seeing only the documents it is told about.
- **tsgo** chooses among three branches at init (`internal/lsp/server.go:1694-1713`): client-side watching when the client advertises `didChangeWatchedFiles.dynamicRegistration`; otherwise an in-process watcher when the backend supports fast recursive watching, which its own comment limits to Windows and FSEvents; otherwise `"file watching: disabled"`. On Linux and WSL2 that is the third branch.

ty and typescript-language-server both gate registration on the same client capability (`crates/ty_server/src/session.rs:958`, `src/lsp-server.ts:187`). pyrefly registers `FileSystemWatcher` patterns with the client (`pyrefly/lib/lsp/non_wasm/server.rs:5811`); the `notify`-based watcher elsewhere in that codebase belongs to the CLI `check` command.

taplo and marksman handle no watched-files notification at all, so they see only what document synchronization tells them. Nothing in this design changes that, and nothing can.

So mcpls implements the client half of `workspace/didChangeWatchedFiles`:

1. `client/registerCapability` for `workspace/didChangeWatchedFiles` stops being answered with a bare `null` (`lsp/client.rs:663`). The registration's `watchers` array (glob pattern plus a change kind bitmask) is stored per server, keyed by registration id. `client/unregisterCapability` drops it. gopls re-registers as its watched directory set grows, incrementing its own registration id, so registrations accumulate per server rather than replacing one another.
2. `workspace.did_change_watched_files.dynamic_registration = true` is advertised, which also moves gopls and tsgo out of their do-nothing branches.
3. When mcpls learns a path changed, it sends `workspace/didChangeWatchedFiles` to every server whose registered globs match that path and whose bitmask includes the event kind.

Advertising the capability without implementing the notification would be worse than today: gopls and tsgo would both abandon what they do now and hear nothing in return.

### Where change events come from

Claude Code has a `FileChanged` hook event backed by a real file watcher, delivering `{file_path, event}` where event is `change`, `add`, or `unlink`. That covers every writer, not just the `Edit` tool: `sed -i`, `cargo fmt`, ad-hoc python scripts, `git checkout`, an editor open in another window.

Its watcher passes no ignore list, so the watched set has to be constrained where it is declared. `SessionStart` returns `watchPaths` built by walking each workspace root to depth one with the `ignore` crate (already a dependency) and returning the non-ignored subdirectories plus root-level files. `target/`, `node_modules/`, and `.git/` are therefore never watched. Glob matchers in `hooks.json` cannot substitute for this: the bundled watcher rejects glob patterns, and hooks run for every event without per-path matching.

Two filters then run inside mcpls, in order, before anything else happens:

1. The path resolves under a configured root and is not ignored by `.gitignore`.
2. The path matches at least one registered watcher glob, or has a routable extension.

A path passing neither is dropped without touching the tracker. This is what keeps a `cargo check` from filling `DocumentTracker` to its 100-document ceiling (`bridge/state.rs:239`) with build artifacts, after which every real tool call would fail with `DocumentLimitExceeded`.

### What a change does

A path already tracked is re-read through `disk_phase` (`bridge/state.rs:585`), and if the content is unchanged, nothing is sent. This matters more than it looks: every `didSave` restarts rust-analyzer's flycheck, cancelling the `cargo check` in progress, so an unconditional resync after a twenty-file apply produces twenty cancel-and-restart cycles before any diagnostics land. When content did change, the document version bumps and every server holding it receives `didChange` then `didSave`, under the path lock.

A path not tracked is never opened by the watcher path. It is reported through `workspace/didChangeWatchedFiles` to the servers that asked for it, which is the mechanism those servers wanted, and it costs no tracker slot. Paths arriving from `PostToolBatch` tool inputs are different: they are bounded by what the agent actually edited and are opened if routable, so a first-touch file still produces diagnostics.

`add` maps to a watched-file create event, `unlink` to a delete event plus `didClose` if the path was tracked.

## Part 3: diagnostics delivery

### Socket ownership

The socket path derives from a hash of mcpls's canonicalized startup working directory: `$XDG_RUNTIME_DIR/mcpls/<hash>.sock` on Linux with a `/tmp/mcpls-<uid>` fallback for systems where that variable is unset, `$TMPDIR` on macOS, `\\.\pipe\mcpls-<hash>` on Windows. `tokio` is already on `features = ["full"]`, so both transports exist without a new dependency.

The startup working directory, not `workspace.roots[0]`, because the hook hashes `CLAUDE_PROJECT_DIR` and the two must agree. A config with `roots` pointing at a subdirectory, a multi-root config, or a symlinked checkout would otherwise produce a permanent silent no-op. Claude Code spawns stdio MCP servers in the project directory, so the two match. `mcpls hook doctor` prints both hashes.

The first mcpls instance in a workspace binds. A later instance gets `EADDRINUSE` and becomes passive, retrying the bind every 5 seconds so that an owner exiting does not strand it for the rest of its session. A connect attempt arbitrates on its own: it succeeds when a listener holds the socket and fails with `ECONNREFUSED` on a stale file, which is then unlinked.

### Protocol

Newline-delimited JSON, one request per connection, connection closes after the response.

```
{"op":"changed","session":"<id>","paths":["/abs/src/x.rs"],"event":"change"}
{"op":"flush","session":"<id>"}
{"op":"status"}
```

`changed` runs the filters and resync from part 2 and does not flush. `flush` drains and does not resync. `PostToolBatch` sends `changed` then `flush` on one connection. `status` serves `doctor`.

### Delivery and deduplication

`NotificationCache` already holds the latest diagnostics per server and URI. It gains a per-session record mapping URI to a hash of that file's diagnostic set, hashed over a sorted set of `(range, severity, message)` rather than publish order, because `store_diagnostics` re-sorts survivors by severity when it truncates (`bridge/notifications.rs:596`). A flush returns every file whose hash differs from the record, then updates the record.

A session seen for the first time, at any op, snapshots the current cache as its baseline and returns nothing. Otherwise the first flush of every session would dump the whole workspace's pre-existing warnings, and so would the first flush after an owner restart.

A file transitioning to zero diagnostics is reported as a single line saying its problems are gone, which is cheap and useful, rather than silently recorded.

The pump (`lib.rs:178-199`) already receives `p.version` and passes it to `store_diagnostics`. It gains the tracker handle so a publish whose version is below the tracked version is dropped rather than stored. Without that, a late publish for pre-edit text overwrites the current entry, the per-file hash flips, the agent is shown diagnostics for text that no longer exists, and the hash flips back on the next publish. Claude Code drops these natively for the same reason.

This deduplication is per file, coarser than Claude Code's per diagnostic. The tradeoff is deliberate: per-file needs far less state, and when a file breaks, its full current error list is more useful than a delta against a list no longer in context.

### Caps and configuration

```toml
[hooks.diagnostics]
enabled      = true      # default true: installing the plugin is the opt-in
languages    = []        # empty means every configured server
severity     = "warning" # error | warning | information | hint
max_per_file = 10
max_total    = 50
```

Unlike `[apply]`, diagnostics injection defaults on. Installing the plugin is the opt-in, and a plugin whose hooks do nothing until a config file is written would look broken. With `enabled = false` the listener never binds and every hook exits 0 without output. There is no separate socket switch and no configurable socket path: the hook process does not read mcpls's config (`MCPLS_CONFIG` lives in the MCP server's environment, not the hook's), so a path it cannot discover would be unusable.

Truncation against the caps is stated in the output rather than applied silently.

### Hook set

| Event | mcpls does | Injects context |
|---|---|---|
| `SessionStart` | returns `watchPaths`, snapshots the session baseline | no |
| `FileChanged` | `changed` for one path | no |
| `PostToolBatch` | `changed` for the batch's paths, then `flush` | yes |
| `UserPromptSubmit` | `flush` | yes |
| `SessionEnd` | drops the session record | no |

`Stop` is deliberately absent. Its output schema states that `additionalContext` is "delivered to the model; the conversation continues so the model can act on it", so a Stop flush would convert every new warning into a keep-working signal and produce a warnings-driven auto-continue loop. Native injection never does this; the next `UserPromptSubmit` flush delivers the same diagnostics.

`PostToolBatch` rather than `PostToolUse`: it fires exactly once per batch, where `PostToolUse` runs concurrently for parallel tool calls and would put several hook processes on the socket at once. It also carries file paths, so it remains a working fallback if `FileChanged` is unavailable.

### Working directory

The hook resolves its socket from `CLAUDE_PROJECT_DIR`, which is exported into both plugin and regular hook environments. Deriving it from the hook's own working directory would break as soon as the agent `cd`s into a subdirectory.

mcpls's own working directory is set at spawn and never changes; a `cd` in the Bash tool happens in a different process. Workspace roots resolve and canonicalize at startup and are fixed for the process lifetime, so agent navigation cannot move them. A file outside every configured root gets no route, no diagnostics, and is refused by apply confinement.

### Failure is silent

No socket, connect timeout (50 ms), malformed response, or any other fault: the hook exits 0 having printed nothing. An edit must never fail because diagnostics were unavailable. Claude Code's default hook timeout is 600 s, so hook latency is bounded by this timeout, not by the host's.

## Multiple agents in one project directory

Each Claude Code session spawns its own mcpls over stdio. The first binds the socket and serves every session's hooks; the rest stay passive and retry. Per-session diagnostic records are keyed by the `session_id` that every hook payload carries, so two agents get independent deduplication state from one warm set of servers, and neither sees the other's already-delivered diagnostics.

Apply is the exception. The global apply mutex is per process, so two mcpls processes applying edits to the same file simultaneously can both pass phase one and the second wins. The staleness check narrows the window but does not close it. Two agents editing one file at the same moment is already unrecoverable regardless of mcpls, so this is documented rather than solved; closing it would mean an advisory lock file per target path and a cross-platform locking dependency.

## Part 4: packaging

### CLI

One hook subcommand, dispatching on the `hook_event_name` the payload already carries. Reading the payload from stdin and writing hook JSON to stdout means no shell script, and the same definitions work on Windows.

```
mcpls hook            # dispatches on hook_event_name from stdin
mcpls hook doctor     # socket path, both hashes, owner pid, liveness, PATH check
```

### Plugin layout

```
plugin/
  .claude-plugin/plugin.json
  .mcp.json                 registers mcpls as the MCP server
  hooks/hooks.json          the five registrations above
  skills/mcpls/SKILL.md     moved from the repository's top-level skills/
```

The existing skill moves in so that installing the plugin brings it along.

Hooks invoke `mcpls` from `PATH`. If the hook process environment lacks the install directory, every hook silently does nothing, which the silent-failure design makes invisible. `mcpls hook doctor` exists to answer that, and the plugin README leads with it.

## Testing

The applier's normalization is pure and gets unit tests: three `WorkspaceEdit` shapes collapsing to one operation list, bottom-up ordering within a file, overlapping ranges rejected, UTF-16 offsets against a non-ASCII fixture, CRLF line endings, a position on the line after the final terminator, a symlink escaping a workspace root rejected, version drift refused, mode bits preserved across the temp-file rename, and rollback after a mid-write failure.

The watcher path gets unit tests for its two filters: a `target/` artifact dropped, a gitignored file dropped, a path matching a registered glob forwarded to that server and not to a server that did not register it.

The socket gets integration tests over a temporary directory: bind, a second instance deferring to a live owner, a second instance taking over a stale socket file, a passive instance acquiring the socket after the owner exits, an unknown session's first flush returning nothing, and a changed file returned once and not twice.

End to end, `tests/ra_e2e.rs` already drives a real rust-analyzer against `tests/fixtures/rust_workspace`. It extends to: rename a symbol with `apply: true` and assert the on-disk files changed; a module rename producing a file rename operation, to confirm the capability change works and that preview and apply agree; break a caller and assert the fanout file appears in a later flush.

One behavior still needs probing against a live server rather than assuming: whether rust-analyzer runs flycheck on a change it learned about from its own watcher rather than from `didSave`. If it does not, an external write to a Rust file produces no compiler diagnostics until a tool call opens it.

The `workspace/applyEdit` path is exercised end to end against gopls rather than rust-analyzer, since rust-analyzer resolves its assists into edits and never takes that path.

Windows named pipes have no CI coverage, but Windows is a supported target, so the listener sits behind a small trait with the logic tested once and the transport verified by hand.

## Risks

- Position encoding conversion is the highest-consequence code in the applier. A wrong offset silently corrupts a file rather than failing loudly. It reuses the existing tested converter for exactly this reason.
- Advertising `didChangeWatchedFiles.dynamic_registration` changes what gopls and tsgo do at init. Both stop doing what they do today on the strength of that advertisement: gopls starts registering watchers it currently skips, tsgo abandons the in-process fallback it uses on Windows and macOS. An incomplete notification side leaves both worse off than before. Land the advertisement and the notification together.
- Claude Code's `FileChanged` watcher spawns a hook process per event and passes no ignore list. The `SessionStart` `watchPaths` computation is the only thing bounding it, so it needs measuring on a repository mid-build rather than at rest.
- Applying edits makes the agent's cached view of a file stale. The tool result says so explicitly.
- Two rust-analyzer processes still exist if Claude Code separately spawns one through a plugin `.lsp.json`. Setting `forwardDiagnostics: false` on such a server suppresses duplicate injection but not the duplicate process; removing the `.lsp.json` is the actual fix.

## Verification of host behavior

The hook events, payload shapes, and native diagnostics behavior described here were read out of the Claude Code binary rather than documentation, and are version-specific. To re-verify against a different version:

```fish
strings -n 4 ~/.local/share/claude/versions/<version> > /tmp/cc.strings
rg -o 'hook_event_name:C\("FileChanged"\).{0,300}' /tmp/cc.strings
rg -o 'Hook-specific output for the Stop event.{0,200}' /tmp/cc.strings
rg -o 'Whether to push publishDiagnostics.{0,200}' /tmp/cc.strings
```

Verified against 2.1.261:

- `Edit` and `Write` call `changeFile` then `saveFile`, both fire-and-forget with a `.catch` that only logs. Nothing is awaited and the tool result does not wait.
- `saveFile` sends `textDocument/didSave`, which is what makes rust-analyzer run flycheck.
- `publishDiagnostics` handlers drop stale publishes by document version, accumulate the rest, and flush them into the next model request as a `<new-diagnostics>` attachment, deduplicated and volume-capped.
- Plugins spawn language servers through `.lsp.json`, whose schema includes a per-server `forwardDiagnostics` toggle documented as suppressing automatic diagnostic injection while keeping navigation.
- `PostToolBatch` input carries `tool_calls: [{tool_name, tool_input, tool_use_id, tool_response?}]` and fires once per batch; `PostToolUse` may run concurrently across parallel calls.
- `FileChanged` input carries `{file_path, event}` where event is `change`, `add`, or `unlink`; hooks declare coverage by returning `watchPaths`; the watcher is created with no ignore list and rejects glob patterns.
- `additionalContext` is accepted from `PostToolBatch`, `UserPromptSubmit`, `Stop`, `SessionStart`, and others, but not from `FileChanged`, whose only output is `watchPaths`. On `Stop` it continues the conversation. Language server behavior was read from source checkouts, not documentation. gopls at `gopls/internal/server/general.go:596` and `gopls/internal/server/command.go:991`; typescript-go at `internal/lsp/server.go:1694-1713`; typescript-language-server at `src/lsp-server.ts:187` and `src/lsp-server.ts:1065`; vtsls at `packages/service/src/service/delegate.ts:49`; pyrefly 1.2.0 at `pyrefly/lib/lsp/non_wasm/server.rs:5811`; ty at `crates/ty_server/src/session.rs:901-962` and `crates/ty_server/src/server/api/requests/execute_command.rs`, whose only supported command returns debug text; lua-language-server at `script/client.lua:595`; taplo and marksman by the absence of any match for the watched-files notification in `crates/taplo-lsp/src` and `Marksman/`.
