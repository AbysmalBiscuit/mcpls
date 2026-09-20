# Backend file watcher implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the backend one `notify` filesystem watcher per project so a file edited outside the agent reaches its language server on every host, and retire Claude Code's `FileChanged` hook and `watchPaths` reply.

**Architecture:** The watcher is a new source of paths for machinery that already exists. It places one non-recursive watch per directory the ignore rules keep, and every event it sees goes into `Sweeper::enqueue`, which already owns the path filtering, the `.gitignore` layering and the quiet-period debounce. One new rule travels with the paths: a path the watcher reported may not cold-start a language server, so a `git pull` in a mixed checkout cannot undo the lazy-spawn guarantee. A checkout inotify cannot cover is reported as unwatched rather than watched in part.

**Tech Stack:** Rust 2024, `notify` 8.2.0, `ignore` 0.4, `tokio`, `rustix` on unix. Tests run under `cargo nextest`.

**Spec:** `docs/superpowers/specs/2026-09-19-backend-file-watcher-design.md`

## Global Constraints

- Workspace MSRV is 1.88 (`Cargo.toml`, `rust-version`). `notify` 8.2.0's MSRV of 1.77 is under it.
- `notify` is pinned at `8.2.0`. Version 9 is at `rc.5` and must not be taken.
- `notify` is CC0-1.0. `deny.toml`'s `[licenses] allow` list must gain `"CC0-1.0"` or `cargo deny check licenses` fails CI (`.github/workflows/ci.yml:372`).
- Workspace lints deny `unsafe_code` and warn on `clippy::pedantic`, `clippy::nursery`, `clippy::unwrap_used` and `clippy::expect_used`. Test modules in this crate carry `#[allow(clippy::unwrap_used, clippy::expect_used)]` on the module; production code must not unwrap.
- `missing_docs` is warned workspace-wide. Every public item needs a doc comment.
- Every new dependency goes in `[workspace.dependencies]` alphabetically, and crates reference it as `name = { workspace = true }`.
- Commits go through `devrun task commit --arg commit_subject=... --arg commit_body=... --arg coauthors="Claude Opus 5 <noreply@anthropic.com>"`, which renders the Conventional Commits subject and appends the trailer. Do not call `git commit` directly.
- House test naming is `test_<what_is_true>`, and assertions carry a prose message saying what breaks if the assertion fails. Follow the surrounding style in the file being edited.

## Review Focus

These are the conditions the spec implies that no task's happy path exercises. Each has a test placed in the task that owns the code.

1. A directory created after startup, populated faster than the watch on its parent is placed, loses every file in it. Covered in Task 3, Step 9.
2. A path reported by both a hook and the watcher inside one quiet period silently loses its spawn if the watcher's origin wins the collision. Covered in Task 2, Step 7.
3. `/proc/mounts` entries carry octal escapes (`\040` for a space) and a nested mount point can be a longer prefix than the one that actually holds the root, so a naive first-match parse names the wrong filesystem. Covered in Task 4, Step 3.
4. `PathFilter::admits_directory` called on a configured root itself asks a `Gitignore` about its own base path, which `ignore` does not promise an answer for. Covered in Task 1, Step 7.
5. An older backend that predates the `watcher` status field must still parse against a newer doctor, the way the Stage 1 fields did. Covered in Task 5, Step 5.

---

### Task 1: The watchable directory set

`PathFilter` learns to answer about a directory, and a walk turns the configured roots into the set of directories to watch plus the files under them the sweeper would admit. No new dependency: this is `ignore`, which the crate already has.

**Files:**
- Modify: `crates/mcpls-core/src/hooks/filters.rs` (refactor `admits`, add `admits_directory`, add `watch_set`)
- Modify: `crates/mcpls-core/src/hooks/mod.rs` (export the new items)
- Test: `crates/mcpls-core/src/hooks/filters.rs` (the existing `mod tests`)

**Interfaces:**
- Consumes: `PathFilter::new(roots, extensions, registry)`, `BUILT_IN_IGNORES`, `read_gitignore`, all already in this file.
- Produces:
  - `PathFilter::admits_directory(&self, dir: &Path) -> bool`
  - `pub struct WatchSet { pub directories: BTreeSet<PathBuf>, pub files: Vec<PathBuf>, pub errors: Vec<String> }`
  - `pub fn watch_set(filter: &Arc<PathFilter>, roots: &[PathBuf]) -> WatchSet`

- [ ] **Step 1: Write the failing test for directory admission**

Add to `mod tests` in `crates/mcpls-core/src/hooks/filters.rs`:

```rust
#[test]
fn test_a_generated_directory_is_not_watchable() {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::create_dir_all(dir.path().join("target/debug")).expect("mkdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    let filter = filter_over(dir.path());

    assert!(
        filter.admits_directory(&dir.path().join("src")),
        "a source directory must carry a watch or nothing under it is ever heard about"
    );
    assert!(
        !filter.admits_directory(&dir.path().join("target")),
        "a watch under target/ is the descriptor exhaustion this design exists to avoid"
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p mcpls-core test_a_generated_directory_is_not_watchable`
Expected: compile error, `no method named admits_directory`.

- [ ] **Step 3: Factor the shared fold out of `admits`**

Replace the body of `admits` in `crates/mcpls-core/src/hooks/filters.rs:88-120` with a call to a new private method, keeping the existing doc comment on `admits` unchanged:

```rust
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
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo nextest run -p mcpls-core test_a_generated_directory_is_not_watchable`
Expected: PASS.

- [ ] **Step 5: Run the whole filters suite to verify the refactor changed nothing**

Run: `cargo nextest run -p mcpls-core hooks::filters`
Expected: every pre-existing `admits` test still passes. If `test_a_nested_root_inside_an_ignored_subtree_is_admitted` fails, the `path == deepest` short circuit was placed after the fold instead of before it.

