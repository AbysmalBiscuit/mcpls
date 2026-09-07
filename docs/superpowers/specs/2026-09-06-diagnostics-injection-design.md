# Diagnostics injection

Status: stage A shipped; stages B and C designed, not implemented.

Revised twice after adversarial review, each round verifying claims against the source rather than against the previous draft. The second round found that B3's original wait could not fire for the case it existed to serve, that B1's split between committing a version and sending the notification would desync a server permanently on a cancelled request, that a footer flushing before the baseline landed would flood the session, that stage C's socket arbitration let two instances both believe they owned it, and that the footer's cap was shorter than this repository's own `cargo check`. Those are fixed here. Where a claim now rests on a measurement, the measurement is named so it can be re-run.

Supersedes parts 2 through 4 of `2026-09-05-lsp-apply-and-diagnostics-hooks-design.md`. That document's part 1, applying edits, shipped; the rest is replaced by this one.

Target: the `AbysmalBiscuit/mcpls` fork, not upstream. Defaults are tuned for one user, and breaking changes to configuration are acceptable when they buy ergonomics.

Host behaviour was read out of the Claude Code binary rather than its documentation and is version specific. Verified against 2.1.263; the recipe for re-verifying is at the end.

## Problem

mcpls holds warm language servers with full workspace indexes and a live `publishDiagnostics` cache, and none of it reaches the agent unless the agent thinks to ask. The agent edits a file, moves on, and learns what it broke when something unrelated fails later.

Underneath that sits a second problem: mcpls tells its language servers about a file only when a tool call touches it. It accepts `client/registerCapability` and answers `null` (`lsp/client.rs:762`), so a server asking the client to watch files is met with silence.

And a third, found during review: applying an edit currently makes servers forget the files it wrote. `apply_locked` brackets the write with `forget_changed_documents` (`bridge/translator/mod.rs:294,302`), which sends `textDocument/didClose` for every changed path and drops the tracker entry (`mod.rs:361-383`). Nothing in the tree sends `didSave`; the only occurrence is the capability advertisement at `lsp/lifecycle.rs:683`. So the moment mcpls finishes a rename is the moment its servers stop knowing about the renamed files.

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

- **Stage A** makes configuration merge, then builds the deduplication core and a flush tool. No IPC, no plugin, no host-specific code. What it does not do is deliver anything automatically; see "What stage A actually delivers". Shipped.
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

**The baseline cannot be taken on first sight.** The obvious rule, snapshot the cache the first time a session appears and return nothing, does not work: language servers spawn in the background (`lib.rs:974`) and rust-analyzer publishes for the whole workspace once its initial analysis finishes, which is well after the first session op on any real workspace. A first-sight baseline is therefore empty, and every publish after it looks new, which is exactly the workspace dump the baseline exists to prevent.

Gate the baseline on servers settling instead. `diagnostics_pump` already receives `$/progress` and discards it (`lib.rs:194-200`); tracking end-of-progress per server gives the signal, and rust-analyzer names its tokens `rustAnalyzer/Indexing` and `rustAnalyzer/Flycheck`. A session's baseline is the cache as of the moment its servers went quiet.

**Stale publishes.** `diagnostics_pump` (`lib.rs:162`) already receives `p.version` and hands it to `store_diagnostics` (`bridge/notifications.rs:606`). It gains a `DocumentTracker` handle in `PumpShared` (`lib.rs:120-135`) so a publish whose version is below the tracked version is dropped rather than stored. Without that, a late publish describing pre-edit text overwrites the current entry, flips the file's hash, shows the agent errors for text that no longer exists, and flips back on the next publish.

Two rules keep the check from eating what it should pass. A publish carrying no version, or naming a path with no tracked entry, is stored rather than dropped: untracked fanout files are precisely what this feature exists to deliver, and rust-analyzer publishes for them without a version. And the check only bites once stage B stops closing documents on apply, because `close` removes the tracker entry (`bridge/translator/mod.rs:363`) and a reopen restarts at version 1, leaving nothing to compare a late version 7 against. Landing the check in stage A is still right; it is inert until B, and B is where it matters.

What the check cannot catch, after B1, is worth stating so a later reader does not try to "fix" it: a server that stamps a publish with the document version current at send time rather than at analysis time produces a publish carrying the new version while describing the old text, and that passes the check. The version check bounds late publishes, not stale analysis. The footer's progress wait is what covers the rest.

`DocumentTracker::documents` is a `StdMutex` held only for short synchronous sections (`bridge/state.rs:280-282`), so a version lookup from the pump cannot stall it.

### A3: the flush tool

`get_new_diagnostics` (`mcp/server.rs:736`). Always present, no configuration. The agent calls it and gets everything new since its last flush. Every later mention of "the flush" in this document means this tool, or the socket op that shares its record.

The record is keyed by session id from the start rather than retrofitted in stage C. Claude Code exports `CLAUDE_CODE_SESSION_ID` into the environment of the stdio MCP servers it spawns, confirmed against a running process, so the in-process door and the later hook door key on the same value and share one record. Where that variable is absent, a per-process constant serves, which is correct for one-process-per-client stdio.

### Configuration

```toml
[diagnostics]
severity            = "warning" # off | error | warning | information | hint
max_per_file        = 10
max_total           = 50
settle_quiet_ms     = 1000      # quiet needed before the baseline is taken
settle_deadline_ms  = 300000    # backstop when a server reports no progress
footer              = false     # stage B; see below
footer_grace_ms     = 250       # stage B
footer_quiet_ms     = 200       # stage B
footer_wait_ms      = 15000     # stage B

[[lsp_servers]]
language_id          = "rust"
diagnostics_severity = "error"   # overrides the global floor for this server
```

`off` mutes a server entirely, which is how a language is excluded. One scale does both jobs, so there is no second place to look when a language goes quiet.

The severity floor is the one knob that genuinely varies by server: rust-analyzer's clippy warnings, marksman's prose hints, and ty's inference notes are not the same signal, and wanting errors only from some of them is normal. A1 is what makes that per-server key reachable without abandoning the built-ins.

