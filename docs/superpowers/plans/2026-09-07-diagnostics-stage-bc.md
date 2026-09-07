# Diagnostics injection, stages B and C, implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make new diagnostics reach the agent without the agent asking, first on the tools that write (stage B), then on every writer in the project through a socket and a Claude Code plugin (stage C).

**Architecture:** Stage B stops an apply from telling language servers to forget the files it wrote and resyncs them instead, implements the `workspace/didChangeWatchedFiles` client half so servers that rely on the client for file watching learn about those writes, and adds an opt-in footer on the three write tools. Stage C adds a per-project Unix socket or Windows named pipe that a `mcpls hook` subcommand talks to, so Claude Code's own file watcher and tool hooks can push changed paths into mcpls and pull new diagnostics back out.

**Tech Stack:** Rust edition 2024, MSRV 1.88, tokio (`features = ["full"]`), `lsp-types` 0.97, `rmcp`, `globset`, `ignore`, `dunce`, `clap`. Tests run under `cargo nextest run`.

**Spec:** `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`

## Global Constraints

- Rust edition 2024, MSRV 1.88. Clippy runs with pedantic and nursery; `unwrap_used` and `expect_used` warn; `missing_docs` warns. Every task must leave `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --check` must be clean at every commit. Every task runs `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings` as its own step, after its tests pass and before its commit step. A task that skips that step is not finished.
- Every `#[cfg(test)] mod tests` block this plan writes into, or creates, opens with `#[allow(clippy::unwrap_used, clippy::expect_used)]` directly under the `#[cfg(test)]` attribute, because `unwrap_used` and `expect_used` warn workspace-wide (`Cargo.toml:55-56`) and the clippy gate above turns warnings into errors. `crates/mcpls-core/src/lsp/lifecycle.rs:946` shows the same allow applied per test, and `crates/mcpls-core/src/lsp/client.rs:801` shows it applied to the whole module; use the module form. Where the module already carries `#[allow(clippy::unwrap_used)]`, widen it to both lints rather than adding a second attribute.
- No `std::sync::Mutex` guard is held across an `.await`: those guards are acquired, used, and dropped inside one synchronous section. A `tokio::sync::Mutex` guard may be held across an `.await`, provided a documented lock order exists and every site follows it. That order is delivery before cache: take `context.delivery` first, then `context.notification_cache`, and never the reverse. Task 10 establishes it, Task 13's footer inherits it through `flush_now`, and Task 19's socket handler cites it explicitly. A site that needs only one of the two takes only that one, which is why `baseline_task` may take the cache alone and then `delivery` alone.
- Commits follow Conventional Commits: `type(scope): description`, imperative, 50 characters or fewer including the prefix, no trailing period, lowercase after the colon, body wrapped at 72 columns, ending with the trailer `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`. Every commit step in this plan spells the message with `git commit -F -` and a quoted heredoc, so the trailer and the wrapping are reproduced exactly rather than retyped. Commits are GPG signed; if signing fails, stop and report rather than passing `--no-gpg-sign` or any other signing override.
- Stage all changes selectively. Never sweep unrelated edits or generated files into a commit. `git add` a specific file or a specific directory this task owns, never a whole top-level directory such as `docs/`.
- Every test that builds a filesystem path must build it with a drive letter on Windows, the way `crates/mcpls-core/src/mcp/server.rs:1736` already does. `Url::from_file_path` fails without one.
- Configuration values from the spec, verbatim: `footer = false`, `footer_grace_ms = 250`, `footer_quiet_ms = 200`, `footer_wait_ms = 15000`, `[diagnostics.hooks] enabled = true`, `sweep_quiet_ms = 500`, `op_deadline_ms = 1500`, hook connect timeout 50 ms, passive lock retry 5 s.
- `DiagnosticsConfig` is `Copy` with `deny_unknown_fields`. Any nested struct added to it is `Copy` too.
- `relativePatternSupport` is deliberately not claimed. A watcher arriving as a `RelativePattern` is logged at `warn` and skipped.
- Work happens in the worktree at `/home/lev/Git/lev/mcpls-diag-bc` on branch `feat/diagnostics-stage-bc`. Never change the branch checked out in the main checkout at `/home/lev/Git/lev/mcpls`. Always pass absolute paths to `git -C`.

---

## File structure

This section is the collision map. Every file a task creates or modifies is named here, so a worker can see before it starts which other tasks touch the same file.

**Stage B, created:**

- `crates/mcpls-core/src/lsp/watched_files.rs`: the `WatchRegistry`: which servers registered which globs under which registration ids, and which of them match a given path and change kind. Pure; no I/O, no LSP client. Task 6.
- `crates/mcpls-core/tests/fixtures/python_workspace/`: the pyrefly fixture, `a.py`, `b.py`, `c.py` and `pyrefly.toml`. Task 9.
- `crates/mcpls-core/tests/pyrefly_e2e.rs`: the B2 end-to-end suite. Task 9.
- `docs/superpowers/notes/2026-09-07-stage-b-measurements.md`: the three measurements stage B rests on. Task 1, already committed.

**Stage B, modified:**

- `crates/mcpls-core/src/bridge/state.rs`: `DocumentState` gains `saved`; `DocumentTracker` gains the resync entry point and the two per-server marking calls. Tasks 2 and 3.
- `crates/mcpls-core/src/bridge/translator/mod.rs`: `forget_changed_documents` becomes `resync_changed_documents`; the drain drives notifications, marking, and the watched-files notification. Tasks 4 and 7.
- `crates/mcpls-core/src/bridge/translator/testing.rs`: `TranslatorHarness`, the fake-server harness the translator tests drive. Tasks 4 and 7.
- `crates/mcpls-core/src/bridge/translator/respawn.rs`: a respawn clears that server's watch registrations. Task 7.
- `crates/mcpls-core/src/bridge/delivery.rs`: the cleared budget, the `Off`-floor arm, and `end_session`. Tasks 11 and 19.
- `crates/mcpls-core/src/bridge/settle.rs`: the footer's own quiet judgment, and an injectable clock. Task 12.
- `crates/mcpls-core/src/bridge/notifications.rs`: the borrowing `diagnostics_entries` accessor. Task 10.
- `crates/mcpls-core/src/config/routing.rs`: `ServerId` derives `PartialOrd, Ord`. Task 2.
- `crates/mcpls-core/src/config/mod.rs`: the four footer keys, then `HooksConfig`. Tasks 12 and 14.
- `crates/mcpls-core/src/lsp/client.rs`: the registry reaches `server_request_result`. Task 7.
- `crates/mcpls-core/src/lsp/lifecycle.rs`: the capability flip, the tripwire test, `ServerInitConfig` carries the registry. Task 7.
- `crates/mcpls-core/src/lsp/mod.rs`: `pub mod watched_files;` and the `WatchRegistry` re-export. Task 6.
- `crates/mcpls-core/src/mcp/handlers.rs`: `BridgeContext` gains the diagnostics config, the settle tracker and the owner flag. Tasks 13 and 19.
- `crates/mcpls-core/src/mcp/server.rs`: the lock-order fix, the payload signature, the footer wrapper and its three call sites. Tasks 10, 13 and 19.
- `crates/mcpls-core/src/lib.rs`: the registry, the settle share, the hooks module, the listener. Tasks 7, 10, 13, 14 and 19.
- `crates/mcpls-core/src/transport.rs`: the three `McplsServer::new` calls in its test module gain the two new arguments. Task 13.
- `Cargo.toml` and `crates/mcpls-core/Cargo.toml`: `globset`, then `fs4`. Tasks 6 and 16.
- `crates/mcpls-core/tests/ra_e2e.rs`, `crates/mcpls-core/tests/fixtures/rust_workspace/src/`: the collision fixture and the B1 e2e. Task 5.
- `crates/mcpls-core/tests/integration/rust_analyzer_tests.rs`: its `ServerInitConfig` literal gains `watch_registry`. Task 7.
- `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`: the B2 pyrefly entry, then the status line. Tasks 9 and 22.

**Stage C, created:**

- `crates/mcpls-core/src/hooks/mod.rs`: module root and the public surface the rest of the crate uses. Created by Task 14; each later hooks module is declared and re-exported here. Tasks 14, 15, 16, 17, 18 and 19.
- `crates/mcpls-core/src/hooks/identity.rs`: canonicalization, the directory hash, the platform socket path, the lock path. Task 14.
- `crates/mcpls-core/src/hooks/protocol.rs`: the newline-delimited JSON request and response types. Created by Task 15, extended by Task 21.
- `crates/mcpls-core/src/hooks/listener.rs`: the transport trait, the Unix and Windows implementations, and lock-based ownership. Task 16.
- `crates/mcpls-core/src/hooks/filters.rs`: the two path filters and the `watchPaths` walk. Task 17.
- `crates/mcpls-core/src/hooks/sweep.rs`: the debounced pending set and the sweep. Created by Task 18; Task 19 adds two more `#[cfg(test)] pub(crate)` accessors beside Task 18's four. Tasks 18 and 19.
- `crates/mcpls-core/src/hooks/service.rs`: `HookRole`, the socket handler, the takeover retry, and their tests. Created by Task 19; Task 21 fills the new `Response::Status` field in its `Status` arm. Tasks 19 and 21.
- `crates/mcpls-cli/src/hook.rs`: the `mcpls hook` and `mcpls hook doctor` subcommands. Tasks 20 and 21.
- `crates/mcpls-core/tests/hooks_socket.rs`: socket integration tests. Task 16. Task 19's tests deliberately live in `hooks/service.rs` instead, because they need `#[cfg(test)] pub(crate)` items no integration-test crate can see.
- `plugin/`: the Claude Code plugin. Task 22.

**Stage C, modified:**

- `crates/mcpls-core/src/config/mod.rs`: `[diagnostics.hooks]`. Task 14.
- `crates/mcpls-core/src/lib.rs`: build the registry and the hook listener; restart nothing else. Tasks 14 and 19.
- `crates/mcpls-cli/src/args.rs`, `crates/mcpls-cli/src/main.rs`: the subcommand. Task 20.
- `crates/mcpls-cli/Cargo.toml`: `serde_json` and `tokio` features the hook dispatcher needs. Task 20.
- `crates/mcpls-core/src/hooks/protocol.rs`: `Response::Status` gains the owner's directory. Task 21.
- `skills/mcpls/`: moved wholesale under `plugin/`. Task 22.

---

# Stage B

## Task 1: measure what stage B's design rests on

Already done. The note is committed at `docs/superpowers/notes/2026-09-07-stage-b-measurements.md` and its three answers are folded into Tasks 9 and 12 below. Do not re-run it; the steps are kept so the measurement can be reproduced when the numbers drift, and each section of the note carries its own re-measurement recipe.

Three claims in the spec are load-bearing for later tasks and none of them was verified before this task ran. This task answers them and commits the answers. Task 9 reads the pyrefly section and Task 12 reads the timing section.

**Files:**
- Create: `docs/superpowers/notes/2026-09-07-stage-b-measurements.md`
- Create (scratch, not committed): a probe script under `/tmp/claude-*/scratchpad`

**Interfaces:**
- Produces: `docs/superpowers/notes/2026-09-07-stage-b-measurements.md`, containing three headed sections: "Flycheck publish versions", "Pyrefly on an unopened file", and "Cargo check timing". Task 9 reads the pyrefly section, which decides the shape of the B2 end-to-end proof. Task 12 reads the timing section, which is what `footer_wait_ms` rests on.

**Results, so the tasks below do not have to re-derive them:**

1. A flycheck publish for a document rust-analyzer holds open carries a `version`, equal to the version the last `didChange` sent. A publish for a file that was never opened carries no `version` key at all.
2. Pyrefly does not publish diagnostics for a file it never received a `didOpen` for. It does register watchers covering that file, and it does act on `workspace/didChangeWatchedFiles`: it re-published for the document it holds open 2 ms after the notification. So watching buys pyrefly analysis currency for its open documents, not diagnostic delivery for unopened ones.
3. `cargo check --workspace --all-targets` after touching a file in `mcpls-core` takes about 4.3 seconds on this machine. `footer_wait_ms = 15000` is confirmed and Task 12 needs no adjustment.

- [ ] **Step 1: write a minimal LSP probe client**

A standalone Rust binary or a Python script, in the scratchpad, that spawns a language server over stdio, performs `initialize` and `initialized`, and logs every inbound message with its method and full params. It must be able to send `textDocument/didOpen`, `textDocument/didChange`, `textDocument/didSave`, and `workspace/didChangeWatchedFiles`. Advertise the same client capabilities `build_client_capabilities` produces (read it at `crates/mcpls-core/src/lsp/lifecycle.rs:671`; `:730` is inside the capability literal's body, not the definition), plus `workspace.didChangeWatchedFiles.dynamicRegistration = true`, since the pyrefly probe needs pyrefly to register watchers.

- [ ] **Step 2: measure flycheck publish versions**

Against `crates/mcpls-core/tests/fixtures/rust_workspace`: spawn rust-analyzer, wait for `$/progress` to go quiet, `didOpen` a file, `didChange` it to introduce an `E0308`, `didSave`, then log every `textDocument/publishDiagnostics` with its `version` field for 60 seconds.

Record: whether the publishes carrying rustc diagnostics have a `version`, and if so whether it equals the version the `didChange` sent.

- [ ] **Step 3: measure pyrefly on an unopened file**

Create a two-file Python package in the scratchpad where `b.py` calls a function defined in `a.py` with the wrong argument type. Spawn pyrefly, `didOpen` only `a.py`, wait for quiet, then write a breaking change to `b.py` on disk and send `workspace/didChangeWatchedFiles` naming `b.py` with `type: 2` (Changed). Log publishes for 60 seconds.

Record: whether pyrefly published anything at all for `b.py`, which it never received a `didOpen` for.

- [ ] **Step 4: measure cargo check timing**

```fish
cd /home/lev/Git/lev/mcpls-diag-bc
for i in 1 2 3 4
    touch crates/mcpls-core/src/lib.rs
    /usr/bin/time -f "%e" cargo check --workspace --all-targets 2>&1 | tail -1
end
for i in 1 2
    touch crates/mcpls-cli/src/main.rs
    /usr/bin/time -f "%e" cargo check --workspace --all-targets 2>&1 | tail -1
end
```

Record all six numbers.

- [ ] **Step 5: write the note**

Three sections, each stating the method, the raw observations, and one sentence of conclusion. Where a measurement contradicts the spec, say so plainly and name the spec paragraph.

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add docs/superpowers/notes/2026-09-07-stage-b-measurements.md
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
docs(diagnostics): measure what stage b assumes

Three claims stage B rests on were designed rather than measured: what
version a flycheck publish carries, whether pyrefly publishes for a
file it never opened, and how long this repository's own cargo check
takes. Record the answers and the recipe for re-taking each.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 2: track a saved version per server

**Files:**
- Modify: `crates/mcpls-core/src/bridge/state.rs` (`DocumentState` around `:124-250`)
- Modify: `crates/mcpls-core/src/config/routing.rs` (`ServerId`'s derive list at `:30`)
- Test: `crates/mcpls-core/src/bridge/state.rs`, the existing `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `ServerId` derives `PartialOrd` and `Ord` in addition to what it has today
  - `DocumentState::saved_version(&self, server: &ServerId) -> Option<i32>`
  - `DocumentState::mark_saved(&mut self, server: ServerId, version: i32)` (private to the module, like `mark_synced`)
  - `DocumentState::servers_needing_change(&self, version: i32) -> Vec<ServerId>`: every server in `synced` whose recorded version is below `version`
  - `DocumentState::servers_needing_save(&self, version: i32) -> Vec<ServerId>`: every server in `synced` whose `saved` entry is absent or below `version`
  - `DocumentState::forget_server` also clears that server's `saved` entry.

- [ ] **Step 1: write the failing tests**

Add to `crates/mcpls-core/src/bridge/state.rs`'s test module. The module declaration must read

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
```

because the tests below use `.expect(...)` and both lints warn workspace-wide against a `-D warnings` gate. If the module already carries a narrower allow, widen it rather than adding a second attribute.

```rust
#[test]
fn test_a_server_that_was_synced_but_not_saved_still_needs_a_save() {
    let mut state = DocumentState::new(test_uri(), "rust".to_string(), "fn a() {}".to_string());
    let rust = ServerId::from("rust");
    state.mark_synced(rust.clone(), 2);

    assert_eq!(state.servers_needing_change(2), Vec::<ServerId>::new());
    assert_eq!(
        state.servers_needing_save(2),
        vec![rust],
        "a didChange that landed says nothing about whether a didSave did, \
         and rust-analyzer runs no check without the save"
    );
}

#[test]
fn test_a_saved_server_needs_neither_at_that_version() {
    let mut state = DocumentState::new(test_uri(), "rust".to_string(), "fn a() {}".to_string());
    let rust = ServerId::from("rust");
    state.mark_synced(rust.clone(), 2);
    state.mark_saved(rust, 2);

    assert!(state.servers_needing_change(2).is_empty());
    assert!(state.servers_needing_save(2).is_empty());
}

#[test]
fn test_a_later_version_makes_a_saved_server_need_both_again() {
    let mut state = DocumentState::new(test_uri(), "rust".to_string(), "fn a() {}".to_string());
    let rust = ServerId::from("rust");
    state.mark_synced(rust.clone(), 2);
    state.mark_saved(rust.clone(), 2);

    assert_eq!(state.servers_needing_change(3), vec![rust.clone()]);
    assert_eq!(state.servers_needing_save(3), vec![rust]);
}

#[test]
fn test_a_server_never_synced_is_not_reported_as_needing_anything() {
    let state = DocumentState::new(test_uri(), "rust".to_string(), "fn a() {}".to_string());
    assert!(
        state.servers_needing_change(2).is_empty(),
        "a resync tells servers that already hold the document; opening it \
         for a new server is ensure_open's job, not the resync's"
    );
    assert!(state.servers_needing_save(2).is_empty());
}

#[test]
fn test_forgetting_a_server_clears_its_saved_version_too() {
    let mut state = DocumentState::new(test_uri(), "rust".to_string(), "fn a() {}".to_string());
    let rust = ServerId::from("rust");
    state.mark_synced(rust.clone(), 2);
    state.mark_saved(rust.clone(), 2);
    state.forget_server(&rust);

    assert_eq!(
        state.saved_version(&rust),
        None,
        "a respawned process has saved nothing, so a stale saved version \
         would suppress the didSave the fresh process needs"
    );
}
```

If the test module has no `test_uri()` helper, add one:

```rust
fn test_uri() -> Uri {
    "file:///tmp/mcpls-test/a.rs".parse().expect("a valid test uri")
}
```

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core bridge::state::tests::test_a_server_that_was_synced`
Expected: FAIL, `no method named servers_needing_change`.

- [ ] **Step 3: give `ServerId` a total order**

Both list-returning methods below iterate a `HashMap`, so they sort before returning to keep the resync's notification order reproducible. `ServerId` (`crates/mcpls-core/src/config/routing.rs:30`) derives `Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize` and nothing else, so `Vec<ServerId>::sort_unstable` does not compile today. Add the two ordering traits to that derive:

```rust
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ServerId(String);
```

The inner `String`'s lexical order is the only order that could be meant, so a derive says it more clearly than a hand-written `impl Ord` over `as_str()`. Task 6 also sorts `Vec<ServerId>` and inherits this derive; it must not add a second one.

- [ ] **Step 4: implement**

In `DocumentState`, add the field beside `synced`:

```rust
    /// Last document version for which `server` was sent a `didSave`.
    ///
    /// Separate from `synced` because a `didChange` and a `didSave` are two
    /// notifications with an interruption point between them, and a server
    /// whose diagnostics come from a build runs nothing on the change
    /// alone. A resync is finished for a server only once both have landed.
    saved: HashMap<ServerId, i32>,
```

Initialize it to `HashMap::new()` in `DocumentState::new` and in any other construction site (there is one in the test module around `:1294`).

Then:

```rust
    /// Last document version for which `server` was sent a `didSave`, or
    /// `None` if it has never been sent one.
    #[must_use]
    pub fn saved_version(&self, server: &ServerId) -> Option<i32> {
        self.saved.get(server).copied()
    }

    /// Records that `server` was sent a `didSave` at `version`.
    fn mark_saved(&mut self, server: ServerId, version: i32) {
        self.saved.insert(server, version);
    }

    /// Servers holding this document that have not yet been told about
    /// `version`.
    #[must_use]
    pub fn servers_needing_change(&self, version: i32) -> Vec<ServerId> {
        self.synced
            .iter()
            .filter(|(_, synced)| **synced < version)
            .map(|(server, _)| server.clone())
            .collect()
    }

    /// Servers holding this document that have not been sent a `didSave` at
    /// `version`.
    #[must_use]
    pub fn servers_needing_save(&self, version: i32) -> Vec<ServerId> {
        self.synced
            .keys()
            .filter(|server| self.saved.get(*server).is_none_or(|saved| *saved < version))
            .cloned()
            .collect()
    }
```

Extend `forget_server`:

```rust
    fn forget_server(&mut self, server: &ServerId) {
        self.synced.remove(server);
        self.saved.remove(server);
    }
```

Both list-returning methods iterate a `HashMap`, so collect into a `Vec` and call `sort_unstable()` on it before returning, which is what Step 3's `Ord` derive is for.

- [ ] **Step 5: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core bridge::state`
Expected: PASS, including every pre-existing `state` test.

- [ ] **Step 6: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 7: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/bridge/state.rs crates/mcpls-core/src/config/routing.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(bridge): track a saved version per server

A didChange landing tells nothing about whether a didSave did, and a
server whose diagnostics come from a build runs no check on the change
alone. Record the two separately, per server, so a resync knows which
of the two notifications each server is still owed.

ServerId gains Ord so the two new server lists sort, which is what
keeps a resync's notification order reproducible.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 3: give the tracker a resync entry point

**Files:**
- Modify: `crates/mcpls-core/src/bridge/state.rs` (`DocumentTracker`, near `ensure_open` at `:590`; `lock_path` is at `:505` and `disk_phase` at `:607`)
- Test: `crates/mcpls-core/src/bridge/state.rs` test module at `:1033`

**Interfaces:**
- Consumes: `DocumentState::servers_needing_change`, `servers_needing_save`, `saved_version`, `mark_saved` from Task 2.
- Produces:

```rust
/// What a resync of one path found, and what still has to be sent for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resync {
    /// URI to name in the notifications.
    pub uri: Uri,
    /// Version the document is now at.
    pub version: i32,
    /// Full text to send in the `didChange`.
    pub text: String,
    /// Servers that still need a `didChange` at `version`.
    pub needs_change: Vec<ServerId>,
    /// Servers that still need a `didSave` at `version`.
    pub needs_save: Vec<ServerId>,
}

impl Resync {
    /// Whether every server this document is open for is caught up.
    #[must_use]
    pub fn is_settled(&self) -> bool { ... }
}

impl DocumentTracker {
    /// Re-read `path` from disk and report what its servers still need.
    /// `Ok(None)` when the path is not tracked.
    /// The guard parameter is how the caller proves it holds this path's
    /// lock: a `PathLockGuard` cannot exist without having awaited it.
    pub async fn resync_from_disk(
        &self,
        path: &Path,
        _guard: &PathLockGuard<'_>,
    ) -> Result<Option<Resync>>;

    /// Record that `server` received the `didChange` for `version`.
    pub fn mark_change_sent(&self, path: &Path, server: &ServerId, version: i32, generation: u64);

    /// Record that `server` received the `didSave` for `version`.
    pub fn mark_save_sent(&self, path: &Path, server: &ServerId, version: i32, generation: u64);

    /// Sync generation for `server`, to be captured before notifying and
    /// passed back to the two marking calls.
    pub fn generation_for(&self, server: &ServerId) -> u64;

    /// A clone of the tracked state for `path`, for tests and diagnostics.
    pub fn snapshot(&self, path: &Path) -> Option<DocumentState>;
}
```

- [ ] **Step 1: write the failing tests**

The test module at `crates/mcpls-core/src/bridge/state.rs:1033` needs `#[allow(clippy::unwrap_used, clippy::expect_used)]` under its `#[cfg(test)]`, since these tests use `.expect(...)` throughout.

Two helpers the tests below use. The module has neither today, so add both beside the existing `fake_lsp_client` at `:1923`:

```rust
    /// An `LspClient` whose notifications succeed, discarding the
    /// `FakeServer` guard the tracker tests do not read back from.
    ///
    /// The guard owns two `cat` children with `kill_on_drop`, so it is
    /// returned alongside the client and the caller must hold it for as
    /// long as it uses the client.
    fn fake_client() -> (LspClient, FakeServer) {
        fake_lsp_client()
    }

    /// The extension map the tracker needs to route `.rs` to `rust`.
    fn extension_map() -> HashMap<String, String> {
        HashMap::from([("rs".to_string(), "rust".to_string())])
    }
```

Each test below therefore binds `let (client, _fake) = fake_client();` and passes `&client`. Do not drop `_fake` early: dropping it kills the `cat` children and every later `notify` on that client fails.

```rust
#[tokio::test]
async fn test_a_resync_reports_both_lists_for_a_changed_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn a() {}").expect("write");

    let tracker = DocumentTracker::new(ResourceLimits::default(), extension_map());
    let (client, _fake) = fake_client();
    let rust = ServerId::from("rust");
    tracker.ensure_open(&path, &rust, &client).await.expect("open");

    std::fs::write(&path, "fn a() -> i32 { }").expect("rewrite");

    let _guard = tracker.lock_path(&path).await;
    let resync = tracker
        .resync_from_disk(&path)
        .await
        .expect("resync")
        .expect("the path is tracked");

    assert_eq!(resync.needs_change, vec![rust.clone()]);
    assert_eq!(resync.needs_save, vec![rust]);
    assert!(!resync.is_settled());
    assert_eq!(resync.text, "fn a() -> i32 { }");
}

#[tokio::test]
async fn test_a_resync_over_identical_content_still_reports_an_unsaved_server() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn a() {}").expect("write");

    let tracker = DocumentTracker::new(ResourceLimits::default(), extension_map());
    let (client, _fake) = fake_client();
    let rust = ServerId::from("rust");
    tracker.ensure_open(&path, &rust, &client).await.expect("open");

    let _guard = tracker.lock_path(&path).await;
    let resync = tracker
        .resync_from_disk(&path)
        .await
        .expect("resync")
        .expect("the path is tracked");

    assert!(
        resync.needs_change.is_empty(),
        "nothing changed, so no server is behind on content"
    );
    assert_eq!(
        resync.needs_save,
        vec![rust],
        "ensure_open sends didOpen and never didSave, so the server has \
         never been told to check this file; a re-drain after a cancelled \
         resync lands here and must not conclude there is nothing to do"
    );
}

#[tokio::test]
async fn test_marking_a_change_sent_does_not_settle_the_save() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn a() {}").expect("write");

    let tracker = DocumentTracker::new(ResourceLimits::default(), extension_map());
    let (client, _fake) = fake_client();
    let rust = ServerId::from("rust");
    tracker.ensure_open(&path, &rust, &client).await.expect("open");
    std::fs::write(&path, "fn a() -> i32 { }").expect("rewrite");

    let generation = tracker.generation_for(&rust);
    let version = {
        let _guard = tracker.lock_path(&path).await;
        let resync = tracker.resync_from_disk(&path).await.expect("resync").expect("tracked");
        tracker.mark_change_sent(&path, &rust, resync.version, generation);
        resync.version
    };

    let _guard = tracker.lock_path(&path).await;
    let again = tracker.resync_from_disk(&path).await.expect("resync").expect("tracked");
    assert!(again.needs_change.is_empty());
    assert_eq!(again.needs_save, vec![rust]);
    assert_eq!(again.version, version, "a second read of unchanged content does not bump");
}

#[tokio::test]
async fn test_a_resync_of_an_untracked_path_reports_nothing() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn a() {}").expect("write");

    let tracker = DocumentTracker::new(ResourceLimits::default(), extension_map());
    let _guard = tracker.lock_path(&path).await;
    assert!(tracker.resync_from_disk(&path).await.expect("resync").is_none());
}

#[tokio::test]
async fn test_a_stale_generation_does_not_mark_a_respawned_server_caught_up() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn a() {}").expect("write");

    let tracker = DocumentTracker::new(ResourceLimits::default(), extension_map());
    let (client, _fake) = fake_client();
    let rust = ServerId::from("rust");
    tracker.ensure_open(&path, &rust, &client).await.expect("open");
    std::fs::write(&path, "fn a() -> i32 { }").expect("rewrite");

    let stale = tracker.generation_for(&rust);
    let version = {
        let _guard = tracker.lock_path(&path).await;
        tracker.resync_from_disk(&path).await.expect("resync").expect("tracked").version
    };
    tracker.forget_server(&rust);
    tracker.mark_save_sent(&path, &rust, version, stale);

    let state = tracker.snapshot(&path).expect("the document is still tracked");
    assert_eq!(
        state.saved_version(&rust),
        None,
        "the process that received that didSave is gone"
    );
}
```

`DocumentTracker` has no `snapshot` accessor today, so the last test does not compile without one. Add it beside `open_paths` at `:452`:

```rust
    /// A clone of the tracked state for `path`, or `None` when the tracker
    /// does not hold it.
    ///
    /// A clone rather than a borrow: `documents` is a `StdMutex` and
    /// handing out a guard would let a caller hold it across an `.await`.
    /// For tests and for diagnostics, not for the hot path.
    #[must_use]
    pub fn snapshot(&self, path: &Path) -> Option<DocumentState> {
        lock_std(&self.documents).get(path).cloned()
    }
```

`DocumentState` already derives `Clone` (`state.rs:122`), so nothing else is needed for this.

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core bridge::state::tests::test_a_resync`
Expected: FAIL, `no method named resync_from_disk`.

- [ ] **Step 3: implement**

```rust
impl Resync {
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.needs_change.is_empty() && self.needs_save.is_empty()
    }
}

impl DocumentTracker {
    /// Re-read `path` from disk, commit any change, and report which servers
    /// holding it open still need a `didChange` or a `didSave` at the
    /// resulting version.
    ///
    /// Always reads. The apply queue this drives is itself proof the file
    /// was written, and `disk_phase`'s stat fast paths would skip a
    /// same-length rewrite landing inside the debounce window.
    ///
    /// Returns `Ok(None)` for a path the tracker does not hold: naming an
    /// untracked path to the servers that asked to watch it is the watched
    /// files registry's job, and opening it is the sweep's.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or exceeds the
    /// configured size limit.
    pub async fn resync_from_disk(
        &self,
        path: &Path,
        _guard: &PathLockGuard<'_>,
    ) -> Result<Option<Resync>> {
        debug_assert_eq!(
            _guard.path(),
            path,
            "resync_from_disk's guard must be the lock for this path"
        );
        let read_at = SystemTime::now();
        if !lock_std(&self.documents).contains_key(path) {
            return Ok(None);
        }
        let meta = fs::metadata(path).await.map_err(|e| Error::FileIo {
            path: path.to_path_buf(),
            source: e,
        })?;
        let (fresh, ..) = self.read_to_string_checked(path).await?;
        let snap = DiskSync {
            mtime: meta.modified().ok(),
            size: meta.len(),
            mtime_settled: mtime_settled(meta.modified().ok(), read_at),
            content_checked_at: Instant::now(),
        };

        let mut documents = lock_std(&self.documents);
        let Some(state) = documents.get_mut(path) else {
            return Ok(None);
        };
        let version = if fresh == state.content {
            state.set_disk(snap);
            state.version()
        } else {
            let next = state.version().saturating_add(1);
            state.commit_reload(next, fresh, Some(snap));
            next
        };
        Ok(Some(Resync {
            uri: state.uri().clone(),
            version,
            text: state.content().to_string(),
            needs_change: state.servers_needing_change(version),
            needs_save: state.servers_needing_save(version),
        }))
    }

    /// Sync generation for `server`, captured before notifying and handed
    /// back to [`Self::mark_change_sent`] and [`Self::mark_save_sent`].
    #[must_use]
    pub fn generation_for(&self, server: &ServerId) -> u64 {
        self.generation(server)
    }

    /// Record that `server` received the `didChange` for `version`.
    ///
    /// Dropped when `generation` no longer matches: the process that would
    /// have received it has been respawned, and the fresh one has seen
    /// nothing.
    pub fn mark_change_sent(&self, path: &Path, server: &ServerId, version: i32, generation: u64) {
        let mut documents = lock_std(&self.documents);
        if self.generation(server) != generation {
            return;
        }
        if let Some(state) = documents.get_mut(path) {
            state.mark_synced(server.clone(), version);
        }
    }

    /// Record that `server` received the `didSave` for `version`.
    pub fn mark_save_sent(&self, path: &Path, server: &ServerId, version: i32, generation: u64) {
        let mut documents = lock_std(&self.documents);
        if self.generation(server) != generation {
            return;
        }
        if let Some(state) = documents.get_mut(path) {
            state.mark_saved(server.clone(), version);
        }
    }
}
```

`mark_synced` and `mark_saved` are private to the module, which is where these live, so no visibility change is needed. `self.generation(server)` (`:484`) takes the `generations` lock while `documents` is held; `sync_phase` (`:739`) already performs the same generation re-check under the `documents` guard at `:853`, so read that and match its order exactly. If the two differ, read the generation before taking `documents` and re-read it under `documents` the way `sync_phase` does.

Both are `std::sync::Mutex` guards taken and dropped inside one synchronous block, so the global constraint on `.await` does not apply here.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core bridge::state`
Expected: PASS.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/bridge/state.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(bridge): add a disk resync to the tracker

The tracker re-reads a written file, commits the new version, and
reports which servers still owe a didChange and which still owe a
didSave. Marking is per server and per notification, so a cancelled
request cannot leave a server recorded as caught up while it still
holds pre-apply text.

The read is unconditional: disk_phase's stat fast paths would skip a
same-length rewrite landing inside the debounce window, and the apply
queue is itself proof the file was written.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 4: resync after an apply instead of forgetting

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/mod.rs:294-383`
- Modify: `crates/mcpls-core/src/bridge/translator/testing.rs` (add `TranslatorHarness`)
- Test: `crates/mcpls-core/src/bridge/translator/mod.rs` test module at `:618`

**Interfaces:**
- Consumes: `DocumentTracker::resync_from_disk(&self, path: &Path, _guard: &PathLockGuard<'_>) -> Result<Option<Resync>>`, whose guard parameter is satisfied by the `_path_guard` this task already takes, `Resync { uri, version, text, needs_change, needs_save }`, `DocumentTracker::mark_change_sent`, `mark_save_sent`, `generation_for`, `snapshot` from Task 3. `Resync::is_settled` is not consumed here: the drain decides from the two notify loops rather than re-reading the struct, and only Task 3's own test calls it.
- Produces:
  - `Translator::resync_changed_documents(&self)`, `pub(crate)`, replacing `forget_changed_documents`
  - `Translator::queue_invalidations(&self, paths: &[PathBuf])`, `pub(crate)`
  - `close_one_document` becomes `close_one_document_locked`, called only for paths absent from disk
  - `TranslatorHarness` in `crates/mcpls-core/src/bridge/translator/testing.rs`

`resync_changed_documents` and `queue_invalidations` are `pub(crate)` rather than private because Task 18's `Sweeper` lives in `crate::hooks::sweep`, a different module, and drives both. The queue itself stays a private field: an accessor is a seam that can keep its invariants, a public field is not.

- [ ] **Step 1: write the failing tests**

Put these in `crates/mcpls-core/src/bridge/translator/mod.rs`'s `#[cfg(test)] mod tests` at `:618`, and give that module `#[allow(clippy::unwrap_used, clippy::expect_used)]` under its `#[cfg(test)]` if it does not already carry it.

```rust
#[tokio::test]
async fn test_a_rewritten_file_gets_a_change_then_a_save() {
    let harness = TranslatorHarness::with_one_server("rust").await;
    let path = harness.write_file("a.rs", "fn a() {}");
    harness.open(&path, "rust").await;
    harness.rewrite_file(&path, "fn a() -> i32 { }");
    harness.queue_invalidation(&path);

    harness.translator.resync_changed_documents().await;

    let sent = harness.notifications_for("rust");
    assert_eq!(
        sent.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["textDocument/didChange", "textDocument/didSave"],
        "the change carries the new text and the save is what starts a build"
    );
}

#[tokio::test]
async fn test_a_file_the_apply_deleted_is_closed() {
    let harness = TranslatorHarness::with_one_server("rust").await;
    let path = harness.write_file("a.rs", "fn a() {}");
    harness.open(&path, "rust").await;
    std::fs::remove_file(&path).expect("remove");
    harness.queue_invalidation(&path);

    harness.translator.resync_changed_documents().await;

    assert_eq!(harness.notifications_for("rust"), vec!["textDocument/didClose"]);
    assert!(harness.translator.document_tracker().snapshot(&path).is_none());
}

#[tokio::test]
async fn test_an_untracked_path_produces_no_notification() {
    let harness = TranslatorHarness::with_one_server("rust").await;
    let path = harness.write_file("a.rs", "fn a() {}");
    harness.queue_invalidation(&path);

    harness.translator.resync_changed_documents().await;

    assert!(harness.notifications_for("rust").is_empty());
}

#[tokio::test]
async fn test_a_second_drain_sends_the_save_a_cancellation_lost() {
    let harness = TranslatorHarness::with_one_server("rust").await;
    let path = harness.write_file("a.rs", "fn a() {}");
    harness.open(&path, "rust").await;
    harness.rewrite_file(&path, "fn a() -> i32 { }");
    harness.queue_invalidation(&path);

    // Drop the drain future after its didChange and before its didSave, the
    // way a cancelled tool call would.
    harness.fail_notifications_after("rust", 1);
    harness.translator.resync_changed_documents().await;
    harness.clear_notifications();
    harness.allow_notifications("rust");

    harness.translator.resync_changed_documents().await;

    assert!(
        harness.notifications_for("rust").contains(&"textDocument/didSave".to_string()),
        "the content now matches disk, so a comparison alone would call this \
         finished and the file would never be checked"
    );
}
```

`TranslatorHarness` does not exist. Build it in `crates/mcpls-core/src/bridge/translator/testing.rs`, which already holds this directory's test helpers, reusing that file's `fake_lsp_client()` at `:99` for the fake server over `cat` pipes. Its surface, in full:

```rust
/// A `Translator` with one fake LSP server, a temp workspace, and a record
/// of every notification the fake server received.
pub(super) struct TranslatorHarness {
    /// The translator under test, shared so a test can call it directly.
    pub(super) translator: Arc<Translator>,
    dir: TempDir,
    /* the fake server guards and the recorded notification log */
}

impl TranslatorHarness {
    /// A harness with one registered server under `language_id`.
    pub(super) async fn with_one_server(language_id: &str) -> Self;
    /// Write `contents` to `relative` under the temp workspace and return
    /// the absolute path.
    pub(super) fn write_file(&self, relative: &str, contents: &str) -> PathBuf;
    /// Overwrite an existing file, the way an apply does.
    pub(super) fn rewrite_file(&self, path: &Path, contents: &str);
    /// Open `path` for `server` through `DocumentTracker::ensure_open`.
    pub(super) async fn open(&self, path: &Path, server: &str);
    /// Put `path` on the translator's invalidation queue.
    pub(super) fn queue_invalidation(&self, path: &Path);
    /// The LSP method names `server` received, in order.
    pub(super) fn notifications_for(&self, server: &str) -> Vec<String>;
    /// Forget everything recorded so far.
    pub(super) fn clear_notifications(&self);
    /// Make `server`'s transport reject every send after the first `n`.
    pub(super) fn fail_notifications_after(&self, server: &str, n: usize);
    /// Undo `fail_notifications_after`.
    pub(super) fn allow_notifications(&self, server: &str);
}
```

`queue_invalidation` calls `self.translator.queue_invalidations(&[path.to_path_buf()])`, the `pub(crate)` accessor this task adds, rather than reaching into the private `pending_invalidations` field.

`with_one_server` needs an extension map, because `DocumentTracker::detect_language` routes on the file extension and Task 7's tests drive `.go` files through this same harness. Give it one explicit table rather than leaving it to be guessed:

```rust
    /// The file extension a fake server of this language answers for.
    ///
    /// A fixed table rather than the real routing config: the harness
    /// exists to drive the resync, not to re-test extension routing, and a
    /// test naming a language with no entry here has almost certainly
    /// misspelled it.
    fn extension_for(language_id: &str) -> &'static str {
        match language_id {
            "rust" => "rs",
            "go" => "go",
            "python" => "py",
            other => panic!("TranslatorHarness has no extension mapped for {other}"),
        }
    }
```

`with_one_server` builds `HashMap::from([(extension_for(language_id).to_string(), language_id.to_string())])`, passes it to `Translator::with_extensions`, and registers the fake client under `ServerId::from(language_id)`.

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core translator::tests::test_a_rewritten_file`
Expected: FAIL, `no method named resync_changed_documents`.

- [ ] **Step 3: implement**

Replace both `forget_changed_documents` calls in `apply_locked` (`:300` and `:308` in the pre-change file) with `resync_changed_documents`. Keep the pre-apply call: it flushes anything a previous cancelled drain left behind before this apply writes over it.

```rust
    /// Resynchronize every document whose file an apply rewrote, moved, or
    /// removed.
    ///
    /// An apply can write a file the tool call never queried: a rename
    /// anchored in one file rewrites every file referencing the symbol. LSP
    /// makes the client authoritative for a document it has opened, so a
    /// server ignores the change on disk until it is told. Telling it means
    /// two notifications, not one: a `didChange` carrying the new text, and
    /// a `didSave`, because a server whose diagnostics come from a build
    /// runs nothing on the change alone.
    ///
    /// A path leaves the queue only once every server holding it is caught
    /// up on both. Content matching disk is not the completion test: after
    /// a drain interrupted between a `didChange` and its `didSave` the
    /// content matches and the save is still owed. This loop runs inside
    /// the request future, which is dropped whenever the caller cancels, so
    /// anything unfinished goes back on the queue for the next drain.
    ///
    /// `pub(crate)` because stage C's sweep, in `crate::hooks::sweep`,
    /// drives the same drain for paths that arrived from the host's file
    /// watcher rather than from an apply.
    pub(crate) async fn resync_changed_documents(&self) {
        let mut drain = PendingDrain {
            queue: &self.pending_invalidations,
            remaining: self.pending_invalidations.take(),
        };
        while let Some(path) = drain.remaining.first().cloned() {
            if self.resync_one_document(&path).await {
                drain.remaining.remove(0);
            } else {
                // Leave it queued and stop: a server that rejected one
                // notification will reject the next, and the paths behind
                // this one are still owed their own drain.
                break;
            }
        }
    }

    /// Resynchronize one path. Returns whether it is finished and can leave
    /// the queue.
    async fn resync_one_document(&self, path: &Path) -> bool {
        let _path_guard = self.document_tracker.lock_path(path).await;

        if !path.exists() {
            self.close_one_document_locked(path).await;
            return true;
        }

        let resync = match self
            .document_tracker
            .resync_from_disk(path, &_path_guard)
            .await
        {
            Ok(Some(resync)) => resync,
            Ok(None) => return true,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "could not re-read an applied file; leaving it queued"
                );
                return false;
            }
        };

        for server in &resync.needs_change {
            let generation = self.document_tracker.generation_for(server);
            let Some(client) = lock_std(&self.lsp_clients).get(server).cloned() else {
                continue;
            };
            let params = lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier {
                    uri: resync.uri.clone(),
                    version: resync.version,
                },
                content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: resync.text.clone(),
                }],
            };
            if let Err(error) = client.notify("textDocument/didChange", params).await {
                tracing::warn!(%server, path = %path.display(), %error, "resync didChange failed");
                return false;
            }
            self.document_tracker
                .mark_change_sent(path, server, resync.version, generation);
        }

        for server in &resync.needs_save {
            let generation = self.document_tracker.generation_for(server);
            let Some(client) = lock_std(&self.lsp_clients).get(server).cloned() else {
                continue;
            };
            let params = lsp_types::DidSaveTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: resync.uri.clone(),
                },
                text: None,
            };
            if let Err(error) = client.notify("textDocument/didSave", params).await {
                tracing::warn!(%server, path = %path.display(), %error, "resync didSave failed");
                return false;
            }
            self.document_tracker
                .mark_save_sent(path, server, resync.version, generation);
        }

        true
    }
