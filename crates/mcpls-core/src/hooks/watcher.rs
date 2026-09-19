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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, watch};

use crate::bridge::lock_std;
use crate::hooks::filters::watch_set;
use crate::hooks::sweep::{Origin, Sweeper};

/// Filesystems inotify does not report changes on.
///
/// `9p`, `drvfs` and `virtiofs` are how WSL2 and some VM setups present a
/// host directory; the network filesystems deliver local events only. A
/// path prefix such as `/mnt/c` would be a guess about one vendor's
/// layout. The mount type is the fact.
const UNWATCHABLE_FILESYSTEMS: &[&str] =
    &["9p", "drvfs", "virtiofs", "cifs", "smb3", "nfs", "nfs4"];

/// What the backend is doing about filesystem changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchState {
    /// A watcher is running over this many directories.
    Watching {
        /// How many directories carry a watch.
        directories: usize,
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
            state: StdMutex::new(WatchState::Unwatched {
                reason: "starting".to_string(),
            }),
        });
        if let Some(reason) = unwatchable_reason(&roots) {
            tracing::warn!(%reason, "not watching this checkout for changes");
            *lock_std(&watcher.state) = WatchState::Unwatched { reason };
            return watcher;
        }
        let filter = sweeper.filter();
        let set = watch_set(&filter, &roots);
        for error in &set.errors {
            tracing::warn!(%error, "could not fully walk the project for watch paths");
        }

        // `notify` calls this handler on a thread of its own, and adding a
        // watch for a newly created directory needs `&mut Watcher`, which
        // the handler cannot take while it is running. So it forwards and
        // nothing else, and the task below owns the watcher.
        let (tx, rx) = mpsc::unbounded_channel();
        let mut native = match notify::recommended_watcher(move |result| {
            let _ = tx.send(result);
        }) {
            Ok(native) => native,
            Err(error) => {
                let reason = format!("could not create a filesystem watcher: {error}");
                tracing::warn!(%reason, "not watching this checkout for changes");
                *lock_std(&watcher.state) = WatchState::Unwatched { reason };
                return watcher;
            }
        };

        let mut placed = BTreeSet::new();
        for directory in &set.directories {
            match native.watch(directory, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    placed.insert(directory.clone());
                }
                Err(error) if is_watch_limit(&error) => {
                    let reason = format!(
                        "the kernel's watch limit was reached after {} directories; \
                         raise fs.inotify.max_user_watches to watch this checkout",
                        placed.len()
                    );
                    tracing::warn!(%reason, "not watching this checkout for changes");
                    // Dropping the watcher releases every descriptor it
                    // took. A checkout watched in part is worse than one
                    // watched not at all: which part depends on walk
                    // order, so the behaviour is not reproducible and a
                    // bug report against it is not readable.
                    drop(native);
                    *lock_std(&watcher.state) = WatchState::Unwatched { reason };
                    return watcher;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        directory = %directory.display(),
                        "could not watch a directory"
                    );
                }
            }
        }

        *lock_std(&watcher.state) = WatchState::Watching {
            directories: placed.len(),
        };
        tokio::spawn(run(native, placed, rx, cancel, roots, Arc::clone(sweeper)));
        watcher
    }

    /// What this watcher is doing, for the doctor.
    #[must_use]
    pub fn state(&self) -> WatchState {
        lock_std(&self.state).clone()
    }
}

/// Drain events until cancelled, placing watches on directories that appear
/// and handing every path to the sweeper.
async fn run(
    mut native: RecommendedWatcher,
    mut placed: BTreeSet<PathBuf>,
    mut rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
    mut cancel: watch::Receiver<bool>,
    roots: Arc<[PathBuf]>,
    sweeper: Arc<Sweeper>,
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
                        rescan(&mut native, &mut placed, &roots, &sweeper);
                    }
                    Ok(event) => {
                        for path in &event.paths {
                            if path.is_dir() {
                                adopt(&mut native, &mut placed, &sweeper, path);
                            }
                        }
                        sweeper.enqueue_from(&event.paths, Origin::Watcher);
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
    drop(native);
}

