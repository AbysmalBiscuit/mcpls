# Language Server Control CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mcpls lsp {status,start,stop,restart}` reads and changes the language servers a checkout's shared backend runs, and a stopped server stays stopped until someone starts it.

**Architecture:** A new `ServerLifecycle::Stopped` state that `ensure_server` refuses. The teardown half of `install_server` becomes `retire_server`, which spawn, stop, and restart all use. The translator gains `control(action, servers)`, reached through a new `Request::Lsp` on the existing hook socket. The CLI sends it, then polls `Request::Status` until started servers settle.

**Tech Stack:** Rust, tokio, clap, serde, cargo-nextest. The fake language servers in tests are Python 3 scripts.

**Spec:** `docs/superpowers/specs/2026-09-21-lsp-control-cli-design.md`

## Global Constraints

- Work in the worktree `/home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli`. Never change the main checkout's branch.
- Write every shell command with absolute paths; no `cd`. Use `git -C <worktree>` for git.
- Commit through `devrun -C <worktree> task commit --arg 'coauthors=Claude Opus 5 <noreply@anthropic.com>' --arg 'commit_subject=<subject>' --arg 'commit_body=<body>' --arg 'files=<space-separated paths>'`. Leave out `commit_body` when the subject says it all. Subjects are Conventional Commits, imperative, lowercase after the colon, 50 characters or fewer.
- State wording is fixed: `stopped` renders as `stopped` and serializes as `"stopped"`. The refusal reason is exactly ``stopped; run `mcpls lsp start <id>` ``. The no-backend message is exactly `no backend serves <DIR>; one starts with an agent session`.
- Wire shapes are fixed: `{"op":"lsp","action":"stop","servers":["rust"]}` and `{"op":"lsp","servers":[{"id":"rust","state":"stopped"}]}`. `action` is one of `start`, `stop`, `restart`. An empty `servers` list means every applicable server.
- `Lsp` requests do not count toward `hooks_seen`.
- A stopped server does not fall back to a catch-all route.
- Comments follow the repository's rule: none by default, and when present they describe why, in the present tense, with no issue or task references.
- Run Rust tests with `cargo nextest run`, scoped by `-p` and `-E 'test(...)'` while iterating. Run `devrun -C <worktree> task verify` before each task's commit.

## Review Focus

1. **Stop issued while the eager startup batch is still spawning** (`spawn = "eager"`, the `register_servers` path in `lib.rs`). Expected: the server ends `stopped` with no process left running. Task 2 Step 7 guards it, but no test holds the batch open. Review the guard by reading it.
2. **`start` or `restart` sent to a backend that is shutting down.** Expected: an error saying so, and no process started. Tested in Task 3.
3. **A tool call arriving while `restart` swaps processes.** Expected: the caller gets "initializing" or an answer, never a hang or a panic. The window between `retire_server` and inserting the new client falls inside the `Starting` state, which callers already handle. Task 4's end-to-end test makes a tool call right after `restart` and expects success.
4. **`--all` on a checkout with no applicable servers.** Expected: exit 0 and the line `no language servers apply here`. Tested by Task 4's unit tests on `render` and `reached`.
5. **A server that never finishes `initialize` after `start`.** Expected: the CLI prints `starting` and exits 1 once the ceiling passes, instead of hanging. The ceiling comes from `timeout_seconds`. Tested by Task 4's `settle` unit test.

---

## File Structure

| File | Responsibility | Task |
|---|---|---|
| `crates/mcpls-core/src/bridge/translator/lifecycle.rs` | `Stopped` state, `set_lifecycle_unless_stopped`, `reset_for_explicit_start` | 1 |
| `crates/mcpls-core/src/bridge/translator/respawn.rs` | `ensure_server` refusal, `retire_server`, spawn outcomes respect `Stopped`, translator tests | 1, 2 |
| `crates/mcpls-core/src/bridge/translator/routing.rs`, `symbols.rs` | exhaustive `Stopped` arms | 1 |
| `crates/mcpls-core/src/bridge/translator/control.rs` (new) | `LspAction`, `control`, `stop_server`, `start_server`, `restart_server`, `tear_down` | 2 |
| `crates/mcpls-core/src/bridge/translator/mod.rs` | `shut_down_server` helper, module wiring | 2 |
| `crates/mcpls-core/src/lib.rs` | eager batch respects `Stopped` | 2 |
| `crates/mcpls-core/src/hooks/protocol.rs` | `Request::Lsp`, `Response::Lsp` | 3 |
| `crates/mcpls-core/src/hooks/service.rs` | handler arm and its tests | 3 |
| `crates/mcpls-core/src/hooks/sweep.rs` | `Sweeper::translator` accessor | 3 |
| `crates/mcpls-cli/src/args.rs` | `lsp` subcommand group | 4 |
| `crates/mcpls-cli/src/lsp.rs` (new) | socket exchange, polling, rendering, exit verdict | 4 |
| `crates/mcpls-cli/src/main.rs` | dispatch | 4 |
| `crates/mcpls-cli/src/hook.rs` | doctor render test gains `stopped` | 1 |
| `crates/mcpls-cli/tests/backend.rs` | end-to-end tests | 4 |
| `plugin/skills/setup-mcpls/references/cli.md`, `troubleshooting.md`, `plugin/skills/mcpls/SKILL.md` | documentation | 5 |

---

### Task 1: The `stopped` lifecycle state

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/lifecycle.rs`
- Modify: `crates/mcpls-core/src/bridge/translator/respawn.rs` (`ensure_server`, near line 225)
- Modify: `crates/mcpls-core/src/bridge/translator/routing.rs:105-108`
- Modify: `crates/mcpls-core/src/bridge/translator/symbols.rs:224-234`
- Modify: `crates/mcpls-cli/src/hook.rs` (test `test_the_servers_line_renders_every_lifecycle_state`, near line 3108)

**Interfaces:**
- Produces: `ServerLifecycle::Stopped`, and on `Translator`:
  - `pub fn set_lifecycle_unless_stopped(&self, id: &ServerId, state: ServerLifecycle) -> bool`, which returns false and changes nothing when `id` is `Stopped`;
  - `pub(crate) fn reset_for_explicit_start(&self, id: &ServerId)`, which moves `Stopped`, `NotInstalled`, or `Failed` to `Idle` and leaves anything else alone.

- [ ] **Step 1: Write the failing tests** in `lifecycle.rs`'s `tests` module. Also update the existing render test's expected vector.

```rust
    #[test]
    fn test_every_state_renders_for_a_reader() {
        let rendered: Vec<String> = ServerLifecycle::iter()
            .map(|state| state.to_string())
            .collect();
        assert_eq!(
            rendered,
            vec!["idle", "starting", "running", "not installed", "failed", "stopped"]
        );
    }

    #[test]
    fn test_a_stopped_server_keeps_its_state_against_a_spawn_outcome() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Stopped);
        assert!(!translator.set_lifecycle_unless_stopped(&id, ServerLifecycle::Running));
        assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Stopped));

        translator.set_lifecycle(&id, ServerLifecycle::Starting);
        assert!(translator.set_lifecycle_unless_stopped(&id, ServerLifecycle::Running));
        assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Running));
    }

    #[test]
    fn test_an_explicit_start_resets_only_states_that_block_a_spawn() {
        let translator = Translator::new();
        for (state, expected) in [
            (ServerLifecycle::Stopped, ServerLifecycle::Idle),
            (ServerLifecycle::NotInstalled, ServerLifecycle::Idle),
            (ServerLifecycle::Failed, ServerLifecycle::Idle),
            (ServerLifecycle::Running, ServerLifecycle::Running),
            (ServerLifecycle::Starting, ServerLifecycle::Starting),
        ] {
            let id = ServerId::from(state.to_string());
            translator.set_lifecycle(&id, state);
            translator.reset_for_explicit_start(&id);
            assert_eq!(translator.lifecycle_of(&id), Some(expected), "{state}");
        }
    }

    #[test]
    fn test_a_stopped_server_does_not_claim_a_spawn() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Stopped);
        assert!(!translator.begin_starting(&id));
    }