```

Marking follows each notification rather than preceding the loop. Marking at commit time would leave a server recorded as caught up while it still holds pre-apply text if the future is dropped between the two, and `ensure_open` would then compute `up_to_date` (`bridge/state.rs:766`) and never send anything again.

Rename the existing `close_one_document` (`translator/mod.rs:361`) to `close_one_document_locked` and remove its own `lock_path` acquisition, since `resync_one_document` now holds that lock. Update its doc comment to say the caller holds the lock.

Add the queue accessor beside it, so the sweep in Task 18 can put paths on the queue without the field becoming public:

```rust
    /// Put `paths` on the invalidation queue the next resync drains.
    ///
    /// The queue is what makes a drain restartable, so a caller that has
    /// learned a file changed adds to it and then drives
    /// [`Self::resync_changed_documents`], rather than resyncing one path
    /// directly and losing the rest if it is cancelled.
    pub(crate) fn queue_invalidations(&self, paths: &[PathBuf]) {
        self.pending_invalidations.extend(paths);
    }
```

`InvalidationQueue::extend` (`bridge/apply/mod.rs:43`) already skips paths that are queued, so a repeat costs nothing.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core`
Expected: PASS. Existing apply tests that assert `didClose` on a rewritten file will fail; those assertions were pinning the behaviour this task deliberately replaces, so update them to assert `didChange` then `didSave` and say why in the assertion message.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/bridge/translator/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(bridge): resync applied files, do not close

Applying an edit told every server to forget the files it wrote, so
the moment a rename finished was the moment its servers stopped
knowing about the renamed files. Send a didChange and then a didSave
instead, and drop a path from the queue only once both have landed for
every server holding it.

Content matching disk is not the completion test: after a drain
interrupted between the two notifications the content matches and the
save is still owed.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 5: prove the resync end to end

**Files:**
- Modify: `crates/mcpls-core/tests/fixtures/rust_workspace/src/lib.rs`
- Modify: `crates/mcpls-core/tests/fixtures/rust_workspace/src/functions.rs`
- Modify: `crates/mcpls-core/tests/ra_e2e.rs`

**Interfaces:**
- Consumes: `resync_changed_documents` from Task 4, reached through the `rename_symbol` MCP tool with `apply: true`.
- Produces: nothing later tasks depend on.

- [ ] **Step 1: give the fixture its own collision pair**

The suite already has an apply sub-case, `sc_rename_symbol_apply` at `ra_e2e.rs:1527`, which renames `add` to `plus` and is registered last with the comment "this one writes to the staged workspace, and every anchor above it looks for text this rename moves". A sub-case that also renamed `add` could only work in one of the two possible orders, so this one brings its own pair of symbols that nothing else in the suite touches, and is then immune to where it sits in the registry.

In `crates/mcpls-core/tests/fixtures/rust_workspace/src/lib.rs`, at the end of the file, add two functions with identical signatures:

```rust
/// One half of a deliberate rename collision, used by the stage B resync
/// e2e. Renaming `tally` to `total` makes rustc report E0428 for this file.
pub fn tally(a: i32, b: i32) -> i32 {
    a + b
}

/// The other half. Same signature on purpose: a rename that changed a call
/// site's arity would produce rust-analyzer's own resident diagnostics,
/// which arrive on a `didChange` alone and would let the e2e pass with the
/// `didSave` half of the resync entirely broken. A duplicate definition is
/// a diagnostic only a completed build can report.
pub fn total(a: i32, b: i32) -> i32 {
    a + b
}
```

In `crates/mcpls-core/tests/fixtures/rust_workspace/src/functions.rs`, add a caller so the apply has to write a second file:

```rust
/// Calls `tally` from another module, so renaming it rewrites this file
/// too and the resync has more than one document to catch up.
pub fn tally_twice(a: i32, b: i32) -> i32 {
    crate::tally(a, b) + crate::tally(a, b)
}
```

Both names are chosen so no existing sub-case sees them. `sc_workspace_symbol_search` searches for `add` and `sc_get_document_symbols` looks for `add`, `caller` and `Point`, all by substring, so three new symbols that contain none of those strings change nothing. The fixture still compiles cleanly before the rename, so `sc_get_new_diagnostics`'s "first real report holds nothing back" property is untouched.

- [ ] **Step 2: write the failing e2e sub-case**

Every sub-case in `ra_e2e.rs` is a synchronous `fn sc_x(client: &mut McpClient, workspace: &Path) -> Result<(), String>`, driven from one `#[test] fn ra_e2e_suite()` that is not async. `McpClient::call_tool` takes `(&mut self, name: &str, arguments: &Value)` and returns `Result<Value, _>`, and `assertions::assert_tool_ok` unwraps the tool result into its text. Match that shape exactly: an `async fn` returning `()` and calling `.expect(...)` does not compile against this suite.

```rust
/// The resync sends `didSave`, so a build error an apply introduces reaches
/// the agent.
///
/// Renaming `tally` to `total` collides with the existing `total`, so rustc
/// reports E0428 for `src/lib.rs`. rust-analyzer does not check a rename for
/// conflicts, so the apply lands and the error appears only once a build
/// runs, which happens only if the resync sent a `didSave`.
///
/// The pair is same-signature on purpose. A rename that changed a call
/// site's arity would produce rust-analyzer's own resident diagnostics,
/// which arrive on a `didChange` alone, and this sub-case would then pass
/// with the `didSave` half of the resync entirely broken.
fn sc_resync_delivers_a_build_error_after_an_apply(
    client: &mut McpClient,
    workspace: &Path,
) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let tally_line = find_line(&lib, "pub fn tally(");

    let resp = client
        .call_tool(
            "rename_symbol",
            &json!({
                "file_path": lib.to_string_lossy(),
                "line": tally_line,
                "character": 8,
                "new_name": "total",
                "apply": true,
            }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    if inner["applied"] != json!(true) {
        return Err(format!("expected applied=true, got {inner}"));
    }
    let written = inner["files_written"]
        .as_array()
        .ok_or_else(|| format!("expected files_written array, got {inner}"))?;
    if written.len() < 2 {
        return Err(format!(
            "the caller in functions.rs must have been rewritten too, so the \
             resync has more than one document to catch up; got {written:?}"
        ));
    }

    // Poll rather than asserting on one call: how long the build takes is
    // rust-analyzer's business, and how far into the suite this sub-case
    // runs is the registry's.
    //
    // Discriminate on `omitted`, not on `note`'s presence.
    // `NewDiagnosticsResult::starting_up` sets `note` before `flush` ever
    // runs, with `omitted == 0`, but `new_diagnostics_payload` also sets
    // `note` on a real report that held files back. Skipping every report
    // carrying a `note` would skip real ones. This is the same rule
    // `sc_get_new_diagnostics` states at `ra_e2e.rs:1445`.
    let deadline = Instant::now() + Duration::from_millis(settle_deadline_ms());
    let mut last = Value::Null;
    loop {
        let raw = client
            .call_tool("get_new_diagnostics", &json!({}))
            .map_err(|e| format!("flush call failed: {e}"))?;
        let body = assertions::assert_tool_ok(&raw);
        let report: Value = serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))?;
        let omitted = report["omitted"].as_u64().unwrap_or(0);
        let starting_up = report.get("note").is_some() && omitted == 0;
        if !starting_up {
            if let Some(hit) = find_rustc_e0428(&report) {
                if hit["code"] != json!("E0428") || hit["source"] != json!("rustc") {
                    return Err(format!("matched the wrong diagnostic: {hit}"));
                }
                return Ok(());
            }
            last = report;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no rustc E0428 arrived within the settle deadline. \
                 rust-analyzer publishes its own resident diagnostics on a \
                 didChange alone, so this failing while rename_symbol still \
                 writes means the resync's didSave never reached the server. \
                 Last report: {last}"
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The first `rustc`-sourced `E0428` anywhere in a flush report, or `None`.
fn find_rustc_e0428(report: &Value) -> Option<Value> {
    report["changed"]
        .as_array()?
        .iter()
        .flat_map(|file| file["diagnostics"].as_array().into_iter().flatten())
        .find(|d| d["source"] == json!("rustc") && d["code"] == json!("E0428"))
        .cloned()
}
```

Register it in `ra_e2e_suite`'s sub-case list immediately after `sub_case!(sc_get_new_diagnostics)` and before `sub_case!(sc_rename_symbol_apply)`:

```rust
        sub_case!(sc_get_new_diagnostics),
        // After the dedup sub-case: this one deliberately introduces a
        // compile error, which would break that sub-case's "a second drain
        // with no edits between is empty" property. Its own anchors are
        // symbols nothing else in the suite touches, so whether the rename
        // sub-case below runs before or after it changes nothing.
        sub_case!(sc_resync_delivers_a_build_error_after_an_apply),
        // Last: this one writes to the staged workspace, and every anchor
        // above it looks for text this rename moves.
        sub_case!(sc_rename_symbol_apply),
```

Nothing restores the fixture, and nothing should: no existing sub-case restores anything either. Re-runnability comes from `stage_workspace()` (`ra_e2e.rs:91`) copying the fixture into a fresh `TempDir` on every run, so the checked-in fixture is never written to.

- [ ] **Step 3: run it against the pre-task-4 behaviour and watch it fail**

The fixture and the sub-case are uncommitted at this point, so a scratch worktree checked out at the task-3 commit has neither. Copy the uncommitted test changes into it first, or the run fails for a missing symbol rather than for the missing `didSave`, which is the opposite of confirming RED for the right reason.

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc log --oneline -3
# note the task-3 commit sha, "feat(bridge): add a disk resync to the tracker"
git -C /home/lev/Git/lev/mcpls-diag-bc worktree add /tmp/mcpls-pre-b1 <task-3 sha>
git -C /home/lev/Git/lev/mcpls-diag-bc diff -- crates/mcpls-core/tests/ \
  | git -C /tmp/mcpls-pre-b1 apply
cargo nextest run --manifest-path /tmp/mcpls-pre-b1/Cargo.toml \
  -p mcpls-core --test ra_e2e -- --ignored ra_e2e_suite
```

Expected there: FAIL on `sc_resync_delivers_a_build_error_after_an_apply` with the "no rustc E0428 arrived" message, because forget-on-apply closes the documents and no build runs. Every other sub-case still passes, which is what says the fixture edit itself is sound.

Then remove the scratch worktree:

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc worktree remove --force /tmp/mcpls-pre-b1
```

Never `git stash` anything in this repository: parallel agents share these checkouts and a stash moves work out from under them. Reading another revision is what the scratch worktree above is for.

- [ ] **Step 4: run it on the current tree and watch it pass**

Run: `cargo nextest run -p mcpls-core --test ra_e2e -- --ignored ra_e2e_suite`
Expected: PASS, including every pre-existing sub-case.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add \
  crates/mcpls-core/tests/ra_e2e.rs \
  crates/mcpls-core/tests/fixtures/rust_workspace/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
test(e2e): prove a build error survives an apply

The fixture gains a same-signature pair, tally and total, plus a caller
in another module. Renaming one into the other collides, so rustc
reports E0428, which only a completed build can produce.

Same signature on purpose: a rename that changed a call site's arity
would produce rust-analyzer's resident diagnostics, which arrive on a
didChange alone, and the sub-case would pass with the resync's didSave
half entirely broken.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---


## Task 6: the watched files registry

**Files:**
- Create: `crates/mcpls-core/src/lsp/watched_files.rs`
- Modify: `crates/mcpls-core/src/lsp/mod.rs` (add `pub mod watched_files;` and re-export `WatchRegistry`)
- Modify: `Cargo.toml` workspace dependencies and `crates/mcpls-core/Cargo.toml` (add `globset`)

**Interfaces:**
- Produces:

```rust
/// Which servers asked to be told about which files.
#[derive(Debug, Default)]
pub struct WatchRegistry { ... }

impl WatchRegistry {
    #[must_use] pub fn new() -> Self;
    /// Store the watchers in a `client/registerCapability` payload.
    pub fn register(&self, server: &ServerId, id: &str, watchers: &serde_json::Value);
    /// Drop a registration by id.
    pub fn unregister(&self, server: &ServerId, id: &str);
    /// Drop every registration a server holds, for a respawn.
    pub fn forget_server(&self, server: &ServerId);
    /// Servers that asked to hear about `path` changing this way.
    #[must_use] pub fn servers_for(&self, path: &Path, kind: lsp_types::FileChangeType) -> Vec<ServerId>;
}
```

- [ ] **Step 1: add the dependency**

In `/home/lev/Git/lev/mcpls-diag-bc/Cargo.toml` under `[workspace.dependencies]`, beside `ignore = "0.4"`:

```toml
globset = "0.4"
```

In `crates/mcpls-core/Cargo.toml` under `[dependencies]`, beside `ignore = { workspace = true }`:

```toml
globset = { workspace = true }
```

- [ ] **Step 2: write the failing tests**

Create `crates/mcpls-core/src/lsp/watched_files.rs` with only the test module and the type name, so the tests compile against a stub:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    fn abs(rel: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:\\work\\{}", rel.replace('/', "\\")))
        } else {
            PathBuf::from(format!("/work/{rel}"))
        }
    }

    #[test]
    fn test_a_registered_glob_matches_its_own_server_only() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        let python = ServerId::from("python");
        registry.register(&go, "r1", &json!([{ "globPattern": "**/*.go" }]));
        registry.register(&python, "r2", &json!([{ "globPattern": "**/*.py" }]));

        assert_eq!(
            registry.servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED),
            vec![go]
        );
        assert_eq!(
            registry.servers_for(&abs("main.py"), lsp_types::FileChangeType::CHANGED),
            vec![python]
        );
    }

    #[test]
    fn test_an_absent_kind_means_every_kind() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        registry.register(&go, "r1", &json!([{ "globPattern": "**/*.go" }]));

        for kind in [
            lsp_types::FileChangeType::CREATED,
            lsp_types::FileChangeType::CHANGED,
            lsp_types::FileChangeType::DELETED,
        ] {
            assert_eq!(
                registry.servers_for(&abs("main.go"), kind),
                vec![go.clone()],
                "the protocol says an absent kind is create, change and delete"
            );
        }
    }

    #[test]
    fn test_a_kind_bitmask_excludes_the_kinds_it_omits() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        // 1 = Created, 2 = Changed, 4 = Deleted. 5 is create and delete.
        registry.register(&go, "r1", &json!([{ "globPattern": "**/*.go", "kind": 5 }]));

        assert!(registry
            .servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED)
            .is_empty());
        assert_eq!(
            registry.servers_for(&abs("main.go"), lsp_types::FileChangeType::DELETED),
            vec![go]
        );
    }

    #[test]
    fn test_a_star_does_not_cross_a_directory_separator() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        registry.register(&go, "r1", &json!([{ "globPattern": "/work/*.go" }]));

        assert_eq!(
            registry.servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED),
            vec![go],
            "a single star matches within one segment"
        );
        assert!(
            registry
                .servers_for(&abs("cmd/main.go"), lsp_types::FileChangeType::CHANGED)
                .is_empty(),
            "globset lets a star cross a separator by default and LSP's glob \
             grammar does not, so literal_separator must be on"
        );
    }

    #[test]
    fn test_unregistering_drops_only_that_registration() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        registry.register(&go, "r1", &json!([{ "globPattern": "**/*.go" }]));
        registry.register(&go, "r2", &json!([{ "globPattern": "**/*.mod" }]));
        registry.unregister(&go, "r1");

        assert!(registry
            .servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED)
            .is_empty());
        assert_eq!(
            registry.servers_for(&abs("go.mod"), lsp_types::FileChangeType::CHANGED),
            vec![go]
        );
    }

    #[test]
    fn test_forgetting_a_server_drops_every_registration_it_held() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        registry.register(&go, "r1", &json!([{ "globPattern": "**/*.go" }]));
        registry.register(&go, "r2", &json!([{ "globPattern": "**/*.mod" }]));
        registry.forget_server(&go);

        assert!(registry
            .servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED)
            .is_empty(),
            "a respawned process registers again with fresh ids, so the old \
             globs would otherwise linger for the life of the mcpls process"
        );
    }

    #[test]
    fn test_a_relative_pattern_is_skipped_and_its_siblings_are_kept() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        registry.register(
            &go,
            "r1",
            &json!([
                { "globPattern": { "baseUri": "file:///work", "pattern": "**/*.go" } },
                { "globPattern": "**/*.mod" }
            ]),
        );

        assert!(
            registry
                .servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED)
                .is_empty(),
            "mcpls does not claim relativePatternSupport, so a relative \
             pattern is a server ignoring the advertised capability"
        );
        assert_eq!(
            registry.servers_for(&abs("go.mod"), lsp_types::FileChangeType::CHANGED),
            vec![go],
            "one unusable watcher must not discard the rest of the array"
        );
    }

    #[test]
    fn test_an_uncompilable_glob_is_skipped_rather_than_panicking() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        registry.register(&go, "r1", &json!([{ "globPattern": "**/[" }]));
        assert!(registry
            .servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED)
            .is_empty());
    }
}
```

- [ ] **Step 3: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core lsp::watched_files`
Expected: FAIL to compile, `cannot find type WatchRegistry`.

