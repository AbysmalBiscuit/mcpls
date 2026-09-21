# Running mcpls

`mcpls --help` is the full list of flags, environment variables, and subcommands. This file covers what `--help` leaves out: how the variables behave, and how to register mcpls with a client.

## Flags and environment variables

Each server flag has an `MCPLS_*` twin; the flag wins when both are set.

| Flag | Env var | Behavior `--help` does not show |
|---|---|---|
| `--config <FILE>` / `-c` | `MCPLS_CONFIG` | Always trusted, even a relative path, because naming a file is consent. Hard-errors if the file is missing, where auto-detection falls back. See [config-loading.md](config-loading.md). |
| `--trust-project-config` | `MCPLS_TRUST_PROJECT_CONFIG` | Loads the checkout's own `mcpls.toml`. See [config-loading.md](config-loading.md#trust). |
| `--no-backend` | `MCPLS_NO_BACKEND` | Serves this session in-process instead of through the checkout's shared backend. For debugging, and for hosts where a detached backend cannot run. |
| `--log-level <LEVEL>` / `-l` | `MCPLS_LOG` | Takes any `tracing-subscriber` filter, e.g. `mcpls=debug,info`. An invalid value silently falls back to `info`. |
| `--log-json` | `MCPLS_LOG_JSON` | JSON log lines. |
| `--listen <ADDR>` | `MCPLS_LISTEN` | Serves Streamable HTTP instead of stdio. `transport-http` builds only; see [HTTP transport](#http-transport). |
| `--http-path <PATH>` | `MCPLS_HTTP_PATH` | Mount path, default `/mcp`. Only meaningful with `--listen`. |

Boolean variables accept `1`/`0`, `true`/`false`, `yes`/`no`, `y`/`n`, and `on`/`off`, case-insensitive. Any other value, including an empty `MCPLS_LOG_JSON=`, fails startup.

## Subcommands

With no subcommand, mcpls runs the MCP server. The others print and exit:

- `mcpls completions <shell>` prints a shell completion script.
- `mcpls schema` prints the `mcpls.toml` JSON Schema; `mcpls schema init` writes a default `mcpls.toml` in the current directory.
- `mcpls doctor [DIR]` reports the socket path, the backend's pid, uptime, sessions, language servers, configuration, and whether `mcpls` resolves on `PATH`. Without `DIR` it examines `$CLAUDE_PROJECT_DIR`, then the working directory; the report names which. It exits non-zero on a fault. `mcpls hook doctor` is an alias that always exits 0, which the hook registrations require.
- `mcpls backend <start|stop|status> [DIR]` controls the shared backend for a checkout. Without `DIR` it examines `$CLAUDE_PROJECT_DIR`, then the working directory. `start` starts a backend, or keeps the one already running, and a kept backend stays up with no session attached until `stop`, ignoring `idle_shutdown_ms`. A new backend runs with the invocation's `--config`, `--trust-project-config`, `--log-level` and `--log-json`. `stop` ends an idle backend. With sessions attached it exits non-zero and leaves the backend to exit once they close; `--force` ends it at once, and those sessions have no mcpls tools until they restart. `status` prints the backend's pid, version, attached sessions and log path, and exits non-zero when none answers.
- `mcpls config [DIR] [--origin] [--json]` prints the configuration resolved for a directory: the tier that won, the file it came from, the fingerprint, and the merged settings with the built-in servers folded in. `--origin` annotates each setting with the file that decided it, or marks it a default. `--json` prints the configuration alone, so a script can read it back. Honors `--config` and `--trust-project-config`, which change the answer.
- `mcpls brief [--additional-context]` prints the note a session start hook hands the agent: the installed language servers that serve the checkout and a pointer to the mcpls tools. It examines `$CLAUDE_PROJECT_DIR`, then the working directory. It prints nothing when `[brief] enabled = false` or when no installed server applies, and always exits 0. `--additional-context` wraps the note in the JSON a Codex `SessionStart` hook returns.
- `mcpls hook <event> --harness <claude-code|codex>` serves one agent hook invocation from stdin, where `<event>` is the harness event in kebab case (`post-tool-use`, `post-tool-batch`, `user-prompt-submit`, `session-end`). The spelling matches devkit's `devkit hook` verbs. The plugins register it; running it by hand is rarely useful.

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

To load a trusted checkout's `mcpls.toml`, put `"--trust-project-config"` in that entry's `args`. Scope the grant this way rather than exporting `MCPLS_TRUST_PROJECT_CONFIG=true` from a shell profile or `.envrc`: the variable trusts every `mcpls` that shell launches, including future untrusted checkouts.

If the client cannot find `mcpls`, give `command` the binary's absolute path. MCP servers start without a login shell, so a `PATH` set in a shell profile may not apply.

## HTTP transport

HTTP needs a binary built with `--features transport-http` ([install.md](install.md)). A build without it fails in two different ways:

- `--listen` on the command line is a startup parse error, `unexpected argument '--listen'`.
- `MCPLS_LISTEN` or `MCPLS_HTTP_PATH` in the environment is silently ignored, and mcpls serves stdio.

Either symptom means reinstalling with the feature.
