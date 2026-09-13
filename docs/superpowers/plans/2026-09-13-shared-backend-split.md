# Shared backend split implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split mcpls into a thin stdio frontend and one detached backend per checkout root, so every session in a project shares one set of language servers.

**Architecture:** The binary keeps one role per invocation. `mcpls` with no subcommand is the frontend: it resolves the checkout root, connects to the project's existing socket or named pipe, opens with a frozen handshake line, and relays newline-delimited JSON-RPC between the host and the backend, answering from the frozen tool surface when no backend is attached. `mcpls backend --root <dir>` is the backend: it binds the endpoint, owns the language servers, document state, diagnostics cache and every connection's delivery record, serves many MCP connections and the short-lived hook connections, and exits after an idle timer. `--no-backend` keeps today's single in-process server. The owner, passive, demotion and forwarding paths are deleted, because one process now holds every record.

**Tech Stack:** Rust 2024, tokio, rmcp 3.1.4, serde and serde_json, fs4 for file locks, `dunce` for canonicalization, `cargo nextest` through devkit.

**Spec:** `docs/superpowers/specs/2026-09-12-shared-backend-design.md`, sections "Shape" through "Resource subscriptions become per connection", "Stages" (Stage 1 only) and "Verification". The prerequisites already on `main` are described in `docs/superpowers/plans/2026-09-12-shared-backend-prerequisites.md`.

## Global Constraints

- All tasks land as one PR, in order. Each task still ends green and commits on its own.
- The workspace sets `unsafe_code = "deny"`. No `pre_exec`, no `libc`, no `std::env::set_var`. Every function that reads the environment splits into a reader and a pure inner function taking the value.
- Canonicalize through `dunce::canonicalize`, never `Path::canonicalize`.
- Clippy runs with `-D warnings` over `all`, `pedantic`, `nursery`, `unwrap_used` and `expect_used`. Tests carry `#[allow(clippy::unwrap_used, clippy::expect_used)]` on their module, as the existing test modules do.
- This fork still merges from upstream `bug-ops/mcpls`. Do not reformat or restructure upstream code you are not changing. `crates/mcpls-core/src/hooks/` and the new `crates/mcpls-core/src/backend/` do not exist upstream.
- Comments are timeless: no reference to this plan, no issue numbers, no "now", "used to" or "previously", no TDD narration. Default to no comment; doc comments on public items stay.
- No em dashes in code comments, docs or commit messages.
- Commits follow Conventional Commits, subject at most 50 characters including the prefix, imperative, lowercase after the colon, no trailing period, body wrapped at 72. Every commit ends with `Co-Authored-By: <model> <address>` per `AGENTS.md`. Commits are GPG signed; if signing fails, stop and report.
- Tests run through devkit only. Direct `cargo nextest` and `cargo test` are blocked by a harness hook. `devrun task check` compiles every target, `devrun task test` runs the whole suite (there is no filter), `devrun task test-e2e` runs the ignored end-to-end suite (added in Task 0), and `devrun task verify` runs formatting, clippy, tests and doctests. Run `devrun task verify` before every commit.
- `devrun task verify` runs on Linux and cannot see a Windows-only compile failure. Check every `#[cfg(windows)]` block against what it calls by reading it.
- A CLI test that spawns `mcpls` isolates the child's runtime directory with `env_remove("XDG_RUNTIME_DIR")`, `env("TMPDIR", <temp>)` and `env("USER", <unique name>)`, and calls `clear_ambient_env` first (`crates/mcpls-cli/tests/cli_integration.rs:27-32`). On Windows the runtime directory is a known folder no environment variable redirects, so such a test relies on a unique temporary checkout root, whose hash is its own, and removes the `{hash}.*` files it leaves there.
- A process that faces the host (the frontend, `--no-backend`) writes nothing to stdout except JSON-RPC frames. Logs go to stderr.

## Decisions this plan makes

Each is a ruling where the spec is silent or cannot be followed literally. The task that implements it repeats what it needs.

1. **`process_group(0)` instead of `setsid`.** `setsid` needs `pre_exec`, which is `unsafe`, and a child calling `setsid` itself fails because a process spawned with its own group is already a group leader. What the spec needs from `setsid` is leaving the group Codex signals, and `std::os::unix::process::CommandExt::process_group(0)` gives exactly that without `unsafe`. On Windows the spawn uses `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, and only a hook ever spawns there.
2. **The frontend replays the host's `initialize` to a backend that attaches late.** On Windows the backend appears when a hook fires, after the host has already initialized. The frontend answers `initialize` from the frozen surface, keeps the request line, and sends it (under a frontend-owned id whose answer it drops) plus `notifications/initialized` to the backend once one attaches. While it waits, a tool call fails at once with the waiting reason, because the spec reports a wait the way it reports any unreachable backend.
3. **The ownership lock stops being watched.** `LockLoss`, `ServeExit::LockLost` and the 200 ms ownership check go. The spec says a backend that loses its lock or socket keeps serving and exits on the idle timer, and nothing else consumed the check.
4. **Handshake wire format.** One JSON object per line, written without buffering past its newline and read one byte at a time, so neither side ever consumes bytes of the protocol that follows. Frozen fields: request `mcpls` (protocol number), `version`, `kind` (`mcp`, `hook`, `shutdown`); reply `mcpls`, `version`, `pid`, `sessions`, `refusal`. Every other field is optional with a default, and no reader rejects an unknown field or an unknown refusal reason.
5. **Trust conflict rule.** Two sides conflict exactly when one loaded a project-local `mcpls.toml` and the other ignored one for want of trust. An explicit `--config`, the global file and built-in defaults conflict with nothing. A differing fingerprint is served and reported.
6. **Build mismatch is refused on every connection kind except `shutdown`.** The reply still names the backend's version, pid and session count, which is what the frontend decides eviction on and what `mcpls hook doctor` prints.
7. **The doctor's pid label is `backend pid:`.** After Task 4 there is no owner, only the process holding the endpoint, which is a backend or an in-process `--no-backend` server. Task 5 renames the label everywhere it is printed or asserted, and every later doctor line is added after it.
8. **Windows gets a runtime directory too, under `%LOCALAPPDATA%`.** On Windows `runtime_dir()` is `dirs::data_local_dir()` joined with `mcpls`, falling back to `%TEMP%\mcpls-<user>` only when that folder is unknown, and `SocketIdentity::lock` becomes a real path there (unused for ownership). `dirs` resolves the folder through `SHGetKnownFolderPath`, which ignores environment overrides, so a frontend and a hook launched by a harness with a different `%TEMP%` agree on the path, and the profile folder's ACL is already per-user. Unix stays under `TMPDIR`. The spawn lock, the backend log and the start request live beside the lock on both platforms as `{hash}.spawn.lock`, `{hash}.log` and `{hash}.start`. Windows process tests cannot redirect the folder, so they rely on unique temporary checkout roots and remove their `{hash}.*` files.
9. **HTTP sessions get a record each.** Every HTTP session is a fresh connection. A closed HTTP session's subscriptions are pruned the first time a notify to it fails, since rmcp offers no close callback there.
10. **`--no-backend` binds the endpoint for hooks only** and refuses `mcp` handshakes, because its lifetime is one host session's.
11. **A backend configured with `diagnostics.hooks.enabled = false` refuses hook handshakes** with a named refusal, so the doctor explains it.
12. **No reconnect after the backend dies mid-session, except in the idle-exit race.** The spec's verification asks the frontend to report the failure, and a silent reattach would hide it. The exception is a backend that starts exiting just as a frontend attaches. A backend that has stopped accepting reads a handshake and hangs up without replying, so the frontend's handshake sees no reply and `attach` takes the spawn lock and starts a fresh backend, as the spec requires of a frontend arriving during the drain. A backend that replied and then closes before sending the frontend any line counts as a failed attach: the relay reruns `attach` once and resends what the host sent since.
13. **`--trust-project-config` trusts the checkout root's `mcpls.toml`.** Discovery runs at the root, so the flag in a session started in a subdirectory trusts the checkout's file, which such a session did not read before. Accepted, because root discovery is what lets every session in a checkout share one backend.

## File structure

| File | Responsibility after this plan |
|---|---|
| `devkit.toml` | Adds the `test-e2e` task. |
| `crates/mcpls-core/src/bridge/delivery.rs` | `SessionId` and `ConnectionId`: who a record belongs to. No environment reads except `SessionId::from_host_env`. |
| `crates/mcpls-core/src/bridge/resources.rs` | URI codec, and `ResourceSubscriptions` as a map from connection to peer and URIs. |
| `crates/mcpls-core/src/config/mod.rs` | Config discovery at the checkout root, `ConfigSource`, `ServerConfig::fingerprint`, `BackendConfig`. |
| `crates/mcpls-core/src/mcp/server.rs` | `McplsServer` carries its connection, session and instruction notes. No hook role. |
| `crates/mcpls-core/src/lib.rs` | `Runtime`: everything one process serves from. `serve_with` runs it in-process. |
| `crates/mcpls-core/src/hooks/service.rs` | The hook request handler and its counters. No roles. |
| `crates/mcpls-core/src/hooks/listener.rs` | Binding the endpoint, accepting streams, the hook line protocol, client connections that handshake first. |
| `crates/mcpls-core/src/hooks/identity.rs` | Adds sibling paths for the spawn lock, log and start request; Windows runtime directory. |
| `crates/mcpls-core/src/backend/mod.rs` | Module root and re-exports. |
| `crates/mcpls-core/src/backend/handshake.rs` | The frozen handshake line and the rules over it. |
| `crates/mcpls-core/src/backend/endpoint.rs` | The backend's accept loop: dispatch by kind, attachments, idle timer, exit ordering. |
| `crates/mcpls-core/src/backend/spawn.rs` | Launch arguments, spawn lock, detached spawn, Windows start requests. |
| `crates/mcpls-core/src/backend/stub.rs` | Answers from the frozen tool surface when no backend is attached. |
| `crates/mcpls-core/src/backend/frontend.rs` | Attaching, eviction, and the relay state machine. |
| `crates/mcpls-cli/src/args.rs` | `--no-backend`, hidden `backend --root`. |
| `crates/mcpls-cli/src/main.rs` | Chooses frontend, backend or in-process. Windows hooks honour start requests. |
| `crates/mcpls-cli/src/hook.rs` | Doctor reports backend fields and refusals. |
| `crates/mcpls-cli/tests/backend.rs` | Multi-process lifecycle tests. |
| `crates/mcpls-core/tests/e2e/protocol_tests.rs` | Shared-backend diagnostics records end to end. |

---

### Task 0: run the ignored end-to-end suite locally

**Files:**
- Modify: `devkit.toml` (after `[tasks.test-doc]`)

**Interfaces:**
- Produces: `devrun task test-e2e`, which later tasks use to run `#[ignore = "Requires mcpls binary built"]` tests.

CI runs these tests with `--run-ignored ignored-only -E 'kind(test) and test(e2e)'` against a built binary (`.github/workflows/ci.yml:248-249`). Locally there is no way to run them through devkit.

- [ ] **Step 1: Add the task**

Insert after the `[tasks.test-doc]` table:

```toml
[tasks.build-bin]
description = "Build the mcpls binary the end-to-end suite drives"
run = ["cargo", "build", "--bin", "mcpls", "--locked"]
guard = true

[tasks.test-e2e-run]
description = "Run the ignored end-to-end tests against the built binary"
run = [
    "cargo",
    "nextest",
    "run",
    "--workspace",
    "--all-features",
    "--locked",
    "--run-ignored",
    "ignored-only",
    "-E",
    "kind(test) and test(e2e)",
]
guard = true

[tasks.test-e2e]
description = "Build mcpls and run the ignored end-to-end tests"
steps = [{ task = "build-bin" }, { task = "test-e2e-run" }]
```

- [ ] **Step 2: Check it resolves**

Run: `devrun task test-e2e --dry-run`
Expected: two steps printed, `cargo build --bin mcpls --locked` then the nextest argv above.

- [ ] **Step 3: Commit**

```bash
git add devkit.toml
git commit -m "build: add a local end-to-end test task"
```

---

### Task 1: a record per connection, not per process

**Files:**
- Modify: `crates/mcpls-core/src/bridge/delivery.rs:16-61` (identity types) and its tests at `:425-450`
- Modify: `crates/mcpls-core/src/bridge/mod.rs:17-19` (export `ConnectionId`)
- Modify: `crates/mcpls-core/src/mcp/server.rs:43-49` (struct), `:571-584` (`from_context`), `:1117`, `:1225`, `:1272`, `:1400` (session reads), `:1741-1773` (`get_info`)
- Modify: `crates/mcpls-core/src/transport.rs:323-351` (`run_stdio`), `:448-456` (HTTP factory)
- Test: `crates/mcpls-core/src/mcp/server.rs`, existing `mod tests`

**Interfaces:**
- Produces:
  - `pub struct ConnectionId(u64)` with `ConnectionId::next() -> Self`, `Copy`, `Ord`, `Hash`, `Display` as `connection-{n}`.
  - `SessionId::named(value: Option<String>) -> Option<SessionId>`: `None` for absent or empty.
  - `SessionId::for_connection(connection: ConnectionId) -> SessionId`.
  - `SessionId::from_host_env() -> Option<SessionId>`: reads `CLAUDE_CODE_SESSION_ID`.
  - `McplsServer::for_connection(&self, session: Option<SessionId>) -> McplsServer`: same context, fresh `ConnectionId`, the named session or the connection's own.
  - `McplsServer::with_notes(self, notes: Vec<String>) -> McplsServer`: sentences appended to `get_info` instructions.
  - `McplsServer::connection(&self) -> ConnectionId`, `McplsServer::session(&self) -> &SessionId` (both `pub(crate)`).
- Removes: `SessionId::from_env_or_process`, `SessionId::from_env_value`, `PROCESS_DEFAULT_SESSION`.

The spec's Stage 1 requires this: a backend inherits the environment of whichever frontend spawned it, so a session read from the process environment hands every session the spawner's record.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/mcpls-core/src/mcp/server.rs`:

```rust
    /// A server over one context, with an adopted empty baseline and one
    /// error cached, so a flush has something to report.
    async fn server_with_one_error() -> McplsServer {
        let (delivery, floors) = default_delivery_and_floors();
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let uri: lsp_types::Uri = if cfg!(windows) {
            "file:///C:/workspace/broken.rs".parse().unwrap()
        } else {
            "file:///workspace/broken.rs".parse().unwrap()
        };
        cache.lock().await.store_diagnostics(
            &ServerId::from("rust"),
            &uri,
            Some(1),
            vec![lsp_types::Diagnostic {
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "broken".to_string(),
                ..lsp_types::Diagnostic::default()
            }],
        );
        delivery.lock().await.set_baseline(HashMap::new());
        McplsServer::new(
            Arc::new(Translator::new()),
            cache,
            Arc::from(Vec::new()),
            Arc::new(ResourceSubscriptions::new()),
            false,
            delivery,
            floors,
            crate::config::DiagnosticsConfig::default(),
            test_settle(),
        )
    }

    /// Two connections whose hosts named no session each read their own
    /// record. A backend serves every session from one process, so a
    /// process-wide fallback would let one session consume another's
    /// report.
    #[tokio::test]
    async fn test_anonymous_connections_read_their_own_records() {
        let server = server_with_one_error().await;
        let first = server.for_connection(None);
        let second = server.for_connection(None);

        assert!(first.get_new_diagnostics().await.unwrap().contains("broken.rs"));
        assert!(
            second.get_new_diagnostics().await.unwrap().contains("broken.rs"),
            "the first connection's flush consumed the second's report"
        );
        assert!(!first.get_new_diagnostics().await.unwrap().contains("broken.rs"));
    }

    /// Two connections naming one session share its record, which is how a
    /// hook and the agent's own tool call agree on what was delivered.
    #[tokio::test]
    async fn test_connections_naming_one_session_share_its_record() {
        let server = server_with_one_error().await;
        let session = || SessionId::named(Some("s1".to_string()));
        let first = server.for_connection(session());
        let second = server.for_connection(session());

        assert!(first.get_new_diagnostics().await.unwrap().contains("broken.rs"));
        assert!(!second.get_new_diagnostics().await.unwrap().contains("broken.rs"));
    }

    #[test]
    fn test_notes_reach_the_instructions() {
        let (delivery, floors) = default_delivery_and_floors();
        let server = McplsServer::new(
            Arc::new(Translator::new()),
            Arc::new(Mutex::new(NotificationCache::new())),
            Arc::from(Vec::new()),
            Arc::new(ResourceSubscriptions::new()),
            false,
            delivery,
            floors,
            crate::config::DiagnosticsConfig::default(),
            test_settle(),
        )
        .with_notes(vec!["NOTE: first.".to_string(), "NOTE: second.".to_string()]);

        let instructions = server.get_info().instructions.unwrap();
        assert!(instructions.ends_with(" NOTE: first. NOTE: second."), "{instructions}");
    }
```

`test_settle`, `default_delivery_and_floors`, `HashMap`, `ServerId`, `Translator` and `NotificationCache` are already in scope in that module (`rg -n "fn test_settle" crates/mcpls-core/src/mcp/server.rs`). If `test_settle` is defined inside a nested module, call it by that path.

Replace the two tests at `crates/mcpls-core/src/bridge/delivery.rs:425-450` (`test_an_exported_session_id_names_the_record`, `test_an_absent_or_empty_session_id_reuses_the_process_token`) with:

```rust
    #[test]
    fn test_a_named_session_needs_a_nonempty_value() {
        assert_eq!(
            SessionId::named(Some("abc-123".to_string())),
            Some(SessionId::from("abc-123".to_string()))
        );
        assert_eq!(SessionId::named(Some(String::new())), None);
        assert_eq!(SessionId::named(None), None);
    }

    #[test]
    fn test_each_connection_gets_its_own_fallback_session() {
        let first = ConnectionId::next();
        let second = ConnectionId::next();
        assert_ne!(first, second);
        assert_ne!(
            SessionId::for_connection(first),
            SessionId::for_connection(second)
        );
    }
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `devrun task check`
Expected: FAIL to compile: `no method named for_connection`, `no function named named`, `cannot find type ConnectionId`, `no method named with_notes`.

- [ ] **Step 3: Implement the identity types**

In `crates/mcpls-core/src/bridge/delivery.rs`, delete `PROCESS_DEFAULT_SESSION` (line 17), `from_env_or_process` and `from_env_value`, drop the now-unused `OnceLock`, `RandomState` and `BuildHasher` imports if nothing else uses them (`rg -n "RandomState|OnceLock|hash_one" crates/mcpls-core/src/bridge/delivery.rs`), and replace the `SessionId` doc comment and `impl SessionId` with:

```rust
/// Identity of one client session.
///
/// A host that names its session names it for every connection it opens,
/// so a hook and the agent's own tool call read one record. A connection
/// whose host names nothing reads a record of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl SessionId {
    /// The session a host named, or `None` when the value is absent or
    /// empty.
    #[must_use]
    pub fn named(value: Option<String>) -> Option<Self> {
        value.filter(|id| !id.is_empty()).map(Self)
    }

    /// The record a connection whose host named no session reads.
    #[must_use]
    pub fn for_connection(connection: ConnectionId) -> Self {
        Self(connection.to_string())
    }

    /// The session Claude Code exported to this process.
    ///
    /// Only a process facing the host reads this. A backend serves every
    /// session and learns each one from its connection's handshake.
    #[must_use]
    pub fn from_host_env() -> Option<Self> {
        Self::named(std::env::var("CLAUDE_CODE_SESSION_ID").ok())
    }
}

/// One MCP connection to this process, numbered in the order this process
/// created them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// A number no other connection in this process has.
    #[must_use]
    pub fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "connection-{}", self.0)
    }
}
```

Keep the existing `impl std::fmt::Display for SessionId`.

In `crates/mcpls-core/src/bridge/mod.rs:17-19`, add `ConnectionId` to the `pub use delivery::{...}` list.

- [ ] **Step 4: Carry the connection on the server**

In `crates/mcpls-core/src/mcp/server.rs`, change the struct at line 45:

```rust
#[derive(Clone)]
pub struct McplsServer {
    context: Arc<BridgeContext>,
    connection: ConnectionId,
    session: SessionId,
    /// Sentences appended to this connection's instructions.
    notes: Arc<[String]>,
    #[cfg(test)]
    footer_pause: Arc<std::sync::Mutex<Option<FooterPause>>>,
}
```

Import `ConnectionId` from `crate::bridge` beside `SessionId`. Replace `from_context` and add the three methods after it:

```rust
    #[allow(clippy::missing_const_for_fn)]
    pub(crate) fn from_context(context: Arc<BridgeContext>) -> Self {
        let connection = ConnectionId::next();
        Self {
            context,
            connection,
            session: SessionId::for_connection(connection),
            notes: Arc::from(Vec::new()),
            #[cfg(test)]
            footer_pause: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// This server answering a new connection: the same shared state, a
    /// fresh connection id, and `session` when the host named one or the
    /// connection's own record when it did not.
    #[must_use]
    pub(crate) fn for_connection(&self, session: Option<SessionId>) -> Self {
        let connection = ConnectionId::next();
        Self {
            context: Arc::clone(&self.context),
            connection,
            session: session.unwrap_or_else(|| SessionId::for_connection(connection)),
            notes: Arc::clone(&self.notes),
            #[cfg(test)]
            footer_pause: Arc::clone(&self.footer_pause),
        }
    }

    /// This server with `notes` appended to its instructions.
    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with_notes(mut self, notes: Vec<String>) -> Self {
        self.notes = Arc::from(notes);
        self
    }

    pub(crate) const fn session(&self) -> &SessionId {
        &self.session
    }
```

`connection()` is added in Task 2, its first reader. If clippy reports the `connection` field unused, add `#[allow(dead_code)]` on the field with no comment, and remove the allow in Task 2.

Replace the four `SessionId::from_env_or_process()` reads:
- line 1117: `let session = self.session.clone();`
- line 1225: `session: self.session.to_string(),`
- line 1272: `session: self.session.to_string(),`
- line 1400: `let session = self.session.clone();`

In `get_info`, after the `project_config_ignored` block and before `server_info.instructions = Some(instructions);`:

```rust
        for note in self.notes.iter() {
            instructions.push(' ');
            instructions.push_str(note);
        }
```

- [ ] **Step 5: Name the session where the host connects**

In `crates/mcpls-core/src/transport.rs`, `run_stdio` line 329:

```rust
        result = mcp_server.for_connection(SessionId::from_host_env()).serve(rmcp::transport::stdio()) => {
```

with `use crate::bridge::SessionId;` inside the function or at the top of the file's `use` block. In `serve_http_on`, line 455:

```rust
        move || Ok::<_, std::io::Error>(mcp_for_factory.for_connection(None)),
```

- [ ] **Step 6: Run the tests**

Run: `devrun task test`
Expected: PASS, including the four new tests. If a `hooks/service.rs` passive test fails because its two in-process servers no longer share a process-wide session, that test is deleted in Task 4; make it pass here by giving both servers one named session through `for_connection(SessionId::named(Some("s1".into())))` rather than changing assertions.

- [ ] **Step 7: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates/mcpls-core/src/bridge/delivery.rs crates/mcpls-core/src/bridge/mod.rs crates/mcpls-core/src/mcp/server.rs crates/mcpls-core/src/transport.rs
git commit -m "refactor(mcp): key records on the connection"
```

---

### Task 2: resource subscriptions per connection

**Files:**
- Modify: `crates/mcpls-core/src/bridge/resources.rs:118-178` (`ResourceSubscriptions`) and its tests at `:300-363`
- Modify: `crates/mcpls-core/src/lib.rs:119-139` (`PumpShared`), `:252-277` (notify), `:733-736`, `:834-843`, `:924`
- Modify: `crates/mcpls-core/src/transport.rs:295-351` (`run_stdio` loses its peer cell)
- Modify: `crates/mcpls-core/src/mcp/server.rs:1663-1739` (`subscribe`, `unsubscribe`), tests at `:5048-5075`
- Modify: `crates/mcpls-core/src/mcp/handlers.rs:27-29,45-46` (doc comments)
- Modify: `crates/mcpls-core/src/notification_lifecycle.rs:163-168`, `crates/mcpls-core/src/recovery_tests.rs:414-431,468,839-864`, and every `pump_tests` site in `crates/mcpls-core/src/lib.rs` that builds a `PumpShared` (`rg -n "peer_cell" crates/mcpls-core/src`)
- Test: `crates/mcpls-core/src/lib.rs` `mod pump_tests`, `crates/mcpls-core/src/bridge/resources.rs` `mod tests`

**Interfaces:**
- Consumes: `ConnectionId` (Task 1).
- Produces:
  - `ResourceSubscriptions::subscribe(&self, connection: ConnectionId, peer: rmcp::Peer<rmcp::RoleServer>, uri: String) -> Result<bool, String>`: cap of `MAX_SUBSCRIPTIONS` per connection.
  - `ResourceSubscriptions::unsubscribe(&self, connection: ConnectionId, uri: &str) -> bool`
  - `ResourceSubscriptions::subscribers(&self, uri: &str) -> Vec<(ConnectionId, rmcp::Peer<rmcp::RoleServer>)>`
  - `ResourceSubscriptions::remove_connection(&self, connection: ConnectionId)`
  - `ResourceSubscriptions::contains(&self, connection: ConnectionId, uri: &str) -> bool`
  - `ResourceSubscriptions::is_empty(&self) -> bool` (unchanged)
  - `McplsServer::connection(&self) -> ConnectionId` (`pub(crate)`)
  - `McplsServer::subscriptions(&self) -> &Arc<ResourceSubscriptions>` (`pub(crate)`)
  - `run_stdio(mcp_server, shutdown_signal)`: no peer cell parameter.
- Removes: `PumpShared::peer_cell`, `ResourceSubscriptions::snapshot`.

- [ ] **Step 1: Write the failing pump test**

Add to `mod pump_tests` in `crates/mcpls-core/src/lib.rs`, beside `broken_peer`:

```rust
        /// A live peer and the client end of its connection, read line by
        /// line. The service skips the MCP handshake, so it can be notified
        /// at once.
        fn live_peer() -> (
            rmcp::Peer<rmcp::RoleServer>,
            tokio::io::Lines<tokio::io::BufReader<tokio::io::DuplexStream>>,
        ) {
            use tokio::io::AsyncBufReadExt as _;

            let (server_io, client_io) = tokio::io::duplex(64 * 1024);
            let running = rmcp::service::serve_directly(BarePeerHandler, server_io, None);
            let peer = running.peer().clone();
            // Keeps the service alive for the rest of the test.
            std::mem::forget(running);
            (peer, tokio::io::BufReader::new(client_io).lines())
        }

        async fn next_uri(
            lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::io::DuplexStream>>,
            within: Duration,
        ) -> Option<String> {
            let line = tokio::time::timeout(within, lines.next_line())
                .await
                .ok()?
                .ok()??;
            let message: serde_json::Value = serde_json::from_str(&line).ok()?;
            message["params"]["uri"].as_str().map(str::to_string)
        }

        /// Each connection hears about the files it subscribed to and not
        /// about another connection's.
        #[tokio::test]
        async fn test_each_connection_is_notified_only_about_its_own_subscriptions() {
            let subs = make_subs();
            let project = tempfile::tempdir().expect("project dir");
            let root = dunce::canonicalize(project.path()).expect("canonical root");
            let (a_path, b_path) = (root.join("a.rs"), root.join("b.rs"));
            let (a, mut a_lines) = live_peer();
            let (b, mut b_lines) = live_peer();
            let (a_id, b_id) = (bridge::ConnectionId::next(), bridge::ConnectionId::next());
            subs.subscribe(a_id, a, make_uri(&a_path).unwrap()).await.unwrap();
            subs.subscribe(b_id, b, make_uri(&b_path).unwrap()).await.unwrap();

            let (tx, rx) = mpsc::channel(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            tokio::spawn(diagnostics_pump(
                ServerId::from("rust"),
                rx,
                cancel_rx,
                true,
                PumpShared {
                    notification_cache: make_cache(),
                    subs: Arc::clone(&subs),
                    workspace_roots: no_workspace_roots(),
                    document_tracker: make_tracker(),
                    settle: make_settle(),
                    delivery: make_delivery(),
                    floors: make_floors(),
                },
            ));
            tx.send(LspNotification::PublishDiagnostics(PublishDiagnosticsParams {
                uri: bridge::path_to_uri(&a_path).unwrap(),
                diagnostics: vec![],
                version: None,
            }))
            .await
            .unwrap();

            assert_eq!(
                next_uri(&mut a_lines, Duration::from_secs(5)).await,
                Some(make_uri(&a_path).unwrap())
            );
            assert_eq!(
                next_uri(&mut b_lines, Duration::from_millis(200)).await,
                None,
                "a connection was told about a file only another connection subscribed to"
            );
        }

        /// A connection that cannot be notified loses its subscriptions, and
        /// the other connections subscribed to the same file keep hearing
        /// about it.
        #[tokio::test]
        async fn test_a_failed_notify_prunes_only_that_connection() {
            let subs = make_subs();
            let project = tempfile::tempdir().expect("project dir");
            let root = dunce::canonicalize(project.path()).expect("canonical root");
            let path = root.join("shared.rs");
            let uri = make_uri(&path).unwrap();
            let (live, mut live_lines) = live_peer();
            let (live_id, broken_id) = (bridge::ConnectionId::next(), bridge::ConnectionId::next());
            subs.subscribe(live_id, live, uri.clone()).await.unwrap();
            subs.subscribe(broken_id, broken_peer().await, uri.clone()).await.unwrap();

            let (tx, rx) = mpsc::channel(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            tokio::spawn(diagnostics_pump(
                ServerId::from("rust"),
                rx,
                cancel_rx,
                true,
                PumpShared {
                    notification_cache: make_cache(),
                    subs: Arc::clone(&subs),
                    workspace_roots: no_workspace_roots(),
                    document_tracker: make_tracker(),
                    settle: make_settle(),
                    delivery: make_delivery(),
                    floors: make_floors(),
                },
            ));
            for _ in 0..2 {
                tx.send(LspNotification::PublishDiagnostics(PublishDiagnosticsParams {
                    uri: bridge::path_to_uri(&path).unwrap(),
                    diagnostics: vec![],
                    version: None,
                }))
                .await
                .unwrap();
                assert_eq!(
                    next_uri(&mut live_lines, Duration::from_secs(5)).await,
                    Some(uri.clone())
                );
            }
            assert!(!subs.contains(broken_id, &uri).await);
            assert!(subs.contains(live_id, &uri).await);
        }
```

`std::mem::forget` on a `RunningService` leaks its task for the test's duration, which is the intent; if clippy flags `mem_forget`, hold the `RunningService` in a returned tuple instead and bind it to `_running` at the call site.

Adapt `test_pump_keeps_caching_after_a_failed_notify` to the new API: replace the peer cell with `subs.subscribe(bridge::ConnectionId::next(), broken_peer().await, make_uri(&first_path).unwrap())`.

- [ ] **Step 2: See it fail**

Run: `devrun task check`
Expected: FAIL to compile: `subscribe` takes 1 argument, `no field peer_cell`... on `PumpShared` literals missing it, `no method named contains` with two arguments.

- [ ] **Step 3: Replace the subscription set**

In `crates/mcpls-core/src/bridge/resources.rs`, replace the `ResourceSubscriptions` doc, struct, `Default` impl and `impl` (lines 118-178) with:

```rust
/// Which connections want updates for which resource URIs, and the peer
/// each one is notified through.
///
/// One process serves many connections, so a notification for a file goes
/// to the connections subscribed to that file and to no other. The hot read
/// path (the diagnostics pump) takes a read lock, so concurrent readers do
/// not block each other.
#[derive(Default)]
pub struct ResourceSubscriptions(RwLock<HashMap<ConnectionId, Subscriber>>);

struct Subscriber {
    peer: Peer<RoleServer>,
    uris: HashSet<String>,
}

impl std::fmt::Debug for ResourceSubscriptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceSubscriptions").finish_non_exhaustive()
    }
}

