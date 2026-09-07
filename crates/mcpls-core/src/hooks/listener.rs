//! The hook socket's transport and its ownership lock.
//!
//! Ownership is an advisory lock held for the owner's whole life, never a
//! connect-and-see probe. Renaming a temporary socket into place is atomic
//! but not exclusive: two processes that both saw a stale socket could both
//! bind and rename, and the loser would be a listener nobody can reach that
//! still believes it owns the session, with no way to notice. Taking
//! `try_lock_exclusive` on a lock file settles it instead, and a process
//! that dies releases the lock along with everything else it held, which is
//! what makes a crashed owner recoverable.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::error::{Error, Result};
use crate::hooks::identity::SocketIdentity;
use crate::hooks::protocol::{Request, Response};

/// One end of the hook transport, so the Unix socket and the Windows named
/// pipe differ in one place rather than throughout `serve` and `send`.
trait HookTransport: Send + Sync {
    /// Wait for the next client.
    fn accept(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>>;
}

/// One client connection.
trait HookStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}

impl<T> HookStream for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}

/// A bound listener, and the lock proving this process owns it.
pub struct HookListener {
    transport: Box<dyn HookTransport>,
    /// Held for as long as `Self` lives; dropping it (including on process
    /// exit) releases ownership. Never unlocked explicitly, and never read:
    /// its only job is to stay open.
    #[cfg(not(windows))]
    _lock: std::fs::File,
}

impl HookListener {
    /// Take ownership of `identity`'s socket, or report that someone else
    /// holds it.
    ///
    /// # Errors
    ///
    /// Returns an error if the runtime directory cannot be created, the
    /// lock file cannot be opened, or binding fails for a reason other than
    /// contention.
    pub async fn acquire(identity: &SocketIdentity) -> Result<Option<Self>> {
        let identity = identity.clone();
        // Locking and binding are blocking filesystem and socket syscalls;
        // they run off the runtime's worker threads like every other
        // blocking step in this codebase.
        tokio::task::spawn_blocking(move || Self::acquire_blocking(&identity))
            .await
            .map_err(|e| Error::Transport(format!("the acquire task panicked: {e}")))?
    }

    #[cfg(not(windows))]
    fn acquire_blocking(identity: &SocketIdentity) -> Result<Option<Self>> {
        use fs4::fs_std::FileExt as _;

        if let Some(parent) = identity.lock.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&identity.lock)?;

