# Custom language server install commands Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a `[[lsp_servers]]` entry carry an install command, and add `mcpls lsp install` to run the ones whose binaries are missing.

**Architecture:** A new `install` key on the server config, typed as a string-or-per-OS enum in `mcpls-core`. A new `crates/mcpls-cli/src/install.rs` selects targets, runs each command through the platform shell as a child of the CLI, and reports one line per server. Afterwards it asks a running backend, if any, to start what it installed. `mcpls doctor` adds a hint when a missing server has a command.

**Tech Stack:** Rust, serde (untagged enum), schemars, clap, tokio `process`, the `base64` crate on Windows, assert_cmd for CLI tests.

**Spec:** `docs/superpowers/specs/2026-09-26-lsp-install-commands-design.md`

## Global Constraints

- Every build, test, lint, format and schema command goes through `devrun task <name>` (`check`, `test`, `lint`, `fmt`, `schema`, `verify`). Pass paths as absolute paths; never `cd`.
- Commit with `devrun -C <worktree> task commit --arg 'files=<comma-separated paths>' --arg 'commit_subject=<type(scope): subject>' --arg 'coauthors=Claude Opus 5 <noreply@anthropic.com>'`, adding `--arg 'commit_body=...'` when the change needs context.
- Unix runs `sh -c <command>`. Windows runs `powershell -NoProfile -EncodedCommand <base64 of the UTF-16LE command>`.
- The command's working directory is the checkout root (`hook::checkout_root`), and it inherits the CLI's environment and stdio. The server's `env` table does not apply.
- Report lines use `lsp.rs`'s shape: `format!("{id}  {outcome}")`, two spaces, newline-terminated.
- Outcome strings, verbatim: `installed`, `already installed`, `no install command`, `would run`, `failed (exit N)`, `failed (no exit code)`, `could not start: <error>`, `installed, but <command> is still not on PATH; open a new shell`.
- Header before each run (and in dry-run): `==> <id>: <command>`.
- Unknown named id message, verbatim from `bridge/translator/control.rs`: `no language server named <ids joined ", "> applies here; these do: <ids joined ", ">`, or `...; none do` when nothing applies.
- Doctor hint, appended to the existing state: ``not installed: <command> is not on PATH; run `mcpls lsp install <id>` ``.
- No built-in server gets an install command.
- Comments follow the repo's density: doc comments on public items, inline comments only for a non-obvious why.

## Review Focus

1. A Windows command containing double quotes must reach PowerShell intact. Task 2's integration tests write the Windows command with double-quoted paths.
2. Running `mcpls lsp install` from a subdirectory must run the command in the checkout root, not the working directory. Task 2 adds `install_from_a_subdirectory_runs_in_the_checkout_root`.
3. Naming the same server twice (`mcpls lsp install fake fake`) must run its command once. Task 2's `plan` unit test covers it.
4. A command killed by a signal has no exit code. The report must still say it failed, and the run must exit 1. Task 2's `Status` display and `fails` unit tests cover `Failed(None)`.
5. `install = ""` or whitespace only must count as no install command, not a shell run that "succeeds". Task 1's `for_this_os` unit test covers it.

---

### Task 1: The `install` config key

**Files:**
- Modify: `crates/mcpls-core/src/config/server.rs` (types, `LspServerConfig`, `PartialLspServerConfig`, `builtin`, `is_spawn_only_overlay`, `merge`, `from_partial`, tests)
- Modify: every other `LspServerConfig { .. }` struct literal in the workspace (add `install: None`). `devrun task check` lists them. They sit in `config/{mod,routing}.rs`, `lib.rs`, `lsp/lifecycle.rs`, `hooks/service.rs`, `backend/endpoint.rs`, `bridge/translator/{respawn,routing,symbols}.rs`, and `crates/mcpls-core/tests/{ra_e2e,pyrefly_e2e,integration/rust_analyzer_tests}.rs`.
- Modify: `schema/mcpls-config.json` (regenerated)
- Modify: `docs/user-guide/configuration.md`, `plugin/skills/setup-mcpls/references/configuration.md`

**Interfaces:**
- Produces, in `mcpls_core::config` (re-exported the way `LspServerConfig` already is):
  - `pub enum InstallCommand { Any(String), PerOs(PerOsInstall) }`, derives `Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema`, `#[serde(untagged)]`.
  - `pub struct PerOsInstall { pub unix: Option<String>, pub windows: Option<String> }`, same derives, `#[serde(deny_unknown_fields)]`, both fields `#[serde(default, skip_serializing_if = "Option::is_none")]`.
  - `impl InstallCommand { #[must_use] pub fn for_this_os(&self) -> Option<&str> }`, where `cfg!(windows)` picks `windows` and everything else picks `unix`, and a blank (`trim().is_empty()`) command returns `None`.
  - `LspServerConfig::install: Option<InstallCommand>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`.
  - `PartialLspServerConfig::install: Option<InstallCommand>` with `#[serde(default)]`.