```

Add to `respawn.rs`'s top-level `tests` module (not `respawn_tests`, since no process is needed):

```rust
    #[tokio::test]
    async fn test_a_stopped_server_is_refused_with_the_command_that_starts_it() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Stopped);
        let err = translator
            .ensure_server(&id, Some(Duration::ZERO))
            .await
            .expect_err("a stopped server is not started on use");
        assert_eq!(
            err.to_string(),
            "LSP server 'rust' is unavailable: stopped; run `mcpls lsp start rust`"
        );
        assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Stopped));
    }
```

Add to `routing.rs`'s tests, directly after `test_a_crashed_narrow_server_does_not_fall_through`:

```rust
    #[tokio::test]
    async fn test_a_stopped_narrow_server_does_not_fall_through() {
        let translator = translator_with_rust_route(router_with_narrow_rust_claim());
        let narrow_id = ServerId::from("rust-narrow");
        let catch_all_id = ServerId::from("rust");
        translator.set_lifecycle(&narrow_id, ServerLifecycle::Stopped);
        translator.set_lifecycle(&catch_all_id, ServerLifecycle::Running);
        let (client, _server) = fake_lsp_client();
        translator.register_client(catch_all_id, client);

        let (id, client) = translator
            .get_client_for_file(Path::new("/work/src/main.rs"), ToolKind::Hover)
            .expect("a stopped server keeps its own route");

        assert_eq!(id, narrow_id);
        assert!(client.is_none());
    }
```

- [ ] **Step 2: Run the tests and watch them fail to compile**

Run: `cargo nextest run --manifest-path /home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli/Cargo.toml -p mcpls-core -E 'test(stopped) | test(every_state_renders) | test(explicit_start)'`
Expected: compile errors, because `ServerLifecycle::Stopped`, `set_lifecycle_unless_stopped`, and `reset_for_explicit_start` don't exist.

- [ ] **Step 3: Implement.** In `lifecycle.rs`, add the variant after `Failed`:

```rust
    /// Stopped by a user. Nothing starts it but an explicit start.
    Stopped,
```

Route every insert-and-broadcast through one private helper, and add the two methods:

```rust
    #[expect(
        clippy::significant_drop_tightening,
        reason = "state and watch updates must remain atomic"
    )]
    pub fn set_lifecycle(&self, id: &ServerId, state: ServerLifecycle) {
        let mut states = lock_std(&self.lifecycles);
        self.publish_locked(&mut states, id, state);
    }

    /// Record `state` for `id` unless a user stopped it, returning whether
    /// it was recorded.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "state and watch updates must remain atomic"
    )]
    pub fn set_lifecycle_unless_stopped(&self, id: &ServerId, state: ServerLifecycle) -> bool {
        let mut states = lock_std(&self.lifecycles);
        if states.get(id) == Some(&ServerLifecycle::Stopped) {
            return false;
        }
        self.publish_locked(&mut states, id, state);
        true
    }

    /// Return a stopped, missing, or failed `id` to `Idle` so an explicit
    /// start may claim it.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "state and watch updates must remain atomic"
    )]
    pub(crate) fn reset_for_explicit_start(&self, id: &ServerId) {
        let mut states = lock_std(&self.lifecycles);
        if matches!(
            states.get(id),
            Some(ServerLifecycle::Stopped | ServerLifecycle::NotInstalled | ServerLifecycle::Failed)
        ) {
            self.publish_locked(&mut states, id, ServerLifecycle::Idle);
        }
    }

    fn publish_locked(
        &self,
        states: &mut HashMap<ServerId, ServerLifecycle>,
        id: &ServerId,
        state: ServerLifecycle,
    ) {
        states.insert(id.clone(), state);
        lock_std(&self.lifecycle_senders)
            .entry(id.clone())
            .or_insert_with(|| watch::channel(state).0)
            .send_replace(state);
    }
```

Rewrite `begin_starting` to use `publish_locked` for its insert. Its accepted-states match stays as is, and `Stopped` already falls to `_ => return false`. If clippy reports an `#[expect]` as unfulfilled, remove that `#[expect]`.

In `respawn.rs` `ensure_server`, add this check at the very top, before `has_live_client`. A stop that races a spawn can leave a live client registered for a moment, and it must not be used:

```rust
        if self.lifecycle_of(id) == Some(ServerLifecycle::Stopped) {
            return Err(Error::ServerUnavailable {
                server_id: id.clone(),
                reason: format!("stopped; run `mcpls lsp start {id}`"),
            });
        }
```

In `routing.rs:107`, extend the arm:

```rust
                Some(
                    ServerLifecycle::Idle
                    | ServerLifecycle::Starting
                    | ServerLifecycle::Failed
                    | ServerLifecycle::Stopped,
                ) => return Ok((id, None)),
```

In `symbols.rs`, add an arm before `Running`:

```rust
            Some(ServerLifecycle::Stopped) => Error::ServerUnavailable {
                server_id: server_id.clone(),
                reason: format!("stopped; run `mcpls lsp start {server_id}`"),
            },
```

In `crates/mcpls-cli/src/hook.rs`, append `, language (stopped)` to the expected string in `test_the_servers_line_renders_every_lifecycle_state`.

- [ ] **Step 4: Run the tests and see them pass**

Run: `cargo nextest run --manifest-path /home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli/Cargo.toml --workspace -E 'test(stopped) | test(every_state_renders) | test(explicit_start) | test(every_lifecycle_state) | test(fall_through)'`
Expected: PASS.

- [ ] **Step 5: Verify and commit**

Run: `devrun -C /home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli task verify`
Expected: all green.

Commit with subject `feat(lsp): add a stopped server lifecycle state` and files `crates/mcpls-core/src/bridge/translator/lifecycle.rs crates/mcpls-core/src/bridge/translator/respawn.rs crates/mcpls-core/src/bridge/translator/routing.rs crates/mcpls-core/src/bridge/translator/symbols.rs crates/mcpls-cli/src/hook.rs`.

---

### Task 2: Stop, start, and restart in the translator

**Files:**
- Create: `crates/mcpls-core/src/bridge/translator/control.rs`
- Modify: `crates/mcpls-core/src/bridge/translator/mod.rs` (`mod control;`, re-export, `shut_down_server`, `shutdown_servers`)
- Modify: `crates/mcpls-core/src/bridge/translator/respawn.rs` (`retire_server`, `install_server`, `run_spawn`, `SpawnGuard`, visibility, tests in `respawn_tests`)
- Modify: `crates/mcpls-core/src/bridge/mod.rs` (re-export `LspAction`)
- Modify: `crates/mcpls-core/src/lib.rs` (eager batch, near lines 1180-1240)

**Interfaces:**
- Consumes (Task 1): `ServerLifecycle::Stopped`, `set_lifecycle_unless_stopped`, `reset_for_explicit_start`.
- Produces:
  - `pub enum LspAction { Start, Stop, Restart }`, which derives `Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize` with `#[serde(rename_all = "snake_case")]` and is exported as `mcpls_core::bridge::LspAction`;
  - `pub async fn control(&self, action: LspAction, requested: &[String]) -> std::result::Result<Vec<(ServerId, ServerLifecycle)>, String>` on `Translator`;
  - `pub(crate) async fn stop_server(&self, id: &ServerId)`, `start_server`, `restart_server`, and `pub(crate) async fn tear_down(&self, id: &ServerId)`.

- [ ] **Step 1: Write the failing tests** in `respawn.rs` inside `mod respawn_tests` (Unix-only). They reuse `spawnable`, `stub_server_config`, `wait_for_lifecycle`, and `RESPAWN_WAIT` from that module. The fixture logs `start <pid>` when launched and `exit <pid>` on the LSP `exit` notification. A clean shutdown shows up in the log, so no test depends on whether the parent has reaped the child.

