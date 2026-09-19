# Watch the project from the backend

Status: not built.

Target: the `AbysmalBiscuit/mcpls` fork, not upstream. One user running many agents on a desktop, where breaking a wire format costs a reinstall and is cheaper than carrying a compatibility path.

Stage 3 of `2026-09-12-shared-backend-design.md`, whose "The watcher moves into the backend" section this replaces in detail. It depends on Stage 1, because one backend per project is what makes one watcher per project possible, and it must not undo the guarantee `2026-09-14-lazy-language-server-spawn-design.md` bought.

## Problem

A file changed by a shell command or an external editor leaves a language server holding a stale in-memory copy of it.

The staleness is the visible half. The other half is a promise mcpls makes and does not keep. `initialize` advertises `workspace.didChangeWatchedFiles.dynamicRegistration` (`crates/mcpls-core/src/lsp/lifecycle.rs:801`), which tells every language server that the client will watch the disk so the server need not. Servers take that at its word and register globs: rust-analyzer `**/*.rs` and `**/Cargo.toml`, gopls `**/go.mod` and `**/go.work`. Those registrations are specifically about files the server has not opened, because a new module file changes the crate graph and a manifest edit changes the build.

What keeps the promise today is host-specific and narrow. On Claude Code, `FileChanged` reports one path per event, and the set of paths the host watches at all comes from `watch_paths`, a scan of the project's top level taken once at `SessionStart` (`crates/mcpls-core/src/hooks/filters.rs:183-224`) and blind to every directory created afterwards. Codex has no file-changed hook, so there the promise is kept for nothing beyond the agent's own `apply_patch`.

## Goals

1. A file edited outside the agent reaches the language server on both hosts, with no tool call touching it, including a file under a directory created after the session started.
2. The backend's own writes trigger no resync.
3. A checkout the backend cannot watch says so rather than appearing to work.
4. A session that touches only TypeScript in a mixed checkout still never starts rust-analyzer.
5. The Claude plugin no longer registers `FileChanged`.

## Non-goals

- Watching anything outside the configured workspace roots.
- Replacing the `changed` operation. `PostToolBatch` still sends it, and it stays the path by which an agent's own edit is attributed to a session and starts a server on demand.
- A second debounce. The sweeper's quiet period is the only one, which is why this design takes `notify` rather than `notify-debouncer-full`.
- Making an unwatchable checkout work. It is reported, not worked around.
- Bounding the watch count. See "Rejected alternatives".

## Shape

The watcher is a source of paths for machinery that already exists. `Sweeper` owns the path filtering, the `.gitignore` layering and the quiet-period debounce (`crates/mcpls-core/src/hooks/sweep.rs`), and `Translator`'s drain owns the resync, the `didSave` and the `didChangeWatchedFiles` notification. Nothing in that chain changes except where its first input comes from, and one new rule about what that input may trigger.

### Where it lives

`crates/mcpls-core/src/hooks/watcher.rs`, beside `sweep.rs` and `filters.rs`.

`Runtime::start` builds it immediately after the `Sweeper` (`crates/mcpls-core/src/lib.rs:698-708`), handing it the sweeper, the workspace-root snapshot and the same `cancel_rx` the sweeper's loop takes. One construction site covers the backend and `--no-backend` alike, because both reach `Runtime::start`.

`notify`'s watcher calls its handler on a thread of its own, and adding a watch for a newly created directory needs `&mut Watcher`, which that handler cannot take while it is running. So the handler does nothing but send the event down an unbounded `tokio::sync::mpsc` channel, and a spawned task owns the watcher, drains the channel, decides what to watch and calls `Sweeper::enqueue`. Cancellation drops the watcher, which releases every inotify descriptor with it.

### The directory set

Each directory the `ignore` walk keeps, watched non-recursively.

`notify`'s recursive mode places an inotify watch on every descendant directory on Linux, `target/` and `node_modules/` included, which is how a watcher exhausts `fs.inotify.max_user_watches` on a checkout that has been built once. Walking with `ignore` and watching each survivor non-recursively keeps generated trees out by construction rather than by filtering their events afterwards.

