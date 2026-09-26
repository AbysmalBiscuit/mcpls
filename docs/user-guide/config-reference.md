# Configuration reference

Every key mcpls reads from `mcpls.toml`, with its type, default, and meaning. Generated from the JSON Schema `mcpls schema` prints; regenerate it with `devrun task schema`, or `MCPLS_UPDATE_SCHEMA=1 cargo test -p mcpls-core --test config_schema`.

Which file mcpls loads, and when it trusts a checkout's own `mcpls.toml`, is in `mcpls help --full` under `--config` and `--trust-project-config`. `mcpls config --origin` prints the configuration resolved for a directory and the file each setting came from.

## `[apply]`

Which tools may write their edits to the source tree. Without this table mcpls is read-only: every tool returns its edit for you to read, and nothing on disk changes.

A key hands the write to the language server, which decides which files the edit touches and what goes in them. mcpls checks that every path stays inside the workspace roots, applies the whole edit or none of it, and reports what it wrote. It does not review the content, and cannot tell a correct rename from a wrong one.

There is no undo. mcpls rolls an edit back only while that apply is running: when a step fails, the completed steps are reversed and the error names any file it could not restore. Once an apply returns, the change is on disk and mcpls keeps no record of what was there. Commit first, and read the `files_written` list the tool returns.

An edit is refused whole, with nothing written, when a path resolves outside the workspace roots (following symlinks), when it would change or destroy a file marked read-only, when a file it edits has no readable text, or when two edits to one document overlap.

- `allow_file_deletion` (boolean, default `false`): Permits operations that destroy a file's content: an explicit delete, a create that overwrites an existing file, and a rename onto an existing destination. Gates all three for every tool above, because losing a file is not the kind of mistake a bad edit is. Without it, an edit containing any of them is refused whole.
- `code_actions` (boolean, default `false`): Enables `apply_code_action`, which applies one action from a `get_code_actions` listing. The widest of the three: an action can create, move, or delete files, and can carry a command the server runs itself, whose edits mcpls applies through the same checks. An action the server resolves lazily shows its edit only when applied.
- `format_document` (boolean, default `false`): Lets `format_document` write its edits when called with `apply: true`. The write stays inside the file named.
- `rename` (boolean, default `false`): Lets `rename_symbol` write its edit when called with `apply: true`. A rename usually rewrites every file that references the symbol, not only the one named.

## `[backend]`

The shared backend. The first session in a checkout starts one backend, and every later session in that checkout, from any subdirectory, attaches to it and shares its language servers. Two worktrees of one repository are two checkouts. `mcpls --no-backend` serves one session in-process instead.

A session whose configuration differs from the running backend's still attaches, and the backend's configuration stays in effect; its server instructions name both fingerprints. A session that trusts the checkout's `mcpls.toml` when the backend does not, or the reverse, is refused.

- `idle_shutdown_ms` (integer, default `10000`): How long a backend with no session attached waits before it exits. Hook connections do not keep it alive. A backend kept by `mcpls backend start` ignores this until `mcpls backend stop` or `mcpls backend auto`.

  Short by default so memory returns to the machine soon after the last session closes. The cost of short is a cold reindex for a session that opens just after the timer; raise it when sessions alternate quickly.
- `spawn` ("lazy" | "eager", default `"lazy"`): When this backend's language servers start. A server's own `spawn` overrides it.

  Lazy holds a server back until the session touches its language, through a file the agent's hooks report or a tool call, which keeps a checkout's unused languages out of memory. Eager starts every applicable server with the backend.

## `[brief]`

The note `mcpls brief` hands an agent session when it starts, naming the installed language servers that serve the checkout and pointing the agent at the mcpls tools. Run `mcpls brief` to see what a session in the current directory gets.

- `enabled` (boolean, default `true`): Whether a session starts with a note naming the language servers that serve its checkout and pointing it at the mcpls tools.

  Defaults on, because reaching this configuration means installing the plugin and installing the plugin is the opt-in. With it off, an agent learns mcpls exists only from its tool list, or from instructions the user writes. A checkout no installed server applies to gets no note either way.

## `[diagnostics]`

How much of what the language servers report reaches the agent.

- `footer` (boolean, default `false`): Whether the tools that write append the diagnostics their own edit caused to their result.
- `footer_grace_ms` (integer, default `250`): How long a footer waits before it starts looking for quiet.

  A footer that checks before the language servers react to the write sees a quiet workspace and reports the state from before the edit; rust-analyzer's flycheck begins about 90 ms after a save.
