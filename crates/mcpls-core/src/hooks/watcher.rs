//! One filesystem watcher per project, feeding the sweeper.
//!
//! mcpls tells every language server that it will watch the disk on the
//! server's behalf: `initialize` advertises `didChangeWatchedFiles` dynamic
//! registration (`crate::lsp::lifecycle`). Servers take that at its word
//! and register globs -- rust-analyzer `**/*.rs` and `**/Cargo.toml`,
//! gopls `**/go.mod` -- which are about files they have not opened. This is
//! what makes that promise true on every host, rather than only where the
//! host offers a file-changed hook and only for the directories that
//! existed when the session started.
//!
//! Everything downstream already exists. The watcher places watches and
//! hands paths to [`Sweeper::enqueue_from`]; the filtering, the debounce,
//! the resync and the notification belong to the sweeper and the
//! translator, unchanged.
//!
//! Nothing here blocks a caller or a tokio worker. The mount probe, the
//! `ignore` walks and every `is_dir` stat run on
//! [`tokio::task::spawn_blocking`], because [`ProjectWatcher::start`] is
//! called from `Runtime::start`, which the MCP `initialize` handshake waits
//! on, and because the event loop must keep draining while a walk runs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, watch};

use crate::bridge::lock_std;
use crate::hooks::filters::{WatchSet, watch_set};
use crate::hooks::sweep::{Origin, Sweeper};

/// Filesystems inotify does not report changes on.
///
/// `9p`, `drvfs` and `virtiofs` are how WSL2 and some VM setups present a
/// host directory; the network filesystems deliver local events only. A
/// path prefix such as `/mnt/c` would be a guess about one vendor's
/// layout. The mount type is the fact.
const UNWATCHABLE_FILESYSTEMS: &[&str] =
    &["9p", "drvfs", "virtiofs", "cifs", "smb3", "nfs", "nfs4"];

/// Reported when a blocking step of the startup walk never came back.
///
/// In practice that is the runtime shutting down under the watcher, where
/// nobody is left to read the answer; the reason exists for the case that
/// is not, a panic somewhere inside the walk, which would otherwise leave
/// the doctor reporting a state that says to look again in a moment.
const UNFINISHED: &str = "the walk of this checkout did not finish";

/// How many unwalkable paths a doctor line names before it just counts
/// them. A permissions failure over a large subtree reports one error per
/// entry, and this line is read at a terminal.
const NAMED_WALK_ERRORS: usize = 3;

/// What the backend is doing about filesystem changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchState {
    /// The watcher is still placing its watches, so nothing is covered
    /// yet. [`ProjectWatcher::start`] returns in this state and leaves it
    /// once the startup walk and every watch are done.
    Starting,
    /// A watcher is running over this many directories.
    Watching {
        /// How many directories carry a watch.
        directories: usize,
        /// What the walk could not reach, when coverage is incomplete.
        ///
        /// A subtree that cannot be traversed -- a permissions failure, a
        /// broken mount -- is silently unwatched, and a watcher reporting
        /// only a directory count would call that coverage. Present means
        /// the count is not the whole checkout, and says why.
        incomplete: Option<String>,
    },
    /// No watcher runs, and why.
    Unwatched {
        /// What stopped it, in words a doctor line can print.
        reason: String,
    },
}

/// The project's filesystem watcher and the state a doctor reports.
pub struct ProjectWatcher {
    state: StdMutex<WatchState>,
}

impl ProjectWatcher {
    /// Start watching `roots`, or record why it could not be done.
    ///
    /// Returns as soon as the placement task is spawned, reporting
    /// [`WatchState::Starting`] until that task has walked the roots and
    /// placed every watch. `Runtime::start` is on the path the MCP
    /// `initialize` handshake waits on, and the walk of a large checkout
    /// is not something to make that handshake wait for.
    ///
    /// Never fails: a checkout that cannot be watched is a reported state
    /// rather than an error, because the hooks still deliver the agent's
    /// own edits and a session that degrades to the coverage it had before
    /// this existed is worth more than one that refuses to start.
    #[must_use]
    pub fn start(
        roots: Arc<[PathBuf]>,
        sweeper: &Arc<Sweeper>,
        cancel: watch::Receiver<bool>,
    ) -> Arc<Self> {
        let watcher = Arc::new(Self {
            state: StdMutex::new(WatchState::Starting),
        });
        tokio::spawn(place(
            Arc::clone(&watcher),
            roots,
            Arc::clone(sweeper),
            cancel,
        ));
        watcher
    }

    /// What this watcher is doing, for the doctor.
    #[must_use]
    pub fn state(&self) -> WatchState {
        lock_std(&self.state).clone()
    }

    /// Record that no watcher runs, and why.
    fn unwatched(&self, reason: String) {
        tracing::warn!(%reason, "not watching this checkout for changes");
        *lock_std(&self.state) = WatchState::Unwatched { reason };
    }
}

/// Where a watch is placed, so the rules around the kernel's watch limit
/// can be exercised without exhausting a real kernel's descriptors.
///
/// Narrower than [`notify::Watcher`], which also demands a constructor and
/// a configuration hook this module has no use for. Every watch here is
/// non-recursive: recursive mode places a descriptor on every descendant,
/// `target/` and `node_modules/` included, which is the exhaustion the walk
/// exists to avoid.
trait Place {
    /// Watch `path` itself, without descending into it.
    fn watch(&mut self, path: &Path) -> notify::Result<()>;
    /// Stop watching `path`.
    fn unwatch(&mut self, path: &Path) -> notify::Result<()>;
}

impl Place for RecommendedWatcher {
    fn watch(&mut self, path: &Path) -> notify::Result<()> {
        Watcher::watch(self, path, RecursiveMode::NonRecursive)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        Watcher::unwatch(self, path)
    }
}

