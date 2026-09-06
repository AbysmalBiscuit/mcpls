//! MCP client simulator for end-to-end testing.
//!
//! This module provides a synchronous MCP client that spawns the mcpls binary
//! and communicates via stdio using the JSON-RPC 2.0 protocol.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// How long `call_tool` retries an LSP `-32801` ("content modified")
/// response before giving up.
const CONTENT_MODIFIED_RETRY_BUDGET: Duration = Duration::from_secs(20);

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
        // Use empty config to avoid LSP server initialization timeouts
        let config_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/empty_config.toml");

        Self::spawn_with_args(&[
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid config path"))?,
        ])
    }

    /// Spawn mcpls process with custom arguments.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The mcpls binary cannot be found or spawned
    /// - stdin or stdout cannot be captured
    pub fn spawn_with_args(args: &[&str]) -> Result<Self> {
        let binary_path = binary_under_test()?;

        let mut process = Command::new(binary_path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
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

        Ok(Self {
            process,
            stdin,
            stdout: BufReader::new(stdout),
            request_id: 0,
            pending_notifications: Vec::new(),
        })
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
                // rust-analyzer (and other servers) answer -32801 when a
                // concurrent re-analysis invalidated the snapshot a request
                // was made against; the LSP spec's documented response is to
                // retry, not to treat it as a real failure.
                Err(e) if e.to_string().contains("-32801") && Instant::now() < retry_deadline => {
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
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
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
