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
///
/// Ownership is exactly this value's lifetime, not the process's. [`serve`]
/// takes `self` by value and keeps it alive for as long as it runs, so
/// ownership lasts exactly as long as whatever task `serve` runs on is
/// alive and unaborted -- dropping that task early (`JoinHandle::abort`,
/// or the task's own panic) drops this `HookListener` and releases the
/// lock immediately, indistinguishably from this process exiting, even
/// though the process itself may still be running and may still believe
/// it owns the session. Whoever wires `serve` into a long-running task
/// must keep that task alive for exactly as long as the process should be
/// considered the session owner: nothing else in this type enforces it.
///
/// [`serve`]: Self::serve
pub struct HookListener {
    transport: Box<dyn HookTransport>,
    /// Held for as long as `Self` lives; dropping it (including on process
    /// exit) releases ownership. Never unlocked explicitly.
    #[cfg(not(windows))]
    lock: std::fs::File,
    /// `identity.lock`'s path, kept so [`Self::still_owns_lock`] can
    /// re-`stat` it against `lock`'s own `fstat`.
    #[cfg(not(windows))]
    lock_path: std::path::PathBuf,
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
        let Some(lock) = Self::lock_file(identity)? else {
            return Ok(None);
        };

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
            lock,
            lock_path: identity.lock.clone(),
        }))
    }

    /// How often [`Self::serve`] re-`stat`s its own lock file to notice a
    /// replacement (see [`Self::still_owns_lock`]). Unconditional (rather
    /// than Unix-only, like the check itself) because [`Self::serve`]
    /// constructs its ownership-check timer on every platform and gates
    /// the check at runtime instead, so the timer's type does not depend
    /// on `cfg`.
    const OWNERSHIP_CHECK_INTERVAL: Duration = Duration::from_millis(200);

    /// Whether `identity.lock` still names the inode this listener holds a
    /// lock on.
    ///
    /// This is the owner-side half of detecting an externally deleted lock
    /// file (see [`Self::lock_file`]'s doc comment for why the newcomer's
    /// side cannot detect it at all). Compares `lock`'s own `fstat`
    /// against a fresh `stat` of the path it was opened from: if a cleaner
    /// deleted and something else recreated that path, the two now name
    /// different inodes, even though `lock` itself is still open and
    /// still locked -- an `fstat` on an open handle keeps working after
    /// its path is unlinked, it just stops matching anything reachable by
    /// name.
    #[cfg(not(windows))]
    fn still_owns_lock(&self) -> bool {
        use std::os::unix::fs::MetadataExt as _;

        let Ok(locked) = self.lock.metadata() else {
            return false;
        };
        std::fs::metadata(&self.lock_path)
            .is_ok_and(|current| current.ino() == locked.ino() && current.dev() == locked.dev())
    }

    /// The most attempts [`Self::lock_file`] makes before giving up on a
    /// lock path that keeps getting replaced out from under it.
    #[cfg(not(windows))]
    const LOCK_FILE_MAX_ATTEMPTS: u32 = 5;

    /// Open, create if needed, and exclusively lock `identity.lock`.
    ///
    /// Returns `Ok(None)` when someone else already holds it and `Ok(Some)`
    /// when this call now owns it.
    ///
    /// This closes only the narrow race where the path is replaced
    /// *during this call*, between its own `open` and `try_lock_exclusive`
    /// succeeding: comparing this handle's `fstat` against a fresh `stat`
    /// of the path catches that, and retries against whatever is at the
    /// path now, up to [`Self::LOCK_FILE_MAX_ATTEMPTS`] times. It says
    /// nothing about a replacement that happens at any later point, once
    /// a lock is already held stably -- an external cleaner (an age-based
    /// `systemd-tmpfiles` policy over `/tmp/mcpls-<user>` is the realistic
    /// case) can delete the file at any moment after this call has
    /// already returned, and a newcomer that then opens the path creates
    /// a fresh inode and locks it uncontended: exclusive at the inode
    /// level, but no longer exclusive at the path, since this call cannot
    /// see a replacement that happens after it. Nothing on the newcomer's
    /// side can close that, because a replaced path is indistinguishable
    /// from a clean start from the newcomer's own point of view.
    /// [`Self::serve`] instead re-`stat`s the path periodically from the
    /// side that actually knows something was taken from it (see
    /// [`Self::still_owns_lock`]) and stands down when it no longer
    /// matches.
    ///
    /// Windows has no equivalent: there is no lock *file* whose path an
    /// external cleaner could sever from the handle holding it, since
    /// exclusivity there comes from the named pipe's own
    /// `first_pipe_instance` semantics rather than from a filesystem path.
    #[cfg(not(windows))]
    fn lock_file(identity: &SocketIdentity) -> Result<Option<std::fs::File>> {
        use std::os::unix::fs::MetadataExt as _;

        use fs4::fs_std::FileExt as _;

        // `socket` and `lock` are always siblings in identities `identity_for`
        // produces, but nothing enforces that, so both parents are created
        // rather than assuming either one covers the other.
        for path in [&identity.socket, &identity.lock] {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }

        for _ in 0..Self::LOCK_FILE_MAX_ATTEMPTS {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&identity.lock)?;

            match file.try_lock_exclusive() {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) => return Err(e.into()),
            }

            let locked = file.metadata()?;
            let replaced = std::fs::metadata(&identity.lock).map_or(true, |current| {
                current.ino() != locked.ino() || current.dev() != locked.dev()
            });
            if !replaced {
                return Ok(Some(file));
            }
        }

        Err(Error::Transport(format!(
            "the lock file at {} was replaced repeatedly while acquiring it",
            identity.lock.display()
        )))
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
    /// lasts, and warns once per run of failures rather than on every one
    /// of them. It otherwise never gives up and returns early: this
    /// socket is best-effort infrastructure whose every failure mode
    /// downstream already degrades to "no diagnostics this turn" rather
    /// than an error a caller must handle, so exiting the accept loop
    /// over a transient condition would trade a recoverable, momentary
    /// degradation for a permanent one lasting the rest of the process's
    /// life. The one exception is a poisoned Windows pipe-transport lock,
    /// which cannot recover by retrying at all: that stands down rather
    /// than retrying forever into a condition that can never clear.
    ///
    /// On Unix, also stands down if [`Self::still_owns_lock`] reports the
    /// lock file no longer names the inode this listener holds: something
    /// external took it while this process was still serving, and
    /// continuing would risk a second process believing it owns the same
    /// session. See [`Self::lock_file`]'s doc comment for why this has to
    /// be checked from here rather than at acquisition.
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
        let mut consecutive_accept_errors: u32 = 0;
        // Unconditionally constructed so its type does not depend on
        // platform, and gated off on Windows (where there is no lock file
        // to lose) with the `if` precondition below rather than `cfg`.
        let has_lock_file = cfg!(not(windows));
        let mut ownership_check = tokio::time::interval(Self::OWNERSHIP_CHECK_INTERVAL);
        ownership_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                result = cancel.changed() => {
                    // Err means the sender was dropped; treat as cancellation.
                    if result.is_err() || *cancel.borrow() {
                        return;
                    }
                }
                _ = ownership_check.tick(), if has_lock_file => {
                    #[cfg(not(windows))]
                    if !self.still_owns_lock() {
                        tracing::warn!(
                            "the lock at {} no longer names the inode this listener \
                             holds; something else now owns it, so this listener is \
                             standing down rather than risk two owners of the same \
                             session",
                            self.lock_path.display()
                        );
                        return;
                    }
                }
                accepted = self.transport.accept() => {
                    match accepted {
                        Ok(stream) => {
                            accept_backoff = Self::ACCEPT_BACKOFF_FLOOR;
                            consecutive_accept_errors = 0;
                            let handler = Arc::clone(&handler);
                            tokio::spawn(serve_connection(stream, handler, op_deadline));
                        }
                        Err(e) => {
                            #[cfg(windows)]
                            if is_pipe_transport_poisoned(&e) {
                                tracing::warn!(
                                    "hook socket accept failed permanently and will not \
                                     recover by retrying, giving up: {e}"
                                );
                                return;
                            }

                            consecutive_accept_errors += 1;
                            if consecutive_accept_errors == 1
                                || consecutive_accept_errors.is_multiple_of(100)
                            {
                                tracing::warn!(
                                    "hook socket accept failed ({consecutive_accept_errors} \
                                     in a row): {e}"
                                );
                            } else {
                                tracing::debug!("hook socket accept failed: {e}");
                            }
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
    use tokio::net::windows::named_pipe::ClientOptions;

    // Win32 `ERROR_PIPE_BUSY` (231): the pipe exists but every instance is
    // connected to a client right now. Hardcoded rather than pulled in
    // from `windows-sys` for one constant; see
    // https://learn.microsoft.com/windows/win32/debug/system-error-codes--0-499-
    // for the value. Tokio's own `ClientOptions::open` documentation gives
    // a sleep-and-retry loop as the intended handling for exactly this,
    // rather than treating it as a hard failure. The overall `send`/
    // `send_many` timeout bounds this loop, not a retry count here.
    const ERROR_PIPE_BUSY: i32 = 231;
    loop {
        match ClientOptions::new().open(&identity.socket) {
            Ok(client) => return Ok(Box::new(client)),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => return Err(e),
        }
    }
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

/// Marks the one `accept` error [`HookListener::serve`] cannot recover
/// from by retrying: a poisoned [`std::sync::Mutex`] never un-poisons, so
/// a [`PipeTransport`] whose `next` mutex is poisoned would otherwise
/// return this on every future `accept` forever.
#[cfg(windows)]
#[derive(Debug)]
struct PipeTransportPoisoned;

#[cfg(windows)]
impl std::fmt::Display for PipeTransportPoisoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hook pipe transport lock poisoned")
    }
}

#[cfg(windows)]
impl std::error::Error for PipeTransportPoisoned {}

#[cfg(windows)]
fn is_pipe_transport_poisoned(e: &io::Error) -> bool {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<PipeTransportPoisoned>())
        .is_some()
}

