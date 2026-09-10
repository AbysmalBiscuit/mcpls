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

const PROGRESS_DURING_RESYNC_SERVER: &str = r#"import json
import os
import sys
import time

progress_begin_marker = sys.argv[1]
begin_observed_marker = sys.argv[2]
resync_hold_marker = sys.argv[3]
release_marker = sys.argv[4]
grace_elapsed_marker = sys.argv[5]
end_release_marker = sys.argv[6]
diagnostic_marker = sys.argv[7]
footer_grace_seconds = float(sys.argv[8])
workspace_uri = "file:///tmp/mcpls-fixture"
document_uri = None
next_request_id = 7001


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


def publish_diagnostic():
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
                "message": "progress-during-resync",
            }],
        },
    })
    mark(diagnostic_marker, "progress-during-resync")


while True:
    request = read_message()
    if request is None:
        break

    method = request.get("method")
    if method == "initialize":
        params = request.get("params", {})
        workspace_uri = params.get("rootUri") or workspace_uri
        folders = params.get("workspaceFolders") or []
        if folders:
            workspace_uri = folders[0].get("uri") or workspace_uri
        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "capabilities": {
                    "textDocumentSync": 1,
                    "renameProvider": True,
                    "documentFormattingProvider": True,
                    "codeActionProvider": True,
                }
            },
        })
    elif method == "initialized":
        send({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {"token": "startup", "value": {"kind": "begin"}},
        })
        send({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {"token": "startup", "value": {"kind": "end"}},
        })
    elif method == "textDocument/didOpen":
        document_uri = request["params"]["textDocument"]["uri"]
    elif method == "textDocument/rename":
        document_uri = request["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "id": request["id"], "result": {"changes": {
            document_uri: [{"range": {"start": {"line": 0, "character": 3},
                                      "end": {"line": 0, "character": 6}},
                           "newText": "new"}]
        }}})
    elif method == "textDocument/formatting":
        document_uri = request["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "id": request["id"], "result": [{
            "range": {"start": {"line": 0, "character": 3},
                      "end": {"line": 0, "character": 6}},
            "newText": "new"
        }]})
    elif method == "textDocument/codeAction":
        document_uri = request["params"]["textDocument"]["uri"]
        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": [{
                "title": "Progress-aware write",
                "command": {
                    "title": "Progress-aware write",
                    "command": "fixture.progress_write",
                    "arguments": [],
                },
            }],
        })
    elif method == "workspace/executeCommand":
        apply_id = next_request_id
        next_request_id += 1
        other_uri = workspace_uri.rstrip("/") + "/other.rs"
        send({
            "jsonrpc": "2.0",
            "id": apply_id,
            "method": "workspace/applyEdit",
            "params": {
                "edit": {
                    "changes": {
                        document_uri: [{
                            "range": {
                                "start": {"line": 0, "character": 3},
                                "end": {"line": 0, "character": 6},
                            },
                            "newText": "new",
                        }],
                        other_uri: [{
                            "range": {
                                "start": {"line": 0, "character": 3},
                                "end": {"line": 0, "character": 6},
                            },
                            "newText": "new",
                        }],
                    },
                },
            },
        })

        saves = 0
        apply_response_seen = False
        while saves < 2 or not apply_response_seen:
            message = read_message()
            if message is None:
                break
            if message.get("method") == "textDocument/didSave":
                saves += 1
                if saves == 1:
                    send({
                        "jsonrpc": "2.0",
                        "method": "$/progress",
                        "params": {
                            "token": "write",
                            "value": {"kind": "begin"},
                        },
                    })
                    mark(progress_begin_marker, "begin")
                elif saves == 2:
                    mark(begin_observed_marker, "begin observed")
                    mark(resync_hold_marker, "waiting for later resync")
            elif message.get("id") == apply_id:
                apply_response_seen = True

        while not os.path.exists(release_marker):
            time.sleep(0.01)

        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": None,
        })
        time.sleep(footer_grace_seconds)
        mark(grace_elapsed_marker, "footer grace elapsed")
        while not os.path.exists(end_release_marker):
            time.sleep(0.01)
        send({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {"token": "write", "value": {"kind": "end"}},
        })
        publish_diagnostic()
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

/// Write a framed Python LSP server that starts progress during a two-file
/// resync and delays its command response until the test releases it.
pub fn write_progress_during_resync_server(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("diagnostics_progress_during_resync_fixture.py");
    fs::write(&path, PROGRESS_DURING_RESYNC_SERVER)
        .with_context(|| format!("failed to write LSP fixture at {}", path.display()))?;
    Ok(path)
}
