# mcpls plugin

Registers `mcpls` as an MCP server, wires it into Claude Code's hook events so diagnostics reach the agent without being asked for, and installs the mcpls skill alongside them.

## Install

`mcpls` itself has to be on `PATH` first; see the [top-level README](../README.md#installation) for install methods. Then load the plugin directory for the session:

```fish
claude --plugin-dir /path/to/mcpls/plugin
```

`--plugin-dir` loads a plugin for that session only. To make it permanent, wrap the command in a shell alias or function, or add `--plugin-dir` to whatever launches your interactive sessions.

## Check that it's working

Every hook failure in this system is silent by design: a hook that cannot reach mcpls exits cleanly and prints nothing, because an edit must never fail just because diagnostics were unavailable. That means a broken install looks exactly like a quiet workspace. Before doing anything else, run:

```fish
mcpls hook doctor
```

A working install prints something like:

```
socket: /run/user/1000/mcpls/39df698ef1ac4f49.sock
hook sees: /home/lev/project -> 39df698ef1ac4f49
server sees: /home/lev/project -> 39df698ef1ac4f49
owner pid: 2816002
mcpls on PATH: /home/lev/.cargo/bin/mcpls
```

Line by line:

- **`socket:`** the socket path both the hook and the running server talk over. Informational; useful when checking permissions on the file itself.
- **`hook sees:`** the project directory and its hash, as the hook process computes them. If the directory looks wrong, the project is being opened from an unexpected path (a symlink, a subdirectory, a second worktree) and that needs fixing before anything else here will line up.
- **`server sees:`** the same pair, as the running mcpls computes them, or `no owner` if nothing is bound to the socket. `no owner` means mcpls for this project is not running right now: start it (open the project in an MCP client that spawns mcpls) and run doctor again.
- **a mismatch warning** appears only when the two hashes differ. It means the hook and the server disagree about which directory they're both in, so hooks will do nothing until they agree. A multi-root config, a symlinked checkout, or one side resolving a subdirectory the other canonicalizes differently all produce this.
- **`owner pid:`** the process id holding the socket, or `none` if nothing does. Confirm it names a live `mcpls` process; a pid that no longer exists or belongs to something else means the socket is orphaned and should be cleared so a new mcpls can bind it.
- **`mcpls on PATH:`** the absolute path doctor found by searching `PATH` for `mcpls`, or `not found`. Hooks invoke `mcpls` by name, not by path, so `not found` means every hook silently does nothing. Fix the `PATH` the hook process inherits, not just your interactive shell's.

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
