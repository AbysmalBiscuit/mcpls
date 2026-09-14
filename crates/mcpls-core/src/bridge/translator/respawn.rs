//! Dead-server detection and respawn-backoff bookkeeping.
//!
//! Tracks consecutive respawn failures per server so a crash-looping
//! process backs off exponentially instead of eating a fresh
//! `timeout_seconds` on every tool call that arrives while it is down.

use std::collections::HashSet;
use std::sync::{Arc, Weak};
use std::time::Instant;

#[cfg(test)]
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant as TokioInstant};

use super::{ServerLifecycle, Translator};
use crate::bridge::lock_std;
use crate::config::ServerId;
use crate::error::{Error, Result};
use crate::lsp::{LspServer, ServerInitConfig};

/// How long a caller waits for a replacement before it is told to retry.
///
/// A respawn of a server that was running has a warm cache behind it, so
/// this is generous where the first-spawn budget in `routing.rs` is not.
pub(super) const RESPAWN_WAIT: Duration = Duration::from_secs(5);

/// Tracks respawn attempts for one server, so [`Translator::ensure_server`]
/// can back off a crash-looping process instead of retrying it on every
/// single tool call.
#[derive(Debug, Clone, Copy)]
pub(super) struct RespawnBackoff {
    /// Number of consecutive attempts that have not produced a server which
    /// stayed alive for at least [`RESPAWN_BACKOFF_BASE`]. A spawn failure
    /// counts immediately; a spawn that succeeds but is found dead again
    /// within that window counts too, once that is discovered -- see
    /// [`Translator::reconcile_respawn_stability`]. Without this, a server
    /// that starts, completes `initialize`, and then crashes a second later
    /// (a common real crash-loop shape) would bypass backoff entirely: each
    /// "success" would otherwise look like a fresh, unbacked-off start.
    consecutive_failures: u32,
    /// When the most recent attempt was made, or (if `last_attempt_succeeded`)
    /// when that success was last found to have not held up.
    last_attempt: Instant,
    /// Whether the most recent attempt completed `initialize` successfully.
    /// `false` for an outright spawn failure. Also reset to `false` once a
    /// "successful" respawn is found to have died again within the
    /// stability window, so that discovery is applied only once.
    last_attempt_succeeded: bool,
}

/// Base delay before the first backed-off retry after a respawn failure.
const RESPAWN_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Upper bound on the exponential backoff delay between respawn attempts.
const RESPAWN_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Publishes a terminal state for a spawn that ended without one.
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
        tracing::error!(
            "spawn task for LSP server '{}' ended without an outcome",
            self.id
        );
        self.translator.record_respawn_failure(&self.id);
        self.translator
            .set_lifecycle(&self.id, ServerLifecycle::Failed);
    }
}

impl Translator {
    /// Whether the server tracked under `id` is registered and has exited.
    ///
    /// Returns `false` ("not dead") for an `id` that isn't registered at
    /// all -- that's the separate `ServerInitializing`/`NoServerForTool`
    /// concern callers already handle, not something the respawn path
    /// should react to -- and for any `try_wait` error, on the conservative
    /// assumption that a health check that itself failed should not trigger
    /// a respawn.
    #[cfg(test)]
    pub(crate) fn is_server_dead(&self, id: &ServerId) -> bool {
        lock_std(&self.lsp_servers)
            .get_mut(id)
            .and_then(|server| server.has_exited().ok())
            .unwrap_or(false)
    }

    /// Remaining backoff delay before `id` may be respawned again, or
    /// `None` if it may be attempted right now.
    ///
    /// Only consults recorded *failures* -- a server with no recorded
    /// attempt is never backed off. A server whose last attempt "succeeded"
    /// is reconciled by [`Self::reconcile_respawn_stability`] (called by
    /// [`Self::ensure_server`] before this) into either a failure (died
    /// again too soon) or removed entirely (proven stable), so by the time
    /// this runs, a lingering "succeeded" entry never reaches here.
    fn respawn_backoff_remaining(&self, id: &ServerId) -> Option<Duration> {
        let (consecutive_failures, last_attempt) = {
            let entry = lock_std(&self.respawn_backoffs).get(id).copied()?;
            (entry.consecutive_failures, entry.last_attempt)
        };
        if consecutive_failures == 0 {
            return None;
        }
        let shift = consecutive_failures.saturating_sub(1).min(5);
        let delay = RESPAWN_BACKOFF_BASE
            .saturating_mul(1 << shift)
            .min(RESPAWN_BACKOFF_MAX);
        let elapsed = self.clock.now().saturating_duration_since(last_attempt);
        (elapsed < delay).then(|| delay.saturating_sub(elapsed))
    }

    /// Records a failed respawn attempt for `id`, extending its backoff.
    pub(crate) fn record_respawn_failure(&self, id: &ServerId) {
        let mut backoffs = lock_std(&self.respawn_backoffs);
        let entry = backoffs
            .entry(id.clone())
            .or_insert_with(|| RespawnBackoff {
                consecutive_failures: 0,
                last_attempt: self.clock.now(),
                last_attempt_succeeded: false,
            });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.last_attempt = self.clock.now();
        entry.last_attempt_succeeded = false;
        drop(backoffs);
    }

    /// Records that a respawn attempt for `id` completed `initialize`
    /// successfully.
    ///
    /// Does *not* clear `consecutive_failures`: whether this attempt
    /// actually broke the crash loop is only known once the server either
    /// stays alive for a while or is found dead again -- see
    /// [`Self::reconcile_respawn_stability`], which is what acts on this
    /// entry.
    fn record_respawn_success(&self, id: &ServerId) {
        let mut backoffs = lock_std(&self.respawn_backoffs);
        let entry = backoffs
            .entry(id.clone())
            .or_insert_with(|| RespawnBackoff {
                consecutive_failures: 0,
                last_attempt: self.clock.now(),
                last_attempt_succeeded: true,
            });
        entry.last_attempt = self.clock.now();
        entry.last_attempt_succeeded = true;
        drop(backoffs);
    }

