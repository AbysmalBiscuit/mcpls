"""Controlled framed LSP process; each launch owns one diagnostic generation."""
import json
import os
from pathlib import Path
import sys
import threading
import time

control, uri, label = sys.argv[1:]
counter = Path(control + ".generation")
generation = int(counter.read_text()) + 1 if counter.exists() else 1
counter.write_text(str(generation))
sentinel = f"{label}-generation-{generation}"


def crash_when_requested():
    while not Path(f"{control}.crash-{generation}").exists():
        time.sleep(0.005)
    os._exit(17)


threading.Thread(target=crash_when_requested, daemon=True).start()


def send(value):
    body = json.dumps({"jsonrpc": "2.0", **value}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()


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
        send({"id": request["id"], "result": {"capabilities": {
            "textDocumentSync": 1,
            "diagnosticProvider": {"interFileDependencies": False, "workspaceDiagnostics": False}
        }}})
    elif method == "initialized":
        send({"method": "textDocument/publishDiagnostics", "params": {
            "uri": uri, "diagnostics": [{"range": {"start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 1}}, "severity": 1, "message": sentinel}]
        }})
        send({"method": "window/logMessage", "params": {"type": 3, "message": sentinel}})
    elif method == "textDocument/diagnostic":
        send({"id": request["id"], "result": {"kind": "full", "items": []}})
    elif method == "shutdown":
        send({"id": request["id"], "result": None})
    elif method == "exit":
        sys.exit(0)
    elif "id" in request:
        send({"id": request["id"], "result": None})
