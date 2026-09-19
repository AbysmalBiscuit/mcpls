//! Deciding which changed paths are worth acting on.
//!
//! Claude Code's hooks report every path a tool touched, with no ignore
//! list of their own: a `cargo build` or an `npm install` can report
//! thousands of paths under `target/` or `node_modules/`, and the document
//! tracker has a ceiling. `PathFilter` is what keeps that flood out, and
//! `watch_set` is what keeps the backend's own watcher from ever placing a
//! descriptor inside one of those trees to begin with.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::lsp::WatchRegistry;

/// Directories that hold generated, vendored or repository-internal files,
/// dropped under every root even when a project's own `.gitignore` does not
/// name them.
///
/// These lines are added to each root's matcher before that root's own
/// `.gitignore`, so a project that genuinely keeps sources under one of
/// these names can re-admit a specific path with a `.gitignore` negation,
/// for example `!target/keep.rs`: everything else under `target/` stays
/// dropped, but that one path is not.
///
/// `.git` is here because no project writes it into its own `.gitignore`,
/// git having no need of that, and because the watcher's walk takes every
/// exclusion from this module: without the line it would place a watch on
/// every directory of the object store, which git rewrites on each commit,
/// fetch and index update.
const BUILT_IN_IGNORES: &[&str] = &[".git", "target", "node_modules"];

/// Decides which changed paths are worth waking a language server for.
pub struct PathFilter {
    roots: Arc<[PathBuf]>,
    ignores: Vec<Gitignore>,
    extensions: Arc<HashMap<String, String>>,
    registry: Option<Arc<WatchRegistry>>,
}

impl PathFilter {
    /// A filter over `roots`, admitting a path whose extension is in
    /// `extensions`, or that a server registered in `registry` asked to
    /// hear about, as long as the path lives under a root and is not
    /// ignored there.
    #[must_use]
    pub fn new(
        roots: Arc<[PathBuf]>,
        extensions: Arc<HashMap<String, String>>,
        registry: Option<Arc<WatchRegistry>>,
    ) -> Self {
        let ignores = roots.iter().map(|root| gitignore_for(root)).collect();
        Self {
            roots,
            ignores,
            extensions,
            registry,
        }
    }

    /// Whether `path` survives both filters.
    ///
    /// Decided entirely from `path` and the configured roots: nothing here
    /// touches the filesystem, so a path reported as a deletion -- nothing
    /// left to stat -- is admitted exactly like one that still exists. A
    /// delete is a change the sweep still has to act on.
    ///
    /// The registry is asked whether it watches `path` at all rather than
    /// whether it watches it under some particular change kind. No kind
    /// exists yet at this point: the host reports bare paths and the sweep
    /// decides what each one is afterwards, so a kind named here would be a
    /// guess, and a guess drops every server whose watcher wants the other
    /// kinds. Kind filtering belongs to the notification, which knows.
    ///
    /// When several configured roots contain `path` -- one nested inside
    /// another, which is how a monorepo with a vendored subproject is
    /// configured -- every one of them is asked, and the path is dropped as
    /// soon as any of them ignores it. A repository root writes `dist/` or
    /// `*.log` once at the top and means it everywhere below, which is what
    /// git does with the same file.
    ///
    /// The one rule that does not get to answer is an outer one excluding
    /// the deepest containing root itself: configuring that root is the
    /// statement that its contents are wanted, so a `vendor/` line above it
    /// cannot drop everything inside it. That outer root is then skipped
    /// entirely for this path, and the roots below it still answer for
    /// themselves.
    ///
    /// Being a fold over a set, the order the roots were configured in
    /// never enters into it.
    #[must_use]
    pub fn admits(&self, path: &Path) -> bool {
        if !self.within_unignored_root(path, false) {
            return false;
        }

        if self.routable_extension(path) {
            return true;
        }
        self.registry
            .as_ref()
            .is_some_and(|registry| registry.is_watched(path))
    }

