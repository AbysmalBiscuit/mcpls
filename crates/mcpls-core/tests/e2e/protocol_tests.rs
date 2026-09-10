//! End-to-end tests for MCP protocol implementation.
//!
//! These tests validate the complete MCP protocol flow by spawning the mcpls
//! binary and communicating with it as a real MCP client would.

use std::path::Path;
use std::time::{Duration, Instant};
use std::{fs, thread};

use anyhow::{Context, Result};
use serde_json::json;
use tempfile::TempDir;

use super::diagnostics_fixture;
use super::mcp_client::McpClient;

fn toml_array(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn toml_patterns(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn write_fixture_config(
    config_path: &Path,
    language_id: &str,
    command: &str,
    args: &[String],
    file_patterns: &[&str],
    workspace_mapping: bool,
    handles: &[&str],
) -> Result<()> {
    let workspace = config_path
        .parent()
        .context("fixture config path must have a parent")?;
    let workspace_mapping = if workspace_mapping {
        "\n[[workspace.language_extensions]]\nextensions = [\"ex\"]\nlanguage_id = \"elixir\"\n"
    } else {
        ""
    };
    let file_patterns = if file_patterns.is_empty() {
        String::new()
    } else {
        format!("file_patterns = [{}]\n", toml_patterns(file_patterns))
    };
    let handles = if handles.is_empty() {
        String::new()
    } else {
        format!("handles = [{}]\n", toml_patterns(handles))
    };
    let workspace = toml::Value::String(workspace.to_string_lossy().into_owned()).to_string();
    let args = toml_array(args);
    let config = format!(
        "[workspace]\nroots = [{workspace}]{workspace_mapping}\n[diagnostics.hooks]\nenabled = false\n\n[[lsp_servers]]\nlanguage_id = {language_id:?}\ncommand = {command:?}\nargs = [{args}]\n{file_patterns}{handles}"
    );
    fs::write(config_path, config)?;
    Ok(())
}

fn call_hover_when_ready(client: &mut McpClient, file_path: &Path) -> Result<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match client.call_tool(
            "get_hover",
            &json!({
                "file_path": file_path,
                "line": 0,
                "character": 0,
            }),
        ) {
            Ok(response) => return Ok(response),
            Err(error)
                if error.to_string().contains("initializing") && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    }
}

fn call_workspace_symbol_when_ready(client: &mut McpClient) -> Result<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match client.call_tool(
            "workspace_symbol_search",
            &json!({"query": "workspace", "limit": 10}),
        ) {
            Ok(response) => return Ok(response),
            Err(error)
                if error.to_string().contains("initializing") && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    }
}

fn run_hover_case(
    language_id: &str,
    file_name: &str,
    sentinel: &str,
    file_patterns: &[&str],
    workspace_mapping: bool,
) -> Result<()> {
    let workspace = TempDir::new()?;
    run_hover_case_in_workspace(
        &workspace,
        language_id,
        file_name,
        sentinel,
        file_patterns,
        workspace_mapping,
    )
}

fn run_hover_case_in_workspace(
    workspace: &TempDir,
    language_id: &str,
    file_name: &str,
    sentinel: &str,
    file_patterns: &[&str],
    workspace_mapping: bool,
) -> Result<()> {
    let script = diagnostics_fixture::write_hover_server(workspace.path())?;
    let config_path = workspace.path().join("mcpls.toml");
    let args = vec![script.to_string_lossy().into_owned(), sentinel.to_string()];
    write_fixture_config(
        &config_path,
        language_id,
        "python3",
        &args,
        file_patterns,
        workspace_mapping,
        &[],
    )?;
    let file_path = workspace.path().join(file_name);
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&file_path, "def fixture():\n    return 1\n")?;

    let config_arg = config_path.to_string_lossy().into_owned();
    let mut client = McpClient::spawn_with_args(&["--config", &config_arg])?;
    client.initialize()?;
    let response = call_hover_when_ready(&mut client, &file_path)?;
    assert!(
        response.to_string().contains(sentinel),
        "hover response should contain {sentinel}, got {response}"
    );
    Ok(())
}

#[test]
#[ignore = "Requires mcpls binary built"]
fn i1_t1_file_pattern_mapping_routes_hover() -> Result<()> {
    run_hover_case(
        "elixir",
        "lib/example.ex",
        "elixir-fixture",
        &["**/*.ex"],
        false,
    )
}

