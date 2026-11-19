//! Which checkout's backend serves a file a tool call names.

use std::path::{Component, Path, PathBuf};

/// How the frontend may reach the backend of a checkout other than its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// A linked worktree of the session's repository: start its backend when none runs.
    Start,
    /// Any other checkout: attach only to a backend that already runs there.
    AttachOnly,
}

/// The checkout root holding `file`, and how to reach its backend, or `None`
/// when the session's own backend serves `file`: a relative path, a path in
/// no checkout, or a path in `session_root` itself.
#[must_use]
pub fn route(session_root: &Path, file: &Path) -> Option<(PathBuf, Reach)> {
    if !file.is_absolute() {
        return None;
    }
    let root = crate::hooks::project_root(nearest_existing(file)).ok()?;
    if !crate::hooks::identity::holds_checkout_marker(&root) {
        return None;
    }
    let session = dunce::canonicalize(session_root).unwrap_or_else(|_| session_root.to_path_buf());
    if root == session {
        return None;
    }
    let reach = match (common_dir(&root), common_dir(&session)) {
        (Some(theirs), Some(ours)) if theirs == ours => Reach::Start,
        _ => Reach::AttachOnly,
    };
    Some((root, reach))
}

/// The root of every other checkout of the repository at `root`: the main
/// checkout and each linked worktree, read from the git common directory
/// without running git. Empty when `root` is not a git checkout.
pub fn linked_checkouts(root: &Path) -> Vec<PathBuf> {
    let root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let Some(common) = common_dir(&root) else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    if common.file_name().is_some_and(|name| name == ".git") {
        candidates.extend(common.parent().map(Path::to_path_buf));
    }
    if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
        candidates.extend(
            entries
                .flatten()
                .filter_map(|entry| worktree_checkout(&entry.path())),
        );
    }

    let mut checkouts: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|candidate| candidate.join(".git").exists())
        .filter_map(|candidate| dunce::canonicalize(candidate).ok())
        .filter(|candidate| *candidate != root)
        .collect();
    checkouts.sort();
    checkouts.dedup();
    checkouts
}

/// The checkout root of the linked worktree whose admin directory is `admin`,
/// from the `gitdir` file, which names `<worktree>/.git`.
fn worktree_checkout(admin: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(admin.join("gitdir")).ok()?;
    let marker = admin.join(contents.trim());
    marker.parent().map(Path::to_path_buf)
}

/// The directory `file` lives in: `file` itself when it is a directory, else
/// its nearest existing ancestor.
fn nearest_existing(file: &Path) -> &Path {
    if file.is_dir() {
        return file;
    }
    file.ancestors()
        .skip(1)
        .find(|path| path.exists())
        .unwrap_or(file)
}

/// The git common directory of the checkout at `root`, read from its `.git`
/// entry without running git.
fn common_dir(root: &Path) -> Option<PathBuf> {
    let marker = root.join(".git");
    let dir = if marker.is_dir() {
        marker
    } else if marker.is_file() {
        linked_common_dir(root, &marker)?
    } else {
        return None;
    };
    Some(dunce::canonicalize(&dir).unwrap_or_else(|_| normalize(&dir)))
}

/// The common directory of a linked worktree, from the `gitdir:` file at `marker`.
fn linked_common_dir(root: &Path, marker: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(marker).ok()?;
    let gitdir = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    let gitdir = root.join(gitdir);
    if let Ok(commondir) = std::fs::read_to_string(gitdir.join("commondir")) {
        return Some(gitdir.join(commondir.trim()));
    }
    let parent = gitdir.parent()?;
    if parent.file_name().is_some_and(|name| name == "worktrees") {
        return parent.parent().map(Path::to_path_buf);
    }
    Some(gitdir)
}

