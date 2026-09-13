# One mcpls backend per project

Status: Stage 1 is built. Stages 2 to 4 are not.

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
- Cross-user sharing. The Windows pipe name carries the user and so does the Unix runtime directory (`crates/mcpls-core/src/hooks/identity.rs:92-124,225-230`). A name is the whole of it today, because nothing sets a mode on that directory (`crates/mcpls-core/src/hooks/listener.rs:230`), so the listener sets it to `0o700` when it creates it and the scoping stops resting on the umask.
- Upstreaming.

## Shape

Two roles for one binary.

**Frontend.** What the host launches over stdio. It resolves the project identity from its working directory, connects to that project's endpoint, and relays MCP traffic in both directions. It holds no language server, no document state and no delivery record. If nothing answers, it starts a backend and waits for it.

It is not a byte pipe. To report a missing backend the way the sections below require, it has to answer `initialize` and `tools/list` itself from the frozen tool surface (`crates/mcpls-core/src/mcp/tool_surface.json`), fail `tools/call` with the reason, and fail any request already in flight if the backend dies mid-session. So it parses JSON-RPC and falls back to a stub service, rather than copying frames blind. It also passes through the notifications the backend sends unprompted, which answer no request of the host's.

**Backend.** One process per project. It owns the language servers, the document state, the file watcher, the diagnostics cache and every session's delivery record. It accepts many connections: one per attached session, plus the short-lived hook connections that already exist. It exits once the last session disconnects and a short timer expires.

Hooks do not change shape. `mcpls hook` still connects to the project endpoint and sends its operation; it simply reaches a process that is nobody's child.

### The endpoint

One endpoint per project, the one `identity_for` already derives: `{hash}.sock` beside `{hash}.lock` in the runtime directory, or `\\.\pipe\{prefix}{hash}` on Windows (`crates/mcpls-core/src/hooks/identity.rs`). Adding a second endpoint for MCP would double the naming, the locking, the lifetime and the Windows pipe work, and give the doctor two things to explain.

The runtime directory reads only variables both hosts pass. Codex launches a stdio server with a cleared environment and a fixed list that carries `HOME`, `USER`, `LOGNAME` and `TMPDIR` but not `XDG_RUNTIME_DIR` (`codex-rs/rmcp-client/src/utils.rs`), while its hook commands inherit the whole environment (`codex-rs/hooks/src/engine/command_runner.rs`). A runtime directory chosen from `XDG_RUNTIME_DIR` would put a Codex frontend and a Codex hook in different directories on Linux whenever the variable is set, and they would never meet. The socket therefore lives under the system temporary directory, in a directory carrying the user, on every Unix host. The listener creates that directory owner-only and changes its mode through a handle opened without following a symlink, so a symlink planted at that name in the shared temporary directory fails the open instead of redirecting the change onto its target. On Windows the pipe prefix reads `USERNAME`, which Codex passes (`codex-rs/protocol/src/shell_environment.rs`).

Every connection opens with a single-line handshake before either protocol starts. Its format is frozen: a build that cannot speak another build's protocol must still be able to read its handshake and answer a takeover request, which is what makes the upgrade path below work. It carries:

- the protocol version, so a frontend and a backend of different builds refuse each other by design rather than by accident;
- the connection kind, `mcp` or `hook`;
- the canonical project root the frontend resolved;
- the host session id when the host names one;
- a fingerprint of the frontend's effective configuration, and the source it came from: an explicit `--config` path, a trusted project-local `mcpls.toml`, a project-local file ignored for want of trust, the global file, or defaults. A boolean would miss `--config ./mcpls.toml`, which loads the project's own file by explicit path and is trusted unconditionally (`crates/mcpls-cli/src/main.rs:118-129`; `crates/mcpls-core/src/config/mod.rs:940-972`).

After the handshake the stream speaks one protocol for its lifetime.

### Identity is the checkout root

