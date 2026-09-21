# mcpls plugin

Registers `mcpls` as an MCP server, wires it into Claude Code's hook events so diagnostics reach the agent without being asked for, and installs the mcpls skills alongside them.

## Install

The plugin installs the mcpls release matching its own version when a session starts, using that release's installer, which places `mcpls` in `$CARGO_HOME/bin` (`~/.cargo/bin` by default) and adds it to `PATH`. An `mcpls` already on `PATH` that the plugin did not install is left alone. Language servers such as `rust-analyzer` are not installed; mcpls drives whichever ones are already on `PATH`.

Claude Code:

```fish
claude plugin marketplace add AbysmalBiscuit/mcpls
claude plugin install mcpls@mcpls
```

Codex:

```fish
codex plugin marketplace add AbysmalBiscuit/mcpls
codex plugin add mcpls@mcpls
```

Restart the session once after installing, from a new terminal if `~/.cargo/bin` was not already on your `PATH`. The first session starts before `mcpls` exists, so its MCP server fails to start and its hooks report `mcpls` as not found. Codex also asks you to trust the plugin's hooks on first load.

When the `mcpls` on `PATH` does not match the plugin's version, the plugin tells the agent at session start, including which version to install and where to get it. That covers an `mcpls` you installed yourself, an install that failed, and an install still running in another session. A failed install is not retried every session: run the installer from the release page, or delete `~/.local/state/mcpls/bootstrap-failed` and restart.

Edits made outside the agent's own edit tools (a terminal, another agent, a `git checkout`) reach mcpls through the backend's own filesystem watcher, on every host rather than only where the host offers a file-changed hook. It places one watch per directory the project's ignore rules keep, so generated trees like `target/` and `node_modules/` cost nothing, and it picks up directories created after the session started. A checkout it cannot watch, a Windows drive under WSL2 or a network mount, is reported as unwatched by `mcpls doctor` rather than silently missing edits.

A change the watcher reports never starts a language server that is not already running. Starting one is what an agent's own edit does, or what any tool call on a file does; a `git pull` touching a language this session never opens is not a reason to pay for its server.

To run a local build instead of the release, put it first on `PATH` and stop the plugin from installing over it on upgrade:

```fish
set -gx MCPLS_NO_BOOTSTRAP 1
set -gx PATH /path/to/mcpls/target/debug $PATH
```

## One backend per checkout

The `mcpls` the MCP entry launches is a small frontend. The first session in a checkout starts a backend in the background, and every later session in that checkout, from any subdirectory, attaches to it and shares its language servers. The backend exits `idle_shutdown_ms` after the last session closes (10 seconds by default; set it under `[backend]`). `mcpls backend start` keeps one running with no session attached until `mcpls backend stop`, and `mcpls backend status` reports on it.

On Windows the backend is started by the plugin's hooks rather than by the frontend, so the hooks are required there. A session with no hooks installed reports that it is waiting for its backend.

`mcpls --no-backend` runs one session entirely in-process, which is useful for debugging or for a host where a background process cannot run.

## Check that it's working

Payload and socket failures exit cleanly without output, so a broken connection can look like a quiet workspace. To inspect the connection and scan, run:

```fish
mcpls doctor
```

It examines `$CLAUDE_PROJECT_DIR`, or the working directory when that is unset, and a directory named on the command line wins over both. It exits non-zero when it finds a fault. `mcpls hook doctor` is an alias that always exits 0, which the hook registrations require.

A working install prints something like:

```
socket: /tmp/mcpls-lev/39df698ef1ac4f49.sock
hook sees: /home/lev/project/crates/core (from CLAUDE_PROJECT_DIR)
root: /home/lev/project -> 39df698ef1ac4f49
server sees: /home/lev/project -> 39df698ef1ac4f49
backend pid: 2816002
hooks seen: 3 request(s) since this owner started
backend: mcpls 0.3.9, up 1m1s
sessions: 2 attached (s1, connection-4)
config: 00000000000000ff
config file: /home/lev/.config/mcpls/mcpls.toml (the user's global config)
language servers: rust (running), python (not installed: pyright is not on PATH), go (not applicable here)
mcpls on PATH: /path/to/mcpls; launch not checked
watcher: 56 directories watched
problems: none
```

Line by line:

- **`socket:`** the socket path both the hook and the running server talk over. Informational; useful when checking permissions on the file itself.
- **`hook sees:`** the directory this report is about, canonicalized, and where its name came from: a path given on the command line, `CLAUDE_PROJECT_DIR`, or the working directory. Any directory inside the checkout works. Run by hand from a terminal inside an agent session, the doctor reports on `CLAUDE_PROJECT_DIR` rather than wherever you are standing, and this clause is what tells you so.
- **`root:`** the checkout root that directory resolves to, and the hash the socket name comes from. The root is the nearest directory at or above the `hook sees:` directory holding a `.git` entry git would accept, stopping before the home directory, or that directory itself when none does. Every session started inside one checkout shares the root and so reaches one mcpls. A linked worktree or a submodule holds its own `.git` entry and is its own root. If the root looks wrong, look for a `.git` entry where you did not expect one, or a missing one where you did.
- **`server sees:`** what the running mcpls, if any, reports back. A healthy line repeats the `root:` line's directory and hash, as in the example above, confirming both sides agree. When no owner answers this project's own socket, the line takes one of four shapes instead:
  - `no owner; nothing is listening on this project's socket`: nothing was found running anywhere nearby. Start mcpls for this project (open it in an MCP client that spawns it) and run the doctor again.
  - `no owner for this directory; an mcpls is running for <root> (pid <pid>) instead`: a running mcpls was found, but for a directory that is an ancestor or descendant of this one, not this one. Check for a symlinked checkout, a nested checkout such as a submodule, or an mcpls started outside any checkout, and restart that mcpls from inside the checkout this project opens.
  - `no owner for this directory; N other mcpls instances are running, none for this directory or a parent of it` (singular wording, `1 other mcpls instance is running`, when there is exactly one): other mcpls processes exist on the machine, but none relate to this project, so the doctor does not name them (naming one would blame an innocent project for this one's silence). Start mcpls for this project the same way as the first case.
  - `no owner for this directory; could not scan for other mcpls instances: <reason>`: the scan itself failed, most likely a permissions problem on the runtime directory named in `socket:` above, so the doctor could not even tell the first three cases apart. Fix the reported `<reason>` and run the doctor again.

  Any of the four can carry further clauses appended to the end of the line: `; N other mcpls sockets are live but did not answer a status request this build could read` means something is holding a socket nearby that this build could not talk to, most likely an mcpls on a different version speaking an older or newer wire shape, worth checking and updating; `; N other mcpls instances also relate to this directory` appears alongside the second case above when more than one related owner exists, and should be treated the same way as that case; `; more may exist beyond the scan's limit` means the runtime directory held more candidate sockets than the doctor checks, so every count on the line is a floor, not a total.

  When an owner does answer but the exchange itself fails, the line reports the specific fault instead: an owner answering with an error message, answering with something other than its own status, a socket that accepted the connection but whose reply this build could not parse, or a socket that answered nothing within the probe window. Each of those pairs with `backend pid: unknown` below. The doctor names no cause for any of them, because an accepted connection and a failed exchange are the whole of what it established; what it prints instead is the answer or the error itself, and that is the thing to act on.
- **`backend pid:`** the process id holding the socket, `none` if nothing does, or `unknown` if an owner exists but the exchange did not get far enough to learn its pid (see the fault messages above). When it does print a pid, confirm it names a live `mcpls` process; a pid that no longer exists or belongs to something else means the socket is orphaned: delete the socket file at the path in `socket:` above (a Windows named pipe clears on its own once nothing holds it) so a new mcpls can bind it.
- **`hooks seen:`** how many `Changed`, `Flush`, or `EndSession` requests this owner has served since it started; a `Status` probe, including the doctor's own, is never counted. Zero is not by itself a fault: `SessionStart` never touches the socket, so an owner that just started, or just took over from a previous one, looks identical to one that has never received a hook. The line's own wording says what to do about a zero.
- **`config file:`** the configuration file this build resolved for the checkout, and the tier it won on, so a fingerprint that surprises you has somewhere to lead. `mcpls config` prints the settings themselves.
- **`project config:`** appears only when an `mcpls.toml` sits at the checkout root and was skipped as untrusted. That file has no effect until you pass `--trust-project-config`, and this line is the only place that says so.
- **`language servers:`** every configured server and what it is doing. A backend that answered supplies the state for the ones it has (`running`, `idle`, `starting`, `not installed`, `failed`); the rest come from the configuration and the filesystem, which is why they are still reported with nothing running. `not applicable here` means no project marker matched this checkout, so nothing would start it. `not installed: <command> is not on PATH` means it would start here and its binary is missing. `installed` means it would start and its binary is there.
- **`problems:`** the faults worth acting on, or `none`. A built-in whose binary is missing is reported above but is not a fault: nobody installs a server for every ecosystem mcpls knows. A server your own configuration file names is, because you asked for it.
- **`mcpls on PATH:`** the absolute path of a candidate found on `PATH`, or `not found`. The doctor does not execute it: a text file named `mcpls.exe` on Windows is still a candidate, not a verified installation. The plugin's MCP server and hooks run this same `PATH` lookup, so this is the binary they run.
- **`watcher:`** what the backend's filesystem watcher is doing, reported by the backend rather than measured locally, so it says `unknown; no backend answered` whenever the lines above show nothing answering. A working watcher says how many directories carry a watch: one per directory the project's ignore rules keep, which excludes `target` and `node_modules`, and the version control stores `.git`, `.jj`, `.hg` and `.svn`, whether or not a `.gitignore` names them. The count moves as the project does, up when a new directory is created and down when one is deleted.

When part of the checkout could not be walked, usually a subtree the user cannot read, the count is followed by `; coverage is incomplete:` and the reason. Take the count as a floor when you see that: changes under the unwalked part are not reported, and the rest of the checkout is unaffected.

Otherwise the line says `not watching` and why. `still placing its watches` is the transient one, seen only in the moment between the backend starting and its first walk finishing; run the doctor again. Two lasting ones are worth knowing: a checkout on a filesystem inotify does not report changes for, which is a Windows drive under WSL2 (`9p`, `drvfs`) or a network mount, and the kernel's per-user watch limit, which names `fs.inotify.max_user_watches` and the count reached before it ran out. In both cases mcpls declines to watch at all rather than watch part of the tree, because which part would depend on the order the walk happened to take. Edits the agent makes through its own tools still reach the language servers either way.

An operation that exceeds the owner's response deadline keeps running in the background. Hook clients remain silent on that response; the deadline does not establish whether the work will succeed or whether a later report will contain diagnostics. The doctor reports a deadline response if it receives one during its probe window.

If mcpls cannot derive a socket identity for the project directory at all (an unreachable path, or a runtime directory deep enough that the resulting socket path exceeds the platform's length limit), the doctor prints a different shape entirely:

```
socket: none; could not derive an identity for this directory: socket path exceeds the 107-byte platform limit (267 bytes): "/home/lev/a/very/deep/temporary/directory/mcpls-lev/39df698ef1ac4f49.sock"
hook sees: /home/lev/project -> unknown
server sees: nothing can run here; no socket exists to probe
backend pid: none
mcpls on PATH: /path/to/mcpls; launch not checked
watcher: 56 directories watched
```

This is a different failure from a missing owner: there, a socket exists and nothing answers it; here, no socket could ever exist for this directory on either side. Fix the reported reason, for example moving the project or `TMPDIR` to a shorter path, then run the doctor again.

## Configuration

The `[diagnostics.hooks]` table controls the hook listener:

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Whether the hook listener binds at all. |
| `sweep_quiet_ms` | `500` | How long pending file changes must sit quiet before diagnostics are gathered and delivered. |
| `op_deadline_ms` | `1500` | How long a hook operation may run before it answers anyway, so a slow op can't block the agent. |

Setting `enabled = false` disables the hook listener, so mcpls binds no socket and socket-dependent hooks receive no diagnostics. Use this setting to run mcpls without push diagnostics, for example while diagnosing whether a problem is in the hook path or elsewhere.

## What's included

- `hooks/bootstrap-binaries` installs the mcpls release this plugin version pins when a session starts, and tells the agent when the `mcpls` on `PATH` does not match. `hooks/run-hook.cmd` runs it through bash, or through its PowerShell twin on Windows without Git Bash.
- `mcpls brief` runs when a session starts and tells the agent which installed language servers serve the checkout and to use the mcpls tools, so your own agent instructions don't have to. `[brief] enabled = false` in `mcpls.toml` turns it off.
- `.mcp.json` registers the server with both harnesses, and `hooks/hooks.json` wires Claude Code's hooks.
- `.codex-plugin/plugin.json` and `hooks/hooks-codex.json` register the plugin with Codex. There is deliberately no `plugin.json` at this directory's root: Codex would load one as an Agent Plugins manifest, start mcpls inside the plugin cache instead of the project, and load no hooks.
- [`skills/mcpls`](skills/mcpls/) teaches the agent when and how to use the tools for coding work.
- [`skills/setup-mcpls`](skills/setup-mcpls/) covers installing, configuring, and troubleshooting mcpls.
