# Diagnostics injection, stages B and C, implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make new diagnostics reach the agent without the agent asking, first on the tools that write (stage B), then on every writer in the project through a socket and a Claude Code plugin (stage C).

**Architecture:** Stage B stops an apply from telling language servers to forget the files it wrote and resyncs them instead, implements the `workspace/didChangeWatchedFiles` client half so servers that rely on the client for file watching learn about those writes, and adds an opt-in footer on the three write tools. Stage C adds a per-project Unix socket or Windows named pipe that a `mcpls hook` subcommand talks to, so Claude Code's own file watcher and tool hooks can push changed paths into mcpls and pull new diagnostics back out.

**Tech Stack:** Rust edition 2024, MSRV 1.88, tokio (`features = ["full"]`), `lsp-types` 0.97, `rmcp`, `globset`, `ignore`, `dunce`, `clap`. Tests run under `cargo nextest run`.

**Spec:** `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`

## Global Constraints

- Rust edition 2024, MSRV 1.88. Clippy runs with pedantic and nursery; `unwrap_used` and `expect_used` warn; `missing_docs` warns. Every task must leave `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --check` must be clean at every commit.
- No lock is held across an `.await`. `std::sync::Mutex` guards are acquired, used, and dropped inside one synchronous section.
- Commits follow Conventional Commits: `type(scope): description`, imperative, 50 characters or fewer including the prefix, no trailing period, lowercase after the colon, body wrapped at 72 columns, ending with the trailer `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`. Commits are GPG signed; if signing fails, stop and report rather than passing `--no-gpg-sign`.
- Stage all changes selectively. Never sweep unrelated edits or generated files into a commit.
- Every test that builds a filesystem path must build it with a drive letter on Windows, the way `crates/mcpls-core/src/mcp/server.rs:1736` already does. `Url::from_file_path` fails without one.
- Configuration values from the spec, verbatim: `footer = false`, `footer_grace_ms = 250`, `footer_quiet_ms = 200`, `footer_wait_ms = 15000`, `[diagnostics.hooks] enabled = true`, `sweep_quiet_ms = 500`, `op_deadline_ms = 1500`, hook connect timeout 50 ms, passive lock retry 5 s.
- `DiagnosticsConfig` is `Copy` with `deny_unknown_fields`. Any nested struct added to it is `Copy` too.
- `relativePatternSupport` is deliberately not claimed. A watcher arriving as a `RelativePattern` is logged at `warn` and skipped.
- Work happens in the worktree at `/home/lev/Git/lev/mcpls-diag-bc` on branch `feat/diagnostics-stage-bc`. Never change the branch checked out in the main checkout at `/home/lev/Git/lev/mcpls`. Always pass absolute paths to `git -C`.

---

## File structure

**Stage B, created:**

- `crates/mcpls-core/src/lsp/watched_files.rs`: the `WatchRegistry`: which servers registered which globs under which registration ids, and which of them match a given path and change kind. Pure; no I/O, no LSP client.
- `crates/mcpls-core/tests/fixtures/python_workspace/`: a pyrefly fixture, created only if Task 1's measurement says pyrefly publishes for unopened files.

**Stage B, modified:**

- `crates/mcpls-core/src/bridge/state.rs`: `DocumentState` gains `saved`; `DocumentTracker` gains the resync entry point and the two per-server marking calls.
- `crates/mcpls-core/src/bridge/translator/mod.rs`: `forget_changed_documents` becomes `resync_changed_documents`; the drain drives notifications and marking.
- `crates/mcpls-core/src/bridge/delivery.rs`: the cleared budget and the `Off`-floor arm.
- `crates/mcpls-core/src/bridge/settle.rs`: the footer's own quiet judgment, and an injectable clock.
- `crates/mcpls-core/src/config/mod.rs`: the three footer keys.
- `crates/mcpls-core/src/lsp/client.rs`: the registry reaches `server_request_result`.
- `crates/mcpls-core/src/lsp/lifecycle.rs`: the capability flip, the tripwire test, `ServerInitConfig` carries the registry.
- `crates/mcpls-core/src/mcp/server.rs`: the lock-order fix, the payload signature, the footer wrapper and its three call sites.
- `crates/mcpls-core/tests/ra_e2e.rs`, `crates/mcpls-core/tests/fixtures/rust_workspace/src/`: the collision fixture and the B1 e2e.

**Stage C, created:**

- `crates/mcpls-core/src/hooks/mod.rs`: module root and the public surface the rest of the crate uses.
- `crates/mcpls-core/src/hooks/identity.rs`: canonicalization, the directory hash, the platform socket path, the lock path.
- `crates/mcpls-core/src/hooks/protocol.rs`: the newline-delimited JSON request and response types.
- `crates/mcpls-core/src/hooks/listener.rs`: the transport trait, the Unix and Windows implementations, and lock-based ownership.
- `crates/mcpls-core/src/hooks/filters.rs`: the two path filters and the `watchPaths` walk.
- `crates/mcpls-core/src/hooks/sweep.rs`: the debounced pending set and the sweep.
- `crates/mcpls-cli/src/hook.rs`: the `mcpls hook` and `mcpls hook doctor` subcommands.
- `crates/mcpls-core/tests/hooks_socket.rs`: socket integration tests.
- `plugin/`: the Claude Code plugin.

**Stage C, modified:**

- `crates/mcpls-core/src/config/mod.rs`: `[diagnostics.hooks]`.
- `crates/mcpls-core/src/lib.rs`: build the registry and the hook listener; restart nothing else.
- `crates/mcpls-cli/src/args.rs`, `crates/mcpls-cli/src/main.rs`: the subcommand.

---

# Stage B

## Task 1: measure what stage B's design rests on

Three claims in the spec are load-bearing for later tasks and none of them is verified. This task answers them and commits the answers. Tasks 5 and 8 read the result.

**Files:**
- Create: `docs/superpowers/notes/2026-09-07-stage-b-measurements.md`
- Create (scratch, not committed): a probe script under `/tmp/claude-*/scratchpad`

**Interfaces:**
- Produces: `docs/superpowers/notes/2026-09-07-stage-b-measurements.md`, containing three headed sections: "flycheck publish versions", "pyrefly on an unopened file", and "cargo check timing". Task 5 reads the pyrefly section to decide whether it writes a pyrefly e2e or a gopls one. Task 8 reads the timing section to confirm or adjust `footer_wait_ms`.

- [ ] **Step 1: write a minimal LSP probe client**

A standalone Rust binary or a Python script, in the scratchpad, that spawns a language server over stdio, performs `initialize` and `initialized`, and logs every inbound message with its method and full params. It must be able to send `textDocument/didOpen`, `textDocument/didChange`, `textDocument/didSave`, and `workspace/didChangeWatchedFiles`. Advertise the same client capabilities `build_client_capabilities` produces (read it at `crates/mcpls-core/src/lsp/lifecycle.rs:730`), plus `workspace.didChangeWatchedFiles.dynamicRegistration = true`, since the pyrefly probe needs pyrefly to register watchers.

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
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "docs(diagnostics): measure what stage b assumes"
```

---

## Task 2: track a saved version per server

**Files:**
- Modify: `crates/mcpls-core/src/bridge/state.rs` (`DocumentState` around `:124-250`)
- Test: `crates/mcpls-core/src/bridge/state.rs`, the existing `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `DocumentState::saved_version(&self, server: &ServerId) -> Option<i32>`
  - `DocumentState::mark_saved(&mut self, server: ServerId, version: i32)` (private to the module, like `mark_synced`)
  - `DocumentState::servers_needing_change(&self, version: i32) -> Vec<ServerId>`: every server in `synced` whose recorded version is below `version`
  - `DocumentState::servers_needing_save(&self, version: i32) -> Vec<ServerId>`: every server in `synced` whose `saved` entry is absent or below `version`
  - `DocumentState::forget_server` also clears that server's `saved` entry.

- [ ] **Step 1: write the failing tests**

Add to `crates/mcpls-core/src/bridge/state.rs`'s test module:

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

- [ ] **Step 3: implement**

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