#[test]
#[ignore = "Requires mcpls binary built"]
fn i1_t1_workspace_mapping_routes_hover() -> Result<()> {
    run_hover_case("elixir", "lib/example.ex", "elixir-fixture", &[], true)
}

#[test]
#[ignore = "Requires mcpls binary built"]
fn i1_t1_builtin_command_override_preserves_mapping() -> Result<()> {
    let workspace = TempDir::new()?;
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )?;
    run_hover_case_in_workspace(
        &workspace,
        "rust",
        "lib/example.rs",
        "rust-fixture",
        &[],
        false,
    )
}

#[test]
#[ignore = "Requires mcpls binary built"]
fn i1_t1_workspace_only_server_without_mapping_is_accepted() -> Result<()> {
    let workspace = TempDir::new()?;
    let script = diagnostics_fixture::write_hover_server(workspace.path())?;
    let config_path = workspace.path().join("mcpls.toml");
    let args = vec![
        script.to_string_lossy().into_owned(),
        "workspace-fixture".to_string(),
    ];
    write_fixture_config(
        &config_path,
        "elixir",
        "python3",
        &args,
        &[],
        false,
        &["workspace_symbols"],
    )?;

    let config_arg = config_path.to_string_lossy().into_owned();
    let mut client = McpClient::spawn_with_args(&["--config", &config_arg])?;
    client.initialize()?;
    let response = call_workspace_symbol_when_ready(&mut client)?;
    assert!(
        response.to_string().contains("workspace-fixture"),
        "workspace symbol response should contain the fixture sentinel, got {response}"
    );
    Ok(())
}

/// Test the MCP initialize handshake.
///
/// Validates that the server:
/// - Accepts the initialize request
/// - Returns the correct protocol version
/// - Exposes tool capabilities
/// - Provides server information
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_initialize_handshake() -> Result<()> {
    let mut client = McpClient::spawn()?;

    let response = client.initialize()?;

    assert!(
        response.get("result").is_some(),
        "Response should have 'result' field"
    );

    let result = &response["result"];

    assert_eq!(
        result["protocolVersion"], "2024-11-05",
        "Protocol version should match"
    );

    assert!(
        result["capabilities"]["tools"].is_object(),
        "Should expose tools capability"
    );

    assert_eq!(
        result["serverInfo"]["name"], "mcpls",
        "Server name should be 'mcpls'"
    );

    Ok(())
}

/// Test listing all available MCP tools.
///
/// Validates that `tools/list` returns exactly the expected set: every name
/// below is present, and nothing else is.
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_list_tools() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let response = client.list_tools()?;

    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools should be an array"));

    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    let expected_names = [
        "get_hover",
        "get_definition",
        "get_references",
        "get_diagnostics",
        "rename_symbol",
        "get_completions",
        "get_document_symbols",
        "format_document",
        "workspace_symbol_search",
        "get_code_actions",
        "prepare_call_hierarchy",
        "get_incoming_calls",
        "get_outgoing_calls",
        "get_cached_diagnostics",
        "get_server_logs",
        "get_server_messages",
        "get_signature_help",
        "go_to_implementation",
        "go_to_type_definition",
        "get_inlay_hints",
        "apply_code_action",
        "get_new_diagnostics",
    ];

    for expected in &expected_names {
        assert!(tool_names.contains(expected), "Should have {expected} tool");
    }
    assert_eq!(
        tools.len(),
        expected_names.len(),
        "tools/list returned something the expected set does not name: {tool_names:?}"
    );

    Ok(())
}

/// Test that all tools have valid JSON schemas.
///
/// Validates that each tool has:
/// - A name (string)
/// - A description (string)
/// - An input schema (object)
/// - Schema with "object" type
/// - Schema with properties
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_tool_schemas() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let response = client.list_tools()?;
    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools should be an array"));

    for tool in tools {
        let tool_name = tool["name"]
            .as_str()
            .unwrap_or_else(|| panic!("Tool should have name field"));

        assert!(
            tool["name"].is_string(),
            "Tool '{tool_name}' should have name as string"
        );

        assert!(
            tool["description"].is_string(),
            "Tool '{tool_name}' should have description as string"
        );

        assert!(
            tool["inputSchema"].is_object(),
            "Tool '{tool_name}' should have inputSchema as object"
        );

        let schema = &tool["inputSchema"];

        assert_eq!(
            schema["type"], "object",
            "Tool '{tool_name}' schema type should be 'object'"
        );

        assert!(
            schema["properties"].is_object(),
            "Tool '{tool_name}' schema should have properties object"
        );
    }

    Ok(())
}