- [ ] **Step 6: Commit**

```bash
git add crates/mcpls-core/src/hooks/filters.rs
devrun task commit \
  --arg commit_subject="feat(hooks): ask the path filter about directories" \
  --arg commit_body="The watcher places one watch per directory and needs the same ignore ruling admits already makes about files. One fold answers both, so the watch set cannot drift from the admitted set." \
  --arg coauthors="Claude Opus 5 <noreply@anthropic.com>"
```

- [ ] **Step 7: Write the failing tests for the walk**

Add to `mod tests`:

```rust
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
            !set.directories.iter().any(|d| d.starts_with(dir.path().join("target"))),
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
```

- [ ] **Step 8: Run them to verify they fail**

Run: `cargo nextest run -p mcpls-core hooks::filters::tests::test_the_watch_set`
Expected: compile error, `cannot find function watch_set`.

- [ ] **Step 9: Implement the walk**

Add to `crates/mcpls-core/src/hooks/filters.rs`, replacing the `WatchPaths` struct and the `watch_paths` function at lines 169-224 entirely:

```rust
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
    /// Used when an event overflow forces a full sweep, where the paths
    /// on the events cannot be trusted and the walk is the only honest
    /// answer to what may have changed.
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
/// read, making the watch set narrower than the admitted set, which is
/// the silent-loss direction.
#[must_use]
pub fn watch_set(filter: &Arc<PathFilter>, roots: &[PathBuf]) -> WatchSet {
    let mut result = WatchSet::default();
    for root in roots {
        if root.is_file() {
            result
                .errors
                .push(format!("{}: project root is not a directory", root.display()));
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
```

Add `use std::collections::BTreeSet;` to the imports at the top of the file, and update the module doc comment's second paragraph, which describes `watch_paths`, to describe `watch_set` instead:

```rust
//! Deciding which changed paths are worth acting on.
//!
//! Claude Code's hooks report every path a tool touched, with no ignore
//! list of their own: a `cargo build` or an `npm install` can report
//! thousands of paths under `target/` or `node_modules/`, and the document
//! tracker has a ceiling. `PathFilter` is what keeps that flood out, and
//! `watch_set` is what keeps the backend's own watcher from ever placing a
//! descriptor inside one of those trees to begin with.
```

- [ ] **Step 10: Delete the tests that belonged to `watch_paths`**

Remove `test_watch_paths_drops_the_built_in_floor_with_no_gitignore_present` and `test_watch_paths_names_children_rather_than_the_root` from `mod tests`. Rewrite `test_a_negated_path_is_admitted_and_still_not_watched`, whose subject survives, against the new function:

```rust
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
```

- [ ] **Step 11: Update the module export**

In `crates/mcpls-core/src/hooks/mod.rs`, replace the filters re-export:

```rust
pub use filters::{PathFilter, WatchSet, watch_set};
```

- [ ] **Step 12: Run the suite**

Run: `cargo nextest run -p mcpls-core hooks::filters`
Expected: PASS. `cargo check -p mcpls-cli` will now fail on `watch_paths`; that is Task 6's work and is expected here. To keep this task independently verifiable, run `cargo nextest run -p mcpls-core hooks::` rather than the workspace.

- [ ] **Step 13: Commit**

```bash
git add crates/mcpls-core/src/hooks/filters.rs crates/mcpls-core/src/hooks/mod.rs
devrun task commit \
  --arg commit_subject="feat(hooks): walk the project for the directories to watch" \
  --arg commit_body="watch_paths scanned the top level once and was blind to every directory created afterwards. watch_set walks the whole tree under each configured root, taking every ignore ruling from PathFilter so the watch set and the admitted set cannot disagree." \
  --arg coauthors="Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: A pending path remembers where it came from

The sweeper starts a language server for any routable file that changed (`sweep.rs:205-226`). That is right for an agent's edit and wrong for a `git pull`. The pending set learns to carry an origin, and only hook-origin paths may spawn.

**Files:**
- Modify: `crates/mcpls-core/src/hooks/sweep.rs`
- Modify: `crates/mcpls-core/src/hooks/mod.rs` (export `Origin`)
- Test: `crates/mcpls-core/src/hooks/sweep.rs` (the existing `mod tests`)

**Interfaces:**
- Consumes: `PathFilter::admits`, `PathFilter::routable_extension`, `Translator::lifecycle_of`, `Translator::ensure_server`, all unchanged.
- Produces:
  - `pub enum Origin { Hook, Watcher }`, deriving `Debug, Clone, Copy, PartialEq, Eq`
  - `Sweeper::enqueue_from(&self, paths: &[PathBuf], origin: Origin) -> usize`
  - `Sweeper::enqueue(&self, paths: &[PathBuf]) -> usize`, kept, delegating with `Origin::Hook`
  - `Sweeper::filter(&self) -> Arc<PathFilter>`

- [ ] **Step 1: Write the failing test that a watcher path does not spawn**

Add to `mod tests` in `crates/mcpls-core/src/hooks/sweep.rs`. `idle_sweeper()` at `sweep.rs:458` is the fixture for exactly this: a sweeper whose `rust` server is applicable and has never been triggered, which is what a lazy backend looks like before the agent touches the language. `write` puts a file at a path under its workspace, and `sweep_now` drives one sweep.

```rust
    #[tokio::test]
    async fn test_a_watcher_path_does_not_start_a_language_server() {
        let sweeper = idle_sweeper();
        let path = sweeper.write("outside.rs");

        sweeper.enqueue_from(std::slice::from_ref(&path), Origin::Watcher);
        sweeper.sweep_now().await;

        assert_eq!(
            sweeper.translator.lifecycle_of(&ServerId::from(SERVER)),
            Some(ServerLifecycle::Idle),
            "a git pull in a mixed checkout must not start rust-analyzer for \
             a session that opens only TypeScript, which is the whole point \
             of the lazy spawn design"
        );
    }