        match lock.try_lock_exclusive() {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e.into()),
        }

        // Holding the lock makes this the only process allowed to touch
        // `socket`: a crashed owner leaves its socket file behind, and
        // nothing else will ever clean it up otherwise.
        if let Err(e) = std::fs::remove_file(&identity.socket)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(e.into());
        }

        let listener = tokio::net::UnixListener::bind(&identity.socket)?;
        Ok(Some(Self {
            transport: Box::new(UnixTransport { listener }),
            _lock: lock,
        }))
    }

    #[cfg(windows)]
    fn acquire_blocking(identity: &SocketIdentity) -> Result<Option<Self>> {
        use tokio::net::windows::named_pipe::ServerOptions;

        match ServerOptions::new()
            .first_pipe_instance(true)
            .create(&identity.socket)
        {
            Ok(server) => Ok(Some(Self {
                transport: Box::new(PipeTransport {
                    path: identity.socket.clone(),
                    next: std::sync::Mutex::new(Some(server)),
                }),
            })),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                // Win32's CreateNamedPipe returns ERROR_ACCESS_DENIED both
                // when another process already holds the first-instance
                // pipe (routine contention) and when a DACL genuinely
                // denies creation rights (e.g. a pipe left behind by
                // another user's session). The two are indistinguishable
                // from this error alone, and resolving the ambiguity by
                // probing further (opening the pipe as a client) would
                // itself race the very contention this is trying to
                // detect. Treating both as "not the owner" is the safe
                // default: failing acquisition outright on every such
                // error would break the ordinary multi-instance takeover
                // this exists to support whenever the real cause is
                // routine contention, to avoid silence in the rarer case
                // where it is a genuine permission problem.
                tracing::warn!("hook pipe creation denied for {:?}: {e}", identity.socket);
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The shortest an `accept` backoff ever waits, and what it resets to
    /// after a success.
    const ACCEPT_BACKOFF_FLOOR: Duration = Duration::from_millis(10);
    /// The longest an `accept` backoff ever waits, however many consecutive
    /// errors precede it.
    const ACCEPT_BACKOFF_CEILING: Duration = Duration::from_millis(500);

    /// Serve connections until `cancel` fires, answering every op within
    /// `op_deadline` whether or not the handler has finished.
    ///
    /// The deadline bounds each op's *answer*, not its work: a handler that
    /// outruns `op_deadline` gets `Response::Error` written back for that
    /// request while its future keeps running to completion on its own
    /// task. Rewriting this to a plain `timeout` around the handler's
    /// future would cancel that work at the deadline instead, silently
    /// dropping every sweep that ran long.
    ///
    /// A persistent `accept` error (an fd-exhausted process is the
    /// realistic case, since mcpls also spawns language servers) backs off
    /// exponentially between [`Self::ACCEPT_BACKOFF_FLOOR`] and
    /// [`Self::ACCEPT_BACKOFF_CEILING`] rather than retrying immediately,
    /// so it cannot spin a core or flood the log while the condition
    /// lasts. It never gives up and returns early: this socket is
    /// best-effort infrastructure whose every failure mode downstream
    /// already degrades to "no diagnostics this turn" rather than an
    /// error a caller must handle, so exiting the accept loop over a
    /// transient condition would trade a recoverable, momentary
    /// degradation for a permanent one lasting the rest of the process's
    /// life.
    pub async fn serve<H>(
        self,
        handler: H,
        op_deadline: Duration,
        mut cancel: watch::Receiver<bool>,
    ) where
        H: Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static,
    {
        let handler = Arc::new(handler);
        let mut accept_backoff = Self::ACCEPT_BACKOFF_FLOOR;
        loop {
            tokio::select! {
                result = cancel.changed() => {
                    // Err means the sender was dropped; treat as cancellation.
                    if result.is_err() || *cancel.borrow() {
                        return;
                    }
                }
                accepted = self.transport.accept() => {
                    match accepted {
                        Ok(stream) => {
                            accept_backoff = Self::ACCEPT_BACKOFF_FLOOR;
                            let handler = Arc::clone(&handler);
                            tokio::spawn(serve_connection(stream, handler, op_deadline));
                        }
                        Err(e) => {
                            tracing::warn!("hook socket accept failed: {e}");
                            tokio::select! {
                                () = tokio::time::sleep(accept_backoff) => {}
                                result = cancel.changed() => {
                                    if result.is_err() || *cancel.borrow() {
                                        return;
                                    }
                                }
                            }
                            accept_backoff = (accept_backoff * 2).min(Self::ACCEPT_BACKOFF_CEILING);
                        }
                    }
                }
            }
        }
    }
}

/// Read newline-delimited requests off `stream` until it closes, answering
/// each within `op_deadline` before reading the next.
async fn serve_connection<H>(stream: Box<dyn HookStream>, handler: Arc<H>, op_deadline: Duration)
where
    H: Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let work = tokio::spawn(handler(request));
                match tokio::time::timeout(op_deadline, work).await {
                    Ok(Ok(response)) => response,
                    Ok(Err(_join_error)) => Response::Error {
                        message: "the handler panicked".to_string(),
                    },
                    Err(_elapsed) => Response::Error {
                        message: format!(
                            "op exceeded {}ms; its work continues and reaches the next flush",
                            op_deadline.as_millis()
                        ),
                    },
                }
            }
            Err(e) => Response::Error {
                message: format!("invalid request: {e}"),
            },
        };

        let Ok(mut line) = serde_json::to_string(&response) else {
            return;
        };
        line.push('\n');
        if write_line(&mut writer, &line).await.is_err() {
            return;
        }
    }
}