The caps stay global because they are one shared context budget. Per-server caps let three servers each spend the whole thing. `footer` stays global because it shapes a tool result, and a result belongs to a call rather than to a server.

Truncation against the caps is stated in the output rather than applied silently.

Stage C adds `[diagnostics.hooks]` under the same table. `DiagnosticsConfig` is `Copy` with `deny_unknown_fields` (`config/mod.rs:134-167`), so the nested hooks struct is `Copy` too. Reaching for `Clone` instead would turn every `config.diagnostics` copy into a move and ripple through call sites that have nothing to do with this feature.

### What stage A actually delivers

Not goal 1. The flush tool is `get_cached_diagnostics` with deduplication, and it fires only when the agent calls it. Stage A is worth landing for A1, which is useful on its own and unblocks every per-server key the config already has, and for the core plus the version fix, which everything after it needs. The spec says so rather than implying more.

The one place stage A could have delivered something automatic is rust-analyzer, the only configured server that watches the filesystem for itself. It does not. This was measured rather than assumed, and the result is in "What rust-analyzer does with an external write" below: its analysis stays current, but no compiler diagnostic is ever published. So stage A covers external writes to no language at all.

### Testing

The core is pure, so it takes unit tests directly: a changed file is returned once and not twice, a cleared file reports once, a publish below the tracked version is dropped while one with no version or no tracked entry is kept, the hash ignores sub-floor diagnostics, and caps truncate, say so, and leave the dropped files' records unadvanced.

A1 gets its own: a partial entry keeps the built-in's other fields, an entry overriding `command` drops the built-in's `args`, an entry with an unmatched id and no command is rejected naming the id, `enabled = false` removes a built-in, `args = []` produces empty arguments rather than the built-in's, and adding a `name` to a built-in `language_id` collides the way `ToolRouter::from_configs` already enforces.

End to end, `tests/ra_e2e.rs` already drives a real rust-analyzer against `tests/fixtures/rust_workspace`. It gains a test that breaks a caller and asserts the fanout file appears in a later flush.

## Stage B

### B1: stop forgetting the files an apply wrote

`apply_locked` closes every changed document and drops it from the tracker (`bridge/translator/mod.rs:294-383`). Replace that with a resync.

The drain keeps the shape `forget_changed_documents` has, because the reasoning behind that shape survives unchanged: the loop runs inside a cancellable request future, so a path leaves the queue only once its work is provably finished. What changes is that "finished" is now three messages rather than one, which is the whole difficulty of this task and is treated below.

The queue holds only paths the apply wrote (`Planner::paths_invalidated`, `bridge/apply/mod.rs:173`, `:441-445`). Each is stat'd rather than assumed, and falls into one of three cases:

- **Absent from disk**, which a rename or a delete produces: close it and drop the tracker entry, exactly as today. Once B2 lands, this is also a watched-file delete event.
- **Present and tracked**: resync it, per the rules below.
- **Present and untracked**: left alone. Naming it to the servers that asked to watch is B2's job, and opening it is stage C's.

**The resync sends `didChange` then `didSave`, per server, and the tracker records both.** `DocumentState` gains `saved: HashMap<ServerId, i32>` beside `synced` (`bridge/state.rs:129`), because a `didChange` without a `didSave` produces no flycheck run and therefore no compiler diagnostic, and the two messages can be interrupted between.

The split between the tracker and the translator has to be exact, since the translator holds `lsp_clients` and the tracker owns the state:

1. The tracker reads disk, compares content, and on a difference commits the new version and content through `commit_reload`, the same call `sync_phase` makes (`bridge/state.rs:841`). It returns the target version and two lists: the servers whose `synced` is behind it, and the servers whose `saved` is behind it.
2. The translator sends each server's `didChange`, then each server's `didSave`.
3. After each individual notification is accepted, the translator calls back into the tracker to advance that one server's `synced` or `saved`, and the tracker performs the generation re-check `sync_phase` already performs under the `documents` lock (`bridge/state.rs:853`) so a respawn racing the resync cannot leave a fresh process marked as caught up.

**Marking every server synced at commit time would be a permanent desync.** A dropped request future between the commit and the notify would leave `synced == version` while that server still holds pre-apply text; the next `ensure_open` computes `up_to_date` and returns without sending anything (`bridge/state.rs:766`), so the server serves stale content for the life of the process and the next edit computed against it splices into text that no longer means what the server meant. That is exactly the corruption forget-on-apply exists to prevent, which is why the marking is per server and follows its own notification.

**A path leaves the queue only when both lists come back empty.** On a re-drain after a cancellation the content now matches disk, so a content comparison alone would say "nothing to do" and silently drop the unsent `didSave`. The comparison is not the completion test; the two lists are. This is also what makes the re-drain idempotent, which is the comparison's real job.

**The comparison does not save the apply from a save cascade, and the earlier claim that it did was wrong.** Every path in this queue was written by the apply, so a tracked one differs by construction and the identical arm is unreachable for it outside a no-op edit. What bounds a twenty-file apply is that most of those twenty are untracked and are left alone. The tracked ones do each get a `didSave`, and rust-analyzer restarts flycheck on each, cancelling the check in flight. The saves land within milliseconds of each other and the last restart runs to completion, so the cost is a few aborted checks rather than a lost result. That is the argument; the content comparison is not.

**The resync always reads.** `disk_phase` (`bridge/state.rs:607`) has two fast paths that trust a matching stat (`:625-636`), and the queue is itself proof the file was written. A same-length rename landing inside `DISK_CHECK_DEBOUNCE` on a filesystem with coarse mtime would take a fast path and send nothing. The new tracker method reads unconditionally rather than reusing that judgment.

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
| pyrefly | no | for currency, not delivery |
| ty | no | yes |
| taplo | no | ignores the notification |
| marksman | no | ignores the notification |