- [ ] **Step 4: implement**

```rust
//! Which language servers asked the client to watch which files.
//!
//! A server sends `client/registerCapability` for
//! `workspace/didChangeWatchedFiles` carrying an array of watchers, each a
//! glob pattern and an optional change-kind bitmask. This holds them so
//! that when mcpls learns a path changed it can tell exactly the servers
//! that asked, and no others.
//!
//! Kept apart from `LspClient` so the matching is testable without a live
//! server, and shared by reference between the client that writes it and
//! the translator that reads it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use globset::{GlobBuilder, GlobMatcher};

use crate::bridge::lock_std;
use crate::config::ServerId;

/// One watcher: a compiled glob and the kinds it wants.
#[derive(Debug)]
struct Watcher {
    glob: GlobMatcher,
    kinds: u32,
}

/// Create, change and delete, which is what an absent `kind` means.
const ALL_KINDS: u32 = 0b111;

impl Watcher {
    fn wants(&self, kind: lsp_types::FileChangeType) -> bool {
        let bit = match kind {
            lsp_types::FileChangeType::CREATED => 1,
            lsp_types::FileChangeType::CHANGED => 2,
            lsp_types::FileChangeType::DELETED => 4,
            _ => return false,
        };
        self.kinds & bit != 0
    }
}

/// Which servers asked to be told about which files.
#[derive(Debug, Default)]
pub struct WatchRegistry {
    /// server -> registration id -> that registration's watchers.
    by_server: Mutex<HashMap<ServerId, HashMap<String, Vec<Watcher>>>>,
}

impl WatchRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Store the watchers in a `client/registerCapability` payload's
    /// `registerOptions.watchers` array, under `id`.
    ///
    /// A watcher whose pattern is a `RelativePattern` object is logged and
    /// skipped: mcpls does not claim `relativePatternSupport`, so receiving
    /// one means a server ignored the advertised capability, and guessing
    /// at its base URI would be worse than a log line. A pattern that does
    /// not compile is skipped the same way. Either case leaves the rest of
    /// the array in force.
    pub fn register(&self, server: &ServerId, id: &str, watchers: &serde_json::Value) {
        let mut compiled = Vec::new();
        for watcher in watchers.as_array().into_iter().flatten() {
            let Some(pattern) = watcher.get("globPattern") else {
                continue;
            };
            let Some(pattern) = pattern.as_str() else {
                tracing::warn!(
                    %server,
                    registration = id,
                    "skipping a relative watcher pattern; mcpls does not claim \
                     relativePatternSupport"
                );
                continue;
            };
            // `literal_separator(true)` is not globset's default: without
            // it a `*` crosses a `/`, and LSP's glob grammar says it does
            // not.
            let glob = match GlobBuilder::new(pattern).literal_separator(true).build() {
                Ok(glob) => glob,
                Err(error) => {
                    tracing::warn!(%server, registration = id, pattern, %error, "skipping an uncompilable watcher glob");
                    continue;
                }
            };
            let kinds = watcher
                .get("kind")
                .and_then(serde_json::Value::as_u64)
                .and_then(|k| u32::try_from(k).ok())
                .unwrap_or(ALL_KINDS);
            compiled.push(Watcher {
                glob: glob.compile_matcher(),
                kinds,
            });
        }
        lock_std(&self.by_server)
            .entry(server.clone())
            .or_default()
            .insert(id.to_string(), compiled);
    }

    /// Drop the registration `id` holds for `server`.
    pub fn unregister(&self, server: &ServerId, id: &str) {
        if let Some(registrations) = lock_std(&self.by_server).get_mut(server) {
            registrations.remove(id);
        }
    }

    /// Drop every registration `server` holds.
    ///
    /// Called when a server is respawned: the fresh process registers again
    /// with new ids, so without this the old globs stay in force forever
    /// and a server that narrowed its watch keeps being told about files it
    /// no longer wants.
    pub fn forget_server(&self, server: &ServerId) {
        lock_std(&self.by_server).remove(server);
    }

    /// Servers that asked to hear about `path` changing this way, sorted so
    /// the notification order is reproducible.
    #[must_use]
    pub fn servers_for(&self, path: &Path, kind: lsp_types::FileChangeType) -> Vec<ServerId> {
        let registrations = lock_std(&self.by_server);
        let mut matched: Vec<ServerId> = registrations
            .iter()
            .filter(|(_, by_id)| {
                by_id
                    .values()
                    .flatten()
                    .any(|w| w.wants(kind) && w.glob.is_match(path))
            })
            .map(|(server, _)| server.clone())
            .collect();
        matched.sort_unstable();
        matched
    }
}
```

`servers_for`'s `matched.sort_unstable()` uses the `Ord` derive Task 2 added to `ServerId` (`config/routing.rs:30`). It is already there; do not add a second derive and do not sort by `to_string()`.

Add to `crates/mcpls-core/src/lsp/mod.rs`:

```rust
pub mod watched_files;
pub use watched_files::WatchRegistry;
```

- [ ] **Step 5: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core lsp::watched_files`
Expected: PASS, eight tests.

- [ ] **Step 6: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 7: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add Cargo.toml Cargo.lock crates/mcpls-core/Cargo.toml crates/mcpls-core/src/lsp/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(lsp): add a watched files registry

Which servers registered which globs, and which of them match a given
path and change kind. Kept beside the client rather than inside it so
the matching is testable without a live server, and shared by
reference between the client that writes it and the translator that
reads it.

Globs compile with literal_separator on: globset lets a star cross a
separator by default and LSP's grammar does not.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 7: advertise dynamic file watching and wire the registry to the client

The spec requires the advertisement and the notification in one commit, twice: B2 says "the advertisement and the notification must land in the same commit", and the Risks section says "Land the advertisement and the notification together, in one commit". The reason is that advertising `dynamicRegistration` moves gopls and tsgo out of their fallback watchers on the strength of the advertisement alone, so a commit that advertises without sending leaves those two servers seeing nothing at all. This task therefore carries both halves and ends in one commit. What was Task 8 is merged in here as Steps 6, 7 and 8.

**Files:**
- Modify: `crates/mcpls-core/src/lsp/client.rs` (`server_request_result` at `:762`, `server_request_response` at `:711`, `spawn_server_request_responder` at `:690`, `message_loop` at `:520` and `message_loop_inner` at `:557`, `from_transport` at `:177` and `from_transport_with_notifications` at `:209`)
- Modify: `crates/mcpls-core/src/lsp/lifecycle.rs` (`ServerInitConfig` at `:116`, `LspServer::spawn` at `:305`, the `spawn_batch` doctest at `:592-627`, `build_client_capabilities` at `:671`, the tripwire test at `:954`)
- Modify: `crates/mcpls-core/src/bridge/translator/mod.rs` (`resync_one_document` from Task 4, the struct at `:60`, the `with_*` builders near `:228`)
- Modify: `crates/mcpls-core/src/bridge/translator/testing.rs` (extend `TranslatorHarness` from Task 4)
- Modify: `crates/mcpls-core/src/bridge/translator/respawn.rs:281`
- Modify: `crates/mcpls-core/src/lib.rs` (`applicable_server_configs` at `:501`, `serve_with` at `:645-670`, `build_translator` at `:785`)
- Modify: `crates/mcpls-core/tests/integration/rust_analyzer_tests.rs:65`

**Interfaces:**
- Consumes: `WatchRegistry::{new, register, unregister, servers_for, forget_server}` from Task 6; `resync_one_document` and `close_one_document_locked` from Task 4.
- Produces:
  - `ServerInitConfig` gains `pub watch_registry: Option<Arc<WatchRegistry>>`, `None` for an embedder that does not want file watching.
  - `build_client_capabilities` advertises `workspace.didChangeWatchedFiles = { dynamicRegistration: Some(true), relativePatternSupport: None }`.
  - `LspClient::server_request_result(method, params, registry, server)`, four arguments.
  - `Translator::with_watch_registry(self, registry: Arc<WatchRegistry>) -> Self`.
  - `Translator::forget_watch_registrations(&self, server: &ServerId)`.
  - `Translator::notify_watched_files(&self, path: &Path, kind: lsp_types::FileChangeType)`, `pub(crate)` so Task 18's sweep in `crate::hooks::sweep` can reach it.
  - `build_translator` gains a `watch_registry: Arc<WatchRegistry>` parameter.
  - `applicable_server_configs` gains a `watch_registry: &Arc<WatchRegistry>` parameter.

- [ ] **Step 1: write the failing tests**

Every test module named below needs `#[allow(clippy::unwrap_used, clippy::expect_used)]` under its `#[cfg(test)]`. `client.rs`'s module at `:801` carries `#[allow(clippy::unwrap_used)]` today, so widen that one rather than adding a second attribute.

Replace the tripwire test in `crates/mcpls-core/src/lsp/lifecycle.rs:954`. It currently asserts the capability is absent, which is the behaviour this task inverts, so it is rewritten rather than deleted; the reasoning it carries stays, pointing the other way.

```rust
/// Advertising this is what moves gopls and tsgo out of their do-nothing
/// branches: gopls returns early from `registerWatchedDirectoriesLocked`
/// without it, and tsgo falls through to a watcher its own comment limits
/// to Windows and FSEvents. Both abandon their previous behaviour on the
/// strength of the advertisement alone, so mcpls must actually send the
/// notification, which is why this task carries both halves.
/// `relativePatternSupport` stays unclaimed: every watcher then arrives as
/// a plain glob string, which is one matching path instead of two.
#[test]
#[allow(clippy::expect_used)]
fn test_client_capabilities_claim_dynamic_file_watching() {
    let caps = build_client_capabilities(vec![], true);
    let watched = caps
        .workspace
        .expect("workspace capabilities are declared")
        .did_change_watched_files
        .expect("didChangeWatchedFiles capabilities are declared");

    assert_eq!(watched.dynamic_registration, Some(true));
    assert_eq!(watched.relative_pattern_support, None);
}
```

And in `crates/mcpls-core/src/lsp/client.rs`'s test module:

```rust
#[test]
fn test_a_watched_files_registration_reaches_the_registry() {
    let registry = Some(Arc::new(WatchRegistry::new()));
    let go = ServerId::from("go");
    let params = json!({
        "registrations": [{
            "id": "r1",
            "method": "workspace/didChangeWatchedFiles",
            "registerOptions": { "watchers": [{ "globPattern": "**/*.go" }] }
        }]
    });

    let result = LspClient::server_request_result(
        "client/registerCapability",
        Some(&params),
        &registry,
        &go,
    );

    assert_eq!(result.expect("the arm answers"), Value::Null);
    assert_eq!(
        registry
            .expect("the registry is present")
            .servers_for(Path::new("/work/main.go"), lsp_types::FileChangeType::CHANGED),
        vec![go]
    );
}

#[test]
fn test_an_unregister_drops_it_again() {
    let registry = Some(Arc::new(WatchRegistry::new()));
    let go = ServerId::from("go");
    let register = json!({
        "registrations": [{
            "id": "r1",
            "method": "workspace/didChangeWatchedFiles",
            "registerOptions": { "watchers": [{ "globPattern": "**/*.go" }] }
        }]
    });
    let unregister = json!({
        "unregisterations": [{ "id": "r1", "method": "workspace/didChangeWatchedFiles" }]
    });

    let _ = LspClient::server_request_result("client/registerCapability", Some(&register), &registry, &go);
    let _ = LspClient::server_request_result("client/unregisterCapability", Some(&unregister), &registry, &go);

    assert!(
        registry
            .expect("the registry is present")
            .servers_for(Path::new("/work/main.go"), lsp_types::FileChangeType::CHANGED)
            .is_empty()
    );
}

#[test]
fn test_a_registration_for_another_method_is_answered_and_ignored() {
    let registry = Some(Arc::new(WatchRegistry::new()));
    let go = ServerId::from("go");
    let params = json!({
        "registrations": [{
            "id": "r1",
            "method": "textDocument/semanticTokens",
            "registerOptions": {}
        }]
    });

    let result = LspClient::server_request_result(
        "client/registerCapability",
        Some(&params),
        &registry,
        &go,
    );

    assert_eq!(
        result.expect("registerCapability still answers null for every method"),
        Value::Null,
        "a server registering something mcpls does not track must not get an error"
    );
    assert!(
        registry
            .expect("the registry is present")
            .servers_for(Path::new("/work/main.go"), lsp_types::FileChangeType::CHANGED)
            .is_empty()
    );
}

#[test]
fn test_a_registration_with_no_registry_is_still_answered() {
    let result = LspClient::server_request_result(
        "client/registerCapability",
        Some(&json!({ "registrations": [] })),
        &None,
        &ServerId::from("go"),
    );

    assert_eq!(
        result.expect("the arm answers"),
        Value::Null,
        "an embedder that passes no registry must not turn every server's \
         registration into a JSON-RPC error"
    );
}
```

Note that the LSP spec spells the unregister field `unregisterations`, with the extra syllable. That is not a typo in this plan.

And in `crates/mcpls-core/src/bridge/translator/mod.rs`'s test module, the send side:

```rust
#[tokio::test]
async fn test_a_watching_server_is_told_an_applied_file_changed() {
    let harness = TranslatorHarness::with_one_server("go").await;
    harness.register_watcher("go", "r1", "**/*.go");
    let path = harness.write_file("main.go", "package main");
    harness.open(&path, "go").await;
    harness.rewrite_file(&path, "package main\nfunc a() {}");
    harness.queue_invalidation(&path);

    harness.translator.resync_changed_documents().await;

    assert!(
        harness
            .notifications_for("go")
            .contains(&"workspace/didChangeWatchedFiles".to_string()),
        "gopls at default settings runs no watcher of its own, so this \
         notification is the only way it learns a rename rewrote this file"
    );
}

#[tokio::test]
async fn test_a_server_that_registered_nothing_is_not_told() {
    let harness = TranslatorHarness::with_one_server("go").await;
    let path = harness.write_file("main.go", "package main");
    harness.open(&path, "go").await;
    harness.rewrite_file(&path, "package main\nfunc a() {}");
    harness.queue_invalidation(&path);

    harness.translator.resync_changed_documents().await;

    assert!(!harness
        .notifications_for("go")
        .contains(&"workspace/didChangeWatchedFiles".to_string()));
}

#[tokio::test]
async fn test_a_deleted_file_is_reported_as_deleted() {
    let harness = TranslatorHarness::with_one_server("go").await;
    harness.register_watcher("go", "r1", "**/*.go");
    let path = harness.write_file("main.go", "package main");
    harness.open(&path, "go").await;
    std::fs::remove_file(&path).expect("remove");
    harness.queue_invalidation(&path);

    harness.translator.resync_changed_documents().await;

    let params = harness.last_watched_files_params("go").expect("a notification went out");
    assert_eq!(
        params["changes"][0]["type"],
        serde_json::json!(3),
        "the kind comes from the file being absent, not from anything the \
         apply summary said, because a cancellation loses that summary"
    );
}

#[tokio::test]
async fn test_an_untracked_applied_file_is_still_reported() {
    let harness = TranslatorHarness::with_one_server("go").await;
    harness.register_watcher("go", "r1", "**/*.go");
    let path = harness.write_file("other.go", "package main");
    harness.queue_invalidation(&path);

    harness.translator.resync_changed_documents().await;

    assert!(
        harness
            .notifications_for("go")
            .contains(&"workspace/didChangeWatchedFiles".to_string()),
        "a rename's fanout writes files no tool call ever opened, and those \
         are exactly what a watching server has no other way to learn about"
    );
}

#[tokio::test]
async fn test_a_respawn_clears_that_server_s_registrations() {
    let harness = TranslatorHarness::with_one_server("go").await;
    harness.register_watcher("go", "r1", "**/*.go");
    harness.translator.forget_watch_registrations(&ServerId::from("go"));
    let path = harness.write_file("main.go", "package main");
    harness.open(&path, "go").await;
    harness.rewrite_file(&path, "package main\nfunc a() {}");
    harness.queue_invalidation(&path);

    harness.translator.resync_changed_documents().await;

    assert!(!harness
        .notifications_for("go")
        .contains(&"workspace/didChangeWatchedFiles".to_string()));
}
```

Extend `TranslatorHarness` from Task 4 with two more methods:

```rust
    /// Register `glob` for `server` under `id`, on the registry the harness
    /// handed the translator.
    pub(super) fn register_watcher(&self, server: &str, id: &str, glob: &str);
    /// The JSON params of the last `workspace/didChangeWatchedFiles` the
    /// fake server for `server` received, or `None` if it received none.
    pub(super) fn last_watched_files_params(&self, server: &str) -> Option<serde_json::Value>;
```

`with_one_server` therefore builds an `Arc<WatchRegistry>`, keeps it, and chains `.with_watch_registry(Arc::clone(&registry))` onto the translator it builds, so `register_watcher` and the translator see the same registry. `notifications_for` records the method name of every outbound notification, so it must record the params too for `last_watched_files_params` to read them back.

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core lsp::client::tests::test_a_watched_files lsp::lifecycle::tests::test_client_capabilities_claim translator::tests::test_a_watching_server`
Expected: FAIL to compile, `server_request_result` takes two arguments and there is no `register_watcher`.

- [ ] **Step 3: advertise the capability**

In `build_client_capabilities` (`crates/mcpls-core/src/lsp/lifecycle.rs:671`), inside the `WorkspaceClientCapabilities` literal beside `apply_edit`:

```rust
            did_change_watched_files: Some(lsp_types::DidChangeWatchedFilesClientCapabilities {
                dynamic_registration: Some(true),
                relative_pattern_support: None,
            }),
```

- [ ] **Step 4: thread the registry to the request handler**

The registry has to reach `server_request_result`, which runs on a task spawned by `spawn_server_request_responder` (`client.rs:690`) from inside the message loop. That loop is created by `tokio::spawn(Self::message_loop(...))` inside `from_transport_with_notifications` (`client.rs:209-238`), during construction, so a builder called on the finished `LspClient` could never reach it. That is why `apply_sink` is an `Arc<Mutex<Option<ApplySink>>>` cloned into the loop and written through afterwards by `set_apply_sink` (`client.rs:253`).

**The registry does not need that shape, and takes a constructor argument instead.** `apply_sink` is installed and removed per in-flight apply, so the loop has to observe a value that changes. The registry is built once in `serve_with` before any server spawns and never changes for the life of the process, so it can travel down as a plain `Option<Arc<WatchRegistry>>` with no lock and no extra `.await` in the loop's request arm. `LspClient` gets no field and no `with_watch_registry` builder, because the client itself never reads it. Do not reinstate one.

Concretely, add two parameters to each hop and pass them straight through:

- `from_transport_with_notifications(config, transport, notification_tx, watch_registry: Option<Arc<WatchRegistry>>, server: ServerId)`. `from_transport` (`:177`, `#[cfg(test)]`) keeps its signature and passes `None` and `config.server_config.id()` internally.
- `message_loop(transport, command_rx, command_tx, pending_requests, apply_sink, notification_tx, watch_registry, server)` and the same two on `message_loop_inner`, taken by reference there the way `apply_sink` is.
- In the `InboundMessage::Request` arm at `:641-652`, clone both alongside the existing `apply_sink.lock().await.clone()` and hand them to `spawn_server_request_responder`.
- `spawn_server_request_responder(command_tx, apply_sink, watch_registry, server, request)` moves both into the spawned task and passes them to `server_request_response`, which passes them to `server_request_result`.

Add to `ServerInitConfig` (`lifecycle.rs:116`):

```rust
    /// Where this server's `didChangeWatchedFiles` registrations are stored.
    ///
    /// `None` for an embedder that does not want file watching; the two
    /// capability arms then answer `null` and record nothing, which is what
    /// they did before this existed.
    pub watch_registry: Option<Arc<WatchRegistry>>,
```

and in `LspServer::spawn` (`:305`), where the client is actually constructed at `:353`, pass `config.watch_registry.clone()` and `config.server_config.id()` into `from_transport_with_notifications`. `spawn_batch` (`:628`) constructs nothing itself; it clones each config and calls `spawn`, so it needs no change.

`ServerInitConfig` has no `Default` impl and every construction site is a full struct literal, so all twenty need the new field. Nineteen get `watch_registry: None`:

- `crates/mcpls-core/src/lsp/lifecycle.rs` at `:1122, :1139, :1170, :1198, :1212, :1660, :1699, :1720, :1741, :1785, :1806, :1918, :1961, :1995, :2016`
- `crates/mcpls-core/src/lsp/lifecycle.rs` at `:599` and `:607`, the two literals inside the `spawn_batch` doctest, which are Rust that has to compile like any other
- `crates/mcpls-core/src/bridge/translator/respawn.rs:463`, in `stub_server_config`
- `crates/mcpls-core/tests/integration/rust_analyzer_tests.rs:65`

The twentieth is the production one, in Step 5.

Two of the nineteen are not compiled by the commands the later steps run. `cargo nextest run` skips doctests entirely and `cargo clippy --all-targets` does not build them either, so the two `spawn_batch` doctest literals can be forgotten with every local check green, and CI's `cargo test --doc --workspace --all-features` then fails on a struct literal missing a field. Step 10 exists to catch exactly that. Verify the enumeration against the source rather than trusting these line numbers, with `rg -n 'ServerInitConfig \{' crates/` from the repository root: the struct definition and `stub_server_config`'s return type match that pattern too, so the literal count is the match count minus those two.

`rust_analyzer_tests.rs` is in an integration-test crate rather than in the library, so run `cargo nextest run -p mcpls-core` rather than `cargo test --lib` to see it fail; `-p mcpls-core` compiles that target too.

Change the signature and the two arms in `client.rs`:

```rust
    fn server_request_result(
        method: &str,
        params: Option<&Value>,
        registry: &Option<Arc<WatchRegistry>>,
        server: &ServerId,
    ) -> std::result::Result<Value, JsonRpcError> {
        match method {
            "client/registerCapability" => {
                Self::record_watch_registrations(params, registry, server);
                Ok(Value::Null)
            }
            "client/unregisterCapability" => {
                Self::drop_watch_registrations(params, registry, server);
                Ok(Value::Null)
            }
            "workspace/workspaceFolders"
            | "workspace/diagnostic/refresh"
            | "workspace/semanticTokens/refresh"
            | "workspace/inlayHint/refresh"
            | "workspace/codeLens/refresh"
            | "window/showMessageRequest"
            | "window/workDoneProgress/create" => Ok(Value::Null),
            "workspace/configuration" => Ok(Self::workspace_configuration_result(params)),
            _ => Err(JsonRpcError {
                code: -32601,
                message: format!("Unhandled server request: {method}"),
                data: None,
            }),
        }
    }

    /// Store every `workspace/didChangeWatchedFiles` registration in a
    /// `client/registerCapability` payload. Registrations for other methods
    /// are answered and not recorded.
    fn record_watch_registrations(
        params: Option<&Value>,
        registry: &Option<Arc<WatchRegistry>>,
        server: &ServerId,
    ) {
        let Some(registry) = registry else { return };
        let Some(registrations) = params
            .and_then(|p| p.get("registrations"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for registration in registrations {
            if registration.get("method").and_then(Value::as_str)
                != Some("workspace/didChangeWatchedFiles")
            {
                continue;
            }
            let Some(id) = registration.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(watchers) = registration
                .get("registerOptions")
                .and_then(|o| o.get("watchers"))
            else {
                continue;
            };
            registry.register(server, id, watchers);
        }
    }

    /// Drop every `workspace/didChangeWatchedFiles` registration a
    /// `client/unregisterCapability` payload names. The protocol spells the
    /// field `unregisterations`.
    fn drop_watch_registrations(
        params: Option<&Value>,
        registry: &Option<Arc<WatchRegistry>>,
        server: &ServerId,
    ) {
        let Some(registry) = registry else { return };
        let Some(entries) = params
            .and_then(|p| p.get("unregisterations"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for entry in entries {
            if entry.get("method").and_then(Value::as_str)
                != Some("workspace/didChangeWatchedFiles")
            {
                continue;
            }
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                registry.unregister(server, id);
            }
        }
    }
```

- [ ] **Step 5: build the registry and pass it to both sides**

In `crates/mcpls-core/src/lib.rs`, immediately before the `applicable_server_configs` call at `:645`:

```rust
    // One registry for the process. The clients write it from their
    // `registerCapability` arms and the translator reads it to decide whom
    // to notify, so both sides must hold the same `Arc`.
    let watch_registry = Arc::new(lsp::WatchRegistry::new());
```

Give `applicable_server_configs` (`:501`) a `watch_registry: &Arc<WatchRegistry>` parameter and set `watch_registry: Some(Arc::clone(watch_registry))` in the `ServerInitConfig` literal at `:522`. That function has exactly one caller, so the parameter is the whole edit: setting the field in a loop afterwards would be a second place to forget.

The translator is not built inline. `build_translator` (`:785`) exists precisely so a `with_*` call cannot be added in one place and forgotten in another, and its doc comment at `:781` says so. Give it a parameter rather than chaining outside it:

```rust
fn build_translator(
    config: &ServerConfig,
    workspace_roots: Vec<PathBuf>,
    extension_map: HashMap<String, String>,
    router: ToolRouter,
    notification_cache: Arc<Mutex<NotificationCache>>,
    watch_registry: Arc<WatchRegistry>,
) -> Translator {
```

with `.with_watch_registry(watch_registry)` in its builder chain. Update both callers: `serve_with` at `:667` and the test at `:1758`, the latter with a fresh `Arc::new(lsp::WatchRegistry::new())`.

- [ ] **Step 6: add the send side to the translator**

Add the field to `Translator` (`bridge/translator/mod.rs:60`):

```rust
    /// Registrations from `client/registerCapability`, shared with the
    /// clients that write them.
    watch_registry: Option<Arc<WatchRegistry>>,
```

`Translator::new` sets it to `None`. Then:

```rust
    /// Attach the registry that decides which servers hear about a changed
    /// file.
    #[must_use]
    pub fn with_watch_registry(mut self, registry: Arc<WatchRegistry>) -> Self {
        self.watch_registry = Some(registry);
        self
    }

    /// Drop every watched-file registration `server` holds.
    pub fn forget_watch_registrations(&self, server: &ServerId) {
        if let Some(registry) = &self.watch_registry {
            registry.forget_server(server);
        }
    }

    /// Tell every server that registered a matching glob that `path`
    /// changed this way.
    ///
    /// Sent per server rather than broadcast: a server that did not ask is
    /// told nothing, which is the difference between this and shouting at
    /// everything with a language id.
    ///
    /// `pub(crate)` because stage C's sweep, in `crate::hooks::sweep`,
    /// reports untracked changed paths the same way.
    pub(crate) async fn notify_watched_files(
        &self,
        path: &Path,
        kind: lsp_types::FileChangeType,
    ) {
        let Some(registry) = &self.watch_registry else {
            return;
        };
        let Ok(uri) = crate::bridge::path_to_uri(path) else {
            return;
        };
        for server in registry.servers_for(path, kind) {
            let Some(client) = lock_std(&self.lsp_clients).get(&server).cloned() else {
                continue;
            };
            let params = lsp_types::DidChangeWatchedFilesParams {
                changes: vec![lsp_types::FileEvent {
                    uri: uri.clone(),
                    typ: kind,
                }],
            };
            if let Err(error) = client
                .notify("workspace/didChangeWatchedFiles", params)
                .await
            {
                tracing::warn!(%server, path = %path.display(), %error, "watched files notify failed");
            }
        }
    }
```

`crate::bridge::path_to_uri` is the crate's one path-to-URI conversion; `DocumentTracker` already uses it at `state.rs:379`. Do not add a second one.

In `resync_one_document` from Task 4, notify after deciding the case. The absent arm:

```rust
        if !path.exists() {
            self.close_one_document_locked(path).await;
            self.notify_watched_files(path, lsp_types::FileChangeType::DELETED)
                .await;
            return true;
        }
```

The untracked arm, which returns early on `Ok(None)` from `resync_from_disk`:

```rust
            Ok(None) => {
                self.notify_watched_files(path, lsp_types::FileChangeType::CHANGED)
                    .await;
                return true;
            }
```

And the present-and-tracked case, after the change and save loops and before `true`:

```rust
        self.notify_watched_files(path, lsp_types::FileChangeType::CHANGED)
            .await;
        true
```

A file the apply created is reported as `CHANGED` rather than `CREATED`. The queue carries bare paths, so nothing at this point distinguishes the two, and every server that accepts one accepts the other.

- [ ] **Step 7: clear a respawned server's registrations**

In `crates/mcpls-core/src/bridge/translator/respawn.rs`, beside `self.document_tracker.forget_server(id)` at `:281`:

```rust
        self.forget_watch_registrations(id);
```

A respawned process registers again with fresh ids, so without this the old globs stay in force for the life of the mcpls process.

- [ ] **Step 8: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core`
Expected: PASS.

- [ ] **Step 9: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 10: compile the doctests**

Run: `cargo test --doc -p mcpls-core`
Expected: PASS.

This is not redundant with Steps 8 and 9 and must not be deleted as such. Neither `cargo nextest run` nor `cargo clippy --all-targets` builds doctests, so the two `ServerInitConfig` literals in the `spawn_batch` doctest are invisible to both, and CI runs `cargo test --doc --workspace --all-features`. Without this step the task can look finished locally and break CI on a missing struct field.

- [ ] **Step 11: commit, both halves together**

One commit, staging the advertisement, the registry plumbing and the notification. Do not split it: the spec forbids an intermediate state in which mcpls advertises `dynamicRegistration` without sending the notification, because gopls and tsgo abandon their fallback watchers on the advertisement alone.

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add \
  crates/mcpls-core/src/lsp/ \
  crates/mcpls-core/src/bridge/translator/ \
  crates/mcpls-core/src/lib.rs \
  crates/mcpls-core/tests/integration/rust_analyzer_tests.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(lsp): claim and send didChangeWatchedFiles

Advertise dynamicRegistration, record what each server registers, and
tell the matching servers about every file an apply wrote.

The three land together because gopls and tsgo abandon their fallback
watchers on the advertisement alone: a commit that advertised without
sending would leave both seeing nothing at all.

The registry travels to the client as a constructor argument rather
than a builder, because the message loop that answers registerCapability
is spawned during construction and no later builder could reach it.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 8: tell watching servers what an apply wrote

Merged into Task 7. Nothing to do here.

The spec requires the `didChangeWatchedFiles` advertisement and the notification to land in one commit, in B2 and again in Risks, because advertising `dynamicRegistration` moves gopls and tsgo off their fallback watchers on the strength of the advertisement alone. Two tasks meant two commits and an intermediate state in which those two servers saw nothing, so this task's steps are Steps 6, 7 and 8 of Task 7 and its commit is Task 7's.

The heading stays so cross-references and the task-brief extractor still resolve, and so Tasks 9 through 22 keep their numbers.

---


## Task 9: prove watched files against a real server

Task 1's measurement is in. Pyrefly does **not** publish diagnostics for a file it never received a `didOpen` for, so the original plan's branch A, which asserted a publish for an unopened file, cannot pass. Its branch B named gopls as the fallback, and `command -v gopls` returns nothing on this machine: the installed servers are rust-analyzer, pyrefly 1.2.0, tsgo, ty and taplo. A branch nobody can run is not a plan, so both branches are gone and this task has one shape.

**The dependency is reversed instead.** The measurement also showed that pyrefly does register watchers covering the workspace and does act on `workspace/didChangeWatchedFiles`: it re-published for the document it holds open 2 ms after the notification arrived. So the provable claim is the one B2 actually rests on. Open the caller, change the definition, and assert the caller's diagnostics move. That needs no server this machine lacks, and it is exactly what "the notification reached the server and changed its analysis of an open document" means.

**Files:**
- Create: `crates/mcpls-core/tests/fixtures/python_workspace/{pyrefly.toml,a.py,b.py,c.py}`
- Create: `crates/mcpls-core/tests/pyrefly_e2e.rs`
- Modify: `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`, the B2 server table's pyrefly row and its pyrefly bullet

**Interfaces:**
- Consumes: Task 7's advertisement, registry and `workspace/didChangeWatchedFiles` notification.
- Produces: nothing later tasks depend on.

- [ ] **Step 1: build the pyrefly fixture**

Three source files and a config. The shape is deliberate and every part of it carries weight, so read the reasoning before changing any of it.

`crates/mcpls-core/tests/fixtures/python_workspace/pyrefly.toml`:

```toml
project-includes = ["**/*.py"]
```

`a.py`, the file nothing opens:

```python
def greet(name: str) -> str:
    return "hello " + name


def helper(value: int) -> int:
    return value + 1
```

`b.py`, the file the test opens and never writes:

```python
from a import greet

RESULT = greet("world")
```

`c.py`, the file the rename is anchored in:

```python
from a import helper

USED = helper(1)
```

The rename is `helper` to `greet`, anchored at `c.py`'s reference. That writes `a.py`, where the definition lives, and `c.py`, where the reference lives. It does not write `b.py`.

Afterwards `a.py` holds two `def greet`, and Python's later definition shadows the earlier, so the surviving `greet` takes an `int`. `b.py` still reads `greet("world")`, which is now a type error, and it is an error only against `a.py`'s **new** content: against the pre-apply `a.py`, where `greet` takes a `str`, that same line is fine. That asymmetry is the whole test. A collision that errored under both the old and the new content would prove nothing, because pyrefly's stale view would report it too.

`b.py` receives no `didChange` and no `didSave`, because the apply never wrote it, and `a.py` receives no `didOpen`, because no tool call names it. So the only path by which the error can reach the flush is the `workspace/didChangeWatchedFiles` naming `a.py` that Task 7 sends for an untracked applied path.

- [ ] **Step 2: write the failing e2e**

Copy `ra_e2e.rs`'s crate-level attribute block into the new file first, or it will not build under `-D warnings`:

```rust
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_docs_in_private_items,
    missing_docs
)]
```

Then create `crates/mcpls-core/tests/pyrefly_e2e.rs` in the shape `ra_e2e.rs` already uses: one `#[test] #[ignore = "..."] fn pyrefly_e2e_suite()`, a binary resolver that skips when `MCPLS_SKIP_PYREFLY=1` is set and panics when the binary is missing without it, `stage_workspace()` copying the fixture into a fresh `TempDir`, a generated mcpls config naming pyrefly as the only server with `apply.rename = true`, and synchronous sub-cases of the form `fn sc_x(client: &mut McpClient, workspace: &Path) -> Result<(), String>`. Reuse `crates/mcpls-core/tests/e2e/mcp_client.rs` the way `ra_e2e.rs` does; `McpClient::call_tool` takes `(&mut self, name: &str, arguments: &Value)`.