Both sides hash one directory: the nearest checkout root enclosing the directory the host hands them. The frontend starts from its working directory, which is where both hosts launch a stdio server (`crates/mcpls-core/src/lib.rs:594-601`; `codex-rs/rmcp-client/src/stdio_server_launcher.rs:273,282`). The hook starts from the directory the host names: `CLAUDE_PROJECT_DIR` under Claude Code (`crates/mcpls-cli/src/main.rs:40-48`), and the payload's `cwd` under Codex, which carries the turn's working directory and is also the directory the hook command runs in (`codex-rs/hooks/src/schema.rs:283`; `codex-rs/core/src/hook_runtime.rs:133`; `codex-rs/hooks/src/engine/command_runner.rs:62`). The Codex adapter ignores `CLAUDE_PROJECT_DIR`, which a Codex run started from a Claude Code shell inherits. Neither side reads a config: the resolution needs only the directory.

The resolution is a walk, not a git invocation. The start directory is canonicalized through `dunce` as it is today (`crates/mcpls-core/src/hooks/identity.rs:49-53`). Then the directory and each of its ancestors are tried in turn, and the first one holding a `.git` entry git would accept is the root: a directory holding `HEAD`, or a file whose contents start with `gitdir:`. A bare `.git` name is not enough, because an empty one left behind in a shared directory such as `/tmp` would claim every directory beneath it. The walk stops before the home directory and never reaches the filesystem root, so a dotfiles repository in the home directory is never the root of a project beneath it. When nothing matches, the start directory itself is the root, which is today's behaviour. The walk is idempotent, so a backend spawned with the root as its working directory derives the same identity its spawner did.

The hash covers the canonical root, the handshake carries it, and the backend's workspace base is the root rather than the working directory of whichever frontend spawned it (`crates/mcpls-core/src/lib.rs:662-672`). A session started in a subdirectory therefore gets the servers a session started at the root gets, and its files fall inside the workspace. A monorepo's sub-project that a language server cannot discover from the checkout root is named in `workspace.roots`, which keeps its meaning: it scopes the servers inside a backend and plays no part in the endpoint name.

A `.git` entry is the test because it marks a working tree rather than a repository. A linked worktree holds a `.git` file pointing into the main checkout, and so does a submodule, so each is its own root, and several worktrees of one repository never share a backend. `git rev-parse --git-common-dir` would collapse them, which is why nothing here resolves through git. Nested repositories fall the same way: the nearest entry wins, a session in a submodule gets the submodule's backend, and a session at the outer root gets the outer one, whose servers may also index the submodule's files. That is a split, never a merge. A directory with no `.git` anywhere beneath home splits by subdirectory as it does today.

Where the two sides can still disagree, a directory rule cannot help. A Codex server entry whose `cwd` names a directory outside the checkout puts the frontend at that directory's root while every hook resolves to the checkout, so that backend serves the session and hears no hooks; `mcpls hook doctor` prints the start directory, the root and the hash on each side for exactly this case.

The rule is not configurable. Two processes on different rules derive different names and never open a handshake, so the fingerprint refusal above cannot report the mismatch, and a hook process has no mcpls configuration to read a knob from. `--no-backend` remains the escape for a checkout the rule serves badly. The walk costs a stat per ancestor on each hook invocation, adds no dependency, and lives in a file upstream does not have.

### Starting the backend

1. The frontend connects. If that succeeds, it is done.
2. Otherwise it takes the spawn lock and holds it until the endpoint accepts.
3. The winner starts a detached backend, using `process_group(0)` on Unix and a request the next hook invocation honours on Windows, for the reasons below. It uses `process_group(0)` rather than `setsid`, which needs `unsafe` code this workspace denies; a new process group is what leaves the group Codex signals. It then waits for the endpoint to accept.
4. Everyone else waits on the lock and retries the connect, so a loser attaches to the winner's backend instead of spawning a second one.

There is no runtime election left. The only race is which frontend spawns the backend, and the lock settles it at startup instead of through role transitions during service.

The spawn lock is its own file, not the ownership lock the hook listener takes today. That one is held for a serving process's whole life and authorises unlinking a stale socket (`crates/mcpls-core/src/hooks/listener.rs:223-255`), and a spawned child cannot inherit it because Rust sets CLOEXEC on the descriptor. Keeping them separate leaves each lock one meaning. `fs4` locks files on Windows as well as Unix, so the spawn lock exists on both platforms even though the ownership lock's path is empty there and exclusivity comes from `first_pipe_instance` (`crates/mcpls-core/src/hooks/identity.rs:29-36`; `crates/mcpls-core/src/hooks/listener.rs:255-270`).

