//! MCP client simulator for end-to-end testing.
//!
//! This module provides a synchronous MCP client that spawns the mcpls binary
//! and communicates via stdio using the JSON-RPC 2.0 protocol.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// How long `call_tool` retries an LSP `-32801` ("content modified")
/// response before giving up.
const CONTENT_MODIFIED_RETRY_BUDGET: Duration = Duration::from_secs(20);

/// How many total attempts `call_tool` makes against a persistent `-32801`.
/// Bounded separately from `CONTENT_MODIFIED_RETRY_BUDGET` so a systematic
/// regression that always answers -32801 fails in a few seconds instead of
/// being absorbed for the whole time budget before finally being reported.
const CONTENT_MODIFIED_RETRY_ATTEMPTS: u32 = 3;

const STDIO_READY_MARKER: &str = "Listening for MCP requests on stdio...";
#[allow(dead_code)]
const STDIO_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Simulates an MCP client (like Claude Code) for E2E testing.
///
/// This client spawns the mcpls binary as a child process and communicates
/// with it via stdio using JSON-RPC 2.0 protocol.
///
/// # Examples
///
/// ```no_run
/// use mcpls_core::tests::e2e::mcp_client::McpClient;
///
/// let mut client = McpClient::spawn()?;
/// let response = client.initialize()?;
/// assert!(response.get("result").is_some());
/// # Ok::<(), anyhow::Error>(())
/// ```
pub struct McpClient {
    process: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_id: i64,
    /// Server-pushed notifications (no matching request `id`) collected while
    /// waiting for a request/response round-trip. Drained via `take_notifications`.
    pending_notifications: Vec<Value>,
    /// Keeps the default client's isolated workspace alive. Separate directories
    /// prevent unrelated clients from sharing a hook socket. Shared-workspace
    /// tests keep their caller-owned directory alive instead.
    _cwd: Option<tempfile::TempDir>,
    stderr_reader: Option<JoinHandle<()>>,
    #[allow(dead_code)]
    stdio_ready: Option<Receiver<()>>,
}

/// The workspace root, from this crate's manifest directory.
fn workspace_root() -> Result<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .ok_or_else(|| anyhow::anyhow!("CARGO_MANIFEST_DIR has no workspace root above it"))
}

/// The newest modification time under `dir`, ignoring anything unreadable.
fn newest_source_time(dir: &Path) -> Option<SystemTime> {
    let mut newest = None;
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let candidate = if path.is_dir() {
            newest_source_time(&path)
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            path.metadata().ok().and_then(|meta| meta.modified().ok())
        } else {
            None
        };
        if candidate > newest {
            newest = candidate;
        }
    }
    newest
}

/// The mcpls binary these tests drive.
///
/// `MCPLS_E2E_BINARY` names one outright, which is how CI points at the
/// artifact it downloaded. Otherwise the binary cargo built for this test
/// target, and failing that the workspace's debug build.
///
/// That last one is the trap this guards: `cargo test -p mcpls-core` does not
/// rebuild `mcpls`, so an e2e suite left to find it on its own can pass
/// against a binary predating every change under test. It is checked against
/// the newest file the binary is built from rather than trusted. Test sources
/// are not among those, so editing a test does not force a rebuild.
fn binary_under_test() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("MCPLS_E2E_BINARY") {
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcpls") {
        return Ok(PathBuf::from(path));
    }

    let root = workspace_root()?;
    let binary = root.join(format!(
        "target/debug/mcpls{}",
        std::env::consts::EXE_SUFFIX
    ));
    let built = binary
        .metadata()
        .and_then(|meta| meta.modified())
        .with_context(|| {
            format!(
                "{} is not built. Run `cargo build --bin mcpls` before the e2e suite, or point \
                 MCPLS_E2E_BINARY at a binary",
                binary.display()
            )
        })?;

    let newest_source = ["crates/mcpls-core/src", "crates/mcpls-cli/src"]
        .iter()
        .filter_map(|dir| newest_source_time(&root.join(dir)))
        .max();
    if let Some(newest) = newest_source
        && newest > built
    {
        anyhow::bail!(
            "{} is older than the sources it is built from, so the e2e suite would test an \
             earlier version of mcpls. Run `cargo build --bin mcpls`, or point MCPLS_E2E_BINARY \
             at a binary",
            binary.display()
        );
    }

    Ok(binary)
}

