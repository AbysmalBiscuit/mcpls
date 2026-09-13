//! Where a project's hook socket lives, and how the two sides agree on it.
//!
//! mcpls and the hook both resolve the checkout root enclosing their start
//! directories through [`project_root`] and hash that shared root. Sessions
//! started anywhere in one checkout therefore reach the same socket without
//! relying on the host's working-directory choice.
//!
//! `project_root` canonicalizes through `dunce`. `Path::canonicalize`
//! returns a `\\?\C:\...` extended-length path on Windows, so a design
//! where one side used the standard library and the other used `dunce`
//! would disagree on every Windows install, permanently and with nothing
//! to look at.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The size of `sockaddr_un.sun_path` on this platform, in bytes, including
/// the terminating null byte a `bind()` call writes into it: 104 on macOS
/// and the BSDs, 108 on Linux.
#[cfg(not(windows))]
const SUN_PATH_LEN: usize = if cfg!(target_os = "macos") { 104 } else { 108 };

/// Where this project's hook socket and its ownership lock live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketIdentity {
    /// 16 hex characters derived from the canonicalized directory.
    pub hash: String,
    /// The socket or named pipe to bind.
    pub socket: PathBuf,
    /// The lock file whose holder owns `socket` on Unix. On Windows the pipe
    /// itself is exclusive and this file is never locked, but the backend's
    /// other files sit beside it.
    pub lock: PathBuf,
}

impl SocketIdentity {
    /// The lock a frontend or hook holds while starting a backend.
    #[must_use]
    pub fn spawn_lock(&self) -> PathBuf {
        self.lock.with_extension("spawn.lock")
    }

    /// Where a detached backend's standard error goes.
    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.lock.with_extension("log")
    }

    /// Where a Windows frontend asks the next hook to start a backend.
    #[must_use]
    pub fn start_request(&self) -> PathBuf {
        self.lock.with_extension("start")
    }
}