- **gopls** builds its watcher registrations in `registerWatchedDirectoriesLocked` (`gopls/internal/server/general.go:596`), whose first statement returns `nil` when `DynamicWatchedFilesSupported` is false. It has a server-side watcher behind the `fileWatcher` setting, off by default, so at default settings a client that does not watch leaves gopls seeing only the documents it is told about. It also unregisters the previous registration after registering the new one (`general.go:510-522`), so registrations replace rather than accumulate.
- **tsgo** chooses among three branches at init (`internal/lsp/server.go:1694-1713`): client-side watching when the client advertises `didChangeWatchedFiles.dynamicRegistration`; otherwise an in-process watcher, which its own comment limits to Windows and FSEvents; otherwise file watching disabled. On Linux and WSL2 that is the third branch.
- **typescript-language-server** gates client watching on `tsserver.useClientFileWatcher`, which defaults to false (`src/lsp-server.ts:183`, `docs/configuration.md:109`), and falls back to tsserver's own watching when the client cannot oblige.
- **vtsls** has no `didChangeWatchedFiles` registration or handler anywhere in its packages, so client watching cannot reach it; tsserver watches.
- **lua-language-server** watches through `bee.filewatch` (`script/filewatch.lua`) and never handles the notification.
- **pyrefly** registers `FileSystemWatcher` patterns with the client (`pyrefly/lib/lsp/non_wasm/server.rs:5811`); the `notify`-based watcher elsewhere in that codebase belongs to the CLI `check` command. Measured against pyrefly 1.2.0, it acts on the notification but publishes only for documents it holds open, so watching buys it analysis currency for open documents rather than diagnostic delivery for unopened ones: `docs/superpowers/notes/2026-09-07-stage-b-measurements.md`, "Pyrefly on an unopened file". **ty** gates registration on the same client capability (`crates/ty_server/src/session.rs:955-975`, inside ruff's checkout rather than ty's).
- **taplo** and **marksman** handle no watched-files notification at all. Nothing here changes that, and nothing can.

So the payoff is gopls at default settings, tsgo on Linux, pyrefly, and ty. That is narrower than it first looked and still worth building, since two of the four are the fork owner's likely second and third languages.

The servers configured beyond these ten (bash, yaml, json, css, dockerfile, latex, nushell) were not checked. Check them before claiming this stage covers them.

The implementation:

1. `client/registerCapability` for `workspace/didChangeWatchedFiles` stops being answered with a bare `null` (`lsp/client.rs:762`). The registration's `watchers` array, a glob pattern plus a change-kind bitmask, is stored per server and keyed by registration id. `client/unregisterCapability` drops it.
2. `workspace.did_change_watched_files.dynamic_registration = true` is advertised, which also moves gopls and tsgo out of their do-nothing branches.
3. When mcpls learns a path changed, it notifies every server whose registered globs match that path and whose bitmask includes the event kind.

The registry is its own module beside the client rather than state inside it, so the matching logic is unit-testable without a live server. It is keyed by server, then by registration id, holding compiled glob matchers paired with their change-kind bitmask. A watcher whose `kind` is absent means all three kinds, which is what the protocol says. `globset` arrives transitively through `ignore` and becomes a direct dependency, and every pattern is compiled with `literal_separator(true)` and matched against the absolute path: globset's default lets `*` cross a `/`, and LSP's glob grammar does not.

**Where the registry lives is most of this task's work.** One `Arc<WatchRegistry>` is built in `serve_with` beside `notification_cache` (`lib.rs:665`) and reaches two places that have no connection to each other today:

- The **client** side writes it. `server_request_result` (`client.rs:762`) is called from `server_request_response`, which runs on a task spawned by `spawn_server_request_responder` (`:690-709`) from inside the message loop. So the `Arc` becomes a field on `LspClient` beside `config` (`:80`), is threaded through the loop's parameters and the responder spawn, and arrives with the client's own `ServerId` (`config.id()`). The client is constructed inside `LspServer::spawn_batch` (`lifecycle.rs:628`) from `ServerInitConfig`, so the `Arc` travels in `ServerInitConfig`.
- The **translator** side reads it, because it is what sends `didChangeWatchedFiles`. It gets the same `Arc` through its builder.

`respawn_if_dead` clears the dead server's registrations, in the same place it calls `document_tracker.forget_server` (`translator/respawn.rs:281`). A respawned process re-registers with fresh ids, so without the clear the old globs linger forever and a server that narrowed its watch keeps being told about files it no longer wants.

**What stage B feeds it.** Only the apply's own targets, from the same queue B1 drains. Not "every file a tool call touches": telling gopls that a file changed because `get_hover` opened it is noise, and for a file that server had never seen it is a lie. The queue carries bare paths, so the kind comes from the same stat B1 makes, not from the apply summary, which a cancellation loses. Absent is `Deleted`; present is `Changed`. A file the apply created is therefore reported as `Changed` rather than `Created`, which every server tolerates and which costs nothing to get right later by widening the queue to carry a kind.

**`relativePatternSupport` is deliberately not claimed.** LSP lets a watcher's pattern be either a plain glob string or a `RelativePattern` carrying its own base URI, and claiming support invites the second form. Not claiming it means every registration arrives as a string, which is one matching path instead of two. A server that sends a relative pattern anyway has its watcher logged and skipped, so the failure is a line in the log rather than silence. If gopls or ty turn out to send them regardless, that log line is the signal to add the branch.

**The tripwire is already in the tree.** `test_client_capabilities_do_not_claim_dynamic_file_watching` (`lsp/lifecycle.rs:955`) asserts that mcpls does not advertise this capability, with the reason inline: advertising without sending the notification blinds gopls and tsgo. Stage B is the change that flips it, and the advertisement and the notification must land in the same commit.

Before stage C there is no watcher, so the apply's targets are the only input. That alone is what tells gopls about the files a rename rewrote.

### B3: the footer

`rename_symbol`, `format_document`, and `apply_code_action` append new diagnostics to their own results, because those calls just changed the tree and the answer is wanted immediately. Off by default. Read tools never carry a footer: an agent asking `get_hover` what type something is should not receive forty lines of unrelated compiler errors.

The footer belongs here rather than in stage A because before B1 an apply ends by telling every server to forget the files it wrote, so a footer would have nothing to report.

