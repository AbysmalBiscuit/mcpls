//! Debounced sweep of changed paths.
//!
//! A Claude Code hook reports every path a tool touched, with no batching of
//! its own: a `cargo fmt` across fifty files reports fifty separate events,
//! each spawning its own hook process that connects whenever the OS gets
//! around to it. Acting on each one as it arrives would restart a server's
//! flycheck, cancelling the check already in flight, once per path -- a run
//! of cancelled checks and no diagnostics at all. [`Sweeper`] collects paths
//! as they arrive and acts on the whole set once it goes quiet, off the hook
//! connection that reported them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};

use lsp_types::FileChangeType;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::bridge::{OpenOutcome, ServerLifecycle, Translator, lock_std};
use crate::hooks::filters::PathFilter;

/// What a stat says a pending path actually is.
///
/// Derived from the filesystem at sweep time, never from the event kind the
/// host sent. A hook process is spawned per event and arrives late, and a
/// save through a temporary file and a rename produces `unlink` for a file
/// that exists again by now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepKind {
    /// Absent from disk.
    Deleted,
    /// Present, and the tracker is not holding it.
    Created,
    /// Present, and the tracker already holds it.
    Changed,
}

/// Where a pending path came from, which decides whether it may start a
/// language server that is not running.
///
/// A hook reports what the agent itself did, which is the signal the lazy
/// spawn design starts a server on. The watcher reports what the disk did,
/// which includes a `git pull`, a background formatter and another editor,
/// none of which is this session asking for a language it does not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A host hook reported an edit the agent made.
    Hook,
    /// The project watcher saw the path change on disk.
    Watcher,
}

/// Collects changed paths and acts on them once the burst settles.
pub struct Sweeper {
    translator: Arc<Translator>,
    filter: Arc<PathFilter>,
    quiet_for: Duration,
    max_documents: usize,
    /// Paths admitted since the last sweep, each with where it came from.
    ///
    /// Holds one entry per distinct admitted path between two sweeps, and a
    /// sweep empties it, so its size follows how much actually changed
    /// rather than how many events arrived: an unlink and an add for the
    /// same save are one entry.
    pending: StdMutex<HashMap<PathBuf, Origin>>,
    /// When the most recent path arrived; `None` before the first one ever
    /// does.
    last_arrival: StdMutex<Option<Instant>>,
    /// How many sweeps have run.
    sweeps_run: AtomicUsize,
    /// What the last sweep decided each path was.
    last_kinds: StdMutex<Vec<(PathBuf, SweepKind)>>,
    /// How many untracked paths the last sweep opened.
    last_opened_count: AtomicUsize,
    /// Files the last sweep could not check, and why.
    last_shortfall: StdMutex<Option<String>>,
    /// When the last sweep began taking the pending set, or `None` before
    /// any sweep has.
    ///
    /// Read by the project watcher: an inotify overflow is answered by a
    /// full walk, and this is what tells that walk which of the files it
    /// finds may have had an event dropped.
    last_sweep_at: StdMutex<Option<SystemTime>>,
    /// Publishes the sweep count each time a sweep finishes.
    ///
    /// A watcher subscribed before the sweep it cares about sees exactly one
    /// change when that sweep completes, so a caller can await the event a
    /// sweep happened instead of assuming `run`'s task has already been
    /// polled -- under a paused clock, advancing time moves the clock but
    /// does not itself guarantee a separately spawned task gets scheduled.
    completed: watch::Sender<usize>,
}

impl Sweeper {
    /// A sweeper that admits paths through `filter`, acts on them through
    /// `translator`, and sweeps the pending set once it has been quiet for
    /// `quiet_for`. A finite `max_documents` leaves one document slot for
    /// ordinary requests; zero allows unbounded background opens.
    #[must_use]
    pub fn new(
        translator: Arc<Translator>,
        filter: PathFilter,
        quiet_for: Duration,
        max_documents: usize,
    ) -> Self {
        Self {
            translator,
            filter: Arc::new(filter),
            quiet_for,
            max_documents,
            pending: StdMutex::new(HashMap::new()),
            last_arrival: StdMutex::new(None),
            sweeps_run: AtomicUsize::new(0),
            last_kinds: StdMutex::new(Vec::new()),
            last_opened_count: AtomicUsize::new(0),
            last_shortfall: StdMutex::new(None),
            last_sweep_at: StdMutex::new(None),
            completed: watch::channel(0).0,
        }
    }

    /// The paths that belong to a configured root, canonicalized so a
    /// symlink alias and its target resolve to one key.
    ///
    /// Canonicalizing costs a `stat` per path on the hook connection,
    /// which has a deadline to answer within. An attributed write has to
    /// claim the same key the published diagnostic carries, and only the
    /// filesystem resolves aliases to it. What each path actually is
    /// still waits for sweep time, off that connection.
    pub fn admitted_paths(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        paths
            .iter()
            .map(|path| crate::bridge::apply::normalize(path))
            .filter(|path| self.filter.admits(path))
            .collect()
    }

    /// Queue paths a host hook reported. Returns how many survived the
    /// filters.
    pub fn enqueue(&self, paths: &[PathBuf]) -> usize {
        self.enqueue_from(paths, Origin::Hook)
    }

    /// Admit `paths` and queue what survives. Returns how many.
    ///
    /// A path already pending from a hook keeps that origin when the
    /// watcher reports it too, which it will: the agent's own write is a
    /// disk event like any other. Letting the watcher overwrite it would
    /// take away the spawn the agent's edit earned.
    pub fn enqueue_from(&self, paths: &[PathBuf], origin: Origin) -> usize {
        self.queue_admitted(&self.admitted_paths(paths), origin)
    }

