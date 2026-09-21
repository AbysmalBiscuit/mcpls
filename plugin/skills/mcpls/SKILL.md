---
name: mcpls
description: "Navigate and change code through language servers with the mcpls tools. Use when finding a symbol's definition, references, callers, implementations, or type; renaming a symbol; or checking errors after an edit. Also when the user asks about setting up mcpls."
argument-hint: "[symbol, file, or code question, e.g. \"who calls parse_config\"]"
allowed-tools: "mcp__plugin_mcpls_mcpls__workspace_symbol_search, mcp__plugin_mcpls_mcpls__get_definition, mcp__plugin_mcpls_mcpls__get_references, mcp__plugin_mcpls_mcpls__get_hover, mcp__plugin_mcpls_mcpls__go_to_implementation, mcp__plugin_mcpls_mcpls__go_to_type_definition, mcp__plugin_mcpls_mcpls__prepare_call_hierarchy, mcp__plugin_mcpls_mcpls__get_incoming_calls, mcp__plugin_mcpls_mcpls__get_outgoing_calls, mcp__plugin_mcpls_mcpls__get_document_symbols, mcp__plugin_mcpls_mcpls__get_signature_help, mcp__plugin_mcpls_mcpls__get_completions, mcp__plugin_mcpls_mcpls__get_inlay_hints, mcp__plugin_mcpls_mcpls__get_diagnostics, mcp__plugin_mcpls_mcpls__get_cached_diagnostics, mcp__plugin_mcpls_mcpls__get_new_diagnostics, mcp__plugin_mcpls_mcpls__get_code_actions, mcp__plugin_mcpls_mcpls__apply_code_action, mcp__plugin_mcpls_mcpls__rename_symbol, mcp__plugin_mcpls_mcpls__format_document, mcp__plugin_mcpls_mcpls__get_server_logs, mcp__plugin_mcpls_mcpls__get_server_messages"
license: MIT OR Apache-2.0
metadata:
  repository: "https://github.com/AbysmalBiscuit/mcpls"
  docs: "https://github.com/AbysmalBiscuit/mcpls/tree/main/docs/user-guide"
---

# Using mcpls

If the user asks for help installing, configuring, or troubleshooting mcpls itself, load the `setup-mcpls` skill instead.

The mcpls tools answer from the compiler's view of the code, so a lookup returns the symbol itself: no matches in comments, strings, or same-named symbols in other scopes. Reach for them first whenever the question is about a symbol rather than about text.

| Question | Tool |
|---|---|
| Where is `Foo` defined? | `workspace_symbol_search`, then `get_definition` from a use site |
| Who uses this? | `get_references` |
| Who calls this function, and what does it call? | `prepare_call_hierarchy`, then `get_incoming_calls` or `get_outgoing_calls` with the returned item |
| Which types implement this trait or interface? | `go_to_implementation` |
| What type is this, and what does its doc say? | `get_hover`; `go_to_type_definition` to jump to the type |
| Rename a symbol everywhere | `rename_symbol` |
| Did my edit break anything? | `get_new_diagnostics` |

Use text search and file reads for text: comments, docs, config, string literals, languages with no language server, and files outside the session's checkout.

## Calling the tools

- Positions are an absolute `file_path` plus 1-based `line` and `character`, and the character must land on the identifier. Every location an mcpls tool returns is already in that form, so feed it straight back in. A text search that reports line and column numbers gives the same 1-based position.
- Start from a name with `workspace_symbol_search`; its result is the position the other tools need.
- A file in another checkout or worktree fails with `path outside workspace`. Read it directly instead.
- The first call for a language starts its server. While it indexes, calls return empty results or a "still initializing" error. Retry shortly before concluding a symbol does not exist.
- A call that fails with ``stopped; run `mcpls lsp start <id>` `` means the user stopped that server. If the task needs it, run the command the error names in the shell.
- `get_document_symbols` returns the whole outline, tests included, which is thousands of lines for a large file. Prefer `workspace_symbol_search` there.
- `rename_symbol` and `format_document` return the edits unless called with `apply: true` and the checkout allows writes; otherwise apply the returned edits yourself.
- With the mcpls plugin installed, diagnostics from your edits arrive in context on their own. Call `get_new_diagnostics` to check before calling a change done; it returns nothing when nothing changed.
