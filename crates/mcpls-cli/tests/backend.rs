//! A frontend and its backend as the host sees them: separate processes
//! sharing one endpoint.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(deprecated)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt as _;
use serde_json::{Value, json};
use tempfile::TempDir;

/// A checkout with its own runtime directory and user name, so its backend
/// never meets a real one or another test's.
struct Project {
    dir: TempDir,
    runtime: TempDir,
    user: String,
}

impl Project {
    fn new(idle_ms: u64) -> Self {
        let dir = TempDir::new().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let project = Self {
            dir,
            runtime: short_temp_dir(),
            user: format!("mcpls-backend-test-{}-{}", std::process::id(), next()),
        };
        project.write_config(idle_ms, "");
        project
    }

    fn root(&self) -> PathBuf {
        dunce::canonicalize(self.dir.path()).unwrap()
    }

    fn config(&self) -> PathBuf {
        self.root().join("test-mcpls.toml")
    }

    /// `servers` is extra TOML appended after the backend table.
    fn write_config(&self, idle_ms: u64, servers: &str) {
        std::fs::write(
            self.config(),
            format!("[backend]\nidle_shutdown_ms = {idle_ms}\nspawn = \"eager\"\n{servers}"),
        )
        .unwrap();
    }

    fn command_with_identity(cwd: &Path, runtime: &Path, user: &str) -> Command {
        let mut command = Command::cargo_bin("mcpls").unwrap();
        command
            .env_remove("MCPLS_LOG")
            .env_remove("MCPLS_CONFIG")
            .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
            .env_remove("MCPLS_LOG_JSON")
            .env_remove("MCPLS_NO_BACKEND")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env("TMPDIR", runtime)
            .env("USER", user)
            .env("USERNAME", user)
            .current_dir(cwd);
        command
    }

    fn command(&self, cwd: &Path) -> Command {
        Self::command_with_identity(cwd, self.runtime.path(), &self.user)
    }

    fn frontend(&self) -> Frontend {
        self.frontend_in(&self.root(), &[])
    }

    fn frontend_in(&self, cwd: &Path, extra: &[&str]) -> Frontend {
        let mut command = self.command(cwd);
        command.arg("--config").arg(self.config()).args(extra);
        Frontend::spawn(command)
    }

    #[cfg(unix)]
    fn spawns(&self) -> PathBuf {
        self.root().join("spawns.txt")
    }

    /// A language server that records each start and never answers, so
    /// the number of lines in `spawns.txt` is the number of servers mcpls
    /// started.
    #[cfg(unix)]
    fn with_counting_server(self, idle_ms: u64) -> Self {
        std::fs::write(self.root().join("marker.fake"), "").unwrap();
        let script = format!("echo $$ >> '{}'; exec sleep 30", self.spawns().display());
        self.write_config(
            idle_ms,
            &format!(
                "\n[[lsp_servers]]\nlanguage_id = \"fake\"\ncommand = \"/bin/sh\"\nargs = [\"-c\", {script:?}]\nfile_patterns = [\"**/*.fake\"]\ntimeout_seconds = 30\n\n[lsp_servers.heuristics]\nproject_markers = [\"marker.fake\"]\n"
            ),
        );
        self
    }