    /// Reconciles `id`'s backoff state against a *newly observed* death,
    /// before deciding whether to back off this respawn attempt.
    ///
    /// A no-op unless the last recorded attempt "succeeded" ([`Self::record_respawn_success`]):
    /// - If it has since survived at least [`RESPAWN_BACKOFF_BASE`], it is
    ///   treated as proven stable and its backoff state is cleared -- a
    ///   later, unrelated crash starts a fresh backoff sequence rather than
    ///   inheriting history from a long-resolved incident.
    /// - Otherwise, the server died again before proving itself: this
    ///   counts as a failure (extending `consecutive_failures`) instead of
    ///   being silently forgotten. Without this, a server that starts,
    ///   completes `initialize`, and crashes again a moment later would
    ///   bypass backoff entirely -- every such cycle would look like a
    ///   fresh, unbacked-off start, spawning one child process per tool
    ///   call forever.
    fn reconcile_respawn_stability(&self, id: &ServerId) {
        let mut backoffs = lock_std(&self.respawn_backoffs);
        let Some(entry) = backoffs.get(id).copied() else {
            return;
        };
        if !entry.last_attempt_succeeded {
            return;
        }
        let now = self.clock.now();
        if now.saturating_duration_since(entry.last_attempt) >= RESPAWN_BACKOFF_BASE {
            backoffs.remove(id);
        } else if let Some(current) = backoffs.get_mut(id) {
            current.consecutive_failures = current.consecutive_failures.saturating_add(1);
            current.last_attempt = now;
            current.last_attempt_succeeded = false;
        }
        drop(backoffs);
    }

