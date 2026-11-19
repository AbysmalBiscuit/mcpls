//! The process a host launches: attach to the project's backend and relay
//! MCP traffic, or explain why there is no backend.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::gather;
use crate::backend::handshake::{
    self, ConfigStamp, Handshake, HandshakeReply, Refusal, compare_builds,
};
use crate::backend::route::{self, Reach};
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
    #[cfg_attr(windows, allow(dead_code))]
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

/// The backend of another checkout, which a tool call's file leads to.
pub(crate) struct Route {
    /// The checkout root, which the handshake names.
    pub(crate) root: PathBuf,
    pub(crate) door: Arc<dyn Door>,
    /// Whether the frontend starts a backend there when none answers, rather
    /// than only attaching to one that runs.
    pub(crate) start: bool,
}

/// Which checkout's backend serves a tool call's file.
pub(crate) trait Router: Send + Sync + 'static {
    /// The route for `file`, or `None` when the session's backend serves it.
    fn route(&self, file: &Path) -> Option<Route>;
}

/// A router that sends every call to the session's backend.
pub(crate) struct NoRoutes;

impl Router for NoRoutes {
    fn route(&self, _file: &Path) -> Option<Route> {
        None
    }
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
    /// How long a gathered call waits for each route backend's answer.
    pub(crate) gather: Duration,
    /// How long a route backend stays attached with no file call to it.
    pub(crate) route_idle: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            start: Duration::from_secs(10),
            retry: Duration::from_millis(500),
            gone: Duration::from_secs(5),
            gather: Duration::from_secs(10),
            route_idle: Duration::from_secs(600),
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
    let (door, router): (Arc<dyn Door>, Arc<dyn Router>) = match (
        hooks::identity_for(&options.launch.root),
        std::env::current_exe(),
    ) {
        (Ok(identity), Ok(exe)) => (
            Arc::new(ProcessDoor {
                identity,
                launch: options.launch.clone(),
                exe: exe.clone(),
            }),
            Arc::new(ProcessRouter {
                launch: options.launch,
                exe,
            }),
        ),
        (Err(error), _) => (
            Arc::new(Unreachable(options.launch.root, error.to_string())),
            Arc::new(NoRoutes),
        ),
        (_, Err(error)) => (
            Arc::new(Unreachable(options.launch.root, error.to_string())),
            Arc::new(NoRoutes),
        ),
    };
    relay(
        tokio::io::stdin(),
        tokio::io::stdout(),
        door,
        router,
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

/// Routes a file in another checkout to that checkout's backend, by
/// [`route::route`].
struct ProcessRouter {
    launch: BackendLaunch,
    exe: PathBuf,
}

impl Router for ProcessRouter {
    fn route(&self, file: &Path) -> Option<Route> {
        let (root, reach) = route::route(&self.launch.root, file)?;
        let identity = hooks::identity_for(&root).ok()?;
        let launch = BackendLaunch {
            root: root.clone(),
            ..self.launch.clone()
        };
        Some(Route {
            root,
            door: Arc::new(ProcessDoor {
                identity,
                launch,
                exe: self.exe.clone(),
            }),
            start: reach == Reach::Start,
        })
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
    /// A host line for the session's backend, sent without a routing check.
    Unrouted(String),
    HostClosed,
    Attach(Outcome),
    /// A line from the backend stream the numbered attach opened.
    Backend(u64, String),
    BackendClosed(u64),
    RouteAttach {
        root: PathBuf,
        place: Place,
        /// `None` when nothing runs there and the route may not start one.
        outcome: Option<Outcome>,
    },
    RouteLine(PathBuf, u64, String),
    RouteClosed(PathBuf, u64),
    RouteIdle(PathBuf, u64),
    GatherTimeout(String),
}

enum State {
    Connecting,
    Attached,
    Waiting(String),
    Failed(String),
}

/// How a gathered call merges its parts.
#[derive(Clone, Copy)]
enum Merge {
    Diagnostics,
    Symbols { limit: usize },
}

/// A tool call that waits for the answers of several backends.
struct Gather {
    id: Value,
    parts: Vec<(usize, Value)>,
    waiting: usize,
    merge: Merge,
}

/// Another checkout's backend, as the relay holds it.
enum Other {
    /// Host calls that wait for the attach.
    Connecting(Vec<(String, Value)>),
    Attached {
        writer: tokio::io::WriteHalf<Box<dyn HookStream>>,
        place: Place,
        generation: u64,
        reader: tokio::task::AbortHandle,
        used: Instant,
    },
    Failed(String),
}

/// Relay `host_in` and `host_out` to the project's backend.
#[allow(clippy::too_many_lines)]
pub(crate) async fn relay<R, W>(
    host_in: R,
    mut host_out: W,
    door: Arc<dyn Door>,
    router: Arc<dyn Router>,
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
    let mut route_generation = 0u64;
    let mut pending: HashSet<String> = HashSet::new();
    let mut deferred: Vec<(String, Value)> = Vec::new();
    // What the host sent the current backend before it sent anything back.
    let mut unheard: Vec<(String, Value)> = Vec::new();
    let mut heard = false;
    let mut reattached = false;
    let mut init: Option<String> = None;
    let mut initialized: Option<String> = None;
    let mut init_answered = false;
    let mut others: HashMap<PathBuf, Other> = HashMap::new();
    let mut sent_to: HashMap<String, PathBuf> = HashMap::new();
    let mut gathers: HashMap<String, Gather> = HashMap::new();
    let mut gather_parts: HashMap<String, (String, usize)> = HashMap::new();
    let mut gather_ids = 0u64;

    while let Some(event) = inbox.recv().await {
        let routable = matches!(event, Event::Host(_));
        match event {
            Event::HostClosed => return,
            Event::Host(line) | Event::Unrouted(line) => {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match message.get("method").and_then(Value::as_str) {
                    Some("initialize") => init = Some(line.clone()),
                    Some("notifications/initialized") => initialized = Some(line.clone()),
                    _ => {}
                }
                if routable
                    && init.is_some()
                    && message.get("method").and_then(Value::as_str) == Some("tools/call")
                    && let Some(id) = message.get("id")
                    && let Some(file) = message
                        .pointer("/params/arguments/file_path")
                        .and_then(Value::as_str)
                    && let Some(route) = router.route(Path::new(file))
                {
                    let root = route.root.clone();
                    match others.get_mut(&root) {
                        Some(Other::Attached {
                            writer,
                            generation,
                            used,
                            ..
                        }) => {
                            *used = Instant::now();
                            sent_to.insert(id.to_string(), root.clone());
                            if write_line(writer, &line).await.is_err() {
                                let _ = events.send(Event::RouteClosed(root, *generation));
                            }
                        }
                        Some(Other::Connecting(queue)) => queue.push((line, message)),
                        Some(Other::Failed(reason)) => {
                            if let Some(reply) = stub::answer(&message, reason) {
                                write_value(&mut host_out, &reply).await;
                            }
                        }
                        None => {
                            others.insert(root, Other::Connecting(vec![(line, message)]));
                            spawn_route_attach(route, request.clone(), timing, events.clone());
                        }
                    }
                    continue;
                }
                match &state {
                    State::Connecting => deferred.push((line, message)),
                    State::Attached => {
                        if let Some(writer) = backend.as_mut() {
                            if let Some(id) = message.get("id")
                                && let Some(merge) = gather_merge(&message)
                                && others
                                    .values()
                                    .any(|other| matches!(other, Other::Attached { .. }))
                            {
                                let host_key = id.to_string();
                                let mut index = 0;
                                for (root, other) in &mut others {
                                    let Other::Attached {
                                        writer: route_writer,
                                        generation,
                                        ..
                                    } = other
                                    else {
                                        continue;
                                    };
                                    index += 1;
                                    gather_ids += 1;
                                    let part_id = Value::from(format!("mcpls-gather-{gather_ids}"));
                                    let part_key = part_id.to_string();
                                    let mut part = message.clone();
                                    part["id"] = part_id;
                                    sent_to.insert(part_key.clone(), root.clone());
                                    gather_parts.insert(part_key, (host_key.clone(), index));
                                    if write_line(route_writer, &part.to_string()).await.is_err() {
                                        let _ = events
                                            .send(Event::RouteClosed(root.clone(), *generation));
                                    }
                                }
                                let key = host_key.clone();
                                gathers.insert(
                                    host_key,
                                    Gather {
                                        id: id.clone(),
                                        parts: Vec::new(),
                                        waiting: 1 + index,
                                        merge,
                                    },
                                );
                                let events = events.clone();
                                let wait = timing.gather;
                                tokio::spawn(async move {
                                    tokio::time::sleep(wait).await;
                                    let _ = events.send(Event::GatherTimeout(key));
                                });
                            }
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
                let mut write_failed = false;
                if init_answered {
                    if let Some(line) = &init
                        && let Ok(mut replay) = serde_json::from_str::<Value>(line)
                    {
                        replay["id"] = Value::from(REPLAY_ID);
                        write_failed = write_line(&mut writer, &replay.to_string()).await.is_err();
                    }
                    if !write_failed && let Some(line) = &initialized {
                        write_failed = write_line(&mut writer, line).await.is_err();
                    }
                }
                let deferred = std::mem::take(&mut deferred);
                for (_, message) in &deferred {
                    track(&mut pending, message);
                }
                unheard.extend(deferred.iter().cloned());
                if !write_failed {
                    for (line, _) in deferred {
                        if write_line(&mut writer, &line).await.is_err() {
                            write_failed = true;
                            break;
                        }
                    }
                }
                backend = Some(writer);
                state = State::Attached;
                if write_failed {
                    let _ = events.send(Event::BackendClosed(current));
                }
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
                    let key = id.to_string();
                    pending.remove(&key);
                    if gathers.contains_key(&key) {
                        if let Some(merged) = settle_part(&mut gathers, &key, 0, message) {
                            write_value(&mut host_out, &merged).await;
                        }
                        continue;
                    }
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
                for key in pending.drain() {
                    let Ok(id) = serde_json::from_str::<Value>(&key) else {
                        continue;
                    };
                    let reply = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32603, "message": reason},
                    });
                    let answer = if gathers.contains_key(&key) {
                        settle_part(&mut gathers, &key, 0, reply)
                    } else {
                        Some(reply)
                    };
                    if let Some(answer) = answer {
                        write_value(&mut host_out, &answer).await;
                    }
                }
                state = State::Failed(reason);
            }
            Event::RouteAttach {
                root,
                place,
                outcome,
            } => {
                let Some(Other::Connecting(queue)) = others.remove(&root) else {
                    continue;
                };
                match outcome {
                    None => {
                        for (line, _) in queue {
                            let _ = events.send(Event::Unrouted(line));
                        }
                    }
                    Some(Outcome::Attached(stream)) => {
                        route_generation += 1;
                        let current = route_generation;
                        let (reader, mut writer) = tokio::io::split(stream);
                        let for_lines = root.clone();
                        let lines = spawn_lines(
                            reader,
                            events.clone(),
                            move |line| Event::RouteLine(for_lines.clone(), current, line),
                            Event::RouteClosed(root.clone(), current),
                        );
                        spawn_route_idle(root.clone(), current, timing.route_idle, events.clone());
                        let replayed = replay_route(
                            &mut writer,
                            init.as_deref(),
                            initialized.as_deref(),
                            queue,
                            &mut sent_to,
                            &root,
                        )
                        .await;
                        if replayed.is_err() {
                            let _ = events.send(Event::RouteClosed(root.clone(), current));
                        }
                        others.insert(
                            root,
                            Other::Attached {
                                writer,
                                place,
                                generation: current,
                                reader: lines,
                                used: Instant::now(),
                            },
                        );
                    }
                    Some(Outcome::Waiting(reason)) => {
                        answer_queue(&mut host_out, queue, &reason).await;
                    }
                    Some(Outcome::Failed(reason)) => {
                        answer_queue(&mut host_out, queue, &reason).await;
                        others.insert(root, Other::Failed(reason));
                    }
                }
            }
            Event::RouteLine(root, route, line) => {
                if !matches!(
                    others.get(&root),
                    Some(Other::Attached { generation, .. }) if *generation == route
                ) {
                    continue;
                }
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match (message.get("method"), message.get("id")) {
                    (None, Some(id)) => {
                        if id.as_str() == Some(REPLAY_ID) {
                            continue;
                        }
                        let key = id.to_string();
                        if sent_to.get(&key) == Some(&root) {
                            sent_to.remove(&key);
                            match gather_parts.remove(&key) {
                                Some((host_key, index)) => {
                                    if let Some(merged) =
                                        settle_part(&mut gathers, &host_key, index, message)
                                    {
                                        write_value(&mut host_out, &merged).await;
                                    }
                                }
                                None => write_raw(&mut host_out, &line).await,
                            }
                        }
                    }
                    (Some(_), None) => write_raw(&mut host_out, &line).await,
                    (Some(_), Some(_)) => {
                        tracing::debug!(root = %root.display(), "dropped a server request from a route");
                    }
                    (None, None) => {}
                }
            }
            Event::RouteClosed(root, route) => {
                let Some(Other::Attached {
                    place, generation, ..
                }) = others.get(&root)
                else {
                    continue;
                };
                if *generation != route {
                    continue;
                }
                let reason = messages::backend_stopped(place);
                others.remove(&root);
                let lost: Vec<String> = sent_to
                    .iter()
                    .filter(|(_, owner)| **owner == root)
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in lost {
                    sent_to.remove(&key);
                    let Ok(id) = serde_json::from_str::<Value>(&key) else {
                        continue;
                    };
                    let reply = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32603, "message": reason},
                    });
                    let answer = match gather_parts.remove(&key) {
                        Some((host_key, index)) => {
                            settle_part(&mut gathers, &host_key, index, reply)
                        }
                        None => Some(reply),
                    };
                    if let Some(answer) = answer {
                        write_value(&mut host_out, &answer).await;
                    }
                }
            }
            Event::RouteIdle(root, route) => {
                let Some(Other::Attached {
                    used, generation, ..
                }) = others.get(&root)
                else {
                    continue;
                };
                if *generation != route {
                    continue;
                }
                let idle_for = used.elapsed();
                let busy = sent_to.values().any(|owner| *owner == root);
                if busy {
                    spawn_route_idle(root, route, timing.route_idle, events.clone());
                } else if idle_for < timing.route_idle {
                    spawn_route_idle(
                        root,
                        route,
                        timing.route_idle.saturating_sub(idle_for),
                        events.clone(),
                    );
                } else if let Some(Other::Attached {
                    mut writer, reader, ..
                }) = others.remove(&root)
                {
                    let _ = writer.shutdown().await;
                    reader.abort();
                    tracing::debug!(root = %root.display(), "detached an idle route");
                }
            }
            Event::GatherTimeout(host_key) => {
                let expired: Vec<String> = gather_parts
                    .iter()
                    .filter(|(_, (owner, _))| *owner == host_key)
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in expired {
                    let Some((_, index)) = gather_parts.remove(&key) else {
                        continue;
                    };
                    let Some(root) = sent_to.remove(&key) else {
                        continue;
                    };
                    let Ok(id) = serde_json::from_str::<Value>(&key) else {
                        continue;
                    };
                    let reply = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32603,
                            "message": format!(
                                "the backend of {} did not answer within {} ms",
                                root.display(),
                                timing.gather.as_millis()
                            ),
                        },
                    });
                    if let Some(merged) = settle_part(&mut gathers, &host_key, index, reply) {
                        write_value(&mut host_out, &merged).await;
                    }
                }
            }
        }
    }
}

