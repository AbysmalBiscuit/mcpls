# Lazy language server spawn implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Start a language server the first time a session touches its language, instead of starting every applicable server when the backend starts.

**Architecture:** A lifecycle map on `Translator` records what each applicable server is doing. `respawn_if_dead` is generalized into `ensure_server`, which spawns in a detached task so a caller can stop waiting without cancelling the spawn. Two callers trigger it: the hook sweep when a file is edited, and the routing path when a tool call names a language. The default stays eager until the final task, so every task before it leaves the suite green under today's behaviour.

**Tech Stack:** Rust 2024, tokio, serde, strum, nextest, devkit (`devrun`).

**Spec:** `docs/superpowers/specs/2026-09-14-lazy-language-server-spawn-design.md`

## Global Constraints

- Workspace is `mcpls-core` (library) and `mcpls-cli` (binary). All paths below are repository-relative.
- Gate every task with `devrun task verify`: nightly rustfmt check, clippy with warnings denied, nextest, and doctests. A task is not done until it passes.
- Run one test with `cargo nextest run -p mcpls-core <test_name>`. Run the ignored end-to-end suite with `devrun task test-e2e`.
- Commit through `devrun task commit --arg commit_subject='...' --arg commit_body=$'...'`. Do **not** pass `--arg coauthors`: it renders `Co-authored-by` and the repository's `commit-msg` hook rejects any capitalization but `Co-Authored-By`. Put the trailer as the last line of `commit_body` instead, after a blank line: `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.
- Conventional Commits. Subject at most 50 characters including the type prefix, imperative mood, lowercase after the colon, no trailing period.
- `clippy::unwrap_used` and `clippy::expect_used` are denied outside test modules. Test modules in this crate carry `#[allow(clippy::unwrap_used, clippy::expect_used)]` on the `mod tests` item; follow that pattern.
- Comments describe what the code does and why, in the present tense. No references to this plan, the issue, tasks, or the change itself.
- The default spawn policy stays `Eager` until Task 9. Do not flip it early: Tasks 3 through 8 rely on today's startup behaviour to keep the existing suite passing.
- `budget: Option<Duration>` on `ensure_server` means: `None` starts the spawn and returns without waiting, `Some(d)` waits up to `d` for a terminal state. `None` is not "wait forever".

## File Structure

**Created:**
- `crates/mcpls-core/src/bridge/translator/lifecycle.rs`: the `ServerLifecycle` enum, the lifecycle map accessors on `Translator`, and the per-server state broadcast. One responsibility, which is what state a server is in and who is told when it changes.

**Modified:**
- `crates/mcpls-core/src/config/server.rs`: `SpawnPolicy`, the per-server `spawn` key, and its merge behaviour.
- `crates/mcpls-core/src/config/mod.rs`: the `[backend] spawn` key and its default.
- `crates/mcpls-core/src/error.rs`: missing-binary classification on `Error`, and the flag on `ServerSpawnFailure`.
- `crates/mcpls-core/src/lsp/lifecycle.rs`: `spawn_batch` carrying that flag.
- `crates/mcpls-core/src/bridge/translator/mod.rs`: the lifecycle map fields, the self handle, the module declaration, and the removal of `expected_servers`.
- `crates/mcpls-core/src/bridge/translator/respawn.rs`: `respawn_if_dead` becomes `ensure_server`.
- `crates/mcpls-core/src/bridge/translator/routing.rs`: the tool-call trigger and the catch-all fallback.
- `crates/mcpls-core/src/bridge/translator/symbols.rs`: workspace-wide tools.
- `crates/mcpls-core/src/bridge/delivery.rs`: `merge_baseline`.
- `crates/mcpls-core/src/notification_lifecycle.rs`: a `PumpShared` accessor.
- `crates/mcpls-core/src/hooks/sweep.rs`: the edit trigger.
- `crates/mcpls-core/src/hooks/protocol.rs`: `ServerStatus` on the wire.
- `crates/mcpls-core/src/lib.rs`: setup wiring, the per-owner baseline merge, and the removal of the post-init rebind.
- `crates/mcpls-cli/src/hook.rs`: `servers_line`.
- `Cargo.toml`, `crates/mcpls-core/Cargo.toml`: the `strum` dependency.

---

### Task 1: The spawn policy configuration key

No behaviour changes. This task adds the key, its default, and the per-server override, so later tasks have something to read.

**Files:**
- Modify: `crates/mcpls-core/src/config/server.rs`
- Modify: `crates/mcpls-core/src/config/mod.rs`
- Test: the `mod tests` blocks already at the bottom of both files

**Interfaces:**
- Consumes: nothing.
- Produces: `config::SpawnPolicy` (`Lazy` or `Eager`; `Copy`, `PartialEq`, `Eq`, `Debug`), `BackendConfig::spawn: SpawnPolicy`, `LspServerConfig::spawn: Option<SpawnPolicy>`, and `LspServerConfig::effective_spawn(&self, backend: SpawnPolicy) -> SpawnPolicy`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `crates/mcpls-core/src/config/server.rs`:

```rust
#[test]
fn test_a_server_without_a_spawn_key_follows_the_backend_default() {
    let server = LspServerConfig::new("rust", "rust-analyzer");
    assert_eq!(server.effective_spawn(SpawnPolicy::Lazy), SpawnPolicy::Lazy);
    assert_eq!(server.effective_spawn(SpawnPolicy::Eager), SpawnPolicy::Eager);
}

#[test]
fn test_a_server_spawn_key_overrides_the_backend_default() {
    let mut server = LspServerConfig::new("rust", "rust-analyzer");
    server.spawn = Some(SpawnPolicy::Eager);
    assert_eq!(server.effective_spawn(SpawnPolicy::Lazy), SpawnPolicy::Eager);
}

#[test]
fn test_a_later_entry_merges_its_spawn_key_over_an_earlier_one() {
    let toml = r#"
        [[lsp_servers]]
        language_id = "rust"
        command = "rust-analyzer"
        spawn = "lazy"

        [[lsp_servers]]
        language_id = "rust"
        spawn = "eager"
    "#;
    let parsed: ServerConfig = toml::from_str(toml).expect("parse");
    let resolved = resolve_lsp_servers(parsed.lsp_servers).expect("resolve");
    let rust = resolved
        .iter()
        .find(|server| server.language_id == "rust")
        .expect("a rust server");
    assert_eq!(rust.spawn, Some(SpawnPolicy::Eager));
}
```

Add to the `mod tests` block at the bottom of `crates/mcpls-core/src/config/mod.rs`:

```rust
#[test]
fn test_the_backend_spawn_policy_defaults_to_eager() {
    assert_eq!(ServerConfig::default().backend.spawn, SpawnPolicy::Eager);
}

#[test]
fn test_the_backend_spawn_policy_is_read_from_config() {
    let parsed: ServerConfig =
        toml::from_str("[backend]\nspawn = \"lazy\"\n").expect("parse");
    assert_eq!(parsed.backend.spawn, SpawnPolicy::Lazy);
}
```

If `LspServerConfig::new` or `resolve_lsp_servers` is not in scope or is named differently in those test modules, match whatever the neighbouring tests in the same file already use to build a server config. Do not invent a constructor.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p mcpls-core spawn`
Expected: FAIL to compile, `cannot find type SpawnPolicy in this scope`.

- [ ] **Step 3: Add the enum**

In `crates/mcpls-core/src/config/server.rs`, above `ServerHeuristics`:

```rust
/// When a language server is started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpawnPolicy {
    /// Started the first time the session touches the server's language.
    Lazy,
    /// Started with the backend, whether or not the session uses it.
    #[default]
    Eager,
}
```

`Eager` is the derived default so this task changes no behaviour. Task 9 moves `#[default]` to `Lazy`.

- [ ] **Step 4: Add the per-server key**

In the same file, add a field to `LspServerConfig` beside `timeout_seconds`:

```rust
    /// When this server starts, overriding `[backend] spawn`.
    ///
    /// Absent means follow the backend default, so a server entry never
    /// has to restate it.
    #[serde(default)]
    pub spawn: Option<SpawnPolicy>,
```

Add the same field to `PartialLspServerConfig`, beside its `enabled` field:

```rust
    /// When this server starts, overriding `[backend] spawn`.
    #[serde(default)]
    pub spawn: Option<SpawnPolicy>,
```

`merge` destructures `PartialLspServerConfig` exhaustively, so the compiler will point at it. Add `spawn` to that destructuring pattern and, beside the other `if let Some(...)` arms:

```rust
        if let Some(spawn) = spawn {
            self.spawn = Some(spawn);
        }
```

Do the same for `from_partial`: carry `partial.spawn` onto the built config. Fix every other site the compiler names, including any struct literal of `LspServerConfig` in tests or in the built-in server table.

- [ ] **Step 5: Add the accessor**

In the `impl LspServerConfig` block:

```rust
    /// The policy that decides when this server starts: its own `spawn`
    /// key when it has one, otherwise the backend's.
    #[must_use]
    pub fn effective_spawn(&self, backend: SpawnPolicy) -> SpawnPolicy {
        self.spawn.unwrap_or(backend)
    }
```

