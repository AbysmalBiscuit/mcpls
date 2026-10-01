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

const DYNAMIC_SERVER: &str = r#"import json
import sys
from pathlib import Path
from urllib.parse import urlparse, unquote

method, capability, receipt, mode = sys.argv[1:]
pending = None
workspace_uri = None
control_method = "textDocument/hover" if method == "textDocument/documentSymbol" else "textDocument/documentSymbol"

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

def respond(request, result):
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})

def control_result():
    if control_method == "textDocument/hover":
        return {"contents": "control"}
    return []

while True:
    request = read_message()
    if request is None:
        break
    action = request.get("method")
    if action is None:
        if request.get("error") is not None:
            send({"jsonrpc": "2.0", "id": pending["id"], "error": request["error"]})
        else:
            respond(pending, control_result())
        pending = None
    elif action == "initialize":
        workspace_uri = request["params"]["workspaceFolders"][0]["uri"]
        family = method.split("/")[1]
        if family == "prepareCallHierarchy":
            family = "callHierarchy"
        client_caps = request["params"]["capabilities"]["workspace" if method == "workspace/symbol" else "textDocument"]
        if client_caps.get(family, {}).get("dynamicRegistration") is not True:
            send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": "client must opt into dynamic registration for " + method}})
            continue
        caps = {"textDocumentSync": 1, "hoverProvider" if control_method == "textDocument/hover" else "documentSymbolProvider": True}
        if mode == "static":
            caps[capability] = {"id": "initial", "documentSelector": [{"language": "rust", "pattern": "**/static.rs"}]}
        respond(request, {"capabilities": caps})
    elif action == control_method:
        filename = Path(unquote(urlparse(request["params"]["textDocument"]["uri"]).path)).name
        if filename.startswith("register"):
            pending = request
            selector = [{"language": "rust", "scheme": "file", "pattern": "**/good.rs"}]
            if filename == "register-language.rs":
                selector[0]["language"] = "python"
            elif filename == "register-scheme.rs":
                selector[0]["scheme"] = "untitled"
            elif filename == "register-empty.rs":
                selector = []
            elif mode == "relative":
                selector[0]["pattern"] = {"baseUri": workspace_uri, "pattern": "good.rs"}
            options = {} if method == "workspace/symbol" else {"documentSelector": selector}
            if method == "textDocument/diagnostic":
                options.update({"interFileDependencies": False, "workspaceDiagnostics": False})
            send({"jsonrpc": "2.0", "id": "register", "method": "client/registerCapability", "params": {"registrations": [{"id": "dynamic", "method": method, "registerOptions": options}]}})
        elif filename.startswith("unregister"):
            pending = request
            registration_id = "initial" if filename == "unregister-static.rs" else "dynamic"
            send({"jsonrpc": "2.0", "id": "unregister", "method": "client/unregisterCapability", "params": {"unregisterations": [{"id": registration_id, "method": method}]}})
        else:
            respond(request, control_result())
    elif action == method:
        with open(receipt, "a", encoding="utf-8", newline="\n") as marker:
            marker.write(method + "\n")
        if action == "textDocument/hover":
            result = {"contents": "dynamic hover"}
        elif action == "textDocument/diagnostic":
            result = {"kind": "full", "items": []}
        elif action == "textDocument/rename":
            result = {"changes": {}}
        elif action == "textDocument/signatureHelp":
            result = {"signatures": []}
        else:
            result = []
        respond(request, result)
    elif action == "exit":
        break
    elif "id" in request:
        respond(request, [] if action not in ("shutdown",) else None)
"#;

pub fn write_dynamic_server(dir: &Path) -> Result<std::path::PathBuf> {
    let path = dir.join("dynamic_capability_fixture.py");
    fs::write(&path, DYNAMIC_SERVER)?;
    Ok(path)
}
