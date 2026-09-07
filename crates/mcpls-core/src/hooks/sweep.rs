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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use lsp_types::FileChangeType;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::bridge::{DocumentTracker, Translator, lock_std};
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
    /// Present, and the tracker has never held it.
    Created,
    /// Present, and either tracked or seen before.
    Changed,
}

/// Collects changed paths and acts on them once the burst settles.
pub struct Sweeper {
    translator: Arc<Translator>,
    filter: PathFilter,
    quiet_for: Duration,
    max_documents: usize,
    /// Paths admitted since the last sweep, deduplicated.
    pending: StdMutex<HashSet<PathBuf>>,
    /// The subset of `pending` that was absent from disk the moment it
    /// arrived -- what a host's `unlink` looks like. A path in here is never
    /// opened as a new document even if it turns out to exist by sweep time:
    /// the host's own view is that this path already existed and is now
    /// gone, which is a change to resync, not a document to create.
    arrived_absent: StdMutex<HashSet<PathBuf>>,
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
            arrived_absent: StdMutex::new(HashSet::new()),
            last_arrival: StdMutex::new(None),
            sweeps_run: AtomicUsize::new(0),
            last_kinds: StdMutex::new(Vec::new()),
            last_opened_count: AtomicUsize::new(0),
            last_shortfall: StdMutex::new(None),
            completed: watch::channel(0).0,
        }
    }

    /// Queue paths. Returns how many survived the filters.
    pub fn enqueue(&self, paths: &[PathBuf]) -> usize {
        let mut admitted = 0;
        for path in paths {
            if !self.filter.admits(path) {
                continue;
            }
            admitted += 1;
            lock_std(&self.pending).insert(path.clone());
            if path.try_exists().unwrap_or(false) {
                lock_std(&self.arrived_absent).remove(path);
            } else {
                lock_std(&self.arrived_absent).insert(path.clone());
            }
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
    /// Tracked and deleted paths (and any that arrived while absent, even if
    /// they exist again by now) are handed to `translator`'s own drain,
    /// which stats each one, closes the ones that are truly gone, resyncs
    /// the rest, and notifies the servers watching them. Paths untracked and
    /// never reported absent are opened as new documents, up to the
    /// remaining headroom, or just watched-files-notified past it.
    async fn sweep(&self) {
        let paths: Vec<PathBuf> = lock_std(&self.pending).drain().collect();
        if paths.is_empty() {
            return;
        }
        let arrived_absent: HashSet<PathBuf> = lock_std(&self.arrived_absent).drain().collect();
        *lock_std(&self.last_shortfall) = None;

        let tracker = self.translator.document_tracker();
        let mut kinds = Vec::with_capacity(paths.len());
        let mut settle = Vec::new();
        let mut untracked = Vec::new();

        for path in paths {
            if !path.try_exists().unwrap_or(false) {
                kinds.push((path.clone(), SweepKind::Deleted));
                settle.push(path);
            } else if tracker.is_open(&path) || arrived_absent.contains(&path) {
                kinds.push((path.clone(), SweepKind::Changed));
                settle.push(path);
            } else {
                untracked.push(path);
            }
        }

        self.translator.queue_invalidations(&settle);
        self.translator.resync_changed_documents().await;

        let headroom = self
            .max_documents
            .saturating_sub(tracker.open_paths().len());
        let mut opened = 0;
        let mut skipped = 0;

        for path in untracked {
            let routable = self.filter.routable_extension(&path);
            let within_headroom = opened < headroom;
            let created = routable && within_headroom && Self::try_open(tracker, &path);

            if created {
                opened += 1;
                kinds.push((path.clone(), SweepKind::Created));
            } else {
                if routable && !within_headroom {
                    skipped += 1;
                }
                kinds.push((path.clone(), SweepKind::Changed));
            }
            self.translator
                .notify_watched_files(&path, FileChangeType::CHANGED)
                .await;
        }

        if skipped > 0 {
            *lock_std(&self.last_shortfall) = Some(format!(
                "{skipped} file(s) not checked: the document limit of {} was reached",
                self.max_documents
            ));
        }

        *lock_std(&self.last_kinds) = kinds;
        self.last_opened_count.store(opened, Ordering::Relaxed);
        let sweeps_run = self.sweeps_run.fetch_add(1, Ordering::Relaxed) + 1;
        // No receivers is not an error: a caller that never subscribed just
        // never learns a sweep happened, which is fine outside tests.
        let _ = self.completed.send(sweeps_run);
    }

    /// Read `path` fresh and open it as a new tracked document. A read
    /// failure or a limit rejection is logged and treated as not opened,
    /// leaving the caller to fall back to a plain watched-files notify.
    ///
    /// Reads synchronously rather than through `tokio::fs`: the sweep runs
    /// off the request path, so briefly blocking its own task costs nothing
    /// a caller is waiting on, and it keeps this step on the same virtual
    /// clock as the rest of the sweep under a paused-time test instead of
    /// handing off to the runtime's separate blocking-thread pool.
    fn try_open(tracker: &DocumentTracker, path: &Path) -> bool {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "could not read a changed file the sweep found"
                );
                return false;
            }
        };
        match tracker.open(path.to_path_buf(), content) {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "could not open a changed file the sweep found"
                );
                false
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;

    use tempfile::TempDir;

    use super::*;
    use crate::bridge::Translator;

    /// A `Sweeper` over a temporary workspace, with its `run` loop already
    /// spawned so the debounce tests can drive it with `tokio::time`.
    ///
    /// The translator has no registered servers. Nothing here asserts on
    /// what reaches a server: these tests are about which paths the sweep
    /// picks up, what it decides each one is, and where it stops. The
    /// notifications are covered by Tasks 4 and 7 against a fake server.
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
        let filter = PathFilter::new(
            Arc::from(vec![dir.path().to_path_buf()]),
            Arc::new(HashMap::from([("rs".to_string(), "rust".to_string())])),
            None,
        );
        let sweeper = Arc::new(Sweeper::new(
            Arc::new(Translator::new()),
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

        assert_eq!(sweeper.sweeps_run(), 0, "the burst has not settled");

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
        let sweeper = test_sweeper(Duration::from_millis(10));
        let path = sweeper.write("saved.rs");

        // The host sends unlink and then add for a temp-file-and-rename save,
        // and the hook process arrives after both. Reproduce that: the file is
        // gone when the unlink is queued and back by the time the sweep stats
        // it, which is the whole point of stating rather than trusting the
        // event kind.
        std::fs::remove_file(&path).expect("unlink");
        sweeper.enqueue(std::slice::from_ref(&path));
        std::fs::write(&path, "").expect("the rename puts it back");

        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.last_kinds(),
            vec![(path, SweepKind::Changed)],
            "acting on the host's unlink would close a live document and tell \
             every watching server the file was deleted"
        );
    }

    #[tokio::test]
    async fn test_the_sweep_stops_short_of_the_document_ceiling() {
        let sweeper = test_sweeper_with_ceiling(Duration::from_millis(10), 3);
        let paths: Vec<PathBuf> = (0..10)
            .map(|i| sweeper.write(&format!("f{i}.rs")))
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