/// The first pipe instance is created during [`HookListener::acquire`], to
/// prove exclusivity. `accept` never lets the instance count reach zero:
/// it creates the replacement *before* touching `next`, so if that create
/// fails (the platform's 255-instance cap, or a transient resource error),
/// `next` is left exactly as it was rather than emptied. A pipe with zero
/// instances stops existing under that name, which would let a competing
/// process's own `first_pipe_instance` claim it while this one still
/// believes it owns the session -- worse than any refusal, since the
/// symptom is a silent takeover rather than a failed connect.
///
/// Once the replacement exists, taking the ready instance out of `next`
/// and storing the replacement in its place happens in one synchronous
/// step with no await between the two, so nothing -- another `accept`, or
/// this future's own cancellation -- can observe `next` empty there
/// either.
///
/// What this does *not* establish, and what Windows does not document:
/// which of two simultaneously-listening instances (the one just popped,
/// about to be `connect`-awaited, and the fresh replacement sitting in
/// `next`) an incoming client is assigned to. This code relies on
/// observed platform behaviour, not a documented guarantee, that a client
/// attaches to the longer-listening instance first. If that assumption
/// is ever wrong, the failure mode is a `connect` stalling until the
/// popped instance happens to receive a client, rather than the clean
/// `ERROR_PIPE_BUSY` refusal this design otherwise produces -- a stall is
/// harder to diagnose than a refusal. This has not been verified against
/// real concurrent clients on Windows.
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

            // Create the replacement before touching `next` at all: if
            // this fails, `next` is untouched and the pipe keeps whatever
            // instance it already had, rather than losing it to a create
            // that never lands.
            let replacement = ServerOptions::new().create(&self.path)?;

            let server = {
                let mut next = self
                    .next
                    .lock()
                    .map_err(|_| io::Error::other(PipeTransportPoisoned))?;
                let server = next.take();
                *next = Some(replacement);
                server
            };
            // `next` is populated by acquire and by every prior accept, so
            // this is always `Some` in practice; the fallback exists so a
            // missing instance is recovered from rather than panicked on.
            let server = match server {
                Some(server) => server,
                None => ServerOptions::new().create(&self.path)?,
            };

            server.connect().await?;
            Ok(Box::new(server) as Box<dyn HookStream>)
        })
    }
}