The config's server block:

```rust
        lsp_servers: vec![LspServerConfig {
            language_id: "python".to_owned(),
            command: pyrefly_path.to_string_lossy().into_owned(),
            args: vec!["lsp".to_owned()],
            file_patterns: vec!["**/*.py".to_owned()],
        }],
```

The sub-case:

```rust
/// A watched-files notification reaches pyrefly and changes its analysis of
/// a document it holds open.
///
/// `b.py` is opened through a read tool and never written. `a.py` is
/// written by the apply's fanout and never opened. Afterwards `b.py`'s call
/// is a type error, and it is one only against `a.py`'s new content, so the
/// error can only have arrived because mcpls told pyrefly that `a.py`
/// changed and pyrefly re-analysed the document it holds.
fn sc_watched_files_reaches_pyrefly(
    client: &mut McpClient,
    workspace: &Path,
) -> Result<(), String> {
    // Open b.py through a read tool, so pyrefly holds a document for it.
    let b = workspace.join("b.py");
    client
        .call_tool(
            "get_hover",
            &json!({
                "file_path": b.to_string_lossy(),
                "line": find_line(&b, "RESULT = greet"),
                "character": 9,
            }),
        )
        .map_err(|e| format!("opening b.py failed: {e}"))?;

    // Drain whatever the baseline and that open produced, so the assertion
    // below is about what this apply caused.
    let _ = client
        .call_tool("get_new_diagnostics", &json!({}))
        .map_err(|e| format!("priming flush failed: {e}"))?;

    let c = workspace.join("c.py");
    let resp = client
        .call_tool(
            "rename_symbol",
            &json!({
                "file_path": c.to_string_lossy(),
                "line": find_line(&c, "USED = helper"),
                "character": 7,
                "new_name": "greet",
                "apply": true,
            }),
        )
        .map_err(|e| format!("rename call failed: {e}"))?;
    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;
    if inner["applied"] != json!(true) {
        return Err(format!("expected applied=true, got {inner}"));
    }
    let written: Vec<String> = inner["files_written"]
        .as_array()
        .ok_or_else(|| format!("expected files_written array, got {inner}"))?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    if !written.iter().any(|p| p.ends_with("a.py")) {
        return Err(format!("the apply must have rewritten a.py; wrote {written:?}"));
    }
    if written.iter().any(|p| p.ends_with("b.py")) {
        return Err(format!(
            "the apply must not have rewritten b.py, or a didChange would \
             deliver the error and this sub-case would prove nothing; \
             wrote {written:?}"
        ));
    }

    let deadline = Instant::now() + Duration::from_millis(settle_deadline_ms());
    let mut last = Value::Null;
    loop {
        let raw = client
            .call_tool("get_new_diagnostics", &json!({}))
            .map_err(|e| format!("flush call failed: {e}"))?;
        let body = assertions::assert_tool_ok(&raw);
        let report: Value = serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))?;
        let omitted = report["omitted"].as_u64().unwrap_or(0);
        let starting_up = report.get("note").is_some() && omitted == 0;
        if !starting_up {
            let hit = report["changed"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|file| {
                    file["file_path"].as_str().is_some_and(|p| p.ends_with("b.py"))
                        && file["diagnostics"]
                            .as_array()
                            .is_some_and(|ds| !ds.is_empty())
                });
            if hit {
                return Ok(());
            }
            last = report;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no diagnostic for b.py arrived within the settle deadline. \
                 b.py was never written and a.py was never opened, so this \
                 failing means the workspace/didChangeWatchedFiles for a.py \
                 either did not go out or pyrefly did not act on it. Check \
                 the mcpls log for 'skipping a relative watcher pattern' \
                 before assuming the former. Last report: {last}"
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
```

Two ways this can fail that are findings rather than bugs, and both must be reported rather than worked around:

- Pyrefly refuses the rename because of the collision. rust-analyzer does not check a rename for conflicts and pyrefly is assumed not to either; if it does, the apply never lands and B2 needs a different trigger.
- Pyrefly sends its watchers as `RelativePattern` objects rather than plain glob strings. mcpls does not claim `relativePatternSupport`, so Task 6's registry logs `skipping a relative watcher pattern` at `warn` and records nothing, and no notification can match. That is the log line Unresolved question 4 exists to catch. Report it; do not add the relative-pattern branch inside this task.

- [ ] **Step 3: run it without the notification and watch it fail**

There is no revision in which mcpls advertises the capability but does not send the notification, because Task 7 ships both in one commit, and the spec forbids one existing without the other. So the RED run is against the commit before Task 7, where neither exists. That confirms the sub-case fails without the feature; it cannot say which half was missing, and it does not need to.

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc log --oneline -3
# note the task-6 commit sha, "feat(lsp): add a watched files registry"
git -C /home/lev/Git/lev/mcpls-diag-bc worktree add /tmp/mcpls-pre-b2 <task-6 sha>
git -C /home/lev/Git/lev/mcpls-diag-bc diff -- crates/mcpls-core/tests/ \
  | git -C /tmp/mcpls-pre-b2 apply
cp -r /home/lev/Git/lev/mcpls-diag-bc/crates/mcpls-core/tests/fixtures/python_workspace \
      /tmp/mcpls-pre-b2/crates/mcpls-core/tests/fixtures/
cp /home/lev/Git/lev/mcpls-diag-bc/crates/mcpls-core/tests/pyrefly_e2e.rs \
   /tmp/mcpls-pre-b2/crates/mcpls-core/tests/
cargo nextest run --manifest-path /tmp/mcpls-pre-b2/Cargo.toml \
  -p mcpls-core --test pyrefly_e2e -- --ignored
git -C /home/lev/Git/lev/mcpls-diag-bc worktree remove --force /tmp/mcpls-pre-b2
```

The fixture and the suite file are new and untracked, so `git diff` does not carry them and they are copied in explicitly.

Expected there: FAIL on the "no diagnostic for b.py arrived" message.

- [ ] **Step 4: run it on the current tree and watch it pass**

Run: `cargo nextest run -p mcpls-core --test pyrefly_e2e -- --ignored`
Expected: PASS.

- [ ] **Step 5: narrow the spec's pyrefly entry**

The measurement contradicts one line of the spec, and the spec is the binding authority for every task after this one, so the correction lands with the test that establishes it rather than in a later cleanup.

In `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`, in B2's server table, change the pyrefly row's "Needs the client to watch" cell from `yes` to `for currency, not delivery`.

Then extend B2's pyrefly bullet, which currently reads:

> **pyrefly** registers `FileSystemWatcher` patterns with the client (`pyrefly/lib/lsp/non_wasm/server.rs:5811`); the `notify`-based watcher elsewhere in that codebase belongs to the CLI `check` command.

with a second sentence:

> Measured against pyrefly 1.2.0, it acts on the notification but publishes only for documents it holds open, so watching buys it analysis currency for open documents rather than diagnostic delivery for unopened ones: `docs/superpowers/notes/2026-09-07-stage-b-measurements.md`, "Pyrefly on an unopened file".

Change nothing else in the spec. In particular, leave the "So the payoff is gopls at default settings, tsgo on Linux, pyrefly, and ty" sentence alone: it is still true under the narrower reading, and rewriting it is a second claim this measurement does not support.

- [ ] **Step 6: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 7: commit**

The spec edit goes in this commit, not a separate one: it is the conclusion the test proves.

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add \
  crates/mcpls-core/tests/pyrefly_e2e.rs \
  crates/mcpls-core/tests/fixtures/python_workspace \
  docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
test(e2e): drive watched files against pyrefly

Open the caller, rewrite the definition, and assert the caller's
diagnostics move. b.py is never written and a.py is never opened, so
the only path for the error is the didChangeWatchedFiles naming a.py.

Pyrefly publishes nothing for a file it never opened, measured, so the
spec's B2 entry for it is narrowed to say the notification buys
analysis currency for open documents rather than delivery for unopened
ones.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---


## Task 10: stop the flush cloning the whole cache

**Files:**
- Modify: `crates/mcpls-core/src/mcp/server.rs` (`get_new_diagnostics` at `:736`, `new_diagnostics_payload` at `:770`, `routable_entries` near `:234`, the test module at `:1183`)
- Modify: `crates/mcpls-core/src/bridge/notifications.rs` (beside `diagnostics_snapshot` at `:734`)
- Modify: `crates/mcpls-core/src/lib.rs` (`baseline_task` at `:1088`)

**Interfaces:**
- Produces:
  - `DiagnosticSource { uri: Uri, version: Option<i32>, owner: ServerId }`, and `new_diagnostics_payload(&self, report: &FlushReport, sources: &HashMap<String, DiagnosticSource>) -> NewDiagnosticsResult`
  - `McplsServer::flush_now(&self, session: &SessionId) -> NewDiagnosticsResult`: the flush and its payload, without the tool wrapper and without the baseline guard, so the tool, the footer (Task 13) and the socket op (Task 19) all run the same code against the same record
  - `NotificationCache::diagnostics_entries(&self) -> Vec<(&str, &DiagnosticInfo, &ServerId)>`
  - `TestServer` and `test_server_with_baseline()` in `mcp::server`'s test module, which Task 13 reuses

- [ ] **Step 1: write the failing test**

`McplsServer` has no context-taking constructor. Its only constructor is `McplsServer::new` (`mcp/server.rs:262`), which takes seven positional arguments and builds the private `BridgeContext` itself; `create_test_server` at `:1198` shows the call. So a test cannot reach the server's cache through the server. Build the `Arc`s first, keep clones, and hand copies to `new`.

Add this beside `default_delivery_and_floors` at `:1190`, and first widen that module's `#[allow(clippy::unwrap_used)]` at `:1183` to `#[allow(clippy::unwrap_used, clippy::expect_used)]`, since these tests use `.expect(...)`.

```rust
    /// An `McplsServer` together with the `Arc`s it shares, so a test can
    /// reach the same cache and the same delivery record the server sees.
    ///
    /// `McplsServer::new` moves its arguments into a private
    /// `BridgeContext`, so a test that needs both sides keeps its own
    /// clones from before the call.
    struct TestServer {
        server: McplsServer,
        notification_cache: Arc<Mutex<NotificationCache>>,
        delivery: Arc<Mutex<DiagnosticsDelivery>>,
    }

    fn test_server_parts() -> TestServer {
        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let (delivery, floors) = default_delivery_and_floors();
        let server = McplsServer::new(
            translator,
            Arc::clone(&notification_cache),
            workspace_roots,
            subscriptions,
            false,
            Arc::clone(&delivery),
            floors,
        );
        TestServer {
            server,
            notification_cache,
            delivery,
        }
    }

    /// The same, with an empty baseline adopted so `has_baseline()` is true
    /// and the flush is not answered with `starting_up()`.
    async fn test_server_with_baseline() -> TestServer {
        let parts = test_server_parts();
        parts.delivery.lock().await.set_baseline(HashMap::new());
        parts
    }
```

```rust
/// A guard for the change rather than a red-first test: it passes against
/// the current code, which clones the snapshot and releases the lock, and it
/// must keep passing afterwards. What it catches is the naive shape of the
/// fix, borrowing out of the cache guard all the way through the payload
/// build.
#[tokio::test]
async fn test_a_flush_does_not_hold_the_cache_lock_while_building_its_payload() {
    let parts = test_server_with_baseline().await;
    let cache = Arc::clone(&parts.notification_cache);
    let server = parts.server;

    // Hold the cache lock from another task the moment the flush is in
    // flight. If the flush holds it across its payload build, this never
    // acquires and the timeout fires.
    let flush = tokio::spawn(async move { server.get_new_diagnostics().await });
    tokio::task::yield_now().await;
    let grabbed = tokio::time::timeout(std::time::Duration::from_secs(5), cache.lock()).await;

    assert!(
        grabbed.is_ok(),
        "the diagnostics pump takes this same lock, and the transport drops \
         notifications on a full channel rather than blocking, so a flush \
         that holds it across its awaits loses publishes under hook traffic"
    );
    flush.await.expect("the flush task").expect("the flush");
}
```

- [ ] **Step 2: run the test and watch it fail or pass for the wrong reason**

Run: `cargo nextest run -p mcpls-core mcp::server::tests::test_a_flush_does_not_hold`
Expected: PASS against the current code, because the current code clones and releases. This test is a guard for the change, not a red-first test: it must keep passing after step 3, and it is what catches the naive "borrow all the way through" implementation. Say that in the test's doc comment.

- [ ] **Step 3: implement**

Replace `get_new_diagnostics`'s body between the baseline guard and the payload build:

```rust
        let session = SessionId::process_default();

        // `delivery` first, then the cache. The flush borrows its entries
        // straight out of the cache guard, so both are held together; taking
        // them in this order everywhere is what keeps that from deadlocking.
        // Neither guard outlives this block: `new_diagnostics_payload` awaits
        // per changed file, and holding the cache lock across those awaits
        // would block the diagnostics pump, which loses publishes rather than
        // waiting for them.
        to_tool_result(Ok(self.flush_now(&session).await))
```

with `flush_now` holding the whole sequence, so the footer in Task 13 and the socket op in Task 19 reach the same record through the same code:

```rust
    /// Flush `session`'s record and render it.
    ///
    /// The caller checks `has_baseline()` first. Both the tool and the
    /// footer must, because `flush` seeds a session's record from the
    /// baseline and `set_baseline` never rewrites one that already exists.
    async fn flush_now(&self, session: &SessionId) -> NewDiagnosticsResult {
        let (report, sources) = {
            let mut delivery = self.context.delivery.lock().await;
            let cache = self.context.notification_cache.lock().await;
            let entries = routable_entries_borrowed(&cache, &self.context.floors);
            let report = delivery.flush(session, &entries);
            let sources = source_map(&cache, &report);
            (report, sources)
        };
        self.new_diagnostics_payload(&report, &sources).await
    }
```

with:

```rust
/// `FileEntry` values borrowing from a held cache guard.
fn routable_entries_borrowed<'a>(
    cache: &'a NotificationCache,
    floors: &FloorTable,
) -> Vec<FileEntry<'a>> { ... }

/// What the payload build needs about one cached entry, after the cache
/// guard is gone.
///
/// Carries `version` as well as the URI and the owner, because
/// `new_diagnostics_payload` rebuilds a `DiagnosticInfo` from these three
/// before handing it to `Translator::diagnostics_from_cache_entry`
/// (`mcp/server.rs:784-789`). A pair of URI and owner alone would silently
/// change what the converter sees.
#[derive(Debug, Clone)]
struct DiagnosticSource {
    uri: Uri,
    version: Option<i32>,
    owner: ServerId,
}

/// The URI, version and owning server of every key a report names, cloned
/// so the payload can be built after both guards are released.
fn source_map(
    cache: &NotificationCache,
    report: &FlushReport,
) -> HashMap<String, DiagnosticSource> { ... }
```

`routable_entries_borrowed` does what `routable_entries` does today but reads the cache's entries in place rather than a cloned snapshot. That needs a borrowing accessor on `NotificationCache`; add one beside `diagnostics_snapshot`:

```rust
    /// Every cached entry with a known owner, ordered by key.
    ///
    /// Borrows rather than cloning, so a caller that only needs to hash or
    /// filter does not copy up to 1000 entries of up to 1 MiB each. The
    /// caller holds the lock for as long as it holds the result, so it must
    /// not await while it does.
    #[must_use]
    pub fn diagnostics_entries(&self) -> Vec<(&str, &DiagnosticInfo, &ServerId)> { ... }
```

Keep `diagnostics_snapshot` for any other caller; if this task leaves it with none, delete it and its test.

`new_diagnostics_payload` takes `sources` instead of `snapshot` and looks each key up there. Its body keeps rebuilding the entry it hands to the converter, now from the source struct:

```rust
            let entry = DiagnosticInfo {
                uri: source.uri.clone(),
                version: source.version,
                diagnostics: file.diagnostics.clone(),
            };
```

The `cleared` list is built from the same map, so a cleared key with no source entry is skipped exactly as it is today.

In `crates/mcpls-core/src/lib.rs`, `baseline_task` at `:1088` does the same clone for the same reason. Give it the borrowing accessor too: take the cache lock, build the hash map from borrowed entries, release, then `set_baseline`. It takes `delivery` only after the cache guard is gone, which is the opposite order from the flush; that is safe because it never holds both at once, and a comment should say so, naming the plan's delivery-before-cache rule so the next reader does not read this as a violation of it.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core`
Expected: PASS.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/mcp/server.rs crates/mcpls-core/src/bridge/notifications.rs crates/mcpls-core/src/lib.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
perf(mcp): flush without cloning the whole cache

The flush deep-cloned every cached entry, bounded at a thousand
entries of up to a mebibyte each, then reduced that to a small changed
set. Borrow out of the cache guard for the flush itself and clone only
the source of each key the report names.

The guards are still dropped before the payload build: it awaits per
changed file, and holding the cache lock across those awaits would
block the diagnostics pump, which drops publishes rather than waiting.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 11: bound cleared files and stop reporting muted ones as fixed

**Files:**
- Modify: `crates/mcpls-core/src/bridge/delivery.rs` (`flush` at `:161-225`)
- Test: `crates/mcpls-core/src/bridge/delivery.rs` test module

**Interfaces:**
- Produces: no signature change. `FlushReport::omitted` now also counts cleared files the budget deferred.

- [ ] **Step 1: write the failing tests**

The test module at `crates/mcpls-core/src/bridge/delivery.rs:264` already has `diagnostic(line, severity, message)` at `:270` and `entry(key, diagnostics, floor)` at `:282`. Use those; do not add parallel helpers. Give the module `#[allow(clippy::unwrap_used, clippy::expect_used)]` under its `#[cfg(test)]`.

A budget of two cannot seed four records in one flush: the seeding flush would deliver `a` and `b`, defer `c` and `d` with no record entry, and the clearing flush would then reach `(None, None)` for those two and report nothing. So the seeding is two flushes of two files each, which every flush's budget covers, and the budget only binds on the clearing flush where the finding lives.

```rust
#[test]
fn test_cleared_files_spend_the_total_budget() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
        max_total: 2,
        ..DiagnosticsConfig::default()
    });
    let session = SessionId::from("s".to_string());
    let broken = vec![diagnostic(0, DiagnosticSeverity::ERROR, "boom")];

    // Seed all four records two at a time, so each seeding flush fits the
    // budget of two and every key has a recorded hash to clear against.
    delivery.flush(
        &session,
        &[
            entry("a", &broken, SeverityFloor::Warning),
            entry("b", &broken, SeverityFloor::Warning),
        ],
    );
    delivery.flush(
        &session,
        &[
            entry("c", &broken, SeverityFloor::Warning),
            entry("d", &broken, SeverityFloor::Warning),
        ],
    );

    // Now all four are fixed at once, under a budget of two.
    let report = delivery.flush(
        &session,
        &[
            entry("a", &[], SeverityFloor::Warning),
            entry("b", &[], SeverityFloor::Warning),
            entry("c", &[], SeverityFloor::Warning),
            entry("d", &[], SeverityFloor::Warning),
        ],
    );

    assert_eq!(
        report.cleared,
        vec!["a".to_string(), "b".to_string()],
        "max_total is one shared context budget and a cleared line spends \
         from it like any other; a workspace-wide fix could otherwise emit \
         up to a thousand of them. The pass is key-ordered, so which two \
         land is reproducible"
    );
    assert_eq!(report.omitted, 2);
}

#[test]
fn test_a_deferred_cleared_file_is_offered_again() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
        max_total: 2,
        ..DiagnosticsConfig::default()
    });
    let session = SessionId::from("s".to_string());
    let broken = vec![diagnostic(0, DiagnosticSeverity::ERROR, "boom")];

    delivery.flush(
        &session,
        &[
            entry("a", &broken, SeverityFloor::Warning),
            entry("b", &broken, SeverityFloor::Warning),
        ],
    );
    delivery.flush(
        &session,
        &[
            entry("c", &broken, SeverityFloor::Warning),
            entry("d", &broken, SeverityFloor::Warning),
        ],
    );

    let all_clean = [
        entry("a", &[], SeverityFloor::Warning),
        entry("b", &[], SeverityFloor::Warning),
        entry("c", &[], SeverityFloor::Warning),
        entry("d", &[], SeverityFloor::Warning),
    ];
    let first = delivery.flush(&session, &all_clean);
    let second = delivery.flush(&session, &all_clean);

    let mut seen: Vec<String> = first.cleared;
    seen.extend(second.cleared);
    seen.sort();
    assert_eq!(
        seen,
        vec!["a".to_string(), "b".to_string(), "c".to_string(), "d".to_string()],
        "a deferred cleared file keeps its record entry, which is the same \
         deferral rule a deferred changed file already follows"
    );
}

#[test]
fn test_a_muted_file_is_dropped_from_the_record_rather_than_reported_fixed() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let session = SessionId::from("s".to_string());
    let broken = vec![diagnostic(0, DiagnosticSeverity::ERROR, "boom")];

    let _ = delivery.flush(&session, &[entry("a", &broken, SeverityFloor::Error)]);
    let report = delivery.flush(&session, &[entry("a", &broken, SeverityFloor::Off)]);

    assert!(
        report.cleared.is_empty(),
        "the file still has an error; only the floor changed, and telling the \
         agent its problems are gone is a lie"
    );
    assert!(report.changed.is_empty());
}

#[test]
fn test_a_genuinely_fixed_file_is_still_reported_cleared() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let session = SessionId::from("s".to_string());
    let broken = vec![diagnostic(0, DiagnosticSeverity::ERROR, "boom")];

    let _ = delivery.flush(&session, &[entry("a", &broken, SeverityFloor::Error)]);
    let report = delivery.flush(&session, &[entry("a", &[], SeverityFloor::Error)]);

    assert_eq!(report.cleared, vec!["a".to_string()]);
}
```

`DiagnosticsConfig` is `Copy`, so the struct-update syntax above copies the defaults rather than moving them.

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core bridge::delivery::tests::test_cleared_files_spend bridge::delivery::tests::test_a_muted_file`
Expected: FAIL, `cleared.len()` is 4 and the muted file is reported cleared.

- [ ] **Step 3: implement**

In `flush`, replace the `(None, Some(_))` arm:

```rust
                (None, Some(_)) if entry.floor == SeverityFloor::Off => {
                    // Muted, not fixed. Dropping the record without
                    // reporting means the file starts fresh if its floor
                    // ever rises again, and the agent is not told its
                    // problems are gone when they were only silenced.
                    record.remove(entry.key);
                }
                (None, Some(_)) => match budget {
                    Some(0) => {
                        // Leave the record in place so the next flush
                        // offers this file again, the same deferral a
                        // changed file gets.
                        report.omitted += 1;
                    }
                    _ => {
                        if let Some(remaining) = budget.as_mut() {
                            *remaining -= 1;
                        }
                        record.remove(entry.key);
                        report.cleared.push(entry.key.to_string());
                    }
                },
```

`budget` is `Option<usize>`, `None` meaning unlimited, so `Some(0)` is the exhausted case and `None` passes through. Both arms are reached in the one key-ordered pass `flush` already makes, so which files a binding budget reaches stays reproducible.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core bridge::delivery`
Expected: PASS.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/bridge/delivery.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
fix(bridge): budget cleared files, keep muted ones

A wide apply that fixed many files could emit one "problems are gone"
line per file while the changed files beside them were budgeted, so
cleared files now spend from the same total budget and a deferred one
keeps its record entry for the next flush.

A file whose floor drops to off is dropped from the record without
being reported cleared: its problems were silenced, not fixed.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 12: the footer's clock and quiet judgment

**Files:**
- Modify: `crates/mcpls-core/src/bridge/settle.rs`
- Modify: `crates/mcpls-core/src/config/mod.rs` (`DiagnosticsConfig`, the struct at `:134-167` and its `Default` impl running to about `:207`)

### A defect this task must also fix

Task 9 uncovered a defect in the same file, confirmed against the code rather
than measured, and it defeats the delivery this whole stage exists to provide
for exactly the servers that are fastest.

`ServerSettle::begin` and `end` are fed from one source only, `$/progress`
begin and end (`lib.rs:196-197`). `quiet_since` is stamped only inside `end`,
and `end` returns early unless a matching `begin` was recorded
(`settle.rs:90-98`). `should_settle_at` is the deadline being reached, or
`outstanding` being empty with `quiet_since` old enough (`settle.rs:107-113`).
So for a server that never completes a `$/progress` operation, `quiet_since`
stays `None` forever and the second disjunct is unreachable. `restart_deadline`
moves the deadline without stamping `quiet_since` (`settle.rs:67-72`), so a
spawn does not rescue it either.