- [ ] **Step 1: Write the failing tests** in the `tests` module of `config/server.rs`:

```rust
#[test]
fn test_install_accepts_one_string_for_every_os() {
    let config: ServerConfig = toml::from_str(
        "[[lsp_servers]]\nlanguage_id = \"python\"\ncommand = \"pyrefly\"\ninstall = \"uv tool install pyrefly\"\n",
    ).unwrap();
    let server = config.lsp_servers.iter().find(|s| s.command == "pyrefly").unwrap();
    assert_eq!(server.install, Some(InstallCommand::Any("uv tool install pyrefly".into())));
    assert_eq!(server.install.as_ref().unwrap().for_this_os(), Some("uv tool install pyrefly"));
}

#[test]
fn test_install_picks_the_variant_for_this_os() {
    let install = InstallCommand::PerOs(PerOsInstall {
        unix: Some("brew install zls".into()),
        windows: Some("winget install zigtools.zls".into()),
    });
    let expected = if cfg!(windows) { "winget install zigtools.zls" } else { "brew install zls" };
    assert_eq!(install.for_this_os(), Some(expected));
    let other_os_only = if cfg!(windows) {
        PerOsInstall { unix: Some("x".into()), windows: None }
    } else {
        PerOsInstall { unix: None, windows: Some("x".into()) }
    };
    assert_eq!(InstallCommand::PerOs(other_os_only).for_this_os(), None);
}

#[test]
fn test_a_blank_install_command_is_no_command() {
    assert_eq!(InstallCommand::Any("   ".into()).for_this_os(), None);
}

#[test]
fn test_install_rejects_an_unknown_os_key() {
    let parsed: std::result::Result<ServerConfig, _> = toml::from_str(
        "[[lsp_servers]]\nlanguage_id = \"zig\"\n[lsp_servers.install]\nmacos = \"brew install zls\"\n",
    );
    assert!(parsed.is_err());
}

#[test]
fn test_install_is_inherited_unless_command_is_replaced() {
    let mut builtin = LspServerConfig::zls();
    builtin.install = Some(InstallCommand::Any("get zls".into()));
    let mut kept = builtin.clone();
    kept.merge(PartialLspServerConfig { timeout_seconds: Some(5), ..Default::default() });
    assert_eq!(kept.install, builtin.install);
    let mut replaced = builtin.clone();
    replaced.merge(PartialLspServerConfig { command: Some("other-zls".into()), ..Default::default() });
    assert_eq!(replaced.install, None);
    let mut set = builtin;
    set.merge(PartialLspServerConfig {
        command: Some("other-zls".into()),
        install: Some(InstallCommand::Any("get other".into())),
        ..Default::default()
    });
    assert_eq!(set.install, Some(InstallCommand::Any("get other".into())));
}

#[test]
fn test_builtins_carry_no_install_command() {
    assert!(LspServerConfig::builtins().iter().all(|s| s.install.is_none()));
}
```

Match the existing tests' way of parsing a `ServerConfig` from TOML if it differs from `toml::from_str`, keeping the same assertions.

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `devrun task test`
Expected: compile failure naming `InstallCommand`, `PerOsInstall` and the missing `install` field.

- [ ] **Step 3: Add the types and the field**

- Add the types and `for_this_os` as the Interfaces block specifies.
- Add `install` to both structs, with a doc comment. The partial's doc states the overlay rule: "An overlay inherits the built-in value unless `command` is replaced; a new server defaults to none".
- Set `install: None` in `builtin`.
- Add `&& self.install.is_none()` to `is_spawn_only_overlay`.
- In `merge`, destructure `install`. Put `self.install = None;` inside the `if let Some(command)` block, then `if let Some(install) = install { self.install = Some(install); }`.
- In `from_partial`, set `install: partial.install`.
- Add `install: None` at every other struct literal `devrun task check` reports.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `devrun task test`
Expected: PASS, including the existing config, schema and routing tests.

- [ ] **Step 5: Regenerate the schema and document the key**

