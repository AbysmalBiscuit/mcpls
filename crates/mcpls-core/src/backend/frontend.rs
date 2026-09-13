//! The process a host launches: attach to the project's backend and relay
//! MCP traffic, or explain why there is no backend.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::backend::handshake::{
    self, ConfigStamp, Handshake, HandshakeReply, Refusal, compare_builds,
};
use crate::backend::spawn::{BackendLaunch, SpawnLock};
use crate::backend::stub;
use crate::bridge::SessionId;
use crate::hooks::listener::HookStream;
use crate::hooks::{self, SocketIdentity};

/// How long one connect attempt may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// How a frontend reaches and starts a backend.
pub(crate) trait Door: Send + Sync + 'static {
    fn connect(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>>;
    fn lock(&self, wait: Duration) -> BoxFuture<'_, io::Result<Option<Box<dyn Send>>>>;
    fn start(&self) -> BoxFuture<'_, io::Result<Start>>;
    fn place(&self) -> Place;
}

/// What starting a backend did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Start {
    /// A backend process was spawned.
    Spawned,
    /// A hook was asked to spawn one.
    #[cfg_attr(not(windows), allow(dead_code))]
    Requested,
}

/// Where the messages point a reader.
pub(crate) struct Place {
    pub(crate) root: PathBuf,
    pub(crate) log: PathBuf,
}

/// The frontend's waits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// How long a spawned backend has to answer.
    pub(crate) start: Duration,
    /// How often a waiting frontend tries again.
    pub(crate) retry: Duration,
    /// How long an evicted backend has to release the endpoint.
    pub(crate) gone: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            start: Duration::from_secs(10),
            retry: Duration::from_millis(500),
            gone: Duration::from_secs(5),
        }
    }
}

/// How an attach attempt ended.
pub(crate) enum Outcome {
    Attached(#[cfg_attr(test, allow(dead_code))] Box<dyn HookStream>),
    Waiting(String),
    Failed(String),
}

/// What the host launches mcpls with.
pub struct FrontendOptions {
    /// How to start a backend, including the checkout root.
    pub launch: BackendLaunch,
    /// This process's configuration.
    pub stamp: ConfigStamp,
}

/// Relay this process's stdio to the project's backend until the host
/// closes stdin.
pub async fn run_frontend(options: FrontendOptions) {
    let request = Handshake::mcp(
        options.launch.root.clone(),
        SessionId::from_host_env().map(|session| session.to_string()),
        options.stamp,
    );
    let door: Arc<dyn Door> = match (
        hooks::identity_for(&options.launch.root),
        std::env::current_exe(),
    ) {
        (Ok(identity), Ok(exe)) => Arc::new(ProcessDoor {
            identity,
            launch: options.launch,
            exe,
        }),
        (Err(error), _) => Arc::new(Unreachable(options.launch.root, error.to_string())),
        (_, Err(error)) => Arc::new(Unreachable(options.launch.root, error.to_string())),
    };
    relay(
        tokio::io::stdin(),
        tokio::io::stdout(),
        door,
        request,
        Timing::default(),
    )
    .await;
}

struct ProcessDoor {
    identity: SocketIdentity,
    launch: BackendLaunch,
    #[cfg_attr(windows, allow(dead_code))]
    exe: PathBuf,
}

impl Door for ProcessDoor {
    fn connect(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>> {
        Box::pin(hooks::listener::connect(&self.identity))
    }

    fn lock(&self, wait: Duration) -> BoxFuture<'_, io::Result<Option<Box<dyn Send>>>> {
        Box::pin(async move {
            Ok(SpawnLock::acquire(&self.identity.spawn_lock(), wait)
                .await?
                .map(|lock| Box::new(lock) as Box<dyn Send>))
        })
    }

    fn start(&self) -> BoxFuture<'_, io::Result<Start>> {
        Box::pin(async move {
            #[cfg(windows)]
            {
                crate::backend::spawn::request_start(&self.identity, &self.launch)?;
                Ok(Start::Requested)
            }
            #[cfg(not(windows))]
            {
                crate::backend::spawn::spawn_detached(
                    &self.exe,
                    &self.launch,
                    &self.identity.log_file(),
                )?;
                Ok(Start::Spawned)
            }
        })
    }

    fn place(&self) -> Place {
        Place {
            root: self.launch.root.clone(),
            log: self.identity.log_file(),
        }
    }
}

/// A frontend that cannot derive its endpoint at all.
struct Unreachable(PathBuf, String);

impl Door for Unreachable {
    fn connect(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>> {
        Box::pin(async { Err(io::ErrorKind::NotFound.into()) })
    }

    fn lock(&self, _wait: Duration) -> BoxFuture<'_, io::Result<Option<Box<dyn Send>>>> {
        let reason = self.1.clone();
        Box::pin(async move { Err(io::Error::other(reason)) })
    }

