# Windows process containment measurements

Can an MCP server leave a backend running behind it? A snapshot of four behavioural
measurements taken on one Windows machine on 2026-09-12, answering that for the two
hosts mcpls runs under. The Windows half of the shared backend design rests on these
(`docs/superpowers/specs/2026-09-12-shared-backend-design.md`, under "Starting the
backend" and "Idle shutdown"). Numbers and versions here are deliberately fixed; the
last section carries the probes for re-taking each measurement.

## Answer

Claude Code does not contain the descendants of a stdio MCP server. Codex does.
A backend spawned by a Codex-launched frontend dies with it, so the hook route the
design chose is required for Codex and optional for Claude Code.

| Host | Door | MCP server / hook process | Detached grandchild |
|---|---|---|---|
| Claude Code, print mode | MCP server | killed on host exit | survives |
| Claude Code, print mode | SessionStart hook | exits immediately by design | survives |
| Claude Code, interactive | MCP server | killed on host exit | survives |
| Claude Code, interactive | SessionStart hook | exits immediately by design | survives |
| Codex, exec mode | MCP server | killed on host exit | killed |
| Codex, exec mode | SessionStart hook | exits immediately by design | survives |

`CREATE_BREAKAWAY_FROM_JOB` is not an escape hatch. The flag never produced an access
denied error on this machine, under either host, yet the child it created still died
along with the plain detached child when Codex exited.

## Environment

- Windows 11 Home, version 25H2, build 10.0.26200.9445. The registry `ProductName`
  still reads `Windows 10 Home`, which is a known Windows 11 quirk.
- Claude Code 2.1.269
- codex-cli 0.154.0
- CPython 3.14.7 for the probes
- COMODO Internet Security is installed alongside Windows Defender. This matters; see
  the ambient job section.

## Measurement 1: the MCP server door on Claude Code

The probe from the handoff brief ran verbatim as the only MCP server of a throwaway
project, launched with `--mcp-config` plus `--strict-mcp-config` so no other server
loaded. A second server reported its own job-object situation in the same session.

Both spawns succeeded. No access denied:

```
probe start pid=39620
spawned breakaway=True pid=27244
spawned breakaway=False pid=28088
```

When the host exited, both MCP server processes were gone and both children were
still running. The heartbeat file kept growing for 29 seconds past host exit, at
which point the measurement was stopped by hand rather than by anything dying.

The interactive run gave the same answer. A real terminal session, exited with
`/exit`, left all four children alive (two from the MCP server, two from the hook)
while both MCP server processes were killed.

## Measurement 2: the hook door on Claude Code

A `SessionStart` hook, passed through `--settings`, spawned the heartbeat detached and
exited at once. Both spawns succeeded and both children outlived the session.

This door is permissive on Claude Code, but measurement 1 means the design does not
need it there.

## Measurement 3: both doors on Codex

Codex ran with an isolated `CODEX_HOME` so the real configuration stayed untouched.

The MCP door contains descendants. Everything died within a second of the host
exiting: the probe server, the child created with the breakaway flag, and the plain
detached child. The last heartbeat line landed one second before the host exit
timestamp.

The result does not depend on which MCP client Codex uses. Containment held with
`features.experimental_use_rmcp_client` both true and false.

The hook door is permissive. A `SessionStart` hook, run with
`--dangerously-bypass-hook-trust`, spawned two children that both outlived the
session.

This confirms the source reading behaviourally, which is the useful direction: the
claim was that Codex contains MCP servers and does not contain hooks, and that is what
the machine does.

Codex was measured in `codex exec` only, not in the interactive TUI.

## The ambient job object, and why flag introspection cannot see the host's job

Every process on this machine reports an immediate job object with limit flags
`0x00003000`, meaning kill-on-job-close plus silent-breakaway-ok, holding exactly one
process. That includes a process spawned through `Win32_Process.Create`, whose parent
is the WMI provider and which therefore sits outside any terminal or agent process
tree. Only `explorer.exe` reported no job. COMODO's containment engine is the likely
source.

The consequence for instrumentation: querying the *immediate* job from inside a
process cannot reveal a job the host wrapped it in, because the per-process job is
assigned later and masks it. Under Codex the MCP server reported the same `0x00003000`
single-member job as every other process, yet Codex demonstrably killed its
descendants. Behaviour detects host containment on this machine; job flags do not.

Because Claude Code and Codex were measured on the same box within minutes of each
other, the difference between them is solid. The absolute results carry a caveat: a
machine without COMODO might behave differently, most plausibly by making Claude Code
look stricter, not looser.

## Measurement 4: named pipe handover

Three processes. A holder created a `NamedPipeServerStream` with
`PipeOptions.FirstPipeInstance`, accepted one client connection from a second process,
then stopped accepting while keeping the connected stream open. A third process tried
to bind the same name.

While the connected stream was open, the bind failed:

```
System.IO.IOException
HResult 0x800700E7  (Win32 231, ERROR_PIPE_BUSY)
All pipe instances are busy.
```

It failed the same way without `FirstPipeInstance`, against `maxNumberOfServerInstances = 1`.

After the holder disposed the connected stream, and with the holder process still
running, the same bind succeeded.

So closing the handle is what frees the name, not the owning process exiting. The
design's shutdown ordering, which requires a departing backend to close every
connected pipe handle before another process binds the name, is correct and is not
stricter than it needs to be.

## Side effect left behind

Claude Code recorded folder trust for the throwaway project directory under the
session scratchpad. That path is temporary, so the entry is stale clutter in the user
configuration rather than anything load-bearing.

## Re-taking the measurements

Make a scratch directory, put both files below in it, and set `PROBE_DIR` to its
absolute path everywhere, because the host launches these with a working directory you
do not control. Delete `heartbeat.txt` and `probe.log` between runs so each measurement
stands alone.

`heartbeat.py`, the process whose survival is the measurement:

```python
import pathlib
import sys
import time

target = pathlib.Path(sys.argv[1])
for _ in range(900):
    with target.open("a", encoding="utf-8") as handle:
        handle.write(f"{time.time():.0f} alive\n")
    time.sleep(2)
```

`mcp_probe.py`, a minimal MCP server that spawns the heartbeat twice, once with the
breakaway flag and once without, then answers enough of the protocol that the host
keeps it connected:

```python
import json
import os
import pathlib
import subprocess
import sys
import time

probe_dir = pathlib.Path(os.environ.get("PROBE_DIR", "."))
heartbeat = probe_dir / "heartbeat.txt"
log_path = probe_dir / "probe.log"

DETACHED_PROCESS = 0x00000008
CREATE_NEW_PROCESS_GROUP = 0x00000200
CREATE_BREAKAWAY_FROM_JOB = 0x01000000


def log(message):
    with log_path.open("a", encoding="utf-8") as handle:
        handle.write(f"{time.time():.0f} {message}\n")


def spawn(breakaway):
    flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    if breakaway:
        flags |= CREATE_BREAKAWAY_FROM_JOB
    command = [sys.executable, str(probe_dir / "heartbeat.py"), str(heartbeat)]
    try:
        child = subprocess.Popen(
            command,
            creationflags=flags,
            close_fds=True,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except OSError as error:
        log(f"spawn failed breakaway={breakaway}: {error!r}")
        return
    log(f"spawned breakaway={breakaway} pid={child.pid}")


log(f"probe start pid={os.getpid()}")
spawn(breakaway=True)
spawn(breakaway=False)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except ValueError:
        continue
    if "id" not in message:
        continue
    method = message.get("method")
    if method == "initialize":
        requested = message.get("params", {}).get("protocolVersion", "2025-06-18")
        result = {
            "protocolVersion": requested,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "probe", "version": "0"},
        }
    elif method == "tools/list":
        result = {"tools": []}
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}) + "\n")
    sys.stdout.flush()
```

For the MCP server door, register `mcp_probe.py` as the only MCP server of a throwaway
project, start a session there, let the server connect, then close the host and watch
whether `heartbeat.txt` keeps growing. For the hook door, register a session-start hook
that runs the same spawns and exits at once, which is the probe with its stdin loop
removed.

For the pipe handover, one process creates a `NamedPipeServerStream` with
`FirstPipeInstance`, accepts a connection from a second, then stops accepting while
holding the connected stream open. A third tries to bind the same name, expecting
failure, and tries again after the holder disposes the stream, expecting success.

Report per measurement what was done, what happened, and the exact error text where
something failed, plus the host versions. A measurement that could not be run is
reported as not run rather than inferred.