- `footer_quiet_ms` (integer, default `200`): How long nothing may be outstanding before a footer calls it done.

  Shorter than `settle_quiet_ms`, which bridges gaps between startup phases a footer never sees. What a footer bridges is the cancel-and-restart between two writes landing back to back.
- `footer_wait_ms` (integer, default `15000`): How long a footer waits in total, `footer_grace_ms` included, before reporting what it has. A wait that never goes quiet is sampled every 50 ms, so it can run up to 50 ms past this.

  Size it to outlast a real build of the workspace, or the footer reports the state from before the edit. The wait ends when the servers go quiet, so a high cap costs a fast workspace nothing.
- `max_per_file` (integer, default `10`): Most diagnostics delivered for one file in one flush. `0` means unlimited.
- `max_total` (integer, default `50`): Most diagnostics delivered in one flush across every file. This is a context budget, which is why it is not per server. `0` means unlimited.
- `record_grace_ms` (integer, default `60000`): How long a delivery record survives without hook activity once no hook connection protects it. `0` expires it at once.
- `settle_deadline_ms` (integer, default `300000`): How long to wait for the quiet `settle_quiet_ms` asks for before giving up and baselining anyway. Bounds the damage from a server that never finishes, or from a progress notification dropped before its pump existed.

  Counted from when the language servers finish starting, so the handshake spends none of it. It must outlast a full index of the workspace: firing before that captures a partial baseline, and every file analyzed afterwards then reads as newly changed.
- `settle_quiet_ms` (integer, default `1000`): How long the language servers must report no work before their view of the workspace counts as complete.

  Raise it on a workspace whose servers pause mid-analysis for longer than this; the cost of raising it is a later baseline, and the cost of setting it too low is a baseline taken mid-index.
- `severity` ("off" | "error" | "warning" | "information" | "hint", default `"warning"`): The least severe diagnostic worth delivering, for any applicable server that does not set its own `diagnostics_severity`. A diagnostic with no severity clears every floor but `"off"`, since LSP makes severity optional.

### `[diagnostics.hooks]`

How the agent's hooks reach the backend.

- `enabled` (boolean, default `true`): Whether the listener binds at all.

  Defaults on, because reaching this configuration means installing the plugin and installing the plugin is the opt-in. With this off, no listener binds, so nothing injects diagnostics between turns and an edit alone no longer starts a language server for a language that has none running; a tool call still does. The backend's watcher does not depend on this: changes on disk, the agent's own included, still reach the servers that are running.
- `op_deadline_ms` (integer, default `1500`): How long an op may take before it answers anyway.

  The host's default hook timeout is 600 seconds, so a hook that hangs blocks the agent. This bound is the hook's protection, not the host's; work already started can continue, but the deadline does not guarantee eventual success or delivery.
- `sweep_quiet_ms` (integer, default `500`): How long the pending set must be quiet before the sweep runs.

  Every `didSave` restarts rust-analyzer's flycheck and cancels the check in flight, so a `cargo fmt` forwarded one path at a time produces a run of cancelled checks and no diagnostics at all.

## `[[lsp_servers]]`

The language servers mcpls runs. mcpls has built-in servers for Rust (rust-analyzer), Python (pyright), TypeScript, Go (gopls), C and C++ (clangd), and Zig (zls), and uses them when this list is omitted.

Entries identify a server by `name`, or `language_id` when `name` is absent. The first enabled entry naming a built-in overlays it: a field the entry omits is inherited, and a field it sets overrides. Setting `command` also clears the built-in's `args`, `env`, `initialization_options`, and `install`, since those belong to the binary being replaced; `file_patterns` describe the language and survive. An entry naming no built-in defines a new server, and needs `command`.

Entries fold top to bottom. Once an identity is claimed, a later entry with `command` adds another server, while a later `spawn`-only entry overlays the resolved server. Two servers with one identity must never both apply to one workspace, so give them mutually exclusive `heuristics.project_markers`. To run two servers for one language at once, give each its own `name` and split the tools with `handles`.

`enabled = false` removes whatever has the entry's identity at that point, including an entry of your own written earlier, so put it before any entry it should leave alone.

