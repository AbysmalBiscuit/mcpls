//! Dispatching one Claude Code hook invocation.
//!
//! A hook registration spawns `mcpls hook`, writes one JSON payload to its
//! stdin, and reads one JSON payload back from its stdout. Routing on the
//! payload's own `hook_event_name` here, in one binary, means there is no
//! shell script translating five hook names into five subcommands, and the
//! same registrations work unmodified on Windows.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use mcpls_core::hooks::{
    ChangeEvent, Request, Response, SocketIdentity, send, send_many, watch_paths,
};
use serde::Deserialize;

/// How long a hook waits for an answer to `Changed` or `EndSession`, which
/// carry no context back and are never worth stalling an edit for.
const SOCKET_TIMEOUT: Duration = Duration::from_millis(50);

/// How long a hook waits for an answer that can carry `additionalContext`:
/// `UserPromptSubmit`'s `Flush`, and the `Flush` half of a `PostToolBatch`.
/// Tracks the spec's `op_deadline_ms` default of 1500: a client timeout
/// tighter than the server's own deadline for finishing that work is never
/// right, since it only cuts off answers from an owner that is going to
/// reply anyway. A missing owner still fails fast, because the connect
/// itself fails immediately rather than waiting out this bound.
const FLUSH_SOCKET_TIMEOUT: Duration = Duration::from_millis(1500);