- [ ] **Step 6: Add the backend key**

In `crates/mcpls-core/src/config/mod.rs`, add to `BackendConfig`:

```rust
    /// When this backend's language servers start.
    ///
    /// Lazy holds a server back until the session touches its language,
    /// which keeps a checkout's unused languages out of memory. Eager
    /// starts every applicable server with the backend.
    #[serde(default)]
    pub spawn: SpawnPolicy,
```

`BackendConfig` carries `#[serde(deny_unknown_fields)]`, so this is what makes `spawn = "lazy"` accepted there rather than rejected. Add `spawn: SpawnPolicy::default()` to its `Default` impl, and re-export `SpawnPolicy` wherever `config` re-exports `LspServerConfig`.

- [ ] **Step 7: Document the key**

`crates/mcpls-core/src/config/mod.rs` holds a commented example configuration, the block containing `# idle_shutdown_ms = 10000`. Add beside it:

```
# spawn = "eager"
```

Then add the key to `docs/user-guide/configuration.md`, following the shape that file already uses for `idle_shutdown_ms`: what it does, what the default is, and that a `[[lsp_servers]]` entry can override it.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls-core spawn`
Expected: PASS.

Run: `devrun task verify`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
devrun task commit --arg commit_subject='feat(config): add a spawn policy key' --arg commit_body=$'A server can now be marked lazy or eager, on the backend as a default\nand on an individual server entry as an override. Nothing reads the\nkey yet and the default stays eager, so behaviour is unchanged.\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>'
```

---

### Task 2: The lifecycle enum and its map

**Files:**
- Create: `crates/mcpls-core/src/bridge/translator/lifecycle.rs`
- Modify: `crates/mcpls-core/src/bridge/translator/mod.rs`
- Modify: `crates/mcpls-core/src/bridge/mod.rs`
- Modify: `Cargo.toml`, `crates/mcpls-core/Cargo.toml`

**Interfaces:**
- Consumes: `config::ServerId`.
- Produces: `bridge::ServerLifecycle` with variants `Idle`, `Starting`, `Running`, `NotInstalled`, `Failed`, deriving `Copy`, `PartialEq`, `Eq`, `Debug`, `Display`, `Serialize`, `Deserialize`, `EnumIter`. On `Translator`: `set_lifecycle(&self, id: &ServerId, state: ServerLifecycle)`, `begin_starting(&self, id: &ServerId) -> bool`, `lifecycle_of(&self, id: &ServerId) -> Option<ServerLifecycle>`, `lifecycles(&self) -> Vec<(ServerId, ServerLifecycle)>` sorted by id, and `subscribe_lifecycle(&self, id: &ServerId) -> watch::Receiver<ServerLifecycle>`.

- [ ] **Step 1: Add the dependency**

In the workspace `Cargo.toml`, under `[workspace.dependencies]`, which is sorted alphabetically, add between `serde_json` and `tempfile`:

```toml
strum = { version = "0.27", features = ["derive"] }
```

In `crates/mcpls-core/Cargo.toml`, under `[dependencies]`, add:

```toml
strum = { workspace = true }
```

- [ ] **Step 2: Verify the dependency is allowed**

Run: `cargo deny check licenses`
Expected: PASS. If strum's license is not in `deny.toml`'s `[licenses] allow` list, add it there in this same commit rather than working around it.

- [ ] **Step 3: Write the failing tests**

Create `crates/mcpls-core/src/bridge/translator/lifecycle.rs` containing only its test module for now:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use strum::IntoEnumIterator;

    #[test]
    fn test_every_state_renders_for_a_reader() {
        let rendered: Vec<String> = ServerLifecycle::iter()
            .map(|state| state.to_string())
            .collect();
        assert_eq!(
            rendered,
            vec!["idle", "starting", "running", "not installed", "failed"]
        );
    }

    #[test]
    fn test_every_state_survives_a_wire_round_trip() {
        for state in ServerLifecycle::iter() {
            let json = serde_json::to_string(&state).expect("serialize");
            let back: ServerLifecycle = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, state);
        }
    }

    #[test]
    fn test_a_missing_binary_is_not_the_same_state_as_a_crash() {
        assert_ne!(ServerLifecycle::NotInstalled, ServerLifecycle::Failed);
        assert_eq!(ServerLifecycle::NotInstalled.to_string(), "not installed");
        assert_eq!(ServerLifecycle::Failed.to_string(), "failed");
    }

    #[test]
    fn test_a_state_change_reaches_a_subscriber() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Idle);
        let mut states = translator.subscribe_lifecycle(&id);
        translator.set_lifecycle(&id, ServerLifecycle::Running);
        assert_eq!(*states.borrow_and_update(), ServerLifecycle::Running);
    }

    #[test]
    fn test_lifecycles_are_reported_in_a_stable_order() {
        let translator = Translator::new();
        translator.set_lifecycle(&ServerId::from("typescript"), ServerLifecycle::Idle);
        translator.set_lifecycle(&ServerId::from("rust"), ServerLifecycle::Running);
        let reported: Vec<String> = translator
            .lifecycles()
            .into_iter()
            .map(|(id, _)| id.to_string())
            .collect();
        assert_eq!(reported, vec!["rust", "typescript"]);
    }

    #[test]
    fn test_only_one_caller_claims_a_spawn() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Idle);
        assert!(translator.begin_starting(&id));
        assert!(!translator.begin_starting(&id));
        assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Starting));
    }

    #[test]
    fn test_a_settled_failure_is_not_claimed_as_a_spawn() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::NotInstalled);
        assert!(!translator.begin_starting(&id));
        assert_eq!(
            translator.lifecycle_of(&id),
            Some(ServerLifecycle::NotInstalled)
        );
    }
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo nextest run -p mcpls-core lifecycle`
Expected: FAIL to compile, `file not found for module lifecycle` until Step 6 declares it, then `cannot find type ServerLifecycle`.

- [ ] **Step 5: Write the enum and the accessors**

At the top of `crates/mcpls-core/src/bridge/translator/lifecycle.rs`, above the test module:

```rust
//! What each applicable language server is doing, and who is told when it
//! changes.
//!
//! Membership in this map is what makes a server applicable to the
//! checkout. `lsp_servers` stays the registry a tool call resolves
//! against; this answers the separate question of why a server is not in
//! it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter};
use tokio::sync::watch;

use super::Translator;
use crate::bridge::lock_std;
use crate::config::ServerId;

/// What an applicable language server is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, EnumIter)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "lowercase")]
pub enum ServerLifecycle {
    /// Applicable to this checkout, never triggered.
    Idle,
    /// A spawn is in flight.
    Starting,
    /// Registered and alive.
    Running,
    /// The command is not on `PATH`. Remembered for the backend's life,
    /// because a backend's environment is fixed when it starts, so the
    /// same attempt fails identically every time after.
    #[strum(serialize = "not installed")]
    NotInstalled,
    /// Spawned and died. Inside the respawn backoff.
    Failed,
}

impl Translator {
    /// Record `state` for `id` and tell every subscriber.
    pub fn set_lifecycle(&self, id: &ServerId, state: ServerLifecycle) {
        let sender = {
            let mut senders = lock_std(&self.lifecycle_senders);
            senders
                .entry(id.clone())
                .or_insert_with(|| watch::channel(state).0)
                .clone()
        };
        lock_std(&self.lifecycles).insert(id.clone(), state);
        sender.send_replace(state);
    }

    /// Claim the right to spawn `id`, moving it to `Starting`.
    ///
    /// Returns `true` to exactly one caller per spawn. A caller that gets
    /// `false` either lost the race, in which case the winner's task will
    /// publish the outcome, or asked about a server whose state is already
    /// settled. Holding the map lock across the read and the write is what
    /// makes this the single-flight test as well as a state change.
    pub fn begin_starting(&self, id: &ServerId) -> bool {
        let sender = {
            let mut states = lock_std(&self.lifecycles);
            match states.get(id) {
                Some(ServerLifecycle::Idle) | Some(ServerLifecycle::Failed) => {}
                _ => return false,
            }
            states.insert(id.clone(), ServerLifecycle::Starting);
            lock_std(&self.lifecycle_senders)
                .entry(id.clone())
                .or_insert_with(|| watch::channel(ServerLifecycle::Starting).0)
                .clone()
        };
        sender.send_replace(ServerLifecycle::Starting);
        true
    }

    /// The state recorded for `id`, or `None` when `id` is not applicable
    /// to this checkout.
    #[must_use]
    pub fn lifecycle_of(&self, id: &ServerId) -> Option<ServerLifecycle> {
        lock_std(&self.lifecycles).get(id).copied()
    }