- `args` (array of string): Arguments to pass to the command. An overlay inherits these from its built-in unless `command` is replaced, in which case an omitted value becomes `[]`. A new server also defaults to `[]`.
- `command` (string): The executable that starts the server, found on `PATH` or given as an absolute path. An overlay inherits the built-in command when omitted; a new server must set it. Setting it on an overlay clears the built-in's `args`, `env`, `initialization_options`, and `install`.
- `diagnostics_severity` ("off" | "error" | "warning" | "information" | "hint"): The least severe diagnostic worth delivering from this server; `"off"` mutes it without disabling it. An overlay inherits the built-in value; a new server without one uses `[diagnostics].severity`, which defaults to `"warning"`.
- `enabled` (boolean): Set to `false` to drop the server this entry names. Omission leaves an existing server enabled and enables a new server by default. Entries fold top to bottom, so it removes whatever has this identity at that point, including an earlier entry of your own.
- `env` (table of string): Variables added to the server's environment. An overlay inherits these from its built-in unless `command` is replaced, in which case an omitted value becomes `{}`. A new server also defaults to `{}`.

  The server does not inherit mcpls's environment. It gets a short allowlist, `PATH`, `HOME`, `USERPROFILE`, the temp directory variables, and the Windows variables its loader needs, and these entries go on top. Setting `PATH` replaces the passed-through value, and on Unix a bare `command` must then be found on your `PATH`, so prefer an absolute `command` to adding a directory.
- `file_patterns` (array of string): Globs for the files this server handles, such as `**/*.rs`. An overlay inherits the built-in patterns; a new server defaults to `[]`, which is valid when `workspace.language_extensions` maps its language or it handles only `workspace_symbols`.
- `handles` (array of "hover" | "definition" | "type_definition" | "implementation" | "references" | "diagnostics" | "rename" | "completions" | "signature_help" | "document_symbols" | "workspace_symbols" | "format_document" | "code_actions" | "call_hierarchy" | "inlay_hints"): Tools this server handles. An overlay inherits the built-in value; a new server without a list is its language's catch-all, handling every tool no other server claims.

  A language may have one catch-all, and a tool may be claimed by one server per language; a config that breaks either for two servers that both apply fails at startup, naming the entries. `rename` routes `rename_symbol`, `workspace_symbols` routes `workspace_symbol_search`, `call_hierarchy` routes all three call hierarchy tools, and `diagnostics` routes both diagnostics tools. A tool whose server has no binary moves to the catch-all. `workspace_symbols` has no document to route by, so it goes to the first server anywhere that claims it, else the first catch-all.
- `initialization_options` (any): Server-specific options sent in the LSP `initialize` request, such as `cargo.features = "all"` for rust-analyzer. Replaces the built-in's value rather than merging into it. An overlay inherits the built-in value unless `command` is replaced; a new server defaults to no initialization options.
- `install` (string or table): The shell command `mcpls lsp install` runs when `command` is not installed: one string for every OS, or a table with `unix` and `windows` keys. An overlay inherits the built-in value unless `command` is replaced; a new server defaults to none, and the built-ins carry none.

  It runs in the checkout root with mcpls's own environment; the server's `env` does not apply. Windows runs it with Windows `PowerShell` 5.1, which has no `&&`, and whose `;` runs the next command after a failure, so put the installer last or follow a step with `if (-not $?) { exit 1 }`. It runs arbitrary code like `command`, so one in a checkout's `mcpls.toml` counts only with `--trust-project-config`.
  - `unix` (string): Run with `sh -c` on Linux and macOS.
  - `windows` (string): Run with Windows `PowerShell` 5.1, which has no `&&`; its `;` runs the next command even after a failure.
- `language_id` (string): The language this server serves, such as `rust` or `python`, and the identity used to find a built-in when `name` is absent. Required for a new entry unless `name` identifies it; for a new named server, an omitted value uses `name` as its language identifier.
- `name` (string): Routing identity. An overlay inherits the built-in value; when absent on a new server, `language_id` is used.

  Two servers for one `language_id` that apply at once each need their own identity. A `name` gives an entry its own, so the built-in for that language keeps running beside it unless an entry disables it.
- `request_timeout_seconds` (integer): Timeout for each LSP request after initialization, in seconds, independent of `timeout_seconds`. Rejects `0`. An overlay inherits the built-in value; a new server defaults to `30` seconds.

  A request answered with content-modified (`-32802`) is retried, up to four attempts with 3.5 seconds of backoff in all, so one tool call can take `4 * request_timeout_seconds + 3.5s`, plus `timeout_seconds` when the server has to restart first. Completions are capped at 10 seconds whatever this says.