    /// Whether `path` lives under a configured root and survives every
    /// containing root's ignore rules, with `is_dir` selecting how the
    /// matchers are asked about it.
    ///
    /// The shared body of [`Self::admits`] and [`Self::admits_directory`].
    /// One fold is what keeps the watch set and the admitted set from
    /// drifting apart: a directory refused here whose files `admits`
    /// accepts loses their events with nothing to show for it, and one
    /// accepted whose files `admits` rejects spends a watch descriptor on
    /// a subtree nobody will ever be told about.
    fn within_unignored_root(&self, path: &Path, is_dir: bool) -> bool {
        let containing: Vec<(&PathBuf, &Gitignore)> = self
            .roots
            .iter()
            .zip(&self.ignores)
            .filter(|(root, _)| path.starts_with(root))
            .collect();
        let Some(deepest) = containing
            .iter()
            .max_by_key(|(root, _)| root.components().count())
            .map(|(root, _)| *root)
        else {
            return false;
        };
        if path == deepest {
            // Configuring a root is the statement that its contents are
            // wanted, and asking a root's own matcher about its base path
            // is not a question `ignore` promises an answer to.
            return true;
        }
        let ignored = containing.iter().any(|(root, ignore)| {
            let excludes_the_deepest_root = *root != deepest
                && ignore
                    .matched_path_or_any_parents(deepest, true)
                    .is_ignore();
            !excludes_the_deepest_root
                && ignore.matched_path_or_any_parents(path, is_dir).is_ignore()
        });
        !ignored
    }

    /// Whether the watcher should place a watch on `dir` and descend into
    /// it.
    ///
    /// The same question [`Self::admits`] answers about a file, without
    /// the extension and registry checks, which only mean something for a
    /// file. A directory is never admitted for being routable; it is
    /// admitted for not being excluded.
    #[must_use]
    pub fn admits_directory(&self, dir: &Path) -> bool {
        self.within_unignored_root(dir, true)
    }

    /// Whether `path`'s extension is one this filter routes directly to a
    /// language server, independent of any watcher glob a server may have
    /// registered for it.
    ///
    /// Exposed so a caller deciding what to do with an already-admitted,
    /// untracked path -- open it as a document, or just notify whichever
    /// server asked to watch it -- can tell which of `admits`'s two reasons
    /// applied, without re-deriving the extension check itself.
    #[must_use]
    pub(crate) fn routable_extension(&self, path: &Path) -> bool {
        path.extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|ext| self.extensions.contains_key(ext))
    }
}

/// The path filter's ignore matcher, with failures logged for the server.
fn gitignore_for(root: &Path) -> Gitignore {
    let (ignore, errors) = read_gitignore(root);
    for error in errors {
        tracing::warn!(%error, "could not fully read the project's ignore rules");
    }
    ignore
}

fn read_gitignore(root: &Path) -> (Gitignore, Vec<String>) {
    let mut errors = Vec::new();
    let mut builder = GitignoreBuilder::new(root);
    for pattern in BUILT_IN_IGNORES {
        let _ = builder.add_line(None, pattern);
    }
    let gitignore = root.join(".gitignore");
    if let Some(error) = builder.add(&gitignore) {
        let missing = error
            .io_error()
            .is_some_and(|io_error| io_error.kind() == std::io::ErrorKind::NotFound);
        if !missing {
            errors.push(format!("{}: {error}", gitignore.display()));
        }
    }
    let ignore = builder.build().unwrap_or_else(|error| {
        errors.push(error.to_string());
        Gitignore::empty()
    });
    (ignore, errors)
}

/// Selected watch paths and failures encountered while inspecting the root.
///
/// Superseded by [`WatchSet`], and kept only until the Claude plugin stops
/// registering the `SessionStart` reply that consumes it.
#[derive(Debug)]
pub struct WatchPaths {
    /// Top-level entries admitted by the scan and ignore rules.
    pub paths: Vec<PathBuf>,
    /// Traversal or ignore-rule failures; selected paths may be incomplete.
    pub errors: Vec<String>,
}

/// Scan top-level entries using ignore rules; hidden entries are excluded by default.
///
/// Explicit allow rules can include hidden entries.
/// This does not verify that a host registered the paths or can watch their descendants.
///
/// Superseded by [`watch_set`], which walks the whole tree rather than one
/// level of it, and kept only until the `SessionStart` reply goes.
#[must_use]
pub fn watch_paths(root: &Path) -> WatchPaths {
    let (ignore, errors) = read_gitignore(root);
    let mut result = WatchPaths {
        paths: Vec::new(),
        errors,
    };
    if root.is_file() {
        result.errors.push(format!(
            "{}: project root is not a directory",
            root.display()
        ));
        return result;
    }
    for entry in WalkBuilder::new(root)
        .hidden(true)
        .max_depth(Some(1))
        .build()
    {
        match entry {
            Ok(entry) => {
                if let Some(error) = entry.error() {
                    result.errors.push(error.to_string());
                }
                if entry.path() == root {
                    continue;
                }
                let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
                if !ignore
                    .matched_path_or_any_parents(entry.path(), is_dir)
                    .is_ignore()
                {
                    result.paths.push(entry.into_path());
                }
            }
            Err(error) => result.errors.push(error.to_string()),
        }
    }
    result.paths.sort();
    result.errors.sort();
    result.errors.dedup();
    result
}