```rust
        fn write_lifecycle_server(dir: &Path) -> (PathBuf, PathBuf) {
            let script_path = dir.join("lifecycle_server.py");
            let log_path = dir.join("lifecycle.log");
            let body = r#"import json, os, sys, time

log = open(sys.argv[1], "a", buffering=1)
log.write(f"start {os.getpid()}\n")

def receive():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode().split(":", 1)
        if key.lower() == "content-length":
            length = int(value.strip())
    return json.loads(sys.stdin.buffer.read(length))

def send(message):
    body = json.dumps({"jsonrpc": "2.0", **message}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()

while True:
    message = receive()
    if message is None:
        break
    method = message.get("method")
    if method == "initialize":
        time.sleep(0.2)
        send({"id": message["id"], "result": {"capabilities": {"positionEncoding": "utf-16"}}})
    elif method == "exit":
        log.write(f"exit {os.getpid()}\n")
        break
    elif "id" in message:
        send({"id": message["id"], "result": None})
"#;
            fs::write(&script_path, body).unwrap();
            (script_path, log_path)
        }

        fn lifecycle_translator(dir: &Path) -> (Arc<Translator>, ServerId, PathBuf) {
            let (script, log) = write_lifecycle_server(dir);
            let id = ServerId::from("rust");
            let mut config = stub_server_config("rust", &script);
            config.server_config.command = "python3".to_string();
            config.server_config.args = vec![
                script.to_string_lossy().into_owned(),
                log.to_string_lossy().into_owned(),
            ];
            let translator = spawnable(Translator::new(), &id);
            translator.register_server_config(id.clone(), config);
            (translator, id, log)
        }

        fn logged(log: &Path, kind: &str) -> Vec<u32> {
            fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| line.strip_prefix(kind)?.trim().parse().ok())
                .collect()
        }

        async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !done() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
        }

        #[tokio::test]
        async fn test_stopping_a_running_server_shuts_it_down_and_keeps_it_down() {
            let dir = TempDir::new().unwrap();
            let (translator, id, log) = lifecycle_translator(dir.path());
            translator.ensure_server(&id, None).await.unwrap();
            wait_for_lifecycle(&translator, &id, ServerLifecycle::Running).await;
            let pid = logged(&log, "start")[0];

            translator.stop_server(&id).await;

            assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Stopped));
            assert!(!translator.has_live_client(&id));
            wait_until("the stopped process to exit", || logged(&log, "exit").contains(&pid)).await;
            assert!(translator.ensure_server(&id, Some(RESPAWN_WAIT)).await.is_err());
            assert_eq!(logged(&log, "start").len(), 1);
        }

        #[tokio::test]
        async fn test_a_stop_during_a_spawn_retires_the_fresh_server() {
            let dir = TempDir::new().unwrap();
            let (translator, id, log) = lifecycle_translator(dir.path());
            translator.ensure_server(&id, None).await.unwrap();
            assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Starting));

            translator.stop_server(&id).await;
            wait_until("the spawn to launch", || logged(&log, "start").len() == 1).await;
            let pid = logged(&log, "start")[0];
            wait_until("the fresh process to exit", || logged(&log, "exit").contains(&pid)).await;

            assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Stopped));
            assert!(!translator.has_live_client(&id));
        }

        #[tokio::test]
        async fn test_starting_a_stopped_server_spawns_it_again() {
            let dir = TempDir::new().unwrap();
            let (translator, id, log) = lifecycle_translator(dir.path());
            translator.stop_server(&id).await;

            translator.start_server(&id).await;
            wait_for_lifecycle(&translator, &id, ServerLifecycle::Running).await;
            assert_eq!(logged(&log, "start").len(), 1);
            translator.shutdown_servers().await;
        }

        #[tokio::test]
        async fn test_an_explicit_start_retries_a_server_recorded_as_not_installed() {
            let dir = TempDir::new().unwrap();
            let (translator, id, log) = lifecycle_translator(dir.path());
            translator.set_lifecycle(&id, ServerLifecycle::NotInstalled);

            translator.start_server(&id).await;
            wait_for_lifecycle(&translator, &id, ServerLifecycle::Running).await;
            assert_eq!(logged(&log, "start").len(), 1);
            translator.shutdown_servers().await;
        }

        #[tokio::test]
        async fn test_restarting_a_running_server_replaces_its_process() {
            let dir = TempDir::new().unwrap();
            let (translator, id, log) = lifecycle_translator(dir.path());
            translator.ensure_server(&id, None).await.unwrap();
            wait_for_lifecycle(&translator, &id, ServerLifecycle::Running).await;
            let first = logged(&log, "start")[0];

            translator.restart_server(&id).await;
            wait_until("the replacement to launch", || logged(&log, "start").len() == 2).await;
            wait_for_lifecycle(&translator, &id, ServerLifecycle::Running).await;
            wait_until("the replaced process to exit", || logged(&log, "exit").contains(&first)).await;
            assert!(translator.has_live_client(&id));
            translator.shutdown_servers().await;
        }

        #[tokio::test]
        async fn test_control_names_unknown_servers_and_applies_nothing() {
            let dir = TempDir::new().unwrap();
            let (translator, id, _log) = lifecycle_translator(dir.path());

            let err = translator
                .control(LspAction::Stop, &["rust".to_string(), "nope".to_string()])
                .await
                .expect_err("an unknown id refuses the whole request");
            assert_eq!(err, "no language server named nope applies here; these do: rust");
            assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Idle));
        }

        #[tokio::test]
        async fn test_control_with_no_names_acts_on_every_applicable_server() {
            let dir = TempDir::new().unwrap();
            let (translator, id, _log) = lifecycle_translator(dir.path());

            let states = translator.control(LspAction::Stop, &[]).await.unwrap();
            assert_eq!(states, vec![(id, ServerLifecycle::Stopped)]);
        }
```

Add `use crate::bridge::translator::control::LspAction;` to `respawn_tests`' imports.

- [ ] **Step 2: Run the tests and watch them fail to compile**

Run: `cargo nextest run --manifest-path /home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli/Cargo.toml -p mcpls-core -E 'test(stop) | test(start) | test(restart) | test(control_)'`
Expected: compile errors, because `stop_server`, `start_server`, `restart_server`, `control`, and `LspAction` don't exist.

- [ ] **Step 3: Add `shut_down_server` to `mod.rs`** and have `shutdown_servers` use it. In `shutdown_servers`, replace the `tasks.spawn(async move { ... })` call with `tasks.spawn(shut_down_server(id, server));`. Add at module level after `SERVER_SHUTDOWN_TIMEOUT`:

```rust
/// Shut `server` down with the LSP `shutdown`/`exit` handshake, letting
/// `kill_on_drop` end it when that fails or outlasts
/// [`SERVER_SHUTDOWN_TIMEOUT`]. A process that already exited is only
/// reaped.
pub(super) async fn shut_down_server(id: ServerId, mut server: LspServer) {
    if matches!(server.has_exited(), Ok(true)) {
        return;
    }
    match tokio::time::timeout(SERVER_SHUTDOWN_TIMEOUT, server.shutdown()).await {
        Ok(Ok(())) => tracing::debug!(%id, "LSP server shut down gracefully"),
        Ok(Err(e)) => tracing::warn!(
            %id, error = %e,
            "LSP server shutdown handshake failed, killing process instead"
        ),
        Err(_) => tracing::warn!(
            %id, timeout = ?SERVER_SHUTDOWN_TIMEOUT,
            "LSP server did not shut down in time, killing process instead"
        ),
    }
}
```

Add `mod control;` beside the other `mod` lines and `pub use self::control::LspAction;` beside `pub use self::lifecycle::ServerLifecycle;`. In `bridge/mod.rs`, add `LspAction` to the `pub use translator::{ ... }` list.

- [ ] **Step 4: Pull `retire_server` out of `install_server`** in `respawn.rs`, inside `impl Translator`:

