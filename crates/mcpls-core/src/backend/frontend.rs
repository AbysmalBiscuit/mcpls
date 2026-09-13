//! The process a host launches: attach to the project's backend and relay
//! MCP traffic, or explain why there is no backend.
#![cfg_attr(not(test), allow(dead_code))]

use std::cmp::Ordering;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::time::Instant;

use crate::backend::handshake::{self, Handshake, HandshakeReply, Refusal, compare_builds};
use crate::hooks::listener::HookStream;

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
    /// How long an evicted backend has to release the endpoint.
    pub(crate) gone: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            start: Duration::from_secs(10),
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
        return ConnectionAttempt::Busy;
    }
    let Ok(Ok(reply)) =
        tokio::time::timeout(handshake::HANDSHAKE_TIMEOUT, handshake::read(&mut stream)).await
    else {
        return ConnectionAttempt::Busy;
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