    /// Every applicable server and its state, ordered by identity so a
    /// reader sees the same line twice in a row.
    #[must_use]
    pub fn lifecycles(&self) -> Vec<(ServerId, ServerLifecycle)> {
        let mut states: Vec<(ServerId, ServerLifecycle)> = lock_std(&self.lifecycles)
            .iter()
            .map(|(id, state)| (id.clone(), *state))
            .collect();
        states.sort_by(|(left, _), (right, _)| left.cmp(right));
        states
    }

    /// Watch `id`'s state. The channel is created on first use, so a
    /// caller can subscribe before anything has been recorded.
    pub fn subscribe_lifecycle(&self, id: &ServerId) -> watch::Receiver<ServerLifecycle> {
        let mut senders = lock_std(&self.lifecycle_senders);
        senders
            .entry(id.clone())
            .or_insert_with(|| watch::channel(ServerLifecycle::Idle).0)
            .subscribe()
    }
}

/// The senders' map type, named so the struct field reads clearly.
pub(super) type LifecycleSenders = HashMap<ServerId, watch::Sender<ServerLifecycle>>;
```

`begin_starting` takes the two locks in the order `lifecycles` then `lifecycle_senders`. Every other method in this file that takes both must use the same order. If `ServerId` does not already derive `Ord`, add `PartialOrd` and `Ord` to its derives where it is defined, since `lifecycles` sorts by it.

- [ ] **Step 6: Wire the module and the fields**

In `crates/mcpls-core/src/bridge/translator/mod.rs`, declare the module beside the existing `mod respawn;`:

```rust
mod lifecycle;
```

Re-export the enum from that file's public surface alongside the other `pub use` items, and again from `crates/mcpls-core/src/bridge/mod.rs`, so `crate::bridge::ServerLifecycle` resolves.

Add two fields to the `Translator` struct, beside `respawn_backoffs`:

```rust
    /// What each applicable server is doing. Membership is the applicable
    /// set: a server absent from here is not configured for this checkout.
    lifecycles: Arc<StdMutex<HashMap<ServerId, ServerLifecycle>>>,
    /// Per-server broadcast of the field above, so a caller waiting on a
    /// spawn learns the outcome without polling.
    lifecycle_senders: Arc<StdMutex<lifecycle::LifecycleSenders>>,
```

Initialize both in `Translator::new`:

```rust
            lifecycles: Arc::new(StdMutex::new(HashMap::new())),
            lifecycle_senders: Arc::new(StdMutex::new(HashMap::new())),
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls-core lifecycle`
Expected: PASS.

Run: `devrun task verify`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
devrun task commit --arg commit_subject='feat(bridge): track a state per language server' --arg commit_body=$'A server is idle, starting, running, missing its binary, or failed.\nNothing writes the map yet beyond its own tests, and lsp_servers stays\nthe registry routing resolves against.\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>'
```

---

### Task 3: Tell a missing binary from a crash

`Error::ServerSpawnFailed` keeps the `io::Error` as its source, so `ErrorKind::NotFound` answers this exactly. `spawn_batch` throws that away when it formats the error into a string, so the flag has to travel on `ServerSpawnFailure` for the eager path to record the same state a lazy trigger records.

**Files:**
- Modify: `crates/mcpls-core/src/error.rs`
- Modify: `crates/mcpls-core/src/lsp/lifecycle.rs:694`
- Test: the `mod tests` blocks already at the bottom of both files

**Interfaces:**
- Consumes: nothing.
- Produces: `Error::is_missing_binary(&self) -> bool`, and `ServerSpawnFailure::missing_binary: bool`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/mcpls-core/src/error.rs`:

```rust
#[test]
fn test_a_not_found_spawn_is_a_missing_binary() {
    let err = Error::ServerSpawnFailed {
        command: "rust-analyzer".to_string(),
        source: std::io::Error::from(std::io::ErrorKind::NotFound),
    };
    assert!(err.is_missing_binary());
}

#[test]
fn test_a_permission_denied_spawn_is_not_a_missing_binary() {
    let err = Error::ServerSpawnFailed {
        command: "rust-analyzer".to_string(),
        source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    };
    assert!(!err.is_missing_binary());
}

#[test]
fn test_a_handshake_failure_is_not_a_missing_binary() {
    let err = Error::ServerUnavailable {
        server_id: ServerId::from("rust"),
        reason: "initialize timed out".to_string(),
    };
    assert!(!err.is_missing_binary());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p mcpls-core missing_binary`
Expected: FAIL to compile, `no method named is_missing_binary`.

- [ ] **Step 3: Add the classifier**

In `crates/mcpls-core/src/error.rs`, in the `impl Error` block:

```rust
    /// Whether this error means the server's command is not on `PATH`.
    ///
    /// Reads the `io::Error` kind rather than matching on the message,
    /// which is why `ServerSpawnFailed` keeps the source error instead of
    /// formatting it away.
    #[must_use]
    pub fn is_missing_binary(&self) -> bool {
        matches!(
            self,
            Self::ServerSpawnFailed { source, .. }
                if source.kind() == std::io::ErrorKind::NotFound
        )
    }
```

- [ ] **Step 4: Carry the answer on the batch failure**

Add a field to `ServerSpawnFailure` in the same file, beside `message`:

```rust
    /// Whether the command was not found, as opposed to any other reason
    /// the spawn or the handshake failed. Decided where the `io::Error`
    /// is still typed, because the message below cannot be re-read for it.
    pub missing_binary: bool,
```

In `crates/mcpls-core/src/lsp/lifecycle.rs`, the `Err(e)` arm of `spawn_batch` constructs that struct. Set the field from the error before it is formatted:

```rust
                    result.add_failure(ServerSpawnFailure {
                        server_id,
                        language_id,
                        command,
                        missing_binary: e.is_missing_binary(),
                        message: e.to_string(),
                    });
```

Fix every other `ServerSpawnFailure` literal the compiler names, including the ones in that file's tests, with `missing_binary: false` unless the test is about a missing binary.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls-core missing_binary`
Expected: PASS.

Run: `devrun task verify`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
devrun task commit --arg commit_subject='feat(error): classify a missing server binary' --arg commit_body=$'A spawn that failed because the command is not on PATH is a different\nfact from one that failed for any other reason, and only the first is\npermanent for a backend. The answer is read from the io::Error kind and\ncarried on the batch failure, which formats the error away.\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>'
```

---

### Task 4: `ensure_server`

`respawn_if_dead` already performs the whole install sequence. This task generalizes its guard, moves the spawn into a detached task so a bounded wait cannot cancel it, and guarantees that a task which dies leaves a terminal state behind.

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/respawn.rs:224`
- Modify: `crates/mcpls-core/src/bridge/translator/mod.rs`
- Modify: `crates/mcpls-core/src/bridge/translator/routing.rs:78`
- Modify: `crates/mcpls-core/src/bridge/translator/symbols.rs:213`
- Test: `crates/mcpls-core/src/bridge/translator/respawn.rs`'s `mod tests`

**Interfaces:**
- Consumes: `ServerLifecycle`, `Translator::begin_starting`, `Translator::set_lifecycle`, `Translator::subscribe_lifecycle`, `Error::is_missing_binary`.
- Produces: `Translator::ensure_server(&self, id: &ServerId, budget: Option<Duration>) -> Result<()>`, and `Translator::set_self_handle(&self, handle: Weak<Translator>)`. `respawn_if_dead` no longer exists.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/mcpls-core/src/bridge/translator/respawn.rs`:

```rust
#[tokio::test]
async fn test_a_translator_with_no_self_handle_cannot_spawn() {
    let translator = Translator::new();
    let id = ServerId::from("rust");
    translator.set_lifecycle(&id, ServerLifecycle::Idle);

    let err = translator
        .ensure_server(&id, Some(Duration::from_millis(50)))
        .await
        .expect_err("a translator built without a self handle cannot spawn");

    assert!(matches!(err, Error::ServerUnavailable { .. }));
}

#[tokio::test]
async fn test_a_missing_binary_state_is_reported_without_a_spawn() {
    let translator = Translator::new();
    let id = ServerId::from("rust");
    translator.set_lifecycle(&id, ServerLifecycle::NotInstalled);

    let err = translator
        .ensure_server(&id, Some(Duration::from_millis(50)))
        .await
        .expect_err("a server whose binary is missing is unavailable");

    assert!(matches!(err, Error::ServerUnavailable { .. }));
    assert_eq!(
        translator.lifecycle_of(&id),
        Some(ServerLifecycle::NotInstalled)
    );
}

#[tokio::test]
async fn test_a_spawn_that_ends_without_publishing_lands_on_failed() {
    let translator = Arc::new(Translator::new());
    translator.set_self_handle(Arc::downgrade(&translator));
    let id = ServerId::from("rust");
    translator.set_lifecycle(&id, ServerLifecycle::Idle);

    // No server config was registered, so the spawn task returns before it
    // reaches a process. The guard is what turns that into a state.
    let _ = translator
        .ensure_server(&id, Some(Duration::from_secs(1)))
        .await;

    assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Failed));
}