The backend's standard streams decide whether detaching works at all. The frontend's stdin and stdout are the host's MCP pipes, and a child that inherits them holds the write end open, so the host never sees EOF when the frontend exits, and anything the backend writes to stdout lands in the middle of the MCP stream. Codex also reads a server's stderr continuously (`codex-rs/rmcp-client/src/stdio_server_launcher.rs:393`), so all three streams are redirected to null and a log file before the backend is spawned.

Leaving the frontend's process group matters as much. Codex launches a stdio server with `process_group(0)` and sends `SIGTERM` to that whole group on cleanup, then `SIGKILL` two seconds later (`codex-rs/rmcp-client/src/stdio_server_launcher.rs:274,354`). The backend uses its own process group, so the group signal does not reach it.

Windows under Codex cannot spawn the backend as an ordinary child. Codex creates a job object per server launch, with kill-on-close and without breakaway, and assigns the suspended server before resuming it (`codex-rs/rmcp-client/src/stdio_server_launcher.rs:290`; `codex-rs/utils/pty/src/win/job.rs:62`), so anything the frontend spawns normally stays inside and dies with it. Wrapping the spawn in another executable changes nothing, because the wrapper is in the job too.

That is measured, not only read. A `codex exec` session's MCP server, a detached grandchild of it, and a grandchild spawned with `CREATE_BREAKAWAY_FROM_JOB` all stopped within a second of the host exiting, with `features.experimental_use_rmcp_client` set either way, so containment does not depend on which MCP client Codex uses. The measurement went through `codex exec` rather than the interactive interface; both use the same server launcher, so the containment should be the same, but that is untested.

`CREATE_BREAKAWAY_FROM_JOB` is neither an escape hatch nor a probe. It returned no error under either host, and the child it created still died under Codex, so a spawn that succeeds says nothing about whether the backend will outlive the session.

The job is per launch rather than per session or per Codex process, so a backend started by something already outside it survives. A hook invocation is that something. Codex launches command hooks from a permissive job that allows breakaway, and its runner disables kill-on-close for descendants once the command finishes (`codex-rs/utils/pty/src/win/job.rs:41`; `codex-rs/hooks/src/engine/command_runner.rs:228,286`), so `mcpls hook` already runs outside the MCP server's job. On Windows the frontend therefore does not spawn the backend at all: it asks, and the next hook invocation starts it.

Claude Code does not contain descendants at all. Measured the same way, its MCP server process died with the host while both grandchildren kept writing, in a headless session and in an interactive one closed with `/exit`. A direct spawn would work there. Both hosts still go through the hook, because nothing the frontend can ask at runtime tells it which host contains it. On the measuring machine a security product had assigned every process a single-member job carrying kill-on-close, a process created through WMI outside any agent's tree included, so the flags a process reads about its own job describe that job rather than the host's. Behaviour separates the two hosts and job introspection does not, which is also why the doctor must not try to read containment off a job's flags.

The hook reuses a door the design already has rather than adding one. `Win32_Process.Create` over WMI is the alternative, since a process it creates does not inherit the caller's job, but it is a platform-specific dependency for one case.

Asking rather than spawning costs latency: the backend appears when a hook next fires rather than at connect time. The frontend reports the wait the same way it reports any unreachable backend. Both hosts fire a session-start hook, which lands before the first tool call, so the wait is usually invisible. A session with no mcpls hooks installed never gets a backend on Windows, which makes the plugin a prerequisite there rather than a convenience, and `--no-backend` the answer for a checkout without it.

The fallback when job creation fails is harmless here: it holds a handle to the server process and calls `TerminateProcess`, which reaches no descendants. An older Codex used `taskkill /T`, which killed the tree.

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

