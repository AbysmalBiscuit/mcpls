//! Shared test fixtures for the `translator` module's sibling `tests`
//! submodules: an `EncodingCtx` builder, a fake in-process LSP server driven
//! over `cat` pipes, JSON-RPC framing helpers, and `TranslatorHarness`, which
//! drives that fake server on its own OS thread for tests that inspect the
//! notifications it received.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use super::Translator;
use super::encoding_ctx::EncodingCtx;
use crate::bridge::encoding::PositionEncoding;
use crate::bridge::state::ResourceLimits;
use crate::bridge::{DiagnosticInfo, DocumentTracker, lock_std};
use crate::config::{LspServerConfig, ServerId, ToolRouter};
use crate::lsp::{LspClient, LspServer, LspTransport, WatchRegistry};
pub(super) use crate::test_support::read_framed_message;

type JsonValue = serde_json::Value;

#[derive(Debug)]
pub(super) struct ResyncPause {
    pub(super) reached: oneshot::Sender<()>,
    pub(super) release: oneshot::Receiver<()>,
}

/// One notification a [`RecordingServer`] received: its method and params.
type Received = (String, JsonValue);

/// Where a [`RecordingServer`] sends the log prefix it observed ahead of a
/// sentinel.
type SentinelWaiter = Arc<StdMutex<Option<std::sync::mpsc::Sender<Vec<Received>>>>>;

/// A UTF-16 `EncodingCtx`, matching the pre-negotiation behavior: no
/// disk reads, pure line/column offsetting.
pub(super) fn test_ctx() -> EncodingCtx {
    test_ctx_with(PositionEncoding::Utf16)
}

/// An `EncodingCtx` with a fresh, empty `DocumentTracker` -- suitable for
/// tests that need a non-UTF-16 encoding and don't care about the
/// tracker fast path (e.g. exercising the disk-read fallback directly).
pub(super) fn test_ctx_with(encoding: PositionEncoding) -> EncodingCtx {
    EncodingCtx {
        encoding,
        tracker: Arc::new(DocumentTracker::new(
            ResourceLimits::default(),
            HashMap::new(),
        )),
    }
}

pub(super) fn test_uri() -> lsp_types::Uri {
    "file:///test.rs".parse().unwrap()
}

/// A fresh, empty `DocumentTracker` for tests that call
/// `diagnostics_from_cache_entry`/`merge_diagnostics` directly and don't
/// care about the tracker fast path.
pub(super) fn test_tracker() -> Arc<DocumentTracker> {
    Arc::new(DocumentTracker::new(
        ResourceLimits::default(),
        HashMap::new(),
    ))
}

/// Builds an LSP-side diagnostic for `merge_diagnostics` cache fixtures.
pub(super) fn lsp_diag(
    line: u32,
    end_character: u32,
    severity: lsp_types::DiagnosticSeverity,
    message: &str,
    code: Option<&str>,
) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: lsp_types::Range {
            start: lsp_types::Position { line, character: 0 },
            end: lsp_types::Position {
                line,
                character: end_character,
            },
        },
        severity: Some(severity),
        message: message.to_string(),
        code: code.map(|c| lsp_types::NumberOrString::String(c.to_string())),
        source: None,
        code_description: None,
        related_information: None,
        tags: None,
        data: None,
    }
}

pub(super) fn diag_info(diagnostics: Vec<lsp_types::Diagnostic>) -> DiagnosticInfo {
    DiagnosticInfo {
        uri: "file:///test.rs".parse().unwrap(),
        version: Some(1),
        diagnostics,
    }
}

pub struct FakeServer {
    _write_half: Child,
    _read_half: Child,
    pub(crate) read_half_stdin: ChildStdin,
    pub(crate) write_stdout: ChildStdout,
}

fn fake_lsp_transport() -> (LspTransport, FakeServer) {
    let mut write_half = Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let write_stdin = write_half.stdin.take().unwrap();
    let write_stdout = write_half.stdout.take().unwrap();

    let mut read_half = Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let read_stdout = read_half.stdout.take().unwrap();
    let read_stdin = read_half.stdin.take().unwrap();

    (
        LspTransport::new(write_stdin, read_stdout),
        FakeServer {
            _write_half: write_half,
            _read_half: read_half,
            read_half_stdin: read_stdin,
            write_stdout,
        },
    )
}

pub(super) fn fake_lsp_client() -> (LspClient, FakeServer) {
    let (transport, server) = fake_lsp_transport();
    (
        LspClient::from_transport(LspServerConfig::rust_analyzer(), transport),
        server,
    )
}