#[tokio::test]
async fn test_a_budget_that_expires_leaves_the_spawn_running() {
    let translator = Arc::new(Translator::new());
    translator.set_self_handle(Arc::downgrade(&translator));
    let id = ServerId::from("rust");
    translator.set_lifecycle(&id, ServerLifecycle::Starting);

    let err = translator
        .ensure_server(&id, Some(Duration::from_millis(10)))
        .await
        .expect_err("a starting server outlasting its budget asks for a retry");

    assert!(matches!(err, Error::ServerInitializing { .. }));
    assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Starting));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p mcpls-core ensure_server`
Expected: FAIL to compile, `no method named ensure_server`.

- [ ] **Step 3: Add the self handle**

In `crates/mcpls-core/src/bridge/translator/mod.rs`, add to the `Translator` struct:

```rust
    /// A handle back to this translator, for the detached spawn task.
    ///
    /// Weak rather than strong so the translator is not kept alive by its
    /// own field. Absent on a translator that was never wrapped in an
    /// `Arc`, which is every unit test that does not spawn.
    self_handle: OnceLock<Weak<Translator>>,
```

Initialize it as `OnceLock::new()` in `Translator::new`, and add the setter beside the other configuration methods:

```rust
    /// Give the translator a handle to itself, so a spawn can outlive the
    /// call that asked for it. Called once during setup, where the `Arc`
    /// already exists.
    pub fn set_self_handle(&self, handle: Weak<Translator>) {
        let _ = self.self_handle.set(handle);
    }
```

- [ ] **Step 4: Write the guard**

At the top of `crates/mcpls-core/src/bridge/translator/respawn.rs`:

```rust
/// Publishes a terminal state for a spawn that ended without one.
///
/// `Starting` is the one state no other caller can resolve: waiters block
/// on it and the sweep hands the same paths back to itself, so a task that
/// panics or returns early without publishing would take the language down
/// for the life of the backend. Disarmed by `publish`, which is what the
/// task calls on every outcome it reaches itself.
struct SpawnGuard {
    translator: Arc<Translator>,
    id: ServerId,
    armed: bool,
}

impl SpawnGuard {
    fn publish(&mut self, state: ServerLifecycle) {
        self.armed = false;
        self.translator.set_lifecycle(&self.id, state);
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        tracing::error!("spawn task for LSP server '{}' ended without an outcome", self.id);
        self.translator.record_respawn_failure(&self.id);
        self.translator.set_lifecycle(&self.id, ServerLifecycle::Failed);
    }
}
```

`record_respawn_failure` is currently private to the `impl Translator` block in this file. It stays in the same module, so no visibility change is needed; if the compiler disagrees, widen it to `pub(super)` rather than duplicating it.

- [ ] **Step 5: Turn `respawn_if_dead` into `ensure_server`**

Rename the method and change its head. Everything from `self.forget_watch_registrations(id)` to `self.record_respawn_success(id)` moves into the detached task unchanged.

```rust
    /// Make sure `id` is registered and alive, starting it if it is not.
    ///
    /// A never-spawned server and a crashed one are the same operation from
    /// different starting states. The spawn runs detached so a caller that
    /// stops waiting does not cancel it: the work survives the call that
    /// asked for it, and a retry finds a server rather than starting over.
    ///
    /// `budget` of `None` starts the spawn and returns immediately.
    /// `Some(d)` waits up to `d` for a terminal state.
    ///
    /// # Errors
    ///
    /// [`Error::ServerUnavailable`] when the command is not installed, when
    /// the server is inside its respawn backoff, or when no config was
    /// registered for `id`. [`Error::ServerInitializing`] when the spawn is
    /// still running at the end of `budget`.
    pub(super) async fn ensure_server(
        &self,
        id: &ServerId,
        budget: Option<Duration>,
    ) -> Result<()> {
        if lock_std(&self.lsp_clients).contains_key(id) && !self.is_server_dead(id) {
            return Ok(());
        }

        let mut states = self.subscribe_lifecycle(id);
        match *states.borrow_and_update() {
            ServerLifecycle::Running if !self.is_server_dead(id) => return Ok(()),
            ServerLifecycle::NotInstalled => {
                return Err(Error::ServerUnavailable {
                    server_id: id.clone(),
                    reason: "command not found when it was last attempted".to_string(),
                });
            }
            _ => {}
        }

        self.reconcile_respawn_stability(id);
        if let Some(remaining) = self.respawn_backoff_remaining(id) {
            return Err(Error::ServerUnavailable {
                server_id: id.clone(),
                reason: format!("crash-looping, retry in {remaining:?}"),
            });
        }

        if self.begin_starting(id) {
            let Some(translator) = self.self_handle.get().and_then(Weak::upgrade) else {
                self.set_lifecycle(id, ServerLifecycle::Failed);
                return Err(Error::ServerUnavailable {
                    server_id: id.clone(),
                    reason: "this translator cannot start a server".to_string(),
                });
            };
            let spawn_id = id.clone();
            tokio::spawn(async move { translator.run_spawn(spawn_id).await });
        }

        let Some(budget) = budget else {
            return Ok(());
        };
        self.await_terminal_state(id, &mut states, budget).await
    }
```

- [ ] **Step 6: Write the spawn body and the wait**

In the same `impl Translator` block:

```rust
    /// Run one spawn attempt to a terminal state.
    ///
    /// Holds the single-flight lock for the whole attempt, so a second
    /// trigger that arrives mid-flight waits on the state rather than
    /// producing a second process.
    async fn run_spawn(self: Arc<Self>, id: ServerId) {
        let lock = self.respawn_lock(&id);
        let _flight = lock.lock().await;
        let mut guard = SpawnGuard {
            translator: Arc::clone(&self),
            id: id.clone(),
            armed: true,
        };

        let Some(config) = lock_std(&self.server_configs).get(&id).cloned() else {
            tracing::error!("no spawn config registered for LSP server '{id}'");
            return;
        };

        match self.install_server(&id, config).await {
            Ok(()) => {
                self.record_respawn_success(&id);
                guard.publish(ServerLifecycle::Running);
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
                guard.publish(state);
            }
        }
    }

    /// Wait for `id` to leave `Starting`, up to `budget`.
    async fn await_terminal_state(
        &self,
        id: &ServerId,
        states: &mut tokio::sync::watch::Receiver<ServerLifecycle>,
        budget: Duration,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            match *states.borrow_and_update() {
                ServerLifecycle::Running => return Ok(()),
                ServerLifecycle::NotInstalled => {
                    return Err(Error::ServerUnavailable {
                        server_id: id.clone(),
                        reason: "command not found".to_string(),
                    });
                }
                ServerLifecycle::Failed => {
                    return Err(Error::ServerUnavailable {
                        server_id: id.clone(),
                        reason: "failed to start".to_string(),
                    });
                }
                ServerLifecycle::Idle | ServerLifecycle::Starting => {}
            }
            if tokio::time::timeout_at(deadline, states.changed())
                .await
                .is_err()
            {
                return Err(Error::ServerInitializing {
                    server_id: id.clone(),
                });
            }
        }
    }
```

`install_server` is the existing body of `respawn_if_dead` from `self.forget_watch_registrations(id)` onward, extracted as `async fn install_server(&self, id: &ServerId, config: ServerInitConfig) -> Result<()>`, with its final `Ok(())` in place of the old `record_respawn_success` and log line, which now live in `run_spawn`. Move it verbatim: every comment in it explains an ordering constraint that still holds.

- [ ] **Step 7: Update the call sites**

`crates/mcpls-core/src/bridge/translator/routing.rs:78` and `crates/mcpls-core/src/bridge/translator/symbols.rs:213` call `respawn_if_dead(&id).await?`. Both become:

```rust
        self.ensure_server(&id, Some(RESPAWN_WAIT)).await?;
```

with, near the other constants in `respawn.rs` and re-exported to those modules:

```rust
/// How long a caller waits for a replacement before it is told to retry.
///
/// A respawn of a server that was running has a warm cache behind it, so
/// this is generous where the first-spawn budget in `routing.rs` is not.
const RESPAWN_WAIT: Duration = Duration::from_secs(5);
```

This keeps today's behaviour for a crashed server: the call waits and then either gets the replacement or is told to retry, where before it waited without a bound.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls-core ensure_server`
Expected: PASS.

Run: `cargo nextest run -p mcpls-core respawn`
Expected: PASS. The existing respawn tests exercise the same install sequence through the new entry point.