impl ResourceSubscriptions {
    /// No subscriptions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `connection`, notified through `peer`, wants updates for
    /// `uri`.
    ///
    /// Returns `Ok(true)` if newly inserted, `Ok(false)` if already present.
    ///
    /// # Errors
    ///
    /// Returns an error string when `connection` already holds
    /// [`MAX_SUBSCRIPTIONS`] URIs.
    pub async fn subscribe(
        &self,
        connection: ConnectionId,
        peer: Peer<RoleServer>,
        uri: String,
    ) -> Result<bool, String> {
        let mut map = self.0.write().await;
        let subscriber = map.entry(connection).or_insert_with(|| Subscriber {
            peer: peer.clone(),
            uris: HashSet::new(),
        });
        subscriber.peer = peer;
        if !subscriber.uris.contains(&uri) && subscriber.uris.len() >= MAX_SUBSCRIPTIONS {
            return Err(format!("subscription limit of {MAX_SUBSCRIPTIONS} reached"));
        }
        Ok(subscriber.uris.insert(uri))
    }

    /// Whether no connection is subscribed to anything, so the pump can
    /// skip building a URI.
    pub async fn is_empty(&self) -> bool {
        self.0.read().await.is_empty()
    }

    /// Remove `uri` from `connection`'s subscriptions. Returns `true` if it
    /// was present.
    pub async fn unsubscribe(&self, connection: ConnectionId, uri: &str) -> bool {
        let mut map = self.0.write().await;
        let Some(subscriber) = map.get_mut(&connection) else {
            return false;
        };
        let removed = subscriber.uris.remove(uri);
        if subscriber.uris.is_empty() {
            map.remove(&connection);
        }
        removed
    }

    /// Whether `connection` is subscribed to `uri`.
    pub async fn contains(&self, connection: ConnectionId, uri: &str) -> bool {
        self.0
            .read()
            .await
            .get(&connection)
            .is_some_and(|subscriber| subscriber.uris.contains(uri))
    }

    /// Every connection subscribed to `uri`, with the peer to notify.
    pub async fn subscribers(&self, uri: &str) -> Vec<(ConnectionId, Peer<RoleServer>)> {
        self.0
            .read()
            .await
            .iter()
            .filter(|(_, subscriber)| subscriber.uris.contains(uri))
            .map(|(connection, subscriber)| (*connection, subscriber.peer.clone()))
            .collect()
    }

    /// Forget everything `connection` subscribed to: its service stopped,
    /// or a notification to it failed.
    pub async fn remove_connection(&self, connection: ConnectionId) {
        self.0.write().await.remove(&connection);
    }
}
```

Imports at the top: `use std::collections::{HashMap, HashSet};`, `use rmcp::{Peer, RoleServer};`, `use super::ConnectionId;`.

Replace the `ResourceSubscriptions` tests (lines 300-363) with tests over the same behaviours taking a connection. They need a peer; add this helper to that test module:

```rust
    #[derive(Debug)]
    struct BarePeerHandler;

    impl rmcp::ServerHandler for BarePeerHandler {}

    fn peer() -> Peer<RoleServer> {
        let (server_io, client_io) = tokio::io::duplex(1024);
        let running = rmcp::service::serve_directly(BarePeerHandler, server_io, None);
        let peer = running.peer().clone();
        std::mem::forget((running, client_io));
        peer
    }

    #[tokio::test]
    async fn test_subscribe_and_contains() {
        let subs = ResourceSubscriptions::new();
        let connection = ConnectionId::next();
        let uri = "lsp-diagnostics:///home/user/main.rs".to_string();
        assert!(!subs.contains(connection, &uri).await);
        assert!(subs.subscribe(connection, peer(), uri.clone()).await.unwrap());
        assert!(subs.contains(connection, &uri).await);
        assert!(!subs.contains(ConnectionId::next(), &uri).await);
    }

    #[tokio::test]
    async fn test_subscribe_duplicate_returns_false() {
        let subs = ResourceSubscriptions::new();
        let connection = ConnectionId::next();
        let uri = "lsp-diagnostics:///tmp/file.rs".to_string();
        assert!(subs.subscribe(connection, peer(), uri.clone()).await.unwrap());
        assert!(!subs.subscribe(connection, peer(), uri).await.unwrap());
    }

    #[tokio::test]
    async fn test_unsubscribe_removes_only_that_connections_entry() {
        let subs = ResourceSubscriptions::new();
        let (one, two) = (ConnectionId::next(), ConnectionId::next());
        let uri = "lsp-diagnostics:///tmp/file.rs".to_string();
        subs.subscribe(one, peer(), uri.clone()).await.unwrap();
        subs.subscribe(two, peer(), uri.clone()).await.unwrap();
        assert!(subs.unsubscribe(one, &uri).await);
        assert!(!subs.contains(one, &uri).await);
        assert!(subs.contains(two, &uri).await);
        assert!(!subs.unsubscribe(one, "lsp-diagnostics:///nonexistent.rs").await);
    }

    #[tokio::test]
    async fn test_the_cap_is_per_connection() {
        let subs = ResourceSubscriptions::new();
        let full = ConnectionId::next();
        for i in 0..MAX_SUBSCRIPTIONS {
            subs.subscribe(full, peer(), format!("lsp-diagnostics:///file{i}.rs"))
                .await
                .unwrap();
        }
        assert!(
            subs.subscribe(full, peer(), "lsp-diagnostics:///overflow.rs".to_string())
                .await
                .is_err()
        );
        assert!(
            subs.subscribe(ConnectionId::next(), peer(), "lsp-diagnostics:///overflow.rs".to_string())
                .await
                .is_ok(),
            "one connection's subscriptions must not use up another's"
        );
    }

    #[tokio::test]
    async fn test_subscribers_and_remove_connection() {
        let subs = ResourceSubscriptions::new();
        let (one, two) = (ConnectionId::next(), ConnectionId::next());
        let uri = "lsp-diagnostics:///a.rs".to_string();
        subs.subscribe(one, peer(), uri.clone()).await.unwrap();
        subs.subscribe(two, peer(), uri.clone()).await.unwrap();
        let mut found: Vec<_> = subs.subscribers(&uri).await.into_iter().map(|(id, _)| id).collect();
        found.sort();
        assert_eq!(found, vec![one, two]);
        subs.remove_connection(one).await;
        assert_eq!(subs.subscribers(&uri).await.len(), 1);
        assert!(!subs.is_empty().await);
        subs.remove_connection(two).await;
        assert!(subs.is_empty().await);
    }
```

Delete `test_subscription_cap_enforced_in_handler_context` and `test_unsubscribe_nonexistent_is_noop` from `crates/mcpls-core/src/mcp/server.rs:5048-5075`; the tests above cover both.

- [ ] **Step 4: Notify subscribers from the pump**

In `crates/mcpls-core/src/lib.rs`, delete the `peer_cell` field from `PumpShared` and `OnceCell` from the `tokio::sync` import if unused. Replace lines 252-277 of `handle_publish_diagnostics` with:

```rust
    if shared.subs.is_empty().await {
        return;
    }
    let Some(path) = bridge::uri_to_path(&p.uri) else {
        return;
    };
    let Ok(mcp_uri) = make_uri(&path) else {
        return;
    };
    for (connection, peer) in shared.subs.subscribers(&mcp_uri).await {
        if let Err(error) = peer
            .notify_resource_updated(ResourceUpdatedNotificationParam::new(mcp_uri.clone()))
            .await
        {
            tracing::debug!(%error, %connection, "a subscriber could not be notified and was dropped");
            shared.subs.remove_connection(connection).await;
        }
    }
```

Update the `diagnostics_pump` doc comment's "Phase A / Phase B" paragraph to: "Caches every notification, and notifies each connection subscribed to a published file's resource URI."

In `serve_with_identity`, delete `let peer_cell = ...` (line 736) and `peer_cell: Arc::clone(&peer_cell),` (line 837), and call `run_stdio(mcp_server, shutdown_signal)` at line 924.

In `crates/mcpls-core/src/transport.rs`, remove the `peer_cell` parameter from `run_stdio`, delete lines 338-340, and change `tokio::select! { result = service.waiting() ... }` to remove the connection's subscriptions when its service stops:

```rust
    let connection = service.service().connection();
    let subscriptions = std::sync::Arc::clone(service.service().subscriptions());
    let result = tokio::select! {
        result = service.waiting() => result
            .map(|_| ())
            .map_err(|e| crate::Error::McpServer(format!("MCP server error: {e}"))),
        () = shutdown_signal.recv() => {
            tracing::info!("shutdown signal received, stopping stdio transport");
            Ok(())
        }
    };
    subscriptions.remove_connection(connection).await;
    result
```

`RunningService::service()` returns `&S`. Add `pub(crate) fn subscriptions(&self) -> &Arc<ResourceSubscriptions> { &self.context.subscriptions }` to `McplsServer` for this. Update `run_stdio`'s doc comment to drop the sentence about populating `peer_cell`, and the test at `transport.rs:817-871` to call `run_stdio(server, ShutdownSignal::new())`.

- [ ] **Step 5: Subscribe under the connection**

In `crates/mcpls-core/src/mcp/server.rs`, add after `session()`:

```rust
    pub(crate) const fn connection(&self) -> ConnectionId {
        self.connection
    }
```

and remove any `#[allow(dead_code)]` Task 1 put on the field. In `subscribe` (line 1691):

```rust
        self.context
            .subscriptions
            .subscribe(self.connection, context.peer.clone(), canonical_uri.clone())
            .await
            .map_err(|e| McpError::invalid_params(e, None))?;
```

In `unsubscribe` (line 1737): `self.context.subscriptions.unsubscribe(self.connection, &key).await;`

In `crates/mcpls-core/src/mcp/handlers.rs`, change the struct doc's last paragraph (lines 27-29) to "The MCP peer handle for resource notifications lives in [`ResourceSubscriptions`], keyed by connection." and the `subscriptions` field doc to "Which connections subscribed to which resource URIs."

- [ ] **Step 6: Update the remaining `PumpShared` builders**

Remove every `peer_cell` field and `make_peer_cell`/`PeerCell` helper in `crates/mcpls-core/src/lib.rs` `pump_tests`, `crates/mcpls-core/src/notification_lifecycle.rs` tests and `crates/mcpls-core/src/recovery_tests.rs`. Where a test set a peer (`peer_cell.set(running.peer().clone())`, recovery_tests `:468` and `:864`), replace it with nothing if the test never subscribes, or with `subs.subscribe(bridge::ConnectionId::next(), running.peer().clone(), <the uri it subscribed>)` if it does. Check with `rg -n "peer_cell|PeerCell|snapshot\(\)" crates/mcpls-core/src`: expected no output.

A pump test asserting a notification reaches "the peer" subscribes that peer under one `ConnectionId` and keeps its assertion.

- [ ] **Step 7: Run the tests**

Run: `devrun task test`
Expected: PASS, including `test_each_connection_is_notified_only_about_its_own_subscriptions` and `test_a_failed_notify_prunes_only_that_connection`.

- [ ] **Step 8: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates/mcpls-core/src
git commit -m "feat(resources): subscribe per connection"
```

---

### Task 3: find config at the checkout root, and stamp it

**Files:**
- Modify: `crates/mcpls-core/src/config/mod.rs`: `ServerConfig` (`:310-343`), `RelativeRootBase` (`:846-860`), `load_with_trust` (`:942-1010`), `load_from` (`:1023-1025`), `load_from_with_root_base` (`:1094`), `DEFAULT_CONFIG_TEMPLATE` (`:390-394`), `impl Default for ServerConfig` (`:1273-1283`), tests at `:1436-1466`, `:2158-2242`, `:2358-2510`
- Modify: every other full `ServerConfig { .. }` literal: `rg -n "project_config_ignored: false" crates` (`crates/mcpls-core/src/lib.rs`, `crates/mcpls-core/src/bridge/translator/routing.rs`)
- Test: `crates/mcpls-core/src/config/mod.rs` `mod tests`

**Interfaces:**
- Produces:
  - `pub enum ConfigSource { Explicit, Project, Global, Defaults }`, `Copy`, `Default = Defaults`, serde `snake_case`.
  - `ServerConfig::source: ConfigSource` (`#[serde(skip)]`), beside the existing `project_config_ignored: bool`.
  - `ServerConfig::backend: BackendConfig` with `BackendConfig { idle_shutdown_ms: u64 }`, default `10_000`, TOML table `[backend]`.
  - `ServerConfig::load_at(trust: ProjectConfigTrust, start: &Path) -> Result<ServerConfig>`: discovery from the checkout root enclosing `start`.
  - `ServerConfig::fingerprint(&self) -> String`: 16 hex characters, independent of map iteration order.
- Changes: `ServerConfig::load_with_trust(trust)` delegates to `load_at(trust, &current_dir)`.

The spec moves discovery to the root in Stage 1: a session started in a subdirectory otherwise never reads the checkout's `mcpls.toml`, and the backend runs at the root. The relative-root base of the global config moves with it, so a frontend and the backend it spawns resolve the same roots and compute the same fingerprint.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/mcpls-core/src/config/mod.rs`:

```rust
    fn mark_checkout(dir: &Path) {
        fs::create_dir(dir.join(".git")).unwrap();
        fs::write(dir.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    /// A session started in a subdirectory reads the checkout's own
    /// `mcpls.toml`, which is where the backend it shares reads it.
    #[test]
    fn test_load_at_finds_the_project_config_at_the_checkout_root() {
        let tmp = TempDir::new().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        mark_checkout(&root);
        fs::write(
            root.join("mcpls.toml"),
            "[diagnostics]\nmax_total = 7\n",
        )
        .unwrap();
        let nested = root.join("crates").join("core");
        fs::create_dir_all(&nested).unwrap();

        let trusted = ServerConfig::load_at(ProjectConfigTrust::Trusted, &nested).unwrap();
        assert_eq!(trusted.diagnostics.max_total, 7);
        assert_eq!(trusted.source, ConfigSource::Project);
        assert!(!trusted.project_config_ignored);

        let untrusted = ServerConfig::load_at(ProjectConfigTrust::Untrusted, &nested).unwrap();
        assert_ne!(untrusted.diagnostics.max_total, 7);
        assert!(untrusted.project_config_ignored);
        assert_ne!(untrusted.source, ConfigSource::Project);
    }

    #[test]
    fn test_an_explicit_path_is_stamped_explicit() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("custom.toml");
        fs::write(&path, "").unwrap();
        assert_eq!(ServerConfig::load_from(&path).unwrap().source, ConfigSource::Explicit);
    }

    #[test]
    fn test_the_fingerprint_follows_the_settings() {
        let base = ServerConfig::default();
        assert_eq!(base.fingerprint(), ServerConfig::default().fingerprint());
        assert_eq!(base.fingerprint().len(), 16);

        let mut changed = ServerConfig::default();
        changed.diagnostics.max_total += 1;
        assert_ne!(base.fingerprint(), changed.fingerprint());

        let mut relabelled = ServerConfig::default();
        relabelled.source = ConfigSource::Explicit;
        relabelled.project_config_ignored = true;
        assert_eq!(
            base.fingerprint(),
            relabelled.fingerprint(),
            "where settings came from is reported beside the fingerprint, not inside it"
        );
    }

    /// An `env` table is a `HashMap`, whose iteration order differs between
    /// two maps holding the same entries. Two processes loading one file
    /// must still agree.
    #[test]
    fn test_the_fingerprint_ignores_map_order() {
        let entries: Vec<(String, String)> =
            (0..32).map(|i| (format!("KEY_{i}"), format!("value-{i}"))).collect();
        let mut forward = ServerConfig::default();
        let mut backward = ServerConfig::default();
        forward.lsp_servers[0].env = entries.iter().cloned().collect();
        backward.lsp_servers[0].env = entries.iter().rev().cloned().collect();
        assert_eq!(forward.fingerprint(), backward.fingerprint());
    }

    #[test]
    fn test_the_backend_table_parses_and_defaults() {
        let parsed: ServerConfig = toml::from_str("[backend]\nidle_shutdown_ms = 250\n").unwrap();
        assert_eq!(parsed.backend.idle_shutdown_ms, 250);
        assert_eq!(ServerConfig::default().backend.idle_shutdown_ms, 10_000);
        assert!(toml::from_str::<ServerConfig>("[backend]\nunknown = 1\n").is_err());
    }
```

If `LspServerConfig::builtins()` could ever be empty, `lsp_servers[0]` panics; it is not (the built-ins include rust-analyzer), and the test states that assumption by indexing.

- [ ] **Step 2: See them fail**

Run: `devrun task check`
Expected: FAIL to compile: `no function load_at`, `cannot find ConfigSource`, `no field backend`, `no method fingerprint`.

- [ ] **Step 3: Add the types**

In `crates/mcpls-core/src/config/mod.rs`, after `HooksConfig`'s `Default` impl (line 307):

```rust
/// How long a shared backend outlives its last session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    /// How long a backend with no session attached waits before it exits.
    ///
    /// Short by default so memory returns to the machine soon after the
    /// last session closes. The cost of short is a cold reindex for a
    /// session that opens just after the timer; raise it when sessions
    /// alternate quickly.
    #[serde(default = "default_idle_shutdown_ms")]
    pub idle_shutdown_ms: u64,
}

const fn default_idle_shutdown_ms() -> u64 {
    10_000
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            idle_shutdown_ms: default_idle_shutdown_ms(),
        }
    }
}

/// Where a loaded configuration came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    /// A path named with `--config` or `MCPLS_CONFIG`.
    Explicit,
    /// The checkout's own `mcpls.toml`, loaded because it was trusted.
    Project,
    /// The user's global configuration file.
    Global,
    /// No file: built-in defaults.
    #[default]
    Defaults,
}
```

In `ServerConfig`, after `diagnostics` and before `project_config_ignored`:

```rust
    /// How a shared backend manages its own lifetime.
    #[serde(default)]
    pub backend: BackendConfig,

    /// Where this configuration was loaded from. Load-time metadata, never
    /// read from or written to a file.
    #[serde(skip)]
    pub source: ConfigSource,
```

Add `backend: BackendConfig::default(), source: ConfigSource::default(),` to `impl Default for ServerConfig`, and to every full `ServerConfig { .. }` literal found by `rg -n "project_config_ignored: false" crates`. Export `BackendConfig` and `ConfigSource` wherever `ProjectConfigTrust` is exported (`crates/mcpls-core/src/lib.rs:58` re-exports `ProjectConfigTrust`; add `ConfigSource` there).

Append to `DEFAULT_CONFIG_TEMPLATE` after the `[diagnostics.hooks]` block:

```text
#
# [backend]
# idle_shutdown_ms = 10000
```

- [ ] **Step 4: Implement the fingerprint**

First learn whether anything in the build enables `serde_json`'s `preserve_order` feature, which decides whether a `serde_json::Value` object keeps its keys sorted.

Run: `cargo tree -e features -i serde_json`
If a harness hook blocks `cargo`, run the same command through `devrun`. Look for `preserve_order` among the features listed for `serde_json`.

Shape A, when nothing enables `preserve_order`. `Value`'s map is then a `BTreeMap`, so converting to a `Value` sorts every object's keys, and the digest needs no recursion. Add only this to `impl ServerConfig`:

```rust
    /// A stable digest of every setting, so two processes can tell whether
    /// they loaded the same configuration. Where the configuration came
    /// from is not part of it.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        use std::hash::{Hash as _, Hasher as _};

        // Through `Value`, whose object keys are sorted, so a `HashMap`'s
        // iteration order never reaches the digest.
        let canonical = serde_json::to_value(self)
            .map_or_else(|error| error.to_string(), |value| value.to_string());
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        canonical.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
```

Do not hash `serde_json::to_string(self)` directly: it writes each `HashMap` field in that map's own iteration order, which is exactly what `test_the_fingerprint_ignores_map_order` catches. The conversion to `Value` is what sorts the keys.

Shape B, when `preserve_order` is enabled or neither command runs. Hash the value with object keys sorted explicitly. In `impl ServerConfig`:

```rust
    /// A stable digest of every setting, so two processes can tell whether
    /// they loaded the same configuration. Where the configuration came
    /// from is not part of it.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        use std::hash::{Hash as _, Hasher as _};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match serde_json::to_value(self) {
            Ok(value) => hash_json(&value, &mut hasher),
            Err(error) => error.to_string().hash(&mut hasher),
        }
        format!("{:016x}", hasher.finish())
    }
```

and a free function beside it:

```rust
/// Hash `value` with object keys in sorted order, so a map's iteration
/// order never reaches the digest.
fn hash_json(value: &serde_json::Value, hasher: &mut std::collections::hash_map::DefaultHasher) {
    use std::hash::Hash as _;

    match value {
        serde_json::Value::Object(map) => {
            0u8.hash(hasher);
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                key.hash(hasher);
                hash_json(&map[key], hasher);
            }
        }
        serde_json::Value::Array(items) => {
            1u8.hash(hasher);
            items.len().hash(hasher);
            for item in items {
                hash_json(item, hasher);
            }
        }
        other => {
            2u8.hash(hasher);
            other.to_string().hash(hasher);
        }
    }
}
```

In either shape, `DefaultHasher` differs between Rust releases, which does not matter: two builds of different versions already refuse each other at the handshake before comparing fingerprints.

- [ ] **Step 5: Discover at the root**

Change `RelativeRootBase::Cwd` to `Dir(PathBuf)` (drop `Copy` from its derive), with doc "Resolve against a given directory: the checkout root, for the auto-discovered global config, which is not tied to the file's own location." In `load_from_with_root_base` line 1094: `RelativeRootBase::Dir(dir) => dir.clone(),` and take `relative_root_base: &RelativeRootBase` if borrowing reads cleaner.

At the end of `load_from_with_root_base`, before `Ok(config)`: `config.source = ConfigSource::Explicit;`.

Replace `load_with_trust`'s body and add `load_at`:

```rust
    pub fn load_with_trust(trust: ProjectConfigTrust) -> Result<Self> {
        let cwd = std::env::current_dir().map_err(Error::Io)?;
        Self::load_at(trust, &cwd)
    }

    /// Load configuration for a session started in `start`.
    ///
    /// Discovery runs from the checkout root enclosing `start` (see
    /// [`crate::hooks::project_root`]), so every session in one checkout
    /// reads the same `mcpls.toml` however deep in it the session started,
    /// and relative `workspace.roots` in the global file resolve against
    /// that root. Otherwise the tiers and trust rules are those of
    /// [`load_with_trust`](Self::load_with_trust).
    ///
    /// # Errors
    ///
    /// Returns an error if `start` cannot be canonicalized or parsing an
    /// existing config fails.
    pub fn load_at(trust: ProjectConfigTrust, start: &Path) -> Result<Self> {
        if let Ok(path) = std::env::var("MCPLS_CONFIG") {
            return Self::load_from(Path::new(&path));
        }

        let root = crate::hooks::project_root(start)?;
        let mut project_config_ignored = false;

        let local_config = root.join("mcpls.toml");
        if local_config.is_file() {
            match trust {
                ProjectConfigTrust::Trusted => {
                    let mut config = Self::load_from(&local_config)?;
                    config.source = ConfigSource::Project;
                    return Ok(config);
                }
                ProjectConfigTrust::Untrusted => {
                    project_config_ignored = true;
                    tracing::warn!(
                        "ignoring untrusted project-local config at {}; pass \
                         --trust-project-config (or set MCPLS_TRUST_PROJECT_CONFIG=true) to \
                         load it",
                        local_config.display()
                    );
                }
            }
        }

        if let Some(config_dir) = dirs::config_dir() {
            let user_config = config_dir.join("mcpls").join("mcpls.toml");
            if user_config.exists() {
                let mut config = Self::load_from_with_root_base(
                    &user_config,
                    RelativeRootBase::Dir(root),
                )?;
                config.project_config_ignored = project_config_ignored;
                config.source = ConfigSource::Global;
                return Ok(config);
            }

            if let Err(e) = Self::create_default_config_file(&user_config) {
                tracing::warn!(
                    "Failed to create default config at {}: {}. Using in-memory defaults.",
                    user_config.display(),
                    e
                );
            } else {
                tracing::info!("Created default config at {}", user_config.display());
            }
        }

        Ok(Self {
            project_config_ignored,
            ..Self::default()
        })
    }
```

Keep `load_with_trust`'s existing doc comment, replacing "current directory" and "`./mcpls.toml`" with "checkout root" and "the checkout's `mcpls.toml`", and replacing the paragraph about the global tier resolving against the cwd with "relative roots in the global config resolve against the checkout root". Update the `ProjectConfigTrust` doc (`:806-819`) the same way, and the `--trust-project-config` and `--config` help text in `crates/mcpls-cli/src/args.rs:45-67` ("Current directory" becomes "The checkout root").

Rename `test_load_from_with_root_base_cwd_resolves_relative_roots_against_cwd` to `test_the_global_config_resolves_relative_roots_against_the_given_dir`, drop its `CwdGuard`, pass `RelativeRootBase::Dir(cwd.clone())`, and rewrite its doc comment to describe the given directory. Existing `load_with_trust` tests enter a temporary cwd with no `.git`, whose root is itself, so they keep passing unchanged.

- [ ] **Step 6: Run the tests**

Run: `devrun task test`
Expected: PASS, including the five new tests and every CLI trust test in `crates/mcpls-cli/tests/cli_integration.rs`.

- [ ] **Step 7: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates/mcpls-core/src crates/mcpls-cli/src/args.rs
git commit -m "feat(config): discover config at the checkout root"
```

---

### Task 4: delete the owner, passive, demotion and forwarding paths

**Files:**
- Modify: `crates/mcpls-core/src/hooks/service.rs` (roles, tasks, their tests)
- Modify: `crates/mcpls-core/src/hooks/listener.rs:36-52` (`LockLoss`), `:68-78` (`lock_path`), `:107-128` (`ServeExit`), `:172-216` (ownership check), `:358-416` (its `select!` arm and docs)
- Modify: `crates/mcpls-core/src/hooks/mod.rs:18-23` (exports)
- Modify: `crates/mcpls-core/src/mcp/handlers.rs:74-84,112` (`hooks` field)
- Modify: `crates/mcpls-core/src/mcp/server.rs`: `HOOK_CHANGED_TIMEOUT` and `HOOK_FLUSH_GRACE` (`:57-76`), `from_owner` and `owner_unreachable` (`:278-309`), `get_new_diagnostics` (`:1102-1107`), `flush_now_if_active` (`:1163-1177`), `flush_from_owner` (`:1211-1250`), `forward_apply_targets` (`:1257-1279`) and its three callers (`:780`, `:863`, `:994`), `footer_for_write` (`:1371-1378`), tests at `:2005-2050`, `:2476-2501`, `RecordingOwner` (`:2833-2890`)
- Modify: `crates/mcpls-core/src/lib.rs:787-918` (role wiring in `serve_with_identity`)
- Modify: `crates/mcpls-core/tests/hooks_socket.rs:173-218` (lock-replacement test)
- Modify: `crates/mcpls-core/tests/e2e/protocol_tests.rs:356-489` (`owner_hooks_seen`, `process_session_reports`, both `i1_t6_*` tests)

**Interfaces:**
- Produces:
  - `pub struct HookStats` in `hooks/service.rs`, `Default`, with `pub(crate) fn record_hook_request(&self)` and `pub fn hooks_seen(&self) -> u64`.
  - `build_handler(server: Arc<McplsServer>, sweeper: Arc<Sweeper>, location: HookLocation, stats: Arc<HookStats>, cancel: watch::Receiver<bool>)`: `stats` replaces `role`.
  - `ServeExit { Cancelled, TransportUnrecoverable }`.
- Removes: `HookRole`, `Role`, `hook_owner_task`, `hook_takeover_task`, `acquire_when_free`, `LOCK_RETRY_INTERVAL`, `LockLoss`, `LockLossPause` and its helpers, `ServeExit::LockLost`, `HookListener::lock_loss`, `OWNERSHIP_CHECK_INTERVAL`, `BridgeContext::hooks`, `McplsServer::{flush_from_owner, forward_apply_targets}`, `NewDiagnosticsResult::{from_owner, owner_unreachable}`.

After this task one process holds the endpoint and answers hooks; any other in-process mcpls in the same project serves its own MCP session and hears no hooks. Tasks 6b to 9 replace that state with one shared backend. The sweeper, `HookLocation`, the listener's accept loop and the hook protocol stay.

This task is deletion. Its test is that nothing references the removed names and the remaining suite passes.

- [ ] **Step 1: Replace the role with counters**

In `crates/mcpls-core/src/hooks/service.rs`, delete the module doc's second paragraph and replace lines 1-7 with:

```rust
//! Serving the hook socket's operations from a running mcpls.
//!
//! The process holding the endpoint answers every session's hooks against
//! the same per-session record its own flush tool uses, so a hook and an
//! agent never see the same diagnostic twice.
```

Delete `LOCK_RETRY_INTERVAL`, `HookRole` and its `impl`, `Role`, `LockLossPause`, `LOCK_LOSS_PAUSE`, `LockLossPauseGuard`, `install_lock_loss_pause`, `pause_after_lock_loss`, `hook_owner_task`, `hook_takeover_task` and `acquire_when_free`. Add in their place:

```rust
/// What the process holding the endpoint has served, for `mcpls hook
/// doctor`.
#[derive(Debug, Default)]
pub struct HookStats {
    /// How many `Changed`, `Flush`, or `EndSession` requests this process
    /// has answered.
    hooks_seen: AtomicU64,
}

