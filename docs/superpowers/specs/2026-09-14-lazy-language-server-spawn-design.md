# Start a language server when the agent first touches its language

Status: not built.

Target: the `AbysmalBiscuit/mcpls` fork, not upstream. One user running many agents on a desktop, where breaking a wire format costs a reinstall and is cheaper than carrying a compatibility path.

Builds on `2026-09-12-shared-backend-design.md` Stage 1, which put one backend per checkout in charge of every language server. Independent of that design's remaining stages.

## Problem

`applicable_server_configs` filters the configured servers by whether their project markers appear under a workspace root (`crates/mcpls-core/src/lib.rs:481`), and everything that survives is spawned in one serial loop at startup (`crates/mcpls-core/src/lsp/lifecycle.rs:671`). The markers answer whether a language is present in the checkout. Nothing asks whether this session will use it.

A mixed monorepo makes the difference expensive. Measured on one worktree of a TypeScript and Rust monorepo: thirteen servers attempted, rust-analyzer resident above a gigabyte, and four of the thirteen failing to spawn because their binaries are not installed, each costing a serial spawn attempt in the startup path. A session working only in TypeScript pays all of it, including rust-analyzer priming its cache across the workspace.

One backend serves every session in a checkout, so that cost is paid once rather than per session. It is still paid in full by a session that will never use any of it.

## Goals

1. A session that only touches TypeScript in a mixed checkout never starts rust-analyzer.
2. Touching a file whose language has no server running starts one, without the agent asking for it. A tool call that arrives before that server has finished starting is told to retry, which is the contract it already has.
3. A configured server whose binary is missing costs nothing at backend start, and one attempt in total.
4. The doctor tells a running server apart from a configured, idle one.
5. No second spawn mechanism. The respawn path already spawns, single-flights and backs off.

## Non-goals

- Stopping a server that has gone quiet. A server stays up until the backend exits, which is already shortly after the last session detaches (`crates/mcpls-core/src/backend/endpoint.rs:119`). Unloading a warm index during a session throws away the work that made it worth starting.
- A read signal. The hook protocol carries changes only: `Request::Changed` with `ChangeEvent` of `Change`, `Add` or `Unlink` (`crates/mcpls-core/src/hooks/protocol.rs:20,61`). Reads are not reported by either host. They would also be the wrong signal, since an agent greps and reads across a whole tree while working in one language.
- Re-running the heuristics mid-session. The applicable set is computed once at startup. A `Cargo.toml` created during a session does not make Rust applicable until the next backend.
- Any change to how a server behaves once it is running.

## Shape

The applicable set keeps its current meaning: which servers this checkout could use. What changes is that membership no longer implies a process. A server starts when something shows the session needs it.

### Five states, one map

`Translator` gains `HashMap<ServerId, ServerLifecycle>`, populated from `applicable_server_configs` during setup. Every entry starts `Idle` except servers configured eager, which start `Starting` and take today's startup path unchanged: `register_servers` publishes `Running` for each one that registered, and the batch's failure path publishes `NotInstalled` or `Failed` for the rest.

An eager entry left at `Idle` while the batch ran would be a second process waiting to happen, since `spawn_batch` does not take the single-flight lock and a tool call arriving in that window would find an untriggered server and start one. Beginning them at `Starting` also reproduces the window `expected_servers` exists for: a call that lands in it waits and retries rather than hearing that nothing is configured.

- `Idle`: applicable, config registered, never triggered.
- `Starting`: a spawn is in flight.
- `Running`: registered in `lsp_servers` and alive.
- `NotInstalled`: the spawn failed because the binary is not there. Remembered for the backend's life. Named for what is wrong rather than for the consequence, so it does not read as a synonym of the `ServerUnavailable` error, which covers this state and the backed-off one alike.
- `Failed`: spawned and died, inside the existing respawn backoff.

Routing keeps reading `lsp_servers`, which stays the registry a tool call resolves against. The map answers what state a server is in, which nothing could answer before. Its writers are `ensure_server` and the startup batch finishing its eager entries, and nothing else, which is what keeps the two from drifting apart.