The walk must agree with `PathFilter::admits` about what is ignored, or the two disagree in the two ways that matter: a watch placed on a directory whose files `admits` rejects is a wasted descriptor, and a directory skipped whose files `admits` accepts loses events silently. Rather than restate the rules, `PathFilter` gains `admits_directory`, running the same fold over containing roots that `admits` runs, with `is_dir` true and without the extension and registry checks that only make sense for a file. The walk asks it about each directory it meets and does not descend into one it refuses. `read_gitignore` and `BUILT_IN_IGNORES` stay where they are and keep their single authority over what a project excludes.

Roots may nest, which is how a monorepo with a vendored subproject is configured and what the `admits` tests already cover. The walk runs per root and unions into a set, so a directory under two roots is watched once.

A directory-creation event adds a watch for the new directory if `admits_directory` allows it, and walks it, because a `git checkout` can create a populated subtree faster than the watch on its parent can be placed. This is goal 1's "created after the session started".

### Events reach the sweeper and nothing else

Every path on an event goes to `Sweeper::enqueue`, which already answers from the path and the configured roots alone, with no filesystem call, and already collapses a burst into one sweep. A rename arrives carrying both its old and its new path, and both are enqueued: the sweep stats each one and decides what it is, which is the same mechanism that already absorbs Windows delivering a rename as a remove plus an add.

The watcher does no filtering of its own beyond choosing where to place watches. Two filters that could disagree would be one too many.

`Event::need_rescan()` means `notify` may have dropped events, from a full inotify queue or a `ReadDirectoryChangesW` buffer that overflowed. It is not a list of changed paths and must not be read as one. The task re-walks the roots, placing watches on directories that appeared and dropping ones that went, and enqueues every admitted file it finds. The sweeper's existing document ceiling bounds what that sweep then does: paths past the headroom are named to the servers that registered a watcher glob for them, which costs no tracker slot.

### Watcher events do not start language servers

`Sweeper::sweep` calls `ensure_servers_for_edits` (`crates/mcpls-core/src/hooks/sweep.rs:205-226`), which starts a language server for any routable file that changed. Under hooks, "changed" meant the agent edited it. Under a watcher it means anything on disk moved, including a `git pull`, a background formatter, or another editor. In the thirteen-server monorepo the lazy-spawn design measured, one `.rs` file arriving in a `git pull` would start rust-analyzer in a session that opens only TypeScript, which is the cost goal 1 of that design exists to remove.

So `Sweeper::pending` becomes a map from path to origin rather than a set, `enqueue` takes the origin, and `ensure_servers_for_edits` skips watcher-origin paths. A path that arrives from both origins inside one window keeps the hook origin, so an agent's edit never loses its spawn to a watcher event that happened to land on the same file.

This withholds nothing from an agent. `resolve_client_for_file` starts a file's server on any routed tool call, with a budget, and tells the caller to retry if it is not ready (`crates/mcpls-core/src/bridge/translator/routing.rs:77`), and `workspace_symbol_search` does the same (`crates/mcpls-core/src/bridge/translator/symbols.rs:215`). An agent that wants diagnostics before editing asks and gets a server. What the rule stops is a background sweep starting processes that no session asked for.

### The backend's own writes

Already handled. `resync_from_disk` compares the bytes it read against the document's in-memory content and only bumps the version when they differ (`crates/mcpls-core/src/bridge/state.rs:639-646`). An `apply_edit` leaves disk equal to memory, so the watcher event it produces resyncs to the same version and sends no `didChange`.

### Checkouts that cannot be watched

inotify delivers no events for files on a Windows drive under WSL2, and delivers them unreliably over network filesystems. A backend that watched such a checkout would look like it was working and miss every edit, which is worse than not watching.

Before placing any watch, Linux resolves the root's filesystem type from `/proc/mounts`, taking the longest mount point that is a prefix of the root. `9p`, `drvfs`, `virtiofs`, `cifs`, `smb3`, `nfs` and `nfs4` mean do not start. A path prefix such as `/mnt/c` is a guess about one vendor's layout; the mount type is the fact. The check is Linux-only, because the failure it detects is inotify's.