Both list-returning methods iterate a `HashMap`, so sort before returning to keep the resync's notification order reproducible: append `.sorted()` is not available, so collect into a `Vec` and call `sort_unstable()` before returning in each.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core bridge::state`
Expected: PASS, including every pre-existing `state` test.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/bridge/state.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(bridge): track a saved version per server"
```

---

## Task 3: give the tracker a resync entry point

**Files:**
- Modify: `crates/mcpls-core/src/bridge/state.rs` (`DocumentTracker`, near `ensure_open` at `:520`)
- Test: `crates/mcpls-core/src/bridge/state.rs` test module

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
    /// The caller must hold this path's lock from `lock_path`.
    pub async fn resync_from_disk(&self, path: &Path) -> Result<Option<Resync>>;

    /// Record that `server` received the `didChange` for `version`.
    pub fn mark_change_sent(&self, path: &Path, server: &ServerId, version: i32, generation: u64);

    /// Record that `server` received the `didSave` for `version`.
    pub fn mark_save_sent(&self, path: &Path, server: &ServerId, version: i32, generation: u64);

    /// Sync generation for `server`, to be captured before notifying and
    /// passed back to the two marking calls.
    pub fn generation_for(&self, server: &ServerId) -> u64;
}
```

- [ ] **Step 1: write the failing tests**

```rust
#[tokio::test]
async fn test_a_resync_reports_both_lists_for_a_changed_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn a() {}").expect("write");

    let tracker = DocumentTracker::new(ResourceLimits::default(), extension_map());
    let client = fake_client();
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
    let client = fake_client();
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
    let client = fake_client();
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
    let client = fake_client();
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

Reuse whatever helpers the existing `state` tests use for `fake_client()` and `extension_map()`; if the module names them differently, use its names rather than adding duplicates. If the tracker has no `snapshot(&Path) -> Option<DocumentState>` accessor, add one returning a clone, documented as a test and diagnostic accessor.

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
    pub async fn resync_from_disk(&self, path: &Path) -> Result<Option<Resync>> {
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

`mark_synced` and `mark_saved` are private to the module, which is where these live, so no visibility change is needed. `self.generation(server)` takes the `generations` lock while `documents` is held; check the existing lock order in `sync_phase` at `:853` and match it exactly. If `generation` takes `generations` and `documents` is already held there too, the order is consistent and nothing changes; if not, read the generation before taking `documents` and re-read it under `documents` the way `sync_phase` does.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core bridge::state`
Expected: PASS.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/bridge/state.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(bridge): add a disk resync to the tracker"
```

---

## Task 4: resync after an apply instead of forgetting

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/mod.rs:294-383`
- Test: `crates/mcpls-core/src/bridge/translator/mod.rs` test module, or the existing translator test module if one is elsewhere in that directory

**Interfaces:**
- Consumes: `DocumentTracker::resync_from_disk -> Result<Option<Resync>>`, `Resync { uri, version, text, needs_change, needs_save }`, `Resync::is_settled`, `DocumentTracker::mark_change_sent`, `mark_save_sent`, `generation_for` from Task 3.
- Produces: `Translator::resync_changed_documents(&self)`, replacing `forget_changed_documents`. `close_one_document` survives unchanged and is called only for paths absent from disk.

- [ ] **Step 1: write the failing tests**

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

