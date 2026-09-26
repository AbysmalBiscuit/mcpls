//! Starting, stopping and inspecting a checkout's backend from the command
//! line, as `mcpls backend start`, `stop`, `auto` and `status` do.
//!
//! A started backend is kept: it stays up with no session attached until a
//! stop or a release, instead of exiting on its idle timer.

use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::time::Instant;

use crate::backend::handshake::{self, Handshake, HandshakeReply};
use crate::backend::spawn::{self, BackendLaunch, SpawnLock};
use crate::hooks::{SocketIdentity, listener};

/// How long one connect attempt may take before the endpoint counts as busy.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// How long a start waits for the spawn lock, and then for its backend.
const START_WAIT: Duration = Duration::from_secs(10);

/// How long a stopped backend has to release the endpoint.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// What a handshake on the endpoint found.
#[derive(Debug)]
pub enum Found {
    /// Nothing answered.
    Absent,
    /// A connection could not be made within the connect timeout.
    Busy,
    /// A server answered; the reply may still refuse the request.
    Answered(HandshakeReply),
}

/// The backend a start left running.
#[derive(Debug)]
pub struct Started {
    /// Its answer to the request to stay up.
    pub reply: HandshakeReply,
    /// Whether this start spawned it.
    pub spawned: bool,
}

/// Why a start left no backend running.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// Another process held the spawn lock for the whole wait.
    #[error("another mcpls held the lock for starting this backend for over {}s", START_WAIT.as_secs())]
    Locked,
    /// The endpoint stayed busy.
    #[error("the endpoint stayed busy")]
    Busy,
    /// The spawned backend exited, or did not answer in time.
    #[error("the backend did not answer within {}s of being started", START_WAIT.as_secs())]
    NoAnswer,
    /// The spawn lock or the process could not be set up.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Whether the endpoint's owner answers, and what it says.
pub async fn status(identity: &SocketIdentity) -> Found {
    ask(identity, &Handshake::hook()).await
}

/// Stop keeping the backend, so it exits on its idle timer once no session
/// is attached.
pub async fn release(identity: &SocketIdentity) -> Found {
    ask(identity, &Handshake::release()).await
}

/// Start a kept backend for `launch` with `exe`, or keep the one already
/// running.
///
/// # Errors
///
/// Returns why no backend answered.
pub async fn start(
    identity: &SocketIdentity,
    exe: &Path,
    launch: &BackendLaunch,
) -> Result<Started, StartError> {
    let Some(_lock) = SpawnLock::acquire(&identity.spawn_lock(), START_WAIT).await? else {
        return Err(StartError::Locked);
    };
    match ask(identity, &Handshake::keep()).await {
        Found::Answered(reply) => {
            return Ok(Started {
                reply,
                spawned: false,
            });
        }
        Found::Busy => return Err(StartError::Busy),
        Found::Absent => {}
    }
    let child = spawn::spawn_detached_with_status(exe, launch, &identity.log_file())?;
    let deadline = Instant::now() + START_WAIT;
    while Instant::now() < deadline && !child.exited.load(Ordering::Acquire) {
        if let Found::Answered(reply) = ask(identity, &Handshake::keep()).await {
            return Ok(Started {
                reply,
                spawned: true,
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(StartError::NoAnswer)
}

/// What a stop did.
#[derive(Debug)]
pub enum Stopped {
    /// Nothing answered.
    NotRunning,
    /// The endpoint stayed busy.
    Busy,
    /// The backend exited and released the endpoint.
    Stopped(HandshakeReply),
    /// The backend agreed to exit but still held the endpoint at the
    /// deadline.
    Lingering(HandshakeReply),
    /// The server refused; the reply says why. A backend refusing for its
    /// attached sessions is no longer kept, and exits once they leave.
    Refused(HandshakeReply),
}

/// Ask the backend to exit; with `force`, whatever sessions are attached.
pub async fn stop(identity: &SocketIdentity, force: bool) -> Stopped {
    let request = if force {
        Handshake::forced_shutdown()
    } else {
        Handshake::shutdown()
    };
    let reply = match ask(identity, &request).await {
        Found::Absent => return Stopped::NotRunning,
        Found::Busy => return Stopped::Busy,
        Found::Answered(reply) if reply.refusal.is_some() => return Stopped::Refused(reply),
        Found::Answered(reply) => reply,
    };
    let deadline = Instant::now() + STOP_WAIT;
    while Instant::now() < deadline {
        if !matches!(
            tokio::time::timeout(CONNECT_TIMEOUT, listener::connect(identity)).await,
            Ok(Ok(_))
        ) {
            return Stopped::Stopped(reply);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Stopped::Lingering(reply)
}

/// Open a connection, send `request`, and read the reply.
///
/// A connection accepted and then closed without a reply is a backend on
/// its way out, so it counts as absent, as it does for a frontend.
async fn ask(identity: &SocketIdentity, request: &Handshake) -> Found {
    let mut stream = match tokio::time::timeout(CONNECT_TIMEOUT, listener::connect(identity)).await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) => return Found::Absent,
        Err(_) => return Found::Busy,
    };
    if handshake::write(&mut stream, request).await.is_err() {
        return Found::Absent;
    }
    match tokio::time::timeout(handshake::HANDSHAKE_TIMEOUT, handshake::read(&mut stream)).await {
        Ok(Ok(reply)) => Found::Answered(reply),
        _ => Found::Absent,
    }
}