impl FakeServer {
    pub(crate) fn with_notifications(
        server: ServerId,
    ) -> (LspClient, Self, mpsc::Receiver<crate::lsp::LspNotification>) {
        let (transport, fake) = fake_lsp_transport();
        let (notification_tx, notification_rx) = mpsc::channel(100);
        let client = LspClient::from_transport_with_notifications(
            LspServerConfig::rust_analyzer(),
            transport,
            notification_tx,
            None,
            server,
        );
        (client, fake, notification_rx)
    }
}

/// Reads framed messages until one carries an `id`, discarding the
/// notifications mcpls interleaves with its replies -- an apply emits a
/// `textDocument/didClose` for every file it rewrote before the request
/// that triggered it is answered.
pub async fn read_framed_reply<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> JsonValue {
    loop {
        let message = read_framed_message(reader).await;
        if !message["id"].is_null() {
            return message;
        }
    }
}

/// Writes `message` behind its `Content-Length` header, the framing every
/// LSP message on the wire uses.
async fn write_frame(stdin: &mut ChildStdin, message: &JsonValue) {
    let content = serde_json::to_string(message).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", content.len());
    stdin.write_all(header.as_bytes()).await.unwrap();
    stdin.write_all(content.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
}

/// Writes a framed JSON-RPC success response, as a real LSP server would.
pub async fn write_response(stdin: &mut ChildStdin, id: &JsonValue, result: JsonValue) {
    write_frame(
        stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
    )
    .await;
}

/// Writes a framed JSON-RPC request from the server to the client, as a
/// server does when it answers a command with `workspace/applyEdit`.
pub(super) async fn write_request(
    stdin: &mut ChildStdin,
    id: &JsonValue,
    method: &str,
    params: JsonValue,
) {
    write_frame(
        stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }),
    )
    .await;
}

/// Writes a framed JSON-RPC error response, e.g. to simulate a push-only
/// server answering `textDocument/diagnostic` with method-not-found.
pub(super) async fn write_error_response(
    stdin: &mut ChildStdin,
    id: &JsonValue,
    code: i64,
    message: &str,
) {
    write_frame(
        stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message,
            },
        }),
    )
    .await;
}

/// Builds a single-server translator routed to `server_id` for every tool,
/// with a registered `LspServer` fixture carrying `capabilities` (default
/// capabilities advertise nothing).
pub fn translator_with_capabilities(
    dir: &TempDir,
    server_id: &ServerId,
    capabilities: lsp_types::ServerCapabilities,
) -> (Translator, FakeServer) {
    let mut extensions = HashMap::new();
    extensions.insert("rs".to_string(), "rust".to_string());

    let mut translator =
        Translator::new()
            .with_extensions(extensions)
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                "rust".to_string(),
            )]));
    translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

    let (client, server) = fake_lsp_client();
    translator.register_client(server_id.clone(), client);
    translator.register_server(server_id.clone(), LspServer::new_for_test(capabilities));

    (translator, server)
}

/// As [`translator_with_capabilities`], but with a caller-chosen
/// negotiated `position_encoding` -- for tests exercising a non-UTF-16
/// `EncodingCtx` conversion path through a full mocked LSP round trip.
pub(super) fn translator_with_capabilities_and_encoding(
    dir: &TempDir,
    server_id: &ServerId,
    capabilities: lsp_types::ServerCapabilities,
    position_encoding: lsp_types::PositionEncodingKind,
) -> (Translator, FakeServer) {
    let mut extensions = HashMap::new();
    extensions.insert("rs".to_string(), "rust".to_string());

    let mut translator =
        Translator::new()
            .with_extensions(extensions)
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                "rust".to_string(),
            )]));
    translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

    let (client, server) = fake_lsp_client();
    translator.register_client(server_id.clone(), client);
    translator.register_server(
        server_id.clone(),
        LspServer::new_for_test_with_encoding(capabilities, position_encoding),
    );

    (translator, server)
}

/// The notification method [`RecordingServer::notifications`] sends through
/// its own client to find out what has actually reached the log so far.
///
/// Never sent by production code (no real LSP method starts this way), and
/// never recorded into the log itself -- the reader task intercepts it.
const SENTINEL_METHOD: &str = "mcpls/harness-sentinel";

