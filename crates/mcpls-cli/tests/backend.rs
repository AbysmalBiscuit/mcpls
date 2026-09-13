//! A frontend and its backend as the host sees them: separate processes
//! sharing one endpoint.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(deprecated)]
#![cfg_attr(windows, allow(dead_code))]

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
            runtime: TempDir::new().unwrap(),
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
            format!("[backend]\nidle_shutdown_ms = {idle_ms}\n{servers}"),
        )
        .unwrap();
    }

    fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::cargo_bin("mcpls").unwrap();
        command
            .env_remove("MCPLS_LOG")
            .env_remove("MCPLS_CONFIG")
            .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
            .env_remove("MCPLS_LOG_JSON")
            .env_remove("MCPLS_NO_BACKEND")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env("TMPDIR", self.runtime.path())
            .env("USER", &self.user)
            .env("USERNAME", &self.user)
            .current_dir(cwd);
        command
    }

    fn frontend(&self) -> Frontend {
        self.frontend_in(&self.root(), &[])
    }

    fn frontend_in(&self, cwd: &Path, extra: &[&str]) -> Frontend {
        let mut command = self.command(cwd);
        command.arg("--config").arg(self.config()).args(extra);
        Frontend::spawn(command)
    }

    /// `mcpls hook doctor`'s report for this project.
    fn doctor(&self) -> String {
        let output = self
            .command(&self.root())
            .env("CLAUDE_PROJECT_DIR", self.root())
            .args(["hook", "doctor"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// The backend's pid, or `None` when nothing answers.
    fn backend_pid(&self) -> Option<u32> {
        self.doctor()
            .lines()
            .find_map(|line| line.strip_prefix("backend pid: "))
            .and_then(|pid| pid.parse().ok())
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
    let pid = project.backend_pid().unwrap();
    assert!(
        frontend.close(Duration::from_secs(5)),
        "the host did not see EOF"
    );
    project.wait_for("the backend to exit", |p| p.backend_pid().is_none());
    assert!(!alive(pid));
}
