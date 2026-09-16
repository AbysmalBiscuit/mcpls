---
name: mcpls
description: >-
  Install, configure, and run mcpls, the MCP-to-LSP bridge that gives an agent compiler-grade
  code intelligence. Use when installing or registering mcpls, choosing its CLI flags or
  MCPLS_* environment variables, writing or debugging an mcpls.toml, or diagnosing why mcpls
  or one of its language servers fails to start.
license: MIT OR Apache-2.0
compatibility: >-
  Wraps the `mcpls` Rust binary. Requires at least one LSP server on PATH (rust-analyzer,
  pyright, gopls, clangd, and so on). HTTP transport requires building with the non-default
  `transport-http` feature.
metadata:
  repository: "https://github.com/AbysmalBiscuit/mcpls"
  docs: "https://github.com/AbysmalBiscuit/mcpls/tree/main/docs/user-guide"
---

# mcpls

mcpls is one Rust binary that speaks LSP to real language servers such as rust-analyzer, pyright, gopls, and clangd, and exposes them to an agent as MCP tools. It does no language analysis itself: a language whose server is missing from `PATH` or from the config has no code intelligence, while the other languages keep working.

The binary is the source of truth for its own surface. `mcpls --help` lists every flag, environment variable, and subcommand. `mcpls schema` prints the JSON Schema for `mcpls.toml`. `mcpls hook doctor` reports the running backend, its sessions, language servers, and loaded configuration.

Read the reference for the task in front of you:

| Task | Read |
|---|---|
| Installing or updating the binary, or building with `transport-http` | [references/install.md](references/install.md) |
| Registering mcpls with an MCP client, picking flags or `MCPLS_*` variables, serving over HTTP | [references/cli.md](references/cli.md) |
| Finding which `mcpls.toml` loads, or why a checkout's `mcpls.toml` is ignored | [references/config-loading.md](references/config-loading.md) |
| Writing `mcpls.toml`: the starter config, server, workspace, and backend fields | [references/configuration.md](references/configuration.md) |
| Running several servers for one language, or sending a tool to a specific server | [references/routing.md](references/routing.md) |
| Letting rename, formatting, or code actions write to disk | [references/apply.md](references/apply.md) |
| Tuning diagnostic severity, volume, write-tool footers, or hook timing | [references/diagnostics.md](references/diagnostics.md) |
| A language server missing an environment variable it needs | [references/env.md](references/env.md) |
| mcpls or a language server failing to start, time out, or answer | [references/troubleshooting.md](references/troubleshooting.md) |

The MCP tools themselves and their parameters are documented in the [Tools Reference](https://github.com/AbysmalBiscuit/mcpls/blob/main/docs/user-guide/tools-reference.md).