impl McpClient {
    /// Spawn mcpls process and connect via stdio.
    ///
    /// Uses an empty configuration file for testing the MCP protocol layer only.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The mcpls binary cannot be found or spawned
    /// - stdin or stdout cannot be captured
    pub fn spawn() -> Result<Self> {
        Self::spawn_with_empty_config(false)
    }

    /// Spawn mcpls and wait until its stdio transport is ready for requests.
    ///
    /// The readiness marker is emitted after signal registration and before the
    /// transport waits for the client's initialize request.
    #[allow(dead_code)]
    pub(crate) fn spawn_and_wait_for_stdio() -> Result<Self> {
        let client = Self::spawn_with_empty_config(true)?;
        client.wait_for_stdio_ready()?;
        Ok(client)
    }

    /// Spawn mcpls with the empty protocol-only configuration.
    fn spawn_with_empty_config(capture_stderr: bool) -> Result<Self> {
        // Use empty config to avoid LSP server initialization timeouts
        let config_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/empty_config.toml");
        let config_path = config_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid config path"))?;
        let mut args = vec!["--no-backend", "--config", config_path];
        if capture_stderr {
            args.extend(["--log-level", "info"]);
            Self::spawn_with_args_and_stderr(&args, true)
        } else {
            Self::spawn_with_args(&args)
        }
    }

    /// Spawn mcpls process with custom arguments.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The mcpls binary cannot be found or spawned
    /// - stdin or stdout cannot be captured
    pub fn spawn_with_args(args: &[&str]) -> Result<Self> {
        Self::spawn_with_args_and_stderr(args, false)
    }

    /// Spawn mcpls with an optional stderr reader used by readiness-sensitive tests.
    fn spawn_with_args_and_stderr(args: &[&str], capture_stderr: bool) -> Result<Self> {
        let binary_path = binary_under_test()?;
        let cwd = tempfile::tempdir().context("failed to create a working directory")?;
        let mut args = args.to_vec();
        if !args.contains(&"--no-backend") {
            args.insert(0, "--no-backend");
        }
        let mut command = Command::new(binary_path);
        command.args(args).current_dir(cwd.path());
        Self::spawn_command(command, Some(cwd), capture_stderr)
    }

    /// Spawn in a caller-owned workspace with a child-only session environment.
    /// `None` removes the variable; `Some("")` exports an empty value.
    #[allow(dead_code)]
    pub(crate) fn spawn_in_workspace(
        args: &[&str],
        workspace: &Path,
        session: Option<&str>,
    ) -> Result<Self> {
        let mut command = Command::new(binary_under_test()?);
        command.args(args).current_dir(workspace);
        command.env_remove("MCPLS_NO_BACKEND");
        match session {
            Some(session) => command.env("CLAUDE_CODE_SESSION_ID", session),
            None => command.env_remove("CLAUDE_CODE_SESSION_ID"),
        };
        Self::spawn_command(command, None, false)
    }

    fn spawn_command(
        mut command: Command,
        cwd: Option<tempfile::TempDir>,
        capture_stderr: bool,
    ) -> Result<Self> {
        let mut process = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if capture_stderr {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .spawn()
            .context("failed to spawn mcpls binary")?;

        let stdin = process
            .stdin
            .take()
            .context("failed to capture stdin of mcpls process")?;

        let stdout = process
            .stdout
            .take()
            .context("failed to capture stdout of mcpls process")?;

        let stderr = if capture_stderr {
            Some(
                process
                    .stderr
                    .take()
                    .context("failed to capture stderr of mcpls process")?,
            )
        } else {
            None
        };

        let (stderr_reader, stdio_ready): (Option<JoinHandle<()>>, Option<Receiver<()>>) = stderr
            .map_or_else(
                || (None, None),
                |stderr| {
                    let (ready_tx, ready_rx) = mpsc::channel();
                    let reader = std::thread::spawn(move || {
                        for line in BufReader::new(stderr).lines() {
                            match line {
                                Ok(line) => {
                                    eprintln!("{line}");
                                    if line.contains(STDIO_READY_MARKER) {
                                        let _ = ready_tx.send(());
                                    }
                                }
                                Err(error) => {
                                    eprintln!("failed to read mcpls stderr: {error}");
                                    break;
                                }
                            }
                        }
                    });
                    (Some(reader), Some(ready_rx))
                },
            );

        Ok(Self {
            process,
            stdin,
            stdout: BufReader::new(stdout),
            request_id: 0,
            pending_notifications: Vec::new(),
            _cwd: cwd,
            stderr_reader,
            stdio_ready,
        })
    }

    #[allow(dead_code)]
    fn wait_for_stdio_ready(&self) -> Result<()> {
        let receiver = self
            .stdio_ready
            .as_ref()
            .context("stdio readiness is unavailable for this client")?;

        match receiver.recv_timeout(STDIO_READY_TIMEOUT) {
            Ok(()) => Ok(()),
            Err(RecvTimeoutError::Timeout) => {
                anyhow::bail!("mcpls did not report stdio readiness within {STDIO_READY_TIMEOUT:?}")
            }
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("mcpls stderr closed before reporting stdio readiness")
            }
        }
    }

