# One mcpls backend per project

Status: proposed. Nothing here is built.

Target: the `AbysmalBiscuit/mcpls` fork, not upstream. Defaults are tuned for one user running many agents on a desktop or laptop, and breaking changes to configuration are acceptable when they buy ergonomics.

Supersedes one non-goal of `2026-09-06-diagnostics-injection-design.md`, which reads "A standalone mcpls daemon surviving host restarts. The MCP process keeps owning the language servers." That is exactly what this document proposes, for the reason below. The rest of that design stands; this one moves where its pieces run.

Host behaviour for Codex was read out of the `codex-cli` source at `rust-v0.154.0` and is version specific. That tag is absent from this machine's Codex checkout, so the Codex citations below were re-checked at `rust-v0.150.0-alpha.7` and carry that tag where the line numbers differ. Claude Code behaviour is inherited from the diagnostics design's own reading of the host binary.

## Problem

Both hosts start one stdio MCP server per session, and every mcpls process spawns its own language servers. `serve_with_identity` validates config and resolves workspace roots from the process working directory (`crates/mcpls-core/src/lib.rs:619-690`), then spawns the applicable servers (`crates/mcpls-core/src/lib.rs:866-876`) before it ever touches a transport (`crates/mcpls-core/src/lib.rs:931`). Nothing in that path asks whether another mcpls already holds a warm index for the same project.

So ten parallel sessions in one checkout means ten rust-analyzer processes, each indexing the same code. On a laptop that is an out-of-memory kill, not a slowdown. Codex makes it worse: it initializes MCP servers as part of setting up each session (`codex-rs/core/src/session/mcp_runtime.rs:104-141` at `rust-v0.150.0-alpha.7`), and its subagents are sessions, so a workflow that spawns subagents multiplies indexes with it. Whether a subagent starts new server processes or shares a pooled connection manager is the one claim the memory argument rests on, and it is still unconfirmed.

The processes already coordinate on one thing. One of them owns the project's hook socket and answers every session's hooks; the others go passive and forward to it, retrying the ownership lock so an owner exiting does not strand them (`crates/mcpls-core/src/hooks/service.rs:1-7`). That machinery exists because several processes each hold language servers while only one holds the delivery records. Sharing the language servers removes the condition it was written for.

One shape of sharing already exists and does not solve this. `Transport::Http` serves every HTTP session from clones of one `McplsServer` (`crates/mcpls-core/src/transport.rs:454-458`), whose state is an `Arc<BridgeContext>` holding the translator and the servers (`crates/mcpls-core/src/mcp/handlers.rs:30-54`). It is behind a non-default feature, it has to be started by hand, and it has no per-session identity at all: every HTTP session falls back to one process-local delivery key (`crates/mcpls-core/src/bridge/delivery.rs:41-59`), so a diagnostic delivered to one session counts as delivered to all of them.

## Goals

1. One set of language servers per project, however many sessions, subagents or hosts are attached.
2. A session that starts while a backend is already warm gets answers immediately, with no reindex.
3. Each agent hears about the diagnostics it caused, and no agent hears about another agent's.
4. Memory returns to the machine shortly after the last session closes.
5. Less code than today, not more. The owner, passive and demotion states go away.

## Non-goals

- Sharing across projects. Two worktrees hold different files and get a backend each.
- Surviving a reboot, or running as a user service. The backend is started on demand by the first frontend and exits on its own.
- Cross-user sharing. The socket's permissions and the Windows pipe name already scope an endpoint to one user (`crates/mcpls-core/src/hooks/identity.rs:92-124`), and that stays true.
- Upstreaming.

## Shape

Two roles for one binary.

**Frontend.** What the host launches over stdio. It resolves the project identity from its working directory, connects to that project's endpoint, and relays MCP traffic in both directions. It holds no language server, no document state and no delivery record. If nothing answers, it starts a backend and waits for it.

It is not a byte pipe. To report a missing backend the way the sections below require, it has to answer `initialize` and `tools/list` itself from the frozen tool surface (`crates/mcpls-core/src/mcp/tool_surface.json`), fail `tools/call` with the reason, and fail any request already in flight if the backend dies mid-session. So it parses JSON-RPC and falls back to a stub service, rather than copying frames blind.

