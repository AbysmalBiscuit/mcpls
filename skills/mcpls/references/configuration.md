# mcpls.toml — configuration reference

Compact field tables for `mcpls.toml`. This is a schema reference, not a tutorial —
for worked examples per language, see
[Configuration Reference](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/configuration.md)
and [Complete Examples](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/configuration.md#complete-examples).

## `[workspace]` fields

| Field | Type | Default | Notes |
|---|---|---|---|
| `roots` | array of strings | `[]` | Workspace root directories. Empty array auto-detects from the current directory. |
| `position_encodings` | array of strings | `["utf-8", "utf-16"]` | Preferred LSP position encodings (`utf-8`, `utf-16`, `utf-32`), offered to each spawned server during the `initialize` handshake in the listed order. A preference, not a restriction — per the LSP spec, UTF-16 is a mandatory fallback a server may still choose even if omitted here. |
| `language_extensions` | array of `{extensions, language_id}` | `[]` within an explicit `[workspace]` table; 30 built-in mappings only when `[workspace]` is absent entirely | Custom or overriding file-extension → language-ID mappings. Adding a `[workspace]` table for any other field (e.g. just `roots`) silently drops the 30 built-ins unless you list `language_extensions` yourself — list every language you need, not just the new one. |
| `heuristics_max_depth` | integer | `10` | Recursion depth for `heuristics.project_markers` search (see below). |

## `[[lsp_servers]]` fields

mcpls ships six built-in servers — rust-analyzer, pyright, the TypeScript language server, gopls, clangd, and zls, for `rust`, `python`, `typescript`, `go`, `cpp`, and `zig`. Each entry **merges onto** the built-in sharing its routing identity (`name` if set, else `language_id`): a field the entry omits is inherited, a field it sets overrides. Overriding `command` also clears the built-in's `args`, `env`, and `initialization_options`, since those belong to the binary being replaced. An entry whose identity matches no built-in defines a new server with nothing to inherit, so it must supply `command` itself. A language may have multiple entries (see `handles` below); the first claiming an identity merges, every later one appends another server.

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `language_id` | string | only if unmatched and no `name` | — | e.g. `rust`, `python`, `typescript`. Also the default routing identity used to find the built-in to merge onto. |
| `command` | string | only if matching no built-in | inherited from the built-in | Executable name (resolved via `PATH`) or absolute path. |
| `args` | array of strings | no | `[]`, or inherited | e.g. `["--stdio"]` for servers that need it. |
| `file_patterns` | array of glob strings | no | `[]`, or inherited | e.g. `["**/*.rs"]`. Determines which files route to this server. |
| `name` | string | no | the `language_id` | Explicit routing identity. Required when two servers share one `language_id`, so each has a distinct identity. |
| `handles` | array of routing values | no | unset = catch-all | Restricts a server to specific tools; see [Tool routing](#tool-routing-handles) below. |
| `timeout_seconds` | integer | no | `30`, or inherited | Timeout for the `initialize` handshake only. Rejects `0`. |
| `request_timeout_seconds` | integer | no | `30`, or inherited | Timeout per individual LSP request after initialization (hover, definition, etc.), independent of `timeout_seconds`. Rejects `0`. Worst case per tool call: `4 * request_timeout_seconds + 3.5s` (retry budget on `-32802` responses). The completions timeout is `min(request_timeout_seconds, 10s)` — a value below 10 lowers the completions cap too; only values above 10 get clamped down to it. |
| `initialization_options` | table | no | `{}`, or inherited | Server-specific options passed in the LSP `initialize` request, e.g. `cargo.features = "all"` for rust-analyzer. |
| `env` | table | no | `{}`, or inherited | See [Environment passthrough](#environment-passthrough-env) below. |
| `heuristics.project_markers` | array of strings | no | unset, or inherited | Marker files/directories that make this server applicable. mcpls searches for them recursively through the workspace tree up to `heuristics_max_depth` levels, skipping `node_modules`, `target`, and `.git`, e.g. `["pyproject.toml"]`. |
| `enabled` | boolean | no | unset = stays enabled | Set to `false` to remove every server sharing this entry's identity, most commonly a built-in you don't want spawned. Order-sensitive: entries fold top to bottom, so it removes whatever already has that identity at that point, including an entry of your own written earlier — put it before any entry it isn't meant to remove. |
| `diagnostics_severity` | `"off"` \| `"error"` \| `"warning"` \| `"information"` \| `"hint"` | no | unset = falls back to `diagnostics.severity` | The least severe diagnostic worth delivering from this server. `"off"` mutes diagnostics for this server without disabling it. |

## Tool routing (`handles`)

`handles` values are routing identifiers, not MCP tool names — most match directly,
a few map many-to-one:

| `handles` value | MCP tool(s) it governs |
|---|---|
| `hover` | `get_hover` |
| `definition` | `get_definition` |
| `type_definition` | `go_to_type_definition` |
| `implementation` | `go_to_implementation` |
| `references` | `get_references` |
| `diagnostics` | `get_diagnostics` (pull) **and** `get_cached_diagnostics` (same route serves both) |
| `rename` | `rename_symbol` |
| `completions` | `get_completions` |
| `signature_help` | `get_signature_help` |
| `document_symbols` | `get_document_symbols` |
| `workspace_symbols` | `workspace_symbol_search` (see the special case below) |
| `format_document` | `format_document` |
| `code_actions` | `get_code_actions` **and** `apply_code_action` (same route serves both) |
| `call_hierarchy` | `prepare_call_hierarchy`, `get_incoming_calls`, `get_outgoing_calls`, sharing one route: an incoming/outgoing-calls lookup only makes sense against the server that produced the originating call-hierarchy item |
| `inlay_hints` | `get_inlay_hints` |

Rules:

- Each language may have one, and only one, server without `handles` set; that
  unrestricted server catches every tool the other servers for the language don't
  explicitly claim.
- A tool may be claimed by only one server per language.
- If the server routed to a tool fails to spawn, that tool falls back to the
  language's catch-all (if running); otherwise the call fails naming "no server
  available" rather than silently reaching a server that explicitly declined it via
  `handles`.
- `workspace_symbol_search` is the one tool with no document to route on. It
  resolves, across *all* configured servers, to the first explicit
  `workspace_symbols` claimant, else the first catch-all — there is no per-language
  fallback since the tool has no language. With neither, the call fails by name.

## Ambiguous configs fail at startup, not silently

A startup check looks for any pair of servers configured for the same language that
would both be active in the same workspace at once (per `heuristics.project_markers`).
If that pair also collides on routing — same `name`, both lacking `handles`, or both
claiming an identical tool — mcpls refuses to start rather than pick one arbitrarily,
and the error names the conflicting `[[lsp_servers]]` entries. Two servers whose
`heuristics.project_markers` are mutually exclusive never overlap in one workspace, so
that combination is not flagged and starts fine.

## `[apply]` fields

The only table that lets mcpls write to the source tree. Omit it and mcpls is read-only: every tool returns an edit to read, and nothing on disk changes. Every field defaults to `false`.

| Field | Type | Default | Notes |
|---|---|---|---|
| `rename` | boolean | `false` | `rename_symbol` may write when called with `apply: true`. A workspace-wide rename rewrites every file referencing the symbol, not just the one named. |
| `format_document` | boolean | `false` | `format_document` may write when called with `apply: true`. Confined to the file named. |
| `code_actions` | boolean | `false` | Enables the `apply_code_action` tool. The widest of the three: an action can create, move, or delete files as well as edit them, and can carry a command the server runs itself, which may send further edits back while it runs. |
| `allow_file_deletion` | boolean | `false` | Permits any operation that destroys a file's content — an explicit delete, a create that overwrites an existing file, or a rename onto an existing destination. Gates all three for every tool above rather than any one of them. With it `false`, an edit containing any of them is refused whole and nothing is written. |

Turning a key on hands the write to the language server: it decides which files the edit touches and what goes in them. mcpls confines every path to `workspace.roots` (resolving symlinks first), applies the whole edit or none of it, and reports what it wrote. It does not review the content.

There is no undo beyond the run itself. A step failing partway through one apply reverses the completed steps and the error names any file it could not restore; once an apply returns successfully the change is on disk and mcpls holds no record of what was there before.

An edit is refused outright when there are no `workspace.roots`, when a path resolves outside them, when it would change or destroy a file the filesystem marks read-only (all such files are named in one refusal, rather than one per attempt), when a file it edits holds no readable text, when two entries address one document, when it creates a file in a directory that does not exist, or when it edits a file under a directory the same edit moves.

## `[diagnostics]` fields

Sets the default severity floor and the volume caps applied when diagnostics are delivered. A per-server `diagnostics_severity` (see `[[lsp_servers]]` fields above) overrides `severity` for that one server.

| Field | Type | Default | Notes |
|---|---|---|---|
| `severity` | `"off"` \| `"error"` \| `"warning"` \| `"information"` \| `"hint"` | `"warning"` | The least severe diagnostic worth delivering, for any server without its own `diagnostics_severity`. A diagnostic with no severity at all clears every floor but `"off"`. |
| `max_per_file` | integer | `10` | Most diagnostics delivered for one file in one flush. `0` disables the limit. |
| `max_total` | integer | `50` | Most diagnostics delivered in one flush across every file — a context budget, so it is not per server. `0` disables the limit. |
| `settle_quiet_ms` | integer (ms) | `1000` | How long the language servers must report no work before their view of the workspace counts as complete. |
| `settle_deadline_ms` | integer (ms) | `60000` | How long to wait for that quiet before baselining anyway, bounding a server that never finishes. |

## Environment passthrough (`env`)

Spawned LSP server processes do **not** inherit mcpls's full environment — this is a
deliberate security boundary, not an oversight. Each child's environment is cleared,
then a minimal allowlist is passed through from mcpls's own process (`PATH`, `HOME`,
`USERPROFILE`, `TMPDIR`/`TEMP`/`TMP` on every platform, plus Windows loader
essentials), and only then is the server's `[lsp_servers.env]` table applied on top,
so entries there can override the passthrough.

Use `env` to restore anything a server needs beyond that allowlist: proxy settings,
`VIRTUAL_ENV`/`PYTHONPATH`, or toolchain variables a `build.rs` reads (`DATABASE_URL`,
`LIBCLANG_PATH`, `SSH_AUTH_SOCK`, …). Values here are written literally into
`mcpls.toml`, a file that's often committed to VCS — don't put real secrets in it;
and forwarding `SSH_AUTH_SOCK` hands the ssh-agent socket to the spawned LSP
process, so only do so for servers you trust.

**`PATH` caution:** an `env.PATH` entry overwrites the passthrough value rather than
extending it, and the two platforms then behave differently. On Unix, a bare
`command` with no directory component is now resolved against your override, so it
stops working unless you kept the original directory in it; on Windows the loader
still consults the parent process's `PATH` as a fallback after your override, so the
same mistake is less likely to break anything. If you just need to add one directory,
give `command` an absolute path instead of touching `PATH` at all.