```rust
    /// Detach `id`'s server from routing and from every cache holding its
    /// state, returning the process for the caller to shut down.
    pub(super) async fn retire_server(&self, id: &ServerId) -> Option<LspServer> {
        let old_client = lock_std(&self.lsp_clients).remove(id);
        if let Some(old_client) = old_client {
            old_client.fail_pending_requests().await;
        }
        if let Some(pumps) = self.notification_pumps.get() {
            pumps.retire(id).await;
        }
        let language = lock_std(&self.server_configs)
            .get(id)
            .map(|config| config.server_config.language_id.clone());
        if language.is_some_and(|language| self.is_diagnostics_route(&language, id))
            && let Some(cache) = &self.notification_cache
        {
            cache.lock().await.clear_server_diagnostics(id);
        }
        self.document_tracker.forget_server(id);
        lock_std(&self.lsp_servers).remove(id)
    }
```

In `install_server`, replace everything from `let old_client = lock_std(&self.lsp_clients).get(id).cloned();` through `self.document_tracker.forget_server(id);` with `let old_server = self.retire_server(id).await;`. Keep the `if let Some(pumps) = self.notification_pumps.get() { if caches_diagnostics { ... } pumps.install(...) }` block unchanged. Replace the tail, from `let old_server = lock_std(&self.lsp_servers).insert(...)` to `Ok(())`, with:

```rust
        lock_std(&self.lsp_servers).insert(id.clone(), new_server);
        lock_std(&self.lsp_clients).insert(id.clone(), new_client);
        if let Some(old_server) = old_server {
            tokio::spawn(super::shut_down_server(id.clone(), old_server));
        }
        Ok(())
```

Make `has_live_client`, `run_spawn`, and `reconcile_diagnostics_owners` `pub(super)`. Add beside `record_respawn_success`:

```rust
    pub(super) fn clear_respawn_backoff(&self, id: &ServerId) {
        lock_std(&self.respawn_backoffs).remove(id);
        lock_std(&self.spawn_errors).remove(id);
    }
```

- [ ] **Step 5: Make spawn outcomes respect `Stopped`.** Replace `SpawnGuard::publish` with:

```rust
    fn publish_unless_stopped(&mut self, state: ServerLifecycle) -> bool {
        self.armed = false;
        self.translator.set_lifecycle_unless_stopped(&self.id, state)
    }
```

In `Drop for SpawnGuard`, replace the `set_lifecycle(&self.id, ServerLifecycle::Failed)` call with `set_lifecycle_unless_stopped(&self.id, ServerLifecycle::Failed);`. In `run_spawn`, replace the `match self.install_server(...)` block with:

```rust
        match self.install_server(&id, config).await {
            Ok(()) => {
                self.record_respawn_success(&id);
                if !guard.publish_unless_stopped(ServerLifecycle::Running) {
                    tracing::info!("LSP server '{id}' was stopped while it started");
                    self.tear_down(&id).await;
                    return;
                }
                self.reconcile_diagnostics_owners(Some(&id)).await;
                tracing::info!("LSP server '{id}' is running");
            }
            Err(err) => {
                self.record_respawn_failure(&id);
                let state = if err.is_missing_binary() {
                    ServerLifecycle::NotInstalled
                } else {
                    ServerLifecycle::Failed
                };
                tracing::error!("LSP server '{id}' failed to start: {err}");
                guard.publish_unless_stopped(state);
                self.reconcile_diagnostics_owners(None).await;
            }
        }
```

- [ ] **Step 6: Create `control.rs`**

```rust
//! Lifecycle changes a user asks for: stop, start, and restart.

use std::sync::Weak;

use serde::{Deserialize, Serialize};

use super::{ServerLifecycle, Translator, shut_down_server};
use crate::config::ServerId;

/// A lifecycle change requested through `mcpls lsp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspAction {
    /// Spawn a server that is not running.
    Start,
    /// Shut a server down and keep it down until an explicit start.
    Stop,
    /// Replace a running server's process, or start one that is not running.
    Restart,
}

impl Translator {
    /// Apply `action` to each server in `requested`, or to every applicable
    /// server when `requested` is empty, returning each one's state
    /// afterwards.
    ///
    /// # Errors
    ///
    /// Names the ids that do not apply to this checkout and applies
    /// nothing.
    pub async fn control(
        &self,
        action: LspAction,
        requested: &[String],
    ) -> std::result::Result<Vec<(ServerId, ServerLifecycle)>, String> {
        let applicable: Vec<ServerId> = self.lifecycles().into_iter().map(|(id, _)| id).collect();
        let targets: Vec<ServerId> = if requested.is_empty() {
            applicable.clone()
        } else {
            requested.iter().map(|id| ServerId::from(id.as_str())).collect()
        };
        let unknown: Vec<&str> = targets
            .iter()
            .filter(|id| !applicable.contains(id))
            .map(ServerId::as_str)
            .collect();
        if !unknown.is_empty() {
            let known: Vec<&str> = applicable.iter().map(ServerId::as_str).collect();
            let known = if known.is_empty() {
                "none do".to_string()
            } else {
                format!("these do: {}", known.join(", "))
            };
            return Err(format!(
                "no language server named {} applies here; {known}",
                unknown.join(", ")
            ));
        }
        for id in &targets {
            match action {
                LspAction::Start => self.start_server(id).await,
                LspAction::Stop => self.stop_server(id).await,
                LspAction::Restart => self.restart_server(id).await,
            }
        }
        Ok(targets
            .into_iter()
            .filter_map(|id| self.lifecycle_of(&id).map(|state| (id, state)))
            .collect())
    }

    /// Mark `id` stopped, then shut its process down.
    pub(crate) async fn stop_server(&self, id: &ServerId) {
        self.set_lifecycle(id, ServerLifecycle::Stopped);
        self.tear_down(id).await;
    }

    /// Spawn `id` unless it is already running or starting. Retries a
    /// server recorded as not installed, since an explicit start usually
    /// follows installing it.
    pub(crate) async fn start_server(&self, id: &ServerId) {
        self.reset_for_explicit_start(id);
        self.clear_respawn_backoff(id);
        // The lifecycle the caller reads afterwards carries the outcome.
        let _ = self.ensure_server(id, None).await;
    }

    /// Replace `id`'s live process with a fresh one, or start it when none
    /// is live.
    pub(crate) async fn restart_server(&self, id: &ServerId) {
        if !self.has_live_client(id) {
            self.start_server(id).await;
            return;
        }
        self.clear_respawn_backoff(id);
        if !self.begin_starting(id) {
            return;
        }
        match self.self_handle.get().and_then(Weak::upgrade) {
            Some(translator) => {
                tokio::spawn(translator.run_spawn(id.clone()));
            }
            None => self.set_lifecycle(id, ServerLifecycle::Failed),
        }
    }

    /// Detach `id` from routing, drop its watches and diagnostics
    /// ownership, and shut its process down in the background.
    pub(crate) async fn tear_down(&self, id: &ServerId) {
        self.forget_watch_registrations(id);
        if let Some(server) = self.retire_server(id).await {
            tokio::spawn(shut_down_server(id.clone(), server));
        }
        self.reconcile_diagnostics_owners(None).await;
    }
}
```

`reconcile_diagnostics_owners` returns early when `notification_pumps` is unset, which happens in these unit tests.

- [ ] **Step 7: Guard the eager batch** in `lib.rs`. A stop can arrive while the eager batch is spawning. Replace the loop that publishes `Running`:

```rust
        let registered = register_servers(result, &translator);
        for id in registered.diagnostics_flags.keys() {
            if !translator.set_lifecycle_unless_stopped(id, ServerLifecycle::Running) {
                translator.tear_down(id).await;
            }
        }