    /// Assert `holds` stays true for the whole of `window`. For a start or
    /// a replacement that must not happen, where no event marks its absence.
    fn holds_for(&self, what: &str, window: Duration, mut holds: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            assert!(holds(self), "{what} stopped holding: {}", self.doctor());
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// `mcpls doctor`'s report for this project.
    fn doctor(&self) -> String {
        Self::doctor_for(&self.root(), self.runtime.path(), &self.user)
    }

    fn doctor_for(root: &Path, runtime: &Path, user: &str) -> String {
        let output = Self::command_with_identity(root, runtime, user)
            .env("CLAUDE_PROJECT_DIR", root)
            .args(["hook", "doctor"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Assert the frontend's tool calls reach its backend.
    ///
    /// On Windows a frontend cannot start a backend: it asks, a hook starts
    /// the one it asked for, and the frontend attaches on its next retry.
    fn assert_attaches(&self, frontend: &mut Frontend) {
        Self::assert_attaches_in(&self.root(), self.runtime.path(), &self.user, frontend);
    }

    fn assert_attaches_in(root: &Path, runtime: &Path, user: &str, frontend: &mut Frontend) {
        #[cfg(windows)]
        {
            Self::fire_hook_for(root, runtime, user);
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let call = frontend.call_tool();
                let waiting = call["result"]["content"][0]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("waiting for its backend"));
                if !waiting || Instant::now() >= deadline {
                    assert_tool_success(&call);
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (root, runtime, user);
            assert_tool_success(&frontend.call_tool());
        }
    }

    #[cfg(windows)]
    fn fire_hook_for(root: &Path, runtime: &Path, user: &str) {
        let status =
            Self::command_with_identity(root, runtime, user)
                .env("CLAUDE_PROJECT_DIR", root)
                .arg("hook")
                .stdin(Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    child.stdin.take().unwrap().write_all(
                        br#"{"hook_event_name":"UserPromptSubmit","session_id":"s1"}"#,
                    )?;
                    child.wait()
                })
                .unwrap();
        assert!(status.success());
    }

    /// The backend's pid, or `None` when nothing answers.
    fn backend_pid(&self) -> Option<u32> {
        Self::backend_pid_for(&self.root(), self.runtime.path(), &self.user)
    }

    fn backend_pid_for(root: &Path, runtime: &Path, user: &str) -> Option<u32> {
        Self::doctor_for(root, runtime, user)
            .lines()
            .find_map(|line| line.strip_prefix("backend pid: "))
            .and_then(|pid| pid.parse().ok())
    }

    fn wait_for_backend_exit(root: &Path, runtime: &Path, user: &str, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Self::backend_pid_for(root, runtime, user).is_some() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {}",
                Self::doctor_for(root, runtime, user)
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Run `mcpls backend ARGS` for this project: whether it succeeded,
    /// and what it printed.
    fn backend(&self, args: &[&str]) -> (bool, String) {
        let output = self
            .command(&self.root())
            .arg("--config")
            .arg(self.config())
            .arg("backend")
            .args(args)
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    }

    #[cfg(unix)]
    fn lifecycle_log(&self) -> PathBuf {
        self.root().join("lifecycle.log")
    }

    /// A language server that answers the handshake, logs `start <pid>`
    /// when launched and `exit <pid>` on the LSP `exit` notification.
    #[cfg(unix)]
    fn with_lifecycle_server(self) -> Self {
        std::fs::write(self.root().join("marker.fake"), "").unwrap();
        std::fs::write(self.root().join("a.fake"), "").unwrap();
        let script = self.root().join("lifecycle_lsp.py");
        std::fs::write(&script, LIFECYCLE_LSP).unwrap();
        self.write_config(
            60_000,
            &format!(
                "\n[[lsp_servers]]\nlanguage_id = \"fake\"\ncommand = \"python3\"\nargs = [{:?}, {:?}]\nfile_patterns = [\"**/*.fake\"]\ntimeout_seconds = 10\n\n[lsp_servers.heuristics]\nproject_markers = [\"marker.fake\"]\n",
                script.display().to_string(),
                self.lifecycle_log().display().to_string(),
            ),
        );
        self
    }

    /// The lifecycle server behind `bin/fake-ls`, which is missing until
    /// the entry's install command writes it.
    #[cfg(unix)]
    fn with_installable_lifecycle_server(self) -> Self {
        std::fs::write(self.root().join("marker.fake"), "").unwrap();
        std::fs::write(self.root().join("a.fake"), "").unwrap();
        let script = self.root().join("lifecycle_lsp.py");
        std::fs::write(&script, LIFECYCLE_LSP).unwrap();
        let install = "mkdir -p bin && printf '#!/bin/sh\\nexec python3 \"$@\"\\n' > bin/fake-ls \
                       && chmod +x bin/fake-ls";
        self.write_config(
            60_000,
            &format!(
                "\n[[lsp_servers]]\nlanguage_id = \"fake\"\ncommand = {:?}\nargs = [{:?}, {:?}]\nfile_patterns = [\"**/*.fake\"]\ntimeout_seconds = 10\ninstall = {install:?}\n\n[lsp_servers.heuristics]\nproject_markers = [\"marker.fake\"]\n",
                self.root().join("bin").join("fake-ls").display().to_string(),
                script.display().to_string(),
                self.lifecycle_log().display().to_string(),
            ),
        );
        self
    }

    #[cfg(unix)]
    fn lifecycle_events(&self, kind: &str) -> Vec<u32> {
        std::fs::read_to_string(self.lifecycle_log())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(kind)?.trim().parse().ok())
            .collect()
    }

    fn lsp(&self, args: &[&str]) -> std::process::Output {
        self.command(&self.root())
            .env("CLAUDE_PROJECT_DIR", self.root())
            .arg("--config")
            .arg(self.config())
            .arg("lsp")
            .args(args)
            .output()
            .unwrap()
    }

    fn wait_for(&self, what: &str, mut ready: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready(self) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {}",
                self.doctor()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(unix)]
const LIFECYCLE_LSP: &str = r#"import json, os, sys

log = open(sys.argv[1], "a", buffering=1)
log.write(f"start {os.getpid()}\n")

def receive():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode().split(":", 1)
        if key.lower() == "content-length":
            length = int(value.strip())
    return json.loads(sys.stdin.buffer.read(length))

def send(message):
    body = json.dumps({"jsonrpc": "2.0", **message}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()

while True:
    message = receive()
    if message is None:
        break
    method = message.get("method")
    if method == "initialize":
        send({"id": message["id"], "result": {"capabilities": {"documentSymbolProvider": True}}})
    elif method == "exit":
        log.write(f"exit {os.getpid()}\n")
        break
    elif "id" in message:
        send({"id": message["id"], "result": None})
"#;

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// macOS's `$TMPDIR` is deep enough that a runtime directory inside it
/// pushes the socket path past the `sun_path` limit.
fn short_temp_dir() -> TempDir {
    #[cfg(unix)]
    let dir = tempfile::Builder::new().tempdir_in("/tmp");
    #[cfg(not(unix))]
    let dir = TempDir::new();
    dir.unwrap()
}

/// Runs its closure when dropped, including while a failed assertion
/// unwinds.
struct OnDrop<F: FnMut()>(F);

impl<F: FnMut()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

fn next() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

struct Frontend {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: i64,
}

impl Frontend {
    fn spawn(command: Command) -> Self {
        let mut frontend = Self::spawn_uninitialized(command);
        frontend.initialize();
        frontend
    }

    fn spawn_uninitialized(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            lines,
            next_id: 0,
        }
    }

    fn request(&mut self, method: &str, params: &Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).expect("a response in time");
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|_| panic!("the host's stream carried a non-JSON line: {line}"));
            assert_eq!(message["jsonrpc"], "2.0", "{line}");
            if message["id"] == id {
                return message;
            }
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    #[cfg(unix)]
    fn symbols(&mut self, path: &Path) -> Value {
        self.request(
            "tools/call",
            &json!({"name": "get_document_symbols", "arguments": {"file_path": path}}),
        )
    }

    fn call_tool(&mut self) -> Value {
        self.request(
            "tools/call",
            &json!({"name": "get_server_logs", "arguments": {}}),
        )
    }

    #[cfg(unix)]
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn initialize(&mut self) -> Value {
        let answer = self.request(
            "initialize",
            &json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "backend-test", "version": "1"}}),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        answer
    }

    /// Close stdin, as a host ending its session does, and return whether
    /// stdout reached EOF within `within`.
    fn close(&mut self, within: Duration) -> bool {
        drop(self.stdin.take());
        let deadline = Instant::now() + within;
        loop {
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(_) => {}
                Err(RecvTimeoutError::Disconnected) => return true,
                Err(RecvTimeoutError::Timeout) => return false,
            }
        }
    }
}