`TranslatorHarness` does not exist. Build it in `crates/mcpls-core/src/bridge/translator/testing.rs`, which already holds this directory's test helpers, using the fake-LSP-server pipe helpers `crates/mcpls-core/src/lib.rs:1129` documents. It needs: a temp dir, a `Translator` with one registered fake server, `write_file`, `rewrite_file`, `open`, `queue_invalidation` (calls `self.translator.pending_invalidations.extend(&[path])`, so the field needs `pub(crate)` visibility or a `pub(crate) fn queue_invalidations(&self, paths: &[PathBuf])` accessor on `Translator`), `notifications_for` returning the method names the fake server received in order, `clear_notifications`, `fail_notifications_after(server, n)` making the fake transport reject sends after `n`, and `allow_notifications`.

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
    async fn resync_changed_documents(&self) {
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

        let resync = match self.document_tracker.resync_from_disk(path).await {
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

Rename the existing `close_one_document` to `close_one_document_locked` and remove its own `lock_path` acquisition, since `resync_one_document` now holds that lock. Update its doc comment to say the caller holds the lock.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core`
Expected: PASS. Existing apply tests that assert `didClose` on a rewritten file will fail; those assertions were pinning the behaviour this task deliberately replaces, so update them to assert `didChange` then `didSave` and say why in the assertion message.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/bridge/translator/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(bridge): resync applied files instead of closing"
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

- [ ] **Step 1: arrange the fixture for a collision**

`add` currently lives only in `src/lib.rs` and nothing outside that file calls it, so the existing rename rewrites one file and, succeeding, produces no error. Both need fixing.

In `crates/mcpls-core/tests/fixtures/rust_workspace/src/lib.rs`, beside `add` at `:51`, add a same-signature sibling:

```rust
/// A same-signature sibling of `add`, so renaming `add` to `sum` collides
/// and rustc reports E0428. Same signature on purpose: a rename that
/// changed a call site's arity would produce rust-analyzer's own resident
/// diagnostics, which arrive on a didChange alone and would let the e2e
/// pass with the didSave half of the resync entirely broken.
pub fn sum(a: i32, b: i32) -> i32 {
    a + b
}
```

In `crates/mcpls-core/tests/fixtures/rust_workspace/src/functions.rs`, add a caller so the apply touches a second file:

```rust
/// Calls `add` from another module, so a rename of it rewrites this file
/// too and the resync has more than one document to catch up.
pub fn add_twice(a: i32, b: i32) -> i32 {
    crate::add(a, b) + crate::add(a, b)
}
```

- [ ] **Step 2: write the failing e2e sub-case**

In `crates/mcpls-core/tests/ra_e2e.rs`, following the file's existing sub-case shape:

```rust
/// Renaming `add` to `sum` collides with the existing `sum`, so rustc
/// reports E0428 for `src/lib.rs`. rust-analyzer does not check a rename
/// for conflicts, so the apply lands and the error appears only once a
/// build runs, which happens only if the resync sent a didSave.
async fn sc_resync_delivers_a_build_error_after_an_apply(client: &mut McpClient) {
    let lib = fixture_path("src/lib.rs");
    let rename = client
        .call_tool(
            "rename_symbol",
            json!({
                "file_path": lib.display().to_string(),
                "line": 51,
                "character": 8,
                "new_name": "sum",
                "apply": true
            }),
        )
        .await
        .expect("the rename tool answers");
    let rename: serde_json::Value = serde_json::from_str(&rename).expect("json");
    assert_eq!(rename["applied"], json!(true), "the rename must have written");
    assert!(
        rename["files_written"].as_array().expect("files_written").len() >= 2,
        "the fixture's caller in functions.rs must have been rewritten too"
    );

    let deadline = Instant::now() + Duration::from_millis(settle_deadline_ms());
    let mut found = None;
    while Instant::now() < deadline {
        let raw = client
            .call_tool("get_new_diagnostics", json!({}))
            .await
            .expect("the flush tool answers");
        let report: serde_json::Value = serde_json::from_str(&raw).expect("json");
        if report["note"].is_null() {
            if let Some(hit) = find_rustc_e0428(&report) {
                found = Some(hit);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let hit = found.expect(
        "no rustc E0428 arrived within the deadline. rust-analyzer publishes \
         its own resident diagnostics on a didChange alone, so this assertion \
         failing while the tool works at all means the resync's didSave never \
         reached the server",
    );
    assert_eq!(hit["source"], json!("rustc"));
    assert_eq!(hit["code"], json!("E0428"));
}

/// The first `rustc`-sourced `E0428` anywhere in a flush report, or `None`.
fn find_rustc_e0428(report: &serde_json::Value) -> Option<serde_json::Value> {
    report["changed"]
        .as_array()?
        .iter()
        .flat_map(|file| file["diagnostics"].as_array().into_iter().flatten())
        .find(|d| d["source"] == json!("rustc") && d["code"] == json!("E0428"))
        .cloned()
}
```

Match the file's own helper names: it already has a fixture-path helper and a `settle_deadline_ms()`. Use those rather than adding parallel ones. Register the new sub-case in the suite function the way the file's other sub-cases are registered, and restore the fixture at the end of the sub-case the way the existing apply sub-cases restore theirs, so the suite stays re-runnable.

- [ ] **Step 3: run the e2e and watch it fail against the pre-task-4 behaviour**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc stash list   # confirm nothing is stashed; this repo forbids stashing
cargo nextest run -p mcpls-core --test ra_e2e -- --ignored ra_e2e_suite
```

To confirm it fails for the right reason, check out the task-3 commit into a second scratch worktree and run the same sub-case there, rather than reverting in place:

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc worktree add /tmp/mcpls-pre-b1 HEAD~1
```

Expected there: FAIL with the E0428 message above, because forget-on-apply closes the documents and no build runs. Remove that worktree afterwards with `git -C /home/lev/Git/lev/mcpls-diag-bc worktree remove /tmp/mcpls-pre-b1`.

- [ ] **Step 4: run it on the current tree and watch it pass**

Run: `cargo nextest run -p mcpls-core --test ra_e2e -- --ignored ra_e2e_suite`
Expected: PASS, including every pre-existing sub-case. Sub-cases that assert on the fixture's symbol set will see the new `sum` and `add_twice`; update their expected sets and say in the assertion message that the fixture carries a deliberate rename collision.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/tests/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "test(e2e): prove a build error survives an apply"
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

use globset::{Glob, GlobMatcher};

use crate::config::ServerId;
use crate::utils::lock_std;

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
            let glob = match Glob::new(pattern) {
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

`Glob::new` does not enable `literal_separator`. Build each glob through `globset::GlobBuilder::new(pattern).literal_separator(true).build()` instead, so `*` stays inside one path segment the way LSP's grammar requires. Adjust the `Glob::new` call above accordingly.

If `ServerId` does not implement `Ord`, derive it there rather than sorting by string; if that derive is not available, sort by `to_string()`.

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
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(lsp): add a watched files registry"
```

---

## Task 7: advertise dynamic file watching and wire the registry to the client

The advertisement and the notification must land together, so this task and Task 8 are the two halves of one behavioural change. This one carries the advertisement and the write side; Task 8 carries the send side. Do not release between them.

**Files:**
- Modify: `crates/mcpls-core/src/lsp/client.rs` (`server_request_result` at `:762`, `server_request_response`, `spawn_server_request_responder` at `:690-709`, the message loop at `:650`, `LspClient`'s fields at `:78-120`)
- Modify: `crates/mcpls-core/src/lsp/lifecycle.rs` (`ServerInitConfig` at `:116`, `build_client_capabilities` at `:730`, `LspServer::spawn_batch` at `:628`, the tripwire test at `:955`)

**Interfaces:**
- Consumes: `WatchRegistry::{new, register, unregister}` from Task 6.
- Produces:
  - `ServerInitConfig` gains `pub watch_registry: Option<Arc<WatchRegistry>>`, defaulting to `None` for embedders that do not want it.
  - `build_client_capabilities` advertises `workspace.didChangeWatchedFiles = { dynamicRegistration: Some(true), relativePatternSupport: None }`.

- [ ] **Step 1: write the failing tests**

Replace the tripwire test in `crates/mcpls-core/src/lsp/lifecycle.rs:955`. It currently asserts the capability is absent, which is the behaviour this task inverts, so it is rewritten rather than deleted; the reasoning it carries stays, pointing the other way.

```rust
/// Advertising this is what moves gopls and tsgo out of their do-nothing
/// branches: gopls returns early from `registerWatchedDirectoriesLocked`
/// without it, and tsgo falls through to a watcher its own comment limits
/// to Windows and FSEvents. Both abandon their previous behaviour on the
/// strength of the advertisement alone, so mcpls must actually send the
/// notification. `relativePatternSupport` stays unclaimed: every watcher
/// then arrives as a plain glob string, which is one matching path
/// instead of two.
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
    let registry = Arc::new(WatchRegistry::new());
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
        registry.servers_for(Path::new("/work/main.go"), lsp_types::FileChangeType::CHANGED),
        vec![go]
    );
}

#[test]
fn test_an_unregister_drops_it_again() {
    let registry = Arc::new(WatchRegistry::new());
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

    assert!(registry
        .servers_for(Path::new("/work/main.go"), lsp_types::FileChangeType::CHANGED)
        .is_empty());
}

#[test]
fn test_a_registration_for_another_method_is_answered_and_ignored() {
    let registry = Arc::new(WatchRegistry::new());
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
    assert!(registry
        .servers_for(Path::new("/work/main.go"), lsp_types::FileChangeType::CHANGED)
        .is_empty());
}
```

Note that the LSP spec spells the unregister field `unregisterations`, with the extra syllable. That is not a typo in this plan.

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core lsp::client::tests::test_a_watched_files lsp::lifecycle::tests::test_client_capabilities_claim`
Expected: FAIL, the tripwire test's old name no longer exists and `server_request_result` takes two arguments.

- [ ] **Step 3: advertise the capability**

In `build_client_capabilities` (`crates/mcpls-core/src/lsp/lifecycle.rs:730`), inside the `WorkspaceClientCapabilities` literal beside `apply_edit`:

```rust
            did_change_watched_files: Some(lsp_types::DidChangeWatchedFilesClientCapabilities {
                dynamic_registration: Some(true),
                relative_pattern_support: None,
            }),
```

- [ ] **Step 4: thread the registry to the request handler**

Add to `ServerInitConfig`:

```rust
    /// Where this server's `didChangeWatchedFiles` registrations are stored.
    ///
    /// `None` for an embedder that does not want file watching; the two
    /// capability arms then answer `null` and record nothing, which is what
    /// they did before this existed.
    pub watch_registry: Option<Arc<WatchRegistry>>,
```

Every construction site of `ServerInitConfig` needs the new field: `crates/mcpls-core/src/lib.rs` where the configs are built for `spawn_batch`, and the two in `lifecycle.rs`'s test module at `:1122` and `:1139`. Give the tests `None`.

Add to `LspClient` beside `config` (`:80`):

```rust
    /// Shared with the translator, which reads it to decide who to notify.
    watch_registry: Option<Arc<WatchRegistry>>,
```

Set it in `LspClient::new` (`:156`) and `from_transport` (`:177`) to `None`, and add a builder that `spawn_batch` calls:

```rust
    /// Attach the registry this client's `registerCapability` arms write to.
    #[must_use]
    pub fn with_watch_registry(mut self, registry: Option<Arc<WatchRegistry>>) -> Self {
        self.watch_registry = registry;
        self
    }
```

Thread it and `config.id()` through the message loop's parameters (`:556-564`) and `spawn_server_request_responder` (`:690-709`) to `server_request_response`, then to `server_request_result`. Follow how `apply_sink` is already threaded to `forward_apply_edit`; that is the same shape and the same path.

Change the signature and the two arms:

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

- [ ] **Step 5: build the registry and pass it in**

In `crates/mcpls-core/src/lib.rs`, beside `let notification_cache = ...` at `:665`:

```rust
    let watch_registry = Arc::new(lsp::WatchRegistry::new());
```

Set `watch_registry: Some(Arc::clone(&watch_registry))` on every `ServerInitConfig` built for `spawn_batch`, and pass the same `Arc` to the translator's builder, which Task 8 adds.

- [ ] **Step 6: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core`
Expected: PASS.

- [ ] **Step 7: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 8: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/lsp/ crates/mcpls-core/src/lib.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(lsp): claim dynamic watched file registration"
```

---

## Task 8: tell watching servers what an apply wrote

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/mod.rs` (`resync_one_document`, the struct, `with_*` builders near `:196`)
- Modify: `crates/mcpls-core/src/bridge/translator/respawn.rs:281`

**Interfaces:**
- Consumes: `WatchRegistry::{servers_for, forget_server}` from Task 6; `resync_one_document` from Task 4.
- Produces: `Translator::with_watch_registry(self, registry: Arc<WatchRegistry>) -> Self`.

- [ ] **Step 1: write the failing tests**

```rust
#[tokio::test]
async fn test_a_watching_server_is_told_an_applied_file_changed() {
    let harness = TranslatorHarness::with_one_server("go").await;
    harness.register_watcher("go", "r1", "**/*.go");
    let path = harness.write_file("main.go", "package main");
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
    harness.rewrite_file(&path, "package main\nfunc a() {}");
    harness.queue_invalidation(&path);

    harness.translator.resync_changed_documents().await;

    assert!(!harness
        .notifications_for("go")
        .contains(&"workspace/didChangeWatchedFiles".to_string()));
}
```

Extend `TranslatorHarness` from Task 4 with `register_watcher(server, id, glob)`, which calls `WatchRegistry::register` on the registry the harness handed the translator, and `last_watched_files_params(server)` returning the JSON params of the last `workspace/didChangeWatchedFiles` the fake server received.

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core translator::tests::test_a_watching_server`
Expected: FAIL, `no method named register_watcher` and then no such notification.

- [ ] **Step 3: implement**

Add the field and builder to `Translator`:

```rust
    /// Registrations from `client/registerCapability`, shared with the
    /// clients that write them.
    watch_registry: Option<Arc<WatchRegistry>>,
```

`Translator::new` sets it to `None`. Add:

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
    async fn notify_watched_files(&self, path: &Path, kind: lsp_types::FileChangeType) {
        let Some(registry) = &self.watch_registry else {
            return;
        };
        let Ok(uri) = path_to_uri(path) else {
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

Use whatever the crate's existing path-to-URI helper is called; `crates/mcpls-core/src/bridge/translator/mod.rs` already converts paths for `ensure_open`, so reuse that rather than adding a second conversion.

In `resync_one_document` from Task 4, notify after deciding the case:

```rust
        if !path.exists() {
            self.close_one_document_locked(path).await;
            self.notify_watched_files(path, lsp_types::FileChangeType::DELETED)
                .await;
            return true;
        }
```

and, for the present cases, after the change and save loops and before `true`:

```rust
        self.notify_watched_files(path, lsp_types::FileChangeType::CHANGED)
            .await;
        true
```

An untracked path returns early on `Ok(None)` from `resync_from_disk` in Task 4's code; change that arm to notify before returning:

```rust
            Ok(None) => {
                self.notify_watched_files(path, lsp_types::FileChangeType::CHANGED)
                    .await;
                return true;
            }
```

A file the apply created is reported as `CHANGED` rather than `CREATED`. The queue carries bare paths, so nothing at this point distinguishes the two, and every server that accepts one accepts the other.

In `crates/mcpls-core/src/bridge/translator/respawn.rs`, beside `self.document_tracker.forget_server(id)` at `:281`:

```rust
        self.forget_watch_registrations(id);
```

In `crates/mcpls-core/src/lib.rs`, chain `.with_watch_registry(Arc::clone(&watch_registry))` onto the translator construction beside the other `with_*` builders.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core`
Expected: PASS.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/bridge/translator/ crates/mcpls-core/src/lib.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(bridge): notify watchers of applied writes"
```

---

## Task 9: prove watched files against a real server

Read `docs/superpowers/notes/2026-09-07-stage-b-measurements.md` first. Its "pyrefly on an unopened file" section decides which half of this task you do.

**Files:**
- Create (branch A): `crates/mcpls-core/tests/fixtures/python_workspace/{a.py,b.py,pyproject.toml}`
- Modify (branch A): `crates/mcpls-core/tests/ra_e2e.rs` or a new `crates/mcpls-core/tests/pyrefly_e2e.rs`
- Modify (branch B): `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`, the B2 server table and its pyrefly bullet

**Interfaces:**
- Consumes: Task 7's advertisement and Task 8's notification.
- Produces: nothing later tasks depend on.

- [ ] **Step 1: read the measurement and pick the branch**

If the note says pyrefly published diagnostics for the file it never received a `didOpen` for, do branch A. If it says pyrefly published nothing, do branch B.

- [ ] **Step 2A: build the pyrefly fixture**

`crates/mcpls-core/tests/fixtures/python_workspace/pyproject.toml`:

```toml
[project]
name = "mcpls-fixture"
version = "0.1.0"
requires-python = ">=3.10"
```

`a.py`:

```python
def takes_an_int(value: int) -> int:
    return value + 1
```

`b.py`:

```python
from a import takes_an_int

RESULT = takes_an_int(1)
```

- [ ] **Step 3A: write the failing e2e**

A gated suite in the shape `ra_e2e.rs` already uses: `#[ignore]`, a binary-freshness guard, a config fixture naming pyrefly as the only server, and a settle deadline. The sub-case:

```rust
/// pyrefly runs no filesystem watcher of its own in language-server mode,
/// so an external write reaches it only through the notification this
/// suite exercises. The write goes to a file no tool call has opened,
/// which is what a rename's fanout produces.
async fn sc_watched_files_reaches_pyrefly(client: &mut McpClient, workspace: &Path) {
    let a = workspace.join("a.py");
    client
        .call_tool("get_hover", json!({
            "file_path": a.display().to_string(), "line": 0, "character": 4
        }))
        .await
        .expect("opening a.py through a read tool");

    let b = workspace.join("b.py");
    std::fs::write(&b, "from a import takes_an_int\n\nRESULT = takes_an_int(\"not an int\")\n")
        .expect("breaking b.py on disk");

    let rename = client
        .call_tool("rename_symbol", json!({
            "file_path": a.display().to_string(),
            "line": 0, "character": 4,
            "new_name": "takes_an_integer",
            "apply": true
        }))
        .await
        .expect("the rename tool answers");
    let rename: serde_json::Value = serde_json::from_str(&rename).expect("json");
    assert_eq!(rename["applied"], json!(true));

    let deadline = Instant::now() + Duration::from_millis(settle_deadline_ms());
    let mut saw_b = false;
    while Instant::now() < deadline && !saw_b {
        let raw = client.call_tool("get_new_diagnostics", json!({})).await.expect("flush");
        let report: serde_json::Value = serde_json::from_str(&raw).expect("json");
        if report["note"].is_null() {
            saw_b = report["changed"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|f| f["file_path"].as_str().is_some_and(|p| p.ends_with("b.py")));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    assert!(
        saw_b,
        "b.py was never opened through a tool call, so if this fails the \
         didChangeWatchedFiles notification either did not go out or pyrefly \
         did not act on it; check the measurement note before assuming the \
         former"
    );
}
```

Restore both fixture files at the end of the sub-case so the suite is re-runnable.

- [ ] **Step 4A: run it and watch it fail without Task 8's notification**

Check the task-7 commit out into a scratch worktree and run the suite there:

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc worktree add /tmp/mcpls-pre-b2 <task-7 commit sha>
cargo nextest run -p mcpls-core --test pyrefly_e2e -- --ignored
git -C /home/lev/Git/lev/mcpls-diag-bc worktree remove /tmp/mcpls-pre-b2
```

Expected there: FAIL on the `saw_b` assertion.

- [ ] **Step 5A: run it on the current tree and watch it pass**

Run: `cargo nextest run -p mcpls-core --test pyrefly_e2e -- --ignored`
Expected: PASS.

- [ ] **Step 2B: correct the spec instead**

If pyrefly published nothing, the B2 table's "needs the client to watch" for pyrefly overstates what it buys. Change the pyrefly row and bullet to say that pyrefly registers watchers and keeps its analysis current from them, but does not publish for a document it does not hold, so the notification buys currency rather than delivery. Then say the e2e goes to gopls, which does publish for unopened workspace files, and that it is deferred until a Go fixture and a `gopls` binary are available in this environment. `command -v gopls` returns nothing here today, which is the fact that defers it.

- [ ] **Step 3B: record why no e2e exists**

Add one paragraph to the spec's stage B testing section stating that B2 ships with unit coverage of the registry and no end-to-end proof, naming the measurement that established why, and naming gopls as the beneficiary to prove it with when a Go toolchain is present. A stage that ships unproven should say so in the document, not only in a commit message.

- [ ] **Step 6: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 7: commit**

Branch A:

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/tests/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "test(e2e): drive watched files against pyrefly"
```

Branch B:

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add docs/superpowers/specs/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "docs(spec): narrow what watching buys pyrefly"
```

---

## Task 10: stop the flush cloning the whole cache

**Files:**
- Modify: `crates/mcpls-core/src/mcp/server.rs` (`get_new_diagnostics` at `:736`, `new_diagnostics_payload` at `:770`, `routable_entries` near `:234`)
- Modify: `crates/mcpls-core/src/lib.rs` (`baseline_task` at `:1088`)

**Interfaces:**
- Produces:
  - `new_diagnostics_payload(&self, report: &FlushReport, sources: &HashMap<String, (Uri, ServerId)>) -> NewDiagnosticsResult`
  - `McplsServer::flush_now(&self, session: &SessionId) -> NewDiagnosticsResult`: the flush and its payload, without the tool wrapper and without the baseline guard, so the tool, the footer (Task 13) and the socket op (Task 19) all run the same code against the same record
  - `NotificationCache::diagnostics_entries(&self) -> Vec<(&str, &DiagnosticInfo, &ServerId)>`

- [ ] **Step 1: write the failing test**

```rust
#[tokio::test]
async fn test_a_flush_does_not_hold_the_cache_lock_while_building_its_payload() {
    let context = test_context_with_baseline().await;
    let server = McplsServer::from_context(context.clone());

    // Hold the cache lock from another task the moment the flush is in
    // flight. If the flush holds it across its payload build, this never
    // acquires and the timeout fires.
    let flush = tokio::spawn(async move { server.get_new_diagnostics().await });
    tokio::task::yield_now().await;
    let grabbed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        context.notification_cache.lock(),
    )
    .await;

    assert!(
        grabbed.is_ok(),
        "the diagnostics pump takes this same lock, and the transport drops \
         notifications on a full channel rather than blocking, so a flush \
         that holds it across its awaits loses publishes under hook traffic"
    );
    flush.await.expect("the flush task").expect("the flush");
}
```

If the test module has no `test_context_with_baseline`, build one from the existing `default_delivery_and_floors` helper at `:1190` and the cache construction at `:1204`, seeding a baseline so `has_baseline()` is true.

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

/// The URI and owning server of every key a report names, cloned so the
/// payload can be built after both guards are released.
fn source_map(
    cache: &NotificationCache,
    report: &FlushReport,
) -> HashMap<String, (Uri, ServerId)> { ... }
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

`new_diagnostics_payload` takes `sources` instead of `snapshot` and looks each key up there.

In `crates/mcpls-core/src/lib.rs`, `baseline_task` at `:1088` does the same clone for the same reason. Give it the borrowing accessor too: take the cache lock, build the hash map from borrowed entries, release, then `set_baseline`. It takes `delivery` only after the cache guard is gone, which is the opposite order from the flush; that is safe because it never holds both, and a comment should say so.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core`
Expected: PASS.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/mcp/server.rs crates/mcpls-core/src/bridge/notifications.rs crates/mcpls-core/src/lib.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "perf(mcp): flush without cloning the whole cache"
```

---

## Task 11: bound cleared files and stop reporting muted ones as fixed

**Files:**
- Modify: `crates/mcpls-core/src/bridge/delivery.rs` (`flush` at `:161-225`)
- Test: `crates/mcpls-core/src/bridge/delivery.rs` test module

**Interfaces:**
- Produces: no signature change. `FlushReport::omitted` now also counts cleared files the budget deferred.

- [ ] **Step 1: write the failing tests**

```rust
#[test]
fn test_cleared_files_spend_the_total_budget() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
        max_total: 2,
        ..DiagnosticsConfig::default()
    });
    let session = SessionId::process_default();
    let broken = vec![error_at(0)];
    let entries = |diags: &[lsp_types::Diagnostic]| { /* build four FileEntry over keys a..d */ };

    // Four files, each with one error, delivered under a budget of 2.
    // Then all four are fixed at once.
    let _ = delivery.flush(&session, &all_broken());
    let report = delivery.flush(&session, &all_clean());

    assert_eq!(
        report.cleared.len(),
        2,
        "max_total is one shared context budget and a cleared line spends \
         from it like any other; a workspace-wide fix could otherwise emit \
         up to a thousand of them"
    );
    assert_eq!(report.omitted, 2);
}

#[test]
fn test_a_deferred_cleared_file_is_offered_again() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
        max_total: 2,
        ..DiagnosticsConfig::default()
    });
    let session = SessionId::process_default();
    let _ = delivery.flush(&session, &all_broken());
    let first = delivery.flush(&session, &all_clean());
    let second = delivery.flush(&session, &all_clean());

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
    let session = SessionId::process_default();
    let broken = vec![error_at(0)];

    let _ = delivery.flush(
        &session,
        &[FileEntry { key: "a", diagnostics: &broken, floor: SeverityFloor::Error }],
    );
    let report = delivery.flush(
        &session,
        &[FileEntry { key: "a", diagnostics: &broken, floor: SeverityFloor::Off }],
    );

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
    let session = SessionId::process_default();
    let broken = vec![error_at(0)];

    let _ = delivery.flush(
        &session,
        &[FileEntry { key: "a", diagnostics: &broken, floor: SeverityFloor::Error }],
    );
    let report = delivery.flush(
        &session,
        &[FileEntry { key: "a", diagnostics: &[], floor: SeverityFloor::Error }],
    );

    assert_eq!(report.cleared, vec!["a".to_string()]);
}
```

Write the `all_broken`, `all_clean` and `error_at` helpers concretely; the test module already has diagnostic-building helpers, so use those names rather than adding parallel ones.

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
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "fix(bridge): budget cleared files and keep muted ones"
```