    /// Start `id` when it has no live client, then wait for its lifecycle outcome.
    /// `None` returns after starting; `Some(d)` waits at most `d`.
    pub(super) async fn ensure_server(
        &self,
        id: &ServerId,
        budget: Option<Duration>,
    ) -> Result<()> {
        if self.has_live_client(id) {
            return Ok(());
        }

        // Check membership before subscribing: subscribing creates an Idle
        // channel for unknown ids, which would otherwise wait for a spawn
        // that nobody started.
        match self.lifecycle_of(id) {
            None => {
                return Err(Error::ServerUnavailable {
                    server_id: id.clone(),
                    reason: "not applicable to this workspace".to_string(),
                });
            }
            Some(ServerLifecycle::NotInstalled) => {
                return Err(Error::ServerUnavailable {
                    server_id: id.clone(),
                    reason: "command not found when it was last attempted".to_string(),
                });
            }
            Some(_) => {}
        }

        let mut states = self.subscribe_lifecycle(id);

        self.reconcile_respawn_stability(id);
        if let Some(remaining) = self.respawn_backoff_remaining(id) {
            return Err(Error::ServerUnavailable {
                server_id: id.clone(),
                reason: format!("crash-looping, retry in {remaining:?}"),
            });
        }

        if self.begin_starting(id) {
            // A caller can read Starting while the active spawn finishes,
            // then claim the newly published Running state. Check again
            // after the claim so that race does not launch a duplicate.
            if self.restore_running_if_live(id) {
                return Ok(());
            }

            self.reconcile_respawn_stability(id);
            if let Some(remaining) = self.respawn_backoff_remaining(id) {
                self.set_lifecycle(id, ServerLifecycle::Failed);
                return Err(Error::ServerUnavailable {
                    server_id: id.clone(),
                    reason: format!("crash-looping, retry in {remaining:?}"),
                });
            }

            let Some(translator) = self.self_handle.get().and_then(Weak::upgrade) else {
                self.record_respawn_failure(id);
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

    fn has_live_client(&self, id: &ServerId) -> bool {
        if !lock_std(&self.lsp_clients).contains_key(id) {
            return false;
        }
        // Registration publishes the client immediately before the server.
        lock_std(&self.lsp_servers)
            .get_mut(id)
            .is_none_or(|server| !matches!(server.has_exited(), Ok(true)))
    }

    /// Return registered clients that remain live across an initial-batch rebind.
    pub(crate) fn registered_live_client_ids(&self) -> HashSet<ServerId> {
        let clients = lock_std(&self.lsp_clients);
        let mut servers = lock_std(&self.lsp_servers);
        clients
            .keys()
            .filter(|id| {
                servers
                    .get_mut(*id)
                    .is_none_or(|server| !matches!(server.has_exited(), Ok(true)))
            })
            .cloned()
            .collect()
    }

    fn restore_running_if_live(&self, id: &ServerId) -> bool {
        if self.has_live_client(id) {
            self.set_lifecycle(id, ServerLifecycle::Running);
            true
        } else {
            false
        }
    }

    /// Run the spawn attempt claimed by `begin_starting`.
    async fn run_spawn(self: Arc<Self>, id: ServerId) {
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
            Ok(caches_diagnostics) => {
                self.record_respawn_success(&id);
                if caches_diagnostics
                    && let Some(pumps) = self.notification_pumps.get()
                    && let Some(generation) = pumps.settle().diagnostics_baseline_generation(&id)
                {
                    tokio::spawn(crate::baseline_merge_task(
                        pumps.shared().clone(),
                        id.clone(),
                        generation,
                        pumps.cancel_rx(),
                    ));
                }
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

    async fn install_server(&self, id: &ServerId, config: ServerInitConfig) -> Result<bool> {
        let language_id = config.server_config.language_id.clone();

        tracing::info!("starting LSP server '{id}'");
        // Cleared before the replacement is spawned, not after: the fresh
        // process registers its own watchers during the `initialize`
        // handshake, and a clear running afterwards would drop those along
        // with the dead process's. Being here also means it runs even when
        // the spawn below then fails, deliberately unlike the document
        // tracker's clear further down: a glob belongs to the process that
        // asked for it, so once that process is gone the glob only produces
        // notify calls to a connection nobody is reading.
        self.forget_watch_registrations(id);
        let caches_diagnostics = self.is_diagnostics_route(&language_id, id);
        let mut replacement_attempt = if caches_diagnostics {
            self.notification_pumps
                .get()
                .map(|pumps| pumps.prepare_diagnostics_replacement(id))
        } else {
            None
        };
        let baseline_generation = if caches_diagnostics {
            self.notification_pumps
                .get()
                .and_then(|pumps| pumps.settle().begin_diagnostics_baseline_merge(id))
        } else {
            None
        };
        let mut new_server = match LspServer::spawn(config).await {
            Ok(server) => server,
            Err(error) => {
                if let (Some(pumps), Some(generation)) =
                    (self.notification_pumps.get(), baseline_generation)
                {
                    pumps
                        .settle()
                        .finish_diagnostics_baseline_merge(id, generation);
                }
                return Err(error);
            }
        };
        let new_client = new_server.client().clone();
        let notification_rx = new_server.take_notification_rx();
        let old_client = lock_std(&self.lsp_clients).get(id).cloned();
        if let Some(old_client) = old_client {
            old_client.fail_pending_requests().await;
        }
        if let Some(pumps) = self.notification_pumps.get() {
            pumps.retire(id).await;
        }
        if caches_diagnostics && let Some(cache) = &self.notification_cache {
            cache.lock().await.clear_server_diagnostics(id);
        }

        self.document_tracker.forget_server(id);
        if let Some(pumps) = self.notification_pumps.get() {
            pumps.install(id.clone(), notification_rx, caches_diagnostics);
            if caches_diagnostics {
                pumps.register_diagnostics_owner(id);
                pumps.settle().restart_deadline();
                if let Some(cache) = &self.notification_cache {
                    cache
                        .lock()
                        .await
                        .set_diagnostics_route_count(pumps.settle().diagnostics_owner_count());
                }
                if let Some(attempt) = replacement_attempt.as_mut() {
                    attempt.complete();
                }
            }
        }
        let old_server = lock_std(&self.lsp_servers).insert(id.clone(), new_server);
        lock_std(&self.lsp_clients).insert(id.clone(), new_client);
        drop(old_server);
        Ok(caches_diagnostics)
    }

    async fn await_terminal_state(
        &self,
        id: &ServerId,
        states: &mut tokio::sync::watch::Receiver<ServerLifecycle>,
        budget: Duration,
    ) -> Result<()> {
        let deadline = TokioInstant::now() + budget;
        loop {
            let state = *states.borrow_and_update();
            match state {
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::bridge::translator::ServerLifecycle;
    use crate::bridge::translator::clock::{Clock, FakeClock};
    use crate::config::ServerId;

    fn spawnable(translator: Translator, id: &ServerId) -> Arc<Translator> {
        let translator = Arc::new(translator);
        translator.set_self_handle(Arc::downgrade(&translator));
        translator.set_lifecycle(id, ServerLifecycle::Idle);
        translator
    }

    #[test]
    fn test_respawn_backoff_remaining_returns_none_once_delay_elapsed() {
        let clock = Arc::new(FakeClock::new());
        let translator = Translator::new().with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        let id = ServerId::from("rust");

        translator.record_respawn_failure(&id);
        assert!(
            translator.respawn_backoff_remaining(&id).is_some(),
            "immediately after a failure, the backoff window must still be active"
        );

        clock.advance(RESPAWN_BACKOFF_MAX);
        assert!(
            translator.respawn_backoff_remaining(&id).is_none(),
            "once the fake clock has advanced past the computed delay, \
             the backoff window must be reported as elapsed"
        );
    }

    #[test]
    fn test_reconcile_respawn_stability_clears_backoff_after_proven_stable() {
        let clock = Arc::new(FakeClock::new());
        let translator = Translator::new().with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        let id = ServerId::from("rust");

        translator.record_respawn_failure(&id);
        translator.record_respawn_success(&id);
        assert!(
            lock_std(&translator.respawn_backoffs).contains_key(&id),
            "a recorded success must still leave a backoff entry pending reconciliation"
        );

        clock.advance(RESPAWN_BACKOFF_BASE);
        translator.reconcile_respawn_stability(&id);

        assert!(
            !lock_std(&translator.respawn_backoffs).contains_key(&id),
            "once proven stable (survived at least RESPAWN_BACKOFF_BASE), \
             the backoff entry must be cleared entirely"
        );
    }

    #[test]
    fn test_is_server_dead_false_when_not_registered() {
        let translator = Translator::new();
        assert!(!translator.is_server_dead(&ServerId::from("rust")));
    }

    #[tokio::test]
    async fn test_a_server_absent_from_the_applicable_set_is_not_started() {
        let translator = Arc::new(Translator::new());
        translator.set_self_handle(Arc::downgrade(&translator));
        let id = ServerId::from("rust");

        let err = translator
            .ensure_server(&id, Some(Duration::from_millis(50)))
            .await
            .expect_err("an id nobody declared applicable cannot be started");

        assert!(matches!(err, Error::ServerUnavailable { .. }));
        assert_eq!(translator.lifecycle_of(&id), None);
    }

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
        assert_eq!(translator.respawn_backoff_remaining(&id), None);
    }

    #[tokio::test]
    async fn test_a_spawn_that_ends_without_publishing_lands_on_failed() {
        let translator = Arc::new(Translator::new());
        translator.set_self_handle(Arc::downgrade(&translator));
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Idle);

        let _ = translator
            .ensure_server(&id, Some(Duration::from_secs(1)))
            .await;

        assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Failed));
    }

    // Gated `#[cfg(unix)]`: this module's fake-LSP-server test double is a
    // hand-written `sh` script (POSIX parameter expansion, `printf`-framed
    // LSP responses, file-based invocation counters), which has no
    // equivalent on Windows. CI's "Test (unit)" job matrix includes
    // `windows-latest`.
    #[cfg(unix)]
    mod respawn_tests {
        use std::collections::{HashMap, HashSet};
        use std::fs;
        use std::path::{Path, PathBuf};

        use tempfile::TempDir;
        use tokio::time::Duration;

        use super::*;
        use crate::config::{LspServerConfig, ToolKind, ToolRouter};
        use crate::lsp::ServerInitConfig;

        /// Writes a `sh` script that answers the LSP `initialize` handshake
        /// with a canned response -- request id `1`, since a freshly spawned
        /// `LspClient`'s request counter always starts there -- and then
        /// exits shortly after, so `LspServer::spawn` succeeds but the
        /// process is already dead moments later. Stands in for "the server
        /// was alive, then crashed" without needing a real language server
        /// binary.
        ///
        /// The brief sleep before exiting matters: `LspServer::spawn` sends
        /// the `initialized` notification right after the `initialize`
        /// response arrives, and without it the process can (racily) have
        /// already exited by the time that notification is written to its
        /// stdin, failing the spawn itself instead of the respawn this is
        /// meant to seed.
        fn write_crash_after_init_script(dir: &Path) -> PathBuf {
            let script_path = dir.join("crash_after_init.sh");
            let body = r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
sleep 0.3
"#;
            fs::write(&script_path, body).unwrap();
            script_path
        }

        /// Like [`write_crash_after_init_script`], but stays alive for
        /// `sleep_secs` after responding instead of exiting immediately.
        fn write_responder_script(dir: &Path, sleep_secs: u64) -> PathBuf {
            let script_path = dir.join("responder.sh");
            let template = r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
sleep __SLEEP__
"#;
            fs::write(
                &script_path,
                template.replace("__SLEEP__", &sleep_secs.to_string()),
            )
            .unwrap();
            script_path
        }

        fn write_delayed_counting_server(dir: &Path) -> (PathBuf, PathBuf) {
            let script_path = dir.join("delayed_counting_server.py");
            let invocation_path = dir.join("invocations");
            let body = r#"import json, os, pathlib, sys, time

counter = pathlib.Path(sys.argv[1])
with counter.open("a") as invocations:
    invocations.write(f"{os.getpid()}\n")
    invocations.flush()

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
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()

while True:
    message = receive()
    if message is None:
        break
    if message.get("method") == "initialize":
        time.sleep(0.2)
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "capabilities": {"positionEncoding": "utf-16"}
        }})
    elif message.get("method") == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif message.get("method") == "exit":
        break
"#;
            fs::write(&script_path, body).unwrap();
            (script_path, invocation_path)
        }

        fn stub_server_config(id: &str, script: &Path) -> ServerInitConfig {
            ServerInitConfig {
                applies_edits: false,
                server_config: LspServerConfig {
                    language_id: id.to_string(),
                    command: "sh".to_string(),
                    args: vec![script.to_string_lossy().to_string()],
                    env: HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 5,
                    spawn: None,
                    request_timeout_seconds: 5,
                    heuristics: None,
                    name: Some(id.to_string()),
                    handles: None,
                    diagnostics_severity: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
                watch_registry: None,
            }
        }

        fn write_unexecutable_command(dir: &Path) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;

            let path = dir.join("not_executable");
            fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            path
        }

        /// Polls `is_server_dead` until it reports `true`, bounding the wait
        /// so a broken script fails the test instead of hanging it.
        async fn wait_until_dead(translator: &Translator, id: &ServerId) {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if translator.is_server_dead(id) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("seed server never reported as exited");
        }

        async fn wait_for_lifecycle(
            translator: &Translator,
            id: &ServerId,
            expected: ServerLifecycle,
        ) {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if translator.lifecycle_of(id) == Some(expected) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("server lifecycle did not reach the expected state");
        }

        #[tokio::test]
        async fn test_a_budget_that_expires_leaves_the_spawn_running() {
            let dir = TempDir::new().unwrap();
            let (script, invocation_path) = write_delayed_counting_server(dir.path());
            let id = ServerId::from("rust");
            let mut config = stub_server_config("rust", &script);
            config.server_config.command = "python3".to_string();
            config.server_config.args = vec![
                script.to_string_lossy().into_owned(),
                invocation_path.to_string_lossy().into_owned(),
            ];
            let translator = spawnable(Translator::new(), &id);
            translator.register_server_config(id.clone(), config);

            let err = translator
                .ensure_server(&id, Some(Duration::from_millis(10)))
                .await
                .expect_err("a delayed spawn outlasting its budget asks for a retry");
            assert!(matches!(err, Error::ServerInitializing { .. }));
            assert_eq!(
                translator.lifecycle_of(&id),
                Some(ServerLifecycle::Starting)
            );

            wait_for_lifecycle(&translator, &id, ServerLifecycle::Running).await;
            let invocations = fs::read_to_string(invocation_path).unwrap();
            assert_eq!(invocations.lines().count(), 1, "{invocations:?}");
            translator.shutdown_servers().await;
        }

        fn setup_initial_batch(
            config: &crate::config::ServerConfig,
            root: &Path,
            applicable: &[ServerInitConfig],
            watch_registry: &Arc<crate::lsp::WatchRegistry>,
        ) -> (
            Arc<Translator>,
            Arc<Mutex<crate::bridge::NotificationCache>>,
            crate::PumpShared,
        ) {
            let router =
                ToolRouter::from_configs(applicable.iter().map(|init| &init.server_config))
                    .unwrap();
            let notification_cache = Arc::new(Mutex::new(crate::bridge::NotificationCache::new()));
            let translator = crate::build_translator(
                config,
                vec![root.to_path_buf()],
                HashMap::new(),
                router,
                Arc::clone(&notification_cache),
                Arc::clone(watch_registry),
            );
            for init in applicable {
                let id = init.server_config.id();
                translator.register_server_config(id.clone(), init.clone());
                translator.set_lifecycle(&id, ServerLifecycle::Starting);
            }
            let shared = crate::PumpShared {
                notification_cache: Arc::clone(&notification_cache),
                subs: Arc::new(crate::bridge::ResourceSubscriptions::new()),
                workspace_roots: Arc::from(vec![root.to_path_buf()]),
                document_tracker: Arc::clone(translator.document_tracker()),
                settle: Arc::new(crate::bridge::ServerSettle::new(
                    Duration::from_millis(10),
                    Duration::from_millis(5000),
                )),
                delivery: Arc::new(Mutex::new(crate::bridge::DiagnosticsDelivery::new(
                    config.diagnostics,
                ))),
                floors: Arc::new(crate::bridge::FloorTable::new(
                    &config.diagnostics,
                    &config.lsp_servers,
                )),
            };
            (translator, notification_cache, shared)
        }

        fn add_initial_batch_fixture(config: &mut crate::config::ServerConfig, root: &Path) {
            let source = root.join("main.rs");
            fs::write(&source, "fn main() {}\n").unwrap();
            let fixture = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/notification_generations.py"
            );
            let control = root.join("rust");
            let initialized = root.join("rust.initialized");
            let published = root.join("rust.published");
            let initialize_attempt = root.join("rust.initialize-attempt");
            let mut running = LspServerConfig::rust_analyzer();
            running.heuristics = None;
            running.command = "python3".to_string();
            running.args = vec![
                fixture.to_string(),
                control.display().to_string(),
                crate::bridge::path_to_uri(&source).unwrap().to_string(),
                "rust".to_string(),
                "default".to_string(),
                "0".to_string(),
                "0".to_string(),
                initialized.display().to_string(),
                published.display().to_string(),
                initialize_attempt.display().to_string(),
            ];
            running.timeout_seconds = 5;
            config.lsp_servers.push(running);
        }

        async fn run_initial_batch(
            applicable: Vec<ServerInitConfig>,
            translator: Arc<Translator>,
            shared: crate::PumpShared,
        ) {
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            translator.notification_pumps.get_or_init(|| {
                crate::notification_lifecycle::NotificationPumps::new(
                    shared.clone(),
                    cancel_rx.clone(),
                )
            });
            let init_task = crate::spawn_lsp_servers_background(
                applicable,
                HashSet::new(),
                Arc::clone(&translator),
                cancel_rx,
                shared,
            );
            tokio::time::timeout(Duration::from_secs(10), init_task)
                .await
                .expect("the initial batch should finish within its spawn budget")
                .expect("the background batch should not panic");
            let _ = cancel_tx.send(true);
            translator.shutdown_servers().await;
        }

        #[tokio::test]
        async fn test_background_spawn_batch_publishes_lifecycle_outcomes() {
            use std::os::unix::fs::PermissionsExt;

            let dir = TempDir::new().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            let non_executable = root.join("not_executable");
            fs::write(&non_executable, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o644)).unwrap();

            let mut config = crate::config::ServerConfig::default();
            config.workspace.roots = vec![root.clone()];
            config.diagnostics.hooks.enabled = false;
            add_initial_batch_fixture(&mut config, &root);

            let mut missing = LspServerConfig::pyright();
            missing.heuristics = None;
            missing.name = Some("missing".to_string());
            missing.command = "mcpls-missing-command-for-lifecycle-test".to_string();
            missing.args.clear();

            let mut denied = LspServerConfig::typescript();
            denied.heuristics = None;
            denied.name = Some("denied".to_string());
            denied.command = non_executable.display().to_string();
            denied.args.clear();
            config.lsp_servers.extend([missing, denied]);

            let watch_registry = Arc::new(crate::lsp::WatchRegistry::new());
            let applicable = crate::applicable_server_configs(
                &config,
                std::slice::from_ref(&root),
                None,
                &watch_registry,
            );
            let (translator, _cache, shared) =
                setup_initial_batch(&config, &root, &applicable, &watch_registry);
            run_initial_batch(applicable, Arc::clone(&translator), shared).await;

            let rust_id = ServerId::from("rust");
            let missing_id = ServerId::from("missing");
            let denied_id = ServerId::from("denied");
            assert_eq!(
                translator.lifecycle_of(&rust_id),
                Some(ServerLifecycle::Running)
            );
            assert_eq!(
                translator.lifecycle_of(&missing_id),
                Some(ServerLifecycle::NotInstalled)
            );
            assert_eq!(
                translator.lifecycle_of(&denied_id),
                Some(ServerLifecycle::Failed)
            );

            let backoff = translator
                .ensure_server(&denied_id, Some(Duration::ZERO))
                .await
                .expect_err("a failed initial spawn must enter backoff");
            assert!(matches!(backoff, Error::ServerUnavailable { .. }));
        }

