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

/// How long a hook waits for the owning mcpls to answer before giving up.
///
/// The spec's figure. An edit must never stall waiting on diagnostics, so
/// this is short enough that a wedged or absent owner is indistinguishable
/// from a fast, empty answer.
const SOCKET_TIMEOUT: Duration = Duration::from_millis(50);

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
/// answer.
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
/// Every fault, a missing socket, a connect timeout, a malformed response,
/// a payload that will not parse, produces the empty string.
pub async fn dispatch_payload(
    stdin: &str,
    project_dir: &Path,
    identity: &SocketIdentity,
) -> String {
    silently(run(stdin, project_dir, identity)).await
}

async fn run(stdin: &str, project_dir: &Path, identity: &SocketIdentity) -> Result<String> {
    let payload: HookPayload = serde_json::from_str(stdin)?;

    match payload.hook_event_name.as_str() {
        // `SessionStart` fires while the host is still spawning the MCP
        // server, so the socket may not be bound yet. Computing the watch
        // set locally, rather than asking over the socket, is what keeps
        // this hook from either leaving the session with no coverage or an
        // unbounded watcher over target/.
        "SessionStart" => Ok(session_start_output(&watch_paths(project_dir))),

        "FileChanged" => {
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
            let responses = send_many(identity, &requests, SOCKET_TIMEOUT).await?;
            let context = responses.into_iter().nth(1).and_then(flush_context);
            Ok(additional_context_output(context))
        }

        "UserPromptSubmit" => {
            let response = send(
                identity,
                &Request::Flush {
                    session: payload.session_id,
                },
                SOCKET_TIMEOUT,
            )
            .await?;
            Ok(additional_context_output(flush_context(response)))
        }

        "SessionEnd" => {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use futures::future::BoxFuture;
    use mcpls_core::hooks::HookListener;
    use serde_json::json;

    use super::*;

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
        super::dispatch_payload(stdin, project_dir, &identity).await
    }

    /// Run the dispatcher against a listener that records the ops it gets.
    async fn dispatch_against(payload: &serde_json::Value, recorder: &RecordingOwner) -> String {
        super::dispatch_payload(
            &payload.to_string(),
            recorder.project_dir(),
            &recorder.identity,
        )
        .await
    }

    /// A `SocketIdentity` whose socket and lock live inside `dir`, tagged
    /// with `label` so two identities in the same directory (a public
    /// front door and the private listener behind it) never collide, built
    /// field by field rather than through `identity_for`, which would put
    /// the socket in the real runtime directory.
    fn temp_identity(dir: &Path, label: &str) -> SocketIdentity {
        let suffix = format!("{label}-{}", rand_suffix());
        #[cfg(windows)]
        let socket = PathBuf::from(format!(r"\\.\pipe\mcpls-recording-owner-{suffix}"));
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

    /// A listener on a temporary socket that records every op it is sent
    /// and answers each with the shape the dispatcher expects.
    ///
    /// `HookListener`'s handler sees only the deserialized `Request`, never
    /// which connection carried it, so there is no way to count
    /// connections from inside it. `start` instead binds two identities: a
    /// private one served for real by `HookListener`, and a public one (the
    /// one handed out as `identity`) fronted by a plain accept loop that
    /// counts every client before relaying its bytes verbatim to the
    /// private socket. `dispatch_against` only ever talks to the public
    /// identity, so `connections()` reports exactly how many connections
    /// the dispatcher itself opened.
    struct RecordingOwner {
        dir: tempfile::TempDir,
        identity: SocketIdentity,
        ops: Arc<Mutex<Vec<String>>>,
        connections: Arc<AtomicUsize>,
        _cancel: tokio::sync::watch::Sender<bool>,
    }

    impl RecordingOwner {
        /// Bind and start serving. The socket and the lock live in the
        /// returned temporary directory, so no test touches the real
        /// runtime path.
        async fn start() -> Self {
            let dir = tempfile::tempdir().expect("a temp dir");
            let public = temp_identity(dir.path(), "public");
            let private = temp_identity(dir.path(), "private");

            let listener = HookListener::acquire(&private)
                .await
                .expect("acquire")
                .expect("nothing else owns a socket inside a fresh temp dir");

            let ops: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let ops_for_handler = Arc::clone(&ops);
            let handler = move |request: Request| {
                let ops = Arc::clone(&ops_for_handler);
                Box::pin(async move { answer(request, &ops) }) as BoxFuture<'static, Response>
            };

            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            tokio::spawn(listener.serve(handler, Duration::from_secs(5), cancel_rx));

            // Bound synchronously, before this function returns: a spawned
            // task's first poll is not guaranteed to happen before the
            // caller's next `send`/`send_many`, and a public socket that
            // does not exist yet would refuse that connection outright.
            let public_listener = bind_public(&public.socket);
            let connections = Arc::new(AtomicUsize::new(0));
            tokio::spawn(relay_and_count(
                public_listener,
                public.socket.clone(),
                private.socket.clone(),
                Arc::clone(&connections),
            ));

            Self {
                dir,
                identity: public,
                ops,
                connections,
                _cancel: cancel_tx,
            }
        }

        /// The directory the dispatcher treats as `CLAUDE_PROJECT_DIR`.
        fn project_dir(&self) -> &Path {
            self.dir.path()
        }

        /// The op names received, in order: "changed", "flush", and so on.
        fn ops(&self) -> Vec<String> {
            self.ops.lock().expect("ops lock").clone()
        }

        /// How many client connections the public socket has accepted.
        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }
    }

    /// Record `request`'s op name and answer it plausibly.
    fn answer(request: Request, ops: &Mutex<Vec<String>>) -> Response {
        let (name, response) = match request {
            Request::Changed { paths, .. } => (
                "changed",
                Response::Changed {
                    queued: paths.len(),
                },
            ),
            Request::Flush { .. } => (
                "flush",
                Response::Flush {
                    context: Some("2 problems in a.rs".to_string()),
                },
            ),
            Request::EndSession { .. } => ("end_session", Response::EndSession),
            Request::Status => (
                "status",
                Response::Status {
                    hash: String::new(),
                    socket: PathBuf::new(),
                    pid: std::process::id(),
                    owner: true,
                },
            ),
        };
        ops.lock().expect("ops lock").push(name.to_string());
        response
    }

    /// Bind the public socket synchronously, so it exists the moment
    /// `RecordingOwner::start` returns rather than whenever its relay task
    /// first gets polled.
    #[cfg(not(windows))]
    fn bind_public(public: &Path) -> tokio::net::UnixListener {
        tokio::net::UnixListener::bind(public).expect("bind the recording proxy")
    }

    /// Accept every client on `listener`, counting each one, and relay its
    /// bytes verbatim to `private`. `_public` is unused here: a `UnixListener`
    /// never needs to recreate itself between clients the way a named pipe
    /// does, but the parameter is kept so both platforms share one call
    /// site in `RecordingOwner::start`.
    #[cfg(not(windows))]
    async fn relay_and_count(
        listener: tokio::net::UnixListener,
        _public: PathBuf,
        private: PathBuf,
        connections: Arc<AtomicUsize>,
    ) {
        loop {
            let Ok((mut client, _addr)) = listener.accept().await else {
                return;
            };
            connections.fetch_add(1, Ordering::SeqCst);
            let private = private.clone();
            tokio::spawn(async move {
                if let Ok(mut upstream) = tokio::net::UnixStream::connect(&private).await {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
            });
        }
    }

    /// The same, over a named pipe's first instance.
    #[cfg(windows)]
    fn bind_public(public: &Path) -> tokio::net::windows::named_pipe::NamedPipeServer {
        tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(public)
            .expect("create the recording proxy pipe")
    }

    /// Accept every client on `server`'s pipe, counting each one, and relay
    /// its bytes verbatim to `private`. `public` is the pipe path `server`
    /// was created on, kept alongside it so each accepted client's
    /// replacement instance can be created on the same name. Unlike the
    /// production transport, this does not keep a spare instance alive
    /// between clients: a test double driving one sequential client at a
    /// time does not need that robustness.
    #[cfg(windows)]
    async fn relay_and_count(
        mut server: tokio::net::windows::named_pipe::NamedPipeServer,
        public: PathBuf,
        private: PathBuf,
        connections: Arc<AtomicUsize>,
    ) {
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
        loop {
            if server.connect().await.is_err() {
                return;
            }
            connections.fetch_add(1, Ordering::SeqCst);
            let current = server;
            server = match ServerOptions::new().create(&public) {
                Ok(next) => next,
                Err(_) => return,
            };
            let private = private.clone();
            tokio::spawn(async move {
                let mut current = current;
                if let Ok(mut client) = ClientOptions::new().open(&private) {
                    let _ = tokio::io::copy_bidirectional(&mut current, &mut client).await;
                }
            });
        }
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
    async fn test_post_tool_batch_sends_changed_then_flush() {
        let recorder = RecordingOwner::start().await;
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
            recorder.ops(),
            vec!["changed", "flush"],
            "changed queues for the sweep and flush drains the record; the pair \
             is what makes a batch's own paths reach the servers"
        );
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"],
            json!("2 problems in a.rs"),
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
}
