//! Small framed LSP process fixtures used by diagnostics configuration tests.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const HOVER_SERVER: &str = r#"import json
import sys

sentinel = sys.argv[1]
workspace_uri = "file:///tmp/mcpls-fixture"


def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line == b"\r\n":
            break
        key, value = line.decode("ascii").split(":", 1)
        headers[key.lower()] = value.strip()

    body = sys.stdin.buffer.read(int(headers["content-length"]))
    return json.loads(body)


def send(message):
    body = json.dumps(message, separators=(",", ":")).encode("utf-8")
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
    sys.stdout.buffer.write(header + body)
    sys.stdout.buffer.flush()


while True:
    request = read_message()
    if request is None:
        break

    method = request.get("method")
    if method == "initialize":
        workspace_uri = request.get("params", {}).get("rootUri") or workspace_uri
        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "capabilities": {
                    "textDocumentSync": 1,
                    "hoverProvider": True,
                    "workspaceSymbolProvider": True,
                }
            },
        })
    elif method == "textDocument/hover":
        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "contents": {"kind": "markdown", "value": sentinel}
            },
        })
    elif method == "workspace/symbol":
        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": [{
                "name": sentinel,
                "kind": 12,
                "location": {
                    "uri": workspace_uri.rstrip("/") + "/lib/example.ex",
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1},
                    },
                },
            }],
        })
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": request["id"], "result": None})
    elif method == "exit":
        break
    elif "id" in request:
        send({"jsonrpc": "2.0", "id": request["id"], "result": None})
"#;

const DIAGNOSTICS_SERVER: &str = r#"import json
import sys

diagnostic_message = sys.argv[1]
hover_message = sys.argv[2]
published_marker = sys.argv[3]
published = False


def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line == b"\r\n":
            break
        key, value = line.decode("ascii").split(":", 1)
        headers[key.lower()] = value.strip()

    body = sys.stdin.buffer.read(int(headers["content-length"]))
    return json.loads(body)


def send(message):
    body = json.dumps(message, separators=(",", ":")).encode("utf-8")
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
    sys.stdout.buffer.write(header + body)
    sys.stdout.buffer.flush()


while True:
    request = read_message()
    if request is None:
        break

    method = request.get("method")
    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "capabilities": {
                    "textDocumentSync": 1,
                    "hoverProvider": True,
                }
            },
        })
    elif method == "initialized":
        send({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {"token": "fixture", "value": {"kind": "begin"}},
        })
        send({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {"token": "fixture", "value": {"kind": "end"}},
        })
    elif method == "textDocument/didOpen":
        if not published:
            published = True
            document_uri = request["params"]["textDocument"]["uri"]
            with open(published_marker, "w", encoding="utf-8") as marker:
                marker.write("published")
            send({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": document_uri,
                    "diagnostics": [{
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 1},
                        },
                        "severity": 2,
                        "message": diagnostic_message,
                    }],
                },
            })
    elif method == "textDocument/hover":
        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "contents": {"kind": "markdown", "value": hover_message}
            },
        })
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": request["id"], "result": None})
    elif method == "exit":
        break
    elif "id" in request:
        send({"jsonrpc": "2.0", "id": request["id"], "result": None})
"#;

const MIXED_STARTUP_SERVER: &str = r#"import json
import sys
import time

role = sys.argv[1]
initialized_marker = sys.argv[2]
startup_marker = sys.argv[3]
changed_marker = sys.argv[4]
diagnostic_path = sys.argv[5]
startup_delay = float(sys.argv[6])
workspace_uri = "file:///tmp/mcpls-fixture"


def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line == b"\r\n":
            break
        key, value = line.decode("ascii").split(":", 1)
        headers[key.lower()] = value.strip()

    body = sys.stdin.buffer.read(int(headers["content-length"]))
    return json.loads(body)


def send(message):
    body = json.dumps(message, separators=(",", ":")).encode("utf-8")
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
    sys.stdout.buffer.write(header + body)
    sys.stdout.buffer.flush()


def mark(path, value):
    with open(path, "w", encoding="utf-8") as marker:
        marker.write(value)


def publish(message, marker_path):
    send({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": {
            "uri": workspace_uri.rstrip("/") + "/" + diagnostic_path,
            "diagnostics": [{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1},
                },
                "severity": 2,
                "message": message,
            }],
        },
    })
    mark(marker_path, message)


while True:
    request = read_message()
    if request is None:
        break

    method = request.get("method")
    if method == "initialize":
        folders = request.get("params", {}).get("workspaceFolders") or []
        if folders:
            workspace_uri = folders[0].get("uri") or workspace_uri
        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "capabilities": {
                    "textDocumentSync": 1,
                    "hoverProvider": True,
                }
            },
        })
    elif method == "initialized":
        mark(initialized_marker, "initialized")
        if role == "reporting":
            send({
                "jsonrpc": "2.0",
                "method": "$/progress",
                "params": {"token": "reporting", "value": {"kind": "begin"}},
            })
            send({
                "jsonrpc": "2.0",
                "method": "$/progress",
                "params": {"token": "reporting", "value": {"kind": "end"}},
            })
        elif role == "holding":
            send({
                "jsonrpc": "2.0",
                "method": "$/progress",
                "params": {"token": "excluded", "value": {"kind": "begin"}},
            })
            mark(startup_marker, "holding")
        else:
            time.sleep(startup_delay)
            publish("silent-startup", startup_marker)
    elif method == "textDocument/didOpen":
        if role == "silent":
            publish("silent-after-startup", changed_marker)
    elif method == "textDocument/hover":
        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "contents": {"kind": "markdown", "value": "mixed-startup-fixture"}
            },
        })
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": request["id"], "result": None})
    elif method == "exit":
        break
    elif "id" in request:
        send({"jsonrpc": "2.0", "id": request["id"], "result": None})
"#;

/// Write a framed Python LSP server that returns a caller-provided sentinel.
pub fn write_hover_server(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("diagnostics_hover_fixture.py");
    fs::write(&path, HOVER_SERVER)
        .with_context(|| format!("failed to write LSP fixture at {}", path.display()))?;
    Ok(path)
}

/// Write a framed Python LSP server that publishes a diagnostic on document open.
pub fn write_diagnostics_server(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("diagnostics_publish_fixture.py");
    fs::write(&path, DIAGNOSTICS_SERVER)
        .with_context(|| format!("failed to write LSP fixture at {}", path.display()))?;
    Ok(path)
}

/// Write a framed Python LSP server with one reporting and one silent startup mode.
pub fn write_mixed_startup_server(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("diagnostics_mixed_startup_fixture.py");
    fs::write(&path, MIXED_STARTUP_SERVER)
        .with_context(|| format!("failed to write LSP fixture at {}", path.display()))?;
    Ok(path)
}