Run: `devrun task verify`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
devrun task commit --arg commit_subject='refactor(bridge): generalize respawn to ensure' --arg commit_body=$'A never-spawned server and a crashed one are one operation from two\nstarting states. The spawn moves into a detached task so a caller that\nstops waiting does not destroy it, and a drop guard publishes a terminal\nstate for a task that ends without one, which would otherwise leave the\nserver starting for the life of the backend.\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>'
```

---

### Task 5: A baseline that grows with its owners

The diagnostics baseline is adopted once, after the startup batch, and seeds every session record. A server that starts later publishes the warnings the workspace already had into a record that has never seen them, and the next flush reports all of them as the agent's new work. This task makes the baseline cumulative, and moves the two pieces of setup that an empty eager batch would otherwise skip.

**Files:**
- Modify: `crates/mcpls-core/src/bridge/delivery.rs:160`
- Modify: `crates/mcpls-core/src/notification_lifecycle.rs`
- Modify: `crates/mcpls-core/src/lib.rs:719`, `:1119`, `:1227`
- Modify: `crates/mcpls-core/src/bridge/translator/respawn.rs`
- Test: `crates/mcpls-core/src/bridge/delivery.rs`'s `mod tests`

**Interfaces:**
- Consumes: `Translator::run_spawn`, `ServerSettle::register_diagnostics_owner`.
- Produces: `DiagnosticsDelivery::merge_baseline(&mut self, entries: HashMap<String, u64>)`, `NotificationPumps::shared(&self) -> &PumpShared`, and `crate::baseline_merge_task(shared: PumpShared, owner: ServerId, cancel_rx: watch::Receiver<bool>)`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/mcpls-core/src/bridge/delivery.rs`. It already has `entry(key, &diags, floor)`, `diagnostic(line, severity, message)` and `SessionId::from("s".to_string())`; these use them.

```rust
#[test]
fn test_a_merged_entry_is_not_reported_to_a_session_that_predates_it() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    delivery.set_baseline(HashMap::new());
    let session = SessionId::from("s".to_string());
    let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
    let entries = [entry("a.rs", &diags, SeverityFloor::Warning)];

    // The first flush is what creates the record, from the empty baseline.
    assert!(delivery.flush(&session, &[]).changed.is_empty());

    let hash = DiagnosticsDelivery::visible_hash(&diags, SeverityFloor::Warning)
        .expect("an error is visible at the warning floor");
    delivery.merge_baseline(HashMap::from([("a.rs".to_string(), hash)]));

    assert!(
        delivery.flush(&session, &entries).changed.is_empty(),
        "a warning the workspace already had is not this session's work"
    );
}

#[test]
fn test_a_change_after_a_merge_is_still_reported() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    delivery.set_baseline(HashMap::new());
    let session = SessionId::from("s".to_string());
    let before = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
    let after = vec![diagnostic(2, DiagnosticSeverity::ERROR, "different")];

    assert!(delivery.flush(&session, &[]).changed.is_empty());

    let hash = DiagnosticsDelivery::visible_hash(&before, SeverityFloor::Warning)
        .expect("an error is visible at the warning floor");
    delivery.merge_baseline(HashMap::from([("a.rs".to_string(), hash)]));

    let report = delivery.flush(&session, &[entry("a.rs", &after, SeverityFloor::Warning)]);
    assert_eq!(report.changed.len(), 1);
}

#[test]
fn test_a_merge_does_not_rewrite_what_a_session_already_believes() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let known = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
    let later = vec![diagnostic(2, DiagnosticSeverity::ERROR, "different")];
    let known_hash = DiagnosticsDelivery::visible_hash(&known, SeverityFloor::Warning)
        .expect("an error is visible at the warning floor");
    let later_hash = DiagnosticsDelivery::visible_hash(&later, SeverityFloor::Warning)
        .expect("an error is visible at the warning floor");

    delivery.set_baseline(HashMap::from([("a.rs".to_string(), known_hash)]));
    let session = SessionId::from("s".to_string());
    assert!(delivery.flush(&session, &[]).changed.is_empty());

    delivery.merge_baseline(HashMap::from([("a.rs".to_string(), later_hash)]));

    let report = delivery.flush(&session, &[entry("a.rs", &later, SeverityFloor::Warning)]);
    assert_eq!(
        report.changed.len(),
        1,
        "a key the session already holds is its own history, not a merge target"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p mcpls-core merge_baseline`
Expected: FAIL to compile, `no method named merge_baseline`.

- [ ] **Step 3: Write the merge**

In `crates/mcpls-core/src/bridge/delivery.rs`, beside `set_baseline`:

```rust
    /// Fold a newly started owner's settled diagnostics into what every
    /// session believes was already there.
    ///
    /// Only entries nobody holds yet are added, on both the baseline and
    /// each live session record. A key a session already has is that
    /// session's own history: overwriting it would hide a change the
    /// session has not been told about. A session that attached before the
    /// owner existed ends up believing what one attaching after it would.
    pub fn merge_baseline(&mut self, entries: HashMap<String, u64>) {
        let baseline = self.baseline.get_or_insert_with(HashMap::new);
        for (key, hash) in &entries {
            baseline.entry(key.clone()).or_insert(*hash);
        }
        for record in self.sessions.values_mut() {
            for (key, hash) in &entries {
                record.entry(key.clone()).or_insert(*hash);
            }
        }
    }
```

- [ ] **Step 4: Write the per-owner merge task**

In `crates/mcpls-core/src/lib.rs`, beside `baseline_task`:

```rust
/// Adopt one lazily started owner's settled diagnostics as pre-existing.
///
/// The same shape as [`baseline_task`], narrowed to one owner and merging
/// rather than replacing, because the sessions this runs for are already
/// holding records from the owners that started before it.
async fn baseline_merge_task(
    shared: PumpShared,
    owner: ServerId,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            result = cancel_rx.changed() => {
                if result.is_err() || *cancel_rx.borrow() {
                    return;
                }
            }
            () = tokio::time::sleep(BASELINE_POLL_INTERVAL) => {
                if !shared.settle.should_settle() {
                    continue;
                }
                let entries: HashMap<String, u64> = {
                    let cache = shared.notification_cache.lock().await;
                    cache
                        .diagnostics_entries()
                        .into_iter()
                        .filter(|(_, _, entry_owner)| *entry_owner == owner)
                        .filter_map(|(key, info, entry_owner)| {
                            bridge::DiagnosticsDelivery::visible_hash(
                                &info.diagnostics,
                                shared.floors.for_server(entry_owner),
                            )
                            .map(|hash| (key.to_string(), hash))
                        })
                        .collect()
                };
                let merged = entries.len();
                shared.delivery.lock().await.merge_baseline(entries);
                debug!("diagnostics baseline extended over {merged} file(s) for '{owner}'");
                return;
            }
        }
    }
}
```

If `diagnostics_entries` yields the owner by reference or by a different type than the comparison above expects, adjust the filter to match its real signature rather than changing the method.

- [ ] **Step 5: Start the merge from a successful spawn**

Add the accessor in `crates/mcpls-core/src/notification_lifecycle.rs`, in the `impl NotificationPumps` block:

```rust
    /// The state every pump shares, so a caller that already holds the
    /// pumps does not need a second path to the same handles.
    pub(crate) const fn shared(&self) -> &PumpShared {
        &self.shared
    }
```

In `run_spawn` in `crates/mcpls-core/src/bridge/translator/respawn.rs`, in the `Ok(())` arm, after `record_respawn_success`:

```rust
                if self.is_diagnostics_route_for(&id)
                    && let Some(pumps) = self.notification_pumps.get()
                {
                    pumps.register_diagnostics_owner(&id);
                    let shared = pumps.shared().clone();
                    let owner = id.clone();
                    tokio::spawn(crate::baseline_merge_task(
                        shared,
                        owner,
                        pumps.cancel_rx(),
                    ));
                }
```

`is_diagnostics_route` takes a language id and a server id; `install_server` already computes both, so either pass the language through or add a thin `is_diagnostics_route_for(&self, id: &ServerId) -> bool` that looks the language up from `server_configs`. `cancel_rx` may need adding to `NotificationPumps` beside `shared`; it already holds one for its own tasks.

- [ ] **Step 6: Move the two setup pieces**

In `crates/mcpls-core/src/lib.rs`, `spawn_lsp_servers_background` initializes the notification pumps at `:1119`. Move that `get_or_init` call to the setup path, before the `if applicable_configs.is_empty()` branch at `:719`, so a lazily started server always has a pump to deliver through.

Then widen the empty-baseline condition at `:719`. It reads `applicable_configs.is_empty()` today. It becomes "no server will start eagerly", which is the same thing when the default is eager and a different thing once Task 9 lands:

```rust
        let eager_configs: Vec<ServerInitConfig> = applicable_configs
            .iter()
            .filter(|c| {
                c.server_config.effective_spawn(config.backend.spawn) == SpawnPolicy::Eager
            })
            .cloned()
            .collect();

        let lsp_init_handle = if eager_configs.is_empty() {
            // Nothing is starting, so there is nothing to wait for and
            // nothing to report. Adopting an empty baseline now is what
            // keeps `get_new_diagnostics` from advising a retry that no
            // startup will ever satisfy. A server started later merges its
            // own settled entries in.
            delivery.lock().await.set_baseline(HashMap::new());
            None
        } else {
            Some(spawn_lsp_servers_background(
                eager_configs,
                Arc::clone(&translator),
                cancel_rx.clone(),
                pump_shared,
            ))
        };
```

