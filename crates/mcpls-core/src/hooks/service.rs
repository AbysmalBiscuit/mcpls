//! Serving the hook socket's operations from a running mcpls.
//!
//! One process per project owns the socket and answers every session's
//! hooks against the same per-session record its own flush tool uses, so a
//! hook and an agent never see the same diagnostic twice. Any other mcpls
//! in the same project is passive: it forwards rather than answering, and
//! keeps competing for the lock so an owner exiting does not strand it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::watch;

use crate::bridge::{SessionId, lock_std};
use crate::hooks::identity::SocketIdentity;
#[cfg(test)]
use crate::hooks::listener::LockLoss;
use crate::hooks::listener::{HookListener, ServeExit};
use crate::hooks::protocol::{Request, Response};
use crate::hooks::sweep::Sweeper;
use crate::mcp::McplsServer;

/// How long a passive instance waits between attempts on the ownership
/// lock.
///
/// Short enough that an agent's next turn reaches a process that has taken
/// over, long enough that a project left running with several instances
/// costs nothing measurable.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// How this process relates to the project's hook socket.
///
/// Interior mutability because it changes at runtime: a passive instance
/// retries the lock every five seconds and becomes the owner when the
/// previous one exits.
pub struct HookRole {
    /// The current role.
    ///
    /// A `std::sync::Mutex` rather than a `tokio` one because every
    /// critical section is a clone or a single assignment with no `.await`
    /// inside it, which is what makes a blocking mutex correct on an async
    /// runtime.
    role: std::sync::Mutex<Role>,
    /// A count that rises on every transition.
    ///
    /// A watcher subscribed before the event it cares about sees exactly
    /// one change when the transition happens, so a caller can await the
    /// transition itself instead of sampling the role until it happens to
    /// flip. The role stays behind the mutex; this carries only the fact
    /// that it moved.
    transitions: watch::Sender<u64>,
    /// How many `Changed`, `Flush`, or `EndSession` requests this process
    /// has answered while it held the socket.
    ///
    /// Never reset on a role transition: a process promoted from `Passive`
    /// to `Owner` starts counting from zero for the hooks it personally
    /// answers, which is exactly what `mcpls hook doctor` needs to tell "a
    /// plugin that has never fired a hook against this process" apart from
    /// "a server that has been serving them all along".
    hooks_seen: AtomicU64,
}

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
    /// A process with hooks switched off.
    #[must_use]
    pub fn disabled() -> Self {
        Self::starting_at(Role::Disabled)
    }

    /// A process that won the ownership lock.
    #[must_use]
    pub fn owner() -> Self {
        Self::starting_at(Role::Owner)
    }

    /// A process that lost the ownership lock to whoever holds `identity`.
    #[must_use]
    pub fn passive(identity: SocketIdentity) -> Self {
        Self::starting_at(Role::Passive { identity })
    }

    fn starting_at(role: Role) -> Self {
        Self {
            role: std::sync::Mutex::new(role),
            transitions: watch::channel(0).0,
            hooks_seen: AtomicU64::new(0),
        }
    }

    /// Record one `Changed`, `Flush`, or `EndSession` request this process
    /// just answered. Not called for `Status`, which is `mcpls hook
    /// doctor` probing rather than a hook firing, and counting it would
    /// make every doctor run look like a working install, nor for `Ack`,
    /// which is the second half of a `Flush` already counted.
    pub(crate) fn record_hook_request(&self) {
        self.hooks_seen.fetch_add(1, Ordering::Relaxed);
    }

    /// How many hook requests this process has answered so far.
    #[must_use]
    pub fn hooks_seen(&self) -> u64 {
        self.hooks_seen.load(Ordering::Relaxed)
    }

    /// A cloned snapshot, so no `std::sync::Mutex` guard is held across an
    /// `.await`.
    #[must_use]
    pub fn get(&self) -> Role {
        lock_std(&self.role).clone()
    }

    /// Run a synchronous local-consumption operation while the role is active.
    pub(crate) fn with_active_role<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
        let role = lock_std(&self.role);
        if matches!(*role, Role::Passive { .. }) {
            return None;
        }
        let result = operation();
        drop(role);
        Some(result)
    }

    /// A receiver that changes on every role transition.
    ///
    /// Subscribe before triggering the work under test, then await
    /// `changed()` rather than sampling `get` until it happens to flip: the
    /// transition follows a lock acquisition that runs on a blocking
    /// thread, which no amount of yielding on the runtime is guaranteed to
    /// have observed. Test-only.
    #[cfg(test)]
    pub(crate) fn subscribe_transitions(&self) -> watch::Receiver<u64> {
        self.transitions.subscribe()
    }

    /// Install `role`, telling every watcher only when it actually moved.
    ///
    /// Re-promoting a process that never stopped being the owner is not a
    /// transition, and signalling one would let a caller awaiting the
    /// signal conclude something changed when nothing did.
    fn set(&self, role: Role) {
        {
            let mut current = lock_std(&self.role);
            if *current == role {
                return;
            }
            *current = role;
        }
        self.transitions.send_modify(|count| *count += 1);
    }

    /// Move this process from `Passive` to `Owner`, called once it wins
    /// the lock.
    pub fn promote_to_owner(&self) {
        self.set(Role::Owner);
    }

    /// Move this process back to `Passive` when a serving listener can no
    /// longer prove that it owns the lock.
    ///
    /// A competitor holds the lock for its whole life, so a process that
    /// kept reading `Owner` would run its footer, answer from its own
    /// record and forward nothing, while every hook for the same session
    /// reached the competitor's separate record: two processes behaving as
    /// owners for one session, which is what the lock exists to make
    /// impossible. A successful reacquisition promotes the process again.
    pub fn demote_to_passive(&self, identity: SocketIdentity) {
        self.set(Role::Passive { identity });
    }
}

/// A project's socket identity together with the canonicalized directory
/// it was derived from.
///
/// Bundled into one value so a `Status` answer can report both without
/// every function that threads them from `serve_with` down to
/// `build_handler` growing a parameter for each.
#[derive(Debug, Clone)]
pub struct HookLocation {
    /// Where the socket and its ownership lock live.
    pub identity: SocketIdentity,
    /// The directory this process canonicalized at startup, before
    /// `identity` was derived from it.
    pub root: std::path::PathBuf,
}

/// The handler [`HookListener::serve`] runs, closing over the MCP server
/// and the sweeper.
///
/// Lock order, the same one the rest of the crate follows: delivery before
/// cache. Every path through here reaches the cache only by way of
/// `McplsServer::flush_for_hook`, which takes delivery then cache and drops
/// both before it awaits the payload build, and reaches delivery alone
/// only by way of `McplsServer::commit_for_hook` and `end_session`. No arm
/// of this match takes either lock itself. Do not add one that does.
pub fn build_handler(
    server: Arc<McplsServer>,
    sweeper: Arc<Sweeper>,
    location: HookLocation,
    role: Arc<HookRole>,
    cancel: watch::Receiver<bool>,
) -> impl Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static {
    move |request| {
        let server = Arc::clone(&server);
        let sweeper = Arc::clone(&sweeper);
        let location = location.clone();
        let role = Arc::clone(&role);
        let cancelled = *cancel.borrow();
        Box::pin(async move {
            match request {
                Request::Changed { paths, .. } => {
                    // Nothing drains the pending set once the sweep loop has
                    // stopped, so paths accepted after cancellation would
                    // accumulate without limit and never be swept or
                    // reported. Refusing says so instead.
                    if cancelled {
                        return Response::Error {
                            message: "mcpls is shutting down; these paths were not queued"
                                .to_string(),
                        };
                    }
                    role.record_hook_request();
                    // The event kind is discarded: the sweep stats.
                    Response::Changed {
                        queued: sweeper.enqueue(&paths),
                    }
                }
                Request::Flush { session } => {
                    role.record_hook_request();
                    let session = SessionId::from(session);
                    let (report, token) = server.flush_for_hook(&session).await;
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(text) = report {
                        parts.push(text);
                    }
                    if let Some(shortfall) = sweeper.last_shortfall() {
                        parts.push(shortfall);
                    }
                    Response::Flush {
                        context: (!parts.is_empty()).then(|| parts.join("\n")),
                        token,
                    }
                }
                Request::Ack { session, token } => {
                    server
                        .commit_for_hook(&SessionId::from(session), token)
                        .await;
                    Response::Ack
                }
                Request::EndSession { session } => {
                    role.record_hook_request();
                    server.end_session(&SessionId::from(session)).await;
                    Response::EndSession
                }
                Request::Status => Response::Status {
                    hash: location.identity.hash.clone(),
                    socket: location.identity.socket.clone(),
                    pid: std::process::id(),
                    owner: true,
                    root: location.root.clone(),
                    hooks_seen: role.hooks_seen(),
                },
            }
        })
    }
}

