//! Where a project's hook socket lives, and how the two sides agree on it.
//!
//! mcpls hashes its own startup working directory; the hook hashes
//! `CLAUDE_PROJECT_DIR`. Those agree because a host spawns a stdio MCP
//! server in the project directory, which is a property of the host rather
//! than a guarantee, which is why `mcpls hook doctor` prints both.
//!
//! Both sides canonicalize here, through `dunce`. `Path::canonicalize`
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
    /// The lock file whose holder owns `socket`. On Windows this is unused
    /// and empty: `first_pipe_instance` makes the pipe itself exclusive.
    pub lock: PathBuf,
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
        // path on one host would derive one pipe name, and the second
        // process to start would read as passive and forward its flushes
        // and the files it wrote into the first user's mcpls, which would
        // then answer user B's `get_new_diagnostics` from user A's
        // delivery record. The user goes into the name for the same reason
        // the Unix runtime directory carries it. This keeps two users'
        // sessions apart wherever the environment names them, and falls back
        // to the bare hash where it names nobody; what stops one user
        // reaching the other's pipe at all is that pipe's own access
        // control, not its name.
        let prefix = windows_pipe_prefix();
        Ok(SocketIdentity {
            socket: PathBuf::from(format!(r"\\.\pipe\{prefix}{hash}")),
            lock: PathBuf::new(),
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

/// Where sockets go on this platform.
///
/// `$XDG_RUNTIME_DIR/mcpls` where that is set, which is the tmpfs a session
/// owns and which is cleaned when the session ends. Otherwise the system
/// temporary directory, which is shared between everyone on the machine and
/// so needs the user in its name.
#[cfg(not(windows))]
fn runtime_dir() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("mcpls");
    }
    shared_temp_runtime_dir(current_user())
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
#[cfg(not(windows))]
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
    fn test_a_username_cannot_carry_a_separator_into_a_socket_path() {
        assert_eq!(
            user_component(Some("DOMAIN\\Ada Lovelace".into())),
            Some("DOMAINAdaLovelace".to_string()),
            "a name reaches a directory path on one platform and a pipe name \
             on the other, and neither takes a separator here"
        );
    }

    /// Unix only: `/tmp` is shared between everyone on the machine, so two
    /// users at the same project path would otherwise bind one socket, and
    /// the second would forward its flushes and its writes into the first
    /// user's process -- the collision the Windows pipe name closes on the
    /// other platform.
    #[test]
    #[cfg(not(windows))]
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
    /// the same project path would otherwise derive one identity, and the
    /// second would forward its flushes and its writes into the first
    /// user's process.
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
}