```

Filter stopped servers out of `diagnostics_owners`:

```rust
        let diagnostics_owners = registered
            .diagnostics_flags
            .iter()
            .filter(|&(id, &is_route)| {
                is_route && translator.lifecycle_of(id) != Some(ServerLifecycle::Stopped)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
```

In the failures loop above it, replace `translator.set_lifecycle(&failure.server_id, state);` with `translator.set_lifecycle_unless_stopped(&failure.server_id, state);`.

- [ ] **Step 8: Run the new tests and the existing respawn and recovery suites**

Run: `cargo nextest run --manifest-path /home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli/Cargo.toml -p mcpls-core -E 'test(stop) | test(start) | test(restart) | test(control_) | test(respawn) | test(ensure_server) | test(recovery) | test(replacement)'`
Expected: PASS. An existing test may fail because it expected the replaced client to stay in `lsp_clients` until the new one lands. If it asserts that ordering rather than something a user sees, update it to the new ordering and say so in the commit body. If it asserts behavior a user sees, stop and report it.

- [ ] **Step 9: Verify and commit**

Run: `devrun -C /home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli task verify`
Expected: all green.

Commit with subject `feat(lsp): stop, start and restart servers` and body:

```
Retiring a server moves out of install_server so a spawn, a stop and
a restart detach a process the same way, and a replaced process gets
the LSP shutdown handshake instead of being dropped. Spawn outcomes
no longer overwrite a stopped state, so a stop that races a spawn
retires the fresh process.
```

Files: `crates/mcpls-core/src/bridge/translator/control.rs crates/mcpls-core/src/bridge/translator/mod.rs crates/mcpls-core/src/bridge/translator/respawn.rs crates/mcpls-core/src/bridge/mod.rs crates/mcpls-core/src/lib.rs`.

---

### Task 3: `Request::Lsp` on the hook socket

**Files:**
- Modify: `crates/mcpls-core/src/hooks/protocol.rs`
- Modify: `crates/mcpls-core/src/hooks/service.rs` (handler arm near line 167, doc comments on `build_handler` and `record_hook_request`, tests)
- Modify: `crates/mcpls-core/src/hooks/sweep.rs` (accessor)

**Interfaces:**
- Consumes (Task 2): `LspAction`, `Translator::control`.
- Produces:
  - `Request::Lsp { action: LspAction, servers: Vec<String> }`;
  - `Response::Lsp { servers: Vec<ServerStatus> }`;
  - `pub(crate) fn translator(&self) -> &Arc<Translator>` on `Sweeper`.

- [ ] **Step 1: Write the failing tests.** In `protocol.rs` tests, next to `test_the_status_request_pins_the_wire_shape`:

```rust
    #[test]
    fn test_the_lsp_request_pins_the_wire_shape() {
        let literal = r#"{"op":"lsp","action":"stop","servers":["rust"]}"#;
        let value = Request::Lsp {
            action: crate::bridge::LspAction::Stop,
            servers: vec!["rust".to_string()],
        };
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Request>(literal).expect("deserialize"),
            value
        );
    }

    #[test]
    fn test_the_lsp_response_pins_the_wire_shape() {
        let literal = r#"{"op":"lsp","servers":[{"id":"rust","state":"stopped"}]}"#;
        let value = Response::Lsp {
            servers: vec![ServerStatus {
                id: "rust".to_string(),
                state: ServerLifecycle::Stopped,
            }],
        };
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Response>(literal).expect("deserialize"),
            value
        );
    }
```

In `service.rs` tests, use the existing `HookHarness::owner_without_baseline()`. Its translator has no server configs, so set lifecycles directly:

```rust
    #[tokio::test]
    async fn test_an_lsp_stop_answers_each_server_s_new_state() {
        let harness = HookHarness::owner_without_baseline().await;
        harness
            .sweeper
            .translator()
            .set_lifecycle(&ServerId::from("rust"), ServerLifecycle::Idle);

        let answer = harness
            .send(Request::Lsp {
                action: LspAction::Stop,
                servers: vec!["rust".to_string()],
            })
            .await;
        assert_eq!(
            answer,
            Response::Lsp {
                servers: vec![ServerStatus {
                    id: "rust".to_string(),
                    state: ServerLifecycle::Stopped,
                }],
            }
        );

        let Response::Status { hooks_seen, .. } = harness.send(Request::Status).await else {
            panic!("expected a status response");
        };
        assert_eq!(hooks_seen, 0, "an lsp request is not a hook firing");
    }

    #[tokio::test]
    async fn test_an_lsp_request_naming_an_unknown_server_is_an_error() {
        let harness = HookHarness::owner_without_baseline().await;
        harness
            .sweeper
            .translator()
            .set_lifecycle(&ServerId::from("rust"), ServerLifecycle::Idle);

        let answer = harness
            .send(Request::Lsp {
                action: LspAction::Start,
                servers: vec!["nope".to_string()],
            })
            .await;
        assert_eq!(
            answer,
            Response::Error {
                message: "no language server named nope applies here; these do: rust"
                    .to_string(),
            }
        );
    }

    #[tokio::test]
    async fn test_an_lsp_start_is_refused_while_shutting_down() {
        let harness = HookHarness::owner_without_baseline().await;
        harness
            .sweeper
            .translator()
            .set_lifecycle(&ServerId::from("rust"), ServerLifecycle::Idle);
        harness._cancel.send_replace(true);

        let answer = harness
            .send(Request::Lsp {
                action: LspAction::Start,
                servers: Vec::new(),
            })
            .await;
        assert!(matches!(answer, Response::Error { .. }), "{answer:?}");
        assert_eq!(
            harness.sweeper.translator().lifecycle_of(&ServerId::from("rust")),
            Some(ServerLifecycle::Idle)
        );
    }
```

Add the imports the test module lacks: `crate::bridge::{LspAction, ServerLifecycle}`, `crate::config::ServerId`, and `crate::hooks::protocol::ServerStatus`. The harness keeps the cancel sender in a field named `_cancel`. Now that a test reads it, rename it to `cancel` everywhere in the harness.

- [ ] **Step 2: Run the tests and watch them fail to compile**

Run: `cargo nextest run --manifest-path /home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli/Cargo.toml -p mcpls-core -E 'test(lsp_)'`
Expected: compile errors, because `Request::Lsp`, `Response::Lsp`, and `Sweeper::translator` don't exist.

- [ ] **Step 3: Implement.** In `protocol.rs`, change the import to `use crate::bridge::{HookAgent, LspAction, ServerLifecycle};`. Add after `Status` in `Request`:

```rust
    /// Sent by `mcpls lsp` to start, stop, or restart language servers.
    Lsp {
        /// The change to make.
        action: LspAction,
        /// The servers to change, every applicable one when empty.
        #[serde(default)]
        servers: Vec<String>,
    },
```

Add after `Status` in `Response`:

```rust
    /// Answers a [`Request::Lsp`] with each named server's state right
    /// after the change, before a start has settled.
    Lsp {
        /// The servers the request named.
        servers: Vec<ServerStatus>,
    },
```

In `sweep.rs`, inside `impl Sweeper`:

```rust
    /// The translator this sweeper drives.
    pub(crate) fn translator(&self) -> &Arc<Translator> {
        &self.translator
    }
```

In `service.rs`, add `use crate::bridge::LspAction;` at the top and this arm after `Request::Status`:

```rust
                Request::Lsp { action, servers } => {
                    if cancelled && action != LspAction::Stop {
                        return Response::Error {
                            message: "mcpls is shutting down; no language server was started"
                                .to_string(),
                        };
                    }
                    match sweeper.translator().control(action, &servers).await {
                        Ok(states) => Response::Lsp {
                            servers: states
                                .into_iter()
                                .map(|(id, state)| ServerStatus {
                                    id: id.to_string(),
                                    state,
                                })
                                .collect(),
                        },
                        Err(message) => Response::Error { message },
                    }
                }
