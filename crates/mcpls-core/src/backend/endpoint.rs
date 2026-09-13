//! The backend's side of the project endpoint: one accept loop dispatching
//! MCP sessions, hook calls and shutdown requests, and the idle timer that
//! ends the process.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::backend::handshake::{
    self, ConfigStamp, ConnectionKind, Handshake, HandshakeReply, Refusal,
};
use crate::bridge::{ConnectionId, SessionId, lock_std};
use crate::config::ServerConfig;
use crate::error::Error;
use crate::hooks::listener::{HookStream, serve_hook_connection};
use crate::hooks::{
    self, HookListener, HookLocation, HookStats, Request, Response, SocketIdentity,
};
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

#[derive(Clone)]
struct TrackedService {
    server: McplsServer,
    handlers: mpsc::UnboundedSender<()>,
}

impl rmcp::Service<rmcp::RoleServer> for TrackedService {
    async fn handle_request(
        &self,
        request: <rmcp::RoleServer as rmcp::service::ServiceRole>::PeerReq,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<<rmcp::RoleServer as rmcp::service::ServiceRole>::Resp, rmcp::ErrorData> {
        let _handler = self.handlers.clone();
        <McplsServer as rmcp::Service<rmcp::RoleServer>>::handle_request(
            &self.server,
            request,
            context,
        )
        .await
    }

    async fn handle_notification(
        &self,
        notification: <rmcp::RoleServer as rmcp::service::ServiceRole>::PeerNot,
        context: rmcp::service::NotificationContext<rmcp::RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        let _handler = self.handlers.clone();
        <McplsServer as rmcp::Service<rmcp::RoleServer>>::handle_notification(
            &self.server,
            notification,
            context,
        )
        .await
    }

    fn get_info(&self) -> <rmcp::RoleServer as rmcp::service::ServiceRole>::Info {
        <McplsServer as rmcp::Service<rmcp::RoleServer>>::get_info(&self.server)
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [rmcp::model::ProtocolVersion]> {
        <McplsServer as rmcp::Service<rmcp::RoleServer>>::supported_protocol_versions(&self.server)
    }
}

/// Run a backend until idle, shutdown, or signal, then drain it.
///
/// Returns without serving if another process holds the endpoint.
///
/// # Errors
///
/// Returns endpoint, binding, or configuration errors.
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
        info!(
            "another backend already holds {}, exiting",
            identity.socket.display()
        );
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
        let handler: Arc<HookHandler> = Arc::new(handler);
        Arc::new(Self {
            template,
            handler,
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
        let (handler_tx, mut handler_rx) = mpsc::unbounded_channel();
        let idle_expired = idle_expired(self.attachments.watch(), idle);
        let mut shutdown = self.shutdown.subscribe();
        tokio::pin!(idle_expired, signal);
        let exit = loop {
            tokio::select! {
                () = &mut signal => break Exit::Signal,
                () = &mut idle_expired => break Exit::Idle,
                () = async {
                    let _ = shutdown.wait_for(|asked| *asked).await;
                } => break Exit::Shutdown,
                accepted = listener.accept() => match accepted {
                    Ok(stream) => {
                        while tasks.try_join_next().is_some() {}
                        tasks.spawn(Arc::clone(&self).connection(stream, handler_tx.clone()));
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
        while tasks.join_next().await.is_some() {}
        drop(handler_tx);
        if tokio::time::timeout(Duration::from_secs(5), async {
            while handler_rx.recv().await.is_some() {}
        })
        .await
        .is_err()
        {
            warn!("MCP handlers remained after the endpoint drain timeout");
        }
        exit
    }

    async fn connection(
        self: Arc<Self>,
        mut stream: Box<dyn HookStream>,
        handler_tx: mpsc::UnboundedSender<()>,
    ) {
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
            ConnectionKind::Mcp => self.serve_mcp(stream, request, closing, handler_tx).await,
            ConnectionKind::Shutdown => {
                let _ = self.shutdown.send(true);
            }
        }
    }

    fn refusal_for(&self, request: &Handshake, sessions: usize) -> Option<Refusal> {
        if request.kind == ConnectionKind::Shutdown {
            if sessions > 0 {
                debug!(
                    sessions = ?self.attachments.sessions(),
                    "shutdown refused while sessions are attached"
                );
            }
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
        handler_tx: mpsc::UnboundedSender<()>,
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
        let server = TrackedService {
            server,
            handlers: handler_tx,
        };
        let _attached = self
            .attachments
            .attach(connection, server.server.session().to_string());

        let running = tokio::select! {
            result = server.serve(stream) => match result {
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
        let _ = closer.await;
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
        if *count.borrow() != 0 && count.wait_for(|attached| *attached == 0).await.is_err() {
            return std::future::pending().await;
        }
        tokio::select! {
            () = tokio::time::sleep(idle) => return,
            changed = count.changed() => {
                if changed.is_err() {
                    return std::future::pending().await;
                }
            }
        }
    }
}

/// The MCP sessions attached to this backend.
pub(crate) struct Attachments {
    sessions: std::sync::Mutex<BTreeMap<ConnectionId, String>>,
    count: watch::Sender<usize>,
}

impl Default for Attachments {
    fn default() -> Self {
        Self {
            sessions: std::sync::Mutex::new(BTreeMap::new()),
            count: watch::channel(0).0,
        }
    }
}

/// Detaches its connection when dropped.
pub(crate) struct Attached {
    attachments: Arc<Attachments>,
    connection: ConnectionId,
}

impl Attachments {
    pub(crate) fn attach(self: &Arc<Self>, connection: ConnectionId, session: String) -> Attached {
        let mut sessions = lock_std(&self.sessions);
        sessions.insert(connection, session);
        self.count.send_replace(sessions.len());
        drop(sessions);
        Attached {
            attachments: Arc::clone(self),
            connection,
        }
    }

    pub(crate) fn count(&self) -> usize {
        lock_std(&self.sessions).len()
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
        let mut sessions = lock_std(&self.attachments.sessions);
        sessions.remove(&self.connection);
        self.attachments.count.send_replace(sessions.len());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::*;
    use crate::backend::handshake::{self, Handshake, HandshakeReply, Refusal};
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
        Backend {
            _dir: dir,
            root,
            identity,
            task,
        }
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
        Handshake::mcp(backend.root.clone(), None, ConfigStamp::of(&config(0)))
    }

    async fn initialize(
        stream: &mut Box<dyn crate::hooks::listener::HookStream>,
    ) -> serde_json::Value {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_attachment_publications_match_sessions() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Barrier};

        const WORKERS: usize = 8;
        const ROUNDS: usize = 10_000;

        let attachments = Arc::new(Attachments::default());
        let start = Arc::new(Barrier::new(WORKERS + 1));
        let done = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(WORKERS);
        for worker in 0..WORKERS {
            let attachments = Arc::clone(&attachments);
            let start = Arc::clone(&start);
            workers.push(tokio::task::spawn_blocking(move || {
                start.wait();
                for round in 0..ROUNDS {
                    let connection = ConnectionId::next();
                    let attached =
                        attachments.attach(connection, format!("session-{worker}-{round}"));
                    std::thread::yield_now();
                    drop(attached);
                }
            }));
        }

        let observed = {
            let attachments = Arc::clone(&attachments);
            let start = Arc::clone(&start);
            let done = Arc::clone(&done);
            tokio::task::spawn_blocking(move || {
                start.wait();
                loop {
                    let sessions = lock_std(&attachments.sessions);
                    let count = *attachments.count.borrow();
                    if sessions.len() != count {
                        let mismatch = (sessions.len(), count);
                        drop(sessions);
                        return Some(mismatch);
                    }
                    if done.load(Ordering::Acquire) {
                        return None;
                    }
                    std::thread::yield_now();
                }
            })
        };

        for worker in workers {
            worker.await.unwrap();
        }
        done.store(true, Ordering::Release);
        assert_eq!(observed.await.unwrap(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn test_idle_timer_restarts_after_a_coalesced_attach_detach() {
        let (sender, receiver) = watch::channel(1usize);
        let timer = tokio::spawn(idle_expired(receiver, Duration::from_secs(10)));

        sender.send_replace(0);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(9)).await;
        tokio::task::yield_now().await;

        sender.send_replace(1);
        sender.send_replace(0);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(
            !timer.is_finished(),
            "the last detachment did not restart idle time"
        );

        tokio::time::advance(Duration::from_secs(9)).await;
        timer.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_endpoint_waits_for_an_initialized_mcp_handler_before_runtime_drain() {
        use std::collections::HashMap;

        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::UnixStream;
        use tokio::sync::oneshot;

        let directory = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(directory.path()).unwrap();
        let source = root.join("main.rs");
        std::fs::write(&source, "fn main() {}\n").unwrap();
        let script = root.join("held_lsp.py");
        std::fs::write(
            &script,
            r#"import json
import socket
import sys
import threading

ready, hover, control_path, runtime_shutdown = sys.argv[1:]

release = threading.Event()
control = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
control.bind(control_path)
control.listen(1)

def wait_for_release():
    connection, _ = control.accept()
    connection.recv(1)
    release.set()
    connection.close()

threading.Thread(target=wait_for_release, daemon=True).start()

def read_message():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        name, value = line.decode().split(":", 1)
        if name.lower() == "content-length":
            length = int(value.strip())
    return json.loads(sys.stdin.buffer.read(length))

def send(message):
    body = json.dumps(message, separators=(",", ":")).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode())
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        break
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {"hoverProvider": True}}})
        open(ready, "w").close()
    elif method == "shutdown":
        open(runtime_shutdown, "w").close()
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif method == "textDocument/hover":
        open(hover, "w").close()
        release.wait()
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
"#,
        )
        .unwrap();

        let ready = root.join("lsp-ready");
        let hover = root.join("hover-in-flight");
        let control = root.join("release.sock");
        let runtime_shutdown = root.join("runtime-shutdown");
        let mut backend_config = config(60_000);
        backend_config.workspace.roots = vec![root.clone()];
        backend_config.lsp_servers = vec![crate::config::LspServerConfig {
            language_id: "rust".to_string(),
            command: "python3".to_string(),
            args: vec![
                script.to_string_lossy().into_owned(),
                ready.to_string_lossy().into_owned(),
                hover.to_string_lossy().into_owned(),
                control.to_string_lossy().into_owned(),
                runtime_shutdown.to_string_lossy().into_owned(),
            ],
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

        let identity = temp_identity(&root);
        let listener = HookListener::acquire(&identity).await.unwrap().unwrap();
        let runtime = crate::Runtime::start(&backend_config, Ok(root.clone()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let endpoint = Endpoint::new(&runtime, &backend_config, root.clone(), identity.clone());
        let (signal_tx, signal_rx) = oneshot::channel();
        let (drain_started_tx, mut drain_started_rx) = oneshot::channel();
        let mut run = tokio::spawn(async move {
            let exit = endpoint
                .run(listener, Duration::from_secs(60), async move {
                    let _ = signal_rx.await;
                })
                .await;
            drain_started_tx.send(()).unwrap();
            runtime.shutdown().await;
            exit
        });

        let mut stream = crate::hooks::listener::connect(&identity).await.unwrap();
        let request = Handshake::mcp(root.clone(), None, ConfigStamp::of(&backend_config));
        handshake::write(&mut stream, &request).await.unwrap();
        let reply = handshake::read::<_, HandshakeReply>(&mut stream)
            .await
            .unwrap();
        assert_eq!(reply.refusal, None);
        let answer = initialize(&mut stream).await;
        assert_eq!(answer["result"]["serverInfo"]["name"], "mcpls");
        stream
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        let call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "get_hover",
                "arguments": {
                    "file_path": source.to_string_lossy(),
                    "line": 0,
                    "character": 0
                }
            }
        });
        stream
            .write_all(format!("{}\n", serde_json::to_string(&call).unwrap()).as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !hover.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        signal_tx.send(()).unwrap();
        let mut closed = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut closed))
            .await
            .unwrap()
            .unwrap();
        let endpoint_exit = tokio::time::timeout(Duration::from_millis(100), &mut run).await;
        assert!(
            endpoint_exit.is_err(),
            "the endpoint exited before the in-flight handler was released"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut drain_started_rx)
                .await
                .is_err(),
            "runtime drain began before the in-flight handler was released"
        );

        let mut release = UnixStream::connect(&control).await.unwrap();
        release.write_all(&[1]).await.unwrap();
        drop(release);
        let exit = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exit, Exit::Signal);
        drop(stream);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !runtime_shutdown.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
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
        assert!(
            !backend.task.is_finished(),
            "exited with a session attached"
        );

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
        assert!(
            matches!(reply.refusal, Some(Refusal::Trust { .. })),
            "{reply:?}"
        );
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
        assert!(
            instructions.contains(&other.fingerprint()),
            "{instructions}"
        );
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
        let listener = crate::hooks::HookListener::acquire(&identity)
            .await
            .unwrap()
            .unwrap();
        let runtime = crate::Runtime::start(&config(50), Ok(root.clone()))
            .await
            .unwrap();
        let endpoint = Endpoint::new(&runtime, &config(50), root, identity.clone());

        let exit = endpoint
            .run(listener, Duration::from_millis(50), std::future::pending())
            .await;
        assert_eq!(exit, Exit::Idle);
        assert!(
            crate::hooks::HookListener::acquire(&identity)
                .await
                .unwrap()
                .is_some(),
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
        let runtime = crate::Runtime::start(&config(60_000), Ok(root.clone()))
            .await
            .unwrap();
        let endpoint = Endpoint::new(&runtime, &config(60_000), root.clone(), identity);
        endpoint.closing.send_replace(true);

        let (mut client, server) = tokio::io::duplex(4096);
        let (handler_tx, _handler_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(Arc::clone(&endpoint).connection(Box::new(server), handler_tx));
        handshake::write(
            &mut client,
            &Handshake::mcp(root, None, ConfigStamp::of(&config(0))),
        )
        .await
        .unwrap();
        let reply = tokio::time::timeout(
            Duration::from_secs(5),
            handshake::read::<_, HandshakeReply>(&mut client),
        )
        .await
        .expect("the connection hung up rather than waiting");
        assert!(
            reply.is_err(),
            "a closing endpoint answered a handshake: {reply:?}"
        );
        task.await.unwrap();
        runtime.shutdown().await;
    }
}