        #[tokio::test]
        async fn test_all_failed_initial_batch_publishes_failures_before_returning() {
            use std::os::unix::fs::PermissionsExt;

            let dir = TempDir::new().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            let non_executable = root.join("not_executable");
            fs::write(&non_executable, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o644)).unwrap();

            let mut config = crate::config::ServerConfig::default();
            config.workspace.roots = vec![root.clone()];
            config.diagnostics.hooks.enabled = false;
            let mut missing = LspServerConfig::pyright();
            missing.heuristics = None;
            missing.name = Some("missing".to_string());
            missing.command = "mcpls-missing-command-for-lifecycle-test".to_string();
            missing.args.clear();
            let mut denied = LspServerConfig::typescript();
            denied.heuristics = None;
            denied.name = Some("denied".to_string());
            denied.command = non_executable.display().to_string();
            denied.args.clear();
            config.lsp_servers = vec![missing, denied];

            let watch_registry = Arc::new(crate::lsp::WatchRegistry::new());
            let applicable = crate::applicable_server_configs(
                &config,
                std::slice::from_ref(&root),
                None,
                &watch_registry,
            );
            let (translator, _cache, shared) =
                setup_initial_batch(&config, &root, &applicable, &watch_registry);
            run_initial_batch(applicable, Arc::clone(&translator), shared).await;

