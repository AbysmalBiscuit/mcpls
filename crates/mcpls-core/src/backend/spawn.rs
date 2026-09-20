//! Starting a backend: the arguments it runs with, the lock that keeps two
//! starters from racing, and the detached spawn itself.
//!
//! On Unix a frontend spawns the backend. On Windows a host may place its
//! MCP server in a job that kills every descendant when the session ends,
//! so the frontend writes a start request and the next hook invocation,
//! which runs outside that job, spawns it.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::hooks::SocketIdentity;

/// A backend log larger than this starts over when a backend is spawned.
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// How long a hook waits to learn whether a backend already answers.
const RUNNING_PROBE: Duration = Duration::from_millis(200);

/// How long a starter keeps ownership while a new backend becomes ready.
const STARTUP_DEADLINE: Duration = Duration::from_secs(5);

/// How often a starter retries a backend that has not answered yet.
const STARTUP_RETRY: Duration = Duration::from_millis(25);

/// What a backend is started with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendLaunch {
    /// The checkout root it serves, and its working directory.
    pub root: PathBuf,
    /// An explicit configuration file, already absolute.
    pub config: Option<PathBuf>,
    /// Whether the project's `mcpls.toml` is trusted.
    pub trust_project_config: bool,
    /// The log level.
    pub log_level: String,
    /// Whether logs are JSON.
    pub log_json: bool,
}

impl BackendLaunch {
    /// The command-line arguments that start this backend.
    #[must_use]
    pub fn args(&self) -> Vec<OsString> {
        let mut args = Vec::new();
        if let Some(config) = &self.config {
            args.push("--config".into());
            args.push(config.clone().into_os_string());
        }
        if self.trust_project_config {
            args.push("--trust-project-config".into());
        }
        args.push("--log-level".into());
        args.push(self.log_level.clone().into());
        if self.log_json {
            args.push("--log-json".into());
        }
        args.push("backend".into());
        args.push("--root".into());
        args.push(self.root.clone().into_os_string());
        args
    }
}

/// Held by whoever is starting a backend, so everyone else waits and then
/// connects to it instead of starting a second.
pub struct SpawnLock {
    _file: File,
}