    /// Drain and return server-pushed notifications collected so far (e.g.
    /// `notifications/resources/updated`).
    ///
    /// Notifications have no JSON-RPC `id` and may arrive interleaved with
    /// request/response traffic on the same stdout stream; `send_request` queues
    /// them here instead of misinterpreting them as the response it is waiting for.
    #[allow(dead_code)]
    pub fn take_notifications(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.pending_notifications)
    }

    /// Send MCP initialize request.
    ///
    /// This establishes the MCP connection and negotiates protocol version.
    /// After receiving the initialize response, sends the initialized notification
    /// as required by the MCP protocol.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The request cannot be sent
    /// - The response cannot be read or parsed
    /// - The server returns an error response
    pub fn initialize(&mut self) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "mcpls-e2e-test",
                    "version": "0.1.0"
                }
            }
        });

        let response = self.send_request(&request)?;

        // Send initialized notification as required by MCP protocol
        self.send_notification("notifications/initialized", &json!({}))?;

        Ok(response)
    }

    /// List available MCP tools.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The request cannot be sent
    /// - The response cannot be read or parsed
    /// - The server returns an error response
    #[allow(dead_code)]
    pub fn list_tools(&mut self) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "tools/list",
            "params": {}
        });

        self.send_request(&request)
    }

    /// Call a tool by name with parameters.
    ///
    /// # Parameters
    ///
    /// - `name`: The name of the tool to call
    /// - `arguments`: JSON object with tool-specific parameters
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The request cannot be sent
    /// - The response cannot be read or parsed
    /// - The server returns an error response
    /// - The tool does not exist
    /// - The parameters are invalid
    pub fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<Value> {
        let retry_deadline = Instant::now() + CONTENT_MODIFIED_RETRY_BUDGET;
        let mut attempt = 1u32;
        loop {
            let request = json!({
                "jsonrpc": "2.0",
                "id": self.next_id(),
                "method": "tools/call",
                "params": {
                    "name": name,
                    "arguments": arguments
                }
            });

            match self.send_request(&request) {
                Ok(response) => return Ok(response),
                // A concurrent re-analysis invalidated the document snapshot
                // the request was made against. The call is idempotent and
                // the test needs a result, so re-issue it against the
                // server's now-current state rather than cancelling as the
                // LSP spec's baseline advice for this code would have a
                // capability-aware client do. `CONTENT_MODIFIED_RETRY_ATTEMPTS`
                // bounds this separately from the time budget so a
                // persistent failure (not a one-off race) still fails fast.
                Err(e)
                    if attempt < CONTENT_MODIFIED_RETRY_ATTEMPTS
                        && Instant::now() < retry_deadline
                        && e.to_string().contains("-32801 - content modified") =>
                {
                    eprintln!(
                        "call_tool({name}): retrying after LSP -32801 content-modified \
                         (attempt {attempt} of {CONTENT_MODIFIED_RETRY_ATTEMPTS})"
                    );
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// List MCP resources (`resources/list`).
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be sent or the server returns an error.
    #[allow(dead_code)]
    pub fn list_resources(&mut self) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "resources/list",
            "params": {}
        });
        self.send_request(&request)
    }

    /// Read an MCP resource by URI (`resources/read`).
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be sent or the server returns an error.
    #[allow(dead_code)]
    pub fn read_resource(&mut self, uri: &str) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "resources/read",
            "params": { "uri": uri }
        });
        self.send_request(&request)
    }

    /// Subscribe to a resource (`resources/subscribe`).
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be sent or the server returns an error.
    #[allow(dead_code)]
    pub fn subscribe_resource(&mut self, uri: &str) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "resources/subscribe",
            "params": { "uri": uri }
        });
        self.send_request(&request)
    }

    /// Unsubscribe from a resource (`resources/unsubscribe`).
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be sent or the server returns an error.
    #[allow(dead_code)]
    pub fn unsubscribe_resource(&mut self, uri: &str) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "resources/unsubscribe",
            "params": { "uri": uri }
        });
        self.send_request(&request)
    }

    /// Send a raw JSON-RPC request and return the response.
    ///
    /// The server may push notifications (e.g. `notifications/resources/updated`)
    /// on the same stdout stream before writing the response; those are queued into
    /// `pending_notifications` rather than being mistaken for the response.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The request cannot be serialized or sent
    /// - The response cannot be read or parsed
    /// - The server returns an error response
    fn send_request(&mut self, request: &Value) -> Result<Value> {
        let request_str = serde_json::to_string(request)?;
        writeln!(self.stdin, "{request_str}")?;
        self.stdin.flush()?;

        let expected_id = request.get("id").cloned();

        let response = loop {
            let mut line = String::new();
            self.stdout
                .read_line(&mut line)
                .context("failed to read response from mcpls")?;

            let value: Value =
                serde_json::from_str(&line).context("failed to parse JSON-RPC message")?;

            if value.get("id") == expected_id.as_ref() {
                break value;
            }
            self.pending_notifications.push(value);
        };

        if let Some(error) = response.get("error") {
            anyhow::bail!("MCP error: {error:?}");
        }

        // rmcp 1.8.0+: deserialization failures return isError=true inside a successful
        // tools/call result instead of a JSON-RPC error (PR #894).
        if response
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let content = response["result"]["content"].to_string();
            anyhow::bail!("MCP tool error (isError=true): {content}");
        }

        Ok(response)
    }

    /// Send a notification (request without expecting a response).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The notification cannot be serialized or sent
    fn send_notification(&mut self, method: &str, params: &Value) -> Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let notification_str = serde_json::to_string(&notification)?;
        writeln!(self.stdin, "{notification_str}")?;
        self.stdin.flush()?;

        Ok(())
    }

    /// Get the next request ID and increment the counter.
    // False positive: clippy suggests const fn, but const fn cannot mutate self
    #[allow(clippy::missing_const_for_fn)]
    fn next_id(&mut self) -> i64 {
        self.request_id += 1;
        self.request_id
    }

    /// Return the OS process ID of the spawned mcpls process.
    #[allow(dead_code)]
    pub(crate) fn pid(&self) -> u32 {
        self.process.id()
    }

    /// Non-blocking check for whether the process has exited.
    ///
    /// # Errors
    ///
    /// Returns an error if the OS query for the process status fails.
    #[allow(dead_code)]
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.process.try_wait()
    }

    /// Query the backend selected for a workspace through hook doctor.
    #[allow(dead_code)]
    pub(crate) fn backend_pid(workspace: &Path) -> Result<Option<u32>> {
        let output = Command::new(binary_under_test()?)
            .args(["hook", "doctor"])
            .current_dir(workspace)
            .env_remove("MCPLS_NO_BACKEND")
            .output()
            .context("failed to query backend status")?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("backend pid: "))
            .and_then(|pid| pid.parse().ok()))
    }

    /// Run the hook a host fires on a prompt. On Windows it starts the
    /// backend a frontend asked for.
    #[allow(dead_code)]
    pub(crate) fn fire_hook(workspace: &Path) -> Result<()> {
        use std::io::Write as _;

        let mut child = Command::new(binary_under_test()?)
            .arg("hook")
            .current_dir(workspace)
            .env("CLAUDE_PROJECT_DIR", workspace)
            .env_remove("MCPLS_NO_BACKEND")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .context("failed to run the hook")?;
        child
            .stdin
            .take()
            .context("the hook has no stdin")?
            .write_all(br#"{"hook_event_name":"UserPromptSubmit","session_id":"e2e-hook"}"#)?;
        let status = child.wait()?;
        anyhow::ensure!(status.success(), "the hook exited with {status}");
        Ok(())
    }

    /// Wait until hook doctor reports no backend for a workspace.
    #[allow(dead_code)]
    pub(crate) fn wait_for_backend_exit(workspace: &Path) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if Self::backend_pid(workspace)?.is_none() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("backend did not exit for {}", workspace.display());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Requires mcpls binary built"]
    fn test_mcp_client_spawn() {
        let client = McpClient::spawn();
        assert!(client.is_ok(), "Should successfully spawn mcpls binary");
    }

    #[test]
    #[ignore = "Requires mcpls binary built"]
    fn test_request_id_increment() -> Result<()> {
        let mut client = McpClient::spawn()?;
        assert_eq!(client.next_id(), 1);
        assert_eq!(client.next_id(), 2);
        assert_eq!(client.next_id(), 3);
        Ok(())
    }
}