---

## Task 12: the footer's clock and quiet judgment

**Files:**
- Modify: `crates/mcpls-core/src/bridge/settle.rs`
- Modify: `crates/mcpls-core/src/config/mod.rs` (`DiagnosticsConfig` at `:134-207`)

**Interfaces:**
- Produces:
  - `DiagnosticsConfig` gains `footer: bool` (default `false`), `footer_grace_ms: u64` (250), `footer_quiet_ms: u64` (200), `footer_wait_ms: u64` (15000).
  - `ServerSettle::end_at(&self, server, token, now: Instant)`, with the existing `end` kept as a thin wrapper passing `Instant::now()`. `begin` is unchanged apart from bumping the epoch.
  - `ServerSettle::is_quiet_at(&self, now: Instant, quiet_for: Duration) -> bool`.
  - `ServerSettle::progress_epoch(&self) -> u64`, incremented on every `begin`.

- [ ] **Step 1: write the failing tests**

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
    /// touch in this repository's largest crate costs about 4.7 seconds of
    /// `cargo check`, so a five second cap would expire on every rename
    /// there and report the pre-edit state. The wait is gated on progress
    /// rather than on a timer, so a fast workspace still returns in about a
    /// second and the high cap costs it nothing.
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
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(bridge): give the footer its own quiet test"
```

---

## Task 13: the footer

**Files:**
- Modify: `crates/mcpls-core/src/mcp/server.rs` (`rename_symbol` at `:437`, `format_document` at `:514`, `apply_code_action` at `:604`)

**Interfaces:**
- Consumes: Task 12's config keys, `ServerSettle::{is_quiet_at, progress_epoch}`; Task 10's `get_new_diagnostics` internals.
- Produces:

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

- [ ] **Step 1: write the failing tests**

```rust
#[tokio::test]
async fn test_a_footer_is_silent_before_the_baseline_lands() {
    let context = test_context_without_baseline().await;
    let server = McplsServer::from_context(context);

    let footer = server.footer_for_write().await;

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
    let context = test_context_with_baseline_and_one_error().await;
    let server = McplsServer::from_context(context);

    let footer = server.footer_for_write().await.expect("a report");
    assert_eq!(footer.changed.len(), 1);

    let raw = server.get_new_diagnostics().await.expect("the flush tool");
    let report: serde_json::Value = serde_json::from_str(&raw).expect("json");
    assert!(
        report["changed"].as_array().expect("changed").is_empty(),
        "one report per problem: the footer and the flush share one record"
    );
}