```

- [ ] **Step 2: Write the failing test that a hook path still spawns**

```rust
    #[tokio::test]
    async fn test_a_hook_path_still_starts_a_language_server() {
        let sweeper = idle_sweeper();
        let path = sweeper.write("edited.rs");

        sweeper.enqueue_from(std::slice::from_ref(&path), Origin::Hook);
        sweeper.sweep_now().await;

        assert_ne!(
            sweeper.translator.lifecycle_of(&ServerId::from(SERVER)),
            Some(ServerLifecycle::Idle),
            "an agent's own edit is exactly the signal the lazy spawn design \
             starts a server on, and the origin rule must not take it away"
        );
    }
```

- [ ] **Step 3: Run both to verify they fail**

Run: `cargo nextest run -p mcpls-core hooks::sweep::tests::test_a_watcher_path hooks::sweep::tests::test_a_hook_path`
Expected: compile error, `cannot find value Origin` and `no method named enqueue_from`.

- [ ] **Step 4: Add the origin type and widen the pending set**

In `crates/mcpls-core/src/hooks/sweep.rs`, add near `SweepKind`:

```rust
/// Where a pending path came from, which decides whether it may start a
/// language server that is not running.
///
/// A hook reports what the agent itself did, which is the signal the lazy
/// spawn design starts a server on. The watcher reports what the disk did,
/// which includes a `git pull`, a background formatter and another editor,
/// none of which is this session asking for a language it does not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A host hook reported an edit the agent made.
    Hook,
    /// The project watcher saw the path change on disk.
    Watcher,
}
```

Change the `pending` field's type and doc comment:

```rust
    /// Paths admitted since the last sweep, each with where it came from.
    ///
    /// Holds one entry per distinct admitted path between two sweeps, and
    /// a sweep empties it, so its size follows how much actually changed
    /// rather than how many events arrived: an unlink and an add for the
    /// same save are one entry.
    pending: StdMutex<HashMap<PathBuf, Origin>>,
```

Change `filter: PathFilter` to `filter: Arc<PathFilter>`, and in `Sweeper::new` wrap the argument: `filter: Arc::new(filter)`. The parameter keeps its `PathFilter` type so no test call site changes. Add `use std::collections::HashMap;` and keep `HashSet` only if still used.

- [ ] **Step 5: Replace `enqueue` and expose the filter**

```rust
    /// Queue paths a host hook reported. Returns how many survived the
    /// filters.
    pub fn enqueue(&self, paths: &[PathBuf]) -> usize {
        self.enqueue_from(paths, Origin::Hook)
    }

    /// Queue paths from `origin`. Returns how many survived the filters.
    ///
    /// Answers from the path and the configured roots alone -- no
    /// filesystem call, no lock held longer than an insert -- because this
    /// runs on the hook connection, which has a deadline to answer within,
    /// and on the watcher's event loop, which must not block the thread
    /// `notify` hands its events to.
    ///
    /// A path already pending from a hook keeps that origin when the
    /// watcher reports it too, which it will: the agent's own write is a
    /// disk event like any other. Letting the watcher overwrite it would
    /// take away the spawn the agent's edit earned.
    pub fn enqueue_from(&self, paths: &[PathBuf], origin: Origin) -> usize {
        let mut admitted = 0;
        for path in paths {
            if !self.filter.admits(path) {
                continue;
            }
            admitted += 1;
            let mut pending = lock_std(&self.pending);
            pending
                .entry(path.clone())
                .and_modify(|held| {
                    if origin == Origin::Hook {
                        *held = Origin::Hook;
                    }
                })
                .or_insert(origin);
        }
        if admitted > 0 {
            *lock_std(&self.last_arrival) = Some(Instant::now());
        }
        admitted
    }

    /// The filter this sweeper admits paths through, so the project
    /// watcher can place its watches by the same ruling rather than a
    /// second one built beside it.
    pub(crate) fn filter(&self) -> Arc<PathFilter> {
        Arc::clone(&self.filter)
    }
```

- [ ] **Step 6: Carry the origin through the sweep**

In `sweep`, change the drain and thread the origins into `ensure_servers_for_edits`:

```rust
        let pending: HashMap<PathBuf, Origin> = lock_std(&self.pending).drain().collect();
        if pending.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = pending.keys().cloned().collect();