```

In `build_handler`'s doc comment, add after "No arm of this match takes either lock itself.": "The `lsp` arm reaches the cache only through the translator, and never while holding delivery." In `HookStats::record_hook_request`'s doc, change "Not called for `Status`, which is `mcpls doctor` probing rather than a hook firing" to "Not called for `Status` or `Lsp`, which are the CLI probing or steering rather than a hook firing".

The build now fails wherever `Request` or `Response` is matched exhaustively. Fix each site the compiler names. `crates/mcpls-cli/src/hook.rs` already has a catch-all `Answered(other)` arm.

- [ ] **Step 4: Run the tests and see them pass**

Run: `cargo nextest run --manifest-path /home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli/Cargo.toml --workspace -E 'test(lsp_) | test(pins_the_wire_shape)'`
Expected: PASS.

- [ ] **Step 5: Verify and commit**

Run: `devrun -C /home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli task verify`
Expected: all green.

Commit with subject `feat(hooks): accept lsp control requests` and files `crates/mcpls-core/src/hooks/protocol.rs crates/mcpls-core/src/hooks/service.rs crates/mcpls-core/src/hooks/sweep.rs`, plus any file the compiler fixes in Step 3 touched.

---

### Task 4: The `mcpls lsp` commands

**Files:**
- Modify: `crates/mcpls-cli/src/args.rs`
- Create: `crates/mcpls-cli/src/lsp.rs`
- Modify: `crates/mcpls-cli/src/main.rs`
- Test: `crates/mcpls-cli/tests/backend.rs`

**Interfaces:**
- Consumes (Task 3): `Request::Lsp`, `Response::Lsp`, `mcpls_core::bridge::LspAction`, and from `mcpls_core::hooks`: `probe`, `ProbeOutcome`, `Request`, `Response`, `SocketIdentity`; also `mcpls_core::hooks::protocol::ServerStatus`.
- Produces:
  - `Command::Lsp { action: LspCommand }`, where `LspCommand` is `Status { dir }`, `Start { targets, no_wait }`, `Stop { targets }`, or `Restart { targets, no_wait }`;
  - `LspTargets { servers: Vec<String>, all: bool, dir: Option<PathBuf> }`;
  - `lsp::status(&SocketIdentity, &Path) -> Outcome` and `lsp::control(&SocketIdentity, &Path, LspAction, Vec<String>, Option<Duration>) -> Outcome`, where `Outcome { text: String, success: bool }`.

- [ ] **Step 1: Write the failing argument tests** in `args.rs` tests:

```rust
    #[test]
    fn test_lsp_actions_take_servers_or_all_but_not_both() {
        assert!(matches!(
            Args::parse_from(["mcpls", "lsp", "start", "rust", "lua"]).command,
            Some(Command::Lsp {
                action: LspCommand::Start { targets, no_wait: false }
            }) if targets.servers == ["rust", "lua"] && !targets.all
        ));
        assert!(matches!(
            Args::parse_from(["mcpls", "lsp", "stop", "--all", "--dir", "/work"]).command,
            Some(Command::Lsp {
                action: LspCommand::Stop { targets }
            }) if targets.all && targets.dir.as_deref() == Some(std::path::Path::new("/work"))
        ));
        assert!(Args::try_parse_from(["mcpls", "lsp", "stop", "rust", "--all"]).is_err());
        assert!(Args::try_parse_from(["mcpls", "lsp", "restart"]).is_err());
    }

    #[test]
    fn test_lsp_status_takes_only_a_directory() {
        assert!(matches!(
            Args::parse_from(["mcpls", "lsp", "status"]).command,
            Some(Command::Lsp {
                action: LspCommand::Status { dir: None }
            })
        ));
        assert!(Args::try_parse_from(["mcpls", "lsp", "stop", "rust", "--no-wait"]).is_err());
    }
```

- [ ] **Step 2: Write the failing end-to-end tests** in `backend.rs`. Add harness helpers inside `impl Project`:

```rust
    #[cfg(unix)]
    fn lifecycle_log(&self) -> PathBuf {
        self.root().join("lifecycle.log")
    }

    /// A language server that answers the handshake, logs `start <pid>`
    /// when launched and `exit <pid>` on the LSP `exit` notification.
    #[cfg(unix)]
    fn with_lifecycle_server(self) -> Self {
        std::fs::write(self.root().join("marker.fake"), "").unwrap();
        std::fs::write(self.root().join("a.fake"), "").unwrap();
        let script = self.root().join("lifecycle_lsp.py");
        std::fs::write(&script, LIFECYCLE_LSP).unwrap();
        self.write_config(
            60_000,
            &format!(
                "\n[[lsp_servers]]\nlanguage_id = \"fake\"\ncommand = \"python3\"\nargs = [{:?}, {:?}]\nfile_patterns = [\"**/*.fake\"]\ntimeout_seconds = 10\n\n[lsp_servers.heuristics]\nproject_markers = [\"marker.fake\"]\n",
                script.display().to_string(),
                self.lifecycle_log().display().to_string(),
            ),
        );
        self
    }

    #[cfg(unix)]
    fn lifecycle_events(&self, kind: &str) -> Vec<u32> {
        std::fs::read_to_string(self.lifecycle_log())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(kind)?.trim().parse().ok())
            .collect()
    }

    fn lsp(&self, args: &[&str]) -> std::process::Output {
        self.command(&self.root())
            .env("CLAUDE_PROJECT_DIR", self.root())
            .arg("--config")
            .arg(self.config())
            .arg("lsp")
            .args(args)
            .output()
            .unwrap()
    }
```

Add at module level:

```rust
#[cfg(unix)]
const LIFECYCLE_LSP: &str = r#"import json, os, sys

log = open(sys.argv[1], "a", buffering=1)
log.write(f"start {os.getpid()}\n")

def receive():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode().split(":", 1)
        if key.lower() == "content-length":
            length = int(value.strip())
    return json.loads(sys.stdin.buffer.read(length))