    /// Queue what [`Sweeper::admitted_paths`] returned. Reports how many.
    ///
    /// Takes the admitted collection rather than raw paths so a hook's
    /// writer claim and its queued paths cannot disagree about which
    /// files the edit touched. Holds no lock longer than an insert.
    pub fn queue_admitted(&self, admitted: &[PathBuf], origin: Origin) -> usize {
        for path in admitted {
            lock_std(&self.pending)
                .entry(path.clone())
                .and_modify(|held| {
                    if origin == Origin::Hook {
                        *held = Origin::Hook;
                    }
                })
                .or_insert(origin);
        }
        if !admitted.is_empty() {
            *lock_std(&self.last_arrival) = Some(Instant::now());
        }
        admitted.len()
    }

    /// The filter this sweeper admits paths through, so the project
    /// watcher can place its watches by the same ruling rather than a
    /// second one built beside it.
    pub(crate) fn filter(&self) -> Arc<PathFilter> {
        Arc::clone(&self.filter)
    }

    /// Run until `cancel` fires, sweeping whenever the set goes quiet.
    pub async fn run(self: Arc<Self>, mut cancel: watch::Receiver<bool>) {
        if *cancel.borrow() {
            return;
        }
        let period = (self.quiet_for / 4).max(Duration::from_millis(1));
        let mut ticker = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if self.due() {
                        self.sweep().await;
                    }
                }
                result = cancel.changed() => {
                    // Err means the sender was dropped; treat as cancellation.
                    if result.is_err() || *cancel.borrow() {
                        return;
                    }
                }
            }
        }
    }

    /// Files the last sweep could not check, and why.
    #[must_use]
    pub fn last_shortfall(&self) -> Option<String> {
        lock_std(&self.last_shortfall).clone()
    }

    /// When the last sweep began taking the pending set.
    pub(crate) fn last_sweep_at(&self) -> Option<SystemTime> {
        *lock_std(&self.last_sweep_at)
    }

    /// Whether the pending set is non-empty and has gone quiet for
    /// `quiet_for`.
    fn due(&self) -> bool {
        if lock_std(&self.pending).is_empty() {
            return false;
        }
        lock_std(&self.last_arrival).is_some_and(|arrival| arrival.elapsed() >= self.quiet_for)
    }

    /// Sweep immediately, ignoring the debounce. Test-only.
    #[cfg(test)]
    pub(crate) async fn sweep_now(&self) {
        self.sweep().await;
    }

    /// How many sweeps have run. Test-only.
    #[cfg(test)]
    pub(crate) fn sweeps_run(&self) -> usize {
        self.sweeps_run.load(Ordering::Relaxed)
    }

    /// What the last sweep decided each path was. Test-only.
    #[cfg(test)]
    pub(crate) fn last_kinds(&self) -> Vec<(PathBuf, SweepKind)> {
        lock_std(&self.last_kinds).clone()
    }

    /// How many untracked paths the last sweep opened. Test-only.
    #[cfg(test)]
    pub(crate) fn opened_count(&self) -> usize {
        self.last_opened_count.load(Ordering::Relaxed)
    }

    /// Set what the last sweep could not check, and why. Test-only.
    #[cfg(test)]
    pub(crate) fn set_shortfall_for_test(&self, shortfall: &str) {
        *lock_std(&self.last_shortfall) = Some(shortfall.to_string());
    }

    /// How many paths are waiting for the next sweep. Test-only.
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        lock_std(&self.pending).len()
    }

    /// The paths waiting for the next sweep. Test-only.
    #[cfg(test)]
    pub(crate) fn pending_paths(&self) -> std::collections::HashSet<PathBuf> {
        lock_std(&self.pending).keys().cloned().collect()
    }

    async fn ensure_servers_for_edits(
        &self,
        kinds: &[(PathBuf, SweepKind)],
        origins: &HashMap<PathBuf, Origin>,
    ) -> Vec<PathBuf> {
        let mut waiting = Vec::new();
        for (path, kind) in kinds {
            if *kind == SweepKind::Deleted || !self.filter.routable_extension(path) {
                continue;
            }
            // Only what the agent itself did may start a server. A `git
            // pull` is not this session asking for a language it does not
            // use.
            if origins.get(path) != Some(&Origin::Hook) {
                continue;
            }
            let Some(id) = self.translator.server_for_path(path) else {
                continue;
            };
            match self.translator.lifecycle_of(&id) {
                None | Some(ServerLifecycle::Running) => continue,
                Some(_) => {}
            }

            // Do not hold this sweeper behind one language's cold start.
            let _ = self.translator.ensure_server(&id, None).await;
            if self.translator.lifecycle_of(&id) == Some(ServerLifecycle::Starting) {
                waiting.push(path.clone());
            }
        }
        waiting
    }

    /// A receiver that changes once the next sweep completes.
    ///
    /// Subscribe before triggering the activity under test, then await
    /// `changed()` (under a timeout) rather than assuming `run`'s task has
    /// been polled. Test-only.
    #[cfg(test)]
    pub(crate) fn subscribe_completions(&self) -> watch::Receiver<usize> {
        self.completed.subscribe()
    }

    /// Take the whole pending set and act on it.
    ///
    /// A path the tracker has never held is opened first, up to the
    /// remaining headroom, so its server gets the `didOpen` every later
    /// notification for that document builds on. Opened paths then join the
    /// tracked and the deleted ones in `translator`'s own drain, which stats
    /// each one, closes the ones that are truly gone, resyncs the rest, and
    /// sends the `didSave` a server whose diagnostics come from a build runs
    /// its check on. Whatever is left -- past the headroom, unopenable, or
    /// routed to no server at all -- is named only to the servers that
    /// registered a watcher glob for it, which costs no tracker slot.
    ///
    /// A path just opened is handed to the drain rather than saved directly,
    /// which costs it a second read of a file the open has already read.
    /// That buys the drain's restartability: the save stays owed on the
    /// queue until it lands, so a sweep dropped partway through leaves the
    /// remaining saves for the next one instead of losing them.
    async fn sweep(&self) {
        let started = SystemTime::now();
        let origins: HashMap<PathBuf, Origin> = lock_std(&self.pending).drain().collect();
        if origins.is_empty() {
            return;
        }
        // Before the stats below, not after them: a file written while
        // this sweep runs must count as changed since it.
        *lock_std(&self.last_sweep_at) = Some(started);
        let paths: Vec<PathBuf> = origins.keys().cloned().collect();
        let tracker = self.translator.document_tracker();
        let mut kinds = Vec::with_capacity(paths.len());
        let mut settle = Vec::new();
        let mut untracked = Vec::new();

        for path in paths {
            let kind = if path.try_exists().unwrap_or(false) {
                if tracker.is_open(&path) {
                    SweepKind::Changed
                } else {
                    SweepKind::Created
                }
            } else {
                SweepKind::Deleted
            };
            kinds.push((path.clone(), kind));
            if kind == SweepKind::Created {
                untracked.push(path);
            } else {
                settle.push(path);
            }
        }

        // Run before opening untracked paths: changed tracked files skip the
        // later open trigger.
        let waiting = self.ensure_servers_for_edits(&kinds, &origins).await;

        if !waiting.is_empty() {
            let mut pending = lock_std(&self.pending);
            for path in &waiting {
                // Only a hook-origin path can be waiting, since only a
                // hook-origin path is offered a spawn at all.
                pending.insert(path.clone(), Origin::Hook);
            }
            drop(pending);
            // Filter before the open loop consumes untracked paths.
            settle.retain(|path| !waiting.contains(path));
            untracked.retain(|path| !waiting.contains(path));
        }

        let open_count = tracker.open_paths().len();
        let headroom = if self.max_documents == 0 {
            usize::MAX
        } else {
            self.max_documents
                .saturating_sub(open_count)
                .saturating_sub(1)
        };
        let mut opened = 0;
        let mut over_limit = 0;
        let mut unopened = 0;
        let mut watched_only = Vec::new();

        for path in untracked {
            if !self.filter.routable_extension(&path) {
                watched_only.push(path);
                continue;
            }
            // The other half of the rule `ensure_servers_for_edits`
            // applies. A created path never reaches that check, because a
            // path the tracker has never held is opened rather than
            // ensured -- and the open resolves a client, which starts the
            // server. So a `git pull` adding one `src/new_module.rs`, or a
            // rescan after an overflow, would start every configured
            // server the checkout has. Only a server that is already up
            // may take a watcher-origin created path; the rest goes to
            // `watched_only`, where the servers that registered a glob for
            // it still hear about it and no process starts.
            if origins.get(&path) == Some(&Origin::Watcher)
                && self.translator.server_for_path(&path).is_none_or(|id| {
                    self.translator.lifecycle_of(&id) != Some(ServerLifecycle::Running)
                })
            {
                watched_only.push(path);
                continue;
            }
            match self
                .translator
                .open_untracked_document(&path, opened < headroom)
                .await
            {
                OpenOutcome::Opened => {
                    opened += 1;
                    settle.push(path);
                }
                OpenOutcome::NoRoute => watched_only.push(path),
                OpenOutcome::NoHeadroom => {
                    over_limit += 1;
                    watched_only.push(path);
                }
                OpenOutcome::Failed => {
                    unopened += 1;
                    watched_only.push(path);
                }
            }
        }

        self.translator.queue_invalidations(&settle);
        self.translator.resync_changed_documents().await;

        // Everything here is a path present on disk that the tracker is not
        // holding right now, which is equally what a file the host has just
        // made and a file mcpls never opens as a document -- a `go.work`
        // edited in place -- look like, and nothing mcpls keeps tells the two
        // apart. Naming one guessed kind to everybody silently drops every
        // server whose watcher registered for the other, so both go out and
        // each server hears the one it asked for. `CHANGED` comes first
        // because it is what every server used to be told, and preferring it
        // keeps every delivery that already worked.
        for path in watched_only {
            self.translator
                .notify_watched_files(&path, &[FileChangeType::CHANGED, FileChangeType::CREATED])
                .await;
        }

        *lock_std(&self.last_shortfall) =
            Self::shortfall(over_limit, unopened, headroom, self.max_documents);
        *lock_std(&self.last_kinds) = kinds;
        self.last_opened_count.store(opened, Ordering::Relaxed);
        let sweeps_run = self.sweeps_run.fetch_add(1, Ordering::Relaxed) + 1;
        // No receivers is not an error: a caller that never subscribed just
        // never learns a sweep happened, which is fine outside tests.
        let _ = self.completed.send(sweeps_run);
    }

    /// The line naming the files a sweep left unchecked, or `None` when it
    /// checked everything it took on.
    ///
    /// Both reasons are reported, and separately: a run of files declined
    /// for the document limit is answered by raising the limit, while one
    /// that could not be opened at all is not, and a single count would
    /// send a reader after the wrong one.
    fn shortfall(
        over_limit: usize,
        unopened: usize,
        headroom: usize,
        max_documents: usize,
    ) -> Option<String> {
        let mut reasons = Vec::new();
        if over_limit > 0 {
            reasons.push(format!(
                "{over_limit} file(s) not checked: background sweep headroom of {headroom} was exhausted; coverage was skipped to reserve one document slot (document limit {max_documents})"
            ));
        }
        if unopened > 0 {
            reasons.push(format!(
                "{unopened} file(s) not checked: they could not be opened"
            ));
        }
        (!reasons.is_empty()).then(|| reasons.join("; "))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;
    use crate::bridge::{ResourceLimits, ServerLifecycle, Translator, TranslatorHarness};
    use crate::config::{ServerId, ToolRouter};

    /// The language and server id every served test uses.
    const SERVER: &str = "rust";

    /// A `Sweeper` over a temporary workspace, with its `run` loop already
    /// spawned so the debounce tests can drive it with `tokio::time`.
    ///
    /// The translator has no registered servers, so nothing the sweep would
    /// send reaches anyone: these tests are about which paths the sweep
    /// picks up, what it decides each one is, and where it stops.
    /// [`ServedSweeper`] is what a test asserting on notifications uses.
    /// Keeping the two apart also keeps the debounce tests free of the
    /// real disk and pipe I/O an open performs, which a paused clock can
    /// auto-advance straight past.
    struct TestSweeper {
        sweeper: Arc<Sweeper>,
        translator: Arc<Translator>,
        dir: TempDir,
        _cancel: tokio::sync::watch::Sender<bool>,
    }

    /// So a test can write `sweeper.enqueue(...)` rather than
    /// `sweeper.sweeper.enqueue(...)`.
    impl std::ops::Deref for TestSweeper {
        type Target = Sweeper;
        fn deref(&self) -> &Self::Target {
            &self.sweeper
        }
    }

    impl TestSweeper {
        /// An absolute path under the workspace. Creates nothing.
        fn path(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }

        /// An absolute path under the workspace, with an empty file at it.
        fn write(&self, rel: &str) -> PathBuf {
            let path = self.path(rel);
            std::fs::write(&path, "").expect("write");
            path
        }
    }

    fn test_sweeper(quiet_for: Duration) -> TestSweeper {
        test_sweeper_with_ceiling(quiet_for, usize::MAX)
    }

    fn test_sweeper_with_ceiling(quiet_for: Duration, max_documents: usize) -> TestSweeper {
        let dir = tempfile::tempdir().expect("a temp dir");
        sweeper_over(dir, Translator::new(), quiet_for, max_documents)
    }

    /// A sweeper whose `rust` server is applicable and has never been
    /// triggered, which is what a lazy backend looks like before the agent
    /// touches the language.
    fn idle_sweeper() -> TestSweeper {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_string(), SERVER.to_string())]))
            .with_router(ToolRouter::catch_all([(
                ServerId::from(SERVER),
                SERVER.to_string(),
            )]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);
        let sweeper = sweeper_over(dir, translator, Duration::from_secs(60), usize::MAX);
        sweeper
            .translator
            .set_lifecycle(&ServerId::from(SERVER), ServerLifecycle::Idle);
        sweeper
    }

    fn sweeper_over(
        dir: TempDir,
        translator: Translator,
        quiet_for: Duration,
        max_documents: usize,
    ) -> TestSweeper {
        let filter = PathFilter::new(
            Arc::from(vec![dir.path().to_path_buf()]),
            Arc::new(HashMap::from([("rs".to_string(), SERVER.to_string())])),
            None,
        );
        let translator = Arc::new(translator);
        let sweeper = Arc::new(Sweeper::new(
            Arc::clone(&translator),
            filter,
            quiet_for,
            max_documents,
        ));
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(Arc::clone(&sweeper).run(cancel_rx));
        TestSweeper {
            sweeper,
            translator,
            dir,
            _cancel: cancel_tx,
        }
    }

    /// A `Sweeper` over a workspace with one fake `rust` server recording
    /// every notification it receives, so a test can assert on what the
    /// sweep sent and not only on what it decided.
    ///
    /// `run` is not spawned here: every test that uses this drives
    /// `sweep_now` itself, and a background loop that could also sweep
    /// would make what the server received depend on which of the two got
    /// there first.
    struct ServedSweeper {
        sweeper: Arc<Sweeper>,
        harness: TranslatorHarness,
    }

    impl std::ops::Deref for ServedSweeper {
        type Target = Sweeper;
        fn deref(&self) -> &Self::Target {
            &self.sweeper
        }
    }

    impl ServedSweeper {
        /// An absolute path under the workspace, with `contents` at it.
        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            self.harness.write_file(rel, contents)
        }

        /// Track `path` and let its server see the `didOpen` -- the state a
        /// file the agent has already asked a tool about is in.
        ///
        /// The notifications recorded so far are dropped, so a later
        /// assertion sees what the sweep sent rather than this setup.
        async fn open(&self, path: &Path) {
            self.harness.open(path, SERVER).await;
        }

        /// The LSP methods the fake server received, in order.
        fn notifications(&self) -> Vec<String> {
            self.harness.notifications_for(SERVER)
        }

        /// The single file event of each `didChangeWatchedFiles` the fake
        /// server received, in order.
        fn watched_file_events(&self) -> Vec<serde_json::Value> {
            self.harness
                .watched_files_params(SERVER)
                .iter()
                .map(|params| params["changes"][0].clone())
                .collect()
        }

        /// The single file event of the last `didChangeWatchedFiles` the
        /// fake server received.
        fn last_watched_file_event(&self) -> serde_json::Value {
            self.watched_file_events()
                .pop()
                .expect("a watched-files notification went out")
        }
    }

    async fn served_sweeper(max_documents: usize) -> ServedSweeper {
        served_sweeper_with_limits(max_documents, ResourceLimits::default()).await
    }

    async fn served_sweeper_with_limits(
        max_documents: usize,
        limits: ResourceLimits,
    ) -> ServedSweeper {
        let harness = TranslatorHarness::with_one_server_and_limits(SERVER, limits).await;
        let filter = PathFilter::new(
            Arc::from(vec![harness.root().to_path_buf()]),
            Arc::new(HashMap::from([("rs".to_string(), SERVER.to_string())])),
            None,
        );
        let sweeper = Arc::new(Sweeper::new(
            Arc::clone(&harness.translator),
            filter,
            Duration::from_millis(500),
            max_documents,
        ));
        ServedSweeper { sweeper, harness }
    }

    /// A served sweeper whose filter reads the same watch registry the
    /// translator answers from, and whose server has registered `watchers`.
    ///
    /// Both ends have to see the registration: the filter is what admits a
    /// path no extension routes, and the translator is what decides which
    /// servers hear about it.
    async fn sweeper_watching(watchers: &serde_json::Value) -> ServedSweeper {
        let harness = TranslatorHarness::with_one_server(SERVER).await;
        harness.register_watchers(SERVER, "r1", watchers);
        let filter = PathFilter::new(
            Arc::from(vec![harness.root().to_path_buf()]),
            Arc::new(HashMap::from([("rs".to_string(), SERVER.to_string())])),
            Some(harness.watch_registry()),
        );
        let sweeper = Arc::new(Sweeper::new(
            Arc::clone(&harness.translator),
            filter,
            Duration::from_millis(500),
            usize::MAX,
        ));
        ServedSweeper { sweeper, harness }
    }

    /// Poll `ready` until it holds, letting other tasks run in between.
    ///
    /// Bounded, so a condition that will never hold fails this test instead
    /// of hanging the run.
    async fn wait_until(ready: impl Fn() -> bool + Send + Sync) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the condition to hold within the timeout");
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_burst_produces_one_sweep() {
        let sweeper = test_sweeper(Duration::from_millis(500));
        let mut completed = sweeper.subscribe_completions();
        let mut written = Vec::with_capacity(50);
        for i in 0..50 {
            let path = sweeper.write(&format!("f{i}.rs"));
            sweeper.enqueue(std::slice::from_ref(&path));
            written.push(path);
            tokio::time::advance(Duration::from_millis(10)).await;
        }
        tokio::time::advance(Duration::from_millis(600)).await;

        // Advancing a paused clock moves the clock but does not by itself
        // guarantee `run`'s spawned task gets polled again; await the
        // sweep's own completion signal (which pulls the clock forward via
        // auto-advance as needed) rather than assuming it already ran.
        tokio::time::timeout(Duration::from_secs(5), completed.changed())
            .await
            .expect("a sweep to complete within the timeout")
            .expect("the sweeper task to still be running");

        assert_eq!(
            sweeper.sweeps_run(),
            1,
            "every didSave restarts rust-analyzer's flycheck and cancels the \
             check in flight, so a cargo fmt forwarded one path at a time \
             produces fifty cancelled checks and no diagnostics at all"
        );

        let mut swept: Vec<PathBuf> = sweeper
            .last_kinds()
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        swept.sort();
        written.sort();
        assert_eq!(
            swept, written,
            "a coalesced sweep that dropped paths on the floor would still \
             pass a bare count of one sweep; every enqueued path must show \
             up in the set the single sweep covered"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_path_arriving_during_the_quiet_period_restarts_it() {
        let sweeper = test_sweeper(Duration::from_millis(500));
        let mut completed = sweeper.subscribe_completions();
        let a = sweeper.write("a.rs");
        sweeper.enqueue(std::slice::from_ref(&a));
        tokio::time::advance(Duration::from_millis(400)).await;
        let b = sweeper.write("b.rs");
        sweeper.enqueue(std::slice::from_ref(&b));
        tokio::time::advance(Duration::from_millis(400)).await;

        // A bare read here would also pass against a sweeper that was never
        // spawned. Blocking on the completion signal instead makes the
        // runtime run the timer up to 1 ms short of when the restarted
        // quiet period expires, so this asserts the sweeper looked and
        // declined rather than merely that nothing was noticed.
        assert!(
            tokio::time::timeout(Duration::from_millis(99), completed.changed())
                .await
                .is_err(),
            "the burst has not settled"
        );

        tokio::time::advance(Duration::from_millis(200)).await;
        // Same reasoning as the burst test: wait for the sweep's own signal
        // rather than assume the advance above already polled `run`.
        tokio::time::timeout(Duration::from_secs(5), completed.changed())
            .await
            .expect("a sweep to complete within the timeout")
            .expect("the sweeper task to still be running");
        assert_eq!(sweeper.sweeps_run(), 1);
    }

    #[tokio::test]
    async fn test_a_deleted_path_is_swept_as_a_delete_whatever_the_host_said() {
        let sweeper = test_sweeper(Duration::from_millis(10));
        let path = sweeper.write("gone.rs");
        sweeper.enqueue(std::slice::from_ref(&path));
        std::fs::remove_file(&path).expect("remove");
        sweeper.sweep_now().await;

        assert_eq!(sweeper.last_kinds(), vec![(path, SweepKind::Deleted)]);
    }

    #[tokio::test]
    async fn test_an_atomic_save_is_swept_as_a_change_not_a_delete() {
        let sweeper = served_sweeper(1).await;
        let path = sweeper.write("saved.rs", "fn main() {}");
        sweeper.open(&path).await;

        // A save through a temporary file and a rename emits unlink and then
        // add, and the hook process for each connects after both have already
        // happened: the file is on disk at both enqueues, and only the stat
        // at sweep time can say what it really is.
        sweeper.enqueue(std::slice::from_ref(&path));
        std::fs::write(&path, "fn main() { todo!() }").expect("the rename puts it back");
        sweeper.enqueue(std::slice::from_ref(&path));

        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.last_kinds(),
            vec![(path, SweepKind::Changed)],
            "acting on the host's unlink would close a live document and tell \
             every watching server the file was deleted, and the two events \
             for one save are one change, not two"
        );
        assert_eq!(
            sweeper.notifications(),
            vec!["textDocument/didChange", "textDocument/didSave"],
            "the server is told the new content and that it was saved -- and \
             never that the document closed"
        );
    }

    #[tokio::test]
    async fn test_a_created_file_is_opened_and_saved_on_its_server() {
        let sweeper = served_sweeper(usize::MAX).await;
        let path = sweeper.write("fresh.rs", "fn main() {}");
        sweeper.enqueue(std::slice::from_ref(&path));
        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.notifications(),
            vec!["textDocument/didOpen", "textDocument/didSave"],
            "a file no server has seen has to be opened before any later \
             notification can name it, and the didSave is what makes a server \
             whose diagnostics come from a build run one: a sweep that took \
             the tracker slot and sent neither would cost the next tool call \
             a document and produce no diagnostics at all"
        );
        assert_eq!(sweeper.last_kinds(), vec![(path, SweepKind::Created)]);
        assert_eq!(sweeper.opened_count(), 1);
    }

    /// The whole delivery path for a file mcpls never opens as a document:
    /// a `Cargo.toml`, a `go.work`, a `.proto`. Nothing routes its
    /// extension, so it costs no tracker slot and gets no `didOpen`, and
    /// the one thing that ever reaches its server is this notification.
    #[tokio::test]
    async fn test_a_watched_file_no_extension_routes_is_named_to_its_server() {
        let sweeper = sweeper_watching(&serde_json::json!([{ "globPattern": "**/go.work" }])).await;
        let path = sweeper.write("go.work", "go 1.22\n");

        assert_eq!(sweeper.enqueue(std::slice::from_ref(&path)), 1);
        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.watched_file_events().len(),
            1,
            "this watcher accepts both of the kinds offered for the path, and \
             a server that hears about the same file once per kind it would \
             have taken runs its check twice for one edit"
        );
        let event = sweeper.last_watched_file_event();
        assert_eq!(
            event["uri"],
            serde_json::json!(
                crate::bridge::path_to_uri(&path)
                    .expect("a uri for the fixture")
                    .as_str()
            )
        );
        assert_eq!(
            event["type"],
            serde_json::json!(2),
            "a server whose watcher takes every kind hears the likelier of the \
             two this path could be"
        );
        assert!(
            !sweeper
                .notifications()
                .contains(&"textDocument/didOpen".to_string()),
            "opening it would spend a tracker slot on a file no server routes"
        );
    }

    /// gopls watches `go.work` for creation and deletion and for nothing
    /// else. The sweep cannot tell a file it has never held from one just
    /// made, so naming a single guessed kind to everybody leaves this
    /// server hearing nothing at all.
    #[tokio::test]
    async fn test_a_watcher_that_wants_only_creations_hears_about_one() {
        let sweeper =
            sweeper_watching(&serde_json::json!([{ "globPattern": "**/go.work", "kind": 5 }]))
                .await;
        let path = sweeper.write("go.work", "go 1.22\n");

        assert_eq!(sweeper.enqueue(std::slice::from_ref(&path)), 1);
        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.last_watched_file_event()["type"],
            serde_json::json!(1)
        );
    }

    #[tokio::test]
    async fn test_a_file_the_sweep_cannot_open_is_reported_as_not_checked() {
        let sweeper = served_sweeper_with_limits(
            usize::MAX,
            ResourceLimits {
                max_documents: 0,
                max_file_size: 8,
            },
        )
        .await;
        let path = sweeper.write("oversized.rs", "fn main() { todo!() }");
        sweeper.enqueue(std::slice::from_ref(&path));
        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.last_shortfall().expect("a shortfall line"),
            "1 file(s) not checked: they could not be opened",
            "a file the sweep gave up on inside the headroom is as unchecked \
             as one it never reached, and reported separately because \
             raising the document limit would not have helped this one"
        );
        assert_eq!(sweeper.opened_count(), 0);
        assert!(sweeper.notifications().is_empty());
    }

    /// A hand-built translator has no spawn handle, so a trigger publishes
    /// `Failed`. Keeping the document tracked isolates the trigger from the
    /// separate open path for created files.
    #[tokio::test]
    async fn test_an_edit_starts_the_language_server() {
        let sweeper = idle_sweeper();
        let path = sweeper.write("main.rs");
        let (transport, _fake_server) = crate::test_support::fake_lsp_transport();
        let client = crate::lsp::LspClient::from_transport(
            crate::config::LspServerConfig::rust_analyzer(),
            transport,
        );
        sweeper
            .translator
            .document_tracker()
            .ensure_open(&path, &ServerId::from(SERVER), &client)
            .await
            .expect("seed a tracked document");
        sweeper
            .translator
            .set_lifecycle(&ServerId::from(SERVER), ServerLifecycle::Idle);
        sweeper.enqueue(std::slice::from_ref(&path));

        sweeper.sweep_now().await;

        assert_ne!(
            sweeper.translator.lifecycle_of(&ServerId::from(SERVER)),
            Some(ServerLifecycle::Idle)
        );
    }

    /// Seed `path` as a document the tracker already holds, which is what
    /// a file the agent has asked a tool about looks like. Isolates the
    /// spawn trigger from the separate open path for created files, the
    /// same way [`test_an_edit_starts_the_language_server`] does.
    ///
    /// Every test that calls this is about the `SweepKind::Changed`
    /// branch, and only that branch. The origin rule has a second half
    /// that a tracked path never reaches -- a created path is opened
    /// rather than ensured, and the open resolves a client of its own --
    /// which the `_created_` tests below cover instead.
    async fn seed_tracked(sweeper: &TestSweeper, path: &Path) {
        let (transport, _fake_server) = crate::test_support::fake_lsp_transport();
        let client = crate::lsp::LspClient::from_transport(
            crate::config::LspServerConfig::rust_analyzer(),
            transport,
        );
        sweeper
            .translator
            .document_tracker()
            .ensure_open(path, &ServerId::from(SERVER), &client)
            .await
            .expect("seed a tracked document");
        sweeper
            .translator
            .set_lifecycle(&ServerId::from(SERVER), ServerLifecycle::Idle);
    }

    /// The `SweepKind::Changed` half of the origin rule: a path the
    /// tracker already holds, which `ensure_servers_for_edits` is what
    /// decides the spawn for.
    #[tokio::test]
    async fn test_a_watcher_change_does_not_start_a_language_server() {
        let sweeper = idle_sweeper();
        let path = sweeper.write("main.rs");
        seed_tracked(&sweeper, &path).await;

        sweeper.enqueue_from(std::slice::from_ref(&path), Origin::Watcher);
        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.translator.lifecycle_of(&ServerId::from(SERVER)),
            Some(ServerLifecycle::Idle),
            "a git pull in a mixed checkout must not start rust-analyzer for \
             a session that only ever opens TypeScript, which is the cost the \
             lazy spawn design exists to remove"
        );
    }

    /// The `SweepKind::Created` half of the same rule, which the tracked
    /// test above cannot reach. A path the tracker has never held skips
    /// `ensure_servers_for_edits` and is handed to
    /// `open_untracked_document` instead, which resolves a client and so
    /// starts the server -- the same cold start by the other door.
    #[tokio::test]
    async fn test_a_watcher_created_path_does_not_start_a_language_server() {
        let sweeper = idle_sweeper();
        let path = sweeper.write("new_module.rs");

        sweeper.enqueue_from(std::slice::from_ref(&path), Origin::Watcher);
        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.last_kinds(),
            vec![(path.clone(), SweepKind::Created)],
            "the control: a path the tracker has never held must reach the \
             sweep as Created, or this test is asserting about the branch the \
             tracked test already covers"
        );
        assert_eq!(
            sweeper.translator.lifecycle_of(&ServerId::from(SERVER)),
            Some(ServerLifecycle::Idle),
            "a git pull adding one src/new_module.rs must not start \
             rust-analyzer in a session that only ever opened TypeScript, and \
             an overflow rescan or a git checkout creating a directory must \
             not start every configured server at once"
        );
        assert_eq!(
            sweeper.opened_count(),
            0,
            "the path must not be opened either: opening is what resolves the \
             client that starts the server"
        );
    }

    /// The `SweepKind::Changed` half of the positive control.
    #[tokio::test]
    async fn test_a_hook_change_still_starts_a_language_server() {
        let sweeper = idle_sweeper();
        let path = sweeper.write("main.rs");
        seed_tracked(&sweeper, &path).await;

        sweeper.enqueue_from(std::slice::from_ref(&path), Origin::Hook);
        sweeper.sweep_now().await;

        assert_ne!(
            sweeper.translator.lifecycle_of(&ServerId::from(SERVER)),
            Some(ServerLifecycle::Idle),
            "an agent's own edit is the signal the lazy spawn design starts a \
             server on, and the origin rule must not take it away"
        );
    }

    /// The `SweepKind::Created` half of the positive control: the rule
    /// above withholds the spawn from the watcher, and must not withhold
    /// it from the agent creating a file in a language it has not used
    /// yet, which is lazy spawn's actual trigger.
    #[tokio::test]
    async fn test_a_hook_created_path_still_starts_a_language_server() {
        let sweeper = idle_sweeper();
        let path = sweeper.write("new_module.rs");

        sweeper.enqueue_from(std::slice::from_ref(&path), Origin::Hook);
        sweeper.sweep_now().await;

        assert_ne!(
            sweeper.translator.lifecycle_of(&ServerId::from(SERVER)),
            Some(ServerLifecycle::Idle),
            "a file the agent itself made is the session asking for that \
             language, and narrowing the created branch must not disable the \
             trigger lazy spawn exists to serve"
        );
    }

    /// The other side of the created rule: a watcher-origin path whose
    /// server is already up costs nothing to open, and declining it would
    /// leave a running server holding no document for a file that exists.
    #[tokio::test]
    async fn test_a_watcher_created_path_is_opened_when_the_server_runs() {
        let sweeper = served_sweeper(usize::MAX).await;
        sweeper
            .harness
            .translator
            .set_lifecycle(&ServerId::from(SERVER), ServerLifecycle::Running);
        let path = sweeper.write("new_module.rs", "fn main() {}");

        sweeper.enqueue_from(std::slice::from_ref(&path), Origin::Watcher);
        sweeper.sweep_now().await;

        assert_eq!(sweeper.opened_count(), 1);
        assert!(
            sweeper
                .notifications()
                .contains(&"textDocument/didOpen".to_string())
        );
    }

    #[tokio::test]
    async fn test_a_hook_origin_survives_a_watcher_report_of_the_same_path() {
        let sweeper = idle_sweeper();
        let path = sweeper.write("main.rs");
        seed_tracked(&sweeper, &path).await;

        sweeper.enqueue_from(std::slice::from_ref(&path), Origin::Hook);
        sweeper.enqueue_from(std::slice::from_ref(&path), Origin::Watcher);
        sweeper.sweep_now().await;

        assert_ne!(
            sweeper.translator.lifecycle_of(&ServerId::from(SERVER)),
            Some(ServerLifecycle::Idle),
            "the agent's own write reaches the watcher too, so a watcher \
             report landing second must not cancel the spawn the edit earned"
        );
    }

    #[tokio::test]
    async fn test_a_path_waiting_on_a_starting_server_is_swept_again() {
        let sweeper = idle_sweeper();
        sweeper
            .translator
            .set_lifecycle(&ServerId::from(SERVER), ServerLifecycle::Starting);
        let path = sweeper.write("main.rs");
        sweeper.enqueue(std::slice::from_ref(&path));

        sweeper.sweep_now().await;

        assert!(
            sweeper.pending_paths().contains(&path),
            "a path whose server is still handshaking comes back on the \
             next tick instead of being checked against a server that \
             cannot answer yet"
        );
    }

    /// A terminal state is not retried forever: pending work stays due after
    /// its quiet period, so re-queueing it on every tick would starve watcher
    /// notifications.
    #[tokio::test]
    async fn test_a_path_whose_server_is_not_installed_is_not_held_back() {
        let sweeper = idle_sweeper();
        sweeper
            .translator
            .set_lifecycle(&ServerId::from(SERVER), ServerLifecycle::NotInstalled);
        let path = sweeper.write("main.rs");
        sweeper.enqueue(std::slice::from_ref(&path));

        sweeper.sweep_now().await;

        assert!(
            sweeper.pending_paths().is_empty(),
            "a server whose binary is missing is not going to be there on \
             the next tick either"
        );
    }

    #[tokio::test]
    async fn test_a_deleted_path_starts_nothing() {
        let sweeper = idle_sweeper();
        sweeper.enqueue(&[sweeper.path("gone.rs")]);

        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.translator.lifecycle_of(&ServerId::from(SERVER)),
            Some(ServerLifecycle::Idle)
        );
    }

    /// Servers spawn in the background, so a file created moments after
    /// startup routes to one that has not registered yet. Treating that
    /// like a language nothing routes would leave the file unopened,
    /// unsaved and unmentioned; it comes back on the next sweep instead.
    #[tokio::test]
    async fn test_a_file_whose_server_is_still_starting_comes_back() {
        let sweeper = idle_sweeper();
        sweeper
            .translator
            .set_lifecycle(&ServerId::from(SERVER), ServerLifecycle::Starting);
        let path = sweeper.write("fresh.rs");
        sweeper.enqueue(std::slice::from_ref(&path));
        sweeper.sweep_now().await;

        assert!(sweeper.pending_paths().contains(&path));
        assert_eq!(sweeper.opened_count(), 0);
        assert_eq!(
            sweeper.last_shortfall(),
            None,
            "a path that is coming back is not a path that was skipped"
        );
    }

    #[tokio::test]
    async fn test_a_file_no_server_routes_is_not_blamed_on_the_ceiling() {
        // No room for another document at all, so every path this sweep
        // declines to open would be blamed on the ceiling by an accounting
        // that decided before it asked whether anything routes them.
        let sweeper = test_sweeper_with_ceiling(Duration::from_secs(60), 1);
        let paths: Vec<PathBuf> = (0..3).map(|i| sweeper.write(&format!("f{i}.rs"))).collect();
        sweeper.enqueue(&paths);
        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.last_shortfall(),
            None,
            "nothing routes these, so no amount of room would have checked \
             them: naming them as casualties of the document limit sends a \
             reader after a limit that was never the problem"
        );
    }

    #[tokio::test]
    async fn test_a_path_arriving_during_a_sweep_lands_in_the_next_one() {
        let sweeper = test_sweeper(Duration::from_secs(60));
        let shortfall = "1 file(s) not checked: they could not be opened";
        sweeper.set_shortfall_for_test(shortfall);
        let first = sweeper.write("first.rs");
        std::fs::remove_file(&first).expect("remove");
        sweeper.enqueue(std::slice::from_ref(&first));

        // The resync takes each path's own lock, so holding it keeps the
        // sweep from finishing while this test enqueues. An empty pending
        // set says the sweep has taken its own and is past the point of
        // picking anything else up, which is the window a hook connection
        // can land a path in.
        let tracker = Arc::clone(sweeper.translator.document_tracker());
        let held = tracker.lock_path(&first).await;
        let running = tokio::spawn({
            let sweeper = Arc::clone(&sweeper.sweeper);
            async move { sweeper.sweep_now().await }
        });
        wait_until(|| sweeper.pending_len() == 0).await;
        assert_eq!(sweeper.last_shortfall().as_deref(), Some(shortfall));

        let second = sweeper.write("second.rs");
        sweeper.enqueue(std::slice::from_ref(&second));
        drop(held);
        running.await.expect("the sweep task");
        assert_eq!(sweeper.last_shortfall(), None);

        assert_eq!(
            sweeper.last_kinds(),
            vec![(first, SweepKind::Deleted)],
            "a sweep acts on the set it drained and nothing that arrived after"
        );
        assert_eq!(
            sweeper.pending_len(),
            1,
            "a path that arrives mid-sweep has to survive to the next one: \
             dropping it means the file the agent just wrote is never checked"
        );

        sweeper.sweep_now().await;
        assert_eq!(sweeper.last_kinds(), vec![(second, SweepKind::Created)]);
    }

    #[tokio::test]
    async fn test_the_sweep_stops_short_of_the_document_ceiling() {
        let sweeper = served_sweeper(3).await;
        let paths: Vec<PathBuf> = (0..10)
            .map(|i| sweeper.write(&format!("f{i}.rs"), "fn main() {}"))
            .collect();
        sweeper.enqueue(&paths);
        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.last_shortfall().expect("a shortfall line"),
            "8 file(s) not checked: background sweep headroom of 2 was exhausted; coverage was skipped to reserve one document slot (document limit 3)",
            "asserted whole rather than by substring: a contains('8') would \
             also match 18, 28 or '8 of 80'"
        );
        assert!(
            sweeper.opened_count() < 3,
            "filling the tracker would make the next unrelated tool call fail \
             with DocumentLimitExceeded, which is a worse outcome than not \
             checking some files"
        );
    }
}