/// Watch a directory that appeared after startup, and sweep what is already
/// inside it.
///
/// A `git checkout` can populate a subtree faster than the watch on its
/// parent can be placed, so the files already there would otherwise never
/// produce an event at all.
fn adopt(
    native: &mut RecommendedWatcher,
    placed: &mut BTreeSet<PathBuf>,
    sweeper: &Arc<Sweeper>,
    directory: &Path,
) {
    let filter = sweeper.filter();
    if placed.contains(directory) || !filter.admits_directory(directory) {
        return;
    }
    let set = watch_set(&filter, &[directory.to_path_buf()]);
    for new in &set.directories {
        if placed.contains(new) {
            continue;
        }
        match native.watch(new, RecursiveMode::NonRecursive) {
            Ok(()) => {
                placed.insert(new.clone());
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    directory = %new.display(),
                    "could not watch a new directory"
                );
            }
        }
    }
    sweeper.enqueue_from(&set.files, Origin::Watcher);
}

/// Re-walk every root, place watches on directories that appeared, drop the
/// ones that went, and enqueue every admitted file.
fn rescan(
    native: &mut RecommendedWatcher,
    placed: &mut BTreeSet<PathBuf>,
    roots: &Arc<[PathBuf]>,
    sweeper: &Arc<Sweeper>,
) {
    let filter = sweeper.filter();
    let set = watch_set(&filter, roots);
    let gone: Vec<PathBuf> = placed.difference(&set.directories).cloned().collect();
    for directory in gone {
        let _ = native.unwatch(&directory);
        placed.remove(&directory);
    }
    for new in &set.directories {
        if placed.contains(new) {
            continue;
        }
        if native.watch(new, RecursiveMode::NonRecursive).is_ok() {
            placed.insert(new.clone());
        }
    }
    sweeper.enqueue_from(&set.files, Origin::Watcher);
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

    use tempfile::TempDir;

    use super::*;
    use crate::bridge::Translator;
    use crate::hooks::filters::PathFilter;

    /// How long a test waits for a write to cross inotify, the channel and
    /// the sweeper. Generous, because delivery is not synchronous with the
    /// write that caused it, and a flaky watcher test is worth less than a
    /// slow one.
    const SETTLE: Duration = Duration::from_secs(5);

    /// A watcher running over a temporary workspace, with the sweeper it
    /// feeds and the sender whose drop stops both.
    struct Watching {
        dir: TempDir,
        sweeper: Arc<Sweeper>,
        watcher: Arc<ProjectWatcher>,
        _cancel: watch::Sender<bool>,
    }

    /// A watched temporary workspace holding one `src/` directory.
    ///
    /// The sweeper's `run` loop is deliberately not spawned and the quiet
    /// period is long: these tests assert on what the watcher handed the
    /// sweeper, not on what a sweep did with it, and a sweep firing
    /// mid-test would empty the pending set out from under the assertion.
    fn watching() -> Watching {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        let roots: Arc<[PathBuf]> = Arc::from(vec![dir.path().to_path_buf()]);
        let filter = PathFilter::new(
            Arc::clone(&roots),
            Arc::new(HashMap::from([("rs".to_string(), "rust".to_string())])),
            None,
        );
        let sweeper = Arc::new(Sweeper::new(
            Arc::new(Translator::new()),
            filter,
            Duration::from_secs(60),
            usize::MAX,
        ));
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let watcher = ProjectWatcher::start(roots, &sweeper, cancel_rx);
        Watching {
            dir,
            sweeper,
            watcher,
            _cancel: cancel_tx,
        }
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

    #[tokio::test]
    async fn test_an_external_write_reaches_the_sweeper() {
        let fixture = watching();
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
        let fixture = watching();

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

    /// The negative half of this test would pass on its own with no
    /// watcher running at all, so it is paired with a source file written
    /// in the same run: the artifact must be absent while its sibling is
    /// present, which is only true of a watcher that is working and
    /// filtering rather than one that is switched off.
    #[tokio::test]
    async fn test_a_build_artifact_never_reaches_the_sweeper() {
        let fixture = watching();
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
