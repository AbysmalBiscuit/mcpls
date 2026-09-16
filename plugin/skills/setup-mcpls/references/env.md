# Language server environment: `env`

Spawned LSP servers start from a cleared environment, as a security boundary. mcpls passes through a minimal allowlist from its own process (`PATH`, `HOME`, `USERPROFILE`, and `TMPDIR`/`TEMP`/`TMP` on every platform, plus Windows loader essentials), then applies the server's `[lsp_servers.env]` table on top, so entries there override the passthrough.

```toml
[[lsp_servers]]
language_id = "python"
env = { VIRTUAL_ENV = "/home/me/project/.venv" }
```

Use `env` to restore anything a server needs beyond the allowlist: proxy settings, `VIRTUAL_ENV`/`PYTHONPATH`, or toolchain variables a `build.rs` reads, such as `DATABASE_URL`, `LIBCLANG_PATH`, or `SSH_AUTH_SOCK`. Values are written literally into `mcpls.toml`, which is often committed, so keep real secrets out of it. Forwarding `SSH_AUTH_SOCK` hands the ssh-agent socket to the language server, so forward it only to servers you trust.

**`PATH` caution:** an `env.PATH` entry replaces the passthrough value rather than extending it. On Unix, a bare `command` then resolves against your override and stops working unless the override keeps its directory. On Windows the loader still falls back to the parent process's `PATH`, so the same mistake rarely breaks anything. To add one directory, give `command` an absolute path and leave `PATH` alone.