`ENOSPC` from `Watcher::watch`, which is the watch-descriptor limit, tears the whole watcher down rather than leaving part of the checkout covered. Which part would depend on walk order, so a partly-watched backend is not reproducible and a bug report against it is not readable. `rustix::io::Errno::NOSPC` names the number, `rustix` being a workspace dependency already.

Either outcome produces one state: not watching, with a reason. The hooks keep working, so a session degrades to the coverage it has today rather than breaking.

This limit is worth a safety net and not an architecture. A development machine running systemd allows watches in the hundreds of thousands; this checkout needs 56.

### The doctor

`watch_scan_line` computed its answer locally in the hook CLI, which it could while the watching was the host's. The watcher now lives in the backend, so the state crosses the socket.

`Response::Status` gains a `watcher` field, `#[serde(default)]` like the fields Stage 1 added, carrying a `WatcherStatus { watching: bool, directories: usize, unwatched_reason: Option<String> }`. A struct rather than a formatted string, following `ServerStatus`, so later detail lands on the type instead of growing a parallel field. The doctor renders one line from it: how many directories are watched, or that the checkout is unwatched and why.

### What leaves the plugin

`FileChanged` goes from `plugin/hooks/hooks.json`, and with it the `"FileChanged"` arm of `hook.rs`'s dispatch.

`SessionStart` stays registered. Issue #25 lists it for removal, written before the plugin-packaging work gave it a different job: it runs `bootstrap-binaries`, which is what puts `mcpls` on `PATH`. There is no longer an `mcpls hook` registration under it. What goes is the `"SessionStart"` arm of the dispatch, `session_start_output`, the `watchPaths` reply and its incomplete-scan warning, `watch_paths` and `WatchPaths` with their tests and re-export, `watch_scan_line`, the `watchPaths` assertions in `plugin_manifests.rs` and `cli_integration.rs`, the `SessionStart` watch-scan sentence in `HooksConfig`'s documentation and the generated schema, and the `FileChanged` mention on `Request::Changed`.

`read_gitignore` stays. The watcher's walk is its new caller.

`Request::Changed` itself stays, sent by `PostToolBatch`.

The troubleshooting guide's `.gitignore` negation workaround keeps its meaning. A negation such as `!target/keep.rs` re-admits the file to `admits` while `admits_directory` still refuses `target/`, so the file is still not watched, which is the distinction the guide documents.

### Configuration

None. The watcher adds no keys.

Both failure modes degrade to hooks-only coverage with the doctor saying why, so there is nothing an off switch would rescue. The two things worth tuning, the debounce and the ignore rules, belong to the sweeper and the project's `.gitignore` and have their keys already.

## Dependency

`notify` 8.2.0. Its MSRV of 1.77 sits under the workspace's 1.88. Version 9 is at `rc.5` and is not taken.

It is licensed CC0-1.0, a public-domain dedication, more permissive in practice than MIT, withholding only a patent grant. That licence was briefly a CI question: `deny.toml`'s allow list did not carry it and `cargo deny check licenses` blocked the pipeline. The allow-list entry was added and then the whole file was retired, because this fork does not upstream and so has no audience for a licence policy. The Security Audit job now runs `rustsec/audit-check` over `Cargo.lock` instead, which needs no configuration and keeps the vulnerability scanning the licence check was bundled with.

## Landing

One change. The watcher, the origin flag and the plugin removals land together, because a `FileChanged` removed before the watcher works loses coverage and a watcher added before the removal delivers every Claude Code edit twice.

## Open decisions

None at present.

## Rejected alternatives

**`watchexec` rather than `notify`.** Its library adds process supervision and filtering of its own, neither of which is wanted here: the filtering is the sweeper's and there is no process to supervise. It sits on `notify` regardless.