/// The stable identity hash for `dir`: 16 hex characters derived from its
/// canonicalized form.
///
/// Split out of [`identity_for`] for a caller that only needs the hash and
/// not a socket to bind, so it is never tripped up by this platform's
/// socket path length limit, which only matters to an actual `bind()`.
///
/// # Errors
///
/// Returns an error if `dir` cannot be canonicalized, which means it does
/// not exist or is not reachable.
pub fn identity_hash(dir: &Path) -> Result<String> {
    let canonical = dunce::canonicalize(dir).map_err(|e| Error::FileIo {
        path: dir.to_path_buf(),
        source: e,
    })?;
    // DefaultHasher is not stable across Rust releases, which does not
    // matter here: both sides are the same binary in the same process
    // family, and a hash that changes between mcpls versions only means a
    // new socket path after an upgrade.
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

/// The checkout root enclosing `dir`: the nearest directory at or above it
/// holding a `.git` entry git would accept, or `dir` itself when none does.
///
/// Both sides of the socket resolve this before hashing, so two sessions in
/// one checkout reach one endpoint however deep in it they started. The test
/// is the `.git` entry rather than anything git reports, because that entry
/// marks a working tree: a linked worktree and a submodule each hold
/// one, so each is its own root, while `git rev-parse --git-common-dir`
/// would collapse every worktree of a repository onto one endpoint and serve
/// one tree's contents under another tree's name.
///
/// # Errors
///
/// Returns an error if `dir` cannot be canonicalized, which means it does
/// not exist or is not reachable.
pub fn project_root(dir: &Path) -> Result<PathBuf> {
    let canonical = dunce::canonicalize(dir).map_err(|e| Error::FileIo {
        path: dir.to_path_buf(),
        source: e,
    })?;
    let home = dirs::home_dir().map(|home| dunce::canonicalize(&home).unwrap_or(home));
    Ok(root_from(&canonical, home.as_deref()))
}

/// The walk itself, over an already canonical path.
///
/// Takes the home directory rather than reading it, for the same reason
/// [`user_component`] takes its raw value: the rule can then be exercised
/// without a test setting a process-global variable, which this workspace
/// cannot do at all.
///
/// Home and the filesystem root are never tested, only stopped at. A
/// dotfiles repository in the home directory would otherwise become the root
/// of every directory beneath it, which points a language server at the
/// whole of home.
fn root_from(canonical: &Path, home: Option<&Path>) -> PathBuf {
    for candidate in canonical.ancestors() {
        if candidate.parent().is_none() || home.is_some_and(|home| candidate == home) {
            break;
        }
        if holds_checkout_marker(candidate) {
            return candidate.to_path_buf();
        }
    }
    canonical.to_path_buf()
}

/// Whether `dir` holds a `.git` directory with a `HEAD`, or a `.git` file
/// pointing at one with `gitdir:`.
///
/// A bare `.git` name is not enough: an empty one left behind in a shared
/// directory such as `/tmp` would otherwise claim every directory under it.
/// The file is checked with `is_file` before it is read, so a FIFO named
/// `.git` is never opened.
fn holds_checkout_marker(dir: &Path) -> bool {
    let marker = dir.join(".git");
    if marker.is_dir() {
        return marker.join("HEAD").is_file();
    }
    marker.is_file()
        && std::fs::read(&marker).is_ok_and(|contents| contents.starts_with(b"gitdir:"))
}

/// The prefix every named pipe this user's mcpls binds on Windows carries.
///
/// The pipe namespace is machine-global rather than per-user the way the
/// Unix runtime directory is, so the user belongs in the name and therefore
/// in its prefix. With nobody named it is the bare `mcpls-` a single-user
/// host has always used.
///
/// [`identity_for`] builds its own pipe name from this, and `mcpls hook
/// doctor`'s scan for an owner running in a different directory filters on
/// it. One function serves both, so the scan cannot come to enumerate pipes
/// `identity_for` would never bind -- another user's -- by holding a second
/// copy of the naming scheme. This is what confines the scan to one user on
/// Windows; on Unix the runtime directory already does it.
#[cfg(windows)]
#[must_use]
pub fn windows_pipe_prefix() -> String {
    current_user().map_or_else(|| "mcpls-".to_string(), |user| format!("mcpls-{user}-"))
}

/// Derive the socket identity for `dir`.
///
/// # Errors
///
/// Returns an error if `dir` cannot be canonicalized, which means it does
/// not exist or is not reachable. On Unix, also returns an error if the
/// computed socket path would exceed this platform's `sockaddr_un.sun_path`
/// limit: a longer path fails at `bind()` with an OS-level error that gives
/// no hint the cause is path length, so this is checked here instead, where
/// the path is built and the failure can name it.
pub fn identity_for(dir: &Path) -> Result<SocketIdentity> {
    let hash = identity_hash(dir)?;

    #[cfg(windows)]
    {
        // The pipe namespace is machine-global, so the project hash is not
        // a whole identity on its own: two users with the same project
        // path on one host would derive one pipe name and mix the second
        // user's hooks and diagnostics with the first user's mcpls. The user
        // goes into the name for the same reason the Unix runtime directory
        // carries it. This keeps two users'
        // sessions apart wherever the environment names them, and falls back
        // to the bare hash where it names nobody; what stops one user
        // reaching the other's pipe at all is that pipe's own access
        // control, not its name.
        let prefix = windows_pipe_prefix();
        Ok(SocketIdentity {
            socket: PathBuf::from(format!(r"\\.\pipe\{prefix}{hash}")),
            lock: runtime_dir().join(format!("{hash}.lock")),
            hash,
        })
    }
    #[cfg(not(windows))]
    {
        let dir = runtime_dir();
        let socket = dir.join(format!("{hash}.sock"));
        ensure_socket_path_fits(&socket)?;
        Ok(SocketIdentity {
            lock: dir.join(format!("{hash}.lock")),
            socket,
            hash,
        })
    }
}

/// A macOS-shaped temporary directory plus a long username already leaves
/// only twenty or thirty bytes of margin against [`SUN_PATH_LEN`], and a
/// sandboxed or CI environment with a deeper temp path can exceed it
/// outright. Fails early and names both the limit and the offending path,
/// rather than leaving that to `bind()`.
#[cfg(not(windows))]
fn ensure_socket_path_fits(socket: &Path) -> Result<()> {
    // One byte is reserved for the null terminator `bind()` writes.
    let limit = SUN_PATH_LEN - 1;
    let len = socket.as_os_str().len();
    if len > limit {
        return Err(Error::SocketPathTooLong {
            path: socket.to_path_buf(),
            len,
            limit,
        });
    }
    Ok(())
}

/// Who this process is running as, reduced to characters safe in a path
/// and in a pipe name, or `None` when the environment names nobody.
///
/// Read from the environment rather than from the OS. The workspace sets
/// `unsafe_code = "deny"`, so neither `unsafe { libc::getuid() }` nor a
/// `GetUserName` call compiles here, and a safe wrapper crate would be a
/// whole dependency bought for one string. Do not reinstate either call.
///
/// `%USERNAME%` is read first on Windows, where it is the variable the
/// system itself sets and `$USER` and `$LOGNAME` appear only under ported
/// shells. One function for both platforms so the two socket namespaces
/// name the same user the same way.
///
/// The variables are read here and reduced by [`user_component`], which is
/// where the rules about what a name may contain live.
fn current_user() -> Option<String> {
    #[cfg(windows)]
    let raw = std::env::var_os("USERNAME")
        .or_else(|| std::env::var_os("USER"))
        .or_else(|| std::env::var_os("LOGNAME"));
    #[cfg(not(windows))]
    let raw = std::env::var_os("USER").or_else(|| std::env::var_os("LOGNAME"));

    user_component(raw)
}

/// `raw` reduced to what both a path component and a pipe name can carry,
/// or `None` when the environment named nobody or nothing usable survives.
///
/// A username can carry a path separator on some systems, and a Windows
/// pipe name may carry none at all after its prefix, so everything outside
/// ASCII alphanumerics and `-`, `_`, `.` is dropped. A name emptied by that
/// is `None` rather than an empty string: an empty component separates
/// nobody, and a socket path is better off saying so than carrying a
/// separator with nothing in front of it.
///
/// Takes the raw value rather than reading the environment itself, so the
/// rule can be exercised without a test setting a process-global variable
/// -- which this workspace cannot do at all, `std::env::set_var` being
/// `unsafe` and `unsafe_code` denied.
fn user_component(raw: Option<std::ffi::OsString>) -> Option<String> {
    raw.and_then(|raw| raw.into_string().ok())
        .map(|name| {
            name.chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .collect::<String>()
        })
        .filter(|name| !name.is_empty())
}

/// Where the socket, lock, spawn lock, log and start request go on Unix: the
/// system temporary directory, which honors `$TMPDIR` when set and otherwise
/// falls back to the platform default, in a directory carrying the user.
///
/// `XDG_RUNTIME_DIR` would be the better directory, being a tmpfs the user
/// owns and cleaned when the session ends, and it cannot be used. Codex
/// launches a stdio MCP server with a cleared environment and a fixed list
/// that omits it, while its hook commands inherit the whole environment, so
/// a server and a hook of one project would look for the socket in two
/// different places on any host that sets it.
#[cfg(not(windows))]
fn runtime_dir() -> PathBuf {
    shared_temp_runtime_dir(current_user())
}

/// Where the lock, spawn lock, backend log and start request go on Windows:
/// `mcpls` in the user's local application data folder.
///
/// `dirs` resolves that folder through `SHGetKnownFolderPath`, which ignores
/// environment overrides, so a frontend and a hook launched by a harness
/// with a different `%TEMP%` agree on the path. The profile folder's access
/// control is already per-user.
#[cfg(windows)]
fn runtime_dir() -> PathBuf {
    local_app_data_runtime_dir(dirs::data_local_dir(), current_user())
}

/// `mcpls` under `local`, or the shared temporary runtime directory for
/// `user` when the platform names no local application data folder.
#[cfg(any(windows, test))]
fn local_app_data_runtime_dir(local: Option<PathBuf>, user: Option<String>) -> PathBuf {
    local.map_or_else(
        || shared_temp_runtime_dir(user),
        |local| local.join("mcpls"),
    )
}

/// The runtime directory under the system temporary directory -- `$TMPDIR`
/// on macOS, `/tmp` on Linux -- which every user on the machine shares, so
/// `user` goes into its name and two of them do not collide on one socket.
/// No user to name gives an unsuffixed directory, which is right for a
/// single-user machine and no worse than what a shared `/tmp` already
/// offers.
///
/// Takes the user rather than reading the environment itself, for the same
/// reason [`user_component`] does: the choice of directory is a rule about
/// a name, and it can be exercised without a test setting a process-global
/// variable, which this workspace cannot do at all.
fn shared_temp_runtime_dir(user: Option<String>) -> PathBuf {
    user.map_or_else(
        || std::env::temp_dir().join("mcpls"),
        |user| std::env::temp_dir().join(format!("mcpls-{user}")),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_the_backend_files_sit_beside_the_lock() {
        let identity = SocketIdentity {
            hash: "abc".to_string(),
            socket: PathBuf::from("/run/mcpls-ada/abc.sock"),
            lock: PathBuf::from("/run/mcpls-ada/abc.lock"),
        };
        assert_eq!(
            identity.spawn_lock(),
            PathBuf::from("/run/mcpls-ada/abc.spawn.lock")
        );
        assert_eq!(identity.log_file(), PathBuf::from("/run/mcpls-ada/abc.log"));
        assert_eq!(
            identity.start_request(),
            PathBuf::from("/run/mcpls-ada/abc.start")
        );
    }

    /// Windows needs somewhere for the spawn lock and the backend log even
    /// though the pipe itself provides exclusivity.
    #[test]
    #[cfg(windows)]
    fn test_a_windows_identity_has_a_lock_path() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let identity = identity_for(dir.path()).expect("identity");
        assert!(identity.lock.ends_with(format!("{}.lock", identity.hash)));
    }

    /// The Windows directory is a known folder, which no environment
    /// variable redirects, and the shared temporary one only when the
    /// platform names no such folder.
    #[test]
    fn test_the_windows_runtime_dir_is_under_local_app_data() {
        let local = PathBuf::from("C:/Users/ada/AppData/Local");
        assert_eq!(
            local_app_data_runtime_dir(Some(local.clone()), Some("ada".to_string())),
            local.join("mcpls")
        );
        assert_eq!(
            local_app_data_runtime_dir(None, Some("ada".to_string())),
            shared_temp_runtime_dir(Some("ada".to_string()))
        );
    }

    /// A child with distinct runtime variables must still derive its socket
    /// directory from `TMPDIR` and `USER`, rather than `XDG_RUNTIME_DIR`.
    #[cfg(not(windows))]
    #[test]
    fn test_runtime_dir_ignores_the_xdg_variable() {
        const SENTINEL: &str = "MCPLS_TEST_RUNTIME_DIR_SENTINEL";
        const TEST_NAME: &str = "hooks::identity::tests::test_runtime_dir_ignores_the_xdg_variable";

        if std::env::var_os(SENTINEL).is_some() {
            assert_eq!(runtime_dir(), shared_temp_runtime_dir(current_user()));
            return;
        }

        let runtime = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME])
            .env(SENTINEL, "1")
            .env("XDG_RUNTIME_DIR", runtime.path().join("xdg-runtime"))
            .env("TMPDIR", runtime.path().join("temp-runtime"))
            .env("USER", "mcpls-runtime-dir-test")
            .env_remove("LOGNAME")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn test_a_username_cannot_carry_a_separator_into_a_socket_path() {
        assert_eq!(
            user_component(Some("DOMAIN\\Ada Lovelace".into())),
            Some("DOMAINAdaLovelace".to_string()),
            "a name reaches a directory path on one platform and a pipe name \
             on the other, and neither takes a separator here"
        );
    }

    /// The shared temporary fallback carries the user so two users on the
    /// same machine do not collide on one endpoint.
    #[test]
    fn test_a_shared_temp_runtime_dir_carries_the_user() {
        assert_eq!(
            shared_temp_runtime_dir(Some("ada".to_string())),
            std::env::temp_dir().join("mcpls-ada")
        );
        assert_eq!(
            shared_temp_runtime_dir(None),
            std::env::temp_dir().join("mcpls"),
            "a machine whose environment names nobody has no second user to \
             separate this from, and a bare separator names none either"
        );
    }

    #[test]
    fn test_a_username_nothing_survives_of_names_nobody() {
        assert_eq!(user_component(Some("!@#$".into())), None);
        assert_eq!(user_component(None), None);
    }

    /// Windows only: the pipe namespace is machine-global, so two users at
    /// the same project path would otherwise derive one identity and mix the
    /// second user's hooks and diagnostics with the first user's process.
    #[test]
    #[cfg(windows)]
    fn test_a_windows_pipe_name_carries_the_user() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let user = current_user().expect("a Windows session always names a user");

        let identity = identity_for(dir.path()).expect("identity");

        assert_eq!(
            identity.socket,
            PathBuf::from(format!(r"\\.\pipe\mcpls-{user}-{}", identity.hash)),
            "spelled out rather than built from windows_pipe_prefix, which is \
             the thing under test: a name assembled from the same function \
             would agree with it however that function changed"
        );
    }

    #[test]
    fn test_the_same_directory_hashes_the_same_way_twice() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let a = identity_for(dir.path()).expect("identity");
        let b = identity_for(dir.path()).expect("identity");
        assert_eq!(a, b);
    }

    #[test]
    fn test_two_directories_hash_differently() {
        let one = tempfile::tempdir().expect("a temp dir");
        let two = tempfile::tempdir().expect("a temp dir");
        assert_ne!(
            identity_for(one.path()).expect("identity").hash,
            identity_for(two.path()).expect("identity").hash
        );
    }

    #[test]
    fn test_the_hash_is_sixteen_hex_characters() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let identity = identity_for(dir.path()).expect("identity");
        assert_eq!(
            identity.hash.len(),
            16,
            "sockaddr_un allows 104 bytes on macOS and $TMPDIR eats most of them"
        );
        assert!(identity.hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_a_symlink_and_its_target_agree() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).expect("mkdir");
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&real, &link).expect("symlink");

        assert_eq!(
            identity_for(&real).expect("identity").hash,
            identity_for(&link).expect("identity").hash,
            "mcpls hashes its own working directory and the hook hashes \
             CLAUDE_PROJECT_DIR; a symlinked checkout must not split them"
        );
    }

    /// Unix only: a Windows named pipe path is not `sockaddr_un.sun_path`
    /// and carries no comparable limit.
    #[test]
    #[cfg(unix)]
    fn test_a_socket_path_over_the_platform_limit_is_rejected() {
        let long_dir = PathBuf::from("/").join("x".repeat(200));
        let socket = long_dir.join("0123456789abcdef.sock");

        let err = ensure_socket_path_fits(&socket)
            .expect_err("the socket path exceeds the platform limit");
        let message = err.to_string();
        assert!(
            message.contains(&(SUN_PATH_LEN - 1).to_string()),
            "names the limit: {message}"
        );
        assert!(
            message.contains(socket.to_str().expect("a utf8 path")),
            "names the offending path: {message}"
        );
    }

    /// Makes `dir` a checkout the way git itself would recognize one.
    fn mark_checkout(dir: &Path) {
        std::fs::create_dir(dir.join(".git")).expect("git dir");
        std::fs::write(dir.join(".git").join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    }

    /// A `.git` entry marks a working tree, so a directory inside one
    /// resolves to the tree rather than to itself.
    #[test]
    fn test_root_from_finds_the_nearest_git_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dunce::canonicalize(dir.path()).expect("canonical");
        mark_checkout(&root);
        let nested = root.join("crates").join("core");
        std::fs::create_dir_all(&nested).expect("nested dirs");

        assert_eq!(root_from(&nested, None), root);
    }

    /// A linked worktree holds a `.git` file rather than a directory, and it
    /// is its own root: two worktrees of one repository hold different files
    /// and must never share an endpoint.
    #[test]
    fn test_root_from_accepts_a_git_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dunce::canonicalize(dir.path()).expect("canonical");
        std::fs::write(root.join(".git"), "gitdir: /elsewhere/.git/worktrees/w").expect("git file");
        let nested = root.join("src");
        std::fs::create_dir(&nested).expect("nested dir");

        assert_eq!(root_from(&nested, None), root);
    }

    /// With no marker anywhere, the start directory is the root, which is
    /// the behaviour of a hash taken straight from the working directory.
    #[test]
    fn test_root_from_falls_back_to_the_start() {
        let dir = tempfile::tempdir().expect("temp dir");
        let boundary = dunce::canonicalize(dir.path()).expect("canonical");
        let start = boundary.join("project");
        std::fs::create_dir(&start).expect("start dir");

        assert_eq!(root_from(&start, Some(&boundary)), start);
    }

    /// An empty `.git` directory is not a checkout, so a stray one left in a
    /// shared directory such as `/tmp` does not claim everything beneath it.
    #[test]
    fn test_root_from_ignores_an_empty_git_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let boundary = dunce::canonicalize(dir.path()).expect("canonical");
        let outer = boundary.join("shared");
        std::fs::create_dir_all(outer.join(".git")).expect("empty git dir");
        let start = outer.join("project");
        std::fs::create_dir(&start).expect("start dir");

        assert_eq!(root_from(&start, Some(&boundary)), start);
    }

    /// A `.git` file that does not point at a git directory is not a
    /// checkout either.
    #[test]
    fn test_root_from_ignores_a_git_file_without_gitdir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let boundary = dunce::canonicalize(dir.path()).expect("canonical");
        let outer = boundary.join("shared");
        std::fs::create_dir(&outer).expect("outer dir");
        std::fs::write(outer.join(".git"), "").expect("empty git file");
        let start = outer.join("project");
        std::fs::create_dir(&start).expect("start dir");

        assert_eq!(root_from(&start, Some(&boundary)), start);
    }

    /// The walk stops before the home directory, so a dotfiles repository
    /// there never becomes the root of a project beneath it, which would
    /// point a language server at the whole of home.
    #[test]
    fn test_root_from_stops_before_home() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dunce::canonicalize(dir.path()).expect("canonical");
        mark_checkout(&home);
        let project = home.join("notes");
        std::fs::create_dir(&project).expect("project dir");

        assert_eq!(root_from(&project, Some(&home)), project);
    }

    /// The nearest marker wins, so a session inside a submodule gets the
    /// submodule rather than the repository containing it.
    #[test]
    fn test_root_from_prefers_the_nearest_marker() {
        let dir = tempfile::tempdir().expect("temp dir");
        let outer = dunce::canonicalize(dir.path()).expect("canonical");
        mark_checkout(&outer);
        let inner = outer.join("vendor").join("dep");
        std::fs::create_dir_all(&inner).expect("inner dirs");
        std::fs::write(inner.join(".git"), "gitdir: ../../.git/modules/dep")
            .expect("inner git file");

        assert_eq!(root_from(&inner, None), inner);
    }

    /// Resolving a root that is already a root returns it unchanged, which
    /// is what lets a process re-derive its own identity from the directory
    /// it was started in.
    #[test]
    fn test_root_from_is_idempotent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dunce::canonicalize(dir.path()).expect("canonical");
        mark_checkout(&root);
        let nested = root.join("src");
        std::fs::create_dir(&nested).expect("nested dir");

        let once = root_from(&nested, None);
        assert_eq!(root_from(&once, None), once);
    }

    /// `project_root` canonicalizes, so a start directory reached through a
    /// symlink resolves to the same root as the real path.
    #[test]
    fn test_project_root_canonicalizes_its_start() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dunce::canonicalize(dir.path()).expect("canonical");
        mark_checkout(&root);
        let nested = root.join("src");
        std::fs::create_dir(&nested).expect("nested dir");

        assert_eq!(project_root(&nested).expect("root"), root);
    }

    /// A start directory that does not exist is an error rather than a
    /// silently different endpoint.
    #[test]
    fn test_project_root_rejects_a_missing_start() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("absent");

        assert!(project_root(&missing).is_err());
    }
}