`ServerLifecycle` derives `strum::Display` and `strum::EnumIter`, which adds `strum` to the workspace. `Display` carries the human rendering, including the one state whose words differ from its identifier, so that rendering lives on the type beside the wire form rather than in the CLI. `EnumIter` lets the round-trip test below iterate every variant rather than list the ones that existed when it was written, which is the property that keeps it honest as the enum grows.

`server_configs` moves earlier to match. `register_server_config` runs after a successful spawn today (`crates/mcpls-core/src/lib.rs:313`), so nothing is in the map before the first one. It is populated for every applicable server during setup instead, and it means how to start this server rather than how to restart it.

`expected_servers` goes away. It exists to answer "not registered yet, wait and retry" (`crates/mcpls-core/src/bridge/translator/routing.rs:119`), and `clear_expected_servers` ends that window when background initialization finishes (`crates/mcpls-core/src/lib.rs:1144,1166`). Lazy spawning has no such moment, and a set left populated forever would report a missing binary as "still initializing" for the life of the backend. The lifecycle map answers the same question with more precision.

### Configuration

A `spawn` key, `"lazy"` or `"eager"`, in two places:

```toml
[backend]
spawn = "lazy"          # default

[[lsp_servers]]
language_id = "rust"
spawn = "eager"         # overrides the backend default for this server
```

It sits in `[backend]` because the backend owns the language servers (`crates/mcpls-core/src/backend/mod.rs:4`) and that table already holds the process-lifetime knob `idle_shutdown_ms`. The per-server key is optional, absent meaning follow the backend default, so a server entry never restates the global choice.

An eager server is spawned in the startup batch and registered by `register_servers` exactly as today. The escape hatch is the current code rather than a reimplementation of it.

### Ensuring a server

```rust
async fn ensure_server(&self, id: &ServerId, budget: Option<Duration>) -> Result<()>
```

The settled states return without touching the process table. `Running` and alive is success. `NotInstalled` is `ServerUnavailable` naming the command that was not found. `Failed` inside its backoff window is today's `ServerUnavailable` naming the remaining delay. Otherwise `ensure_server` moves the entry from `Idle` to `Starting` under the map lock and starts a spawn task, and the caller waits.

That write is the single-flight test as well as a state change. A caller that finds the entry already `Starting` subscribes and waits rather than starting a second task, so whether a spawn is under way has one answer and the map holds it.

The spawn runs in a detached task, and the bounded wait is what forces that. A spawn awaited inline under a timeout would be cancelled when the timed-out future is dropped, so the tool call that triggered the work would also destroy it and a retry would start again from nothing. Detaching is what separates giving up waiting from giving up spawning. The single-flight lock (`crates/mcpls-core/src/bridge/translator/respawn.rs:71`) moves inside that task, so two triggers for one language still produce one process.

Detaching needs an owned handle, and nothing on the path to `ensure_server` has one. `resolve_client_for_file` takes `&self`, and so does every tool method above it. `Translator` therefore keeps a `Weak` reference to itself, set once during setup where the `Arc` already exists, and `ensure_server` upgrades it for the task. The alternative, threading `self: &Arc<Self>` down from every tool entry point, spreads a receiver change across the whole translator surface to reach one function. A translator built without that handle, which is every unit test that does not spawn, simply cannot start a server lazily.

Waiters use a per-id `tokio::sync::watch<ServerLifecycle>` created with the map entry. `ensure_server` publishes `Starting`, and the spawn task publishes the terminal state it reaches. A watch rather than a notify, because it carries the state and leaves no lost-wakeup window between reading the map and subscribing.

The task carries a drop guard, because `Starting` is the one state nothing else can resolve. A panic inside the task, a `Weak` upgrade that fails while the translator is going away, or any early return that skips the publish would strand the entry there for the life of the backend: every tool call for that language would spend its whole budget and return `ServerInitializing` forever, and the sweep would keep handing itself the same paths. The guard publishes `Failed` and records a backoff failure if the task ends without having published a terminal state, which puts the server back on the ordinary retry path instead of stranding it.