/// Whether the watcher may carry on, or has run out of the kernel's watch
/// descriptors and must tear itself down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coverage {
    /// Every directory asked for carries a watch.
    Whole,
    /// The kernel refused a watch for want of descriptors.
    Exhausted,
}

/// How the paths `notify` reports are spelled, against how the roots are
/// configured.
///
/// macOS `FSEvents` resolves every symlink on the way to a watched
/// directory before it reports anything under it: a root configured as
/// `/var/checkout` arrives as `/private/var/checkout`, because `/var` is a
/// symlink. Everything downstream decides by prefix against the configured
/// roots -- `PathFilter::admits`, `admits_directory`, the set of placed
/// watches -- so an unrewritten path matches no root, is dropped as
/// outside the project, and the doctor reports a directory count for a
/// checkout nothing ever reaches the sweeper from.
struct Resolved {
    /// Resolved prefix and the configured root it stands for, for the
    /// roots the two differ on. Empty is the common case, and the one
    /// every platform but macOS is in.
    aliases: Vec<(PathBuf, PathBuf)>,
}

impl Resolved {
    /// Resolve `roots`, keeping the ones that came back spelled
    /// differently.
    ///
    /// Stats every root, so it belongs on the blocking pool. A root that
    /// cannot be resolved keeps its configured spelling, which is what an
    /// unwatchable root would have produced anyway.
    fn new(roots: &[PathBuf]) -> Self {
        let aliases = roots
            .iter()
            .filter_map(|root| {
                let resolved = dunce::canonicalize(root).ok()?;
                (resolved != *root).then(|| (resolved, root.clone()))
            })
            .collect();
        Self { aliases }
    }

    /// `paths` spelled the way the roots are configured.
    fn configured(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        paths
            .iter()
            .map(|path| {
                self.aliases
                    .iter()
                    .find_map(|(resolved, root)| {
                        path.strip_prefix(resolved).ok().map(|rest| root.join(rest))
                    })
                    .unwrap_or_else(|| path.clone())
            })
            .collect()
    }
}

/// The watches this project holds, and the state a doctor reads them from.
///
/// One owner of both, because every change to what is watched is also a
/// change to what the doctor should say: a watch added moves the count, a
/// directory that went moves it back, and a walk that could not finish
/// makes the count no longer the whole checkout.
struct Placement<P: Place> {
    native: P,
    placed: BTreeSet<PathBuf>,
    incomplete: Option<String>,
    watcher: Arc<ProjectWatcher>,
}

impl<P: Place> Placement<P> {
    const fn new(native: P, watcher: Arc<ProjectWatcher>, incomplete: Option<String>) -> Self {
        Self {
            native,
            placed: BTreeSet::new(),
            incomplete,
            watcher,
        }
    }

    /// Whether `directory` already carries a watch.
    fn holds(&self, directory: &Path) -> bool {
        self.placed.contains(directory)
    }

    /// Place a watch on `directory`, unless it already carries one.
    ///
    /// A failure that is not the watch limit is logged and skipped: one
    /// directory that went away between the walk and the watch is not a
    /// reason to stop covering the rest.
    fn watch(&mut self, directory: &Path) -> Coverage {
        if self.placed.contains(directory) {
            return Coverage::Whole;
        }
        match self.native.watch(directory) {
            Ok(()) => {
                self.placed.insert(directory.to_path_buf());
                Coverage::Whole
            }
            Err(error) if is_watch_limit(&error) => Coverage::Exhausted,
            Err(error) => {
                tracing::warn!(
                    %error,
                    directory = %directory.display(),
                    "could not watch a directory"
                );
                Coverage::Whole
            }
        }
    }

    /// Drop `directory` and everything under it from the watch set.
    ///
    /// Called when the watcher learns a directory is gone, so a later
    /// re-creation of the same path is adopted rather than mistaken for a
    /// directory already covered. Without this, `rm -rf src/foo && git
    /// checkout src/foo` -- or any branch switch that drops and re-adds a
    /// directory -- leaves every edit under it invisible for the rest of
    /// the session while the doctor still reports the checkout watched.
    ///
    /// Descendants go too. A `BTreeSet` of paths orders a directory
    /// immediately before everything under it, so they are one contiguous
    /// range, and the kernel has already dropped their descriptors.
    fn forget(&mut self, directory: &Path) {
        let under: Vec<PathBuf> = self
            .placed
            .range(directory.to_path_buf()..)
            .take_while(|held| held.starts_with(directory))
            .cloned()
            .collect();
        if under.is_empty() {
            return;
        }
        for path in under {
            let _ = self.native.unwatch(&path);
            self.placed.remove(&path);
        }
        self.publish();
    }

    /// Every watched directory the latest walk no longer finds.
    fn vanished(&self, found: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
        self.placed.difference(found).cloned().collect()
    }

    /// Record what the doctor reports as unreachable, when this walk
    /// could not reach something.
    ///
    /// A walk of one adopted subtree says nothing about the rest of the
    /// checkout, so a clean one does not clear a reason an earlier walk
    /// recorded; that is what the guard below is for. A dirty one names
    /// what it hit in place of the older reason rather than appending to
    /// it, so the reported reason is the most recent rather than the
    /// complete list. The flag itself, which is the part the doctor acts
    /// on, stays set either way.
    fn note_incomplete(&mut self, errors: &[String]) {
        if let Some(reason) = incomplete_reason(errors) {
            self.incomplete = Some(reason);
        }
    }

    /// Publish what is watched now.
    fn publish(&self) {
        *lock_std(&self.watcher.state) = WatchState::Watching {
            directories: self.placed.len(),
            incomplete: self.incomplete.clone(),
        };
    }