Keep the existing `warn!` about protocol-only mode on the branch where `applicable_configs` itself is empty, since that is still the case it describes.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls-core merge_baseline`
Expected: PASS.

Run: `cargo nextest run -p mcpls-core baseline`
Expected: PASS. The recovery tests at `crates/mcpls-core/src/recovery_tests.rs:876` and `:927` wait on `has_baseline`; they must still pass unchanged.

Run: `devrun task verify`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
devrun task commit --arg commit_subject='fix(bridge): extend the baseline per owner' --arg commit_body=$'The baseline was adopted once, after the startup batch, and seeded every\nsession record. A server starting after that point published the\nwarnings the workspace already had into records that had never seen\nthem, and the next flush reported all of them as new work.\n\nEach owner now merges its own settled entries into the baseline and into\nevery live session record, taking only the keys nobody holds yet. The\nnotification pumps move to setup, where a server that starts without a\nstartup batch can still reach one.\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>'
```

---

### Task 6: The map replaces `expected_servers`

`expected_servers` answers "not registered yet, wait and retry" and is cleared when background initialization finishes. Lazy spawning has no such moment. This task populates the lifecycle map and `server_configs` during setup, binds the router to the applicable set, has the eager batch publish its outcomes, and moves the catch-all redirect to lookup time.

**Files:**
- Modify: `crates/mcpls-core/src/lib.rs:658`, `:775`, `:1143`, `:1161`
- Modify: `crates/mcpls-core/src/bridge/translator/mod.rs:830`, `:938`
- Modify: `crates/mcpls-core/src/bridge/translator/routing.rs:119`
- Modify: `crates/mcpls-core/src/bridge/translator/symbols.rs:202`
- Test: `crates/mcpls-core/src/bridge/translator/routing.rs`'s `mod tests`

**Interfaces:**
- Consumes: `Translator::lifecycle_of`, `Translator::set_lifecycle`, `SpawnPolicy`, `ServerSpawnFailure::missing_binary`.
- Produces: no new public surface. `set_expected_servers`, `clear_expected_servers` and the `expected_servers` field are gone. `rebind_router` is gone.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/mcpls-core/src/bridge/translator/routing.rs`. Build the router and the lifecycle map the way the neighbouring tests build a router today.

```rust
#[test]
fn test_a_starting_server_asks_the_caller_to_retry() {
    let translator = Translator::new().with_router(router_for_rust());
    translator.set_lifecycle(&ServerId::from("rust"), ServerLifecycle::Starting);

    let err = translator
        .get_client_for_file(Path::new("/work/src/main.rs"), ToolKind::Hover)
        .expect_err("a starting server is not a client");

    assert!(matches!(err, Error::ServerInitializing { .. }));
}

#[test]
fn test_a_missing_binary_falls_through_to_the_catch_all() {
    // `rust-narrow` claims hover for rust; `rust` is the language's
    // catch-all and is running.
    let translator = Translator::new().with_router(router_with_narrow_claim());
    translator.set_lifecycle(&ServerId::from("rust-narrow"), ServerLifecycle::NotInstalled);
    translator.set_lifecycle(&ServerId::from("rust"), ServerLifecycle::Running);
    translator.register_client(&ServerId::from("rust"), fake_client());

    let (id, _) = translator
        .get_client_for_file(Path::new("/work/src/main.rs"), ToolKind::Hover)
        .expect("the catch-all answers for a server that is not installed");

    assert_eq!(id, ServerId::from("rust"));
}

#[test]
fn test_a_crashed_narrow_server_does_not_fall_through() {
    let translator = Translator::new().with_router(router_with_narrow_claim());
    translator.set_lifecycle(&ServerId::from("rust-narrow"), ServerLifecycle::Failed);
    translator.set_lifecycle(&ServerId::from("rust"), ServerLifecycle::Running);
    translator.register_client(&ServerId::from("rust"), fake_client());

    let err = translator
        .get_client_for_file(Path::new("/work/src/main.rs"), ToolKind::Hover)
        .expect_err("a server that is expected back keeps its own route");

    assert!(matches!(err, Error::ServerUnavailable { .. }));
}
```

`register_client` and `fake_client` stand for whatever the existing tests in this module already use to put a client in `lsp_clients`. Use those.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p mcpls-core catch_all`
Expected: FAIL. The fall-through test resolves to `rust-narrow` and errors.

- [ ] **Step 3: Populate the map and the configs at setup**

In `crates/mcpls-core/src/lib.rs`, replace the `expected_servers` block at `:658` with:

```rust
        // Membership is the applicable set. A server is idle until
        // something shows the session needs it; an eager one is already
        // starting by the time any request can observe this.
        for init_config in &applicable_configs {
            let id = init_config.server_config.id();
            let eager = init_config
                .server_config
                .effective_spawn(config.backend.spawn)
                == SpawnPolicy::Eager;
            translator.register_server_config(id.clone(), init_config.clone());
            translator.set_lifecycle(
                &id,
                if eager {
                    ServerLifecycle::Starting
                } else {
                    ServerLifecycle::Idle
                },
            );
        }
```

An eager entry starts at `Starting`, not `Idle`: `spawn_batch` does not take the single-flight lock, so an entry left idle while the batch runs would let a request in that window start a second process for the same server.

Then, after `let translator = Arc::new(translator);`:

```rust
        translator.set_self_handle(Arc::downgrade(&translator));
```

- [ ] **Step 4: Publish the batch's outcomes**

`register_servers` consumes the `ServerInitResult`, and `RegisteredServers` carries only `diagnostics_flags`. Publish the failures from the result first, then the successes from that map's keys, which hold exactly one entry per registered server:

```rust
        for failure in &result.failures {
            translator.set_lifecycle(
                &failure.server_id,
                if failure.missing_binary {
                    ServerLifecycle::NotInstalled
                } else {
                    ServerLifecycle::Failed
                },
            );
        }

        let server_count = result.server_count();
        let registered = register_servers(result, &translator, &configs_by_id);
        for id in registered.diagnostics_flags.keys() {
            translator.set_lifecycle(id, ServerLifecycle::Running);
        }
```

Do this rather than adding an `ids` field to `RegisteredServers`: `diagnostics_flags` is already built from `result.servers`, so a second list of the same keys would be two things to keep in step.

On the `all_failed` path at `:1143`, publish the same failure states and delete the `rebind_router(&HashSet::new())` call. Delete both `clear_expected_servers` calls.

- [ ] **Step 5: Retire the router rebind**

`ToolRouter::from_configs` at `crates/mcpls-core/src/lib.rs:625` already builds the router from the applicable configs, which is the binding this design wants. Delete `Translator::rebind_router` and the `ToolRouter::rebind_to_registered` it calls, along with their tests. Nothing rebinds after setup.

The call to delete first is inside `register_servers` itself, at `crates/mcpls-core/src/lib.rs:292`. Its `register_server_config` call at `:313` also becomes redundant, since Step 3 registers every applicable config during setup; drop it and delete the local `registered` set if nothing else reads it.

- [ ] **Step 6: Resolve against the map**

In `crates/mcpls-core/src/bridge/translator/routing.rs`, replace the `expected_servers` arm of `get_client_for_file` with a match on the state, and add the catch-all fall-through:

```rust
            match self.lifecycle_of(&id) {
                Some(ServerLifecycle::Idle | ServerLifecycle::Starting) => {
                    return Err(Error::ServerInitializing { server_id: id });
                }
                // A server whose binary is missing is never coming back
                // for this backend, so the language's catch-all answers
                // the tools it claimed, exactly as it would have if the
                // route had been rebound at startup. A failed or starting
                // server keeps its own route: it is expected back, and
                // conscripting the catch-all would hand its diagnostics
                // the tools the user routed away.
                Some(ServerLifecycle::NotInstalled) => {
                    let catch_all = lock_std(&self.router).catch_all(lang).cloned();
                    if let Some(catch_all) = catch_all
                        && catch_all != id
                        && self.lifecycle_of(&catch_all) != Some(ServerLifecycle::NotInstalled)
                        && let Some(client) = lock_std(&self.lsp_clients).get(&catch_all).cloned()
                    {
                        return Ok((catch_all, client));
                    }
                    return Err(Error::ServerUnavailable {
                        server_id: id,
                        reason: "command not found".to_string(),
                    });
                }
                Some(ServerLifecycle::Failed) => {
                    return Err(Error::ServerUnavailable {
                        server_id: id,
                        reason: "failed to start".to_string(),
                    });
                }
                Some(ServerLifecycle::Running) | None => {}
            }
```

`ToolRouter` needs a `catch_all(&self, language_id: &str) -> Option<&ServerId>` accessor returning `by_language[language].default`. Add it beside `resolve`.

The `tracing::error!` fall-through below this block stays: it is now reached only by a library consumer that called `with_router` without registering matching clients, which is what its comment already says.

In `crates/mcpls-core/src/bridge/translator/symbols.rs:202`, the `NothingRegistered` arm checks `expected_servers.is_empty()`. It becomes a check on the map:

```rust
                NoServerReason::NothingRegistered => {
                    if self.lifecycles().is_empty() {
                        Error::NoServerConfigured
                    } else {
                        Error::WorkspaceServersInitializing
                    }
                }
```