impl Drop for Frontend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(unix)]
struct KillOnDrop(u32);

#[cfg(unix)]
impl KillOnDrop {
    fn terminate(&self) -> bool {
        !alive(self.0)
            || Command::new("kill")
                .args(["-TERM", &self.0.to_string()])
                .status()
                .is_ok_and(|status| status.success())
    }
}

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(unix)]
fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |text| text.lines().count())
}

#[cfg(unix)]
fn last_pid(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .parse()
        .unwrap()
}

fn assert_tool_success(call: &Value) {
    assert!(call["result"].is_object(), "{call}");
    assert_ne!(call["result"]["isError"], true, "{call}");
}

/// The reason a failed call carries: a tool result's text, or a JSON-RPC
/// error's message when the call was in flight as the backend died.
#[cfg(unix)]
fn failure_text(call: &Value) -> String {
    call["result"]["content"][0]["text"]
        .as_str()
        .or_else(|| call["error"]["message"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// Removes this checkout's files from the runtime directory, which a
/// Windows test cannot redirect to a temporary one.
#[cfg(windows)]
impl Drop for Project {
    fn drop(&mut self) {
        let Ok(identity) = mcpls_core::hooks::identity_for(&self.root()) else {
            return;
        };
        let Some(dir) = identity.lock.parent() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let prefix = format!("{}.", identity.hash);
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Closing the last session removes the backend after its timer, and the
/// host sees EOF as soon as its frontend exits.
#[cfg(unix)]
#[test]
fn closing_the_last_session_removes_the_backend() {
    let project = Project::new(300);
    let mut frontend = project.frontend();
    let frontend_pid = frontend.pid();
    let backend_pid = project.backend_pid().unwrap();
    assert_ne!(
        backend_pid, frontend_pid,
        "the frontend and backend must be separate processes"
    );
    assert!(
        frontend.close(Duration::from_secs(5)),
        "the host did not see EOF"
    );
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
    assert!(!alive(backend_pid));
}

/// Two sessions, one started in a subdirectory, share one backend and one
/// language server, and the second answers without waiting on a start.
#[cfg(unix)]
#[test]
fn two_sessions_share_one_backend_and_one_server() {
    let project = Project::new(500).with_counting_server(500);
    let mut first = project.frontend();
    project.wait_for("the first server to start", |p| {
        line_count(&p.spawns()) == 1
    });
    let pid = project.backend_pid().expect("a backend");

    let nested = project.root().join("src").join("deep");
    std::fs::create_dir_all(&nested).unwrap();
    let started = Instant::now();
    let mut second = project.frontend_in(&nested, &[]);
    assert_tool_success(&second.call_tool());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the second session waited on a start"
    );

    assert_eq!(project.backend_pid(), Some(pid));
    assert!(
        project.doctor().contains("sessions: 2 attached"),
        "{}",
        project.doctor()
    );
    // A second runtime would spawn its server as it starts, before its
    // session answers; one idle period covers a start still in flight.
    project.holds_for("one language server", Duration::from_millis(500), |p| {
        line_count(&p.spawns()) == 1
    });
    assert_tool_success(&first.call_tool());

    first.close(Duration::from_secs(5));
    second.close(Duration::from_secs(5));
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
    project.wait_for("the backend process to exit", |_| !alive(pid));
}

/// Two worktrees of one repository hold different files and get a backend
/// each.
#[test]
fn two_worktrees_get_a_backend_each() {
    let one = Project::new(500);
    let two = Project::new(500);
    std::fs::remove_dir_all(two.root().join(".git")).unwrap();
    std::fs::write(
        two.root().join(".git"),
        format!("gitdir: {}/.git/worktrees/two", one.root().display()),
    )
    .unwrap();

    let mut a = one.frontend();
    let mut b = Frontend::spawn({
        let mut command = two.command(&two.root());
        command
            .env("TMPDIR", one.runtime.path())
            .env("USER", &one.user)
            .env("USERNAME", &one.user)
            .arg("--config")
            .arg(two.config());
        command
    });
    one.assert_attaches(&mut a);
    Project::assert_attaches_in(&two.root(), one.runtime.path(), &one.user, &mut b);
    let first_pid = one.backend_pid().expect("the first backend");
    let second_pid = Project::backend_pid_for(&two.root(), one.runtime.path(), &one.user)
        .expect("the second backend");
    assert_ne!(first_pid, second_pid);
    assert!(
        one.doctor().contains("sessions: 1 attached"),
        "{}",
        one.doctor()
    );

    assert!(a.close(Duration::from_secs(5)));
    assert!(b.close(Duration::from_secs(5)));
    one.wait_for("the first backend to exit", |p| p.backend_pid().is_none());
    Project::wait_for_backend_exit(
        &two.root(),
        one.runtime.path(),
        &one.user,
        "the second backend to exit",
    );
    #[cfg(unix)]
    {
        assert!(!alive(first_pid));
        assert!(!alive(second_pid));
    }
}

/// Frontends racing from nothing start one backend, and none of them sees
/// an error.
#[test]
fn racing_frontends_start_one_backend() {
    let project = Project::new(500);
    let mut frontends: Vec<Frontend> = (0..4)
        .map(|_| {
            let command = {
                let mut command = project.command(&project.root());
                command.arg("--config").arg(project.config());
                command
            };
            std::thread::spawn(move || Frontend::spawn(command))
        })
        .map(|thread| thread.join().unwrap())
        .collect();
    for frontend in &mut frontends {
        project.assert_attaches(frontend);
    }
    assert!(
        project.doctor().contains("sessions: 4 attached"),
        "{}",
        project.doctor()
    );
    for mut frontend in frontends {
        frontend.close(Duration::from_secs(5));
    }
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
}

/// A killed backend is reported, and the frontend starts no language
/// server of its own.
#[cfg(unix)]
#[test]
fn a_killed_backend_is_reported_not_replaced() {
    let project = Project::new(500).with_counting_server(500);
    let mut frontend = project.frontend();
    project.wait_for("the server to start", |p| line_count(&p.spawns()) == 1);
    let server_pid = last_pid(&project.spawns());
    let server_cleanup = KillOnDrop(server_pid);
    let pid = project.backend_pid().unwrap();

    let status = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "failed to kill backend {pid}");
    project.wait_for("the killed backend to be gone", |_| !alive(pid));
    let call = frontend.call_tool();
    assert!(failure_text(&call).contains("stopped"), "{call}");
    // No event marks a replacement that never starts; one idle period
    // bounds the wait for one.
    project.holds_for("no replacement backend", Duration::from_millis(500), |p| {
        line_count(&p.spawns()) == 1 && p.backend_pid().is_none()
    });
    assert!(
        server_cleanup.terminate(),
        "failed to terminate counting server {server_pid}"
    );
    project.wait_for("the counting server to exit", |_| !alive(server_pid));
}

/// A deleted socket leaves attached sessions working, and the backend still
/// exits on its timer.
#[cfg(unix)]
#[test]
fn a_deleted_socket_does_not_stop_attached_sessions() {
    let project = Project::new(300);
    let mut frontend = project.frontend();
    let pid = project.backend_pid().unwrap();
    for entry in std::fs::read_dir(
        project
            .runtime
            .path()
            .join(format!("mcpls-{}", project.user)),
    )
    .unwrap()
    {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "sock") {
            std::fs::remove_file(path).unwrap();
        }
    }
    assert_tool_success(&frontend.call_tool());
    frontend.close(Duration::from_secs(5));
    project.wait_for("the backend to exit on its idle timer", |_| !alive(pid));
}

/// Codex ends a session by signalling the server's whole process group. The
/// backend is not in it.
#[cfg(unix)]
#[test]
fn a_group_signal_to_the_frontend_spares_the_backend() {
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};

    // Long enough that the backend's idle exit cannot pass for the signal.
    let project = Project::new(2_000);
    let mut command = project.command(&project.root());
    command
        .arg("--config")
        .arg(project.config())
        .process_group(0);
    let mut frontend = Frontend::spawn(command);
    let pid = project.backend_pid().unwrap();
    let group = frontend.child.id();

    let status = Command::new("kill")
        .args(["-TERM", "--", &format!("-{group}")])
        .status()
        .unwrap();
    assert!(status.success(), "failed to signal frontend group {group}");
    let deadline = Instant::now() + Duration::from_secs(20);
    let frontend_status = loop {
        if let Some(status) = frontend.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the frontend to die of the group signal"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(frontend_status.signal(), Some(15));
    assert!(alive(pid), "the backend died with the frontend's group");
    drop(frontend);
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
    project.wait_for("the backend process to exit", |_| !alive(pid));
}

/// Sessions that disagree about trusting the project's config cannot share
/// a backend, and the second is told why.
#[test]
fn a_trust_disagreement_is_refused_with_both_states() {
    let project = Project::new(500);
    std::fs::write(
        project.root().join("mcpls.toml"),
        "[backend]\nidle_shutdown_ms = 500\n",
    )
    .unwrap();
    let mut trusting = Frontend::spawn({
        let mut command = project.command(&project.root());
        command.arg("--trust-project-config");
        command
    });
    project.assert_attaches(&mut trusting);

    let mut untrusting = Frontend::spawn_uninitialized(project.command(&project.root()));
    let init = untrusting.initialize();
    let text = init["result"]["instructions"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(text.contains("trust"), "{init}");
    assert_eq!(untrusting.call_tool()["result"]["isError"], true);

    trusting.close(Duration::from_secs(5));
    untrusting.close(Duration::from_secs(5));
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
}

/// On Windows the frontend cannot spawn a backend. A hook invocation starts
/// the one it asked for.
#[cfg(windows)]
#[test]
fn a_hook_starts_the_backend_a_frontend_asked_for() {
    let project = Project::new(500);
    let mut frontend = project.frontend();
    let before = project.doctor();
    assert!(before.contains("backend pid: none"), "{before}");

    project.assert_attaches(&mut frontend);
    assert!(project.backend_pid().is_some(), "{}", project.doctor());
    frontend.close(Duration::from_secs(5));
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
}

/// `mcpls backend start` keeps a backend up with no session attached, past
/// its idle timer, until `mcpls backend stop` ends it.
#[test]
fn a_started_backend_outlives_its_idle_timer_until_stopped() {
    let project = Project::new(300);
    let (started, text) = project.backend(&["start"]);
    assert!(started, "{text}");
    let pid = project.backend_pid().expect("a started backend");
    assert!(text.contains(&pid.to_string()), "{text}");

    project.holds_for("the kept backend", Duration::from_millis(1_500), |p| {
        p.backend_pid() == Some(pid)
    });
    let (running, status) = project.backend(&["status"]);
    assert!(running && status.contains("kept"), "{status}");

    let (stopped, text) = project.backend(&["stop"]);
    assert!(stopped, "{text}");
    assert_eq!(project.backend_pid(), None, "{}", project.doctor());
    let (running, status) = project.backend(&["status"]);
    assert!(!running && status.contains("not running"), "{status}");
    let (stopped, text) = project.backend(&["stop"]);
    assert!(stopped && text.contains("not running"), "{text}");
}

/// `mcpls backend auto` hands a kept backend back to its idle timer, so with
/// no session attached it exits on its own.
#[test]
fn an_auto_backend_exits_on_its_idle_timer() {
    let project = Project::new(300);
    let (started, text) = project.backend(&["start"]);
    assert!(started, "{text}");
    let pid = project.backend_pid().expect("a started backend");
    project.holds_for("the kept backend", Duration::from_millis(1_000), |p| {
        p.backend_pid() == Some(pid)
    });

    let (released, text) = project.backend(&["auto"]);
    assert!(released, "{text}");
    assert!(text.contains(&pid.to_string()), "{text}");
    project.wait_for("the released backend to exit", |p| {
        p.backend_pid().is_none()
    });

    let (released, text) = project.backend(&["auto"]);
    assert!(released && text.contains("not running"), "{text}");
}

/// A start keeps the backend a session already started, and a stop with
/// that session attached leaves the backend to exit once the session does.
#[test]
fn a_stop_leaves_an_attached_backend_to_exit_with_its_sessions() {
    // Outlasts the frontend's retry, so a hook-started backend on Windows is
    // still up when the frontend comes to attach.
    let project = Project::new(2_000);
    let mut frontend = project.frontend();
    project.assert_attaches(&mut frontend);
    let pid = project.backend_pid().expect("a backend");

    let (started, text) = project.backend(&["start"]);
    assert!(started, "{text}");
    assert_eq!(
        project.backend_pid(),
        Some(pid),
        "start replaced the backend"
    );

    let (stopped, text) = project.backend(&["stop"]);
    assert!(!stopped, "{text}");
    assert!(text.contains("1 session"), "{text}");
    assert_eq!(project.backend_pid(), Some(pid));

    frontend.close(Duration::from_secs(5));
    project.wait_for("the backend to exit with its session", |p| {
        p.backend_pid().is_none()
    });
}

/// A forced stop ends the backend with a session attached, and that
/// session's tools then fail rather than hang.
#[test]
fn a_forced_stop_ends_a_backend_with_a_session_attached() {
    let project = Project::new(60_000);
    let mut frontend = project.frontend();
    project.assert_attaches(&mut frontend);

    let (stopped, text) = project.backend(&["stop", "--force"]);
    assert!(stopped, "{text}");
    assert_eq!(project.backend_pid(), None, "{}", project.doctor());
    let call = frontend.call_tool();
    assert!(
        call["result"]["isError"] == true || call["error"].is_object(),
        "{call}"
    );
}

/// `mcpls status` lists the backend for the checkout it is asked from,
/// `--all` lists every backend this user runs, and each says so when none
/// runs.
#[test]
fn status_lists_this_checkouts_backend_or_every_one_with_all() {
    let one = Project::new(60_000);
    let two = Project::new(60_000);
    let backend = |project: &Project, action: &str| {
        Project::command_with_identity(&project.root(), one.runtime.path(), &one.user)
            .arg("--config")
            .arg(project.config())
            .args(["backend", action])
            .output()
            .unwrap()
    };
    // A kept backend never exits on its own, so a failed assertion must not
    // skip the stop.
    let _stop = OnDrop(|| {
        for project in [&one, &two] {
            let _ = Project::command_with_identity(&project.root(), one.runtime.path(), &one.user)
                .arg("--config")
                .arg(project.config())
                .args(["backend", "stop", "--force"])
                .output();
        }
    });
    let status = |args: &[&str]| {
        Project::command_with_identity(&one.root(), one.runtime.path(), &one.user)
            .env("CLAUDE_PROJECT_DIR", one.root())
            .arg("status")
            .args(args)
            .output()
            .unwrap()
    };
    let none_here = format!("no mcpls backend is running for {}\n", one.root().display());

    for (args, expected) in [
        (&[][..], none_here.as_str()),
        (&["--all"][..], "no mcpls backend is running\n"),
    ] {
        let empty = status(args);
        assert!(empty.status.success(), "{}", stdout(&empty));
        assert_eq!(stdout(&empty), expected);
    }

    let logs: Vec<String> = [&one, &two]
        .into_iter()
        .map(|project| {
            let started = backend(project, "start");
            assert!(started.status.success(), "{}", stdout(&started));
            stdout(&started)
                .lines()
                .find_map(|line| line.strip_prefix("log: "))
                .expect("a start names its log")
                .to_string()
        })
        .collect();

    let block_for = |text: &str, project: &Project| {
        text.split("\n\n")
            .find(|block| block.starts_with(&format!("{}\n", project.root().display())))
            .map(str::to_string)
    };
    let everything = status(&["--all"]);
    let text = stdout(&everything);
    assert!(everything.status.success(), "{text}");
    for (project, log) in [&one, &two].into_iter().zip(&logs) {
        let pid = Project::backend_pid_for(&project.root(), one.runtime.path(), &one.user)
            .expect("a started backend");
        let block = block_for(&text, project)
            .unwrap_or_else(|| panic!("no block for {}: {text}", project.root().display()));
        assert!(block.contains(&format!("pid {pid},")), "{text}");
        assert!(block.contains(log.as_str()), "{text}");
    }

    let here = status(&[]);
    let text = stdout(&here);
    assert!(here.status.success(), "{text}");
    assert!(block_for(&text, &one).is_some(), "{text}");
    assert!(block_for(&text, &two).is_none(), "{text}");

    for project in [&one, &two] {
        let stopped = backend(project, "stop");
        assert!(stopped.status.success(), "{}", stdout(&stopped));
    }
    assert_eq!(stdout(&status(&[])), none_here);
    assert_eq!(stdout(&status(&["--all"])), "no mcpls backend is running\n");
}

/// A stopped server stays down through a tool call, and start and
/// restart each launch one fresh process and retire the one before.
#[cfg(unix)]
#[test]
fn lsp_commands_stop_start_and_restart_a_server() {
    let project = Project::new(60_000).with_lifecycle_server();
    let mut frontend = project.frontend();
    project.assert_attaches(&mut frontend);
    project.wait_for("the server to run", |p| {
        stdout(&p.lsp(&["status"])).contains("fake  running")
    });
    let first = project.lifecycle_events("start")[0];

    let stop = project.lsp(&["stop", "fake"]);
    assert!(stop.status.success(), "{}", stdout(&stop));
    assert_eq!(stdout(&stop), "fake  stopped\n");
    project.wait_for("the stopped process to exit", |p| {
        p.lifecycle_events("exit").contains(&first)
    });

    let call = frontend.symbols(&project.root().join("a.fake"));
    assert!(
        failure_text(&call).contains("stopped; run `mcpls lsp start fake`"),
        "{call}"
    );
    assert_eq!(project.lifecycle_events("start").len(), 1);

    let start = project.lsp(&["start", "fake"]);
    assert!(start.status.success(), "{}", stdout(&start));
    assert_eq!(stdout(&start), "fake  running\n");
    assert_eq!(project.lifecycle_events("start").len(), 2);
    let second = project.lifecycle_events("start")[1];

    let restart = project.lsp(&["restart", "fake"]);
    assert!(restart.status.success(), "{}", stdout(&restart));
    assert_eq!(stdout(&restart), "fake  running\n");
    assert_eq!(project.lifecycle_events("start").len(), 3);
    project.wait_for("the replaced process to exit", |p| {
        p.lifecycle_events("exit").contains(&second)
    });
    assert_tool_success(&frontend.symbols(&project.root().join("a.fake")));
}

#[cfg(unix)]
#[test]
fn lsp_commands_name_the_servers_that_apply_when_given_an_unknown_one() {
    let project = Project::new(60_000).with_lifecycle_server();
    let mut frontend = project.frontend();
    project.assert_attaches(&mut frontend);

    let start = project.lsp(&["start", "nope"]);
    assert!(!start.status.success());
    assert_eq!(
        stdout(&start),
        "no language server named nope applies here; these do: fake\n"
    );
}

#[cfg(unix)]
#[test]
fn lsp_install_starts_the_installed_server_in_a_running_backend() {
    let project = Project::new(60_000).with_installable_lifecycle_server();
    let mut frontend = project.frontend();
    project.assert_attaches(&mut frontend);
    project.wait_for("the server to be recorded missing", |p| {
        stdout(&p.lsp(&["status"])).contains("fake  not installed")
    });

    let install = project.lsp(&["install", "fake"]);
    assert!(install.status.success(), "{}", stdout(&install));
    assert!(
        stdout(&install).ends_with("fake  installed\n"),
        "{}",
        stdout(&install)
    );

    project.wait_for("the backend to start it", |p| {
        stdout(&p.lsp(&["status"])).contains("fake  running")
    });
}

#[test]
fn lsp_status_with_no_backend_says_where_one_comes_from() {
    let project = Project::new(60_000);
    let status = project.lsp(&["status"]);
    assert!(!status.status.success());
    assert_eq!(
        stdout(&status),
        format!(
            "no backend serves {}; one starts with an agent session\n",
            project.root().display()
        )
    );
}