`default_settle_deadline_ms` is 300000 (`config/mod.rs:194-196`), and
`baseline_task` polls until `should_settle` (`lib.rs:1115`), while
`get_new_diagnostics` returns `starting_up()` until then
(`mcp/server.rs:738-742`). The consequence: such a server delivers nothing for
five minutes, and at the deadline the baseline absorbs everything it published
in that window, so those diagnostics can never be reported as new. pyrefly, ty
and taplo are all in this class.

`settle.rs:9-12` names this case and treats the deadline as the answer, but
that deadline is sized for rust-analyzer's indexing, so it hands the
no-progress server the worst case instead of the best.

Fix it here, because this task already owns the file and is about exactly this
judgment. Give a server that has produced no progress at all a much shorter
grace, or stamp `quiet_since` at `restart_deadline` and let `quiet_for` do the
work. Do not change the five-minute backstop itself: it is correct for the
server it was sized for, and shortening it would trade this defect for a
different one.

Write a test that fails without the fix: a settle tracker that never receives
a `begin` must report settled well before the deadline. Sabotage-check it by
reverting the fix and confirming it goes red.

`crates/mcpls-core/tests/pyrefly_e2e.rs` sets `settle_deadline_ms = 500` to
work around this. Once the fix lands, check whether that override is still
needed and say either way in the commit body.

Read `docs/superpowers/notes/2026-09-07-stage-b-measurements.md`, the "Cargo check timing" section, before setting `footer_wait_ms`. It measured about 4.3 seconds for an edit in `mcpls-core` and about 0.3 seconds for one in `mcpls-cli`, which confirms the spec's 15000. So this task writes the spec's number unchanged; only a later re-measurement that disagrees would change it, and that change would say so in its commit body.

**Interfaces:**
- Produces:
  - `DiagnosticsConfig` gains `footer: bool` (default `false`), `footer_grace_ms: u64` (250), `footer_quiet_ms: u64` (200), `footer_wait_ms: u64` (15000).
  - `ServerSettle::end_at(&self, server, token, now: Instant)`, with the existing `end` kept as a thin wrapper passing `Instant::now()`. `begin` is unchanged apart from bumping the epoch.
  - `ServerSettle::is_quiet_at(&self, now: Instant, quiet_for: Duration) -> bool`.
  - `ServerSettle::progress_epoch(&self) -> u64`, incremented on every `begin`.

- [ ] **Step 1: write the failing tests**

The settle tests go in `crates/mcpls-core/src/bridge/settle.rs`'s test module and the defaults test in `crates/mcpls-core/src/config/mod.rs`'s. Both need `#[allow(clippy::unwrap_used, clippy::expect_used)]` under their `#[cfg(test)]` if they do not already carry it.

```rust
#[test]
fn test_footer_quiet_holds_for_a_workspace_that_never_reported_progress() {
    let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
    let now = Instant::now();

    assert!(
        !settle.should_settle_at(now),
        "the baseline's judgment waits for a first end, because a workspace \
         that has said nothing yet may simply not have started"
    );
    assert!(
        settle.is_quiet_at(now, Duration::from_millis(200)),
        "the footer's does not: it runs after its own grace period, and a \
         server that reports no progress at all would otherwise burn the \
         whole cap on every write"
    );
}

#[test]
fn test_footer_quiet_waits_while_work_is_outstanding() {
    let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
    let now = Instant::now();
    settle.begin(&ServerId::from("rust"), &json!("flycheck"));

    assert!(!settle.is_quiet_at(now + Duration::from_secs(30), Duration::from_millis(200)));
}

#[test]
fn test_footer_quiet_needs_its_own_debounce_after_the_last_end() {
    let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
    let start = Instant::now();
    let rust = ServerId::from("rust");
    settle.begin(&rust, &json!("flycheck"));
    settle.end_at(&rust, &json!("flycheck"), start);

    assert!(!settle.is_quiet_at(start + Duration::from_millis(100), Duration::from_millis(200)));
    assert!(settle.is_quiet_at(start + Duration::from_millis(300), Duration::from_millis(200)));
}

#[test]
fn test_the_progress_epoch_moves_only_when_work_begins() {
    let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
    let rust = ServerId::from("rust");
    let before = settle.progress_epoch();

    settle.end_at(&rust, &json!("orphan"), Instant::now());
    assert_eq!(
        settle.progress_epoch(),
        before,
        "an unmatched end is not new work; the footer uses this to tell an \
         index already in flight from a check its own save started"
    );

    settle.begin(&rust, &json!("flycheck"));
    assert_eq!(settle.progress_epoch(), before + 1);
}

#[test]
fn test_the_footer_defaults_are_what_the_spec_says() {
    let config = DiagnosticsConfig::default();
    assert!(!config.footer);
    assert_eq!(config.footer_grace_ms, 250);
    assert_eq!(config.footer_quiet_ms, 200);
    assert_eq!(config.footer_wait_ms, 15_000);
}
```

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core bridge::settle config::tests::test_the_footer_defaults`
Expected: FAIL, `no method named is_quiet_at`.

- [ ] **Step 3: implement the settle changes**

`SettleState` gains `epoch: u64`, starting at 0.

```rust
    /// Record that `server` started a long-running operation.
    ///
    /// Takes no instant: nothing here is time-dependent, since starting
    /// work only clears the quiet stamp and bumps the epoch.
    pub fn begin(&self, server: &ServerId, token: &serde_json::Value) {
        let Ok(mut state) = self.state.lock() else { return };
        state.outstanding.insert((server.clone(), token.to_string()));
        state.quiet_since = None;
        state.epoch += 1;
    }

    /// Record that `server` finished one, as of `now`.
    ///
    /// Takes the instant rather than reading the clock, so the footer's
    /// wait can be driven in a test without sleeping. `end` stamps the wall
    /// clock, which is why this module's older tests sleep; the footer
    /// cannot afford that, since its assertions are about which of three
    /// branches ended the wait.
    pub fn end_at(&self, server: &ServerId, token: &serde_json::Value, now: Instant) {
        let Ok(mut state) = self.state.lock() else { return };
        if !state.outstanding.remove(&(server.clone(), token.to_string())) {
            return;
        }
        if state.outstanding.is_empty() {
            state.quiet_since = Some(now);
        }
    }

    /// Record that `server` finished one.
    pub fn end(&self, server: &ServerId, token: &serde_json::Value) {
        self.end_at(server, token, Instant::now());
    }

    /// How many long-running operations have ever begun.
    ///
    /// The footer captures this before its resync and compares afterwards,
    /// so an index already running when the rename landed does not read as
    /// the check that rename started.
    #[must_use]
    pub fn progress_epoch(&self) -> u64 {
        self.state.lock().map_or(0, |state| state.epoch)
    }

    /// Whether nothing has been outstanding for `quiet_for` as of `now`.
    ///
    /// Differs from [`Self::should_settle_at`] in two ways, both deliberate.
    /// There is no deadline: the footer carries its own, much shorter, cap.
    /// And a workspace that has never reported any progress counts as
    /// quiet, where the baseline's judgment waits for a first `end`. The
    /// baseline can afford to wait because it has five minutes and one
    /// chance to get the workspace's real state; a footer runs after its own
    /// grace period on every write, and a configured server that reports no
    /// `$/progress` would otherwise make every footer burn its whole cap.
    #[must_use]
    pub fn is_quiet_at(&self, now: Instant, quiet_for: Duration) -> bool {
        let Ok(state) = self.state.lock() else { return false };
        state.outstanding.is_empty()
            && state
                .quiet_since
                .is_none_or(|since| now.duration_since(since) >= quiet_for)
    }
```

- [ ] **Step 4: implement the config changes**

In `DiagnosticsConfig`:

```rust
    /// Whether the tools that write append their new diagnostics to their
    /// own result.
    #[serde(default)]
    pub footer: bool,
    /// How long a footer waits before it starts looking for quiet.
    ///
    /// rust-analyzer's flycheck begins about 90 ms after a `didSave`, and a
    /// footer that checks before then sees a quiet workspace and reports
    /// the state from before the edit.
    #[serde(default = "default_footer_grace_ms")]
    pub footer_grace_ms: u64,
    /// How long nothing may be outstanding before a footer calls it done.
    ///
    /// Shorter than `settle_quiet_ms`, which exists to bridge the 70 to 100
    /// millisecond gaps between rust-analyzer's startup phases. A footer
    /// never sees those; what it bridges is the cancel-and-restart between
    /// two saves landing back to back.
    #[serde(default = "default_footer_quiet_ms")]
    pub footer_quiet_ms: u64,
    /// How long a footer waits in total before reporting what it has.
    ///
    /// Sized against a real build rather than against patience: a no-op
    /// touch in this repository's largest crate costs about 4.3 seconds of
    /// `cargo check`, measured, so a five second cap would expire on every
    /// rename there and report the pre-edit state. The wait is gated on
    /// progress rather than on a timer, so a fast workspace still returns
    /// in about a second and the high cap costs it nothing.
    #[serde(default = "default_footer_wait_ms")]
    pub footer_wait_ms: u64,
```

```rust
const fn default_footer_grace_ms() -> u64 { 250 }
const fn default_footer_quiet_ms() -> u64 { 200 }
const fn default_footer_wait_ms() -> u64 { 15_000 }
```

Add all four to the `Default` impl.

- [ ] **Step 5: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core`
Expected: PASS.

- [ ] **Step 6: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 7: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/bridge/settle.rs crates/mcpls-core/src/config/mod.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(bridge): give the footer its own quiet test

The baseline's judgment waits for a first end of progress, so a
workspace whose servers report none is never quiet and every footer
would burn its whole cap. The footer's judgment treats nothing
outstanding and nothing ever reported as quiet, after its own grace
period.

end_at takes the instant rather than reading the clock, so the three
branches of the footer's wait can be asserted without sleeping.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 13: the footer

**Files:**
- Modify: `crates/mcpls-core/src/mcp/handlers.rs` (`BridgeContext` at `:27-58` and `BridgeContext::new` at `:60-82`)
- Modify: `crates/mcpls-core/src/mcp/server.rs` (`McplsServer::new` at `:262`, `rename_symbol` at `:437`, `format_document` at `:514`, `apply_code_action` at `:604`, and the three `McplsServer::new` calls in the test module at `:1208`, `:1694` and `:2273`)
- Modify: `crates/mcpls-core/src/lib.rs` (`serve_with`'s `settle` at `:708` and its `McplsServer::new` call at `:754`)
- Modify: `crates/mcpls-core/src/transport.rs` (the `McplsServer::new` calls at `:839`, `:1022`, `:1051`)

**Interfaces:**
- Consumes: Task 12's config keys and `ServerSettle::{is_quiet_at, progress_epoch, end_at}`; Task 10's `flush_now(&self, session: &SessionId)` and its `TestServer` helpers.
- Produces:
  - `BridgeContext` gains `pub diagnostics: DiagnosticsConfig` and `pub settle: Arc<ServerSettle>`; `BridgeContext::new` and `McplsServer::new` each take two more arguments.
  - `McplsServer::footer_for_write(&self) -> Option<NewDiagnosticsResult>` and `McplsServer::footer_if_written(&self, applied: bool) -> Option<NewDiagnosticsResult>`.
  - `FooterTiming`, `wait_for_footer_quiet_at`, `footer_should_stop`.

```rust
/// A tool result with the diagnostics that call produced appended.
#[derive(Debug, Serialize)]
struct WithDiagnostics<T> {
    #[serde(flatten)]
    result: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_diagnostics: Option<NewDiagnosticsResult>,
}
```

**How the config and the settle tracker reach the MCP layer.** `BridgeContext` (`mcp/handlers.rs:27-58`) has exactly `translator`, `notification_cache`, `workspace_roots`, `subscriptions`, `project_config_ignored`, `delivery` and `floors`. It has no config and no settle tracker, and `ServerSettle` is built in `serve_with` at `:708` and **moved** into `PumpShared` at `:726`, so today it reaches only the pump and `baseline_task`. This task adds both to the context:

- `pub diagnostics: DiagnosticsConfig`, by value rather than behind an `Arc`. `DiagnosticsConfig` is `Copy` and fixed for the process lifetime, and the footer reads four scalars off it. Carrying the whole `ServerConfig` would drag `lsp_servers` and `apply` into a struct that has no use for them.
- `pub settle: Arc<ServerSettle>`, the same `Arc` the pump holds, so the footer sees the `begin` and `end` the pump records.

`BridgeContext::new` is a `pub const fn` with seven positional parameters and exactly two callers, the test at `handlers.rs:100` and `McplsServer::new` at `mcp/server.rs:272`. `McplsServer::new` has seven callers: `lib.rs:754`, `transport.rs:839`, `transport.rs:1022`, `transport.rs:1051`, and the three test helpers at `mcp/server.rs:1208` (`create_test_server`), `:1694` (`new_diagnostics_test_server`) and `:2273` (`server_permitting_writes`). Both constructors gain the two parameters at the end, in that order, and every caller passes `DiagnosticsConfig::default()` and a freshly built `Arc<ServerSettle>` unless it has real ones. In `serve_with`, change line `:726` from `settle,` to `settle: Arc::clone(&settle),` so the local binding survives the `PumpShared` construction, then pass `config.diagnostics` and `settle` to `McplsServer::new`.

`BridgeContext::new` stays `const`: `Arc` moves and a `Copy` struct are both const-compatible.

- [ ] **Step 1: write the failing tests**

Extend `mcp::server`'s test module, which Task 10 gave `#[allow(clippy::unwrap_used, clippy::expect_used)]` and the `TestServer` helpers. Two more helpers here:

```rust
    /// A server whose config enables the footer, with a baseline adopted
    /// and one error in the cache, so a footer has something to report.
    async fn test_server_with_footer_and_one_error() -> TestServer {
        let uri: lsp_types::Uri = if cfg!(windows) {
            "file:///C:/workspace/broken.rs".parse().expect("a valid uri")
        } else {
            "file:///workspace/broken.rs".parse().expect("a valid uri")
        };
        let owner = ServerId::from("rust");
        let diagnostics = DiagnosticsConfig {
            footer: true,
            // Keep the wait out of the test's way: what these assert is the
            // guard and the record, not the timing, which
            // `wait_for_footer_quiet_at` covers directly.
            footer_grace_ms: 0,
            footer_quiet_ms: 0,
            footer_wait_ms: 0,
            ..DiagnosticsConfig::default()
        };
        let parts = test_server_parts_with(diagnostics);
        parts
            .notification_cache
            .lock()
            .await
            .store_diagnostics(&owner, &uri, Some(1), vec![diagnostic_at("broken")]);
        parts.delivery.lock().await.set_baseline(HashMap::new());
        parts
    }

    /// A rename result shaped the way `rename_symbol` returns one.
    fn sample_rename_result() -> RenameResult {
        RenameResult {
            changes: Vec::new(),
            resource_operations: Vec::new(),
            applied: true,
            files_written: vec!["/workspace/broken.rs".to_string()],
        }
    }
```

`test_server_parts_with(diagnostics)` is `test_server_parts` from Task 10 with the config threaded through instead of `DiagnosticsConfig::default()`; keep `test_server_parts()` as a thin wrapper passing the default so Task 10's test still reads the same.

```rust
#[tokio::test]
async fn test_a_footer_is_silent_before_the_baseline_lands() {
    let parts = test_server_parts_with(DiagnosticsConfig {
        footer: true,
        ..DiagnosticsConfig::default()
    });

    let footer = parts.server.footer_for_write().await;

    assert!(
        footer.is_none(),
        "flush seeds a session record from the baseline, and a record made \
         before the baseline lands stays empty forever, so the next flush \
         would report the whole workspace. The settle deadline is 300 \
         seconds, which puts the first rename of a session squarely inside \
         this window"
    );
}

#[tokio::test]
async fn test_a_footer_consumes_what_it_reports() {
    let parts = test_server_with_footer_and_one_error().await;

    let footer = parts.server.footer_for_write().await.expect("a report");
    assert_eq!(footer.changed.len(), 1);

    let raw = parts.server.get_new_diagnostics().await.expect("the flush tool");
    let report: serde_json::Value = serde_json::from_str(&raw).expect("json");
    assert!(
        report["changed"].as_array().expect("changed").is_empty(),
        "one report per problem: the footer and the flush share one record"
    );
}

/// The guard the three write tools run the footer behind, both ways.
///
/// This is what `if result.applied` buys, so it is asserted against the
/// method the call sites use rather than against a serialized struct: a
/// serde test proves `skip_serializing_if`, not the guard.
#[tokio::test]
async fn test_no_footer_when_the_tool_wrote_nothing() {
    let parts = test_server_with_footer_and_one_error().await;

    assert!(
        parts.server.footer_if_written(false).await.is_none(),
        "a rename with apply false changed nothing and has nothing to report"
    );
    assert!(
        parts.server.footer_if_written(true).await.is_some(),
        "and a call that did write must still get one, or the guard is just \
         a footer that never fires"
    );
}

#[test]
fn test_the_wrapper_omits_an_absent_footer_from_its_json() {
    let wrapped = WithDiagnostics {
        result: sample_rename_result(),
        new_diagnostics: None,
    };
    let json = serde_json::to_string(&wrapped).expect("serialize");

    assert!(!json.contains("new_diagnostics"));
}

#[test]
fn test_the_wrapper_flattens_rather_than_nesting() {
    let wrapped = WithDiagnostics {
        result: sample_rename_result(),
        new_diagnostics: None,
    };
    let json: serde_json::Value = serde_json::to_value(&wrapped).expect("serialize");

    assert!(
        json.get("applied").is_some(),
        "the existing result's fields stay at the top level; a caller \
         parsing RenameResult must keep parsing it"
    );
}
```

The spec's stage B testing section requires all three branches of the wait, so all three get a test. `wait_for_footer_quiet_at` is pure over an injected clock precisely so they can be asserted; a `tokio::time::pause()` test could not, because `ServerSettle` stamps `std::time::Instant` and pausing tokio's clock does not move that one.

```rust
/// Branch one: the grace period elapses before quiet is consulted.
#[test]
fn test_the_footer_wait_never_returns_before_its_grace_period() {
    let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
    let start = Instant::now();

    // Nothing has ever begun, so the workspace reads as quiet from the very
    // first sample.
    let ended = wait_for_footer_quiet_at(
        &settle,
        settle.progress_epoch(),
        FooterTiming {
            grace: Duration::from_millis(250),
            quiet: Duration::from_millis(200),
            cap: Duration::from_secs(15),
        },
        |elapsed| start + elapsed,
    );

    assert_eq!(
        ended,
        Duration::from_millis(250),
        "rust-analyzer's flycheck begins about 90ms after a didSave, and a \
         footer that sampled before then would see a quiet workspace and \
         report the state from before the edit"
    );
}

/// Branch two: quiet ends the wait early.
#[test]
fn test_the_footer_wait_ends_on_quiet_rather_than_on_its_cap() {
    let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
    let rust = ServerId::from("rust");
    let start = Instant::now();
    let epoch_before = settle.progress_epoch();
    settle.begin(&rust, &json!("flycheck"));
    settle.end_at(&rust, &json!("flycheck"), start + Duration::from_millis(400));

    let ended = wait_for_footer_quiet_at(
        &settle,
        epoch_before,
        FooterTiming {
            grace: Duration::from_millis(250),
            quiet: Duration::from_millis(200),
            cap: Duration::from_secs(15),
        },
        |elapsed| start + elapsed,
    );

    assert!(
        ended < Duration::from_secs(1),
        "quiet arrived at 600ms, well inside the cap; a test that could only \
         ever end on the cap would pass against a broken quiet check"
    );
    assert!(
        ended >= Duration::from_millis(600),
        "and not before the quiet debounce has actually run out"
    );
}

/// Branch three: the cap ends it when quiet never arrives.
#[test]
fn test_the_footer_wait_ends_on_its_cap_when_quiet_never_arrives() {
    let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
    let rust = ServerId::from("rust");
    let start = Instant::now();
    let epoch_before = settle.progress_epoch();
    settle.begin(&rust, &json!("flycheck"));
    // No `end_at`: the check is still running when the cap expires.

    let ended = wait_for_footer_quiet_at(
        &settle,
        epoch_before,
        FooterTiming {
            grace: Duration::from_millis(250),
            quiet: Duration::from_millis(200),
            cap: Duration::from_secs(15),
        },
        |elapsed| start + elapsed,
    );

    assert_eq!(
        ended,
        Duration::from_secs(15),
        "the footer is best effort: it reports what has landed rather than \
         waiting on a build that has not finished"
    );
}

/// Work that was already running when the edit landed does not eat the cap.
#[test]
fn test_an_index_already_in_flight_does_not_hold_the_footer() {
    let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
    let rust = ServerId::from("rust");
    let start = Instant::now();
    settle.begin(&rust, &json!("rustAnalyzer/Indexing"));
    // Captured after the begin, the way `footer_for_write` captures it
    // after the resync has already returned.
    let epoch_before = settle.progress_epoch();

    let ended = wait_for_footer_quiet_at(
        &settle,
        epoch_before,
        FooterTiming {
            grace: Duration::from_millis(250),
            quiet: Duration::from_millis(200),
            cap: Duration::from_secs(15),
        },
        |elapsed| start + elapsed,
    );

    assert_eq!(
        ended,
        Duration::from_millis(250),
        "an index after a Cargo.toml change can run for minutes and is not \
         this call's check; waiting on it would spend the whole cap on \
         something this tool call did not cause"
    );
}
```

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core mcp::server::tests::test_a_footer mcp::server::tests::test_the_footer`
Expected: FAIL, `no method named footer_for_write`.

- [ ] **Step 3: thread the config and the settle tracker to the MCP layer**

Add to `BridgeContext` (`mcp/handlers.rs:27-58`):

```rust
    /// The diagnostics configuration, fixed at startup.
    ///
    /// Held by value: `DiagnosticsConfig` is `Copy` and never changes while
    /// the process runs, and the footer reads four scalars off it. Carrying
    /// the whole `ServerConfig` would drag `lsp_servers` and `apply` into a
    /// struct with no use for either.
    pub diagnostics: DiagnosticsConfig,
    /// The same settle tracker the diagnostics pump feeds.
    ///
    /// The footer waits on `$/progress` and the pump is what records it, so
    /// this must be the pump's own `Arc` rather than a fresh tracker.
    pub settle: Arc<ServerSettle>,
```

Extend `BridgeContext::new` and `McplsServer::new` with the two parameters, appended in that order, and update every caller listed in the Files block above. In `crates/mcpls-core/src/lib.rs`, change `settle,` in the `PumpShared` literal at `:726` to `settle: Arc::clone(&settle),`, then pass `config.diagnostics` and `settle` to `McplsServer::new` at `:754`. The `transport.rs` and test callers pass `DiagnosticsConfig::default()` and `Arc::new(bridge::ServerSettle::new(Duration::from_secs(1), Duration::from_secs(300)))`, since nothing there runs a footer.

- [ ] **Step 4: implement the wait**

```rust
/// How long each phase of a footer's wait lasts.
#[derive(Debug, Clone, Copy)]
struct FooterTiming {
    grace: Duration,
    quiet: Duration,
    cap: Duration,
}

impl FooterTiming {
    fn from_config(config: &DiagnosticsConfig) -> Self {
        Self {
            grace: Duration::from_millis(config.footer_grace_ms),
            quiet: Duration::from_millis(config.footer_quiet_ms),
            cap: Duration::from_millis(config.footer_wait_ms),
        }
    }
}

/// How long a footer would wait, given a clock.
///
/// Pure so the three branches that can end the wait are each assertable:
/// the grace period elapsing before anything is consulted, quiet arriving,
/// and the cap expiring. `at` maps elapsed time to the `Instant` the settle
/// tracker stamps against.
///
/// Sampling starts at `grace` rather than at zero, and that is what covers
/// the case where the check has not begun yet: flycheck starts about 90 ms
/// after a `didSave`, and before it does the workspace reads as quiet.
fn wait_for_footer_quiet_at(
    settle: &ServerSettle,
    epoch_before: u64,
    timing: FooterTiming,
    at: impl Fn(Duration) -> Instant,
) -> Duration {
    const STEP: Duration = Duration::from_millis(50);
    let mut elapsed = timing.grace;
    while elapsed < timing.cap {
        if footer_should_stop(settle, epoch_before, at(elapsed), timing.quiet) {
            return elapsed;
        }
        elapsed += STEP;
    }
    timing.cap
}

/// Whether a footer has waited long enough, as of `now`.
///
/// Two ways to be done. The workspace is quiet, which is the ordinary one
/// and the only one that fires before any work has begun. Or work is
/// outstanding and none of it began since the resync, which means that work
/// was already running when the edit landed: an index after a `Cargo.toml`
/// change can run for minutes, and it is not this call's check.
fn footer_should_stop(
    settle: &ServerSettle,
    epoch_before: u64,
    now: Instant,
    quiet: Duration,
) -> bool {
    if settle.is_quiet_at(now, quiet) {
        return true;
    }
    settle.progress_epoch() == epoch_before
}
```

- [ ] **Step 5: implement the footer**

```rust
impl McplsServer {
    /// The diagnostics a write tool's own edit produced, or `None` when the
    /// call wrote nothing.
    ///
    /// One method rather than an `if` repeated at three call sites, so a
    /// fourth write tool cannot be added with the guard forgotten.
    async fn footer_if_written(&self, applied: bool) -> Option<NewDiagnosticsResult> {
        if !applied {
            return None;
        }
        self.footer_for_write().await
    }

    /// The diagnostics a write tool's own edit produced, or `None`.
    ///
    /// Silent while no baseline exists. `flush` seeds a session's record
    /// from the baseline, and `set_baseline` does not rewrite a record that
    /// already exists, so a footer flushing early would leave that session
    /// permanently believing the workspace started clean.
    async fn footer_for_write(&self) -> Option<NewDiagnosticsResult> {
        if !self.context.diagnostics.footer {
            return None;
        }
        if !self.context.delivery.lock().await.has_baseline() {
            return None;
        }
        let timing = FooterTiming::from_config(&self.context.diagnostics);
        let epoch_before = self.context.settle.progress_epoch();
        tokio::time::sleep(timing.grace).await;

        let start = Instant::now();
        while start.elapsed() < timing.cap {
            if footer_should_stop(
                &self.context.settle,
                epoch_before,
                Instant::now(),
                timing.quiet,
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let session = SessionId::process_default();
        let mut report = self.flush_now(&session).await;
        report.note = Some(match report.note.take() {
            Some(existing) => format!("{existing} This footer is best effort; anything slower than the wait arrives in the next get_new_diagnostics."),
            None => "This footer is best effort; anything slower than the wait arrives in the next get_new_diagnostics.".to_string(),
        });
        Some(report)
    }
}
```

`flush_now` takes a session, matching Task 10's signature. The `note` line is the spec's "the footer is best effort by construction and says so in its own text"; `NewDiagnosticsResult::note` (`mcp/server.rs:203`) already exists to carry exactly this kind of explanation, and the `omitted` explanation the payload may already have written is kept rather than overwritten.

The footer takes `delivery` and then, inside `flush_now`, the cache, which is the plan's delivery-before-cache order. It holds neither across its sleeps: the `has_baseline` guard drops its guard at the end of its statement, and `flush_now` drops both before it awaits the payload build.

At each of the three call sites:

```rust
        let footer = self.footer_if_written(result.applied).await;
        to_tool_result(Ok(WithDiagnostics { result, new_diagnostics: footer }))
```

`rename_symbol` reads `RenameResult::applied` (`bridge/translator/dto.rs:118`). `format_document` and `apply_code_action` have their own applied flags; read each result type rather than assuming the field name is `applied` on all three.

- [ ] **Step 6: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core`
Expected: PASS.

- [ ] **Step 7: check the e2e still passes with the footer off**

Run: `cargo nextest run -p mcpls-core --test ra_e2e -- --ignored ra_e2e_suite`
Expected: PASS. The footer defaults to false, so no sub-case should change.

- [ ] **Step 8: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 9: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add \
  crates/mcpls-core/src/mcp/ \
  crates/mcpls-core/src/lib.rs \
  crates/mcpls-core/src/transport.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(mcp): append new diagnostics to write results

The three tools that write append the diagnostics their own edit
produced, behind a config key that defaults off.

The footer waits on $/progress rather than on document versions,
because a rename that introduces no problem publishes nothing and a
version wait would sit until its cap on every clean edit.

BridgeContext gains the diagnostics config and the settle tracker,
which the pump already owned and nothing at the MCP layer could reach.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---


# Stage C

Everything below is inert until Task 22 installs the plugin. Each task still ends green on its own tests.

## Task 14: socket identity

**Files:**
- Create: `crates/mcpls-core/src/hooks/mod.rs`
- Create: `crates/mcpls-core/src/hooks/identity.rs`
- Modify: `crates/mcpls-core/src/lib.rs` (add `pub mod hooks;`)
- Modify: `crates/mcpls-core/src/config/mod.rs` (add `HooksConfig`)

**Interfaces:**
- Produces:

```rust
/// Where this project's hook socket and its ownership lock live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketIdentity {
    /// 16 hex characters derived from the canonicalized directory.
    pub hash: String,
    /// The socket or named pipe to bind.
    pub socket: PathBuf,
    /// The lock file whose holder owns `socket`. Empty on Windows, where
    /// the pipe itself is the lock.
    pub lock: PathBuf,
}

/// Derive the identity for `dir`.
///
/// # Errors
///
/// Returns an error if `dir` cannot be canonicalized.
pub fn identity_for(dir: &Path) -> Result<SocketIdentity>;

/// `HooksConfig { enabled: bool, sweep_quiet_ms: u64, op_deadline_ms: u64 }`,
/// `Copy`, reached as `config.diagnostics.hooks`.
```

- [ ] **Step 1: write the failing tests**

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_the_same_directory_hashes_the_same_way_twice() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let a = identity_for(dir.path()).expect("identity");
        let b = identity_for(dir.path()).expect("identity");
        assert_eq!(a, b);
    }

    #[test]
    fn test_two_directories_hash_differently() {
        let one = tempfile::tempdir().expect("a temp dir");
        let two = tempfile::tempdir().expect("a temp dir");
        assert_ne!(
            identity_for(one.path()).expect("identity").hash,
            identity_for(two.path()).expect("identity").hash
        );
    }

    #[test]
    fn test_the_hash_is_sixteen_hex_characters() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let identity = identity_for(dir.path()).expect("identity");
        assert_eq!(identity.hash.len(), 16, "sockaddr_un allows 104 bytes on macOS and $TMPDIR eats most of them");
        assert!(identity.hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_a_symlink_and_its_target_agree() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).expect("mkdir");
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&real, &link).expect("symlink");

        assert_eq!(
            identity_for(&real).expect("identity").hash,
            identity_for(&link).expect("identity").hash,
            "mcpls hashes its own working directory and the hook hashes \
             CLAUDE_PROJECT_DIR; a symlinked checkout must not split them"
        );
    }

    #[test]
    fn test_the_hooks_defaults_are_what_the_spec_says() {
        let config = HooksConfig::default();
        assert!(config.enabled);
        assert_eq!(config.sweep_quiet_ms, 500);
        assert_eq!(config.op_deadline_ms, 1500);
    }
}
```

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core hooks::identity`
Expected: FAIL to compile.