**A footer consumes.** It runs the same flush `get_new_diagnostics` runs, against the same per-session record, so what it shows is marked delivered and the next `UserPromptSubmit` flush stays quiet about it. One report per problem. The alternative, reading without advancing, spends the context budget twice on every write tool call to insure against an agent that ignores its own tool result.

**It runs under the same guard the tool runs under.** `get_new_diagnostics` returns `starting_up()` without calling `flush` while `has_baseline()` is false (`mcp/server.rs:737-743`), and that guard is load-bearing rather than cosmetic: `flush` seeds a session's record from `baseline.clone().unwrap_or_default()` (`bridge/delivery.rs:162-165`), and `set_baseline` does not rewrite records that already exist. A footer that flushed before the baseline landed would create an empty record permanently, and the next flush would then report every warning in the workspace. Since the settle deadline is 300 seconds, the first rename of a session is squarely inside that window. So a footer is silent while `has_baseline()` is false, and it says nothing rather than saying "still starting up".

**Where it lives.** The flush machinery is on `McplsServer`'s context, and `apply_locked` is on `Translator`, which cannot reach any of it. The footer therefore runs in the `#[tool]` handlers after the translator call returns, and its clock starts there rather than at the resync. Results are serialised as JSON (`mcp/server.rs:54-62`), so "append" means a field, not trailing text: one wrapper type at the MCP layer carrying `#[serde(flatten)] result: T` plus `#[serde(skip_serializing_if = "Option::is_none")] new_diagnostics: Option<NewDiagnosticsResult>`, used at all three call sites. That leaves the bridge's own DTOs untouched and keeps `NewDiagnosticsResult` where it already is.

A footer fires only when the call actually wrote. `RenameResult::applied` (`bridge/translator/dto.rs:118`) is the test; a rename with `apply: false` changed nothing and has nothing to report.

**The wait is on progress, not on document versions.** A footer fires right after a resync and the diagnostics for that resync have not arrived yet, so without a wait it reports the previous edit's state, which is worse than reporting none. Waiting for each changed path to publish at or above its new tracked version does not work, but not for the reason first written here: rust-analyzer stamps a publish for an open document with that document's version, so the versions do arrive. What breaks the version wait is that a rename which introduces no problem publishes nothing at all for those paths, so every clean footer would sit until its cap. That claim is worth pinning rather than trusting; see the measurement note below.

So the footer waits on the same `$/progress` signal the baseline uses, in three parts:

1. Hold for `footer_grace_ms` (250) before looking at anything. The measurement below puts flycheck's start about 90 ms after `didSave`, and a footer that checks before then sees a quiet workspace and reports the pre-edit state.
2. Then wait for quiet: nothing outstanding, held for `footer_quiet_ms` (200). Only progress that **began after the resync** counts, so an indexing run already in flight when the rename landed does not eat the whole cap.
3. Give up at `footer_wait_ms` (15000) and report whatever has landed.

**Quiet has to be defined for a workspace that never reports progress.** `should_settle_at` requires `quiet_since` to have been stamped, and `quiet_since` is `None` until the first `end` ever arrives (`bridge/settle.rs:31-34`, `:110-113`). Under that judgment a workspace whose servers send no `$/progress` at all is never quiet, so every footer would burn the full cap. The footer's own judgment is therefore `outstanding.is_empty() && quiet_since.map_or(true, |since| now - since >= quiet)`: after the grace period, nothing outstanding and nothing ever reported counts as quiet. That is a second method on `ServerSettle`, not a change to the baseline's.

**`footer_quiet_ms` is not `settle_quiet_ms`, and reusing it was a mistake.** The one-second debounce exists because rust-analyzer's *startup* phases hand off to each other through gaps of 70 to 100 milliseconds. A footer never sees that; flycheck publishes its diagnostics before its own `end`. What the footer's quiet has to bridge is the cancel-and-restart between two `didSave`s landing back to back, which is far shorter. Paying a full second on every footer, fast or slow, buys nothing.

**Why the cap is 15 seconds rather than 5.** Measured on this repository with the command rust-analyzer's default flycheck runs, `cargo check --workspace --all-targets`, after a no-op touch, which is the floor for a real edit: about 4.7 seconds for a touch in `mcpls-core` (4.65, 4.71, 4.72, 5.10 across four runs) and about 0.3 seconds for one in `mcpls-cli`. A 5 second cap would therefore expire on every rename in the crate that holds most of the code, and report the pre-edit state, which is the worst of both behaviours. Because the wait is gated on progress rather than on a timer, a high cap costs nothing when the check is fast: a `mcpls-cli` rename still returns in about 0.6 seconds. Re-run the measurement when the codebase grows; the numbers drift with it.

The footer is best effort by construction and says so in its own text. Anything slower than the cap arrives in the next flush instead.

### B4: three stage-A findings that stop being cosmetic

Stage A shipped with a register of deferred findings. Three of them are harmless while a flush is something the agent occasionally asks for and are not harmless once a hook flushes after every tool batch. They land in stage B, ahead of the traffic that exposes them.

**The flush deep-clones the whole diagnostics cache.** `diagnostics_snapshot` (`bridge/notifications.rs:734`) clones every key, every `DiagnosticInfo`, and every owner id, bounded at 1000 entries of up to 1 MiB each, and the caller then reduces that to a hash or a small changed set. It is written that way for the reason its own doc gives: the diagnostics pump needs the same lock, and the caller goes on to await.

That constraint is real and the fix has to respect it. Both `notification_cache` and `delivery` are `tokio::sync::Mutex` (`mcp/server.rs:18`), so there is no std guard in play; what matters is that `new_diagnostics_payload` awaits `Translator::diagnostics_from_cache_entry` once per changed file (`mcp/server.rs:789-794`). Holding the cache guard across those awaits would block the pump on `notification_cache.lock()`, and the LSP transport forwards with `try_send` and drops on a full channel, so under a hook flushing after every tool batch that is silently lost `publishDiagnostics`. Borrowing all the way through the payload build is therefore worse than the clone, not better.