- **Different fingerprint, same source:** the backend serves the session, names both fingerprints in its instructions, and the doctor reports the mismatch. This is the ordinary case rather than the edge, because each host's own configuration decides what reaches mcpls: Claude Code's `.mcp.json` sets environment variables directly, while Codex clears the environment and passes a fixed list plus what the server's own entry names (`codex-rs/rmcp-client/src/stdio_server_launcher.rs:269`; `codex-rs/rmcp-client/src/utils.rs:122-134`).
- **Different trust:** the backend refuses, in both directions. One that loaded a project-local `mcpls.toml` refuses a frontend without `--trust-project-config` (`crates/mcpls-cli/src/args.rs:56-67`), and one that ignored that file for want of trust refuses a frontend that passes the flag. Project config can name the command mcpls spawns, so the two sides either agree about trusting it or they do not share a backend. The refusal carries the same shape of message as a version mismatch: both states named, and the action that reconciles them.

Whether the project file was ignored is currently a per-process fact reported through `get_info` (`crates/mcpls-core/src/mcp/handlers.rs:47-53`; `crates/mcpls-core/src/mcp/server.rs:1762-1768`). In a shared backend it describes the backend's own load, and each frontend's trust state is reported beside it.

### Idle shutdown

Configurable, defaulting to 10 seconds after the last `mcp` connection closes. Hook connections are per call and never hold the backend open.

The order on exit matters more than the timer. The backend stops accepting and closes every open stream first, then drains its language servers. A frontend arriving during the drain therefore fails to connect, takes the spawn lock, and starts a fresh backend, instead of handshaking with a process that is on its way out.

Closing every stream rather than only stopping `accept` is what Windows needs. A named pipe exists while any server-side instance handle is open, connected ones included, and `accept` creates the replacement instance before handing out the ready one so the count never reaches zero (`crates/mcpls-core/src/hooks/listener.rs:915-965`). A new backend's `first_pipe_instance` fails until the old process has dropped every instance, not merely stopped accepting. Measurement bounds that: binding the same name while the holder's connected stream was open failed with `ERROR_PIPE_BUSY`, with and without `first_pipe_instance` and against a one-instance limit, and succeeded once the holder dropped that stream while the holder itself kept running. Dropping the handles is enough, so the ordering asks for no more than it needs. On Unix nothing unlinks the socket file on exit, so connect refuses as soon as the listener descriptor closes.

The cost is honest and worth naming: close the last session, wait past the timer, open a new one, and rust-analyzer reindexes from cold. The timer is the knob for people who alternate sessions quickly.

### Sessions, agents and records

The process stops being the session, so the session can no longer come from the process environment. Two of `SessionId::from_env_or_process`'s call sites in the server go with the forwarding paths, and the two that remain, `get_new_diagnostics` and the footer (`crates/mcpls-core/src/mcp/server.rs:1117,1400`), become a per-request lookup:

- **Codex** identifies the caller by thread, not by session. A spawned subagent shares its root's `session_id`, so the discriminator is the thread: `params._meta["x-codex-turn-metadata"].thread_id`, or the simpler top-level `params._meta.threadId`, both naming the issuing thread on every model-issued tool call (`codex-rs/core/src/mcp_tool_call.rs:1238,506`). Subagents also carry `parent_thread_id` and `subagent_kind`, which a root thread omits. Keying on `session_id` would merge a root and all of its subagents into one record. The hook door names the same thread: a subagent's hook payload carries `agent_id`, which is the subagent's thread id, beside the root's `session_id`, and a root's payload omits it (`codex-rs/core/src/hook_runtime.rs`). Logged Codex hook payloads on Windows and WSL agree, and every one also carries a `turn_id`. So a tool call's `threadId` and a hook's `agent_id` are one key, falling back to `session_id` for the root. Codex exports no session variable at all: it clears the environment and rebuilds it from a fixed list plus what the server's own entry names (`codex-rs/rmcp-client/src/stdio_server_launcher.rs:269`), so per-request metadata is the only route. Calls issued by hooks rather than by the model guarantee `threadId` but not the turn metadata (`codex-rs/core/src/hook_mcp_executor.rs:19`), so a missing key is a fallback rather than an error.
- **Claude Code** exports `CLAUDE_CODE_SESSION_ID` to the frontend, which passes it in the handshake. Calls on that connection inherit it.
- **A Codex connection starts anonymous.** `initialize` carries no thread id, so the frontend has nothing to put in the handshake and the backend holds the connection under its own id until the first identified tool call arrives (`codex-rs/codex-mcp/src/rmcp_client.rs:1049`). A connection therefore adopts an identity mid-life and its records merge into that thread's at that point. This is also how a session recovers its history after a refresh: the replacement process presents the same thread id, since Codex keeps it on the session rather than deriving it from the connection (`codex-rs/core/src/session/mcp_runtime.rs:322`; `codex-rs/core/src/mcp_tool_call.rs:512`), but only once a call reveals it. A connection that never makes an identified call keeps its own records and loses them on close.
- **Neither:** the connection's own id, which keeps two anonymous sessions apart. This is where the HTTP transport lands, which is already better than the process-wide key it uses today.