#[tokio::test]
async fn test_the_footer_is_absent_when_the_tool_wrote_nothing() {
    let context = test_context_with_baseline_and_one_error().await;
    let server = McplsServer::from_context(context);

    let wrapped = WithDiagnostics {
        result: RenameResult { applied: false, ..sample_rename_result() },
        new_diagnostics: None,
    };
    let json = serde_json::to_string(&wrapped).expect("serialize");

    assert!(
        !json.contains("new_diagnostics"),
        "a rename with apply false changed nothing and has nothing to report"
    );
}

#[test]
fn test_the_wrapper_flattens_rather_than_nesting() {
    let wrapped = WithDiagnostics {
        result: sample_rename_result(),
        new_diagnostics: None,
    };
    let json: serde_json::Value =
        serde_json::to_value(&wrapped).expect("serialize");

    assert!(
        json.get("applied").is_some(),
        "the existing result's fields stay at the top level; a caller \
         parsing RenameResult must keep parsing it"
    );
}

#[tokio::test]
async fn test_the_footer_wait_ends_on_quiet_rather_than_on_its_cap() {
    let settle = Arc::new(ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600)));
    let rust = ServerId::from("rust");
    let start = Instant::now();
    settle.begin(&rust, &json!("flycheck"));
    settle.end_at(&rust, &json!("flycheck"), start + Duration::from_millis(400));

    let ended = wait_for_footer_quiet_at(
        &settle,
        settle.progress_epoch() - 1,
        FooterTiming { grace: Duration::from_millis(250), quiet: Duration::from_millis(200), cap: Duration::from_secs(15) },
        |elapsed| start + elapsed,
    );

    assert!(
        ended < Duration::from_secs(1),
        "quiet arrived at 600ms, well inside the cap; a test that could only \
         ever end on the cap would pass against a broken quiet check"
    );
}
```

`wait_for_footer_quiet_at` is the clock-injected core; the async wrapper around it does the sleeping. Writing it as a pure function of a clock closure is what makes the "ended on quiet" branch assertable, which a `tokio::time::pause()` test could not do: `ServerSettle` stamps `std::time::Instant`, and pausing tokio's clock does not move that one.

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core mcp::server::tests::test_a_footer`
Expected: FAIL, `no method named footer_for_write`.