`respawn_if_dead` becomes this function. Its guard changes from "return unless the server is dead" to "return if a live server is registered", and a never-spawned server is the same operation from an emptier starting state. Everything it does after that stays: fail the old client's pending requests, retire and reinstall the notification pump, invalidate cached diagnostics, clear the document tracker's per-server history, register the diagnostics owner, then swap into `lsp_servers` and `lsp_clients` in that order.

### The diagnostics baseline

`baseline_task` adopts the workspace's settled diagnostics once, after the startup batch registers, and every session record is seeded from whatever it adopted (`crates/mcpls-core/src/bridge/delivery.rs:308`). Its own comment names the failure it exists to prevent: without it the first flush of a session reports every warning the workspace already had (`crates/mcpls-core/src/bridge/delivery.rs:155`). A respawn is safe today only because the replacement republishes hashes the record already holds. A first spawn has no record to match.

Left alone, lazy spawning breaks that outright. `all_failed` is false for an empty batch (`crates/mcpls-core/src/lsp/lifecycle.rs:248`), so an all-lazy checkout settles on zero diagnostics owners and adopts an empty baseline. rust-analyzer then starts on the first `.rs` edit, publishes every warning the workspace already had, and the next flush reports all of them as the agent's new work.

The baseline therefore stops being one startup event and becomes cumulative. A server that becomes a diagnostics owner joins `set_diagnostics_owners`, restarts the settle deadline, and when its output goes quiet its entries are merged into `baseline` and into every live session record that does not already hold them. A session that started before that server existed then believes what a session started after it would believe, and only warnings that appear later are reported.

`set_diagnostics_route_count` divides the shared cache budget among the diagnostics routes (`crates/mcpls-core/src/lib.rs:1181`). It is recomputed on the same event rather than fixed once, so the first owner is not left holding a budget sized for one server after the fourth arrives.

Two things move out of `spawn_lsp_servers_background` into setup, because that task no longer runs for every checkout. The notification pumps are initialised there today (`crates/mcpls-core/src/lib.rs:1119`), and a lazily started server with no pump delivers no diagnostics at all. An empty baseline is adopted whenever no eager server will start, which today happens only when the applicable set is empty (`crates/mcpls-core/src/lib.rs:719`); an applicable set with no eager member reaches the same state by a different route, and `has_baseline` has to be true either way so `get_new_diagnostics` does not advise a retry against a backend where nothing is starting. What stays in the task is the eager batch itself.

### What triggers a spawn

**An edit.** `Sweeper::sweep` already receives every path the host reported, classifies each as changed, created or deleted, and filters by routable extension (`crates/mcpls-core/src/hooks/sweep.rs:226`). The language resolve and the `ensure_server` call go in after the kinds are built and before the created and changed paths are split, so both kinds trigger. Only created paths reach `open_untracked_document` today, so a trigger placed after the split would miss every edit to a file that is already open.

A deleted path does not start a server. There is no document to open, and a delete does not arrive without a nearby edit in practice.

The sweep fires the trigger and does not wait. There is one `Sweeper` per backend and its loop awaits each sweep in turn (`crates/mcpls-core/src/hooks/sweep.rs:125`), so a wait there is not one session's cost: a first Rust edit would hold every other language's `didChange`, for every session, for as long as rust-analyzer's `initialize` takes, which is documented in minutes on a large solution (`crates/mcpls-core/src/lsp/lifecycle.rs:484`). `sweep` also drains `pending` into a local vector before doing any work (`crates/mcpls-core/src/hooks/sweep.rs:227`), so during a wait those other paths are neither pending nor queued, and a tool call landing in that window reads a document the sweep already knows is stale.

Paths whose server is not yet `Running` go back into `pending` instead. The ticker runs at a quarter of the quiet interval (`crates/mcpls-core/src/hooks/sweep.rs:129`), so they come round again shortly and land on the first sweep after the server is up. The guarantee is that an edited file is open by the end of the first sweep after its server is running, which is what an agent observes either way.