#[cfg(test)]
struct LockLossPause {
    identity: SocketIdentity,
    loss: LockLoss,
    arrived: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
static LOCK_LOSS_PAUSE: std::sync::Mutex<Option<LockLossPause>> = std::sync::Mutex::new(None);

#[cfg(all(test, unix))]
struct LockLossPauseGuard;

#[cfg(all(test, unix))]
impl Drop for LockLossPauseGuard {
    fn drop(&mut self) {
        install_lock_loss_pause(None);
    }
}

#[cfg(all(test, unix))]
fn install_lock_loss_pause(pause: Option<LockLossPause>) {
    *lock_std(&LOCK_LOSS_PAUSE) = pause;
}

#[cfg(test)]
async fn pause_after_lock_loss(identity: &SocketIdentity, loss: LockLoss) {
    let rendezvous = lock_std(&LOCK_LOSS_PAUSE)
        .as_ref()
        .filter(|pause| pause.identity == *identity && pause.loss == loss)
        .map(|pause| (Arc::clone(&pause.arrived), Arc::clone(&pause.resume)));
    if let Some((arrived, resume)) = rendezvous {
        arrived.notify_one();
        resume.notified().await;
    }
}

/// Serve while owning the socket, retrying after either kind of lock loss.
/// Both lock-loss variants demote before retry; cancellation and
/// unrecoverable transport errors end the task.
pub(crate) async fn hook_owner_task(
    mut listener: HookListener,
    location: HookLocation,
    role: Arc<HookRole>,
    server: Arc<McplsServer>,
    sweeper: Arc<Sweeper>,
    op_deadline: Duration,
    mut cancel: watch::Receiver<bool>,
) {
    loop {
        let handler = build_handler(
            Arc::clone(&server),
            Arc::clone(&sweeper),
            location.clone(),
            Arc::clone(&role),
            cancel.clone(),
        );
        let exit = listener.serve(handler, op_deadline, cancel.clone()).await;
        match exit {
            ServeExit::Cancelled | ServeExit::TransportUnrecoverable => return,
            ServeExit::LockLost(loss) => {
                role.demote_to_passive(location.identity.clone());
                #[cfg(test)]
                {
                    pause_after_lock_loss(&location.identity, loss).await;
                }
                #[cfg(not(test))]
                {
                    let _ = loss;
                }
            }
        }
        let Some(reacquired) = acquire_when_free(&location.identity, &mut cancel).await else {
            return;
        };
        role.promote_to_owner();
        listener = reacquired;
    }
}

/// Retry the ownership lock every five seconds while this process is
/// passive, and start serving the moment it wins.
///
/// Without this, an owner exiting leaves every other instance in the
/// project permanently passive, forwarding to a socket nobody is listening
/// on. Retrying the lock rather than probing the socket is what keeps the
/// arbitration exclusive: two processes that both saw a connection refused
/// would both bind.
pub(crate) async fn hook_takeover_task(
    location: HookLocation,
    role: Arc<HookRole>,
    server: Arc<McplsServer>,
    sweeper: Arc<Sweeper>,
    op_deadline: Duration,
    mut cancel: watch::Receiver<bool>,
) {
    let Some(listener) = acquire_when_free(&location.identity, &mut cancel).await else {
        return;
    };
    role.promote_to_owner();
    hook_owner_task(
        listener,
        location,
        role,
        server,
        sweeper,
        op_deadline,
        cancel,
    )
    .await;
}

/// Try the ownership lock every [`LOCK_RETRY_INTERVAL`] until it is won, or
/// `None` if `cancel` fires first.
///
/// `tokio::time::interval` fires immediately on its first `tick`, so the
/// first attempt happens at once and every five seconds after; that is what
/// makes the taking-over case fast when the owner has already gone.
async fn acquire_when_free(
    identity: &SocketIdentity,
    cancel: &mut watch::Receiver<bool>,
) -> Option<HookListener> {
    let mut ticker = tokio::time::interval(LOCK_RETRY_INTERVAL);
    loop {
        tokio::select! {
            _ = cancel.changed() => return None,
            _ = ticker.tick() => {
                match HookListener::acquire(identity).await {
                    Ok(Some(listener)) => return Some(listener),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, "could not retry the hook ownership lock");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    #[cfg(unix)]
    use rmcp::ServiceExt as _;
    #[cfg(unix)]
    use serde_json::json;
    use tempfile::TempDir;
    #[cfg(unix)]
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, BufStream};
    use tokio::sync::Mutex;

    use super::*;
    #[cfg(unix)]
    use crate::bridge::apply::Applier;
    use crate::bridge::{
        DiagnosticsDelivery, FloorTable, NotificationCache, ResourceLimits, ResourceSubscriptions,
        ServerSettle, Translator, TranslatorHarness,
    };
    #[cfg(unix)]
    use crate::bridge::{
        FakeServer, read_framed_reply, translator_with_capabilities, write_response,
    };
    #[cfg(unix)]
    use crate::config::ApplyConfig;
    use crate::config::{DiagnosticsConfig, ServerId};
    use crate::hooks::{ChangeEvent, HookListener, PathFilter, Request, Response, SocketIdentity};
    use crate::mcp::McplsServer;

    /// An in-process owner: a real listener on a temporary socket, a real
    /// `McplsServer`, and a real `Sweeper`, wired by the same
    /// `build_handler` `serve_with` uses.
    struct HookHarness {
        dir: TempDir,
        root: PathBuf,
        identity: SocketIdentity,
        role: Arc<HookRole>,
        server: Arc<McplsServer>,
        sweeper: Arc<Sweeper>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        delivery: Arc<Mutex<DiagnosticsDelivery>>,
        translator_harness: Option<TranslatorHarness>,
        cancel: tokio::sync::watch::Sender<bool>,
    }

    impl HookHarness {
        /// An owner with an empty baseline adopted, so a flush answers a
        /// real report rather than `starting_up()`.
        async fn owner() -> Self {
            let harness = Self::owner_without_baseline().await;
            harness.delivery.lock().await.set_baseline(HashMap::new());
            harness
        }

        /// An owner whose language servers have not settled yet, so its
        /// delivery core has no baseline to seed a session's record from.
        async fn owner_without_baseline() -> Self {
            Self::owner_without_baseline_with(DiagnosticsConfig::default()).await
        }

        /// An owner with the supplied diagnostics timing and delivery config,
        /// before its baseline is adopted.
        async fn owner_without_baseline_with(diagnostics: DiagnosticsConfig) -> Self {
            let (dir, identity) = temp_identity();
            let translator = Arc::new(Translator::new());
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            let delivery = Arc::new(Mutex::new(DiagnosticsDelivery::new(diagnostics)));
            let role = Arc::new(HookRole::owner());
            Self::start_owner(
                dir,
                identity,
                role,
                translator,
                None,
                notification_cache,
                delivery,
                usize::MAX,
                diagnostics,
            )
            .await
        }

        /// An owner with a fake LSP server and the same document limit
        /// on its tracker and background sweeper.
        async fn owner_with_document_limit(max_documents: usize) -> Self {
            let (dir, identity) = temp_identity();
            let translator_harness = TranslatorHarness::with_diagnostics_server_and_limits(
                "rust",
                ResourceLimits {
                    max_documents,
                    ..ResourceLimits::default()
                },
            )
            .await;
            let translator = Arc::clone(&translator_harness.translator);
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            let delivery = Arc::new(Mutex::new(DiagnosticsDelivery::new(
                DiagnosticsConfig::default(),
            )));
            let role = Arc::new(HookRole::owner());
            let harness = Self::start_owner(
                dir,
                identity,
                role,
                translator,
                Some(translator_harness),
                notification_cache,
                delivery,
                max_documents,
                DiagnosticsConfig::default(),
            )
            .await;
            harness.delivery.lock().await.set_baseline(HashMap::new());
            harness
        }

        #[allow(clippy::too_many_arguments)]
        async fn start_owner(
            dir: TempDir,
            identity: SocketIdentity,
            role: Arc<HookRole>,
            translator: Arc<Translator>,
            translator_harness: Option<TranslatorHarness>,
            notification_cache: Arc<Mutex<NotificationCache>>,
            delivery: Arc<Mutex<DiagnosticsDelivery>>,
            max_documents: usize,
            diagnostics: DiagnosticsConfig,
        ) -> Self {
            let root = translator_harness.as_ref().map_or_else(
                || dir.path().to_path_buf(),
                |harness| harness.root().to_path_buf(),
            );
            let context = Arc::new(test_context(
                &root,
                Arc::clone(&translator),
                Arc::clone(&notification_cache),
                Arc::clone(&delivery),
                diagnostics,
                Arc::clone(&role),
            ));
            let server = Arc::new(McplsServer::from_context(context));
            let sweeper = Arc::new(test_sweeper_with_limit(
                &root,
                Arc::clone(&translator),
                max_documents,
            ));
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            tokio::spawn(Arc::clone(&sweeper).run(cancel_rx.clone()));

            let listener = HookListener::acquire(&identity)
                .await
                .expect("acquire")
                .expect("the harness owns its own temporary socket");
            tokio::spawn(hook_owner_task(
                listener,
                HookLocation {
                    identity: identity.clone(),
                    root: root.clone(),
                },
                Arc::clone(&role),
                Arc::clone(&server),
                Arc::clone(&sweeper),
                Duration::from_millis(1500),
                cancel_rx,
            ));

            Self {
                dir,
                root,
                identity,
                role,
                server,
                sweeper,
                notification_cache,
                delivery,
                translator_harness,
                cancel: cancel_tx,
            }
        }

        /// The same, with one error already in the notification cache for
        /// `broken.rs`, so the first flush has something to report.
        async fn owner_with_one_error() -> Self {
            Self::owner_with_diagnostic(DiagnosticsConfig::default(), "broken").await
        }

        /// An owner with one named diagnostic and an adopted empty baseline.
        async fn owner_with_diagnostic(diagnostics: DiagnosticsConfig, message: &str) -> Self {
            let harness = Self::owner_without_baseline_with(diagnostics).await;
            harness.delivery.lock().await.set_baseline(HashMap::new());
            harness.notification_cache.lock().await.store_diagnostics(
                &ServerId::from("rust"),
                &broken_uri(),
                Some(1),
                vec![diagnostic(message)],
            );
            harness
        }

        #[cfg(unix)]
        async fn owner_with_write_diagnostic(
            diagnostics: DiagnosticsConfig,
            message: &str,
        ) -> (Self, FakeServer) {
            let (dir, identity) = temp_identity();
            let role = Arc::new(HookRole::owner());
            let (translator, fake) = translator_with_capabilities(
                &dir,
                &ServerId::from("rust"),
                lsp_types::ServerCapabilities {
                    rename_provider: Some(lsp_types::OneOf::Left(true)),
                    ..Default::default()
                },
            );
            let translator = Arc::new(translator.with_applier(Arc::new(Applier::new(
                vec![dir.path().to_path_buf()],
                ApplyConfig {
                    rename: true,
                    ..ApplyConfig::default()
                },
            ))));
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            let delivery = Arc::new(Mutex::new(DiagnosticsDelivery::new(diagnostics)));
            let harness = Self::start_owner(
                dir,
                identity,
                role,
                translator,
                None,
                Arc::clone(&notification_cache),
                Arc::clone(&delivery),
                usize::MAX,
                diagnostics,
            )
            .await;
            delivery.lock().await.set_baseline(HashMap::new());
            notification_cache.lock().await.store_diagnostics(
                &ServerId::from("rust"),
                &broken_uri(),
                Some(1),
                vec![diagnostic(message)],
            );
            (harness, fake)
        }

        /// An absolute path under this harness's temporary workspace.
        fn fixture(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        /// Methods received by the fake LSP server, represented as the
        /// method-only shape the assertions need.
        fn lsp_notifications(&self) -> Vec<serde_json::Value> {
            self.translator_harness
                .as_ref()
                .map(|harness| {
                    harness
                        .notifications_for("rust")
                        .into_iter()
                        .map(|method| serde_json::json!({ "method": method }))
                        .collect()
                })
                .unwrap_or_default()
        }

        /// Call the diagnostics MCP tool over an in-memory RMCP connection.
        async fn call_file_tool(&self, path: &std::path::Path) -> rmcp::model::CallToolResult {
            use rmcp::ServiceExt;
            use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufStream};

            async fn request(
                wire: &mut BufStream<tokio::io::DuplexStream>,
                request: serde_json::Value,
            ) -> serde_json::Value {
                let id = request["id"].clone();
                wire.write_all(format!("{request}\n").as_bytes())
                    .await
                    .expect("write MCP request");
                wire.flush().await.expect("flush MCP request");

                let mut line = String::new();
                loop {
                    line.clear();
                    assert_ne!(
                        wire.read_line(&mut line).await.expect("read MCP response"),
                        0,
                        "MCP server closed before answering request {id}"
                    );
                    let response: serde_json::Value =
                        serde_json::from_str(&line).expect("MCP server sends valid JSON");
                    if response["id"] == id {
                        return response;
                    }
                }
            }

            let (server_transport, client_transport) = tokio::io::duplex(65_536);
            let server = Arc::clone(&self.server);
            let server_task = tokio::spawn(async move {
                let service = server
                    .serve(server_transport)
                    .await
                    .expect("MCP server starts");
                service.waiting().await.expect("MCP server stays alive");
            });
            let mut wire = BufStream::new(client_transport);
            let initialized = request(
                &mut wire,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {"name": "hook-sweep-test", "version": "1"}
                    }
                }),
            )
            .await;
            assert!(initialized["result"].is_object(), "{initialized}");
            wire.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
                .await
                .expect("write MCP initialized notification");
            wire.flush()
                .await
                .expect("flush MCP initialized notification");
            let response = request(
                &mut wire,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "get_diagnostics",
                        "arguments": {"file_path": path.display().to_string()}
                    }
                }),
            )
            .await;
            server_task.abort();
            serde_json::from_value(response["result"].clone())
                .unwrap_or_else(|error| panic!("MCP file tool failed: {response}: {error}"))
        }

        /// Send one request over the real socket and return the answer.
        async fn send(&self, request: Request) -> Response {
            crate::hooks::send(&self.identity, &request, Duration::from_secs(5))
                .await
                .expect("the owner answers")
        }

        /// Send one flush over the real socket and acknowledge its answer
        /// on the same connection, the way `mcpls hook` does.
        async fn flush_acknowledged(&self, session: &str) -> Response {
            crate::hooks::send_and_acknowledge(
                &self.identity,
                &[Request::Flush {
                    session: session.to_string(),
                }],
                Duration::from_secs(5),
            )
            .await
            .expect("the owner answers")
            .remove(0)
        }

        /// How many sweeps the owner's sweeper has run.
        fn sweeps_run(&self) -> usize {
            self.sweeper.sweeps_run()
        }

        /// A second `McplsServer` in the passive role, pointed at this
        /// harness's socket. Its own delivery record is empty and its own
        /// cache holds nothing, so anything it reports came from the owner.
        async fn passive_instance(&self) -> PassiveInstance {
            self.passive_instance_with(DiagnosticsConfig::default())
                .await
        }

        /// The same, with `footer = true` in its diagnostics config, which
        /// is the only configuration under which a footer could fire at all.
        async fn passive_instance_with_footer_enabled(&self) -> PassiveInstance {
            self.passive_instance_with(DiagnosticsConfig {
                footer: true,
                footer_grace_ms: 0,
                footer_quiet_ms: 0,
                footer_wait_ms: 0,
                ..DiagnosticsConfig::default()
            })
            .await
        }

        /// A passive instance over `diagnostics`, with its own baseline
        /// adopted so a fall back to its own record is a working flush that
        /// simply has nothing to report, rather than a startup answer.
        async fn passive_instance_with(&self, diagnostics: DiagnosticsConfig) -> PassiveInstance {
            Self::passive_pointing_at(self.dir.path(), self.identity.clone(), diagnostics, false)
                .await
        }

        /// A passive instance pointing at `identity`, holding an error of
        /// its own, so a flush that read the local record would visibly
        /// report something.
        async fn passive_instance_with_its_own_error(identity: SocketIdentity) -> PassiveInstance {
            let dir = tempfile::tempdir().expect("a temp dir");
            Self::passive_pointing_at(dir.path(), identity, DiagnosticsConfig::default(), true)
                .await
        }

        async fn passive_pointing_at(
            root: &std::path::Path,
            identity: SocketIdentity,
            diagnostics: DiagnosticsConfig,
            with_own_error: bool,
        ) -> PassiveInstance {
            let delivery = Arc::new(Mutex::new(DiagnosticsDelivery::new(diagnostics)));
            let cache = Arc::new(Mutex::new(NotificationCache::new()));
            let context = Arc::new(test_context(
                root,
                Arc::new(Translator::new()),
                Arc::clone(&cache),
                Arc::clone(&delivery),
                diagnostics,
                Arc::new(HookRole::passive(identity)),
            ));
            if with_own_error {
                cache.lock().await.store_diagnostics(
                    &ServerId::from("rust"),
                    &broken_uri(),
                    Some(1),
                    vec![error_diagnostic()],
                );
            }
            delivery.lock().await.set_baseline(HashMap::new());
            PassiveInstance {
                server: McplsServer::from_context(context),
            }
        }
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
    /// The integration tests in `tests/hooks_socket.rs` have a function of
    /// the same name, which no library test can call, so this module
    /// carries its own copy.
    fn temp_identity() -> (TempDir, SocketIdentity) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let hash = format!("{:016x}", rand_suffix());
        #[cfg(windows)]
        let socket = PathBuf::from(format!(r"\\.\pipe\mcpls-service-{hash}"));
        #[cfg(not(windows))]
        let socket = dir.path().join(format!("{hash}.sock"));
        let identity = SocketIdentity {
            socket,
            lock: dir.path().join(format!("{hash}.lock")),
            hash,
        };
        (dir, identity)
    }

    /// A per-test suffix, so two tests running in parallel never collide on
    /// a Windows pipe name, which is process-global rather than
    /// directory-scoped.
    fn rand_suffix() -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::thread::current().id().hash(&mut hasher);
        std::time::SystemTime::now().hash(&mut hasher);
        hasher.finish()
    }

    /// A `BridgeContext` rooted at `root`, carrying `role`.
    fn test_context(
        root: &std::path::Path,
        translator: Arc<Translator>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        delivery: Arc<Mutex<DiagnosticsDelivery>>,
        diagnostics: DiagnosticsConfig,
        role: Arc<HookRole>,
    ) -> crate::mcp::BridgeContext {
        let mut context = crate::mcp::BridgeContext::new(
            translator,
            notification_cache,
            Arc::from(vec![root.to_path_buf()]),
            Arc::new(ResourceSubscriptions::new()),
            false,
            delivery,
            Arc::new(FloorTable::new(&diagnostics, &[])),
            diagnostics,
            Arc::new(ServerSettle::new(
                Duration::from_secs(1),
                Duration::from_secs(300),
            )),
        );
        context.hooks = role;
        context
    }

    /// A `Sweeper` over `root`, admitting `.rs` files.
    fn test_sweeper(root: &std::path::Path, translator: Arc<Translator>) -> Sweeper {
        test_sweeper_with_limit(root, translator, usize::MAX)
    }

    fn test_sweeper_with_limit(
        root: &std::path::Path,
        translator: Arc<Translator>,
        max_documents: usize,
    ) -> Sweeper {
        Sweeper::new(
            translator,
            PathFilter::new(
                Arc::from(vec![root.to_path_buf()]),
                Arc::new(HashMap::from([("rs".to_string(), "rust".to_string())])),
                None,
            ),
            Duration::from_millis(500),
            max_documents,
        )
    }

    /// The `Arc<McplsServer>` and `Arc<Sweeper>` a takeover would start
    /// serving with: the same pieces `owner()` builds, over `dir`, with
    /// `role` already installed on the server's context, and no listener
    /// acquired.
    fn takeover_candidate(dir: &TempDir, role: Arc<HookRole>) -> (Arc<McplsServer>, Arc<Sweeper>) {
        let translator = Arc::new(Translator::new());
        let context = Arc::new(test_context(
            dir.path(),
            Arc::clone(&translator),
            Arc::new(Mutex::new(NotificationCache::new())),
            Arc::new(Mutex::new(DiagnosticsDelivery::new(
                DiagnosticsConfig::default(),
            ))),
            DiagnosticsConfig::default(),
            role,
        ));
        (
            Arc::new(McplsServer::from_context(context)),
            Arc::new(test_sweeper(dir.path(), translator)),
        )
    }

    #[cfg(unix)]
    async fn server_with_diagnostic(
        root: &std::path::Path,
        role: Arc<HookRole>,
        diagnostics: DiagnosticsConfig,
        message: &str,
    ) -> (Arc<McplsServer>, Arc<Sweeper>) {
        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        notification_cache.lock().await.store_diagnostics(
            &ServerId::from("rust"),
            &broken_uri(),
            Some(1),
            vec![diagnostic(message)],
        );
        let delivery = Arc::new(Mutex::new(DiagnosticsDelivery::new(diagnostics)));
        delivery.lock().await.set_baseline(HashMap::new());
        let context = Arc::new(test_context(
            root,
            translator.clone(),
            notification_cache,
            delivery,
            diagnostics,
            role,
        ));
        (
            Arc::new(McplsServer::from_context(context)),
            Arc::new(test_sweeper(root, translator)),
        )
    }

    /// The URI the harness's one cached error belongs to.
    fn broken_uri() -> lsp_types::Uri {
        if cfg!(windows) {
            "file:///C:/workspace/broken.rs"
                .parse()
                .expect("a valid uri")
        } else {
            "file:///workspace/broken.rs".parse().expect("a valid uri")
        }
    }

    /// One error, at the top of whatever file it is stored against.
    fn error_diagnostic() -> lsp_types::Diagnostic {
        diagnostic("broken")
    }

    /// One named error, at the top of whatever file it is stored against.
    fn diagnostic(message: &str) -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: message.to_string(),
            ..lsp_types::Diagnostic::default()
        }
    }

    async fn wait_for_sweep(completion: &mut tokio::sync::watch::Receiver<usize>) {
        tokio::time::timeout(Duration::from_secs(5), completion.changed())
            .await
            .expect("the queued paths to be swept")
            .expect("the sweeper stays alive");
    }

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
    async fn i1_t7_sweep_reserves_tool_capacity() {
        let harness = HookHarness::owner_with_document_limit(3).await;
        let mut completion = harness.sweeper.subscribe_completions();
        let paths: Vec<_> = (0..3)
            .map(|index| {
                let path = harness.fixture(&format!("background-{index}.rs"));
                std::fs::write(&path, "fn background() {}").expect("write fixture");
                path
            })
            .collect();

        for path in &paths {
            assert_eq!(
                harness
                    .send(Request::Changed {
                        session: "s1".to_string(),
                        paths: vec![path.clone()],
                        event: ChangeEvent::Change,
                    })
                    .await,
                Response::Changed { queued: 1 }
            );
        }
        wait_for_sweep(&mut completion).await;

        let Response::Flush { context, .. } = harness.flush_acknowledged("s1").await else {
            panic!("the finite sweep answers with its status");
        };

        let open_count = harness
            .translator_harness
            .as_ref()
            .expect("the finite harness has a recording server")
            .translator
            .open_document_paths()
            .len();
        let lsp_notifications = harness.lsp_notifications();

        let unrelated = harness.fixture("unrelated.rs");
        std::fs::write(&unrelated, "fn unrelated() {}").expect("write fixture");
        let unrelated_tool_response = harness.call_file_tool(&unrelated).await;
        assert!(!unrelated_tool_response.is_error.unwrap_or(false));
        assert!(open_count < 3);
        assert_eq!(
            harness
                .translator_harness
                .as_ref()
                .expect("the finite harness has a recording server")
                .translator
                .open_document_paths()
                .len(),
            open_count + 1
        );

        let context = context.expect("the finite sweep reports its skipped coverage");
        assert!(context.contains("background sweep headroom"), "{context}");
        assert!(context.contains("coverage was skipped"), "{context}");

        assert!(
            lsp_notifications
                .iter()
                .any(|n| n["method"] == "textDocument/didOpen")
        );
        assert!(
            lsp_notifications
                .iter()
                .any(|n| n["method"] == "textDocument/didSave")
        );

        let latest = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        assert!(
            matches!(&latest, Response::Flush { context: Some(text), .. } if text.contains("background sweep headroom")),
            "the latest sweep status remains visible after ACK: {latest:?}"
        );
    }

    #[tokio::test]
    async fn i1_t7_zero_limit_sweeps_unbounded() {
        let harness = HookHarness::owner_with_document_limit(0).await;
        let mut completion = harness.sweeper.subscribe_completions();
        let preopened = harness.fixture("preopened.rs");
        std::fs::write(&preopened, "fn preopened() {}").expect("write fixture");
        let preopened_tool_response = harness.call_file_tool(&preopened).await;
        assert!(!preopened_tool_response.is_error.unwrap_or(false));
        let notification_offset = harness.lsp_notifications().len();

        std::fs::write(&preopened, "fn preopened_changed() {}").expect("rewrite fixture");
        let paths: Vec<_> = (0..3)
            .map(|index| {
                let path = harness.fixture(&format!("unbounded-{index}.rs"));
                std::fs::write(&path, "fn unbounded() {}").expect("write fixture");
                path
            })
            .collect();
        let mut changed_paths = vec![preopened.clone()];
        changed_paths.extend(paths);
        for path in &changed_paths {
            assert_eq!(
                harness
                    .send(Request::Changed {
                        session: "s1".to_string(),
                        paths: vec![path.clone()],
                        event: ChangeEvent::Change,
                    })
                    .await,
                Response::Changed { queued: 1 }
            );
        }
        wait_for_sweep(&mut completion).await;

        let flush = harness.flush_acknowledged("s1").await;
        let open_count = harness
            .translator_harness
            .as_ref()
            .expect("the zero-limit harness has a recording server")
            .translator
            .open_document_paths()
            .len();
        assert!(open_count >= 4);
        assert!(
            matches!(&flush, Response::Flush { context: None, .. })
                || matches!(&flush, Response::Flush { context: Some(text), .. } if !text.contains("headroom")),
            "zero-limit sweeps must not report finite headroom failure: {flush:?}"
        );

        let lsp_notifications = harness.lsp_notifications();
        let sweep_notifications = &lsp_notifications[notification_offset..];
        assert!(
            sweep_notifications
                .iter()
                .any(|n| n["method"] == "textDocument/didOpen")
        );
        assert!(
            sweep_notifications
                .iter()
                .any(|n| n["method"] == "textDocument/didSave")
        );
        assert!(
            sweep_notifications
                .iter()
                .any(|n| n["method"] == "textDocument/didChange")
        );
    }

    #[tokio::test]
    async fn test_a_flush_op_and_the_tool_share_one_record() {
        let harness = HookHarness::owner_with_one_error().await;
        let Response::Flush {
            context: Some(text),
            ..
        } = harness.flush_acknowledged("s1").await
        else {
            panic!("the first flush reports the error");
        };
        assert!(text.contains("broken.rs"));

        let second = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        assert_eq!(
            second,
            Response::Flush {
                context: None,
                token: None
            },
            "one report per problem, whichever door asked for it"
        );
    }

    /// `mcpls hook doctor` reads `hooks_seen` to tell a server nothing has
    /// ever sent a hook to apart from one a host is really driving, so an
    /// acknowledgement counted alongside its own flush would make every
    /// install look twice as busy as it is.
    #[tokio::test]
    async fn test_an_acknowledgement_is_not_counted_as_a_hook_request() {
        let harness = HookHarness::owner_with_one_error().await;
        let answer = harness.flush_acknowledged("s1").await;
        assert!(
            matches!(answer, Response::Flush { token: Some(_), .. }),
            "the flush has to carry a token, or no acknowledgement follows it \
             and this proves nothing: {answer:?}"
        );

        let Response::Status { hooks_seen, .. } = harness.send(Request::Status).await else {
            panic!("expected a status response");
        };
        assert_eq!(
            hooks_seen, 1,
            "one hook fired, and the flush and the acknowledgement it sent are \
             its two halves"
        );
    }

    #[tokio::test]
    async fn test_an_unacknowledged_flush_is_offered_again() {
        let harness = HookHarness::owner_with_one_error().await;
        let first = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        assert!(
            matches!(
                first,
                Response::Flush {
                    context: Some(_),
                    token: Some(_)
                }
            ),
            "a report with content carries the token its acknowledgement names: {first:?}"
        );

        let second = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        assert!(
            matches!(
                second,
                Response::Flush {
                    context: Some(_),
                    token: Some(_)
                }
            ),
            "the hook that read the first answer may have died before printing \
             it, and nothing said otherwise, so the report is offered again \
             rather than lost: {second:?}"
        );
    }

    #[tokio::test]
    async fn test_a_stale_acknowledgement_commits_nothing() {
        let harness = HookHarness::owner_with_one_error().await;
        let Response::Flush {
            token: Some(stale), ..
        } = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await
        else {
            panic!("the first flush carries a token");
        };
        let Response::Flush {
            token: Some(current),
            ..
        } = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await
        else {
            panic!("the second flush carries a token");
        };
        assert_ne!(stale, current);

        let answer = harness
            .send(Request::Ack {
                session: "s1".to_string(),
                token: stale,
            })
            .await;
        assert_eq!(answer, Response::Ack);

        let third = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        assert!(
            matches!(
                third,
                Response::Flush {
                    context: Some(_),
                    ..
                }
            ),
            "the acknowledged token named a report a later flush replaced, so \
             the record did not move and the report is still owed: {third:?}"
        );
    }

    #[tokio::test]
    async fn test_two_sessions_have_independent_records() {
        let harness = HookHarness::owner_with_one_error().await;
        let _ = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        let other = harness
            .send(Request::Flush {
                session: "s2".to_string(),
            })
            .await;

        assert!(
            matches!(
                other,
                Response::Flush {
                    context: Some(_),
                    ..
                }
            ),
            "two agents in one directory share warm servers and not delivery state"
        );
    }

    #[tokio::test]
    async fn test_ending_a_session_drops_its_record() {
        let harness = HookHarness::owner_with_one_error().await;
        let _ = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        let _ = harness
            .send(Request::EndSession {
                session: "s1".to_string(),
            })
            .await;
        let again = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;

        assert!(matches!(
            again,
            Response::Flush {
                context: Some(_),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn test_a_shortfall_reaches_the_flush_response() {
        let harness = HookHarness::owner().await;
        harness
            .sweeper
            .set_shortfall_for_test(
                "7 file(s) not checked: background sweep headroom of 2 was exhausted; coverage was skipped to reserve one document slot (document limit 3)",
            );

        let Response::Flush {
            context: Some(text),
            ..
        } = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await
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

        let again = passive.call_flush_tool().await;
        assert!(
            !again.contains("broken.rs"),
            "the passive acknowledged the first answer once it had it in hand, \
             so the owner does not offer the report again: {again}"
        );
    }

    /// A forward that fails must not fall back to this process's own
    /// record.
    ///
    /// The owner's record is the session's record. A diff against this
    /// process's own would be against a baseline the session never
    /// agreed to, and would leave the two disagreeing about what the
    /// session has been shown for the rest of its life. The failed
    /// forward costs nothing on the owner's side: its staged report is
    /// unacknowledged, and the next flush offers it again.
    #[tokio::test]
    async fn test_a_passive_instance_whose_owner_is_gone_says_so_rather_than_reading_itself() {
        let (_dir, unserved) = temp_identity();
        let passive = HookHarness::passive_instance_with_its_own_error(unserved).await;

        let report: serde_json::Value =
            serde_json::from_str(&passive.call_flush_tool().await).expect("a json object");

        assert!(
            report["changed"]
                .as_array()
                .expect("the documented object shape, not a bare string")
                .is_empty(),
            "answering from the local record desynchronizes the two for good: \
             {report}"
        );
        assert!(
            report["note"]
                .as_str()
                .is_some_and(|note| note.contains("could not be reached")),
            "an empty diff with no note reads as a clean workspace: {report}"
        );
    }

    /// The forwarded flush waits at least as long as the owner's own op
    /// deadline.
    ///
    /// The owner answers at its deadline rather than before it. A client
    /// bound tighter gives up on a report it asked for and is offered it
    /// again next time, which costs the agent a turn for nothing.
    #[tokio::test]
    async fn test_a_forwarded_flush_outwaits_a_slow_owner() {
        let (dir, identity) = temp_identity();
        let listener = HookListener::acquire(&identity)
            .await
            .expect("acquire")
            .expect("owner");
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(listener.serve(
            slow_flush_handler(Duration::from_millis(300)),
            Duration::from_millis(1500),
            cancel_rx,
        ));
        let passive = HookHarness::passive_instance_with_its_own_error(identity).await;

        let report: serde_json::Value =
            serde_json::from_str(&passive.call_flush_tool().await).expect("a json object");

        assert_eq!(
            report["note"].as_str(),
            Some("the owner's own report"),
            "a client bound tighter than the owner's op deadline gives up on the \
             answer it asked for, and the owner has already consumed it: {report}"
        );
        drop(dir);
    }

    /// An owner that takes longer to answer than a tight client bound but
    /// less than its own op deadline.
    fn slow_flush_handler(
        delay: Duration,
    ) -> impl Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static {
        move |_request| {
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Response::Flush {
                    context: Some("the owner's own report".to_string()),
                    token: None,
                }
            })
        }
    }

    /// Both halves of the spec's sentence about a passive instance: the
    /// footer stays silent, and the apply targets still reach the owner.
    ///
    /// Asserted against the two methods the write tools call rather than by
    /// driving a real rename, because a rename needs a live language server
    /// and what is under test is which branch the passive role takes.
    #[tokio::test]
    async fn test_a_passive_instance_runs_no_footer_but_still_reports_its_writes() {
        let harness = HookHarness::owner_with_one_error().await;
        let passive = harness.passive_instance_with_footer_enabled().await;
        let written = harness.fixture("written.rs");
        std::fs::write(&written, "fn a() {}").expect("write");

        assert!(
            passive.server.footer_if_written(true, 0).await.is_none(),
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

    /// Wait until the ownership lock names a file again, which only
    /// happens once the listener has noticed it vanished, stopped serving,
    /// and won it back.
    ///
    /// The listener re-`stat`s its lock on its own interval, so a test that
    /// asserted straight after removing the file would usually assert
    /// before the code it is about had run at all. Bounded, so a
    /// reacquisition that never comes fails this test rather than hanging
    /// the run.
    #[cfg(unix)]
    async fn wait_for_lock_to_return(identity: &SocketIdentity) {
        for _ in 0..200 {
            if identity.lock.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the owner never won {} back", identity.lock.display());
    }

    /// The listener re-`stat`s its lock on its own interval, so a test that
    /// removes the file has to wait for that tick rather than assume it.
    /// Bounded, and every arm ends in an assertion naming what went wrong.
    async fn status_from_owner(identity: &SocketIdentity) -> Response {
        for _ in 0..100 {
            if let Ok(response) =
                crate::hooks::send(identity, &Request::Status, Duration::from_millis(200)).await
            {
                return response;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("nobody answered a status request on {:?}", identity.socket);
    }

    #[cfg(unix)]
    async fn mcp_request(
        wire: &mut BufStream<tokio::io::DuplexStream>,
        request: serde_json::Value,
    ) -> serde_json::Value {
        wire.write_all(format!("{request}\n").as_bytes())
            .await
            .expect("write MCP request");
        wire.flush().await.expect("flush MCP request");
        loop {
            let mut line = String::new();
            assert_ne!(
                wire.read_line(&mut line).await.expect("read MCP response"),
                0
            );
            let response: serde_json::Value =
                serde_json::from_str(&line).expect("MCP response is JSON");
            if response["id"] == request["id"] {
                return response;
            }
        }
    }

    #[cfg(unix)]
    async fn apply_rename_over_mcp(
        server: Arc<McplsServer>,
        fake: &mut FakeServer,
        written: PathBuf,
    ) -> serde_json::Value {
        let tool_result = apply_rename_over_mcp_result(server, fake, written).await;
        assert!(
            tool_result.get("new_diagnostics").is_none(),
            "a passive applied write must not serialize a local footer: {tool_result}"
        );
        tool_result
    }

    #[cfg(unix)]
    async fn apply_rename_over_mcp_result(
        server: Arc<McplsServer>,
        fake: &mut FakeServer,
        written: PathBuf,
    ) -> serde_json::Value {
        std::fs::write(&written, "fn old() {}\n").expect("write");
        let written_uri = crate::bridge::path_to_uri(&written).expect("the fixture has a URI");
        let (server_io, client_io) = tokio::io::duplex(65_536);
        let started = tokio::spawn(async move {
            server
                .serve(server_io)
                .await
                .expect("the MCP service starts")
        });
        let mut wire = BufStream::new(client_io);
        let initialized = mcp_request(
            &mut wire,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "task-8-test", "version": "1"}
                }
            }),
        )
        .await;
        assert!(initialized["result"].is_object(), "{initialized}");
        wire.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .expect("write MCP initialized notification");
        wire.flush()
            .await
            .expect("flush MCP initialized notification");
        let _running = started.await.expect("the MCP service task survives");

        let path = written.display().to_string();
        let reply = json!({
            "changes": {
                written_uri.as_str(): [{
                    "range": {
                        "start": {"line": 0, "character": 3},
                        "end": {"line": 0, "character": 6}
                    },
                    "newText": "new"
                }]
            }
        });
        let mcp_call = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "rename_symbol",
                "arguments": {
                    "file_path": path,
                    "line": 1,
                    "character": 4,
                    "new_name": "new",
                    "apply": true
                }
            }
        });
        let fake = &mut *fake;
        let (tool_response, lsp_request) =
            tokio::join!(mcp_request(&mut wire, mcp_call), async move {
                let mut lsp_wire = BufReader::new(&mut fake.write_stdout);
                let request = read_framed_reply(&mut lsp_wire).await;
                write_response(&mut fake.read_half_stdin, &request["id"], reply).await;
                request
            });
        assert_eq!(lsp_request["method"], "textDocument/rename");
        assert!(tool_response["result"].is_object(), "{tool_response}");
        let tool_text = tool_response["result"]["content"][0]["text"]
            .as_str()
            .expect("the rename response has serialized tool content");
        let tool_result: serde_json::Value =
            serde_json::from_str(tool_text).expect("the rename response is JSON");
        assert_eq!(
            std::fs::read_to_string(&written).expect("read the applied fixture"),
            "fn new() {}\n"
        );
        tool_result
    }

    /// A flush before the servers have settled must not seed the session's
    /// record, because `set_baseline` never rewrites a record that already
    /// exists and that session would then believe the workspace started
    /// clean for the rest of its life.
    #[tokio::test]
    async fn test_a_flush_before_the_baseline_reports_nothing_and_consumes_nothing() {
        let harness = HookHarness::owner_without_baseline().await;
        harness.notification_cache.lock().await.store_diagnostics(
            &ServerId::from("rust"),
            &broken_uri(),
            Some(1),
            vec![error_diagnostic()],
        );

        let early = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        assert_eq!(
            early,
            Response::Flush {
                context: None,
                token: None
            }
        );

        harness.delivery.lock().await.set_baseline(HashMap::new());
        let Response::Flush {
            context: Some(text),
            ..
        } = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await
        else {
            panic!(
                "the early flush consumed the report it was too early to send, so \
                 this session will never be told about broken.rs"
            );
        };
        assert!(text.contains("broken.rs"));
    }

    /// `mcpls hook doctor` compares this against the hook's own hash of
    /// `CLAUDE_PROJECT_DIR`, so a `Status` answer must report the exact
    /// directory the owner actually started in, not an empty or
    /// uncanonicalized stand-in.
    #[tokio::test]
    async fn test_the_status_response_reports_the_owners_canonicalized_root() {
        let (dir, identity) = temp_identity();
        let role = Arc::new(HookRole::owner());
        let (_tx, cancel) = tokio::sync::watch::channel(false);
        let (server, sweeper) = takeover_candidate(&dir, Arc::clone(&role));
        let listener = HookListener::acquire(&identity)
            .await
            .expect("acquire")
            .expect("owner");
        let canonical_root = dunce::canonicalize(dir.path()).expect("canonicalize");
        tokio::spawn(hook_owner_task(
            listener,
            HookLocation {
                identity: identity.clone(),
                root: canonical_root.clone(),
            },
            role,
            server,
            sweeper,
            Duration::from_millis(1500),
            cancel,
        ));

        let Response::Status { root, .. } = status_from_owner(&identity).await else {
            panic!("expected a status response");
        };
        assert_eq!(root, canonical_root);
    }

    /// `mcpls hook doctor` tells "a server that has never been sent a hook"
    /// apart from "one that has been serving them all along" by this
    /// count. `Status` itself, the doctor's own probe, must not inflate it,
    /// or every doctor run would make an unregistered plugin look wired up.
    #[tokio::test]
    async fn test_the_status_response_counts_hook_requests_the_owner_has_answered() {
        let (dir, identity) = temp_identity();
        let role = Arc::new(HookRole::owner());
        let (_tx, cancel) = tokio::sync::watch::channel(false);
        let (server, sweeper) = takeover_candidate(&dir, Arc::clone(&role));
        let listener = HookListener::acquire(&identity)
            .await
            .expect("acquire")
            .expect("owner");
        tokio::spawn(hook_owner_task(
            listener,
            HookLocation {
                identity: identity.clone(),
                root: dir.path().to_path_buf(),
            },
            role,
            server,
            sweeper,
            Duration::from_millis(1500),
            cancel,
        ));

        let Response::Status { hooks_seen, .. } = status_from_owner(&identity).await else {
            panic!("expected a status response");
        };
        assert_eq!(
            hooks_seen, 0,
            "several Status probes above must not themselves count as hooks"
        );

        crate::hooks::send(
            &identity,
            &Request::EndSession {
                session: "s1".to_string(),
            },
            Duration::from_secs(5),
        )
        .await
        .expect("the owner answers");

        let Response::Status { hooks_seen, .. } = status_from_owner(&identity).await else {
            panic!("expected a status response");
        };
        assert_eq!(
            hooks_seen, 1,
            "one real hook request was answered since startup"
        );
    }

    /// A lock file removed by a temporary-file cleaner is not a competitor,
    /// so the owner wins it back and goes on serving.
    ///
    /// Unix-only, like the ownership check it exercises: Windows arbitrates
    /// on the pipe itself, keeps no lock file, and never produces a
    /// `LockLoss` at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_an_owner_whose_lock_file_vanishes_reacquires_and_serves_again() {
        let (dir, identity) = temp_identity();
        let role = Arc::new(HookRole::owner());
        let (_tx, cancel) = tokio::sync::watch::channel(false);
        let (server, sweeper) = takeover_candidate(&dir, Arc::clone(&role));
        let listener = HookListener::acquire(&identity)
            .await
            .expect("acquire")
            .expect("owner");
        tokio::spawn(hook_owner_task(
            listener,
            HookLocation {
                identity: identity.clone(),
                root: dir.path().to_path_buf(),
            },
            Arc::clone(&role),
            server,
            sweeper,
            Duration::from_millis(1500),
            cancel,
        ));
        assert!(matches!(
            status_from_owner(&identity).await,
            Response::Status { owner: true, .. }
        ));
        let mut transitions = role.subscribe_transitions();
        let before = *transitions.borrow();
        let arrived = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        install_lock_loss_pause(Some(LockLossPause {
            identity: identity.clone(),
            loss: LockLoss::Missing,
            arrived: Arc::clone(&arrived),
            resume: Arc::clone(&resume),
        }));
        let _pause_guard = LockLossPauseGuard;

        let lock_loss = arrived.notified();
        std::fs::remove_file(&identity.lock).expect("stand in for a temp-file cleaner");
        tokio::time::timeout(Duration::from_secs(3), lock_loss)
            .await
            .expect("the owner must observe the missing lock");
        assert!(matches!(role.get(), Role::Passive { .. }));
        tokio::time::timeout(Duration::from_secs(1), transitions.changed())
            .await
            .expect("the passive transition must be observable before reacquisition")
            .expect("the role outlives the owner task");
        resume.notify_one();

        wait_for_lock_to_return(&identity).await;
        tokio::time::timeout(Duration::from_secs(6), transitions.changed())
            .await
            .expect("the owner must promote after reacquiring the lock")
            .expect("the role outlives the owner task");

        assert!(
            matches!(
                status_from_owner(&identity).await,
                Response::Status { owner: true, .. }
            ),
            "standing down for good would end hook support for this project for \
             the rest of the process's life over a file anything may delete"
        );
        assert_eq!(*transitions.borrow(), before + 2);
        assert_eq!(role.get(), Role::Owner);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn i1_t8_missing_lock_loser_forwards() {
        let old_diagnostics = DiagnosticsConfig {
            footer: true,
            footer_grace_ms: 0,
            footer_quiet_ms: 0,
            footer_wait_ms: 0,
            ..DiagnosticsConfig::default()
        };
        let (old, mut fake) =
            HookHarness::owner_with_write_diagnostic(old_diagnostics, "former-owner-diagnostic")
                .await;
        let old_role = Arc::clone(&old.role);
        let arrived = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        install_lock_loss_pause(Some(LockLossPause {
            identity: old.identity.clone(),
            loss: LockLoss::Missing,
            arrived: Arc::clone(&arrived),
            resume: Arc::clone(&resume),
        }));
        let _pause_guard = LockLossPauseGuard;

        assert!(matches!(
            status_from_owner(&old.identity).await,
            Response::Status { owner: true, .. }
        ));

        let lock_loss = arrived.notified();
        std::fs::remove_file(&old.identity.lock).expect("remove the old lock");
        tokio::time::timeout(Duration::from_secs(3), lock_loss)
            .await
            .expect("the owner must reach the missing-lock acquisition pause");

        let competitor_role = Arc::new(HookRole::owner());
        let (competitor_server, competitor_sweeper) = server_with_diagnostic(
            old.dir.path(),
            Arc::clone(&competitor_role),
            DiagnosticsConfig::default(),
            "competitor-diagnostic",
        )
        .await;
        let (competitor_cancel_tx, competitor_cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(Arc::clone(&competitor_sweeper).run(competitor_cancel_rx.clone()));
        let competitor_listener = HookListener::acquire(&old.identity)
            .await
            .expect("acquire the replacement")
            .expect("the competitor owns the replacement lock");
        tokio::spawn(competitor_listener.serve(
            build_handler(
                Arc::clone(&competitor_server),
                Arc::clone(&competitor_sweeper),
                HookLocation {
                    identity: old.identity.clone(),
                    root: old.dir.path().to_path_buf(),
                },
                Arc::clone(&competitor_role),
                competitor_cancel_rx.clone(),
            ),
            Duration::from_millis(1500),
            competitor_cancel_rx,
        ));
        assert!(matches!(
            status_from_owner(&old.identity).await,
            Response::Status { owner: true, .. }
        ));

        resume.notify_one();

        assert!(matches!(old_role.get(), Role::Passive { .. }));
        let forwarded_report = old
            .server
            .get_new_diagnostics()
            .await
            .expect("the passive MCP flush answers");
        assert!(forwarded_report.contains("competitor-diagnostic"));
        assert!(!forwarded_report.contains("former-owner-diagnostic"));

        let mut completed = competitor_sweeper.subscribe_completions();
        let requests_before = competitor_role.hooks_seen();
        let sweeps_before = competitor_sweeper.sweeps_run();
        let tool_result = apply_rename_over_mcp(
            Arc::clone(&old.server),
            &mut fake,
            old.fixture("written.rs"),
        )
        .await;
        assert!(
            tool_result.get("new_diagnostics").is_none(),
            "a passive applied write must not serialize a local footer: {tool_result}"
        );
        assert!(!tool_result.to_string().contains("former-owner-diagnostic"));
        assert_eq!(competitor_role.hooks_seen(), requests_before + 1);
        tokio::time::timeout(Duration::from_secs(5), completed.changed())
            .await
            .expect("the competitor sweeps the forwarded write")
            .expect("the competitor's sweeper remains alive");
        assert!(competitor_sweeper.sweeps_run() > sweeps_before);

        old.cancel.send(true).expect("cancel old owner");
        competitor_cancel_tx.send(true).expect("cancel competitor");
    }

    /// A write that entered its footer while owning the hook must not consume
    /// its local record after the real listener loses the lock.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn i1_t9_demotion_pending_footer_keeps_the_local_record() {
        let diagnostics = DiagnosticsConfig {
            footer: true,
            footer_grace_ms: 0,
            footer_quiet_ms: 0,
            footer_wait_ms: 0,
            ..DiagnosticsConfig::default()
        };
        let (old, mut fake) =
            HookHarness::owner_with_write_diagnostic(diagnostics, "former-owner-diagnostic").await;
        let mut transitions = old.role.subscribe_transitions();
        let (footer_entered_tx, footer_entered_rx) = tokio::sync::oneshot::channel();
        let (footer_release_tx, footer_release_rx) = tokio::sync::oneshot::channel();
        old.server
            .install_footer_pause(footer_entered_tx, footer_release_rx);

        let arrived = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        install_lock_loss_pause(Some(LockLossPause {
            identity: old.identity.clone(),
            loss: LockLoss::Missing,
            arrived: Arc::clone(&arrived),
            resume: Arc::clone(&resume),
        }));
        let _pause_guard = LockLossPauseGuard;

        assert!(matches!(
            status_from_owner(&old.identity).await,
            Response::Status { owner: true, .. }
        ));

        let expected_path = old.fixture("written.rs");
        let expected_path_text = expected_path.display().to_string();
        let apply_path = expected_path.clone();
        let apply = tokio::spawn({
            let server = Arc::clone(&old.server);
            async move { apply_rename_over_mcp_result(server, &mut fake, apply_path).await }
        });
        tokio::time::timeout(Duration::from_secs(3), footer_entered_rx)
            .await
            .expect("the serialized MCP write enters its pending footer")
            .expect("the footer pause sender stays connected");

        let lock_loss = arrived.notified();
        std::fs::remove_file(&old.identity.lock).expect("remove the old owner lock");
        tokio::time::timeout(Duration::from_secs(3), lock_loss)
            .await
            .expect("the real owner listener observes the removed lock");
        tokio::time::timeout(Duration::from_secs(1), transitions.changed())
            .await
            .expect("the role transition is observable")
            .expect("the owner task remains alive");
        assert!(matches!(old.role.get(), Role::Passive { .. }));

        let competitor_role = Arc::new(HookRole::owner());
        let (competitor_server, competitor_sweeper) = server_with_diagnostic(
            old.dir.path(),
            Arc::clone(&competitor_role),
            DiagnosticsConfig::default(),
            "competitor-diagnostic",
        )
        .await;
        let (competitor_cancel_tx, competitor_cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(Arc::clone(&competitor_sweeper).run(competitor_cancel_rx.clone()));
        let competitor_listener = HookListener::acquire(&old.identity)
            .await
            .expect("acquire the replacement hook listener")
            .expect("the competitor owns the replacement lock");
        let competitor_task = tokio::spawn(competitor_listener.serve(
            build_handler(
                Arc::clone(&competitor_server),
                Arc::clone(&competitor_sweeper),
                HookLocation {
                    identity: old.identity.clone(),
                    root: old.dir.path().to_path_buf(),
                },
                Arc::clone(&competitor_role),
                competitor_cancel_rx.clone(),
            ),
            Duration::from_millis(1500),
            competitor_cancel_rx,
        ));
        assert!(matches!(
            status_from_owner(&old.identity).await,
            Response::Status { owner: true, .. }
        ));

        footer_release_tx
            .send(())
            .expect("the pending footer remains connected");
        let tool_result = tokio::time::timeout(Duration::from_secs(5), apply)
            .await
            .expect("the MCP write completes after its footer is released")
            .expect("the MCP write task remains connected");
        assert_eq!(tool_result["applied"], true, "{tool_result}");
        assert_eq!(
            tool_result["files_written"],
            serde_json::json!([expected_path_text]),
            "{tool_result}"
        );
        assert!(
            tool_result.get("new_diagnostics").is_none(),
            "a demoted writer must not serialize its local footer: {tool_result}"
        );
        assert_eq!(
            std::fs::read_to_string(&expected_path).expect("read the applied file"),
            "fn new() {}\n"
        );

        competitor_cancel_tx
            .send(true)
            .expect("cancel the competing listener");
        tokio::time::timeout(Duration::from_secs(3), competitor_task)
            .await
            .expect("the competing listener stops")
            .expect("the competing listener task remains healthy");
        resume.notify_one();
        tokio::time::timeout(Duration::from_secs(6), transitions.changed())
            .await
            .expect("the original owner reacquires after competition ends")
            .expect("the owner task remains alive");
        assert_eq!(old.role.get(), Role::Owner);

        let local = old
            .server
            .get_new_diagnostics()
            .await
            .expect("the reacquired owner flush answers");
        assert!(
            local.contains("former-owner-diagnostic"),
            "the pending footer must leave the local record for a later owner flush: {local}"
        );
        old.cancel.send(true).expect("cancel the original owner");
    }

    /// A role change while footer lock acquisition is blocked must win over
    /// the later local flush, so a check before either async lock is unsafe.
    #[tokio::test]
    async fn i1_t9_demotion_during_footer_lock_contention_skips_consumption() {
        let diagnostics = DiagnosticsConfig {
            footer: true,
            footer_grace_ms: 0,
            footer_quiet_ms: 0,
            footer_wait_ms: 0,
            ..DiagnosticsConfig::default()
        };
        let harness =
            HookHarness::owner_with_diagnostic(diagnostics, "contention-diagnostic").await;
        let held_cache = harness.notification_cache.lock().await;
        let server = Arc::clone(&harness.server);
        let footer = tokio::spawn(async move { server.footer_for_write(0).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if harness.delivery.try_lock().is_err() {
                    tokio::task::yield_now().await;
                    if harness.delivery.try_lock().is_err() {
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("footer reaches delivery while cache remains held");

        harness.role.demote_to_passive(harness.identity.clone());
        drop(held_cache);
        assert!(
            tokio::time::timeout(Duration::from_secs(3), footer)
                .await
                .expect("the footer completes after cache contention")
                .expect("the footer task remains healthy")
                .is_none(),
            "a demoted footer must not consume after waiting for delivery and cache"
        );
        harness.cancel.send(true).expect("cancel the test owner");
    }

    /// A lock file a competitor took is a different situation: that process
    /// holds it for its whole life, so this one must stop behaving as the
    /// owner while it waits.
    ///
    /// The competitor is staged by locking a fresh file and renaming it over
    /// the lock path. Deleting the file and acquiring in two steps would
    /// race: an ownership check landing in the gap sees a missing file
    /// rather than a replaced one, and the owner then wins the empty path
    /// back before the competitor gets there. A rename is atomic, so the
    /// path always resolves and always names an inode somebody else holds.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_an_owner_whose_lock_is_taken_goes_passive_rather_than_answering() {
        use fs4::fs_std::FileExt as _;

        let (dir, identity) = temp_identity();
        let role = Arc::new(HookRole::owner());
        let mut transitions = role.subscribe_transitions();
        let (_tx, cancel) = tokio::sync::watch::channel(false);
        let (server, sweeper) = takeover_candidate(&dir, Arc::clone(&role));
        let listener = HookListener::acquire(&identity)
            .await
            .expect("acquire")
            .expect("owner");
        tokio::spawn(hook_owner_task(
            listener,
            HookLocation {
                identity: identity.clone(),
                root: dir.path().to_path_buf(),
            },
            Arc::clone(&role),
            server,
            sweeper,
            Duration::from_millis(1500),
            cancel,
        ));
        assert!(matches!(
            status_from_owner(&identity).await,
            Response::Status { owner: true, .. }
        ));

        let staged = dir.path().join("competitor.lock");
        let competitor = std::fs::File::create(&staged).expect("create");
        competitor.try_lock_exclusive().expect("lock");
        std::fs::rename(&staged, &identity.lock).expect("replace the lock atomically");

        tokio::time::timeout(Duration::from_secs(60), transitions.changed())
            .await
            .expect(
                "a process still reading Owner runs its footer and answers from \
                 its own record while every hook for the same session reaches the \
                 competitor's, which is two owners for one session",
            )
            .expect("the role outlives the owner task");
        assert!(matches!(role.get(), Role::Passive { .. }));
        drop(competitor);
    }

    /// Nothing drains the pending set once the sweep loop has stopped, so a
    /// `changed` accepted after cancellation would grow it without limit and
    /// never be swept or reported.
    #[tokio::test]
    async fn test_a_changed_op_after_cancellation_is_refused_rather_than_queued() {
        let harness = HookHarness::owner().await;
        let path = harness.fixture("late.rs");
        std::fs::write(&path, "fn a() {}").expect("write");
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let handler = build_handler(
            Arc::new(McplsServer::from_context(Arc::new(test_context(
                harness.dir.path(),
                Arc::new(Translator::new()),
                Arc::new(Mutex::new(NotificationCache::new())),
                Arc::new(Mutex::new(DiagnosticsDelivery::new(
                    DiagnosticsConfig::default(),
                ))),
                DiagnosticsConfig::default(),
                Arc::new(HookRole::owner()),
            )))),
            Arc::clone(&harness.sweeper),
            HookLocation {
                identity: harness.identity.clone(),
                root: harness.dir.path().to_path_buf(),
            },
            Arc::new(HookRole::owner()),
            cancel_rx,
        );
        cancel_tx.send(true).expect("cancel");

        let response = handler(Request::Changed {
            session: "s1".to_string(),
            paths: vec![path],
            event: ChangeEvent::Change,
        })
        .await;

        assert!(matches!(response, Response::Error { .. }));
        assert_eq!(
            harness.sweeper.pending_len(),
            0,
            "a path queued after the last sweep is never swept and never reported"
        );
    }

    /// A config over `root` with no language servers at all.
    #[cfg(feature = "transport-http")]
    fn bare_config_over(root: &std::path::Path) -> crate::config::ServerConfig {
        use crate::config::{ServerConfig, WorkspaceConfig};

        ServerConfig {
            workspace: WorkspaceConfig {
                roots: vec![dunce::canonicalize(root).expect("canonicalize the workspace")],
                ..WorkspaceConfig::default()
            },
            ..ServerConfig::default()
        }
    }

    /// A command that starts, reads nothing, and never answers
    /// `initialize`, so routing keeps reporting the server as still
    /// starting for as long as a test needs it to.
    #[cfg(all(unix, feature = "transport-http"))]
    const NEVER_ANSWERS: (&str, &[&str]) = ("/bin/sleep", &["30"]);
    #[cfg(all(windows, feature = "transport-http"))]
    const NEVER_ANSWERS: (&str, &[&str]) = ("powershell", &["-Command", "Start-Sleep 30"]);

    /// A loopback HTTP transport on an ephemeral port.
    ///
    /// The stdio transport reads real process stdin, which a test runner
    /// closes: the handshake fails at once, `serve_with` runs its shutdown,
    /// and the socket is gone before anything can connect to it. HTTP blocks
    /// until it is aborted, which is what a real session's transport does
    /// and what leaves the socket up long enough to answer.
    #[cfg(feature = "transport-http")]
    fn loopback_transport() -> crate::Transport {
        crate::Transport::Http(crate::HttpConfig::new(
            "127.0.0.1:0".parse().expect("a loopback address"),
            "/mcp",
        ))
    }

    /// The socket half of what `serve_with` wires: acquire the lock, put the
    /// role on the context, and serve from a server sharing that context.
    #[cfg(feature = "transport-http")]
    #[tokio::test]
    async fn test_serve_with_binds_the_socket_and_answers_as_its_owner() {
        let (dir, identity) = temp_identity();
        let workspace = tempfile::tempdir().expect("a temp dir");
        let served = tokio::spawn(crate::serve_with_identity(
            bare_config_over(workspace.path()),
            loopback_transport(),
            Some(identity.clone()),
        ));

        let status = status_from_owner(&identity).await;
        assert!(
            matches!(&status, Response::Status { owner: true, hash, .. } if *hash == identity.hash),
            "a running mcpls that binds nothing leaves every hook to cold-start a \
             language server on each edit, which is the whole point of the socket: \
             {status:?}"
        );

        served.abort();
        drop(dir);
    }

    /// The sweep half: the paths a `changed` op queues are acted on by a
    /// loop `serve_with` also has to start.
    ///
    /// A sweep's one effect visible over the socket is its shortfall line,
    /// and a sweep only reports one for a path that routes somewhere. So the
    /// config names a server that starts and never answers `initialize`:
    /// routing then reports it as still starting, which is a file that
    /// should have been checked and was not. A command that cannot spawn
    /// would not do -- the background spawn fails, every route is dropped,
    /// and the sweep goes silent.
    #[cfg(feature = "transport-http")]
    #[tokio::test]
    async fn test_serve_with_runs_the_sweep_loop_it_built() {
        use crate::config::{HooksConfig, LspServerConfig};

        let (dir, identity) = temp_identity();
        let workspace = tempfile::tempdir().expect("a temp dir");
        let changed = workspace.path().join("a.rs");
        std::fs::write(&changed, "fn a() {}").expect("write");

        let mut config = bare_config_over(workspace.path());
        config.lsp_servers = vec![LspServerConfig {
            language_id: "rust".to_string(),
            command: NEVER_ANSWERS.0.to_string(),
            args: NEVER_ANSWERS
                .1
                .iter()
                .map(|arg| (*arg).to_string())
                .collect(),
            env: HashMap::new(),
            file_patterns: vec!["**/*.rs".to_string()],
            initialization_options: None,
            timeout_seconds: 30,
            request_timeout_seconds: 30,
            heuristics: None,
            name: None,
            handles: None,
            diagnostics_severity: None,
        }];
        config.diagnostics.hooks = HooksConfig {
            sweep_quiet_ms: 50,
            ..HooksConfig::default()
        };

        let served = tokio::spawn(crate::serve_with_identity(
            config,
            loopback_transport(),
            Some(identity.clone()),
        ));
        assert!(matches!(
            status_from_owner(&identity).await,
            Response::Status { owner: true, .. }
        ));

        let queued = crate::hooks::send(
            &identity,
            &Request::Changed {
                session: "s1".to_string(),
                paths: vec![dunce::canonicalize(&changed).expect("canonicalize")],
                event: ChangeEvent::Change,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("the owner answers");
        assert_eq!(queued, Response::Changed { queued: 1 });

        let mut reported = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Ok(Response::Flush {
                context: Some(text),
                ..
            }) = crate::hooks::send(
                &identity,
                &Request::Flush {
                    session: "s1".to_string(),
                },
                Duration::from_secs(5),
            )
            .await
            {
                reported = Some(text);
                break;
            }
        }
        let reported = reported.expect(
            "nothing ever swept the queued path, so every path a hook reports \
             reaches a set that is never drained and no flush ever mentions it",
        );
        assert!(reported.contains("not checked"), "{reported}");

        served.abort();
        drop(dir);
    }

    /// Real time, not a paused clock: the retry's `acquire` runs on a
    /// blocking thread, and no amount of advancing a virtual clock or
    /// yielding on the runtime is guaranteed to have observed it finish.
    /// The role's own transition signal is what the test waits on, so each
    /// wait ends on the event itself rather than on an iteration count, and
    /// both timeouts are far longer than the five second retry they bound.
    #[tokio::test]
    async fn test_a_passive_instance_takes_over_when_the_owner_exits() {
        let (dir, identity) = temp_identity();
        let owner = HookListener::acquire(&identity)
            .await
            .expect("acquire")
            .expect("owner");
        let role = Arc::new(HookRole::passive(identity.clone()));
        let mut transitions = role.subscribe_transitions();
        let (_tx, cancel) = tokio::sync::watch::channel(false);
        let (server, sweeper) = takeover_candidate(&dir, Arc::clone(&role));
        tokio::spawn(hook_takeover_task(
            HookLocation {
                identity: identity.clone(),
                root: dir.path().to_path_buf(),
            },
            Arc::clone(&role),
            server,
            sweeper,
            Duration::from_millis(1500),
            cancel,
        ));

        // The retry's first tick fires at once, so this window covers a whole
        // attempt against a lock the owner still holds.
        assert!(
            tokio::time::timeout(Duration::from_millis(500), transitions.changed())
                .await
                .is_err(),
            "the owner still holds the lock"
        );
        assert!(matches!(role.get(), Role::Passive { .. }));

        drop(owner);
        tokio::time::timeout(Duration::from_secs(60), transitions.changed())
            .await
            .expect(
                "an owner exiting must not leave every other session permanently \
                 passive, which is the failure the spec's five second retry exists \
                 to prevent",
            )
            .expect("the role outlives the takeover task");

        assert_eq!(role.get(), Role::Owner);
        drop(dir);
    }
}
