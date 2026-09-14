"""Controlled framed LSP process; each launch owns one diagnostic generation."""
import json
import os
from pathlib import Path
import sys
import threading
import time

control, uri, label = sys.argv[1:4]
options = sys.argv[4:]
mode = options[0] if options else "default"
startup_delay = float(options[1]) if len(options) > 1 else 0.0
initialize_delay = float(options[2]) if len(options) > 2 else 0.0
initialized_marker = Path(options[3]) if len(options) > 3 else None
published_marker = Path(options[4]) if len(options) > 4 else None
initialize_attempt_marker = Path(options[5]) if len(options) > 5 else None
counter = Path(control + ".generation")
generation = int(counter.read_text()) + 1 if counter.exists() else 1
counter.write_text(str(generation))
sentinel = label if mode == "on-open" else f"{label}-generation-{generation}"
publish_started = False


def crash_when_requested():
    while not Path(f"{control}.crash-{generation}").exists():
        time.sleep(0.005)
    os._exit(17)


threading.Thread(target=crash_when_requested, daemon=True).start()
send_lock = threading.Lock()


def send(value):
    body = json.dumps({"jsonrpc": "2.0", **value}).encode()
    with send_lock:
        sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
        sys.stdout.buffer.flush()


def mark(path):
    if path is not None:
        path.write_text(sentinel)


def publish():
    send({"method": "textDocument/publishDiagnostics", "params": {
        "uri": uri, "diagnostics": [{"range": {"start": {"line": 0, "character": 0},
            "end": {"line": 0, "character": 1}}, "severity": 1, "message": sentinel}]
    }})
    send({"method": "window/logMessage", "params": {"type": 3, "message": sentinel}})
    mark(published_marker)


def publish_after_delay():
    time.sleep(startup_delay)
    publish()


while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line == b"\r\n":
            break
        key, value = line.decode().split(":", 1)
        headers[key.lower()] = value.strip()
    request = json.loads(sys.stdin.buffer.read(int(headers["content-length"])))
    method = request.get("method")
    if method == "initialize":
        mark(initialize_attempt_marker)
        if mode == "fail-initialize" and generation > 1:
            send({"id": request["id"], "error": {"code": -32000,
                "message": "controlled initialization failure"}})
            continue
        if mode == "hold-initialize" and generation > 1:
            while True:
                time.sleep(1)
        if generation > 1 and initialize_delay:
            time.sleep(initialize_delay)
        send({"id": request["id"], "result": {"capabilities": {
            "textDocumentSync": 1,
            "workspaceSymbolProvider": True,
            "diagnosticProvider": {"interFileDependencies": False, "workspaceDiagnostics": False}
        }}})
    elif method == "initialized":
        mark(initialized_marker)
        if mode == "reporting" or (mode == "reporting-then-silent" and generation == 1):
            send({"method": "$/progress", "params": {
                "token": "reporting", "value": {"kind": "begin"}
            }})
            send({"method": "$/progress", "params": {
                "token": "reporting", "value": {"kind": "end"}
            }})
        if mode == "on-open" or (mode == "reporting-then-silent" and generation > 1):
            pass
        elif mode == "silent":
            time.sleep(startup_delay)
            publish()
        else:
            publish()
    elif method == "textDocument/didOpen" and mode == "on-open":
        uri = request["params"]["textDocument"]["uri"]
        publish()
    elif method == "textDocument/diagnostic":
        send({"id": request["id"], "result": {"kind": "full", "items": []}})
    elif method == "workspace/symbol":
        if mode == "reporting-then-silent" and generation > 1 and not publish_started:
            publish_started = True
            threading.Thread(target=publish_after_delay, daemon=True).start()
        send({"id": request["id"], "result": [{"name": sentinel, "kind": 12,
            "location": {"uri": uri, "range": {"start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 1}}}}]})
    elif method == "shutdown":
        send({"id": request["id"], "result": None})
    elif method == "exit":
        sys.exit(0)
    elif "id" in request:
        send({"id": request["id"], "result": None})