The `expected_servers` check further down at `:215` becomes the same `lifecycle_of` match the routing path uses. Then delete the field, `set_expected_servers`, `clear_expected_servers`, and every test that only exercised them.

- [ ] **Step 7: Point the doctor at the map**

`crates/mcpls-core/src/lib.rs:775` passes `translator.registered_server_ids()` as the status response's `servers`. Change it to report from the lifecycle map, keeping the field's `Vec<String>` shape for now, so an untriggered server is listed rather than absent:

```rust
            servers: translator
                .lifecycles()
                .into_iter()
                .map(|(id, _)| id.to_string())
                .collect(),
```

Task 9 changes the field's type. Splitting it here keeps this task's diff to one concern.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls-core catch_all`
Expected: PASS.

Run: `devrun task verify`
Expected: PASS. Every test that asserted `ServerInitializing` now reaches it through the lifecycle map.

- [ ] **Step 9: Commit**

```bash
devrun task commit --arg commit_subject='refactor(bridge): resolve against the state map' --arg commit_body=$'expected_servers answered "not registered yet, wait and retry" and was\ncleared when startup finished. Lazy spawning has no such moment, and a\nset left populated would report a missing binary as still initializing\nfor the life of the backend.\n\nThe router now binds to the applicable set at setup and is never rebound,\nso the catch-all redirect that rebinding performed moves to lookup time:\na route naming a server whose binary is missing falls through to the\nlanguage catch-all.\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>'
```

---

### Task 7: The tool-call trigger

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/routing.rs:71`, `:97`
- Modify: `crates/mcpls-core/src/bridge/translator/symbols.rs:192`
- Test: `crates/mcpls-core/src/bridge/translator/routing.rs`'s `mod tests`

**Interfaces:**
- Consumes: `Translator::ensure_server`.
- Produces: `get_client_for_file` returns `Result<(ServerId, Option<LspClient>)>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn test_a_tool_call_on_an_idle_language_starts_its_server() {
    let translator = Arc::new(Translator::new().with_router(router_for_rust()));
    translator.set_self_handle(Arc::downgrade(&translator));
    translator.set_lifecycle(&ServerId::from("rust"), ServerLifecycle::Idle);

    let _ = translator
        .resolve_client_for_file(Path::new("/work/src/main.rs"), ToolKind::Hover)
        .await;

    assert_ne!(
        translator.lifecycle_of(&ServerId::from("rust")),
        Some(ServerLifecycle::Idle)
    );
}

#[tokio::test]
async fn test_a_tool_call_on_an_unused_language_starts_nothing_else() {
    let translator = Arc::new(Translator::new().with_router(router_for_rust_and_typescript()));
    translator.set_self_handle(Arc::downgrade(&translator));
    translator.set_lifecycle(&ServerId::from("rust"), ServerLifecycle::Idle);
    translator.set_lifecycle(&ServerId::from("typescript"), ServerLifecycle::Idle);

    let _ = translator
        .resolve_client_for_file(Path::new("/work/src/main.rs"), ToolKind::Hover)
        .await;

    assert_eq!(
        translator.lifecycle_of(&ServerId::from("typescript")),
        Some(ServerLifecycle::Idle)
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p mcpls-core tool_call`
Expected: FAIL. The first assertion holds at `Idle` because nothing triggers a spawn.

- [ ] **Step 3: Change the resolver's return type**

`get_client_for_file` cannot hand back an identity for a server with no client while it returns `(ServerId, LspClient)`. Change its signature to `Result<(ServerId, Option<LspClient>)>`, and in the `Idle | Starting` arm added in Task 6, return `Ok((id, None))` rather than `Error::ServerInitializing`. The decision moves to the wrapper with the identity.

Move the tests that assert `ServerInitializing` from this method to `resolve_client_for_file`, and make them `#[tokio::test]`.

- [ ] **Step 4: Trigger from the wrapper**

```rust
/// How long a tool call waits for a server it just started.
///
/// Long enough for a fast handshake such as taplo or lua-ls to land inside
/// the originating call, short enough that a cold rust-analyzer hands the
/// caller back to the retry contract instead of stalling it.
const FIRST_SPAWN_BUDGET: Duration = Duration::from_millis(1_500);

    pub(super) async fn resolve_client_for_file(
        &self,
        path: &Path,
        tool: ToolKind,
    ) -> Result<(ServerId, LspClient)> {
        let (id, client) = self.get_client_for_file(path, tool)?;
        match client {
            Some(client) if !self.is_server_dead(&id) => Ok((id, client)),
            _ => {
                self.ensure_server(&id, Some(FIRST_SPAWN_BUDGET)).await?;
                let client = lock_std(&self.lsp_clients).get(&id).cloned();
                client.map_or(
                    Err(Error::ServerInitializing {
                        server_id: id.clone(),
                    }),
                    |client| Ok((id, client)),
                )
            }
        }
    }
```

- [ ] **Step 5: Trigger from the workspace path**

In `crates/mcpls-core/src/bridge/translator/symbols.rs`, `handle_workspace_symbol` resolves one server through `resolve_any` and then calls `ensure_server`. Change that call to pass the same budget:

```rust
        self.ensure_server(&server_id, Some(FIRST_SPAWN_BUDGET)).await?;
```

It ensures that one server and no others. `resolve_any` picks a single claimant and the handler queries exactly that one, so ensuring every applicable server would start rust-analyzer from a TypeScript-only session's symbol search for a result it would never contribute to.

If that server's state is `NotInstalled`, fall through to the next claimant in `order` before failing, matching the per-language rule. Add a `resolve_any_excluding(&self, tool: ToolKind, skip: &HashSet<ServerId>)` to `ToolRouter` if the existing `resolve_any` cannot express it, and loop until a claimant is not `NotInstalled` or the set is exhausted.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls-core tool_call`
Expected: PASS.

Run: `devrun task verify`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
devrun task commit --arg commit_subject='feat(bridge): start a server on a tool call' --arg commit_body=$'A tool call naming a language with no server running starts one and\nwaits a bounded time for the handshake. A fast server lands inside the\noriginating call; anything slower hands the caller back to the retry\ncontract it already has, with the spawn still running.\n\nWorkspace symbol search ensures the one server resolve_any picks, since\nthat is the only one it queries.\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>'
```

---

### Task 8: The edit trigger

**Files:**
- Modify: `crates/mcpls-core/src/hooks/sweep.rs:226`
- Test: `crates/mcpls-core/src/hooks/sweep.rs`'s `mod tests`

**Interfaces:**
- Consumes: `Translator::ensure_server`, `Translator::lifecycle_of`, `Translator::server_for_path`.
- Produces: nothing outside the module.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn test_an_edit_starts_the_language_server() {
    let harness = sweep_harness_with_idle_rust().await;
    harness.report("src/main.rs");

    harness.sweeper.sweep().await;

    assert_ne!(
        harness.translator.lifecycle_of(&ServerId::from("rust")),
        Some(ServerLifecycle::Idle)
    );
}

#[tokio::test]
async fn test_a_path_waiting_on_a_starting_server_is_swept_again() {
    let harness = sweep_harness_with_idle_rust().await;
    harness
        .translator
        .set_lifecycle(&ServerId::from("rust"), ServerLifecycle::Starting);
    harness.report("src/main.rs");

    harness.sweeper.sweep().await;

    assert!(harness.sweeper.pending_paths().contains(&harness.path("src/main.rs")));
}

#[tokio::test]
async fn test_a_deleted_path_starts_nothing() {
    let harness = sweep_harness_with_idle_rust().await;
    harness.report_deleted("src/gone.rs");

    harness.sweeper.sweep().await;

    assert_eq!(
        harness.translator.lifecycle_of(&ServerId::from("rust")),
        Some(ServerLifecycle::Idle)
    );
}
```

Build `sweep_harness_with_idle_rust` on whatever harness the neighbouring sweep tests already use, adding a lifecycle entry for `rust`. `pending_paths` is a `#[cfg(test)]` accessor on `Sweeper` reading `self.pending`; add it if it does not exist.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p mcpls-core sweep`
Expected: FAIL. The server stays `Idle` and the path is not re-queued.

- [ ] **Step 3: Trigger after the kinds are built**

In `Sweeper::sweep`, after the `for path in paths` loop that fills `kinds`, `settle` and `untracked`, and before the headroom computation:

```rust
        // Both changed and created paths trigger: only created paths reach
        // `open_untracked_document` below, so a trigger placed after that
        // split would miss every edit to a file that is already open. A
        // deleted path triggers nothing, since there is no document to open
        // and a delete does not arrive without a nearby edit.
        let mut waiting = Vec::new();
        for (path, kind) in &kinds {
            if *kind == SweepKind::Deleted || !self.filter.routable_extension(path) {
                continue;
            }
            let Some(id) = self.translator.server_for_path(path) else {
                continue;
            };
            if self.translator.lifecycle_of(&id) == Some(ServerLifecycle::Running) {
                continue;
            }
            // Fire and return. There is one sweeper for the backend and its
            // loop awaits each sweep in turn, so waiting here would hold
            // every other language's changes, for every session, for as
            // long as this handshake takes. The path comes back on the next
            // tick instead, which runs at a quarter of the quiet interval.
            let _ = self.translator.ensure_server(&id, None).await;
            waiting.push(path.clone());
        }
