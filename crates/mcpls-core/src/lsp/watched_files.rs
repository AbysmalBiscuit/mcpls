//! Which language servers asked the client to watch which files.
//!
//! A server sends `client/registerCapability` for
//! `workspace/didChangeWatchedFiles` carrying an array of watchers, each a
//! glob pattern and an optional change-kind bitmask. This holds them so
//! that when mcpls learns a path changed it can tell exactly the servers
//! that asked, and no others.
//!
//! Kept apart from `LspClient` so the matching is testable without a live
//! server, and shared by reference between the client that writes it and
//! the translator that reads it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use globset::{GlobBuilder, GlobMatcher};

use crate::bridge::lock_std;
use crate::config::ServerId;

/// One watcher: a compiled glob and the kinds it wants.
///
/// `pattern` is kept alongside the compiled `glob` because `GlobMatcher`
/// carries no equality of its own; `register` needs the source string back
/// to deduplicate a re-registered watcher against ones already stored.
#[derive(Debug)]
struct Watcher {
    pattern: String,
    glob: GlobMatcher,
    kinds: u32,
}

/// Create, change and delete, which is what an absent `kind` means.
const ALL_KINDS: u32 = 0b111;

impl Watcher {
    const fn wants(&self, kind: lsp_types::FileChangeType) -> bool {
        let bit = match kind {
            lsp_types::FileChangeType::CREATED => 1,
            lsp_types::FileChangeType::CHANGED => 2,
            lsp_types::FileChangeType::DELETED => 4,
            _ => return false,
        };
        self.kinds & bit != 0
    }
}

/// Which servers asked to be told about which files.
#[derive(Debug, Default)]
pub struct WatchRegistry {
    /// server -> registration id -> that registration's watchers.
    by_server: Mutex<HashMap<ServerId, HashMap<String, Vec<Watcher>>>>,
}