def send(message):
    body = json.dumps({"jsonrpc": "2.0", **message}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()

while True:
    message = receive()
    if message is None:
        break
    method = message.get("method")
    if method == "initialize":
        send({"id": message["id"], "result": {"capabilities": {"documentSymbolProvider": True}}})
    elif method == "exit":
        log.write(f"exit {os.getpid()}\n")
        break
    elif "id" in message:
        send({"id": message["id"], "result": None})
"#;

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
```

Add to `impl Frontend`:

```rust
    #[cfg(unix)]
    fn symbols(&mut self, path: &Path) -> Value {
        self.request(
            "tools/call",
            &json!({"name": "get_document_symbols", "arguments": {"file_path": path}}),
        )
    }
```

The tests (`failure_text`, `assert_tool_success`, and `Project::wait_for` already exist in this file):

```rust
/// A stopped server stays down through a tool call, and start and
/// restart each launch one fresh process and retire the one before.
#[cfg(unix)]
#[test]
fn lsp_commands_stop_start_and_restart_a_server() {
    let project = Project::new(60_000).with_lifecycle_server();
    let mut frontend = project.frontend();
    project.assert_attaches(&mut frontend);
    project.wait_for("the server to run", |p| {
        stdout(&p.lsp(&["status"])).contains("fake  running")
    });
    let first = project.lifecycle_events("start")[0];

    let stop = project.lsp(&["stop", "fake"]);
    assert!(stop.status.success(), "{}", stdout(&stop));
    assert_eq!(stdout(&stop), "fake  stopped\n");
    project.wait_for("the stopped process to exit", |p| {
        p.lifecycle_events("exit").contains(&first)
    });

    let call = frontend.symbols(&project.root().join("a.fake"));
    assert!(
        failure_text(&call).contains("stopped; run `mcpls lsp start fake`"),
        "{call}"
    );
    assert_eq!(project.lifecycle_events("start").len(), 1);

    let start = project.lsp(&["start", "fake"]);
    assert!(start.status.success(), "{}", stdout(&start));
    assert_eq!(stdout(&start), "fake  running\n");
    assert_eq!(project.lifecycle_events("start").len(), 2);
    let second = project.lifecycle_events("start")[1];

    let restart = project.lsp(&["restart", "fake"]);
    assert!(restart.status.success(), "{}", stdout(&restart));
    assert_eq!(stdout(&restart), "fake  running\n");
    assert_eq!(project.lifecycle_events("start").len(), 3);
    project.wait_for("the replaced process to exit", |p| {
        p.lifecycle_events("exit").contains(&second)
    });
    assert_tool_success(&frontend.symbols(&project.root().join("a.fake")));
}

#[cfg(unix)]
#[test]
fn lsp_commands_name_the_servers_that_apply_when_given_an_unknown_one() {
    let project = Project::new(60_000).with_lifecycle_server();
    let mut frontend = project.frontend();
    project.assert_attaches(&mut frontend);

    let start = project.lsp(&["start", "nope"]);
    assert!(!start.status.success());
    assert_eq!(
        stdout(&start),
        "no language server named nope applies here; these do: fake\n"
    );
}

#[test]
fn lsp_status_with_no_backend_says_where_one_comes_from() {
    let project = Project::new(60_000);
    let status = project.lsp(&["status"]);
    assert!(!status.status.success());
    assert_eq!(
        stdout(&status),
        format!(
            "no backend serves {}; one starts with an agent session\n",
            project.root().display()
        )
    );
}
```

- [ ] **Step 3: Create `lsp.rs` with only its unit tests,** and add `mod lsp;` to `main.rs` so they compile into the binary's test target:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn status(id: &str, state: ServerLifecycle) -> ServerStatus {
        ServerStatus {
            id: id.to_string(),
            state,
        }
    }

    #[test]
    fn test_a_server_still_starting_misses_its_target() {
        let states = vec![status("fake", ServerLifecycle::Starting)];
        assert!(!reached(LspAction::Start, &states));
        assert_eq!(render(&states), "fake  starting\n");
    }

    #[test]
    fn test_success_needs_every_server_at_the_target() {
        assert!(reached(
            LspAction::Stop,
            &[status("a", ServerLifecycle::Stopped), status("b", ServerLifecycle::Stopped)]
        ));
        assert!(!reached(
            LspAction::Restart,
            &[status("a", ServerLifecycle::Running), status("b", ServerLifecycle::Failed)]
        ));
    }

    #[test]
    fn test_no_applicable_servers_is_a_success_that_says_so() {
        assert!(reached(LspAction::Stop, &[]));
        assert_eq!(render(&[]), "no language servers apply here\n");
    }

    #[tokio::test]
    async fn test_settling_gives_up_at_the_ceiling() {
        let started = std::time::Instant::now();
        let states = settle(
            || async { Some(vec![status("fake", ServerLifecycle::Starting)]) },
            vec![status("fake", ServerLifecycle::Starting)],
            Duration::from_millis(300),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(states, vec![status("fake", ServerLifecycle::Starting)]);
    }

    #[tokio::test]
    async fn test_settling_picks_up_the_latest_state() {
        let states = settle(
            || async { Some(vec![status("fake", ServerLifecycle::Running)]) },
            vec![status("fake", ServerLifecycle::Starting)],
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(states, vec![status("fake", ServerLifecycle::Running)]);
    }
}
```

- [ ] **Step 4: Run all new tests and watch them fail**

Run: `cargo nextest run --manifest-path /home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli/Cargo.toml -p mcpls-cli -E 'test(lsp)'`
Expected: compile errors for `LspCommand`, `reached`, `render`, and `settle`.

- [ ] **Step 5: Add the arguments** to `args.rs`. Add the variant to `Command` after `Config`:

```rust
    /// Start, stop, restart, or list the language servers a checkout's
    /// backend runs
    ///
    /// Talks to the running backend. A stopped server stays stopped until
    /// `mcpls lsp start` or `restart`, or until the backend exits. Exits
    /// non-zero when a server misses the state asked for, or no backend
    /// answers.
    Lsp {
        /// What to do
        #[command(subcommand)]
        action: LspCommand,
    },
```

and the types beside `SchemaAction`:

```rust
/// Actions for `mcpls lsp`.
#[derive(Debug, Subcommand)]
pub enum LspCommand {
    /// Print each applicable server and its state
    Status {
        /// Directory whose backend to ask
        ///
        /// Defaults to `$CLAUDE_PROJECT_DIR`, then the working directory.
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
    /// Start servers that are not running
    Start {
        #[command(flatten)]
        targets: LspTargets,
        /// Return without waiting for the servers to finish starting
        #[arg(long)]
        no_wait: bool,
    },
    /// Shut servers down and keep them down
    Stop {
        #[command(flatten)]
        targets: LspTargets,
    },
    /// Replace running servers' processes, starting any that are not running
    Restart {
        #[command(flatten)]
        targets: LspTargets,
        /// Return without waiting for the servers to finish starting
        #[arg(long)]
        no_wait: bool,
    },
}

/// The servers an `mcpls lsp` action applies to.
#[derive(Debug, clap::Args)]
pub struct LspTargets {
    /// Server ids, as `mcpls lsp status` prints them
    #[arg(value_name = "SERVER", required_unless_present = "all", conflicts_with = "all")]
    pub servers: Vec<String>,

    /// Every server that applies to the checkout
    #[arg(long)]
    pub all: bool,

    /// Directory whose backend to ask
    ///
    /// Defaults to `$CLAUDE_PROJECT_DIR`, then the working directory.
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
}
```

Search `crates/mcpls-cli/src/completions.rs` and `crates/mcpls-cli/tests/cli_integration.rs` for a hard-coded list of subcommands (`rg -n '"doctor"' <those files>`), and add `lsp` wherever one exists.

- [ ] **Step 6: Implement `lsp.rs`** above the tests from Step 3:

```rust
//! `mcpls lsp`: read and change the language servers a checkout's
//! backend runs.

use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use mcpls_core::bridge::{LspAction, ServerLifecycle};
use mcpls_core::hooks::protocol::ServerStatus;
use mcpls_core::hooks::{ProbeOutcome, Request, Response, SocketIdentity, probe};

/// Longer than the backend's default hook `op_deadline_ms`, so its own
/// deadline error arrives instead of a local timeout.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What a command prints, and whether it exits 0.
pub struct Outcome {
    /// The report, newline-terminated.
    pub text: String,
    /// Whether every server reached the state asked for.
    pub success: bool,
}

impl Outcome {
    fn failed(text: &str) -> Self {
        Self {
            text: format!("{text}\n"),
            success: false,
        }
    }
}

/// Print every applicable server and its state.
pub async fn status(identity: &SocketIdentity, root: &Path) -> Outcome {
    match ask(identity, root, &Request::Status).await {
        Ok(Response::Status { servers, .. }) => Outcome {
            text: render(&servers),
            success: true,
        },
        Ok(other) => Outcome::failed(&format!("the backend answered unexpectedly: {other:?}")),
        Err(text) => Outcome::failed(&text),
    }
}

/// Apply `action` to `servers`, every applicable server when empty, then
/// wait up to `wait` for started servers to settle.
pub async fn control(
    identity: &SocketIdentity,
    root: &Path,
    action: LspAction,
    servers: Vec<String>,
    wait: Option<Duration>,
) -> Outcome {
    let request = Request::Lsp { action, servers };
    let states = match ask(identity, root, &request).await {
        Ok(Response::Lsp { servers }) => servers,
        Ok(Response::Error { message }) => return Outcome::failed(&message),
        Ok(other) => {
            return Outcome::failed(&format!("the backend answered unexpectedly: {other:?}"));
        }
        Err(text) => return Outcome::failed(&text),
    };
    let states = match wait {
        Some(ceiling) => {
            let current = move || async move {
                match ask(identity, root, &Request::Status).await {
                    Ok(Response::Status { servers, .. }) => Some(servers),
                    _ => None,
                }
            };
            settle(current, states, ceiling).await
        }
        None => states,
    };
    Outcome {
        success: reached(action, &states),
        text: render(&states),
    }
}

async fn ask(identity: &SocketIdentity, root: &Path, request: &Request) -> Result<Response, String> {
    match probe(identity, request, ANSWER_TIMEOUT).await {
        ProbeOutcome::Answered(response) => Ok(response),
        ProbeOutcome::NoOwner => Err(format!(
            "no backend serves {}; one starts with an agent session",
            root.display()
        )),
        ProbeOutcome::Refused(reply) => Err(format!(
            "the backend (mcpls {}) refused this build ({}); run `mcpls doctor`",
            reply.version,
            mcpls_core::backend::VERSION
        )),
        ProbeOutcome::Busy => Err(format!(
            "the backend did not answer within {}s",
            ANSWER_TIMEOUT.as_secs()
        )),
        ProbeOutcome::Unintelligible(error) => {
            Err(format!("could not read the backend's answer: {error}"))
        }
    }
}

/// Poll until no server in `states` is `starting`, `ceiling` passes, or
/// the backend stops answering.
async fn settle<F, Fut>(
    mut current: F,
    mut states: Vec<ServerStatus>,
    ceiling: Duration,
) -> Vec<ServerStatus>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<Vec<ServerStatus>>>,
{
    let deadline = Instant::now() + ceiling;
    while states.iter().any(|s| s.state == ServerLifecycle::Starting) && Instant::now() < deadline {
        tokio::time::sleep(POLL_INTERVAL).await;
        let Some(latest) = current().await else {
            break;
        };
        for state in &mut states {
            if let Some(found) = latest.iter().find(|latest| latest.id == state.id) {
                state.state = found.state;
            }
        }
    }
    states
}

fn reached(action: LspAction, states: &[ServerStatus]) -> bool {
    let target = match action {
        LspAction::Stop => ServerLifecycle::Stopped,
        LspAction::Start | LspAction::Restart => ServerLifecycle::Running,
    };
    states.iter().all(|s| s.state == target)
}

fn render(states: &[ServerStatus]) -> String {
    if states.is_empty() {
        return "no language servers apply here\n".to_string();
    }
    states
        .iter()
        .map(|s| format!("{}  {}\n", s.id, s.state))
        .collect()
}
```

- [ ] **Step 7: Dispatch from `main.rs`.** Add `use args::LspCommand;` and `use mcpls_core::bridge::LspAction;`. Insert after the `Doctor` block, before `Config`:

```rust
    if let Some(Command::Lsp { action }) = &args.command {
        let outcome = run_lsp(&args, action).await;
        write_report(&outcome.text);
        std::process::exit(i32::from(!outcome.success));
    }
```

Add these functions beside `diagnose`:

```rust
/// Run one `mcpls lsp` action against the backend for its directory.
async fn run_lsp(args: &Args, action: &LspCommand) -> lsp::Outcome {
    let (dir, change) = match action {
        LspCommand::Status { dir } => (dir.as_deref(), None),
        LspCommand::Start { targets, no_wait } => {
            (targets.dir.as_deref(), Some((targets, LspAction::Start, !no_wait)))
        }
        LspCommand::Restart { targets, no_wait } => {
            (targets.dir.as_deref(), Some((targets, LspAction::Restart, !no_wait)))
        }
        LspCommand::Stop { targets } => (targets.dir.as_deref(), Some((targets, LspAction::Stop, false))),
    };
    let (directory, _) = examined_directory(dir);
    let root = hook::checkout_root(&directory);
    let identity = match mcpls_core::hooks::identity_for(&root) {
        Ok(identity) => identity,
        Err(error) => {
            return lsp::Outcome {
                text: format!("cannot locate the backend for {}: {error}\n", root.display()),
                success: false,
            };
        }
    };
    let Some((targets, lsp_action, wait)) = change else {
        return lsp::status(&identity, &root).await;
    };
    let servers = if targets.all {
        Vec::new()
    } else {
        targets.servers.clone()
    };
    let ceiling = wait.then(|| wait_ceiling(resolve_config(args, &root).ok().as_ref()));
    lsp::control(&identity, &root, lsp_action, servers, ceiling).await
}

/// The longest any configured server may take to finish `initialize`.
fn wait_ceiling(resolved: Option<&mcpls_core::Resolved>) -> std::time::Duration {
    let seconds = resolved
        .and_then(|resolved| {
            resolved
                .config
                .lsp_servers
                .iter()
                .map(|server| server.timeout_seconds)
                .max()
        })
        .unwrap_or(30);
    std::time::Duration::from_secs(seconds)
}
```

If `timeout_seconds` isn't `u64`, convert with `u64::from`.

- [ ] **Step 8: Run the tests and see them pass**

Run: `cargo nextest run --manifest-path /home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli/Cargo.toml -p mcpls-cli -E 'test(lsp) | test(settling) | test(reached) | test(no_applicable)'`
Expected: PASS.

- [ ] **Step 9: Verify and commit**

Run: `devrun -C /home/lev/Git/lev/mcpls_worktrees/42-lsp-control-cli task verify`
Expected: all green.

Commit with subject `feat(cli): add mcpls lsp to control servers` and files `crates/mcpls-cli/src/args.rs crates/mcpls-cli/src/lsp.rs crates/mcpls-cli/src/main.rs crates/mcpls-cli/tests/backend.rs`, plus `completions.rs` or `cli_integration.rs` if Step 5 touched them.

---

### Task 5: Documentation

**Files:**
- Modify: `plugin/skills/setup-mcpls/references/cli.md` (beside the `mcpls doctor` entry, line 27)
- Modify: `plugin/skills/setup-mcpls/references/troubleshooting.md`
- Modify: `plugin/skills/mcpls/SKILL.md`
- Modify: `docs/superpowers/specs/2026-09-21-lsp-control-cli-design.md` (Testing section)

**Interfaces:** none.

- [ ] **Step 1: `cli.md`.** Add after the `mcpls doctor` bullet:

```markdown
- `mcpls lsp status` lists each language server that applies to the checkout and its state: `idle`, `starting`, `running`, `not installed`, `failed`, or `stopped`.
- `mcpls lsp start|stop|restart <SERVER>... | --all` changes them in the running backend without disturbing attached sessions. `stop` holds until `start` or `restart`, or until the backend exits; a tool call on a stopped server is refused rather than starting it. `start` also retries a server recorded as not installed. `start` and `restart` wait for the server to finish starting unless given `--no-wait`. Each takes `--dir DIR`, defaulting like `mcpls doctor`. Exits non-zero when a server misses the state asked for, a name does not apply, or no backend answers.
```

- [ ] **Step 2: `troubleshooting.md`.** Read the file and find the section about a server that answers slowly, wrongly, or not at all. Add this paragraph there, or, if no such section exists, add it under a new heading `## A language server is wedged`:

```markdown
Restart the one server with `mcpls lsp restart <SERVER>`, taking the name from `mcpls lsp status`. The backend replaces the server's process and re-opens the documents it held, and every attached session keeps running. Restart the backend only when that does not help.
```

- [ ] **Step 3: `plugin/skills/mcpls/SKILL.md`.** Read it and find where it covers errors a tool call can return. Add this entry, matching the file's list and code formatting:

```markdown
- `stopped; run \`mcpls lsp start <id>\``: the user stopped that server. If the task needs it, run the command the error names in the shell.
```

If the file has no errors section, add the line under the nearest heading about failures or troubleshooting.

- [ ] **Step 4: Update the spec.** In the spec's Testing section, replace "with a backend serving the `notification_generations.py` fixture" with "with a backend serving a small Python language server that logs each launch and each clean exit", and replace "and the previous process is gone" with "and the previous process exits through the LSP shutdown handshake". In the stop-during-a-spawn bullet, replace "using the fixture's startup delay to hold the spawn open" with "using a fixture whose `initialize` answer is delayed to hold the spawn open".

- [ ] **Step 5: Commit**

Commit with subject `docs(plugin): document mcpls lsp` and files `plugin/skills/setup-mcpls/references/cli.md plugin/skills/setup-mcpls/references/troubleshooting.md plugin/skills/mcpls/SKILL.md docs/superpowers/specs/2026-09-21-lsp-control-cli-design.md`.