- `spawn` ("lazy" | "eager"): When this server starts, overriding `[backend] spawn`. When omitted, it follows `[backend] spawn`, which defaults to `"lazy"`. Eager suits a server whose index is slow enough that the first request should not wait on it.
- `timeout_seconds` (integer): Timeout for the `initialize` handshake alone, in seconds. Raise it for a server that loads a large project before answering. Rejects `0`. An overlay inherits the built-in value; a new server defaults to `30` seconds.

### `[lsp_servers.heuristics]`

Which workspaces this server applies to. An overlay inherits the built-in markers; a new server without markers always applies.

- `project_markers` (array of string): Files or directories that make the server applicable, such as `pyproject.toml`. Any match counts, searched through `workspace.heuristics_max_depth` levels, skipping directories such as `node_modules`, `target`, and `.git`. An omitted list inherits for built-ins; no markers means always applicable.

## `[workspace]`

Where the workspace is, and which files mcpls opens.

- `heuristics_max_depth` (integer, default `10`): How many directory levels deep mcpls searches for a server's `heuristics.project_markers`.
- `max_documents` (integer, default `100`): Most documents mcpls keeps open. Documents are never evicted, so once the limit is hit, a tool call that opens a new file fails until the backend restarts; files already open keep working. `0` disables the limit.

  A diagnostics sweep opens files too, and keeps one slot free for an ordinary request. Each open document's content stays in memory.
- `max_file_size` (integer, default `10485760`): Largest file, in bytes, mcpls opens. `0` disables the limit.
- `position_encodings` (array of string, default `["utf-8","utf-16"]`): Position encodings offered to each language server during `initialize`, most preferred first: `"utf-8"`, `"utf-16"`, or `"utf-32"`. Must not be empty. A server may still pick UTF-16, which the LSP spec makes mandatory.
- `roots` (array of string, default `[]`): Workspace root directories. Empty means the checkout root. Each root must exist.

  A relative root in a file named by `--config`, or in a checkout's `mcpls.toml`, resolves against the directory holding that file, so `roots = [".."]` in `<repo>/.agents/mcpls.toml` means the repository root. In the user config file it resolves against the checkout root. On Windows, `\dir` and `C:dir` are joined like any other relative root.

### `[[workspace.language_extensions]]`

Maps file extensions to the language ID mcpls reports to a server. Setting this key replaces the whole built-in list, so list every language you need, not only the new one.

A server for a language outside this list needs its files mapped, here or through its own `file_patterns`, unless it handles only `workspace_symbols`.

Default:

```json
[
  {"extensions":["rs"],"language_id":"rust"},
  {"extensions":["py","pyw","pyi"],"language_id":"python"},
  {"extensions":["js","mjs","cjs"],"language_id":"javascript"},
  {"extensions":["ts","mts","cts"],"language_id":"typescript"},
  {"extensions":["tsx"],"language_id":"typescriptreact"},
  {"extensions":["jsx"],"language_id":"javascriptreact"},
  {"extensions":["go"],"language_id":"go"},
  {"extensions":["c","h"],"language_id":"c"},
  {"extensions":["cpp","cc","cxx","hpp","hh","hxx"],"language_id":"cpp"},
  {"extensions":["java"],"language_id":"java"},
  {"extensions":["rb"],"language_id":"ruby"},
  {"extensions":["php"],"language_id":"php"},
  {"extensions":["swift"],"language_id":"swift"},
  {"extensions":["kt","kts"],"language_id":"kotlin"},
  {"extensions":["scala","sc"],"language_id":"scala"},
  {"extensions":["zig"],"language_id":"zig"},
  {"extensions":["lua"],"language_id":"lua"},
  {"extensions":["sh","bash","zsh"],"language_id":"shellscript"},
  {"extensions":["json"],"language_id":"json"},
  {"extensions":["toml"],"language_id":"toml"},
  {"extensions":["yaml","yml"],"language_id":"yaml"},
  {"extensions":["xml"],"language_id":"xml"},
  {"extensions":["html","htm"],"language_id":"html"},
  {"extensions":["css"],"language_id":"css"},
  {"extensions":["scss"],"language_id":"scss"},
  {"extensions":["less"],"language_id":"less"},
  {"extensions":["md","markdown"],"language_id":"markdown"},
  {"extensions":["cs"],"language_id":"csharp"},
  {"extensions":["fs","fsi","fsx"],"language_id":"fsharp"},
  {"extensions":["r","R"],"language_id":"r"}
]
```

- `extensions` (array of string): File extensions, without the dot.
- `language_id` (string): The language ID reported to the server for these extensions.