/// Write a pre-serialized line and flush it.
///
/// Takes the line already serialized, rather than a value to serialize,
/// because a generic value parameter held across the `write_all` await
/// point would need `Sync` to keep this `Send`, and a caller's `Request` or
/// `Response` has no reason to promise that.
async fn write_line<W>(writer: &mut W, line: &str) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Send + Unpin,
{
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

/// Send one request to whoever owns `identity`'s socket.
///
/// # Errors
///
/// Returns an error if no one owns the socket, the connection drops before
/// an answer arrives, or `timeout` elapses first.
pub async fn send(
    identity: &SocketIdentity,
    request: &Request,
    timeout: Duration,
) -> Result<Response> {
    let mut responses = send_many(identity, std::slice::from_ref(request), timeout).await?;
    Ok(responses.remove(0))
}

/// Send several requests down one connection, in order, and collect the
/// answers.
///
/// The spec's protocol says `PostToolBatch` sends `changed` then `flush` on
/// one connection, and the framing already allows it: a connection carries
/// one or more requests. [`send`] is this with a one-element slice.
///
/// # Errors
///
/// Returns an error if no one owns the socket, the connection drops before
/// every answer arrives, or `timeout` elapses first.
pub async fn send_many(
    identity: &SocketIdentity,
    requests: &[Request],
    timeout: Duration,
) -> Result<Vec<Response>> {
    match tokio::time::timeout(timeout, send_many_inner(identity, requests)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(Error::Transport(format!(
            "the hook socket did not answer within {}ms",
            timeout.as_millis()
        ))),
    }
}

async fn send_many_inner(identity: &SocketIdentity, requests: &[Request]) -> Result<Vec<Response>> {
    let stream = connect(identity).await?;
    let (reader, mut writer) = tokio::io::split(stream);

    for request in requests {
        let mut line = serde_json::to_string(request)?;
        line.push('\n');
        write_line(&mut writer, &line).await?;
    }

    let mut lines = BufReader::new(reader).lines();
    let mut responses = Vec::with_capacity(requests.len());
    for _ in requests {
        let line = lines.next_line().await?.ok_or_else(|| {
            Error::Transport("the hook socket closed before answering".to_string())
        })?;
        responses.push(serde_json::from_str(&line)?);
    }
    Ok(responses)
}

#[cfg(not(windows))]
async fn connect(identity: &SocketIdentity) -> io::Result<Box<dyn HookStream>> {
    let stream = tokio::net::UnixStream::connect(&identity.socket).await?;
    Ok(Box::new(stream))
}

#[cfg(windows)]
async fn connect(identity: &SocketIdentity) -> io::Result<Box<dyn HookStream>> {
    let client = tokio::net::windows::named_pipe::ClientOptions::new().open(&identity.socket)?;
    Ok(Box::new(client))
}

#[cfg(not(windows))]
struct UnixTransport {
    listener: tokio::net::UnixListener,
}

#[cfg(not(windows))]
impl HookTransport for UnixTransport {
    fn accept(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>> {
        Box::pin(async move {
            let (stream, _addr) = self.listener.accept().await?;
            Ok(Box::new(stream) as Box<dyn HookStream>)
        })
    }
}

/// The first pipe instance is created during [`HookListener::acquire`], to
/// prove exclusivity. `accept` pops whichever instance is in `next` and, in
/// the same synchronous step with no await between the two, creates and
/// stores its replacement before awaiting `connect` on the popped one: a
/// ready instance is therefore in `next` at every point either a
/// concurrent client's `connect` or this future's own cancellation could
/// observe it, rather than only after the previous connection completed.
#[cfg(windows)]
struct PipeTransport {
    path: std::path::PathBuf,
    next: std::sync::Mutex<Option<tokio::net::windows::named_pipe::NamedPipeServer>>,
}

#[cfg(windows)]
impl HookTransport for PipeTransport {
    fn accept(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>> {
        Box::pin(async move {
            use tokio::net::windows::named_pipe::ServerOptions;

            // Take the ready instance and create its replacement here,
            // before awaiting `connect`: both are synchronous, so nothing
            // -- another accept, or this select! branch losing to
            // cancellation and dropping this future -- can observe `next`
            // empty. Creating the replacement only after `connect`
            // returns would leave a window with no free instance, in
            // which a second client's own connect gets ERROR_PIPE_BUSY.
            let server = {
                let mut next = self
                    .next
                    .lock()
                    .map_err(|_| io::Error::other("hook pipe transport lock poisoned"))?;
                let server = match next.take() {
                    Some(server) => server,
                    None => ServerOptions::new().create(&self.path)?,
                };
                *next = Some(ServerOptions::new().create(&self.path)?);
                server
            };

            server.connect().await?;
            Ok(Box::new(server) as Box<dyn HookStream>)
        })
    }
}