impl HookStats {
    /// Record one `Changed`, `Flush`, or `EndSession` request this process
    /// just answered. Not called for `Status`, which is `mcpls hook doctor`
    /// probing rather than a hook firing, nor for `Ack`, which is the
    /// second half of a `Flush` already counted.
    pub(crate) fn record_hook_request(&self) {
        self.hooks_seen.fetch_add(1, Ordering::Relaxed);
    }

    /// How many hook requests this process has answered so far.
    #[must_use]
    pub fn hooks_seen(&self) -> u64 {
        self.hooks_seen.load(Ordering::Relaxed)
    }
}
```

In `build_handler`, rename the `role: Arc<HookRole>` parameter to `stats: Arc<HookStats>` and every `role.` inside to `stats.`. Drop the imports that no longer resolve (`watch` stays for `cancel`; `LockLoss`, `HookListener`, `ServeExit`, `SocketIdentity`, `Duration` go if unused).

In `crates/mcpls-core/src/hooks/mod.rs`, export `pub use service::{HookLocation, HookStats, build_handler};`, remove `LockLoss` from the listener exports, and delete the `pub(crate) use service::{hook_owner_task, hook_takeover_task};` line.

- [ ] **Step 2: Stop watching the lock**

In `crates/mcpls-core/src/hooks/listener.rs`:
- delete `LockLoss`, `OWNERSHIP_CHECK_INTERVAL`, `lock_loss`, the `lock_path` field and its assignment in `acquire_blocking`;
- delete `ServeExit::LockLost` and its doc;
- in `serve`, delete `has_lock_file`, `ownership_check` and the `_ = ownership_check.tick()` arm;
- replace the last paragraph of `serve`'s doc (from "On Unix, also stands down" to the end) with nothing, and the paragraph of `lock_file`'s doc starting "It says nothing about a replacement that happens at any later point" with: "A replacement after this call returns goes unnoticed. The holder keeps serving the connections it has, and a newcomer that locks the fresh file binds its own socket."
- `HookListener`'s `lock` field doc stays.

In `crates/mcpls-core/tests/hooks_socket.rs`, delete `test_an_owner_stands_down_when_its_lock_file_is_replaced` (lines 173-218).

- [ ] **Step 3: Remove the forwarding paths from the server**

In `crates/mcpls-core/src/mcp/server.rs`:
- delete `HOOK_CHANGED_TIMEOUT`, `HOOK_FLUSH_GRACE`, `NewDiagnosticsResult::from_owner`, `NewDiagnosticsResult::owner_unreachable`, `flush_from_owner`, `forward_apply_targets` and its three call statements;
- in `get_new_diagnostics`, delete the comment and `if let Role::Passive` block (lines 1102-1107);
- in `footer_for_write`, delete the comment and `if matches!(... Role::Passive ...)` block (lines 1371-1378);
- delete `flush_now_if_active`, and in `footer_for_write` replace `let mut report = self.flush_now_if_active(&session).await?;` with `let mut report = self.flush_now(&session, Advance::Now).await.0;`;
- drop `Role`, `SocketIdentity`, `ChangeEvent` and `hooks` from the `crate::hooks` import at line 41 if unused.

Delete these tests and the helpers only they use: `test_a_forwarded_flush_keeps_the_documented_object_shape`, the unreachable-owner test right after it, `test_every_write_tool_reports_its_writes_to_the_socket_owner`, and `RecordingOwner` with `unique_suffix` if nothing else calls them (`rg -n "RecordingOwner|unique_suffix" crates/mcpls-core/src/mcp/server.rs`). `WriteFixture::new` loses its `hooks` parameter and the `context.hooks = hooks;` line; update its callers.

In `crates/mcpls-core/src/mcp/handlers.rs`, delete the `hooks` field, its doc, its initializer, and the `HookRole` import.

- [ ] **Step 4: Wire the in-process endpoint without roles**

In `crates/mcpls-core/src/lib.rs` `serve_with_identity`, replace lines 787-918 (from the comment "Acquired before the context is built" through the end of the `if let (Some(identity), Some(sweeper), Some(root))` block) with:

```rust
    // A failure to acquire is not a failure to start: this process still
    // answers every MCP tool, and only the hooks go unanswered.
    let listener = match &hook_identity {
        Some(identity) => match hooks::HookListener::acquire(identity).await {
            Ok(Some(listener)) => Some(listener),
            Ok(None) => {
                warn!(
                    "another mcpls holds this project's endpoint, so its hooks are answered there \
                     rather than here"
                );
                None
            }
            Err(error) => {
                warn!("the project's endpoint could not be bound, so no hooks are served: {error}");
                None
            }
        },
        None => None,
    };

    let sweeper = listener.as_ref().map(|_| {
        let sweeper = Arc::new(hooks::Sweeper::new(
            Arc::clone(&translator),
            hooks::PathFilter::new(
                Arc::clone(&workspace_roots_snapshot),
                Arc::clone(&extension_map),
                Some(Arc::clone(&watch_registry)),
            ),
            Duration::from_millis(config.diagnostics.hooks.sweep_quiet_ms),
            config.workspace.max_documents,
        ));
        tokio::spawn(Arc::clone(&sweeper).run(cancel_rx.clone()));
        sweeper
    });
```

keep the `pump_shared`, `lsp_init_handle` and `context` construction that follows unchanged except deleting `context.hooks = Arc::new(role);` (the context no longer needs `mut`), and replace the socket task spawn with:

```rust
    if let (Some(listener), Some(identity), Some(sweeper), Some(root)) =
        (listener, hook_identity, sweeper, hook_root)
    {
        let handler = hooks::build_handler(
            Arc::new(mcp::McplsServer::from_context(Arc::clone(&context))),
            sweeper,
            hooks::HookLocation { identity, root },
            Arc::new(hooks::HookStats::default()),
            cancel_rx.clone(),
        );
        let op_deadline = Duration::from_millis(config.diagnostics.hooks.op_deadline_ms);
        tokio::spawn(log_hook_task_panic(async move {
            let _ = listener.serve(handler, op_deadline, cancel_rx).await;
        }));
    }
```

`log_hook_task_panic`'s doc: replace its body with "Run the hook socket task, reporting a panic instead of losing it. The task owns the listener, so a panic releases the endpoint while this process keeps running." `hook_server` and its comment above `mcp_server` go.

The sweeper was built before the listener; building it only when a listener exists is deliberate, since nothing enqueues into it otherwise.

- [ ] **Step 5: Update the service tests**

In `crates/mcpls-core/src/hooks/service.rs` `mod tests`:
- `HookHarness`: the `role` field becomes `stats: Arc<HookStats>`; `start_owner` takes `stats` in place of `role`, and instead of `tokio::spawn(hook_owner_task(...))` spawns `listener.serve(build_handler(Arc::clone(&server), Arc::clone(&sweeper), HookLocation { identity: identity.clone(), root: root.clone() }, Arc::clone(&stats), cancel_rx.clone()), Duration::from_millis(1500), cancel_rx)`.
- `test_context` loses its `role` parameter and the `context.hooks = role;` line.
- delete `PassiveInstance`, `passive_instance*`, `passive_pointing_at`, `takeover_candidate`, `wait_for_lock_to_return`, and every test from `test_a_passive_instance_forwards_its_flush_to_the_owner` through `test_a_passive_instance_runs_no_footer_but_still_reports_its_writes`, plus `test_an_owner_whose_lock_file_vanishes_reacquires_and_serves_again`, `test_an_owner_whose_lock_is_taken_goes_passive_rather_than_answering` and `test_a_passive_instance_takes_over_when_the_owner_exits`, and any helper they alone used (`slow_flush_handler`, `apply_rename_over_mcp*` if unused afterwards: check with `rg -n "<name>\(" crates/mcpls-core/src/hooks/service.rs`).
- `test_serve_with_binds_the_socket_and_answers_as_its_owner` and `test_serve_with_runs_the_sweep_loop_it_built` stay.

- [ ] **Step 6: Remove the owner-and-passive e2e tests**

In `crates/mcpls-core/tests/e2e/protocol_tests.rs`, delete `owner_hooks_seen`, `process_session_reports`, `i1_t6_process_fallbacks_are_independent` and `i1_t6_explicit_same_session_shares_delivery`, and imports only they used. Task 11 adds their replacement against a shared backend.

- [ ] **Step 7: Confirm nothing refers to the removed names**

Run: `rg -n "HookRole|Role::|LockLoss|LockLost|hook_owner_task|hook_takeover_task|forward_apply_targets|flush_from_owner|owner_unreachable|from_owner|passive" crates`
Expected: no output, except prose in doc comments that you then rewrite to not describe passive instances.

Run: `devrun task test`
Expected: PASS.

- [ ] **Step 8: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates/mcpls-core
git commit -m "refactor(hooks): drop owner and passive roles"
```

---

### Task 5: every endpoint connection opens with the handshake

**Files:**
- Create: `crates/mcpls-core/src/backend/mod.rs`, `crates/mcpls-core/src/backend/handshake.rs`
- Modify: `crates/mcpls-core/src/lib.rs:38-48` (`pub mod backend;`)
- Modify: `crates/mcpls-core/src/hooks/listener.rs`: `HookStream` visibility (`:32`), `serve` accept arm (`:417-424`), `serve_connection` (`:463-502`), `ProbeOutcome` (`:671-691`), `probe` (`:699-723`), `Connection` (`:801-860`), `connect` visibility (`:862-899`)
- Modify: `crates/mcpls-cli/src/hook.rs`: doctor arms (`:264-321`), foreign scan (`:585-609`), test fake `serve_connection` (`:1233-1300`), and the doctor's pid label wherever it is printed or asserted
- Modify: `plugin/README.md` (the doctor's pid label in its examples and field description)
- Test: `crates/mcpls-core/src/backend/handshake.rs`, `crates/mcpls-core/tests/hooks_socket.rs`, `crates/mcpls-cli/src/hook.rs` tests

**Interfaces:**
- Consumes: `ConfigSource` (Task 3).
- Produces (all in `mcpls_core::backend::handshake`, re-exported from `mcpls_core::backend`):
  - `pub const PROTOCOL: u32 = 1;`, `pub const VERSION: &str`.
  - `pub enum ConnectionKind { Mcp, Hook, Shutdown }`.
  - `pub struct ConfigStamp { pub fingerprint: String, pub source: ConfigSource, pub project_ignored: bool }` with `ConfigStamp::of(config: &ServerConfig) -> ConfigStamp` and `ConfigStamp::conflicts_with(&self, other: &ConfigStamp) -> bool`.
  - `pub struct Handshake { pub mcpls: u32, pub version: String, pub kind: ConnectionKind, pub root: Option<PathBuf>, pub session: Option<String>, pub config: Option<ConfigStamp> }` with `Handshake::mcp(root: PathBuf, session: Option<String>, config: ConfigStamp)`, `Handshake::hook()`, `Handshake::shutdown()`, `Handshake::same_build(&self) -> bool`.
  - `pub struct HandshakeReply { pub mcpls: u32, pub version: String, pub pid: u32, pub sessions: usize, pub refusal: Option<Refusal> }` with `HandshakeReply::new(sessions: usize, refusal: Option<Refusal>)`, `HandshakeReply::same_build(&self) -> bool`.
  - `pub enum Refusal { Build, Trust { backend: ConfigStamp }, Attached, InProcess, HooksDisabled, Other }`.
  - `pub fn compare_builds(ours: (u32, &str), theirs: (u32, &str)) -> std::cmp::Ordering`.
  - `pub(crate) async fn write<W, T>(writer: &mut W, value: &T) -> io::Result<()>` and `pub(crate) async fn read<R, T>(reader: &mut R) -> io::Result<T>`.
  - `pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);`
- Produces in `hooks::listener`: `pub(crate) trait HookStream`, `pub(crate) async fn connect(identity) -> io::Result<Box<dyn HookStream>>`, `pub(crate) async fn serve_hook_connection<H: ?Sized>(stream: Box<dyn HookStream>, handler: Arc<H>, op_deadline: Duration)`, `ProbeOutcome::Refused(HandshakeReply)`.

The spec freezes this line so two builds that cannot speak each other's protocol still read each other's handshake. Hook connections handshake too, which is what lets the doctor name the build it reached.

- [ ] **Step 1: Write the handshake module and its tests**

Create `crates/mcpls-core/src/backend/mod.rs`:

```rust
//! One mcpls backend per checkout root, shared by every session in it.
//!
//! The frontend a host launches relays MCP traffic to the backend over the
//! project's endpoint, and the backend owns the language servers and every
//! session's records.

pub mod handshake;

pub use handshake::{
    ConfigStamp, ConnectionKind, Handshake, HandshakeReply, PROTOCOL, Refusal, VERSION,
    compare_builds,
};
```

Add `pub mod backend;` to `crates/mcpls-core/src/lib.rs` beside `pub mod bridge;`.

Create `crates/mcpls-core/src/backend/handshake.rs`:

```rust
//! The line every endpoint connection opens with.
//!
//! The format is frozen. A build that cannot speak another build's protocol
//! still reads this line and answers it, which is what lets a newer
//! frontend ask an idle older backend to exit and lets `mcpls hook doctor`
//! name the build it reached. Fields are only added, each optional or
//! defaulted, and no reader rejects a field or a refusal it does not know.
//!
//! Both sides read one byte at a time up to the newline, so neither ever
//! buffers bytes of the protocol that follows the handshake.

use std::cmp::Ordering;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::config::{ConfigSource, ServerConfig};

/// The endpoint protocol this build speaks. Raised whenever what follows
/// the handshake changes shape.
pub const PROTOCOL: u32 = 1;

/// This build's version, as the handshake reports it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The longest handshake line either side reads.
const MAX_LINE: usize = 64 * 1024;

/// How long either side waits for the other's handshake line.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// What a connection carries after its handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionKind {
    /// One host session's MCP traffic, relayed by its frontend.
    Mcp,
    /// Newline-delimited hook requests.
    Hook,
    /// A request that the backend exit, honoured only with no session
    /// attached.
    Shutdown,
}

/// The configuration a process loaded, reduced to what another process
/// compares against its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigStamp {
    /// [`ServerConfig::fingerprint`].
    pub fingerprint: String,
    /// Where the configuration came from.
    pub source: ConfigSource,
    /// Whether a project-local `mcpls.toml` was ignored for want of trust.
    #[serde(default)]
    pub project_ignored: bool,
}

impl ConfigStamp {
    /// The stamp of `config`.
    #[must_use]
    pub fn of(config: &ServerConfig) -> Self {
        Self {
            fingerprint: config.fingerprint(),
            source: config.source,
            project_ignored: config.project_config_ignored,
        }
    }

    /// Whether one side loaded the project's `mcpls.toml` while the other
    /// ignored it as untrusted. Project config can name the command mcpls
    /// spawns, so the two may not share a backend.
    #[must_use]
    pub fn conflicts_with(&self, other: &Self) -> bool {
        (self.source == ConfigSource::Project && other.project_ignored)
            || (other.source == ConfigSource::Project && self.project_ignored)
    }
}

/// The first line a client writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshake {
    /// The client's [`PROTOCOL`].
    pub mcpls: u32,
    /// The client's [`VERSION`].
    pub version: String,
    /// What follows.
    pub kind: ConnectionKind,
    /// The canonical checkout root the client resolved.
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// The session the host named.
    #[serde(default)]
    pub session: Option<String>,
    /// The client's configuration.
    #[serde(default)]
    pub config: Option<ConfigStamp>,
}

impl Handshake {
    /// A frontend attaching one host session.
    #[must_use]
    pub fn mcp(root: PathBuf, session: Option<String>, config: ConfigStamp) -> Self {
        Self {
            root: Some(root),
            session,
            config: Some(config),
            ..Self::bare(ConnectionKind::Mcp)
        }
    }

    /// A hook invocation or `mcpls hook doctor`.
    #[must_use]
    pub fn hook() -> Self {
        Self::bare(ConnectionKind::Hook)
    }

    /// A frontend asking an idle backend to exit.
    #[must_use]
    pub fn shutdown() -> Self {
        Self::bare(ConnectionKind::Shutdown)
    }

    fn bare(kind: ConnectionKind) -> Self {
        Self {
            mcpls: PROTOCOL,
            version: VERSION.to_string(),
            kind,
            root: None,
            session: None,
            config: None,
        }
    }

    /// Whether this handshake came from a build identical to this one.
    #[must_use]
    pub fn same_build(&self) -> bool {
        self.mcpls == PROTOCOL && self.version == VERSION
    }
}

/// The line a server answers a handshake with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeReply {
    /// The server's [`PROTOCOL`].
    pub mcpls: u32,
    /// The server's [`VERSION`].
    pub version: String,
    /// The server's process id.
    pub pid: u32,
    /// How many MCP sessions are attached to the server.
    #[serde(default)]
    pub sessions: usize,
    /// Why the connection is refused, absent when it is accepted.
    #[serde(default)]
    pub refusal: Option<Refusal>,
}

impl HandshakeReply {
    /// This process's answer.
    #[must_use]
    pub fn new(sessions: usize, refusal: Option<Refusal>) -> Self {
        Self {
            mcpls: PROTOCOL,
            version: VERSION.to_string(),
            pid: std::process::id(),
            sessions,
            refusal,
        }
    }

    /// Whether the answering server is a build identical to this one.
    #[must_use]
    pub fn same_build(&self) -> bool {
        self.mcpls == PROTOCOL && self.version == VERSION
    }
}

/// Why a server refused a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Refusal {
    /// The two builds differ.
    Build,
    /// One side trusted the project's `mcpls.toml` and the other did not.
    Trust {
        /// The server's configuration.
        backend: ConfigStamp,
    },
    /// A shutdown was asked for while sessions are attached.
    Attached,
    /// The endpoint is held by an mcpls serving one session in-process.
    InProcess,
    /// The server's configuration turns hooks off.
    HooksDisabled,
    /// A reason this build does not know.
    #[serde(other)]
    Other,
}

/// How two builds order, oldest first: by protocol, then by version,
/// compared numerically component by component.
#[must_use]
pub fn compare_builds(ours: (u32, &str), theirs: (u32, &str)) -> Ordering {
    ours.0
        .cmp(&theirs.0)
        .then_with(|| numeric(ours.1).cmp(&numeric(theirs.1)))
}

fn numeric(version: &str) -> Vec<u64> {
    version
        .split(['.', '-', '+'])
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// Write `value` as one line.
pub(crate) async fn write<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize + Sync,
{
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

/// Read one line, one byte at a time, and parse it.
pub(crate) async fn read<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    let mut line = Vec::with_capacity(256);
    loop {
        let byte = reader.read_u8().await?;
        if byte == b'\n' {
            break;
        }
        if line.len() == MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the handshake line is too long",
            ));
        }
        line.push(byte);
    }
    serde_json::from_slice(&line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The frozen fields, spelled out. Changing this literal changes what
    /// every older and newer build can read.
    #[test]
    fn test_the_request_line_is_frozen() {
        let literal = r#"{"mcpls":1,"version":"0.3.9","kind":"mcp","root":"/work","session":"s1","config":{"fingerprint":"00000000000000ff","source":"project","project_ignored":false}}"#;
        let parsed: Handshake = serde_json::from_str(literal).unwrap();
        assert_eq!(parsed.mcpls, 1);
        assert_eq!(parsed.kind, ConnectionKind::Mcp);
        assert_eq!(parsed.session.as_deref(), Some("s1"));
        assert_eq!(parsed.config.unwrap().source, ConfigSource::Project);
    }

    #[test]
    fn test_the_reply_line_is_frozen() {
        let literal = r#"{"mcpls":1,"version":"0.3.9","pid":42,"sessions":2,"refusal":{"reason":"build"}}"#;
        let parsed: HandshakeReply = serde_json::from_str(literal).unwrap();
        assert_eq!(parsed.pid, 42);
        assert_eq!(parsed.sessions, 2);
        assert_eq!(parsed.refusal, Some(Refusal::Build));
    }

    #[test]
    fn test_a_newer_builds_lines_still_parse() {
        let request: Handshake =
            serde_json::from_str(r#"{"mcpls":9,"version":"9.0.0","kind":"shutdown","future":true}"#)
                .unwrap();
        assert_eq!(request.kind, ConnectionKind::Shutdown);
        let reply: HandshakeReply = serde_json::from_str(
            r#"{"mcpls":9,"version":"9.0.0","pid":1,"refusal":{"reason":"something_new","detail":1}}"#,
        )
        .unwrap();
        assert_eq!(reply.refusal, Some(Refusal::Other));
        assert_eq!(reply.sessions, 0);
    }

    #[test]
    fn test_builds_order_by_protocol_then_version() {
        assert_eq!(compare_builds((1, "0.3.10"), (1, "0.3.9")), Ordering::Greater);
        assert_eq!(compare_builds((1, "0.3.9"), (1, "0.3.9")), Ordering::Equal);
        assert_eq!(compare_builds((1, "9.0.0"), (2, "0.1.0")), Ordering::Less);
    }

    #[test]
    fn test_only_trusted_against_ignored_conflicts() {
        let stamp = |source, project_ignored| ConfigStamp {
            fingerprint: String::new(),
            source,
            project_ignored,
        };
        let loaded = stamp(ConfigSource::Project, false);
        let ignored = stamp(ConfigSource::Global, true);
        let explicit = stamp(ConfigSource::Explicit, false);
        assert!(loaded.conflicts_with(&ignored));
        assert!(ignored.conflicts_with(&loaded));
        assert!(!loaded.conflicts_with(&explicit));
        assert!(!ignored.conflicts_with(&explicit));
        assert!(!loaded.conflicts_with(&loaded));
    }

    #[tokio::test]
    async fn test_read_consumes_exactly_one_line() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        write(&mut client, &Handshake::hook()).await.unwrap();
        client.write_all(b"after\n").await.unwrap();

        let handshake: Handshake = read(&mut server).await.unwrap();
        assert_eq!(handshake, Handshake::hook());
        let mut rest = [0u8; 6];
        server.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"after\n");
    }
}
```

- [ ] **Step 2: Write the failing listener test**

Add to `crates/mcpls-core/tests/hooks_socket.rs`:

```rust
/// A client that skips the handshake gets nothing served: the listener
/// cannot tell a hook request from an MCP frame or a newer build's line.
#[tokio::test]
async fn test_a_connection_without_a_handshake_is_not_served() {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    let (dir, identity) = temp_identity();
    let listener = HookListener::acquire(&identity).await.expect("acquire").expect("owner");
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(listener.serve(handler(), Duration::from_secs(1), cancel_rx));

    #[cfg(not(windows))]
    let mut stream = tokio::net::UnixStream::connect(&identity.socket).await.expect("connect");
    #[cfg(windows)]
    let mut stream = tokio::net::windows::named_pipe::ClientOptions::new()
        .open(&identity.socket)
        .expect("connect");
    stream.write_all(b"{\"op\":\"status\"}\n").await.expect("write");

    let mut line = String::new();
    let read = tokio::time::timeout(
        Duration::from_secs(5),
        BufReader::new(stream).read_line(&mut line),
    )
    .await
    .expect("the listener answers or hangs up rather than waiting");
    let reply: Option<mcpls_core::backend::HandshakeReply> = serde_json::from_str(&line).ok();
    assert!(
        read.map_or(true, |n| n == 0) || reply.is_some_and(|reply| reply.refusal.is_some()),
        "a request line was served as if it were a handshake: {line}"
    );
    drop(dir);
}
```

`handler()` is the file's existing helper (`crates/mcpls-core/tests/hooks_socket.rs:66`). The listener reads the status line as a handshake, fails to parse it, and hangs up.

- [ ] **Step 3: See the tests fail**

Run: `devrun task test`
Expected: the handshake module's tests PASS once it compiles; `test_a_connection_without_a_handshake_is_not_served` FAILS because the listener answers `{"op":"status",...}` with a status response.

- [ ] **Step 4: Handshake on the listener**

In `crates/mcpls-core/src/hooks/listener.rs`, make `HookStream` `pub(crate)` and `connect` (both platform variants) `pub(crate)`. Change the accept arm in `serve` (line 422-423) to:

```rust
                            let handler = Arc::clone(&handler);
                            tokio::spawn(async move {
                                let Some(stream) = accept_hook_handshake(stream).await else {
                                    return;
                                };
                                serve_hook_connection(stream, handler, op_deadline).await;
                            });
```

and add:

```rust
/// Read a connection's handshake and answer it as a listener that serves
/// hooks and nothing else. `None` when the connection is refused or never
/// handshakes.
async fn accept_hook_handshake(mut stream: Box<dyn HookStream>) -> Option<Box<dyn HookStream>> {
    use crate::backend::handshake::{self, ConnectionKind, Handshake, HandshakeReply, Refusal};

    let handshake: Handshake =
        tokio::time::timeout(handshake::HANDSHAKE_TIMEOUT, handshake::read(&mut stream))
            .await
            .ok()?
            .ok()?;
    let refusal = if !handshake.same_build() {
        Some(Refusal::Build)
    } else if handshake.kind == ConnectionKind::Hook {
        None
    } else {
        Some(Refusal::InProcess)
    };
    let refused = refusal.is_some();
    handshake::write(&mut stream, &HandshakeReply::new(0, refusal))
        .await
        .ok()?;
    (!refused).then_some(stream)
}
```

Rename `serve_connection` to `serve_hook_connection`, make it `pub(crate)`, and relax its bound to `H: Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + ?Sized + 'static`.

- [ ] **Step 5: Handshake on the client**

Add to `ProbeOutcome`:

```rust
    /// A server answered the handshake and refused this connection. The
    /// reply names its build, pid and why.
    Refused(HandshakeReply),
```

Replace `Connection::open` and add `establish`:

```rust
    async fn open(identity: &SocketIdentity) -> Result<Self> {
        let stream = connect(identity).await?;
        match establish(stream).await? {
            Established::Accepted(stream) => Ok(Self::over(stream)),
            Established::Refused(reply) => Err(refused(identity, &reply)),
        }
    }
```

```rust
enum Established {
    Accepted(Box<dyn HookStream>),
    Refused(HandshakeReply),
}

/// Handshake as a hook client on a connected stream.
async fn establish(mut stream: Box<dyn HookStream>) -> Result<Established> {
    handshake::write(&mut stream, &Handshake::hook()).await?;
    let reply: HandshakeReply = handshake::read(&mut stream).await?;
    if reply.refusal.is_some() {
        return Ok(Established::Refused(reply));
    }
    Ok(Established::Accepted(stream))
}

fn refused(identity: &SocketIdentity, reply: &HandshakeReply) -> Error {
    Error::Transport(format!(
        "the mcpls {} (pid {}) at {} refused this build ({}): {:?}",
        reply.version,
        reply.pid,
        identity.socket.display(),
        handshake::VERSION,
        reply.refusal
    ))
}
```

with `use crate::backend::handshake::{self, Handshake, HandshakeReply};` at the top.

In `probe`, after the connect phase:

```rust
    let established = match tokio::time::timeout_at(deadline, establish(stream)).await {
        Ok(Ok(established)) => established,
        Ok(Err(error)) => return ProbeOutcome::Unintelligible(error),
        Err(_) => return ProbeOutcome::Busy,
    };
    let stream = match established {
        Established::Accepted(stream) => stream,
        Established::Refused(reply) => return ProbeOutcome::Refused(reply),
    };
```

then the existing `answer_one` exchange. `answer_one` keeps calling `Connection::over`, which stays synchronous and handshake-free because its callers have already handshaken.

- [ ] **Step 6: Teach the doctor and its fake**

In `crates/mcpls-cli/src/hook.rs` `doctor_scanning`, add before the `Busy` arm:

```rust
        ProbeOutcome::Refused(reply) => {
            lines.push(format!(
                "server sees: mcpls {} refused this build ({}): {}",
                reply.version,
                mcpls_core::backend::VERSION,
                refusal_text(reply.refusal.as_ref())
            ));
            lines.push(format!("backend pid: {}", reply.pid));
        }
```

with

```rust
/// A refusal as the doctor prints it.
fn refusal_text(refusal: Option<&mcpls_core::backend::Refusal>) -> String {
    use mcpls_core::backend::Refusal;
    match refusal {
        Some(Refusal::Build) => "the two builds differ".to_string(),
        Some(Refusal::HooksDisabled) => "its configuration turns hooks off".to_string(),
        Some(Refusal::InProcess) => "it serves one session in-process".to_string(),
        Some(other) => format!("{other:?}"),
        None => "no reason given".to_string(),
    }
}
```

In `find_foreign_owner`, count `ProbeOutcome::Refused(_)` with `Busy | Unintelligible(_)` as `unidentified += 1`.

In the test fake `serve_connection` (`hook.rs:1233`), before `let (reader, mut writer) = tokio::io::split(stream);`, consume the handshake the way a real listener does. Change its signature to take `mut stream: S` and insert:

```rust
        use mcpls_core::backend::{Handshake, HandshakeReply};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut line = Vec::new();
        loop {
            let Ok(byte) = stream.read_u8().await else { return };
            if byte == b'\n' {
                break;
            }
            line.push(byte);
        }
        if serde_json::from_slice::<Handshake>(&line).is_err() {
            return;
        }
        let Ok(mut reply) = serde_json::to_vec(&HandshakeReply::new(0, None)) else { return };
        reply.push(b'\n');
        if stream.write_all(&reply).await.is_err() {
            return;
        }
```

The `silent` branch stays above this, so a silent owner still never answers the handshake and the doctor still reports it busy.

Rename the doctor's pid label to `backend pid:` (Decision 7). Every print and assertion of the old label is in `crates/mcpls-cli/src/hook.rs` or `plugin/README.md`; list them with `rg -n 'owner.pid' crates plugin`. In `hook.rs` they are the answered-status arm, the no-owner arm, `doctor_without_identity`, the `OWNER_PID_UNKNOWN` constant (rename it `BACKEND_PID_UNKNOWN`, value `"backend pid: unknown"`), and the doctor tests that assert those lines exactly or find them by prefix, including the assertion messages that name the line. Change only the label; every assertion stays exact. In `plugin/README.md`, change both doctor examples and the field description. Afterwards `rg -n 'owner.pid' crates plugin` prints nothing.

Add a doctor test beside `test_doctor_reports_an_answer_that_is_not_a_status`:

```rust
    /// A backend of another build refuses the doctor's handshake. Its reply
    /// still names the build and pid, which is what a reader needs.
    #[tokio::test]
    async fn test_doctor_reports_a_refusing_backends_build_and_pid() {
        use mcpls_core::backend::{HandshakeReply, Refusal};

        let project = tempfile::tempdir().expect("a temp dir");
        mark_checkout(project.path());
        let mut reply = HandshakeReply::new(3, Some(Refusal::Build));
        reply.version = "0.0.1".to_string();
        reply.pid = 4242;
        let raw = serde_json::to_string(&reply).expect("serialize");
        let out = doctor_with_raw_handshake_reply(project.path(), &raw).await;

        assert!(
            out.contains("server sees: mcpls 0.0.1 refused this build"),
            "{out}"
        );
        assert!(out.contains("backend pid: 4242"), "{out}");
    }
```

`doctor_with_raw_handshake_reply` is a new helper beside `doctor_with_owner_answering_raw_status` that binds a listener on `local_identity_for(project, socket_dir)` and, per connection, reads one line and writes `raw` plus a newline. Build it from the same `bind_owner` and platform accept code the fake already uses: add `handshake_reply_raw: Option<String>` to `OwnerBehavior`, and in the fake's handshake block write that raw line instead of the accepted reply when it is set, then return.

- [ ] **Step 7: Run the tests**

Run: `devrun task test`
Expected: PASS, including the handshake module's six tests, `test_a_connection_without_a_handshake_is_not_served` and `test_doctor_reports_a_refusing_backends_build_and_pid`.

- [ ] **Step 8: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates/mcpls-core crates/mcpls-cli/src/hook.rs plugin/README.md
git commit -m "feat(backend): handshake on every connection"
```

---

### Task 6a: extract the runtime

**Files:**
- Modify: `crates/mcpls-core/src/lib.rs` (`serve_with_identity` split into `Runtime::start`, `serve_hooks_in_process` and the in-process runner)
- Modify: `crates/mcpls-core/src/hooks/listener.rs` (`HookListener::accept`)
- Test: the existing suite, notably `test_serve_with_binds_the_socket_and_answers_as_its_owner` and `test_serve_with_runs_the_sweep_loop_it_built` in `crates/mcpls-core/src/hooks/service.rs`

**Interfaces:**
- Consumes: `McplsServer::from_context` (Task 1), `HookStats`, `build_handler(server, sweeper, location, stats, cancel)`, `HookListener::acquire`, `HookListener::serve` (Task 4).
- Produces:
  - `pub(crate) struct Runtime` in `lib.rs` with `pub(crate) async fn start(config: &ServerConfig, root: Result<PathBuf, Error>) -> Result<Runtime, Error>`, `pub(crate) async fn shutdown(self)`, crate-visible fields `context: Arc<mcp::BridgeContext>`, `sweeper: Arc<hooks::Sweeper>`, `cancel_rx: watch::Receiver<bool>`, and private fields `translator: Arc<Translator>`, `cancel_tx: watch::Sender<bool>`, `lsp_init_handle: Option<JoinHandle<()>>`.
  - `async fn serve_hooks_in_process(runtime: &Runtime, config: &ServerConfig, identity_override: Option<hooks::SocketIdentity>, hook_root: Option<PathBuf>)` in `lib.rs`, private, so tests in descendant modules call it as `crate::serve_hooks_in_process`.
  - `HookListener::accept(&self) -> io::Result<Box<dyn HookStream>>` (`pub(crate)`).

A backend and an in-process server serve from the same state, so that state moves out of `serve_with_identity` into a value both build. This task changes no behaviour, and the existing suite is its test.

- [ ] **Step 1: Extract the runtime**

In `crates/mcpls-core/src/lib.rs`, move the body of `serve_with_identity` from `config.validate()?;` through the construction of `context` into:

```rust
/// Everything one mcpls process serves from, however many connections
/// reach it: the language servers, the caches and every record.
pub(crate) struct Runtime {
    pub(crate) context: Arc<mcp::BridgeContext>,
    pub(crate) sweeper: Arc<hooks::Sweeper>,
    pub(crate) cancel_rx: tokio::sync::watch::Receiver<bool>,
    translator: Arc<Translator>,
    cancel_tx: tokio::sync::watch::Sender<bool>,
    lsp_init_handle: Option<JoinHandle<()>>,
}

impl Runtime {
    /// Validate `config`, resolve its workspace against `root`, start the
    /// language servers in the background, and build the shared state.
    ///
    /// `root` is only consulted when a workspace root is empty or
    /// relative, so an unreadable working directory does not block a
    /// configuration whose roots are all absolute.
    pub(crate) async fn start(
        config: &ServerConfig,
        root: Result<PathBuf, Error>,
    ) -> Result<Self, Error> {
        config.validate()?;
        let workspace_roots = if config.workspace.roots.is_empty()
            || config.workspace.roots.iter().any(|root| root.is_relative())
        {
            resolve_workspace_roots(&config.workspace.roots, &root?)?
        } else {
            canonicalize_workspace_roots(&config.workspace.roots, Path::new(""))?
        };
        // ... the rest of the moved block, unchanged, from `extension_map`
        // through `context`, with the sweeper built unconditionally:
        //     let sweeper = Arc::new(hooks::Sweeper::new(...));
        //     tokio::spawn(Arc::clone(&sweeper).run(cancel_rx.clone()));
        Ok(Self {
            context: Arc::new(context),
            sweeper,
            cancel_rx,
            translator,
            cancel_tx,
            lsp_init_handle,
        })
    }

    /// Stop the background tasks and drain the language servers.
    pub(crate) async fn shutdown(self) {
        shutdown(&self.cancel_tx, &self.translator, self.lsp_init_handle).await;
    }
}
```

The comment lines inside the snippet mark where the existing statements go; do not leave them in the code. Everything the moved block referenced from `config` by value (`config.diagnostics`, `config.workspace.max_documents`) reads through `&ServerConfig`; `DiagnosticsConfig` is `Copy`. The block that built the sweeper only when a listener existed (Task 4) builds it unconditionally here.

`serve_with_identity` becomes, with its hook wiring moved into a function of its own:

```rust
pub(crate) async fn serve_with_identity(
    config: ServerConfig,
    transport: Transport,
    identity_override: Option<hooks::SocketIdentity>,
) -> Result<(), Error> {
    info!("Starting MCPLS server...");
    let shutdown_signal = ShutdownSignal::new();
    let root = std::env::current_dir()
        .map_err(Error::Io)
        .and_then(|dir| hooks::project_root(&dir));
    let hook_root = root.as_ref().ok().cloned();
    let runtime = Runtime::start(&config, root).await?;
    serve_hooks_in_process(&runtime, &config, identity_override, hook_root).await;

    let mcp_server = mcp::McplsServer::from_context(Arc::clone(&runtime.context));
    info!("MCPLS server initialized successfully");
    let result = match transport {
        Transport::Stdio => {
            info!("Listening for MCP requests on stdio...");
            run_stdio(mcp_server, shutdown_signal).await
        }
        #[cfg(feature = "transport-http")]
        Transport::Http(cfg) => run_http(mcp_server, cfg, shutdown_signal).await,
    };
    runtime.shutdown().await;
    info!("MCPLS server shutting down");
    result
}

/// Bind the project's endpoint for hooks when hooks are on and no other
/// process holds it, and answer hooks from `runtime` until it is cancelled.
async fn serve_hooks_in_process(
    runtime: &Runtime,
    config: &ServerConfig,
    identity_override: Option<hooks::SocketIdentity>,
    hook_root: Option<PathBuf>,
) {
    if !config.diagnostics.hooks.enabled {
        return;
    }
    let identity = match identity_override.map_or_else(
        || hook_root.as_deref().map(hooks::identity_for).transpose(),
        |identity| Ok(Some(identity)),
    ) {
        Ok(Some(identity)) => identity,
        Ok(None) => return,
        Err(error) => {
            warn!("hooks are configured on but this project's endpoint could not be derived: {error}");
            return;
        }
    };
    let listener = match hooks::HookListener::acquire(&identity).await {
        Ok(Some(listener)) => listener,
        Ok(None) => {
            warn!(
                "another mcpls holds this project's endpoint, so its hooks are answered there \
                 rather than here"
            );
            return;
        }
        Err(error) => {
            warn!("the project's endpoint could not be bound, so no hooks are served: {error}");
            return;
        }
    };
    let root = hook_root.unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|dir| dunce::canonicalize(dir).ok())
            .unwrap_or_default()
    });
    let handler = hooks::build_handler(
        Arc::new(mcp::McplsServer::from_context(Arc::clone(&runtime.context))),
        Arc::clone(&runtime.sweeper),
        hooks::HookLocation { identity, root },
        Arc::new(hooks::HookStats::default()),
        runtime.cancel_rx.clone(),
    );
    let op_deadline = Duration::from_millis(config.diagnostics.hooks.op_deadline_ms);
    let cancel = runtime.cancel_rx.clone();
    tokio::spawn(log_hook_task_panic(async move {
        let _ = listener.serve(handler, op_deadline, cancel).await;
    }));
}
```

Keep the existing "Registered before any other startup work" comment above `ShutdownSignal::new()` and the `serve_with_identity` doc. Delete `canonicalized_root_identity` if nothing else calls it. The HTTP tests in `hooks/service.rs` pass an identity override and a workspace with absolute roots. For them `hook_root` may be absent, and the `unwrap_or_else` gives `HookLocation::root` the canonical current directory, as the deleted override branch did.

- [ ] **Step 2: Expose accept**

In `crates/mcpls-core/src/hooks/listener.rs` `impl HookListener`:

```rust
    /// Wait for the next client, for a caller running its own accept loop.
    pub(crate) async fn accept(&self) -> io::Result<Box<dyn HookStream>> {
        self.transport.accept().await
    }
