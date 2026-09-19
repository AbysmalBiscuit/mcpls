//! Serving the hook socket's operations from a running mcpls.
//!
//! The process holding the endpoint answers every session's hooks against
//! the same per-session record its own flush tool uses, so a hook and an
//! agent never see the same diagnostic twice.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::future::BoxFuture;
use tokio::sync::watch;

use crate::bridge::SessionId;
use crate::hooks::identity::SocketIdentity;
use crate::hooks::protocol::{Request, Response, ServerStatus};
use crate::hooks::sweep::Sweeper;
use crate::mcp::McplsServer;

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
    pub servers: Vec<ServerStatus>,
    /// The configuration fingerprint this process started with.
    pub config_fingerprint: String,
    /// What the filesystem watcher is doing.
    pub watcher: crate::hooks::protocol::WatcherStatus,
}

/// Computes [`StatusExtras`] at the moment a status is asked for.
pub type StatusSource = Arc<dyn Fn() -> StatusExtras + Send + Sync>;

/// The handler [`crate::hooks::listener::HookListener::serve`] runs,
/// closing over the MCP server and the sweeper.
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
    stats: Arc<HookStats>,
    source: StatusSource,
    cancel: watch::Receiver<bool>,
) -> impl Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static {
    move |request| {
        let server = Arc::clone(&server);
        let sweeper = Arc::clone(&sweeper);
        let location = location.clone();
        let stats = Arc::clone(&stats);
        let source = Arc::clone(&source);
        let cancelled = *cancel.borrow();
        Box::pin(async move {
            match request {
                Request::Changed {
                    agent,
                    session,
                    attributed,
                    paths,
                    ..
                } => {
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
                    stats.record_hook_request();
                    server.touch_hook(&agent.caller(&session)).await;
                    let paths = sweeper.admitted_paths(&paths);
                    if attributed {
                        server
                            .attribute_paths(&agent.caller(&session), &paths)
                            .await;
                    }
                    Response::Changed {
                        queued: sweeper.enqueue(&paths),
                    }
                }
                Request::Flush { agent, session } => {
                    stats.record_hook_request();
                    let caller = agent.caller(&session);
                    server.touch_hook(&caller).await;
                    let (report, token) = server.flush_for_hook(&caller.record).await;
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
                Request::Ack {
                    agent,
                    session,
                    token,
                } => {
                    server.touch_hook(&agent.caller(&session)).await;
                    server
                        .commit_for_hook(&agent.caller(&session).record, token)
                        .await;
                    Response::Ack
                }
                Request::EndSession { session } => {
                    stats.record_hook_request();
                    server.end_session(&SessionId::from(session)).await;
                    Response::EndSession
                }
                Request::Status => {
                    let extras = source();
                    Response::Status {
                        hash: location.identity.hash.clone(),
                        socket: location.identity.socket.clone(),
                        pid: std::process::id(),
                        owner: true,
                        root: location.root.clone(),
                        hooks_seen: stats.hooks_seen(),
                        version: extras.version,
                        uptime_ms: extras.uptime_ms,
                        sessions: extras.sessions,
                        servers: extras.servers,
                        config_fingerprint: extras.config_fingerprint,
                        watcher: Box::new(extras.watcher),
                    }
                }
            }
        })
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
        DiagnosticsDelivery, FloorTable, NotificationCache, ResourceLimits, ResourceSubscriptions,
        ServerSettle, Translator, TranslatorHarness,
    };
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
        stats: Arc<HookStats>,
        server: Arc<McplsServer>,
        sweeper: Arc<Sweeper>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        delivery: Arc<Mutex<DiagnosticsDelivery>>,
        translator_harness: Option<TranslatorHarness>,
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
            Self::owner_without_baseline_with(DiagnosticsConfig::default()).await
        }

        /// An owner with the supplied diagnostics timing and delivery config,
        /// before its baseline is adopted.
        async fn owner_without_baseline_with(diagnostics: DiagnosticsConfig) -> Self {
            let (dir, identity) = temp_identity();
            let translator = Arc::new(Translator::new());
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            let delivery = Arc::new(Mutex::new(DiagnosticsDelivery::new(diagnostics)));
            let stats = Arc::new(HookStats::default());
            Self::start_owner(
                dir,
                identity,
                stats,
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
            let stats = Arc::new(HookStats::default());
            let harness = Self::start_owner(
                dir,
                identity,
                stats,
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
            stats: Arc<HookStats>,
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
            let root = dunce::canonicalize(root).expect("canonicalize the owner root");
            let context = Arc::new(test_context(
                &root,
                Arc::clone(&translator),
                Arc::clone(&notification_cache),
                Arc::clone(&delivery),
                diagnostics,
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
            tokio::spawn(listener.serve(
                build_handler(
                    Arc::clone(&server),
                    Arc::clone(&sweeper),
                    HookLocation {
                        identity: identity.clone(),
                        root: root.clone(),
                    },
                    Arc::clone(&stats),
                    Arc::new(StatusExtras::default),
                    cancel_rx.clone(),
                ),
                Duration::from_millis(1500),
                cancel_rx,
            ));

            Self {
                dir,
                root,
                identity,
                stats,
                server,
                sweeper,
                notification_cache,
                delivery,
                translator_harness,
                _cancel: cancel_tx,
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
                    agent: crate::bridge::HookAgent::default(),
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
        use std::sync::atomic::{AtomicU64, Ordering};

        // The pid separates concurrent processes, the counter separates
        // calls within one. The clock cannot: it ticks every 100ns and
        // `DefaultHasher` is unseeded, so simultaneous starts collide.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::process::id().hash(&mut hasher);
        COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
        hasher.finish()
    }

    /// A `BridgeContext` rooted at `root`.
    fn test_context(
        root: &std::path::Path,
        translator: Arc<Translator>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        delivery: Arc<Mutex<DiagnosticsDelivery>>,
        diagnostics: DiagnosticsConfig,
    ) -> crate::mcp::BridgeContext {
        crate::mcp::BridgeContext::new(
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
        )
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
                attributed: false,
                agent: crate::bridge::HookAgent::default(),
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
                        attributed: false,
                        agent: crate::bridge::HookAgent::default(),
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
                agent: crate::bridge::HookAgent::default(),
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
                        attributed: false,
                        agent: crate::bridge::HookAgent::default(),
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
                agent: crate::bridge::HookAgent::default(),
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
                agent: crate::bridge::HookAgent::default(),
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
                agent: crate::bridge::HookAgent::default(),
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
                agent: crate::bridge::HookAgent::default(),
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
                agent: crate::bridge::HookAgent::default(),
                session: "s1".to_string(),
            })
            .await
        else {
            panic!("the second flush carries a token");
        };
        assert_ne!(stale, current);

        let answer = harness
            .send(Request::Ack {
                agent: crate::bridge::HookAgent::default(),
                session: "s1".to_string(),
                token: stale,
            })
            .await;
        assert_eq!(answer, Response::Ack);

        let third = harness
            .send(Request::Flush {
                agent: crate::bridge::HookAgent::default(),
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
                agent: crate::bridge::HookAgent::default(),
                session: "s1".to_string(),
            })
            .await;
        let other = harness
            .send(Request::Flush {
                agent: crate::bridge::HookAgent::default(),
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
                agent: crate::bridge::HookAgent::default(),
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
                agent: crate::bridge::HookAgent::default(),
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
                agent: crate::bridge::HookAgent::default(),
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

    /// The listener re-`stat`s its lock on its own interval, so a test that
    /// removes the file has to wait for that tick rather than assume it.
    /// Bounded, and every arm ends in an assertion naming what went wrong.
    async fn status_from_endpoint(identity: &SocketIdentity) -> Response {
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
                agent: crate::bridge::HookAgent::default(),
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
                agent: crate::bridge::HookAgent::default(),
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
        let harness = HookHarness::owner().await;

        let Response::Status { root, .. } = status_from_endpoint(&harness.identity).await else {
            panic!("expected a status response");
        };
        assert_eq!(root, harness.root);
    }

    /// `mcpls hook doctor` tells "a server that has never been sent a hook"
    /// apart from "one that has been serving them all along" by this
    /// count. `Status` itself, the doctor's own probe, must not inflate it,
    /// or every doctor run would make an unregistered plugin look wired up.
    #[tokio::test]
    async fn test_the_status_response_counts_hook_requests_the_owner_has_answered() {
        let harness = HookHarness::owner().await;

        let Response::Status { hooks_seen, .. } = status_from_endpoint(&harness.identity).await
        else {
            panic!("expected a status response");
        };
        assert_eq!(
            hooks_seen, 0,
            "several Status probes above must not themselves count as hooks"
        );
        assert_eq!(harness.stats.hooks_seen(), 0);

        crate::hooks::send(
            &harness.identity,
            &Request::EndSession {
                session: "s1".to_string(),
            },
            Duration::from_secs(5),
        )
        .await
        .expect("the owner answers");

        let Response::Status { hooks_seen, .. } = status_from_endpoint(&harness.identity).await
        else {
            panic!("expected a status response");
        };
        assert_eq!(
            hooks_seen, 1,
            "one real hook request was answered since startup"
        );
        assert_eq!(harness.stats.hooks_seen(), 1);
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
            )))),
            Arc::clone(&harness.sweeper),
            HookLocation {
                identity: harness.identity.clone(),
                root: harness.dir.path().to_path_buf(),
            },
            Arc::new(HookStats::default()),
            Arc::new(StatusExtras::default),
            cancel_rx,
        );
        cancel_tx.send(true).expect("cancel");

        let response = handler(Request::Changed {
            attributed: false,
            agent: crate::bridge::HookAgent::default(),
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

    #[tokio::test]
    async fn tool_changes_claim_files_but_watcher_changes_do_not() {
        let harness = HookHarness::owner().await;
        let path = harness.fixture("owned.rs");
        std::fs::write(&path, "broken").unwrap();
        let uri = crate::bridge::path_to_uri(&path).unwrap();
        for (attributed, agent) in [(true, Some("child")), (false, None)] {
            let request = serde_json::from_value(serde_json::json!({
                "op": "changed", "session": "root", "agent_id": agent,
                "host": "codex", "paths": [path], "event": "change",
                "attributed": attributed
            }))
            .unwrap();
            crate::hooks::send(&harness.identity, &request, Duration::from_secs(5))
                .await
                .unwrap();
        }
        harness.notification_cache.lock().await.store_diagnostics(
            &ServerId::from("rust"),
            &uri,
            Some(1),
            vec![diagnostic("owned error")],
        );
        for (agent, expected) in [(None, false), (Some("child"), true)] {
            let request = serde_json::from_value(serde_json::json!({
                "op": "flush", "session": "root", "agent_id": agent, "host": "codex"
            }))
            .unwrap();
            let response = crate::hooks::send(&harness.identity, &request, Duration::from_secs(5))
                .await
                .unwrap();
            let Response::Flush { context, .. } = response else {
                panic!("flush");
            };
            assert_eq!(context.is_some(), expected);
        }
    }

    #[tokio::test]
    async fn attribution_admits_deleted_paths_and_rejects_ignored_and_outside_paths() {
        let harness = HookHarness::owner().await;
        let deleted = harness.fixture("gone.rs");
        let ignored = harness.fixture("target/generated.rs");
        let outside = tempfile::tempdir().unwrap().path().join("outside.rs");
        let request = serde_json::from_value(serde_json::json!({
            "op": "changed", "session": "root", "agent_id": "child", "host": "codex",
            "paths": [deleted, ignored, outside], "event": "unlink", "attributed": true
        }))
        .unwrap();
        let response = crate::hooks::send(&harness.identity, &request, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(response, Response::Changed { queued: 1 });
        for path in [&deleted, &ignored, &outside] {
            harness.notification_cache.lock().await.store_diagnostics(
                &ServerId::from("rust"),
                &crate::bridge::path_to_uri(path).unwrap(),
                None,
                vec![diagnostic("error")],
            );
        }
        let caller = crate::bridge::HookAgent {
            agent_id: Some("child".into()),
            host: crate::bridge::HookHost::Codex,
        }
        .caller("root");
        let (context, _) = harness.server.flush_for_hook(&caller.record).await;
        let text = context.unwrap();
        assert!(text.contains("gone.rs"), "{text}");
        assert!(
            !text.contains("generated.rs") && !text.contains("outside.rs"),
            "{text}"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn socket_reports_keep_other_writers_snapshots_across_edits_and_caps() {
        async fn request(harness: &HookHarness, value: serde_json::Value) -> Response {
            crate::hooks::send(
                &harness.identity,
                &serde_json::from_value(value).unwrap(),
                Duration::from_secs(5),
            )
            .await
            .unwrap()
        }
        for host in ["claude", "codex"] {
            let harness = HookHarness::owner_without_baseline_with(DiagnosticsConfig {
                max_total: 1,
                ..Default::default()
            })
            .await;
            harness.delivery.lock().await.set_baseline(HashMap::new());
            let a = harness.fixture("a.rs");
            let b = harness.fixture("b.rs");
            let unowned = harness.fixture("unowned.rs");
            for agent in ["a", "b"] {
                request(
                    &harness,
                    serde_json::json!({
                        "op": "changed", "session": "root", "agent_id": agent, "host": host,
                        "attributed": true, "event": "change", "paths": [a, b]
                    }),
                )
                .await;
            }
            for path in [&a, &b, &unowned] {
                harness.notification_cache.lock().await.store_diagnostics(
                    &ServerId::from("rust"),
                    &crate::bridge::path_to_uri(path).unwrap(),
                    None,
                    vec![diagnostic("old snapshot")],
                );
            }
            let Response::Flush {
                context: Some(first),
                token: Some(token),
            } = request(
                &harness,
                serde_json::json!({
                    "op": "flush", "session": "root", "agent_id": "a", "host": host
                }),
            )
            .await
            else {
                panic!("first report");
            };
            assert!(
                first.contains("a.rs") && !first.contains("b.rs") && !first.contains("unowned.rs"),
                "{first}"
            );
            request(&harness, serde_json::json!({"op": "ack", "session": "root", "agent_id": "a", "host": host, "token": token})).await;
            request(
                &harness,
                serde_json::json!({
                    "op": "changed", "session": "root", "agent_id": "c", "host": host,
                    "attributed": true, "event": "change", "paths": [a]
                }),
            )
            .await;
            harness.notification_cache.lock().await.store_diagnostics(
                &ServerId::from("rust"),
                &crate::bridge::path_to_uri(&a).unwrap(),
                None,
                vec![diagnostic("new snapshot")],
            );
            for (agent, expected, absent) in [
                ("b", "old snapshot", "new snapshot"),
                ("c", "new snapshot", "old snapshot"),
            ] {
                let Response::Flush {
                    context: Some(text),
                    token: Some(token),
                } = request(
                    &harness,
                    serde_json::json!({
                        "op": "flush", "session": "root", "agent_id": agent, "host": host
                    }),
                )
                .await
                else {
                    panic!("retained report");
                };
                assert!(text.contains(expected) && !text.contains(absent), "{text}");
                request(&harness, serde_json::json!({"op": "ack", "session": "root", "agent_id": agent, "host": host, "token": token})).await;
            }
            for agent in ["a", "b"] {
                let Response::Flush {
                    context: Some(text),
                    token: Some(token),
                } = request(
                    &harness,
                    serde_json::json!({
                        "op": "flush", "session": "root", "agent_id": agent, "host": host
                    }),
                )
                .await
                else {
                    panic!("deferred file");
                };
                assert!(
                    text.contains("b.rs")
                        && text.contains("old snapshot")
                        && !text.contains("a.rs"),
                    "{text}"
                );
                request(&harness, serde_json::json!({"op": "ack", "session": "root", "agent_id": agent, "host": host, "token": token})).await;
            }
            let Response::Flush {
                context: Some(text),
                ..
            } = request(
                &harness,
                serde_json::json!({
                    "op": "flush", "session": "root", "host": host
                }),
            )
            .await
            else {
                panic!("root report");
            };
            assert!(
                text.contains("unowned.rs") && !text.contains("a.rs") && !text.contains("b.rs"),
                "{text}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn admitted_paths_resolve_aliases_and_missing_parent_traversal() {
        let harness = HookHarness::owner().await;
        let actual = harness.fixture("actual.rs");
        let alias = harness.fixture("alias.rs");
        std::fs::write(&actual, "source").unwrap();
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        assert_eq!(harness.sweeper.admitted_paths(&[alias]), vec![actual]);
        let escape = harness.fixture("missing/../../escape.rs");
        assert!(harness.sweeper.admitted_paths(&[escape]).is_empty());
        let deleted = harness.fixture("missing/../deleted.rs");
        assert_eq!(
            harness.sweeper.admitted_paths(&[deleted]),
            vec![harness.fixture("deleted.rs")]
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

    /// The socket half of what `serve_with` wires: acquire the endpoint and
    /// serve from a server sharing the MCP context.
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

        let status = status_from_endpoint(&identity).await;
        assert!(
            matches!(&status, Response::Status { owner: true, hash, .. } if *hash == identity.hash),
            "a running mcpls that binds nothing leaves every hook to cold-start a \
             language server on each edit, which is the whole point of the socket: \
             {status:?}"
        );

        served.abort();
        drop(dir);
    }

    /// An in-process mcpls (`--no-backend`) that finds the project's
    /// endpoint already held serves its own session without hooks, and
    /// tells that session so.
    #[cfg(feature = "transport-http")]
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

        let notes =
            crate::serve_hooks_in_process(&runtime, &config, Some(identity), Some(root)).await;

        assert_eq!(notes, vec![crate::ENDPOINT_HELD_NOTE.to_string()]);
        runtime.shutdown().await;
        drop(dir);
    }

    /// The sweep half: `serve_with` must act on paths a `changed` op queues.
    /// The missing binary settles as `NotInstalled`, so the path reaches the
    /// open loop and its failure is reported.
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
            command: workspace
                .path()
                .join("missing-lsp-server")
                .to_string_lossy()
                .into_owned(),
            args: Vec::new(),
            env: HashMap::new(),
            file_patterns: vec!["**/*.rs".to_string()],
            initialization_options: None,
            timeout_seconds: 30,
            spawn: None,
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
            status_from_endpoint(&identity).await,
            Response::Status { owner: true, .. }
        ));

        let queued = crate::hooks::send(
            &identity,
            &Request::Changed {
                attributed: false,
                agent: crate::bridge::HookAgent::default(),
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
                    agent: crate::bridge::HookAgent::default(),
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
        assert_eq!(reported, "1 file(s) not checked: they could not be opened");

        served.abort();
        drop(dir);
    }
}
