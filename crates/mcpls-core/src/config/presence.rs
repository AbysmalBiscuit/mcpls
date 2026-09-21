//! Which languages a checkout actually holds files for.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ignore::{WalkBuilder, WalkState};

use super::ServerConfig;
use super::language::base_language_id;

impl ServerConfig {
    /// The languages in `wanted` that have at least one file under `root`,
    /// detected by extension the way routing detects them.
    ///
    /// A parallel walk that honours `.gitignore` and skips hidden entries,
    /// so build output and vendored trees never count. It stops as soon as
    /// every wanted language is found, so a checkout that holds them all
    /// pays for a walk up to the last first match, not the whole tree.
    #[must_use]
    pub fn languages_present(&self, root: &Path, wanted: &HashSet<String>) -> HashSet<String> {
        let wanted: Vec<&String> = wanted.iter().collect();
        if wanted.is_empty() {
            return HashSet::new();
        }
        let slots: Arc<HashMap<String, usize>> = Arc::new(
            self.build_effective_extension_map()
                .into_iter()
                .filter_map(|(extension, language)| {
                    let language = base_language_id(&language).unwrap_or(&language);
                    let slot = wanted.iter().position(|w| w.as_str() == language)?;
                    Some((extension, slot))
                })
                .collect(),
        );
        let found: Arc<Vec<AtomicBool>> =
            Arc::new(wanted.iter().map(|_| AtomicBool::new(false)).collect());
        let missing = Arc::new(AtomicUsize::new(wanted.len()));

        WalkBuilder::new(root)
            .require_git(false)
            .follow_links(false)
            .build_parallel()
            .run(|| {
                let slots = Arc::clone(&slots);
                let found = Arc::clone(&found);
                let missing = Arc::clone(&missing);
                Box::new(move |entry| {
                    if missing.load(Ordering::Relaxed) == 0 {
                        return WalkState::Quit;
                    }
                    let Ok(entry) = entry else {
                        return WalkState::Continue;
                    };
                    let slot = entry
                        .path()
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .and_then(|extension| slots.get(extension));
                    if let Some(&slot) = slot
                        && entry.file_type().is_some_and(|kind| kind.is_file())
                        && !found[slot].swap(true, Ordering::Relaxed)
                        && missing.fetch_sub(1, Ordering::Relaxed) == 1
                    {
                        return WalkState::Quit;
                    }
                    WalkState::Continue
                })
            });

        wanted
            .into_iter()
            .zip(found.iter())
            .filter(|(_, found)| found.load(Ordering::Relaxed))
            .map(|(language, _)| language.clone())
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn wanted(languages: &[&str]) -> HashSet<String> {
        languages.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn test_a_language_counts_once_it_has_one_file() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/lib.rs"), "").unwrap();

        let present =
            ServerConfig::default().languages_present(dir.path(), &wanted(&["rust", "python"]));

        assert_eq!(present, wanted(&["rust"]));
    }

    /// `.tsx` detects as `typescriptreact`, which the TypeScript server
    /// serves.
    #[test]
    fn test_a_react_variant_counts_for_its_base_language() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("App.tsx"), "").unwrap();

        let present =
            ServerConfig::default().languages_present(dir.path(), &wanted(&["typescript"]));

        assert_eq!(present, wanted(&["typescript"]));
    }

    #[test]
    fn test_hidden_directories_do_not_count() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join(".cache")).unwrap();
        fs::write(dir.path().join(".cache/tool.py"), "").unwrap();

        let present = ServerConfig::default().languages_present(dir.path(), &wanted(&["python"]));

        assert!(present.is_empty());
    }
}