/// The directories to watch and the files under them worth sweeping.
#[derive(Debug, Default)]
pub struct WatchSet {
    /// Directories to place one non-recursive watch on each.
    ///
    /// Directories rather than files, because a watch on a file's inode
    /// misses an atomic save: the writer creates a temporary file and
    /// renames it over the target, destroying the inode the watch was
    /// placed on. The rename arrives on the parent directory instead.
    pub directories: BTreeSet<PathBuf>,
    /// Files under those directories that [`PathFilter::admits`] accepts.
    ///
    /// Used when an event overflow forces a full sweep, where the paths on
    /// the events cannot be trusted and the walk is the only honest answer
    /// to what may have changed.
    pub files: Vec<PathBuf>,
    /// Traversal failures; the set may be incomplete.
    pub errors: Vec<String>,
}

/// Walk `roots` and collect every directory `filter` admits, with the
/// admitted files under them.
///
/// `ignore`'s own standard filters are switched off and every ruling is
/// taken from `filter`. Letting `WalkBuilder` apply its own `.gitignore`
/// handling would honour nested ignore files that [`PathFilter`] does not
/// read, making the watch set narrower than the admitted set, which is the
/// silent-loss direction.
#[must_use]
pub fn watch_set(filter: &Arc<PathFilter>, roots: &[PathBuf]) -> WatchSet {
    let mut result = WatchSet::default();
    for root in roots {
        if root.is_file() {
            result.errors.push(format!(
                "{}: project root is not a directory",
                root.display()
            ));
            continue;
        }
        result.directories.insert(root.clone());
        let walk_filter = Arc::clone(filter);
        let root_owned = root.clone();
        let walk = WalkBuilder::new(root)
            .standard_filters(false)
            .filter_entry(move |entry| {
                if entry.path() == root_owned {
                    return true;
                }
                if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                    walk_filter.admits_directory(entry.path())
                } else {
                    true
                }
            })
            .build();
        for entry in walk {
            match entry {
                Ok(entry) => {
                    if let Some(error) = entry.error() {
                        result.errors.push(error.to_string());
                    }
                    if entry.path() == root {
                        continue;
                    }
                    if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                        result.directories.insert(entry.into_path());
                    } else if filter.admits(entry.path()) {
                        result.files.push(entry.into_path());
                    }
                }
                Err(error) => result.errors.push(error.to_string()),
            }
        }
    }
    result.files.sort();
    result.files.dedup();
    result.errors.sort();
    result.errors.dedup();
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::config::ServerId;

    /// The configured roots for a filter over one temporary directory.
    fn roots(dir: &Path) -> Arc<[PathBuf]> {
        Arc::from(vec![dir.to_path_buf()])
    }

    /// The extension map a filter routes on, matching the built-in rust
    /// entry. `.xyz` is deliberately absent, so the registry-override test
    /// has something unroutable to admit.
    fn extensions() -> Arc<HashMap<String, String>> {
        Arc::new(HashMap::from([("rs".to_string(), "rust".to_string())]))
    }

    /// A filter over `dir` with no watch registry, which is the ordinary
    /// case: only the registry-override test passes one.
    fn filter_over(dir: &Path) -> PathFilter {
        PathFilter::new(roots(dir), extensions(), None)
    }

    /// A monorepo holding two subprojects, each configured as a root of its
    /// own alongside the repository root and each keeping its own
    /// `.gitignore`. The outer `.gitignore` excludes `vendor/` and says
    /// nothing about `sub/`.
    ///
    /// The two subprojects are the two ways an outer root and a nested one
    /// can disagree: `vendor/tool` is a root the outer `.gitignore`
    /// excludes wholesale, and `sub` is a root that excludes a file the
    /// outer `.gitignore` would have admitted.
    fn nested_roots(dir: &Path) -> Vec<PathBuf> {
        let vendored = dir.join("vendor/tool");
        let sub = dir.join("sub");
        std::fs::create_dir_all(vendored.join("src")).expect("mkdir");
        std::fs::create_dir_all(sub.join("src")).expect("mkdir");
        std::fs::write(dir.join(".gitignore"), "vendor/\n").expect("write");
        std::fs::write(sub.join(".gitignore"), "generated.rs\n").expect("write");
        vec![dir.to_path_buf(), vendored, sub]
    }

    /// Both orders of the same roots, so a test can assert an answer does
    /// not depend on which order they were configured in.
    fn both_orders(dir: &Path) -> [PathFilter; 2] {
        let roots = nested_roots(dir);
        let reversed: Vec<PathBuf> = roots.iter().rev().cloned().collect();
        [
            PathFilter::new(Arc::from(roots), extensions(), None),
            PathFilter::new(Arc::from(reversed), extensions(), None),
        ]
    }

    #[test]
    fn test_a_nested_root_inside_an_ignored_subtree_is_admitted() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let source = dir.path().join("vendor/tool/src/a.rs");

        for filter in both_orders(dir.path()) {
            std::fs::write(&source, "").expect("write");
            assert!(
                filter.admits(&source),
                "the outer root's vendor/ line says nothing about a subproject \
                 configured as a root in its own right, and an answer that \
                 changes when the roots are listed the other way round is not \
                 an answer about the file"
            );
        }
    }

    #[test]
    fn test_a_nested_root_governs_its_own_contents() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let generated = dir.path().join("sub/src/generated.rs");

        for filter in both_orders(dir.path()) {
            std::fs::write(&generated, "").expect("write");
            assert!(
                !filter.admits(&generated),
                "the deepest root containing a file is the one whose .gitignore \
                 was written about it; the repository root, which knows nothing \
                 of this file, must not be able to re-admit what the subproject \
                 itself excluded"
            );
        }
    }

    /// A repository root that writes its build directory into its own
    /// `.gitignore` once, and a subproject configured as a root of its own
    /// which does not repeat the rule -- how a JS or Python monorepo is
    /// actually laid out, `dist/` being neither `target/` nor
    /// `node_modules/` and so outside [`BUILT_IN_IGNORES`].
    fn monorepo_sharing_one_build_rule(dir: &Path) -> Vec<PathBuf> {
        let sub = dir.join("sub");
        std::fs::create_dir_all(sub.join("dist")).expect("mkdir");
        std::fs::write(dir.join(".gitignore"), "dist/\n").expect("write");
        vec![dir.to_path_buf(), sub]
    }

    #[test]
    fn test_an_outer_build_rule_reaches_under_a_nested_root() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let roots = monorepo_sharing_one_build_rule(dir.path());
        let bundle = dir.path().join("sub/dist/bundle.rs");
        std::fs::write(&bundle, "").expect("write");
        let reversed: Vec<PathBuf> = roots.iter().rev().cloned().collect();

        for roots in [roots, reversed] {
            let filter = PathFilter::new(Arc::from(roots), extensions(), None);
            assert!(
                !filter.admits(&bundle),
                "a repository root's build rule is written once and meant for \
                 everything below it, and a subproject configured as a root \
                 does not switch it off: an npm build under one would \
                 otherwise fill the tracker to its ceiling"
            );
        }
    }

    #[test]
    fn test_a_generated_directory_is_not_watchable() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("target/debug")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        let filter = filter_over(dir.path());

        assert!(
            filter.admits_directory(&dir.path().join("src")),
            "a source directory must carry a watch or nothing under it is ever \
             heard about"
        );
        assert!(
            !filter.admits_directory(&dir.path().join("target")),
            "a watch under target/ is the descriptor exhaustion this design \
             exists to avoid"
        );
    }

    #[test]
    fn test_the_watch_set_reaches_below_the_top_level() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("src/deep/deeper")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("target/debug")).expect("mkdir");
        std::fs::write(dir.path().join("src/deep/a.rs"), "").expect("write");
        let filter = Arc::new(filter_over(dir.path()));

        let set = watch_set(&filter, &[dir.path().to_path_buf()]);

        assert!(
            set.directories.contains(&dir.path().join("src/deep/deeper")),
            "watchPaths only ever saw the top level, which is the gap this \
             walk closes"
        );
        assert!(
            !set.directories
                .iter()
                .any(|d| d.starts_with(dir.path().join("target"))),
            "a watch under target/ is the descriptor exhaustion this design \
             avoids by construction rather than by filtering events"
        );
        assert!(set.files.contains(&dir.path().join("src/deep/a.rs")));
    }

    #[test]
    fn test_the_watch_set_includes_the_root_itself() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let filter = Arc::new(filter_over(dir.path()));

        let set = watch_set(&filter, &[dir.path().to_path_buf()]);

        assert!(
            set.directories.contains(&dir.path().to_path_buf()),
            "a file created directly in the root has no other watch to \
             arrive on, and asking a root's own matcher about its base path \
             is not a question ignore promises to answer"
        );
    }

    #[test]
    fn test_a_directory_under_two_roots_is_watched_once() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(sub.join("src")).expect("mkdir");
        let roots = vec![dir.path().to_path_buf(), sub.clone()];
        let filter = Arc::new(PathFilter::new(
            Arc::from(roots.clone()),
            extensions(),
            None,
        ));

        let set = watch_set(&filter, &roots);

        assert!(
            set.directories.contains(&sub.join("src")),
            "a monorepo configures the repository root and a subproject \
             both, and the subproject's sources must still be watched"
        );
        assert_eq!(
            set.directories
                .iter()
                .filter(|d| **d == sub.join("src"))
                .count(),
            1,
            "a set, not a list: two roots containing one directory must not \
             cost two inotify descriptors for it"
        );
    }

    #[test]
    fn test_a_build_artifact_is_dropped() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("target/debug")).expect("mkdir");
        let artifact = dir.path().join("target/debug/thing.rs");
        std::fs::write(&artifact, "").expect("write");
        let filter = filter_over(dir.path());

        assert!(
            !filter.admits(&artifact),
            "a cargo check would otherwise fill the tracker to its ceiling and \
             every later tool call would fail with DocumentLimitExceeded"
        );
    }

    #[test]
    fn test_a_gitignored_file_is_dropped() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join(".gitignore"), "generated.rs\n").expect("write");
        let generated = dir.path().join("generated.rs");
        std::fs::write(&generated, "").expect("write");

        assert!(!filter_over(dir.path()).admits(&generated));
    }

    #[test]
    fn test_a_path_outside_every_root_is_dropped() {
        let inside = tempfile::tempdir().expect("a temp dir");
        let outside = tempfile::tempdir().expect("a temp dir");
        let stray = outside.path().join("a.rs");
        std::fs::write(&stray, "").expect("write");

        assert!(!filter_over(inside.path()).admits(&stray));
    }

    #[test]
    fn test_a_routable_extension_is_admitted() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let source = dir.path().join("src/a.rs");
        std::fs::create_dir_all(source.parent().expect("a parent")).expect("mkdir");
        std::fs::write(&source, "").expect("write");

        assert!(filter_over(dir.path()).admits(&source));
    }

    #[test]
    fn test_an_unroutable_extension_with_no_watcher_is_dropped() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let blob = dir.path().join("notes.xyz");
        std::fs::write(&blob, "").expect("write");

        assert!(!filter_over(dir.path()).admits(&blob));
    }

    /// A filter over `dir` whose registry holds `watchers` for one server,
    /// in the `registerOptions.watchers` shape a `client/registerCapability`
    /// carries.
    fn filter_watching(dir: &Path, watchers: &serde_json::Value) -> PathFilter {
        let registry = Arc::new(WatchRegistry::new());
        registry.register(&ServerId::from("weird"), "r1", watchers);
        PathFilter::new(roots(dir), extensions(), Some(registry))
    }

    #[test]
    fn test_an_unroutable_extension_a_server_registered_for_is_admitted() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let blob = dir.path().join("notes.xyz");
        std::fs::write(&blob, "").expect("write");

        let filter = filter_watching(
            dir.path(),
            &serde_json::json!([{ "globPattern": "**/*.xyz" }]),
        );
        assert!(
            filter.admits(&blob),
            "a server that asked for a pattern is the authority on whether that \
             file matters to it"
        );
    }

    /// gopls registers `**/go.work` and `**/go.mod` for creation and
    /// deletion and for nothing else. Admission runs before anything has
    /// decided what happened to the path, so a gate that asks about one
    /// kind answers no here and the path is dropped before the sweep can
    /// classify it -- the server's own watcher never fires, for either of
    /// the kinds it asked for.
    #[test]
    fn test_a_path_watched_only_for_creation_and_deletion_is_admitted() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let work = dir.path().join("go.work");
        std::fs::write(&work, "").expect("write");

        let filter = filter_watching(
            dir.path(),
            &serde_json::json!([{ "globPattern": "**/go.work", "kind": 5 }]),
        );
        assert!(filter.admits(&work));
    }

    /// A watcher that wants no kind at all wants no events, so the path it
    /// names is not worth waking anything for.
    #[test]
    fn test_a_path_watched_for_no_kind_at_all_is_dropped() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let work = dir.path().join("go.work");
        std::fs::write(&work, "").expect("write");

        let filter = filter_watching(
            dir.path(),
            &serde_json::json!([{ "globPattern": "**/go.work", "kind": 0 }]),
        );
        assert!(!filter.admits(&work));
    }

    /// The two facts a `.gitignore` negation under a built-in ignore buys,
    /// which are not one fact: the negated file is admitted, so mcpls
    /// checks it whenever it hears about it, and its directory still
    /// carries no watch, so nothing outside the session's own tool calls
    /// ever tells mcpls it changed. A negation cannot re-include a file
    /// whose parent directory is excluded, and the watcher is handed
    /// directories.
    #[test]
    fn test_a_negated_path_is_admitted_and_still_not_watched() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("target")).expect("mkdir");
        let kept = dir.path().join("target/keep.rs");
        std::fs::write(&kept, "").expect("write");
        std::fs::write(dir.path().join(".gitignore"), "!target/keep.rs\n").expect("write");
        let filter = Arc::new(filter_over(dir.path()));

        assert!(filter.admits(&kept));

        let set = watch_set(&filter, &[dir.path().to_path_buf()]);
        assert!(
            !set.directories.contains(&dir.path().join("target")),
            "the troubleshooting guide tells a user what this workaround does \
             and does not buy them, and it can only be true while these two \
             answers stay apart"
        );
        assert!(
            !set.files.contains(&kept),
            "the walk never descends into target/, so the negated file is not \
             reached by it either"
        );
    }

    #[test]
    fn test_the_watch_set_drops_the_built_in_floor_with_no_gitignore_present() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("target/debug")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("node_modules")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        let filter = Arc::new(filter_over(dir.path()));

        let directories = watch_set(&filter, &[dir.path().to_path_buf()]).directories;

        assert!(directories.contains(&dir.path().join("src")));
        assert!(
            !directories.contains(&dir.path().join("target")),
            "the walk must apply the same built-in floor admits does, or a \
             project with no .gitignore spends a watch descriptor on every \
             directory of its own build output"
        );
        assert!(!directories.contains(&dir.path().join("node_modules")));
    }

    #[test]
    fn test_watch_paths_names_children_rather_than_the_root() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("target")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("mkdir");
        std::fs::write(dir.path().join("Cargo.toml"), "").expect("write");
        std::fs::write(dir.path().join(".gitignore"), "/target\n").expect("write");

        let paths = watch_paths(dir.path()).paths;

        assert!(paths.contains(&dir.path().join("src")));
        assert!(paths.contains(&dir.path().join("Cargo.toml")));
        assert!(
            !paths.contains(&dir.path().join("target")),
            "the host's watcher passes no ignore list, so this list is the only \
             thing keeping a hook process off every build artifact"
        );
        assert!(!paths.contains(&dir.path().join(".git")));
    }

    #[test]
    fn test_the_watch_set_excludes_the_git_directory() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("target")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join(".git/objects")).expect("mkdir");
        std::fs::write(dir.path().join(".gitignore"), "/target\n").expect("write");
        let filter = Arc::new(filter_over(dir.path()));

        let directories = watch_set(&filter, &[dir.path().to_path_buf()]).directories;

        assert!(directories.contains(&dir.path().join("src")));
        assert!(!directories.contains(&dir.path().join("target")));
        assert!(
            !directories.contains(&dir.path().join(".git/objects")),
            "no project writes .git into its own .gitignore, so only the \
             built-in floor keeps it out; git rewrites the object store on \
             every commit, fetch and index update, and a watch on it would \
             wake the sweeper for each one"
        );
    }
}
