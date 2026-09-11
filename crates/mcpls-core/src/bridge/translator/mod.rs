//! MCP to LSP translation layer.
//!
//! `Translator` owns the LSP client/server registries and dispatches MCP
//! tool calls to per-domain handler modules. This module defines the
//! `Translator` struct itself plus setup/lifecycle methods (construction,
//! registration, shutdown); actual tool-call handling lives in the sibling
//! modules below, grouped by domain.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use tokio::sync::Mutex;

use self::clock::{Clock, SystemClock};
use self::encoding_ctx::EncodingCtx;
use self::respawn::RespawnBackoff;
use crate::bridge::apply::{Applier, ApplySummary, EditPlan, InvalidationQueue};
use crate::bridge::encoding::PositionEncoding;
use crate::bridge::state::ResourceLimits;
use crate::bridge::{DocumentTracker, NotificationCache, lock_std};
use crate::config::{ApplyConfig, ServerId, ToolKind, ToolRouter};
use crate::error::{Error, Result};
use crate::lsp::{LspClient, LspServer, ServerInitConfig, WatchRegistry};

mod assist;
mod call_hierarchy;
mod clock;
mod diagnostics;
mod dto;
mod edits;
mod encoding_ctx;
mod navigation;
mod respawn;
mod routing;
mod symbols;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub mod testing;

pub use dto::*;
pub use routing::validate_path_against_roots;

/// Translator handles MCP tool calls by converting them to LSP requests.
///
/// All fields use interior mutability so `Translator` can be shared via a
/// plain `Arc<Translator>` with no outer lock: every LSP tool call would
/// otherwise serialize behind a single mutex for its entire round trip
/// (including the LSP request timeout), which is the root cause fixed here.
/// Each field is locked independently and only for the short, synchronous
/// section that touches it. In particular, the actual LSP request/response
/// round trip (`client.request(...)`) always runs with no lock held.
///
/// `document_tracker` is no exception: `DocumentTracker` locks its own state
/// per-path internally (see its docs), so `prepare_document`'s call into
/// `ensure_open` never holds a lock shared across unrelated paths or
/// languages while it does that document's disk I/O and
/// `textDocument/didOpen`/`didChange` notify.
#[derive(Debug)]
pub struct Translator {
    #[cfg(all(test, unix))]
    pub(crate) registration_pause: StdMutex<Option<crate::recovery_tests::RegistrationPause>>,
    #[cfg(test)]
    resync_pause: StdMutex<Option<testing::ResyncPause>>,
    pub(crate) notification_pumps: OnceLock<crate::notification_lifecycle::NotificationPumps>,
    /// LSP clients indexed by routing identity. Locked only for the map
    /// lookup/insert itself, never across an LSP request.
    lsp_clients: Arc<StdMutex<HashMap<ServerId, LspClient>>>,
    /// LSP servers indexed by routing identity (held for lifetime management).
    lsp_servers: Arc<StdMutex<HashMap<ServerId, LspServer>>>,
    /// Document state tracker. Locks its own state internally, per path.
    document_tracker: Arc<DocumentTracker>,
    /// Resource limits `document_tracker` was last built with. Kept
    /// alongside `document_tracker` so [`Self::with_extensions`] and
    /// [`Self::with_resource_limits`] can each rebuild the tracker from
    /// whichever of (limits, extension map) the other has already set,
    /// regardless of call order -- see [`Self::with_resource_limits`].
    resource_limits: ResourceLimits,
    /// Allowed workspace roots for path validation. Read-only after `serve()`
    /// setup, so no lock is needed.
    workspace_roots: Arc<Vec<PathBuf>>,
    /// Custom file extension to language ID mappings. Read-only after
    /// `serve()` setup, so no lock is needed.
    extension_map: Arc<HashMap<String, String>>,
    /// Servers that are configured + applicable but may not have finished
    /// initializing yet (background init). Used to return a clear "still
    /// initializing" error instead of "no server configured".
    expected_servers: Arc<StdMutex<HashSet<ServerId>>>,
    /// Per-tool routing table: resolves `(language, tool)` to a `ServerId`.
    /// Locked independently so `rebind_router` (called from a background
    /// task once registration completes) never contends with an in-flight
    /// LSP round trip.
    router: Arc<StdMutex<ToolRouter>>,
    /// Configs needed to respawn a server if its process dies later, keyed
    /// by routing identity. Populated once per server right after a
    /// successful spawn (see [`Self::register_server_config`]); the respawn
    /// path ([`Self::respawn_if_dead`]) is the only reader.
    server_configs: Arc<StdMutex<HashMap<ServerId, ServerInitConfig>>>,
    /// Per-server single-flight lock so concurrent callers that both observe
    /// a dead process don't race to respawn it independently -- the loser
    /// waits for the winner's attempt to finish (success or failure) and
    /// then re-reads whatever ended up registered. See
    /// [`Self::respawn_if_dead`].
    respawn_locks: Arc<StdMutex<HashMap<ServerId, Arc<Mutex<()>>>>>,
    /// Consecutive respawn failures and last-attempt time per server, so a
    /// crash-looping server backs off instead of eating a fresh
    /// `timeout_seconds` on every tool call that arrives while it is down.
    /// See [`Self::respawn_if_dead`].
    respawn_backoffs: Arc<StdMutex<HashMap<ServerId, RespawnBackoff>>>,
    /// Diagnostics cache, shared with `serve_with`'s notification pump.
    ///
    /// `None` for a `Translator` built without [`Self::with_notification_cache`]
    /// (e.g. most unit tests). When present, [`Self::respawn_if_dead`] uses
    /// it to invalidate a respawned server's stale cached diagnostics --
    /// see that method's docs for why that matters.
    notification_cache: Option<Arc<Mutex<NotificationCache>>>,
    /// Serializes the windows during which an inbound `workspace/applyEdit`
    /// is honored, so only one is open at a time. A client's apply sink is
    /// a single slot shared by every clone of that client, so two
    /// overlapping windows would clobber each other: the second's install
    /// drops the first's sender, and the first's teardown clears the
    /// second's sink mid-command. Both would silently degrade to no window
    /// at all.
    ///
    /// Deliberately not the applier's own lock: that one is taken per apply
    /// *inside* the window, so holding it across a command whose inbound
    /// edit needs it would deadlock. A window always takes this lock first
    /// and the apply lock second, never the other way round.
    apply_sink_lock: Arc<Mutex<()>>,
    /// Writes a permitted `WorkspaceEdit` to the working tree. Always
    /// present; a translator that may not write carries one whose
    /// `ApplyConfig` permits nothing.
    applier: Arc<Applier>,
    /// Applied paths whose content and save notifications are still pending.
    ///
    /// The applier fills this before it writes, so an apply whose caller
    /// stopped awaiting it -- the user pressing Esc, a
    /// `notifications/cancelled`, a client disconnect -- still leaves behind
    /// the paths that have to be resynchronized. Drained by
    /// [`Self::resync_changed_documents`], which runs after every apply and
    /// before every call that opens a document, so no tool call can read a
    /// tracked document a completed write has already invalidated.
    pending_invalidations: InvalidationQueue,
    /// Registrations from `client/registerCapability`, shared with the
    /// clients that write them.
    watch_registry: Option<Arc<WatchRegistry>>,
    /// Time source for respawn-backoff bookkeeping ([`respawn`](self::respawn)).
    /// Always [`SystemClock`] in production; overridden via
    /// [`Self::with_clock`] in tests so backoff-window tests can advance
    /// time deterministically instead of sleeping in real time.
    clock: Arc<dyn Clock>,
}

/// The paths a drain of [`Translator::pending_invalidations`] has taken off
/// the queue and not yet dealt with.
///
/// Returns them if the drain does not finish. The drain runs inside the
/// request future, which is dropped whenever the caller cancels, and a path
/// forgotten here is forgotten for good: nothing else knows the file on disk
/// no longer matches the document still tracked for it.
struct PendingDrain<'a> {
    queue: &'a InvalidationQueue,
    remaining: Vec<PathBuf>,
}

