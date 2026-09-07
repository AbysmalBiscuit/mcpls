//! Serving the hook socket's operations from a running mcpls.
//!
//! One process per project owns the socket and answers every session's
//! hooks against the same per-session record its own flush tool uses, so a
//! hook and an agent never see the same diagnostic twice. Any other mcpls
//! in the same project is passive: it forwards rather than answering, and
//! keeps competing for the lock so an owner exiting does not strand it.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::watch;

use crate::bridge::{SessionId, lock_std};
use crate::hooks::identity::SocketIdentity;
use crate::hooks::listener::{HookListener, LockLoss, ServeExit};
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
        }
    }

    /// A cloned snapshot, so no `std::sync::Mutex` guard is held across an
    /// `.await`.
    #[must_use]
    pub fn get(&self) -> Role {
        lock_std(&self.role).clone()
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

    /// Move this process back to `Passive`, called when a competitor has
    /// taken the ownership lock away from a listener that was serving.
    ///
    /// A competitor holds the lock for its whole life, so a process that
    /// kept reading `Owner` would run its footer, answer from its own
    /// record and forward nothing, while every hook for the same session
    /// reached the competitor's separate record: two processes behaving as
    /// owners for one session, which is what the lock exists to make
    /// impossible. Only a replaced lock demotes; a lock file that was
    /// merely removed leaves no competitor and is won back on the next
    /// attempt.
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
/// cache. Every path through here reaches both only by way of
/// `McplsServer::flush_for_hook`, which takes them in that order and drops
/// both before it awaits the payload build, so no arm of this match has to
/// take either lock itself. Do not add one that does.
pub fn build_handler(
    server: Arc<McplsServer>,
    sweeper: Arc<Sweeper>,
    location: HookLocation,
    cancel: watch::Receiver<bool>,
) -> impl Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static {
    move |request| {
        let server = Arc::clone(&server);
        let sweeper = Arc::clone(&sweeper);
        let location = location.clone();
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
                    // The event kind is discarded: the sweep stats.
                    Response::Changed {
                        queued: sweeper.enqueue(&paths),
                    }
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
                    hash: location.identity.hash.clone(),
                    socket: location.identity.socket.clone(),
                    pid: std::process::id(),
                    owner: true,
                    root: location.root.clone(),
                },
            }
        })
    }
}