/// The merge a tool call's backends answer under, when the call is gathered.
fn gather_merge(message: &Value) -> Option<Merge> {
    if message.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    match message.pointer("/params/name").and_then(Value::as_str)? {
        "get_new_diagnostics" => Some(Merge::Diagnostics),
        "workspace_symbol_search" => {
            let limit = message
                .pointer("/params/arguments/limit")
                .and_then(Value::as_u64)
                .unwrap_or(100);
            Some(Merge::Symbols {
                limit: usize::try_from(limit).unwrap_or(usize::MAX),
            })
        }
        _ => None,
    }
}

/// Records one backend's answer to a gathered call. Returns the merged answer
/// once every part of the call has arrived.
fn settle_part(
    gathers: &mut HashMap<String, Gather>,
    key: &str,
    index: usize,
    reply: Value,
) -> Option<Value> {
    let gather = gathers.get_mut(key)?;
    gather.parts.push((index, reply));
    gather.waiting -= 1;
    if gather.waiting > 0 {
        return None;
    }
    let Gather {
        id,
        merge,
        mut parts,
        ..
    } = gathers.remove(key)?;
    parts.sort_by_key(|(index, _)| *index);
    let parts = parts.into_iter().map(|(_, part)| part).collect();
    Some(match merge {
        Merge::Diagnostics => gather::merge(&id, parts),
        Merge::Symbols { limit } => gather::merge_symbols(&id, parts, limit),
    })
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

async fn answer_queue<W: AsyncWrite + Unpin>(
    host: &mut W,
    queue: Vec<(String, Value)>,
    reason: &str,
) {
    for (_, message) in queue {
        if let Some(reply) = stub::answer(&message, reason) {
            write_value(host, &reply).await;
        }
    }
}

/// Sends the replayed handshake and the queued calls to a route's new backend.
/// The backend never saw `initialize`, so it is replayed whatever the host got.
async fn replay_route<W: AsyncWrite + Unpin>(
    writer: &mut W,
    init: Option<&str>,
    initialized: Option<&str>,
    queue: Vec<(String, Value)>,
    sent_to: &mut HashMap<String, PathBuf>,
    root: &Path,
) -> io::Result<()> {
    if let Some(line) = init
        && let Ok(mut replay) = serde_json::from_str::<Value>(line)
    {
        replay["id"] = Value::from(REPLAY_ID);
        write_line(writer, &replay.to_string()).await?;
    }
    if let Some(line) = initialized {
        write_line(writer, line).await?;
    }
    for (line, message) in queue {
        if let Some(id) = message.get("id") {
            sent_to.insert(id.to_string(), root.to_path_buf());
        }
        write_line(writer, &line).await?;
    }
    Ok(())
}

fn spawn_route_attach(
    route: Route,
    request: Handshake,
    timing: Timing,
    events: mpsc::UnboundedSender<Event>,
) {
    tokio::spawn(async move {
        let request = Handshake {
            root: Some(route.root.clone()),
            ..request
        };
        let door = route.door.as_ref();
        let place = door.place();
        let outcome = if route.start {
            Some(attach(door, &request, &timing).await)
        } else {
            attach_running(door, &request, &timing).await
        };
        let _ = events.send(Event::RouteAttach {
            root: route.root,
            place,
            outcome,
        });
    });
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
) -> tokio::task::AbortHandle
where
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
    })
    .abort_handle()
}