impl Drop for PendingDrain<'_> {
    fn drop(&mut self) {
        if !self.remaining.is_empty() {
            self.queue.extend(&std::mem::take(&mut self.remaining));
        }
    }
}

/// What [`Translator::resync_one_document`] accomplished for one path, and
/// what [`Translator::resync_changed_documents`] should do next because of
/// it.
enum ResyncStep {
    /// Content is synchronized; saves still need the captured version and processes.
    NeedsSave {
        version: i32,
        servers: Vec<(ServerId, u64)>,
    },
    /// Fully resynchronized, gone, or not routable: leave the queue.
    Done,
    /// A failed open/read or a stale save leaves this path queued for retry.
    Deferred,
    /// A notify failed on this path's connection to a server. Every other
    /// path still queued for that same server is behind the same broken
    /// connection, so the drain stops here instead of failing through the
    /// rest of the queue one path at a time.
    NotifyFailed,
}

/// What [`Translator::open_untracked_document`] achieved for one path.
///
/// The three ways of not opening one are kept apart because a caller
/// reports them differently. Nothing routes the path, so no check was ever
/// going to run and silence is the honest answer; the caller had no room
/// for another document, which more room would fix; or the open itself did
/// not work, which it would not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOutcome {
    /// The document is tracked and its server was sent a `didOpen`.
    Opened,
    /// Nothing routes this path's language, and nothing ever will while the
    /// current configuration stands. Nothing was opened, nothing was told.
    NoRoute,
    /// A server routes it and the caller said it had no room for another
    /// document, so the open was not attempted.
    NoHeadroom,
    /// A server routes it, but the file was not opened -- unreadable, past
    /// a resource limit, or routed to a server that has not finished
    /// starting.
    Failed,
}