```

Nothing calls `accept` until the backend's accept loop in Task 6b. Put `#[allow(dead_code)]` on the method with no comment; Task 6b removes it.

- [ ] **Step 3: Run the tests**

Run: `devrun task test`
Expected: PASS with no test added or changed, including `test_serve_with_binds_the_socket_and_answers_as_its_owner` and `test_serve_with_runs_the_sweep_loop_it_built`.

- [ ] **Step 4: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates/mcpls-core/src/lib.rs crates/mcpls-core/src/hooks/listener.rs
git commit -m "refactor(core): extract the runtime"
```

---

### Task 6b: the backend serves many connections and exits when idle

**Files:**
- Create: `crates/mcpls-core/src/backend/endpoint.rs`
- Modify: `crates/mcpls-core/src/backend/mod.rs`
- Modify: `crates/mcpls-core/src/lib.rs` (`serve_hooks_in_process` returns the session's notes, `ENDPOINT_HELD_NOTE`)
- Modify: `crates/mcpls-core/src/hooks/listener.rs` (drop the `dead_code` allow on `accept`)
- Test: `crates/mcpls-core/src/backend/endpoint.rs`, `crates/mcpls-core/src/hooks/service.rs`

**Interfaces:**
- Consumes: `Runtime::{start, shutdown}`, the fields `Runtime::{context, sweeper, cancel_rx}`, `serve_hooks_in_process`, `HookListener::accept` (Task 6a); `McplsServer::for_connection`, `with_notes`, `connection`, `session` (Task 1), `McplsServer::subscriptions` (Task 2); `ServerConfig::backend` (Task 3); `HookStats`, `build_handler` (Task 4); `ConfigStamp`, `ConnectionKind`, `Handshake`, `HandshakeReply`, `Refusal`, `handshake::{read, write, HANDSHAKE_TIMEOUT, VERSION}`, `serve_hook_connection`, `HookStream`, `listener::connect` (Task 5).
- Produces:
  - `pub async fn serve_backend(config: ServerConfig, root: PathBuf) -> Result<(), Error>` in `backend::endpoint`, re-exported as `mcpls_core::backend::serve_backend`.
  - `pub(crate) async fn serve_backend_on(config: ServerConfig, root: PathBuf, identity: SocketIdentity) -> Result<(), Error>` for tests.
  - `pub(crate) struct Endpoint` with `pub(crate) fn new(runtime: &crate::Runtime, config: &ServerConfig, root: PathBuf, identity: SocketIdentity) -> Arc<Endpoint>` and `pub(crate) async fn run(self: Arc<Self>, listener: HookListener, idle: Duration, signal: impl Future<Output = ()>) -> Exit`; `pub(crate) enum Exit { Idle, Shutdown, Signal }`.
  - `pub(crate) struct Attachments` with `attach(self: &Arc<Self>, connection: ConnectionId, session: String) -> Attached`, `count(&self) -> usize`, `sessions(&self) -> Vec<String>`, `watch(&self) -> watch::Receiver<usize>`.
  - `async fn serve_hooks_in_process(runtime: &Runtime, config: &ServerConfig, identity_override: Option<hooks::SocketIdentity>, hook_root: Option<PathBuf>) -> Vec<String>` and `const ENDPOINT_HELD_NOTE: &str` in `lib.rs`.

The spec's order on exit matters more than its timer: stop accepting, close every open stream, then drain the language servers. Dropping the listener releases the endpoint (and on Unix its ownership lock), and a connection whose handshake is read after the loop stopped accepting is closed without a reply, so a frontend arriving during the drain starts a new backend instead of attaching to one on its way out.

- [ ] **Step 1: Write the failing tests**

Create `crates/mcpls-core/src/backend/endpoint.rs` with only its test module for now (the implementation lands in Step 3):

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::*;
    use crate::backend::handshake::{self, ConnectionKind, Handshake, HandshakeReply, Refusal};
    use crate::config::ServerConfig;
    use crate::hooks::SocketIdentity;

    fn temp_identity(dir: &std::path::Path) -> SocketIdentity {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        dir.hash(&mut hasher);
        let hash = format!("{:016x}", hasher.finish());
        #[cfg(windows)]
        let socket = PathBuf::from(format!(r"\\.\pipe\mcpls-endpoint-test-{hash}"));
        #[cfg(not(windows))]
        let socket = dir.join(format!("{hash}.sock"));
        SocketIdentity {
            socket,
            lock: dir.join(format!("{hash}.lock")),
            hash,
        }
    }

    fn config(idle_ms: u64) -> ServerConfig {
        let mut config = ServerConfig {
            lsp_servers: Vec::new(),
            ..ServerConfig::default()
        };
        config.backend.idle_shutdown_ms = idle_ms;
        config
    }

    struct Backend {
        _dir: tempfile::TempDir,
        root: PathBuf,
        identity: SocketIdentity,
        task: tokio::task::JoinHandle<Result<(), crate::Error>>,
    }

    async fn start(config: ServerConfig) -> Backend {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let identity = temp_identity(&root);
        let task = tokio::spawn(serve_backend_on(config, root.clone(), identity.clone()));
        for _ in 0..200 {
            if crate::hooks::listener::connect(&identity).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Backend { _dir: dir, root, identity, task }
    }

    async fn open(
        identity: &SocketIdentity,
        request: &Handshake,
    ) -> (Box<dyn crate::hooks::listener::HookStream>, HandshakeReply) {
        let mut stream = crate::hooks::listener::connect(identity).await.unwrap();
        handshake::write(&mut stream, request).await.unwrap();
        let reply = handshake::read(&mut stream).await.unwrap();
        (stream, reply)
    }

    fn mcp_handshake(backend: &Backend) -> Handshake {
        Handshake::mcp(
            backend.root.clone(),
            None,
            ConfigStamp::of(&config(0)),
        )
    }

    async fn initialize(stream: &mut Box<dyn crate::hooks::listener::HookStream>) -> serde_json::Value {
        stream
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"endpoint-test\",\"version\":\"1\"}}}\n",
            )
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn test_an_mcp_connection_is_served_after_its_handshake() {
        let backend = start(config(60_000)).await;
        let (mut stream, reply) = open(&backend.identity, &mcp_handshake(&backend)).await;
        assert_eq!(reply.refusal, None);
        let answer = initialize(&mut stream).await;
        assert_eq!(answer["result"]["serverInfo"]["name"], "mcpls", "{answer}");
        backend.task.abort();
    }

    #[tokio::test]
    async fn test_a_backend_nobody_attaches_to_exits_after_the_idle_timer() {
        let backend = start(config(100)).await;
        tokio::time::timeout(Duration::from_secs(5), backend.task)
            .await
            .expect("the idle backend exited")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn test_an_attached_session_holds_the_backend_open() {
        let backend = start(config(100)).await;
        let (stream, reply) = open(&backend.identity, &mcp_handshake(&backend)).await;
        assert_eq!(reply.refusal, None);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!backend.task.is_finished(), "exited with a session attached");

        drop(stream);
        tokio::time::timeout(Duration::from_secs(5), backend.task)
            .await
            .expect("the backend exited once its last session left")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn test_a_shutdown_is_refused_while_a_session_is_attached() {
        let backend = start(config(60_000)).await;
        let (_attached, _) = open(&backend.identity, &mcp_handshake(&backend)).await;
        let (_, refused) = open(&backend.identity, &Handshake::shutdown()).await;
        assert_eq!(refused.refusal, Some(Refusal::Attached));
        assert_eq!(refused.sessions, 1);
        backend.task.abort();
    }

    #[tokio::test]
    async fn test_a_shutdown_from_any_build_ends_an_idle_backend() {
        let backend = start(config(60_000)).await;
        let mut request = Handshake::shutdown();
        request.version = "999.0.0".to_string();
        let (_, reply) = open(&backend.identity, &request).await;
        assert_eq!(reply.refusal, None);
        tokio::time::timeout(Duration::from_secs(5), backend.task)
            .await
            .expect("the backend honoured the shutdown")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn test_another_build_is_refused_with_the_backends_build() {
        let backend = start(config(60_000)).await;
        let mut request = mcp_handshake(&backend);
        request.version = "0.0.1".to_string();
        let (_, reply) = open(&backend.identity, &request).await;
        assert_eq!(reply.refusal, Some(Refusal::Build));
        assert_eq!(reply.version, handshake::VERSION);
        assert_eq!(reply.pid, std::process::id());
        backend.task.abort();
    }

    #[tokio::test]
    async fn test_a_trust_disagreement_is_refused() {
        let mut trusting = config(60_000);
        trusting.source = crate::config::ConfigSource::Project;
        let backend = start(trusting).await;
        let mut ignoring = config(0);
        ignoring.source = crate::config::ConfigSource::Global;
        ignoring.project_config_ignored = true;
        let request = Handshake::mcp(backend.root.clone(), None, ConfigStamp::of(&ignoring));
        let (_, reply) = open(&backend.identity, &request).await;
        assert!(matches!(reply.refusal, Some(Refusal::Trust { .. })), "{reply:?}");
        backend.task.abort();
    }

    #[tokio::test]
    async fn test_a_different_fingerprint_is_served_and_named() {
        let backend = start(config(60_000)).await;
        let mut other = config(0);
        other.diagnostics.max_total = 3;
        let request = Handshake::mcp(backend.root.clone(), None, ConfigStamp::of(&other));
        let (mut stream, reply) = open(&backend.identity, &request).await;
        assert_eq!(reply.refusal, None);
        let answer = initialize(&mut stream).await;
        let instructions = answer["result"]["instructions"].as_str().unwrap();
        assert!(instructions.contains(&other.fingerprint()), "{instructions}");
        backend.task.abort();
    }

    /// Exit closes streams that are still open, so a client is never left
    /// holding a connection to a process on its way out.
    #[tokio::test]
    async fn test_exit_closes_every_open_stream() {
        let backend = start(config(100)).await;
        let (stream, reply) = open(&backend.identity, &Handshake::hook()).await;
        assert_eq!(reply.refusal, None);
        let mut line = String::new();
        let read = tokio::time::timeout(
            Duration::from_secs(5),
            BufReader::new(stream).read_line(&mut line),
        )
        .await
        .expect("the hook stream was closed when the backend exited");
        assert_eq!(read.unwrap(), 0);
    }

    /// The endpoint is released before the language servers drain, so a
    /// frontend arriving mid-drain can bind a fresh backend.
    #[tokio::test]
    async fn test_the_endpoint_is_free_before_the_runtime_drains() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let identity = temp_identity(&root);
        let listener = crate::hooks::HookListener::acquire(&identity).await.unwrap().unwrap();
        let runtime = crate::Runtime::start(&config(50), Ok(root.clone())).await.unwrap();
        let endpoint = Endpoint::new(&runtime, &config(50), root, identity.clone());

        let exit = endpoint
            .run(listener, Duration::from_millis(50), std::future::pending())
            .await;
        assert_eq!(exit, Exit::Idle);
        assert!(
            crate::hooks::HookListener::acquire(&identity).await.unwrap().is_some(),
            "the endpoint was still held when run returned"
        );
        runtime.shutdown().await;
    }

    /// A handshake read after the endpoint stopped accepting gets no reply,
    /// which is what sends its client to start a fresh backend.
    #[tokio::test]
    async fn test_a_handshake_during_the_drain_gets_no_reply() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let identity = temp_identity(&root);
        let runtime = crate::Runtime::start(&config(60_000), Ok(root.clone())).await.unwrap();
        let endpoint = Endpoint::new(&runtime, &config(60_000), root.clone(), identity);
        endpoint.closing.send_replace(true);

        let (mut client, server) = tokio::io::duplex(4096);
        let served = tokio::spawn(Arc::clone(&endpoint).connection(Box::new(server)));
        handshake::write(&mut client, &Handshake::mcp(root, None, ConfigStamp::of(&config(0))))
            .await
            .unwrap();
        let reply = tokio::time::timeout(
            Duration::from_secs(5),
            handshake::read::<_, HandshakeReply>(&mut client),
        )
        .await
        .expect("the connection hung up rather than waiting");
        assert!(reply.is_err(), "a closing endpoint answered a handshake: {reply:?}");
        served.await.unwrap();
        runtime.shutdown().await;
    }
}
```

Add to `mod tests` in `crates/mcpls-core/src/hooks/service.rs`, beside `test_serve_with_binds_the_socket_and_answers_as_its_owner`:

```rust
    /// An in-process mcpls (`--no-backend`) that finds the project's
    /// endpoint already held serves its own session without hooks, and
    /// tells that session so.
    #[tokio::test]
    async fn test_an_in_process_server_names_an_endpoint_already_held() {
        let (dir, identity) = temp_identity();
        let workspace = tempfile::tempdir().expect("a temp dir");
        let root = dunce::canonicalize(workspace.path()).expect("a canonical workspace");
        let _held = crate::hooks::HookListener::acquire(&identity)
            .await
            .expect("acquire")
            .expect("the endpoint is free");
        let config = bare_config_over(&root);
        let runtime = crate::Runtime::start(&config, Ok(root.clone()))
            .await
            .expect("a runtime");

        let notes = crate::serve_hooks_in_process(&runtime, &config, Some(identity), Some(root)).await;

        assert_eq!(notes, vec![crate::ENDPOINT_HELD_NOTE.to_string()]);
        runtime.shutdown().await;
        drop(dir);
    }
```

It drives no transport, so it needs no `transport-http` gate. `temp_identity` and `bare_config_over` are that module's helpers; if either is gated on a feature, gate this test the same way.

Drop any import clippy reports unused (`ConnectionKind` is only needed by the implementation).

Add `pub mod endpoint;` and `pub use endpoint::serve_backend;` to `crates/mcpls-core/src/backend/mod.rs`.

- [ ] **Step 2: See them fail**

Run: `devrun task check`
Expected: FAIL to compile: `cannot find function serve_backend_on`, `cannot find type Endpoint`, `cannot find value ENDPOINT_HELD_NOTE`.

- [ ] **Step 3: Implement the endpoint**

Put the implementation above the test module in `crates/mcpls-core/src/backend/endpoint.rs`:

```rust
//! The backend's side of the project endpoint: one accept loop dispatching
//! MCP sessions, hook calls and shutdown requests, and the idle timer that
//! ends the process.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::backend::handshake::{self, ConfigStamp, ConnectionKind, Handshake, HandshakeReply, Refusal};
use crate::bridge::{ConnectionId, SessionId, lock_std};
use crate::config::ServerConfig;
use crate::error::Error;
use crate::hooks::listener::{HookStream, serve_hook_connection};
use crate::hooks::{self, HookListener, HookLocation, HookStats, Request, Response, SocketIdentity};
use crate::mcp::McplsServer;
use crate::transport::ShutdownSignal;

/// Why the accept loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    /// No session was attached for the whole idle timer.
    Idle,
    /// A client asked, with no session attached.
    Shutdown,
    /// The process was signalled.
    Signal,
}

type HookHandler =
    dyn Fn(Request) -> futures::future::BoxFuture<'static, Response> + Send + Sync + 'static;

/// Run a backend for `root` until it has been idle for its configured
/// timer, then drain it.
///
/// Returns without serving when another process already holds the
/// endpoint.
///
/// # Errors
///
/// Returns an error when the endpoint cannot be derived or bound, or the
/// configuration is invalid.
pub async fn serve_backend(config: ServerConfig, root: PathBuf) -> Result<(), Error> {
    let identity = hooks::identity_for(&root)?;
    serve_backend_on(config, root, identity).await
}

pub(crate) async fn serve_backend_on(
    config: ServerConfig,
    root: PathBuf,
    identity: SocketIdentity,
) -> Result<(), Error> {
    let mut signal = ShutdownSignal::new();
    let Some(listener) = HookListener::acquire(&identity).await? else {
        info!("another backend already holds {}, exiting", identity.socket.display());
        return Ok(());
    };
    let runtime = crate::Runtime::start(&config, Ok(root.clone())).await?;
    let idle = Duration::from_millis(config.backend.idle_shutdown_ms);
    let endpoint = Endpoint::new(&runtime, &config, root, identity);
    let exit = endpoint.run(listener, idle, signal.recv()).await;
    info!(?exit, "backend stopping");
    runtime.shutdown().await;
    Ok(())
}

/// The shared state every connection task reads.
pub(crate) struct Endpoint {
    template: McplsServer,
    handler: Arc<HookHandler>,
    attachments: Arc<Attachments>,
    stamp: ConfigStamp,
    hooks_enabled: bool,
    op_deadline: Duration,
    closing: watch::Sender<bool>,
    shutdown: watch::Sender<bool>,
}

impl Endpoint {
    pub(crate) fn new(
        runtime: &crate::Runtime,
        config: &ServerConfig,
        root: PathBuf,
        identity: SocketIdentity,
    ) -> Arc<Self> {
        let template = McplsServer::from_context(Arc::clone(&runtime.context));
        let handler = hooks::build_handler(
            Arc::new(template.clone()),
            Arc::clone(&runtime.sweeper),
            HookLocation { identity, root },
            Arc::new(HookStats::default()),
            runtime.cancel_rx.clone(),
        );
        Arc::new(Self {
            template,
            handler: Arc::new(handler),
            attachments: Arc::new(Attachments::default()),
            stamp: ConfigStamp::of(config),
            hooks_enabled: config.diagnostics.hooks.enabled,
            op_deadline: Duration::from_millis(config.diagnostics.hooks.op_deadline_ms),
            closing: watch::channel(false).0,
            shutdown: watch::channel(false).0,
        })
    }

    /// Accept until idle, asked, or signalled; then stop accepting and
    /// close every open stream, in that order.
    pub(crate) async fn run(
        self: Arc<Self>,
        listener: HookListener,
        idle: Duration,
        signal: impl std::future::Future<Output = ()>,
    ) -> Exit {
        let mut tasks = tokio::task::JoinSet::new();
        let idle_expired = idle_expired(self.attachments.watch(), idle);
        let mut shutdown = self.shutdown.subscribe();
        tokio::pin!(idle_expired, signal);
        let exit = loop {
            tokio::select! {
                () = &mut signal => break Exit::Signal,
                () = &mut idle_expired => break Exit::Idle,
                _ = shutdown.wait_for(|asked| *asked) => break Exit::Shutdown,
                accepted = listener.accept() => match accepted {
                    Ok(stream) => {
                        while tasks.try_join_next().is_some() {}
                        tasks.spawn(Arc::clone(&self).connection(stream));
                    }
                    Err(error) => {
                        warn!(%error, "the endpoint failed to accept a connection");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                },
            }
        };
        drop(listener);
        self.closing.send_replace(true);
        if tokio::time::timeout(Duration::from_secs(2), async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            warn!("some connections did not close within 2s and were abandoned");
            tasks.abort_all();
        }
        exit
    }

    async fn connection(self: Arc<Self>, mut stream: Box<dyn HookStream>) {
        let Ok(Ok(request)) = tokio::time::timeout(
            handshake::HANDSHAKE_TIMEOUT,
            handshake::read::<_, Handshake>(&mut stream),
        )
        .await
        else {
            return;
        };
        // A backend that has stopped accepting hangs up without a reply, so
        // its client starts a fresh backend rather than attaching to this one.
        if *self.closing.borrow() {
            return;
        }
        let sessions = self.attachments.count();
        let refusal = self.refusal_for(&request, sessions);
        let refused = refusal.is_some();
        if handshake::write(&mut stream, &HandshakeReply::new(sessions, refusal))
            .await
            .is_err()
            || refused
        {
            return;
        }
        let mut closing = self.closing.subscribe();
        match request.kind {
            ConnectionKind::Hook => {
                tokio::select! {
                    () = serve_hook_connection(stream, Arc::clone(&self.handler), self.op_deadline) => {}
                    _ = closing.wait_for(|closing| *closing) => {}
                }
            }
            ConnectionKind::Mcp => self.serve_mcp(stream, request, closing).await,
            ConnectionKind::Shutdown => {
                let _ = self.shutdown.send(true);
            }
        }
    }

    fn refusal_for(&self, request: &Handshake, sessions: usize) -> Option<Refusal> {
        if request.kind == ConnectionKind::Shutdown {
            return (sessions > 0).then_some(Refusal::Attached);
        }
        if !request.same_build() {
            return Some(Refusal::Build);
        }
        match request.kind {
            ConnectionKind::Hook => (!self.hooks_enabled).then_some(Refusal::HooksDisabled),
            ConnectionKind::Mcp => request
                .config
                .as_ref()
                .filter(|theirs| self.stamp.conflicts_with(theirs))
                .map(|_| Refusal::Trust {
                    backend: self.stamp.clone(),
                }),
            ConnectionKind::Shutdown => None,
        }
    }

    async fn serve_mcp(
        &self,
        stream: Box<dyn HookStream>,
        request: Handshake,
        mut closing: watch::Receiver<bool>,
    ) {
        use rmcp::ServiceExt as _;

        let notes = request
            .config
            .as_ref()
            .filter(|theirs| theirs.fingerprint != self.stamp.fingerprint)
            .map(|theirs| vec![fingerprint_note(&self.stamp, theirs)])
            .unwrap_or_default();
        let server = self
            .template
            .for_connection(SessionId::named(request.session))
            .with_notes(notes);
        let connection = server.connection();
        let subscriptions = Arc::clone(server.subscriptions());
        let _attached = self
            .attachments
            .attach(connection, server.session().to_string());

        let running = tokio::select! {
            served = server.serve(stream) => match served {
                Ok(running) => running,
                Err(error) => {
                    debug!(%error, %connection, "an MCP session ended before initializing");
                    return;
                }
            },
            _ = closing.wait_for(|closing| *closing) => return,
        };
        let token = running.cancellation_token();
        let closer = tokio::spawn(async move {
            let _ = closing.wait_for(|closing| *closing).await;
            token.cancel();
        });
        let _ = running.waiting().await;
        closer.abort();
        subscriptions.remove_connection(connection).await;
    }
}

fn fingerprint_note(backend: &ConfigStamp, frontend: &ConfigStamp) -> String {
    format!(
        "NOTE: this session's mcpls configuration (fingerprint {}, from {:?}) differs from the \
         one the shared backend for this project started with (fingerprint {}, from {:?}). The \
         backend's configuration is in effect. Restart every session in this checkout to apply \
         one configuration.",
        frontend.fingerprint, frontend.source, backend.fingerprint, backend.source
    )
}

/// Resolves once nothing has been attached for `idle` without a break.
async fn idle_expired(mut count: watch::Receiver<usize>, idle: Duration) {
    loop {
        if count.wait_for(|attached| *attached == 0).await.is_err() {
            return std::future::pending().await;
        }
        tokio::select! {
            () = tokio::time::sleep(idle) => return,
            changed = count.wait_for(|attached| *attached > 0) => {
                if changed.is_err() {
                    return std::future::pending().await;
                }
            }
        }
    }
}

/// The MCP sessions attached to this backend.
#[derive(Default)]
pub(crate) struct Attachments {
    sessions: std::sync::Mutex<BTreeMap<ConnectionId, String>>,
    count: watch::Sender<usize>,
}

/// Detaches its connection when dropped.
pub(crate) struct Attached {
    attachments: Arc<Attachments>,
    connection: ConnectionId,
}

impl Attachments {
    pub(crate) fn attach(self: &Arc<Self>, connection: ConnectionId, session: String) -> Attached {
        let count = {
            let mut sessions = lock_std(&self.sessions);
            sessions.insert(connection, session);
            sessions.len()
        };
        self.count.send_replace(count);
        Attached {
            attachments: Arc::clone(self),
            connection,
        }
    }

    pub(crate) fn count(&self) -> usize {
        *self.count.borrow()
    }

    pub(crate) fn sessions(&self) -> Vec<String> {
        lock_std(&self.sessions).values().cloned().collect()
    }

    pub(crate) fn watch(&self) -> watch::Receiver<usize> {
        self.count.subscribe()
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let count = {
            let mut sessions = lock_std(&self.attachments.sessions);
            sessions.remove(&self.connection);
            sessions.len()
        };
        self.attachments.count.send_replace(count);
    }
}
```