**Backend.** One process per project. It owns the language servers, the document state, the file watcher, the diagnostics cache and every session's delivery record. It accepts many connections: one per attached session, plus the short-lived hook connections that already exist. It exits once the last session disconnects and a short timer expires.

Hooks do not change shape. `mcpls hook` still connects to the project endpoint and sends its operation; it simply reaches a process that is nobody's child.

### The endpoint

One endpoint per project, the one `identity_for` already derives: `{hash}.sock` beside `{hash}.lock` in the runtime directory, or `\\.\pipe\{prefix}{hash}` on Windows (`crates/mcpls-core/src/hooks/identity.rs:92-124`). Adding a second endpoint for MCP would double the naming, the locking, the lifetime and the Windows pipe work, and give the doctor two things to explain.

Every connection opens with a single-line handshake before either protocol starts. Its format is frozen: a build that cannot speak another build's protocol must still be able to read its handshake and answer a takeover request, which is what makes the upgrade path below work. It carries:

- the protocol version, so a frontend and a backend of different builds refuse each other by design rather than by accident;
- the connection kind, `mcp` or `hook`;
- the canonical project root the frontend resolved;
- the host session id when the host names one;
- a fingerprint of the frontend's effective configuration, and the source it came from: an explicit `--config` path, a trusted project-local `mcpls.toml`, a project-local file ignored for want of trust, the global file, or defaults. A boolean would miss `--config ./mcpls.toml`, which loads the project's own file by explicit path and is trusted unconditionally (`crates/mcpls-cli/src/main.rs:118-129`; `crates/mcpls-core/src/config/mod.rs:940-972`).

After the handshake the stream speaks one protocol for its lifetime.

### Identity stays the working directory

The hash covers the process working directory, and the hook side hashes the project directory the host names. Both keep doing that. Hashing resolved workspace roots instead would let sessions started in subdirectories share a backend, but hooks derive their identity from a directory, not from a config, and splitting the rule across the two sides is worse than the sharing it buys. Sessions started in different subdirectories of one repository therefore get a backend each. If that shows up in practice, it is a later change to both sides at once.

### Starting the backend

1. The frontend connects. If that succeeds, it is done.
2. Otherwise it takes the spawn lock and holds it until the endpoint accepts.
3. The winner spawns a detached backend, `setsid` on Unix and a detached process on Windows, so the backend outlives the session that started it. It then waits for the endpoint to accept.
4. Everyone else waits on the lock and retries the connect, so a loser attaches to the winner's backend instead of spawning a second one.

There is no runtime election left. The only race is which frontend spawns the backend, and the lock settles it at startup instead of through role transitions during service.

The spawn lock is its own file, not the ownership lock the hook listener takes today. That one is held for a serving process's whole life and authorises unlinking a stale socket (`crates/mcpls-core/src/hooks/listener.rs:223-255`), and a spawned child cannot inherit it because Rust sets CLOEXEC on the descriptor. Keeping them separate leaves each lock one meaning. `fs4` locks files on Windows as well as Unix, so the spawn lock exists on both platforms even though the ownership lock's path is empty there and exclusivity comes from `first_pipe_instance` (`crates/mcpls-core/src/hooks/identity.rs:29-36`; `crates/mcpls-core/src/hooks/listener.rs:255-270`).

The backend's standard streams decide whether detaching works at all. The frontend's stdin and stdout are the host's MCP pipes, and a child that inherits them holds the write end open, so the host never sees EOF when the frontend exits, and anything the backend writes to stdout lands in the middle of the MCP stream. The backend's streams are therefore redirected to null and a log file before it is spawned.

A backend that loses its lock or its socket file keeps serving the sessions it already holds and exits on the idle timer. It does not try to rebind. An age-based cleaner deleting either one costs one extra backend for as long as the old sessions last, which is cheaper than two processes racing for one endpoint.

If the backend cannot be started or cannot be reached before the deadline, the frontend fails loudly: it reports the reason through `get_info` instructions and fails tool calls with the same text. It does not silently start its own language servers, because a silent fallback is how you get the memory problem back without being told.

`--no-backend` covers the cases that need one process: a debugging session, or a host where detaching does not work. It serves exactly one stdio client in-process, binds the endpoint if it is free, and says so in its instructions if it is not.