/// Reads one `Content-Length`-framed JSON-RPC message off `reader`, or
/// `None` once the stream closes.
///
/// Unlike [`read_framed_message`], EOF is an expected outcome here rather
/// than a bug to panic on: a [`RecordingServer`] shut down mid-read looks
/// exactly like this to its reader task.
async fn try_read_frame<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Option<JsonValue> {
    let mut headers = HashMap::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((key, value)) = line.trim_end().split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length")?.parse().ok()?;
    let mut content = vec![0u8; len];
    reader.read_exact(&mut content).await.ok()?;
    serde_json::from_slice(&content).ok()
}

/// A fake LSP server recording the notifications it receives, run on its own
/// OS thread and Tokio runtime so its background message loop makes
/// progress independently of the test's own runtime.
///
/// [`LspClient::notify`] only enqueues onto that client's own background
/// message loop; nothing about a successful `notify().await` waits for the
/// wire write to happen, and there is no `.await` between a drain call and
/// a plain, synchronous `notifications_for` check for the two runtimes to
/// interleave on. `notifications_for` closes that gap deterministically
/// instead of guessing at a delay: it sends a [`SENTINEL_METHOD`]
/// notification through this same client, whose channel to the transport
/// preserves order, and reads back everything logged before the sentinel is
/// observed -- which, by that ordering, is exactly what was sent before it.
struct RecordingServer {
    log: Arc<StdMutex<Vec<Received>>>,
    sentinel_waiter: SentinelWaiter,
    client: LspClient,
    /// The one client returned by [`fake_lsp_client`] that owns the
    /// connection's background task (every other handle, including
    /// `client` above and the one registered with the `Translator`, is a
    /// `Clone` that shares the same channel but not that ownership). Held
    /// here, separate from `client`, so [`Self::kill`] can consume it and
    /// await the task's own exit -- the only way to know the connection is
    /// dead for certain rather than probably.
    owning_client: StdMutex<Option<LspClient>>,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RecordingServer {
    /// Start the background thread/runtime and its fake server, returning
    /// the client half for the harness to register.
    fn spawn(respond_to_diagnostics: bool) -> (LspClient, Self) {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let sentinel_waiter: SentinelWaiter = Arc::new(StdMutex::new(None));
        let (client_tx, client_rx) = std::sync::mpsc::channel::<LspClient>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let log_for_thread = Arc::clone(&log);
        let waiter_for_thread = Arc::clone(&sentinel_waiter);
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("build the fake-server runtime");
            rt.block_on(async move {
                let (client, server) = fake_lsp_client();
                client_tx.send(client).expect("harness awaiting the client");
                tokio::spawn(async move {
                    // Names the whole struct, not just `write_stdout`, so
                    // Rust's disjoint closure capture moves all of `server`
                    // in here -- the other fields exist only to keep the
                    // fake server's processes alive via `kill_on_drop`, and
                    // a capture of `write_stdout` alone would otherwise
                    // drop (and kill) the rest the moment this async block
                    // is constructed.
                    let mut server = server;
                    let FakeServer {
                        read_half_stdin,
                        write_stdout,
                        ..
                    } = &mut server;
                    let mut wire = BufReader::new(write_stdout);
                    while let Some(message) = try_read_frame(&mut wire).await {
                        let method = message["method"].as_str().unwrap_or_default();
                        if method == SENTINEL_METHOD {
                            let prefix = lock_std(&log_for_thread).clone();
                            let waiter = lock_std(&waiter_for_thread).take();
                            if let Some(waiter) = waiter {
                                let _ = waiter.send(prefix);
                            }
                            continue;
                        }
                        lock_std(&log_for_thread)
                            .push((method.to_string(), message["params"].clone()));
                        if respond_to_diagnostics && method == "textDocument/diagnostic" {
                            write_response(
                                &mut *read_half_stdin,
                                &message["id"],
                                serde_json::json!({"kind": "full", "items": []}),
                            )
                            .await;
                        }
                    }
                });
                let _ = shutdown_rx.await;
            });
        });