`watch::Sender<usize>` has no `Default`; replace `#[derive(Default)]` on `Attachments` with a manual `impl Default` building `watch::channel(0).0`. `ShutdownSignal` is `pub(crate)` in `transport.rs` already. If `build_handler` returns an `impl Fn` that `Arc::new` cannot coerce to `Arc<HookHandler>` directly, annotate: `let handler: Arc<HookHandler> = Arc::new(handler);`.

The accept arm sleeps 50 ms after an error to avoid spinning; it holds no lock while sleeping. `closing` is set with `send_replace`, because `send` leaves the value unchanged when no receiver exists, and a connection still reading its handshake reads the value rather than subscribing. `Refusal` has no variant for a differing root: both sides hash the canonical root into the endpoint name, so a client never reaches a backend for another root, and `Handshake::root` stays in the request because the request line is frozen.

Remove the `#[allow(dead_code)]` Task 6a put on `HookListener::accept`, and the `#[cfg_attr(not(test), allow(dead_code))]` Task 1 put on `McplsServer::with_notes`.

- [ ] **Step 4: Tell an in-process session the endpoint is held**

In `crates/mcpls-core/src/lib.rs`, replace `serve_hooks_in_process` with:

```rust
/// Bind the project's endpoint for hooks when hooks are on and no other
/// process holds it, and answer hooks from `runtime` until it is cancelled.
/// Returns the notes the in-process session is told.
async fn serve_hooks_in_process(
    runtime: &Runtime,
    config: &ServerConfig,
    identity_override: Option<hooks::SocketIdentity>,
    hook_root: Option<PathBuf>,
) -> Vec<String> {
    if !config.diagnostics.hooks.enabled {
        return Vec::new();
    }
    let identity = match identity_override.map_or_else(
        || hook_root.as_deref().map(hooks::identity_for).transpose(),
        |identity| Ok(Some(identity)),
    ) {
        Ok(Some(identity)) => identity,
        Ok(None) => return Vec::new(),
        Err(error) => {
            warn!("hooks are configured on but this project's endpoint could not be derived: {error}");
            return Vec::new();
        }
    };
    let listener = match hooks::HookListener::acquire(&identity).await {
        Ok(Some(listener)) => listener,
        Ok(None) => return vec![ENDPOINT_HELD_NOTE.to_string()],
        Err(error) => {
            warn!("the project's endpoint could not be bound, so no hooks are served: {error}");
            return Vec::new();
        }
    };
    let root = hook_root.unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|dir| dunce::canonicalize(dir).ok())
            .unwrap_or_default()
    });
    let handler = hooks::build_handler(
        Arc::new(mcp::McplsServer::from_context(Arc::clone(&runtime.context))),
        Arc::clone(&runtime.sweeper),
        hooks::HookLocation { identity, root },
        Arc::new(hooks::HookStats::default()),
        runtime.cancel_rx.clone(),
    );
    let op_deadline = Duration::from_millis(config.diagnostics.hooks.op_deadline_ms);
    let cancel = runtime.cancel_rx.clone();
    tokio::spawn(log_hook_task_panic(async move {
        let _ = listener.serve(handler, op_deadline, cancel).await;
    }));
    Vec::new()
}

/// Told to a session whose in-process mcpls found the project's endpoint
/// already held.
const ENDPOINT_HELD_NOTE: &str = "NOTE: another mcpls already serves this project's \
    endpoint, so the plugin's hooks reach that process and not this one; diagnostics \
    delivered through hooks are not this session's.";
```

In `serve_with_identity`, replace the `serve_hooks_in_process` call and the `mcp_server` line with:

```rust
    let notes = serve_hooks_in_process(&runtime, &config, identity_override, hook_root).await;
    let mcp_server = mcp::McplsServer::from_context(Arc::clone(&runtime.context)).with_notes(notes);
```

- [ ] **Step 5: Run the tests**

Run: `devrun task test`
Expected: PASS, including the eleven endpoint tests and `test_an_in_process_server_names_an_endpoint_already_held`. `test_a_backend_nobody_attaches_to_exits_after_the_idle_timer` also proves the empty-config runtime drains promptly.

- [ ] **Step 6: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates/mcpls-core
git commit -m "feat(backend): serve many sessions in one process"
```

---

### Task 7: start a detached backend exactly once

**Files:**
- Modify: `crates/mcpls-core/src/hooks/identity.rs:26-36` (`SocketIdentity` methods), `:155-190` (Windows lock path), `:262-294` (runtime directory on every platform)
- Modify: `crates/mcpls-core/src/hooks/listener.rs:80-93` (`ensure_private_dir` visibility)
- Create: `crates/mcpls-core/src/backend/spawn.rs`
- Modify: `crates/mcpls-core/src/backend/mod.rs`
- Test: `crates/mcpls-core/src/backend/spawn.rs`, `crates/mcpls-core/src/hooks/identity.rs`

**Interfaces:**
- Consumes: `listener::connect` (Task 5), `HookListener::acquire` (existing), `dirs::data_local_dir` (existing `mcpls-core` dependency).
- Produces:
  - `SocketIdentity::spawn_lock(&self) -> PathBuf` (`{hash}.spawn.lock`), `log_file(&self) -> PathBuf` (`{hash}.log`), `start_request(&self) -> PathBuf` (`{hash}.start`), all siblings of `lock`.
  - `pub struct BackendLaunch { pub root: PathBuf, pub config: Option<PathBuf>, pub trust_project_config: bool, pub log_level: String, pub log_json: bool }` with `args(&self) -> Vec<OsString>`.
  - `pub struct SpawnLock` with `pub async fn acquire(path: &Path, wait: Duration) -> io::Result<Option<SpawnLock>>`.
  - `pub fn spawn_detached(exe: &Path, launch: &BackendLaunch, log: &Path) -> io::Result<u32>`.
  - `pub fn request_start(identity: &SocketIdentity, launch: &BackendLaunch) -> io::Result<()>`.
  - `pub async fn start_requested(identity: &SocketIdentity, exe: &Path) -> io::Result<bool>`.
  - `pub(crate) fn ensure_runtime_dir(dir: &Path) -> io::Result<()>`.

The spawn lock is its own file, separate from the ownership lock, so each lock keeps one meaning. The backend's three standard streams never inherit the frontend's: its stdin and stdout are the host's MCP pipes, and a child holding them would keep the host from seeing EOF and could write into the MCP stream.

- [ ] **Step 1: Write the failing tests**

Add to `crates/mcpls-core/src/hooks/identity.rs` tests:

```rust
    #[test]
    fn test_the_backend_files_sit_beside_the_lock() {
        let identity = SocketIdentity {
            hash: "abc".to_string(),
            socket: PathBuf::from("/run/mcpls-ada/abc.sock"),
            lock: PathBuf::from("/run/mcpls-ada/abc.lock"),
        };
        assert_eq!(identity.spawn_lock(), PathBuf::from("/run/mcpls-ada/abc.spawn.lock"));
        assert_eq!(identity.log_file(), PathBuf::from("/run/mcpls-ada/abc.log"));
        assert_eq!(identity.start_request(), PathBuf::from("/run/mcpls-ada/abc.start"));
    }

    /// Windows needs somewhere for the spawn lock and the backend log even
    /// though the pipe itself provides exclusivity.
    #[test]
    #[cfg(windows)]
    fn test_a_windows_identity_has_a_lock_path() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let identity = identity_for(dir.path()).expect("identity");
        assert!(identity.lock.ends_with(format!("{}.lock", identity.hash)));
    }

    /// The Windows directory is a known folder, which no environment
    /// variable redirects, and the shared temporary one only when the
    /// platform names no such folder.
    #[test]
    fn test_the_windows_runtime_dir_is_under_local_app_data() {
        let local = PathBuf::from("C:/Users/ada/AppData/Local");
        assert_eq!(
            local_app_data_runtime_dir(Some(local.clone()), Some("ada".to_string())),
            local.join("mcpls")
        );
        assert_eq!(
            local_app_data_runtime_dir(None, Some("ada".to_string())),
            shared_temp_runtime_dir(Some("ada".to_string()))
        );
    }
```

Create `crates/mcpls-core/src/backend/spawn.rs` with its tests first:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn launch(root: &Path) -> BackendLaunch {
        BackendLaunch {
            root: root.to_path_buf(),
            config: Some(PathBuf::from("/etc/mcpls.toml")),
            trust_project_config: true,
            log_level: "debug".to_string(),
            log_json: true,
        }
    }

    #[test]
    fn test_the_launch_arguments_name_every_setting() {
        let args = launch(Path::new("/work")).args();
        let args: Vec<_> = args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args,
            [
                "--config", "/etc/mcpls.toml", "--trust-project-config", "--log-level", "debug",
                "--log-json", "backend", "--root", "/work",
            ]
        );
    }

    #[test]
    fn test_optional_launch_arguments_are_left_out() {
        let bare = BackendLaunch {
            config: None,
            trust_project_config: false,
            log_json: false,
            ..launch(Path::new("/work"))
        };
        let args: Vec<_> = bare.args().iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["--log-level", "debug", "backend", "--root", "/work"]);
    }

    #[tokio::test]
    async fn test_the_spawn_lock_admits_one_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime").join("x.spawn.lock");
        let first = SpawnLock::acquire(&path, Duration::ZERO).await.unwrap();
        assert!(first.is_some());
        assert!(SpawnLock::acquire(&path, Duration::from_millis(100)).await.unwrap().is_none());
        drop(first);
        assert!(SpawnLock::acquire(&path, Duration::from_secs(2)).await.unwrap().is_some());
    }

    #[test]
    fn test_an_oversized_log_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("x.log");
        std::fs::write(&log, vec![b'x'; usize::try_from(MAX_LOG_BYTES).unwrap() + 1]).unwrap();
        drop(open_log(&log).unwrap());
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);

        std::fs::write(&log, b"kept").unwrap();
        drop(open_log(&log).unwrap());
        assert_eq!(std::fs::read(&log).unwrap(), b"kept");
    }

    /// The backend leaves the frontend's process group, which is the group
    /// Codex signals when a session ends.
    #[test]
    #[cfg(target_os = "linux")]
    fn test_a_detached_child_leads_its_own_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("group");
        let script = format!(
            "awk '{{print $5 == $1}}' /proc/self/stat > '{}'",
            out.display()
        );
        let pid = spawn_detached_command(
            Path::new("/bin/sh"),
            [OsString::from("-c"), OsString::from(script)],
            dir.path(),
            &dir.path().join("log"),
        )
        .unwrap();
        assert!(pid > 0);
        for _ in 0..100 {
            if std::fs::read_to_string(&out).is_ok_and(|text| !text.is_empty()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "1");
    }

    #[tokio::test]
    async fn test_no_request_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path());
        assert!(!start_requested(&identity, Path::new("/bin/true")).await.unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_request_is_honoured_once() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path());
        request_start(&identity, &launch(dir.path())).unwrap();
        assert!(identity.start_request().exists());

        assert!(start_requested(&identity, Path::new("/bin/true")).await.unwrap());
        assert!(!identity.start_request().exists());
        assert!(!start_requested(&identity, Path::new("/bin/true")).await.unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_request_for_a_running_backend_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path());
        let _listener = crate::hooks::HookListener::acquire(&identity).await.unwrap().unwrap();
        request_start(&identity, &launch(dir.path())).unwrap();

        assert!(!start_requested(&identity, Path::new("/bin/false")).await.unwrap());
        assert!(!identity.start_request().exists());
    }

    fn test_identity(dir: &Path) -> SocketIdentity {
        SocketIdentity {
            hash: "t".to_string(),
            #[cfg(not(windows))]
            socket: dir.join("t.sock"),
            #[cfg(windows)]
            socket: PathBuf::from(format!(r"\\.\pipe\mcpls-spawn-test-{}", std::process::id())),
            lock: dir.join("t.lock"),
        }
    }
}
```

Add `pub mod spawn;` and `pub use spawn::{BackendLaunch, SpawnLock, request_start, spawn_detached, start_requested};` to `backend/mod.rs`.

- [ ] **Step 2: See them fail**

Run: `devrun task check`
Expected: FAIL to compile on the missing items.

- [ ] **Step 3: Sibling paths and a Windows runtime directory**

In `crates/mcpls-core/src/hooks/identity.rs`, change `SocketIdentity::lock`'s doc to "The lock file whose holder owns `socket` on Unix. On Windows the pipe itself is exclusive and this file is never locked, but the backend's other files sit beside it." and add:

```rust
impl SocketIdentity {
    /// The lock a frontend or hook holds while starting a backend.
    #[must_use]
    pub fn spawn_lock(&self) -> PathBuf {
        self.lock.with_extension("spawn.lock")
    }

    /// Where a detached backend's standard error goes.
    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.lock.with_extension("log")
    }

    /// Where a Windows frontend asks the next hook to start a backend.
    #[must_use]
    pub fn start_request(&self) -> PathBuf {
        self.lock.with_extension("start")
    }
}
```

Remove `#[cfg(not(windows))]` from `shared_temp_runtime_dir` and from `test_a_shared_temp_runtime_dir_carries_the_user`; `test_runtime_dir_ignores_the_xdg_variable` stays Unix-only. Keep the Unix `runtime_dir`, which stays under `TMPDIR`, and update its doc to say it holds the socket, lock, spawn lock, log and start request. Add beside it:

```rust
/// Where the lock, spawn lock, backend log and start request go on Windows:
/// `mcpls` in the user's local application data folder.
///
/// `dirs` resolves that folder through `SHGetKnownFolderPath`, which ignores
/// environment overrides, so a frontend and a hook launched by a harness
/// with a different `%TEMP%` agree on the path. The profile folder's access
/// control is already per-user.
#[cfg(windows)]
fn runtime_dir() -> PathBuf {
    local_app_data_runtime_dir(dirs::data_local_dir(), current_user())
}

/// `mcpls` under `local`, or the shared temporary runtime directory for
/// `user` when the platform names no local application data folder.
#[cfg(any(windows, test))]
fn local_app_data_runtime_dir(local: Option<PathBuf>, user: Option<String>) -> PathBuf {
    local.map_or_else(|| shared_temp_runtime_dir(user), |local| local.join("mcpls"))
}
```

In `identity_for`'s Windows branch, set `lock: runtime_dir().join(format!("{hash}.lock"))`. `dirs` is already a dependency of `mcpls-core` (`crates/mcpls-core/Cargo.toml`).

The directory is created on first use by `ensure_runtime_dir` (Step 4). On Unix that is `ensure_private_dir`; `ensure_private_dir` is Unix-only, so on Windows it is `create_dir_all`, and the profile folder's ACL keeps other users out.

In `crates/mcpls-core/src/hooks/listener.rs`, make `ensure_private_dir` `pub(crate)`.

- [ ] **Step 4: Implement spawning**

Put above the tests in `crates/mcpls-core/src/backend/spawn.rs`:

```rust
//! Starting a backend: the arguments it runs with, the lock that keeps two
//! starters from racing, and the detached spawn itself.
//!
//! On Unix a frontend spawns the backend. On Windows a host may place its
//! MCP server in a job that kills every descendant when the session ends,
//! so the frontend writes a start request and the next hook invocation,
//! which runs outside that job, spawns it.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::hooks::SocketIdentity;

/// A backend log larger than this starts over when a backend is spawned.
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// How long a hook waits to learn whether a backend already answers.
const RUNNING_PROBE: Duration = Duration::from_millis(200);

/// What a backend is started with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendLaunch {
    /// The checkout root it serves, and its working directory.
    pub root: PathBuf,
    /// An explicit configuration file, already absolute.
    pub config: Option<PathBuf>,
    /// Whether the project's `mcpls.toml` is trusted.
    pub trust_project_config: bool,
    /// The log level.
    pub log_level: String,
    /// Whether logs are JSON.
    pub log_json: bool,
}

impl BackendLaunch {
    /// The command-line arguments that start this backend.
    #[must_use]
    pub fn args(&self) -> Vec<OsString> {
        let mut args = Vec::new();
        if let Some(config) = &self.config {
            args.push("--config".into());
            args.push(config.clone().into_os_string());
        }
        if self.trust_project_config {
            args.push("--trust-project-config".into());
        }
        args.push("--log-level".into());
        args.push(self.log_level.clone().into());
        if self.log_json {
            args.push("--log-json".into());
        }
        args.push("backend".into());
        args.push("--root".into());
        args.push(self.root.clone().into_os_string());
        args
    }
}

/// Held by whoever is starting a backend, so everyone else waits and then
/// connects to it instead of starting a second.
pub struct SpawnLock {
    _file: File,
}

impl SpawnLock {
    /// Take the lock at `path`, waiting up to `wait` for another holder.
    /// `None` when the wait ran out.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock file cannot be created or locked for
    /// a reason other than contention.
    pub async fn acquire(path: &Path, wait: Duration) -> io::Result<Option<Self>> {
        let deadline = Instant::now() + wait;
        loop {
            let attempt = path.to_path_buf();
            let locked = tokio::task::spawn_blocking(move || try_lock(&attempt))
                .await
                .map_err(io::Error::other)??;
            if locked.is_some() || Instant::now() >= deadline {
                return Ok(locked);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn try_lock(path: &Path) -> io::Result<Option<SpawnLock>> {
    use fs4::fs_std::FileExt as _;

    if let Some(parent) = path.parent() {
        ensure_runtime_dir(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(SpawnLock { _file: file })),
        Err(error)
            if error.kind() == io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs4::lock_contended_error().raw_os_error() =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Create the runtime directory, owner-only where the platform has modes.
pub(crate) fn ensure_runtime_dir(dir: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        crate::hooks::listener::ensure_private_dir(dir)
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Open the backend log for appending, starting it over past
/// [`MAX_LOG_BYTES`].
fn open_log(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        ensure_runtime_dir(parent)?;
    }
    let oversized = std::fs::metadata(path).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES);
    std::fs::OpenOptions::new()
        .create(true)
        .append(!oversized)
        .write(true)
        .truncate(oversized)
        .open(path)
}

/// Start `exe` as a backend for `launch`, detached from this process's
/// standard streams and process group. Returns its pid.
///
/// # Errors
///
/// Returns an error when the log cannot be opened or the process cannot be
/// spawned.
pub fn spawn_detached(exe: &Path, launch: &BackendLaunch, log: &Path) -> io::Result<u32> {
    spawn_detached_command(exe, launch.args(), &launch.root, log)
}

fn spawn_detached_command(
    exe: &Path,
    args: impl IntoIterator<Item = OsString>,
    cwd: &Path,
    log: &Path,
) -> io::Result<u32> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(open_log(log)?))
        .env_remove("CLAUDE_CODE_SESSION_ID");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = command.spawn()?;
    let pid = child.id();
    // Reaped here so an exiting backend never lingers as a zombie of this
    // process.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

/// Ask the next hook invocation to start a backend for `launch`.
///
/// # Errors
///
/// Returns an error when the request cannot be written.
pub fn request_start(identity: &SocketIdentity, launch: &BackendLaunch) -> io::Result<()> {
    let path = identity.start_request();
    if let Some(parent) = path.parent() {
        ensure_runtime_dir(parent)?;
    }
    let pending = path.with_extension("start.tmp");
    std::fs::write(&pending, serde_json::to_vec(launch).map_err(io::Error::other)?)?;
    std::fs::rename(pending, path)
}

/// Start the backend a frontend asked for, when one asked and none runs.
/// Returns whether this call spawned one.
///
/// # Errors
///
/// Returns an error when the request is unreadable or the spawn fails.
pub async fn start_requested(identity: &SocketIdentity, exe: &Path) -> io::Result<bool> {
    let request = identity.start_request();
    if !request.exists() {
        return Ok(false);
    }
    let Some(_lock) = SpawnLock::acquire(&identity.spawn_lock(), Duration::ZERO).await? else {
        return Ok(false);
    };
    let Ok(bytes) = std::fs::read(&request) else {
        return Ok(false);
    };
    let running = tokio::time::timeout(RUNNING_PROBE, crate::hooks::listener::connect(identity))
        .await
        .is_ok_and(|connected| connected.is_ok());
    std::fs::remove_file(&request)?;
    if running {
        return Ok(false);
    }
    let launch: BackendLaunch = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    spawn_detached(exe, &launch, &identity.log_file())?;
    Ok(true)
}
```

`fs4::lock_contended_error` is `fs4-0.12.0/src/lib.rs:194`. The `test_a_detached_child_leads_its_own_process_group` script compares field 5 (process group) to field 1 (pid) of `/proc/self/stat`; `awk` is the child reading its own stat, and a comm with spaces would shift fields, which `sh` never has.

`start_requested` spawns `exe` with arguments `/bin/true` ignores, which is what the Unix test relies on.

- [ ] **Step 5: Run the tests**

Run: `devrun task test`
Expected: PASS, including the identity test and the spawn tests.

- [ ] **Step 6: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates/mcpls-core
git commit -m "feat(backend): spawn one detached backend"
```

---

### Task 8a: the frontend attaches, starts and evicts

**Files:**
- Create: `crates/mcpls-core/src/backend/stub.rs`, `crates/mcpls-core/src/backend/frontend.rs`
- Modify: `crates/mcpls-core/src/backend/mod.rs`
- Modify: `crates/mcpls-core/src/mcp/server.rs:1754-1760` (`INSTRUCTIONS` constant), `crates/mcpls-core/src/mcp/mod.rs` (its re-export)
- Test: `crates/mcpls-core/src/backend/stub.rs`, `crates/mcpls-core/src/backend/frontend.rs` (`mod fake`, `mod attach_tests`)

**Interfaces:**
- Consumes: `ConfigSource` (Task 3); `ConfigStamp`, `ConnectionKind`, `Handshake`, `HandshakeReply`, `Refusal`, `compare_builds`, `VERSION`, `handshake::{read, write, HANDSHAKE_TIMEOUT}`, `HookStream` (Task 5); `McplsServer::new` (existing).
- Produces:
  - `pub(crate) const INSTRUCTIONS: &str` in `mcp::server`, re-exported as `crate::mcp::INSTRUCTIONS`.
  - `pub(crate) fn answer(message: &serde_json::Value, reason: &str) -> Option<serde_json::Value>` in `backend::stub`: the response to write to the host, or `None` for a message that expects none.
  - In `backend::frontend`:
    - `pub(crate) trait Door: Send + Sync + 'static` with `fn connect(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>>`, `fn lock(&self, wait: Duration) -> BoxFuture<'_, io::Result<Option<Box<dyn Send>>>>`, `fn start(&self) -> BoxFuture<'_, io::Result<Start>>`, `fn place(&self) -> Place`.
    - `pub(crate) enum Start { Spawned, Requested }` and `pub(crate) struct Place { pub(crate) root: PathBuf, pub(crate) log: PathBuf }`.
    - `pub(crate) struct Timing { pub(crate) start: Duration, pub(crate) gone: Duration }` with `Default` (10 s, 5 s).
    - `pub(crate) enum Outcome { Attached(Box<dyn HookStream>), Waiting(String), Failed(String) }` and `pub(crate) async fn attach(door: &dyn Door, request: &Handshake, timing: &Timing) -> Outcome`.
    - `const CONNECT_TIMEOUT: Duration`.
    - `mod messages` with `pub(super) fn start_failed(place: &Place, error: &std::io::Error)`, `did_not_start(place: &Place, wait: Duration)`, `busy_starting(place: &Place, wait: Duration)`, `waiting_for_hook(place: &Place)` and `refused(place: &Place, request: &Handshake, reply: &HandshakeReply)`, each returning `String`.
    - `#[cfg(test)] mod fake`, the harness both frontend test modules use: `pub(super) enum Script { Nobody, Server(HandshakeReply, Then) }`, `pub(super) enum Then { Serve, Close }`, `pub(super) struct FakeDoor` with `pub(super) fn new(start: Start, scripts: Vec<Script>) -> Arc<FakeDoor>` and fields `pub(super) starts: Mutex<usize>` and `pub(super) kinds: Arc<Mutex<Vec<ConnectionKind>>>`, and `pub(super) fn accepted() -> HandshakeReply`, `pub(super) fn refused(version: &str, sessions: usize, refusal: Refusal) -> HandshakeReply`, `pub(super) fn fast() -> Timing`, `pub(super) fn request() -> Handshake`.

One attempt to reach the backend ends attached, waiting for a hook to start it, or failed with a reason addressed to the user. This task builds that decision and what the frontend answers with no backend; relaying host traffic over it is Task 8b. Nothing outside the tests calls this code until then, so both new files open with `#![cfg_attr(not(test), allow(dead_code))]`, which Task 8b removes.

- [ ] **Step 1: Extract the instructions**

In `crates/mcpls-core/src/mcp/server.rs`, add near the top:

```rust
/// What every mcpls tells an agent about itself at `initialize`.
pub(crate) const INSTRUCTIONS: &str = concat!(
    "Universal MCP to LSP bridge. Exposes Language Server Protocol ",
    "capabilities as MCP tools for semantic code intelligence. ",
    "Supports hover, definition, references, diagnostics, rename, ",
    "completions, symbols, and formatting."
);
```

and in `get_info` use `let mut instructions = INSTRUCTIONS.to_string();`.

In `crates/mcpls-core/src/mcp/mod.rs`, which declares `mod server;` privately, add `pub(crate) use server::INSTRUCTIONS;`.

- [ ] **Step 2: Write the stub with its tests**

Create `crates/mcpls-core/src/backend/stub.rs`:

```rust
//! What the frontend answers when no backend is attached: the frozen tool
//! surface, and the reason every tool call fails.
#![cfg_attr(not(test), allow(dead_code))]

use std::sync::OnceLock;

use serde_json::{Value, json};

use crate::backend::handshake::VERSION;
use crate::mcp::INSTRUCTIONS;

fn tools() -> &'static Value {
    static TOOLS: OnceLock<Value> = OnceLock::new();
    TOOLS.get_or_init(|| {
        serde_json::from_str(include_str!("../mcp/tool_surface.json")).unwrap_or(Value::Null)
    })
}

/// The response to `message` for a session with no backend, because of
/// `reason`. `None` for a notification or a response to nothing.
///
/// A tool call fails at once, whether the session is waiting for a backend
/// or has none, so a wait reads like any other unreachable backend.
pub(crate) fn answer(message: &Value, reason: &str) -> Option<Value> {
    let id = message.get("id")?;
    let method = message.get("method").and_then(Value::as_str)?;
    let result = match method {
        "initialize" => json!({
            "protocolVersion": message["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}, "resources": {"subscribe": true}},
            "serverInfo": {"name": "mcpls", "version": VERSION},
            "instructions": format!("{INSTRUCTIONS} {reason}"),
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "resources/list" => json!({"resources": []}),
        "resources/templates/list" => json!({"resourceTemplates": []}),
        "tools/call" => json!({
            "content": [{"type": "text", "text": reason}],
            "isError": true,
        }),
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": reason},
            }));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_initialize_carries_the_reason_in_the_instructions() {
        let reply = answer(
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}),
            "No backend.",
        )
        .expect("initialize is answered");
        assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
        assert!(reply["result"]["instructions"].as_str().unwrap().ends_with(" No backend."));
        assert_eq!(reply["result"]["serverInfo"]["name"], "mcpls");
    }

    #[test]
    fn test_the_tool_list_is_the_frozen_surface() {
        let reply = answer(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}), "r")
            .expect("tools/list is answered");
        let expected: Value =
            serde_json::from_str(include_str!("../mcp/tool_surface.json")).unwrap();
        assert_eq!(reply["result"]["tools"], expected);
    }

    #[test]
    fn test_a_tool_call_fails_at_once_with_the_reason() {
        let call = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_hover"}});
        let reply = answer(&call, "Backend stopped.").expect("a tool call is answered");
        assert_eq!(reply["id"], 3);
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(reply["result"]["content"][0]["text"], "Backend stopped.");
    }

    #[test]
    fn test_notifications_are_dropped_and_unknown_requests_fail() {
        assert_eq!(
            answer(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}), "r"),
            None
        );
        let reply = answer(&json!({"jsonrpc":"2.0","id":"x","method":"resources/read"}), "gone")
            .expect("a request gets a response");
        assert_eq!(reply["error"]["message"], "gone");
        assert_eq!(reply["id"], "x");
    }
}
```

Add `mod stub;` to `crates/mcpls-core/src/backend/mod.rs`.

- [ ] **Step 3: Write the harness and the failing attach tests**

Create `crates/mcpls-core/src/backend/frontend.rs` with the harness and the attach tests; the implementation goes above them in Step 5:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod fake {
    use std::collections::VecDeque;
    use std::io;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures::future::BoxFuture;
    use rmcp::ServiceExt as _;

    use super::{Door, Place, Start, Timing};
    use crate::backend::handshake::{
        self, ConfigStamp, ConnectionKind, Handshake, HandshakeReply, Refusal,
    };
    use crate::hooks::listener::HookStream;

    /// What one connect attempt reaches.
    pub(super) enum Script {
        /// Nothing listens.
        Nobody,
        /// A server answering the handshake with the reply, then doing
        /// what `Then` says.
        Server(HandshakeReply, Then),
    }

    pub(super) enum Then {
        /// Serve a real `McplsServer` on the stream.
        Serve,
        /// Close at once.
        Close,
    }

    /// A door whose connect attempts follow a script, and which counts the
    /// starts it is asked for and the connection kinds it saw.
    pub(super) struct FakeDoor {
        scripts: Mutex<VecDeque<Script>>,
        start: Start,
        pub(super) starts: Mutex<usize>,
        pub(super) kinds: Arc<Mutex<Vec<ConnectionKind>>>,
    }

    impl FakeDoor {
        pub(super) fn new(start: Start, scripts: Vec<Script>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(scripts.into()),
                start,
                starts: Mutex::new(0),
                kinds: Arc::new(Mutex::new(Vec::new())),
            })
        }
    }

    impl Door for FakeDoor {
        fn connect(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>> {
            let script = self.scripts.lock().unwrap().pop_front().unwrap_or(Script::Nobody);
            let kinds = Arc::clone(&self.kinds);
            Box::pin(async move {
                let Script::Server(reply, then) = script else {
                    return Err(io::ErrorKind::ConnectionRefused.into());
                };
                let (client, mut server) = tokio::io::duplex(1 << 20);
                tokio::spawn(async move {
                    let Ok(request) = handshake::read::<_, Handshake>(&mut server).await else {
                        return;
                    };
                    kinds.lock().unwrap().push(request.kind);
                    if handshake::write(&mut server, &reply).await.is_err() {
                        return;
                    }
                    match then {
                        Then::Serve => {
                            if let Ok(running) = test_server().serve(server).await {
                                let _ = running.waiting().await;
                            }
                        }
                        Then::Close => {}
                    }
                });
                Ok(Box::new(client) as Box<dyn HookStream>)
            })
        }

        fn lock(&self, _wait: Duration) -> BoxFuture<'_, io::Result<Option<Box<dyn Send>>>> {
            Box::pin(async { Ok(Some(Box::new(()) as Box<dyn Send>)) })
        }

        fn start(&self) -> BoxFuture<'_, io::Result<Start>> {
            *self.starts.lock().unwrap() += 1;
            let start = self.start;
            Box::pin(async move { Ok(start) })
        }

        fn place(&self) -> Place {
            Place {
                root: PathBuf::from("/work"),
                log: PathBuf::from("/run/mcpls/x.log"),
            }
        }
    }

    fn test_server() -> crate::mcp::McplsServer {
        use crate::bridge::{DiagnosticsDelivery, FloorTable, NotificationCache, ResourceSubscriptions, ServerSettle, Translator};
        use crate::config::DiagnosticsConfig;
        crate::mcp::McplsServer::new(
            Arc::new(Translator::new()),
            Arc::new(tokio::sync::Mutex::new(NotificationCache::new())),
            Arc::from(Vec::new()),
            Arc::new(ResourceSubscriptions::new()),
            false,
            Arc::new(tokio::sync::Mutex::new(DiagnosticsDelivery::new(DiagnosticsConfig::default()))),
            Arc::new(FloorTable::new(&DiagnosticsConfig::default(), &[])),
            DiagnosticsConfig::default(),
            Arc::new(ServerSettle::new(Duration::from_secs(1), Duration::from_secs(300))),
        )
    }

    pub(super) fn accepted() -> HandshakeReply {
        HandshakeReply::new(0, None)
    }

    pub(super) fn refused(version: &str, sessions: usize, refusal: Refusal) -> HandshakeReply {
        HandshakeReply {
            version: version.to_string(),
            ..HandshakeReply::new(sessions, Some(refusal))
        }
    }

    pub(super) fn fast() -> Timing {
        Timing {
            start: Duration::from_millis(300),
            gone: Duration::from_millis(300),
        }
    }

    pub(super) fn request() -> Handshake {
        Handshake::mcp(
            PathBuf::from("/work"),
            None,
            ConfigStamp::of(&crate::config::ServerConfig::default()),
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod attach_tests {
    use std::path::PathBuf;

    use super::fake::{FakeDoor, Script, Then, accepted, fast, refused, request};
    use super::{Outcome, Start, attach};
    use crate::backend::handshake::{
        self, ConfigStamp, ConnectionKind, Handshake, HandshakeReply, Refusal,
    };

    fn failure(outcome: Outcome) -> String {
        match outcome {
            Outcome::Failed(text) => text,
            Outcome::Waiting(text) => panic!("expected a failure, got a wait: {text}"),
            Outcome::Attached(_) => panic!("expected a failure, got an attached backend"),
        }
    }

    #[tokio::test]
    async fn test_a_backend_that_never_starts_is_reported_with_its_log() {
        let door = FakeDoor::new(Start::Spawned, vec![]);
        let text = failure(attach(door.as_ref(), &request(), &fast()).await);
        assert!(text.contains("/run/mcpls/x.log"), "{text}");
        assert_eq!(*door.starts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_a_requested_start_waits_for_a_hook() {
        let door = FakeDoor::new(Start::Requested, vec![]);
        let Outcome::Waiting(text) = attach(door.as_ref(), &request(), &fast()).await else {
            panic!("a requested start waits");
        };
        assert!(text.contains("hook"), "{text}");
    }

    #[tokio::test]
    async fn test_an_idle_older_backend_is_evicted_and_replaced() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Server(refused("0.0.1", 0, Refusal::Build), Then::Close),
                Script::Server(accepted(), Then::Close),
                Script::Nobody,
                Script::Nobody,
                Script::Server(accepted(), Then::Serve),
            ],
        );
        let outcome = attach(door.as_ref(), &request(), &fast()).await;
        assert!(matches!(outcome, Outcome::Attached(_)));
        assert!(door.kinds.lock().unwrap().contains(&ConnectionKind::Shutdown));
        assert_eq!(*door.starts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_an_older_backend_with_sessions_is_reported_with_both_versions() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(refused("0.0.1", 2, Refusal::Build), Then::Close)],
        );
        let text = failure(attach(door.as_ref(), &request(), &fast()).await);
        assert!(text.contains("0.0.1") && text.contains(handshake::VERSION), "{text}");
        assert!(!door.kinds.lock().unwrap().contains(&ConnectionKind::Shutdown));
        assert_eq!(*door.starts.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_an_older_frontend_never_evicts_a_newer_backend() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(refused("999.0.0", 0, Refusal::Build), Then::Close)],
        );
        let text = failure(attach(door.as_ref(), &request(), &fast()).await);
        assert!(text.contains("older"), "{text}");
        assert!(!door.kinds.lock().unwrap().contains(&ConnectionKind::Shutdown));
    }

    #[tokio::test]
    async fn test_a_trust_refusal_names_both_states() {
        let mut backend = crate::config::ServerConfig::default();
        backend.source = crate::config::ConfigSource::Project;
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(
                HandshakeReply::new(1, Some(Refusal::Trust { backend: ConfigStamp::of(&backend) })),
                Then::Close,
            )],
        );
        let mut ignoring = crate::config::ServerConfig::default();
        ignoring.project_config_ignored = true;
        let request = Handshake::mcp(PathBuf::from("/work"), None, ConfigStamp::of(&ignoring));
        let text = failure(attach(door.as_ref(), &request, &fast()).await);
        assert!(text.contains("loaded the project's mcpls.toml"), "{text}");
        assert!(text.contains("ignored the project's mcpls.toml"), "{text}");
    }
}
```

In `test_an_idle_older_backend_is_evicted_and_replaced`, the scripts are consumed in order: the first connect finds the old build idle; the second is the shutdown connection, accepted; the next two are the frontend checking the endpoint is gone, and the re-check under the spawn lock; after the spawn the fifth serves. If the implementation connects a different number of times on this path, adjust the number of `Script::Nobody` entries to match what Step 5's `attach` does and say so in the report, rather than changing `attach` to fit the test.

Add `pub mod frontend;` to `crates/mcpls-core/src/backend/mod.rs`.

- [ ] **Step 4: See them fail**

Run: `devrun task check`
Expected: FAIL to compile: `unresolved imports super::Door`, `super::attach`, `super::Outcome`.

- [ ] **Step 5: Implement attaching**

Put above the test modules in `crates/mcpls-core/src/backend/frontend.rs`:

```rust
//! The process a host launches: attach to the project's backend and relay
//! MCP traffic, or explain why there is no backend.
#![cfg_attr(not(test), allow(dead_code))]

use std::cmp::Ordering;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::time::Instant;

use crate::backend::handshake::{self, Handshake, HandshakeReply, Refusal, compare_builds};
use crate::hooks::listener::HookStream;

/// How long one connect attempt may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// How a frontend reaches and starts a backend.
pub(crate) trait Door: Send + Sync + 'static {
    fn connect(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>>;
    fn lock(&self, wait: Duration) -> BoxFuture<'_, io::Result<Option<Box<dyn Send>>>>;
    fn start(&self) -> BoxFuture<'_, io::Result<Start>>;
    fn place(&self) -> Place;
}

/// What starting a backend did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Start {
    /// A backend process was spawned.
    Spawned,
    /// A hook was asked to spawn one.
    Requested,
}

/// Where the messages point a reader.
pub(crate) struct Place {
    pub(crate) root: PathBuf,
    pub(crate) log: PathBuf,
}

/// The frontend's waits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// How long a spawned backend has to answer.
    pub(crate) start: Duration,
    /// How long an evicted backend has to release the endpoint.
    pub(crate) gone: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            start: Duration::from_secs(10),
            gone: Duration::from_secs(5),
        }
    }
}

/// How an attach attempt ended.
pub(crate) enum Outcome {
    Attached(Box<dyn HookStream>),
    Waiting(String),
    Failed(String),
}
```

Attaching:

```rust
async fn handshake_with(
    door: &dyn Door,
    request: &Handshake,
) -> Option<(Box<dyn HookStream>, HandshakeReply)> {
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, door.connect()).await.ok()?.ok()?;
    handshake::write(&mut stream, request).await.ok()?;
    let reply = tokio::time::timeout(handshake::HANDSHAKE_TIMEOUT, handshake::read(&mut stream))
        .await
        .ok()?
        .ok()?;
    Some((stream, reply))
}

enum Judged {
    Done(Outcome),
    /// An idle older backend was asked to exit and released the endpoint.
    Evicted,
}