Records are keyed by session and agent. A diagnostic goes to exactly one of them:

- a diagnostic in a file goes to every agent that wrote that file since the last time the file's diagnostics were delivered, which is one agent in every ordinary case;
- a diagnostic in a file nobody in the session wrote goes to the root session, which is where the fallout from a signature change lands;
- nobody else is told.

Keeping the recipient list to the file's own writers is the point. A shared record would let a subagent hear about a file another subagent is still editing, and a broadcast would have every agent in a workflow racing to fix the same error. Two agents writing one file in a turn is a conflict they both need to see, so both are told rather than only the later one; tooling that hands out file claims makes this case rare in the first place. Where a host names no agent, the session tree shares one record, which is the behaviour today.

Codex has a hook door too, and mcpls does not speak it. Hooks are enabled by default at this version, load from `.codex/hooks.json` among other places, and a synchronous `PostToolUse` hook returns the same `hookSpecificOutput.additionalContext` shape mcpls already emits for Claude Code (`codex-rs/hooks/src/schema.rs:228`; `codex-rs/core/src/tools/registry.rs:674`). mcpls cannot read it yet because the hooks file is Claude-only and the parser expects Claude's payload (`plugin/hooks/hooks.json`; `crates/mcpls-cli/src/hook.rs:31-56`). An adapter is feasible rather than speculative, with two obstacles. Codex has no changed-files field: `apply_patch` hands over the raw patch as `tool_input.command` (`codex-rs/core/src/tools/handlers/apply_patch.rs:458`), so an adapter has to parse the patch to learn which files were touched. It also has no `CLAUDE_PROJECT_DIR`, so it reads its project directory from the payload's `cwd`, which every Codex hook payload carries as a required field (`codex-rs/hooks/src/schema.rs:283`).

Claude Code names an agent on one door only. A subagent's hook payloads carry `agent_id` and, almost always, `agent_type`, beside the root's `session_id` and `transcript_path`; no field names a caller or parent session, so `agent_id` is the only thing that tells a subagent's event from the root's. A subagent's own tool call arrives on the session's connection with no agent marker. Per-agent records therefore key off the hook door, and a tool call with no agent reads the session's record, which is the fallback the rules above already describe.

A session's records live as long as its connections, counted rather than assumed:

- `SessionEnd` with no open connection for that session drops its records at once.
- `SessionEnd` while a connection is still open marks the session, and the drop happens when the last one closes.
- A connection closing with no `SessionEnd` starts a grace timer instead of dropping anything, so a session whose MCP server the host restarted keeps its delivery history and does not get every diagnostic again as new.

Those rules are written for Claude Code, which is the host that names both events. Codex names neither for a subagent. Its `SessionEnd` is root-only, and `SubagentStop` is a turn-stop hook rather than a lifetime one: it fires after a child's turn settles, can fire again when the child takes more work, can itself block stopping, and is skipped on interrupt, on a sampling error, on parent cancellation and on a crash (`codex-rs/core/src/session/turn.rs:552,615`; `codex-rs/core/src/tasks/mod.rs:900`; `codex-rs/core/src/hook_runtime.rs:464,486`).

So no event is authoritative. Closing a connection plus the grace is what expires a record, and a host event only accelerates it where the host offers one. Treating `SubagentStop` as a teardown would both leak records for children that never fire it and delete records for children still working.

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