        let owning_client = client_rx
            .recv()
            .expect("the fake-server thread to hand back its client");
        let sentinel_client = owning_client.clone();
        let registered_client = owning_client.clone();
        (
            registered_client,
            Self {
                log,
                sentinel_waiter,
                client: sentinel_client,
                owning_client: StdMutex::new(Some(owning_client)),
                shutdown: Some(shutdown_tx),
                thread: Some(thread),
            },
        )
    }

    /// Kill this server's connection for good: every notification any
    /// handle to it sends from now on fails.
    ///
    /// Consumes the one client that owns the connection's background task
    /// and awaits that task's exit, so by the time this returns the
    /// connection is confirmed dead rather than merely asked to die --
    /// nothing racing a notify against this call can observe the notify
    /// still succeeding.
    async fn kill(&self) {
        let owning = lock_std(&self.owning_client)
            .take()
            .expect("kill called twice on the same RecordingServer");
        let _ = owning.shutdown().await;
    }

    /// The notification methods received so far, in order.
    fn notifications(&self) -> Vec<String> {
        self.received()
            .into_iter()
            .map(|(method, _)| method)
            .collect()
    }

    /// The params of every notification received for `method`, in order.
    fn params_for(&self, method: &str) -> Vec<JsonValue> {
        self.received()
            .into_iter()
            .filter(|(received, _)| received == method)
            .map(|(_, params)| params)
            .collect()
    }

    /// Every notification received so far, method and params, in order.
    ///
    /// Deterministic rather than timing-based: everything the resync sent
    /// travels through the same client's command channel, in order, ahead
    /// of the sentinel this sends after it, so by the time the sentinel is
    /// observed every earlier notification has already been written and
    /// logged. The 500ms bound only guards against a genuinely hung
    /// transport; it is not part of the normal completion path.
    fn received(&self) -> Vec<Received> {
        let (tx, rx) = std::sync::mpsc::channel();
        *lock_std(&self.sentinel_waiter) = Some(tx);
        let _ = futures::executor::block_on(
            self.client.notify(SENTINEL_METHOD, serde_json::Value::Null),
        );
        rx.recv_timeout(Duration::from_millis(500))
            .expect("the fake server never answered the sentinel")
    }

    fn clear(&self) {
        lock_std(&self.log).clear();
    }
}

impl Drop for RecordingServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A `Translator` with one fake LSP server, a temp workspace, and a record
/// of every notification the fake server received.
pub struct TranslatorHarness {
    /// The translator under test, shared so a test can call it directly.
    pub translator: Arc<Translator>,
    dir: TempDir,
    servers: HashMap<String, RecordingServer>,
    /// The same registry the translator holds, so a test can register a
    /// watcher the way an inbound `client/registerCapability` would.
    watch_registry: Arc<WatchRegistry>,
}

