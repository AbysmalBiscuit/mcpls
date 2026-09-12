# One mcpls backend per project

Status: proposed. Nothing here is built.

Target: the `AbysmalBiscuit/mcpls` fork, not upstream. Defaults are tuned for one user running many agents on a desktop or laptop, and breaking changes to configuration are acceptable when they buy ergonomics.

Supersedes one non-goal of `2026-09-06-diagnostics-injection-design.md`, which reads "A standalone mcpls daemon surviving host restarts. The MCP process keeps owning the language servers." That is exactly what this document proposes, for the reason below. The rest of that design stands; this one moves where its pieces run.

Host behaviour for Codex was read out of the `codex-cli` source at `rust-v0.154.0` and is version specific; several of the behaviours below arrived in `0.148.0`, so an older Codex behaves differently. Claude Code behaviour is inherited from the diagnostics design's own reading of the host binary.

## Problem

Both hosts start one stdio MCP server per session, and every mcpls process spawns its own language servers. `serve_with_identity` validates config and resolves workspace roots from the process working directory (`crates/mcpls-core/src/lib.rs:619-690`), then spawns the applicable servers (`crates/mcpls-core/src/lib.rs:866-876`) before it ever touches a transport (`crates/mcpls-core/src/lib.rs:931`). Nothing in that path asks whether another mcpls already holds a warm index for the same project.

So ten parallel sessions in one checkout means ten rust-analyzer processes, each indexing the same code. On a laptop that is an out-of-memory kill, not a slowdown. Codex makes it worse. Every session builds its own MCP runtime owning its own connections, and a spawned subagent is a session, so subagents do not pool a parent's servers (`codex-rs/core/src/session/session.rs:1336`; `codex-rs/codex-mcp/src/runtime.rs:92`). Startup is lazy: a subagent can advertise cached tools without starting the server, and the process appears when the client is first obtained (`codex-rs/core/src/session/mcp_runtime.rs:383`). So ten spawned subagents do not immediately mean ten mcpls processes, but ten that actually call mcpls do.

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

It is not a byte pipe. To report a missing backend the way the sections below require, it has to answer `initialize` and `tools/list` itself from the frozen tool surface (`crates/mcpls-core/src/mcp/tool_surface.json`), fail `tools/call` with the reason, and fail any request already in flight if the backend dies mid-session. So it parses JSON-RPC and falls back to a stub service, rather than copying frames blind. It also passes through the notifications the backend sends unprompted, which answer no request of the host's.

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

The backend's standard streams decide whether detaching works at all. The frontend's stdin and stdout are the host's MCP pipes, and a child that inherits them holds the write end open, so the host never sees EOF when the frontend exits, and anything the backend writes to stdout lands in the middle of the MCP stream. Codex also reads a server's stderr continuously (`codex-rs/rmcp-client/src/stdio_server_launcher.rs:393`), so all three streams are redirected to null and a log file before the backend is spawned.

Leaving the frontend's process group matters as much. Codex launches a stdio server with `process_group(0)` and sends `SIGTERM` to that whole group on cleanup, then `SIGKILL` two seconds later (`codex-rs/rmcp-client/src/stdio_server_launcher.rs:274,354`). `setsid` puts the backend in a new session and group, so the group signal does not reach it.

Windows under Codex is the one platform where this does not work. Since `0.148.0` Codex assigns a stdio server to a job object with kill-on-close, created without allowing breakaway, before the child resumes (`codex-rs/rmcp-client/src/stdio_server_launcher.rs:290`; `codex-rs/utils/pty/src/win/job.rs:62`). A grandchild cannot escape such a job, so a backend spawned by a Codex frontend dies with it. Whether Claude Code on Windows does the same has not been checked, and the answer decides whether this needs a general mechanism or only a Codex fallback. Until it is settled this design does not deliver a shared backend to Codex on Windows.

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