The shape that works: lock `delivery`, then the cache; build the `FileEntry` values borrowing from the cache guard; run `flush` under both; clone only the `(uri, owner)` pairs the report's `changed` and `cleared` keys name; release both guards; then build the payload. `flush` still clones each changed file's admitted diagnostics into `ChangedFile`, which is the output and is bounded by the caps. `new_diagnostics_payload`'s signature changes to take that small map rather than the full snapshot.

**Cleared files escape both volume caps.** `flush`'s cleared arm pushes unconditionally, so a wide apply that fixes many files can emit up to the tracker's whole document count in "problems are gone" lines while the changed files beside them are budgeted to 50. Each cleared file costs one unit of the total budget. When the budget runs out, the remaining cleared files keep their record entry and count into `report.omitted`, so the next flush offers them again. That is the deferral rule the changed files already follow, applied to the other arm. Both arms spend from the budget in the one key-ordered pass `flush` already makes, so which files a binding budget reaches stays as reproducible as the sort made it, rather than depending on cleared files being counted before or after changed ones.

**A muted file is reported as fixed.** The first shape of this finding said to recompute the baseline hash under the current owner's floor, which is not implementable and not the bug: `flush` already resolves the floor at flush time through `routable_entries`, and what the record holds is a hash with no diagnostics behind it to recompute from. The reachable failure is narrower. When a path's diagnostics rebind to a server whose floor is `off`, `visible_hash` returns `None` against a recorded hash, the `(None, Some(_))` arm fires (`bridge/delivery.rs:176-179`), and the agent is told the file's problems are gone when they were only muted. Any other floor change reports as a change, which is defensible. So: an entry whose floor is `Off` removes its record entry without reporting `cleared`. One arm, one test.

### Testing

B1 gets the e2e it needs, in `tests/ra_e2e.rs`, and the fixture has to be arranged for it. `add` lives in `src/lib.rs` and nothing outside that file calls it, so the existing rename rewrites one file and, being successful, produces no error at all. Neither property is what this test needs.

Arrange the collision instead. Add `pub fn sum(a: i32, b: i32) -> i32` to `src/lib.rs` beside `add`, give `src/functions.rs` a caller of `add`, then rename `add` to `sum`. rust-analyzer does not check a rename for conflicts, so the apply lands and rustc reports `E0428`, a duplicate definition. The assertion is on `source == "rustc"` and code `E0428`, not merely on `lib.rs` turning up in `changed`. That distinction is the test's whole value: rust-analyzer publishes its own resident diagnostics on `didChange` alone, so a rename that changed a call site's arity would satisfy a looser assertion with the `didSave` half of the resync entirely broken. Giving the sibling the same signature is what keeps the resident diagnostics out of it, leaving only a diagnostic that requires a completed build.

The `saved` bookkeeping gets unit tests directly, since the e2e cannot see it: a resync reports both lists, marking one server synced leaves it still needing a save, and a re-drain of a path whose content now matches disk still reports the servers that were never saved.

B2's four beneficiaries are none of them rust-analyzer, so the existing e2e cannot prove it, and the beneficiary to prove it with has to be chosen on a measurement rather than on the registration table. **Measure pyrefly first**, the way the rust-analyzer section below was measured: drive it with a minimal client, send `workspace/didChangeWatchedFiles` for a file that was never opened, and watch for a publish. The B2 table says pyrefly needs client watching because it *registers* watchers, which is not the same claim as publishing diagnostics for a file it holds no document for. If it publishes, it gets a fixture workspace and a gated e2e where an apply through mcpls breaks an untracked Python file and a later flush finds it. If it does not, then B2 buys pyrefly analysis currency rather than delivery, the spec should say exactly that, and the e2e goes to gopls, which does publish for unopened workspace files.

Note also what the e2e harness can and cannot do: it speaks MCP (`tests/e2e/mcp_client.rs`), so no test can "send only `workspace/didChangeWatchedFiles`". In stage B the only trigger is an apply target, so the test is always an apply plus a flush.

The registry gets unit tests underneath: a registration stored and matched, an unregister dropping it, a respawn clearing a server's registrations, a path matching one server's glob and not another's, a bitmask excluding an event kind, a `*` that does not cross a `/`, and a relative pattern logged and skipped.

B3's wait is time-dependent, and the obvious test of it would pass for the wrong reason. `ServerSettle::end` stamps `quiet_since` from `std::time::Instant::now()` (`bridge/settle.rs:97`), which is why its own tests sleep. A footer wait built on `tokio::time::sleep` under `tokio::time::pause()` advances tokio's clock and not std's, so the "quiet ends the wait early" branch could never fire and the test would pass on the cap branch alone. Make the clock injectable end to end: either `begin` and `end` take a `now`, or the footer wait takes a clock like `bridge/translator/clock.rs` and the assertions read that clock. Then test all three branches: grace elapses before quiet is consulted, quiet ends the wait early, and the cap ends it when quiet never arrives.

The consume semantics get direct tests: a footer's report does not appear again in the next flush, and a footer is silent while `has_baseline()` is false rather than seeding an empty record.

## Stage C

### Socket identity

The socket path derives from a canonicalized directory hash: `$XDG_RUNTIME_DIR/mcpls/<hash>.sock` on Linux with a `/tmp/mcpls-<uid>` fallback where that variable is unset, `$TMPDIR` on macOS, `\\.\pipe\mcpls-<hash>` on Windows. `tokio` is already on `features = ["full"]`, so both transports exist without a new dependency.

The two sides reach that directory differently, and this is checked against a live process rather than assumed. A running mcpls spawned as a stdio MCP server has `CLAUDE_CODE_SESSION_ID` in its environment but **not** `CLAUDE_PROJECT_DIR`; its working directory is the project directory. The hook has `CLAUDE_PROJECT_DIR`. So mcpls hashes its own canonicalized startup working directory and the hook hashes `CLAUDE_PROJECT_DIR`, and the two agree because the host spawns stdio MCP servers in the project directory.