    fn start(&self) -> BoxFuture<'_, io::Result<Start>> {
        let reason = self.1.clone();
        Box::pin(async move { Err(io::Error::other(reason)) })
    }

    fn place(&self) -> Place {
        Place {
            root: self.0.clone(),
            log: PathBuf::new(),
        }
    }
}

/// The id a replayed `initialize` goes out under, whose answer the host
/// already had from the stub.
const REPLAY_ID: &str = "mcpls-frontend-replay";

enum Event {
    Host(String),
    HostClosed,
    Attach(Outcome),
    /// A line from the backend stream the numbered attach opened.
    Backend(u64, String),
    BackendClosed(u64),
}

enum State {
    Connecting,
    Attached,
    Waiting(String),
    Failed(String),
}

/// Relay `host_in` and `host_out` to the project's backend.
#[allow(clippy::too_many_lines)]
pub(crate) async fn relay<R, W>(
    host_in: R,
    mut host_out: W,
    door: Arc<dyn Door>,
    request: Handshake,
    timing: Timing,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send,
{
    let (events, mut inbox) = mpsc::unbounded_channel();
    spawn_lines(host_in, events.clone(), Event::Host, Event::HostClosed);
    spawn_attach(
        Arc::clone(&door),
        request.clone(),
        timing,
        events.clone(),
        Duration::ZERO,
    );

    let place = door.place();
    let mut state = State::Connecting;
    let mut backend: Option<tokio::io::WriteHalf<Box<dyn HookStream>>> = None;
    // Events from any backend stream but the latest one are stale.
    let mut generation = 0u64;
    let mut pending: HashSet<String> = HashSet::new();
    let mut deferred: Vec<(String, Value)> = Vec::new();
    // What the host sent the current backend before it sent anything back.
    let mut unheard: Vec<(String, Value)> = Vec::new();
    let mut heard = false;
    let mut reattached = false;
    let mut init: Option<String> = None;
    let mut initialized: Option<String> = None;
    let mut init_answered = false;

    while let Some(event) = inbox.recv().await {
        match event {
            Event::HostClosed => return,
            Event::Host(line) => {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match message.get("method").and_then(Value::as_str) {
                    Some("initialize") => init = Some(line.clone()),
                    Some("notifications/initialized") => initialized = Some(line.clone()),
                    _ => {}
                }
                match &state {
                    State::Connecting => deferred.push((line, message)),
                    State::Attached => {
                        if let Some(writer) = backend.as_mut() {
                            track(&mut pending, &message);
                            if write_line(writer, &line).await.is_err() {
                                let _ = events.send(Event::BackendClosed(generation));
                            }
                            if !heard {
                                unheard.push((line, message));
                            }
                        }
                    }
                    State::Waiting(reason) | State::Failed(reason) => {
                        if let Some(reply) = stub::answer(&message, reason) {
                            init_answered |= message["method"] == "initialize";
                            write_value(&mut host_out, &reply).await;
                        }
                    }
                }
            }
            Event::Attach(Outcome::Attached(stream)) => {
                generation += 1;
                let current = generation;
                let (reader, mut writer) = tokio::io::split(stream);
                spawn_lines(
                    reader,
                    events.clone(),
                    move |line| Event::Backend(current, line),
                    Event::BackendClosed(current),
                );
                heard = false;
                if init_answered {
                    if let Some(line) = &init
                        && let Ok(mut replay) = serde_json::from_str::<Value>(line)
                    {
                        replay["id"] = Value::from(REPLAY_ID);
                        let _ = write_line(&mut writer, &replay.to_string()).await;
                    }
                    if let Some(line) = &initialized {
                        let _ = write_line(&mut writer, line).await;
                    }
                }
                for (line, message) in std::mem::take(&mut deferred) {
                    track(&mut pending, &message);
                    let _ = write_line(&mut writer, &line).await;
                    unheard.push((line, message));
                }
                backend = Some(writer);
                state = State::Attached;
            }
            Event::Attach(Outcome::Waiting(reason)) => {
                answer_from_stub(&mut host_out, &mut deferred, &reason, &mut init_answered).await;
                state = State::Waiting(reason);
                spawn_attach(
                    Arc::clone(&door),
                    request.clone(),
                    timing,
                    events.clone(),
                    timing.retry,
                );
            }
            Event::Attach(Outcome::Failed(reason)) => {
                answer_from_stub(&mut host_out, &mut deferred, &reason, &mut init_answered).await;
                state = State::Failed(reason);
            }
            Event::Backend(from, line) => {
                if from != generation {
                    continue;
                }
                heard = true;
                unheard.clear();
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message.get("method").is_none()
                    && let Some(id) = message.get("id")
                {
                    if id.as_str() == Some(REPLAY_ID) {
                        continue;
                    }
                    pending.remove(&id.to_string());
                }
                write_raw(&mut host_out, &line).await;
            }
            Event::BackendClosed(from) => {
                if from != generation || !matches!(state, State::Attached) {
                    continue;
                }
                backend = None;
                if !heard && !reattached {
                    reattached = true;
                    pending.clear();
                    deferred = std::mem::take(&mut unheard);
                    state = State::Connecting;
                    spawn_attach(
                        Arc::clone(&door),
                        request.clone(),
                        timing,
                        events.clone(),
                        Duration::ZERO,
                    );
                    continue;
                }
                unheard.clear();
                let reason = messages::backend_stopped(&place);
                for id in pending.drain() {
                    let Ok(id) = serde_json::from_str::<Value>(&id) else {
                        continue;
                    };
                    let reply = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32603, "message": reason},
                    });
                    write_value(&mut host_out, &reply).await;
                }
                state = State::Failed(reason);
            }
        }
    }
}

