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

/// Write a framed Python LSP server that returns a caller-provided sentinel.
pub fn write_hover_server(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("diagnostics_hover_fixture.py");
    fs::write(&path, HOVER_SERVER)
        .with_context(|| format!("failed to write LSP fixture at {}", path.display()))?;
    Ok(path)
}