/// The hook payload Claude Code writes to stdin, keeping only the fields
/// the dispatch table below reads. Every field is optional or defaulted,
/// since which ones are present depends on `hook_event_name`.
#[derive(Debug, Deserialize)]
struct HookPayload {
    hook_event_name: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    file_path: Option<PathBuf>,
    #[serde(default)]
    event: Option<ChangeEvent>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

/// One entry of `PostToolBatch`'s `tool_calls`, keeping only the field its
/// `changed` request needs.
#[derive(Debug, Deserialize)]
struct ToolCall {
    tool_input: ToolInput,
}

#[derive(Debug, Deserialize)]
struct ToolInput {
    #[serde(default)]
    file_path: Option<PathBuf>,
}

/// Await `body`, and swallow whatever it does wrong.
///
/// An edit must never fail because diagnostics were unavailable, so there
/// is exactly one exit code and it is zero. The cost is that a broken
/// installation is invisible, which is what `mcpls hook doctor` exists to
/// answer. This covers an `Err` from `body`; nothing on this path panics
/// today, so there is no `catch_unwind` here, but a caller relying on this
/// wrapper for panic safety would be relying on something it does not do.
///
/// Takes a future rather than a closure: everything it wraps goes through
/// `hooks::send`, which is `async`, and a synchronous `FnOnce` could not
/// contain the await, so the one-wrapper guarantee would not hold.
async fn silently<T: Default>(body: impl Future<Output = Result<T>>) -> T {
    body.await.unwrap_or_default()
}

/// Read one hook payload from `stdin` and return the hook JSON to print,
/// dispatching on the payload's own `hook_event_name` against the socket
/// `identity` names.
///
/// `identity` is `None` when the caller could not derive one (an
/// unreachable or over-long project directory): `SessionStart` still
/// answers, since it never touches the socket, and every other arm
/// degrades to the empty string exactly as it would against an
/// unreachable socket.
///
/// Every fault, a missing identity, a missing socket, a connect timeout, a
/// malformed response, a payload that will not parse, produces the empty
/// string.
pub async fn dispatch_payload(
    stdin: &str,
    project_dir: &Path,
    identity: Option<&SocketIdentity>,
) -> String {
    silently(run(stdin, project_dir, identity)).await
}

async fn run(stdin: &str, project_dir: &Path, identity: Option<&SocketIdentity>) -> Result<String> {
    let payload: HookPayload = serde_json::from_str(stdin)?;

    match payload.hook_event_name.as_str() {
        // `SessionStart` fires while the host is still spawning the MCP
        // server, so the socket may not be bound yet. Computing the watch
        // set locally, rather than asking over the socket, is what keeps
        // this hook from either leaving the session with no coverage or an
        // unbounded watcher over target/. It never touches `identity`.
        "SessionStart" => Ok(session_start_output(&watch_paths(project_dir))),

        "FileChanged" => {
            let Some(identity) = identity else {
                return Ok(String::new());
            };
            let Some(file_path) = payload.file_path else {
                return Ok(String::new());
            };
            let event = payload.event.unwrap_or(ChangeEvent::Change);
            send(
                identity,
                &Request::Changed {
                    session: payload.session_id,
                    paths: vec![file_path],
                    event,
                },
                SOCKET_TIMEOUT,
            )
            .await?;
            Ok(String::new())
        }

        "PostToolBatch" => {
            let Some(identity) = identity else {
                return Ok(String::new());
            };
            let paths = payload
                .tool_calls
                .into_iter()
                .filter_map(|call| call.tool_input.file_path)
                .collect();
            let requests = [
                Request::Changed {
                    session: payload.session_id.clone(),
                    paths,
                    event: ChangeEvent::Change,
                },
                Request::Flush {
                    session: payload.session_id,
                },
            ];
            let responses = send_many(identity, &requests, FLUSH_SOCKET_TIMEOUT).await?;
            let context = responses.into_iter().nth(1).and_then(flush_context);
            Ok(additional_context_output(context))
        }

        "UserPromptSubmit" => {
            let Some(identity) = identity else {
                return Ok(String::new());
            };
            let response = send(
                identity,
                &Request::Flush {
                    session: payload.session_id,
                },
                FLUSH_SOCKET_TIMEOUT,
            )
            .await?;
            Ok(additional_context_output(flush_context(response)))
        }

        "SessionEnd" => {
            let Some(identity) = identity else {
                return Ok(String::new());
            };
            send(
                identity,
                &Request::EndSession {
                    session: payload.session_id,
                },
                SOCKET_TIMEOUT,
            )
            .await?;
            Ok(String::new())
        }

        // `Stop` is deliberately absent. Its `additionalContext` is
        // documented as non-error feedback after which the conversation
        // continues so the model can act on it, so flushing there would
        // turn every new warning into a keep-working signal. The next
        // `UserPromptSubmit` delivers the same diagnostics anyway.
        _ => Ok(String::new()),
    }
}

/// The context a `Flush` response carries, or `None` for any other answer.
fn flush_context(response: Response) -> Option<String> {
    if let Response::Flush { context } = response {
        context
    } else {
        None
    }
}

/// The `SessionStart` hook's JSON, naming the directories to watch.
fn session_start_output(paths: &[PathBuf]) -> String {
    let watch_paths: Vec<String> = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    serde_json::json!({ "hookSpecificOutput": { "watchPaths": watch_paths } }).to_string()
}

/// The hook JSON carrying diagnostics context, or the empty string when
/// there is nothing to report.
fn additional_context_output(context: Option<String>) -> String {
    context
        .filter(|text| !text.is_empty())
        .map_or_else(String::new, |text| {
            serde_json::json!({ "hookSpecificOutput": { "additionalContext": text } }).to_string()
        })
}

/// How long the doctor waits for a `Status` answer before concluding
/// nothing owns the socket. Matches the timeout every other socket-using
/// hook arm uses for a request that carries no context back.
const DOCTOR_TIMEOUT: Duration = SOCKET_TIMEOUT;

/// Probe this project's hook socket and describe what it finds: the
/// socket path, the directory hash each side computes, whether an owner
/// answers, and whether `mcpls` resolves on `PATH`.
///
/// Every other command in this feature fails silently by design, so a
/// broken install looks exactly like a quiet workspace. This is the one
/// command allowed to print on failure, because breaking that silence is
/// its entire purpose.
pub async fn doctor(project_dir: &Path, identity: &SocketIdentity) -> String {
    let mut lines = vec![
        format!("socket: {}", identity.socket.display()),
        format!("hook sees: {} -> {}", project_dir.display(), identity.hash),
    ];

    let status = send(identity, &Request::Status, DOCTOR_TIMEOUT)
        .await
        .ok()
        .and_then(|response| {
            if let Response::Status {
                hash, pid, root, ..
            } = response
            {
                Some((hash, pid, root))
            } else {
                None
            }
        });

    lines.push(status.as_ref().map_or_else(
        || "server sees: no owner".to_string(),
        |(hash, _pid, root)| format!("server sees: {} -> {hash}", root.display()),
    ));

    if status
        .as_ref()
        .is_some_and(|(hash, ..)| *hash != identity.hash)
    {
        lines.push("the two do not match; hooks will do nothing until they do".to_string());
    }

    lines.push(status.as_ref().map_or_else(
        || "owner pid: none".to_string(),
        |(_hash, pid, _root)| format!("owner pid: {pid}"),
    ));

    lines.push(mcpls_on_path().map_or_else(
        || "mcpls on PATH: not found".to_string(),
        |path| format!("mcpls on PATH: {}", path.display()),
    ));

    lines.join("\n")
}

/// The doctor's answer when this project's own socket identity cannot be
/// derived at all: an unreachable project directory, or a runtime
/// directory deep enough that the derived socket path exceeds this
/// platform's length limit. A running mcpls that hit the same failure
/// logs a warning and serves no socket rather than aborting startup, so
/// this state is real, not hypothetical.
///
/// Kept distinct from "server sees: no owner": that line means a socket
/// exists and nothing answers it; this means no socket could ever exist
/// here, which a user needs to be able to tell apart from a server that
/// simply is not running right now.
pub fn doctor_without_identity(project_dir: &Path, error: &mcpls_core::Error) -> String {
    let lines = [
        format!("socket: none; could not derive an identity for this directory: {error}"),
        format!("hook sees: {} -> unknown", project_dir.display()),
        "server sees: unknown; no socket exists to probe".to_string(),
        mcpls_on_path().map_or_else(
            || "mcpls on PATH: not found".to_string(),
            |path| format!("mcpls on PATH: {}", path.display()),
        ),
    ];
    lines.join("\n")
}

/// The absolute path to an executable named `mcpls` (`mcpls.exe` on
/// Windows) on the first `PATH` entry that has one, or `None`.
///
/// Hooks invoke `mcpls` by name off `PATH` rather than by an absolute
/// path, so a hook environment missing the install directory makes every
/// hook do nothing, invisibly. Walking `PATH` by hand rather than
/// shelling out to `which`, which is not installed on every host mcpls
/// runs on.
fn mcpls_on_path() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) { "mcpls.exe" } else { "mcpls" };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe_name))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    use super::*;

    /// The canned text a default `RecordingOwner` answers `Flush` with:
    /// multi-line, with a blank-free but indented second line, so a
    /// dispatcher that trims or re-wraps the owner's text before handing it
    /// to `additionalContext` fails the exact-value assertions below rather
    /// than passing on a fixture too flat to notice.
    const DEFAULT_FLUSH_TEXT: &str = "2 problems in a.rs\n  1 warning in b.rs\n";

    /// Run the dispatcher over one payload, against the socket identity
    /// `project_dir` derives, and return what it printed.
    ///
    /// Nothing is listening on that identity unless the test bound one,
    /// which is the point of most of these: the silent-failure rule says an
    /// unreachable socket prints nothing and exits zero.
    async fn dispatch(payload: &serde_json::Value, project_dir: &Path) -> String {
        dispatch_raw(&payload.to_string(), project_dir).await
    }

    /// The same, from raw stdin bytes, so a payload that is not JSON at all
    /// goes through the same path.
    async fn dispatch_raw(stdin: &str, project_dir: &Path) -> String {
        let identity = mcpls_core::hooks::identity_for(project_dir).expect("identity");
        super::dispatch_payload(stdin, project_dir, Some(&identity)).await
    }

    /// Run the dispatcher against a listener that records the requests it
    /// gets.
    async fn dispatch_against(payload: &serde_json::Value, recorder: &RecordingOwner) -> String {
        super::dispatch_payload(
            &payload.to_string(),
            recorder.project_dir(),
            Some(&recorder.identity),
        )
        .await
    }

    /// A `SocketIdentity` whose socket and lock live inside `dir`, built
    /// field by field rather than through `identity_for`, which would put
    /// the socket in the real runtime directory. `hash` and `lock` are
    /// never used by a `RecordingOwner`, which owns its socket directly
    /// rather than through `HookListener`'s lock file.
    fn temp_identity(dir: &Path) -> SocketIdentity {
        let suffix = format!("owner-{}", rand_suffix());
        #[cfg(windows)]
        let socket = PathBuf::from(format!(r"\\.\pipe\mcpls-recording-{suffix}"));
        #[cfg(not(windows))]
        let socket = dir.join(format!("{suffix}.sock"));
        SocketIdentity {
            lock: dir.join(format!("{suffix}.lock")),
            socket,
            hash: suffix,
        }
    }

    /// A per-call suffix, so two tests running in parallel never collide on
    /// a Windows pipe name, which is process-global rather than
    /// directory-scoped.
    fn rand_suffix() -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::thread::current().id().hash(&mut hasher);
        std::time::SystemTime::now().hash(&mut hasher);
        hasher.finish()
    }

    /// The op name for a `Request`, e.g. "changed" or "flush", matching the
    /// wire's own `op` tag.
    fn op_name(request: &Request) -> String {
        match request {
            Request::Changed { .. } => "changed",
            Request::Flush { .. } => "flush",
            Request::EndSession { .. } => "end_session",
            Request::Status => "status",
        }
        .to_string()
    }

    /// How `RecordingOwner` answers requests: the `Flush` context text, how
    /// long to wait before answering one, and whether `Changed` answers an
    /// error instead of queuing. The delay defaults to zero and the error
    /// defaults to off; a test exercising either sets it explicitly, so a
    /// regression that narrows a client's tolerance has something in the
    /// suite that would notice.
    #[derive(Clone, Default)]
    struct OwnerBehavior {
        flush_text: Option<String>,
        flush_delay: Duration,
        changed_errors: bool,
        /// What `Status` reports as its directory hash and startup root.
        /// Empty by default, which is fine for every test that never sends
        /// `Status` against this behavior.
        status_hash: String,
        status_root: PathBuf,
    }

    /// A plausible answer to `request` under `behavior`.
    ///
    /// A real owner answers `Changed` with `Response::Error` once its
    /// sweeper has stopped (ordinarily: shutdown) rather than silently
    /// dropping the path, which `behavior.changed_errors` reproduces here.
    fn answer(request: &Request, behavior: &OwnerBehavior) -> Response {
        match request {
            Request::Changed { paths, .. } => {
                if behavior.changed_errors {
                    Response::Error {
                        message: "mcpls is shutting down; these paths were not queued".to_string(),
                    }
                } else {
                    Response::Changed {
                        queued: paths.len(),
                    }
                }
            }
            Request::Flush { .. } => Response::Flush {
                context: behavior.flush_text.clone(),
            },
            Request::EndSession { .. } => Response::EndSession,
            Request::Status => Response::Status {
                hash: behavior.status_hash.clone(),
                socket: PathBuf::new(),
                pid: std::process::id(),
                owner: true,
                root: behavior.status_root.clone(),
            },
        }
    }

    /// A listener on a temporary socket that records every request it is
    /// sent, in order, and answers each plausibly.
    ///
    /// A hand-rolled newline-delimited JSON accept loop, not
    /// `HookListener`: the protocol's framing is pinned by
    /// `mcpls_core::hooks::protocol`'s own tests, so nothing here depends
    /// on `HookListener`'s specific accept/serve machinery, and speaking
    /// the wire format directly means one socket yields the full `Request`
    /// (paths, session ids, event kinds) and a true connection count from
    /// its own accept counter, rather than needing a second socket in
    /// front of a `HookListener` whose handler cannot see connection
    /// boundaries at all.
    struct RecordingOwner {
        dir: tempfile::TempDir,
        identity: SocketIdentity,
        requests: Arc<Mutex<Vec<Request>>>,
        connections: Arc<AtomicUsize>,
        _cancel: tokio::sync::watch::Sender<bool>,
    }

    impl RecordingOwner {
        /// Bind and start serving, answering every `Flush` with
        /// [`DEFAULT_FLUSH_TEXT`] immediately and every `Changed` with
        /// `Response::Changed`.
        fn start() -> Self {
            Self::start_with_flush(Some(DEFAULT_FLUSH_TEXT.to_string()))
        }

        /// The same, answering every `Flush` with `flush_text` instead.
        fn start_with_flush(flush_text: Option<String>) -> Self {
            Self::start_with(OwnerBehavior {
                flush_text,
                ..OwnerBehavior::default()
            })
        }

        /// The same, waiting `delay` before answering each `Flush`. Used to
        /// prove a client actually waits out an owner slower than an
        /// instant in-process reply, rather than merely using whichever
        /// timeout constant happens to be in scope at its call site.
        fn start_with_flush_delay(flush_text: Option<String>, delay: Duration) -> Self {
            Self::start_with(OwnerBehavior {
                flush_text,
                flush_delay: delay,
                ..OwnerBehavior::default()
            })
        }

        /// The same, answering every `Changed` with `Response::Error`
        /// instead of `Response::Changed`, the shape a real owner sends
        /// once its sweeper has stopped taking new paths (ordinarily:
        /// shutdown). `Flush` still answers `flush_text` normally, so a
        /// `PostToolBatch`'s `changed` erroring must not be allowed to
        /// swallow its own `flush`'s context.
        fn start_with_changed_error(flush_text: Option<String>) -> Self {
            Self::start_with(OwnerBehavior {
                flush_text,
                changed_errors: true,
                ..OwnerBehavior::default()
            })
        }

        /// Not `async`: everything here is a synchronous bind or a plain
        /// `tokio::spawn` call, which needs a runtime running (true of
        /// every `#[tokio::test]` caller) but not an `async` caller.
        fn start_with(behavior: OwnerBehavior) -> Self {
            let dir = tempfile::tempdir().expect("a temp dir");
            let identity = temp_identity(dir.path());
            Self::start_on(dir, identity, behavior)
        }

        /// The same, bound on `identity` rather than one generated fresh.
        ///
        /// Lets a caller start an owner on the exact socket some other,
        /// independently derived identity names, which `mcpls hook
        /// doctor`'s tests need: the socket the doctor connects to has to
        /// be the one it computed for a project directory, not one this
        /// harness invented.
        fn start_on(
            dir: tempfile::TempDir,
            identity: SocketIdentity,
            behavior: OwnerBehavior,
        ) -> Self {
            let requests: Arc<Mutex<Vec<Request>>> = Arc::new(Mutex::new(Vec::new()));
            let connections = Arc::new(AtomicUsize::new(0));
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

            // Bound synchronously, before this function returns: a spawned
            // task's first poll is not guaranteed to happen before the
            // caller's next `send`/`send_many`, and a socket that does not
            // exist yet would refuse that connection outright.
            let listener = bind_owner(&identity.socket);
            tokio::spawn(accept_loop(
                listener,
                identity.socket.clone(),
                Arc::clone(&requests),
                Arc::clone(&connections),
                behavior,
                cancel_rx,
            ));

            Self {
                dir,
                identity,
                requests,
                connections,
                _cancel: cancel_tx,
            }
        }

        /// An owner bound on `identity`, answering `Status` as though its
        /// own startup directory were `root` rather than wherever this
        /// process actually runs.
        ///
        /// Lets `mcpls hook doctor`'s tests put the hook's own hash and the
        /// reported server hash into agreement or disagreement on demand,
        /// without needing a second real directory that a running mcpls
        /// would actually have started in.
        fn start_reporting_root(identity: SocketIdentity, root: &Path) -> Self {
            let hash = mcpls_core::hooks::identity_for(root)
                .expect("identity for the reported root")
                .hash;
            let dir = tempfile::tempdir().expect("a temp dir");
            Self::start_on(
                dir,
                identity,
                OwnerBehavior {
                    status_hash: hash,
                    status_root: root.to_path_buf(),
                    ..OwnerBehavior::default()
                },
            )
        }

        /// The directory the dispatcher treats as `CLAUDE_PROJECT_DIR`.
        fn project_dir(&self) -> &Path {
            self.dir.path()
        }

        /// The requests received, in order, in full.
        fn requests(&self) -> Vec<Request> {
            self.requests.lock().expect("requests lock").clone()
        }

        /// The op names received, in order: "changed", "flush", and so on.
        fn ops(&self) -> Vec<String> {
            self.requests().iter().map(op_name).collect()
        }

        /// How many client connections the socket has accepted.
        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }
    }

    /// Read newline-delimited requests off `stream` until it closes,
    /// recording each and answering it before reading the next. Generic
    /// over the stream type so the same loop body serves both a Unix
    /// socket and a Windows named pipe.
    async fn serve_connection<S>(
        stream: S,
        requests: Arc<Mutex<Vec<Request>>>,
        behavior: OwnerBehavior,
    ) where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut lines = tokio::io::BufReader::new(reader).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(request) = serde_json::from_str::<Request>(&line) else {
                return;
            };
            if matches!(request, Request::Flush { .. }) && !behavior.flush_delay.is_zero() {
                tokio::time::sleep(behavior.flush_delay).await;
            }
            let response = answer(&request, &behavior);
            requests.lock().expect("requests lock").push(request);

            let Ok(mut out) = serde_json::to_string(&response) else {
                return;
            };
            out.push('\n');
            if writer.write_all(out.as_bytes()).await.is_err() {
                return;
            }
            if writer.flush().await.is_err() {
                return;
            }
        }
    }

    /// Bind the owner's socket synchronously.
    #[cfg(not(windows))]
    fn bind_owner(socket: &Path) -> tokio::net::UnixListener {
        tokio::net::UnixListener::bind(socket).expect("bind the recording owner")
    }

    /// Accept every client on `listener`, counting each one and serving it.
    /// `_socket` is unused here: a `UnixListener` never needs to recreate
    /// itself between clients the way a named pipe does, but the parameter
    /// is kept so both platforms share one call site in
    /// `RecordingOwner::start_with`.
    #[cfg(not(windows))]
    async fn accept_loop(
        listener: tokio::net::UnixListener,
        _socket: PathBuf,
        requests: Arc<Mutex<Vec<Request>>>,
        connections: Arc<AtomicUsize>,
        behavior: OwnerBehavior,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                result = cancel.changed() => {
                    if result.is_err() || *cancel.borrow() {
                        return;
                    }
                }
                accepted = listener.accept() => {
                    let Ok((stream, _addr)) = accepted else {
                        return;
                    };
                    connections.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(serve_connection(
                        stream,
                        Arc::clone(&requests),
                        behavior.clone(),
                    ));
                }
            }
        }
    }

    /// Create the owner's first pipe instance synchronously.
    #[cfg(windows)]
    fn bind_owner(socket: &Path) -> tokio::net::windows::named_pipe::NamedPipeServer {
        tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(socket)
            .expect("create the recording owner pipe")
    }

    /// Accept every client on `server`'s pipe, counting each one and
    /// serving it. `socket` is the pipe path `server` was created on, kept
    /// alongside it so each accepted client's replacement instance can be
    /// created on the same name.
    #[cfg(windows)]
    async fn accept_loop(
        mut server: tokio::net::windows::named_pipe::NamedPipeServer,
        socket: PathBuf,
        requests: Arc<Mutex<Vec<Request>>>,
        connections: Arc<AtomicUsize>,
        behavior: OwnerBehavior,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) {
        use tokio::net::windows::named_pipe::ServerOptions;
        loop {
            tokio::select! {
                result = cancel.changed() => {
                    if result.is_err() || *cancel.borrow() {
                        return;
                    }
                }
                connected = server.connect() => {
                    if connected.is_err() {
                        return;
                    }
                    // The replacement instance is created *before* the
                    // connected one is moved out and handed off, mirroring
                    // `crates/mcpls-core/src/hooks/listener.rs`'s
                    // `PipeTransport::accept`: a client arriving between
                    // this instance no longer listening and a replacement
                    // existing would get `ERROR_FILE_NOT_FOUND` rather than
                    // the `ERROR_PIPE_BUSY` a real client's connect loop
                    // retries on.
                    let Ok(next) = ServerOptions::new().create(&socket) else {
                        return;
                    };
                    let current = std::mem::replace(&mut server, next);
                    connections.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(serve_connection(
                        current,
                        Arc::clone(&requests),
                        behavior.clone(),
                    ));
                }
            }
        }
    }

    #[test]
    fn test_the_flush_timeout_tracks_the_op_deadline_default() {
        assert_eq!(
            FLUSH_SOCKET_TIMEOUT,
            Duration::from_millis(1500),
            "a client timeout tighter than the server's own op_deadline_ms \
             default is never right; if the spec's default changes, this \
             constant must change deliberately alongside it"
        );
    }

    /// Well past the old, uniform 50ms bound and well inside
    /// `FLUSH_SOCKET_TIMEOUT`'s 1500ms: a ~15x margin under the timeout
    /// budget and a 2x margin over the bound a flush-bearing op must no
    /// longer be held to, so this is not a flake risk the way a
    /// tight-margin timing test would be.
    const SLOW_FLUSH_DELAY: Duration = Duration::from_millis(100);

    #[tokio::test]
    async fn test_a_missing_identity_still_lets_session_start_answer() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");

        let out = super::dispatch_payload(
            &json!({ "hook_event_name": "SessionStart" }).to_string(),
            dir.path(),
            None,
        )
        .await;

        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert!(
            !parsed["hookSpecificOutput"]["watchPaths"]
                .as_array()
                .expect("watchPaths")
                .is_empty(),
            "SessionStart never touches the socket, so a missing identity \
             (an unreachable or over-long project directory) must not \
             suppress it: {out}"
        );
    }

    #[tokio::test]
    async fn test_a_missing_identity_produces_no_output_for_socket_using_arms() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let out = super::dispatch_payload(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }).to_string(),
            dir.path(),
            None,
        )
        .await;
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn test_user_prompt_submit_waits_out_a_slow_owner_within_the_flush_timeout() {
        let recorder = RecordingOwner::start_with_flush_delay(
            Some(DEFAULT_FLUSH_TEXT.to_string()),
            SLOW_FLUSH_DELAY,
        );
        let out = dispatch_against(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
            &recorder,
        )
        .await;

        assert_eq!(
            out,
            additional_context_output(Some(DEFAULT_FLUSH_TEXT.to_string())),
            "an owner slower than the old 50ms bound but inside \
             FLUSH_SOCKET_TIMEOUT must still be waited out, or raising the \
             timeout had no effect at this call site"
        );
    }

    #[tokio::test]
    async fn test_post_tool_batch_waits_out_a_slow_owner_within_the_flush_timeout() {
        let recorder = RecordingOwner::start_with_flush_delay(
            Some(DEFAULT_FLUSH_TEXT.to_string()),
            SLOW_FLUSH_DELAY,
        );
        let file = recorder.project_dir().join("a.rs");
        std::fs::write(&file, "fn a() {}").expect("write");
        let out = dispatch_against(
            &json!({
                "hook_event_name": "PostToolBatch",
                "session_id": "s1",
                "tool_calls": [{ "tool_input": { "file_path": file.display().to_string() } }]
            }),
            &recorder,
        )
        .await;

        assert_eq!(
            out,
            additional_context_output(Some(DEFAULT_FLUSH_TEXT.to_string())),
            "an owner slower than the old 50ms bound but inside \
             FLUSH_SOCKET_TIMEOUT must still be waited out, or raising the \
             timeout had no effect at this call site"
        );
    }

    #[tokio::test]
    async fn test_session_start_returns_watch_paths_without_a_socket() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");

        let out = dispatch(&json!({ "hook_event_name": "SessionStart" }), dir.path()).await;

        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
        let paths = parsed["hookSpecificOutput"]["watchPaths"]
            .as_array()
            .expect("watchPaths");
        assert!(!paths.is_empty());
        for path in paths {
            let path = path.as_str().expect("a string path");
            assert!(
                Path::new(path).starts_with(dir.path()),
                "SessionStart must watch the project directory it was given, \
                 not some other directory: {path}"
            );
        }
        // No listener is bound in this test at all.
        assert!(
            !out.contains("error"),
            "SessionStart fires while the host is still spawning the MCP server, \
             so asking a socket that is not bound yet would leave the session \
             with either no coverage or an unbounded watcher over target/"
        );
    }

    #[tokio::test]
    async fn test_an_unreachable_socket_produces_no_output_and_exit_zero() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let out = dispatch(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
            dir.path(),
        )
        .await;
        assert_eq!(
            out, "",
            "an edit must never fail because diagnostics were unavailable"
        );
    }

    #[tokio::test]
    async fn test_an_unknown_event_produces_no_output() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let out = dispatch(&json!({ "hook_event_name": "Whatever" }), dir.path()).await;
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn test_a_malformed_payload_produces_no_output() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let out = dispatch_raw("not json at all", dir.path()).await;
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn test_file_changed_sends_a_changed_request_with_the_path_and_event() {
        let recorder = RecordingOwner::start();
        let file = recorder.project_dir().join("a.rs");
        let out = dispatch_against(
            &json!({
                "hook_event_name": "FileChanged",
                "session_id": "s1",
                "file_path": file.display().to_string(),
                "event": "add"
            }),
            &recorder,
        )
        .await;

        assert_eq!(out, "", "FileChanged never injects context");
        let requests = recorder.requests();
        assert_eq!(requests.len(), 1);
        let Request::Changed {
            session,
            paths,
            event,
        } = &requests[0]
        else {
            panic!("expected a changed request: {:?}", requests[0]);
        };
        assert_eq!(session.as_str(), "s1");
        assert_eq!(*paths, vec![file]);
        assert_eq!(*event, ChangeEvent::Add);
    }

    #[tokio::test]
    async fn test_file_changed_without_an_event_defaults_to_change() {
        let recorder = RecordingOwner::start();
        let file = recorder.project_dir().join("a.rs");
        dispatch_against(
            &json!({
                "hook_event_name": "FileChanged",
                "session_id": "s1",
                "file_path": file.display().to_string()
            }),
            &recorder,
        )
        .await;

        let requests = recorder.requests();
        assert_eq!(requests.len(), 1);
        let Request::Changed { event, .. } = &requests[0] else {
            panic!("expected a changed request: {:?}", requests[0]);
        };
        assert_eq!(
            *event,
            ChangeEvent::Change,
            "an event kind the host omits must default to Change, not some \
             other kind that would mislead the sweep's hint"
        );
    }

    #[tokio::test]
    async fn test_file_changed_without_a_file_path_sends_nothing() {
        let recorder = RecordingOwner::start();
        let out = dispatch_against(
            &json!({ "hook_event_name": "FileChanged", "session_id": "s1" }),
            &recorder,
        )
        .await;
        assert_eq!(out, "");
        assert_eq!(
            recorder.ops(),
            Vec::<String>::new(),
            "a FileChanged payload with no file_path must not open a connection at all"
        );
    }

    #[tokio::test]
    async fn test_session_end_sends_an_end_session_request() {
        let recorder = RecordingOwner::start();
        let out = dispatch_against(
            &json!({ "hook_event_name": "SessionEnd", "session_id": "s1" }),
            &recorder,
        )
        .await;
        assert_eq!(out, "");
        let requests = recorder.requests();
        assert_eq!(requests.len(), 1);
        let Request::EndSession { session } = &requests[0] else {
            panic!("expected an end_session request: {:?}", requests[0]);
        };
        assert_eq!(session.as_str(), "s1");
    }

    #[tokio::test]
    async fn test_user_prompt_submit_flushes_and_injects_context() {
        let recorder = RecordingOwner::start();
        let out = dispatch_against(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
            &recorder,
        )
        .await;

        let requests = recorder.requests();
        assert_eq!(requests.len(), 1);
        let Request::Flush { session } = &requests[0] else {
            panic!("expected a flush request: {:?}", requests[0]);
        };
        assert_eq!(session.as_str(), "s1");

        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"],
            json!(DEFAULT_FLUSH_TEXT)
        );
    }

    #[tokio::test]
    async fn test_an_empty_flush_context_injects_nothing() {
        let recorder = RecordingOwner::start_with_flush(Some(String::new()));
        let out = dispatch_against(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
            &recorder,
        )
        .await;
        assert_eq!(
            out, "",
            "an owner reporting an empty flush must not produce an \
             additionalContext key at all"
        );
    }

    #[tokio::test]
    async fn test_stop_sends_nothing() {
        let recorder = RecordingOwner::start();
        let out = dispatch_against(
            &json!({ "hook_event_name": "Stop", "session_id": "s1" }),
            &recorder,
        )
        .await;
        assert_eq!(out, "");
        assert_eq!(
            recorder.ops(),
            Vec::<String>::new(),
            "Stop's additionalContext continues the conversation, so flushing \
             there would turn every new warning into a keep-working signal"
        );
    }

    #[tokio::test]
    async fn test_post_tool_batch_sends_changed_then_flush() {
        let recorder = RecordingOwner::start();
        let file = recorder.project_dir().join("a.rs");
        std::fs::write(&file, "fn a() {}").expect("write");
        let out = dispatch_against(
            &json!({
                "hook_event_name": "PostToolBatch",
                "session_id": "s1",
                "tool_calls": [{ "tool_input": { "file_path": file.display().to_string() } }]
            }),
            &recorder,
        )
        .await;

        let requests = recorder.requests();
        assert_eq!(
            requests.len(),
            2,
            "expected a changed request followed by a flush request: {requests:?}"
        );
        let Request::Changed {
            session: changed_session,
            paths,
            event,
        } = &requests[0]
        else {
            panic!(
                "expected the first request to be changed: {:?}",
                requests[0]
            );
        };
        let Request::Flush {
            session: flush_session,
        } = &requests[1]
        else {
            panic!("expected the second request to be flush: {:?}", requests[1]);
        };
        assert_eq!(changed_session.as_str(), "s1");
        assert_eq!(
            *paths,
            vec![file],
            "the batch's own path must reach the changed request"
        );
        assert_eq!(*event, ChangeEvent::Change);
        assert_eq!(flush_session.as_str(), "s1");

        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"],
            json!(DEFAULT_FLUSH_TEXT),
            "the owner's flush text must reach additionalContext unmodified, \
             not re-rendered, wrapped, or trimmed"
        );
        assert_eq!(
            recorder.connections(),
            1,
            "the spec's protocol says PostToolBatch sends changed then flush on \
             one connection, which is what send_many is for"
        );
    }

    /// A real owner answers `Changed` with `Response::Error` once its
    /// sweeper has stopped taking new paths (ordinarily: shutdown), rather
    /// than silently dropping the path. `PostToolBatch` reads `send_many`'s
    /// responses by position, not by matching the `changed` half's
    /// variant, so an error there must not prevent the `flush` half's
    /// context, arriving over the same connection, from reaching
    /// `additionalContext`.
    #[tokio::test]
    async fn test_a_changed_error_does_not_swallow_the_batch_s_flush() {
        let recorder =
            RecordingOwner::start_with_changed_error(Some(DEFAULT_FLUSH_TEXT.to_string()));
        let file = recorder.project_dir().join("a.rs");
        std::fs::write(&file, "fn a() {}").expect("write");
        let out = dispatch_against(
            &json!({
                "hook_event_name": "PostToolBatch",
                "session_id": "s1",
                "tool_calls": [{ "tool_input": { "file_path": file.display().to_string() } }]
            }),
            &recorder,
        )
        .await;

        assert_eq!(
            out,
            additional_context_output(Some(DEFAULT_FLUSH_TEXT.to_string())),
            "an error answering changed must not swallow a flush answer \
             that arrived on the same connection: {out}"
        );
    }

    /// The identity `identity_for(project)` would derive, with its socket
    /// and lock moved into `dir` in place of the real runtime directory
    /// `identity_for` would otherwise choose.
    ///
    /// Only the hash is carried over from the real function: that is the
    /// value the doctor's "hook sees" line and its mismatch check both
    /// depend on, and binding a listener on a real per-user runtime
    /// directory from a test would leave a stray socket behind on the
    /// host.
    fn local_identity_for(project: &Path, dir: &Path) -> SocketIdentity {
        let hash = mcpls_core::hooks::identity_for(project)
            .expect("identity")
            .hash;
        #[cfg(windows)]
        let socket = PathBuf::from(format!(r"\\.\pipe\mcpls-doctor-{hash}"));
        #[cfg(not(windows))]
        let socket = dir.join(format!("{hash}.sock"));
        SocketIdentity {
            lock: dir.join(format!("{hash}.lock")),
            socket,
            hash,
        }
    }

    /// Run the doctor for `project`, against an owner that reports `root`
    /// as its own startup directory, or against no owner at all.
    ///
    /// The owner is a real listener answering a real `Status`, so what
    /// this exercises is the same probe the installed binary runs.
    async fn doctor_with(project: &Path, owner_root: Option<&Path>) -> String {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project, socket_dir.path());
        let _owner =
            owner_root.map(|root| RecordingOwner::start_reporting_root(identity.clone(), root));
        super::doctor(project, &identity).await
    }

    #[tokio::test]
    async fn test_doctor_prints_both_hashes_so_a_mismatch_is_visible() {
        let project = tempfile::tempdir().expect("a temp dir");
        let elsewhere = tempfile::tempdir().expect("a temp dir");

        let out = doctor_with(project.path(), Some(elsewhere.path())).await;

        assert!(out.contains("hook sees"));
        assert!(out.contains("server sees"));
        assert!(
            out.contains("do not match"),
            "a config whose roots point at a subdirectory, a multi-root config, \
             or a symlinked checkout otherwise produces a permanent silent \
             no-op with nothing to look at"
        );
    }

    #[tokio::test]
    async fn test_doctor_says_nothing_is_wrong_when_the_hashes_agree() {
        let project = tempfile::tempdir().expect("a temp dir");

        let out = doctor_with(project.path(), Some(project.path())).await;

        assert!(
            !out.contains("do not match"),
            "the mismatch line is the one thing a reader acts on, so it must not \
             appear when there is nothing to act on"
        );
    }

    #[tokio::test]
    async fn test_doctor_reports_no_owner_when_nothing_is_bound() {
        let project = tempfile::tempdir().expect("a temp dir");
        let out = doctor_with(project.path(), None).await;
        assert!(out.contains("server sees: no owner"));
    }

    #[tokio::test]
    async fn test_doctor_reports_whether_mcpls_is_on_path() {
        let project = tempfile::tempdir().expect("a temp dir");
        let out = doctor_with(project.path(), None).await;

        let line = out
            .lines()
            .find(|line| line.starts_with("mcpls on PATH: "))
            .expect(
                "hooks invoke mcpls from PATH, and a hook environment missing \
                 the install directory makes every hook do nothing, invisibly, \
                 so the doctor must carry one line that answers it",
            );
        assert!(
            line.ends_with("not found") || line.contains(std::path::MAIN_SEPARATOR),
            "the line has to carry the result of the lookup, an absolute path or \
             a plain 'not found', rather than merely mentioning PATH: {line}"
        );
    }

    #[test]
    fn test_doctor_without_identity_says_no_socket_could_exist_rather_than_no_owner() {
        let project = tempfile::tempdir().expect("a temp dir");
        let missing = project.path().join("does-not-exist");
        let error = mcpls_core::hooks::identity_for(&missing).expect_err("an unreachable dir");

        let out = super::doctor_without_identity(project.path(), &error);

        assert!(
            !out.contains("no owner"),
            "a directory whose identity can never be derived is a different \
             failure than a socket nobody answered: {out}"
        );
        assert!(
            out.contains("could not derive an identity"),
            "the reader needs to know hooks can never work here at all, not \
             just that nothing answered right now: {out}"
        );
    }
}