- Run `devrun task schema`. Expected: `schema/mcpls-config.json` gains `install` under the server entry, as a string or an object with `unix`/`windows`.
- In `docs/user-guide/configuration.md`, add an `### \`install\`` section beside `### \`diagnostics_severity\``. It shows both TOML forms from the spec and states:
  - it runs only through `mcpls lsp install`;
  - Unix runs `sh -c`;
  - Windows runs Windows PowerShell 5.1, so chain with `;` rather than `&&`;
  - the working directory is the checkout root;
  - a project-root config needs `--trust-project-config`.
- Add an `install` row to the key table in `plugin/skills/setup-mcpls/references/configuration.md`.

- [ ] **Step 6: Commit**

`devrun -C <worktree> task commit` with every touched file, subject `feat(config): add an install command to lsp servers`.

---

### Task 2: `mcpls lsp install`

**Files:**
- Create: `crates/mcpls-cli/src/install.rs`
- Modify: `crates/mcpls-cli/src/args.rs` (`LspCommand::Install`; `Command::Lsp` doc)
- Modify: `crates/mcpls-cli/src/main.rs` (`mod install;`, dispatch in `run_lsp`)
- Modify: `crates/mcpls-cli/Cargo.toml` (`[target.'cfg(windows)'.dependencies] base64`, the version `Cargo.lock` already resolves)
- Create: `crates/mcpls-cli/tests/lsp_install.rs`
- Modify: `plugin/skills/setup-mcpls/references/cli.md`

**Interfaces:**
- Consumes: `LspServerConfig::install`, `InstallCommand::for_this_os` (Task 1); `hook::resolve_program(&str, &Path) -> Option<PathBuf>`; `hook::checkout_root`; `main.rs`'s `examined_directory` and `resolve_config`; `lsp::Outcome { text, success }`.
- Produces:
  - `LspCommand::Install { #[command(flatten)] targets: LspTargets, #[arg(long)] dry_run: bool }`, doc "Run the configured install command for servers whose binary is missing".
  - `pub struct Target { pub id: String, pub program: String, pub install: Option<String>, pub named: bool }`, where `program` is the server's `command` and `install` is the `for_this_os` command.
  - `pub fn plan(config: &ServerConfig, root: &Path, named: &[String]) -> Result<Vec<Target>, String>`. An empty `named` means every applicable server. It keeps configuration order, keeps the first server per id, and drops repeated names. It returns `Err` with the unknown-id message from Global Constraints.
  - `pub enum Status { Installed, AlreadyInstalled, NoCommand, WouldRun, Failed(Option<i32>), CouldNotStart(String), StillMissing(String) }` with `Display` giving the Global Constraints strings, and `pub fn fails(&self, named: bool) -> bool`.
  - `pub async fn install(targets: &[Target], root: &Path, dry_run: bool, installed: impl Fn(&str) -> bool) -> Vec<(String, Status)>`.
  - `fn shell(command: &str) -> tokio::process::Command`: `sh -c` or `powershell -NoProfile -EncodedCommand`, `cfg`-selected.
  - `#[cfg(windows)] fn encode_powershell(command: &str) -> String`: base64 (standard alphabet, padded) of the command's UTF-16LE bytes.
  - `pub fn render(results: &[(String, Status)]) -> String` in the report shape.

- [ ] **Step 1: Write the failing unit tests** in `install.rs`'s `tests` module:
  - `test_plan_keeps_configuration_order_and_drops_repeated_names`: a config with two applicable servers `a` then `b` (markers present in a temp dir). `plan(.., &["b", "a", "b"])` returns ids `["a", "b"]`, both `named`.
  - `test_plan_all_skips_servers_that_do_not_apply`: a server whose marker is absent is not in `plan(.., &[])`.
  - `test_plan_names_the_applicable_servers_for_an_unknown_one`: `plan(.., &["nope"])` returns `Err("no language server named nope applies here; these do: a, b")`.
  - `test_failed_without_an_exit_code_still_fails`: `Status::Failed(None).to_string() == "failed (no exit code)"`, and `Status::Failed(None).fails(false)` is true.
  - `test_only_a_named_server_fails_for_lacking_a_command`: `NoCommand.fails(true)` is true, `NoCommand.fails(false)` is false. `Installed`, `AlreadyInstalled` and `WouldRun` never fail. `StillMissing(_)` and `CouldNotStart(_)` always fail.
  - `#[cfg(windows)] test_powershell_commands_are_utf16_base64`: `encode_powershell("a") == "YQA="`.

- [ ] **Step 2: Write the failing integration tests** in `tests/lsp_install.rs`.

Copy `checkout()`, `short_temp_dir()`, `run()` and `stdout()` from `tests/doctor_and_config.rs`, and add:

```rust
/// The fake server's path, which the install command creates under `root`.
fn fake_program(root: &Path) -> PathBuf {
    if cfg!(windows) { root.join("bin").join("fake-ls.cmd") } else { root.join("bin").join("fake-ls") }
}

/// Writes a config with one applicable server `fake` whose command is
/// `fake_program(root)`, plus `install` (a TOML value) when given.
fn write_config(root: &Path, install: Option<&str>) -> PathBuf
```

The creating command, used wherever a test needs a working install. It holds double quotes on purpose (Review Focus 1):

```toml
[lsp_servers.install]
unix = 'mkdir -p "bin" && printf "#!/bin/sh\n" > "bin/fake-ls" && chmod +x "bin/fake-ls"'
windows = 'New-Item -ItemType Directory -Force "bin" | Out-Null; Set-Content -Path "bin/fake-ls.cmd" -Value "@echo off"'
```

Tests, each run as `mcpls --config <file> lsp install ...`:
  - `install_runs_the_command_and_reports_the_server_installed`: `lsp install fake`. Exit 0. Stdout contains `==> fake: ` and ends with `fake  installed\n`. `fake_program(root)` exists.
  - `install_from_a_subdirectory_runs_in_the_checkout_root` (Review Focus 2): run from `root/sub` with no `--dir`. `fake_program(root)` exists and `root/sub/bin` does not.
  - `a_failing_install_command_fails_the_run`: `install = "exit 3"`. Exit 1, stdout ends with `fake  failed (exit 3)\n`.
  - `an_install_that_leaves_the_binary_missing_says_to_open_a_new_shell`: `install = "echo done"`. Exit 1, stdout ends with `fake  installed, but <fake_program(root)> is still not on PATH; open a new shell\n`.
  - `dry_run_prints_the_command_and_runs_nothing`: `lsp install --all --dry-run`. Exit 0. Stdout contains `==> fake: ` and ends with `fake  would run\n`. `fake_program(root)` does not exist.
  - `an_installed_server_is_skipped`: create `fake_program(root)` first (executable on Unix), with `install = "exit 3"`. Exit 0, stdout is exactly `fake  already installed\n`.
  - `a_named_server_without_an_install_command_fails_but_all_does_not`: no `install`. `lsp install fake` exits 1 with `fake  no install command\n`. `lsp install --all` exits 0 with the same line.
  - `an_unknown_server_names_the_ones_that_apply`: `lsp install nope`. Exit 1, stdout `no language server named nope applies here; these do: fake\n`.

- [ ] **Step 3: Run the tests and confirm they fail**

Run: `devrun task test`
Expected: compile failure on the missing `install` module and `LspCommand::Install`.

- [ ] **Step 4: Implement**

- `args.rs`: add the `Install` variant. Reword the `Command::Lsp` doc to "Install, start, stop, restart, or list the language servers of a checkout", and note that `install` needs no backend.
- `main.rs`: add `mod install;`. In `run_lsp`, handle `LspCommand::Install { targets, dry_run }` first. That arm resolves the directory and root the way the other arms do, loads the config with `resolve_config`, and turns a load error into a failed `lsp::Outcome` holding the error text. It then calls `install::plan`, where an `Err` becomes a failed outcome whose text is the message plus a newline. Next it calls `install::install` with `|program| hook::resolve_program(program, &root).is_some()`. It returns `lsp::Outcome { text: install::render(..), success: !any fails(named) }`.
- `install.rs`:
  - Applicability is `server.should_spawn(root, Some(config.workspace.heuristics_max_depth))`.
  - For each target, the checks run in order:
    1. `installed(program)` gives `AlreadyInstalled`.
    2. No command gives `NoCommand`.
    3. `dry_run` prints the header and gives `WouldRun`.
    4. Otherwise it prints the header and flushes stdout, then runs `shell(cmd).current_dir(root).status().await`.
  - A spawn error gives `CouldNotStart(error.to_string())`. A non-success exit gives `Failed(status.code())`. After success, `installed(program)` decides `Installed` or `StillMissing(program)`.
  - Commands run strictly one after another.

- [ ] **Step 5: Run the tests and confirm they pass**

Run: `devrun task test`
Expected: PASS for the new unit and integration tests and the existing suite.

- [ ] **Step 6: Document the verb**

In `plugin/skills/setup-mcpls/references/cli.md`, add a bullet for `mcpls lsp install <SERVER>... | --all [--dir DIR] [--dry-run]`. It covers:
- what it skips;
- that it runs commands one at a time in the checkout root;
- that it needs no backend;
- when it exits non-zero, per the spec's list.

- [ ] **Step 7: Lint and commit**

Run `devrun task lint` and `devrun task fmt`, then commit with subject `feat(cli): install language servers with mcpls lsp install`.