/// Attach to the project's backend, starting one when none answers.
pub(crate) async fn attach(door: &dyn Door, request: &Handshake, timing: &Timing) -> Outcome {
    let place = door.place();
    if let Some(found) = handshake_with(door, request).await {
        if let Judged::Done(outcome) = judge(door, request, found, timing, true).await {
            return outcome;
        }
    }
    let _lock = match door.lock(timing.start).await {
        Ok(Some(lock)) => lock,
        Ok(None) => return Outcome::Failed(messages::busy_starting(&place, timing.start)),
        Err(error) => return Outcome::Failed(messages::start_failed(&place, &error)),
    };
    if let Some(found) = handshake_with(door, request).await {
        if let Judged::Done(outcome) = judge(door, request, found, timing, false).await {
            return outcome;
        }
    }
    match door.start().await {
        Err(error) => Outcome::Failed(messages::start_failed(&place, &error)),
        Ok(Start::Requested) => Outcome::Waiting(messages::waiting_for_hook(&place)),
        Ok(Start::Spawned) => {
            let deadline = Instant::now() + timing.start;
            loop {
                if let Some(found) = handshake_with(door, request).await
                    && let Judged::Done(outcome) = judge(door, request, found, timing, false).await
                {
                    return outcome;
                }
                if Instant::now() >= deadline {
                    return Outcome::Failed(messages::did_not_start(&place, timing.start));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn judge(
    door: &dyn Door,
    request: &Handshake,
    (stream, reply): (Box<dyn HookStream>, HandshakeReply),
    timing: &Timing,
    may_evict: bool,
) -> Judged {
    let newer = compare_builds((request.mcpls, &request.version), (reply.mcpls, &reply.version))
        == Ordering::Greater;
    match &reply.refusal {
        None => Judged::Done(Outcome::Attached(stream)),
        Some(Refusal::Build) if may_evict && newer && reply.sessions == 0 => {
            drop(stream);
            if evict(door, request, timing).await {
                Judged::Evicted
            } else {
                Judged::Done(Outcome::Failed(messages::refused(&door.place(), request, &reply)))
            }
        }
        Some(_) => Judged::Done(Outcome::Failed(messages::refused(&door.place(), request, &reply))),
    }
}

/// Ask an idle backend to exit and wait for its endpoint to go.
async fn evict(door: &dyn Door, request: &Handshake, timing: &Timing) -> bool {
    let shutdown = Handshake {
        kind: handshake::ConnectionKind::Shutdown,
        ..request.clone()
    };
    let Some((_stream, reply)) = handshake_with(door, &shutdown).await else {
        return true;
    };
    if reply.refusal.is_some() {
        return false;
    }
    let deadline = Instant::now() + timing.gone;
    while Instant::now() < deadline {
        let reachable = tokio::time::timeout(CONNECT_TIMEOUT, door.connect())
            .await
            .is_ok_and(|connected| connected.is_ok());
        if !reachable {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}
```

When `judge` returns `Evicted` on the first probe, `attach` falls through to the lock and start path, which is what replaces the old backend. On the second probe and in the spawn loop `may_evict` is `false`, so an eviction can happen at most once per attach and cannot loop.

Messages, addressed to the user through the agent:

```rust
mod messages {
    use std::cmp::Ordering;
    use std::time::Duration;

    use super::Place;
    use crate::backend::handshake::{ConfigStamp, Handshake, HandshakeReply, Refusal, compare_builds};
    use crate::config::ConfigSource;

    pub(super) fn start_failed(place: &Place, error: &std::io::Error) -> String {
        format!(
            "Tell the user: mcpls could not start a backend for {}: {error}. This session has no \
             mcpls tools. Restart the session after fixing it, or run mcpls with --no-backend.",
            place.root.display()
        )
    }

    pub(super) fn did_not_start(place: &Place, wait: Duration) -> String {
        format!(
            "Tell the user: the mcpls backend for {} did not answer within {}s of being started, \
             so this session has no mcpls tools. Its log is {}. Restart the session after fixing \
             it, or run mcpls with --no-backend.",
            place.root.display(),
            wait.as_secs(),
            place.log.display()
        )
    }

    pub(super) fn busy_starting(place: &Place, wait: Duration) -> String {
        format!(
            "Tell the user: another mcpls held the lock for starting the backend for {} for over \
             {}s, so this session has no mcpls tools. Its log is {}.",
            place.root.display(),
            wait.as_secs(),
            place.log.display()
        )
    }

    pub(super) fn waiting_for_hook(place: &Place) -> String {
        format!(
            "mcpls is waiting for its backend for {}. On Windows a hook starts it, so the mcpls \
             plugin's hooks must be installed; tools answer once one has fired. If they are not \
             installed, tell the user to run mcpls with --no-backend.",
            place.root.display()
        )
    }

    fn trust_state(stamp: &ConfigStamp) -> &'static str {
        if stamp.source == ConfigSource::Project {
            "loaded the project's mcpls.toml as trusted"
        } else if stamp.project_ignored {
            "ignored the project's mcpls.toml as untrusted"
        } else {
            "does not use the project's mcpls.toml"
        }
    }

    pub(super) fn refused(place: &Place, request: &Handshake, reply: &HandshakeReply) -> String {
        let root = place.root.display();
        match &reply.refusal {
            Some(Refusal::Build) => match compare_builds(
                (request.mcpls, &request.version),
                (reply.mcpls, &reply.version),
            ) {
                Ordering::Less => format!(
                    "Tell the user: this session's mcpls is version {}, older than the version {} \
                     backend already serving {root}, so this session has no mcpls tools. Update \
                     the mcpls this session launches, or restart the session with the newer one.",
                    request.version, reply.version
                ),
                _ => format!(
                    "Tell the user: this session's mcpls is version {} but the backend serving \
                     {root} is version {} with {} other session(s) attached, so this session has \
                     no mcpls tools. Restart those sessions to upgrade the backend, then restart \
                     this one.",
                    request.version, reply.version, reply.sessions
                ),
            },
            Some(Refusal::Trust { backend }) => format!(
                "Tell the user: the mcpls backend serving {root} {}, but this session's mcpls {}. \
                 Sessions that disagree about trusting the project's mcpls.toml cannot share a \
                 backend. Start every session in this checkout with the same \
                 --trust-project-config setting.",
                trust_state(backend),
                request.config.as_ref().map_or("has no configuration", trust_state)
            ),
            other => format!(
                "Tell the user: the mcpls backend serving {root} refused this session ({other:?}), \
                 so this session has no mcpls tools."
            ),
        }
    }
}
```

The message for a backend that stops after attaching belongs to the relay, and Task 8b adds it.

- [ ] **Step 6: Run the tests**

Run: `devrun task test`
Expected: PASS, including the four stub tests and the six attach tests.

- [ ] **Step 7: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates/mcpls-core
git commit -m "feat(backend): attach a frontend to its backend"
```

---

### Task 8b: the frontend relays a session

**Files:**
- Modify: `crates/mcpls-core/src/backend/frontend.rs` (`Timing::retry`, `messages::backend_stopped`, `FrontendOptions`, `run_frontend`, `relay`, two new `fake::Then` behaviours, `mod relay_tests`)
- Modify: `crates/mcpls-core/src/backend/stub.rs` (drop the dead-code `cfg_attr`)
- Modify: `crates/mcpls-core/src/backend/mod.rs`
- Test: `crates/mcpls-core/src/backend/frontend.rs` (`mod relay_tests`)

**Interfaces:**
- Consumes: from Task 8a, `attach(door: &dyn Door, request: &Handshake, timing: &Timing) -> Outcome`, `Outcome`, `Door`, `Start`, `Place`, `Timing { start, gone }`, `messages`, `stub::answer(message: &Value, reason: &str) -> Option<Value>`, `crate::mcp::INSTRUCTIONS`, and the `fake` harness (`FakeDoor`, `Script`, `Then`, `accepted`, `refused`, `fast`, `request`); from Task 7, `BackendLaunch`, `SpawnLock::acquire`, `spawn_detached`, `request_start`, `SocketIdentity::{spawn_lock, log_file}`; from Task 5, `ConfigStamp`, `Handshake::mcp`, `listener::connect`, `HookStream`, `VERSION`; `hooks::identity_for` (existing); `SessionId::from_host_env` (Task 1).
- Produces:
  - `Timing::retry: Duration` (`pub(crate)`), default 500 ms.
  - `pub(crate) async fn relay<R, W>(host_in: R, host_out: W, door: Arc<dyn Door>, request: Handshake, timing: Timing)` where `R: AsyncRead + Unpin + Send + 'static` and `W: AsyncWrite + Unpin + Send`.
  - `pub struct FrontendOptions { pub launch: BackendLaunch, pub stamp: ConfigStamp }` and `pub async fn run_frontend(options: FrontendOptions)`, re-exported from `mcpls_core::backend`.
  - `fake::Then::{CloseOnFirstLine, AnswerOnceThenClose}`.

The frontend is not a byte pipe. To report a missing backend it answers `initialize` and `tools/list` itself, fails `tools/call` with the reason, and fails any request in flight when the backend dies. It relays everything else in both directions, including the backend's unsolicited notifications.

States, in one task driven by one event channel:

| State | Host request | Leaves when |
|---|---|---|
| `Connecting` | deferred, in order | the attach finishes: `Attached`, `Waiting` or `Failed` |
| `Attached` | forwarded; id recorded as pending | the backend stream closes. If the backend never sent a line and this is the first such close, what the host sent since the attach is deferred again and `attach` reruns (`Connecting`). Otherwise pending ids are answered with an error, then `Failed` |
| `Waiting` | answered by the stub; `tools/call` fails at once with the waiting reason | a retry attaches: the host's `initialize` is replayed, then later requests are forwarded |
| `Failed` | answered by the stub with the reason | never |

A wait is reported the way any unreachable backend is, so nothing is held back while `Waiting`. A backend that accepts the handshake and closes before sending a line is one that began exiting as this frontend attached; rerunning `attach` starts a fresh backend under the spawn lock. It reruns once, so a backend that drops every connection cannot make the frontend loop.

- [ ] **Step 1: Write the failing relay tests**

In `mod fake` in `crates/mcpls-core/src/backend/frontend.rs`, add these imports:

```rust
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
```

add two variants to `Then`:

```rust
        /// Close as soon as the first MCP line arrives, having sent nothing.
        CloseOnFirstLine,
        /// Answer the first MCP line as an `initialize`, then close as soon
        /// as the next line arrives.
        AnswerOnceThenClose,
```

add their arms to the `match then` in `FakeDoor::connect`:

```rust
                        Then::CloseOnFirstLine => {
                            let mut lines = BufReader::new(server).lines();
                            let _ = lines.next_line().await;
                        }
                        Then::AnswerOnceThenClose => {
                            let (reader, mut writer) = tokio::io::split(server);
                            let mut lines = BufReader::new(reader).lines();
                            let Ok(Some(first)) = lines.next_line().await else {
                                return;
                            };
                            let first: Value = serde_json::from_str(&first).unwrap();
                            let answer = json!({"jsonrpc":"2.0","id":first["id"],"result":{
                                "protocolVersion":"2025-11-25",
                                "capabilities":{},
                                "serverInfo":{"name":"mcpls","version":"0"},
                            }});
                            let _ = writer.write_all(format!("{answer}\n").as_bytes()).await;
                            let _ = lines.next_line().await;
                        }
```

and give `fast()` a retry: `retry: Duration::from_millis(20),` between `start` and `gone`.

Append to `crates/mcpls-core/src/backend/frontend.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod relay_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, Lines};
    use tokio::time::Instant;

    use super::fake::{FakeDoor, Script, Then, accepted, fast, refused, request};
    use super::{Door, Start, Timing, relay};
    use crate::backend::handshake::{self, Handshake, Refusal};

    struct Host {
        input: DuplexStream,
        output: Lines<BufReader<DuplexStream>>,
        relay: tokio::task::JoinHandle<()>,
    }

    impl Host {
        fn start(door: Arc<dyn Door>, request: Handshake, timing: Timing) -> Self {
            let (input, relay_in) = tokio::io::duplex(1 << 20);
            let (relay_out, output) = tokio::io::duplex(1 << 20);
            let relay = tokio::spawn(relay(relay_in, relay_out, door, request, timing));
            Self { input, output: BufReader::new(output).lines(), relay }
        }

        async fn send(&mut self, message: Value) {
            self.input.write_all(format!("{message}\n").as_bytes()).await.unwrap();
        }

        async fn response(&mut self, id: i64) -> Value {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let line = self.output.next_line().await.unwrap().expect("the relay closed");
                    let message: Value = serde_json::from_str(&line).unwrap();
                    if message["id"] == id {
                        return message;
                    }
                }
            })
            .await
            .expect("a response arrived")
        }

        async fn initialize(&mut self) -> Value {
            self.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}})).await;
            let answer = self.response(1).await;
            self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
            answer
        }

        async fn call_tool(&mut self, id: i64) -> Value {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"get_server_logs","arguments":{}}})).await;
            self.response(id).await
        }
    }

    fn initialize_request() -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}})
    }

    #[tokio::test]
    async fn test_an_attached_session_is_answered_by_the_backend() {
        let door = FakeDoor::new(Start::Spawned, vec![Script::Server(accepted(), Then::Serve)]);
        let mut host = Host::start(door, request(), fast());
        let init = host.initialize().await;
        assert_eq!(init["result"]["instructions"], crate::mcp::INSTRUCTIONS, "{init}");
        let call = host.call_tool(2).await;
        assert_ne!(call["result"]["isError"], true, "{call}");
    }

    #[tokio::test]
    async fn test_a_refusal_is_repeated_in_initialize_and_every_tool_call() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(refused("0.0.1", 2, Refusal::Build), Then::Close)],
        );
        let mut host = Host::start(door, request(), fast());
        let init = host.initialize().await;
        let text = init["result"]["instructions"].as_str().unwrap().to_string();
        assert!(text.contains("0.0.1") && text.contains(handshake::VERSION), "{text}");
        for id in [2, 3] {
            let call = host.call_tool(id).await;
            assert_eq!(call["result"]["isError"], true, "{call}");
            assert!(call["result"]["content"][0]["text"].as_str().unwrap().contains("0.0.1"), "{call}");
        }
    }

    #[tokio::test]
    async fn test_a_backend_dying_mid_request_fails_that_request_and_the_rest() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::AnswerOnceThenClose)],
        );
        let mut host = Host::start(door, request(), fast());
        host.send(initialize_request()).await;
        let init = host.response(1).await;
        assert_eq!(init["result"]["serverInfo"]["name"], "mcpls", "{init}");
        host.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_server_logs","arguments":{}}})).await;
        let failed = host.response(2).await;
        assert!(failed["error"]["message"].as_str().unwrap().contains("stopped"), "{failed}");
        let call = host.call_tool(3).await;
        assert_eq!(call["result"]["isError"], true, "{call}");
    }

    /// A backend that accepts the handshake and closes before sending
    /// anything was exiting as the frontend attached. The frontend attaches
    /// again and the host never sees the first backend.
    #[tokio::test]
    async fn test_a_backend_closing_before_its_first_line_is_attached_again() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Server(accepted(), Then::CloseOnFirstLine),
                Script::Server(accepted(), Then::Serve),
            ],
        );
        let mut host = Host::start(Arc::clone(&door) as Arc<dyn Door>, request(), fast());
        let init = host.initialize().await;
        assert_eq!(init["result"]["instructions"], crate::mcp::INSTRUCTIONS, "{init}");
        let call = host.call_tool(2).await;
        assert_ne!(call["result"]["isError"], true, "{call}");
        assert_eq!(*door.starts.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_a_second_silent_close_fails_the_session() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Server(accepted(), Then::CloseOnFirstLine),
                Script::Server(accepted(), Then::CloseOnFirstLine),
            ],
        );
        let mut host = Host::start(door, request(), fast());
        host.send(initialize_request()).await;
        let failed = host.response(1).await;
        assert!(failed["error"]["message"].as_str().unwrap().contains("stopped"), "{failed}");
    }

    #[tokio::test]
    async fn test_a_waiting_tool_call_fails_at_once() {
        let door = FakeDoor::new(Start::Requested, vec![]);
        let mut host = Host::start(door, request(), fast());
        host.initialize().await;
        let started = Instant::now();
        let call = host.call_tool(2).await;
        assert!(started.elapsed() < Duration::from_secs(1), "the call waited for a backend");
        assert_eq!(call["result"]["isError"], true, "{call}");
        assert!(call["result"]["content"][0]["text"].as_str().unwrap().contains("hook"), "{call}");
    }

    #[tokio::test]
    async fn test_a_waiting_session_replays_initialize_when_the_backend_arrives() {
        let door = FakeDoor::new(
            Start::Requested,
            vec![Script::Nobody, Script::Nobody, Script::Nobody, Script::Server(accepted(), Then::Serve)],
        );
        let timing = Timing { retry: Duration::from_millis(200), ..fast() };
        let mut host = Host::start(door, request(), timing);
        let init = host.initialize().await;
        assert!(init["result"]["instructions"].as_str().unwrap().contains("hook"), "{init}");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut id = 2;
        loop {
            let call = host.call_tool(id).await;
            if call["result"]["isError"] != true {
                break;
            }
            assert!(call["result"]["content"][0]["text"].as_str().unwrap().contains("hook"), "{call}");
            assert!(Instant::now() < deadline, "no backend attached: {call}");
            id += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn test_the_relay_ends_when_the_host_closes() {
        let door = FakeDoor::new(Start::Spawned, vec![Script::Server(accepted(), Then::Serve)]);
        let mut host = Host::start(door, request(), fast());
        host.initialize().await;
        let Host { input, relay, .. } = host;
        drop(input);
        tokio::time::timeout(Duration::from_secs(5), relay)
            .await
            .expect("the relay returned after host EOF")
            .unwrap();
    }
}
```

A successful tool call in `test_a_waiting_session_replays_initialize_when_the_backend_arrives` proves the replay: the served `McplsServer` answers no tool call before it has been initialized. The 200 ms retry keeps the host's `initialize` ahead of the attach, so the stub answers it.

- [ ] **Step 2: See them fail**

Run: `devrun task check`
Expected: FAIL to compile: `struct Timing has no field named retry`, `unresolved import super::relay`.

- [ ] **Step 3: Implement the relay**

In `crates/mcpls-core/src/backend/frontend.rs` and `crates/mcpls-core/src/backend/stub.rs`, delete the `#![cfg_attr(not(test), allow(dead_code))]` line.

Replace the top-level imports of `frontend.rs` with:

```rust
use std::cmp::Ordering;
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::backend::handshake::{
    self, ConfigStamp, Handshake, HandshakeReply, Refusal, compare_builds,
};
use crate::backend::spawn::{BackendLaunch, SpawnLock};
use crate::backend::stub;
use crate::bridge::SessionId;
use crate::hooks::listener::HookStream;
use crate::hooks::{self, SocketIdentity};
```

Replace `Timing` and its `Default` with:

```rust
/// The frontend's waits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// How long a spawned backend has to answer.
    pub(crate) start: Duration,
    /// How often a waiting frontend tries again.
    pub(crate) retry: Duration,
    /// How long an evicted backend has to release the endpoint.
    pub(crate) gone: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            start: Duration::from_secs(10),
            retry: Duration::from_millis(500),
            gone: Duration::from_secs(5),
        }
    }
}
```

Add to `mod messages`, after `waiting_for_hook`:

```rust
    pub(super) fn backend_stopped(place: &Place) -> String {
        format!(
            "Tell the user: the mcpls backend for {} stopped while this session was attached, so \
             this session has no mcpls tools. Restart the session to start a new one. Its log is \
             {}.",
            place.root.display(),
            place.log.display()
        )
    }
```

After `Outcome`, add the process's doors and entry point:

```rust
/// What the host launches mcpls with.
pub struct FrontendOptions {
    /// How to start a backend, including the checkout root.
    pub launch: BackendLaunch,
    /// This process's configuration.
    pub stamp: ConfigStamp,
}

/// Relay this process's stdio to the project's backend until the host
/// closes stdin.
pub async fn run_frontend(options: FrontendOptions) {
    let request = Handshake::mcp(
        options.launch.root.clone(),
        SessionId::from_host_env().map(|session| session.to_string()),
        options.stamp,
    );
    let door: Arc<dyn Door> = match (hooks::identity_for(&options.launch.root), std::env::current_exe()) {
        (Ok(identity), Ok(exe)) => Arc::new(ProcessDoor {
            identity,
            launch: options.launch,
            exe,
        }),
        (Err(error), _) => Arc::new(Unreachable(options.launch.root, error.to_string())),
        (_, Err(error)) => Arc::new(Unreachable(options.launch.root, error.to_string())),
    };
    relay(tokio::io::stdin(), tokio::io::stdout(), door, request, Timing::default()).await;
}

struct ProcessDoor {
    identity: SocketIdentity,
    launch: BackendLaunch,
    exe: PathBuf,
}

impl Door for ProcessDoor {
    fn connect(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>> {
        Box::pin(hooks::listener::connect(&self.identity))
    }

    fn lock(&self, wait: Duration) -> BoxFuture<'_, io::Result<Option<Box<dyn Send>>>> {
        Box::pin(async move {
            Ok(SpawnLock::acquire(&self.identity.spawn_lock(), wait)
                .await?
                .map(|lock| Box::new(lock) as Box<dyn Send>))
        })
    }

    fn start(&self) -> BoxFuture<'_, io::Result<Start>> {
        Box::pin(async move {
            #[cfg(windows)]
            {
                crate::backend::spawn::request_start(&self.identity, &self.launch)?;
                Ok(Start::Requested)
            }
            #[cfg(not(windows))]
            {
                crate::backend::spawn::spawn_detached(&self.exe, &self.launch, &self.identity.log_file())?;
                Ok(Start::Spawned)
            }
        })
    }

    fn place(&self) -> Place {
        Place {
            root: self.launch.root.clone(),
            log: self.identity.log_file(),
        }
    }
}

/// A frontend that cannot derive its endpoint at all.
struct Unreachable(PathBuf, String);

impl Door for Unreachable {
    fn connect(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>> {
        Box::pin(async { Err(io::ErrorKind::NotFound.into()) })
    }

    fn lock(&self, _wait: Duration) -> BoxFuture<'_, io::Result<Option<Box<dyn Send>>>> {
        let reason = self.1.clone();
        Box::pin(async move { Err(io::Error::other(reason)) })
    }

    fn start(&self) -> BoxFuture<'_, io::Result<Start>> {
        let reason = self.1.clone();
        Box::pin(async move { Err(io::Error::other(reason)) })
    }

    fn place(&self) -> Place {
        Place {
            root: self.0.clone(),
            log: PathBuf::new(),
        }
    }
}
```

`ProcessDoor::exe` is unused on Windows; mark the field `#[cfg_attr(windows, allow(dead_code))]`.

The relay:

```rust
/// The id a replayed `initialize` goes out under, whose answer the host
/// already had from the stub.
const REPLAY_ID: &str = "mcpls-frontend-replay";

enum Event {
    Host(String),
    HostClosed,
    Attach(Outcome),
    /// A line from the backend stream the numbered attach opened.
    Backend(u64, String),
    BackendClosed(u64),
}

enum State {
    Connecting,
    Attached,
    Waiting(String),
    Failed(String),
}

/// Relay `host_in` and `host_out` to the project's backend.
pub(crate) async fn relay<R, W>(
    host_in: R,
    mut host_out: W,
    door: Arc<dyn Door>,
    request: Handshake,
    timing: Timing,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send,
{
    let (events, mut inbox) = mpsc::unbounded_channel();
    spawn_lines(host_in, events.clone(), Event::Host, Event::HostClosed);
    spawn_attach(Arc::clone(&door), request.clone(), timing, events.clone(), Duration::ZERO);

    let place = door.place();
    let mut state = State::Connecting;
    let mut backend: Option<tokio::io::WriteHalf<Box<dyn HookStream>>> = None;
    // Events from any backend stream but the latest one are stale.
    let mut generation = 0u64;
    let mut pending: HashSet<String> = HashSet::new();
    let mut deferred: Vec<(String, Value)> = Vec::new();
    // What the host sent the current backend before it sent anything back.
    let mut unheard: Vec<(String, Value)> = Vec::new();
    let mut heard = false;
    let mut reattached = false;
    let mut init: Option<String> = None;
    let mut initialized: Option<String> = None;
    let mut init_answered = false;

    while let Some(event) = inbox.recv().await {
        match event {
            Event::HostClosed => return,
            Event::Host(line) => {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match message.get("method").and_then(Value::as_str) {
                    Some("initialize") => init = Some(line.clone()),
                    Some("notifications/initialized") => initialized = Some(line.clone()),
                    _ => {}
                }
                match &state {
                    State::Connecting => deferred.push((line, message)),
                    State::Attached => {
                        if let Some(writer) = backend.as_mut() {
                            track(&mut pending, &message);
                            if write_line(writer, &line).await.is_err() {
                                let _ = events.send(Event::BackendClosed(generation));
                            }
                            if !heard {
                                unheard.push((line, message));
                            }
                        }
                    }
                    State::Waiting(reason) | State::Failed(reason) => {
                        if let Some(reply) = stub::answer(&message, reason) {
                            init_answered |= message["method"] == "initialize";
                            write_value(&mut host_out, &reply).await;
                        }
                    }
                }
            }
            Event::Attach(Outcome::Attached(stream)) => {
                generation += 1;
                let current = generation;
                let (reader, mut writer) = tokio::io::split(stream);
                spawn_lines(
                    reader,
                    events.clone(),
                    move |line| Event::Backend(current, line),
                    Event::BackendClosed(current),
                );
                heard = false;
                if init_answered {
                    if let Some(line) = &init
                        && let Ok(mut replay) = serde_json::from_str::<Value>(line)
                    {
                        replay["id"] = Value::from(REPLAY_ID);
                        let _ = write_line(&mut writer, &replay.to_string()).await;
                    }
                    if let Some(line) = &initialized {
                        let _ = write_line(&mut writer, line).await;
                    }
                }
                for (line, message) in std::mem::take(&mut deferred) {
                    track(&mut pending, &message);
                    let _ = write_line(&mut writer, &line).await;
                    unheard.push((line, message));
                }
                backend = Some(writer);
                state = State::Attached;
            }
            Event::Attach(Outcome::Waiting(reason)) => {
                answer_from_stub(&mut host_out, &mut deferred, &reason, &mut init_answered).await;
                state = State::Waiting(reason);
                spawn_attach(Arc::clone(&door), request.clone(), timing, events.clone(), timing.retry);
            }
            Event::Attach(Outcome::Failed(reason)) => {
                answer_from_stub(&mut host_out, &mut deferred, &reason, &mut init_answered).await;
                state = State::Failed(reason);
            }
            Event::Backend(from, line) => {
                if from != generation {
                    continue;
                }
                heard = true;
                unheard.clear();
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message.get("method").is_none()
                    && let Some(id) = message.get("id")
                {
                    if *id == Value::from(REPLAY_ID) {
                        continue;
                    }
                    pending.remove(&id.to_string());
                }
                write_raw(&mut host_out, &line).await;
            }
            Event::BackendClosed(from) => {
                if from != generation || !matches!(state, State::Attached) {
                    continue;
                }
                backend = None;
                if !heard && !reattached {
                    reattached = true;
                    pending.clear();
                    deferred = std::mem::take(&mut unheard);
                    state = State::Connecting;
                    spawn_attach(Arc::clone(&door), request.clone(), timing, events.clone(), Duration::ZERO);
                    continue;
                }
                unheard.clear();
                let reason = messages::backend_stopped(&place);
                for id in pending.drain() {
                    let Ok(id) = serde_json::from_str::<Value>(&id) else {
                        continue;
                    };
                    let reply = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32603, "message": reason},
                    });
                    write_value(&mut host_out, &reply).await;
                }
                state = State::Failed(reason);
            }
        }
    }
}

/// Record `message`'s id as awaiting the backend's answer, if it is a
/// request.
fn track(pending: &mut HashSet<String>, message: &Value) {
    if let (Some(id), Some(_)) = (message.get("id"), message.get("method")) {
        pending.insert(id.to_string());
    }
}

async fn answer_from_stub<W: AsyncWrite + Unpin>(
    host: &mut W,
    deferred: &mut Vec<(String, Value)>,
    reason: &str,
    init_answered: &mut bool,
) {
    for (_, message) in deferred.drain(..) {
        if let Some(reply) = stub::answer(&message, reason) {
            *init_answered |= message["method"] == "initialize";
            write_value(host, &reply).await;
        }
    }
}

fn spawn_attach(
    door: Arc<dyn Door>,
    request: Handshake,
    timing: Timing,
    events: mpsc::UnboundedSender<Event>,
    after: Duration,
) {
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        let outcome = attach(door.as_ref(), &request, &timing).await;
        let _ = events.send(Event::Attach(outcome));
    });
}

fn spawn_lines<R>(
    reader: R,
    events: mpsc::UnboundedSender<Event>,
    line: impl Fn(String) -> Event + Send + 'static,
    closed: Event,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(text)) = lines.next_line().await {
            if events.send(line(text)).is_err() {
                return;
            }
        }
        let _ = events.send(closed);
    });
}

async fn write_line<W: AsyncWrite + Unpin + ?Sized>(writer: &mut W, line: &str) -> io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

async fn write_raw<W: AsyncWrite + Unpin>(host: &mut W, line: &str) {
    if let Err(error) = write_line(host, line).await {
        tracing::debug!(%error, "the host's stdout closed");
    }
}

async fn write_value<W: AsyncWrite + Unpin>(host: &mut W, value: &Value) {
    write_raw(host, &value.to_string()).await;
}
```

While `Waiting`, every retry runs `attach`, which on Windows rewrites the start request. That is intended: a hook that fired before the frontend's first request removed nothing, and the next hook sees the fresh one.

A `Waiting` retry that returns `Failed` moves the session to `Failed`. A stream error while writing to the backend is reported as a close of that stream, so it takes the same path.

Add to `crates/mcpls-core/src/backend/mod.rs`: `pub use frontend::{FrontendOptions, run_frontend};`.

- [ ] **Step 4: Run the tests**

Run: `devrun task test`
Expected: PASS, including the eight relay tests, and the stub and attach tests from Task 8a unchanged.

- [ ] **Step 5: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates/mcpls-core
git commit -m "feat(backend): relay a session through a frontend"
```

---

### Task 9: `mcpls` becomes the frontend

**Files:**
- Modify: `crates/mcpls-cli/src/args.rs:40-145` (`--no-backend`, hidden `backend` subcommand) and its tests
- Modify: `crates/mcpls-cli/src/main.rs:32-162`
- Modify: `crates/mcpls-cli/tests/cli_integration.rs` (tests that drive the MCP server over stdio)
- Modify: `crates/mcpls-core/tests/e2e/mcp_client.rs` (`--no-backend` for the in-process protocol tests)
- Create: `crates/mcpls-cli/tests/backend.rs` (the process harness and the default frontend's lifecycle test)
- Test: `crates/mcpls-cli/src/args.rs` tests, `crates/mcpls-cli/tests/cli_integration.rs`, `crates/mcpls-cli/tests/backend.rs`

**Interfaces:**
- Consumes: `ServerConfig::load_at` (Task 3); `ConfigStamp::of` and the doctor's `backend pid:` label (Task 5); `serve_backend` (Task 6b); `BackendLaunch`, `start_requested` (Task 7); `run_frontend`, `FrontendOptions` (Task 8b).
- Produces:
  - `Args::no_backend: bool` (`--no-backend`, env `MCPLS_NO_BACKEND`), `Command::Backend { root: PathBuf }` (hidden).
  - `fn load_config(args: &Args, root: &std::path::Path) -> Result<mcpls_core::ServerConfig>` in `crates/mcpls-cli/src/main.rs`.
  - In `crates/mcpls-cli/tests/backend.rs`: `struct Project { dir: TempDir, runtime: TempDir, user: String }` with `new(idle_ms: u64) -> Project`, `root(&self) -> PathBuf`, `config(&self) -> PathBuf`, `write_config(&self, idle_ms: u64, servers: &str)`, `command(&self, cwd: &Path) -> Command`, `frontend(&self) -> Frontend`, `frontend_in(&self, cwd: &Path, extra: &[&str]) -> Frontend`, `doctor(&self) -> String`, `backend_pid(&self) -> Option<u32>`, `wait_for(&self, what: &str, ready: impl FnMut(&Project) -> bool)`; `struct Frontend { child: Child, stdin: Option<ChildStdin>, lines: Receiver<String>, next_id: i64 }` with `spawn(command: Command) -> Frontend`, `spawn_uninitialized(command: Command) -> Frontend`, `request(&mut self, method: &str, params: Value) -> Value`, `send(&mut self, message: &Value)`, `initialize(&mut self) -> Value`, `close(&mut self, within: Duration) -> bool`; `fn next() -> u64`; `#[cfg(unix)] fn alive(pid: u32) -> bool`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/mcpls-cli/src/args.rs` tests:

```rust
    #[test]
    fn test_no_backend_flag() {
        assert!(!Args::parse_from(["mcpls"]).no_backend);
        assert!(Args::parse_from(["mcpls", "--no-backend"]).no_backend);
    }

    #[test]
    fn test_backend_subcommand_takes_a_root() {
        let args = Args::parse_from(["mcpls", "--log-level", "debug", "backend", "--root", "/work"]);
        assert!(matches!(
            args.command,
            Some(Command::Backend { root }) if root == std::path::Path::new("/work")
        ));
        assert_eq!(args.log_level, "debug");
    }

    /// The arguments a frontend launches a backend with parse back into the
    /// settings it launched it with.
    #[test]
    fn test_launch_arguments_round_trip_through_the_parser() {
        let launch = mcpls_core::backend::BackendLaunch {
            root: std::path::PathBuf::from("/work"),
            config: Some(std::path::PathBuf::from("/etc/mcpls.toml")),
            trust_project_config: true,
            log_level: "trace".to_string(),
            log_json: true,
        };
        let mut argv = vec![std::ffi::OsString::from("mcpls")];
        argv.extend(launch.args());
        let args = Args::parse_from(argv);
        assert_eq!(args.config.as_deref(), Some(std::path::Path::new("/etc/mcpls.toml")));
        assert!(args.trust_project_config);
        assert_eq!(args.log_level, "trace");
        assert!(args.log_json);
        assert!(matches!(args.command, Some(Command::Backend { .. })));
    }
```

`help` output must not list `backend`; add to `crates/mcpls-cli/tests/cli_integration.rs`:

```rust
#[test]
fn test_help_hides_the_backend_subcommand() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--no-backend"))
        .stdout(predicate::str::is_match(r"(?m)^\s+backend\s").unwrap().not());
}
```

- [ ] **Step 2: See them fail**

Run: `devrun task check`
Expected: FAIL to compile: no field `no_backend`, no variant `Backend`.

- [ ] **Step 3: Add the arguments**

In `Args`, after `trust_project_config`:

```rust
    /// Serve this one session in-process instead of through the project's
    /// shared backend.
    ///
    /// For debugging, and for hosts where a detached backend cannot run.
    /// This process binds the project's endpoint for hooks if it is free.
    #[arg(long, env = "MCPLS_NO_BACKEND", value_parser = parse_bool_flag)]
    pub no_backend: bool,
```

In `Command`:

```rust
    /// Serve a checkout's shared backend (started by the frontend)
    #[command(hide = true)]
    Backend {
        /// The canonical checkout root to serve
        #[arg(long, value_name = "DIR")]
        root: PathBuf,
    },
```

- [ ] **Step 4: Choose the role in main**

In `crates/mcpls-cli/src/main.rs`, before the `Command::Hook` block, nothing changes. Inside the hook `None =>` arm, after `let identity = ...`:

```rust
                #[cfg(windows)]
                if let (Some(identity), Ok(exe)) = (identity.as_ref(), std::env::current_exe()) {
                    // The frontend cannot spawn a backend that outlives a
                    // job-contained session, so it asks and a hook starts it.
                    let _ = mcpls_core::backend::start_requested(identity, &exe).await;
                }
```

Replace `run` with:

```rust
async fn run(args: Args) -> Result<()> {
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting mcpls");

    if let Some(Command::Backend { root }) = &args.command {
        let config = load_config(&args, root)?;
        mcpls_core::backend::serve_backend(config, root.clone())
            .await
            .context("backend error")?;
        return Ok(());
    }

    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let root = mcpls_core::hooks::project_root(&cwd).unwrap_or(cwd);
    let config = load_config(&args, &root)?;
    tracing::debug!(lsp_servers = config.lsp_servers.len(), "configuration loaded");

    #[cfg(feature = "transport-http")]
    let in_process = args.no_backend || args.listen.is_some();
    #[cfg(not(feature = "transport-http"))]
    let in_process = args.no_backend;

    if !in_process {
        let stamp = mcpls_core::backend::ConfigStamp::of(&config);
        let launch = mcpls_core::backend::BackendLaunch {
            root,
            config: args
                .config
                .as_ref()
                .map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.clone())),
            trust_project_config: args.trust_project_config,
            log_level: args.log_level.clone(),
            log_json: args.log_json,
        };
        mcpls_core::backend::run_frontend(mcpls_core::backend::FrontendOptions { launch, stamp })
            .await;
        return Ok(());
    }

    let transport = {
        #[cfg(feature = "transport-http")]
        {
            match args.listen {
                Some(bind) => mcpls_core::Transport::Http(mcpls_core::HttpConfig::new(
                    bind,
                    args.http_path.clone(),
                )),
                None => mcpls_core::Transport::Stdio,
            }
        }
        #[cfg(not(feature = "transport-http"))]
        {
            mcpls_core::Transport::Stdio
        }
    };

    mcpls_core::serve_with(config, transport)
        .await
        .context("server error")?;

    tracing::info!("mcpls shutdown complete");
    Ok(())
}

/// Load the configuration a session in `root` runs with.
fn load_config(args: &Args, root: &std::path::Path) -> Result<mcpls_core::ServerConfig> {
    if let Some(config_path) = &args.config {
        return mcpls_core::ServerConfig::load_from(config_path)
            .with_context(|| format!("failed to load config from {}", config_path.display()));
    }
    let trust = if args.trust_project_config {
        ProjectConfigTrust::Trusted
    } else {
        ProjectConfigTrust::Untrusted
    };
    mcpls_core::ServerConfig::load_at(trust, root).context("failed to load configuration")
}
```

The frontend's configuration load fails the same way today's server does, before any stdio traffic, which the existing config-error CLI tests assert (`test_config_file_not_found`, `test_config_with_invalid_toml`).

`--config` relative to the frontend's working directory is absolutized before it reaches the backend, whose working directory is the root.

The `std::process::exit` after `run` stays: the frontend reads `tokio::io::stdin()` too.

- [ ] **Step 5: Keep the stdio CLI tests in-process**

CLI tests in `crates/mcpls-cli/tests/cli_integration.rs` that pipe `MCP_INPUT` and assert on the server's own stderr (the trust tests and the logging tests: `rg -n "MCP_INPUT" crates/mcpls-cli/tests/cli_integration.rs`) test the in-process server's configuration handling. Add `.arg("--no-backend")` to each, so they keep asserting on a process whose stderr they capture. Task 11 covers the frontend and backend processes.

`crates/mcpls-core/tests/e2e/mcp_client.rs` spawns `mcpls` for protocol tests that expect an in-process server reading `STDIO_READY_MARKER` from stderr: add `--no-backend` in `spawn_with_empty_config` and `spawn_with_args_and_stderr`'s argument list unless the caller already passes it, so the existing e2e suite keeps its meaning. `spawn_in_workspace` does not add it; Task 11's shared-backend test uses it.

- [ ] **Step 6: Prove the default across processes**

The flip is real only if a host launching plain `mcpls` gets a backend that goes away with it. Create `crates/mcpls-cli/tests/backend.rs` with the process harness and one lifecycle test; Task 11 extends both.

```rust
//! A frontend and its backend as the host sees them: separate processes
//! sharing one endpoint.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(deprecated)]
#![cfg_attr(windows, allow(dead_code))]

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt as _;
use serde_json::{Value, json};
use tempfile::TempDir;

/// A checkout with its own runtime directory and user name, so its backend
/// never meets a real one or another test's.
struct Project {
    dir: TempDir,
    runtime: TempDir,
    user: String,
}

impl Project {
    fn new(idle_ms: u64) -> Self {
        let dir = TempDir::new().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let project = Self {
            dir,
            runtime: TempDir::new().unwrap(),
            user: format!("mcpls-backend-test-{}-{}", std::process::id(), next()),
        };
        project.write_config(idle_ms, "");
        project
    }

    fn root(&self) -> PathBuf {
        dunce::canonicalize(self.dir.path()).unwrap()
    }

    fn config(&self) -> PathBuf {
        self.root().join("test-mcpls.toml")
    }

    /// `servers` is extra TOML appended after the backend table.
    fn write_config(&self, idle_ms: u64, servers: &str) {
        std::fs::write(
            self.config(),
            format!("[backend]\nidle_shutdown_ms = {idle_ms}\n{servers}"),
        )
        .unwrap();
    }

    fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::cargo_bin("mcpls").unwrap();
        command
            .env_remove("MCPLS_LOG")
            .env_remove("MCPLS_CONFIG")
            .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
            .env_remove("MCPLS_LOG_JSON")
            .env_remove("MCPLS_NO_BACKEND")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env("TMPDIR", self.runtime.path())
            .env("USER", &self.user)
            .env("USERNAME", &self.user)
            .current_dir(cwd);
        command
    }

    fn frontend(&self) -> Frontend {
        self.frontend_in(&self.root(), &[])
    }

    fn frontend_in(&self, cwd: &Path, extra: &[&str]) -> Frontend {
        let mut command = self.command(cwd);
        command.arg("--config").arg(self.config()).args(extra);
        Frontend::spawn(command)
    }

    /// `mcpls hook doctor`'s report for this project.
    fn doctor(&self) -> String {
        let output = self
            .command(&self.root())
            .env("CLAUDE_PROJECT_DIR", self.root())
            .args(["hook", "doctor"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// The backend's pid, or `None` when nothing answers.
    fn backend_pid(&self) -> Option<u32> {
        self.doctor()
            .lines()
            .find_map(|line| line.strip_prefix("backend pid: "))
            .and_then(|pid| pid.parse().ok())
    }

    fn wait_for(&self, what: &str, mut ready: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {}", self.doctor());
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn next() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

struct Frontend {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: i64,
}

impl Frontend {
    fn spawn(command: Command) -> Self {
        let mut frontend = Self::spawn_uninitialized(command);
        frontend.initialize();
        frontend
    }

    fn spawn_uninitialized(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            lines,
            next_id: 0,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).expect("a response in time");
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|_| panic!("the host's stream carried a non-JSON line: {line}"));
            assert_eq!(message["jsonrpc"], "2.0", "{line}");
            if message["id"] == id {
                return message;
            }
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn initialize(&mut self) -> Value {
        let answer = self.request(
            "initialize",
            json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "backend-test", "version": "1"}}),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        answer
    }

    /// Close stdin, as a host ending its session does, and return whether
    /// stdout reached EOF within `within`.
    fn close(&mut self, within: Duration) -> bool {
        drop(self.stdin.take());
        let deadline = Instant::now() + within;
        loop {
            match self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(_) => {}
                Err(RecvTimeoutError::Disconnected) => return true,
                Err(RecvTimeoutError::Timeout) => return false,
            }
        }
    }
}

impl Drop for Frontend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    Command::new("kill").args(["-0", &pid.to_string()]).status().is_ok_and(|s| s.success())
}

/// Removes this checkout's files from the runtime directory, which a
/// Windows test cannot redirect to a temporary one.
#[cfg(windows)]
impl Drop for Project {
    fn drop(&mut self) {
        let Ok(identity) = mcpls_core::hooks::identity_for(&self.root()) else {
            return;
        };
        let Some(dir) = identity.lock.parent() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let prefix = format!("{}.", identity.hash);
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Closing the last session removes the backend after its timer, and the
/// host sees EOF as soon as its frontend exits.
#[cfg(unix)]
#[test]
fn closing_the_last_session_removes_the_backend() {
    let project = Project::new(300);
    let mut frontend = project.frontend();
    let pid = project.backend_pid().unwrap();
    assert!(frontend.close(Duration::from_secs(5)), "the host did not see EOF");
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
    assert!(!alive(pid));
}
```

The `cfg_attr` covers Windows, where this file's only test is gated off until Task 11 adds one. On Windows the frontend does not spawn a backend, so the lifecycle test is Unix-only; the checkout root is a fresh temporary directory, so its hash, and every file the backend leaves beside its lock, is unique to the test.

- [ ] **Step 7: Run the tests**

Run: `devrun task test`, then `devrun task test-e2e`
Expected: PASS for both, including `closing_the_last_session_removes_the_backend`.

- [ ] **Step 8: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates/mcpls-cli crates/mcpls-core/tests/e2e/mcp_client.rs
git commit -m "feat(cli): launch the frontend by default"
```

---

### Task 10: the doctor reports the backend

**Files:**
- Modify: `crates/mcpls-core/src/hooks/protocol.rs:94-113` (`Response::Status`), `:254-273` (its wire test)
- Modify: `crates/mcpls-core/src/hooks/service.rs` (`build_handler` status arm)
- Modify: `crates/mcpls-core/src/bridge/translator/mod.rs:951-956` (`registered_server_ids`)
- Modify: `crates/mcpls-core/src/backend/endpoint.rs` (`Endpoint::new` passes a status source)
- Modify: `crates/mcpls-core/src/lib.rs` (in-process status source)
- Modify: `crates/mcpls-cli/src/hook.rs` (doctor lines, `config_line`, fake owner `answer`, `test_doctor_reports_the_live_owners_root_pid_and_hook_activity`)
- Modify: `crates/mcpls-cli/src/main.rs` (the doctor loads this checkout's configuration)
- Modify: `plugin/README.md` (doctor section)

**Interfaces:**
- Consumes:
  - `Runtime` (Task 6a): reads its private `translator: Arc<Translator>` and adds a private `started: Instant` field, set in `Runtime::start`.
  - `build_handler(server, sweeper, location, stats, cancel)` (Task 4), whose signature this task changes, at its call sites: `Endpoint::new` in `backend/endpoint.rs` (Task 6b), `serve_hooks_in_process` in `lib.rs` (Tasks 6a and 6b), and the `HookHarness` in `hooks/service.rs` tests (Task 4).
  - `Endpoint::new` building `attachments: Arc<Attachments>` and `Attachments::sessions(&self) -> Vec<String>` (Task 6b).
  - `ServerConfig::fingerprint` (Task 3); `load_config(args: &Args, root: &Path) -> Result<ServerConfig>` in `crates/mcpls-cli/src/main.rs` (Task 9).
  - `doctor(project_dir: &Path, root: &Path, identity: &SocketIdentity) -> String` and `doctor_scanning(project_dir, root, identity, prefix: &str) -> String` in `crates/mcpls-cli/src/hook.rs` (existing, printing `backend pid:` since Task 5), whose signatures this task extends.
- Produces:
  - `doctor(project_dir, root, identity, local_fingerprint: Option<&str>) -> String` and `doctor_scanning(project_dir, root, identity, prefix, local_fingerprint: Option<&str>) -> String`; `fn config_line(backend: &str, local: Option<&str>) -> String`.
  - `Runtime::status_source(&self, sessions: impl Fn() -> Vec<String> + Send + Sync + 'static, config_fingerprint: String) -> hooks::StatusSource` (`pub(crate)`).
  - `Response::Status` gains `#[serde(default)] version: String`, `uptime_ms: u64`, `sessions: Vec<String>`, `servers: Vec<String>`, `config_fingerprint: String`.
  - `pub struct StatusExtras { pub version: String, pub uptime_ms: u64, pub sessions: Vec<String>, pub servers: Vec<String>, pub config_fingerprint: String }` and `pub type StatusSource = Arc<dyn Fn() -> StatusExtras + Send + Sync>` in `hooks::service`.
  - `build_handler(..., stats: Arc<HookStats>, status: StatusSource, cancel)`.
  - `Translator::registered_server_ids(&self) -> Vec<String>`, sorted.

- [ ] **Step 1: Write the failing tests**

In `crates/mcpls-core/src/hooks/protocol.rs`, replace `test_the_status_response_pins_the_wire_shape`'s literal and value with:

```rust
        let literal = r#"{"op":"status","hash":"abc123","socket":"mcpls.sock","pid":42,"owner":true,"root":"/work","hooks_seen":7,"version":"0.3.9","uptime_ms":61000,"sessions":["s1","connection-4"],"servers":["rust"],"config_fingerprint":"00000000000000ff"}"#;
        let value = Response::Status {
            hash: "abc123".to_string(),
            socket: PathBuf::from("mcpls.sock"),
            pid: 42,
            owner: true,
            root: PathBuf::from("/work"),
            hooks_seen: 7,
            version: "0.3.9".to_string(),
            uptime_ms: 61_000,
            sessions: vec!["s1".to_string(), "connection-4".to_string()],
            servers: vec!["rust".to_string()],
            config_fingerprint: "00000000000000ff".to_string(),
        };
```

and add:

```rust
    /// A status from a build that predates the backend fields still parses.
    #[test]
    fn test_an_older_status_parses_with_empty_backend_fields() {
        let literal = r#"{"op":"status","hash":"a","socket":"s","pid":1,"owner":true,"root":"/w","hooks_seen":0}"#;
        let Response::Status { sessions, version, .. } =
            serde_json::from_str::<Response>(literal).expect("deserialize")
        else {
            panic!("a status");
        };
        assert!(sessions.is_empty());
        assert!(version.is_empty());
    }
```

In `crates/mcpls-cli/src/hook.rs`, change `test_doctor_reports_the_live_owners_root_pid_and_hook_activity` to expect twelve lines: indexes 0 to 5 as today, then

```rust
        assert_eq!(lines[6], "backend: mcpls 0.3.9, up 1m1s");
        assert_eq!(lines[7], "sessions: 2 attached (s1, connection-4)");
        assert_eq!(lines[8], "language servers: rust");
        assert_eq!(lines[9], "config: 00000000000000ff");
        assert!(lines[10].starts_with("mcpls on PATH: "));
        assert_eq!(lines[11], "watch scan: no eligible top-level paths; hidden entries excluded by default; ignore rules applied; host registration unverified");
```

with the fake owner's `answer` for `Request::Status` returning those backend fields (`version: "0.3.9"`, `uptime_ms: 61_000`, the two sessions, `["rust"]`, the fingerprint). Add a test:

```rust
    #[test]
    fn test_backend_lines_for_an_idle_backend() {
        assert_eq!(sessions_line(&[]), "sessions: none attached");
        assert_eq!(servers_line(&[]), "language servers: none");
        assert_eq!(uptime(0), "0s");
        assert_eq!(uptime(3_725_000), "1h2m");
    }

    /// The doctor prints the backend's fingerprint beside the one this
    /// build loads for the checkout, and says when they differ.
    #[test]
    fn test_the_config_line_marks_a_mismatch() {
        assert_eq!(config_line("00000000000000ff", None), "config: 00000000000000ff");
        assert_eq!(
            config_line("00000000000000ff", Some("00000000000000ff")),
            "config: 00000000000000ff, matches this build's"
        );
        assert_eq!(
            config_line("00000000000000ff", Some("0000000000000001")),
            "config: 00000000000000ff, differs from this build's 0000000000000001; the backend's is in effect"
        );
    }
```

- [ ] **Step 2: See them fail**

Run: `devrun task check`
Expected: FAIL to compile on the new fields and functions.

- [ ] **Step 3: Extend the status**

Add the five fields to `Response::Status` after `hooks_seen`, each `#[serde(default)]` and documented ("This mcpls's version", "How long the backend has run", "The sessions attached", "The language servers registered", "The configuration fingerprint the backend started with").

In `crates/mcpls-core/src/hooks/service.rs`:

```rust
/// What a status answer reports beyond the socket itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusExtras {
    /// This build's version.
    pub version: String,
    /// How long this process has served.
    pub uptime_ms: u64,
    /// The MCP sessions attached.
    pub sessions: Vec<String>,
    /// The language servers registered.
    pub servers: Vec<String>,
    /// The configuration fingerprint this process started with.
    pub config_fingerprint: String,
}

/// Computes [`StatusExtras`] at the moment a status is asked for.
pub type StatusSource = Arc<dyn Fn() -> StatusExtras + Send + Sync>;
```

`build_handler` takes `status: StatusSource` after `stats`; the `Request::Status` arm builds `let extras = status();` and fills the new fields from it. Export both from `hooks/mod.rs`.

In `crates/mcpls-core/src/bridge/translator/mod.rs`, beside `registered_server_count`:

```rust
    /// The routing identities of every registered language server, sorted.
    #[must_use]
    pub fn registered_server_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = lock_std(&self.lsp_servers).keys().map(ToString::to_string).collect();
        ids.sort();
        ids
    }
```

`Runtime` (Task 6a) keeps its `translator` private; add `pub(crate) fn status_source(&self, sessions: impl Fn() -> Vec<String> + Send + Sync + 'static, fingerprint: String) -> StatusSource` that captures `Instant::now()` taken in `Runtime::start` (store it as `started: Instant`) and `Arc::clone(&self.translator)`:

```rust
    pub(crate) fn status_source(
        &self,
        sessions: impl Fn() -> Vec<String> + Send + Sync + 'static,
        config_fingerprint: String,
    ) -> hooks::StatusSource {
        let translator = Arc::clone(&self.translator);
        let started = self.started;
        Arc::new(move || hooks::StatusExtras {
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            sessions: sessions(),
            servers: translator.registered_server_ids(),
            config_fingerprint: config_fingerprint.clone(),
        })
    }
```

In `Endpoint::new`, build `attachments` first and pass `runtime.status_source({ let attachments = Arc::clone(&attachments); move || attachments.sessions() }, config.fingerprint())`. In `serve_hooks_in_process` in `lib.rs`, pass `runtime.status_source(Vec::new, config.fingerprint())`. Tests that call `build_handler`, the `HookHarness` in `hooks/service.rs`, pass `Arc::new(StatusExtras::default)`.

- [ ] **Step 4: Print it**

In `crates/mcpls-cli/src/hook.rs` `doctor_scanning`'s answered-status arm, destructure the new fields and push after `hooks_seen_line`:

```rust
            lines.push(format!("backend: mcpls {version}, up {}", uptime(uptime_ms)));
            lines.push(sessions_line(&sessions));
            lines.push(servers_line(&servers));
            lines.push(config_line(&config_fingerprint, local_fingerprint));
```

with

```rust
fn uptime(ms: u64) -> String {
    let secs = ms / 1000;
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s}s"),
        (h, m, _) => format!("{h}h{m}m"),
    }
}

fn sessions_line(sessions: &[String]) -> String {
    if sessions.is_empty() {
        "sessions: none attached".to_string()
    } else {
        format!("sessions: {} attached ({})", sessions.len(), sessions.join(", "))
    }
}

fn servers_line(servers: &[String]) -> String {
    if servers.is_empty() {
        "language servers: none".to_string()
    } else {
        format!("language servers: {}", servers.join(", "))
    }
}

/// The `config:` line: the backend's fingerprint, and whether the one this
/// build loads for the checkout, the way a frontend would, agrees with it.
fn config_line(backend: &str, local: Option<&str>) -> String {
    match local {
        None => format!("config: {backend}"),
        Some(local) if local == backend => format!("config: {backend}, matches this build's"),
        Some(local) => format!(
            "config: {backend}, differs from this build's {local}; the backend's is in effect"
        ),
    }
}
```

`doctor` and `doctor_scanning` each take a last parameter `local_fingerprint: Option<&str>`, which `doctor` passes through. Every existing test call of `doctor_scanning` passes `None` (`rg -n 'doctor_scanning\(' crates/mcpls-cli/src/hook.rs`), so their `config:` line stays the bare fingerprint and `lines[9]` above holds.

In `crates/mcpls-cli/src/main.rs`, the `HookAction::Doctor` arm loads this checkout's configuration the way the frontend does, through `load_config`, which calls `ServerConfig::load_at` with the trust `--trust-project-config` or `MCPLS_TRUST_PROJECT_CONFIG` gives (clap reads both into `args.trust_project_config`) and honours `--config`. Replace its `let out = match ...` statement with:

```rust
                let local_fingerprint = load_config(&args, &root)
                    .ok()
                    .map(|config| config.fingerprint());
                let out = match mcpls_core::hooks::identity_for(&root) {
                    Ok(identity) => {
                        hook::doctor(&project_dir, &root, &identity, local_fingerprint.as_deref())
                            .await
                    }
                    Err(error) => hook::doctor_without_identity(&project_dir, &root, &error),
                };
```

A configuration that fails to load leaves the line bare, because the doctor reports rather than fails. Loading can create the default global config file, as starting any session already does.

Other doctor tests that assert an exact line count or the index of the PATH or watch-scan line for an answering owner move those indexes by four (`rg -n "lines\[|lines.len\(\)" crates/mcpls-cli/src/hook.rs`). Update the count and indexes; do not loosen them to `contains`.

`HookAction::Doctor`'s help text in `crates/mcpls-cli/src/args.rs:142-144` becomes "Print the socket path, both directory hashes, the backend's pid, uptime, sessions, language servers and configuration, and whether mcpls resolves on PATH".

Update the doctor example output in `plugin/README.md` to show the four new lines after `hooks seen:`.

- [ ] **Step 5: Run the tests**

Run: `devrun task test`
Expected: PASS.

- [ ] **Step 6: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates plugin/README.md
git commit -m "feat(doctor): report the backend's state"
```

---

### Task 11: prove the lifecycle across real processes

**Files:**
- Modify: `crates/mcpls-cli/tests/backend.rs` (harness additions and the lifecycle tests)
- Modify: `crates/mcpls-core/tests/e2e/protocol_tests.rs` (shared-backend records test)

**Interfaces:**
- Consumes: everything above, through the `mcpls` binary only, and the harness Task 9 created in `crates/mcpls-cli/tests/backend.rs`: `Project { dir, runtime, user }` with `new(idle_ms)`, `root()`, `config()`, `write_config(idle_ms, servers)`, `command(cwd)`, `frontend()`, `frontend_in(cwd, extra)`, `doctor()`, `backend_pid()`, `wait_for(what, ready)`; `Frontend { child, stdin, lines, next_id }` with `spawn`, `spawn_uninitialized`, `request`, `send`, `initialize`, `close(within)`; `next()`; `alive(pid)` (Unix).
- Produces, in that file: `Project::spawns`, `Project::with_counting_server`, `Project::holds_for(what, window, holds)`, `Frontend::call_tool`, `line_count(path)`, `failure_text(call)`.

These are the spec's Verification items that need processes. Items already proven elsewhere are not repeated: per-connection records (Task 1), per-connection subscriptions (Task 2), upgrade and trust decisions (Tasks 6b and 8a), exit ordering and the drain race (Tasks 6b and 8b), and the last session's close removing the backend (Task 9).

- [ ] **Step 1: Extend the harness**

In `crates/mcpls-cli/tests/backend.rs`, delete the `#![cfg_attr(windows, allow(dead_code))]` line. Add to `impl Project`:

```rust
    #[cfg(unix)]
    fn spawns(&self) -> PathBuf {
        self.root().join("spawns.txt")
    }

    /// A language server that records each start and never answers, so
    /// the number of lines in `spawns.txt` is the number of servers mcpls
    /// started.
    #[cfg(unix)]
    fn with_counting_server(self, idle_ms: u64) -> Self {
        std::fs::write(self.root().join("marker.fake"), "").unwrap();
        let script = format!("echo $$ >> '{}'; exec sleep 30", self.spawns().display());
        self.write_config(
            idle_ms,
            &format!(
                "\n[[lsp_servers]]\nlanguage_id = \"fake\"\ncommand = \"/bin/sh\"\nargs = [\"-c\", {script:?}]\nfile_patterns = [\"**/*.fake\"]\ntimeout_seconds = 30\n\n[lsp_servers.heuristics]\nproject_markers = [\"marker.fake\"]\n"
            ),
        );
        self
    }

    /// Assert `holds` stays true for the whole of `window`. For a start or
    /// a replacement that must not happen, where no event marks its absence.
    #[cfg(unix)]
    fn holds_for(&self, what: &str, window: Duration, mut holds: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            assert!(holds(self), "{what} stopped holding: {}", self.doctor());
            std::thread::sleep(Duration::from_millis(100));
        }
    }
```

Add to `impl Frontend`:

```rust
    fn call_tool(&mut self) -> Value {
        self.request("tools/call", json!({"name": "get_server_logs", "arguments": {}}))
    }
```

and after `alive`:

```rust
#[cfg(unix)]
fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |text| text.lines().count())
}

/// The reason a failed call carries: a tool result's text, or a JSON-RPC
/// error's message when the call was in flight as the backend died.
#[cfg(unix)]
fn failure_text(call: &Value) -> String {
    call["result"]["content"][0]["text"]
        .as_str()
        .or_else(|| call["error"]["message"].as_str())
        .unwrap_or_default()
        .to_string()
}
```


Every test below ends by closing its frontends and waiting for the backend to exit, so none leaves a process behind.

- [ ] **Step 2: Write the tests**

Append to `crates/mcpls-cli/tests/backend.rs`:

```rust
/// Two sessions, one started in a subdirectory, share one backend and one
/// language server, and the second answers without waiting on a start.
#[cfg(unix)]
#[test]
fn two_sessions_share_one_backend_and_one_server() {
    let project = Project::new(500).with_counting_server(500);
    let mut first = project.frontend();
    project.wait_for("the first server to start", |p| line_count(&p.spawns()) == 1);
    let pid = project.backend_pid().expect("a backend");

    let nested = project.root().join("src").join("deep");
    std::fs::create_dir_all(&nested).unwrap();
    let started = Instant::now();
    let mut second = project.frontend_in(&nested, &[]);
    assert!(second.call_tool()["result"].is_object());
    assert!(started.elapsed() < Duration::from_secs(5), "the second session waited on a start");

    assert_eq!(project.backend_pid(), Some(pid));
    assert!(project.doctor().contains("sessions: 2 attached"), "{}", project.doctor());
    // A second runtime would spawn its server as it starts, before its
    // session answers; one idle period covers a start still in flight.
    project.holds_for("one language server", Duration::from_millis(500), |p| {
        line_count(&p.spawns()) == 1
    });
    assert!(first.call_tool()["result"].is_object());

    first.close(Duration::from_secs(5));
    second.close(Duration::from_secs(5));
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
    assert!(!alive(pid));
}

/// Two worktrees of one repository hold different files and get a backend
/// each.
#[test]
fn two_worktrees_get_a_backend_each() {
    let one = Project::new(500);
    let two = Project::new(500);
    std::fs::remove_dir_all(two.root().join(".git")).unwrap();
    std::fs::write(two.root().join(".git"), format!("gitdir: {}/.git/worktrees/two", one.root().display())).unwrap();

    let mut a = one.frontend();
    let mut b = Frontend::spawn({
        let mut command = two.command(&two.root());
        command
            .env("TMPDIR", one.runtime.path())
            .env("USER", &one.user)
            .env("USERNAME", &one.user)
            .arg("--config")
            .arg(two.config());
        command
    });
    assert!(a.call_tool()["result"].is_object());
    assert!(b.call_tool()["result"].is_object());
    assert!(one.doctor().contains("sessions: 1 attached"), "{}", one.doctor());

    a.close(Duration::from_secs(5));
    b.close(Duration::from_secs(5));
    one.wait_for("both backends to exit", |p| p.backend_pid().is_none());
}

/// Frontends racing from nothing start one backend, and none of them sees
/// an error.
#[test]
fn racing_frontends_start_one_backend() {
    let project = Project::new(500);
    let frontends: Vec<_> = (0..4)
        .map(|_| {
            let command = {
                let mut command = project.command(&project.root());
                command.arg("--config").arg(project.config());
                command
            };
            std::thread::spawn(move || Frontend::spawn(command))
        })
        .collect();
    let mut frontends: Vec<Frontend> = frontends.into_iter().map(|t| t.join().unwrap()).collect();
    for frontend in &mut frontends {
        let call = frontend.call_tool();
        assert_ne!(call["result"]["isError"], true, "{call}");
    }
    assert!(project.doctor().contains("sessions: 4 attached"), "{}", project.doctor());
    for mut frontend in frontends {
        frontend.close(Duration::from_secs(5));
    }
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
}

/// A killed backend is reported, and the frontend starts no language
/// server of its own.
#[cfg(unix)]
#[test]
fn a_killed_backend_is_reported_not_replaced() {
    let project = Project::new(500).with_counting_server(500);
    let mut frontend = project.frontend();
    project.wait_for("the server to start", |p| line_count(&p.spawns()) == 1);
    let pid = project.backend_pid().unwrap();

    Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap();
    project.wait_for("the killed backend to be gone", |_| !alive(pid));
    let call = frontend.call_tool();
    assert!(failure_text(&call).contains("stopped"), "{call}");
    // No event marks a replacement that never starts; one idle period
    // bounds the wait for one.
    project.holds_for("no replacement backend", Duration::from_millis(500), |p| {
        line_count(&p.spawns()) == 1 && p.backend_pid().is_none()
    });
}

/// A deleted socket leaves attached sessions working, and the backend still
/// exits on its timer.
#[cfg(unix)]
#[test]
fn a_deleted_socket_does_not_stop_attached_sessions() {
    let project = Project::new(300);
    let mut frontend = project.frontend();
    let pid = project.backend_pid().unwrap();
    for entry in std::fs::read_dir(project.runtime.path().join(format!("mcpls-{}", project.user))).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "sock") {
            std::fs::remove_file(path).unwrap();
        }
    }
    assert_ne!(frontend.call_tool()["result"]["isError"], true);
    frontend.close(Duration::from_secs(5));
    project.wait_for("the backend to exit on its idle timer", |_| !alive(pid));
}

/// Codex ends a session by signalling the server's whole process group. The
/// backend is not in it.
#[cfg(unix)]
#[test]
fn a_group_signal_to_the_frontend_spares_the_backend() {
    use std::os::unix::process::CommandExt as _;

    // Long enough that the backend's idle exit cannot pass for the signal.
    let project = Project::new(2_000);
    let mut command = project.command(&project.root());
    command.arg("--config").arg(project.config()).process_group(0);
    let mut frontend = Frontend::spawn(command);
    let pid = project.backend_pid().unwrap();
    let group = frontend.child.id();

    Command::new("kill").args(["-TERM", &format!("-{group}")]).status().unwrap();
    project.wait_for("the frontend to die of the group signal", |_| {
        frontend.child.try_wait().is_ok_and(|status| status.is_some())
    });
    assert!(alive(pid), "the backend died with the frontend's group");
    drop(frontend);
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
}

/// Sessions that disagree about trusting the project's config cannot share
/// a backend, and the second is told why.
#[test]
fn a_trust_disagreement_is_refused_with_both_states() {
    let project = Project::new(500);
    std::fs::write(project.root().join("mcpls.toml"), "[backend]\nidle_shutdown_ms = 500\n").unwrap();
    let mut trusting = Frontend::spawn({
        let mut command = project.command(&project.root());
        command.arg("--trust-project-config");
        command
    });
    assert_ne!(trusting.call_tool()["result"]["isError"], true);

    let mut untrusting = Frontend::spawn_uninitialized(project.command(&project.root()));
    let init = untrusting.initialize();
    let text = init["result"]["instructions"].as_str().unwrap_or_default().to_string();
    assert!(text.contains("trust"), "{init}");
    assert_eq!(untrusting.call_tool()["result"]["isError"], true);

    trusting.close(Duration::from_secs(5));
    untrusting.close(Duration::from_secs(5));
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
}

/// On Windows the frontend cannot spawn a backend. A hook invocation starts
/// the one it asked for.
#[cfg(windows)]
#[test]
fn a_hook_starts_the_backend_a_frontend_asked_for() {
    let project = Project::new(500);
    let mut frontend = project.frontend();
    let before = project.doctor();
    assert!(before.contains("backend pid: none"), "{before}");

    let status = project
        .command(&project.root())
        .env("CLAUDE_PROJECT_DIR", project.root())
        .arg("hook")
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(br#"{"hook_event_name":"UserPromptSubmit","session_id":"s1"}"#)?;
            child.wait()
        })
        .unwrap();
    assert!(status.success());
    project.wait_for("the hook-started backend", |p| p.backend_pid().is_some());
    assert_ne!(frontend.call_tool()["result"]["isError"], true);
    frontend.close(Duration::from_secs(5));
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
}
```

`two_worktrees_get_a_backend_each` points both projects at one runtime directory so one doctor sees both endpoints; it asserts on the first project's session count to show the second session went elsewhere.

- [ ] **Step 3: Run them to see the lifecycle hold**

Run: `devrun task test`
Expected: PASS. A failure here is a defect in Tasks 5 to 10: reproduce it in that task's in-crate tests before fixing, rather than widening a timeout.

- [ ] **Step 4: Replace the shared-record e2e test**

In `crates/mcpls-core/tests/e2e/protocol_tests.rs`, add a test in the shape of the deleted `process_session_reports`, against a shared backend:

```rust
/// Two sessions in one project share one backend. Sessions naming no host
/// session keep their own records; sessions naming one share it.
#[rstest::rstest]
#[case::anonymous(None, true)]
#[case::named(Some("shared-backend-session"), false)]
#[tokio::test]
#[ignore = "Requires mcpls binary built"]
async fn e2e_shared_backend_keeps_records_per_session(
    #[case] session: Option<&str>,
    #[case] both_see_it: bool,
) -> Result<()> {
    let workspace = TempDir::new()?;
    let root = dunce::canonicalize(workspace.path())?;
    let script = diagnostics_fixture::write_diagnostics_server(&root)?;
    let published_marker = root.join("published.marker");
    let config_path = root.join("mcpls.toml");
    let workspace_value = toml::Value::String(root.to_string_lossy().into_owned());
    let args = toml_array(&[
        script.to_string_lossy().into_owned(),
        "shared-backend-probe".to_string(),
        "shared-backend-hover".to_string(),
        published_marker.to_string_lossy().into_owned(),
    ]);
    fs::write(
        &config_path,
        format!(
            "[workspace]\nroots = [{workspace_value}]\n[backend]\nidle_shutdown_ms = 500\n[diagnostics]\nsettle_quiet_ms = 50\nsettle_deadline_ms = 5000\n\n[[lsp_servers]]\nlanguage_id = \"python\"\ncommand = \"python3\"\nargs = [{args}]\nfile_patterns = [\"**/*.py\"]\ndiagnostics_severity = \"warning\"\n\n[lsp_servers.heuristics]\nproject_markers = [\"main.py\"]\n"
        ),
    )?;
    let file_path = root.join("main.py");
    fs::write(&file_path, "def fixture():\n    return 1\n")?;
    let config_arg = config_path.to_str().context("fixture config must be UTF-8")?;

    let mut first = McpClient::spawn_in_workspace(&["--config", config_arg], &root, session)?;
    first.initialize()?;
    wait_for_diagnostics_baseline(&mut first)?;
    let mut second = McpClient::spawn_in_workspace(&["--config", config_arg], &root, session)?;
    second.initialize()?;
    wait_for_diagnostics_baseline(&mut second)?;

    let hover = call_hover_when_ready(&mut first, &file_path)?;
    assert!(hover.to_string().contains("shared-backend-hover"));
    wait_for_marker(&published_marker)?;

    let deadline = Instant::now() + Duration::from_secs(5);
    let first_report = loop {
        let report = first.call_tool("get_new_diagnostics", &json!({}))?;
        if report.to_string().contains("shared-backend-probe") {
            break report;
        }
        anyhow::ensure!(Instant::now() < deadline, "publication missing: {report}");
        thread::sleep(Duration::from_millis(25));
    };
    let second_report = second.call_tool("get_new_diagnostics", &json!({}))?;
    let first_repeat = first.call_tool("get_new_diagnostics", &json!({}))?;

    assert!(first_report.to_string().contains("shared-backend-probe"));
    assert_eq!(
        second_report.to_string().contains("shared-backend-probe"),
        both_see_it,
        "{second_report}"
    );
    assert!(!first_repeat.to_string().contains("shared-backend-probe"));
    Ok(())
}
```

The name contains `e2e` so the CI filter `test(e2e)` selects it. `spawn_in_workspace` does not pass `--no-backend` (Task 9), so both clients are frontends of one backend.

- [ ] **Step 5: Run the end-to-end suite**

Run: `devrun task test-e2e`
Expected: PASS, including both cases.

- [ ] **Step 6: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add crates/mcpls-cli/tests/backend.rs crates/mcpls-core/tests/e2e/protocol_tests.rs
git commit -m "test(backend): cover the process lifecycle"
```

---

### Task 12: documentation

**Files:**
- Modify: `docs/superpowers/specs/2026-09-12-shared-backend-design.md:3` (status), the Stages section's Stage 1 paragraph
- Modify: `plugin/README.md` (how the MCP entry reaches a backend, `--no-backend`, `[backend]`, the Windows hook prerequisite)
- Modify: `plugin/skills/mcpls/references/configuration.md` (`[backend]` table)

**Interfaces:**
- Consumes: the behaviour it documents: root discovery and `[backend] idle_shutdown_ms` (Task 3), the doctor's lines (Tasks 5 and 10), the Windows start request (Tasks 7 and 9), `--no-backend` (Task 9), and Decisions 1 and 13.
- Produces: nothing a later task reads.

- [ ] **Step 1: Update the spec's status**

Line 3 becomes: "Status: Stage 1 is built. Stages 2 to 4 are not." Append to the Stage 1 paragraph: "It lands as the frontend and backend described above, with the decisions recorded in `docs/superpowers/plans/2026-09-13-shared-backend-split.md` under "Decisions this plan makes"." Where a decision changes something the spec states (`setsid` in "Starting the backend"), edit that sentence to describe what was built, and state why in one sentence: "`process_group(0)` rather than `setsid`, which needs `unsafe` code this workspace denies; a new process group is what leaves the group Codex signals."

- [ ] **Step 2: Update the plugin docs**

In `plugin/README.md`, add a section after the install section:

```markdown
## One backend per checkout

The `mcpls` the MCP entry launches is a small frontend. The first session in a checkout starts a backend in the background, and every later session in that checkout, from any subdirectory, attaches to it and shares its language servers. The backend exits `idle_shutdown_ms` after the last session closes (10 seconds by default; set it under `[backend]`).

On Windows the backend is started by the plugin's hooks rather than by the frontend, so the hooks are required there. A session with no hooks installed reports that it is waiting for its backend.

`mcpls --no-backend` runs one session entirely in-process, which is useful for debugging or for a host where a background process cannot run.
```

In `plugin/skills/mcpls/references/configuration.md`, document `[backend] idle_shutdown_ms` beside `[diagnostics.hooks]` in the same style as that section, and state that `mcpls.toml` is discovered at the checkout root.

- [ ] **Step 3: Verify and commit**

Run: `devrun task verify`
Expected: PASS.

```bash
git add docs plugin
git commit -m "docs: describe the shared backend"
```

---

## Spec coverage

| Spec requirement (Stage 1 and Verification) | Task |
|---|---|
| Frontend relays both ways, including unsolicited notifications | 8b |
| Frontend answers `initialize`/`tools/list` from the frozen surface, fails `tools/call` with the reason (at once, also while waiting), fails in-flight requests | 8a (stub), 8b |
| Frontend never starts language servers of its own | 8b, 11 (`a_killed_backend_is_reported_not_replaced`) |
| Backend owns servers, documents, cache, records; many `mcp` connections plus hooks | 6a, 6b |
| Frozen handshake: protocol, kind, root, session, fingerprint and source | 5 |
| Spawn lock separate from the ownership lock; detached with streams redirected | 7 |
| Windows: frontend asks, next hook starts | 7, 8a, 9, 11 |
| Upgrade: newer frontend evicts an idle older backend; mismatch repeated in `get_info` and every tool call otherwise; older frontend never evicts | 6b, 8a, 8b (`test_a_refusal_is_repeated_in_initialize_and_every_tool_call`) |
| Different fingerprint served and named in the session's instructions | 6b (`test_a_different_fingerprint_is_served_and_named`) |
| The doctor reports a fingerprint mismatch | 10 (`test_the_config_line_marks_a_mismatch`) |
| Different trust refused both ways | 5, 6b, 8a, 11 |
| Idle shutdown, configurable, default 10 s; close streams before draining | 3, 6b |
| `--no-backend` | 5 (in-process refusal of `mcp`), 6b (`test_an_in_process_server_names_an_endpoint_already_held`), 9 |
| Session identity from the handshake; no `from_env_or_process` in the server | 1, 6b |
| Per-connection subscriptions; a failed notify prunes that peer | 2 |
| Project config discovery at the root | 3 |
| Deletions: owner, passive, demotion, forwarding | 4 |
| Doctor: pid, uptime, sessions, servers, fingerprint | 10 |
| Two sessions share one backend and one server; subdirectory joins | 11 |
| Two worktrees get a backend each | 11 |
| Racing frontends produce one backend | 7, 11 |
| Closing the last session removes backend and servers, no orphans | 6b, 9 (`closing_the_last_session_removes_the_backend`), 11 |
| Connect during the drain gets a new backend | 6b (`test_the_endpoint_is_free_before_the_runtime_drains`, `test_a_handshake_during_the_drain_gets_no_reply`), 8b (`test_a_backend_closing_before_its_first_line_is_attached_again`) |
| Socket deleted under a running backend | 11 |
| Nothing the backend writes reaches the host; host sees EOF | 7, 9, 11 |
| Two sessions read their own records | 1, 11 (e2e) |
| Codex group cleanup spares the backend | 7, 11 |
| Windows named pipe suite | CI runs `crates/mcpls-cli/tests/backend.rs` on Windows; only the Unix-gated tests are skipped there |

Out of scope, per the spec's stages: the Codex `_meta` thread lookup (Stage 2), the watcher (Stage 3), last-writer attribution and record grace (Stage 4). The spec's "A Codex root and a subagent each read their own record" and "a connection returning inside the grace" verification items belong to those stages.