/// Test calling a non-existent tool.
///
/// Validates that the server properly rejects invalid tool calls
/// with an appropriate error response.
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_invalid_tool_call() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let result = client.call_tool("non_existent_tool", &json!({}));

    assert!(result.is_err(), "Should return error for non-existent tool");

    if let Err(err) = result {
        let error_msg = format!("{err:?}");
        assert!(
            error_msg.contains("error") || error_msg.contains("Error"),
            "Error message should indicate failure"
        );
    }

    Ok(())
}

/// Test calling a tool with missing required parameters.
///
/// Validates that the server properly validates tool parameters
/// and rejects calls with missing required fields.
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_tool_call_missing_params() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let result = client.call_tool("get_hover", &json!({}));

    assert!(
        result.is_err(),
        "Should return error for missing required parameters"
    );

    if let Err(err) = result {
        let error_msg = format!("{err:?}");
        assert!(
            error_msg.contains("error") || error_msg.contains("Error"),
            "Error message should indicate parameter validation failure"
        );
    }

    Ok(())
}

/// Test calling `get_hover` with invalid file path.
///
/// Validates that the server properly handles file path validation
/// and returns appropriate errors for non-existent files.
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_tool_call_invalid_file() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let result = client.call_tool(
        "get_hover",
        &json!({
            "file_path": "/nonexistent/path/to/file.rs",
            "line": 1,
            "character": 1
        }),
    );

    assert!(result.is_err(), "Should return error for non-existent file");

    Ok(())
}

/// Test calling `get_definition` with out-of-bounds position.
///
/// Validates that the server handles position validation correctly.
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_tool_call_invalid_position() -> Result<()> {
    use std::fs;

    use tempfile::TempDir;

    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let temp_dir = TempDir::new()?;
    let test_file = temp_dir.path().join("test.rs");
    fs::write(&test_file, "fn main() {}\n")?;

    let result = client.call_tool(
        "get_definition",
        &json!({
            "file_path": test_file.to_string_lossy(),
            "line": 9999,
            "character": 9999
        }),
    );

    // Server should either return error or empty result for out-of-bounds position
    // Both are acceptable behaviors
    if let Ok(response) = result {
        // If successful, result should indicate no definition found
        let result_field = &response["result"];
        // Accept both null/empty results as valid responses
        assert!(
            result_field.is_null() || result_field.is_array() || result_field.is_object(),
            "Should return null or empty result for invalid position"
        );
    }
    // Error response is also acceptable

    Ok(())
}

/// Test the complete workflow: initialize → list → call tool.
///
/// This test validates the typical usage pattern of an MCP client.
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_complete_workflow() -> Result<()> {
    let mut client = McpClient::spawn()?;

    // Step 1: Initialize
    let init_response = client.initialize()?;
    assert!(init_response.get("result").is_some());

    // Step 2: List tools
    let list_response = client.list_tools()?;
    let tools = list_response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools should be an array"));
    assert!(!tools.is_empty(), "Should have tools available");

    // Step 3: Verify we can attempt to call a tool (even if it fails due to no LSP)
    // This validates the protocol flow works end-to-end
    let _result = client.call_tool("get_diagnostics", &json!({"file_path": "test.rs"}));
    // We don't assert success here because LSP servers may not be configured
    // The important part is that the protocol flow works

    Ok(())
}

/// Test multiple sequential requests on the same connection.
///
/// Validates that:
/// - The connection remains stable across multiple requests
/// - Request IDs increment correctly
/// - The server handles concurrent operations properly
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_multiple_requests() -> Result<()> {
    let mut client = McpClient::spawn()?;

    // Multiple initialize calls should work (idempotent)
    let response1 = client.initialize()?;
    assert!(response1.get("result").is_some());

    let response2 = client.list_tools()?;
    assert!(response2.get("result").is_some());

    let response3 = client.list_tools()?;
    assert!(response3.get("result").is_some());

    // Responses should have different IDs
    assert_ne!(
        response1.get("id"),
        response2.get("id"),
        "Different requests should have different IDs"
    );
    assert_ne!(
        response2.get("id"),
        response3.get("id"),
        "Different requests should have different IDs"
    );

    Ok(())
}