fn spawn_route_idle(
    root: PathBuf,
    generation: u64,
    after: Duration,
    events: mpsc::UnboundedSender<Event>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        let _ = events.send(Event::RouteIdle(root, generation));
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

/// Attach to a backend that already answers, or `None` when none does.
async fn attach_running(door: &dyn Door, request: &Handshake, timing: &Timing) -> Option<Outcome> {
    match handshake_with(door, request).await {
        ConnectionAttempt::Connected(found) => {
            match judge(door, request, found, timing, false).await {
                Judged::Done(outcome) => Some(outcome),
                Judged::Evicted => None,
            }
        }
        ConnectionAttempt::Absent | ConnectionAttempt::Busy => None,
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
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use futures::future::BoxFuture;
    use rmcp::ServiceExt as _;
    use serde_json::{Value, json};
    use tokio::io::{
        AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader, ReadBuf,
    };

    use super::{Door, Place, Route, Router, Start, Timing};
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
        /// Close the backend's read half while keeping its output half open.
        CloseRead,
        /// Answer the first MCP line as an `initialize`, then close as soon
        /// as the next line arrives.
        AnswerOnceThenClose,
        /// Answer `initialize`, then answer every request with a tool result whose text is the given report.
        Report(&'static str),
        /// Like `Report`, but sends no answer to a call of the named tool.
        Silent(&'static str, &'static str),
    }

    struct WriteClosedStream {
        inner: tokio::io::DuplexStream,
        closed: Arc<AtomicBool>,
    }

    impl AsyncRead for WriteClosedStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_read(cx, buffer)
        }
    }

    impl AsyncWrite for WriteClosedStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            if this.closed.load(Ordering::Acquire) {
                Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
            } else {
                Pin::new(&mut this.inner).poll_write(cx, buffer)
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    /// A door whose connect attempts follow a script, and which counts the
    /// starts it is asked for and the connection kinds it saw.
    pub(super) struct FakeDoor {
        scripts: Mutex<VecDeque<Script>>,
        start: Start,
        pub(super) starts: Mutex<usize>,
        pub(super) kinds: Arc<Mutex<Vec<ConnectionKind>>>,
        pub(super) roots: Arc<Mutex<Vec<Option<PathBuf>>>>,
    }

    impl FakeDoor {
        pub(super) fn new(start: Start, scripts: Vec<Script>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(scripts.into()),
                start,
                starts: Mutex::new(0),
                kinds: Arc::new(Mutex::new(Vec::new())),
                roots: Arc::new(Mutex::new(Vec::new())),
            })
        }
    }

    /// Routes a file under one of its roots to that root's door.
    pub(super) struct FakeRouter(pub(super) Vec<(PathBuf, Arc<FakeDoor>, bool)>);

    impl Router for FakeRouter {
        fn route(&self, file: &Path) -> Option<Route> {
            self.0
                .iter()
                .find(|(root, _, _)| file.starts_with(root))
                .map(|(root, door, start)| Route {
                    root: root.clone(),
                    door: Arc::clone(door) as Arc<dyn Door>,
                    start: *start,
                })
        }
    }

    impl Door for FakeDoor {
        #[allow(clippy::too_many_lines)]
        fn connect(&self) -> BoxFuture<'_, io::Result<Box<dyn HookStream>>> {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Script::Nobody);
            let kinds = Arc::clone(&self.kinds);
            let roots = Arc::clone(&self.roots);
            Box::pin(async move {
                let (reply, then) = match script {
                    Script::Nobody => return Err(io::ErrorKind::ConnectionRefused.into()),
                    Script::Busy => {
                        return std::future::pending::<io::Result<Box<dyn HookStream>>>().await;
                    }
                    Script::Server(reply, then) => (reply, then),
                };
                let (client, mut server) = tokio::io::duplex(1 << 20);
                let write_closed = Arc::new(AtomicBool::new(false));
                let client: Box<dyn HookStream> = if matches!(&then, Then::CloseRead) {
                    Box::new(WriteClosedStream {
                        inner: client,
                        closed: Arc::clone(&write_closed),
                    })
                } else {
                    Box::new(client)
                };
                tokio::spawn(async move {
                    let Ok(request) = handshake::read::<_, Handshake>(&mut server).await else {
                        return;
                    };
                    kinds.lock().unwrap().push(request.kind);
                    roots.lock().unwrap().push(request.root);
                    if matches!(&then, Then::CloseBeforeReply) {
                        return;
                    }
                    if handshake::write(&mut server, &reply).await.is_err() {
                        return;
                    }
                    let silent_tool = match &then {
                        Then::Silent(_, tool) => Some(*tool),
                        _ => None,
                    };
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
                        Then::CloseRead => {
                            write_closed.store(true, Ordering::Release);
                            std::future::pending::<()>().await;
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
                        Then::Report(report) | Then::Silent(report, _) => {
                            let (reader, mut writer) = tokio::io::split(server);
                            let mut lines = BufReader::new(reader).lines();
                            while let Ok(Some(line)) = lines.next_line().await {
                                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                                    continue;
                                };
                                let (Some(id), Some(method)) =
                                    (message.get("id"), message["method"].as_str())
                                else {
                                    continue;
                                };
                                if silent_tool.is_some_and(|tool| message["params"]["name"] == tool)
                                {
                                    continue;
                                }
                                let answer = if method == "initialize" {
                                    json!({"jsonrpc":"2.0","id":id,"result":{
                                        "protocolVersion":"2025-11-25",
                                        "capabilities":{},
                                        "serverInfo":{"name":"mcpls","version":"0"},
                                    }})
                                } else {
                                    json!({"jsonrpc":"2.0","id":id,"result":{
                                        "content":[{"type":"text","text":report}],
                                        "isError":false,
                                    }})
                                };
                                if writer
                                    .write_all(format!("{answer}\n").as_bytes())
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }
                });
                Ok(client)
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
            gather: Duration::from_millis(300),
            route_idle: Duration::from_secs(60),
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
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, Lines};
    use tokio::time::Instant;

    use super::fake::{FakeDoor, FakeRouter, Script, Then, accepted, fast, refused, request};
    use super::{Door, NoRoutes, Router, Start, Timing, relay};
    use crate::backend::handshake::{self, Handshake, Refusal};

    struct Host {
        input: DuplexStream,
        output: Lines<BufReader<DuplexStream>>,
        relay: tokio::task::JoinHandle<()>,
    }

    impl Host {
        fn start(
            door: Arc<dyn Door>,
            router: Arc<dyn Router>,
            request: Handshake,
            timing: Timing,
        ) -> Self {
            let (input, relay_in) = tokio::io::duplex(1 << 20);
            let (relay_out, output) = tokio::io::duplex(1 << 20);
            let relay = tokio::spawn(relay(relay_in, relay_out, door, router, request, timing));
            Self {
                input,
                output: BufReader::new(output).lines(),
                relay,
            }
        }

        async fn start_with_pending(
            door: Arc<dyn Door>,
            router: Arc<dyn Router>,
            request: Handshake,
            timing: Timing,
            message: Value,
        ) -> Self {
            let (mut input, relay_in) = tokio::io::duplex(1 << 20);
            let (relay_out, output) = tokio::io::duplex(1 << 20);
            input
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
            let relay = tokio::spawn(relay(relay_in, relay_out, door, router, request, timing));
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

        async fn call_file(&mut self, id: i64, file: &str) -> Value {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"get_server_logs","arguments":{"file_path":file}}})).await;
            self.response(id).await
        }

        async fn call_named(&mut self, id: i64, name: &str) -> Value {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":{}}})).await;
            self.response(id).await
        }

        async fn call_symbols(&mut self, id: i64) -> Value {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"workspace_symbol_search","arguments":{"query":"x"}}})).await;
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
        let mut host = Host::start(door, Arc::new(NoRoutes), request(), fast());
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
        let mut host = Host::start(door, Arc::new(NoRoutes), request(), fast());
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
        let mut host = Host::start(door, Arc::new(NoRoutes), request(), fast());
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
        let mut host = Host::start(
            Arc::clone(&door) as Arc<dyn Door>,
            Arc::new(NoRoutes),
            request(),
            fast(),
        );
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
        let mut host = Host::start(door, Arc::new(NoRoutes), request(), fast());
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
    async fn test_a_half_closed_backend_fails_a_deferred_request() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Nobody,
                Script::Nobody,
                Script::Server(accepted(), Then::CloseRead),
                Script::Server(accepted(), Then::CloseRead),
            ],
        );
        let mut host = Host::start_with_pending(
            door,
            Arc::new(NoRoutes),
            request(),
            fast(),
            initialize_request(),
        )
        .await;
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
        let mut host = Host::start(door, Arc::new(NoRoutes), request(), fast());
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
        let mut host = Host::start(door, Arc::new(NoRoutes), request(), timing);
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
        let mut host = Host::start(door, Arc::new(NoRoutes), request(), fast());
        host.initialize().await;
        let Host { input, relay, .. } = host;
        drop(input);
        tokio::time::timeout(Duration::from_secs(5), relay)
            .await
            .expect("the relay returned after host EOF")
            .unwrap();
    }

    /// A file under a route with no backend running starts one at that route's root.
    #[tokio::test]
    async fn test_a_file_in_a_worktree_is_answered_by_a_backend_started_there() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let wt_door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Nobody,
                Script::Nobody,
                Script::Server(accepted(), Then::Serve),
            ],
        );
        let router = FakeRouter(vec![(PathBuf::from("/wt"), Arc::clone(&wt_door), true)]);
        let mut host = Host::start(door, Arc::new(router), request(), fast());
        host.initialize().await;

        let call = host.call_file(2, "/wt/a.rs").await;
        assert!(call.get("error").is_none(), "{call}");
        assert_ne!(call["result"]["isError"], true, "{call}");
        assert_eq!(*wt_door.starts.lock().unwrap(), 1);
        assert!(
            wt_door
                .roots
                .lock()
                .unwrap()
                .contains(&Some(PathBuf::from("/wt")))
        );
    }

    /// `get_new_diagnostics` is answered with the reports of the session backend and every attached route backend.
    #[tokio::test]
    async fn test_new_diagnostics_merge_every_attached_backend() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(
                accepted(),
                Then::Report(
                    r#"{"changed":[{"file_path":"/work/a.rs","diagnostics":[],"omitted":0}],"cleared":[],"omitted":0}"#,
                ),
            )],
        );
        let wt_door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(
                accepted(),
                Then::Report(
                    r#"{"changed":[{"file_path":"/wt/b.rs","diagnostics":[],"omitted":0}],"cleared":[],"omitted":0}"#,
                ),
            )],
        );
        let router = FakeRouter(vec![(PathBuf::from("/wt"), Arc::clone(&wt_door), true)]);
        let mut host = Host::start(door, Arc::new(router), request(), fast());
        host.initialize().await;
        host.call_file(2, "/wt/b.rs").await;

        let merged = host.call_named(3, "get_new_diagnostics").await;
        assert!(merged.get("error").is_none(), "{merged}");
        let report: Value =
            serde_json::from_str(merged["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let files: Vec<&str> = report["changed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["file_path"].as_str().unwrap())
            .collect();
        assert_eq!(files, ["/work/a.rs", "/wt/b.rs"]);
    }

    /// `workspace_symbol_search` is answered with the symbols of the session backend and every attached route backend.
    #[tokio::test]
    async fn test_workspace_symbols_merge_every_attached_backend() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(
                accepted(),
                Then::Report(
                    r#"{"symbols":[{"name":"main_only","kind":"Function","location":{"uri":"file:///work/a.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}}]}"#,
                ),
            )],
        );
        let wt_door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(
                accepted(),
                Then::Report(
                    r#"{"symbols":[{"name":"worktree_only","kind":"Function","location":{"uri":"file:///wt/b.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}}]}"#,
                ),
            )],
        );
        let router = FakeRouter(vec![(PathBuf::from("/wt"), Arc::clone(&wt_door), true)]);
        let mut host = Host::start(door, Arc::new(router), request(), fast());
        host.initialize().await;
        host.call_file(2, "/wt/b.rs").await;

        let merged = host.call_symbols(3).await;
        assert!(merged.get("error").is_none(), "{merged}");
        let report: Value =
            serde_json::from_str(merged["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let names: Vec<&str> = report["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["main_only", "worktree_only"]);
    }

    /// A route backend that never answers `workspace_symbol_search` does not hide the session's symbols.
    #[tokio::test]
    async fn test_a_silent_route_does_not_hide_the_session_symbols() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(
                accepted(),
                Then::Report(
                    r#"{"symbols":[{"name":"main_only","kind":"Function","location":{"uri":"file:///work/a.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}}]}"#,
                ),
            )],
        );
        let wt_door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(
                accepted(),
                Then::Silent(
                    r#"{"symbols":[{"name":"worktree_only","kind":"Function","location":{"uri":"file:///wt/b.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}}]}"#,
                    "workspace_symbol_search",
                ),
            )],
        );
        let router = FakeRouter(vec![(PathBuf::from("/wt"), Arc::clone(&wt_door), true)]);
        let mut host = Host::start(door, Arc::new(router), request(), fast());
        host.initialize().await;
        host.call_file(2, "/wt/b.rs").await;

        let answered = tokio::time::timeout(Duration::from_secs(5), host.call_symbols(3))
            .await
            .expect("the symbol search is answered");
        assert!(answered.get("error").is_none(), "{answered}");
        let report: Value =
            serde_json::from_str(answered["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        let names: Vec<&str> = report["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["main_only"]);
    }

    /// A file under a route whose checkout has no backend to attach to is answered by the session's backend.
    #[tokio::test]
    async fn test_a_file_with_no_backend_to_attach_to_goes_to_the_session_backend() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let wt_door = FakeDoor::new(Start::Spawned, vec![]);
        let router = FakeRouter(vec![(PathBuf::from("/wt"), Arc::clone(&wt_door), false)]);
        let mut host = Host::start(
            Arc::clone(&door) as Arc<dyn Door>,
            Arc::new(router),
            request(),
            fast(),
        );
        host.initialize().await;

        let call = host.call_file(2, "/wt/a.rs").await;
        assert_ne!(call["result"]["isError"], true, "{call}");
        assert_eq!(*wt_door.starts.lock().unwrap(), 0);
        assert_eq!(door.kinds.lock().unwrap().len(), 1);
    }

    /// A running backend of another checkout is attached to without starting one.
    #[tokio::test]
    async fn test_a_running_backend_of_another_checkout_is_attached_without_a_start() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let wt_door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let router = FakeRouter(vec![(PathBuf::from("/wt"), Arc::clone(&wt_door), false)]);
        let mut host = Host::start(door, Arc::new(router), request(), fast());
        host.initialize().await;

        let call = host.call_file(2, "/wt/a.rs").await;
        assert_ne!(call["result"]["isError"], true, "{call}");
        assert_eq!(*wt_door.starts.lock().unwrap(), 0);
        assert_eq!(wt_door.kinds.lock().unwrap().len(), 1);
    }

    /// A refused route fails only the call that needed it, and the session's backend keeps answering.
    #[tokio::test]
    async fn test_a_refused_route_fails_its_call_and_spares_the_session() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let wt_door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(
                refused("0.0.1", 2, Refusal::Build),
                Then::Close,
            )],
        );
        let router = FakeRouter(vec![(PathBuf::from("/wt"), Arc::clone(&wt_door), true)]);
        let mut host = Host::start(door, Arc::new(router), request(), fast());
        host.initialize().await;

        let call = host.call_file(2, "/wt/a.rs").await;
        assert_eq!(call["result"]["isError"], true, "{call}");
        assert!(call.to_string().contains("0.0.1"), "{call}");

        let session = host.call_tool(3).await;
        assert_ne!(session["result"]["isError"], true, "{session}");
    }

    /// Calls to one route share a single connection to that checkout's backend.
    #[tokio::test]
    async fn test_calls_to_one_route_share_one_connection() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let wt_door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let router = FakeRouter(vec![(PathBuf::from("/wt"), Arc::clone(&wt_door), false)]);
        let mut host = Host::start(door, Arc::new(router), request(), fast());
        host.initialize().await;

        let first = host.call_file(2, "/wt/a.rs").await;
        let second = host.call_file(3, "/wt/a.rs").await;
        assert_ne!(first["result"]["isError"], true, "{first}");
        assert_ne!(second["result"]["isError"], true, "{second}");
        assert_eq!(wt_door.kinds.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_a_route_reattaches_after_its_backend_closes() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let wt_door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Server(accepted(), Then::AnswerOnceThenClose),
                Script::Server(accepted(), Then::Serve),
            ],
        );
        let router = FakeRouter(vec![(PathBuf::from("/wt"), Arc::clone(&wt_door), false)]);
        let mut host = Host::start(door, Arc::new(router), request(), fast());
        host.initialize().await;

        let failed = host.call_file(2, "/wt/a.rs").await;
        assert!(
            failed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stopped"),
            "{failed}"
        );
        let retried = host.call_file(3, "/wt/a.rs").await;
        assert_ne!(retried["result"]["isError"], true, "{retried}");
        assert!(retried.get("error").is_none(), "{retried}");
        assert_eq!(wt_door.kinds.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_an_idle_route_is_detached_and_attached_again() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let wt_door = FakeDoor::new(
            Start::Spawned,
            vec![
                Script::Server(accepted(), Then::Serve),
                Script::Server(accepted(), Then::Serve),
            ],
        );
        let router = FakeRouter(vec![(PathBuf::from("/wt"), Arc::clone(&wt_door), false)]);
        let timing = Timing {
            route_idle: Duration::from_millis(200),
            ..fast()
        };
        let mut host = Host::start(
            Arc::clone(&door) as Arc<dyn Door>,
            Arc::new(router),
            request(),
            timing,
        );
        host.initialize().await;

        let first = host.call_file(2, "/wt/a.rs").await;
        assert_ne!(first["result"]["isError"], true, "{first}");

        tokio::time::sleep(Duration::from_millis(600)).await;

        let second = host.call_file(3, "/wt/a.rs").await;
        assert_ne!(second["result"]["isError"], true, "{second}");
        assert!(second.get("error").is_none(), "{second}");
        assert_eq!(wt_door.kinds.lock().unwrap().len(), 2);
        assert_eq!(door.kinds.lock().unwrap().len(), 1);

        let session = host.call_tool(4).await;
        assert_ne!(session["result"]["isError"], true, "{session}");
    }

    #[tokio::test]
    async fn test_a_route_in_use_stays_attached() {
        let door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let wt_door = FakeDoor::new(
            Start::Spawned,
            vec![Script::Server(accepted(), Then::Serve)],
        );
        let router = FakeRouter(vec![(PathBuf::from("/wt"), Arc::clone(&wt_door), false)]);
        let timing = Timing {
            route_idle: Duration::from_millis(300),
            ..fast()
        };
        let mut host = Host::start(door, Arc::new(router), request(), timing);
        host.initialize().await;

        for id in 2..=5 {
            let call = host.call_file(id, "/wt/a.rs").await;
            assert_ne!(call["result"]["isError"], true, "{call}");
            tokio::time::sleep(Duration::from_millis(150)).await;
        }

        assert_eq!(wt_door.kinds.lock().unwrap().len(), 1);
    }
}