    /// Release every descriptor and report the checkout unwatched.
    ///
    /// A checkout watched in part is worse than one watched not at all:
    /// which part depends on walk order, so the behaviour is not
    /// reproducible and a bug report against it is not readable. The rule
    /// is the same after startup as at it, because a checkout that grows
    /// past `fs.inotify.max_user_watches` while a session runs is the same
    /// checkout as one that was already past it.
    fn tear_down(self) {
        let reason = format!(
            "the kernel's watch limit was reached after {} directories; \
             raise fs.inotify.max_user_watches to watch this checkout",
            self.placed.len()
        );
        // Dropping the watcher releases every descriptor it took.
        drop(self.native);
        self.watcher.unwatched(reason);
    }
}

/// Walk `roots`, place a watch on every directory they keep, and drain
/// events until cancelled.
///
/// The whole of this runs off [`ProjectWatcher::start`]'s caller, which is
/// `Runtime::start`.
///
/// Every path out of here settles the state. Returning while it still
/// reads [`WatchState::Starting`] would wedge the doctor on a line that
/// invites the reader to look again in a moment, forever.
async fn place(
    watcher: Arc<ProjectWatcher>,
    roots: Arc<[PathBuf]>,
    sweeper: Arc<Sweeper>,
    cancel: watch::Receiver<bool>,
) {
    let probed = Arc::clone(&roots);
    let Ok(unwatchable) = tokio::task::spawn_blocking(move || unwatchable_reason(&probed)).await
    else {
        watcher.unwatched(UNFINISHED.to_string());
        return;
    };
    if let Some(reason) = unwatchable {
        watcher.unwatched(reason);
        return;
    }

    let named = Arc::clone(&roots);
    let Ok(resolved) = tokio::task::spawn_blocking(move || Resolved::new(&named)).await else {
        watcher.unwatched(UNFINISHED.to_string());
        return;
    };

    let Some(set) = walk(&sweeper, Arc::clone(&roots)).await else {
        watcher.unwatched(UNFINISHED.to_string());
        return;
    };
    let incomplete = incomplete_reason(&set.errors);

    // `notify` calls this handler on a thread of its own, and adding a
    // watch for a newly created directory needs `&mut Watcher`, which
    // the handler cannot take while it is running. So it forwards and
    // nothing else, and the loop below owns the watcher.
    let (tx, rx) = mpsc::unbounded_channel();
    let native = match notify::recommended_watcher(move |result| {
        let _ = tx.send(result);
    }) {
        Ok(native) => native,
        Err(error) => {
            watcher.unwatched(format!("could not create a filesystem watcher: {error}"));
            return;
        }
    };

    let mut placement = Placement::new(native, watcher, incomplete);
    for directory in &set.directories {
        if placement.watch(directory) == Coverage::Exhausted {
            placement.tear_down();
            return;
        }
    }
    placement.publish();
    run(placement, rx, cancel, roots, sweeper, resolved).await;
}

/// Walk `roots` off the worker, logging what could not be reached.
///
/// `None` means the blocking pool is gone, which only happens as the
/// runtime shuts down.
async fn walk(sweeper: &Arc<Sweeper>, roots: Arc<[PathBuf]>) -> Option<WatchSet> {
    let filter = sweeper.filter();
    let set = tokio::task::spawn_blocking(move || watch_set(&filter, &roots))
        .await
        .ok()?;
    for error in &set.errors {
        tracing::warn!(%error, "could not fully walk the project for watch paths");
    }
    Some(set)
}

/// What the walk could not reach, for the doctor, or `None` when it reached
/// everything.
fn incomplete_reason(errors: &[String]) -> Option<String> {
    if errors.is_empty() {
        return None;
    }
    let named = errors
        .iter()
        .take(NAMED_WALK_ERRORS)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    Some(if errors.len() > NAMED_WALK_ERRORS {
        format!(
            "{} path(s) could not be walked, including: {named}",
            errors.len()
        )
    } else {
        format!("{} path(s) could not be walked: {named}", errors.len())
    })
}

/// Drain events until cancelled, placing watches on directories that appear,
/// dropping the ones that go, and handing every path to the sweeper.
async fn run<P: Place>(
    mut placement: Placement<P>,
    mut rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
    mut cancel: watch::Receiver<bool>,
    roots: Arc<[PathBuf]>,
    sweeper: Arc<Sweeper>,
    resolved: Resolved,
) {
    loop {
        tokio::select! {
            received = rx.recv() => {
                let Some(result) = received else { break };
                match result {
                    Ok(event) if event.need_rescan() => {
                        // Not a list of changed paths. The queue
                        // overflowed, so what was lost is unknown and the
                        // walk is the only honest answer to what may have
                        // changed.
                        if rescan(&mut placement, &roots, &sweeper).await == Coverage::Exhausted {
                            placement.tear_down();
                            return;
                        }
                    }
                    Ok(event) if is_read(event.kind) => {}
                    Ok(event) => {
                        let paths = resolved.configured(&event.paths);
                        let Some(directories) = directories_among(&paths).await else {
                            break;
                        };
                        for path in &paths {
                            if directories.contains(path) {
                                if adopt(&mut placement, &sweeper, path).await
                                    == Coverage::Exhausted
                                {
                                    placement.tear_down();
                                    return;
                                }
                            } else {
                                // Not a directory any more, if it ever was.
                                // A watched directory that has gone must
                                // leave the set, or the path it stood at is
                                // never adopted again when it comes back.
                                placement.forget(path);
                            }
                        }
                        sweeper.enqueue_from(&paths, Origin::Watcher);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "a filesystem watch reported an error");
                    }
                }
            }
            result = cancel.changed() => {
                // Err means the sender was dropped; treat as cancellation.
                if result.is_err() || *cancel.borrow() {
                    break;
                }
            }
        }
    }
    // Dropping the watcher here releases every descriptor with it, so no
    // watch outlives the backend.
    drop(placement);
}