/// Resolves `.` and `..` components lexically, for paths that do not exist.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn canonical(dir: &Path) -> PathBuf {
        dunce::canonicalize(dir).expect("canonical")
    }

    /// A main checkout: a `.git` directory with a `HEAD`.
    fn make_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git")).expect("git dir");
        std::fs::write(dir.join(".git").join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    }

    /// A linked worktree: a `.git` file pointing at its admin directory.
    fn make_worktree(dir: &Path, gitdir: &Path) {
        std::fs::create_dir_all(dir).expect("worktree dir");
        std::fs::write(dir.join(".git"), format!("gitdir: {}\n", gitdir.display()))
            .expect("git file");
    }

    fn make_file(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("parent dirs");
        std::fs::write(path, "fn main() {}\n").expect("file");
    }

    #[test]
    fn test_a_file_in_the_session_checkout_stays_with_the_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let main = canonical(dir.path()).join("main");
        make_repo(&main);
        let file = main.join("src").join("a.rs");
        make_file(&file);

        assert_eq!(route(&main, &file), None);
    }

    #[test]
    fn test_a_linked_worktree_of_the_session_repository_may_start() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = canonical(dir.path());
        let main = root.join("main");
        make_repo(&main);
        let admin = main.join(".git").join("worktrees").join("w");
        std::fs::create_dir_all(&admin).expect("admin dir");
        std::fs::write(admin.join("commondir"), "../..\n").expect("commondir");
        let wt = root.join("wt");
        make_worktree(&wt, &admin);
        let file = wt.join("a.rs");
        make_file(&file);

        assert_eq!(route(&main, &file), Some((wt, Reach::Start)));
    }

    #[test]
    fn test_a_worktree_without_a_commondir_file_still_matches() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = canonical(dir.path());
        let main = root.join("main");
        make_repo(&main);
        let admin = main.join(".git").join("worktrees").join("w");
        let wt = root.join("wt");
        make_worktree(&wt, &admin);
        let file = wt.join("a.rs");
        make_file(&file);

        assert_eq!(route(&main, &file), Some((wt, Reach::Start)));
    }

    #[test]
    fn test_the_main_checkout_is_reached_from_a_worktree_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = canonical(dir.path());
        let main = root.join("main");
        make_repo(&main);
        let admin = main.join(".git").join("worktrees").join("w");
        std::fs::create_dir_all(&admin).expect("admin dir");
        std::fs::write(admin.join("commondir"), "../..\n").expect("commondir");
        let wt = root.join("wt");
        make_worktree(&wt, &admin);
        let file = main.join("src").join("a.rs");
        make_file(&file);

        assert_eq!(route(&wt, &file), Some((main, Reach::Start)));
    }

    #[test]
    fn test_an_unrelated_checkout_is_attach_only() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = canonical(dir.path());
        let session = root.join("session");
        make_repo(&session);
        let other = root.join("other");
        make_repo(&other);
        let file = other.join("src").join("a.rs");
        make_file(&file);

        assert_eq!(route(&session, &file), Some((other, Reach::AttachOnly)));
    }

    #[test]
    fn test_a_path_in_no_checkout_stays_with_the_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = canonical(dir.path());
        let session = root.join("session");
        make_repo(&session);
        let plain = root.join("plain");
        let file = plain.join("a.rs");
        make_file(&file);

        assert_eq!(route(&session, &file), None);
    }

    #[test]
    fn test_a_relative_path_stays_with_the_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = canonical(dir.path()).join("main");
        make_repo(&session);

        assert_eq!(route(&session, Path::new("src/a.rs")), None);
    }

    #[test]
    fn test_a_missing_file_routes_by_its_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = canonical(dir.path());
        let main = root.join("main");
        make_repo(&main);
        let admin = main.join(".git").join("worktrees").join("w");
        let wt = root.join("wt");
        make_worktree(&wt, &admin);
        let file = wt.join("new.rs");

        assert_eq!(route(&main, &file), Some((wt, Reach::Start)));
    }

    /// Registers `wt` as a linked worktree of the repository at `main`, under
    /// the admin directory `main/.git/worktrees/<name>`.
    fn link_worktree(main: &Path, name: &str, wt: &Path) -> PathBuf {
        let admin = main.join(".git").join("worktrees").join(name);
        std::fs::create_dir_all(&admin).expect("admin dir");
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .expect("gitdir");
        std::fs::write(admin.join("commondir"), "../..\n").expect("commondir");
        make_worktree(wt, &admin);
        admin
    }

    #[test]
    fn test_a_main_checkout_lists_its_linked_worktrees() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = canonical(dir.path());
        let main = root.join("main");
        make_repo(&main);
        let wt = root.join("wt");
        link_worktree(&main, "w", &wt);

        assert_eq!(linked_checkouts(&main), vec![wt]);
    }

    #[test]
    fn test_a_worktree_lists_the_main_checkout_and_its_siblings() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = canonical(dir.path());
        let main = root.join("main");
        make_repo(&main);
        let wt1 = root.join("wt1");
        let wt2 = root.join("wt2");
        link_worktree(&main, "w1", &wt1);
        link_worktree(&main, "w2", &wt2);

        let mut expected = vec![main, wt2];
        expected.sort();
        assert_eq!(linked_checkouts(&wt1), expected);
    }

    #[test]
    fn test_a_repository_without_worktrees_lists_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let main = canonical(dir.path()).join("main");
        make_repo(&main);

        assert_eq!(linked_checkouts(&main), Vec::<PathBuf>::new());
    }

    #[test]
    fn test_a_stale_worktree_entry_is_skipped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = canonical(dir.path());
        let main = root.join("main");
        make_repo(&main);
        let admin = main.join(".git").join("worktrees").join("stale");
        std::fs::create_dir_all(&admin).expect("admin dir");
        let gone = root.join("gone").join(".git");
        std::fs::write(admin.join("gitdir"), format!("{}\n", gone.display())).expect("gitdir");

        assert_eq!(linked_checkouts(&main), Vec::<PathBuf>::new());
    }
}
