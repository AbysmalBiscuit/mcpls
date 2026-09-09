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

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use lsp_types::FileChangeType;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::bridge::{OpenOutcome, Translator, lock_std};
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

/// Collects changed paths and acts on them once the burst settles.
pub struct Sweeper {
    translator: Arc<Translator>,
    filter: PathFilter,
    quiet_for: Duration,
    max_documents: usize,
    /// Paths admitted since the last sweep, deduplicated.
    ///
    /// Holds one entry per distinct admitted path between two sweeps, and a
    /// sweep empties it, so its size follows how much the host actually
    /// changed rather than how many events it sent: an unlink and an add
    /// for the same save are one entry.
    pending: StdMutex<HashSet<PathBuf>>,
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
    /// `quiet_for`. Never opens more than `max_documents` documents beyond
    /// however many `translator` already holds open.
    #[must_use]
    pub fn new(
        translator: Arc<Translator>,
        filter: PathFilter,
        quiet_for: Duration,
        max_documents: usize,
    ) -> Self {
        Self {
            translator,
            filter,
            quiet_for,
            max_documents,
            pending: StdMutex::new(HashSet::new()),
            last_arrival: StdMutex::new(None),
            sweeps_run: AtomicUsize::new(0),
            last_kinds: StdMutex::new(Vec::new()),
            last_opened_count: AtomicUsize::new(0),
            last_shortfall: StdMutex::new(None),
            completed: watch::channel(0).0,
        }
    }

    /// Queue paths. Returns how many survived the filters.
    ///
    /// Answers from the path and the configured roots alone -- no
    /// filesystem call, no lock held longer than an insert -- because this
    /// runs on the hook connection, which has a deadline to answer within.
    /// What each path actually is gets decided at sweep time, off that
    /// connection.
    pub fn enqueue(&self, paths: &[PathBuf]) -> usize {
        let mut admitted = 0;
        for path in paths {
            if !self.filter.admits(path) {
                continue;
            }
            admitted += 1;
            lock_std(&self.pending).insert(path.clone());
        }
        if admitted > 0 {
            *lock_std(&self.last_arrival) = Some(Instant::now());
        }
        admitted
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
        let paths: Vec<PathBuf> = lock_std(&self.pending).drain().collect();
        if paths.is_empty() {
            return;
        }
        // Cleared before the work rather than only overwritten after it: a
        // read landing mid-sweep must not be answered with the previous
        // sweep's line.
        *lock_std(&self.last_shortfall) = None;

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

        let headroom = self
            .max_documents
            .saturating_sub(tracker.open_paths().len());
        let mut opened = 0;
        let mut over_limit = 0;
        let mut unopened = 0;
        let mut watched_only = Vec::new();

        for path in untracked {
            if !self.filter.routable_extension(&path) {
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

        *lock_std(&self.last_shortfall) = Self::shortfall(over_limit, unopened, self.max_documents);
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
    fn shortfall(over_limit: usize, unopened: usize, max_documents: usize) -> Option<String> {
        let mut reasons = Vec::new();
        if over_limit > 0 {
            reasons.push(format!(
                "{over_limit} file(s) not checked: the document limit of {max_documents} was reached"
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
    use crate::bridge::{ResourceLimits, Translator, TranslatorHarness};
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

    /// A sweeper whose router names a server that has not registered yet --
    /// the state mcpls is in while it spawns its servers in the background,
    /// and the state a file created moments after startup meets.
    fn initializing_sweeper() -> TestSweeper {
        let dir = tempfile::tempdir().expect("a temp dir");
        let server = ServerId::from(SERVER);
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_string(), SERVER.to_string())]))
            .with_router(ToolRouter::catch_all([(
                server.clone(),
                SERVER.to_string(),
            )]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);
        translator.set_expected_servers(HashSet::from([server]));
        sweeper_over(dir, translator, Duration::from_secs(60), usize::MAX)
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
        let sweeper = Arc::new(Sweeper::new(
            Arc::new(translator),
            filter,
            quiet_for,
            max_documents,
        ));
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(Arc::clone(&sweeper).run(cancel_rx));
        TestSweeper {
            sweeper,
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
        let sweeper = served_sweeper(usize::MAX).await;
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

    #[tokio::test]
    async fn test_a_file_whose_server_is_still_starting_is_not_checked() {
        let sweeper = initializing_sweeper();
        let path = sweeper.write("fresh.rs");
        sweeper.enqueue(std::slice::from_ref(&path));
        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.last_shortfall().expect("a shortfall line"),
            "1 file(s) not checked: they could not be opened",
            "servers spawn in the background, so a file created moments \
             after startup routes to one that has not registered yet; \
             treating that like a language nothing routes leaves the file \
             unopened, unsaved, and unmentioned"
        );
        assert_eq!(sweeper.opened_count(), 0);
    }

    #[tokio::test]
    async fn test_a_file_no_server_routes_is_not_blamed_on_the_ceiling() {
        // No room for another document at all, so every path this sweep
        // declines to open would be blamed on the ceiling by an accounting
        // that decided before it asked whether anything routes them.
        let sweeper = test_sweeper_with_ceiling(Duration::from_secs(60), 0);
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

        let second = sweeper.write("second.rs");
        sweeper.enqueue(std::slice::from_ref(&second));
        drop(held);
        running.await.expect("the sweep task");

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
            "7 file(s) not checked: the document limit of 3 was reached",
            "asserted whole rather than by substring: a contains('7') would \
             also match 17, 27 or '7 of 70'"
        );
        assert!(
            sweeper.opened_count() <= 3,
            "filling the tracker would make the next unrelated tool call fail \
             with DocumentLimitExceeded, which is a worse outcome than not \
             checking some files"
        );
    }
}