- **Different fingerprint, same source:** the backend serves the session, names both fingerprints in its instructions, and the doctor reports the mismatch. This is the ordinary case rather than the edge, because each host's own configuration decides what reaches mcpls: Claude Code's `.mcp.json` sets environment variables directly, while Codex clears the environment and passes only what the server's own entry names (`codex-rs/rmcp-client/src/stdio_server_launcher.rs:269`).
- **Different trust:** the backend refuses, in both directions. One that loaded a project-local `mcpls.toml` refuses a frontend without `--trust-project-config` (`crates/mcpls-cli/src/args.rs:56-67`), and one that ignored that file for want of trust refuses a frontend that passes the flag. Project config can name the command mcpls spawns, so the two sides either agree about trusting it or they do not share a backend. The refusal carries the same shape of message as a version mismatch: both states named, and the action that reconciles them.

Whether the project file was ignored is currently a per-process fact reported through `get_info` (`crates/mcpls-core/src/mcp/handlers.rs:47-53`; `crates/mcpls-core/src/mcp/server.rs:1762-1768`). In a shared backend it describes the backend's own load, and each frontend's trust state is reported beside it.

### Idle shutdown

Configurable, defaulting to 10 seconds after the last `mcp` connection closes. Hook connections are per call and never hold the backend open.

The order on exit matters more than the timer. The backend stops accepting and closes every open stream first, then drains its language servers. A frontend arriving during the drain therefore fails to connect, takes the spawn lock, and starts a fresh backend, instead of handshaking with a process that is on its way out.

Closing every stream rather than only stopping `accept` is what Windows needs. A named pipe exists while any server-side instance handle is open, connected ones included, and `accept` creates the replacement instance before handing out the ready one so the count never reaches zero (`crates/mcpls-core/src/hooks/listener.rs:915-965`). A new backend's `first_pipe_instance` fails until the old process has dropped every instance, not merely stopped accepting. On Unix nothing unlinks the socket file on exit, so connect refuses as soon as the listener descriptor closes.

The cost is honest and worth naming: close the last session, wait past the timer, open a new one, and rust-analyzer reindexes from cold. The timer is the knob for people who alternate sessions quickly.

### Sessions, agents and records

The process stops being the session, so the session can no longer come from the process environment. Two of `SessionId::from_env_or_process`'s call sites in the server go with the forwarding paths, and the two that remain, `get_new_diagnostics` and the footer (`crates/mcpls-core/src/mcp/server.rs:1117,1400`), become a per-request lookup:

- **Codex** identifies the caller by thread, not by session. A spawned subagent shares its root's `session_id`, so the discriminator is the thread: `params._meta["x-codex-turn-metadata"].thread_id`, or the simpler top-level `params._meta.threadId`, both naming the issuing thread on every model-issued tool call (`codex-rs/core/src/mcp_tool_call.rs:1238,506`). Subagents also carry `parent_thread_id` and `subagent_kind`, which a root thread omits. Keying on `session_id` would merge a root and all of its subagents into one record. Codex exports no session variable at all: it clears the environment and rebuilds it from a fixed list plus what the server's own entry names (`codex-rs/rmcp-client/src/stdio_server_launcher.rs:269`), so per-request metadata is the only route. Calls issued by hooks rather than by the model guarantee `threadId` but not the turn metadata (`codex-rs/core/src/hook_mcp_executor.rs:19`), so a missing key is a fallback rather than an error.
- **Claude Code** exports `CLAUDE_CODE_SESSION_ID` to the frontend, which passes it in the handshake. Calls on that connection inherit it.
- **Neither:** the connection's own id, which keeps two anonymous sessions apart. This is where the HTTP transport lands, which is already better than the process-wide key it uses today.

Records are keyed by session and agent. A diagnostic goes to exactly one of them:

- a diagnostic in a file goes to every agent that wrote that file since the last time the file's diagnostics were delivered, which is one agent in every ordinary case;
- a diagnostic in a file nobody in the session wrote goes to the root session, which is where the fallout from a signature change lands;
- nobody else is told.

Keeping the recipient list to the file's own writers is the point. A shared record would let a subagent hear about a file another subagent is still editing, and a broadcast would have every agent in a workflow racing to fix the same error. Two agents writing one file in a turn is a conflict they both need to see, so both are told rather than only the later one; tooling that hands out file claims makes this case rare in the first place. Where a host names no agent, the session tree shares one record, which is the behaviour today.