/// Whether an event says only that something read the path.
///
/// inotify reports opens and reads alongside writes, and mcpls reads what
/// it sweeps, so answering them is a sweep that causes the next sweep,
/// with nothing to stop it. A write arrives as `Modify`, `Create` or
/// `Remove` on every platform, so dropping the reads loses no change --
/// including the `Close(Write)` ending a save whose `Modify` already came.
const fn is_read(kind: notify::EventKind) -> bool {
    matches!(kind, notify::EventKind::Access(_))
}

/// Which of `paths` are directories right now.
///
/// One `spawn_blocking` for the whole event rather than a stat apiece on
/// the worker: an event carries a handful of paths and each stat is a
/// filesystem call that must not run on the thread draining the channel.
/// `None` means the blocking pool is gone.
async fn directories_among(paths: &[PathBuf]) -> Option<BTreeSet<PathBuf>> {
    let owned = paths.to_vec();
    tokio::task::spawn_blocking(move || {
        owned
            .into_iter()
            .filter(|path| path.is_dir())
            .collect::<BTreeSet<PathBuf>>()
    })
    .await
    .ok()
}

/// Watch a directory that appeared after startup, and sweep what is already
/// inside it.
///
/// A `git checkout` can populate a subtree faster than the watch on its
/// parent can be placed, so the files already there would otherwise never
/// produce an event at all.
async fn adopt<P: Place>(
    placement: &mut Placement<P>,
    sweeper: &Arc<Sweeper>,
    directory: &Path,
) -> Coverage {
    if placement.holds(directory) || !sweeper.filter().admits_directory(directory) {
        return Coverage::Whole;
    }
    let target: Arc<[PathBuf]> = Arc::from(vec![directory.to_path_buf()]);
    let Some(set) = walk(sweeper, target).await else {
        return Coverage::Whole;
    };
    placement.note_incomplete(&set.errors);
    for new in &set.directories {
        if placement.watch(new) == Coverage::Exhausted {
            return Coverage::Exhausted;
        }
    }
    placement.publish();
    sweeper.enqueue_from(&set.files, Origin::Watcher);
    Coverage::Whole
}

/// Re-walk every root, place watches on directories that appeared, drop the
/// ones that went, and enqueue the files that have changed since the last
/// sweep.
///
/// Not every file the walk finds. The paths on an overflowing event cannot
/// be trusted, but the whole checkout is not a list of changes either:
/// naming every admitted file in one `didChangeWatchedFiles` hands every
/// language server a notification about the entire project, which for
/// rust-analyzer is a full reload -- and an overflow happens exactly when
/// the tree is already churning. So the walk decides what is watched and
/// the mtimes decide what is swept: a file modified at or after the last
/// completed sweep may have had its event dropped, and one older than that
/// was already on disk in its current form when that sweep read it. A file
/// whose mtime cannot be read is kept, and with no sweep yet to measure
/// against the whole set goes, because this rule may narrow the sweep but
/// not by guessing.
async fn rescan<P: Place>(
    placement: &mut Placement<P>,
    roots: &Arc<[PathBuf]>,
    sweeper: &Arc<Sweeper>,
) -> Coverage {
    let Some(set) = walk(sweeper, Arc::clone(roots)).await else {
        return Coverage::Whole;
    };
    placement.incomplete = incomplete_reason(&set.errors);
    for directory in placement.vanished(&set.directories) {
        placement.forget(&directory);
    }
    for new in &set.directories {
        if placement.watch(new) == Coverage::Exhausted {
            return Coverage::Exhausted;
        }
    }
    placement.publish();

    let cutoff = sweeper.last_sweep_at();
    let files = set.files;
    let Ok(changed) = tokio::task::spawn_blocking(move || changed_since(files, cutoff)).await
    else {
        return Coverage::Whole;
    };
    sweeper.enqueue_from(&changed, Origin::Watcher);
    Coverage::Whole
}

/// The files of `files` modified at or after `cutoff`, or all of them when
/// there is no cutoff to measure against.
///
/// Stats each file, so it belongs on the blocking pool.
fn changed_since(files: Vec<PathBuf>, cutoff: Option<SystemTime>) -> Vec<PathBuf> {
    let Some(cutoff) = cutoff else {
        return files;
    };
    files
        .into_iter()
        .filter(|path| {
            // A file whose mtime the platform will not give up is swept:
            // this rule narrows, and never on a guess.
            std::fs::metadata(path)
                .and_then(|data| data.modified())
                .map_or(true, |modified| modified >= cutoff)
        })
        .collect()
}

/// Why these roots cannot be watched, or `None` when they can.
///
/// Linux only, because the failure it detects is inotify's. A reading
/// failure means no reason, so a kernel that does not offer `/proc/mounts`
/// gets a watcher rather than a refusal.
fn unwatchable_reason(roots: &[PathBuf]) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    roots
        .iter()
        .find_map(|root| reason_for_mounts(&mounts, root))
}

/// [`unwatchable_reason`] for one root against a `/proc/mounts` body, so
/// the rule can be tested without one.
fn reason_for_mounts(mounts: &str, root: &Path) -> Option<String> {
    let (point, fstype) = mount_type_for(mounts, root)?;
    UNWATCHABLE_FILESYSTEMS.contains(&fstype.as_str()).then(|| {
        format!(
            "{} is on a {fstype} mount at {point}, where inotify reports no changes",
            root.display()
        )
    })
}

/// The mount point holding `path` and its filesystem type.
///
/// The longest matching mount point, not the first: `/` is a prefix of
/// every path, so a first match would answer with the root filesystem for
/// every checkout on the machine. Mount points are written with octal
/// escapes for the characters that would otherwise break the field
/// separation, so they are decoded before comparison.
fn mount_type_for(mounts: &str, path: &Path) -> Option<(String, String)> {
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _device = fields.next()?;
            let point = decode_octal_escapes(fields.next()?);
            let fstype = fields.next()?.to_string();
            path.starts_with(&point).then_some((point, fstype))
        })
        .max_by_key(|(point, _)| point.len())
}