**A tool call.** `resolve_client_for_file` is already the async wrapper that calls the synchronous resolver and then gives a dead server a chance to be replaced (`crates/mcpls-core/src/bridge/translator/routing.rs:71`). It calls `ensure_server` with a bounded budget and re-reads the client. The synchronous resolver returns `(ServerId, LspClient)` today (`crates/mcpls-core/src/bridge/translator/routing.rs:97`), so it cannot hand back an identity for a server that has no client without changing that type: it returns the resolved identity and an optional client, and the wrapper decides what an absent client means. The tests that assert `ServerInitializing` from the synchronous path move up to the wrapper along with the decision. `get_client_for_file` stays synchronous, so its unit tests still need no runtime.

The budget is a constant, not a configuration key. It covers a fast handshake such as taplo or lua-ls inside the originating call, and hands anything slower back to the retry contract.

### Routing binds to the applicable set

`rebind_router` narrows routes to servers that actually registered (`crates/mcpls-core/src/bridge/translator/mod.rs:830`). Under lazy spawning nothing is registered at startup, so that call would drop every route and no trigger could resolve an identity to ensure. The router binds to the applicable set once during setup, and the post-initialization rebind calls go with `expected_servers`.

That loses something. `rebind_to_registered` does more than drop dead routes: it redirects a failed narrowly-scoped server's tools to the language's live catch-all (`crates/mcpls-core/src/config/routing.rs:378`). Its own precondition comment says incremental registration would need a design that derives the active table on each lookup rather than mutating it once (`crates/mcpls-core/src/config/routing.rs:352`), and lazy registration is exactly that. So the redirect moves to lookup time. When an explicit route resolves to a server that is `NotInstalled`, `get_client_for_file` falls through to the language's catch-all unless that catch-all is `NotInstalled` too, in which case it reports `ServerUnavailable` naming the missing command. `Starting` and `Failed` do not fall through, because those servers are expected back and conscripting the catch-all for them would hand its diagnostics the tools the user routed away.

One error changes with this. A language whose only server is `NotInstalled` answers `ServerUnavailable` where today it answers `NoServerForLanguage`, because `has_language` reads route contents and the routes are no longer pruned (`crates/mcpls-core/src/config/routing.rs:482`). That is the better of the two: the language is configured, and naming the binary that is missing tells the agent more than claiming nothing handles the language.

### Workspace-wide tools

`workspace_symbol_search` has no file to resolve a language from, which is why `WorkspaceServersInitializing` exists as its own error. It tells that error apart from "nothing is configured" by consulting `expected_servers` (`crates/mcpls-core/src/bridge/translator/symbols.rs:202`), so retiring that set leaves it needing a new answer, and under lazy spawning it is the common case rather than a startup window.

It ensures one server: the one `resolve_any` already picks, in configuration order (`crates/mcpls-core/src/config/routing.rs:448`), which is the one `handle_workspace_symbol` then queries (`crates/mcpls-core/src/bridge/translator/symbols.rs:192`). The budget is the same bounded one the per-file path uses. If that server is `NotInstalled` the resolve falls through to the next claimant in order, on the same rule as a per-language route; if it is still `Starting` when the budget expires the call returns `WorkspaceServersInitializing`, which keeps that error meaning what it means today.

Ensuring every applicable server instead would start rust-analyzer from a TypeScript-only session's first symbol search, which is goal 1 broken by the one path that gains nothing from breaking it, since only one server is ever asked. Fanning a workspace query out across every applicable server is a real feature and a separate one: it changes what the tool answers, not when its server starts.

### Missing binaries

`Error::ServerSpawnFailed` carries the underlying `io::Error` as its source (`crates/mcpls-core/src/lsp/lifecycle.rs:377`), so `ErrorKind::NotFound` identifies a missing binary exactly and no string matching is needed.

The eager path never sees that error. `spawn_batch` turns each failure into a `ServerSpawnFailure` holding a formatted message rather than the error itself (`crates/mcpls-core/src/lsp/lifecycle.rs:671`), so by the time `register_servers` could record a state, the kind is gone. The judgement therefore belongs on `Error`, where the source is still typed, and its answer travels on `ServerSpawnFailure` as a field. Both the eager batch and a lazy trigger then record the same state from the same test, rather than one of them guessing from a string.

