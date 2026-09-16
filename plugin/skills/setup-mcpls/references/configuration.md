# Writing mcpls.toml

`mcpls schema` prints the full JSON Schema, with every field's description and default. This file covers the starter config and the server, workspace, and backend fields. Routing, `[apply]`, `[diagnostics]`, and `env` each have their own file, linked from the fields that use them. Which file mcpls loads is in [config-loading.md](config-loading.md).

## Starter config

```toml
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]

[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]
```

Each entry merges onto the built-in server sharing its `language_id`. Leave `name` unset unless you run a second server for the same language: a `name` gives the entry its own identity, and the built-in then spawns alongside it.

The starter has no `[workspace]` table on purpose. `roots` already defaults to the current checkout, and writing `[workspace]` for any field drops the built-in `language_extensions` mappings unless you list them all back.

## `[[lsp_servers]]` fields

mcpls has built-in servers for Rust (rust-analyzer), Python (pyright), TypeScript, Go (gopls), C/C++ (clangd), and Zig (zls). Each entry **merges onto** the built-in sharing its routing identity (`name` if set, else `language_id`): a field the entry omits is inherited, a field it sets overrides. Overriding `command` also clears the built-in's `args`, `env`, and `initialization_options`, since those belong to the binary being replaced. An entry whose identity matches no built-in defines a new server with nothing to inherit, so it must supply `command` itself. A language may have several entries; the first claiming an identity merges, and every later one adds another server.

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `language_id` | string | only if unmatched and no `name` | none | e.g. `rust`, `python`, `typescript`. Also the default routing identity used to find the built-in to merge onto. |
| `command` | string | only if matching no built-in | inherited from the built-in | Executable name (resolved via `PATH`) or absolute path. |
| `args` | array of strings | no | `[]`, or inherited | e.g. `["--stdio"]` for servers that need it. |
| `file_patterns` | array of glob strings | no | `[]`, or inherited | e.g. `["**/*.rs"]`. Determines which files route to this server. |
| `name` | string | no | the `language_id` | Explicit routing identity. Required when two servers share one `language_id`. See [routing.md](routing.md). |
| `handles` | array of routing values | no | unset = catch-all | Restricts a server to specific tools. See [routing.md](routing.md). |
| `spawn` | `"lazy"` \| `"eager"` | no | follows `[backend] spawn` | When this server starts. See [`[backend]`](#backend-fields). |
| `timeout_seconds` | integer | no | `30`, or inherited | Timeout for the `initialize` handshake only. Rejects `0`. |
| `request_timeout_seconds` | integer | no | `30`, or inherited | Timeout per LSP request after initialization, independent of `timeout_seconds`. Rejects `0`. Worst case per tool call: `4 * request_timeout_seconds + 3.5s`, the retry budget on `-32802` responses. Completions time out at `min(request_timeout_seconds, 10s)`. |
| `initialization_options` | table | no | `{}`, or inherited | Server-specific options passed in the LSP `initialize` request, e.g. `cargo.features = "all"` for rust-analyzer. |
| `env` | table | no | `{}`, or inherited | Variables added to the server's cleared environment. See [env.md](env.md). |
| `heuristics.project_markers` | array of strings | no | unset, or inherited | Marker files or directories that make this server applicable, e.g. `["pyproject.toml"]`. mcpls searches the workspace tree up to `heuristics_max_depth` levels, skipping `node_modules`, `target`, and `.git`. |
| `enabled` | boolean | no | unset = stays enabled | `false` removes every server sharing this entry's identity, most often a built-in you don't want. Entries fold top to bottom, so it removes whatever has that identity at that point, including your own earlier entry. Put it before any entry it should leave alone. |
| `diagnostics_severity` | `"off"` \| `"error"` \| `"warning"` \| `"information"` \| `"hint"` | no | falls back to `[diagnostics] severity` | The least severe diagnostic worth delivering from this server. `"off"` mutes it without disabling the server. |

## `[workspace]` fields

| Field | Type | Default | Notes |
|---|---|---|---|
| `roots` | array of strings | `[]` | Workspace root directories. Empty auto-detects from the current directory. |
| `language_extensions` | array of `{extensions, language_id}` | the built-in mappings, but `[]` inside an explicit `[workspace]` table | File extension to language ID mappings. Writing `[workspace]` for any other field drops the built-ins, so list every language you need, not just the new one. |
| `max_documents` | integer | `100` | Most documents mcpls keeps open. Documents are never evicted, so once the limit is hit, opening any new path fails until the backend restarts. `0` disables the limit. |
| `max_file_size` | integer (bytes) | `10485760` | Largest file mcpls opens. `0` disables the limit. |
| `position_encodings` | array of strings | `["utf-8", "utf-16"]` | Preferred LSP position encodings, offered to each server in order. A server may still pick UTF-16, which the LSP spec makes mandatory. |
| `heuristics_max_depth` | integer | `10` | Directory depth searched for `heuristics.project_markers`. |

## `[backend]` fields

One backend serves every session in a checkout.

| Field | Type | Default | Notes |
|---|---|---|---|
| `spawn` | `"lazy"` \| `"eager"` | `"lazy"` | When language servers start. `lazy` starts a server the first time a session touches its language, keeping unused languages out of memory. `eager` starts every applicable server with the backend. A server's own `spawn` overrides this. |
| `idle_shutdown_ms` | integer (ms) | `10000` | How long the backend waits after its last session closes before exiting. Hook connections do not keep it alive. Raise it when sessions come and go quickly, since each restart reindexes cold. |
