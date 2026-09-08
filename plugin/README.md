# mcpls plugin

Registers `mcpls` as an MCP server, wires it into Claude Code's hook events so diagnostics reach the agent without being asked for, and installs the mcpls skill alongside them.

## Install

`mcpls` itself has to be on `PATH` first; see the [top-level README](../README.md#installation) for install methods. Then load the plugin directory for the session:

```fish
claude --plugin-dir /path/to/mcpls/plugin
```

`--plugin-dir` loads a plugin for that session only. To make it permanent, wrap the command in a shell alias or function, or add `--plugin-dir` to whatever launches your interactive sessions.

## Check that it's working

Payload and socket failures exit cleanly without output, so a broken connection can look like a quiet workspace. `SessionStart` reports watch-path scan failures through a non-blocking user warning. To inspect the connection and scan, run:

```fish
mcpls hook doctor
```

A working install prints something like:

```
socket: /run/user/1000/mcpls/39df698ef1ac4f49.sock
hook sees: /home/lev/project -> 39df698ef1ac4f49
server sees: /home/lev/project -> 39df698ef1ac4f49
owner pid: 2816002
hooks seen: 3 request(s) since this owner started
mcpls on PATH: /home/lev/.cargo/bin/mcpls; launch not checked
watch scan: selected 4 top-level path(s); hidden and ignored entries excluded; host registration unverified
```

Line by line:

- **`socket:`** the socket path both the hook and the running server talk over. Informational; useful when checking permissions on the file itself.
- **`hook sees:`** the project directory and its hash, as the hook process computes them from `CLAUDE_PROJECT_DIR`. If the directory looks wrong, the project was opened from an unexpected path (a symlink, a subdirectory, a second worktree): open it at its real, canonical root instead and run the doctor again.
- **`server sees:`** what the running mcpls, if any, reports back. A healthy line repeats the hook's own directory and hash, as in the example above, confirming both sides agree. When no owner answers this project's own socket, the line takes one of four shapes instead:
  - `no owner; nothing is listening on this project's socket`: nothing was found running anywhere nearby. Start mcpls for this project (open it in an MCP client that spawns it) and run the doctor again.
  - `no owner for this directory; an mcpls is running for <root> (pid <pid>) instead`: a running mcpls was found, but for a directory that is an ancestor or descendant of this one, not this one. Check for a symlinked checkout, a multi-root workspace, or a `CLAUDE_PROJECT_DIR` pointing at a subdirectory, and restart that mcpls against the directory this project actually opens.
  - `no owner for this directory; N other mcpls instances are running, none for this directory or a parent of it` (singular wording, `1 other mcpls instance is running`, when there is exactly one): other mcpls processes exist on the machine, but none relate to this project, so the doctor does not name them (naming one would blame an innocent project for this one's silence). Start mcpls for this project the same way as the first case.
  - `no owner for this directory; could not scan for other mcpls instances: <reason>`: the scan itself failed, most likely a permissions problem on the runtime directory named in `socket:` above, so the doctor could not even tell the first three cases apart. Fix the reported `<reason>` and run the doctor again.

  Any of the four can carry further clauses appended to the end of the line: `; N other mcpls sockets are live but did not answer a status request this build could read` means something is holding a socket nearby that this build could not talk to, most likely an mcpls on a different version speaking an older or newer wire shape, worth checking and updating; `; N other mcpls instances also relate to this directory` appears alongside the second case above when more than one related owner exists, and should be treated the same way as that case; `; more may exist beyond the scan's limit` means the runtime directory held more candidate sockets than the doctor checks, so every count on the line is a floor, not a total.

  When an owner does answer but the exchange itself fails, the line reports the specific fault instead: an owner answering with an error message, answering with something other than its own status, a socket that accepted the connection but whose reply this build could not parse, or a socket that answered nothing within the probe window. Each of those pairs with `owner pid: unknown` below. The doctor names no cause for any of them, because an accepted connection and a failed exchange are the whole of what it established; what it prints instead is the answer or the error itself, and that is the thing to act on.
- **`owner pid:`** the process id holding the socket, `none` if nothing does, or `unknown` if an owner exists but the exchange did not get far enough to learn its pid (see the fault messages above). When it does print a pid, confirm it names a live `mcpls` process; a pid that no longer exists or belongs to something else means the socket is orphaned: delete the socket file at the path in `socket:` above (a Windows named pipe clears on its own once nothing holds it) so a new mcpls can bind it.
- **`hooks seen:`** how many `Changed`, `Flush`, or `EndSession` requests this owner has served since it started; a `Status` probe, including the doctor's own, is never counted. Zero is not by itself a fault: `SessionStart` never touches the socket, so an owner that just started, or just took over from a previous one, looks identical to one that has never received a hook. The line's own wording says what to do about a zero.
- **`mcpls on PATH:`** the absolute path of a candidate found on `PATH`, or `not found`. The doctor does not execute it: a text file named `mcpls.exe` on Windows is still a candidate, not a verified installation. Hooks invoke `mcpls` by name, so check the `PATH` the hook process inherits, not just your interactive shell's.
- **`watch scan:`** what a fresh local scan can select for `SessionStart`. An empty successful scan says `no eligible top-level paths`; traversal or ignore-rule failures say `incomplete` and include their causes. A selected path does not prove the host registered it or can read its descendants. Hidden top-level files and directories, including `.github`, are excluded along with ignored entries. After fixing scan errors, restart the session to register a fresh watch list.

An operation that exceeds the owner's response deadline keeps running in the background. Hook clients remain silent on that response; the deadline does not establish whether the work will succeed or whether a later report will contain diagnostics. The doctor reports a deadline response if it receives one during its probe window.

If mcpls cannot derive a socket identity for the project directory at all (an unreachable path, or a runtime directory deep enough that the resulting socket path exceeds the platform's length limit), the doctor prints a different shape entirely:

```
socket: none; could not derive an identity for this directory: socket path exceeds the 107-byte platform limit (267 bytes): "/run/user/1000/very/deep/mcpls/39df698ef1ac4f49.sock"
hook sees: /home/lev/project -> unknown
server sees: nothing can run here; no socket exists to probe
owner pid: none
mcpls on PATH: /home/lev/.cargo/bin/mcpls; launch not checked
watch scan: selected 4 top-level path(s); hidden and ignored entries excluded; host registration unverified
```

This is a different failure from a missing owner: there, a socket exists and nothing answers it; here, no socket could ever exist for this directory on either side. Fix the reported reason, for example moving the project or `XDG_RUNTIME_DIR` to a shorter path, then run the doctor again.

## Configuration

The `[diagnostics.hooks]` table controls the hook listener:

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Whether the hook listener binds at all. |
| `sweep_quiet_ms` | `500` | How long pending file changes must sit quiet before diagnostics are gathered and delivered. |
| `op_deadline_ms` | `1500` | How long a hook operation may run before it answers anyway, so a slow op can't block the agent. |

Setting `enabled = false` turns the listener off entirely: mcpls binds no socket, and every `mcpls hook` invocation exits having done nothing. Use it to run mcpls without push diagnostics, for example while diagnosing whether a problem is in the hook path or elsewhere.

## What's included

- `.mcp.json` registers `mcpls` as an MCP server.
- `hooks/hooks.json` wires the hook events mcpls needs into `mcpls hook`.
- [`skills/mcpls`](skills/mcpls/) is the same agent skill previously shipped from the repository's top-level `skills/` directory, now installed alongside the server and hooks that make use of it.
