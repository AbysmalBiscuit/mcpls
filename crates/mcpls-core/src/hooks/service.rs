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
use crate::hooks::listener::{HookListener, ServeExit};
use crate::hooks::protocol::{Request, Response};
use crate::hooks::sweep::Sweeper;
use crate::mcp::McplsServer;

/// How long a passive instance waits between attempts on the ownership
/// lock.
///
/// The spec's figure. Short enough that an agent's next turn reaches a
/// process that has taken over, long enough that a project left running
/// with several instances costs nothing measurable.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_secs(5);

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
    /// A process with hooks switched off.
    #[must_use]
    pub const fn disabled() -> Self {
        Self(std::sync::Mutex::new(Role::Disabled))
    }

    /// A process that won the ownership lock.
    #[must_use]
    pub const fn owner() -> Self {
        Self(std::sync::Mutex::new(Role::Owner))
    }

    /// A process that lost the ownership lock to whoever holds `identity`.
    #[must_use]
    pub const fn passive(identity: SocketIdentity) -> Self {
        Self(std::sync::Mutex::new(Role::Passive { identity }))
    }

    /// A cloned snapshot, so no `std::sync::Mutex` guard is held across an
    /// `.await`.
    #[must_use]
    pub fn get(&self) -> Role {
        lock_std(&self.0).clone()
    }

    /// Move this process from `Passive` to `Owner`, called by the takeover
    /// task once it wins the lock. The only runtime transition there is.
    pub fn promote_to_owner(&self) {
        *lock_std(&self.0) = Role::Owner;
    }
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
                    hash: identity.hash.clone(),
                    socket: identity.socket.clone(),
                    pid: std::process::id(),
                    owner: true,
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
pub(crate) async fn hook_owner_task(
    mut listener: HookListener,
    identity: SocketIdentity,
    server: Arc<McplsServer>,
    sweeper: Arc<Sweeper>,
    op_deadline: Duration,
    mut cancel: watch::Receiver<bool>,
) {
    loop {
        let handler = build_handler(Arc::clone(&server), Arc::clone(&sweeper), identity.clone());
        match listener.serve(handler, op_deadline, cancel.clone()).await {
            ServeExit::Cancelled | ServeExit::TransportUnrecoverable => return,
            ServeExit::LockLost => {}
        }
        let Some(reacquired) = acquire_when_free(&identity, &mut cancel).await else {
            return;
        };
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
    identity: SocketIdentity,
    role: Arc<HookRole>,
    server: Arc<McplsServer>,
    sweeper: Arc<Sweeper>,
    op_deadline: Duration,
    mut cancel: watch::Receiver<bool>,
) {
    let Some(listener) = acquire_when_free(&identity, &mut cancel).await else {
        return;
    };
    role.promote_to_owner();
    hook_owner_task(listener, identity, server, sweeper, op_deadline, cancel).await;
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
        /// Kept alive alongside the `serve` task that also holds it, so the
        /// owner's server outlives any one connection.
        _server: Arc<McplsServer>,
        sweeper: Arc<Sweeper>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        delivery: Arc<Mutex<DiagnosticsDelivery>>,
        _cancel: tokio::sync::watch::Sender<bool>,
    }

    impl HookHarness {
        /// An owner with an empty baseline adopted, so a flush answers a
        /// real report rather than `starting_up()`.
        async fn owner() -> Self {
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
                build_handler(Arc::clone(&server), Arc::clone(&sweeper), identity.clone()),
                Duration::from_millis(1500),
                cancel_rx,
            ));

            let harness = Self {
                dir,
                identity,
                _server: server,
                sweeper,
                notification_cache,
                delivery,
                _cancel: cancel_tx,
            };
            harness.delivery.lock().await.set_baseline(HashMap::new());
            harness
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
            let delivery = Arc::new(Mutex::new(DiagnosticsDelivery::new(diagnostics)));
            let context = Arc::new(test_context(
                self.dir.path(),
                Arc::new(Translator::new()),
                Arc::new(Mutex::new(NotificationCache::new())),
                Arc::clone(&delivery),
                diagnostics,
                Arc::new(HookRole::passive(self.identity.clone())),
            ));
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

    #[tokio::test(start_paused = true)]
    async fn test_a_passive_instance_takes_over_when_the_owner_exits() {
        let (dir, identity) = temp_identity();
        let owner = HookListener::acquire(&identity)
            .await
            .expect("acquire")
            .expect("owner");
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
        // The retry runs its `acquire` on a blocking thread, so advancing
        // the clock does not by itself mean the task has observed the lock
        // go free. Give the transition a bounded number of chances and then
        // assert what happened, rather than asserting after one poll.
        for _ in 0..200 {
            if role.get() == Role::Owner {
                break;
            }
            tokio::time::advance(Duration::from_secs(6)).await;
            tokio::task::yield_now().await;
        }

        assert_eq!(
            role.get(),
            Role::Owner,
            "an owner exiting must not leave every other session permanently \
             passive, which is the failure the spec's five second retry exists \
             to prevent"
        );
        drop(dir);
    }
}