/// Decode the `\040`-style escapes the kernel writes into `/proc/mounts`
/// for space, tab, newline and backslash.
fn decode_octal_escapes(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let escape = rest
            .get(at + 1..at + 4)
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        if let Some(byte) = escape {
            out.push(char::from(byte));
            rest = &rest[at + 4..];
        } else {
            out.push('\\');
            rest = &rest[at + 1..];
        }
    }
    out.push_str(rest);
    out
}

/// Whether a watch failed because the kernel has no descriptors left.
///
/// `ENOSPC` from an inotify watch means the per-user watch limit, not a
/// full disk. Unix only: this is the errno inotify raises, and
/// `ReadDirectoryChangesW` has no equivalent to confuse it with.
#[cfg(unix)]
fn is_watch_limit(error: &notify::Error) -> bool {
    let notify::ErrorKind::Io(io) = &error.kind else {
        return false;
    };
    io.raw_os_error() == Some(rustix::io::Errno::NOSPC.raw_os_error())
}

#[cfg(not(unix))]
const fn is_watch_limit(_error: &notify::Error) -> bool {
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use notify::EventKind;
    #[cfg(unix)]
    use notify::event::Flag;
    use notify::event::{AccessKind, AccessMode};
    use tempfile::TempDir;

    use super::*;
    use crate::bridge::Translator;
    use crate::hooks::filters::PathFilter;

    /// How long a test waits for a write to cross inotify, the channel and
    /// the sweeper. Generous, because delivery is not synchronous with the
    /// write that caused it, and a flaky watcher test is worth less than a
    /// slow one.
    const SETTLE: Duration = Duration::from_secs(5);

    /// A sweeper over `roots`, with no `run` loop spawned and a long quiet
    /// period: these tests assert on what the watcher handed the sweeper,
    /// not on what a sweep did with it, and a sweep firing mid-test would
    /// empty the pending set out from under the assertion.
    fn sweeper_over(roots: &[PathBuf]) -> Arc<Sweeper> {
        let roots: Arc<[PathBuf]> = Arc::from(roots.to_vec());
        let filter = PathFilter::new(
            Arc::clone(&roots),
            Arc::new(HashMap::from([("rs".to_string(), "rust".to_string())])),
            None,
        );
        Arc::new(Sweeper::new(
            Arc::new(Translator::new()),
            filter,
            Duration::from_secs(60),
            usize::MAX,
        ))
    }

    /// A watcher running over a temporary workspace, with the sweeper it
    /// feeds and the sender whose drop stops both.
    struct Watching {
        dir: TempDir,
        sweeper: Arc<Sweeper>,
        watcher: Arc<ProjectWatcher>,
        _cancel: watch::Sender<bool>,
    }

    /// A watched temporary workspace holding one `src/` directory, with
    /// its watches already placed.
    ///
    /// Waits for placement, because `start` returns before the walk runs
    /// and a file written into an unwatched directory produces no event at
    /// all.
    async fn watching() -> Watching {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        let root = dir.path().to_path_buf();
        watching_over(dir, root).await
    }

    /// [`watching`] over a `root` that is somewhere inside `dir` rather
    /// than `dir` itself.
    async fn watching_over(dir: TempDir, root: PathBuf) -> Watching {
        let roots: Arc<[PathBuf]> = Arc::from(vec![root]);
        let sweeper = sweeper_over(&roots);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let watcher = ProjectWatcher::start(roots, &sweeper, cancel_rx);
        let fixture = Watching {
            dir,
            sweeper,
            watcher,
            _cancel: cancel_tx,
        };
        wait_until(|| matches!(fixture.watcher.state(), WatchState::Watching { .. })).await;
        fixture
    }

    /// Poll `ready` until it holds, or fail this test at [`SETTLE`].
    async fn wait_until(ready: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + SETTLE;
        while tokio::time::Instant::now() < deadline {
            if ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the condition did not hold within {SETTLE:?}");
    }

    /// Whether `path` reaches the sweeper's pending set before [`SETTLE`].
    async fn reaches_the_sweeper(fixture: &Watching, path: &Path) -> bool {
        let deadline = tokio::time::Instant::now() + SETTLE;
        while tokio::time::Instant::now() < deadline {
            if fixture.sweeper.pending_paths().contains(path) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    /// How many directories a watcher reports, or `None` while it is not
    /// watching.
    fn directories(watcher: &ProjectWatcher) -> Option<usize> {
        match watcher.state() {
            WatchState::Watching { directories, .. } => Some(directories),
            _ => None,
        }
    }

    /// A kernel that records what it was asked to watch and can run out of
    /// descriptors, so the rules around the watch limit are testable
    /// without exhausting a real kernel's.
    struct FakePlace {
        watched: BTreeSet<PathBuf>,
        /// How many more watches this kernel has descriptors for; `None`
        /// for as many as asked.
        budget: Option<usize>,
    }

    /// The errno the kernel gives when it has no watch descriptors left,
    /// spelled the same way [`is_watch_limit`] recognises it, so the fake
    /// exercises that function rather than agreeing with it by
    /// construction.
    ///
    /// `rustix` is a unix-only dependency and Windows has no watch limit
    /// for `is_watch_limit` to find, so there the fake refuses with a
    /// plain error and the tests that drive it out of descriptors do not
    /// run at all.
    #[cfg(unix)]
    fn exhausted() -> std::io::Error {
        std::io::Error::from_raw_os_error(rustix::io::Errno::NOSPC.raw_os_error())
    }

    #[cfg(not(unix))]
    fn exhausted() -> std::io::Error {
        std::io::Error::other("out of watch descriptors")
    }

    impl Place for FakePlace {
        fn watch(&mut self, path: &Path) -> notify::Result<()> {
            match &mut self.budget {
                Some(0) => {
                    return Err(notify::Error::io(exhausted()));
                }
                Some(left) => *left -= 1,
                None => {}
            }
            self.watched.insert(path.to_path_buf());
            Ok(())
        }

        fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
            self.watched.remove(path);
            Ok(())
        }
    }

    /// A placement over a fake kernel with `budget` watches left.
    fn fake_placement(budget: Option<usize>) -> Placement<FakePlace> {
        let watcher = Arc::new(ProjectWatcher {
            state: StdMutex::new(WatchState::Starting),
        });
        let placement = Placement::new(
            FakePlace {
                watched: BTreeSet::new(),
                budget,
            },
            watcher,
            None,
        );
        placement.publish();
        placement
    }

    /// A `run` loop over a fake kernel, with the channel that feeds it the
    /// events a real `notify` would send.
    ///
    /// Driving `run` directly rather than a real watcher keeps these tests
    /// off the kernel's own delivery schedule: the event under test is the
    /// one sent, and nothing else arrives to race it.
    struct Driven {
        events: mpsc::UnboundedSender<notify::Result<Event>>,
        watcher: Arc<ProjectWatcher>,
        _cancel: watch::Sender<bool>,
    }

    fn drive(sweeper: &Arc<Sweeper>, roots: &Arc<[PathBuf]>, budget: Option<usize>) -> Driven {
        let placement = fake_placement(budget);
        let watcher = Arc::clone(&placement.watcher);
        let (events, rx) = mpsc::unbounded_channel();
        let (cancel, cancel_rx) = watch::channel(false);
        tokio::spawn(run(
            placement,
            rx,
            cancel_rx,
            Arc::clone(roots),
            Arc::clone(sweeper),
            Resolved::new(roots),
        ));
        Driven {
            events,
            watcher,
            _cancel: cancel,
        }
    }

    impl Driven {
        /// Report `path` the way `notify` reports a change to it.
        fn report(&self, path: &Path) {
            self.events
                .send(Ok(Event::new(EventKind::Any).add_path(path.to_path_buf())))
                .expect("the run loop to still be draining");
        }

        /// Report `path` the way `notify` reports something opening it
        /// for reading.
        fn report_read(&self, path: &Path) {
            let kind = EventKind::Access(AccessKind::Open(AccessMode::Any));
            self.events
                .send(Ok(Event::new(kind).add_path(path.to_path_buf())))
                .expect("the run loop to still be draining");
        }

        /// Report the queue overflowed, which is not a list of paths.
        ///
        /// Only the watch-limit tests drive this, and the watch limit is
        /// a unix notion, so on Windows this helper has no caller and an
        /// ungated one would be dead code in a workspace that denies
        /// warnings.
        #[cfg(unix)]
        fn report_overflow(&self) {
            self.events
                .send(Ok(Event::new(EventKind::Any).set_flag(Flag::Rescan)))
                .expect("the run loop to still be draining");
        }

        /// Why this watcher stopped, or `None` while it is still running.
        ///
        /// Gated for the same reason as [`Driven::report_overflow`].
        #[cfg(unix)]
        fn unwatched_reason(&self) -> Option<String> {
            match self.watcher.state() {
                WatchState::Unwatched { reason } => Some(reason),
                _ => None,
            }
        }
    }

    #[tokio::test]
    async fn test_an_external_write_reaches_the_sweeper() {
        let fixture = watching().await;
        assert!(
            matches!(fixture.watcher.state(), WatchState::Watching { .. }),
            "a plain temporary directory must be watchable, or every other \
             assertion in this module is vacuous"
        );

        let path = fixture.dir.path().join("src/created.rs");
        std::fs::write(&path, "fn main() {}").expect("write");

        assert!(
            reaches_the_sweeper(&fixture, &path).await,
            "a file written by a shell command, with no tool call touching \
             it, is the case this whole design exists for"
        );
    }

    #[tokio::test]
    async fn test_a_directory_created_after_startup_is_watched() {
        let fixture = watching().await;

        let created = fixture.dir.path().join("late");
        std::fs::create_dir(&created).expect("mkdir");
        let path = created.join("arrived.rs");
        std::fs::write(&path, "fn main() {}").expect("write");

        assert!(
            reaches_the_sweeper(&fixture, &path).await,
            "watchPaths took one snapshot of the top level at SessionStart and \
             was blind to everything created afterwards, which is the gap this \
             watcher exists to close"
        );
    }

    /// macOS `FSEvents` reports every path with the symlinks on the way to
    /// it already resolved, while the sweeper admits a path by prefix
    /// against the root as it was configured. Without the two being
    /// reconciled, a checkout reached through a symlink -- which on macOS
    /// is every path under `/tmp` and `/var` -- has every one of its
    /// events dropped as outside the project, and the doctor still counts
    /// the directories it is watching in name only.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_root_reached_through_a_symlink_still_reaches_the_sweeper() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("src")).expect("mkdir");
        let root = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &root).expect("symlink");
        let path = root.join("src/created.rs");

        let fixture = watching_over(dir, root).await;
        std::fs::write(&path, "fn main() {}").expect("write");

        assert!(
            reaches_the_sweeper(&fixture, &path).await,
            "a root configured through a symlink is watched like any other, \
             so what the watcher hands the sweeper has to be spelled the way \
             the root was configured"
        );
    }

    /// The negative half of this test would pass on its own with no
    /// watcher running at all, so it is paired with a source file written
    /// in the same run: the artifact must be absent while its sibling is
    /// present, which is only true of a watcher that is working and
    /// filtering rather than one that is switched off.
    #[tokio::test]
    async fn test_a_build_artifact_never_reaches_the_sweeper() {
        let fixture = watching().await;
        let target = fixture.dir.path().join("target/debug");
        std::fs::create_dir_all(&target).expect("mkdir");
        let artifact = target.join("build.rs");
        std::fs::write(&artifact, "fn main() {}").expect("write");
        let source = fixture.dir.path().join("src/real.rs");
        std::fs::write(&source, "fn main() {}").expect("write");

        assert!(
            reaches_the_sweeper(&fixture, &source).await,
            "the control: without this the assertion below passes whenever \
             the watcher is doing nothing at all"
        );
        assert!(
            !fixture.sweeper.pending_paths().contains(&artifact),
            "no watch is placed under target/, and the sweeper's filter drops \
             anything from it that arrives by another route"
        );
    }

    /// `Runtime::start` is on the path the MCP `initialize` handshake
    /// waits on, and this runtime is single-threaded: the placement task
    /// cannot have run by the time `start` returns, so a state that is
    /// already `Watching` here means the walk and every `inotify_add_watch`
    /// ran on the caller's thread.
    #[tokio::test]
    async fn test_start_returns_before_it_places_any_watch() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let roots: Arc<[PathBuf]> = Arc::from(vec![dir.path().to_path_buf()]);
        let sweeper = sweeper_over(&roots);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let watcher = ProjectWatcher::start(roots, &sweeper, cancel_rx);

        assert_eq!(
            watcher.state(),
            WatchState::Starting,
            "the startup walk and its watches must not run on the thread \
             Runtime::start is holding, which the initialize handshake is \
             waiting on"
        );
        wait_until(|| matches!(watcher.state(), WatchState::Watching { .. })).await;
    }

    /// `rm -rf src/foo && git checkout src/foo`, and every branch switch
    /// that drops and re-adds a directory. Nothing pruned a path from the
    /// watch set when its directory went, so `adopt` saw the recreated
    /// directory as one already covered, placed no watch, and every later
    /// edit under it was invisible for the rest of the session while the
    /// doctor still reported the checkout watched.
    #[tokio::test]
    async fn test_a_directory_that_went_is_adopted_again_when_it_comes_back() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let roots: Arc<[PathBuf]> = Arc::from(vec![dir.path().to_path_buf()]);
        let sweeper = sweeper_over(&roots);
        let driven = drive(&sweeper, &roots, None);
        let late = dir.path().join("late");

        std::fs::create_dir(&late).expect("mkdir");
        driven.report(&late);
        wait_until(|| directories(&driven.watcher) == Some(1)).await;

        std::fs::remove_dir_all(&late).expect("rm -rf");
        driven.report(&late);
        wait_until(|| directories(&driven.watcher) == Some(0)).await;

        std::fs::create_dir(&late).expect("mkdir");
        let path = late.join("arrived.rs");
        std::fs::write(&path, "fn main() {}").expect("write");
        driven.report(&late);

        wait_until(|| sweeper.pending_paths().contains(&path)).await;
    }

    /// Reads are not changes. inotify reports every open, and a sweep
    /// opens the untracked files it finds, so a sweeper that took its own
    /// reads for edits would keep sweeping a checkout nobody is editing
    /// and keep telling the servers a file changed.
    #[tokio::test]
    async fn test_a_read_of_a_file_is_not_swept() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let roots: Arc<[PathBuf]> = Arc::from(vec![dir.path().to_path_buf()]);
        let sweeper = sweeper_over(&roots);
        let driven = drive(&sweeper, &roots, None);
        let read = dir.path().join("read.rs");
        let written = dir.path().join("written.rs");
        std::fs::write(&read, "fn main() {}").expect("write");
        std::fs::write(&written, "fn main() {}").expect("write");

        driven.report_read(&read);
        // The loop takes events in order, so the write landing proves the
        // read ahead of it has already been handled.
        driven.report(&written);
        wait_until(|| sweeper.pending_paths().contains(&written)).await;

        assert!(
            !sweeper.pending_paths().contains(&read),
            "a file mcpls only read must not come back as a change"
        );
    }

    /// The watch limit is the same failure after startup as at it: a
    /// checkout that grows past `fs.inotify.max_user_watches` while a
    /// session runs would otherwise end up partly watched, with no
    /// teardown, and a doctor line reporting a directory count for a
    /// watcher covering an arbitrary, walk-order-dependent fraction of the
    /// tree.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_the_watch_limit_after_startup_tears_the_watcher_down() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let roots: Arc<[PathBuf]> = Arc::from(vec![dir.path().to_path_buf()]);
        let sweeper = sweeper_over(&roots);
        let driven = drive(&sweeper, &roots, Some(0));
        let late = dir.path().join("late");
        std::fs::create_dir(&late).expect("mkdir");

        driven.report(&late);

        wait_until(|| driven.unwatched_reason().is_some()).await;
        assert!(
            driven
                .unwatched_reason()
                .is_some_and(|reason| reason.contains("watch limit")),
            "the doctor has to name the limit, because raising \
             fs.inotify.max_user_watches is the only thing that fixes it"
        );
    }

    /// The same rule on the other post-startup walk. `rescan` discarded
    /// its watch errors entirely, so an overflow on a checkout past the
    /// limit left the watcher reporting whatever count the last successful
    /// walk had set.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_rescan_that_hits_the_watch_limit_tears_the_watcher_down() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir(dir.path().join("src")).expect("mkdir");
        let roots: Arc<[PathBuf]> = Arc::from(vec![dir.path().to_path_buf()]);
        let sweeper = sweeper_over(&roots);
        let driven = drive(&sweeper, &roots, Some(0));

        driven.report_overflow();

        wait_until(|| driven.unwatched_reason().is_some()).await;
    }

    /// A watch added after startup moves the count the doctor prints. It
    /// stayed frozen at whatever the startup walk placed, so a session
    /// that adopted half a monorepo still reported the number it started
    /// with.
    #[tokio::test]
    async fn test_a_watch_placed_after_startup_moves_the_reported_count() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let roots: Arc<[PathBuf]> = Arc::from(vec![dir.path().to_path_buf()]);
        let sweeper = sweeper_over(&roots);
        let driven = drive(&sweeper, &roots, None);
        assert_eq!(directories(&driven.watcher), Some(0));

        let late = dir.path().join("late/deeper");
        std::fs::create_dir_all(&late).expect("mkdir");
        driven.report(&dir.path().join("late"));

        wait_until(|| directories(&driven.watcher) == Some(2)).await;
    }

    /// A subtree the walk cannot traverse is silently unwatched, and the
    /// incomplete-scan warning that `session_start_output` used to surface
    /// went with `FileChanged`. A directory count on its own would call
    /// that coverage.
    #[tokio::test]
    async fn test_a_root_the_walk_cannot_reach_is_reported_as_incomplete() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let unreachable = dir.path().join("no-such-subtree");
        let roots: Arc<[PathBuf]> = Arc::from(vec![dir.path().to_path_buf(), unreachable.clone()]);
        let sweeper = sweeper_over(&roots);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let watcher = ProjectWatcher::start(roots, &sweeper, cancel_rx);

        wait_until(|| matches!(watcher.state(), WatchState::Watching { .. })).await;
        let WatchState::Watching { incomplete, .. } = watcher.state() else {
            panic!("a temporary directory must be watchable");
        };
        assert!(
            incomplete.is_some_and(|reason| reason.contains("could not be walked")),
            "the doctor has to say the count is not the whole checkout, or a \
             partly walked tree reads exactly like a fully walked one"
        );
    }

    /// An overflow is most likely exactly when the tree is churning.
    /// Enqueueing every admitted file the walk finds names the entire
    /// checkout in one `didChangeWatchedFiles` to every language server,
    /// which for rust-analyzer is a full reload.
    #[tokio::test]
    async fn test_a_rescan_sweeps_only_what_changed_since_the_last_sweep() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let roots: Arc<[PathBuf]> = Arc::from(vec![dir.path().to_path_buf()]);
        let sweeper = sweeper_over(&roots);
        let settled = dir.path().join("settled.rs");
        std::fs::write(&settled, "fn main() {}").expect("write");
        sweeper.enqueue(std::slice::from_ref(&settled));
        sweeper.sweep_now().await;

        // Both files are written within the same instant, and a
        // filesystem that stores mtimes to the second would then give
        // them the same one and put the settled file back in the sweep.
        // Dating it an hour back says what the test means -- a file that
        // has not moved since the sweep -- on any granularity, and drops
        // the sleep that stood in for it.
        std::fs::File::options()
            .write(true)
            .open(&settled)
            .expect("reopen")
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .expect("backdate");
        let churned = dir.path().join("churned.rs");
        std::fs::write(&churned, "fn main() {}").expect("write");
        // Windows dates a file from the interrupt-tick clock, which lags
        // the precise one `SystemTime::now` reads by up to its ~15ms
        // period, so a file written right after the sweep can carry an
        // mtime just before the cutoff. Stamping it says what the test
        // means on any clock.
        std::fs::File::options()
            .write(true)
            .open(&churned)
            .expect("reopen")
            .set_modified(SystemTime::now())
            .expect("stamp");
        let mut placement = fake_placement(None);
        rescan(&mut placement, &roots, &sweeper).await;

        let pending = sweeper.pending_paths();
        assert!(
            pending.contains(&churned),
            "an overflow drops events, so a file changed since the last sweep \
             has to be swept on the walk that answers it"
        );
        assert!(
            !pending.contains(&settled),
            "a file that has not moved since the last sweep was already \
             reported, and naming it again hands every server a notification \
             about the whole checkout"
        );
    }

    /// `/proc/mounts` as WSL2 writes it: the Linux filesystems first, the
    /// Windows drive mounted under `/mnt`, and a space in a mount point
    /// written as the octal escape `\040`.
    const WSL_MOUNTS: &str = "\
/dev/sdc / ext4 rw,relatime 0 0
drivers /usr/lib/wsl/drivers 9p ro,dirsync 0 0
C:\\134 /mnt/c drvfs rw,noatime 0 0
D:\\134 /mnt/my\\040drive drvfs rw,noatime 0 0
";

    #[test]
    fn test_the_longest_mount_point_wins() {
        let (point, fstype) =
            mount_type_for(WSL_MOUNTS, Path::new("/usr/lib/wsl/drivers/x")).expect("a mount");
        assert_eq!(point, "/usr/lib/wsl/drivers");
        assert_eq!(
            fstype, "9p",
            "/ is also a prefix of this path, and answering with ext4 would \
             call an unwatchable checkout watchable"
        );
    }

    #[test]
    fn test_an_octal_escape_in_a_mount_point_is_decoded() {
        let (point, fstype) =
            mount_type_for(WSL_MOUNTS, Path::new("/mnt/my drive/repo")).expect("a mount");
        assert_eq!(
            point, "/mnt/my drive",
            "the kernel writes a space as \\040, and a mount point compared \
             without decoding it matches nothing"
        );
        assert_eq!(fstype, "drvfs");
    }

    #[test]
    fn test_a_windows_drive_under_wsl_is_unwatchable() {
        assert!(
            reason_for_mounts(WSL_MOUNTS, Path::new("/mnt/c/work/repo")).is_some(),
            "inotify delivers no events at all for a Windows drive under \
             WSL2, so a watcher there would look like it worked and miss \
             every edit"
        );
        assert!(
            reason_for_mounts(WSL_MOUNTS, Path::new("/home/user/repo")).is_none(),
            "an ext4 checkout on the same machine is watchable and must not \
             be refused alongside it"
        );
    }
}