/// Serve the socket for as long as this process owns it, re-entering the
/// ownership race if the lock file stops proving ownership.
///
/// [`ServeExit::LockLost`] means the lock file is no longer the file this
/// listener holds a lock on: a competitor took it, or a temporary-file
/// cleaner simply removed it. Neither is a reason to end hook support for
/// the project for the rest of this process's life, so the listener that
/// stood down goes back to competing on the same five second interval a
/// passive instance uses. [`ServeExit::TransportUnrecoverable`] is the one
/// exit that retrying cannot clear, and it stops here.
///
/// A [`LockLoss::Replaced`] also moves the role back to `Passive` for the
/// duration of that wait, because a competitor holds the lock for its own
/// whole life: a process still reading `Owner` would answer from its own
/// record while every hook for the same session reached the competitor's.
/// [`LockLoss::Missing`] leaves the role alone, because nothing has taken
/// this listener's place and the next attempt wins the lock back.
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
            cancel.clone(),
        );
        match listener.serve(handler, op_deadline, cancel.clone()).await {
            ServeExit::Cancelled | ServeExit::TransportUnrecoverable => return,
            ServeExit::LockLost(LockLoss::Replaced) => {
                role.demote_to_passive(location.identity.clone());
            }
            ServeExit::LockLost(LockLoss::Missing) => {}
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

    use tempfile::TempDir;
    use tokio::sync::Mutex;

    use super::*;
    use crate::bridge::{
        DiagnosticsDelivery, FloorTable, NotificationCache, ResourceSubscriptions, ServerSettle,
        Translator,
    };
    use crate::config::{DiagnosticsConfig, ServerId};
    use crate::hooks::{ChangeEvent, HookListener, PathFilter, Request, Response, SocketIdentity};
    use crate::mcp::McplsServer;

    /// An in-process owner: a real listener on a temporary socket, a real
    /// `McplsServer`, and a real `Sweeper`, wired by the same
    /// `build_handler` `serve_with` uses.
    struct HookHarness {
        dir: TempDir,
        identity: SocketIdentity,
        sweeper: Arc<Sweeper>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        delivery: Arc<Mutex<DiagnosticsDelivery>>,
        _cancel: tokio::sync::watch::Sender<bool>,
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
            let (dir, identity) = temp_identity();
            let translator = Arc::new(Translator::new());
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            let delivery = Arc::new(Mutex::new(DiagnosticsDelivery::new(
                DiagnosticsConfig::default(),
            )));
            let context = Arc::new(test_context(
                dir.path(),
                Arc::clone(&translator),
                Arc::clone(&notification_cache),
                Arc::clone(&delivery),
                DiagnosticsConfig::default(),
                Arc::new(HookRole::owner()),
            ));
            let server = Arc::new(McplsServer::from_context(context));
            let sweeper = Arc::new(test_sweeper(dir.path(), translator));
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            tokio::spawn(Arc::clone(&sweeper).run(cancel_rx.clone()));

            let listener = HookListener::acquire(&identity)
                .await
                .expect("acquire")
                .expect("the harness owns its own temporary socket");
            tokio::spawn(listener.serve(
                build_handler(
                    server,
                    Arc::clone(&sweeper),
                    HookLocation {
                        identity: identity.clone(),
                        root: dir.path().to_path_buf(),
                    },
                    cancel_rx.clone(),
                ),
                Duration::from_millis(1500),
                cancel_rx,
            ));

            Self {
                dir,
                identity,
                sweeper,
                notification_cache,
                delivery,
                _cancel: cancel_tx,
            }
        }

        /// The same, with one error already in the notification cache for
        /// `broken.rs`, so the first flush has something to report.
        async fn owner_with_one_error() -> Self {
            let harness = Self::owner().await;
            harness.notification_cache.lock().await.store_diagnostics(
                &ServerId::from("rust"),
                &broken_uri(),
                Some(1),
                vec![error_diagnostic()],
            );
            harness
        }

        /// An absolute path under this harness's temporary workspace.
        fn fixture(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }

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
        Sweeper::new(
            translator,
            PathFilter::new(
                Arc::from(vec![root.to_path_buf()]),
                Arc::new(HashMap::from([("rs".to_string(), "rust".to_string())])),
                None,
            ),
            Duration::from_millis(500),
            usize::MAX,
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
            message: "broken".to_string(),
            ..lsp_types::Diagnostic::default()
        }
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
    async fn test_a_flush_op_and_the_tool_share_one_record() {
        let harness = HookHarness::owner_with_one_error().await;
        let first = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        let Response::Flush {
            context: Some(text),
        } = first
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
            Response::Flush { context: None },
            "one report per problem, whichever door asked for it"
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
            matches!(other, Response::Flush { context: Some(_) }),
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

        assert!(matches!(again, Response::Flush { context: Some(_) }));
    }

    #[tokio::test]
    async fn test_a_shortfall_reaches_the_flush_response() {
        let harness = HookHarness::owner().await;
        harness
            .sweeper
            .set_shortfall_for_test("7 file(s) not checked: the document limit of 3 was reached");

        let Response::Flush {
            context: Some(text),
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
    }

    /// A forward that fails must not fall back to this process's own
    /// record.
    ///
    /// The owner advances the session's record before it writes a byte back,
    /// so a forward that fails may already have consumed a report. Answering
    /// from the local record hands the agent a different record's diff and
    /// leaves the two disagreeing about what this session has been shown for
    /// the rest of its life.
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
    /// The owner answers at its deadline rather than before it, and it has
    /// already consumed the session's report by then, so a client bound
    /// tighter loses a report nobody ever sees.
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
        assert_eq!(early, Response::Flush { context: None });

        harness.delivery.lock().await.set_baseline(HashMap::new());
        let Response::Flush {
            context: Some(text),
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

    /// A lock file removed by a temporary-file cleaner is not a competitor,
    /// so the owner wins it back and goes on serving.
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

        std::fs::remove_file(&identity.lock).expect("stand in for a temp-file cleaner");

        assert!(
            matches!(
                status_from_owner(&identity).await,
                Response::Status { owner: true, .. }
            ),
            "standing down for good would end hook support for this project for \
             the rest of the process's life over a file anything may delete"
        );
        assert_eq!(
            role.get(),
            Role::Owner,
            "nothing took this listener's place, so there is nobody to be passive to"
        );
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