```

Leave the rest of the `for path in paths` loop that builds `kinds`, `settle` and `untracked` exactly as it is. Change the call and the re-queue:

```rust
        let waiting = self.ensure_servers_for_edits(&kinds, &pending).await;

        if !waiting.is_empty() {
            let mut requeued = lock_std(&self.pending);
            for path in &waiting {
                // Only a hook-origin path can be waiting, since only a
                // hook-origin path is offered a spawn.
                requeued.insert(path.clone(), Origin::Hook);
            }
            drop(requeued);
```

And change the signature and the guard:

```rust
    async fn ensure_servers_for_edits(
        &self,
        kinds: &[(PathBuf, SweepKind)],
        origins: &HashMap<PathBuf, Origin>,
    ) -> Vec<PathBuf> {
        let mut waiting = Vec::new();
        for (path, kind) in kinds {
            if *kind == SweepKind::Deleted || !self.filter.routable_extension(path) {
                continue;
            }
            if origins.get(path) != Some(&Origin::Hook) {
                continue;
            }
            let Some(id) = self.translator.server_for_path(path) else {
                continue;
            };
```

Leave the rest of the function body unchanged.

- [ ] **Step 7: Write the failing test for the collision rule**

This is Review Focus item 2.

```rust
    #[tokio::test]
    async fn test_a_hook_origin_survives_a_watcher_report_of_the_same_path() {
        let sweeper = idle_sweeper();
        let path = sweeper.write("edited.rs");

        sweeper.enqueue_from(std::slice::from_ref(&path), Origin::Hook);
        sweeper.enqueue_from(std::slice::from_ref(&path), Origin::Watcher);
        sweeper.sweep_now().await;

        assert_ne!(
            sweeper.translator.lifecycle_of(&ServerId::from(SERVER)),
            Some(ServerLifecycle::Idle),
            "the agent's own write reaches the watcher too, so a watcher \
             report landing second must not cancel the spawn the edit earned"
        );
    }
```

- [ ] **Step 8: Fix `pending_paths` and run the suite**

The test-only accessor at `sweep.rs:200-203` returns a `HashSet`. Change it to return the keys, so its callers are unaffected:

```rust
    #[cfg(test)]
    pub(crate) fn pending_paths(&self) -> HashSet<PathBuf> {
        lock_std(&self.pending).keys().cloned().collect()
    }
```

Run: `cargo nextest run -p mcpls-core hooks::sweep`
Expected: PASS, every pre-existing sweeper test included.

- [ ] **Step 9: Export the origin and commit**

In `crates/mcpls-core/src/hooks/mod.rs`:

```rust
pub use sweep::{Origin, SweepKind, Sweeper};
```

```bash
git add crates/mcpls-core/src/hooks/sweep.rs crates/mcpls-core/src/hooks/mod.rs
devrun task commit \
  --arg commit_subject="feat(hooks): record where a pending path came from" \
  --arg commit_body="The sweeper starts a language server for any routable file that changed, which is right for an agent's edit and wrong for a git pull. A pending path now carries its origin and only a hook-origin path is offered a spawn. A hook origin survives a later watcher report of the same path, since the agent's own write reaches the watcher too." \
  --arg coauthors="Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: The watcher

`notify` enters the workspace, the watcher module is written, and `Runtime::start` builds one beside the sweeper.

**Files:**
- Modify: `Cargo.toml` (workspace dependency)
- Modify: `crates/mcpls-core/Cargo.toml`
- Modify: `deny.toml` (licence allowance)
- Create: `crates/mcpls-core/src/hooks/watcher.rs`
- Modify: `crates/mcpls-core/src/hooks/mod.rs`
- Modify: `crates/mcpls-core/src/lib.rs:698-710`

**Interfaces:**
- Consumes: `WatchSet`, `watch_set`, `PathFilter::admits_directory` from Task 1; `Origin`, `Sweeper::enqueue_from`, `Sweeper::filter` from Task 2.
- Produces:
  - `pub enum WatchState { Watching { directories: usize }, Unwatched { reason: String } }`
  - `pub struct ProjectWatcher`
  - `ProjectWatcher::start(roots: Arc<[PathBuf]>, sweeper: &Arc<Sweeper>, cancel: watch::Receiver<bool>) -> Arc<ProjectWatcher>`
  - `ProjectWatcher::state(&self) -> WatchState`

- [ ] **Step 1: Add the dependency and the licence allowance**

In `Cargo.toml`, add to `[workspace.dependencies]` in alphabetical position, between `mcpls-core` and `predicates`:

```toml
notify = "8.2.0"
```

In `crates/mcpls-core/Cargo.toml`, add to `[dependencies]` after `lsp-types`:

```toml
notify = { workspace = true }
```

In `deny.toml`, add `"CC0-1.0"` to the `[licenses] allow` list, after `"Apache-2.0 WITH LLVM-exception"`:

```toml
allow = [
    "MIT",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "CC0-1.0",
    "ISC",
    "Zlib",
    "MPL-2.0",
    "Unicode-3.0",
    "Unicode-DFS-2016",
]
```

- [ ] **Step 2: Verify the licence check passes**

Run: `cargo deny check licenses`
Expected: `licenses ok`. If it names a transitive dependency of `notify` under another licence, add that licence only if it is one of MIT, Apache-2.0, BSD or ISC, and stop and report anything else rather than widening the list further.

- [ ] **Step 3: Write the failing test and its fixture**

Add a test module at the bottom of `crates/mcpls-core/src/hooks/watcher.rs`. The fixture mirrors `sweep.rs`'s `sweeper_over` rather than importing it, because that module's helpers are private to it:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::bridge::Translator;

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
}
```

`pending_paths` is `#[cfg(test)]` and `pub(crate)` on `Sweeper`, so it is reachable from here without widening anything.

- [ ] **Step 4: Run it to verify it fails**

Run: `cargo nextest run -p mcpls-core hooks::watcher`
Expected: compile error, `cannot find module watcher`.

- [ ] **Step 5: Write the watcher module**

Create `crates/mcpls-core/src/hooks/watcher.rs`:

```rust
//! One filesystem watcher per project, feeding the sweeper.
//!
//! mcpls tells every language server it will watch the disk on the
//! server's behalf (`crate::lsp::lifecycle`'s `initialize` advertises
//! `didChangeWatchedFiles` dynamic registration). This is what makes that
//! true on every host, rather than only where the host offers a
//! file-changed hook and only for the directories that existed when the
//! session started.
//!
//! Everything downstream already exists. The watcher places watches and
//! hands paths to [`Sweeper::enqueue_from`]; the filtering, the debounce,
//! the resync and the notification are the sweeper's and the translator's,
//! unchanged.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, watch};

use crate::bridge::lock_std;
use crate::hooks::filters::{PathFilter, watch_set};
use crate::hooks::sweep::{Origin, Sweeper};

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
    /// own edits and a session that degrades to today's coverage is worth
    /// more than one that refuses to start.
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

        let mut watched = BTreeSet::new();
        for directory in &set.directories {
            match native.watch(directory, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    watched.insert(directory.clone());
                }
                Err(error) if is_watch_limit(&error) => {
                    let reason = format!(
                        "the kernel's watch limit was reached after {} directories; \
                         raise fs.inotify.max_user_watches to watch this checkout",
                        watched.len()
                    );
                    tracing::warn!(%reason, "not watching this checkout for changes");
                    // Dropping the watcher releases every descriptor it
                    // took. A checkout watched in part is worse than one
                    // watched not at all: which part depends on walk
                    // order, so the behaviour is not reproducible.
                    drop(native);
                    *lock_std(&watcher.state) = WatchState::Unwatched { reason };
                    return watcher;
                }
                Err(error) => {
                    tracing::warn!(%error, directory = %directory.display(), "could not watch a directory");
                }
            }
        }

        *lock_std(&watcher.state) = WatchState::Watching {
            directories: watched.len(),
        };
        tokio::spawn(run(
            native,
            watched,
            rx,
            cancel,
            Arc::clone(&roots),
            Arc::clone(sweeper),
        ));
        watcher
    }

    /// What this watcher is doing, for the doctor.
    #[must_use]
    pub fn state(&self) -> WatchState {
        lock_std(&self.state).clone()
    }
}

/// Drain events until cancelled, placing watches on directories that
/// appear and handing every path to the sweeper.
async fn run(
    mut native: RecommendedWatcher,
    mut watched: BTreeSet<PathBuf>,
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
                        rescan(&mut native, &mut watched, &roots, &sweeper);
                    }
                    Ok(event) => {
                        for path in &event.paths {
                            if path.is_dir() {
                                adopt(&mut native, &mut watched, &sweeper, path);
                            }
                        }
                        sweeper.enqueue_from(&event.paths, Origin::Watcher);
                    }
                    Err(error) => tracing::warn!(%error, "a filesystem watch reported an error"),
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

/// Watch a directory that appeared after startup, and sweep what is
/// already inside it.
///
/// A `git checkout` can populate a subtree faster than the watch on its
/// parent can be placed, so the files already there would otherwise never
/// produce an event at all.
fn adopt(
    native: &mut RecommendedWatcher,
    watched: &mut BTreeSet<PathBuf>,
    sweeper: &Arc<Sweeper>,
    directory: &Path,
) {
    let filter = sweeper.filter();
    if watched.contains(directory) || !filter.admits_directory(directory) {
        return;
    }
    let set = watch_set(&filter, &[directory.to_path_buf()]);
    for new in &set.directories {
        if watched.contains(new) {
            continue;
        }
        match native.watch(new, RecursiveMode::NonRecursive) {
            Ok(()) => {
                watched.insert(new.clone());
            }
            Err(error) => {
                tracing::warn!(%error, directory = %new.display(), "could not watch a new directory");
            }
        }
    }
    sweeper.enqueue_from(&set.files, Origin::Watcher);
}

/// Re-walk every root, place watches on directories that appeared, and
/// enqueue every admitted file.
fn rescan(
    native: &mut RecommendedWatcher,
    watched: &mut BTreeSet<PathBuf>,
    roots: &Arc<[PathBuf]>,
    sweeper: &Arc<Sweeper>,
) {
    let filter = sweeper.filter();
    let set = watch_set(&filter, roots);
    for gone in watched.difference(&set.directories).cloned().collect::<Vec<_>>() {
        let _ = native.unwatch(&gone);
        watched.remove(&gone);
    }
    for new in &set.directories {
        if watched.contains(new) {
            continue;
        }
        if native.watch(new, RecursiveMode::NonRecursive).is_ok() {
            watched.insert(new.clone());
        }
    }
    sweeper.enqueue_from(&set.files, Origin::Watcher);
}
```

Leave `unwatchable_reason` and `is_watch_limit` as `todo!()`-free stubs for now, so this task compiles on its own:

```rust
/// Why these roots cannot be watched, or `None` when they can.
///
/// Filled in by the mount-type check in the next task; until then every
/// checkout is treated as watchable, which is what the hook-era code
/// assumed too.
fn unwatchable_reason(_roots: &[PathBuf]) -> Option<String> {
    None
}

/// Whether a watch failed because the kernel has no descriptors left.
fn is_watch_limit(_error: &notify::Error) -> bool {
    false
}
```

- [ ] **Step 6: Register the module**

In `crates/mcpls-core/src/hooks/mod.rs`, add `pub mod watcher;` after `pub mod sweep;` and add the re-export:

```rust
pub use watcher::{ProjectWatcher, WatchState};
```

- [ ] **Step 7: Run the test to verify it passes**

Run: `cargo nextest run -p mcpls-core hooks::watcher`
Expected: PASS.

- [ ] **Step 8: Build it in `Runtime::start`**

In `crates/mcpls-core/src/lib.rs`, immediately after the `tokio::spawn(Arc::clone(&sweeper).run(cancel_rx.clone()));` line at 708:

```rust
        // One watcher per project, built here so the shared backend and an
        // in-process `--no-backend` run get the same one: both reach this
        // function.
        let watcher = hooks::ProjectWatcher::start(
            Arc::clone(&workspace_roots_snapshot),
            &sweeper,
            cancel_rx.clone(),
        );
```

Add `watcher: Arc<hooks::ProjectWatcher>` to the `Runtime` struct beside `pub(crate) sweeper` at line 574, and to the struct literal beside `sweeper` at line 764.

- [ ] **Step 9: Write the failing test for a directory created after startup**

This is Review Focus item 1. Add to `watcher.rs`'s `mod tests`:

```rust
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
```

The file is written immediately after the directory, so this also covers the race `adopt` exists for: if the write lands before the watch on `late/` is placed, only the walk `adopt` runs will find it.

- [ ] **Step 10: Run the suite and commit**

Run: `cargo nextest run -p mcpls-core hooks::`
Expected: PASS.

```bash
git add Cargo.toml Cargo.lock deny.toml crates/mcpls-core/Cargo.toml \
  crates/mcpls-core/src/hooks/watcher.rs crates/mcpls-core/src/hooks/mod.rs \
  crates/mcpls-core/src/lib.rs
devrun task commit \
  --arg commit_subject="feat(hooks): watch the project from the backend" \
  --arg commit_body="One notify watcher per project, one non-recursive watch per directory the ignore rules keep, feeding the sweeper that already owns the filtering and the debounce. A directory created after startup is adopted and the files already inside it are swept, since a git checkout can populate a subtree faster than the watch on its parent is placed. notify is CC0-1.0, so deny.toml gains that licence." \
  --arg coauthors="Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: Checkouts that cannot be watched

Fill in the two stubs from Task 3. inotify delivers nothing for a Windows drive under WSL2 and delivers unreliably over network filesystems, and a watcher that covered part of a checkout would look like one that covered all of it.

**Files:**
- Modify: `crates/mcpls-core/src/hooks/watcher.rs`

**Interfaces:**
- Consumes: `WatchState::Unwatched` from Task 3.
- Produces: `fn mount_type_for(mounts: &str, path: &Path) -> Option<(String, String)>`, private, tested directly.

- [ ] **Step 1: Write the failing test for mount detection**

Add to `watcher.rs`'s `mod tests`:

```rust
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
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p mcpls-core hooks::watcher::tests::test_the_longest_mount hooks::watcher::tests::test_an_octal hooks::watcher::tests::test_a_windows_drive`
Expected: compile error, `cannot find function mount_type_for`.

- [ ] **Step 3: Implement the mount check**

Replace the `unwatchable_reason` stub in `crates/mcpls-core/src/hooks/watcher.rs`:

```rust
/// Filesystems inotify does not report changes on.
///
/// `9p`, `drvfs` and `virtiofs` are how WSL2 and some VM setups present a
/// host directory; the network filesystems deliver local events only. A
/// path prefix such as `/mnt/c` would be a guess about one vendor's
/// layout. The mount type is the fact.
const UNWATCHABLE_FILESYSTEMS: &[&str] =
    &["9p", "drvfs", "virtiofs", "cifs", "smb3", "nfs", "nfs4"];

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

/// [`unwatchable_reason`] for one root against a `/proc/mounts` body,
/// so the rule can be tested without one.
fn reason_for_mounts(mounts: &str, root: &Path) -> Option<String> {
    let (point, fstype) = mount_type_for(mounts, root)?;
    UNWATCHABLE_FILESYSTEMS
        .contains(&fstype.as_str())
        .then(|| {
            format!(
                "{} is on a {fstype} mount at {point}, where inotify reports no changes",
                root.display()
            )
        })
}

/// The mount point holding `path` and its filesystem type.
///
/// The longest matching mount point, not the first: `/` is a prefix of
/// every path, so a first match would call every checkout on the machine
/// whatever the root filesystem is. Mount points are written with octal
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

/// Decode the `\\040`-style escapes the kernel writes into `/proc/mounts`
/// for space, tab, newline and backslash.
fn decode_octal_escapes(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let escape = rest.get(at + 1..at + 4);
        match escape.and_then(|digits| u8::from_str_radix(digits, 8).ok()) {
            Some(byte) => {
                out.push(char::from(byte));
                rest = &rest[at + 4..];
            }
            None => {
                out.push('\\');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}
```

- [ ] **Step 4: Run the mount tests to verify they pass**

Run: `cargo nextest run -p mcpls-core hooks::watcher`
Expected: PASS.

- [ ] **Step 5: Implement the watch-limit check**

Replace the `is_watch_limit` stub:

```rust
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
```

- [ ] **Step 6: Run the suite and commit**

Run: `cargo nextest run -p mcpls-core hooks::` then `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: PASS, no warnings.

```bash
git add crates/mcpls-core/src/hooks/watcher.rs
devrun task commit \
  --arg commit_subject="feat(hooks): refuse to watch a checkout inotify cannot cover" \
  --arg commit_body="inotify reports nothing for a Windows drive under WSL2 and reports unreliably over network filesystems, and ENOSPC from a watch means the per-user limit. Either way the backend reports the checkout unwatched rather than watching part of it: which part would depend on walk order, so a partly watched backend is not reproducible. The hooks still deliver the agent's own edits." \
  --arg coauthors="Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: The watcher's state on the wire and on the doctor

`watch_scan_line` computed its answer in the hook CLI, which it could while the watching was the host's. The watcher is in the backend now, so the state crosses the socket.

**Files:**
- Modify: `crates/mcpls-core/src/hooks/protocol.rs`
- Modify: `crates/mcpls-core/src/hooks/service.rs:61-72` (`StatusExtras`)
- Modify: `crates/mcpls-core/src/lib.rs:780` (the `StatusSource` closure)
- Modify: `crates/mcpls-cli/src/hook.rs` (the doctor's rendering)

**Interfaces:**
- Consumes: `WatchState` from Task 3.
- Produces: `pub struct WatcherStatus { pub watching: bool, pub directories: usize, pub unwatched_reason: Option<String> }` in `protocol.rs`, on `Response::Status` as `watcher`.

- [ ] **Step 1: Write the failing wire test**

In `crates/mcpls-core/src/hooks/protocol.rs`'s `mod tests`, extend the literal in `test_the_status_response_pins_the_wire_shape` with the new field and add its value to the struct literal:

```rust
        let literal = r#"{"op":"status","hash":"abc123","socket":"mcpls.sock","pid":42,"owner":true,"root":"/work","hooks_seen":7,"version":"0.3.9","uptime_ms":61000,"sessions":["s1","connection-4"],"servers":[{"id":"rust","state":"running"},{"id":"lua","state":"not_installed"}],"config_fingerprint":"00000000000000ff","watcher":{"watching":true,"directories":56,"unwatched_reason":null}}"#;
```

and in the `Response::Status { .. }` value it compares against:

```rust
            watcher: WatcherStatus {
                watching: true,
                directories: 56,
                unwatched_reason: None,
            },
```

Every other `Response::Status` literal in this module's tests needs the same field added to its struct value; the ones built from JSON literals that omit the key rely on `#[serde(default)]` and stay as they are.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p mcpls-core hooks::protocol`
Expected: compile error, `cannot find type WatcherStatus`.

- [ ] **Step 3: Add the type and the field**

In `crates/mcpls-core/src/hooks/protocol.rs`, add after `ServerStatus`:

```rust
/// What the backend's filesystem watcher is doing.
///
/// A struct rather than a rendered line, following [`ServerStatus`], so
/// later detail lands on the type instead of growing a parallel field on
/// the response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatcherStatus {
    /// Whether a watcher is running at all.
    pub watching: bool,
    /// How many directories carry a watch.
    pub directories: usize,
    /// Why no watcher runs, absent while one does.
    #[serde(default)]
    pub unwatched_reason: Option<String>,
}
```

and to the `Response::Status` variant, after `config_fingerprint`:

```rust
        /// What the backend's filesystem watcher is doing.
        #[serde(default)]
        watcher: WatcherStatus,
```

- [ ] **Step 4: Carry it from the runtime to the response**

In `crates/mcpls-core/src/hooks/service.rs`, add to `StatusExtras` after `config_fingerprint`:

```rust
    /// What the filesystem watcher is doing.
    pub watcher: WatcherStatus,
```

and set it wherever `StatusExtras` is built into a `Response::Status` in that file. In `crates/mcpls-core/src/lib.rs:780`, inside the `StatusSource` closure, add:

```rust
            watcher: match runtime_watcher.state() {
                hooks::WatchState::Watching { directories } => hooks::WatcherStatus {
                    watching: true,
                    directories,
                    unwatched_reason: None,
                },
                hooks::WatchState::Unwatched { reason } => hooks::WatcherStatus {
                    watching: false,
                    directories: 0,
                    unwatched_reason: Some(reason),
                },
            },
```

capturing an `Arc<ProjectWatcher>` clone as `runtime_watcher` where the closure is built. Export `WatcherStatus` from `crates/mcpls-core/src/hooks/mod.rs` alongside the other protocol types:

```rust
pub use protocol::{ChangeEvent, Request, Response, WatcherStatus};
```

- [ ] **Step 5: Write the failing test that an older backend still parses**

This is Review Focus item 5. Extend `test_an_older_status_parses_with_empty_backend_fields` in `protocol.rs`:

```rust
        let Response::Status {
            sessions,
            version,
            watcher,
            ..
        } = serde_json::from_str::<Response>(literal).expect("deserialize")
        else {
            panic!("a status");
        };
        assert!(sessions.is_empty());
        assert!(version.is_empty());
        assert!(
            !watcher.watching,
            "a backend from before this field existed reports no watcher, \
             which is exactly what it has; a doctor that failed to parse it \
             would report nothing at all instead"
        );
```

- [ ] **Step 6: Replace the doctor's line**

In `crates/mcpls-cli/src/hook.rs`, add `WatcherStatus` to the `mcpls_core::hooks` import list at line 21, delete `watch_scan_line` at lines 223-237, and add:

```rust
/// The doctor's line for the backend's filesystem watcher.
fn watcher_line(watcher: &WatcherStatus) -> String {
    match (&watcher.unwatched_reason, watcher.watching) {
        (Some(reason), _) => format!("watcher: not watching; {reason}"),
        (None, true) => format!("watcher: {} directories watched", watcher.directories),
        (None, false) => {
            "watcher: not watching; this backend predates the watcher".to_string()
        }
    }
}
```

In `doctor_scanning` at lines 308-333, destructure `watcher` from the `Response::Status` pattern and push its line after `servers_line`:

```rust
            lines.push(servers_line(&servers));
            lines.push(watcher_line(&watcher));
            lines.push(config_line(&config_fingerprint, local_fingerprint));
```

Find every other call site of `watch_scan_line` in this file and remove it; the doctor is the only caller.

- [ ] **Step 7: Run the suite and commit**

Run: `cargo nextest run -p mcpls-core hooks::protocol && cargo nextest run -p mcpls-cli`
Expected: the core protocol tests pass. `mcpls-cli` still fails on `watch_paths` and the `SessionStart` arm, which Task 6 removes.

```bash
git add crates/mcpls-core/src/hooks/protocol.rs crates/mcpls-core/src/hooks/service.rs \
  crates/mcpls-core/src/hooks/mod.rs crates/mcpls-core/src/lib.rs crates/mcpls-cli/src/hook.rs
devrun task commit \
  --arg commit_subject="feat(hooks): report the watcher's state on the doctor" \
  --arg commit_body="The watch scan line was computed locally while the watching was the host's. The watcher lives in the backend now, so its state crosses the socket on the status response, defaulted so an older backend still parses against a newer doctor." \
  --arg coauthors="Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: Retire `FileChanged` and the `watchPaths` reply

The watcher works, so the host-side machinery it replaces goes. This lands last because a `FileChanged` removed before the watcher works loses coverage, and a watcher added before the removal delivers every Claude Code edit twice.

**Files:**
- Modify: `plugin/hooks/hooks.json`
- Modify: `crates/mcpls-cli/src/hook.rs`
- Modify: `crates/mcpls-cli/tests/plugin_manifests.rs`
- Modify: `crates/mcpls-cli/tests/cli_integration.rs`
- Modify: `crates/mcpls-core/src/hooks/protocol.rs` (one doc comment)
- Modify: `crates/mcpls-core/src/config/mod.rs:287`
- Modify: `schema/mcpls-config.json`
- Modify: `crates/mcpls-cli/src/hook/codex.rs` (one doc comment)

**Interfaces:**
- Consumes: everything from Tasks 1 to 5.
- Produces: nothing new. This task only removes.

- [ ] **Step 1: Write the failing manifest test**

In `crates/mcpls-cli/tests/plugin_manifests.rs`, change the expected Claude hook-event list near line 84 to drop `"FileChanged"` while keeping `"SessionStart"`, which now runs `bootstrap-binaries` rather than `mcpls hook`:

```rust
            [
                "PostToolBatch",
                "SessionEnd",
                "SessionStart",
                "UserPromptSubmit",
            ],
```

Match the existing ordering and shape of the assertion rather than replacing it wholesale; only the `"FileChanged"` entry leaves.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p mcpls-cli plugin_manifests`
Expected: FAIL, the manifest still registers `FileChanged`.

- [ ] **Step 3: Remove the hook registration**

In `plugin/hooks/hooks.json`, delete the `"FileChanged"` line. `"SessionStart"` stays exactly as it is: it runs `bootstrap-binaries`, which is what puts `mcpls` on `PATH`, and is unrelated to watching.

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo nextest run -p mcpls-cli plugin_manifests`
Expected: PASS.

- [ ] **Step 5: Remove the dispatch arms and the reply**

In `crates/mcpls-cli/src/hook.rs`:

- Delete the `"SessionStart" => Ok(session_start_output(&watch_paths(project_dir))),` arm at line 109 and its preceding comment. A `SessionStart` payload now falls through to the catch-all `_ => Ok(String::new())`, which is correct: the plugin no longer sends one to `mcpls hook`.
- Delete the entire `"FileChanged" => { ... }` arm at lines 111-128.
- Delete `session_start_output` at lines 201-217.
- Delete the `WatchPaths` and `watch_paths` imports at line 21.
- In `dispatch_payload`'s doc comment at lines 90-91, delete the sentence about `SessionStart` working without an identity and reporting local watch-scan failures.
- In `doctor`'s doc comment near line 264, delete the sentence "`SessionStart` separately warns when its watch-path scan is incomplete."
- Delete the tests that cover the removed behaviour: `test_session_start_returns_watch_paths_without_a_socket` at 1684, the `SessionStart` assertions at 1604-1616 and 3595-3604, and every `FileChanged` test at 1746, 1777 and 1802. Delete the comment at 425 and 2486 about `SessionStart` never touching the socket.

- [ ] **Step 6: Remove the CLI integration tests**

In `crates/mcpls-cli/tests/cli_integration.rs`, delete `test_hook_session_start_emits_absolute_watch_paths` at 731 and every other test asserting on `hookSpecificOutput.watchPaths` (lines 776, 847, 855, 884, 932, 953, 1232). Read each one before deleting: any that also asserts something still true about another hook event keeps that part.

- [ ] **Step 7: Remove the documentation references**

- `crates/mcpls-core/src/hooks/protocol.rs`, the `Request::Changed` doc at line 23: change "Sent by the `FileChanged` hook for one path, and by the `PostToolBatch` hook for a batch's paths ahead of its `flush`." to "Sent by the `PostToolBatch` hook for a batch's paths ahead of its `flush`."
- `crates/mcpls-core/src/config/mod.rs:287`: change "With this off, no listener binds, but `SessionStart` still performs its local watch scan." to "With this off, no listener binds and the agent's own edits reach no language server, though the backend's watcher still reports what changes on disk."
- `crates/mcpls-cli/src/hook/codex.rs:7`: the comment says "nothing here emits Claude Code's `watchPaths`". Delete that clause, since nothing anywhere emits it now.
- Search `plugin/skills/setup-mcpls/references/` for `watchPaths`, `FileChanged` and `watch scan`, and update each to describe the backend watcher and its doctor line. `troubleshooting.md` carries the `.gitignore` negation workaround, whose meaning is unchanged: the negated file is still admitted and its directory still carries no watch. Reword it to name the backend watcher rather than the host's.

- [ ] **Step 8: Regenerate the schema**

Run: `devrun task schema`
Expected: `schema/mcpls-config.json` line 125's description changes to match the new `HooksConfig` doc comment.

- [ ] **Step 9: Run the full verification**

Run: `devrun task verify`
Expected: formatting, clippy and every test pass. Then `cargo deny check licenses` and `cargo deny check advisories`.

- [ ] **Step 10: Commit**

```bash
git add plugin/hooks/hooks.json crates/mcpls-cli/src/hook.rs crates/mcpls-cli/src/hook/codex.rs \
  crates/mcpls-cli/tests/plugin_manifests.rs crates/mcpls-cli/tests/cli_integration.rs \
  crates/mcpls-core/src/hooks/protocol.rs crates/mcpls-core/src/config/mod.rs \
  schema/mcpls-config.json plugin/skills/setup-mcpls/references/
devrun task commit \
  --arg commit_subject="feat(plugin)!: retire FileChanged and the watchPaths reply" \
  --arg commit_body="The backend watches the project on every host now, so the Claude-only hook it replaces goes, along with the watchPaths reply that only ever named the top level. SessionStart stays registered: it bootstraps the binaries onto PATH and has nothing to do with watching. The changed operation stays, since PostToolBatch still sends it.

Closes #25" \
  --arg coauthors="Claude Opus 5 <noreply@anthropic.com>"
```

---

## Verification against the spec

After Task 6, walk the spec's Verification list and check each line. These need a running host rather than a unit test, so record the result rather than automating them:

- A file edited by a shell command under a directory that existed at startup, and under one created after it.
- A file matching a registered glob that no session has opened, on Claude Code and on Codex.
- An `apply_edit` through mcpls producing no `didChange`.
- A mixed checkout where a `git pull` touches a `.rs` file in a TypeScript-only session: `mcpls hook doctor` still shows rust-analyzer idle.
- `mcpls hook doctor` on a normal checkout showing the directory count, and on a `/mnt/c` checkout showing the reason.
- `--no-backend` watching the same way.