/// Upper bound on how long [`Translator::shutdown_servers`] waits for a
/// single LSP server's graceful `shutdown`/`exit` handshake before giving up
/// and letting `kill_on_drop` terminate it instead.
const SERVER_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl Translator {
    /// Create a new translator.
    ///
    /// Starts with an empty router: nothing is routable until [`Self::with_router`]
    /// installs one, which matches having no servers registered.
    #[must_use]
    pub fn new() -> Self {
        Self {
            #[cfg(all(test, unix))]
            registration_pause: StdMutex::new(None),
            #[cfg(test)]
            resync_pause: StdMutex::new(None),
            notification_pumps: OnceLock::new(),
            lsp_clients: Arc::new(StdMutex::new(HashMap::new())),
            lsp_servers: Arc::new(StdMutex::new(HashMap::new())),
            document_tracker: Arc::new(DocumentTracker::new(
                ResourceLimits::default(),
                HashMap::new(),
            )),
            resource_limits: ResourceLimits::default(),
            workspace_roots: Arc::new(Vec::new()),
            extension_map: Arc::new(HashMap::new()),
            expected_servers: Arc::new(StdMutex::new(HashSet::new())),
            router: Arc::new(StdMutex::new(ToolRouter::default())),
            server_configs: Arc::new(StdMutex::new(HashMap::new())),
            respawn_locks: Arc::new(StdMutex::new(HashMap::new())),
            respawn_backoffs: Arc::new(StdMutex::new(HashMap::new())),
            notification_cache: None,
            apply_sink_lock: Arc::new(Mutex::new(())),
            applier: Arc::new(Applier::new(Vec::new(), ApplyConfig::default())),
            pending_invalidations: InvalidationQueue::default(),
            watch_registry: None,
            clock: Arc::new(SystemClock),
        }
    }

    /// Override the time source used by respawn-backoff bookkeeping.
    ///
    /// Test-only: production always uses [`SystemClock`]. Lets
    /// backoff-window tests advance a `FakeClock` deterministically instead
    /// of sleeping in real time.
    #[cfg(test)]
    #[must_use]
    fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    #[cfg(test)]
    pub(crate) fn install_resync_pause(
        &self,
        reached: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    ) {
        *lock_std(&self.resync_pause) = Some(testing::ResyncPause { reached, release });
    }

    #[cfg(test)]
    async fn pause_after_resync_save(&self) {
        let pause = lock_std(&self.resync_pause).take();
        if let Some(pause) = pause {
            let _ = pause.reached.send(());
            let _ = pause.release.await;
        }
    }

    /// Set the workspace roots for path validation.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared, so this replaces the `Arc` wholesale rather than locking.
    pub fn set_workspace_roots(&mut self, roots: Vec<PathBuf>) {
        self.workspace_roots = Arc::new(roots);
    }

    /// Give the translator a handle to the shared diagnostics cache, so the
    /// respawn path can invalidate a respawned server's stale entries.
    ///
    /// Only called during single-owner setup (mirrors [`Self::with_router`]),
    /// before the translator is shared -- `serve_with` passes the same
    /// `Arc<Mutex<NotificationCache>>` used by the notification pump tasks.
    #[must_use]
    pub fn with_notification_cache(mut self, cache: Arc<Mutex<NotificationCache>>) -> Self {
        self.notification_cache = Some(cache);
        self
    }

    /// Attach the registry that decides which servers hear about a changed
    /// file.
    #[must_use]
    pub fn with_watch_registry(mut self, registry: Arc<WatchRegistry>) -> Self {
        self.watch_registry = Some(registry);
        self
    }

    /// Drop every watched-file registration `server` holds.
    pub fn forget_watch_registrations(&self, server: &ServerId) {
        if let Some(registry) = &self.watch_registry {
            registry.forget_server(server);
        }
    }

    /// Tell every server that registered a matching glob that `path`
    /// changed, naming to each the first of `kinds` that server's own
    /// watcher accepts.
    ///
    /// Sent per server rather than broadcast: a server that did not ask is
    /// told nothing, which is the difference between this and shouting at
    /// everything with a language id. Each server is told once, under one
    /// kind, however many of `kinds` its watchers would take.
    ///
    /// A caller that knows exactly what happened passes one kind. A caller
    /// that has narrowed it to two -- the sweep, for a path present on disk
    /// that the tracker is not holding -- passes both, most preferred
    /// first, so a server that registered for only one of them still hears
    /// about the file instead of being dropped by the caller's guess.
    ///
    /// `pub(crate)` rather than private so a caller that learns of a
    /// changed path from outside an apply -- a host file-watcher sweep --
    /// can report it the same way.
    pub(crate) async fn notify_watched_files(
        &self,
        path: &Path,
        kinds: &[lsp_types::FileChangeType],
    ) {
        let Some(registry) = &self.watch_registry else {
            return;
        };
        let Ok(uri) = crate::bridge::path_to_uri(path) else {
            return;
        };
        let mut told: Vec<ServerId> = Vec::new();
        for &kind in kinds {
            for server in registry.servers_for(path, kind) {
                if told.contains(&server) {
                    continue;
                }
                told.push(server.clone());
                let Some(client) = lock_std(&self.lsp_clients).get(&server).cloned() else {
                    continue;
                };
                let params = lsp_types::DidChangeWatchedFilesParams {
                    changes: vec![lsp_types::FileEvent {
                        uri: uri.clone(),
                        typ: kind,
                    }],
                };
                if let Err(error) = client
                    .notify("workspace/didChangeWatchedFiles", params)
                    .await
                {
                    tracing::warn!(%server, path = %path.display(), %error, "watched files notify failed");
                }
            }
        }
    }

    /// Install the applier that permitted tools write through.
    ///
    /// Only called during single-owner setup (mirrors
    /// [`Self::with_router`]), before the translator is shared.
    #[must_use]
    pub fn with_applier(mut self, applier: Arc<Applier>) -> Self {
        self.applier = applier;
        self
    }

    /// Which tools this deployment lets write.
    pub(crate) fn apply_config(&self) -> &ApplyConfig {
        self.applier.config()
    }

    /// The applier, if `tool` is permitted to write.
    ///
    /// Called before any LSP request, so a refused call costs nothing and
    /// reports the config key that would allow it rather than a generic
    /// permission error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyDisabled`] naming `config_key` when the tool's
    /// key is `false`.
    pub(crate) fn applier_for(
        &self,
        tool: ToolKind,
        tool_name: &'static str,
        config_key: &'static str,
    ) -> Result<Arc<Applier>> {
        if self.applier.config().permits(tool) {
            Ok(Arc::clone(&self.applier))
        } else {
            Err(Error::ApplyDisabled {
                tool: tool_name,
                config_key,
            })
        }
    }

    /// Write `plan` to disk through `applier`, using the `PositionEncoding`
    /// negotiated for `server_id`, and resynchronize every document it
    /// changed.
    ///
    /// [`Applier::apply`] serializes applies against each other, so a second
    /// apply-enabled call cannot plan against content this one is about to
    /// replace.
    ///
    /// The resync is driven off [`Self::pending_invalidations`] rather than
    /// off the returned summary, so it covers a run that failed partway and
    /// rolled back -- a restore that failed leaves the file holding the new
    /// content, and one that succeeded still moved it out and back -- and so
    /// that a caller who stops awaiting this does not take the list of paths
    /// with it. The drain before the apply picks up whatever an earlier
    /// cancelled apply left.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Applier::apply`] returns: [`Error::ApplyRefused`]
    /// when the edit is rejected before anything is written, or
    /// [`Error::ApplyPartiallyFailed`] when a step fails partway through.
    async fn apply_locked(
        &self,
        applier: &Applier,
        plan: EditPlan,
        server_id: &ServerId,
    ) -> Result<ApplySummary> {
        self.resync_changed_documents().await;
        let outcome = applier
            .apply(
                plan,
                self.position_encoding_for(server_id),
                &self.pending_invalidations,
            )
            .await;
        self.resync_changed_documents().await;
        outcome
    }

    /// Resynchronize every document whose file an apply rewrote, moved, or
    /// removed.
    ///
    /// An apply can write a file the tool call never queried: a rename
    /// anchored in one file rewrites every file referencing the symbol. LSP
    /// makes the client authoritative for a document it has opened, so a
    /// server ignores the change on disk until it is told. Telling it means
    /// synchronizing the entire batch's content before any `didSave` can
    /// start a build. Unopened routed files receive `didOpen`; tracked
    /// files receive `didChange` when their content changed.
    ///
    /// A path leaves the queue only once every server holding it is caught
    /// up on both. Content matching disk is not the completion test: after
    /// a drain interrupted between a `didChange` and its `didSave` the
    /// content matches and the save is still owed. This loop runs inside
    /// the request future, which is dropped whenever the caller cancels, so
    /// anything unfinished goes back on the queue for the next drain.
    ///
    /// `pub(crate)` because the hook sweep, in `crate::hooks::sweep`,
    /// drives the same drain for paths that arrived from the host's file
    /// watcher rather than from an apply.
    pub(crate) async fn resync_changed_documents(&self) {
        let mut drain = PendingDrain {
            queue: &self.pending_invalidations,
            remaining: self.pending_invalidations.take(),
        };
        let mut saves = Vec::new();
        let mut index = 0;
        while index < drain.remaining.len() {
            let path = drain.remaining[index].clone();
            match self.resync_one_document(&path).await {
                ResyncStep::NeedsSave { version, servers } => {
                    saves.push((path, version, servers));
                    index += 1;
                }
                ResyncStep::Done => {
                    drain.remaining.remove(index);
                }
                ResyncStep::Deferred => index += 1,
                ResyncStep::NotifyFailed => return,
            }
        }
        for (path, version, servers) in saves {
            match self.save_resynced_document(&path, version, &servers).await {
                ResyncStep::Done => {
                    drain.remaining.retain(|pending| pending != &path);
                    #[cfg(test)]
                    self.pause_after_resync_save().await;
                }
                ResyncStep::NotifyFailed => break,
                ResyncStep::Deferred | ResyncStep::NeedsSave { .. } => {}
            }
        }
    }

    /// Synchronize one path's content before the batch sends any saves.
    ///
    /// Opening precedes the path lock because `ensure_open` acquires that
    /// same lock. The subsequent refresh and changes hold it together so a
    /// concurrent open cannot replace the version being synchronized.
    #[allow(clippy::significant_drop_tightening, clippy::used_underscore_binding)]
    async fn resync_one_document(&self, path: &Path) -> ResyncStep {
        let routed_server = self
            .get_client_for_file(path, ToolKind::Diagnostics)
            .ok()
            .map(|(server, _)| {
                let generation = self.document_tracker.generation_for(&server);
                (server, generation)
            });
        let needs_open = routed_server.as_ref().is_none_or(|(server, _)| {
            self.document_tracker
                .get(path)
                .is_none_or(|state| state.synced_version(server).is_none())
        });
        if matches!(path.try_exists(), Ok(true)) && needs_open {
            match self.open_untracked_document(path, true).await {
                OpenOutcome::Opened => {}
                OpenOutcome::NoRoute if self.document_tracker.is_open(path) => {}
                OpenOutcome::NoRoute => {
                    self.notify_watched_files(path, &[lsp_types::FileChangeType::CHANGED])
                        .await;
                    return ResyncStep::Done;
                }
                OpenOutcome::NoHeadroom | OpenOutcome::Failed => {
                    self.notify_watched_files(path, &[lsp_types::FileChangeType::CHANGED])
                        .await;
                    return ResyncStep::Deferred;
                }
            }
        }
        let _path_guard = self.document_tracker.lock_path(path).await;

        match path.try_exists() {
            Ok(false) => {
                self.close_one_document_locked(path).await;
                self.notify_watched_files(path, &[lsp_types::FileChangeType::DELETED])
                    .await;
                return ResyncStep::Done;
            }
            Ok(true) => {}
            Err(error) => {
                // A stat error (e.g. a parent directory gone briefly, or a
                // permission problem) is not proof the file is gone: treat
                // it the same as a failed read rather than closing a
                // document whose file may still be sitting right there.
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "could not stat an applied file; leaving it queued"
                );
                return ResyncStep::Deferred;
            }
        }

        let resync = match self
            .document_tracker
            .resync_from_disk(path, &_path_guard)
            .await
        {
            Ok(Some(resync)) => resync,
            Ok(None) => {
                self.notify_watched_files(path, &[lsp_types::FileChangeType::CHANGED])
                    .await;
                return ResyncStep::Done;
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "could not re-read an applied file; leaving it queued"
                );
                return ResyncStep::Deferred;
            }
        };

        let Some(state) = self.document_tracker.get(path) else {
            return ResyncStep::Deferred;
        };
        let mut servers: Vec<_> = state
            .synced_servers()
            .into_iter()
            .map(|server| {
                let generation = self.document_tracker.generation_for(&server);
                (server, generation)
            })
            .collect();
        if let Some((server, generation)) = routed_server {
            servers.retain(|(synced, _)| synced != &server);
            servers.push((server, generation));
        }
        if servers.is_empty() {
            return ResyncStep::Deferred;
        }
        if !self.send_resync_changes(path, &resync, &servers).await {
            return ResyncStep::NotifyFailed;
        }
        ResyncStep::NeedsSave {
            version: resync.version,
            servers,
        }
    }

    /// Send the disk snapshot using the generations captured for this content phase.
    async fn send_resync_changes(
        &self,
        path: &Path,
        resync: &crate::bridge::state::Resync,
        servers: &[(ServerId, u64)],
    ) -> bool {
        for (server, generation) in servers
            .iter()
            .filter(|(server, _)| resync.needs_change.contains(server))
        {
            let Some(client) = lock_std(&self.lsp_clients).get(server).cloned() else {
                return false;
            };
            let params = lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier {
                    uri: resync.uri.clone(),
                    version: resync.version,
                },
                content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: resync.text.clone(),
                }],
            };
            if let Err(error) = client.notify("textDocument/didChange", params).await {
                tracing::warn!(%server, path = %path.display(), %error, "resync didChange failed");
                return false;
            }
            self.document_tracker
                .mark_change_sent(path, server, resync.version, *generation);
        }
        true
    }

    /// A concurrent document open or server replacement invalidates a prepared save.
    #[allow(clippy::significant_drop_tightening)]
    async fn save_resynced_document(
        &self,
        path: &Path,
        version: i32,
        servers: &[(ServerId, u64)],
    ) -> ResyncStep {
        let _guard = self.document_tracker.lock_path(path).await;
        for (server, generation) in servers {
            let Some(state) = self.document_tracker.get(path) else {
                return ResyncStep::Deferred;
            };
            if state.version() != version
                || state.synced_version(server) != Some(version)
                || self.document_tracker.generation_for(server) != *generation
            {
                return ResyncStep::Deferred;
            }
            if state
                .saved_version(server)
                .is_some_and(|saved| saved >= version)
            {
                continue;
            }
            let Some(client) = lock_std(&self.lsp_clients).get(server).cloned() else {
                return ResyncStep::NotifyFailed;
            };
            let params = lsp_types::DidSaveTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: state.uri().clone(),
                },
                text: None,
            };
            if let Err(error) = client.notify("textDocument/didSave", params).await {
                tracing::warn!(%server, path = %path.display(), %error, "resync didSave failed");
                return ResyncStep::NotifyFailed;
            }
            self.document_tracker
                .mark_save_sent(path, server, version, *generation);
            if self.document_tracker.generation_for(server) != *generation {
                return ResyncStep::Deferred;
            }
        }
        self.notify_watched_files(path, &[lsp_types::FileChangeType::CHANGED])
            .await;
        ResyncStep::Done
    }

    /// Forget one path and tell every server holding it open, closing it.
    ///
    /// Called only for a path absent from disk: `resync_one_document`
    /// re-syncs everything still there instead. The caller holds `path`'s
    /// lock for the whole call -- see `resync_one_document`'s own guard --
    /// so a concurrent call for the same path cannot find the entry gone
    /// between its disk phase and its commit, or open the document again in
    /// the window between the close and the notify.
    ///
    /// A failed notify is logged rather than returned: the tracker entry is
    /// gone either way, so the next call still re-opens the document from
    /// whatever is on disk by then.
    async fn close_one_document_locked(&self, path: &Path) {
        let Some(state) = self.document_tracker.close(path) else {
            return;
        };
        for server in state.synced_servers() {
            let client = lock_std(&self.lsp_clients).get(&server).cloned();
            let Some(client) = client else { continue };
            let params = lsp_types::DidCloseTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: state.uri().clone(),
                },
            };
            if let Err(error) = client.notify("textDocument/didClose", params).await {
                tracing::warn!(
                    %server,
                    path = %path.display(),
                    %error,
                    "could not tell the server that a removed file is closed"
                );
            }
        }
    }

    /// Put `paths` on the invalidation queue the next resync drains.
    ///
    /// The queue is what makes a drain restartable, so a caller that has
    /// learned a file changed adds to it and then drives
    /// [`Self::resync_changed_documents`], rather than resyncing one path
    /// directly and losing the rest if it is cancelled.
    pub(crate) fn queue_invalidations(&self, paths: &[PathBuf]) {
        self.pending_invalidations.extend(paths);
    }

    /// Open a path the tracker has never held and send the server that
    /// routes its language the `didOpen` every later notification for that
    /// document builds on.
    ///
    /// Routing is resolved before anything is read, so a path no registered
    /// server handles costs nothing and stays untracked: a tracker slot
    /// spent on a document no server holds buys no diagnostics and brings
    /// the next tool call that much closer to the document limit.
    ///
    /// `has_headroom` says whether the caller can afford one more tracked
    /// document. It is read after routing rather than before the call, so a
    /// path nothing routes is never reported against the document limit it
    /// was not competing for.
    ///
    /// A route to a server that is still starting is [`OpenOutcome::Failed`],
    /// not [`OpenOutcome::NoRoute`]: servers spawn in the background, so a
    /// file created during startup routes to one that has not registered
    /// yet, and that file should be checked and was not. Only a language
    /// with no route at all is silent.
    ///
    /// The open runs through [`DocumentTracker::ensure_open`], which holds
    /// the path's own lock across the tracker insert and the notify, so a
    /// tool call opening the same path concurrently cannot have its version
    /// reset underneath it.
    ///
    /// `pub(crate)` because the host file-watcher sweep, in
    /// `crate::hooks::sweep`, is what learns a file was created.
    pub(crate) async fn open_untracked_document(
        &self,
        path: &Path,
        has_headroom: bool,
    ) -> OpenOutcome {
        let (server, client) = match self.get_client_for_file(path, ToolKind::Diagnostics) {
            Ok(resolved) => resolved,
            Err(Error::ServerInitializing { .. }) => return OpenOutcome::Failed,
            Err(_) => return OpenOutcome::NoRoute,
        };
        if !has_headroom {
            return OpenOutcome::NoHeadroom;
        }
        match self
            .document_tracker
            .ensure_open(path, &server, &client)
            .await
        {
            Ok(_) => OpenOutcome::Opened,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "could not open a changed file for its server"
                );
                OpenOutcome::Failed
            }
        }
    }

    /// Mark the set of servers that are expected (configured + applicable)
    /// but may still be initializing in the background.
    pub fn set_expected_servers(&self, servers: HashSet<ServerId>) {
        *lock_std(&self.expected_servers) = servers;
    }

    /// Clear the expected-servers set (e.g. after background init failed).
    pub fn clear_expected_servers(&self) {
        lock_std(&self.expected_servers).clear();
    }

    /// Install the per-tool routing table built from the applicable configs.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared, so this replaces the `Arc`-wrapped router wholesale.
    #[must_use]
    pub fn with_router(mut self, router: ToolRouter) -> Self {
        self.router = Arc::new(StdMutex::new(router));
        self
    }

    /// Rebind the routing table to the set of servers that actually
    /// registered, dropping or redirecting routes to servers that failed to
    /// spawn. See `ToolRouter::rebind_to_registered` for the full semantics.
    pub fn rebind_router(&self, registered: &HashSet<ServerId>) {
        lock_std(&self.router).rebind_to_registered(registered);
    }

    /// Whether `id` is the server the router currently resolves
    /// `ToolKind::Diagnostics` to for `language_id`.
    ///
    /// Purpose-built for `register_servers`, which needs this to compute the
    /// diagnostics-cache filter passed into each pump task, without exposing
    /// the router's lock guard outside this module.
    #[must_use]
    pub fn is_diagnostics_route(&self, language_id: &str, id: &ServerId) -> bool {
        lock_std(&self.router).resolve(language_id, ToolKind::Diagnostics) == Some(id)
    }

    /// Negotiated [`PositionEncoding`] of the registered server `id`, or the
    /// LSP spec's own default (UTF-16) if `id` is not currently registered.
    ///
    /// Note this falls back to UTF-16, not [`PositionEncoding::default`]
    /// (UTF-8): UTF-16 is what an absent/unrecognized negotiation means per
    /// the LSP spec and what [`crate::lsp::LspServer::spawn`] itself falls
    /// back to, so this must match rather than use the bridge type's own
    /// default, which exists only for `PositionEncoding`'s own internal use.
    #[must_use]
    pub(crate) fn position_encoding_for(&self, server_id: &ServerId) -> PositionEncoding {
        lock_std(&self.lsp_servers)
            .get(server_id)
            .and_then(|server| PositionEncoding::from_lsp(server.position_encoding().as_str()))
            .unwrap_or(PositionEncoding::Utf16)
    }

    /// Build the [`EncodingCtx`] for converting positions/ranges in
    /// responses from the registered server `id`.
    fn encoding_ctx(&self, server_id: &ServerId) -> EncodingCtx {
        EncodingCtx {
            encoding: self.position_encoding_for(server_id),
            tracker: self.document_tracker.clone(),
        }
    }

    /// Rebuilds `document_tracker` from `self.resource_limits` and
    /// `self.extension_map`, whatever the two are currently set to.
    ///
    /// Called by every builder that touches either input ([`Self::with_extensions`],
    /// [`Self::with_resource_limits`]), so each one only needs to set its own
    /// field and call this -- it always reads *both* current values, so the
    /// builders remain order-independent (see [`Self::with_resource_limits`])
    /// without each one needing to know the other's field. A future builder
    /// that adds a third tracker input should follow the same pattern:
    /// update its own field, then call this.
    fn rebuild_document_tracker(&mut self) {
        self.document_tracker = Arc::new(DocumentTracker::new(
            self.resource_limits,
            (*self.extension_map).clone(),
        ));
    }

    /// Configure custom file extension mappings.
    ///
    /// This method sets the extension map and updates the document tracker
    /// to use the same mappings for language detection.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared, so this replaces the `Arc`-wrapped fields wholesale.
    #[must_use]
    pub fn with_extensions(mut self, extension_map: HashMap<String, String>) -> Self {
        self.extension_map = Arc::new(extension_map);
        self.rebuild_document_tracker();
        self
    }

    /// Configure resource limits (max open documents, max file size) for the
    /// document tracker.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared. This builder and [`Self::with_extensions`] may be called in
    /// either order -- each rebuilds `document_tracker` from *both* of
    /// `self.resource_limits`/`self.extension_map`'s current values,
    /// instead of one of them starting fresh from
    /// `ResourceLimits::default()`/an empty extension map, which previously
    /// meant whichever builder ran last silently discarded the other's
    /// effect.
    #[must_use]
    pub fn with_resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.resource_limits = limits;
        self.rebuild_document_tracker();
        self
    }

    /// Register an LSP client under its routing identity.
    ///
    /// Only called once per server, from `register_servers` during initial
    /// background init. The respawn path does not reuse this method: it
    /// needs the previous client back (to fail its pending requests) and
    /// must also reset `document_tracker` for the swapped-in server, neither
    /// of which this method does.
    pub fn register_client(&self, id: impl Into<ServerId>, client: LspClient) {
        lock_std(&self.lsp_clients).insert(id.into(), client);
    }

    /// Register an LSP server under its routing identity.
    pub fn register_server(&self, id: impl Into<ServerId>, server: LspServer) {
        lock_std(&self.lsp_servers).insert(id.into(), server);
    }

    /// Store the config needed to respawn `id` if its process dies later.
    ///
    /// Called once per server, right after a successful spawn (see the
    /// crate-root `register_servers`); [`Self::respawn_if_dead`] is the only
    /// reader.
    pub(crate) fn register_server_config(&self, id: impl Into<ServerId>, config: ServerInitConfig) {
        lock_std(&self.server_configs).insert(id.into(), config);
    }

    /// Number of currently registered LSP servers.
    ///
    /// Test-only: `lsp_servers` is private, so this is the one way a test
    /// outside this module (e.g. `crate::tests`, exercising
    /// [`Translator::shutdown_servers`] indirectly through `serve_with`'s
    /// shutdown sequence) can observe that a registered server was actually
    /// drained.
    #[cfg(test)]
    pub(crate) fn registered_server_count(&self) -> usize {
        lock_std(&self.lsp_servers).len()
    }

    /// Snapshot of currently open document paths, used for MCP resource listing.
    #[must_use]
    pub fn open_document_paths(&self) -> Vec<PathBuf> {
        self.document_tracker.open_paths()
    }

    /// Whether a document is currently tracked as open.
    #[must_use]
    pub fn is_document_open(&self, path: &Path) -> bool {
        self.document_tracker.is_open(path)
    }

    /// The document tracker, shared with [`EncodingCtx`] so a cache-only
    /// caller (e.g. `get_cached_diagnostics`) can still prefer tracked
    /// in-memory content over a disk read when converting positions.
    #[must_use]
    pub(crate) const fn document_tracker(&self) -> &Arc<DocumentTracker> {
        &self.document_tracker
    }

    /// Gracefully shut down every registered LSP server.
    ///
    /// Drains the registered LSP servers and, for each one concurrently,
    /// sends the LSP `shutdown` request and `exit` notification via
    /// [`LspServer::shutdown`], bounded by a fixed per-server timeout. A
    /// server that errors or fails to respond in time is simply dropped
    /// instead: its child process handle is `kill_on_drop(true)`, so the
    /// process is killed rather than left running. Call this once, from the
    /// top-level shutdown path, after the MCP transport has stopped
    /// accepting new requests.
    ///
    /// # Limitations
    ///
    /// This only runs on the normal shutdown path (stdio EOF, `SIGTERM`/
    /// `SIGINT`, or the HTTP transport's own graceful shutdown). This crate's
    /// workspace `[profile.release]` builds with `panic = "abort"`, so a
    /// panic reachable from a request handler or background pump task in a
    /// release build still terminates the process without unwinding — this
    /// method never runs, and spawned LSP children are orphaned exactly as
    /// before this fix. Making that path safe would need process-group
    /// isolation (`kill_on_drop` alone doesn't help, since no `Drop` runs
    /// either); tracked separately, out of scope here.
    ///
    /// `pub(crate)` rather than `pub`: this is meant for exactly one call
    /// site (`serve_with`'s post-transport shutdown sequence), after the MCP
    /// transport is already down. An external caller invoking it mid-session
    /// would drain `lsp_servers` while `lsp_clients` (routing table) still
    /// points at the now-shut-down servers, so in-flight tool calls would
    /// resolve to a client whose server is gone.
    pub(crate) async fn shutdown_servers(&self) {
        if let Some(pumps) = self.notification_pumps.get() {
            pumps.shutdown().await;
        }
        let servers: Vec<(ServerId, LspServer)> = lock_std(&self.lsp_servers).drain().collect();
        if servers.is_empty() {
            return;
        }

        let mut tasks = tokio::task::JoinSet::new();
        for (id, server) in servers {
            tasks.spawn(async move {
                match tokio::time::timeout(SERVER_SHUTDOWN_TIMEOUT, server.shutdown()).await {
                    Ok(Ok(())) => tracing::debug!(%id, "LSP server shut down gracefully"),
                    Ok(Err(e)) => tracing::warn!(
                        %id, error = %e,
                        "LSP server shutdown handshake failed, killing process instead"
                    ),
                    Err(_) => tracing::warn!(
                        %id, timeout = ?SERVER_SHUTDOWN_TIMEOUT,
                        "LSP server did not shut down in time, killing process instead"
                    ),
                }
            });
        }
        tasks.join_all().await;
    }
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    use tokio::time::Duration;

    use self::testing::TranslatorHarness;
    use super::*;
    use crate::bridge::state::detect_language;
    use crate::config::{ServerId, ToolKind, ToolRouter};
    use crate::error::Error;

    #[test]
    fn test_translator_new() {
        let translator = Translator::new();
        assert_eq!(translator.workspace_roots.len(), 0);
        assert_eq!(lock_std(&translator.lsp_clients).len(), 0);
        assert_eq!(lock_std(&translator.lsp_servers).len(), 0);
    }

    #[test]
    fn test_set_workspace_roots() {
        let mut translator = Translator::new();
        let roots = vec![PathBuf::from("/test/root1"), PathBuf::from("/test/root2")];
        translator.set_workspace_roots(roots.clone());
        assert_eq!(*translator.workspace_roots, roots);
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_applier_for_refuses_a_tool_its_config_forbids() {
        let applier = std::sync::Arc::new(crate::bridge::apply::Applier::new(
            Vec::new(),
            crate::config::ApplyConfig {
                rename: true,
                ..crate::config::ApplyConfig::default()
            },
        ));
        let translator = Translator::new().with_applier(applier);

        assert!(
            translator
                .applier_for(ToolKind::Rename, "rename_symbol", "apply.rename")
                .is_ok()
        );

        let error = translator
            .applier_for(
                ToolKind::FormatDocument,
                "format_document",
                "apply.format_document",
            )
            .expect_err("format_document is not permitted");
        assert!(
            error.to_string().contains("apply.format_document"),
            "the error names the key that would permit it: {error}"
        );
    }

    #[test]
    fn test_a_default_translator_permits_no_writes() {
        let translator = Translator::new();
        for (tool, name, key) in [
            (ToolKind::Rename, "rename_symbol", "apply.rename"),
            (
                ToolKind::FormatDocument,
                "format_document",
                "apply.format_document",
            ),
            (
                ToolKind::CodeActions,
                "apply_code_action",
                "apply.code_actions",
            ),
        ] {
            assert!(
                translator.applier_for(tool, name, key).is_err(),
                "{name} must be refused with no [apply] table"
            );
        }
    }

    #[test]
    fn test_register_server() {
        let translator = Translator::new();

        // Initial state: no servers registered
        assert_eq!(lock_std(&translator.lsp_servers).len(), 0);

        // The register_server method exists and is callable
        // Full integration testing with real LspServer is done in integration tests
        // This unit test verifies the method signature and basic functionality

        // Note: We can't easily construct an LspServer in a unit test without async
        // and a real LSP server process. The actual registration functionality is
        // tested in integration tests (see rust_analyzer_tests.rs).
        // This test verifies the data structure is properly initialized.
    }

    /// #241: `shutdown_servers` on an empty registry must return immediately
    /// rather than blocking (e.g. on a `JoinSet` that's never populated).
    #[tokio::test]
    async fn test_shutdown_servers_empty_registry_returns_promptly() {
        let translator = Translator::new();

        let result =
            tokio::time::timeout(Duration::from_secs(1), translator.shutdown_servers()).await;

        assert!(
            result.is_ok(),
            "shutdown_servers must return promptly when no servers are registered"
        );
    }

    /// #241: `shutdown_servers` must drain every registered `LspServer` —
    /// this is the core behavior the issue is about (orphaned LSP children
    /// on shutdown). Uses `fake_lsp_server()` (mock `echo` child process,
    /// real `LspServer`, see `lsp::lifecycle`), which won't
    /// answer the LSP `shutdown` handshake — proving the drain completes,
    /// via the timeout/error fallback path, without hanging on
    /// non-responsive servers.
    #[tokio::test]
    async fn test_shutdown_servers_drains_registered_servers() {
        let translator = Translator::new();
        translator.register_server("server-a", crate::lsp::fake_lsp_server());
        translator.register_server("server-b", crate::lsp::fake_lsp_server());
        assert_eq!(lock_std(&translator.lsp_servers).len(), 2);

        // Bounded well above `SERVER_SHUTDOWN_TIMEOUT` (10s) so a genuine
        // regression (a hang) still fails the test instead of the harness
        // itself timing out ambiguously.
        let result =
            tokio::time::timeout(Duration::from_secs(20), translator.shutdown_servers()).await;

        assert!(
            result.is_ok(),
            "shutdown_servers must not hang against non-responsive mock servers"
        );
        assert_eq!(
            lock_std(&translator.lsp_servers).len(),
            0,
            "all registered servers must be drained"
        );
    }

    #[test]
    fn test_clear_expected_servers_reverts_to_no_server_after_all_routes_dropped() {
        // Mirrors the real `serve_with` flow: `rebind_router` (called from
        // `register_servers`/the all-failed path) drops routes to servers
        // that never registered, then `clear_expected_servers` runs under
        // the same lock. Subsequent lookups must fall back to
        // NoServerForLanguage rather than keep implying the server is still
        // on its way.
        let path = PathBuf::from("/ws/Assets/Scripts/Player.cs");
        let lang = detect_language(&path, &HashMap::new());
        let id = ServerId::from(lang.clone());

        let translator = Translator::new().with_router(ToolRouter::catch_all([(id.clone(), lang)]));
        let mut expected = HashSet::new();
        expected.insert(id);
        translator.set_expected_servers(expected);

        translator.rebind_router(&HashSet::new());
        translator.clear_expected_servers();

        let err = translator
            .get_client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        assert!(matches!(err, Error::NoServerForLanguage(_)));
    }

    #[test]
    fn test_translator_with_custom_extensions() {
        let mut extension_map = HashMap::new();
        extension_map.insert("nu".to_string(), "nushell".to_string());
        extension_map.insert("customext".to_string(), "customlang".to_string());

        let translator = Translator::new().with_extensions(extension_map.clone());

        assert_eq!(translator.extension_map.len(), 2);
        assert_eq!(
            translator.extension_map.get("nu"),
            Some(&"nushell".to_string())
        );
        assert_eq!(
            translator.extension_map.get("customext"),
            Some(&"customlang".to_string())
        );
    }

    /// `with_resource_limits` called before `with_extensions` (the order
    /// `serve()` uses) must reach `document_tracker`.
    #[test]
    fn test_with_resource_limits_applies_before_with_extensions() {
        let limits = ResourceLimits {
            max_documents: 1,
            max_file_size: 0,
        };
        let translator = Translator::new()
            .with_resource_limits(limits)
            .with_extensions(HashMap::new());

        translator
            .document_tracker
            .open(PathBuf::from("/tmp/a.rs"), "a".to_string())
            .unwrap();
        let err = translator
            .document_tracker
            .open(PathBuf::from("/tmp/b.rs"), "b".to_string())
            .unwrap_err();
        assert!(matches!(err, Error::DocumentLimitExceeded { max: 1, .. }));
    }

    /// `with_resource_limits` called *after* `with_extensions` (the reverse
    /// of `serve()`'s order) must still reach `document_tracker` -- the two
    /// builders must not clobber each other regardless of call order. See
    /// `Translator::with_resource_limits`'s docs.
    ///
    /// Uses a non-empty extension map (unlike the "before" test above) and
    /// asserts it survived `with_resource_limits`'s rebuild by checking the
    /// tracked document's resolved `language_id` -- a bug that dropped the
    /// extension map (e.g. rebuilding from `HashMap::new()` instead of
    /// `self.extension_map`) would leave `max_documents` correct but the
    /// extension map silently empty, which the "before" test alone cannot
    /// detect.
    #[test]
    fn test_with_resource_limits_applies_after_with_extensions() {
        let limits = ResourceLimits {
            max_documents: 1,
            max_file_size: 0,
        };
        let translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]))
            .with_resource_limits(limits);

        let path = PathBuf::from("/tmp/a.rs");
        translator
            .document_tracker
            .open(path.clone(), "a".to_string())
            .unwrap();
        let err = translator
            .document_tracker
            .open(PathBuf::from("/tmp/b.rs"), "b".to_string())
            .unwrap_err();
        assert!(matches!(err, Error::DocumentLimitExceeded { max: 1, .. }));

        let state = translator.document_tracker.close(&path).unwrap();
        assert_eq!(state.language_id(), "rust");
    }

    #[tokio::test]
    async fn test_a_rewritten_file_gets_a_change_then_a_save() {
        let harness = TranslatorHarness::with_one_server("rust").await;
        let path = harness.write_file("a.rs", "fn a() {}");
        harness.open(&path, "rust").await;
        harness.rewrite_file(&path, "fn a() -> i32 { }");
        harness.queue_invalidation(&path);

        harness.translator.resync_changed_documents().await;

        let sent = harness.notifications_for("rust");
        assert_eq!(
            sent.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["textDocument/didChange", "textDocument/didSave"],
            "the change carries the new text and the save is what starts a build"
        );
    }

    #[tokio::test]
    async fn test_a_file_the_apply_deleted_is_closed() {
        let harness = TranslatorHarness::with_one_server("rust").await;
        let path = harness.write_file("a.rs", "fn a() {}");
        harness.open(&path, "rust").await;
        std::fs::remove_file(&path).expect("remove");
        harness.queue_invalidation(&path);

        harness.translator.resync_changed_documents().await;

        assert_eq!(
            harness.notifications_for("rust"),
            vec!["textDocument/didClose"]
        );
        assert!(
            harness
                .translator
                .document_tracker()
                .snapshot(&path)
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_resync_opens_an_untracked_routed_path() {
        let harness = TranslatorHarness::with_one_server("rust").await;
        let path = harness.write_file("a.rs", "fn a() {}");
        harness.queue_invalidation(&path);

        harness.translator.resync_changed_documents().await;

        assert_eq!(
            harness.notifications_for("rust"),
            vec!["textDocument/didOpen", "textDocument/didSave"]
        );
        assert!(harness.translator.document_tracker().is_open(&path));
    }

    #[tokio::test]
    async fn test_resync_retains_unopened_targets_at_the_document_limit() {
        let harness = TranslatorHarness::with_one_server_and_limits(
            "rust",
            ResourceLimits {
                max_documents: 1,
                max_file_size: 0,
            },
        )
        .await;
        let anchor = harness.write_file("anchor.rs", "fn a() {}");
        let target = harness.write_file("target.rs", "fn b() {}");
        harness.open(&anchor, "rust").await;
        harness.register_watcher("rust", "sources", "**/*.rs");
        harness.queue_invalidation(&target);
        harness.translator.resync_changed_documents().await;
        assert_eq!(
            harness.translator.pending_invalidations.take(),
            vec![target.clone()]
        );
        assert!(!harness.translator.document_tracker().is_open(&target));
        assert_eq!(
            harness.notifications_for("rust"),
            vec!["workspace/didChangeWatchedFiles"]
        );
        harness.translator.document_tracker().close(&anchor);
        harness.queue_invalidation(&target);
        harness.translator.resync_changed_documents().await;
        assert!(harness.translator.document_tracker().is_open(&target));
        assert!(harness.translator.pending_invalidations.take().is_empty());
    }

    #[tokio::test]
    async fn test_resync_reopens_the_reset_route_with_a_healthy_other_owner() {
        use tokio::io::BufReader;
        use tokio::time::{Duration, timeout};

        for reset_under_lock in [false, true] {
            let harness = TranslatorHarness::with_one_server("rust").await;
            let path = harness.write_file("shared.rs", "fn a() {}");
            harness.open(&path, "rust").await;
            let (healthy_client, mut healthy_wire) = testing::fake_lsp_client();
            let healthy = ServerId::from("healthy");
            harness
                .translator
                .register_client(healthy.clone(), healthy_client.clone());
            let tracker = harness.translator.document_tracker();
            tracker
                .ensure_open(&path, &healthy, &healthy_client)
                .await
                .unwrap();
            let mut wire = BufReader::new(&mut healthy_wire.write_stdout);
            assert_eq!(
                timeout(
                    Duration::from_secs(5),
                    crate::test_support::read_framed_message(&mut wire)
                )
                .await
                .unwrap()["method"],
                "textDocument/didOpen"
            );
            assert_eq!(
                tracker.get(&path).unwrap().synced_version(&healthy),
                Some(1)
            );
            harness.queue_invalidation(&path);
            if reset_under_lock {
                let guard = tracker.lock_path(&path).await;
                let drain = harness.translator.resync_changed_documents();
                tokio::pin!(drain);
                assert!(futures::poll!(&mut drain).is_pending());
                tracker.forget_server(&ServerId::from("rust"));
                drop(guard);
                drain.await;
                assert_eq!(
                    harness.translator.pending_invalidations.take(),
                    vec![path.clone()],
                    "a route reset while waiting for the content lock must retain its save"
                );
                harness.queue_invalidation(&path);
            } else {
                tracker.forget_server(&ServerId::from("rust"));
            }
            harness.translator.resync_changed_documents().await;
            let state = tracker.get(&path).unwrap();
            assert_eq!(
                state.synced_version(&ServerId::from("rust")),
                Some(1),
                "the reset route must reopen even while another owner remains synced"
            );
            assert_eq!(state.synced_version(&healthy), Some(1));
            assert_eq!(
                harness.notifications_for("rust"),
                vec!["textDocument/didOpen", "textDocument/didSave"]
            );
            let saved = timeout(
                Duration::from_secs(5),
                crate::test_support::read_framed_message(&mut wire),
            )
            .await
            .unwrap();
            assert_eq!(saved["method"], "textDocument/didSave");
            assert_eq!(saved["params"]["textDocument"]["uri"], state.uri().as_str());
            assert_eq!(state.saved_version(&healthy), Some(1));
            assert!(harness.translator.pending_invalidations.take().is_empty());
        }
    }

    #[tokio::test]
    async fn test_resync_keeps_watches_for_non_routable_paths() {
        let harness = TranslatorHarness::with_one_server("rust").await;
        let path = harness.write_file("Cargo.toml", "[package]");
        harness.register_watcher("rust", "manifest", "**/Cargo.toml");
        harness.queue_invalidation(&path);
        harness.translator.resync_changed_documents().await;
        assert_eq!(
            harness.notifications_for("rust"),
            vec!["workspace/didChangeWatchedFiles"]
        );
        assert!(!harness.translator.document_tracker().is_open(&path));
        assert!(harness.translator.pending_invalidations.take().is_empty());
    }

    /// Matching disk content does not discharge the save owed after an
    /// interrupted drain has already recorded its change notification.
    #[tokio::test]
    async fn test_a_second_drain_sends_the_save_a_cancellation_lost() {
        let harness = TranslatorHarness::with_one_server("rust").await;
        let path = harness.write_file("a.rs", "fn a() {}");
        harness.open(&path, "rust").await;
        harness.rewrite_file(&path, "fn a() -> i32 { }");

        let server_id = ServerId::from("rust");
        let tracker = harness.translator.document_tracker();
        let guard = tracker.lock_path(&path).await;
        let resync = tracker
            .resync_from_disk(&path, &guard)
            .await
            .expect("resync_from_disk reads the rewritten file")
            .expect("the path is tracked");
        assert_eq!(
            resync.needs_change,
            vec![server_id.clone()],
            "the premise of this test is that a change is owed before setup runs"
        );
        assert_eq!(
            resync.needs_save,
            vec![server_id.clone()],
            "and that a save is too, or marking only the change proves nothing"
        );
        tracker.mark_change_sent(
            &path,
            &server_id,
            resync.version,
            tracker.generation_for(&server_id),
        );
        drop(guard);

        harness.queue_invalidation(&path);
        harness.translator.resync_changed_documents().await;

        assert_eq!(
            harness.notifications_for("rust"),
            vec!["textDocument/didSave".to_string()],
            "the content now matches disk, so a comparison alone would call this \
             finished and the file would never be checked"
        );
    }

    /// A failed notify must never be followed by the mark for it: that
    /// ordering is what lets a genuinely cancelled or refused send be
    /// retried correctly later, and it is the one property the test above
    /// cannot see, because a mark that ran early is invisible once the
    /// notify goes on to succeed anyway.
    ///
    /// Kills the transport before the drain starts rather than partway
    /// through: every notify then fails on its own, with no race to win
    /// against the drain's own, much faster, synchronous bookkeeping.
    #[tokio::test]
    async fn test_a_failed_notify_marks_nothing_and_stays_queued() {
        let harness = TranslatorHarness::with_one_server("rust").await;
        let server_id = ServerId::from("rust");

        // Both paths must be opened (and, for the second, pre-marked)
        // while the transport still works: `open` and `mark_change_sent`'s
        // own setup below need a live connection, even though the drain
        // this test actually exercises must not.
        let path = harness.write_file("a.rs", "fn a() {}");
        harness.open(&path, "rust").await;
        harness.rewrite_file(&path, "fn a() -> i32 { }");

        let saved_path = harness.write_file("b.rs", "fn b() {}");
        harness.open(&saved_path, "rust").await;
        harness.rewrite_file(&saved_path, "fn b() -> i32 { }");
        let tracker = harness.translator.document_tracker();
        let guard = tracker.lock_path(&saved_path).await;
        let resync = tracker
            .resync_from_disk(&saved_path, &guard)
            .await
            .expect("resync_from_disk reads the rewritten file")
            .expect("the path is tracked");
        tracker.mark_change_sent(
            &saved_path,
            &server_id,
            resync.version,
            tracker.generation_for(&server_id),
        );
        drop(guard);

        harness.kill_transport("rust").await;
        let dead_client = lock_std(&harness.translator.lsp_clients)
            .get(&server_id)
            .cloned()
            .expect("the server stays registered, just dead");
        assert!(
            dead_client
                .notify("textDocument/didChange", serde_json::Value::Null)
                .await
                .is_err(),
            "the premise of this test is that every notify on this connection now fails"
        );

        // A fresh rewrite owes both a change and a save. The didChange
        // itself now fails, so this half catches a mark_change_sent that
        // ran before its notify.
        harness.queue_invalidation(&path);

        harness.translator.resync_changed_documents().await;

        let state = harness
            .translator
            .document_tracker()
            .snapshot(&path)
            .expect("a failed notify does not close the document");
        let version = state.version();
        assert_eq!(
            state.servers_needing_change(version),
            vec![server_id.clone()],
            "a premature mark_change_sent would clear this even though the \
             didChange never reached the wire"
        );
        assert_eq!(
            state.servers_needing_save(version),
            vec![server_id.clone()],
            "the save is owed too, since the change never went out to unlock it"
        );
        assert_eq!(
            harness.translator.pending_invalidations.take(),
            vec![path],
            "a failed notify leaves the path queued for a later drain"
        );

        // `saved_path`'s change was already marked sent above, before the
        // transport died, which isolates the save loop specifically: with
        // nothing left to do but the save, this half catches a
        // mark_save_sent that ran before its own notify -- the mutation
        // the first half cannot reach, since its didChange fails before
        // the save loop ever starts.
        harness.queue_invalidation(&saved_path);

        harness.translator.resync_changed_documents().await;

        let state = harness
            .translator
            .document_tracker()
            .snapshot(&saved_path)
            .expect("a failed notify does not close the document");
        assert!(
            state.servers_needing_change(state.version()).is_empty(),
            "the change was already marked sent by this test's own setup"
        );
        assert_eq!(
            state.servers_needing_save(state.version()),
            vec![server_id],
            "a premature mark_save_sent would clear this even though the \
             didSave never reached the wire"
        );
        assert_eq!(
            harness.translator.pending_invalidations.take(),
            vec![saved_path],
            "a failed notify leaves the path queued for a later drain"
        );
    }

    /// A path whose disk read fails (here, by growing past the configured
    /// size limit) says nothing about any other path. It must not stall
    /// the drain: the healthy path behind it still gets resynchronized,
    /// and the failing path alone stays queued for a later attempt.
    #[tokio::test]
    async fn test_a_read_failure_leaves_only_that_path_queued_behind_it() {
        let limits = ResourceLimits {
            max_documents: 0,
            max_file_size: 16,
        };
        let harness = TranslatorHarness::with_one_server_and_limits("rust", limits).await;

        let big = harness.write_file("big.rs", "fn a() {}");
        let small = harness.write_file("small.rs", "fn b() {}");
        harness.open(&big, "rust").await;
        harness.open(&small, "rust").await;

        harness.rewrite_file(&big, "fn a_but_now_far_too_long_to_fit_the_limit() {}");
        harness.rewrite_file(&small, "fn c() {}");
        harness.queue_invalidation(&big);
        harness.queue_invalidation(&small);

        harness.translator.resync_changed_documents().await;

        assert_eq!(
            harness.notifications_for("rust"),
            vec![
                "textDocument/didChange".to_string(),
                "textDocument/didSave".to_string(),
            ],
            "the failing path must not block the healthy one queued behind it"
        );
        assert_eq!(
            harness.translator.pending_invalidations.take(),
            vec![big],
            "only the path whose own read failed stays queued"
        );
    }

    /// `Path::exists()` reports `false` for any stat error, not only
    /// absence -- an inaccessible parent directory looks identical to a
    /// deleted file. Closing the document on that basis would drop a
    /// document whose file never moved. Unix-only: the permission trick
    /// this drives has no Windows equivalent.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_stat_error_leaves_the_document_tracked_and_queued() {
        use std::os::unix::fs::PermissionsExt;

        let harness = TranslatorHarness::with_one_server("rust").await;
        let path = harness.write_file("a.rs", "fn a() {}");
        harness.open(&path, "rust").await;
        harness.queue_invalidation(&path);

        let dir = path.parent().expect("a.rs has a parent").to_path_buf();
        let original_mode = std::fs::metadata(&dir)
            .expect("stat the directory")
            .permissions()
            .mode();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000))
            .expect("lock the directory down");
        let inaccessible = std::fs::metadata(&path).is_err();

        harness.translator.resync_changed_documents().await;

        // Restore access before any assertion can panic and skip this,
        // leaving the harness's own `TempDir` unable to clean itself up.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(original_mode))
            .expect("restore the directory's permissions");

        if !inaccessible {
            eprintln!(
                "permission fixture unavailable: runner can stat through a mode-000 directory"
            );
            return;
        }

        assert!(
            harness.notifications_for("rust").is_empty(),
            "a stat error must not close the document or notify anyone"
        );
        assert_eq!(
            harness.translator.pending_invalidations.take(),
            vec![path.clone()],
            "the path stays queued for a later attempt, not treated as gone"
        );
        assert!(
            harness
                .translator
                .document_tracker()
                .snapshot(&path)
                .is_some(),
            "a stat error is not proof the file is gone; the document stays tracked"
        );
    }

    #[tokio::test]
    async fn test_a_watching_server_is_told_an_applied_file_changed() {
        let harness = TranslatorHarness::with_one_server("go").await;
        harness.register_watcher("go", "r1", "**/*.go");
        let path = harness.write_file("main.go", "package main");
        harness.open(&path, "go").await;
        harness.rewrite_file(&path, "package main\nfunc a() {}");
        harness.queue_invalidation(&path);

        harness.translator.resync_changed_documents().await;

        assert!(
            harness
                .notifications_for("go")
                .contains(&"workspace/didChangeWatchedFiles".to_string()),
            "gopls at default settings runs no watcher of its own, so this \
             notification is the only way it learns a rename rewrote this file"
        );
    }

    #[tokio::test]
    async fn test_a_server_that_registered_nothing_is_not_told() {
        let harness = TranslatorHarness::with_one_server("go").await;
        let path = harness.write_file("main.go", "package main");
        harness.open(&path, "go").await;
        harness.rewrite_file(&path, "package main\nfunc a() {}");
        harness.queue_invalidation(&path);

        harness.translator.resync_changed_documents().await;

        assert!(
            !harness
                .notifications_for("go")
                .contains(&"workspace/didChangeWatchedFiles".to_string())
        );
    }

    #[tokio::test]
    async fn test_a_deleted_file_is_reported_as_deleted() {
        let harness = TranslatorHarness::with_one_server("go").await;
        harness.register_watcher("go", "r1", "**/*.go");
        let path = harness.write_file("main.go", "package main");
        harness.open(&path, "go").await;
        std::fs::remove_file(&path).expect("remove");
        harness.queue_invalidation(&path);

        harness.translator.resync_changed_documents().await;

        let params = harness
            .watched_files_params("go")
            .pop()
            .expect("a notification went out");
        assert_eq!(
            params["changes"][0]["type"],
            serde_json::json!(3),
            "the kind comes from the file being absent, not from anything the \
             apply summary said, because a cancellation loses that summary"
        );
    }

    #[tokio::test]
    async fn test_an_untracked_applied_file_is_still_reported() {
        let harness = TranslatorHarness::with_one_server("go").await;
        harness.register_watcher("go", "r1", "**/*.go");
        let path = harness.write_file("other.go", "package main");
        harness.queue_invalidation(&path);

        harness.translator.resync_changed_documents().await;

        assert!(
            harness
                .notifications_for("go")
                .contains(&"workspace/didChangeWatchedFiles".to_string()),
            "a rename's fanout writes files no tool call ever opened, and those \
             are exactly what a watching server has no other way to learn about"
        );
    }

    /// The registry is what gates the notification, so clearing a server's
    /// registrations silences it. That a respawn actually does the clearing
    /// is `respawn`'s own test.
    #[tokio::test]
    async fn test_forgetting_a_server_s_registrations_stops_the_notification() {
        let harness = TranslatorHarness::with_one_server("go").await;
        harness.register_watcher("go", "r1", "**/*.go");
        harness
            .translator
            .forget_watch_registrations(&ServerId::from("go"));
        let path = harness.write_file("main.go", "package main");
        harness.open(&path, "go").await;
        harness.rewrite_file(&path, "package main\nfunc a() {}");
        harness.queue_invalidation(&path);

        harness.translator.resync_changed_documents().await;

        assert!(
            !harness
                .notifications_for("go")
                .contains(&"workspace/didChangeWatchedFiles".to_string())
        );
    }
}