impl SpawnLock {
    /// Take the lock at `path`, waiting up to `wait` for another holder.
    /// `None` when the wait ran out.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock file cannot be created or locked for
    /// a reason other than contention.
    pub async fn acquire(path: &Path, wait: Duration) -> io::Result<Option<Self>> {
        let deadline = Instant::now() + wait;
        loop {
            let attempt = path.to_path_buf();
            let locked = tokio::task::spawn_blocking(move || try_lock(&attempt))
                .await
                .map_err(io::Error::other)??;
            if locked.is_some() || Instant::now() >= deadline {
                return Ok(locked);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn try_lock(path: &Path) -> io::Result<Option<SpawnLock>> {
    use fs4::fs_std::FileExt as _;

    if let Some(parent) = path.parent() {
        ensure_runtime_dir(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(SpawnLock { _file: file })),
        Err(error)
            if error.kind() == io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs4::lock_contended_error().raw_os_error() =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Create the runtime directory, owner-only where the platform has modes.
pub(crate) fn ensure_runtime_dir(dir: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        crate::hooks::listener::ensure_private_dir(dir)
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Open the backend log for appending, starting it over past
/// [`MAX_LOG_BYTES`].
fn open_log(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        ensure_runtime_dir(parent)?;
    }
    let oversized = std::fs::metadata(path).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES);
    std::fs::OpenOptions::new()
        .create(true)
        .append(!oversized)
        .write(true)
        .truncate(oversized)
        .open(path)
}

/// Start `exe` as a backend for `launch`, detached from this process's
/// standard streams and process group. Returns its pid.
///
/// # Errors
///
/// Returns an error when the log cannot be opened or the process cannot be
/// spawned.
pub fn spawn_detached(exe: &Path, launch: &BackendLaunch, log: &Path) -> io::Result<u32> {
    spawn_detached_command(exe, launch.args(), &launch.root, log)
}

fn spawn_detached_command(
    exe: &Path,
    args: impl IntoIterator<Item = OsString>,
    cwd: &Path,
    log: &Path,
) -> io::Result<u32> {
    Ok(spawn_detached_command_with_status(exe, args, cwd, log)?.pid)
}

struct SpawnedChild {
    pid: u32,
    exited: Arc<AtomicBool>,
}

fn spawn_detached_with_status(
    exe: &Path,
    launch: &BackendLaunch,
    log: &Path,
) -> io::Result<SpawnedChild> {
    spawn_detached_command_with_status(exe, launch.args(), &launch.root, log)
}

/// Keep this process's standard handles out of the backend.
///
/// Windows gives a child every inheritable handle its parent holds, whatever
/// `STARTUPINFO` names as the child's own streams. A hook's standard output
/// is a pipe its host reads until the end, so a backend holding a copy of it
/// keeps that host waiting for as long as the backend runs.
#[cfg(windows)]
#[allow(unsafe_code)]
fn detach_standard_handles() {
    use windows_sys::Win32::Foundation::{
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: both calls take a handle this process already owns and
        // answer with a status. Nothing here dereferences, reads or closes.
        unsafe {
            let handle = GetStdHandle(id);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

fn spawn_detached_command_with_status(
    exe: &Path,
    args: impl IntoIterator<Item = OsString>,
    cwd: &Path,
    log: &Path,
) -> io::Result<SpawnedChild> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(open_log(log)?))
        .env_remove("CLAUDE_CODE_SESSION_ID");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        detach_standard_handles();
    }
    let mut child = command.spawn()?;
    let pid = child.id();
    let exited = Arc::new(AtomicBool::new(false));
    let child_exited = Arc::clone(&exited);
    // Reaped here so an exiting backend never lingers as a zombie of this
    // process.
    std::thread::spawn(move || {
        let _ = child.wait();
        child_exited.store(true, Ordering::Release);
    });
    Ok(SpawnedChild { pid, exited })
}

/// Ask the next hook invocation to start a backend for `launch`.
///
/// # Errors
///
/// Returns an error when the request cannot be written.
pub fn request_start(identity: &SocketIdentity, launch: &BackendLaunch) -> io::Result<()> {
    let path = identity.start_request();
    if let Some(parent) = path.parent() {
        ensure_runtime_dir(parent)?;
    }
    let pending = path.with_extension("start.tmp");
    std::fs::write(
        &pending,
        serde_json::to_vec(launch).map_err(io::Error::other)?,
    )?;
    std::fs::rename(pending, path)
}

/// Start the backend a frontend asked for, when one asked and none runs.
/// Returns whether this call spawned one.
///
/// # Errors
///
/// Returns an error when the request is unreadable or the spawn fails.
pub async fn start_requested(identity: &SocketIdentity, exe: &Path) -> io::Result<bool> {
    start_requested_with(identity, exe, spawn_detached_with_status).await
}

async fn start_requested_with<S>(
    identity: &SocketIdentity,
    exe: &Path,
    spawn: S,
) -> io::Result<bool>
where
    S: FnOnce(&Path, &BackendLaunch, &Path) -> io::Result<SpawnedChild>,
{
    let request = identity.start_request();
    if !request.exists() {
        return Ok(false);
    }
    let Some(_lock) = SpawnLock::acquire(&identity.spawn_lock(), Duration::ZERO).await? else {
        return Ok(false);
    };
    let Ok(bytes) = std::fs::read(&request) else {
        return Ok(false);
    };
    let running = tokio::time::timeout(RUNNING_PROBE, crate::hooks::listener::connect(identity))
        .await
        .is_ok_and(|connected| connected.is_ok());
    std::fs::remove_file(&request)?;
    if running {
        return Ok(false);
    }
    let launch: BackendLaunch = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    let child = spawn(exe, &launch, &identity.log_file())?;
    wait_for_startup(identity, &child.exited).await;
    Ok(true)
}

async fn wait_for_startup(identity: &SocketIdentity, exited: &AtomicBool) {
    let deadline = Instant::now() + STARTUP_DEADLINE;
    loop {
        if exited.load(Ordering::Acquire) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        let probe = remaining.min(RUNNING_PROBE);
        let running = tokio::time::timeout(probe, crate::hooks::listener::connect(identity))
            .await
            .is_ok_and(|connected| connected.is_ok());
        if running || exited.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(remaining.min(STARTUP_RETRY)).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn launch(root: &Path) -> BackendLaunch {
        BackendLaunch {
            root: root.to_path_buf(),
            config: Some(PathBuf::from("/etc/mcpls.toml")),
            trust_project_config: true,
            log_level: "debug".to_string(),
            log_json: true,
        }
    }

    #[test]
    fn test_the_launch_arguments_name_every_setting() {
        let args = launch(Path::new("/work")).args();
        let args: Vec<_> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--config",
                "/etc/mcpls.toml",
                "--trust-project-config",
                "--log-level",
                "debug",
                "--log-json",
                "backend",
                "--root",
                "/work",
            ]
        );
    }

    #[test]
    fn test_optional_launch_arguments_are_left_out() {
        let bare = BackendLaunch {
            config: None,
            trust_project_config: false,
            log_json: false,
            ..launch(Path::new("/work"))
        };
        let args: Vec<_> = bare
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["--log-level", "debug", "backend", "--root", "/work"]);
    }

    #[tokio::test]
    async fn test_the_spawn_lock_admits_one_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime").join("x.spawn.lock");
        let first = SpawnLock::acquire(&path, Duration::ZERO).await.unwrap();
        assert!(first.is_some());
        assert!(
            SpawnLock::acquire(&path, Duration::from_millis(100))
                .await
                .unwrap()
                .is_none()
        );
        drop(first);
        assert!(
            SpawnLock::acquire(&path, Duration::from_secs(2))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_an_oversized_log_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("x.log");
        std::fs::write(
            &log,
            vec![b'x'; usize::try_from(MAX_LOG_BYTES).unwrap() + 1],
        )
        .unwrap();
        drop(open_log(&log).unwrap());
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);

        std::fs::write(&log, b"kept").unwrap();
        drop(open_log(&log).unwrap());
        assert_eq!(std::fs::read(&log).unwrap(), b"kept");
    }

    /// The backend leaves the frontend's process group, which is the group
    /// Codex signals when a session ends.
    #[test]
    #[cfg(target_os = "linux")]
    fn test_a_detached_child_leads_its_own_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("group");
        let script = format!(
            "exec awk '{{print $5 == $1}}' /proc/self/stat > '{}'",
            out.display()
        );
        let pid = spawn_detached_command(
            Path::new("/bin/sh"),
            [OsString::from("-c"), OsString::from(script)],
            dir.path(),
            &dir.path().join("log"),
        )
        .unwrap();
        assert!(pid > 0);
        for _ in 0..100 {
            if std::fs::read_to_string(&out).is_ok_and(|text| !text.is_empty()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "1");
    }

    /// A host reads its hook's standard output until it ends. The backend
    /// the hook starts outlives it by hours, so a backend holding a copy of
    /// that stream leaves the host waiting on an end that does not come.
    ///
    /// Three processes, all this binary, told apart by their working
    /// directory: the test reads the output of a stand-in hook, which starts
    /// a detached stand-in backend that stays alive well past the read.
    #[cfg(windows)]
    #[test]
    fn test_a_backend_does_not_hold_its_starters_standard_output() {
        const MARKER: &str = "MCPLS_TEST_DETACHED_HANDLES";
        const TEST_NAME: &str = concat!(
            "backend::spawn::tests::",
            "test_a_backend_does_not_hold_its_starters_standard_output"
        );
        const BACKEND_LIFETIME: Duration = Duration::from_secs(20);
        const READ_BOUND: Duration = Duration::from_secs(8);

        let role = std::env::current_dir()
            .ok()
            .and_then(|dir| dir.file_name().map(std::ffi::OsStr::to_os_string));
        let role = role.as_deref().unwrap_or_default().to_string_lossy();
        if std::env::var_os(MARKER).is_some() {
            match role.as_ref() {
                "backend" => {
                    std::thread::sleep(BACKEND_LIFETIME);
                    return;
                }
                "starter" => {
                    let here = std::env::current_dir().unwrap();
                    let child = spawn_detached_command_with_status(
                        &std::env::current_exe().unwrap(),
                        ["--exact".into(), TEST_NAME.into(), "--nocapture".into()],
                        &here.parent().unwrap().join("backend"),
                        &here.join("backend.log"),
                    )
                    .unwrap();
                    std::fs::write(here.join("pid"), child.pid.to_string()).unwrap();
                    return;
                }
                _ => {}
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let starter = dir.path().join("starter");
        std::fs::create_dir(&starter).unwrap();
        std::fs::create_dir(dir.path().join("backend")).unwrap();
        let mut hook = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .current_dir(&starter)
            .env(MARKER, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let mut stdout = hook.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let read = std::io::Read::read_to_end(&mut stdout, &mut sink);
            let _ = tx.send(read.is_ok());
        });
        let ended = rx.recv_timeout(READ_BOUND).unwrap_or(false);

        let _ = hook.wait();
        if let Ok(pid) = std::fs::read_to_string(starter.join("pid")) {
            let _ = Command::new("taskkill")
                .args(["/PID", pid.trim(), "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        assert!(
            ended,
            "the starter's standard output stayed open while its backend ran"
        );
    }

    #[tokio::test]
    async fn test_no_request_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path());
        assert!(
            !start_requested(&identity, Path::new("/usr/bin/true"))
                .await
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_request_is_honoured_once() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path());
        request_start(&identity, &launch(dir.path())).unwrap();
        assert!(identity.start_request().exists());

        assert!(
            start_requested(&identity, Path::new("/usr/bin/true"))
                .await
                .unwrap()
        );
        assert!(!identity.start_request().exists());
        assert!(
            !start_requested(&identity, Path::new("/usr/bin/true"))
                .await
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_second_request_cannot_spawn_during_delayed_bind() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path());
        request_start(&identity, &launch(dir.path())).unwrap();

        let (spawned_tx, spawned_rx) = tokio::sync::oneshot::channel();
        let spawn_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let child_exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let first_identity = identity.clone();
        let first_count = std::sync::Arc::clone(&spawn_count);
        let first_exited = std::sync::Arc::clone(&child_exited);
        let first = tokio::spawn(async move {
            start_requested_with(
                &first_identity,
                Path::new("/bin/ignored"),
                move |_, _, _| {
                    first_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    spawned_tx.send(()).unwrap();
                    Ok(SpawnedChild {
                        pid: 0,
                        exited: first_exited,
                    })
                },
            )
            .await
        });
        spawned_rx.await.unwrap();

        request_start(&identity, &launch(dir.path())).unwrap();
        let second_count = std::sync::Arc::clone(&spawn_count);
        assert!(
            !start_requested_with(&identity, Path::new("/bin/ignored"), move |_, _, _| {
                second_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(SpawnedChild {
                    pid: 0,
                    exited: std::sync::Arc::clone(&child_exited),
                })
            },)
            .await
            .unwrap()
        );
        assert_eq!(spawn_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(identity.start_request().exists());

        let _listener = tokio::net::UnixListener::bind(&identity.socket).unwrap();
        assert!(first.await.unwrap().unwrap());
        assert_eq!(spawn_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_request_for_a_running_backend_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path());
        let _listener = crate::hooks::HookListener::acquire(&identity)
            .await
            .unwrap()
            .unwrap();
        request_start(&identity, &launch(dir.path())).unwrap();

        assert!(
            !start_requested(&identity, Path::new("/bin/false"))
                .await
                .unwrap()
        );
        assert!(!identity.start_request().exists());
    }

    fn test_identity(dir: &Path) -> SocketIdentity {
        SocketIdentity {
            hash: "t".to_string(),
            #[cfg(not(windows))]
            socket: dir.join("t.sock"),
            #[cfg(windows)]
            socket: PathBuf::from(format!(r"\\.\pipe\mcpls-spawn-test-{}", std::process::id())),
            lock: dir.join("t.lock"),
        }
    }
}