**`notify-debouncer-full`.** It debounces, and so does the sweeper, whose quiet period is tuned to what restarts a rust-analyzer flycheck. Two debouncers in series add latency and a second place to look when a sweep fires at the wrong moment.

**Recursive watching.** One call instead of a walk. On Linux it places a watch on every descendant directory including `target/` and `node_modules/`, which is the exhaustion this design avoids by construction, and it would deliver events for paths the filter drops anyway.

**An LRU of watches over the files the agent has touched.** Bounds the descriptor count by the working set, which for an agent is small: `max_documents` defaults to 100. It is right about staleness, which can only afflict a document a server has open, and open documents are capped. It cannot serve the other half. The globs servers register are about files they have not opened, and the files that matter most are the ones nobody has touched yet: a `git pull` adds `src/new_module.rs`, the LRU has never seen it, rust-analyzer never learns the module exists, and the next diagnostics call reports an unresolved import that resolves fine on disk. Keeping the design's promise means covering the project, not the working set.

It is the right shape for a fallback, if the descriptor limit ever proves reachable in practice: on `ENOSPC`, retreat to watching the directories of open documents and report coverage as partial. That is a follow-up with a reproduction behind it rather than a second watching mode built on a guess.

**`PollWatcher` for checkouts inotify cannot watch.** It stats every watched path each interval, so on a large checkout it is a recurring full walk, and its own default interval of 30 seconds is slow enough that an agent would read stale content anyway. The honest report costs nothing and misleads nobody.

**Keeping the part of the checkout that was watched when `ENOSPC` arrives.** Some external edits would reach their servers. Which ones depends on walk order, so the behaviour is not reproducible and the resulting bug report is not readable.

**A watcher that filters events itself before the sweeper sees them.** It would save the sweeper a few `admits` calls, which are pure path arithmetic. It would also be a second answer to the question of which paths matter, and the first time the two disagreed the difference would show up as events that vanish.

## Verification

- A file edited by a shell command, under a directory that existed at startup: the language server holds the new content, with no tool call touching the file.
- The same, under a directory created after startup: the same result, which is what `watchPaths` could not do.
- A file created by an external editor and matching a glob a server registered, which no session has opened: that server is notified.
- The same on Codex, which has no file-changed hook at all.
- An `apply_edit` through mcpls: the watcher event it produces resyncs to the same version and sends no `didChange`.
- A save through a temporary file and a rename: the resulting remove and add are one entry in the pending set and one sweep.
- A mixed TypeScript and Rust checkout, a session that touches only TypeScript, while a `git pull` changes a `.rs` file: rust-analyzer is not started, and the doctor still shows it idle.
- The same checkout, the agent editing a `.rs` file through a tool: rust-analyzer starts, as it does today.
- The same `.rs` path arriving from a hook and from the watcher inside one quiet period: the server starts, the hook origin having won.
- A `cargo build` filling `target/`: no watch is held under it and no event from it reaches the sweeper.
- A `.gitignore` negation re-admitting one file under `target/`: `admits` takes it, no watch is placed on `target/`, and the troubleshooting guide's sentence still holds.
- A nested workspace root inside an ignored subtree: its directories are watched, and a directory under two roots is watched once.
- An event overflow: the roots are re-walked, watches on directories that appeared are placed, and the sweep that follows is driven by the walk rather than by the event's paths.
- A checkout on a `9p` or `drvfs` mount: no watcher, the doctor names the filesystem, and `PostToolBatch` still delivers the agent's own edits.
- `fs.inotify.max_user_watches` lowered under a checkout large enough to exceed it: no watcher at all rather than a partial one, the doctor names the limit, and hooks still work.
- The backend shutting down: every inotify descriptor is released with it, and no watcher thread outlives the process.
- Two sessions in one project: one watcher for the pair, and an external edit reaches the shared language server once.
- `--no-backend`: the in-process runtime watches the same way, since both reach `Runtime::start`.
- The plugin manifest: no `FileChanged` entry, and `SessionStart` still bootstrapping binaries on both hosts.
- `cargo deny check licenses` with `notify` in the tree.