/// Record `message`'s id as awaiting the backend's answer, if it is a
/// request.
fn track(pending: &mut HashSet<String>, message: &Value) {
    if let (Some(id), Some(_)) = (message.get("id"), message.get("method")) {
        pending.insert(id.to_string());
    }
}

async fn answer_from_stub<W: AsyncWrite + Unpin>(
    host: &mut W,
    deferred: &mut Vec<(String, Value)>,
    reason: &str,
    init_answered: &mut bool,
) {
    for (_, message) in deferred.drain(..) {
        if let Some(reply) = stub::answer(&message, reason) {
            *init_answered |= message["method"] == "initialize";
            write_value(host, &reply).await;
        }
    }
}

fn spawn_attach(
    door: Arc<dyn Door>,
    request: Handshake,
    timing: Timing,
    events: mpsc::UnboundedSender<Event>,
    after: Duration,
) {
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        let outcome = attach(door.as_ref(), &request, &timing).await;
        let _ = events.send(Event::Attach(outcome));
    });
}

fn spawn_lines<R>(
    reader: R,
    events: mpsc::UnboundedSender<Event>,
    line: impl Fn(String) -> Event + Send + 'static,
    closed: Event,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(text)) = lines.next_line().await {
            if events.send(line(text)).is_err() {
                return;
            }
        }
        let _ = events.send(closed);
    });
}

async fn write_line<W: AsyncWrite + Unpin + ?Sized>(writer: &mut W, line: &str) -> io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

async fn write_raw<W: AsyncWrite + Unpin>(host: &mut W, line: &str) {
    if let Err(error) = write_line(host, line).await {
        tracing::debug!(%error, "the host's stdout closed");
    }
}

async fn write_value<W: AsyncWrite + Unpin>(host: &mut W, value: &Value) {
    write_raw(host, &value.to_string()).await;
}

async fn handshake_with(
    door: &dyn Door,
    request: &Handshake,
) -> ConnectionAttempt<(Box<dyn HookStream>, HandshakeReply)> {
    let mut stream = match connect_with(door).await {
        ConnectionAttempt::Connected(stream) => stream,
        ConnectionAttempt::Absent => return ConnectionAttempt::Absent,
        ConnectionAttempt::Busy => return ConnectionAttempt::Busy,
    };
    if handshake::write(&mut stream, request).await.is_err() {
        return ConnectionAttempt::Absent;
    }
    let Ok(Ok(reply)) =
        tokio::time::timeout(handshake::HANDSHAKE_TIMEOUT, handshake::read(&mut stream)).await
    else {
        return ConnectionAttempt::Absent;
    };
    ConnectionAttempt::Connected((stream, reply))
}

enum ConnectionAttempt<T> {
    Connected(T),
    Absent,
    Busy,
}

async fn connect_with(door: &dyn Door) -> ConnectionAttempt<Box<dyn HookStream>> {
    match tokio::time::timeout(CONNECT_TIMEOUT, door.connect()).await {
        Ok(Ok(stream)) => ConnectionAttempt::Connected(stream),
        Ok(Err(_)) => ConnectionAttempt::Absent,
        Err(_) => ConnectionAttempt::Busy,
    }
}

enum Judged {
    Done(Outcome),
    /// An idle older backend was asked to exit and released the endpoint.
    Evicted,
}