A not-found spawn records `NotInstalled` once and is never attempted again for the life of that backend. The environment a backend runs with, `PATH` included, is fixed when it starts, so an attempt that failed to find a binary fails identically every time after. Installing the binary mid-session does not bring the server back; the next backend picks it up, and the doctor says why the language is dark.

The command is not resolved against `PATH` ahead of the spawn to avoid the attempt. That duplicates what exec already does and races anything writing to `PATH`. Costing nothing at backend start means one lazy attempt, not zero attempts.

Any other spawn or initialize failure is `Failed` and keeps the existing exponential backoff unchanged. A first attempt is never delayed: `respawn_backoff_remaining` returns nothing for a server with no recorded failure (`crates/mcpls-core/src/bridge/translator/respawn.rs:88`).

### The doctor

`Response::Status`'s `servers` field changes from `Vec<String>` to `Vec<ServerStatus>`, a named struct of an identity and a state rather than a tuple or a formatted string. A struct because everything the doctor later wants to say about a server, a command, a pid, a reason for being dark, is a field added to it rather than another parallel list on the response (`crates/mcpls-core/src/hooks/protocol.rs:123`).

Its state field is `ServerLifecycle` itself, not a second enum mirroring it. The two would have to be kept in step by hand for no gain, since both live in `mcpls-core` and both name the same five states. So `ServerLifecycle` derives `Serialize` and `Deserialize` alongside its strum derives, and serializes snake case: `running`, `starting`, `idle`, `not_installed`, `failed`. The wire form and the human form therefore come from different derives on one type, which is the reason `Display` is worth having rather than reusing the serde name.

The `Response` literals asserted in that module's tests are the wire contract, so they change with it.

`servers_line` renders the state beside each identity (`crates/mcpls-cli/src/hook.rs:462`):

```
language servers: rust (running), typescript (idle), lua (not installed)
```

One line rather than two lists. The doctor line is read in hook output at session start and is terse on purpose, and splitting running from configured forces the reader to cross-reference two lists to answer whether a language is covered.

`not_installed` and `failed` stay two states rather than collapsing into one word for "not working". One is a user action item and the other is a bug report.

The list comes from the lifecycle map rather than `registered_server_ids` (`crates/mcpls-core/src/lib.rs:775`), which is also what keeps an untriggered server on the line instead of silently absent. Only servers whose heuristics matched the checkout appear, so the line stays the length it is today rather than enumerating every built-in.

`Running` in the map means last known to be running. Nothing writes the map when a process exits: death is noticed by `is_server_dead` on the next call that needs the server (`crates/mcpls-core/src/bridge/translator/respawn.rs:58`). The status is therefore rendered by running that check over the `Running` entries, so a server that crashed while nothing was asking reads as failed rather than as healthy. The doctor is the one place where the map is read without a call about to act on it.

Replacing the field rather than adding beside it means a backend still running from an older build answers a newer doctor with a shape it cannot parse, and the doctor reports an unexpected answer until that backend exits. Idle shutdown clears it without intervention.

## Landing

One change rather than staged. The doctor cannot be split off: a release carrying lazy spawning with today's doctor would report an idle server as absent, which reads as a fault and is the opposite of what the goal asks for. The configuration key cannot be split off either, since it decides which path a server takes at startup.

Within the change, the order that keeps the tree working starts with the configuration key, because the lifecycle map cannot be populated without knowing which servers are eager. Then the map itself and the earlier `server_configs` population, then `ensure_server` absorbing `respawn_if_dead`, then the cumulative baseline with the pump initialisation moving into setup, then the router binding, the lookup-time catch-all fallback and the retirement of `expected_servers`, then the two triggers, then the doctor.

The baseline work lands before either trigger, and that ordering is not cosmetic. A trigger that can start a server while the baseline is still a single startup event turns the first lazy spawn into a flood of pre-existing warnings reported as new.

## Open decisions

The inline budget for the tool-call fallback is set by guess rather than measurement. It should be long enough for a fast server's `initialize` round trip and short enough that an agent is not left waiting on a slow one. Measure a taplo and a lua-ls handshake on a cold cache before fixing the constant.

## Rejected alternatives