Both sides canonicalize through one function in `mcpls-core::hooks`, and that function uses `dunce::canonicalize` the way `canonicalize_workspace_roots` does (`lib.rs:395-406`). `Path::canonicalize` yields a `\\?\C:\...` extended-length path on Windows, so a design where one side canonicalizes through the standard library and the other through `dunce` disagrees on every Windows install, permanently and with no error to look at. The hash is truncated to 16 hex characters, which keeps the whole macOS path inside `sockaddr_un`'s 104 bytes with room to spare.

That agreement is a property of the host, not a guarantee, which is why `mcpls hook doctor` prints both hashes: a config whose roots point at a subdirectory, a multi-root config, or a symlinked checkout would otherwise produce a permanent silent no-op with nothing to look at. mcpls's own working directory is fixed at spawn, so a `cd` in the agent's shell cannot move it.

The first mcpls instance in a project binds. A later instance becomes passive and retries every 5 seconds, so an owner exiting does not strand it.

**Arbitration is a lock, not a probe.** Deciding ownership by connecting, and treating `ECONNREFUSED` as "the socket is stale, take it", cannot be made exclusive. `rename(2)` is atomic but not exclusive either: two passives that both saw the refusal both bind a temporary name and both rename over the final path, and both succeed. The second rename unlinks the first's inode from the path, leaving a listener nobody can reach that still believes it owns the session, serving its own record to its own flush tool while every hook reaches the other process. Nothing in that design lets the loser notice, so no test can catch it.

So ownership is an advisory lock on `<hash>.lock`, held by the owner for its whole life: `LOCK_EX | LOCK_NB` on Unix, `first_pipe_instance(true)` on Windows, which fails when the pipe already exists. Whoever holds the lock owns the socket path and is the one who unlinks a stale file before binding. A passive instance retries the lock every 5 seconds rather than probing the socket. Stale detection stops being a connect attempt, which is what made it racy.

A passive instance's flush tool forwards to the owner over the socket rather than reading its own record, so one session has one record no matter which process the agent is talking to. **A passive instance also disables its footer**, and forwards a `changed` for its apply targets to the owner before returning the tool result. Otherwise the two doors read different records: the footer would consume from the passive's record while the next flush reads the owner's, delivering the same diagnostics twice, and the passive's own apply would never reach the owner's servers at all except by way of a `FileChanged` event.

### Protocol

Newline-delimited JSON. A connection carries one or more requests and closes when the client is done.

```
{"op":"changed","session":"<id>","paths":["/abs/src/x.rs"],"event":"change"}
{"op":"flush","session":"<id>"}
{"op":"status"}
```

`changed` runs the filters below and **enqueues** the surviving paths for the sweep; it does not resync inline and it does not flush. `flush` drains the delivery record and does not resync. `PostToolBatch` sends `changed` then `flush` on one connection. `status` serves `doctor`.

The consequence is worth stating plainly, because it is surprising: a `PostToolBatch` pair never reports the batch it belongs to. The sweep has not run yet when the `flush` arrives. Those results land in the next flush, which for an agent working in a loop is the very next tool batch or prompt. Making `changed` synchronous instead would put the debounce inside a 1500 ms deadline it cannot fit, and would defeat the burst coalescing that exists so a `cargo fmt` does not produce fifty cancelled checks.

Every op answers within `op_deadline_ms` (1500), whether or not the work behind it has finished, because the host's default hook timeout is 600 seconds and a hook that hangs blocks the agent. The deadline is the hook's protection, not the host's. Work already started keeps running after the deadline answers; what it produces reaches the next flush. The op that can genuinely queue is the sweep, which takes per-path locks, and after this change it does that off the connection entirely.

### Bounding the watched set

Claude Code has a `FileChanged` hook event backed by a real file watcher, delivering `{file_path, event}` where event is `change`, `add`, or `unlink`. That covers every writer, which is the point.

Its watcher passes no ignore list, so the watched set has to be constrained where it is declared, by returning `watchPaths` from `SessionStart`. That list is built by walking to depth one with the `ignore` crate, already a dependency, and returning the non-ignored subdirectories plus root-level files. `target/`, `node_modules/`, and `.git/` are therefore never watched.

**The hook computes it locally and does not ask the socket.** `SessionStart` fires while the host is still spawning the MCP server, which is the same race that took the baseline off this hook, and the socket is bound only after mcpls loads its config. A `SessionStart` that asked over the socket would hit an unbound path, exit 0 with nothing under the silent-failure rule, and leave the session with either no `FileChanged` coverage at all or an unbounded watcher over `target/`, depending on how the host reads an absent `watchPaths`. Neither is acceptable and neither would be visible. So the hook walks `CLAUDE_PROJECT_DIR` itself; it needs no configuration to do that, and filter 1 inside mcpls still drops anything outside the configured roots. Where the project directory and the configured roots differ, the watcher is bounded by the project directory and the diagnostics by the roots, and `doctor` prints both so the difference is visible.

`FileChanged` may also return a revised `watchPaths`, but this design does not use that. Workspace roots are fixed for the process lifetime, so there is nothing for a revision to track: recomputing after a `CwdChanged` would either name paths outside the roots, which filter 1 drops anyway, or narrow the set and lose coverage of the rest of the root.

Two filters then run inside mcpls, in order:

1. The path resolves under a configured root and is not ignored by `.gitignore`.
2. The path matches at least one registered watcher glob, or has a routable extension.

A path passing neither is dropped without touching the tracker. This is what keeps a `cargo check` from filling `DocumentTracker` to its document ceiling with build artifacts, after which every real tool call would fail with `DocumentLimitExceeded`.

### What a change does

Changes arrive in bursts, so they are collected rather than acted on one at a time. A path enters a pending set and the sweep runs once the set has been quiet for `sweep_quiet_ms` (500). This is not an optimisation: every `didSave` restarts rust-analyzer's flycheck and cancels the check in flight, so a `cargo fmt` forwarded one path at a time produces a run of cancelled checks and no diagnostics at all.

The sweep then treats a path by what its server needs:

- **A tracked path** takes B1's resync, whose content comparison drops the paths that did not really change.
- **An untracked path routed to a server whose diagnostics come from a build** must be opened and saved, because naming it is not enough. rust-analyzer already knows what the file says and still will not check it without a `didSave`. This costs a tracker slot, which is what the filters above exist to protect.
- **An untracked path routed to a server that wanted the notification** is reported through `workspace/didChangeWatchedFiles` and costs no slot.

Paths arriving from tool inputs are bounded by what the agent actually edited, so they are opened if routable and a first-touch file still produces diagnostics.

**The sweep stats every path and never trusts the host's event kind.** A hook process is spawned per event and arrives late, and an atomic save through a temporary file and a rename, which mcpls itself performs (`bridge/apply/journal.rs:204-227`), produces `unlink` followed by `add` or a bare `change` depending on how the watcher handles it. Acting on `unlink` would close a document that is alive again by the time the hook connects, drop its tracker entry, and tell every matching server the file was deleted, so gopls or pyrefly lose it until something reopens it. The kind is derived from the filesystem at sweep time: absent is a delete, present and tracked is a resync, present and untracked is a create if the tracker has never held it and a change otherwise. Servers tolerate either of the last two. The kind the host sent is a hint that the path is worth looking at, nothing more. This is the same rule B1 already applies to the apply queue.

**The document ceiling is checked before the sweep opens anything, not discovered by hitting it.** Opening untracked paths to get build diagnostics spends slots against `workspace.max_documents`, and a wide external change can want more slots than remain. The sweep compares the current open count against the ceiling, opens as many of its untracked paths as fit, and reports the shortfall as a line in the flush output naming how many files it did not check and why. It never fails, and it never leaves the tracker so full that the next unrelated tool call dies with `DocumentLimitExceeded`. The paths it skipped are not remembered: the next change to any of them brings it back through the same filters.

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
enabled        = true
sweep_quiet_ms = 500
op_deadline_ms = 1500
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

The socket and its protocol live in `mcpls-core` under a `hooks` module, split three ways: the directory hash and the platform socket path, the wire protocol, and the listener with its client. The listener sits behind a small trait so the Windows named pipe swaps in for the Unix socket without the ops above knowing. `mcpls-cli` holds only the subcommand: read stdin, dispatch on `hook_event_name`, connect, print. Nothing host-specific reaches the bridge or the LSP layer.

```
plugin/
  .claude-plugin/plugin.json
  .mcp.json                 registers mcpls as the MCP server
  hooks/hooks.json          the registrations above
  skills/mcpls/SKILL.md     moved from the repository's top-level skills/
```

Hooks invoke `mcpls` from `PATH`. If the hook process environment lacks the install directory, every hook silently does nothing, which the silent-failure rule makes invisible. `mcpls hook doctor` exists to answer that, and the plugin README leads with it.

### Testing

The socket gets integration tests over a temporary directory: bind, a second instance deferring to a live owner, a second instance taking over a stale socket file, two instances racing for a stale path where exactly one acquires the lock and the other observes that it did not, a passive instance acquiring the socket after the owner exits, a passive instance's flush reaching the owner's record, a passive instance's footer staying silent while its apply targets reach the owner, and an op answering within its deadline while the work behind it is still running.

The watcher filters get unit tests: a `target/` artifact dropped, a gitignored file dropped, a path matching a registered glob forwarded to that server and not to a server that did not register it, and the sweep deriving a delete from an absent file rather than from the event kind it was handed.

Every test that builds a path for one of these must build it with a drive letter on Windows, the way `mcp/server.rs:1736` already does. `Url::from_file_path` fails without one, and a test that silently skips its own assertion proves nothing on the platform this fork is installed on.

Windows named pipes have no CI coverage, but Windows is a supported target, so the listener sits behind a small trait with the logic tested once and the transport verified by hand.

**What no test in this repository can prove.** Whether Claude Code actually invokes `mcpls hook`, whether `mcpls` is on the hook process's `PATH`, and whether the two directory hashes agree on a given machine are all properties of a live host session. The tests above cover the socket, the protocol, and the filters; they say nothing about the wiring. `mcpls hook doctor` against a real session is the gate, and stage C is not done until it has been run. This is stated rather than papered over with a test that would only prove the mock agrees with itself.

## Multiple agents in one project directory

Each session spawns its own mcpls over stdio. Whichever takes the lock owns the socket and serves every session's hooks; the rest stay passive and retry. Per-session records are keyed by `CLAUDE_CODE_SESSION_ID`, which both the hook payload and the MCP server's environment carry, so two agents get independent deduplication state from one warm set of servers and neither sees the other's already-delivered diagnostics.

A passive instance's servers are warm but nobody is feeding them, so it routes both its doors to the owner: the flush forwards, the footer stays off, and an apply made through it sends its targets to the owner as a `changed`. The alternative is two records disagreeing about what has been delivered, which reads to the agent as duplicated diagnostics from one door and missing ones from the other.

If the worktree-per-session habit holds, the passive path is rarely exercised, and a plain lock-or-log would do. It is specified because getting it wrong is silent.

Apply is the exception, inherited from the shipped part 1 rather than introduced here. The global apply mutex is per process, so two mcpls processes applying edits to the same file simultaneously can both pass phase one and the second wins. Two agents editing one file at the same moment is already unrecoverable regardless of mcpls, so this is documented rather than solved.

## What rust-analyzer does with an external write

Measured against rust-analyzer 1.98.1 with a minimal LSP client advertising the same capabilities mcpls does today, driving a two-file scratch crate. The method is a script, not a tool call: spawn the server, let it settle, write the file with a plain shell redirect, send nothing, and watch.

**Its watcher sees the change.** A function appended to a file with no notification of any kind was returned by `workspace/symbol` 35 seconds later, having been absent before the write. rust-analyzer's virtual file system is current without help.

**It does not check the change.** Sixty seconds after an external write introducing an `E0308`, there were zero `publishDiagnostics` and `rust-analyzer/flycheck/0` never began. The control run proves the setup: `didOpen` for the same erroneous text published an empty list, and the following `didSave` began flycheck and delivered the three `E0308` diagnostics 90 milliseconds later.

