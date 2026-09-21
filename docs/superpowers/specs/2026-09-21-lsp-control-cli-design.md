# Language server control from the CLI

Status: approved design, not yet implemented.

Target: the `AbysmalBiscuit/mcpls` fork, issue #42.

## Problem

A checkout's shared backend owns its language servers, and nothing outside an MCP session can change what they are doing. When rust-analyzer wedges, the only fix is to kill the backend and every session attached to it. When a server eats memory nobody needs, there is no way to evict it. When a user installs a binary the backend recorded as missing, there is no way to try again.

`mcpls doctor` already reports each server's lifecycle through `Request::Status`, so reading state is solved. Changing it is not.

## Goals

1. Start, stop, and restart one language server, several, or all of them, in the backend serving a checkout, without disturbing attached sessions.
2. A stopped server stays stopped until someone starts it. Lazy start does not undo a deliberate stop.
3. An agent that hits a stopped server learns the command that brings it back, and runs it through its shell.
4. A short status view that lists server states and nothing else.

## Non-goals

- Stopping or restarting the backend process.
- Keeping a stopped server stopped across a backend restart.
- An MCP tool for lifecycle control. Agents use the CLI, which keeps one code path and adds no tool schema to every session.
- Starting a backend from the `lsp` commands. A backend with no session exits after its idle timer, so starting one only to start a server has no lasting effect.

## CLI

```
mcpls lsp status  [--dir DIR]
mcpls lsp start   <SERVER>... | --all  [--dir DIR] [--no-wait]
mcpls lsp stop    <SERVER>... | --all  [--dir DIR]
mcpls lsp restart <SERVER>... | --all  [--dir DIR] [--no-wait]
```

- `SERVER` is the routing id `mcpls doctor` prints, such as `rust` or `typescript`. Named servers and `--all` conflict, and one of them is required for `start`, `stop`, and `restart`.
- `--dir` defaults to `$CLAUDE_PROJECT_DIR`, then the working directory, matching `mcpls doctor`. It is a flag because the positional slot holds server ids.
- `status` prints one line per applicable server, id then state, such as `rust  running`.
- `start` and `restart` wait for each named server to leave `starting` unless `--no-wait` is given. `stop` returns once the server is `stopped`, which the backend publishes before its process has exited.
- Every command prints the final state of each server it touched.

Exit status is 0 when every named server reaches its target state: `running` for `start` and `restart`, `stopped` for `stop`. It is 1 when:

- a server ends `failed` or `not installed`;
- an id does not apply to this checkout, with the applicable ids listed;
- the wait exceeds its ceiling;
- no backend answers, reported as `no backend serves DIR; one starts with an agent session`.

The wait ceiling is the largest `timeout_seconds` among the named servers.

## Wire protocol

One request and one response join the hook protocol in `hooks/protocol.rs`, on the existing hook connection kind:

```rust
Request::Lsp { action: LspAction, servers: Vec<String> }   // empty = all
Response::Lsp { servers: Vec<ServerStatus> }

enum LspAction { Start, Stop, Restart }                     // snake_case on the wire
```

- The response carries each named server's state immediately after the action is applied, not after it settles.
- An unknown id answers `Response::Error` whose message names the ids that apply here. No action is applied when any id is unknown.
- `Lsp` requests do not count toward `hooks_seen`, the same as `Status`.
- The handler never waits on a spawn or a process exit, so it always answers inside `op_deadline`. The CLI polls `Request::Status` for the settled state.

## Backend behavior

### The `stopped` state

`ServerLifecycle` gains `Stopped`, rendered `stopped`.

`ensure_server` refuses a stopped server with `ServerUnavailable` and the reason ``stopped; run `mcpls lsp start <id>` ``, which is the text an agent sees on a tool call. A stopped server does not fall through to a catch-all route the way a `not installed` one does: the user chose to stop it, and quietly routing its files to another server hides that choice. The hook sweep's `ensure_servers_for_edits` goes through `ensure_server` and ignores its error, so edits do not wake a stopped server. `server_for_path` and `get_client_for_file` treat `Stopped` like `Failed` when choosing whether to fall back.

### Retiring a server

The teardown at the top of `install_server` becomes `retire_server(id) -> Option<LspServer>`:

1. fail the old client's pending requests;
2. retire its notification pump;
3. clear its cached diagnostics;
4. forget its watch registrations and open documents;
5. remove its client and server from `lsp_clients` and `lsp_servers`, returning the server.

`install_server` calls it before swapping in the replacement. `stop` calls it, then `reconcile_diagnostics_owners(None)` so a flush stops waiting on a retired diagnostics owner.

The returned `LspServer` is shut down in a spawned task: the LSP `shutdown` and `exit` handshake bounded by `SERVER_SHUTDOWN_TIMEOUT`, then a kill. Restart treats the replaced process the same way, rather than dropping it.

### Transitions

| Action | From | Result |
|---|---|---|
| `stop` | any | `stopped`, retiring a live server. Idempotent. |
| `start` | `stopped`, `idle`, `failed` | clear the respawn backoff and spawn |
| `start` | `not installed` | retry the spawn once, since an explicit start is when someone has just installed the binary |
| `start` | `running`, `starting` | no change |
| `restart` | `running` | forced respawn through `install_server` |
| `restart` | anything else | same as `start` |

### Stop during a spawn

`stop` publishes `Stopped` at once, even when a spawn is in flight. When `run_spawn` finishes, it moves the state from `Starting` to `Running` only if the state is still `Starting`. If it finds `Stopped`, it retires the fresh server instead of publishing it. The check and the transition happen under the existing `lifecycles` lock, so a stop and a finishing spawn cannot both win.

## Testing

Tests are written first and watched failing before the code lands.

- **End to end through the binary**, in `crates/mcpls-cli/tests/backend.rs`, with a backend serving a small Python language server that logs each launch and each clean exit. A frontend attaches and a tool call starts the server. Then:
  1. `mcpls lsp stop fake` exits 0 and `mcpls lsp status` prints `fake  stopped`;
  2. a tool call on a `.fake` file returns the error naming `mcpls lsp start fake`, and the fixture's launch count does not move;
  3. `mcpls lsp start fake` exits 0 reporting `running`, and the launch count rises by one;
  4. `mcpls lsp restart fake` produces a new generation, and the previous process exits through the LSP shutdown handshake;
  5. an unknown id exits 1 and lists the applicable ids;
  6. `mcpls lsp status` with no backend running exits 1.
- **Stop during a spawn**, as a translator test in `bridge/translator/respawn.rs`, using a fixture whose `initialize` answer is delayed to hold the spawn open: the fresh server is retired and the state stays `stopped`.
- **Wire literals** for `Request::Lsp` and `Response::Lsp` pinned in `hooks/protocol.rs` beside the existing ones. The lifecycle render test covers `stopped` through `EnumIter`.
- **Argument parsing** in `args.rs`: `--all` conflicts with named servers, and a mutating action without either is an error.

## Documentation

- `plugin/skills/setup-mcpls/references/cli.md` documents the `lsp` group.
- `plugin/skills/setup-mcpls/references/troubleshooting.md` points at `mcpls lsp restart` for a wedged server, ahead of killing the backend.
- The `mcpls` skill tells agents that a `stopped` error is resolved with `mcpls lsp start <id>`.
