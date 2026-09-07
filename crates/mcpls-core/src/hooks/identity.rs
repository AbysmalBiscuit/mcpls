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

/// Derive the socket identity for `dir`.
///
/// # Errors
///
/// Returns an error if `dir` cannot be canonicalized, which means it does
/// not exist or is not reachable.
pub fn identity_for(dir: &Path) -> Result<SocketIdentity> {
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
    let hash = format!("{:016x}", hasher.finish());

    #[cfg(windows)]
    {
        Ok(SocketIdentity {
            socket: PathBuf::from(format!(r"\\.\pipe\mcpls-{hash}")),
            lock: PathBuf::new(),
            hash,
        })
    }
    #[cfg(not(windows))]
    {
        let dir = runtime_dir();
        Ok(SocketIdentity {
            socket: dir.join(format!("{hash}.sock")),
            lock: dir.join(format!("{hash}.lock")),
            hash,
        })
    }
}

/// Where sockets go on this platform.
///
/// `$XDG_RUNTIME_DIR/mcpls` where that is set, which is the tmpfs a session
/// owns and which is cleaned when the session ends. Otherwise the system
/// temporary directory, which is `$TMPDIR` on macOS and `/tmp` on Linux,
/// with a per-user suffix so two users on one machine do not collide on a
/// shared `/tmp`.
///
/// The suffix comes from `$USER` or `$LOGNAME` rather than from `getuid`.
/// The workspace sets `unsafe_code = "deny"`, so `unsafe { libc::getuid() }`
/// does not compile here, and a safe wrapper crate would be a whole
/// dependency bought for one integer. Do not reinstate the uid call. Both
/// variables being absent gives an unsuffixed directory, which is right for
/// a single-user machine and no worse than what a shared `/tmp` already
/// offers.
///
/// A username can carry a path separator on some systems, so it is reduced
/// to ASCII alphanumerics and `-`, `_`, `.` before it goes into a path.
#[cfg(not(windows))]
fn runtime_dir() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("mcpls");
    }
    let user = std::env::var_os("USER")
        .or_else(|| std::env::var_os("LOGNAME"))
        .and_then(|raw| raw.into_string().ok())
        .map(|name| {
            name.chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .collect::<String>()
        })
        .filter(|name| !name.is_empty());
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
}