- [ ] **Step 3: implement**

`crates/mcpls-core/src/hooks/mod.rs`:

```rust
//! The per-project socket that lets Claude Code's hooks push changed paths
//! into a running mcpls and pull new diagnostics back out.
//!
//! Inert without the plugin: with no hook process ever connecting, the
//! listener binds, waits, and does nothing.

pub mod identity;

pub use identity::{SocketIdentity, identity_for};
```

`crates/mcpls-core/src/hooks/identity.rs`:

```rust
//! Where a project's hook socket lives, and how the two sides agree on it.
//!
//! mcpls hashes its own startup working directory; the hook hashes
//! `CLAUDE_PROJECT_DIR`. Those agree because a host spawns a stdio MCP
//! server in the project directory, which is a property of the host rather
//! than a guarantee, which is why `mcpls hook doctor` prints both.
//!
//! Both sides canonicalize here, through `dunce`. `Path::canonicalize`
//! returns a `\\?\C:\...` extended-length path on Windows, so a design
//! where one side used the standard library and the other used `dunce`
//! would disagree on every Windows install, permanently and with nothing
//! to look at.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Where this project's hook socket and its ownership lock live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketIdentity {
    /// 16 hex characters derived from the canonicalized directory.
    pub hash: String,
    /// The socket or named pipe to bind.
    pub socket: PathBuf,
    /// The lock file whose holder owns `socket`. On Windows this is unused
    /// and empty: `first_pipe_instance` makes the pipe itself exclusive.
    pub lock: PathBuf,
}

/// Derive the socket identity for `dir`.
///
/// # Errors
///
/// Returns an error if `dir` cannot be canonicalized, which means it does
/// not exist or is not reachable.
pub fn identity_for(dir: &Path) -> Result<SocketIdentity> {
    let canonical = dunce::canonicalize(dir).map_err(|e| Error::FileIo {
        path: dir.to_path_buf(),
        source: e,
    })?;
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());

    #[cfg(windows)]
    {
        Ok(SocketIdentity {
            socket: PathBuf::from(format!(r"\\.\pipe\mcpls-{hash}")),
            lock: PathBuf::new(),
            hash,
        })
    }
    #[cfg(not(windows))]
    {
        let dir = runtime_dir();
        Ok(SocketIdentity {
            socket: dir.join(format!("{hash}.sock")),
            lock: dir.join(format!("{hash}.lock")),
            hash,
        })
    }
}

/// Where sockets go on this platform.
///
/// `$XDG_RUNTIME_DIR/mcpls` where that is set, which is the tmpfs a session
/// owns and which is cleaned when the session ends. Otherwise the system
/// temporary directory, which is `$TMPDIR` on macOS and `/tmp` on Linux,
/// with a per-user suffix so two users on one machine do not collide on a
/// shared `/tmp`.
///
/// The suffix comes from `$USER` or `$LOGNAME` rather than from `getuid`.
/// The workspace sets `unsafe_code = "deny"`, so `unsafe { libc::getuid() }`
/// does not compile here, and a safe wrapper crate would be a whole
/// dependency bought for one integer. Do not reinstate the uid call. Both
/// variables being absent gives an unsuffixed directory, which is right for
/// a single-user machine and no worse than what a shared `/tmp` already
/// offers.
///
/// A username can carry a path separator on some systems, so it is reduced
/// to ASCII alphanumerics and `-`, `_`, `.` before it goes into a path.
#[cfg(not(windows))]
fn runtime_dir() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("mcpls");
    }
    let user = std::env::var_os("USER")
        .or_else(|| std::env::var_os("LOGNAME"))
        .and_then(|raw| raw.into_string().ok())
        .map(|name| {
            name.chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .collect::<String>()
        })
        .filter(|name| !name.is_empty());
    match user {
        Some(user) => std::env::temp_dir().join(format!("mcpls-{user}")),
        None => std::env::temp_dir().join("mcpls"),
    }
}
```

`DefaultHasher` is not stable across Rust releases, which does not matter here: both sides are the same binary in the same process family, and a hash that changes between mcpls versions only means a new socket path after an upgrade. Say that in a comment so a later reader does not reach for a cryptographic hash to fix a problem that does not exist.

The workspace lints deny `unsafe_code` (`/home/lev/Git/lev/mcpls-diag-bc/Cargo.toml:48`), which is why the uid is derived this way and why this task adds **no** new dependency. `dunce` is already a direct dependency of `mcpls-core` (`crates/mcpls-core/Cargo.toml:16`); confirm with `rg dunce /home/lev/Git/lev/mcpls-diag-bc/crates/mcpls-core/Cargo.toml` and add nothing else.

The spec's "Socket identity" section describes the fallback as `/tmp/mcpls-<uid>`. `std::env::temp_dir()` is `/tmp` unless `$TMPDIR` says otherwise, and the username stands in for the uid, so the shape and the property the spec wanted, one directory per user, both hold. Do not edit the spec for this; it is an implementation detail of how the per-user suffix is derived.

Add to `crates/mcpls-core/src/config/mod.rs`:

```rust
/// How the Claude Code hooks reach a running mcpls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksConfig {
    /// Whether the listener binds at all.
    ///
    /// Defaults on, because reaching this configuration means installing
    /// the plugin and installing the plugin is the opt-in. With this off,
    /// nothing binds and every hook exits 0 without output.
    #[serde(default = "default_hooks_enabled")]
    pub enabled: bool,
    /// How long the pending set must be quiet before the sweep runs.
    ///
    /// Every `didSave` restarts rust-analyzer's flycheck and cancels the
    /// check in flight, so a `cargo fmt` forwarded one path at a time
    /// produces a run of cancelled checks and no diagnostics at all.
    #[serde(default = "default_sweep_quiet_ms")]
    pub sweep_quiet_ms: u64,
    /// How long an op may take before it answers anyway.
    ///
    /// The host's default hook timeout is 600 seconds, so a hook that hangs
    /// blocks the agent. This bound is the hook's protection, not the
    /// host's; work already started keeps running and reaches the next
    /// flush.
    #[serde(default = "default_op_deadline_ms")]
    pub op_deadline_ms: u64,
}

const fn default_hooks_enabled() -> bool { true }
const fn default_sweep_quiet_ms() -> u64 { 500 }
const fn default_op_deadline_ms() -> u64 { 1_500 }

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            enabled: default_hooks_enabled(),
            sweep_quiet_ms: default_sweep_quiet_ms(),
            op_deadline_ms: default_op_deadline_ms(),
        }
    }
}
```

Add to `DiagnosticsConfig`, which stays `Copy` because `HooksConfig` is:

```rust
    /// How the Claude Code hooks reach this process.
    #[serde(default)]
    pub hooks: HooksConfig,
```

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core hooks config`
Expected: PASS.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add \
  crates/mcpls-core/src/hooks/ \
  crates/mcpls-core/src/lib.rs \
  crates/mcpls-core/src/config/mod.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(hooks): derive the per-project socket path

mcpls hashes its own canonicalized startup directory and the hook
hashes CLAUDE_PROJECT_DIR; both go through dunce, because
Path::canonicalize yields an extended-length path on Windows and a
design where one side used each would disagree on every Windows
install with nothing to look at.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 15: the wire protocol

**Files:**
- Create: `crates/mcpls-core/src/hooks/protocol.rs`
- Modify: `crates/mcpls-core/src/hooks/mod.rs`

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Changed { session: String, paths: Vec<PathBuf>, event: ChangeEvent },
    Flush { session: String },
    EndSession { session: String },
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeEvent { Change, Add, Unlink }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Response {
    Changed { queued: usize },
    Flush { context: Option<String> },
    EndSession,
    Status { hash: String, socket: PathBuf, pid: u32, owner: bool },
    Error { message: String },
}
```

- [ ] **Step 1: write the failing tests**

In a new `#[cfg(test)] mod tests` at the bottom of `crates/mcpls-core/src/hooks/protocol.rs`, opening with `#[allow(clippy::unwrap_used, clippy::expect_used)]`. One helper, which the first test needs:

```rust
    /// An absolute path with a drive letter on Windows, where
    /// `Url::from_file_path` fails without one.
    fn abs(rel: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:\\work\\{}", rel.replace('/', "\\")))
        } else {
            PathBuf::from(format!("/work/{rel}"))
        }
    }
```

```rust
#[test]
fn test_a_changed_request_round_trips() {
    let request = Request::Changed {
        session: "s1".to_string(),
        paths: vec![abs("src/a.rs")],
        event: ChangeEvent::Change,
    };
    let line = serde_json::to_string(&request).expect("serialize");
    assert!(!line.contains('\n'), "the framing is one request per line");
    assert_eq!(
        serde_json::from_str::<Request>(&line).expect("deserialize"),
        request
    );
}

#[test]
fn test_the_wire_names_match_the_spec() {
    let line = serde_json::to_string(&Request::Flush { session: "s1".to_string() })
        .expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&line).expect("json");
    assert_eq!(parsed["op"], serde_json::json!("flush"));
    assert_eq!(parsed["session"], serde_json::json!("s1"));
}

#[test]
fn test_an_unknown_op_is_an_error_rather_than_a_panic() {
    let parsed = serde_json::from_str::<Request>(r#"{"op":"explode"}"#);
    assert!(parsed.is_err());
}

#[test]
fn test_the_three_change_events_the_host_sends_all_parse() {
    for (wire, expected) in [
        ("change", ChangeEvent::Change),
        ("add", ChangeEvent::Add),
        ("unlink", ChangeEvent::Unlink),
    ] {
        let line = format!(r#"{{"op":"changed","session":"s","paths":[],"event":"{wire}"}}"#);
        let Request::Changed { event, .. } = serde_json::from_str(&line).expect("deserialize")
        else {
            panic!("a changed request parsed as something else");
        };
        assert_eq!(event, expected);
    }
}
```

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core hooks::protocol`
Expected: FAIL to compile.

- [ ] **Step 3: implement**

Write the two enums above with the derives shown. `missing_docs` warns workspace-wide against a `-D warnings` gate, so every variant and every field needs a doc comment or the crate does not build; the Interfaces block above shows the shapes, not finished code. Each doc comment names which hook sends or reads that variant. Note in the module doc that the host's event kind is a hint only, and that the sweep derives the real kind from a stat, because a formatter saving through a temporary file and a rename produces `unlink` for a file that exists again by the time the hook connects.

Add `pub mod protocol;` and the re-exports to `crates/mcpls-core/src/hooks/mod.rs`.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core hooks::protocol`
Expected: PASS.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both clean. This is where a missing doc comment on a new public variant shows up.

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/hooks/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(hooks): define the socket wire protocol

Newline-delimited JSON, one request per line, so a connection can
carry a batch's changed and flush together.

The host's event kind is carried but documented as a hint: an atomic
save through a temporary file and a rename arrives as an unlink for a
file that exists again by the time the hook connects, so the sweep
stats rather than trusting it.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 16: the listener and its ownership lock

**Files:**
- Create: `crates/mcpls-core/src/hooks/listener.rs`
- Create: `crates/mcpls-core/tests/hooks_socket.rs`
- Modify: `crates/mcpls-core/src/hooks/mod.rs`, `crates/mcpls-core/Cargo.toml`

**Interfaces:**
- Consumes: `SocketIdentity` from Task 14, `Request`/`Response` from Task 15.
- Produces:

```rust
/// A bound listener, and the lock proving this process owns it.
pub struct HookListener { ... }

impl HookListener {
    /// Take ownership of `identity`'s socket, or report that someone else
    /// holds it.
    pub async fn acquire(identity: &SocketIdentity) -> Result<Option<Self>>;
    /// Serve connections until `cancel` fires, answering every op within
    /// `op_deadline` whether or not the handler has finished.
    pub async fn serve<H>(
        self,
        handler: H,
        op_deadline: Duration,
        cancel: watch::Receiver<bool>,
    )
    where H: Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static;
}

/// Send one request to whoever owns `identity`'s socket.
pub async fn send(identity: &SocketIdentity, request: &Request, timeout: Duration) -> Result<Response>;

/// Send several requests down one connection, in order, and collect the
/// answers.
///
/// The spec's protocol says `PostToolBatch` sends `changed` then `flush` on
/// one connection, and the framing already allows it: a connection carries
/// one or more requests. `send` is this with a one-element slice.
pub async fn send_many(
    identity: &SocketIdentity,
    requests: &[Request],
    timeout: Duration,
) -> Result<Vec<Response>>;
```

`op_deadline` is a `serve` parameter rather than state on `HookListener` because the value lives on `HooksConfig` (`config.diagnostics.hooks.op_deadline_ms`), which `acquire` has no reason to see. Task 19 passes `Duration::from_millis(config.diagnostics.hooks.op_deadline_ms)`.

`BoxFuture` comes from `futures`, already a direct dependency of `mcpls-core` (`crates/mcpls-core/Cargo.toml:17`). No manifest change for it.

- [ ] **Step 1: add the lock dependency**

In the workspace `Cargo.toml`:

```toml
fs4 = { version = "0.12", features = ["sync"] }
```

and in `crates/mcpls-core/Cargo.toml`, `fs4 = { workspace = true }`.

- [ ] **Step 2: write the failing integration tests**

`crates/mcpls-core/tests/hooks_socket.rs`:

An integration test is its own crate root, so the lint allow is an inner attribute at the top of the file rather than an attribute on a module.

```rust
//! The hook socket, over a temporary runtime directory.
//!
//! These are integration tests rather than unit tests because what they
//! check is ownership between processes-worth of state: two listeners
//! racing, a stale file, a lock outliving a socket.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::future::BoxFuture;
use mcpls_core::hooks::{HookListener, Request, Response, SocketIdentity, send};
use tempfile::TempDir;

/// A `SocketIdentity` whose socket and lock live inside a `TempDir`, so no
/// test touches the real runtime directory.
///
/// The guard is returned rather than dropped: dropping it deletes the
/// directory the socket lives in.
///
/// On Windows the socket is a pipe name, which is not a filesystem path, so
/// the guard covers only the lock there and the pipe name is made unique
/// with the same random suffix.
fn temp_identity() -> (TempDir, SocketIdentity) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let hash = format!("{:016x}", rand_suffix());
    #[cfg(windows)]
    let socket = std::path::PathBuf::from(format!(r"\\.\pipe\mcpls-test-{hash}"));
    #[cfg(not(windows))]
    let socket = dir.path().join(format!("{hash}.sock"));
    let identity = SocketIdentity {
        socket,
        lock: dir.path().join(format!("{hash}.lock")),
        hash,
    };
    (dir, identity)
}

/// A per-test suffix, so two tests running in parallel never collide on a
/// Windows pipe name, which is process-global rather than directory-scoped.
///
/// `TempDir` already gives uniqueness on Unix; this is what gives it on
/// Windows. Derived from the current thread id and the clock rather than
/// from a new random-number dependency.
fn rand_suffix() -> u64 { /* hash of std::thread::current().id() and SystemTime::now() */ }

/// A handler literal, annotated so it coerces to the `Fn(Request) ->
/// BoxFuture<'static, Response>` bound `serve` declares.
///
/// Without the return-type annotation the closure's opaque future type does
/// not unify with `BoxFuture`, and the error points at `serve` rather than
/// at the closure.
fn handler(
    f: impl Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static,
) -> impl Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static {
    f
}

#[tokio::test]
async fn test_one_listener_acquires_and_a_second_defers() {
    let (_guard, identity) = temp_identity();
    let first = HookListener::acquire(&identity).await.expect("acquire").expect("the first owns it");
    let second = HookListener::acquire(&identity).await.expect("acquire");

    assert!(second.is_none(), "the lock is held");
    drop(first);
}

#[tokio::test]
async fn test_a_second_listener_acquires_after_the_owner_drops() {
    let (_guard, identity) = temp_identity();
    let first = HookListener::acquire(&identity).await.expect("acquire").expect("owner");
    drop(first);

    let second = HookListener::acquire(&identity).await.expect("acquire");
    assert!(second.is_some(), "an owner exiting must not strand every later instance");
}

#[tokio::test]
async fn test_a_stale_socket_file_does_not_block_acquisition() {
    let (_guard, identity) = temp_identity();
    std::fs::create_dir_all(identity.socket.parent().expect("a parent")).expect("mkdir");
    std::fs::write(&identity.socket, b"").expect("a stale file where a socket used to be");

    let listener = HookListener::acquire(&identity).await.expect("acquire");
    assert!(
        listener.is_some(),
        "a crashed owner leaves its socket file behind and nothing else \
         will ever clean it up"
    );
}

#[tokio::test]
async fn test_exactly_one_of_many_racing_acquirers_wins() {
    let (_guard, identity) = temp_identity();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let identity = identity.clone();
        set.spawn(async move { HookListener::acquire(&identity).await.expect("acquire").is_some() });
    }
    let mut winners = 0;
    while let Some(result) = set.join_next().await {
        if result.expect("the task") {
            winners += 1;
        }
    }
    assert_eq!(
        winners, 1,
        "renaming a temp socket into place is atomic but not exclusive, so \
         several racers would each believe they won and all but one would \
         be orphaned with no way to notice"
    );
}

#[tokio::test]
async fn test_a_request_reaches_the_owner_and_is_answered() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity).await.expect("acquire").expect("owner");
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    tokio::spawn(listener.serve(
        handler(|_req| Box::pin(async { Response::Flush { context: Some("hello".to_string()) } })),
        Duration::from_millis(1500),
        cancel,
    ));

    let response = send(
        &identity,
        &Request::Flush { session: "s1".to_string() },
        Duration::from_secs(5),
    )
    .await
    .expect("the owner answers");

    assert_eq!(response, Response::Flush { context: Some("hello".to_string()) });
}

/// The server's own deadline, not the client's.
///
/// The client timeout here is 5 seconds and the server deadline is 200
/// milliseconds, so a run with no server-side deadline sits for the whole 5
/// seconds and then returns `Err`. Both assertions below fail in that case.
/// The reverse arrangement, a 30 second handler under a 1500 ms client
/// timeout, would be satisfied by the client erroring and would prove
/// nothing about the server.
#[tokio::test]
async fn test_an_op_answers_within_its_deadline_while_its_work_runs_on() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity).await.expect("acquire").expect("owner");
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    tokio::spawn(listener.serve(
        handler(|_req| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Response::Flush { context: None }
            })
        }),
        Duration::from_millis(200),
        cancel,
    ));

    let started = std::time::Instant::now();
    let response = send(
        &identity,
        &Request::Flush { session: "s1".to_string() },
        Duration::from_secs(5),
    )
    .await;

    assert!(
        matches!(response, Ok(Response::Error { .. })),
        "a hook that hangs blocks the agent, and the host's own timeout is \
         600 seconds, so the bound has to be ours and it has to answer \
         rather than drop the connection; got {response:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "and it has to answer at the deadline, not when the work finishes"
    );
}

#[tokio::test]
async fn test_two_requests_share_one_connection() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity).await.expect("acquire").expect("owner");
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    tokio::spawn(listener.serve(
        handler(|req| {
            Box::pin(async move {
                match req {
                    Request::Changed { paths, .. } => Response::Changed { queued: paths.len() },
                    _ => Response::Flush { context: Some("drained".to_string()) },
                }
            })
        }),
        Duration::from_millis(1500),
        cancel,
    ));

    let answers = mcpls_core::hooks::send_many(
        &identity,
        &[
            Request::Changed {
                session: "s1".to_string(),
                paths: vec![std::path::PathBuf::from("a.rs")],
                event: mcpls_core::hooks::ChangeEvent::Change,
            },
            Request::Flush { session: "s1".to_string() },
        ],
        Duration::from_secs(5),
    )
    .await
    .expect("the owner answers both");

    assert_eq!(
        answers,
        vec![
            Response::Changed { queued: 1 },
            Response::Flush { context: Some("drained".to_string()) },
        ],
        "the spec's PostToolBatch sends changed then flush on one \
         connection, and the answers come back in the order they were sent"
    );
}

#[tokio::test]
async fn test_sending_to_nobody_fails_fast() {
    let (_guard, identity) = temp_identity();
    let started = std::time::Instant::now();
    let result = send(
        &identity,
        &Request::Status,
        Duration::from_millis(50),
    )
    .await;

    assert!(result.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "an edit must never wait on diagnostics that are not there"
    );
}
```

`SocketIdentity`'s three fields are `pub` (Task 14), so `temp_identity` builds one by hand rather than going through `identity_for`, which would put the socket in the real runtime directory. Skip `test_a_stale_socket_file_does_not_block_acquisition` on Windows with `#[cfg(unix)]`, and say in a comment that a named pipe is not a filesystem object and leaves nothing behind for a crashed owner to strand.

- [ ] **Step 3: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core --test hooks_socket`
Expected: FAIL to compile.

- [ ] **Step 4: implement**

Ownership is the lock, never a connect probe. Acquire in this order:

1. Create the runtime directory.
2. Open or create the lock file and take `try_lock_exclusive` on it. Failure means someone else owns the socket; return `Ok(None)`.
3. Holding the lock, remove any existing socket file. Whoever holds the lock is the only process that may do this, which is what makes it safe.
4. Bind.

The lock file handle lives in `HookListener` for as long as it does, so dropping the listener releases ownership. Do not unlock explicitly; a process that dies releases it too, which is the property that makes a crashed owner recoverable.

On Windows there is no lock file. `tokio::net::windows::named_pipe::ServerOptions::new().first_pipe_instance(true).create(&identity.socket)` fails with `ERROR_ACCESS_DENIED` when the pipe exists, which is the same exclusivity, so `acquire` maps that error to `Ok(None)` and everything else to `Err`.

Put both behind one trait so `serve` and `send` are written once:

```rust
/// One end of the hook transport, so the Unix socket and the Windows named
/// pipe differ in one place rather than throughout.
trait HookTransport: Send + Sync {
    /// Wait for the next client.
    fn accept(&self) -> BoxFuture<'_, std::io::Result<Box<dyn HookStream>>>;
}

/// One client connection.
trait HookStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
```

`serve` loops on `accept`, spawning a task per connection that reads newline-delimited requests, calls the handler under `tokio::time::timeout(op_deadline, ...)`, and writes one response line per request. `op_deadline` is `serve`'s own parameter, so the value that reaches it is the configured `op_deadline_ms` and nothing has to reach inside the listener to set it.

A handler that outruns the deadline gets `Response::Error { message }` written for that request while its future keeps running, so the client always gets a line back rather than a dropped connection. Spawn the handler's future with `tokio::spawn` and `timeout` the join handle, rather than `timeout`ing the future itself, or the work is cancelled at the deadline instead of continuing:

```rust
        let work = tokio::spawn(handler(request));
        let answer = match tokio::time::timeout(op_deadline, work).await {
            Ok(Ok(response)) => response,
            Ok(Err(_join_error)) => Response::Error {
                message: "the handler panicked".to_string(),
            },
            Err(_elapsed) => Response::Error {
                message: format!("op exceeded {}ms; its work continues and \
                                  reaches the next flush", op_deadline.as_millis()),
            },
        };
```

The doc comment on `serve` must say that the deadline answers rather than cancels, because a reader who "fixes" it into a plain `timeout` on the future would silently drop every sweep that ran long.

`send` connects with `tokio::time::timeout(timeout, ...)`, writes one line, reads one line, and returns. `send_many` does the same with several lines, writing all of them and then reading one answer line per request, in order, under one overall timeout. `send` is `send_many` with a one-element slice, so the framing lives in one place. Every failure is an `Err`; the caller in Task 20 turns every `Err` into exit 0 with no output.

- [ ] **Step 5: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core --test hooks_socket`
Expected: PASS.

- [ ] **Step 6: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 7: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/hooks/ crates/mcpls-core/tests/hooks_socket.rs crates/mcpls-core/Cargo.toml Cargo.toml Cargo.lock
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(hooks): own the socket through a lock

Deciding ownership by connecting cannot be made exclusive: rename is
atomic but not exclusive, so two instances that both saw a refusal
would both bind and the loser could never notice. An advisory lock
held for the owner's whole life settles it, and a process that dies
releases it.

Every op answers within its deadline whether or not the work behind it
has finished, because the host's own hook timeout is 600 seconds and a
hook that hangs blocks the agent.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 17: which paths are worth looking at

**Files:**
- Create: `crates/mcpls-core/src/hooks/filters.rs`
- Modify: `crates/mcpls-core/src/hooks/mod.rs`

**Interfaces:**
- Consumes: `WatchRegistry::servers_for` from Task 6.
- Produces:

```rust
/// Decides which changed paths are worth waking a language server for.
pub struct PathFilter { ... }

impl PathFilter {
    #[must_use]
    pub fn new(roots: Arc<[PathBuf]>, extensions: Arc<HashMap<String, String>>, registry: Option<Arc<WatchRegistry>>) -> Self;
    /// Whether `path` survives both filters.
    #[must_use]
    pub fn admits(&self, path: &Path) -> bool;
}

/// The directories and root-level files a session's watcher should cover.
#[must_use]
pub fn watch_paths(root: &Path) -> Vec<PathBuf>;
```

- [ ] **Step 1: write the failing tests**

In a new `#[cfg(test)] mod tests` at the bottom of `crates/mcpls-core/src/hooks/filters.rs`, opening with `#[allow(clippy::unwrap_used, clippy::expect_used)]`. Three helpers, which every test below uses and none of which exists yet:

```rust
    /// The configured roots for a filter over one temporary directory.
    fn roots(dir: &Path) -> Arc<[PathBuf]> {
        Arc::from(vec![dir.to_path_buf()])
    }

    /// The extension map a filter routes on, matching the built-in rust
    /// entry. `.xyz` is deliberately absent, so the registry-override test
    /// has something unroutable to admit.
    fn extensions() -> Arc<HashMap<String, String>> {
        Arc::new(HashMap::from([("rs".to_string(), "rust".to_string())]))
    }

    /// A filter over `dir` with no watch registry, which is the ordinary
    /// case: only the registry-override test passes one.
    fn filter_over(dir: &Path) -> PathFilter {
        PathFilter::new(roots(dir), extensions(), None)
    }
```

```rust
#[test]
fn test_a_build_artifact_is_dropped() {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::create_dir_all(dir.path().join("target/debug")).expect("mkdir");
    let artifact = dir.path().join("target/debug/thing.rs");
    std::fs::write(&artifact, "").expect("write");
    let filter = filter_over(dir.path());

    assert!(
        !filter.admits(&artifact),
        "a cargo check would otherwise fill the tracker to its ceiling and \
         every later tool call would fail with DocumentLimitExceeded"
    );
}

#[test]
fn test_a_gitignored_file_is_dropped() {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::write(dir.path().join(".gitignore"), "generated.rs\n").expect("write");
    let generated = dir.path().join("generated.rs");
    std::fs::write(&generated, "").expect("write");

    assert!(!filter_over(dir.path()).admits(&generated));
}

#[test]
fn test_a_path_outside_every_root_is_dropped() {
    let inside = tempfile::tempdir().expect("a temp dir");
    let outside = tempfile::tempdir().expect("a temp dir");
    let stray = outside.path().join("a.rs");
    std::fs::write(&stray, "").expect("write");

    assert!(!filter_over(inside.path()).admits(&stray));
}

#[test]
fn test_a_routable_extension_is_admitted() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let source = dir.path().join("src/a.rs");
    std::fs::create_dir_all(source.parent().expect("a parent")).expect("mkdir");
    std::fs::write(&source, "").expect("write");

    assert!(filter_over(dir.path()).admits(&source));
}

#[test]
fn test_an_unroutable_extension_with_no_watcher_is_dropped() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let blob = dir.path().join("notes.xyz");
    std::fs::write(&blob, "").expect("write");

    assert!(!filter_over(dir.path()).admits(&blob));
}

#[test]
fn test_an_unroutable_extension_a_server_registered_for_is_admitted() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let blob = dir.path().join("notes.xyz");
    std::fs::write(&blob, "").expect("write");
    let registry = Arc::new(WatchRegistry::new());
    registry.register(&ServerId::from("weird"), "r1", &serde_json::json!([{ "globPattern": "**/*.xyz" }]));

    let filter = PathFilter::new(roots(dir.path()), extensions(), Some(registry));
    assert!(
        filter.admits(&blob),
        "a server that asked for a pattern is the authority on whether that \
         file matters to it"
    );
}

#[test]
fn test_watch_paths_names_children_rather_than_the_root() {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::create_dir_all(dir.path().join("target")).expect("mkdir");
    std::fs::create_dir_all(dir.path().join(".git")).expect("mkdir");
    std::fs::write(dir.path().join("Cargo.toml"), "").expect("write");
    std::fs::write(dir.path().join(".gitignore"), "/target\n").expect("write");

    let paths = watch_paths(dir.path());

    assert!(paths.contains(&dir.path().join("src")));
    assert!(paths.contains(&dir.path().join("Cargo.toml")));
    assert!(
        !paths.contains(&dir.path().join("target")),
        "the host's watcher passes no ignore list, so this list is the only \
         thing keeping a hook process off every build artifact"
    );
    assert!(!paths.contains(&dir.path().join(".git")));
}
```

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core hooks::filters`
Expected: FAIL to compile.

- [ ] **Step 3: implement**

`admits` runs the two filters in order, cheapest first:

1. The path resolves under one of `roots` and is not ignored. Use `ignore::gitignore::GitignoreBuilder` rooted at the matching root, built once in `PathFilter::new` and reused, rather than walking for every path.
2. The path's extension is in `extensions`, or `registry.servers_for(path, FileChangeType::CHANGED)` is non-empty.

`watch_paths` walks `root` to depth one with `ignore::WalkBuilder::new(root).max_depth(Some(1))`, collecting every entry except `root` itself. Both directories and files come back, which is what the host expects.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core hooks::filters`
Expected: PASS, seven tests.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/hooks/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(hooks): filter which changed paths matter