impl WatchRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Store the watchers in a `client/registerCapability` payload's
    /// `registerOptions.watchers` array, under `id`.
    ///
    /// A watcher whose pattern is a `RelativePattern` object is logged and
    /// skipped: mcpls does not claim `relativePatternSupport`, so receiving
    /// one means a server ignored the advertised capability, and guessing
    /// at its base URI would be worse than a log line. A pattern that does
    /// not compile is skipped the same way. Either case leaves the rest of
    /// the array in force.
    ///
    /// A registration under an id already in use for this server adds to
    /// it rather than replacing it, deduplicating by the pair of glob
    /// pattern string and change-kind mask. pyrefly registers `FILEWATCHER`
    /// more than once per session with disjoint, complementary watcher
    /// sets -- workspace patterns (including `**/*.py`) right after
    /// `initialized`, then interpreter- and bytecode-derived patterns on
    /// the first `didOpen` -- and means both to stay in force at once.
    /// Replace semantics does not model that: treating the second
    /// registration as superseding the first silently dropped `**/*.py`,
    /// the one glob that mattered, a message before any registration ever
    /// arrived empty. Deduplication is what keeps accumulation bounded: a
    /// server that re-registers globs it already holds adds nothing, so
    /// the stored set can never exceed the number of distinct globs that
    /// server has ever asked for -- for any server, not only pyrefly. An
    /// empty array now contributes nothing by construction and needs no
    /// special case.
    ///
    /// The lock is held across the dedup check and the pushes deliberately:
    /// dropping it in between would let two concurrent registrations for
    /// the same id interleave, so each saw the other's watchers as absent
    /// and both pushed the same one.
    #[allow(clippy::significant_drop_tightening)]
    pub fn register(&self, server: &ServerId, id: &str, watchers: &serde_json::Value) {
        let mut compiled = Vec::new();
        for watcher in watchers.as_array().into_iter().flatten() {
            let Some(pattern) = watcher.get("globPattern") else {
                continue;
            };
            let Some(pattern) = pattern.as_str() else {
                tracing::warn!(
                    %server,
                    registration = id,
                    "skipping a relative watcher pattern; mcpls does not claim \
                     relativePatternSupport"
                );
                continue;
            };
            // `literal_separator(true)` is not globset's default: without
            // it a `*` crosses a `/`, and LSP's glob grammar says it does
            // not.
            let glob = match GlobBuilder::new(pattern).literal_separator(true).build() {
                Ok(glob) => glob,
                Err(error) => {
                    tracing::warn!(%server, registration = id, pattern, %error, "skipping an uncompilable watcher glob");
                    continue;
                }
            };
            let kinds = watcher
                .get("kind")
                .and_then(serde_json::Value::as_u64)
                .and_then(|k| u32::try_from(k).ok())
                .unwrap_or(ALL_KINDS);
            compiled.push(Watcher {
                pattern: pattern.to_owned(),
                glob: glob.compile_matcher(),
                kinds,
            });
        }
        let mut by_server = lock_std(&self.by_server);
        let existing = by_server
            .entry(server.clone())
            .or_default()
            .entry(id.to_string())
            .or_default();
        for watcher in compiled {
            let already_held = existing
                .iter()
                .any(|w| w.pattern == watcher.pattern && w.kinds == watcher.kinds);
            if !already_held {
                existing.push(watcher);
            }
        }
    }

    /// The number of distinct watchers stored for `(server, id)`.
    ///
    /// Test-only: production code has no reason to count watchers, only to
    /// match against them, but a test that asserts accumulation stays
    /// bounded needs to see the count directly rather than inferring it
    /// from `servers_for`'s yes/no answer.
    #[cfg(test)]
    fn watcher_count(&self, server: &ServerId, id: &str) -> usize {
        lock_std(&self.by_server)
            .get(server)
            .and_then(|by_id| by_id.get(id))
            .map_or(0, Vec::len)
    }

    /// Drop the registration `id` holds for `server`.
    pub fn unregister(&self, server: &ServerId, id: &str) {
        if let Some(registrations) = lock_std(&self.by_server).get_mut(server) {
            registrations.remove(id);
        }
    }

    /// Drop every registration `server` holds.
    ///
    /// Called when a server is respawned: the fresh process registers again
    /// with new ids, so without this the old globs stay in force forever
    /// and a server that narrowed its watch keeps being told about files it
    /// no longer wants.
    pub fn forget_server(&self, server: &ServerId) {
        lock_std(&self.by_server).remove(server);
    }

    /// Whether any server asked to hear about `path` under any change kind.
    ///
    /// The question a caller asks when nothing has yet established what
    /// happened to the path. Claude Code's hooks report paths and the sweep
    /// classifies them later, so the admission gate runs before any kind
    /// exists; asking `servers_for` about a guessed kind there drops the
    /// path of a server that registered `**/go.work` for creation and
    /// deletion only, and the sweep never sees it. A watcher whose mask
    /// wants no kind at all matches nothing here either.
    #[must_use]
    pub fn is_watched(&self, path: &Path) -> bool {
        lock_std(&self.by_server)
            .values()
            .flat_map(HashMap::values)
            .flatten()
            .any(|watcher| watcher.kinds & ALL_KINDS != 0 && watcher.glob.is_match(path))
    }

    /// Servers that asked to hear about `path` changing this way, sorted so
    /// the notification order is reproducible.
    #[must_use]
    pub fn servers_for(&self, path: &Path, kind: lsp_types::FileChangeType) -> Vec<ServerId> {
        let mut matched: Vec<ServerId> = {
            let registrations = lock_std(&self.by_server);
            registrations
                .iter()
                .filter(|(_, by_id)| {
                    by_id
                        .values()
                        .flatten()
                        .any(|w| w.wants(kind) && w.glob.is_match(path))
                })
                .map(|(server, _)| server.clone())
                .collect()
        };
        matched.sort_unstable();
        matched
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn abs(rel: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:\\work\\{}", rel.replace('/', "\\")))
        } else {
            PathBuf::from(format!("/work/{rel}"))
        }
    }

    #[test]
    fn test_a_registered_glob_matches_its_own_server_only() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        let python = ServerId::from("python");
        registry.register(&go, "r1", &json!([{ "globPattern": "**/*.go" }]));
        registry.register(&python, "r2", &json!([{ "globPattern": "**/*.py" }]));

        assert_eq!(
            registry.servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED),
            vec![go]
        );
        assert_eq!(
            registry.servers_for(&abs("main.py"), lsp_types::FileChangeType::CHANGED),
            vec![python]
        );
    }

    #[test]
    fn test_an_absent_kind_means_every_kind() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        registry.register(&go, "r1", &json!([{ "globPattern": "**/*.go" }]));

        for kind in [
            lsp_types::FileChangeType::CREATED,
            lsp_types::FileChangeType::CHANGED,
            lsp_types::FileChangeType::DELETED,
        ] {
            assert_eq!(
                registry.servers_for(&abs("main.go"), kind),
                vec![go.clone()],
                "the protocol says an absent kind is create, change and delete"
            );
        }
    }

    #[test]
    fn test_a_kind_bitmask_excludes_the_kinds_it_omits() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        // 1 = Created, 2 = Changed, 4 = Deleted. 5 is create and delete.
        registry.register(&go, "r1", &json!([{ "globPattern": "**/*.go", "kind": 5 }]));

        assert!(
            registry
                .servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED)
                .is_empty()
        );
        assert_eq!(
            registry.servers_for(&abs("main.go"), lsp_types::FileChangeType::DELETED),
            vec![go]
        );
    }

    #[test]
    fn test_a_watcher_that_omits_a_kind_still_counts_as_watching_the_path() {
        let registry = WatchRegistry::new();
        // 1 = Created, 2 = Changed, 4 = Deleted. 5 is create and delete,
        // which is what gopls registers for `go.work` and `go.mod`.
        registry.register(
            &ServerId::from("go"),
            "r1",
            &json!([{ "globPattern": "**/go.work", "kind": 5 }]),
        );

        assert!(
            registry.is_watched(&abs("go.work")),
            "the caller has not decided what happened to this path yet, so a \
             gate that answered no here would drop it before anything could"
        );
        assert!(!registry.is_watched(&abs("go.mod")));
    }

    #[test]
    fn test_a_watcher_wanting_no_kind_watches_nothing() {
        let registry = WatchRegistry::new();
        registry.register(
            &ServerId::from("go"),
            "r1",
            &json!([{ "globPattern": "**/go.work", "kind": 0 }]),
        );

        assert!(!registry.is_watched(&abs("go.work")));
    }

    #[test]
    fn test_a_star_does_not_cross_a_directory_separator() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        registry.register(&go, "r1", &json!([{ "globPattern": "/work/*.go" }]));

        assert_eq!(
            registry.servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED),
            vec![go],
            "a single star matches within one segment"
        );
        assert!(
            registry
                .servers_for(&abs("cmd/main.go"), lsp_types::FileChangeType::CHANGED)
                .is_empty(),
            "globset lets a star cross a separator by default and LSP's glob \
             grammar does not, so literal_separator must be on"
        );
    }

    #[test]
    fn test_unregistering_drops_only_that_registration() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        registry.register(&go, "r1", &json!([{ "globPattern": "**/*.go" }]));
        registry.register(&go, "r2", &json!([{ "globPattern": "**/*.mod" }]));
        registry.unregister(&go, "r1");

        assert!(
            registry
                .servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED)
                .is_empty()
        );
        assert_eq!(
            registry.servers_for(&abs("go.mod"), lsp_types::FileChangeType::CHANGED),
            vec![go]
        );
    }

    #[test]
    fn test_disjoint_registrations_under_one_id_both_stay_matchable() {
        let registry = WatchRegistry::new();
        let python = ServerId::from("python");
        registry.register(
            &python,
            "FILEWATCHER",
            &json!([{ "globPattern": "**/*.py" }]),
        );
        registry.register(
            &python,
            "FILEWATCHER",
            &json!([{ "globPattern": "**/*.pyc" }]),
        );

        assert_eq!(
            registry.servers_for(&abs("main.py"), lsp_types::FileChangeType::CHANGED),
            vec![python.clone()],
            "pyrefly registers its workspace patterns and its \
             interpreter-derived patterns as two separate calls under the \
             same id, and means both to stay in force -- this is that case"
        );
        assert_eq!(
            registry.servers_for(&abs("main.pyc"), lsp_types::FileChangeType::CHANGED),
            vec![python]
        );
    }

    #[test]
    fn test_an_empty_reregistration_does_not_replace_a_working_one() {
        let registry = WatchRegistry::new();
        let python = ServerId::from("python");
        registry.register(
            &python,
            "FILEWATCHER",
            &json!([{ "globPattern": "**/*.py" }]),
        );
        registry.register(&python, "FILEWATCHER", &json!([]));

        assert_eq!(
            registry.servers_for(&abs("main.py"), lsp_types::FileChangeType::CHANGED),
            vec![python],
            "an empty registration adds no watchers under accumulation, so \
             it can no longer erase a working one -- this now holds for \
             free rather than needing a special case for the empty array"
        );
    }

    #[test]
    fn test_reregistering_the_same_globs_does_not_grow_the_stored_set() {
        let registry = WatchRegistry::new();
        let python = ServerId::from("python");
        registry.register(
            &python,
            "FILEWATCHER",
            &json!([{ "globPattern": "**/*.py" }]),
        );
        registry.register(
            &python,
            "FILEWATCHER",
            &json!([{ "globPattern": "**/*.py" }]),
        );

        assert_eq!(
            registry.watcher_count(&python, "FILEWATCHER"),
            1,
            "re-registering a glob the server already holds must add nothing, \
             or accumulation would grow the stored set without bound"
        );
    }

    #[test]
    fn test_forgetting_a_server_drops_every_registration_it_held() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        registry.register(&go, "r1", &json!([{ "globPattern": "**/*.go" }]));
        registry.register(&go, "r2", &json!([{ "globPattern": "**/*.mod" }]));
        registry.forget_server(&go);

        assert!(
            registry
                .servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED)
                .is_empty(),
            "a respawned process registers again with fresh ids, so the old \
             globs would otherwise linger for the life of the mcpls process"
        );
    }

    #[test]
    fn test_a_relative_pattern_is_skipped_and_its_siblings_are_kept() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        registry.register(
            &go,
            "r1",
            &json!([
                { "globPattern": { "baseUri": "file:///work", "pattern": "**/*.go" } },
                { "globPattern": "**/*.mod" }
            ]),
        );

        assert!(
            registry
                .servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED)
                .is_empty(),
            "mcpls does not claim relativePatternSupport, so a relative \
             pattern is a server ignoring the advertised capability"
        );
        assert_eq!(
            registry.servers_for(&abs("go.mod"), lsp_types::FileChangeType::CHANGED),
            vec![go],
            "one unusable watcher must not discard the rest of the array"
        );
    }

    #[test]
    fn test_an_uncompilable_glob_is_skipped_rather_than_panicking() {
        let registry = WatchRegistry::new();
        let go = ServerId::from("go");
        registry.register(&go, "r1", &json!([{ "globPattern": "**/[" }]));
        assert!(
            registry
                .servers_for(&abs("main.go"), lsp_types::FileChangeType::CHANGED)
                .is_empty()
        );
    }
}