- [ ] **Step 3: implement**

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
/// quiet arriving, the cap expiring, and the grace period elapsing before
/// either is consulted. `at` maps elapsed time to the `Instant` the settle
/// tracker stamps against.
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
/// Two ways to be done. The workspace is quiet, which is the ordinary one.
/// Or work is outstanding but nothing has begun since the resync, which
/// means that work was already running when the edit landed: an index after
/// a `Cargo.toml` change can run for minutes, and it is not this call's
/// check. Waiting on it would spend the whole cap on something this tool
/// call did not cause.
fn footer_should_stop(
    settle: &ServerSettle,
    epoch_before: u64,
    now: Instant,
    quiet: Duration,
) -> bool {
    settle.is_quiet_at(now, quiet) || settle.progress_epoch() == epoch_before
}
```

```rust
impl McplsServer {
    /// The diagnostics a write tool's own edit produced, or `None`.
    ///
    /// Silent while no baseline exists. `flush` seeds a session's record
    /// from the baseline, and `set_baseline` does not rewrite a record that
    /// already exists, so a footer flushing early would leave that session
    /// permanently believing the workspace started clean.
    async fn footer_for_write(&self) -> Option<NewDiagnosticsResult> {
        if !self.context.config.diagnostics.footer {
            return None;
        }
        if !self.context.delivery.lock().await.has_baseline() {
            return None;
        }
        let timing = FooterTiming::from_config(&self.context.config.diagnostics);
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

        Some(self.flush_now().await)
    }
}
```

At each of the three call sites, wrap the result:

```rust
        let footer = if result.applied { self.footer_for_write().await } else { None };
        to_tool_result(Ok(WithDiagnostics { result, new_diagnostics: footer }))
```

`format_document` and `apply_code_action` have their own applied flags; read each result type rather than assuming the field name is `applied` on all three.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core`
Expected: PASS.

- [ ] **Step 5: check the e2e still passes with the footer off**

Run: `cargo nextest run -p mcpls-core --test ra_e2e -- --ignored ra_e2e_suite`
Expected: PASS. The footer defaults to false, so no sub-case should change.

- [ ] **Step 6: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 7: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/mcp/server.rs
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(mcp): append new diagnostics to write results"
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
/// owns and which is cleaned when the session ends. `$TMPDIR` on macOS,
/// which does not set that variable. A uid-suffixed `/tmp` directory
/// otherwise, so two users on one machine do not collide.
#[cfg(not(windows))]
fn runtime_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        if let Some(tmp) = std::env::var_os("TMPDIR") {
            return PathBuf::from(tmp).join("mcpls");
        }
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("mcpls");
    }
    // Safe on every Unix: getuid cannot fail and touches no shared state.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/mcpls-{uid}"))
}
```

`DefaultHasher` is not stable across Rust releases, which does not matter here: both sides are the same binary in the same process family, and a hash that changes between mcpls versions only means a new socket path after an upgrade. Say that in a comment so a later reader does not reach for a cryptographic hash to fix a problem that does not exist.

Add `libc` to `crates/mcpls-core/Cargo.toml` under a `[target.'cfg(unix)'.dependencies]` section if it is not already there. `dunce` is already a dependency; confirm with `rg dunce crates/mcpls-core/Cargo.toml`.

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
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/hooks/ crates/mcpls-core/src/lib.rs crates/mcpls-core/src/config/mod.rs crates/mcpls-core/Cargo.toml Cargo.toml Cargo.lock
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(hooks): derive the per-project socket path"
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

Write the two enums above with the derives shown, plus doc comments on every variant and field naming which hook sends it. Note in the module doc that the host's event kind is a hint only, and that the sweep derives the real kind from a stat, because a formatter saving through a temporary file and a rename produces `unlink` for a file that exists again by the time the hook connects.

Add `pub mod protocol;` and the re-exports to `crates/mcpls-core/src/hooks/mod.rs`.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core hooks::protocol`
Expected: PASS.

- [ ] **Step 5: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/hooks/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(hooks): define the socket wire protocol"
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
    /// Serve connections until `cancel` fires.
    pub async fn serve<H>(self, handler: H, cancel: watch::Receiver<bool>)
    where H: Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static;
}

/// Send one request to whoever owns `identity`'s socket.
pub async fn send(identity: &SocketIdentity, request: &Request, timeout: Duration) -> Result<Response>;
```

- [ ] **Step 1: add the lock dependency**

In the workspace `Cargo.toml`:

```toml
fs4 = { version = "0.12", features = ["sync"] }
```

and in `crates/mcpls-core/Cargo.toml`, `fs4 = { workspace = true }`.

- [ ] **Step 2: write the failing integration tests**

`crates/mcpls-core/tests/hooks_socket.rs`:

```rust
//! The hook socket, over a temporary runtime directory.
//!
//! These are integration tests rather than unit tests because what they
//! check is ownership between processes-worth of state: two listeners
//! racing, a stale file, a lock outliving a socket.

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
        |_req| Box::pin(async { Response::Flush { context: Some("hello".to_string()) } }),
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

#[tokio::test]
async fn test_an_op_answers_within_its_deadline_while_its_work_runs_on() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity).await.expect("acquire").expect("owner");
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    tokio::spawn(listener.serve(
        |_req| Box::pin(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Response::Flush { context: None }
        }),
        cancel,
    ));

    let started = std::time::Instant::now();
    let response = send(
        &identity,
        &Request::Flush { session: "s1".to_string() },
        Duration::from_millis(1500),
    )
    .await;

    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        response.is_err() || matches!(response, Ok(Response::Flush { context: None })),
        "a hook that hangs blocks the agent, and the host's own timeout is \
         600 seconds, so the bound has to be ours"
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

`temp_identity()` returns a `TempDir` guard and a `SocketIdentity` whose socket and lock sit inside it, so the tests never touch the real runtime directory. On Windows the socket is a pipe name and the guard covers only the lock; skip the stale-file test there with `#[cfg(unix)]` and say why in a comment.

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

`serve` loops on `accept`, spawning a task per connection that reads newline-delimited requests, calls the handler under `tokio::time::timeout(op_deadline)`, and writes one response line per request. A handler that outruns the deadline gets `Response::Error { message }` written for that request while its future keeps running; that is deliberate and the doc comment must say so.

`send` connects with `tokio::time::timeout(timeout, ...)`, writes one line, reads one line, and returns. Every failure is an `Err`; the caller in Task 20 turns every `Err` into exit 0 with no output.

- [ ] **Step 5: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core --test hooks_socket`
Expected: PASS.

- [ ] **Step 6: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 7: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/hooks/ crates/mcpls-core/tests/hooks_socket.rs crates/mcpls-core/Cargo.toml Cargo.toml Cargo.lock
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(hooks): own the socket through a lock"
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
Expected: PASS.

- [ ] **Step 5: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/hooks/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(hooks): filter which changed paths matter"
```

---

## Task 18: the debounced sweep

**Files:**
- Create: `crates/mcpls-core/src/hooks/sweep.rs`
- Modify: `crates/mcpls-core/src/hooks/mod.rs`