### Upgrades, which look like a version mismatch

Updating the plugin leaves a backend of the old build running for as long as sessions keep it alive, so the next frontend to start is newer than the process it finds. The frontend reads the version out of the frozen handshake and takes one of two paths.

**Nobody else is attached:** the frontend asks the old backend to exit, waits for the endpoint to go, and starts its own. The user does nothing and sees nothing, which is the right outcome for the common case of updating between sessions.

**Other sessions are attached:** their work is not worth interrupting for an upgrade, so the frontend attaches to nothing and says so. The message names both versions and the one action that fixes it, restarting the other sessions, and it is repeated:

- once in `get_info` instructions, which the agent sees at startup;
- and on every tool call, as the error text, because an agent that read the instructions ten turns ago will otherwise keep calling a server that is not there.

The message is addressed to the user through the agent, in the imperative, so the agent relays it rather than trying to work around it.

**The frontend is older than the backend:** the same mismatch pointed the other way, which happens when two installs of mcpls serve one project, or when a build is rolled back while a backend from the newer one is still attached. The older frontend never evicts the backend. It attaches to nothing and reports the mismatch exactly as above. Allowing the eviction would let two hosts on different builds take turns killing each other's backend, and all the user would see is an unexplained reindex.

### Configuration and trust

The backend's configuration is whatever the frontend that started it had. A later frontend with different configuration is a real possibility, and silence would be the wrong answer.

- **Different fingerprint, same source:** the backend serves the session, names both fingerprints in its instructions, and the doctor reports the mismatch. This is the ordinary case rather than the edge, because `MCPLS_CONFIG` is set differently by what Claude Code's `.mcp.json` passes and what Codex passes.
- **Different trust:** the backend refuses, in both directions. One that loaded a project-local `mcpls.toml` refuses a frontend without `--trust-project-config` (`crates/mcpls-cli/src/args.rs:56-67`), and one that ignored that file for want of trust refuses a frontend that passes the flag. Project config can name the command mcpls spawns, so the two sides either agree about trusting it or they do not share a backend. The refusal carries the same shape of message as a version mismatch: both states named, and the action that reconciles them.

Whether the project file was ignored is currently a per-process fact reported through `get_info` (`crates/mcpls-core/src/mcp/handlers.rs:47-53`; `crates/mcpls-core/src/mcp/server.rs:1762-1768`). In a shared backend it describes the backend's own load, and each frontend's trust state is reported beside it.

### Idle shutdown

Configurable, defaulting to 10 seconds after the last `mcp` connection closes. Hook connections are per call and never hold the backend open.

The order on exit matters more than the timer. The backend stops accepting and closes every open stream first, then drains its language servers. A frontend arriving during the drain therefore fails to connect, takes the spawn lock, and starts a fresh backend, instead of handshaking with a process that is on its way out.

Closing every stream rather than only stopping `accept` is what Windows needs. A named pipe exists while any server-side instance handle is open, connected ones included, and `accept` creates the replacement instance before handing out the ready one so the count never reaches zero (`crates/mcpls-core/src/hooks/listener.rs:915-965`). A new backend's `first_pipe_instance` fails until the old process has dropped every instance, not merely stopped accepting. On Unix nothing unlinks the socket file on exit, so connect refuses as soon as the listener descriptor closes.

The cost is honest and worth naming: close the last session, wait past the timer, open a new one, and rust-analyzer reindexes from cold. The timer is the knob for people who alternate sessions quickly.

### Sessions, agents and records

The process stops being the session, so the session can no longer come from the process environment. Two of `SessionId::from_env_or_process`'s call sites in the server go with the forwarding paths, and the two that remain, `get_new_diagnostics` and the footer (`crates/mcpls-core/src/mcp/server.rs:1117,1400`), become a per-request lookup:

- **Codex** puts `session_id` and `thread_id` in `params._meta["x-codex-turn-metadata"]` on every tool call, and a top-level `params._meta.threadId` naming the calling thread, which is the simpler of the two to read (`codex-rs/core/src/mcp_tool_call.rs:1179-1250` at `rust-v0.150.0-alpha.7`). Its hook payloads carry a session id and an `agent_id` that is the subagent's thread id (`codex-rs/core/src/hook_runtime.rs:898-905`), but mcpls has no Codex hook support: the hooks file is Claude-only and the parser expects Claude's payload shape (`plugin/hooks/hooks.json`; `crates/mcpls-cli/src/hook.rs:31-56`). On Codex mcpls has the tool door and nothing else until an adapter exists.
- **Claude Code** exports `CLAUDE_CODE_SESSION_ID` to the frontend, which passes it in the handshake. Calls on that connection inherit it.
- **Neither:** the connection's own id, which keeps two anonymous sessions apart. This is where the HTTP transport lands, which is already better than the process-wide key it uses today.

Records are keyed by session and agent. A diagnostic goes to exactly one of them:

- a diagnostic in a file goes to every agent that wrote that file since the last time the file's diagnostics were delivered, which is one agent in every ordinary case;
- a diagnostic in a file nobody in the session wrote goes to the root session, which is where the fallout from a signature change lands;
- nobody else is told.

Keeping the recipient list to the file's own writers is the point. A shared record would let a subagent hear about a file another subagent is still editing, and a broadcast would have every agent in a workflow racing to fix the same error. Two agents writing one file in a turn is a conflict they both need to see, so both are told rather than only the later one; tooling that hands out file claims makes this case rare in the first place. Where a host names no agent, the session tree shares one record, which is the behaviour today.

Claude Code names an agent on one door only. Hook payloads carry `agent_id`, `agent_type` and `caller_session_id`, while a subagent's own tool call arrives on the session's connection with no agent marker. Per-agent records therefore key off the hook door, and a tool call with no agent reads the session's record, which is the fallback the rules above already describe.

A session's records live as long as its connections, counted rather than assumed:

- `SessionEnd` with no open connection for that session drops its records at once.
- `SessionEnd` while a connection is still open marks the session, and the drop happens when the last one closes.
- A connection closing with no `SessionEnd` starts a grace timer instead of dropping anything, so a session whose MCP server the host restarted keeps its delivery history and does not get every diagnostic again as new.

The grace defaults to 60 seconds and is configurable. It only has to outlive a restart, because a backend holding no sessions at all exits on the idle timer and takes every record with it. A grace longer than the idle timer therefore only has effect while some other session holds the backend open.

Records live in memory and leave with the backend. Persisting them would buy one case: the last session's connection drops, the backend times out mid-grace, and the session returns to hear its diagnostics again as new. A repeated diagnostic is cheaper than a file format to version, migrate and garbage collect.

### The watcher moves into the backend

One watcher per project instead of one per session, feeding the `WatchRegistry` and the open-document resync that the diagnostics design already built. It reuses the path filtering and `.gitignore` layering the sweeper already applies (`crates/mcpls-core/src/hooks/filters.rs:26-171`; `crates/mcpls-core/src/lib.rs:827-841`) and the existing quiet-period debounce. Recursive watching needs a new dependency; the workspace has no filesystem watcher today.

This closes the gap where a file changed by a shell command or an external editor leaves a language server holding a stale in-memory copy, and it closes it on every host rather than only where the host offers a file-changed hook. On Claude Code it retires `FileChanged` and the `watchPaths` reply, which is a snapshot of the project's top level taken once at `SessionStart` and blind to anything created later. That reply is the only reason the plugin registers `SessionStart` at all (`plugin/hooks/hooks.json`), so the hook goes with it, along with `watch_paths` itself (`crates/mcpls-core/src/hooks/filters.rs:183-223`), the doctor's scan line, and the tests and documentation covering them. The `changed` operation stays, because `PostToolBatch` still uses it.

A watcher must not act on the backend's own writes, which the content comparison the diagnostics design already performs takes care of. On Windows a rename arrives as a remove and an add rather than one event, and the sweep's stat-per-path handles that as it stands (`crates/mcpls-core/src/hooks/protocol.rs:8-11`).

## Stages

Each stage lands on main in a working state.

**Stage 1: the split.** Frontend, detached backend, handshake, spawn lock, idle shutdown, `--no-backend`, and the doctor reporting the backend's pid, uptime, attached sessions, language servers and configuration fingerprint. The owner, passive, demotion and forwarding paths are deleted here, because nothing can reach them once one process holds every record.