**A `spawn_if_absent` beside `respawn_if_dead`.** Sharing the single-flight lock and the backoff table but not the body leaves the respawn path untouched, so it carries no regression risk there. It also puts the install sequence in two places. That sequence is neither small nor obvious, every step of it carries a comment explaining why it must happen where it does, and two copies diverge the first time one of them is fixed.

**A spawn supervisor task fed by a channel.** One task owning every spawn serializes them, keeps callers off the locks, and holds the owned handle the detached spawn needs without a `Weak` reference back to the translator. That last point is the real argument for it, and it is close. What decides against it is that the supervisor is a second place where a server can be starting: the lifecycle map says `Starting`, the channel holds a request the supervisor has not picked up yet, and the two disagree for as long as the queue is non-empty. A task spawned at the point of decision leaves one answer to the question of whether a spawn is under way.

Its other advantage costs less than it looks. The waiter map a supervisor would need already exists as the lifecycle watch, so it is not an argument either way.

**The first read as the trigger.** Named in the issue, and wrong twice over. Reads are not reported by the hook protocol, so it is not the free signal it was described as; and an agent reads across a whole tree while working in one language, so it would start rust-analyzer in exactly the session this design exists to keep it out of. Adding a read hook would also be Claude-only, since Codex wires `PostToolUse` on `apply_patch` alone (`plugin/hooks/hooks-codex.json`), which would give two hosts different startup behaviour for one checkout and one backend.

**`enum_dispatch` for the lifecycle states.** It replaces a boxed trait object with an enum that dispatches statically, and it needs a trait with several implementors to do that. `ServerLifecycle` carries no behaviour: it is data that `ensure_server` matches on once. Giving it a trait with a method per state so the macro had something to dispatch would produce five single-use implementations in place of one readable match. The one boxed trait in reach, the translator's `Arc<dyn Clock>`, has exactly two implementors and one of them is a test double, so folding it into an enum would put the fake in the production type. Neither is worth a dependency.

**Blocking the tool call until the server is ready.** Consistent with what the respawn path does today, and `initialize` is documented to take minutes on a large solution (`crates/mcpls-core/src/lsp/lifecycle.rs:484`), so a hover on the first Rust file in a big workspace would stall that long or time out.

**Returning `ServerInitializing` immediately with no inline wait.** The simplest rule, and it charges every lazy first call at least one retry round trip even for a server that would have been ready in the time it took to read the error.

## Verification

- A mixed TypeScript and Rust checkout, a session that touches only TypeScript: rust-analyzer is never spawned, and the doctor shows it idle rather than absent.
- The same checkout, first edit to a `.rs` file: rust-analyzer starts, the file is open by the end of the first sweep after it is running, and the next tool call on it returns a result rather than asking for a retry.
- A `.rs` edit and a `.ts` edit reported together, with rust-analyzer cold: the TypeScript paths are swept on schedule rather than held behind rust-analyzer's handshake.
- A checkout with no eager server, in a workspace that already has warnings: the first flush after a lazily started server settles reports nothing, and a warning introduced after it settles is reported. A session attached before that server started and one attached after see the same thing.
- A spawn task that ends without publishing a terminal state: the entry lands on `Failed`, the next trigger retries it under the backoff, and the sweep keeps running for every other language throughout.
- A configured server whose command does not exist: no spawn attempt at backend start, exactly one on the first trigger, none on any trigger after it, and `not installed` on the doctor line.
- A narrowly-scoped server whose binary is missing, alongside a live catch-all for the same language: the catch-all answers the tools it declared, as it does today.
- A TypeScript-only session issuing a workspace symbol search: it is answered, and rust-analyzer is not started.
- An edit and a tool call for one language landing together: one process.
- A server whose `initialize` outlasts the budget: the tool call is told to retry, the spawn is not cancelled, and the retry finds it running.
- A server that starts and then crashes: the existing backoff, unchanged, and `failed` on the doctor line.
- `spawn = "eager"` on one server in an otherwise lazy checkout: that server is up before the first tool call and the others are idle.
- Every `ServerLifecycle` variant, reached by iteration rather than by name: each renders on the doctor line and survives a wire round trip, so a state added later cannot ship unrendered.
