# Routing tools between servers

A language can run several servers. Give each extra server its own `name` so the servers have distinct identities, and set `handles` to decide which tools each one answers.

## `handles` values

`handles` values are routing identifiers, not MCP tool names. Most match one tool; a few govern several:

| `handles` value | MCP tools it governs |
|---|---|
| `hover` | `get_hover` |
| `definition` | `get_definition` |
| `type_definition` | `go_to_type_definition` |
| `implementation` | `go_to_implementation` |
| `references` | `get_references` |
| `diagnostics` | `get_diagnostics` and `get_cached_diagnostics` |
| `rename` | `rename_symbol` |
| `completions` | `get_completions` |
| `signature_help` | `get_signature_help` |
| `document_symbols` | `get_document_symbols` |
| `workspace_symbols` | `workspace_symbol_search` (see the rules below) |
| `format_document` | `format_document` |
| `code_actions` | `get_code_actions` and `apply_code_action` |
| `call_hierarchy` | `prepare_call_hierarchy`, `get_incoming_calls`, and `get_outgoing_calls`, because incoming and outgoing calls only make sense against the server that produced the call-hierarchy item |
| `inlay_hints` | `get_inlay_hints` |

## Rules

- Each language may have at most one server without `handles`. That catch-all server takes every tool the language's other servers leave unclaimed.
- A tool may be claimed by only one server per language.
- If the server routed to a tool fails to spawn, the tool falls back to the language's catch-all when one is running. Otherwise the call fails with an error naming the tool; it never reaches a server whose `handles` left that tool out.
- `workspace_symbol_search` has no document to route on. Across *all* configured servers it goes to the first explicit `workspace_symbols` claimant, else the first catch-all. With neither, the call fails naming the tool.

## Ambiguous configs fail at startup

A startup check looks for any pair of servers for the same language that would both be active in one workspace, per `heuristics.project_markers`. If that pair also collides on routing (same `name`, both lacking `handles`, or both claiming one tool), mcpls refuses to start rather than pick one, and the error names the conflicting `[[lsp_servers]]` entries. Two servers whose `heuristics.project_markers` are mutually exclusive never overlap, so that pair starts fine.