---

### Task 3: Start what was installed in a running backend

**Files:**
- Modify: `crates/mcpls-cli/src/install.rs` (`nudge`)
- Modify: `crates/mcpls-cli/src/main.rs` (call it in the `Install` arm)
- Test: `crates/mcpls-cli/tests/backend.rs`

**Interfaces:**
- Consumes: `install::install`'s results (Task 2); `mcpls_core::hooks::{identity_for, probe, Request}`; `mcpls_core::bridge::LspAction::Start`.
- Produces: `pub async fn nudge(root: &Path, installed: Vec<String>)`. It does nothing when `installed` is empty or `identity_for(root)` fails. Otherwise it `probe`s `Request::Lsp { action: LspAction::Start, servers: installed }` with a 3-second timeout (the same value as `lsp::ANSWER_TIMEOUT`) and discards the result. It never prints.

- [ ] **Step 1: Write the failing test** in `tests/backend.rs`, `#[cfg(unix)]`, next to the other `lsp_commands_*` tests.

Add a `Project::with_installable_lifecycle_server(self) -> Self`. It is `with_lifecycle_server`'s config with three changes:
- `command` is `<root>/bin/fake-ls`;
- `args` keeps the script and log paths;
- it adds `install = 'mkdir -p bin && printf "#!/bin/sh\nexec python3 \"\$@\"\n" > bin/fake-ls && chmod +x bin/fake-ls'`, TOML-escaped as needed so the generated script runs `python3` with the args.

```rust
#[cfg(unix)]
#[test]
fn lsp_install_starts_the_installed_server_in_a_running_backend() {
    let project = Project::new(60_000).with_installable_lifecycle_server();
    let mut frontend = project.frontend();
    project.assert_attaches(&mut frontend);
    project.wait_for("the server to be recorded missing", |p| {
        stdout(&p.lsp(&["status"])).contains("fake  not installed")
    });

    let install = project.lsp(&["install", "fake"]);
    assert!(install.status.success(), "{}", stdout(&install));
    assert!(stdout(&install).ends_with("fake  installed\n"), "{}", stdout(&install));

    project.wait_for("the backend to start it", |p| {
        stdout(&p.lsp(&["status"])).contains("fake  running")
    });
}
```

- [ ] **Step 2: Run it and confirm it fails**

Run: `devrun task test`
Expected: the new test times out in `wait_for("the backend to start it")`, with status still `fake  not installed`. On Windows the test is compiled out, so confirm RED on a Unix machine or CI.

- [ ] **Step 3: Implement `nudge`.** Call it from the `Install` arm with the ids whose status is `Installed`, after the installs finish and before building the outcome.

- [ ] **Step 4: Run it and confirm it passes**

Run: `devrun task test`
Expected: PASS.

- [ ] **Step 5: Commit** with subject `feat(cli): start installed servers in a running backend`.

---

### Task 4: Doctor names the install command

**Files:**
- Modify: `crates/mcpls-cli/src/hook.rs` (`doctor_scanning`, the `not installed` branch near line 713)
- Test: `crates/mcpls-cli/tests/doctor_and_config.rs`
- Modify: `plugin/skills/setup-mcpls/references/troubleshooting.md`

**Interfaces:**
- Consumes: `LspServerConfig::install`, `InstallCommand::for_this_os` (Task 1).

- [ ] **Step 1: Write the failing test** `the_doctor_points_a_missing_server_at_its_install_command`. It copies `the_doctor_says_a_configured_server_is_not_installed_and_exits_non_zero`'s config and adds `install = "echo nothing"`, then asserts that the report contains:

```
invented (not installed: mcpls-no-such-language-server is not on PATH; run `mcpls lsp install invented`)
```

The existing test, with no `install`, keeps asserting the line without the hint.

- [ ] **Step 2: Run it and confirm it fails**

Run: `devrun task test`
Expected: FAIL. The report has the line without the hint.

- [ ] **Step 3: Implement.** In the `not installed` branch, when `server.install.as_ref().and_then(InstallCommand::for_this_os).is_some()`, append the hint to `state` per Global Constraints.

- [ ] **Step 4: Run it and confirm it passes**

Run: `devrun task test`
Expected: PASS for both doctor tests.

- [ ] **Step 5: Document.** Add a row to `troubleshooting.md`'s table for a doctor line ending in ``run `mcpls lsp install <id>` ``, telling the reader to run that command.

- [ ] **Step 6: Verify and commit**

Run `devrun task verify`. Expected: formatting, lint and every test pass. Commit with subject `feat(doctor): point a missing server at its install command`.