Codex has a hook door too, and mcpls does not speak it. Hooks are enabled by default at this version, load from `.codex/hooks.json` among other places, and a synchronous `PostToolUse` hook returns the same `hookSpecificOutput.additionalContext` shape mcpls already emits for Claude Code (`codex-rs/hooks/src/schema.rs:228`; `codex-rs/core/src/tools/registry.rs:674`). mcpls cannot read it yet because the hooks file is Claude-only and the parser expects Claude's payload (`plugin/hooks/hooks.json`; `crates/mcpls-cli/src/hook.rs:31-56`). An adapter is feasible rather than speculative, with one real obstacle: Codex has no changed-files field. `apply_patch` hands over the raw patch as `tool_input.command` (`codex-rs/core/src/tools/handlers/apply_patch.rs:458`), so an adapter has to parse the patch to learn which files were touched.

Claude Code names an agent on one door only. Hook payloads carry `agent_id`, `agent_type` and `caller_session_id`, while a subagent's own tool call arrives on the session's connection with no agent marker. Per-agent records therefore key off the hook door, and a tool call with no agent reads the session's record, which is the fallback the rules above already describe.

A session's records live as long as its connections, counted rather than assumed:

- `SessionEnd` with no open connection for that session drops its records at once.
- `SessionEnd` while a connection is still open marks the session, and the drop happens when the last one closes.
- A connection closing with no `SessionEnd` starts a grace timer instead of dropping anything, so a session whose MCP server the host restarted keeps its delivery history and does not get every diagnostic again as new.

Those rules are written for Claude Code, which is the host that names both events. On Codex `SessionEnd` is root-only and a spawned child's teardown is `SubagentStop` instead (`codex-rs/core/src/hook_runtime.rs:124,455`), so a subagent's records drop on that event and the root's on `SessionEnd`.

The grace defaults to 60 seconds and is configurable. Claude Code is the reason it exists; Codex has no crash-to-relaunch supervisor for a stdio server at all, only an explicit MCP refresh that preserves the owning identities (`codex-rs/core/src/session/handlers.rs:237`), so on Codex the grace covers a deliberate refresh rather than a crash. It only has to outlive either, because a backend holding no sessions at all exits on the idle timer and takes every record with it. A grace longer than the idle timer therefore only has effect while some other session holds the backend open.

Records live in memory and leave with the backend. Persisting them would buy one case: the last session's connection drops, the backend times out mid-grace, and the session returns to hear its diagnostics again as new. A repeated diagnostic is cheaper than a file format to version, migrate and garbage collect.

### The watcher moves into the backend

One watcher per project instead of one per session, feeding the `WatchRegistry` and the open-document resync that the diagnostics design already built. It reuses the path filtering and `.gitignore` layering the sweeper already applies (`crates/mcpls-core/src/hooks/filters.rs:26-171`; `crates/mcpls-core/src/lib.rs:827-841`) and the existing quiet-period debounce. Recursive watching needs a new dependency; the workspace has no filesystem watcher today.

This closes the gap where a file changed by a shell command or an external editor leaves a language server holding a stale in-memory copy, and it closes it on every host rather than only where the host offers a file-changed hook. On Claude Code it retires `FileChanged` and the `watchPaths` reply, which is a snapshot of the project's top level taken once at `SessionStart` and blind to anything created later. That reply is the only reason the plugin registers `SessionStart` at all (`plugin/hooks/hooks.json`), so the hook goes with it, along with `watch_paths` itself (`crates/mcpls-core/src/hooks/filters.rs:183-223`), the doctor's scan line, and the tests and documentation covering them. The `changed` operation stays, because `PostToolBatch` still uses it.

A watcher must not act on the backend's own writes, which the content comparison the diagnostics design already performs takes care of. On Windows a rename arrives as a remove and an add rather than one event, and the sweep's stat-per-path handles that as it stands (`crates/mcpls-core/src/hooks/protocol.rs:8-11`).

### Resource subscriptions become per connection