The host's file watcher passes no ignore list, so watchPaths bounds
what is watched and two filters bound what is acted on: inside a
configured root and not gitignored, then a routable extension or a
glob some server registered.

Without them a cargo check would fill the document tracker to its
ceiling and every later tool call would fail with
DocumentLimitExceeded.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 18: the debounced sweep

**Files:**
- Create: `crates/mcpls-core/src/hooks/sweep.rs`
- Modify: `crates/mcpls-core/src/hooks/mod.rs`

**Interfaces:**
- Consumes: `PathFilter::admits` from Task 17; `Translator::{queue_invalidations, resync_changed_documents, notify_watched_files}`, all `pub(crate)`, from Tasks 4 and 7; `DocumentTracker::open_paths` (`bridge/state.rs:452`).
- Produces:

```rust
/// Collects changed paths and acts on them once the burst settles.
pub struct Sweeper { ... }

impl Sweeper {
    #[must_use]
    pub fn new(translator: Arc<Translator>, filter: PathFilter, quiet_for: Duration, max_documents: usize) -> Self;
    /// Queue paths. Returns how many survived the filters.
    pub fn enqueue(&self, paths: &[PathBuf]) -> usize;
    /// Run until `cancel` fires, sweeping whenever the set goes quiet.
    pub async fn run(self: Arc<Self>, cancel: watch::Receiver<bool>);
    /// Files the last sweep could not check, and why.
    #[must_use]
    pub fn last_shortfall(&self) -> Option<String>;

    /// Sweep immediately, ignoring the debounce. Test-only.
    #[cfg(test)]
    pub(crate) async fn sweep_now(&self);
    /// How many sweeps have run. Test-only.
    #[cfg(test)]
    pub(crate) fn sweeps_run(&self) -> usize;
    /// What the last sweep decided each path was. Test-only.
    #[cfg(test)]
    pub(crate) fn last_kinds(&self) -> Vec<(PathBuf, SweepKind)>;
    /// How many untracked paths the last sweep opened. Test-only.
    #[cfg(test)]
    pub(crate) fn opened_count(&self) -> usize;
}

/// What a stat says a pending path actually is.
///
/// Derived from the filesystem at sweep time, never from the event kind the
/// host sent. A hook process is spawned per event and arrives late, and a
/// save through a temporary file and a rename produces `unlink` for a file
/// that exists again by now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepKind {
    /// Absent from disk.
    Deleted,
    /// Present, and the tracker has never held it.
    Created,
    /// Present, and either tracked or seen before.
    Changed,
}
```

**Why the four inspection accessors are `#[cfg(test)] pub(crate)`.** They are scaffolding and do not belong in a published API, so `#[doc(hidden)] pub` is the wrong shape for them. `#[cfg(test)]` items are compiled only for the library's own unit-test build and do not exist for an integration-test crate under `tests/`, so this choice binds Task 19: its tests live in the library, in `crates/mcpls-core/src/hooks/service.rs`, not in `crates/mcpls-core/tests/hooks_socket.rs`. Task 19 needs that anyway, because it also asserts on `McplsServer`'s non-public methods, which no integration-test crate can reach either. `pub(crate)` rather than private, because those tests are in a different module of the same crate. Tasks 18 and 19 agree on this; do not change one without the other.

- [ ] **Step 1: write the failing tests**

In a new `#[cfg(test)] mod tests` at the bottom of `crates/mcpls-core/src/hooks/sweep.rs`, opening with `#[allow(clippy::unwrap_used, clippy::expect_used)]`. The harness first:

```rust
    /// A `Sweeper` over a temporary workspace, with its `run` loop already
    /// spawned so the debounce tests can drive it with `tokio::time`.
    ///
    /// The translator has no registered servers. Nothing here asserts on
    /// what reaches a server: these tests are about which paths the sweep
    /// picks up, what it decides each one is, and where it stops. The
    /// notifications are covered by Tasks 4 and 7 against a fake server.
    struct TestSweeper {
        sweeper: Arc<Sweeper>,
        dir: TempDir,
        _cancel: tokio::sync::watch::Sender<bool>,
    }

    /// So a test can write `sweeper.enqueue(...)` rather than
    /// `sweeper.sweeper.enqueue(...)`.
    impl std::ops::Deref for TestSweeper {
        type Target = Sweeper;
        fn deref(&self) -> &Self::Target {
            &self.sweeper
        }
    }

    impl TestSweeper {
        /// An absolute path under the workspace. Creates nothing.
        fn path(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }

        /// An absolute path under the workspace, with an empty file at it.
        fn write(&self, rel: &str) -> PathBuf {
            let path = self.path(rel);
            std::fs::write(&path, "").expect("write");
            path
        }
    }

    fn test_sweeper(quiet_for: Duration) -> TestSweeper {
        test_sweeper_with_ceiling(quiet_for, usize::MAX)
    }

    fn test_sweeper_with_ceiling(quiet_for: Duration, max_documents: usize) -> TestSweeper {
        let dir = tempfile::tempdir().expect("a temp dir");
        let filter = PathFilter::new(
            Arc::from(vec![dir.path().to_path_buf()]),
            Arc::new(HashMap::from([("rs".to_string(), "rust".to_string())])),
            None,
        );
        let sweeper = Arc::new(Sweeper::new(
            Arc::new(Translator::new()),
            filter,
            quiet_for,
            max_documents,
        ));
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(Arc::clone(&sweeper).run(cancel_rx));
        TestSweeper {
            sweeper,
            dir,
            _cancel: cancel_tx,
        }
    }
```

Every path a test enqueues has to exist on disk unless the test is about a deletion, because `PathFilter::admits` resolves the path under a root and the sweep stats it. `write` is what creates them.

```rust
#[tokio::test(start_paused = true)]
async fn test_a_burst_produces_one_sweep() {
    let sweeper = test_sweeper(Duration::from_millis(500));
    for i in 0..50 {
        let path = sweeper.write(&format!("f{i}.rs"));
        sweeper.enqueue(&[path]);
        tokio::time::advance(Duration::from_millis(10)).await;
    }
    tokio::time::advance(Duration::from_millis(600)).await;

    assert_eq!(
        sweeper.sweeps_run(),
        1,
        "every didSave restarts rust-analyzer's flycheck and cancels the \
         check in flight, so a cargo fmt forwarded one path at a time \
         produces fifty cancelled checks and no diagnostics at all"
    );
}

#[tokio::test(start_paused = true)]
async fn test_a_path_arriving_during_the_quiet_period_restarts_it() {
    let sweeper = test_sweeper(Duration::from_millis(500));
    let a = sweeper.write("a.rs");
    sweeper.enqueue(&[a]);
    tokio::time::advance(Duration::from_millis(400)).await;
    let b = sweeper.write("b.rs");
    sweeper.enqueue(&[b]);
    tokio::time::advance(Duration::from_millis(400)).await;

    assert_eq!(sweeper.sweeps_run(), 0, "the burst has not settled");

    tokio::time::advance(Duration::from_millis(200)).await;
    assert_eq!(sweeper.sweeps_run(), 1);
}

#[tokio::test]
async fn test_a_deleted_path_is_swept_as_a_delete_whatever_the_host_said() {
    let sweeper = test_sweeper(Duration::from_millis(10));
    let path = sweeper.write("gone.rs");
    sweeper.enqueue(&[path.clone()]);
    std::fs::remove_file(&path).expect("remove");
    sweeper.sweep_now().await;

    assert_eq!(sweeper.last_kinds(), vec![(path, SweepKind::Deleted)]);
}

#[tokio::test]
async fn test_an_atomic_save_is_swept_as_a_change_not_a_delete() {
    let sweeper = test_sweeper(Duration::from_millis(10));
    let path = sweeper.write("saved.rs");

    // The host sends unlink and then add for a temp-file-and-rename save,
    // and the hook process arrives after both. Reproduce that: the file is
    // gone when the unlink is queued and back by the time the sweep stats
    // it, which is the whole point of stating rather than trusting the
    // event kind.
    std::fs::remove_file(&path).expect("unlink");
    sweeper.enqueue(&[path.clone()]);
    std::fs::write(&path, "").expect("the rename puts it back");

    sweeper.sweep_now().await;

    assert_eq!(
        sweeper.last_kinds(),
        vec![(path, SweepKind::Changed)],
        "acting on the host's unlink would close a live document and tell \
         every watching server the file was deleted"
    );
}

#[tokio::test]
async fn test_the_sweep_stops_short_of_the_document_ceiling() {
    let sweeper = test_sweeper_with_ceiling(Duration::from_millis(10), 3);
    let paths: Vec<PathBuf> = (0..10).map(|i| sweeper.write(&format!("f{i}.rs"))).collect();
    sweeper.enqueue(&paths);
    sweeper.sweep_now().await;

    assert_eq!(
        sweeper.last_shortfall().expect("a shortfall line"),
        "7 file(s) not checked: the document limit of 3 was reached",
        "asserted whole rather than by substring: a contains('7') would \
         also match 17, 27 or '7 of 70'"
    );
    assert!(
        sweeper.opened_count() <= 3,
        "filling the tracker would make the next unrelated tool call fail \
         with DocumentLimitExceeded, which is a worse outcome than not \
         checking some files"
    );
}
```

`test_an_atomic_save_is_swept_as_a_change_not_a_delete` enqueues while the file is absent, which is what a `PathFilter` sees for an unlink. If `admits` rejects an absent path, the filter must be the one that lets a deletion through, since a delete is a change the sweep has to act on; make `admits` decide on the path and the roots rather than on the file existing, and note that in `PathFilter`'s doc.

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core hooks::sweep`
Expected: FAIL to compile.

- [ ] **Step 3: implement**

`enqueue` runs each path through `PathFilter::admits`, inserts the survivors into a `Mutex<HashSet<PathBuf>>`, records `Instant::now()` as the last arrival, and returns how many survived. `run` loops on a `tokio::time::interval` of `quiet_for / 4`, sweeping when the set is non-empty and the last arrival is older than `quiet_for`.

A sweep takes the whole set, then, for each path:

1. Stat it. Absent is `SweepKind::Deleted`. Present and tracked is `SweepKind::Changed`. Present and untracked is `SweepKind::Created` when the tracker has never held it, and `SweepKind::Changed` otherwise.
2. Hand every tracked path, and every deleted one, to `translator.queue_invalidations(&paths)` and then `translator.resync_changed_documents().await`. That drain already stats each path itself, closes the absent ones, resyncs the present ones, and notifies the watching servers, so the sweep does not repeat any of it. Both methods are `pub(crate)` (Tasks 4 and 7), which is what lets `hooks::sweep` call them from another module.
3. For untracked paths whose extension routes to a server, open and save them, but only up to the remaining headroom: `max_documents.saturating_sub(tracker.open_paths().len())`. For the rest, call `translator.notify_watched_files(path, FileChangeType::CHANGED).await`, which costs no tracker slot, and record the shortfall.

The shortfall string is exactly `format!("{skipped} file(s) not checked: the document limit of {max} was reached")`, which is what Step 1's test asserts whole. Store it, and clear it at the start of every sweep so a stale line never reaches a later flush. The skipped paths are not remembered: the next change to any of them brings it back through the same filters.

The shortfall reaches the agent through the flush's output, which Task 19 wires into both doors.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core hooks::sweep`
Expected: PASS, five tests.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/hooks/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(hooks): sweep changed paths after a burst

Changes arrive in bursts and every didSave restarts rust-analyzer's
flycheck, cancelling the check in flight, so a cargo fmt forwarded one
path at a time would produce a run of cancelled checks and no
diagnostics. Collect the paths and act once the set goes quiet.

The kind comes from a stat at sweep time, never from the event kind the
host sent: an atomic save arrives as an unlink for a file that exists
again by the time the hook connects.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---


## Task 19: serve the ops from a running mcpls

**Files:**
- Create: `crates/mcpls-core/src/hooks/service.rs` (the role, the handler, the takeover retry, and this task's tests)
- Modify: `crates/mcpls-core/src/hooks/mod.rs`
- Modify: `crates/mcpls-core/src/hooks/sweep.rs` (`set_shortfall_for_test` and `pending_len`, two more `#[cfg(test)] pub(crate)` accessors beside Task 18's four)
- Modify: `crates/mcpls-core/src/mcp/handlers.rs` (`BridgeContext`)
- Modify: `crates/mcpls-core/src/mcp/server.rs` (the session key, the flush's rendering, the passive branches)
- Modify: `crates/mcpls-core/src/bridge/delivery.rs` (`SessionId::from_env_or_process`, `DiagnosticsDelivery::end_session`)
- Modify: `crates/mcpls-core/src/lib.rs` (`serve_with`)

**Interfaces:**
- Consumes: everything from Tasks 14 through 18.
- Produces:
  - `hooks::HookRole` and `hooks::Role`, the process's relationship to the socket, and `hooks::build_handler`, which turns an `Arc<McplsServer>`, an `Arc<Sweeper>` and the `SocketIdentity` being served into the handler `HookListener::serve` takes.
  - `McplsServer::from_context(context: Arc<BridgeContext>) -> Self`, `pub(crate)`, so `serve_with` can decide the hook role on the context before the server is built.
  - `BridgeContext` gains `pub hooks: Arc<HookRole>`.
  - `McplsServer::flush_for_hook(&self, session: &SessionId) -> Option<String>`, `pub(crate)`.
  - `McplsServer::forward_apply_targets(&self, files_written: &[String])`, `pub(crate)`.
  - `SessionId::from_env_or_process()`.
  - `DiagnosticsDelivery::end_session(&mut self, session: &SessionId)`.

**Three design decisions this task owns, each with the reason, so a later reader does not undo them.**

**One role field, not two booleans.** A process is one of three things, and two independent flags could disagree:

```rust
/// How this process relates to the project's hook socket.
///
/// Interior mutability because it changes at runtime: a passive instance
/// retries the lock every five seconds and becomes the owner when the
/// previous one exits.
pub struct HookRole(std::sync::Mutex<Role>);

/// A snapshot of [`HookRole`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// Hooks are off. One door, this process's own record, no socket.
    Disabled,
    /// This process holds the lock and serves every session's hooks.
    Owner,
    /// Another process holds it. This one's flush forwards there and its
    /// footer stays silent.
    Passive {
        /// Where to forward to.
        identity: SocketIdentity,
    },
}

impl HookRole {
    #[must_use] pub fn disabled() -> Self;
    #[must_use] pub fn owner() -> Self;
    #[must_use] pub fn passive(identity: SocketIdentity) -> Self;
    /// A cloned snapshot, so no `std::sync::Mutex` guard is held across an
    /// `.await`.
    #[must_use] pub fn get(&self) -> Role;
    /// Move this process from `Passive` to `Owner`, called by the takeover
    /// task once it wins the lock. The only runtime transition there is.
    pub fn promote_to_owner(&self);
}
```

`promote_to_owner` is the whole mutable surface. There is deliberately no way to reach `Passive` after construction, because `serve_with` tries the ownership lock before it builds the context (Step 8) and therefore knows which of the three variants this process starts in. A process that loses the lock is constructed `HookRole::passive(identity)`; a demotion mutator would have no caller.

**The concurrency discipline for `hooks`, which both the takeover task and the MCP handler read.** The field is an `Arc<HookRole>` shared by the takeover task, the socket handler and every tool call, so state it once here rather than leaving each reader to guess:

- The inner lock is a `std::sync::Mutex`, not a `tokio::sync::Mutex`. Every critical section is a clone or a single assignment with no `.await` inside it, which is what makes a blocking mutex correct on an async runtime.
- `get` clones the `Role` and drops the guard before returning, so no guard ever crosses an await point. Read the role by binding `hooks.get()` to a value; never hold a guard while sending on the socket.
- Take the lock with `crate::bridge::lock_std`, the crate's poison-tolerant helper, for the same reason every other `std::sync::Mutex` in the crate does: a panic in an unrelated task must not turn every later role read into a panic.
- The transition is one way and happens at most once. `Disabled` and `Owner` are terminal, `Passive` moves only to `Owner`, and only `hook_takeover_task` calls `promote_to_owner`, immediately before it returns. So no reader needs the role and the lock to be consistent under a compare-and-swap.
- The one race that reaching monotonicity leaves is benign. A tool call can read `Passive` in the window between the takeover task winning the lock and calling `promote_to_owner`, and will then forward to the socket this process is about to serve. It is answered by this process's own handler against the same record, which is the same answer it would have got from the local branch. The reverse, a reader sending to a socket nobody owns, cannot happen, because the role is never widened back to `Passive`.

`BridgeContext` gains `pub hooks: Arc<HookRole>`, and `BridgeContext::new` sets `Arc::new(HookRole::disabled())`, which is what every caller except `serve_with` wants and means no constructor gains another parameter. `serve_with` decides the role first, overwrites the field on the struct before wrapping it in an `Arc`, which the `pub` fields already allow, then hands the server that context through a new `pub(crate) fn McplsServer::from_context(context: Arc<BridgeContext>) -> Self`. `McplsServer::new` stays as it is and becomes a wrapper that builds the context and calls `from_context`, so no other call site changes.

**A passive instance forwards its apply targets from the MCP layer, not from the translator.** The spec says "an apply made through it sends its targets to the owner as a `changed`". The obvious reading is to put that in `resync_changed_documents`, but `Translator` has no socket, no `SocketIdentity` and no session id, and the spec's CLI section says plainly that "nothing host-specific reaches the bridge or the LSP layer". The apply's targets are already on the tool result as `files_written`, at the MCP layer, beside the footer guard and the role. So the forward lives there, in `McplsServer::forward_apply_targets`, called from the same three write tools that call `footer_if_written`. `Translator` is not changed by this task.

**The passive retry is a task, not a poll inside the flush.** The spec requires a passive instance to retry the lock every five seconds so an owner exiting does not strand it. Nothing in the plan did that before. A retry driven from the flush tool would only fire when an agent happened to call it, so it is a background task with the same cancellation channel every other pump uses.

- [ ] **Step 1: write the failing tests**

These go in `crates/mcpls-core/src/hooks/service.rs`'s `#[cfg(test)] mod tests`, opening with `#[allow(clippy::unwrap_used, clippy::expect_used)]`, and **not** in `crates/mcpls-core/tests/hooks_socket.rs`. They reach `Sweeper`'s `#[cfg(test)] pub(crate)` accessors from Task 18 and `McplsServer`'s `pub(crate)` methods, and an integration-test crate can see neither.

The harness, in full:

```rust
    /// An in-process owner: a real listener on a temporary socket, a real
    /// `McplsServer`, and a real `Sweeper`, wired by the same
    /// `build_handler` `serve_with` uses.
    struct HookHarness {
        dir: TempDir,
        identity: SocketIdentity,
        server: Arc<McplsServer>,
        sweeper: Arc<Sweeper>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        delivery: Arc<Mutex<DiagnosticsDelivery>>,
        _cancel: tokio::sync::watch::Sender<bool>,
    }

    impl HookHarness {
        /// An owner with an empty baseline adopted, so a flush answers a
        /// real report rather than `starting_up()`.
        async fn owner() -> Self;

        /// The same, with one error already in the notification cache for
        /// `broken.rs`, so the first flush has something to report.
        async fn owner_with_one_error() -> Self;

        /// An absolute path under this harness's temporary workspace.
        fn fixture(&self, rel: &str) -> PathBuf;

        /// Send one request over the real socket and return the answer.
        async fn send(&self, request: Request) -> Response {
            crate::hooks::send(&self.identity, &request, Duration::from_secs(5))
                .await
                .expect("the owner answers")
        }

        /// How many sweeps the owner's sweeper has run.
        fn sweeps_run(&self) -> usize {
            self.sweeper.sweeps_run()
        }

        /// A second `McplsServer` in the passive role, pointed at this
        /// harness's socket. Its own delivery record is empty and its own
        /// cache holds nothing, so anything it reports came from the owner.
        async fn passive_instance(&self) -> PassiveInstance;

        /// The same, with `footer = true` in its diagnostics config, which
        /// is the only configuration under which a footer could fire at all.
        async fn passive_instance_with_footer_enabled(&self) -> PassiveInstance;
    }

    /// A passive `McplsServer` and the pieces a test asserts against.
    struct PassiveInstance {
        server: McplsServer,
    }

    impl PassiveInstance {
        /// What the flush tool answers, as its raw JSON string.
        async fn call_flush_tool(&self) -> String {
            self.server
                .get_new_diagnostics()
                .await
                .expect("the flush tool answers")
        }
    }

    /// A `SocketIdentity` whose socket and lock live inside a fresh
    /// `TempDir`, built field by field rather than through `identity_for`,
    /// which would put the socket in the real runtime directory.
    ///
    /// Task 16's integration tests have a function of the same name, but it
    /// is in `tests/hooks_socket.rs` and no library test can call it, so
    /// this module carries its own copy.
    fn temp_identity() -> (TempDir, SocketIdentity);

    /// The `Arc<McplsServer>` and `Arc<Sweeper>` a takeover would start
    /// serving with: the same pieces `owner()` builds, over `dir`, with
    /// `role` already installed on the server's context, and no listener
    /// acquired.
    fn takeover_candidate(
        dir: &TempDir,
        role: Arc<HookRole>,
    ) -> (Arc<McplsServer>, Arc<Sweeper>);
```

`owner()` builds its own `TempDir`, a `SocketIdentity` whose socket and lock live inside it, an `McplsServer` over a fresh translator, cache, delivery and floors with `HookRole::owner()`, a `Sweeper` over a `PathFilter` rooted at the temp directory, then acquires the listener and spawns `serve(build_handler(Arc::clone(&server), Arc::clone(&sweeper), identity.clone()), Duration::from_millis(1500), cancel_rx)`. All three arguments are required: `build_handler` takes the identity so the `Status` arm can answer with the socket it is serving.

```rust
#[tokio::test]
async fn test_a_changed_op_queues_and_returns_without_sweeping() {
    let harness = HookHarness::owner().await;
    let path = harness.fixture("a.rs");
    std::fs::write(&path, "fn a() {}").expect("write");

    let response = harness
        .send(Request::Changed {
            session: "s1".to_string(),
            paths: vec![path],
            event: ChangeEvent::Change,
        })
        .await;

    assert_eq!(response, Response::Changed { queued: 1 });
    assert_eq!(
        harness.sweeps_run(),
        0,
        "the debounce cannot fit inside the op deadline, and coalescing the \
         burst is the point"
    );
}

#[tokio::test]
async fn test_a_flush_op_and_the_tool_share_one_record() {
    let harness = HookHarness::owner_with_one_error().await;
    let first = harness.send(Request::Flush { session: "s1".to_string() }).await;
    let Response::Flush { context: Some(text) } = first else {
        panic!("the first flush reports the error");
    };
    assert!(text.contains("broken.rs"));

    let second = harness.send(Request::Flush { session: "s1".to_string() }).await;
    assert_eq!(
        second,
        Response::Flush { context: None },
        "one report per problem, whichever door asked for it"
    );
}

#[tokio::test]
async fn test_two_sessions_have_independent_records() {
    let harness = HookHarness::owner_with_one_error().await;
    let _ = harness.send(Request::Flush { session: "s1".to_string() }).await;
    let other = harness.send(Request::Flush { session: "s2".to_string() }).await;

    assert!(
        matches!(other, Response::Flush { context: Some(_) }),
        "two agents in one directory share warm servers and not delivery state"
    );
}

#[tokio::test]
async fn test_ending_a_session_drops_its_record() {
    let harness = HookHarness::owner_with_one_error().await;
    let _ = harness.send(Request::Flush { session: "s1".to_string() }).await;
    let _ = harness.send(Request::EndSession { session: "s1".to_string() }).await;
    let again = harness.send(Request::Flush { session: "s1".to_string() }).await;

    assert!(matches!(again, Response::Flush { context: Some(_) }));
}

#[tokio::test]
async fn test_a_shortfall_reaches_the_flush_response() {
    let harness = HookHarness::owner().await;
    harness.sweeper.set_shortfall_for_test("7 file(s) not checked: the document limit of 3 was reached");

    let Response::Flush { context: Some(text) } =
        harness.send(Request::Flush { session: "s1".to_string() }).await
    else {
        panic!("a shortfall alone must still produce a context line");
    };
    assert!(
        text.contains("7 file(s) not checked"),
        "an agent that never learns some files went unchecked would read an \
         empty report as a clean workspace"
    );
}

#[tokio::test]
async fn test_a_passive_instance_forwards_its_flush_to_the_owner() {
    let harness = HookHarness::owner_with_one_error().await;
    let passive = harness.passive_instance().await;

    let raw = passive.call_flush_tool().await;
    assert!(
        raw.contains("broken.rs"),
        "a passive instance's servers are warm but nobody feeds them, so \
         reading its own record would report nothing while the owner's \
         record holds everything"
    );
}

/// Both halves of the spec's sentence about a passive instance: the footer
/// stays silent, and the apply targets still reach the owner.
///
/// Asserted against the two methods the write tools call rather than by
/// driving a real rename, because a rename needs a live language server and
/// what is under test is which branch the passive role takes.
#[tokio::test]
async fn test_a_passive_instance_runs_no_footer_but_still_reports_its_writes() {
    let harness = HookHarness::owner_with_one_error().await;
    let passive = harness.passive_instance_with_footer_enabled().await;
    let written = harness.fixture("written.rs");
    std::fs::write(&written, "fn a() {}").expect("write");

    assert!(
        passive.server.footer_if_written(true).await.is_none(),
        "the footer would consume from the passive's own record while the \
         next flush reads the owner's, so the same diagnostics arrive twice \
         from one door and never from the other"
    );

    passive
        .server
        .forward_apply_targets(&[written.display().to_string()])
        .await;

    assert_eq!(
        harness.sweeper.pending_len(),
        1,
        "otherwise the passive instance's own writes never reach the warm \
         servers the owner is feeding"
    );
}

#[tokio::test(start_paused = true)]
async fn test_a_passive_instance_takes_over_when_the_owner_exits() {
    let (dir, identity) = temp_identity();
    let owner = HookListener::acquire(&identity).await.expect("acquire").expect("owner");
    let role = Arc::new(HookRole::passive(identity.clone()));
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    let (server, sweeper) = takeover_candidate(&dir, Arc::clone(&role));
    tokio::spawn(hook_takeover_task(
        identity.clone(),
        Arc::clone(&role),
        server,
        sweeper,
        Duration::from_millis(1500),
        cancel,
    ));

    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(
        matches!(role.get(), Role::Passive { .. }),
        "the owner still holds the lock"
    );

    drop(owner);
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::task::yield_now().await;

    assert_eq!(
        role.get(),
        Role::Owner,
        "an owner exiting must not leave every other session permanently \
         passive, which is the failure the spec's five second retry exists \
         to prevent"
    );
    drop(dir);
}
```

`Sweeper::set_shortfall_for_test` and `Sweeper::pending_len` are two more `#[cfg(test)] pub(crate)` accessors, added here rather than in Task 18 because only these tests need them; put them beside Task 18's four and document them the same way.

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core hooks::service`
Expected: FAIL to compile.

- [ ] **Step 3: give the delivery core the two calls the socket needs**

In `crates/mcpls-core/src/bridge/delivery.rs`:

```rust
impl SessionId {
    /// The session id the host exported, or the per-process constant.
    ///
    /// Claude Code exports `CLAUDE_CODE_SESSION_ID` into the environment of
    /// the stdio MCP servers it spawns, and the hook payload carries the
    /// same value, so both doors key on one record. Where the variable is
    /// absent, a per-process constant is correct: one process per client is
    /// what stdio means.
    #[must_use]
    pub fn from_env_or_process() -> Self {
        std::env::var("CLAUDE_CODE_SESSION_ID")
            .ok()
            .filter(|id| !id.is_empty())
            .map_or_else(Self::process_default, Self::from)
    }
}

impl DiagnosticsDelivery {
    /// Drop `session`'s record, so a later flush for the same id starts
    /// from the baseline again.
    pub fn end_session(&mut self, session: &SessionId) {
        self.sessions.remove(session);
    }
}
```

Replace `SessionId::process_default()` with `SessionId::from_env_or_process()` at both places the MCP layer keys a flush: `get_new_diagnostics` (`mcp/server.rs:745`) and `footer_for_write` from Task 13. Without that the in-process door and the hook door key on different ids and each sees a record the other never touched.

- [ ] **Step 4: render a flush for the socket**

`NewDiagnosticsResult` is private to `mcp::server` and nothing outside it can name the type, which is fine: the socket wants text, not a struct. Add to `McplsServer`:

```rust
    /// `session`'s flush, rendered as the text a hook prints, or `None`
    /// when nothing changed.
    ///
    /// The same flush the tool runs, against the same record, so a hook and
    /// an agent never see the same diagnostic twice.
    pub(crate) async fn flush_for_hook(&self, session: &SessionId) -> Option<String> { ... }
```

It returns `None` when `changed`, `cleared` and `note` are all empty, so a hook with nothing to say prints nothing. Also make `get_new_diagnostics`, `footer_for_write` and `footer_if_written` `pub(crate)` rather than private, so `hooks::service`'s tests can drive them; they stay out of the public API.

- [ ] **Step 5: write the handler**

In `crates/mcpls-core/src/hooks/service.rs`:

```rust
/// The handler `HookListener::serve` runs, closing over the MCP server and
/// the sweeper.
///
/// Lock order, the same one the rest of the crate follows: delivery before
/// cache. Every path through here reaches both only by way of
/// `McplsServer::flush_for_hook`, which takes them in that order and drops
/// both before it awaits the payload build, so no arm of this match has to
/// take either lock itself. Do not add one that does.
pub fn build_handler(
    server: Arc<McplsServer>,
    sweeper: Arc<Sweeper>,
    identity: SocketIdentity,
) -> impl Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static {
    move |request| {
        let server = Arc::clone(&server);
        let sweeper = Arc::clone(&sweeper);
        let identity = identity.clone();
        Box::pin(async move {
            match request {
                Request::Changed { paths, .. } => {
                    // The event kind is discarded: the sweep stats.
                    Response::Changed { queued: sweeper.enqueue(&paths) }
                }
                Request::Flush { session } => {
                    let session = SessionId::from(session);
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(text) = server.flush_for_hook(&session).await {
                        parts.push(text);
                    }
                    if let Some(shortfall) = sweeper.last_shortfall() {
                        parts.push(shortfall);
                    }
                    Response::Flush {
                        context: (!parts.is_empty()).then(|| parts.join("\n")),
                    }
                }
                Request::EndSession { session } => {
                    server.end_session(&SessionId::from(session)).await;
                    Response::EndSession
                }
                Request::Status => Response::Status {
                    hash: identity.hash.clone(),
                    socket: identity.socket.clone(),
                    pid: std::process::id(),
                    owner: true,
                },
            }
        })
    }
}
```

Task 21 adds a field to `Response::Status` carrying the owner's canonical directory, and updates this arm to fill it. That is stated there too.

`McplsServer::end_session` is a two-line `pub(crate)` method taking the delivery lock and calling `DiagnosticsDelivery::end_session`.

- [ ] **Step 6: write the takeover retry**

```rust
/// Retry the ownership lock every five seconds while this process is
/// passive, and start serving the moment it wins.
///
/// Without this, an owner exiting leaves every other instance in the
/// project permanently passive, forwarding to a socket nobody is listening
/// on. Retrying the lock rather than probing the socket is what keeps the
/// arbitration exclusive: two processes that both saw a connection refused
/// would both bind.
async fn hook_takeover_task(
    identity: SocketIdentity,
    role: Arc<HookRole>,
    server: Arc<McplsServer>,
    sweeper: Arc<Sweeper>,
    op_deadline: Duration,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = cancel.changed() => return,
            _ = ticker.tick() => {
                match HookListener::acquire(&identity).await {
                    Ok(Some(listener)) => {
                        role.promote_to_owner();
                        let handler = build_handler(
                            Arc::clone(&server),
                            Arc::clone(&sweeper),
                            identity.clone(),
                        );
                        tokio::spawn(listener.serve(handler, op_deadline, cancel.clone()));
                        return;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, "could not retry the hook ownership lock");
                    }
                }
            }
        }
    }
}
```

`tokio::time::interval` fires immediately on its first `tick`, so the first retry happens at once and every five seconds after; that is what makes the taking-over case fast when the owner has already gone.

- [ ] **Step 7: give the passive role its two branches**

In `crates/mcpls-core/src/mcp/server.rs`:

```rust
    /// Forward the paths an apply wrote to the socket's owner.
    ///
    /// A passive instance's language servers are warm but nobody is feeding
    /// them, so a write made through this process would otherwise never
    /// reach the servers the owner's flush reads. Silent on every failure,
    /// for the same reason the hook is: a tool call must not fail because
    /// the socket was unavailable.
    pub(crate) async fn forward_apply_targets(&self, files_written: &[String]) {
        let Role::Passive { identity } = self.context.hooks.get() else {
            return;
        };
        if files_written.is_empty() {
            return;
        }
        let request = Request::Changed {
            session: SessionId::from_env_or_process().to_string(),
            paths: files_written.iter().map(PathBuf::from).collect(),
            event: ChangeEvent::Change,
        };
        if let Err(error) = hooks::send(&identity, &request, Duration::from_millis(50)).await {
            tracing::debug!(%error, "could not forward apply targets to the hook owner");
        }
    }
