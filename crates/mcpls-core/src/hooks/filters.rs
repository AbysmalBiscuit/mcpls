//! Deciding which changed paths are worth acting on.
//!
//! Claude Code's hooks report every path a tool touched, with no ignore
//! list of their own: a `cargo build` or an `npm install` can report
//! thousands of paths under `target/` or `node_modules/`, and the document
//! tracker has a ceiling. `PathFilter` is what keeps that flood out, and
//! `watch_paths` is what keeps the host's watcher from ever picking it up
//! in the first place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::lsp::WatchRegistry;

/// Directories that hold generated or vendored files, dropped under every
/// root even when a project's own `.gitignore` does not name them.
///
/// These lines are added to each root's matcher before that root's own
/// `.gitignore`, so a project that genuinely keeps sources under one of
/// these names can re-admit a specific path with a `.gitignore` negation,
/// for example `!target/keep.rs`: everything else under `target/` stays
/// dropped, but that one path is not.
const BUILT_IN_IGNORES: &[&str] = &["target", "node_modules"];

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
    /// configured -- the deepest of them decides. It is the one whose
    /// `.gitignore` was written about this file: an outer root's `vendor/`
    /// line says nothing about what the subproject keeps, and letting it
    /// answer would drop every path under a nested root the user configured
    /// on purpose. The order the roots were configured in never enters into
    /// it.
    #[must_use]
    pub fn admits(&self, path: &Path) -> bool {
        let Some(ignore) = self
            .roots
            .iter()
            .zip(&self.ignores)
            .filter(|(root, _)| path.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .map(|(_, ignore)| ignore)
        else {
            return false;
        };
        if ignore.matched_path_or_any_parents(path, false).is_ignore() {
            return false;
        }

        if self.routable_extension(path) {
            return true;
        }
        self.registry
            .as_ref()
            .is_some_and(|registry| registry.is_watched(path))
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

/// One root's ignore matcher: the built-in denylist plus that root's own
/// `.gitignore`, if it has one. `admits` and `watch_paths` both build a
/// matcher through this function rather than each deciding "ignored" its
/// own way, so the two cannot drift into disagreeing about the same path.
fn gitignore_for(root: &Path) -> Gitignore {
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
            tracing::warn!(
                path = %gitignore.display(),
                %error,
                "ignoring the project's .gitignore, which could not be read or parsed"
            );
        }
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// The directories and root-level files a session's watcher should cover.
///
/// Filtered through the same matcher `admits` uses, so a build directory
/// the project's own `.gitignore` does not name is kept off the watcher
/// exactly as it is kept off the tracker.
#[must_use]
pub fn watch_paths(root: &Path) -> Vec<PathBuf> {
    let ignore = gitignore_for(root);
    WalkBuilder::new(root)
        .max_depth(Some(1))
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.path() != root)
        .filter(|entry| {
            let is_dir = entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir());
            !ignore
                .matched_path_or_any_parents(entry.path(), is_dir)
                .is_ignore()
        })
        .map(ignore::DirEntry::into_path)
        .collect()
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

    #[test]
    fn test_watch_paths_drops_the_built_in_floor_with_no_gitignore_present() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("target/debug")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("node_modules")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");

        let paths = watch_paths(dir.path());

        assert!(paths.contains(&dir.path().join("src")));
        assert!(
            !paths.contains(&dir.path().join("target")),
            "watch_paths must apply the same built-in floor admits does, or a \
             project with no .gitignore hands the host a watcher over its own \
             build output"
        );
        assert!(!paths.contains(&dir.path().join("node_modules")));
    }

    #[test]
    fn test_watch_paths_names_children_rather_than_the_root() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("target")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("mkdir");
        std::fs::write(dir.path().join("Cargo.toml"), "").expect("write");
        std::fs::write(dir.path().join(".gitignore"), "/target\n").expect("write");

        let paths = watch_paths(dir.path());

        assert!(paths.contains(&dir.path().join("src")));
        assert!(paths.contains(&dir.path().join("Cargo.toml")));
        assert!(
            !paths.contains(&dir.path().join("target")),
            "the host's watcher passes no ignore list, so this list is the only \
             thing keeping a hook process off every build artifact"
        );
        assert!(!paths.contains(&dir.path().join(".git")));
    }
}
