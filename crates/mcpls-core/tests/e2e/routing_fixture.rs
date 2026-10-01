use std::fs;
use std::path::Path;

use anyhow::Result;

const CALL_HIERARCHY_SERVER: &str = r#"import json
import sys

role, foreign_uri = sys.argv[1:]
opaque = {"_mcpls_call_hierarchy": {"version": 1, "server_id": "typescript"}, "payload": [None, "opaque", 7]}

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
    return json.loads(sys.stdin.buffer.read(int(headers["content-length"])))

def send(message):
    body = json.dumps(message).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body)
    sys.stdout.buffer.flush()

def item(name):
    return {"name": name, "kind": 12, "uri": foreign_uri,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "data": opaque}

while True:
    request = read_message()
    if request is None:
        break
    method = request.get("method")
    if method == "initialize":
        result = {"capabilities": {"textDocumentSync": 1, "callHierarchyProvider": True}}
    elif method == "textDocument/prepareCallHierarchy":
        result = [item(role + "-prepare")]
    elif method in ("callHierarchy/incomingCalls", "callHierarchy/outgoingCalls"):
        if request["params"]["item"].get("data") != opaque:
            send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": "opaque data changed: " + json.dumps(request["params"]["item"].get("data"))}})
            continue
        if method == "callHierarchy/incomingCalls":
            result = [{"from": item(role + "-incoming"), "fromRanges": []}]
        else:
            result = [{"to": item(role + "-outgoing"), "fromRanges": []}]
    elif method == "exit":
        break
    elif "id" not in request:
        continue
    else:
        result = None
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})
"#;

pub fn write_call_hierarchy_server(dir: &Path) -> Result<std::path::PathBuf> {
    let path = dir.join("call_hierarchy_fixture.py");
    fs::write(&path, CALL_HIERARCHY_SERVER)?;
    Ok(path)
}