The subscription set is one collection per process and notifications go through a write-once peer cell (`crates/mcpls-core/src/bridge/resources.rs:118-175`; `crates/mcpls-core/src/lib.rs:743-745`; `crates/mcpls-core/src/transport.rs:339-341`), so with many connections only the first to attach would hear anything, and about files other sessions subscribed to.

Dropping the capability would be less code, and nothing calls it today: neither host sends `resources/subscribe`, neither does more than log `notifications/resources/updated`, and rmcp has deprecated both handlers as legacy-only for the protocol version mcpls negotiates. It stays anyway, because it is upstream's feature rather than this fork's (`bug-ops/mcpls` 0.3.7), this fork still merges from upstream, and deleting code that upstream keeps developing buys a conflict on every pull. A host that starts subscribing later would also find the mechanism intact rather than needing it rebuilt.

So the subscription set becomes a map from connection to that connection's peer and URIs, `subscribe` records the peer its request arrived on, and the pump sends to the peers subscribed to each URI. rmcp's request context carries the peer but no connection id, so the id is assigned per accept and lives on the `McplsServer` clone that connection already gets, the way the HTTP transport clones one per session today (`crates/mcpls-core/src/transport.rs:454-458`). A connection's entry goes when its service stops.

The pump must stop treating a failed notify as its own death. Today `handle_publish_diagnostics` returns true when the single peer's notify fails and the pump breaks (`crates/mcpls-core/src/lib.rs:184-186,268-278`), which in a shared backend would let one session disconnecting stop diagnostics caching for every session. A failed notify prunes that peer instead.

This is the only message the backend sends that answers no request, so the frontend relay carries it in the backend-to-host direction unsolicited, keyed by nothing but its absence of an id.

## Stages

Each stage lands on main in a working state.

**Stage 1: the split.** Frontend, detached backend, handshake, spawn lock, idle shutdown, `--no-backend`, and the doctor reporting the backend's pid, uptime, attached sessions, language servers and configuration fingerprint. The owner, passive, demotion and forwarding paths are deleted here, because nothing can reach them once one process holds every record. Per-connection resource subscriptions land here too: one process serving many connections is exactly what breaks the single peer cell.

Session identity belongs to this stage rather than the next one. `SessionId::from_env_or_process` reads the process environment and falls back to one token per process (`crates/mcpls-core/src/bridge/delivery.rs:17,41-59`), and the backend inherits the environment of whichever frontend spawned it. Left in place it would hand every session the spawner's record: one session's `get_new_diagnostics` would consume another's, while the hook door kept keying correctly off the payload's session id, which is the split-record failure the diagnostics design built forwarding to prevent. So `McplsServer` carries the session its connection named in the handshake, the two remaining call sites read that field, and the backend never asks its own environment.

**Stage 2: the rest of identity.** The Codex `_meta` lookup, which is what remains once Stage 1 holds the handshake id and the connection fallback.

**Stage 3: the watcher.** Backend-side watching, and `FileChanged`, `watchPaths` and the `SessionStart` hook removed from the Claude plugin.

**Stage 4: attribution.** Last-writer tracking, the root-session fallback for unattributed diagnostics, and per-agent records wherever the host names an agent. On Claude Code that is the hook door as it stands. On Codex it needs the hook adapter and a patch parser, so it is the larger half of this stage rather than a variation on the first.

Plugin packaging is a separate document. It depends on this one only through which command the MCP entry launches, which is the frontend either way.

## Open decisions

**Windows under Codex.** Codex's job object has kill-on-close and forbids breakaway, so a backend spawned by a Codex frontend cannot outlive it. Whether Claude Code on Windows does the same decides the shape of the answer, so that check comes first. The options after it are spawning the backend through something outside the job, or declaring that Codex on Windows runs in-process and saying so in its instructions.

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
- A Codex root and a subagent it spawned: each reads its own record, though both carry the same `session_id`.
- A Codex session ending: the backend outlives the group cleanup that kills the frontend.
- Two sessions subscribed to different files: each is notified about its own and neither about the other's, and one of them disconnecting does not stop the other's diagnostics.
- A session whose connection drops and comes back inside the grace: its already-delivered diagnostics stay delivered.
- The same suite on Windows over the named pipe, since the endpoint code is shared.