/// Attach to the project's backend, starting one when none answers.
pub(crate) async fn attach(door: &dyn Door, request: &Handshake, timing: &Timing) -> Outcome {
    let place = door.place();
    match handshake_with(door, request).await {
        ConnectionAttempt::Connected(found) => {
            if let Judged::Done(outcome) = judge(door, request, found, timing, true).await {
                return outcome;
            }
        }
        ConnectionAttempt::Absent => {}
        ConnectionAttempt::Busy => return Outcome::Failed(messages::busy_endpoint(&place)),
    }
    let _lock = match door.lock(timing.start).await {
        Ok(Some(lock)) => lock,
        Ok(None) => return Outcome::Failed(messages::busy_starting(&place, timing.start)),
        Err(error) => return Outcome::Failed(messages::start_failed(&place, &error)),
    };
    match handshake_with(door, request).await {
        ConnectionAttempt::Connected(found) => {
            if let Judged::Done(outcome) = judge(door, request, found, timing, false).await {
                return outcome;
            }
        }
        ConnectionAttempt::Absent => {}
        ConnectionAttempt::Busy => return Outcome::Failed(messages::busy_endpoint(&place)),
    }
    match door.start().await {
        Err(error) => Outcome::Failed(messages::start_failed(&place, &error)),
        Ok(Start::Requested) => Outcome::Waiting(messages::waiting_for_hook(&place)),
        Ok(Start::Spawned) => {
            let deadline = Instant::now() + timing.start;
            loop {
                match handshake_with(door, request).await {
                    ConnectionAttempt::Connected(found) => {
                        if let Judged::Done(outcome) =
                            judge(door, request, found, timing, false).await
                        {
                            return outcome;
                        }
                    }
                    ConnectionAttempt::Absent | ConnectionAttempt::Busy => {}
                }
                if Instant::now() >= deadline {
                    return Outcome::Failed(messages::did_not_start(&place, timing.start));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn judge(
    door: &dyn Door,
    request: &Handshake,
    (stream, reply): (Box<dyn HookStream>, HandshakeReply),
    timing: &Timing,
    may_evict: bool,
) -> Judged {
    let newer = compare_builds(
        (request.mcpls, &request.version),
        (reply.mcpls, &reply.version),
    ) == Ordering::Greater;
    match &reply.refusal {
        None => Judged::Done(Outcome::Attached(stream)),
        Some(Refusal::Build) if may_evict && newer && reply.sessions == 0 => {
            drop(stream);
            if evict(door, request, timing).await {
                Judged::Evicted
            } else {
                Judged::Done(Outcome::Failed(messages::refused(
                    &door.place(),
                    request,
                    &reply,
                )))
            }
        }
        Some(_) => Judged::Done(Outcome::Failed(messages::refused(
            &door.place(),
            request,
            &reply,
        ))),
    }
}

/// Ask an idle backend to exit and wait for its endpoint to go.
async fn evict(door: &dyn Door, request: &Handshake, timing: &Timing) -> bool {
    let shutdown = Handshake {
        kind: handshake::ConnectionKind::Shutdown,
        ..request.clone()
    };
    let reply = match handshake_with(door, &shutdown).await {
        ConnectionAttempt::Connected((_stream, reply)) => reply,
        ConnectionAttempt::Absent => return true,
        ConnectionAttempt::Busy => return false,
    };
    if reply.refusal.is_some() {
        return false;
    }
    let deadline = Instant::now() + timing.gone;
    while Instant::now() < deadline {
        match connect_with(door).await {
            ConnectionAttempt::Connected(stream) => drop(stream),
            ConnectionAttempt::Absent => return true,
            ConnectionAttempt::Busy => return false,
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

mod messages {
    use std::cmp::Ordering;
    use std::time::Duration;

    use super::Place;
    use crate::backend::handshake::{
        ConfigStamp, Handshake, HandshakeReply, Refusal, compare_builds,
    };
    use crate::config::ConfigSource;

    pub(super) fn start_failed(place: &Place, error: &std::io::Error) -> String {
        format!(
            "Tell the user: mcpls could not start a backend for {}: {error}. This session has no \
             mcpls tools. Restart the session after fixing it, or run mcpls with --no-backend.",
            place.root.display()
        )
    }

    pub(super) fn did_not_start(place: &Place, wait: Duration) -> String {
        format!(
            "Tell the user: the mcpls backend for {} did not answer within {}s of being started, \
             so this session has no mcpls tools. Its log is {}. Restart the session after fixing \
             it, or run mcpls with --no-backend.",
            place.root.display(),
            wait.as_secs(),
            place.log.display()
        )
    }

    pub(super) fn busy_starting(place: &Place, wait: Duration) -> String {
        format!(
            "Tell the user: another mcpls held the lock for starting the backend for {} for over \
             {}s, so this session has no mcpls tools. Its log is {}.",
            place.root.display(),
            wait.as_secs(),
            place.log.display()
        )
    }

    pub(super) fn busy_endpoint(place: &Place) -> String {
        format!(
            "Tell the user: the mcpls backend endpoint for {} stayed busy while this session \
             tried to attach, so this session has no mcpls tools. Restart the session after the \
             other mcpls session finishes, or run mcpls with --no-backend.",
            place.root.display()
        )
    }

    pub(super) fn waiting_for_hook(place: &Place) -> String {
        format!(
            "mcpls is waiting for its backend for {}. On Windows a hook starts it, so the mcpls \
             plugin's hooks must be installed; tools answer once one has fired. If they are not \
             installed, tell the user to run mcpls with --no-backend.",
            place.root.display()
        )
    }

    pub(super) fn backend_stopped(place: &Place) -> String {
        format!(
            "Tell the user: the mcpls backend for {} stopped while this session was attached, so \
             this session has no mcpls tools. Restart the session to start a new one. Its log is \
             {}.",
            place.root.display(),
            place.log.display()
        )
    }

    fn trust_state(stamp: &ConfigStamp) -> &'static str {
        if stamp.source == ConfigSource::Project {
            "loaded the project's mcpls.toml as trusted"
        } else if stamp.project_ignored {
            "ignored the project's mcpls.toml as untrusted"
        } else {
            "does not use the project's mcpls.toml"
        }
    }

    pub(super) fn refused(place: &Place, request: &Handshake, reply: &HandshakeReply) -> String {
        let root = place.root.display();
        match &reply.refusal {
            Some(Refusal::Build) => match compare_builds(
                (request.mcpls, &request.version),
                (reply.mcpls, &reply.version),
            ) {
                Ordering::Less => format!(
                    "Tell the user: this session's mcpls is version {}, older than the version {} \
                     backend already serving {root}, so this session has no mcpls tools. Update \
                     the mcpls this session launches, or restart the session with the newer one.",
                    request.version, reply.version
                ),
                _ => format!(
                    "Tell the user: this session's mcpls is version {} but the backend serving \
                     {root} is version {} with {} other session(s) attached, so this session has \
                     no mcpls tools. Restart those sessions to upgrade the backend, then restart \
                     this one.",
                    request.version, reply.version, reply.sessions
                ),
            },
            Some(Refusal::Trust { backend }) => format!(
                "Tell the user: the mcpls backend serving {root} {}, but this session's mcpls {}. \
                 Sessions that disagree about trusting the project's mcpls.toml cannot share a \
                 backend. Start every session in this checkout with the same \
                 --trust-project-config setting.",
                trust_state(backend),
                request
                    .config
                    .as_ref()
                    .map_or("has no configuration", trust_state)
            ),
            other => format!(
                "Tell the user: the mcpls backend serving {root} refused this session ({other:?}), \
                 so this session has no mcpls tools."
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod fake {
    use std::collections::VecDeque;
    use std::io;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures::future::BoxFuture;
    use rmcp::ServiceExt as _;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::{Door, Place, Start, Timing};
    use crate::backend::handshake::{
        self, ConfigStamp, ConnectionKind, Handshake, HandshakeReply, Refusal,
    };
    use crate::hooks::listener::HookStream;

    /// What one connect attempt reaches.
    pub(super) enum Script {
        /// Nothing listens.
        Nobody,
        /// A connect attempt remains pending because the endpoint is busy.
        Busy,
        /// A server answering the handshake with the reply, then doing
        /// what `Then` says.
        Server(HandshakeReply, Then),
    }

    pub(super) enum Then {
        /// Serve a real `McplsServer` on the stream.
        Serve,
        /// Close at once.
        Close,
        /// Close after reading the request and before sending a reply.
        CloseBeforeReply,
        /// Close as soon as the first MCP line arrives, having sent nothing.
        CloseOnFirstLine,
        /// Answer the first MCP line as an `initialize`, then close as soon
        /// as the next line arrives.
        AnswerOnceThenClose,
    }

    /// A door whose connect attempts follow a script, and which counts the
    /// starts it is asked for and the connection kinds it saw.
    pub(super) struct FakeDoor {
        scripts: Mutex<VecDeque<Script>>,
        start: Start,
        pub(super) starts: Mutex<usize>,
        pub(super) kinds: Arc<Mutex<Vec<ConnectionKind>>>,
    }

    impl FakeDoor {
        pub(super) fn new(start: Start, scripts: Vec<Script>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(scripts.into()),
                start,
                starts: Mutex::new(0),
                kinds: Arc::new(Mutex::new(Vec::new())),
            })
        }
    }

    impl Door for FakeDoor {
        fn connect(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>> {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Script::Nobody);
            let kinds = Arc::clone(&self.kinds);
            Box::pin(async move {
                let (reply, then) = match script {
                    Script::Nobody => return Err(io::ErrorKind::ConnectionRefused.into()),
                    Script::Busy => {
                        return std::future::pending::<io::Result<Box<dyn HookStream>>>().await;
                    }
                    Script::Server(reply, then) => (reply, then),
                };
                let (client, mut server) = tokio::io::duplex(1 << 20);
                tokio::spawn(async move {
                    let Ok(request) = handshake::read::<_, Handshake>(&mut server).await else {
                        return;
                    };
                    kinds.lock().unwrap().push(request.kind);
                    if matches!(&then, Then::CloseBeforeReply) {
                        return;
                    }
                    if handshake::write(&mut server, &reply).await.is_err() {
                        return;
                    }
                    match then {
                        Then::Serve => {
                            if let Ok(running) = test_server().serve(server).await {
                                let _ = running.waiting().await;
                            }
                        }
                        Then::Close => {}
                        Then::CloseBeforeReply => unreachable!(),
                        Then::CloseOnFirstLine => {
                            let mut lines = BufReader::new(server).lines();
                            let _ = lines.next_line().await;
                        }
                        Then::AnswerOnceThenClose => {
                            let (reader, mut writer) = tokio::io::split(server);
                            let mut lines = BufReader::new(reader).lines();
                            let Ok(Some(first)) = lines.next_line().await else {
                                return;
                            };
                            let first: Value = serde_json::from_str(&first).unwrap();
                            let answer = json!({"jsonrpc":"2.0","id":first["id"],"result":{
                                "protocolVersion":"2025-11-25",
                                "capabilities":{},
                                "serverInfo":{"name":"mcpls","version":"0"},
                            }});
                            let _ = writer.write_all(format!("{answer}\n").as_bytes()).await;
                            let _ = lines.next_line().await;
                        }
                    }
                });
                Ok(Box::new(client) as Box<dyn HookStream>)
            })
        }

        fn lock(&self, _wait: Duration) -> BoxFuture<'_, io::Result<Option<Box<dyn Send>>>> {
            Box::pin(async { Ok(Some(Box::new(()) as Box<dyn Send>)) })
        }

        fn start(&self) -> BoxFuture<'_, io::Result<Start>> {
            *self.starts.lock().unwrap() += 1;
            let start = self.start;
            Box::pin(async move { Ok(start) })
        }

        fn place(&self) -> Place {
            Place {
                root: PathBuf::from("/work"),
                log: PathBuf::from("/run/mcpls/x.log"),
            }
        }
    }

    fn test_server() -> crate::mcp::McplsServer {
        use crate::bridge::{
            DiagnosticsDelivery, FloorTable, NotificationCache, ResourceSubscriptions,
            ServerSettle, Translator,
        };
        use crate::config::DiagnosticsConfig;
        crate::mcp::McplsServer::new(
            Arc::new(Translator::new()),
            Arc::new(tokio::sync::Mutex::new(NotificationCache::new())),
            Arc::from(Vec::new()),
            Arc::new(ResourceSubscriptions::new()),
            false,
            Arc::new(tokio::sync::Mutex::new(DiagnosticsDelivery::new(
                DiagnosticsConfig::default(),
            ))),
            Arc::new(FloorTable::new(&DiagnosticsConfig::default(), &[])),
            DiagnosticsConfig::default(),
            Arc::new(ServerSettle::new(
                Duration::from_secs(1),
                Duration::from_secs(300),
            )),
        )
    }

    pub(super) fn accepted() -> HandshakeReply {
        HandshakeReply::new(0, None)
    }

    pub(super) fn refused(version: &str, sessions: usize, refusal: Refusal) -> HandshakeReply {
        HandshakeReply {
            version: version.to_string(),
            ..HandshakeReply::new(sessions, Some(refusal))
        }
    }

    pub(super) fn fast() -> Timing {
        Timing {
            start: Duration::from_millis(300),
            retry: Duration::from_millis(20),
            gone: Duration::from_millis(300),
        }
    }

    pub(super) fn request() -> Handshake {
        Handshake::mcp(
            PathBuf::from("/work"),
            None,
            ConfigStamp::of(&crate::config::ServerConfig::default()),
        )
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::field_reassign_with_default
)]
mod attach_tests {
    use std::path::PathBuf;

    use super::fake::{FakeDoor, Script, Then, accepted, fast, refused, request};
    use super::{Outcome, Start, attach};
    use crate::backend::handshake::{
        self, ConfigStamp, ConnectionKind, Handshake, HandshakeReply, Refusal,
    };

    fn failure(outcome: Outcome) -> String {
        match outcome {
            Outcome::Failed(text) => text,
            Outcome::Waiting(text) => panic!("expected a failure, got a wait: {text}"),
            Outcome::Attached(_) => panic!("expected a failure, got an attached backend"),
        }
    }

    #[tokio::test]
    async fn test_a_backend_that_never_starts_is_reported_with_its_log() {
        let door = FakeDoor::new(Start::Spawned, vec![]);
        let text = failure(attach(door.as_ref(), &request(), &fast()).await);
        assert!(text.contains("/run/mcpls/x.log"), "{text}");
        assert_eq!(*door.starts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_a_requested_start_waits_for_a_hook() {
        let door = FakeDoor::new(Start::Requested, vec![]);
        let Outcome::Waiting(text) = attach(door.as_ref(), &request(), &fast()).await else {
            panic!("a requested start waits");
        };
        assert!(text.contains("hook"), "{text}");
    }

    #[tokio::test]
    async fn test_a_busy_initial_endpoint_does_not_start_another_backend() {
        let door = FakeDoor::new(Start::Spawned, vec![Script::Busy]);
        let text = failure(attach(door.as_ref(), &request(), &fast()).await);
        assert!(text.contains("stayed busy"), "{text}");
        assert_eq!(*door.starts.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_a_connected_endpoint_that_closes_before_reply_starts_a_backend() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Server(accepted(), Then::CloseBeforeReply),
                Script::Nobody,
                Script::Server(accepted(), Then::Serve),
            ],
        );
        let outcome = attach(door.as_ref(), &request(), &fast()).await;
        assert!(matches!(outcome, Outcome::Attached(_)));
        assert_eq!(*door.starts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_an_idle_older_backend_is_evicted_and_replaced() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Server(refused("0.0.1", 0, Refusal::Build), Then::Close),
                Script::Server(accepted(), Then::Close),
                Script::Nobody,
                Script::Nobody,
                Script::Server(accepted(), Then::Serve),
            ],
        );
        let outcome = attach(door.as_ref(), &request(), &fast()).await;
        assert!(matches!(outcome, Outcome::Attached(_)));
        assert!(
            door.kinds
                .lock()
                .unwrap()
                .contains(&ConnectionKind::Shutdown)
        );
        assert_eq!(*door.starts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_a_busy_endpoint_is_not_reported_as_gone_during_eviction() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Server(refused("0.0.1", 0, Refusal::Build), Then::Close),
                Script::Server(accepted(), Then::Close),
                Script::Busy,
            ],
        );
        let text = failure(attach(door.as_ref(), &request(), &fast()).await);
        assert!(text.contains("0.0.1"), "{text}");
        assert!(
            door.kinds
                .lock()
                .unwrap()
                .contains(&ConnectionKind::Shutdown)
        );
        assert_eq!(*door.starts.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_eviction_treats_a_connected_close_before_reply_as_gone() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Server(refused("0.0.1", 0, Refusal::Build), Then::Close),
                Script::Server(accepted(), Then::CloseBeforeReply),
                Script::Nobody,
                Script::Server(accepted(), Then::Serve),
            ],
        );
        let outcome = attach(door.as_ref(), &request(), &fast()).await;
        assert!(matches!(outcome, Outcome::Attached(_)));
        assert!(
            door.kinds
                .lock()
                .unwrap()
                .contains(&ConnectionKind::Shutdown)
        );
        assert_eq!(*door.starts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_an_older_backend_with_sessions_is_reported_with_both_versions() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(
                refused("0.0.1", 2, Refusal::Build),
                Then::Close,
            )],
        );
        let text = failure(attach(door.as_ref(), &request(), &fast()).await);
        assert!(
            text.contains("0.0.1") && text.contains(handshake::VERSION),
            "{text}"
        );
        assert!(
            !door
                .kinds
                .lock()
                .unwrap()
                .contains(&ConnectionKind::Shutdown)
        );
        assert_eq!(*door.starts.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_an_older_frontend_never_evicts_a_newer_backend() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(
                refused("999.0.0", 0, Refusal::Build),
                Then::Close,
            )],
        );
        let text = failure(attach(door.as_ref(), &request(), &fast()).await);
        assert!(text.contains("older"), "{text}");
        assert!(
            !door
                .kinds
                .lock()
                .unwrap()
                .contains(&ConnectionKind::Shutdown)
        );
    }

    #[tokio::test]
    async fn test_a_trust_refusal_names_both_states() {
        let mut backend = crate::config::ServerConfig::default();
        backend.source = crate::config::ConfigSource::Project;
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(
                HandshakeReply::new(
                    1,
                    Some(Refusal::Trust {
                        backend: ConfigStamp::of(&backend),
                    }),
                ),
                Then::Close,
            )],
        );
        let mut ignoring = crate::config::ServerConfig::default();
        ignoring.project_config_ignored = true;
        let request = Handshake::mcp(PathBuf::from("/work"), None, ConfigStamp::of(&ignoring));
        let text = failure(attach(door.as_ref(), &request, &fast()).await);
        assert!(text.contains("loaded the project's mcpls.toml"), "{text}");
        assert!(text.contains("ignored the project's mcpls.toml"), "{text}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod relay_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, Lines};
    use tokio::time::Instant;

    use super::fake::{FakeDoor, Script, Then, accepted, fast, refused, request};
    use super::{Door, Start, Timing, relay};
    use crate::backend::handshake::{self, Handshake, Refusal};

    struct Host {
        input: DuplexStream,
        output: Lines<BufReader<DuplexStream>>,
        relay: tokio::task::JoinHandle<()>,
    }

    impl Host {
        fn start(door: Arc<dyn Door>, request: Handshake, timing: Timing) -> Self {
            let (input, relay_in) = tokio::io::duplex(1 << 20);
            let (relay_out, output) = tokio::io::duplex(1 << 20);
            let relay = tokio::spawn(relay(relay_in, relay_out, door, request, timing));
            Self {
                input,
                output: BufReader::new(output).lines(),
                relay,
            }
        }

        async fn send(&mut self, message: Value) {
            self.input
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn response(&mut self, id: i64) -> Value {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let line = self
                        .output
                        .next_line()
                        .await
                        .unwrap()
                        .expect("the relay closed");
                    let message: Value = serde_json::from_str(&line).unwrap();
                    if message["id"] == id {
                        return message;
                    }
                }
            })
            .await
            .expect("a response arrived")
        }

        async fn initialize(&mut self) -> Value {
            self.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}})).await;
            let answer = self.response(1).await;
            self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .await;
            answer
        }

        async fn call_tool(&mut self, id: i64) -> Value {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"get_server_logs","arguments":{}}})).await;
            self.response(id).await
        }
    }

    fn initialize_request() -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}})
    }

    #[tokio::test]
    async fn test_an_attached_session_is_answered_by_the_backend() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let mut host = Host::start(door, request(), fast());
        let init = host.initialize().await;
        assert_eq!(
            init["result"]["instructions"],
            crate::mcp::INSTRUCTIONS,
            "{init}"
        );
        let call = host.call_tool(2).await;
        assert_ne!(call["result"]["isError"], true, "{call}");
    }

    #[tokio::test]
    async fn test_a_refusal_is_repeated_in_initialize_and_every_tool_call() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(
                refused("0.0.1", 2, Refusal::Build),
                Then::Close,
            )],
        );
        let mut host = Host::start(door, request(), fast());
        let init = host.initialize().await;
        let text = init["result"]["instructions"].as_str().unwrap().to_string();
        assert!(
            text.contains("0.0.1") && text.contains(handshake::VERSION),
            "{text}"
        );
        for id in [2, 3] {
            let call = host.call_tool(id).await;
            assert_eq!(call["result"]["isError"], true, "{call}");
            assert!(
                call["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("0.0.1"),
                "{call}"
            );
        }
    }

    #[tokio::test]
    async fn test_a_backend_dying_mid_request_fails_that_request_and_the_rest() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::AnswerOnceThenClose)],
        );
        let mut host = Host::start(door, request(), fast());
        host.send(initialize_request()).await;
        let init = host.response(1).await;
        assert_eq!(init["result"]["serverInfo"]["name"], "mcpls", "{init}");
        host.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_server_logs","arguments":{}}})).await;
        let failed = host.response(2).await;
        assert!(
            failed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stopped"),
            "{failed}"
        );
        let call = host.call_tool(3).await;
        assert_eq!(call["result"]["isError"], true, "{call}");
    }

    /// A backend that accepts the handshake and closes before sending
    /// anything was exiting as the frontend attached. The frontend attaches
    /// again and the host never sees the first backend.
    #[tokio::test]
    async fn test_a_backend_closing_before_its_first_line_is_attached_again() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Server(accepted(), Then::CloseOnFirstLine),
                Script::Server(accepted(), Then::Serve),
            ],
        );
        let mut host = Host::start(Arc::clone(&door) as Arc<dyn Door>, request(), fast());
        let init = host.initialize().await;
        assert_eq!(
            init["result"]["instructions"],
            crate::mcp::INSTRUCTIONS,
            "{init}"
        );
        let call = host.call_tool(2).await;
        assert_ne!(call["result"]["isError"], true, "{call}");
        assert_eq!(*door.starts.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_a_second_silent_close_fails_the_session() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Server(accepted(), Then::CloseOnFirstLine),
                Script::Server(accepted(), Then::CloseOnFirstLine),
            ],
        );
        let mut host = Host::start(door, request(), fast());
        host.send(initialize_request()).await;
        let failed = host.response(1).await;
        assert!(
            failed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stopped"),
            "{failed}"
        );
    }

    #[tokio::test]
    async fn test_a_waiting_tool_call_fails_at_once() {
        let door = FakeDoor::new(Start::Requested, vec![]);
        let mut host = Host::start(door, request(), fast());
        host.initialize().await;
        let started = Instant::now();
        let call = host.call_tool(2).await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the call waited for a backend"
        );
        assert_eq!(call["result"]["isError"], true, "{call}");
        assert!(
            call["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("hook"),
            "{call}"
        );
    }

    #[tokio::test]
    async fn test_a_waiting_session_replays_initialize_when_the_backend_arrives() {
        let door = FakeDoor::new(
            Start::Requested,
            vec![
                Script::Nobody,
                Script::Nobody,
                Script::Nobody,
                Script::Server(accepted(), Then::Serve),
            ],
        );
        let timing = Timing {
            retry: Duration::from_millis(200),
            ..fast()
        };
        let mut host = Host::start(door, request(), timing);
        let init = host.initialize().await;
        assert!(
            init["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("hook"),
            "{init}"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut id = 2;
        loop {
            let call = host.call_tool(id).await;
            if call["result"]["isError"] != true {
                break;
            }
            assert!(
                call["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("hook"),
                "{call}"
            );
            assert!(Instant::now() < deadline, "no backend attached: {call}");
            id += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn test_the_relay_ends_when_the_host_closes() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let mut host = Host::start(door, request(), fast());
        host.initialize().await;
        let Host { input, relay, .. } = host;
        drop(input);
        tokio::time::timeout(Duration::from_secs(5), relay)
            .await
            .expect("the relay returned after host EOF")
            .unwrap();
    }
}
