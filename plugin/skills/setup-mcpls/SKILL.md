---
name: setup-mcpls
description: "Set up mcpls: install or register it, choose its CLI flags or MCPLS_* variables, write or debug an mcpls.toml, or diagnose why mcpls or a language server fails to start."
argument-hint: "[what to set up, or what is failing, e.g. \"pyright not starting\"]"
allowed-tools: "mcp__plugin_mcpls_mcpls__get_server_logs, mcp__plugin_mcpls_mcpls__get_server_messages, mcp__plugin_mcpls_mcpls__get_diagnostics"
license: MIT OR Apache-2.0
compatibility: "Wraps the `mcpls` Rust binary. Requires at least one LSP server on PATH (rust-analyzer, pyright, gopls, clangd, and so on). HTTP transport requires building with the non-default `transport-http` feature."
metadata:
  repository: "https://github.com/AbysmalBiscuit/mcpls"
  docs: "https://github.com/AbysmalBiscuit/mcpls/tree/main/docs/user-guide"
---

# Setting up mcpls

mcpls is one Rust binary that speaks LSP to real language servers such as rust-analyzer, pyright, gopls, and clangd, and exposes them to an agent as MCP tools. It does no language analysis itself: a language whose server is missing from `PATH` or from the config has no code intelligence, while the other languages keep working.

The binary documents itself. Ask it before reading anything else:

| Question | Command |
|---|---|
| Flags, `MCPLS_*` variables, subcommands, exit codes | `mcpls help --full` |
| Every `mcpls.toml` key: its type, default, and rules | `mcpls schema` |
| The configuration resolved for a directory, and which file set each value | `mcpls config --origin` |
| The running backend, its sessions, and which language servers apply and are installed | `mcpls doctor` |
| A starter `mcpls.toml` with every setting commented out | `mcpls schema init` |

## Registering with an MCP client

stdio works with every build:

```json
{
  "mcpServers": {
    "mcpls": {
      "command": "mcpls",
      "args": []
    }
  }
}
```

To load a trusted checkout's `mcpls.toml`, put `"--trust-project-config"` in that entry's `args` rather than exporting `MCPLS_TRUST_PROJECT_CONFIG`, which trusts every checkout the shell launches mcpls in. MCP servers start without a login shell, so when the client cannot find `mcpls`, give `command` the binary's absolute path.

## References

Read the reference for the task in front of you:

| Task | Read |
|---|---|
| Installing or updating the binary, or building with `transport-http` | [references/install.md](references/install.md) |
| Finding which `mcpls.toml` loads, or why a checkout's `mcpls.toml` is ignored | [references/config-loading.md](references/config-loading.md) |
| Running several servers for one language, or sending a tool to a specific server | [references/routing.md](references/routing.md) |
| Letting rename, formatting, or code actions write to disk | [references/apply.md](references/apply.md) |
| Tuning diagnostic severity, volume, write-tool footers, or hook timing | [references/diagnostics.md](references/diagnostics.md) |
| A language server missing an environment variable it needs | [references/env.md](references/env.md) |
| mcpls or a language server failing to start, time out, or answer | [references/troubleshooting.md](references/troubleshooting.md) |

Using the MCP tools for coding work is the `mcpls` skill.
