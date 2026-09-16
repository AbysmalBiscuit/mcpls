# Installing mcpls

The Claude Code and Codex plugins install the release binary matching the plugin version on their own. Install by hand only outside a plugin, or to replace that binary.

## Installer (recommended)

Installs the release binary into `$CARGO_HOME/bin`.

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/AbysmalBiscuit/mcpls/releases/latest/download/mcpls-installer.sh | sh
```

On Windows:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/AbysmalBiscuit/mcpls/releases/latest/download/mcpls-installer.ps1 | iex"
```

## Pre-built binaries

Download the archive for your platform from [GitHub Releases](https://github.com/AbysmalBiscuit/mcpls/releases), extract it, and move the `mcpls` binary onto your `PATH`. Unix archives are `mcpls-<target>.tar.xz`, Windows archives are `mcpls-<target>.zip`. Each archive ships with a `.sha256` sidecar; verify before extracting:

```bash
curl -LO https://github.com/AbysmalBiscuit/mcpls/releases/latest/download/mcpls-<target>.tar.xz
curl -LO https://github.com/AbysmalBiscuit/mcpls/releases/latest/download/mcpls-<target>.tar.xz.sha256
shasum -a 256 -c mcpls-<target>.tar.xz.sha256
```

## Cargo

```bash
cargo install --git https://github.com/AbysmalBiscuit/mcpls mcpls
```

From a checkout of this repository:

```bash
cargo install --path crates/mcpls-cli
```

Both build the default feature set, which has no HTTP transport. Add `--features transport-http` to serve over HTTP.

## Verify

```bash
mcpls --version
```

## Language servers

mcpls needs a language server on `PATH` for each language it should understand. See [Language Server Setup](https://github.com/AbysmalBiscuit/mcpls/blob/main/docs/user-guide/installation.md#language-server-setup) for per-language install commands.