/// `get_new_diagnostics` in protocol-only mode (every built-in LSP server
/// disabled, per the default e2e config) must answer normally instead of
/// advising a retry.
///
/// With no server ever spawned, nothing ever settles, so nothing would
/// otherwise adopt a diagnostics baseline -- `has_baseline()` would stay
/// false for the process's whole life. There is genuinely nothing starting
/// up and nothing to report, so the response must carry no `note` and empty
/// `changed`/`cleared`, not the "still starting up" shape repeated forever.
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_new_diagnostics_in_protocol_only_mode() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let response = client.call_tool("get_new_diagnostics", &json!({}))?;
    let is_error = response["result"]["isError"].as_bool().unwrap_or(false);
    assert!(!is_error, "call should succeed, got {response}");

    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected text content, got {response}"));
    let payload: serde_json::Value = serde_json::from_str(text)?;

    assert!(
        payload.get("note").is_none(),
        "protocol-only mode has nothing starting up and nothing to report, so there is no \
         retry to advise; got {payload}"
    );
    assert_eq!(payload["changed"].as_array().map(Vec::len), Some(0));
    assert_eq!(payload["cleared"].as_array().map(Vec::len), Some(0));

    Ok(())
}

/// Test that mcpls exits promptly on `SIGTERM` while the client's stdin
/// write end is still open (regression test for #308).
///
/// The MCP stdio transport is backed by `tokio::io::stdin()`, which parks an
/// uncancellable blocking-pool thread in a raw `read()` syscall. Without the
/// `std::process::exit` fix in `mcpls-cli`'s `main`, `#[tokio::main]`'s
/// runtime-shutdown wait for that thread would hang indefinitely as long as
/// the client (this test, via `McpClient`) keeps stdin's write end open.
///
/// Sending `SIGTERM` immediately after the handshake completes (no
/// artificial delay) also touches the tail of #318's window — the narrow gap
/// between `run_stdio`'s two `select!` blocks — but only weakly: signaling
/// this soon after `initialize()` returns reproduced the pre-fix bug in just
/// 1/15 runs, since the client-side I/O latency before the `kill` command
/// even runs dwarfs that gap. `test_e2e_sigterm_exits_promptly_during_handshake_wait`
/// below is the reliable reproducer for #318 (5/5 against pre-fix code).
#[test]
#[cfg(unix)]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_sigterm_exits_promptly_while_client_stdin_open() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    // No delay here is intentional: `run_stdio` now registers its SIGTERM
    // handler before awaiting the handshake at all (see #318), so the signal
    // is raced against the handshake/select loop from the moment the
    // process starts. Sending SIGTERM immediately after `initialize()`
    // returns exercises the narrowest part of that window instead of
    // masking it behind an artificial delay.
    let pid = client.pid();
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()?;
    assert!(
        status.success(),
        "failed to send SIGTERM to mcpls (pid {pid})"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(exit_status) = client.try_wait()? {
            // Distinguishes a graceful `process::exit(0)` from the process
            // being killed outright by the default SIGTERM disposition
            // (e.g. if the signal handler failed to register) -- the latter
            // would also make `try_wait` return `Some`, but with no LSP
            // shutdown having run. On Unix, `code()` is `None` for
            // signal-termination, so this one assertion covers both.
            assert_eq!(
                exit_status.code(),
                Some(0),
                "mcpls should exit with status 0 via its own shutdown path, not be killed \
                 by the default SIGTERM disposition (issue #308 regression)"
            );
            return Ok(());
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mcpls did not exit within 5s of SIGTERM while the client's stdin write end \
             was still open (issue #308 regression)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Test that mcpls exits promptly on `SIGTERM` during the MCP handshake wait.
///
/// The readiness marker follows signal registration and precedes the wait for
/// the client's first `initialize` request, so the signal is sent without a
/// request while avoiding the process startup race.
#[test]
#[cfg(unix)]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_sigterm_exits_promptly_during_handshake_wait() -> Result<()> {
    let mut client = McpClient::spawn_and_wait_for_stdio()?;

    let pid = client.pid();
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()?;
    assert!(
        status.success(),
        "failed to send SIGTERM to mcpls (pid {pid})"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(exit_status) = client.try_wait()? {
            assert_eq!(
                exit_status.code(),
                Some(0),
                "mcpls should exit with status 0 via its own shutdown path even when SIGTERM \
                 arrives before the MCP handshake completes, not be killed by the default \
                 SIGTERM disposition (issue #318 regression)"
            );
            return Ok(());
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mcpls did not exit within 5s of SIGTERM sent before the handshake completed \
             (issue #318 regression)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
