//! Shared test fixtures for the `translator` module's sibling `tests`
//! submodules: an `EncodingCtx` builder, a fake in-process LSP server driven
//! over `cat` pipes, JSON-RPC framing helpers, and `TranslatorHarness`, which
//! drives that fake server on its own OS thread for tests that inspect the
//! notifications it received.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

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
use crate::lsp::{LspClient, LspServer, LspTransport};
pub(super) use crate::test_support::read_framed_message;

type JsonValue = serde_json::Value;

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

pub(super) struct FakeServer {
    /// Kept alive so the pipes `write_stdout` reads from stay open; killed
    /// directly by [`RecordingServer`] to simulate a transport that starts
    /// rejecting sends.
    write_half: Child,
    _read_half: Child,
    pub(super) read_half_stdin: ChildStdin,
    pub(super) write_stdout: ChildStdout,
}

pub(super) fn fake_lsp_client() -> (LspClient, FakeServer) {
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

    let transport = LspTransport::new(write_stdin, read_stdout);
    let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);

    (
        client,
        FakeServer {
            write_half,
            _read_half: read_half,
            read_half_stdin: read_stdin,
            write_stdout,
        },
    )
}

/// Reads framed messages until one carries an `id`, discarding the
/// notifications mcpls interleaves with its replies -- an apply emits a
/// `textDocument/didClose` for every file it rewrote before the request
/// that triggered it is answered.
pub(super) async fn read_framed_reply<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> JsonValue {
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
pub(super) async fn write_response(stdin: &mut ChildStdin, id: &JsonValue, result: JsonValue) {
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
pub(super) fn translator_with_capabilities(
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

/// One command [`RecordingServer`] accepts from [`TranslatorHarness`], run on
/// the recording server's own background runtime.
enum ServerCommand {
    /// Reject every notification after the `usize`-th one received so far.
    FailAfter(usize),
    /// Undo a previous `FailAfter`: swap in a fresh fake client/transport
    /// pair and hand it back so the harness can re-register it.
    Allow,
}

/// The live fake-server pair a [`RecordingServer`]'s background runtime
/// currently has installed: the client half the harness hands to the
/// `Translator`, and the notification-count threshold armed by
/// `ServerCommand::FailAfter` for the matching transport half.
struct LiveServer {
    client: LspClient,
    fail_after: Arc<AtomicUsize>,
}

/// Reads one `Content-Length`-framed JSON-RPC message off `reader`, or
/// `None` once the stream closes.
///
/// Unlike [`read_framed_message`], EOF is an expected outcome here rather
/// than a bug to panic on: [`RecordingServer`]'s reader loop keeps running
/// after a deliberate `ServerCommand::FailAfter` kill closes the pipe out
/// from under it.
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

/// Spawns a [`fake_lsp_client`] pair and a reader task that appends every
/// notification method it sees to `log`, killing the fake server's write
/// half once `fail_after`'s count is reached.
///
/// Must run on a runtime the caller has already entered: the constructed
/// `LspClient`'s own background message loop, and the reader task this
/// spawns, both need somewhere to be driven.
fn spawn_live_server(log: &Arc<StdMutex<Vec<String>>>) -> LiveServer {
    let (client, server) = fake_lsp_client();
    let fail_after = Arc::new(AtomicUsize::new(usize::MAX));
    let threshold = Arc::clone(&fail_after);
    let log = Arc::clone(log);
    tokio::spawn(async move {
        // Names the whole struct, not just `write_stdout`, so Rust's
        // disjoint closure capture moves all of `server` in here -- the
        // other fields exist only to keep the fake server's processes
        // alive via `kill_on_drop`, and a capture of `write_stdout` alone
        // would otherwise drop (and kill) the rest the moment this
        // function returns.
        let mut server = server;
        let mut wire = BufReader::new(&mut server.write_stdout);
        while let Some(message) = try_read_frame(&mut wire).await {
            let method = message["method"].as_str().unwrap_or_default().to_string();
            let mut log = lock_std(&log);
            log.push(method);
            if log.len() >= threshold.load(Ordering::SeqCst) {
                let _ = server.write_half.start_kill();
            }
        }
    });
    LiveServer { client, fail_after }
}

/// A fake LSP server recording the notifications it receives, run on its own
/// OS thread and Tokio runtime.
///
/// [`LspClient::notify`] only enqueues onto that client's own background
/// message loop; nothing about a successful `notify().await` waits for the
/// wire write to happen. On the test's own single-threaded runtime, that
/// background task never gets a turn to run before a plain, synchronous
/// `notifications_for` call returns -- there is no `.await` between them for
/// the scheduler to interleave on. Running the fake server on a separate
/// thread with its own runtime gives it real, OS-scheduled concurrency
/// instead, so by the time a test calls `notifications_for` the write has
/// actually landed.
struct RecordingServer {
    log: Arc<StdMutex<Vec<String>>>,
    commands: mpsc::UnboundedSender<ServerCommand>,
    // `mpsc::Receiver` is `Send` but not `Sync`; wrapped so `RecordingServer`
    // (and the `TranslatorHarness` holding it) stays `Sync`, which an
    // `async fn` taking `&self` needs its returned future to be.
    fresh_clients: StdMutex<std::sync::mpsc::Receiver<LspClient>>,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RecordingServer {
    /// Start the background thread/runtime and its first fake server,
    /// returning the client half for the harness to register.
    fn spawn() -> (LspClient, Self) {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (initial_tx, initial_rx) = std::sync::mpsc::channel::<LspClient>();
        let (fresh_tx, fresh_rx) = std::sync::mpsc::channel::<LspClient>();
        let (command_tx, mut command_rx) = mpsc::unbounded_channel::<ServerCommand>();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let log_for_thread = Arc::clone(&log);
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("build the fake-server runtime");
            rt.block_on(async move {
                let mut current = spawn_live_server(&log_for_thread);
                initial_tx
                    .send(current.client.clone())
                    .expect("harness awaiting the initial client");
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => break,
                        Some(command) = command_rx.recv() => match command {
                            ServerCommand::FailAfter(n) => {
                                current.fail_after.store(n, Ordering::SeqCst);
                            }
                            ServerCommand::Allow => {
                                current = spawn_live_server(&log_for_thread);
                                let _ = fresh_tx.send(current.client.clone());
                            }
                        },
                    }
                }
            });
        });

        let client = initial_rx
            .recv()
            .expect("the fake-server thread to hand back its first client");
        (
            client,
            Self {
                log,
                commands: command_tx,
                fresh_clients: StdMutex::new(fresh_rx),
                shutdown: Some(shutdown_tx),
                thread: Some(thread),
            },
        )
    }

    /// The notification methods received so far, in order.
    ///
    /// Waits for the log to stop growing for a short quiet window rather
    /// than reading it immediately: the fake server's runtime is a separate
    /// OS thread, so a notification `.await`ed moments ago may not have
    /// reached the log yet. Bounded overall so a genuinely missing
    /// notification still fails promptly instead of hanging the test.
    fn notifications(&self) -> Vec<String> {
        const QUIET_WINDOW: Duration = Duration::from_millis(15);
        const MAX_WAIT: Duration = Duration::from_millis(500);

        let deadline = Instant::now() + MAX_WAIT;
        let mut last_len = usize::MAX;
        let mut stable_since = Instant::now();
        loop {
            let current = lock_std(&self.log).clone();
            let now = Instant::now();
            if current.len() == last_len {
                if now.duration_since(stable_since) >= QUIET_WINDOW || now >= deadline {
                    return current;
                }
            } else {
                last_len = current.len();
                stable_since = now;
            }
            if now >= deadline {
                return current;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn clear(&self) {
        lock_std(&self.log).clear();
    }

    fn fail_after(&self, n: usize) {
        self.commands
            .send(ServerCommand::FailAfter(n))
            .expect("the fake-server thread is still running");
    }

    fn allow(&self) -> LspClient {
        self.commands
            .send(ServerCommand::Allow)
            .expect("the fake-server thread is still running");
        lock_std(&self.fresh_clients)
            .recv()
            .expect("the fake-server thread to hand back a fresh client")
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
pub(super) struct TranslatorHarness {
    /// The translator under test, shared so a test can call it directly.
    pub(super) translator: Arc<Translator>,
    dir: TempDir,
    servers: HashMap<String, RecordingServer>,
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
    pub(super) fn with_one_server(language_id: &str) -> impl Future<Output = Self> {
        let dir = TempDir::new().expect("temp dir");
        let server_id = ServerId::from(language_id);

        let mut translator = Translator::new()
            .with_extensions(HashMap::from([(
                Self::extension_for(language_id).to_string(),
                language_id.to_string(),
            )]))
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                language_id.to_string(),
            )]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let (client, server) = RecordingServer::spawn();
        translator.register_client(server_id, client);

        std::future::ready(Self {
            translator: Arc::new(translator),
            dir,
            servers: HashMap::from([(language_id.to_string(), server)]),
        })
    }

    /// Write `contents` to `relative` under the temp workspace and return
    /// the absolute path.
    pub(super) fn write_file(&self, relative: &str, contents: &str) -> PathBuf {
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
    pub(super) async fn open(&self, path: &Path, server: &str) {
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
    pub(super) fn notifications_for(&self, server: &str) -> Vec<String> {
        self.servers
            .get(server)
            .unwrap_or_else(|| panic!("{server} is not registered with this harness"))
            .notifications()
    }

    /// Forget everything recorded so far.
    pub(super) fn clear_notifications(&self) {
        for server in self.servers.values() {
            server.clear();
        }
    }

    /// Make `server`'s transport reject every send after the first `n`.
    pub(super) fn fail_notifications_after(&self, server: &str, n: usize) {
        self.servers
            .get(server)
            .unwrap_or_else(|| panic!("{server} is not registered with this harness"))
            .fail_after(n);
    }

    /// Undo `fail_notifications_after`.
    pub(super) fn allow_notifications(&self, server: &str) {
        let server_id = ServerId::from(server);
        let fresh = self
            .servers
            .get(server)
            .unwrap_or_else(|| panic!("{server} is not registered with this harness"))
            .allow();
        self.translator.register_client(server_id, fresh);
    }
}