Session identity belongs to this stage rather than the next one. `SessionId::from_env_or_process` reads the process environment and falls back to one token per process (`crates/mcpls-core/src/bridge/delivery.rs:17,41-59`), and the backend inherits the environment of whichever frontend spawned it. Left in place it would hand every session the spawner's record: one session's `get_new_diagnostics` would consume another's, while the hook door kept keying correctly off the payload's session id, which is the split-record failure the diagnostics design built forwarding to prevent. So `McplsServer` carries the session its connection named in the handshake, the two remaining call sites read that field, and the backend never asks its own environment.

**Stage 2: the rest of identity.** The Codex `_meta` lookup, which is what remains once Stage 1 holds the handshake id and the connection fallback.

**Stage 3: the watcher.** Backend-side watching, and `FileChanged`, `watchPaths` and the `SessionStart` hook removed from the Claude plugin.

**Stage 4: attribution.** Last-writer tracking, the root-session fallback for unattributed diagnostics, and per-agent records wherever the host names an agent.

Plugin packaging is a separate document. It depends on this one only through which command the MCP entry launches, which is the frontend either way.

## Pending investigation

Two things here are asserted rather than established, and both are being checked.

**Codex behaviour.** Whether a subagent starts new MCP server processes or shares a pooled manager, which is what the memory argument rests on; the `_meta` keys at the version that actually ships; whether Codex exports a session id into a server's environment; what its hook system carries and whether mcpls can target it; whether it relaunches a server that exits mid-session, which is what the record grace exists for; and whether it places a server in a process group or job object that would kill a detached grandchild.

**Resource subscriptions.** The subscription set is one collection per process and notifications go through a write-once peer cell (`crates/mcpls-core/src/bridge/resources.rs:118-175`; `crates/mcpls-core/src/lib.rs:743-745`; `crates/mcpls-core/src/transport.rs:339-341`), so with many connections only the first to attach would hear anything, about files other sessions subscribed to. Either subscriptions become per connection and notifications fan out, or the backend stops advertising the capability (`crates/mcpls-core/src/mcp/server.rs:1747-1751`).

## Rejected alternatives

**An LSP-level multiplexer such as lspmux.** It shares one rust-analyzer between clients below mcpls. It leaves every mcpls process holding its own document state and delivery records, so the owner and passive machinery stays, and the sharing stops at language servers that the multiplexer has been tested against.

**The HTTP transport as the sharing mechanism.** It already shares one `McplsServer` across sessions, but it has to be started by hand, it is behind a non-default feature, and pointing a plugin at a TCP port means owning a port, its lifetime and its access control. A local socket the frontend can start on demand is the same sharing with none of that.

**Keeping the owner and passive roles, with language servers handed over.** A promoted process would have to rebuild every index the exiting owner had. The handover is the expensive thing, which is the argument for a process that no session owns.

## Verification

- Two sessions in one project: exactly one backend process and one rust-analyzer for the pair, and the second session answers a hover before a cold index could have finished.
- Kill the backend with a session attached: the frontend reports the failure through its instructions and does not start language servers of its own.
- Close the last session: the backend and its language servers are gone shortly after the timer, with no orphans.
- Connect during the drain: the arriving frontend gets a new backend, not the dying one.
- Two frontends racing from nothing: one backend, no error on either side.
- A newer frontend against an idle older backend: the old one exits, the new one serves, and nothing reaches the agent about it.
- The same with another session attached: every tool call carries the upgrade message naming both versions, and the attached session keeps working.
- An older frontend against a newer backend: the backend keeps serving its sessions, and the older frontend reports the mismatch instead of taking over.
- Two sessions whose trust flags disagree: the second is refused, with both states named.
- The socket file deleted under a running backend: its attached sessions keep working and the process exits on the idle timer.
- The backend's own output: nothing it writes reaches the host's MCP stream, and the host sees EOF when its frontend exits.
- Two sessions in one project, each asking for diagnostics: each reads its own record, and neither consumes the other's.
- A session whose connection drops and comes back inside the grace: its already-delivered diagnostics stay delivered.
- The same suite on Windows over the named pipe, since the endpoint code is shared.