The pump already survives a failed notify, because in a shared backend one session disconnecting must not stop diagnostics caching for every session. With the registry, a failed notify also prunes that peer.

This is the only message the backend sends that answers no request, so the frontend relay carries it in the backend-to-host direction unsolicited, keyed by nothing but its absence of an id.

## Stages

Each stage lands on main in a working state.

**Stage 1: the split.** Frontend, detached backend, handshake, spawn lock, idle shutdown, `--no-backend`, and the doctor reporting the backend's pid, uptime, attached sessions, language servers and configuration fingerprint. The owner, passive, demotion and forwarding paths are deleted here, because nothing can reach them once one process holds every record. Per-connection resource subscriptions land here too: one process serving many connections is exactly what breaks the single peer cell. It lands as the frontend and backend described above, with the decisions recorded in `docs/superpowers/plans/2026-09-13-shared-backend-split.md` under "Decisions this plan makes".

Session identity belongs to this stage rather than the next one. `SessionId::from_env_or_process` reads the process environment and falls back to one token per process (`crates/mcpls-core/src/bridge/delivery.rs:17,41-59`), and the backend inherits the environment of whichever frontend spawned it. Left in place it would hand every session the spawner's record: one session's `get_new_diagnostics` would consume another's, while the hook door kept keying correctly off the payload's session id, which is the split-record failure the diagnostics design built forwarding to prevent. So `McplsServer` carries the session its connection named in the handshake, the two remaining call sites read that field, and the backend never asks its own environment.

The identity rule landed ahead of the split, because the endpoint's name is the first thing the two sides have to agree on (`docs/superpowers/plans/2026-09-12-shared-backend-prerequisites.md`). Both sides call `project_root` before hashing, the workspace base is the root, and the Unix runtime directory no longer reads `XDG_RUNTIME_DIR`. Project configuration is found at the checkout root, so a session started in a subdirectory reads the checkout's `mcpls.toml`.

**Stage 2: the rest of identity.** The Codex `_meta` lookup, which is what remains once Stage 1 holds the handshake id and the connection fallback.

**Stage 3: the watcher.** Backend-side watching, and `FileChanged`, `watchPaths` and the `SessionStart` hook removed from the Claude plugin.

**Stage 4: attribution.** Last-writer tracking, the root-session fallback for unattributed diagnostics, and per-agent records wherever the host names an agent. On Claude Code that is the hook door as it stands. On Codex it needs the hook adapter and a patch parser, so it is the larger half of this stage rather than a variation on the first.

Plugin packaging is a separate document. It depends on this one only through which command the MCP entry launches, which is the frontend either way.

## Open decisions

None at present.

## Rejected alternatives

**An LSP-level multiplexer such as lspmux.** It shares one rust-analyzer between clients below mcpls. It leaves every mcpls process holding its own document state and delivery records, so the owner and passive machinery stays, and the sharing stops at language servers that the multiplexer has been tested against.

**The HTTP transport as the sharing mechanism.** It already shares one `McplsServer` across sessions, but it has to be started by hand, it is behind a non-default feature, and pointing a plugin at a TCP port means owning a port, its lifetime and its access control. A local socket the frontend can start on demand is the same sharing with none of that.

**Keeping the owner and passive roles, with language servers handed over.** A promoted process would have to rebuild every index the exiting owner had. The handover is the expensive thing, which is the argument for a process that no session owns.

## Verification

- Two sessions in one project: exactly one backend process and one rust-analyzer for the pair, and the second session answers a hover before a cold index could have finished.
- One of those two started in a subdirectory: the same backend, and its files fall inside a workspace root rather than outside every one.
- Two worktrees of one repository: a backend each, and neither answers about the other's files.
- A Codex frontend and a Codex hook on a Linux host that sets `XDG_RUNTIME_DIR`: both reach the same endpoint.
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
- A Codex connection whose first tool call names a thread the backend already holds records for: those records carry over rather than starting empty.
- On Windows, a session that fires no hook: the frontend says why there is no backend rather than appearing to work.
- The interactive Codex interface on Windows: it contains a server's descendants the way `codex exec` does, so the hook route is still the one that works.
- The same suite on Windows over the named pipe, since the endpoint code is shared.