            assert_eq!(
                translator.lifecycle_of(&ServerId::from("missing")),
                Some(ServerLifecycle::NotInstalled)
            );
            assert_eq!(
                translator.lifecycle_of(&ServerId::from("denied")),
                Some(ServerLifecycle::Failed)
            );
            assert!(matches!(
                translator
                    .ensure_server(&ServerId::from("denied"), Some(Duration::ZERO))
                    .await,
                Err(Error::ServerUnavailable { .. })
            ));
        }

        #[tokio::test]
        async fn test_ensure_server_noop_when_server_alive() {
            let dir = TempDir::new().unwrap();
            let script = write_responder_script(dir.path(), 1);
            let id = ServerId::from("rust");
            let config = stub_server_config("rust", &script);

            let server = LspServer::spawn(config).await.unwrap();
            let translator = Translator::new();
            translator.register_client(id.clone(), server.client().clone());
            translator.register_server(id.clone(), server);
            // Deliberately no `register_server_config`: if a respawn were
            // (wrongly) attempted despite the server being alive, the
            // missing config would surface as `Error::ServerUnavailable`
            // instead of quietly succeeding -- so `Ok(())` here is proof
            // the alive fast path skipped respawning entirely.

            assert!(
                translator
                    .ensure_server(&id, Some(RESPAWN_WAIT))
                    .await
                    .is_ok()
            );
        }

        #[tokio::test]
        async fn test_ensure_server_errors_when_no_config_registered() {
            let dir = TempDir::new().unwrap();
            let script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let config = stub_server_config("rust", &script);

            let server = LspServer::spawn(config).await.unwrap();
            let translator = spawnable(Translator::new(), &id);
            translator.register_client(id.clone(), server.client().clone());
            translator.register_server(id.clone(), server);
            wait_until_dead(&translator, &id).await;

            let err = translator
                .ensure_server(&id, Some(RESPAWN_WAIT))
                .await
                .unwrap_err();
            assert!(
                matches!(err, Error::ServerUnavailable { .. }),
                "got {err:?}"
            );
        }

        #[tokio::test]
        async fn test_ensure_server_reports_a_missing_command_as_not_installed() {
            let dir = TempDir::new().unwrap();
            let script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &script);

            let server = LspServer::spawn(seed_config).await.unwrap();
            let translator = spawnable(Translator::new(), &id);
            translator.register_client(id.clone(), server.client().clone());
            translator.register_server(id.clone(), server);
            wait_until_dead(&translator, &id).await;

            let mut broken = stub_server_config("rust", &script);
            broken.server_config.command = "nonexistent-lsp-cmd-xyz".to_string();
            broken.server_config.args.clear();
            translator.register_server_config(id.clone(), broken);

            let err = translator
                .ensure_server(&id, Some(RESPAWN_WAIT))
                .await
                .unwrap_err();
            assert!(
                matches!(err, Error::ServerUnavailable { .. }),
                "got {err:?}"
            );
            assert_eq!(
                translator.lifecycle_of(&id),
                Some(ServerLifecycle::NotInstalled)
            );
        }

        /// #249: two concurrent tool calls that both observe the same dead
        /// server must not each perform their own respawn -- only one
        /// replacement process should ever be spawned, and both callers
        /// must still resolve successfully.
        ///
        /// The fake server script counts every invocation and, on its
        /// first run only, exits right after answering `initialize`
        /// (simulating "was alive, then crashed"); every later invocation
        /// answers and then sleeps, standing in for a healthy replacement.
        /// If the lifecycle claim gate were broken, both callers would
        /// spawn their own replacement and the invocation count would be
        /// 3 (seed + two independent respawns) instead of 2 (seed + one
        /// shared respawn).
        #[tokio::test]
        async fn test_ensure_server_single_flights_concurrent_callers() {
            let dir = TempDir::new().unwrap();
            let marker = dir.path().join("marker");
            let counter = dir.path().join("invocations");
            let script_path = dir.path().join("flaky.sh");
            let template = r#"echo x >> "__COUNTER__"
if [ -f "__MARKER__" ]; then
  body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
  printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
  sleep 1
else
  touch "__MARKER__"
  body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
  printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
  sleep 0.3
fi
"#;
            let script_body = template
                .replace("__COUNTER__", &counter.display().to_string())
                .replace("__MARKER__", &marker.display().to_string());
            fs::write(&script_path, script_body).unwrap();

            let id = ServerId::from("rust");
            let config = stub_server_config("rust", &script_path);

            let seed = LspServer::spawn(config.clone()).await.unwrap();
            let translator = spawnable(Translator::new(), &id);
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            translator.register_server_config(id.clone(), config);
            wait_until_dead(&translator, &id).await;

            let (t1, id1) = (Arc::clone(&translator), id.clone());
            let (t2, id2) = (Arc::clone(&translator), id.clone());
            let (r1, r2) = tokio::join!(
                tokio::spawn(async move { t1.ensure_server(&id1, Some(RESPAWN_WAIT)).await }),
                tokio::spawn(async move { t2.ensure_server(&id2, Some(RESPAWN_WAIT)).await }),
            );
            assert!(r1.unwrap().is_ok());
            assert!(r2.unwrap().is_ok());

            let invocations = fs::read_to_string(&counter).unwrap();
            assert_eq!(
                invocations.lines().count(),
                2,
                "expected exactly one seed spawn + one single-flighted \
                 respawn, got:\n{invocations}"
            );
        }

        #[tokio::test]
        async fn test_ensure_server_rechecks_a_live_server_after_claim() {
            let dir = TempDir::new().unwrap();
            let script = write_responder_script(dir.path(), 1);
            let id = ServerId::from("rust");
            let server = LspServer::spawn(stub_server_config("rust", &script))
                .await
                .unwrap();
            let translator = spawnable(Translator::new(), &id);
            translator.register_client(id.clone(), server.client().clone());
            translator.register_server(id.clone(), server);
            translator.set_lifecycle(&id, ServerLifecycle::Running);

            assert!(translator.begin_starting(&id));
            assert!(translator.restore_running_if_live(&id));
            assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Running));

            translator.shutdown_servers().await;
        }

        #[tokio::test]
        async fn test_ensure_server_without_budget_returns_before_spawn_finishes() {
            let dir = TempDir::new().unwrap();
            let script = write_responder_script(dir.path(), 1);
            let id = ServerId::from("rust");
            let translator = spawnable(Translator::new(), &id);
            translator.register_server_config(id.clone(), stub_server_config("rust", &script));

            translator.ensure_server(&id, None).await.unwrap();

            assert_eq!(
                translator.lifecycle_of(&id),
                Some(ServerLifecycle::Starting)
            );
            wait_for_lifecycle(&translator, &id, ServerLifecycle::Running).await;
            translator.shutdown_servers().await;
        }

        /// A second call inside the backoff window must not attempt another
        /// real spawn. The unchanged window proves it short-circuited.
        #[tokio::test]
        async fn test_ensure_server_backs_off_after_repeated_failure() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();
            let clock = Arc::new(FakeClock::new());
            let translator = spawnable(
                Translator::new().with_clock(Arc::clone(&clock) as Arc<dyn Clock>),
                &id,
            );
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            let mut broken = stub_server_config("rust", &seed_script);
            broken.server_config.command = write_unexecutable_command(dir.path())
                .to_string_lossy()
                .to_string();
            broken.server_config.args.clear();
            translator.register_server_config(id.clone(), broken);

            let err1 = translator
                .ensure_server(&id, Some(RESPAWN_WAIT))
                .await
                .unwrap_err();
            assert!(
                matches!(err1, Error::ServerUnavailable { .. }),
                "first attempt should be a real (failed) spawn, got {err1:?}"
            );
            assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Failed));
            let window = translator
                .respawn_backoff_remaining(&id)
                .expect("a failed spawn opens a backoff window");

            let err2 = translator
                .ensure_server(&id, Some(RESPAWN_WAIT))
                .await
                .unwrap_err();
            assert!(
                matches!(err2, Error::ServerUnavailable { .. }),
                "second call within the backoff window must fail fast \
                 without attempting another real spawn, got {err2:?}"
            );
            assert_eq!(
                translator.respawn_backoff_remaining(&id),
                Some(window),
                "a second attempt would have recorded a second failure and lengthened the window"
            );
        }

        /// #292 regression: once the backoff window has elapsed, the next
        /// `ensure_server` call must actually attempt a fresh spawn
        /// instead of continuing to fail fast -- proven by swapping in a
        /// config that succeeds and observing `Ok(())`, not merely a
        /// different error kind.
        #[tokio::test]
        async fn test_ensure_server_reattempts_once_backoff_window_elapses() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();
            let clock = Arc::new(FakeClock::new());
            let translator = spawnable(
                Translator::new().with_clock(Arc::clone(&clock) as Arc<dyn Clock>),
                &id,
            );
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            let mut broken = stub_server_config("rust", &seed_script);
            broken.server_config.command = write_unexecutable_command(dir.path())
                .to_string_lossy()
                .to_string();
            broken.server_config.args.clear();
            translator.register_server_config(id.clone(), broken);

            let err1 = translator
                .ensure_server(&id, Some(RESPAWN_WAIT))
                .await
                .unwrap_err();
            assert!(
                matches!(err1, Error::ServerUnavailable { .. }),
                "first attempt should be a real (failed) spawn, got {err1:?}"
            );

            let err2 = translator
                .ensure_server(&id, Some(RESPAWN_WAIT))
                .await
                .unwrap_err();
            assert!(
                matches!(err2, Error::ServerUnavailable { .. }),
                "second call within the backoff window must still fail fast, got {err2:?}"
            );

            // Advance well past the computed backoff delay and swap in a
            // config that will actually succeed this time.
            clock.advance(RESPAWN_BACKOFF_MAX);
            let working_script = write_crash_after_init_script(dir.path());
            translator
                .register_server_config(id.clone(), stub_server_config("rust", &working_script));

            let result = translator.ensure_server(&id, Some(RESPAWN_WAIT)).await;
            assert!(
                result.is_ok(),
                "once the backoff window has elapsed, ensure_server must actually \
                 reattempt a respawn instead of continuing to short-circuit, got {result:?}"
            );
        }

        /// #249 R3 regression: a respawn that *succeeds* (completes
        /// `initialize`) but dies again almost immediately must still
        /// engage backoff -- this is the more realistic crash-loop shape
        /// (start, initialize, then OOM-die a second later) than an
        /// outright spawn failure, and without this fix every such cycle
        /// looked like a fresh, unbacked-off start, spawning one child
        /// process per tool call forever.
        #[tokio::test]
        async fn test_ensure_server_backs_off_after_quick_recrash_following_success() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();
            let clock = Arc::new(FakeClock::new());
            let translator = spawnable(
                Translator::new().with_clock(Arc::clone(&clock) as Arc<dyn Clock>),
                &id,
            );
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            // Reuse the same crash-after-init script as the respawn target:
            // every attempt completes `initialize` successfully, then dies
            // ~0.3s later -- a post-init crash loop, not a spawn failure.
            translator.register_server_config(id.clone(), stub_server_config("rust", &seed_script));

            translator
                .ensure_server(&id, Some(RESPAWN_WAIT))
                .await
                .expect("the replacement completes initialize, so this attempt succeeds");
            wait_until_dead(&translator, &id).await;

            let err = translator
                .ensure_server(&id, Some(RESPAWN_WAIT))
                .await
                .unwrap_err();
            assert!(
                matches!(err, Error::ServerUnavailable { .. }),
                "a respawn that dies again within the stability window must \
                 back off instead of being treated as a fresh attempt, got {err:?}"
            );
        }

        /// #249 C1 regression: respawning the *diagnostics-route* server
        /// for a language must invalidate that server's diagnostics cache
        /// entries, rather than leaving stale entries to be merged into
        /// fresh pull results as if still current -- the crashed process's
        /// pump is gone and will never update or clear them itself.
        ///
        /// Covers the "under-clear" failure mode a scoped-to-synced-URIs
        /// clear has: a real diagnostics-route server (e.g. rust-analyzer)
        /// publishes workspace-wide (`cargo check` results for files never
        /// opened through mcpls), so `never_opened_uri` below stands in for
        /// an entry that must still be cleared despite never having gone
        /// through `ensure_open`.
        ///
        /// #266 S2 regression (over-clear direction, multi-language case):
        /// `other_language_uri` is owned by a *different* diagnostics-route
        /// server (e.g. pyright for Python, in the same workspace as the
        /// rust-analyzer under test here) and must survive -- `clear_server_diagnostics`
        /// replaced a workspace-wide `clear_all_diagnostics` that used to
        /// wipe every language's cache on any single server's respawn.
        #[tokio::test]
        async fn test_ensure_server_clears_diagnostics_cache_when_diagnostics_route() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();

            let cache = Arc::new(Mutex::new(crate::bridge::NotificationCache::new()));
            let translator = spawnable(
                Translator::new()
                    .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]))
                    .with_notification_cache(Arc::clone(&cache)),
                &id,
            );
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);

            let synced_uri: lsp_types::Uri = "file:///workspace/opened.rs".parse().unwrap();
            let never_opened_uri: lsp_types::Uri =
                "file:///workspace/never_opened.rs".parse().unwrap();
            let other_language_uri: lsp_types::Uri = "file:///workspace/main.py".parse().unwrap();
            cache
                .lock()
                .await
                .store_diagnostics(&id, &synced_uri, None, vec![]);
            cache
                .lock()
                .await
                .store_diagnostics(&id, &never_opened_uri, None, vec![]);
            cache.lock().await.store_diagnostics(
                &ServerId::from("python"),
                &other_language_uri,
                None,
                vec![],
            );

            wait_until_dead(&translator, &id).await;

            let respawn_script = write_responder_script(dir.path(), 1);
            translator
                .register_server_config(id.clone(), stub_server_config("rust", &respawn_script));

            translator
                .ensure_server(&id, Some(RESPAWN_WAIT))
                .await
                .unwrap();

            let guard = cache.lock().await;
            assert!(
                guard.get_diagnostics(synced_uri.as_str()).is_none(),
                "diagnostics attributed to the crashed connection must be \
                 invalidated on respawn, not served as current"
            );
            assert!(
                guard.get_diagnostics(never_opened_uri.as_str()).is_none(),
                "workspace-wide diagnostics for a file mcpls never opened \
                 must also be invalidated, not just synced documents"
            );
            assert!(
                guard.get_diagnostics(other_language_uri.as_str()).is_some(),
                "a different diagnostics-route server's entries must survive \
                 an unrelated server's respawn-triggered cache clear"
            );
            drop(guard);
        }

        /// A respawned process registers again with fresh ids, so the
        /// globs the dead one left behind have to go: otherwise a server
        /// that narrowed its watch keeps being told about files it no
        /// longer wants, for the life of the mcpls process.
        ///
        /// Drives a real `ensure_server` rather than calling
        /// `forget_watch_registrations` directly, so deleting the clear
        /// from the respawn path fails this.
        #[tokio::test]
        async fn test_ensure_server_clears_that_server_s_watch_registrations() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let other = ServerId::from("python");

            let seed = LspServer::spawn(stub_server_config("rust", &seed_script))
                .await
                .unwrap();

            let registry = Arc::new(crate::lsp::WatchRegistry::new());
            let translator = spawnable(
                Translator::new().with_watch_registry(Arc::clone(&registry)),
                &id,
            );
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);

            registry.register(
                &id,
                "r1",
                &serde_json::json!([{ "globPattern": "**/*.rs" }]),
            );
            registry.register(
                &other,
                "r2",
                &serde_json::json!([{ "globPattern": "**/*.py" }]),
            );

            wait_until_dead(&translator, &id).await;

            let respawn_script = write_responder_script(dir.path(), 1);
            translator
                .register_server_config(id.clone(), stub_server_config("rust", &respawn_script));

            translator
                .ensure_server(&id, Some(RESPAWN_WAIT))
                .await
                .unwrap();

            assert!(
                registry
                    .servers_for(
                        &dir.path().join("main.rs"),
                        lsp_types::FileChangeType::CHANGED
                    )
                    .is_empty(),
                "the crashed process's globs must not outlive it"
            );
            assert_eq!(
                registry.servers_for(
                    &dir.path().join("main.py"),
                    lsp_types::FileChangeType::CHANGED
                ),
                vec![other],
                "one server's respawn must not drop another server's registrations"
            );
        }

        /// #249 C1 regression (over-clear direction): respawning a server
        /// that is *not* the diagnostics route for its language must not
        /// touch the cache at all -- otherwise a crashed hover-only server
        /// would wipe out a healthy, still-running diagnostics server's
        /// valid entries for the same files.
        #[tokio::test]
        async fn test_ensure_server_does_not_clear_cache_when_not_diagnostics_route() {
            use crate::config::LspServerConfig;

            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let hover_id = ServerId::from("hover-only");
            let hover_seed_config = stub_server_config("hover-only", &seed_script);

            let seed = LspServer::spawn(hover_seed_config).await.unwrap();

            // `hover_id` handles only Hover; a separate (never-registered
            // here, purely routing-table) server is the catch-all and thus
            // the diagnostics route.
            let configs = [
                LspServerConfig {
                    language_id: "rust".to_string(),
                    command: "sh".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 5,
                    spawn: None,
                    request_timeout_seconds: 5,
                    heuristics: None,
                    name: Some("hover-only".to_string()),
                    handles: Some(vec![ToolKind::Hover]),
                    diagnostics_severity: None,
                },
                LspServerConfig {
                    language_id: "rust".to_string(),
                    command: "sh".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 5,
                    spawn: None,
                    request_timeout_seconds: 5,
                    heuristics: None,
                    name: Some("diag-catchall".to_string()),
                    handles: None,
                    diagnostics_severity: None,
                },
            ];
            let router = ToolRouter::from_configs(configs.iter()).unwrap();

            let cache = Arc::new(Mutex::new(crate::bridge::NotificationCache::new()));
            let translator = spawnable(
                Translator::new()
                    .with_router(router)
                    .with_notification_cache(Arc::clone(&cache)),
                &hover_id,
            );
            translator.register_client(hover_id.clone(), seed.client().clone());
            translator.register_server(hover_id.clone(), seed);

            let owned_by_healthy_server: lsp_types::Uri =
                "file:///workspace/still_healthy.rs".parse().unwrap();
            cache
                .lock()
                .await
                .store_diagnostics(&hover_id, &owned_by_healthy_server, None, vec![]);

            wait_until_dead(&translator, &hover_id).await;

            let respawn_script = write_responder_script(dir.path(), 1);
            // `language_id` must match the router's ("rust"), not the
            // routing identity ("hover-only"): otherwise `is_diagnostics_route`
            // returns `false` because of a language mismatch rather than
            // because of the `handles: Some([Hover])` restriction this test
            // means to exercise, which would pass for the wrong reason.
            let mut respawn_config = stub_server_config("hover-only", &respawn_script);
            respawn_config.server_config.language_id = "rust".to_string();
            translator.register_server_config(hover_id.clone(), respawn_config);

            translator
                .ensure_server(&hover_id, Some(RESPAWN_WAIT))
                .await
                .unwrap();

            assert!(
                cache
                    .lock()
                    .await
                    .get_diagnostics(owned_by_healthy_server.as_str())
                    .is_some(),
                "respawning a non-diagnostics-route server must not clear \
                 the diagnostics-route server's cache entries"
            );
        }

        /// #249 test-gap closure: proves `resolve_client_for_file`'s
        /// dead-server branch is actually reached through the shared
        /// entry point every public tool handler (`handle_hover`,
        /// `handle_definition`, ...) funnels through -- not just through
        /// the private `ensure_server`/`is_server_dead` calls the other
        /// tests in this module make directly.
        #[tokio::test]
        async fn test_prepare_document_respawns_dead_server_through_shared_entry_point() {
            let dir = TempDir::new().unwrap();
            let workspace = dir.path();
            let file_path = workspace.join("main.rs");
            fs::write(&file_path, "fn main() {}").unwrap();

            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();
            let mut translator = Translator::new()
                .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]))
                .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
            translator.set_workspace_roots(vec![workspace.to_path_buf()]);
            let translator = spawnable(translator, &id);
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            let respawn_script = write_responder_script(dir.path(), 1);
            translator
                .register_server_config(id.clone(), stub_server_config("rust", &respawn_script));

            let result = translator
                .prepare_document(&file_path.to_string_lossy(), ToolKind::Hover)
                .await;
            assert!(result.is_ok(), "got {result:?}");

            assert!(
                !translator.is_server_dead(&id),
                "the respawned replacement should be alive"
            );
        }
    }
}