**Interfaces:**
- Consumes: `PathFilter::admits` from Task 17; `Translator::resync_changed_documents`, `notify_watched_files` from Tasks 4 and 8; `DocumentTracker::open_paths`.
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
    pub async fn sweep_now(&self);
    /// How many sweeps have run. Test-only.
    #[cfg(test)]
    pub fn sweeps_run(&self) -> usize;
    /// What the last sweep decided each path was. Test-only.
    #[cfg(test)]
    pub fn last_kinds(&self) -> Vec<(PathBuf, SweepKind)>;
    /// How many untracked paths the last sweep opened. Test-only.
    #[cfg(test)]
    pub fn opened_count(&self) -> usize;
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

- [ ] **Step 1: write the failing tests**

```rust
#[tokio::test(start_paused = true)]
async fn test_a_burst_produces_one_sweep() {
    let sweeper = test_sweeper(Duration::from_millis(500));
    for i in 0..50 {
        sweeper.enqueue(&[fixture_path(&format!("f{i}.rs"))]);
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
    sweeper.enqueue(&[fixture_path("a.rs")]);
    tokio::time::advance(Duration::from_millis(400)).await;
    sweeper.enqueue(&[fixture_path("b.rs")]);
    tokio::time::advance(Duration::from_millis(400)).await;

    assert_eq!(sweeper.sweeps_run(), 0, "the burst has not settled");

    tokio::time::advance(Duration::from_millis(200)).await;
    assert_eq!(sweeper.sweeps_run(), 1);
}

#[tokio::test]
async fn test_a_deleted_path_is_swept_as_a_delete_whatever_the_host_said() {
    let sweeper = test_sweeper(Duration::from_millis(10));
    let path = fixture_path("gone.rs");
    std::fs::remove_file(&path).ok();
    sweeper.enqueue(&[path.clone()]);
    sweeper.sweep_now().await;

    assert_eq!(sweeper.last_kinds(), vec![(path, SweepKind::Deleted)]);
}

#[tokio::test]
async fn test_an_atomic_save_is_swept_as_a_change_not_a_delete() {
    let sweeper = test_sweeper(Duration::from_millis(10));
    let path = fixture_path("saved.rs");
    // The host sends unlink then add for a temp-file-and-rename save, and
    // the hook process arrives after both. The file exists again by now.
    sweeper.enqueue(&[path.clone()]);
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
    let paths: Vec<PathBuf> = (0..10).map(|i| fixture_path(&format!("f{i}.rs"))).collect();
    sweeper.enqueue(&paths);
    sweeper.sweep_now().await;

    let shortfall = sweeper.last_shortfall().expect("a shortfall line");
    assert!(shortfall.contains('7'), "seven of ten did not fit");
    assert!(
        sweeper.opened_count() <= 3,
        "filling the tracker would make the next unrelated tool call fail \
         with DocumentLimitExceeded, which is a worse outcome than not \
         checking some files"
    );
}
```

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core hooks::sweep`
Expected: FAIL to compile.

- [ ] **Step 3: implement**

`enqueue` filters and inserts into a `Mutex<HashSet<PathBuf>>`, and records `Instant::now()` as the last arrival. `run` loops on a `tokio::time::interval` of `quiet_for / 4`, sweeping when the set is non-empty and the last arrival is older than `quiet_for`.

A sweep takes the whole set, then for each path:

1. Stat it. Absent means `SweepKind::Deleted`; present and tracked means `SweepKind::Changed` through the resync; present and untracked means `SweepKind::Created` if the tracker has never held it, else `SweepKind::Changed`.
2. Queue the tracked ones onto `translator.pending_invalidations` and call `resync_changed_documents`, which already does the right thing per path and already notifies watchers.
3. For untracked paths routed to a server whose diagnostics come from a build, open and save them, but only up to the remaining headroom: `max_documents.saturating_sub(tracker.open_paths().len())`. Record the rest as a shortfall string, `"{n} file(s) not checked: the document limit of {max} was reached"`, and do not remember them. The next change to any of them brings it back through the same filters.

The shortfall reaches the agent through the flush's output, which Task 19 wires.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core hooks::sweep`
Expected: PASS.

