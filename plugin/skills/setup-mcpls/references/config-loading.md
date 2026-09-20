# Which mcpls.toml loads

## Search order

The first match wins:

1. `--config <FILE>` or `$MCPLS_CONFIG`. Always trusted. A missing file is a hard error; this path never falls back and never creates a file.
2. `mcpls.toml` at the checkout root, only when trusted (see [Trust](#trust)). Sessions started in a subdirectory find the same file.
3. The user config file:

   | Platform | Path |
   |---|---|
   | Linux | `$XDG_CONFIG_HOME/mcpls/mcpls.toml`, else `~/.config/mcpls/mcpls.toml` |
   | macOS | `~/Library/Application Support/mcpls/mcpls.toml` |
   | Windows | `%APPDATA%\mcpls\mcpls.toml` |

   macOS reads only the `Application Support` path, never `~/.config`.
4. Built-in defaults.

When auto-detection finds no file, mcpls writes a commented template to the user config path and runs on the built-in defaults. If that write fails, for example on a read-only filesystem, it logs a warning and keeps running on the defaults.

## Trust

A checkout's `mcpls.toml` can set the `command` and `args` mcpls spawns, so loading one from an unfamiliar checkout would run arbitrary code. mcpls ignores it unless started with `--trust-project-config` or `MCPLS_TRUST_PROJECT_CONFIG=true`. The variable grants trust to every mcpls process that inherits it, not to one project; [cli.md](cli.md#registering-with-an-mcp-client) shows how to scope the grant to one client entry.

An ignored config is reported in two places: a warning on stderr, which a stdio client rarely shows, and a note appended to the server instructions in the MCP `initialize` response. When the server instructions mention an ignored project config, the file only needs a trust decision.

`mcpls config` prints the configuration this checkout resolves to: the tier that won, the file it came from, and the merged settings. It names an ignored project config outright, on a `# ignored:` line, rather than leaving it to be inferred from a fingerprint that does not match what was just written. `mcpls config --origin` goes further and marks each setting with the file that decided it. `mcpls doctor` prints the fingerprint the running backend actually loaded, beside the one this build resolves.
