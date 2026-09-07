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
#[derive(Debug)]
struct Watcher {
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
                glob: glob.compile_matcher(),
                kinds,
            });
        }
        lock_std(&self.by_server)
            .entry(server.clone())
            .or_default()
            .insert(id.to_string(), compiled);
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