```

Then, immediately before `self.translator.queue_invalidations(&settle);`, put those paths back and drop them from this pass:

```rust
        if !waiting.is_empty() {
            let mut pending = lock_std(&self.pending);
            for path in &waiting {
                pending.insert(path.clone());
            }
            drop(pending);
            settle.retain(|path| !waiting.contains(path));
            untracked.retain(|path| !waiting.contains(path));
        }
```

`untracked` is consumed by the loop above this point, so hoist that loop below this block or filter `untracked` before it runs. The ordering that works: build `kinds`, trigger and collect `waiting`, re-queue and filter, then run the open loop and `queue_invalidations`.

`server_for_path` is a thin `Translator` method resolving a path's language to a `ServerId` through the router, without touching `lsp_clients`. Add it beside `get_client_for_file` if it does not exist, reusing `detect_language` and the same React base-language fallback.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls-core sweep`
Expected: PASS.

Run: `devrun task verify`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
devrun task commit --arg commit_subject='feat(hooks): start a server on the first edit' --arg commit_body=$'An edit to a file whose language has no server running starts one. The\nsweep fires the trigger and does not wait: there is one sweeper for the\nbackend, so a wait would hold every other language and session behind one\ncold handshake. Paths whose server is not up yet go back on the pending\nset and land on the first sweep after it is running.\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>'
```

---

### Task 9: The doctor, and lazy by default

The doctor cannot ship after the default flips: an idle server would read as absent, which is a fault report for working software. Both land together.

**Files:**
- Modify: `crates/mcpls-core/src/hooks/protocol.rs:123`, `:271`
- Modify: `crates/mcpls-core/src/lib.rs:775`
- Modify: `crates/mcpls-cli/src/hook.rs:462`
- Modify: `crates/mcpls-core/src/config/server.rs`
- Test: the `mod tests` blocks in `protocol.rs` and `hook.rs`

**Interfaces:**
- Consumes: `ServerLifecycle`, `Translator::lifecycles`, `Translator::is_server_dead`.
- Produces: `hooks::protocol::ServerStatus { id: String, state: ServerLifecycle }`. `Response::Status::servers` is `Vec<ServerStatus>`.

- [ ] **Step 1: Write the failing tests**

Replace the status literal test in `crates/mcpls-core/src/hooks/protocol.rs:271`. The literals in that module are the wire contract, so this assertion is the change:

```rust
    #[test]
    fn test_the_status_response_pins_the_wire_shape() {
        let literal = r#"{"op":"status","hash":"abc123","socket":"mcpls.sock","pid":42,"owner":true,"root":"/work","hooks_seen":7,"version":"0.3.9","uptime_ms":61000,"sessions":["s1","connection-4"],"servers":[{"id":"rust","state":"running"},{"id":"lua","state":"not_installed"}],"config_fingerprint":"00000000000000ff"}"#;
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
            servers: vec![
                ServerStatus {
                    id: "rust".to_string(),
                    state: ServerLifecycle::Running,
                },
                ServerStatus {
                    id: "lua".to_string(),
                    state: ServerLifecycle::NotInstalled,
                },
            ],
            config_fingerprint: "00000000000000ff".to_string(),
        };
        // ... the two assertions already in this test, unchanged
    }
```

Add to the `mod tests` block in `crates/mcpls-cli/src/hook.rs`:

```rust
#[test]
fn test_the_servers_line_names_a_state_beside_each_server() {
    let servers = vec![
        status("rust", ServerLifecycle::Running),
        status("typescript", ServerLifecycle::Idle),
        status("lua", ServerLifecycle::NotInstalled),
    ];
    assert_eq!(
        servers_line(&servers),
        "language servers: rust (running), typescript (idle), lua (not installed)"
    );
}

#[test]
fn test_the_servers_line_says_none_when_nothing_is_configured() {
    assert_eq!(servers_line(&[]), "language servers: none");
}
```

Add a `status(id, state)` helper beside the other test helpers in that module.

Add to `crates/mcpls-core/src/config/mod.rs`'s tests, replacing the Task 1 default assertion:

```rust
#[test]
fn test_the_backend_spawn_policy_defaults_to_lazy() {
    assert_eq!(ServerConfig::default().backend.spawn, SpawnPolicy::Lazy);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p mcpls-core status_response && cargo nextest run -p mcpls-cli servers_line`
Expected: FAIL to compile, `cannot find struct ServerStatus`.

- [ ] **Step 3: Add the wire struct**

In `crates/mcpls-core/src/hooks/protocol.rs`, above `Response`:

```rust
/// One language server on the status response.
///
/// A struct rather than a pair, because what the doctor says about a
/// server grows: a command, a pid, a reason for being dark are fields
/// here rather than more parallel lists on the response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatus {
    /// The server's routing identity.
    pub id: String,
    /// What it is doing.
    pub state: ServerLifecycle,
}
```

Change the `servers` field on `Response::Status` to `Vec<ServerStatus>`. The state field is `ServerLifecycle` itself rather than a second enum mirroring it: both live in `mcpls-core` and both name the same five states, so a mirror would only be something to keep in step by hand.

- [ ] **Step 4: Report the real state**

In `crates/mcpls-core/src/lib.rs:775`:

```rust
            servers: translator
                .lifecycles()
                .into_iter()
                .map(|(id, state)| {
                    // Nothing writes the map when a process exits: death is
                    // found by `is_server_dead` on the next call that needs
                    // the server. This is the one reader with no such call
                    // behind it, so it runs the check itself.
                    let state = if state == ServerLifecycle::Running
                        && translator.is_server_dead(&id)
                    {
                        ServerLifecycle::Failed
                    } else {
                        state
                    };
                    hooks::protocol::ServerStatus {
                        id: id.to_string(),
                        state,
                    }
                })
                .collect(),
```

`is_server_dead` is `pub(crate)`; widen it to `pub` if this call site is outside the crate boundary it allows.

- [ ] **Step 5: Render the line**

In `crates/mcpls-cli/src/hook.rs`:

```rust
fn servers_line(servers: &[ServerStatus]) -> String {
    if servers.is_empty() {
        "language servers: none".to_string()
    } else {
        let rendered: Vec<String> = servers
            .iter()
            .map(|server| format!("{} ({})", server.id, server.state))
            .collect();
        format!("language servers: {}", rendered.join(", "))
    }
}
```

One line rather than two lists: the doctor line is read in hook output at session start, and splitting running from configured would make the reader cross-reference two lists to answer whether a language is covered.

- [ ] **Step 6: Flip the default**

In `crates/mcpls-core/src/config/server.rs`, move `#[default]` from `Eager` to `Lazy` on `SpawnPolicy`. Update the commented example in `crates/mcpls-core/src/config/mod.rs` to `# spawn = "lazy"`, and the configuration guide to say lazy is the default.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls-core && cargo nextest run -p mcpls-cli`
Expected: PASS.

Run: `devrun task verify`
Expected: PASS.

Run: `devrun task test-e2e`
Expected: PASS. This suite spawns real backends, so it is where the default flip shows up.

- [ ] **Step 8: Verify against a real checkout**

Build and install, then from a mixed checkout run `mcpls hook doctor` and confirm the `language servers:` line names a state beside each server and that only the languages this session touched read `running`.

Walk the spec's Verification list. Each bullet is a claim about a running backend, and the automated tests cover the mechanism rather than the whole path. Record any bullet that does not hold rather than adjusting the spec to match.

- [ ] **Step 9: Commit**

```bash
devrun task commit --arg commit_subject='feat: start language servers on first use' --arg commit_body=$'A checkout no longer starts every server whose markers matched. A server\nstarts when an edit or a tool call shows the session needs its language,\nand spawn = "eager" holds the old behaviour for one server or all of\nthem.\n\nThe doctor reports a state beside each server, so an idle one reads as\nidle rather than as absent. The status response carries a struct per\nserver in place of a bare identity, which a backend from an older build\ncannot answer; idle shutdown clears that without intervention.\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>'
```

---

## Unresolved questions

1. `FIRST_SPAWN_BUDGET` is 1.5 seconds by guess. The spec's Open decisions section asks for a taplo and a lua-ls handshake measured on a cold cache before the constant is fixed. Measure during Task 7 and change the number there if it is wrong.
2. `RESPAWN_WAIT` in Task 4 is new. Today a respawn waits without a bound, so five seconds is a behaviour change for a crashed server on a slow machine. If the e2e suite goes flaky around respawn, that constant is the first suspect.
3. Task 6 deletes `ToolRouter::rebind_to_registered`. If something outside this repository calls it, that is a breaking change to a public item. Nothing in the workspace does.
