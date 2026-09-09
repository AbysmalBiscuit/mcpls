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

/// Why a listener can no longer prove it holds the lock it acquired.
///
/// The two call for different responses, which is why they are not
/// flattened into one: `Replaced` means a competitor holds the lock for its
/// whole life and this process will not win it back, while `Missing` means
/// nothing has taken this listener's place and the next attempt succeeds.
/// Never produced on Windows, which has no lock file to lose; the type is
/// unconditional so [`ServeExit`] has one shape on every platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockLoss {
    /// The path now names a different inode: a competitor created its own
    /// lock file there and may now believe it owns this session.
    Replaced,
    /// The path no longer exists: nothing has taken this listener's place
    /// yet, but the file it relied on to prove ownership is gone.
    Missing,
}

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
    /// `identity.lock`'s path, kept so [`Self::lock_loss`] can re-`stat`
    /// it against `lock`'s own `fstat`.
    #[cfg(not(windows))]
    lock_path: std::path::PathBuf,
}

/// Why [`HookListener::serve`] stopped serving.
///
/// Named for what happened, not for what a caller should do about it,
/// because that decision belongs to the caller. In particular,
/// [`ServeExit::LockLost`] is not itself a reason to give up: the file a
/// lock lives in can be recreated, so the ordinary response is to try
/// acquiring the socket again rather than exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeExit {
    /// The `cancel` watch fired, or its sender was dropped.
    Cancelled,
    /// Unix only: the lock file this listener held is no longer the file
    /// it holds a lock on. The payload says whether the file was replaced
    /// (a competitor may now believe it owns this session) or simply
    /// removed (no competitor has appeared, only a missing file), which is
    /// what decides whether a caller can expect to win the lock back.
    /// Never returned on Windows, which has no lock file to lose.
    LockLost(LockLoss),
    /// The transport hit an error it can never recover from by retrying
    /// (currently: a poisoned Windows pipe-transport lock).
    TransportUnrecoverable,
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
    /// replacement (see [`Self::lock_loss`]). Unconditional (rather than
    /// Unix-only, like the check itself) because [`Self::serve`]
    /// constructs its ownership-check timer on every platform and gates
    /// the check at runtime instead, so the timer's type does not depend
    /// on `cfg`.
    const OWNERSHIP_CHECK_INTERVAL: Duration = Duration::from_millis(200);

    /// Whether `identity.lock` no longer names the inode this listener
    /// holds a lock on, and if not, why not.
    ///
    /// This is the owner-side half of detecting an externally deleted lock
    /// file (see [`Self::lock_file`]'s doc comment for why the newcomer's
    /// side cannot detect it at all). Compares `lock`'s own `fstat`
    /// against a fresh `stat` of the path it was opened from -- an
    /// `fstat` on an open handle keeps working after its path is
    /// unlinked, it just stops matching anything reachable by name --
    /// and distinguishes the path naming a different inode now (something
    /// replaced it, and that something may believe it owns this session)
    /// from the path not existing at all (nothing has taken this
    /// listener's place, but it can no longer prove it owns the session
    /// either). Those are different situations: only the first means a
    /// competitor exists.
    #[cfg(not(windows))]
    fn lock_loss(&self) -> Option<LockLoss> {
        use std::os::unix::fs::MetadataExt as _;

        let Ok(locked) = self.lock.metadata() else {
            // `fstat` on a handle this call itself still has open should
            // not fail; if it somehow does, there is no path comparison
            // left to make, so this is treated the same as the path
            // having vanished.
            return Some(LockLoss::Missing);
        };
        match std::fs::metadata(&self.lock_path) {
            Ok(current) if current.ino() == locked.ino() && current.dev() == locked.dev() => None,
            Ok(_) => Some(LockLoss::Replaced),
            Err(_) => Some(LockLoss::Missing),
        }
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
    /// [`Self::lock_loss`]) and stands down when it no longer matches.
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
    /// exponentially between `Self::ACCEPT_BACKOFF_FLOOR` and
    /// `Self::ACCEPT_BACKOFF_CEILING` rather than retrying immediately,
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
    /// On Unix, also stands down if `Self::lock_loss` reports the lock
    /// file no longer names the inode this listener holds: something
    /// external took it while this process was still serving, and
    /// continuing would risk a second process believing it owns the same
    /// session. See `Self::lock_file`'s doc comment for why this has to
    /// be checked from here rather than at acquisition. This includes the
    /// case where nothing has actually taken the lock's place -- the file
    /// was simply removed -- because this listener can no longer prove it
    /// owns the session either way; the `tracing::warn!` this emits names
    /// which of the two happened.
    pub async fn serve<H>(
        self,
        handler: H,
        op_deadline: Duration,
        mut cancel: watch::Receiver<bool>,
    ) -> ServeExit
    where
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
                        return ServeExit::Cancelled;
                    }
                }
                _ = ownership_check.tick(), if has_lock_file => {
                    #[cfg(not(windows))]
                    if let Some(loss) = self.lock_loss() {
                        match loss {
                            LockLoss::Replaced => tracing::warn!(
                                "the lock at {} now names a different inode than the \
                                 one this listener holds; something else has taken \
                                 it and may now believe it owns this session, so \
                                 this listener is standing down rather than risk two \
                                 owners",
                                self.lock_path.display()
                            ),
                            LockLoss::Missing => tracing::warn!(
                                "the lock at {} no longer exists; nothing has taken \
                                 this listener's place yet, but it can no longer \
                                 prove it owns the session, so it is standing down",
                                self.lock_path.display()
                            ),
                        }
                        return ServeExit::LockLost(loss);
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
                                return ServeExit::TransportUnrecoverable;
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
                                        return ServeExit::Cancelled;
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
                let overrun = overrun_message(&request, op_deadline);
                let work = tokio::spawn(handler(request));
                match tokio::time::timeout(op_deadline, work).await {
                    Ok(Ok(response)) => response,
                    Ok(Err(_join_error)) => Response::Error {
                        message: "the handler panicked".to_string(),
                    },
                    Err(_elapsed) => Response::Error { message: overrun },
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

/// What a client is told when its op outran the deadline: what the work
/// it did not wait for does once it finishes, which differs per op.
///
/// A `flush` that outruns the deadline stages a report nobody
/// acknowledges, so the record does not move and the next `flush` offers
/// the same report; saying so is what lets a client treat the error as a
/// deferral rather than a loss.
fn overrun_message(request: &Request, op_deadline: Duration) -> String {
    let ms = op_deadline.as_millis();
    match request {
        Request::Changed { .. } => format!(
            "op exceeded {ms}ms; the paths are queued when the work finishes and their \
             diagnostics reach a later flush"
        ),
        Request::Flush { .. } => format!(
            "op exceeded {ms}ms; nothing confirmed this report was delivered, so the next \
             flush offers it again"
        ),
        Request::Ack { .. } => format!(
            "op exceeded {ms}ms; the record advances when the work finishes, unless this \
             report was superseded or its session ended"
        ),
        Request::EndSession { .. } => {
            format!("op exceeded {ms}ms; the session's record is dropped when the work finishes")
        }
        Request::Status => format!("op exceeded {ms}ms"),
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
        Err(_elapsed) => Err(timed_out(timeout)),
    }
}

/// What a caller is told when its own bound elapsed before the owner
/// answered. One producer for [`send_many`] and [`send_and_acknowledge`],
/// so the two cannot come to describe the same failure differently.
fn timed_out(timeout: Duration) -> Error {
    Error::Transport(format!(
        "the hook socket did not answer within {}ms",
        timeout.as_millis()
    ))
}

/// How long an acknowledgement gets, measured from the moment the flush
/// answer is in hand.
///
/// Its own allowance rather than what is left of the caller's, because an
/// owner that answers a flush at its own deadline leaves a caller bound to
/// the same number nothing to spend, and the two are configured
/// independently and default to the same 1500 ms. What such a caller
/// certainly loses is the acknowledgement's answer; where the write itself
/// can pend it loses the acknowledgement, and then the owner never commits
/// and offers the same report on every flush after it. A commit that lands
/// only because a write completed on its first poll is not a property to
/// rest on.
///
/// Short, because the exchange is one round trip on a connection that is
/// already open and its outcome is discarded either way: a longer bound
/// would only delay a hook that has already printed.
const ACK_TIMEOUT: Duration = Duration::from_millis(250);

/// Send `requests` down one connection and, when the last flush among
/// them was answered with a token, acknowledge it on that connection
/// before returning.
///
/// The answers are returned whether or not the acknowledgement lands. By
/// the time it is sent the caller's report is in hand, and an owner that
/// never hears it offers the same report again next time; withholding the
/// report over a failed acknowledgement would be the one outcome the
/// acknowledgement exists to rule out. The acknowledgement runs under
/// `ACK_TIMEOUT`, its own allowance, rather than under what `timeout`
/// has left, and its outcome is discarded either way.
///
/// It is sent before the caller prints, not after. The host reads a
/// hook's output at the hook's exit and discards it if the hook is
/// killed first, so the interval in which a kill loses the report runs
/// from the ack leaving this process to the process exiting under either
/// order; sending after the print would trim that interval by the print
/// alone and leave the round trip and the exit, which dominate it, where
/// they are.
///
/// # Errors
///
/// Returns an error if no one owns the socket, the connection drops before
/// every answer arrives, or `timeout` elapses before they do.
pub async fn send_and_acknowledge(
    identity: &SocketIdentity,
    requests: &[Request],
    timeout: Duration,
) -> Result<Vec<Response>> {
    let exchanged = tokio::time::timeout(timeout, async {
        let mut connection = Connection::open(identity).await?;
        let responses = connection.exchange(requests).await?;
        Ok::<_, Error>((connection, responses))
    })
    .await
    .map_err(|_elapsed| timed_out(timeout))?;
    let (mut connection, responses) = exchanged?;

    if let Some(ack) = acknowledgement_for(requests, &responses) {
        match tokio::time::timeout(ACK_TIMEOUT, connection.exchange(&[ack])).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::debug!(%error, "the flush acknowledgement was not answered; the owner offers the report again");
            }
            Err(_elapsed) => {
                tracing::debug!(
                    "the flush acknowledgement outran the deadline; the owner offers the report again"
                );
            }
        }
    }
    Ok(responses)
}

/// The acknowledgement `responses` call for: one for the last `Flush` in
/// `requests` whose answer carries a token, or none.
fn acknowledgement_for(requests: &[Request], responses: &[Response]) -> Option<Request> {
    requests
        .iter()
        .zip(responses)
        .rev()
        .find_map(|(request, response)| match (request, response) {
            (
                Request::Flush { session },
                Response::Flush {
                    token: Some(token), ..
                },
            ) => Some(Request::Ack {
                session: session.clone(),
                token: *token,
            }),
            _ => None,
        })
}

/// What probing a socket once found.
///
/// [`send`] and [`send_many`] collapse everything short of a clean answer
/// into one `Err`, which is right for every hook arm that only cares
/// whether it got an answer back. `mcpls hook doctor` needs the finer
/// distinctions: a refused or missing socket has no owner, so looking for
/// one running a different directory is the right next step; a socket
/// that accepted the connection and then went quiet has an owner that is
/// merely busy; and a socket whose exchange failed before a readable
/// answer arrived is neither of those. Naming some other project as the
/// cause of any but the first would be an accusation with no evidence
/// behind it, and calling the last one "busy" would send the reader
/// looking for load that is not there.
#[derive(Debug)]
pub enum ProbeOutcome {
    /// The peer answered before the deadline with a `Response` this
    /// build could parse.
    Answered(Response),
    /// The connection itself could not be made: refused, or the socket
    /// does not exist. Nobody owns this socket.
    NoOwner,
    /// A connection was accepted, but no complete answer arrived before
    /// the deadline. Something is there.
    Busy,
    /// A connection was accepted and the exchange then failed before a
    /// readable answer arrived: the write failed, the read failed, the
    /// peer hung up without answering, or what came back could not be
    /// turned into a `Response`. Something holds the socket; the `Error`
    /// carries which of the four happened. A wire shape this build does
    /// not recognize lands in the last of them, and this protocol has
    /// gained a required field more than once in this codebase's own
    /// history, but the other three have nothing to do with versions.
    Unintelligible(Error),
}

/// Probe `identity`'s socket with one `request`.
///
/// Distinguishes a refused or missing socket ([`ProbeOutcome::NoOwner`]),
/// one that accepted the connection but did not answer within `timeout`
/// ([`ProbeOutcome::Busy`]), and one whose exchange failed before a
/// readable answer arrived ([`ProbeOutcome::Unintelligible`]).
pub async fn probe(
    identity: &SocketIdentity,
    request: &Request,
    timeout: Duration,
) -> ProbeOutcome {
    let deadline = tokio::time::Instant::now() + timeout;
    let stream = match probe_connect_phase(identity, deadline).await {
        ConnectPhase::Connected(stream) => stream,
        ConnectPhase::GaveUp(outcome) => return outcome,
    };
    match tokio::time::timeout_at(deadline, answer_one(stream, request)).await {
        Ok(Ok(response)) => ProbeOutcome::Answered(response),
        // The connection was already accepted and the exchange then
        // failed, so this is not a busy owner. `Err(_)` here is a write
        // failure, a read failure, a peer that hung up without sending
        // anything, or a reply that would not parse; it is never a
        // timeout, because the exchange is not itself time-bounded and
        // only the outer `timeout_at` is. Which of the four it was lives
        // in the error, not in this variant.
        Ok(Err(error)) => ProbeOutcome::Unintelligible(error),
        // Nothing usable arrived before the deadline at all: the
        // connection was accepted but the owner never got back to us.
        Err(_) => ProbeOutcome::Busy,
    }
}

/// What [`probe`]'s connect phase found: a stream, or a reason to stop
/// with no stream at all.
enum ConnectPhase {
    Connected(Box<dyn HookStream>),
    GaveUp(ProbeOutcome),
}

/// On Unix, [`connect`]'s own error already means exactly "nobody is
/// there" (a refused or missing socket), so a connect that cannot
/// complete before `deadline` either way is [`ProbeOutcome::NoOwner`].
///
/// Windows has its own path below, because [`connect`]'s retry loop
/// there can spend the whole deadline waiting on an owner who is right
/// there, and racing it against an outer timeout would lose the one
/// fact that tells the two states apart the moment the timeout drops the
/// future.
#[cfg(not(windows))]
async fn probe_connect_phase(
    identity: &SocketIdentity,
    deadline: tokio::time::Instant,
) -> ConnectPhase {
    match tokio::time::timeout_at(deadline, connect(identity)).await {
        Ok(Ok(stream)) => ConnectPhase::Connected(stream),
        Ok(Err(_)) | Err(_) => ConnectPhase::GaveUp(ProbeOutcome::NoOwner),
    }
}

/// [`probe`]'s own connect loop, kept separate from [`connect`] rather
/// than sharing it, so [`send`]/[`send_many`]'s behavior is untouched by
/// this: a pipe whose single instance is already taken (`ERROR_PIPE_BUSY`)
/// is retried, exactly like [`connect`], but this loop tracks its own
/// deadline instead of being raced against one from outside, so it still
/// knows what it last saw when it gives up rather than losing that fact
/// to a dropped future.
#[cfg(windows)]
async fn probe_connect_phase(
    identity: &SocketIdentity,
    deadline: tokio::time::Instant,
) -> ConnectPhase {
    use tokio::net::windows::named_pipe::ClientOptions;

    // See `connect`'s own comment for why this is the documented way to
    // handle `ERROR_PIPE_BUSY` rather than a hard failure.
    const ERROR_PIPE_BUSY: i32 = 231;
    loop {
        match ClientOptions::new().open(&identity.socket) {
            Ok(client) => return ConnectPhase::Connected(Box::new(client)),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if tokio::time::Instant::now() >= deadline {
                    return ConnectPhase::GaveUp(classify_gave_up_connect(true));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(_) => return ConnectPhase::GaveUp(classify_gave_up_connect(false)),
        }
    }
}

/// Whether a connect attempt that gave up before succeeding means nobody
/// was listening, or an owner exists but every instance of the pipe was
/// already taken.
///
/// A pure mapping, kept separate from the retry loop that produces its
/// input so it has a real unit test on every platform: the only way to
/// feed it `true` in production is a Windows pipe whose single instance
/// is busy, which nothing on this platform can produce, but the mapping
/// itself does not depend on the platform that produced its input.
#[cfg(any(windows, test))]
const fn classify_gave_up_connect(saw_pipe_busy: bool) -> ProbeOutcome {
    if saw_pipe_busy {
        ProbeOutcome::Busy
    } else {
        ProbeOutcome::NoOwner
    }
}

/// One open connection to whoever owns a socket, so a caller can run more
/// than one exchange on it: a flush and, once its answer is in hand, the
/// acknowledgement that lets the owner advance the record. One connection
/// rather than two because a connect is the one step that can find a
/// Windows pipe busy; an exchange on an open connection cannot.
struct Connection {
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<Box<dyn HookStream>>>>,
    writer: tokio::io::WriteHalf<Box<dyn HookStream>>,
}

impl Connection {
    async fn open(identity: &SocketIdentity) -> Result<Self> {
        Ok(Self::over(connect(identity).await?))
    }

    fn over(stream: Box<dyn HookStream>) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        Self {
            lines: BufReader::new(reader).lines(),
            writer,
        }
    }

    /// Write every request in `requests`, in order, then read back one
    /// response line for each.
    ///
    /// The one place that writes the request framing and reads the
    /// response framing, for [`send_many_inner`], [`answer_one`] and
    /// [`send_and_acknowledge`] alike: if any two disagreed on either,
    /// `mcpls hook doctor` would misread a healthy owner's answer and
    /// report [`ProbeOutcome::Busy`] for it, which is exactly the
    /// misdiagnosis this module exists to prevent.
    async fn exchange(&mut self, requests: &[Request]) -> Result<Vec<Response>> {
        for request in requests {
            let mut line = serde_json::to_string(request)?;
            line.push('\n');
            write_line(&mut self.writer, &line).await?;
        }
        let mut responses = Vec::with_capacity(requests.len());
        for _ in requests {
            let line = self.lines.next_line().await?.ok_or_else(|| {
                Error::Transport("the hook socket closed before answering".to_string())
            })?;
            responses.push(serde_json::from_str(&line)?);
        }
        Ok(responses)
    }
}

/// Write `request` on `stream` and read back one response line.
async fn answer_one(stream: Box<dyn HookStream>, request: &Request) -> Result<Response> {
    let mut responses = Connection::over(stream)
        .exchange(std::slice::from_ref(request))
        .await?;
    Ok(responses.remove(0))
}

async fn send_many_inner(identity: &SocketIdentity, requests: &[Request]) -> Result<Vec<Response>> {
    Connection::open(identity).await?.exchange(requests).await
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
    //
    // Collapsing `ERROR_PIPE_BUSY` into a retry, with no trace of it left
    // once the loop gives up, is correct for this function's only two
    // callers (`send`/`send_many`), which care about nothing but whether
    // an answer came back. Do not reuse this loop for `probe`: an owner
    // holding the pipe's only instance is not the same fact as no owner
    // at all, and `probe`'s own connect loop below exists specifically
    // to keep that fact alive past the deadline instead of losing it the
    // way racing this one against an outer timeout would.
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

// Windows only: `AlwaysPoisoned` exists to reach `PipeTransportPoisoned`,
// which is itself Windows-only, so the whole module would otherwise be an
// unused `use super::*` on every other platform.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(windows)]
mod tests {
    use super::*;

    /// A transport whose every `accept` fails with the same error a
    /// poisoned [`PipeTransport`] mutex would produce, so
    /// [`HookListener::serve`]'s bail path can be exercised without a
    /// real named pipe.
    struct AlwaysPoisoned;

    impl HookTransport for AlwaysPoisoned {
        fn accept(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>> {
            Box::pin(async { Err(io::Error::other(PipeTransportPoisoned)) })
        }
    }

    #[tokio::test]
    async fn test_serve_reports_transport_unrecoverable_on_a_poisoned_pipe_lock() {
        let listener = HookListener {
            transport: Box::new(AlwaysPoisoned),
        };
        let (_tx, cancel) = tokio::sync::watch::channel(false);

        let exit = tokio::time::timeout(
            Duration::from_secs(1),
            listener.serve(
                |_req: Request| -> BoxFuture<'static, Response> {
                    Box::pin(async {
                        Response::Flush {
                            context: None,
                            token: None,
                        }
                    })
                },
                Duration::from_millis(100),
                cancel,
            ),
        )
        .await
        .expect("a transport that always fails must not make serve hang");

        assert_eq!(exit, ServeExit::TransportUnrecoverable);
    }
}

/// Runs on every platform, unlike the module above: the retry loop that
/// produces `classify_gave_up_connect`'s input only exists on Windows,
/// but the mapping itself is a plain function of a `bool`, and pinning it
/// here is the only verification this behavior can get on a machine that
/// cannot run the Windows pipe path at all.
#[cfg(test)]
mod classify_tests {
    use super::*;

    #[test]
    fn test_classify_gave_up_connect_maps_pipe_busy_to_busy() {
        assert!(matches!(classify_gave_up_connect(true), ProbeOutcome::Busy));
    }

    #[test]
    fn test_classify_gave_up_connect_maps_anything_else_to_no_owner() {
        assert!(matches!(
            classify_gave_up_connect(false),
            ProbeOutcome::NoOwner
        ));
    }
}

/// Each overrun message is the only thing a client ever learns about work
/// its deadline cut off, so what every one of them promises is pinned
/// whole here rather than by substring: a message that named the wrong
/// consequence would read as authoritative and send the reader looking
/// for a report that is not coming, or stop them looking for one that is.
#[cfg(test)]
mod overrun_tests {
    use super::*;
    use crate::hooks::ChangeEvent;

    fn message_for(request: &Request) -> String {
        message_for_deadline(request, Duration::from_millis(1500))
    }

    fn message_for_deadline(request: &Request, deadline: Duration) -> String {
        overrun_message(request, deadline)
    }

    #[test]
    fn test_a_changed_overrun_says_its_paths_reach_a_later_flush() {
        assert_eq!(
            message_for(&Request::Changed {
                session: "s1".to_string(),
                paths: Vec::new(),
                event: ChangeEvent::Change,
            }),
            "op exceeded 1500ms; the paths are queued when the work finishes and their \
             diagnostics reach a later flush"
        );
    }

    #[test]
    fn test_a_flush_overrun_says_the_report_is_offered_again() {
        assert_eq!(
            message_for(&Request::Flush {
                session: "s1".to_string(),
            }),
            "op exceeded 1500ms; nothing confirmed this report was delivered, so the next \
             flush offers it again"
        );
    }

    #[test]
    fn test_an_ack_overrun_does_not_promise_an_advance_commit_may_refuse() {
        assert_eq!(
            message_for(&Request::Ack {
                session: "s1".to_string(),
                token: 7,
            }),
            "op exceeded 1500ms; the record advances when the work finishes, unless this \
             report was superseded or its session ended"
        );
    }

    #[test]
    fn test_an_end_session_overrun_says_the_record_is_still_dropped() {
        assert_eq!(
            message_for(&Request::EndSession {
                session: "s1".to_string(),
            }),
            "op exceeded 1500ms; the session's record is dropped when the work finishes"
        );
    }

    #[test]
    fn test_a_status_overrun_promises_nothing_beyond_the_deadline() {
        assert_eq!(message_for(&Request::Status), "op exceeded 1500ms");
    }

    /// The number is the deadline the owner was configured with, not the
    /// default it usually holds. Every other test here asks for 1500,
    /// which is what a frozen literal would answer too, so this is the
    /// one that tells the value apart from the prose around it.
    #[test]
    fn test_an_overrun_names_the_deadline_it_actually_ran_under() {
        assert_eq!(
            message_for_deadline(&Request::Status, Duration::from_millis(300)),
            "op exceeded 300ms",
            "an owner configured with a 300ms deadline that reports 1500 \
             sends a reader to the wrong setting"
        );
        assert_eq!(
            message_for_deadline(
                &Request::Flush {
                    session: "s1".to_string(),
                },
                Duration::from_millis(300),
            ),
            "op exceeded 300ms; nothing confirmed this report was delivered, so the next \
             flush offers it again",
            "the same holds for a message that carries prose after it"
        );
    }
}

/// The rules [`send_and_acknowledge`] follows without a socket: which
/// answer it acknowledges, and what it says when a caller's own bound
/// elapsed. Both are reachable as plain functions, and both are things a
/// caller acts on.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod client_rule_tests {
    use super::*;

    fn flush(session: &str) -> Request {
        Request::Flush {
            session: session.to_string(),
        }
    }

    fn answered(token: Option<u64>) -> Response {
        Response::Flush {
            context: Some("2 errors in a.rs".to_string()),
            token,
        }
    }

    #[test]
    fn test_the_acknowledgement_names_the_last_tokened_flush() {
        let requests = [flush("s1"), flush("s2")];
        let responses = [answered(Some(1)), answered(Some(2))];
        assert_eq!(
            acknowledgement_for(&requests, &responses),
            Some(Request::Ack {
                session: "s2".to_string(),
                token: 2,
            }),
            "a connection carrying two flushes leaves the earlier report \
             superseded by the later one, so acknowledging the earlier token \
             would commit a report the owner has already replaced"
        );
    }

    #[test]
    fn test_a_later_tokenless_flush_leaves_the_earlier_one_acknowledged() {
        let requests = [flush("s1"), flush("s2")];
        let responses = [answered(Some(1)), answered(None)];
        assert_eq!(
            acknowledgement_for(&requests, &responses),
            Some(Request::Ack {
                session: "s1".to_string(),
                token: 1,
            }),
            "an answer with no token implies no record change, so the last \
             answer that does is the one still owed an acknowledgement"
        );
    }

    #[test]
    fn test_no_tokened_flush_means_no_acknowledgement() {
        assert_eq!(acknowledgement_for(&[flush("s1")], &[answered(None)]), None);
    }

    #[test]
    fn test_the_timeout_message_names_the_bound_that_elapsed() {
        let Error::Transport(message) = timed_out(Duration::from_millis(1500)) else {
            panic!("a client bound that elapsed is a transport failure");
        };
        assert_eq!(message, "the hook socket did not answer within 1500ms");
    }
}

/// `probe`'s whole guarantee otherwise lived one crate away, exercised
/// only indirectly through `mcpls hook doctor`'s own tests. Unix-only:
/// the Windows connect path is `probe_connect_phase`'s separate branch,
/// covered by `classify_tests` above rather than by binding a real pipe
/// here.
#[cfg(all(test, not(windows)))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod probe_tests {
    use super::*;

    /// A `SocketIdentity` whose socket lives inside a fresh `TempDir`,
    /// built by hand rather than through `identity_for`, which would put
    /// it in the real runtime directory.
    fn temp_identity(dir: &std::path::Path) -> SocketIdentity {
        SocketIdentity {
            socket: dir.join("probe-test.sock"),
            lock: dir.join("probe-test.lock"),
            hash: "probe-test".to_string(),
        }
    }

    #[tokio::test]
    async fn test_probe_reports_no_owner_for_a_socket_that_does_not_exist() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let identity = temp_identity(dir.path());

        let outcome = probe(&identity, &Request::Status, Duration::from_millis(50)).await;

        assert!(matches!(outcome, ProbeOutcome::NoOwner), "{outcome:?}");
    }

    #[tokio::test]
    async fn test_probe_reports_busy_for_a_connection_accepted_and_never_answered() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let identity = temp_identity(dir.path());
        let listener = tokio::net::UnixListener::bind(&identity.socket).expect("bind");
        tokio::spawn(async move {
            let (stream, _addr) = listener.accept().await.expect("accept");
            // Accepted, then held open forever without being read from or
            // written to: a real owner too busy to get back to the
            // client, not a missing one.
            std::future::pending::<()>().await;
            drop(stream);
        });

        let outcome = probe(&identity, &Request::Status, Duration::from_millis(50)).await;

        assert!(matches!(outcome, ProbeOutcome::Busy), "{outcome:?}");
    }

    #[tokio::test]
    async fn test_probe_reports_unintelligible_for_a_reply_that_will_not_parse() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let identity = temp_identity(dir.path());
        let listener = tokio::net::UnixListener::bind(&identity.socket).expect("bind");
        tokio::spawn(async move {
            let (mut stream, _addr) = listener.accept().await.expect("accept");
            stream.write_all(b"not json at all\n").await.expect("write");
        });

        let outcome = probe(&identity, &Request::Status, Duration::from_millis(500)).await;

        assert!(
            matches!(outcome, ProbeOutcome::Unintelligible(_)),
            "{outcome:?}"
        );
    }
}