impl TranslatorHarness {
    /// The file extension a fake server of this language answers for.
    ///
    /// A fixed table rather than the real routing config: the harness
    /// exists to drive the resync, not to re-test extension routing, and a
    /// test naming a language with no entry here has almost certainly
    /// misspelled it.
    fn extension_for(language_id: &str) -> &'static str {
        match language_id {
            "rust" => "rs",
            "go" => "go",
            "python" => "py",
            other => panic!("TranslatorHarness has no extension mapped for {other}"),
        }
    }

    /// A harness with one registered server under `language_id`.
    ///
    /// Not itself `async fn`: construction does no awaiting, but every call
    /// site awaits it anyway for symmetry with the rest of the harness's
    /// async surface, so this returns an already-ready future instead of
    /// forcing callers to special-case it.
    pub fn with_one_server(language_id: &str) -> impl Future<Output = Self> {
        Self::with_one_server_and_limits(language_id, ResourceLimits::default())
    }

    /// As [`Self::with_one_server`], with a caller-chosen [`ResourceLimits`]
    /// -- for tests that need a resync's disk read to fail on purpose (e.g.
    /// a `max_file_size` a rewrite exceeds).
    pub fn with_one_server_and_limits(
        language_id: &str,
        limits: ResourceLimits,
    ) -> impl Future<Output = Self> {
        Self::with_server_options(language_id, limits, false)
    }

    /// A harness with one server that answers pull diagnostics with an empty full report.
    pub fn with_diagnostics_server_and_limits(
        language_id: &str,
        limits: ResourceLimits,
    ) -> impl Future<Output = Self> {
        Self::with_server_options(language_id, limits, true)
    }

    fn with_server_options(
        language_id: &str,
        limits: ResourceLimits,
        respond_to_diagnostics: bool,
    ) -> impl Future<Output = Self> {
        let dir = TempDir::new().expect("temp dir");
        let server_id = ServerId::from(language_id);
        let watch_registry = Arc::new(WatchRegistry::new());

        let mut translator = Translator::new()
            .with_extensions(HashMap::from([(
                Self::extension_for(language_id).to_string(),
                language_id.to_string(),
            )]))
            .with_resource_limits(limits)
            .with_watch_registry(Arc::clone(&watch_registry))
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                language_id.to_string(),
            )]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let (client, server) = RecordingServer::spawn(respond_to_diagnostics);
        translator.register_client(server_id, client);

        std::future::ready(Self {
            translator: Arc::new(translator),
            dir,
            servers: HashMap::from([(language_id.to_string(), server)]),
            watch_registry,
        })
    }

    /// The temp workspace every path this harness hands out lives under,
    /// for a caller that has to configure something else against the same
    /// roots the translator was given.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Write `contents` to `relative` under the temp workspace and return
    /// the absolute path.
    pub fn write_file(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.dir.path().join(relative);
        std::fs::write(&path, contents).expect("write the fixture");
        path
    }

    /// Overwrite an existing file, the way an apply does.
    #[allow(clippy::unused_self)]
    pub(super) fn rewrite_file(&self, path: &Path, contents: &str) {
        std::fs::write(path, contents).expect("rewrite the fixture");
    }

    /// Open `path` for `server` through `DocumentTracker::ensure_open`.
    ///
    /// Clears `server`'s recorded notifications afterward: opening sends its
    /// own `didOpen`, which is setup for a test, not the behavior under
    /// test.
    pub async fn open(&self, path: &Path, server: &str) {
        let server_id = ServerId::from(server);
        let client = lock_std(&self.translator.lsp_clients)
            .get(&server_id)
            .cloned()
            .unwrap_or_else(|| panic!("{server} is not registered with this harness"));
        self.translator
            .document_tracker()
            .ensure_open(path, &server_id, &client)
            .await
            .expect("ensure_open");
        // Wait for the didOpen to actually land before clearing: otherwise
        // a slow write could land after this clear and be mistaken for a
        // notification the resync itself sent.
        self.notifications_for(server);
        self.servers
            .get(server)
            .unwrap_or_else(|| panic!("{server} is not registered with this harness"))
            .clear();
    }

    /// Put `path` on the translator's invalidation queue.
    pub(super) fn queue_invalidation(&self, path: &Path) {
        self.translator.queue_invalidations(&[path.to_path_buf()]);
    }

    /// The LSP method names `server` received, in order.
    pub fn notifications_for(&self, server: &str) -> Vec<String> {
        self.servers
            .get(server)
            .unwrap_or_else(|| panic!("{server} is not registered with this harness"))
            .notifications()
    }

    /// Register `glob` for `server` under `id`, on the registry the harness
    /// handed the translator, for every change kind.
    pub(crate) fn register_watcher(&self, server: &str, id: &str, glob: &str) {
        self.register_watchers(server, id, &serde_json::json!([{ "globPattern": glob }]));
    }

    /// Register `watchers` -- a `registerOptions.watchers` array, the shape
    /// an inbound `client/registerCapability` carries -- for `server` under
    /// `id`, for a test that needs a `kind` mask the glob-only form cannot
    /// express.
    pub(crate) fn register_watchers(&self, server: &str, id: &str, watchers: &JsonValue) {
        self.watch_registry
            .register(&ServerId::from(server), id, watchers);
    }

    /// The registry the harness handed the translator, for a caller that
    /// has to build something else -- a `PathFilter` -- against the same
    /// registrations the translator answers from.
    pub(crate) fn watch_registry(&self) -> Arc<WatchRegistry> {
        Arc::clone(&self.watch_registry)
    }

    /// The JSON params of every `workspace/didChangeWatchedFiles` the fake
    /// server for `server` received, in order.
    ///
    /// How many arrived is its own fact: a server told twice under the same
    /// kind is indistinguishable from one told once by anything that reads
    /// only the last of them.
    pub(crate) fn watched_files_params(&self, server: &str) -> Vec<JsonValue> {
        self.servers
            .get(server)
            .unwrap_or_else(|| panic!("{server} is not registered with this harness"))
            .params_for("workspace/didChangeWatchedFiles")
    }

    /// Kill `server`'s connection for good, so every notification the
    /// translator sends it from now on fails.
    ///
    /// Kills the connection up front, before anything is sent, rather than
    /// reacting to a chosen frame mid-drain: the latter is a race between
    /// this fake server's own cross-thread round trip and the translator's
    /// next, purely synchronous enqueue, and the translator always wins it.
    /// Starting dead sidesteps the race instead of trying to win it.
    pub(super) async fn kill_transport(&self, server: &str) {
        self.servers
            .get(server)
            .unwrap_or_else(|| panic!("{server} is not registered with this harness"))
            .kill()
            .await;
    }
}