```

`footer_for_write` returns `None` for a passive instance **before** the config check, so enabling the footer in a passive instance's config still produces nothing:

```rust
        if matches!(self.context.hooks.get(), Role::Passive { .. }) {
            return None;
        }
        if !self.context.diagnostics.footer {
            return None;
        }
```

`get_new_diagnostics` forwards for a passive instance, falling back to its own record when the send fails, because a socket that has gone away is not a reason to answer nothing:

```rust
        if let Role::Passive { identity } = self.context.hooks.get() {
            let request = Request::Flush {
                session: SessionId::from_env_or_process().to_string(),
            };
            if let Ok(Response::Flush { context }) =
                hooks::send(&identity, &request, Duration::from_millis(50)).await
            {
                return to_tool_result(Ok(context.unwrap_or_default()));
            }
        }
```

At each of the three write tools, beside the footer call Task 13 added:

```rust
        self.forward_apply_targets(&result.files_written).await;
        let footer = self.footer_if_written(result.applied).await;
```

`format_document` and `apply_code_action` name their written files differently; read each result type rather than assuming `files_written` on all three.

- [ ] **Step 8: wire it in `serve_with`**

The order matters and is fixed, because the role has to be known before the context it lives on is frozen into an `Arc`, while the handler cannot be built until the server exists. Three phases, in this order.

**First, before the `BridgeContext` is built**, which is before the `McplsServer::new` call at `lib.rs:754` that Task 13 already edits, and after `cancel_rx` exists:

```rust
    let identity = hooks::identity_for(&std::env::current_dir()?)?;
    let ownership = if config.diagnostics.hooks.enabled {
        hooks::HookListener::acquire(&identity).await?
    } else {
        None
    };
    let role = match (config.diagnostics.hooks.enabled, &ownership) {
        (false, _) => HookRole::disabled(),
        (true, Some(_)) => HookRole::owner(),
        (true, None) => HookRole::passive(identity.clone()),
    };
```

Also when `hooks.enabled`, build the `PathFilter` from the workspace roots, the extension map and the watch registry Task 7 created, build the `Sweeper` from it, and spawn `Arc::clone(&sweeper).run(cancel_rx.clone())` whether this process owns the socket or not. A passive instance's sweeper is idle until it takes over, and building it unconditionally means the takeover has nothing left to construct.

**Second, build the context and the server.** Construct the `BridgeContext` explicitly rather than through `McplsServer::new`, overwrite `hooks` with `Arc::new(role)` before wrapping the struct in an `Arc`, and build the server with `McplsServer::from_context(Arc::clone(&context))`. This is why the acquire comes first: the losing process is constructed `Passive`, so `HookRole` needs no demotion mutator and only one runtime transition exists, `Passive` to `Owner`, described above.

**Third, once the server is in an `Arc`, spawn the socket side:**

- With `ownership` `Some(listener)`, spawn `listener.serve(build_handler(Arc::clone(&server), Arc::clone(&sweeper), identity.clone()), Duration::from_millis(config.diagnostics.hooks.op_deadline_ms), cancel_rx.clone())`.
- With `ownership` `None` and `hooks.enabled`, spawn `hook_takeover_task(identity.clone(), Arc::clone(&context.hooks), Arc::clone(&server), Arc::clone(&sweeper), Duration::from_millis(config.diagnostics.hooks.op_deadline_ms), cancel_rx.clone())`. It shares the same `Arc<HookRole>` the context holds, which is how its `promote_to_owner` becomes visible to the tool calls.

With `hooks.enabled` false, the role is `HookRole::disabled()`, no sweeper is built, nothing binds, and no task is spawned.

`run_stdio` takes an `McplsServer` by value while the handler needs one it can keep, so build the server, wrap it in an `Arc` for the handler and the takeover task, and pass a second `McplsServer::from_context(Arc::clone(&context))` to the transport. Both share one `Arc<BridgeContext>`, which is where all the state lives, so they are the same server in every sense that matters.

- [ ] **Step 9: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core`
Expected: PASS.

- [ ] **Step 10: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 11: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add \
  crates/mcpls-core/src/hooks/ \
  crates/mcpls-core/src/mcp/ \
  crates/mcpls-core/src/bridge/delivery.rs \
  crates/mcpls-core/src/lib.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(hooks): serve the socket ops from mcpls

The owner answers changed, flush, end_session and status against the
same per-session record the flush tool uses, so a hook and an agent
never see the same diagnostic twice.

A later instance is passive: its flush forwards to the owner, its
footer stays silent, and an apply made through it sends its targets to
the owner. It retries the lock every five seconds, so an owner exiting
does not strand it.

The forward lives at the MCP layer rather than in the translator,
which has no socket and which the design keeps free of host-specific
knowledge.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---


## Task 20: the `mcpls hook` subcommand

**Files:**
- Create: `crates/mcpls-cli/src/hook.rs`
- Modify: `crates/mcpls-cli/src/args.rs` (the `Command` enum at `:107`)
- Modify: `crates/mcpls-cli/src/main.rs`
- Modify: `crates/mcpls-cli/Cargo.toml`

**Interfaces:**
- Consumes: `hooks::{identity_for, send, send_many}`, `Request`, `Response` from Tasks 14 through 16; `hooks::watch_paths` from Task 17.
- Produces: `mcpls hook`, reading one hook payload from stdin and writing hook JSON to stdout, and the `mcpls hook doctor` subcommand shape Task 21 fills in.

`mcpls-cli` already depends on `mcpls-core`, and Task 14 makes `hooks` a `pub mod` of the library root, so `mcpls_core::hooks` is reachable. Two manifest additions it does not have:

```toml
[dependencies]
serde_json = { workspace = true }

[dev-dependencies]
futures = { workspace = true }
```

`serde_json` parses the hook payload and writes the hook JSON. `futures` is only for naming `BoxFuture` in the recording listener the tests bind, which is why it is a dev dependency.

- [ ] **Step 1: write the failing tests**

In a new `#[cfg(test)] mod tests` at the bottom of `crates/mcpls-cli/src/hook.rs`, opening with `#[allow(clippy::unwrap_used, clippy::expect_used)]`. Every test is `#[tokio::test]`, because everything they drive goes through `hooks::send`, which is `async`.

Four helpers, none of which exists yet:

```rust
    /// Run the dispatcher over one payload, against the socket identity
    /// `project_dir` derives, and return what it printed.
    ///
    /// Nothing is listening on that identity unless the test bound one,
    /// which is the point of most of these: the silent-failure rule says an
    /// unreachable socket prints nothing and exits zero.
    async fn dispatch(payload: &serde_json::Value, project_dir: &Path) -> String {
        dispatch_raw(&payload.to_string(), project_dir).await
    }

    /// The same, from raw stdin bytes, so a payload that is not JSON at all
    /// goes through the same path.
    async fn dispatch_raw(stdin: &str, project_dir: &Path) -> String {
        let identity = mcpls_core::hooks::identity_for(project_dir).expect("identity");
        super::dispatch_payload(stdin, project_dir, &identity).await
    }

    /// Run the dispatcher against a listener that records the ops it gets.
    async fn dispatch_against(
        payload: &serde_json::Value,
        recorder: &RecordingOwner,
    ) -> String {
        super::dispatch_payload(
            &payload.to_string(),
            recorder.project_dir(),
            &recorder.identity,
        )
        .await
    }

    /// A listener on a temporary socket that records every op it is sent
    /// and answers each with the shape the dispatcher expects.
    struct RecordingOwner {
        dir: tempfile::TempDir,
        identity: mcpls_core::hooks::SocketIdentity,
        ops: Arc<std::sync::Mutex<Vec<String>>>,
        _cancel: tokio::sync::watch::Sender<bool>,
    }

    impl RecordingOwner {
        /// Bind and start serving. The socket and the lock live in the
        /// returned temporary directory, so no test touches the real
        /// runtime path.
        async fn start() -> Self;
        /// The directory the dispatcher treats as `CLAUDE_PROJECT_DIR`.
        fn project_dir(&self) -> &Path;
        /// The op names received, in order: "changed", "flush", and so on.
        fn ops(&self) -> Vec<String>;
    }
```

`dispatch_payload(stdin: &str, project_dir: &Path, identity: &SocketIdentity) -> String` is the function Step 3 writes: the whole dispatcher with its two inputs passed in rather than read from the environment, so no test has to set `CLAUDE_PROJECT_DIR` on a process it shares with every other test. `main.rs` calls it with `CLAUDE_PROJECT_DIR` and `identity_for` of that directory.

```rust
#[tokio::test]
async fn test_session_start_returns_watch_paths_without_a_socket() {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");

    let out = dispatch(&json!({ "hook_event_name": "SessionStart" }), dir.path()).await;

    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    let paths = parsed["hookSpecificOutput"]["watchPaths"]
        .as_array()
        .expect("watchPaths");
    assert!(!paths.is_empty());
    // No listener is bound in this test at all.
    assert!(
        !out.contains("error"),
        "SessionStart fires while the host is still spawning the MCP server, \
         so asking a socket that is not bound yet would leave the session \
         with either no coverage or an unbounded watcher over target/"
    );
}

#[tokio::test]
async fn test_an_unreachable_socket_produces_no_output_and_exit_zero() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dispatch(
        &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
        dir.path(),
    )
    .await;
    assert_eq!(out, "", "an edit must never fail because diagnostics were unavailable");
}

#[tokio::test]
async fn test_an_unknown_event_produces_no_output() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dispatch(&json!({ "hook_event_name": "Whatever" }), dir.path()).await;
    assert_eq!(out, "");
}

#[tokio::test]
async fn test_a_malformed_payload_produces_no_output() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dispatch_raw("not json at all", dir.path()).await;
    assert_eq!(out, "");
}

#[tokio::test]
async fn test_post_tool_batch_sends_changed_then_flush() {
    let recorder = RecordingOwner::start().await;
    let file = recorder.project_dir().join("a.rs");
    std::fs::write(&file, "fn a() {}").expect("write");
    let out = dispatch_against(
        &json!({
            "hook_event_name": "PostToolBatch",
            "session_id": "s1",
            "tool_calls": [{ "tool_input": { "file_path": file.display().to_string() } }]
        }),
        &recorder,
    )
    .await;

    assert_eq!(
        recorder.ops(),
        vec!["changed", "flush"],
        "changed queues for the sweep and flush drains the record; the pair \
         is what makes a batch's own paths reach the servers"
    );
    assert!(out.contains("additionalContext"));
    assert_eq!(
        recorder.connections(),
        1,
        "the spec's protocol says PostToolBatch sends changed then flush on \
         one connection, which is what send_many is for"
    );
}
```

`RecordingOwner::connections()` counts how many times its listener accepted, alongside `ops()`.

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-cli hook`
Expected: FAIL to compile.

- [ ] **Step 3: implement**

Add to `Command` in `crates/mcpls-cli/src/args.rs`:

```rust
    /// Serve one Claude Code hook invocation
    ///
    /// With no argument, reads the hook payload from stdin and writes hook
    /// JSON to stdout, dispatching on the payload's own `hook_event_name`.
    /// One subcommand rather than five means no shell script and the same
    /// registrations work on Windows.
    Hook {
        /// What to do instead of reading a hook payload from stdin
        #[command(subcommand)]
        action: Option<HookAction>,
    },
}

/// What `mcpls hook` can do besides serving a hook invocation.
#[derive(Debug, Subcommand)]
pub enum HookAction {
    /// Print the socket path, both directory hashes, the owner's pid and
    /// liveness, and whether mcpls resolves on PATH
    Doctor,
}
```

A nested `#[command(subcommand)]` rather than `#[arg(long)] doctor: bool`, because a flag produces `mcpls hook --doctor` and the spec's CLI layout, this plan's Task 21 and Task 22's manual gate all spell it `mcpls hook doctor`. `Option<HookAction>` is what keeps the bare `mcpls hook` working, which is the form the five hook registrations use.

`main.rs` matches `Command::Hook { action }`: `None` reads stdin and calls `dispatch_payload`, `Some(HookAction::Doctor)` calls the doctor Task 21 writes.

Dispatch on `hook_event_name`:

| Event | Action | Output |
|---|---|---|
| `SessionStart` | `watch_paths(CLAUDE_PROJECT_DIR)`, computed locally | `hookSpecificOutput.watchPaths` |
| `FileChanged` | `Changed` for `file_path` | none |
| `PostToolBatch` | `Changed` for every `file_path` in `tool_calls`, then `Flush`, both through one `send_many` on one connection | `hookSpecificOutput.additionalContext` when the flush returned any |
| `UserPromptSubmit` | `Flush` | `hookSpecificOutput.additionalContext` when non-empty |
| `SessionEnd` | `EndSession` | none |
| anything else | nothing | none |

Every fault, a missing socket, a connect timeout of 50 ms, a malformed response, a payload that will not parse, exits 0 having printed nothing. Write that as one wrapper so no branch can forget it:

```rust
/// Await `body`, and swallow whatever it does wrong.
///
/// An edit must never fail because diagnostics were unavailable, so there
/// is exactly one exit code and it is zero. The cost is that a broken
/// installation is invisible, which is what `mcpls hook doctor` exists to
/// answer.
///
/// Takes a future rather than a closure: everything it wraps goes through
/// `hooks::send`, which is `async`, and a synchronous `FnOnce` could not
/// contain the await, so the one-wrapper guarantee would not hold.
async fn silently<T: Default>(body: impl Future<Output = Result<T>>) -> T {
    body.await.unwrap_or_default()
}
```

`dispatch_payload` is therefore `async fn dispatch_payload(stdin: &str, project_dir: &Path, identity: &SocketIdentity) -> String`, and its body is one `silently(...)` around the whole match, so every fault, a missing socket, a 50 ms connect timeout, a malformed response, a payload that will not parse, produces the empty string and exit 0. `main.rs` prints whatever it returns and exits 0.

`Stop` is deliberately absent from the table. Its `additionalContext` is documented as non-error feedback after which the conversation continues so the model can act on it, so flushing there would turn every new warning into a keep-working signal. The next `UserPromptSubmit` delivers the same diagnostics anyway. Put that in a comment beside the match, since the next reader's first instinct will be to add it.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-cli`
Expected: PASS, five tests.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-cli/src/ crates/mcpls-cli/Cargo.toml Cargo.lock
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(cli): dispatch claude code hooks

One subcommand reading the payload from stdin and dispatching on its own
hook_event_name, rather than five registrations and a shell script, so
the same definitions work on Windows.

Every fault exits zero having printed nothing: an edit must never fail
because diagnostics were unavailable. That makes a broken install
invisible, which is what mcpls hook doctor answers.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---

## Task 21: `mcpls hook doctor`

Silent failure makes a broken install invisible. This is the one thing that answers it, so it is not optional polish.

**Files:**
- Modify: `crates/mcpls-cli/src/hook.rs`
- Modify: `crates/mcpls-core/src/hooks/protocol.rs` (`Response::Status` gains a field, and Task 15's round-trip test)
- Modify: `crates/mcpls-core/src/hooks/service.rs` (Task 19's `Status` arm fills the new field)

**Interfaces:**
- Consumes: `identity_for`, `send`, `Request::Status`, `Response::Status`.
- Produces: `Response::Status` gains `root: PathBuf`, the owner's canonical startup directory.

- [ ] **Step 1: write the failing tests**

These join the module Task 20 created in `crates/mcpls-cli/src/hook.rs`, which already carries `#[allow(clippy::unwrap_used, clippy::expect_used)]` and `RecordingOwner`. They are `#[tokio::test]`, because the doctor probes the socket.

One helper, which does not exist yet:

```rust
    /// Run the doctor for `project`, against an owner that reports `root`
    /// as its own startup directory, or against no owner at all.
    ///
    /// The owner is a real listener answering a real `Status`, so what this
    /// exercises is the same probe the installed binary runs.
    async fn doctor_with(project: &Path, owner_root: Option<&Path>) -> String {
        let identity = mcpls_core::hooks::identity_for(project).expect("identity");
        let _owner = match owner_root {
            Some(root) => Some(StatusOwner::start(&identity, root).await),
            None => None,
        };
        super::doctor(project, &identity).await
    }

    /// A listener that answers `Status` and nothing else, reporting `root`
    /// as the directory it started in.
    struct StatusOwner { /* the listener task and its cancel sender */ }

    impl StatusOwner {
        async fn start(
            identity: &mcpls_core::hooks::SocketIdentity,
            root: &Path,
        ) -> Self;
    }
```

`doctor(project_dir: &Path, identity: &SocketIdentity) -> String` is the function Step 3 writes, taking its inputs rather than reading the environment, for the same reason `dispatch_payload` does.

```rust
#[tokio::test]
async fn test_doctor_prints_both_hashes_so_a_mismatch_is_visible() {
    let project = tempfile::tempdir().expect("a temp dir");
    let elsewhere = tempfile::tempdir().expect("a temp dir");

    let out = doctor_with(project.path(), Some(elsewhere.path())).await;

    assert!(out.contains("hook sees"));
    assert!(out.contains("server sees"));
    assert!(
        out.contains("do not match"),
        "a config whose roots point at a subdirectory, a multi-root config, \
         or a symlinked checkout otherwise produces a permanent silent \
         no-op with nothing to look at"
    );
}

#[tokio::test]
async fn test_doctor_says_nothing_is_wrong_when_the_hashes_agree() {
    let project = tempfile::tempdir().expect("a temp dir");

    let out = doctor_with(project.path(), Some(project.path())).await;

    assert!(
        !out.contains("do not match"),
        "the mismatch line is the one thing a reader acts on, so it must not \
         appear when there is nothing to act on"
    );
}

#[tokio::test]
async fn test_doctor_reports_no_owner_when_nothing_is_bound() {
    let project = tempfile::tempdir().expect("a temp dir");
    let out = doctor_with(project.path(), None).await;
    assert!(out.contains("server sees: no owner"));
}

#[tokio::test]
async fn test_doctor_reports_whether_mcpls_is_on_path() {
    let project = tempfile::tempdir().expect("a temp dir");
    let out = doctor_with(project.path(), None).await;

    let line = out
        .lines()
        .find(|line| line.starts_with("mcpls on PATH: "))
        .expect(
            "hooks invoke mcpls from PATH, and a hook environment missing \
             the install directory makes every hook do nothing, invisibly, \
             so the doctor must carry one line that answers it",
        );
    assert!(
        line.ends_with("not found") || line.contains(std::path::MAIN_SEPARATOR),
        "the line has to carry the result of the lookup, an absolute path or \
         a plain 'not found', rather than merely mentioning PATH: {line}"
    );
}
```

The last test asserts on a prefixed line rather than on `out.contains("PATH")`, which any incidental mention, including an error string, would satisfy.

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-cli hook::tests::test_doctor`
Expected: FAIL to compile.

- [ ] **Step 3: give `Response::Status` the owner's directory**

In `crates/mcpls-core/src/hooks/protocol.rs`, the `Status` variant gains a field:

```rust
    /// What the owner reports about itself, for `mcpls hook doctor`.
    Status {
        /// The owner's directory hash.
        hash: String,
        /// The socket it bound.
        socket: PathBuf,
        /// The owner's process id.
        pid: u32,
        /// Always true from an owner; the field exists so a future
        /// forwarding proxy can answer false.
        owner: bool,
        /// The owner's canonical startup directory, so the doctor can print
        /// what the server sees beside what the hook sees.
        root: PathBuf,
    },
```

Update Task 15's `Status` round-trip test to construct the new field, and Task 19's `Status` arm in `crates/mcpls-core/src/hooks/service.rs` to fill it from the directory `serve_with` canonicalized at startup. That arm is the only constructor of `Response::Status` in the crate.

- [ ] **Step 4: implement the doctor**

Print, one per line, in order:

1. `socket: <path>`.
2. `hook sees: <project dir> -> <hash>`.
3. `server sees: <the owner's root, from Status> -> <hash>`, or exactly `server sees: no owner` when nothing answers inside the 50 ms connect timeout.
4. When both are known and the hashes differ, `the two do not match; hooks will do nothing until they do`.
5. `owner pid: <pid>`, or `owner pid: none`.
6. `mcpls on PATH: <resolved absolute path>`, or `mcpls on PATH: not found`. Resolve it by walking `PATH` entries for an executable named `mcpls`, with `.exe` on Windows, rather than shelling out to `which`, which is not on every host.

The doctor is the one command in this feature that is allowed to print on failure, because it exists to break the silence everything else keeps.

- [ ] **Step 5: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-cli && cargo nextest run -p mcpls-core hooks`
Expected: PASS. The second run is what catches Task 15's round-trip test and Task 19's handler if either was missed.

- [ ] **Step 6: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 7: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-cli/src/ crates/mcpls-core/src/hooks/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(cli): answer whether the hooks can work

Every failure in this feature is silent by design, so a broken install
looks exactly like a quiet workspace. The doctor prints the socket, the
two directory hashes, whether an owner answers, and whether mcpls
resolves on PATH.

Response::Status carries the owner's startup directory so the two
hashes can be shown side by side.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

---


## Task 22: the plugin, and the manual gate

**Files:**
- Create: `plugin/.claude-plugin/plugin.json`
- Create: `plugin/.mcp.json`
- Create: `plugin/hooks/hooks.json`
- Create: `plugin/README.md`
- Move: the whole `skills/mcpls/` directory to `plugin/skills/mcpls/`, which is `SKILL.md` and `references/configuration.md`
- Modify: `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md` (the status line at `:3` only)

**Interfaces:**
- Consumes: `mcpls hook` from Task 20 and `mcpls hook doctor` from Task 21.

- [ ] **Step 1: write the plugin manifest**

`plugin/.claude-plugin/plugin.json`:

```json
{
  "name": "mcpls",
  "description": "Language server intelligence and push diagnostics through mcpls",
  "version": "0.1.0"
}
```

`plugin/.mcp.json`:

```json
{
  "mcpServers": {
    "mcpls": {
      "command": "mcpls",
      "args": []
    }
  }
}
```

`plugin/hooks/hooks.json`:

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "mcpls hook" }] }],
    "FileChanged": [{ "hooks": [{ "type": "command", "command": "mcpls hook" }] }],
    "PostToolBatch": [{ "hooks": [{ "type": "command", "command": "mcpls hook" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "mcpls hook" }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "command": "mcpls hook" }] }]
  }
}
```

Check these key names against the host version you are installing into before committing them; the spec's verification section has the recipe for reading them out of the binary.

- [ ] **Step 2: move the skill, all of it**

`skills/mcpls/` holds `SKILL.md` and `references/configuration.md`. Moving only `SKILL.md` would leave the skill pointing at a reference file that stayed behind, so the whole directory moves. The spec says "moved from the repository's top-level `skills/`", which reads the same way.

```bash
mkdir -p /home/lev/Git/lev/mcpls-diag-bc/plugin/skills
git -C /home/lev/Git/lev/mcpls-diag-bc mv skills/mcpls plugin/skills/mcpls
```

Then update any path reference to the old location: `rg -n 'skills/mcpls' /home/lev/Git/lev/mcpls-diag-bc` finds them. Check `README.md` and the installation docs in particular.

- [ ] **Step 3: write the README, leading with doctor**

Open with the install command, then, before anything else, `mcpls hook doctor` and what each of its lines means. Every failure in this system is silent by design, so the first thing a reader needs is the command that breaks that silence. Then the `[diagnostics.hooks]` table and what `enabled = false` does.

- [ ] **Step 4: run the whole suite**

Run: `cargo nextest run --workspace && cargo nextest run -p mcpls-core --test ra_e2e -- --ignored ra_e2e_suite`
Expected: PASS.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

Stage the spec by name rather than staging `docs/`, which would sweep in every unrelated edit under it. `skills/` is staged so the deletions the move produced are recorded.

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add \
  plugin/ \
  skills/ \
  docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md
git -C /home/lev/Git/lev/mcpls-diag-bc status --short
# read it: nothing outside plugin/, skills/ and that one spec file may be staged
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
feat(plugin): ship mcpls as a claude code plugin

The plugin registers mcpls as the MCP server and wires five hook events
to `mcpls hook`, which is what makes delivery push rather than pull and
covers writers outside the agent entirely.

The mcpls skill moves under the plugin with its references directory,
so the two ship and install together.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
```

- [ ] **Step 7: the manual gate**

No test in this repository can prove the wiring. The socket, the protocol and the filters are covered; whether Claude Code actually invokes `mcpls hook`, whether `mcpls` is on the hook process's `PATH`, and whether the two directory hashes agree on this machine are properties of a live session.

Install the plugin into a real Claude Code session, then run:

```fish
mcpls hook doctor
```

Confirm four things: the two hashes match, an owner answers, its pid is a live mcpls, and `mcpls` resolves on `PATH`. Then edit a file with `Edit` in that session and confirm new diagnostics arrive in the next turn.

Report the result. Stage C is not done until this has been run and has passed; do not mark it complete on a green test suite alone.

---

## Unresolved questions

1. **Does the range in the hash re-report a file when an edit shifts an unrelated diagnostic's line?** Stage A shipped hashing `(range, severity, message)`, which re-reports whenever a line moves. Stage C's push traffic is what will make the answer obvious. If it is noisy, the alternative is `(severity, code, message, line text)`, which conflates two identical messages on different lines. Not worth changing before it is measured.
2. Answered. `footer_wait_ms = 15000` stands: `cargo check --workspace --all-targets` after touching a file in `mcpls-core` takes about 4.3 seconds on this machine. Task 12 writes the spec's number unchanged. Re-run the note's "Cargo check timing" recipe when the codebase grows, since the number drifts with it.
3. Answered. Pyrefly publishes nothing for a file it never opened, so watching buys it analysis currency for its open documents rather than delivery for unopened ones. Task 9 proves B2 by reversing the dependency, opening the caller and rewriting the definition, and narrows the spec's B2 entry to match. gopls, which does publish for unopened workspace files, is not installed on this machine, which is why the e2e is not written against it.
4. **Do pyrefly, gopls or ty send `RelativePattern` watchers despite the unclaimed capability?** The skipped-watcher log line is the only signal. Task 9's e2e fails outright if pyrefly does, and its failure message says where to look. Check the logs after the first real session with gopls or ty configured.
5. **What does `watchPaths` cost on a repository mid-build?** The host spawns a hook process per file event and the `watchPaths` list is the only bound. Measure it during a `cargo build`, not at rest.
6. **Should `Stop` ever flush?** Ruled against here, because its `additionalContext` continues the conversation and would make warnings an auto-continue loop. Revisit only if the host's documented semantics change.
