# Troubleshooting mcpls

Start with `mcpls hook doctor`: it shows whether a backend is running, its language servers, the configuration it loaded, and whether `mcpls` resolves on `PATH`. For startup failures, rerun with `--log-level debug` (or `trace`) and read stderr.

| Symptom | Cause and fix |
|---|---|
| A tool returns a "still initializing" error | The server is up but has not finished its `initialize` handshake, common with rust-analyzer on a large repo. The error returns immediately rather than timing out, so raising timeouts does nothing. Wait and retry. |
| A request times out | Raise that server's `request_timeout_seconds` (per request) or `timeout_seconds` (handshake) in `mcpls.toml`. |
| "no LSP server configured for language: ..." | No `[[lsp_servers]]` entry covers the file's language. Add one, or map the extension in `language_extensions`. See [configuration.md](configuration.md). |
| "failed to spawn LSP server '...'" or "LSP server '...' is unavailable" | The server's `command` is not on `PATH`, or it crashed. Install it, or give `command` an absolute path. |
| "no server handles tool '...'" | No server claims that tool through `handles`, and the language has no catch-all server. See [configuration.md](configuration.md#tool-routing-handles). |
| mcpls refuses to start, naming two `[[lsp_servers]]` entries | Two servers for one language overlap in this workspace and collide on routing. See [configuration.md](configuration.md#ambiguous-configs-fail-at-startup-not-silently). |
| A checkout's `mcpls.toml` has no effect | It is untrusted, or another file wins the search. See [config-loading.md](config-loading.md). |
| `--listen` is an unknown argument, or `MCPLS_LISTEN` is ignored | The binary lacks `transport-http`. See [cli.md](cli.md#http-transport). |
| "command not found: mcpls", or the client does not list mcpls | Cargo's bin directory is not on the client's `PATH`. Give the client entry an absolute `command`. |
| High memory or CPU | Usually over-broad `workspace.roots` or more servers than the workspace needs. Set `enabled = false` on unused built-ins. |

The [Troubleshooting Guide](https://github.com/AbysmalBiscuit/mcpls/blob/main/docs/user-guide/troubleshooting.md) covers each symptom at length, including testing a language server directly.