Two consequences the design has to carry:

- **A watched-files notification would not help, even if rust-analyzer accepted one.** It already knows. What it wants is a `didSave`. So for Rust, and for any server whose diagnostics come from a build rather than from resident analysis, an externally changed file needs mcpls to open it and save it, not merely to name it. That costs a tracker slot per file, which is why stage C's filters matter.
- **A burst of external writes must be debounced into one save.** Every `didSave` restarts flycheck and cancels the `cargo check` in flight, so forwarding a `cargo fmt` across fifty files as fifty saves produces fifty cancelled checks and no diagnostics. One save after the burst settles, per changed file, with a short quiet period before the sweep.

The same measurement should be repeated for any server later added to the routing table whose diagnostics come from a build step rather than from resident analysis.

**Two more measurements stage B needs, using the same script.**

- **What version a flycheck publish carries.** Log `version` on every publish the control run produces. The claim this document rests on is that an open document's flycheck publish is stamped with that document's version, and only unopened fanout files are unversioned; if the log disagrees, B3's reasoning about why the version wait fails needs restating, though its conclusion (wait on progress) does not change.
- **Whether pyrefly publishes for a file it never opened.** Drive pyrefly the same way, send `workspace/didChangeWatchedFiles` for a file with an error that was never sent as a `didOpen`, and watch. This decides whether B2's payoff for pyrefly is delivery or merely analysis currency, and therefore whether the pyrefly e2e can exist at all.

Measured `cargo check` timings for the footer's cap live in B3 rather than here, since they are a property of this repository rather than of a language server.

## Open questions

- **Does a range in the hash make edits above a diagnostic re-report it?** Hashing `(range, severity, message)` re-reports a file whenever an edit shifts an unrelated diagnostic's line. Hashing `(severity, code, message, line text)` would suppress that at the cost of conflating two identical messages on different lines. Native injection appears to have the same property. Stage A shipped with the range, so the measurement is now possible and has not been made; stage C's push traffic is what will make the answer obvious either way.
- **HTTP transport.** `transport-http` exists behind a default-off feature. One process per client holds for stdio only; the record would need rmcp's session id there.

## Deferred from stage A

Stage A's whole-branch review left a register of findings. Three of them land in B4 above because stage C's traffic makes them matter. These are the rest, recorded here so they survive the review workspace, each with the location that proves it:

- A file whose visible set exceeds the whole total budget is deferred forever whenever any other file is delivered first in the same flush (`delivery.rs:203`). Unreachable at the defaults, reachable the moment `max_per_file = 0` is set.
- The settle debounce takes one server's quiet for the workspace's quiet, so a configured server that reports no `$/progress` can still be analysing when the baseline is captured (`bridge/settle.rs:110`). Every built-in reports progress, so this waits on a configuration that adds one that does not.
- `enabled = false` followed by a redefinition of the same id produces a server with no `file_patterns` and no `heuristics`, which loads without error and routes nothing (`config/server.rs:693-708`, `:780-804`). Separately, any entry with a novel `language_id` and no `file_patterns` is unroutable for the same reason.
- The e2e's deduplication assertion is vacuous: the fixture's one error lands in the baseline, so the first real report is empty and the second being empty proves nothing (`ra_e2e.rs:1441-1520`). The sub-case still earns its place on reachability and on ruling out the deadline backstop, but it is not end-to-end coverage of dedup. B1's new e2e is the place to fix this.
- `create_default_config_file` serialises `Self::default()`, freezing every built-in server into a new user's config file (`config/mod.rs:895-897`). It should write a commented template instead.
- `SeverityFloor` serialises `lowercase` where `ToolKind` uses `snake_case` (`config/mod.rs:21`, `config/routing.rs:82`). Identical today for every existing variant; a bug the moment either gains a multi-word one.
- `uri_to_path` is computed twice in the `PublishDiagnostics` arm, now on the hot path for every versioned publish (`lib.rs:140`, `:164`).
- When every server fails to spawn, the early return at `lib.rs:986-1003` reaches neither the baseline task nor the settle deadline restart, so the flush tool answers "still starting up" for the life of the process. Pre-existing, and the empty-config path at `lib.rs:731-739` already shows the fix.

## Risks

- Advertising `didChangeWatchedFiles.dynamic_registration` changes what gopls and tsgo do at init. Both abandon their current behaviour on the strength of the advertisement. Land the advertisement and the notification together, in one commit.
- Replacing forget-on-apply with a resync changes what servers see after every apply, which is the shipped feature's most-used path. The content comparison is what keeps it from cancelling flycheck repeatedly, and it needs the e2e test to prove it.
- Claude Code's `FileChanged` watcher spawns a hook process per event and passes no ignore list. The `watchPaths` computation is the only thing bounding it, so it needs measuring on a repository mid-build rather than at rest.
- Opening externally changed files to get build diagnostics consumes tracker slots against `workspace.max_documents`, and a wide external change can consume many at once. The filters and the debounce bound it, and the sweep checks the ceiling before opening rather than after.
- The footer's cap trades latency against a delivered result, and the first value chosen for it (5 seconds) was already too low for this repository's own `cargo check`. The measurement in B3 is what 15 seconds rests on, and it drifts with the codebase. Re-run it rather than trusting the number.
- Replacing forget-on-apply with a resync makes the apply path send three messages per tracked file where it sent one, inside a future the caller can drop at any point. The per-server `synced` and `saved` bookkeeping is what keeps a cancellation from leaving a server permanently behind, and it is the part of stage B most worth reviewing closely.
- Not claiming `relativePatternSupport` assumes the four servers that matter send plain glob strings. If one sends a relative pattern anyway, its watching silently stops working, which is why the skipped watcher is logged.
- A1 changed what servers spawn for any config that already lists `[[lsp_servers]]`. That was intended, and it is the one change here that can surprise an existing config rather than only adding to it. Shipped in stage A.
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
