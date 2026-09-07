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
use lsp_types::FileChangeType;

use crate::lsp::WatchRegistry;

/// Directories that hold generated or vendored files, dropped under every
/// root even when a project's own `.gitignore` does not name them.
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
    #[must_use]
    pub fn admits(&self, path: &Path) -> bool {
        let Some(ignore) = self
            .roots
            .iter()
            .zip(&self.ignores)
            .find_map(|(root, ignore)| path.starts_with(root).then_some(ignore))
        else {
            return false;
        };
        if ignore.matched_path_or_any_parents(path, false).is_ignore() {
            return false;
        }

        let routable = path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|ext| self.extensions.contains_key(ext));
        if routable {
            return true;
        }
        self.registry.as_ref().is_some_and(|registry| {
            !registry
                .servers_for(path, FileChangeType::CHANGED)
                .is_empty()
        })
    }
}

/// One root's ignore matcher: the built-in denylist plus that root's own
/// `.gitignore`, if it has one.
fn gitignore_for(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in BUILT_IN_IGNORES {
        let _ = builder.add_line(None, pattern);
    }
    let _ = builder.add(root.join(".gitignore"));
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// The directories and root-level files a session's watcher should cover.
#[must_use]
pub fn watch_paths(root: &Path) -> Vec<PathBuf> {
    WalkBuilder::new(root)
        .max_depth(Some(1))
        .build()
        .filter_map(Result::ok)
        .map(ignore::DirEntry::into_path)
        .filter(|path| path != root)
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

    #[test]
    fn test_an_unroutable_extension_a_server_registered_for_is_admitted() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let blob = dir.path().join("notes.xyz");
        std::fs::write(&blob, "").expect("write");
        let registry = Arc::new(WatchRegistry::new());
        registry.register(
            &ServerId::from("weird"),
            "r1",
            &serde_json::json!([{ "globPattern": "**/*.xyz" }]),
        );

        let filter = PathFilter::new(roots(dir.path()), extensions(), Some(registry));
        assert!(
            filter.admits(&blob),
            "a server that asked for a pattern is the authority on whether that \
             file matters to it"
        );
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