- [ ] **Step 5: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/hooks/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(hooks): sweep changed paths once a burst settles"
```

---

## Task 19: serve the ops from a running mcpls

**Files:**
- Modify: `crates/mcpls-core/src/lib.rs` (`serve_with`)
- Modify: `crates/mcpls-core/src/hooks/mod.rs`
- Modify: `crates/mcpls-core/src/mcp/server.rs` (the flush's session key, and forwarding from a passive instance)

**Interfaces:**
- Consumes: everything from Tasks 14 through 18.
- Produces: a running listener whose handler serves `Changed`, `Flush`, `EndSession` and `Status`; `McplsServer` learns whether it is the owner.

- [ ] **Step 1: write the failing tests**

```rust
#[tokio::test]
async fn test_a_changed_op_queues_and_returns_without_sweeping() {
    let harness = HookHarness::owner().await;
    let response = harness.send(Request::Changed {
        session: "s1".to_string(),
        paths: vec![harness.fixture("a.rs")],
        event: ChangeEvent::Change,
    }).await;

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

#[tokio::test]
async fn test_a_passive_instance_runs_no_footer() {
    let harness = HookHarness::owner_with_one_error().await;
    let passive = harness.passive_instance_with_footer_enabled().await;

    let result = passive.call_rename_with_apply().await;
    assert!(
        !result.contains("new_diagnostics"),
        "the footer would consume from the passive's own record while the \
         next flush reads the owner's, so the same diagnostics arrive twice \
         from one door and never from the other"
    );
}
```

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core --test hooks_socket`
Expected: FAIL to compile.

- [ ] **Step 3: implement**

In `serve_with`, after the translator and the delivery core exist and before the MCP server starts:

```rust
    let identity = hooks::identity_for(&std::env::current_dir()?)?;
    let ownership = if config.diagnostics.hooks.enabled {
        hooks::HookListener::acquire(&identity).await?
    } else {
        None
    };
```

When `Some`, build the `Sweeper`, spawn its `run`, and spawn `listener.serve(handler, cancel_rx.clone())`. The handler closes over the delivery core, the notification cache, the floors and the sweeper:

- `Changed { session: _, paths, event: _ }` calls `sweeper.enqueue(&paths)` and answers the count. The event is discarded; the sweep stats.
- `Flush { session }` runs the same `flush_now` the tool runs, keyed on that session id, renders it, folds in `sweeper.last_shortfall()`, and answers `Response::Flush { context }`. `None` when nothing changed, so the hook prints nothing.
- `EndSession { session }` drops that session's record from the delivery core. Add `DiagnosticsDelivery::end_session(&mut self, session: &SessionId)` for this.
- `Status` answers the identity, this process's pid, and `owner: true`.

When `None`, this process is passive. Record that on the MCP server's context as `hook_owner: bool`, and:

- The flush tool forwards to the owner with `hooks::send` and returns what comes back, falling back to its own record only if the send fails.
- `footer_for_write` returns `None` immediately, before the config check.
- `resync_changed_documents` additionally sends a `Changed` naming its apply targets to the owner, so the owner's servers learn what this process wrote.

The session key stops being `SessionId::process_default()` unconditionally: read `CLAUDE_CODE_SESSION_ID` from the environment at startup and use it when present, since the hook payload carries the same value and the two doors must share one record. `SessionId` already has the shape for this; add `SessionId::from_env_or_process()`.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core`
Expected: PASS.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(hooks): serve the socket ops from a live server"
```

---

## Task 20: the `mcpls hook` subcommand

**Files:**
- Create: `crates/mcpls-cli/src/hook.rs`
- Modify: `crates/mcpls-cli/src/args.rs` (the `Command` enum at `:108`)
- Modify: `crates/mcpls-cli/src/main.rs`

**Interfaces:**
- Consumes: `hooks::{identity_for, send}`, `Request`, `Response` from Tasks 14 through 16; `hooks::watch_paths` from Task 17.
- Produces: `mcpls hook`, reading one hook payload from stdin and writing hook JSON to stdout.

- [ ] **Step 1: write the failing tests**

```rust
#[test]
fn test_session_start_returns_watch_paths_without_a_socket() {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");

    let out = dispatch(
        &json!({ "hook_event_name": "SessionStart" }),
        dir.path(),
    );

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

#[test]
fn test_an_unreachable_socket_produces_no_output_and_exit_zero() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dispatch(
        &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
        dir.path(),
    );
    assert_eq!(out, "", "an edit must never fail because diagnostics were unavailable");
}

#[test]
fn test_an_unknown_event_produces_no_output() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dispatch(&json!({ "hook_event_name": "Whatever" }), dir.path());
    assert_eq!(out, "");
}

#[test]
fn test_a_malformed_payload_produces_no_output() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dispatch_raw("not json at all", dir.path());
    assert_eq!(out, "");
}

#[test]
fn test_post_tool_batch_sends_changed_then_flush() {
    let recorder = RecordingOwner::start();
    let out = dispatch_against(
        &json!({
            "hook_event_name": "PostToolBatch",
            "session_id": "s1",
            "tool_calls": [{ "tool_input": { "file_path": "/work/a.rs" } }]
        }),
        &recorder,
    );

    assert_eq!(
        recorder.ops(),
        vec!["changed", "flush"],
        "changed queues for the sweep and flush drains the record; the pair \
         is what makes a batch's own paths reach the servers"
    );
    assert!(out.contains("additionalContext"));
}
```

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-cli hook`
Expected: FAIL to compile.

- [ ] **Step 3: implement**

Add to `Command`:

```rust
    /// Serve one Claude Code hook invocation
    ///
    /// Reads the hook payload from stdin and writes hook JSON to stdout,
    /// dispatching on the payload's own `hook_event_name`. One subcommand
    /// rather than five means no shell script and the same registrations
    /// work on Windows.
    Hook {
        /// Print the socket path, both directory hashes, and whether an
        /// owner is live
        #[arg(long)]
        doctor: bool,
    },
```

`mcpls hook doctor` is Task 21. This task implements the plain form.

Dispatch on `hook_event_name`:

| Event | Action | Output |
|---|---|---|
| `SessionStart` | `watch_paths(CLAUDE_PROJECT_DIR)`, computed locally | `hookSpecificOutput.watchPaths` |
| `FileChanged` | `Changed` for `file_path` | none |
| `PostToolBatch` | `Changed` for every `file_path` in `tool_calls`, then `Flush` | `hookSpecificOutput.additionalContext` when the flush returned any |
| `UserPromptSubmit` | `Flush` | `hookSpecificOutput.additionalContext` when non-empty |
| `SessionEnd` | `EndSession` | none |
| anything else | nothing | none |

Every fault, a missing socket, a connect timeout of 50 ms, a malformed response, a payload that will not parse, exits 0 having printed nothing. Write that as one wrapper so no branch can forget it:

```rust
/// Run `body`, and swallow whatever it does wrong.
///
/// An edit must never fail because diagnostics were unavailable, so there
/// is exactly one exit code and it is zero. The cost is that a broken
/// installation is invisible, which is what `mcpls hook doctor` exists to
/// answer.
fn silently<T: Default>(body: impl FnOnce() -> Result<T>) -> T {
    body().unwrap_or_default()
}
```

`Stop` is deliberately absent from the table. Its `additionalContext` is documented as non-error feedback after which the conversation continues so the model can act on it, so flushing there would turn every new warning into a keep-working signal. The next `UserPromptSubmit` delivers the same diagnostics anyway. Put that in a comment beside the match, since the next reader's first instinct will be to add it.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-cli`
Expected: PASS.

- [ ] **Step 5: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-cli/src/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(cli): dispatch claude code hooks over the socket"
```

---

## Task 21: `mcpls hook doctor`

Silent failure makes a broken install invisible. This is the one thing that answers it, so it is not optional polish.

**Files:**
- Modify: `crates/mcpls-cli/src/hook.rs`

**Interfaces:**
- Consumes: `identity_for`, `send`, `Request::Status`, `Response::Status`.

- [ ] **Step 1: write the failing tests**

```rust
#[test]
fn test_doctor_prints_both_hashes_so_a_mismatch_is_visible() {
    let project = tempfile::tempdir().expect("a temp dir");
    let elsewhere = tempfile::tempdir().expect("a temp dir");

    let out = doctor_with(project.path(), Some(elsewhere.path()));

    assert!(out.contains("hook sees"));
    assert!(out.contains("server sees"));
    assert!(
        out.contains("do not match"),
        "a config whose roots point at a subdirectory, a multi-root config, \
         or a symlinked checkout otherwise produces a permanent silent \
         no-op with nothing to look at"
    );
}

#[test]
fn test_doctor_reports_no_owner_when_nothing_is_bound() {
    let project = tempfile::tempdir().expect("a temp dir");
    let out = doctor_with(project.path(), None);
    assert!(out.contains("no owner"));
}

#[test]
fn test_doctor_reports_whether_mcpls_is_on_path() {
    let project = tempfile::tempdir().expect("a temp dir");
    let out = doctor_with(project.path(), None);
    assert!(
        out.contains("PATH"),
        "hooks invoke mcpls from PATH, and a hook environment missing the \
         install directory makes every hook do nothing, invisibly"
    );
}
```

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-cli hook::tests::test_doctor`
Expected: FAIL to compile.

- [ ] **Step 3: implement**

Print, in order:

1. The socket path.
2. `hook sees: <CLAUDE_PROJECT_DIR> -> <hash>`.
3. `server sees: <the live owner's directory, from Status> -> <hash>`, or `server sees: no owner` when nothing answers.
4. When both are known and differ, a line saying they do not match and that the hooks will do nothing until they do.
5. The owner's pid and whether it answered inside the 50 ms connect timeout.
6. Whether `mcpls` resolves on `PATH`, printing the resolved path or saying it does not.

`Response::Status` gains the owner's canonical directory so line 3 can print it. Add that field in this task and update Task 15's enum and its round-trip test to match.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-cli`
Expected: PASS.

- [ ] **Step 5: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-cli/src/ crates/mcpls-core/src/hooks/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(cli): answer whether the hooks can work"
```

---

## Task 22: the plugin, and the manual gate

**Files:**
- Create: `plugin/.claude-plugin/plugin.json`
- Create: `plugin/.mcp.json`
- Create: `plugin/hooks/hooks.json`
- Create: `plugin/README.md`
- Move: `skills/mcpls/SKILL.md` to `plugin/skills/mcpls/SKILL.md`
- Modify: `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md` (status line)

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

- [ ] **Step 2: move the skill**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc mv skills/mcpls/SKILL.md plugin/skills/mcpls/SKILL.md
```

Create the directory first if `git mv` refuses. Update any path reference to the old location; `rg -n 'skills/mcpls' /home/lev/Git/lev/mcpls-diag-bc` finds them.

- [ ] **Step 3: write the README, leading with doctor**

Open with the install command, then, before anything else, `mcpls hook doctor` and what each of its lines means. Every failure in this system is silent by design, so the first thing a reader needs is the command that breaks that silence. Then the `[diagnostics.hooks]` table and what `enabled = false` does.

- [ ] **Step 4: run the whole suite**

Run: `cargo nextest run --workspace && cargo nextest run -p mcpls-core --test ra_e2e -- --ignored ra_e2e_suite`
Expected: PASS.

- [ ] **Step 5: check formatting and lints**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 6: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc add plugin/ skills/ docs/
git -C /home/lev/Git/lev/mcpls-diag-bc commit -m "feat(plugin): ship mcpls as a claude code plugin"
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
2. **Is `footer_wait_ms = 15000` right after Task 1's measurement?** The number rests on `cargo check` taking about 4.7 seconds in this repository's largest crate. If Task 1 measures materially differently, adjust it in Task 12 and say so in that commit.
3. **Does pyrefly publish for a file it never opened?** Task 1 answers it and Task 9 branches on the answer. If it does not, B2 ships with no end-to-end proof against any server, which is worth knowing before stage C builds on it.
4. **Do gopls or ty send `RelativePattern` watchers despite the unclaimed capability?** The skipped-watcher log line is the only signal. Check the logs after the first real session with either server configured.
5. **What does `watchPaths` cost on a repository mid-build?** The host spawns a hook process per file event and the `watchPaths` list is the only bound. Measure it during a `cargo build`, not at rest.
6. **Should `Stop` ever flush?** Ruled against here, because its `additionalContext` continues the conversation and would make warnings an auto-continue loop. Revisit only if the host's documented semantics change.
