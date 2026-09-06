# Diagnostics injection, stage A: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make configuration entries merge onto the built-in servers, then build the per-session diagnostics deduplication core and the tool that drains it.

**Architecture:** Config entries become partial records folded onto the built-in server list by routing identity, which is what makes a per-server severity floor reachable. A new `bridge::delivery` module holds one record per session mapping each file to a hash of its diagnostics, so a flush returns only what changed. The diagnostics pump gains a document-tracker handle to drop publishes describing text that is already gone, and a background task takes the baseline once the language servers have been quiet long enough to mean it.

**Tech Stack:** Rust 2024 edition, MSRV 1.88, tokio, rmcp 3.1.4, serde/toml, clippy pedantic + nursery.

**Spec:** `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`

## Global Constraints

- Rust edition 2024, MSRV 1.88. `cargo fmt` clean and `cargo clippy --workspace --all-targets -- -D warnings` clean before every commit.
- `unwrap_used` and `expect_used` are warn-level in this crate. Production code uses neither. Test code that needs one carries `#[allow(clippy::unwrap_used)]` or `#[allow(clippy::expect_used)]` on the test module or the individual test, matching the surrounding file. Note that these are two separate lints: a module allowing `unwrap_used` still fails on `expect` and `expect_err`.
- `missing_docs` is warn-level. Every public item gets a doc comment. Every public function returning `Result` gets an `# Errors` section.
- `too_many_lines` triggers at 100 and `too_many_arguments` at 7. Split rather than allow. `spawn_lsp_servers_background` already sits at exactly 7 parameters, so any task adding one to it must collapse instead: task 4 does this.
- Comments explain a non-obvious *why*, never restate the *what*. No comment mentions this plan, a task number, a PR, or a TDD phase.
- This is the `AbysmalBiscuit/mcpls` fork. Breaking configuration changes are acceptable and intended. Do not add backward-compatibility shims.
- Work in the worktree `/home/lev/Git/lev/mcpls-diagnostics` on branch `feat/diagnostics-injection`. Never switch the branch checked out in `/home/lev/Git/lev/mcpls`.
- Commits follow Conventional Commits: `type(scope): description`, imperative, at most 50 characters in the subject, lowercase after the colon, no trailing period. Body wrapped at 72 characters when the change needs context. Every commit ends with the trailer `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.
- Commits are GPG signed. If signing fails with a timeout, stop and report rather than passing `--no-gpg-sign`.
- Update `CHANGELOG.md` under `## [Unreleased]` in the same commit as any user-visible change.

## File structure

| File | Responsibility |
|---|---|
| `crates/mcpls-core/src/config/server.rs` | `LspServerConfig`, the new `PartialLspServerConfig`, the built-in list, and the fold of one onto the other. |
| `crates/mcpls-core/src/config/mod.rs` | `ServerConfig`, the new `DiagnosticsConfig` and `SeverityFloor`, and the serde wiring that resolves partial entries at load time. |
| `crates/mcpls-core/src/bridge/delivery.rs` | New. The per-session record, the diagnostic-set hash, the severity floor and volume caps, and the flush. Pure: no locks, no IO, and no notion of a URI, a path or an encoding. |
| `crates/mcpls-core/src/bridge/settle.rs` | New. Tracks outstanding `$/progress` operations so the baseline is taken when the servers have gone quiet and stayed quiet. |
| `crates/mcpls-core/src/bridge/notifications.rs` | `NotificationCache` gains an owner-aware snapshot accessor. |
| `crates/mcpls-core/src/lib.rs` | `PumpShared` carries everything the background tasks need; the pump drops stale publishes and feeds the settle tracker; a new task takes the baseline. |
| `crates/mcpls-core/src/mcp/handlers.rs` | `BridgeContext` gains the delivery core and the floor table. |
| `crates/mcpls-core/src/mcp/server.rs` | The `get_new_diagnostics` tool. |
| `crates/mcpls-core/tests/ra_e2e.rs` | A sub-case proving the tool is wired and deduplicates against a real rust-analyzer. |

## Deliberate deviations from the spec

- The spec's `[diagnostics]` block shows a `footer` key. This plan does not add it. Nothing in stage A reads it, and an inert configuration key is worse than an absent one. Stage B adds it in the commit that makes it do something.
- Task 1 also changes what a config file with no `[[lsp_servers]]` block produces. Today that is zero servers (`config/mod.rs:1600-1610`), so a file setting only `[workspace]` spawns nothing at all. After the merge it produces the built-ins. This is a strict improvement and falls out of the same change, so it is not called out separately in the spec.
- The spec describes the baseline as taken "when the servers go quiet". Measurement against rust-analyzer 1.98.1 shows that the first moment of quiet is not that moment: its startup phases run in sequence with gaps of about 70 to 100 milliseconds between them, and a latch on the first zero-crossing fires around 3.2 seconds in, before indexing and before the first `cargo check`. Quiet therefore has to be sustained, not instantaneous. Task 5 implements it as a debounce with a deadline. The evidence is in the spec's measurement section.

---

### Task 1: Configuration entries merge onto the built-in servers

**Files:**
- Modify: `crates/mcpls-core/src/config/server.rs`
- Modify: `crates/mcpls-core/src/config/mod.rs`
- Modify: `crates/mcpls-core/tests/integration/basic_tests.rs`
- Modify: `crates/mcpls-core/tests/fixtures/empty_config.toml`, `crates/mcpls-core/tests/fixtures/configs/two_server_routing.toml`, `crates/mcpls-core/tests/fixtures/configs/mutually_exclusive_heuristics.toml`
- Modify: `docs/user-guide/configuration.md`, `skills/mcpls/references/configuration.md`, `examples/mcpls.toml`
- Test: `crates/mcpls-core/src/config/mod.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub struct PartialLspServerConfig` with every field optional plus `pub enabled: Option<bool>`
  - `LspServerConfig::builtins() -> Vec<LspServerConfig>`
  - `PartialLspServerConfig::id(&self) -> Option<ServerId>`
  - `pub fn resolve_lsp_servers(partials: Vec<PartialLspServerConfig>) -> Result<Vec<LspServerConfig>>`

Background the implementer needs:

- `LspServerConfig::id()` (`config/server.rs:269`) returns `name` when set and `language_id` otherwise. It is already the routing key across `Translator`'s maps, and `ToolRouter::from_configs` rejects duplicates that are applicable in the same workspace. That is the merge key; do not invent another.
- The six built-ins and their ids: `rust`, `python`, `typescript`, `go`, `cpp`, `zig` (`config/server.rs:301-377`).
- Two entries naming the same id is a **supported configuration**, not a mistake: `tests/fixtures/configs/mutually_exclusive_heuristics.toml` is the #174 §5 regression case, two servers for one language whose `heuristics.project_markers` never both match. So the fold must not collapse them and must not reject them. The rule below is that only the *first* entry claiming an id merges; a later one appends a second server.

- [ ] **Step 1: Write the failing tests**

Add to the inline test module in `crates/mcpls-core/src/config/mod.rs`:

```rust
#[test]
#[allow(clippy::expect_used)]
fn test_a_partial_entry_keeps_the_builtin_fields_it_omits() {
    let config: ServerConfig = toml::from_str(
        "[[lsp_servers]]\nlanguage_id = \"rust\"\nrequest_timeout_seconds = 60\n",
    )
    .expect("config parses");

    let rust = config
        .lsp_servers
        .iter()
        .find(|s| s.language_id == "rust")
        .expect("rust server survives the merge");
    assert_eq!(rust.request_timeout_seconds, 60);
    assert_eq!(rust.command, "rust-analyzer");
    assert_eq!(rust.file_patterns, vec!["**/*.rs".to_string()]);
    assert_eq!(
        config.lsp_servers.len(),
        LspServerConfig::builtins().len(),
        "the other built-ins are still there"
    );
}

#[test]
#[allow(clippy::expect_used)]
fn test_a_config_without_a_server_table_keeps_every_builtin() {
    let config: ServerConfig = toml::from_str("[workspace]\nroots = []\n")
        .expect("config parses");
    assert_eq!(config.lsp_servers.len(), LspServerConfig::builtins().len());
}

#[test]
#[allow(clippy::expect_used)]
fn test_disabling_a_builtin_removes_it() {
    let config: ServerConfig =
        toml::from_str("[[lsp_servers]]\nlanguage_id = \"python\"\nenabled = false\n")
            .expect("config parses");
    assert!(
        !config.lsp_servers.iter().any(|s| s.language_id == "python"),
        "python is gone"
    );
    assert!(config.lsp_servers.iter().any(|s| s.language_id == "rust"));
}

#[test]
#[allow(clippy::expect_used)]
fn test_overriding_the_command_drops_the_builtin_arguments() {
    // pyright's built-in carries `--stdio`, which means nothing to a
    // different binary. The file patterns are not the binary's, so they
    // survive: that is what distinguishes a merge from a replace here.
    let config: ServerConfig = toml::from_str(
        "[[lsp_servers]]\nlanguage_id = \"python\"\ncommand = \"ty\"\n",
    )
    .expect("config parses");

    let python = config
        .lsp_servers
        .iter()
        .find(|s| s.language_id == "python")
        .expect("python server survives");
    assert_eq!(python.command, "ty");
    assert!(
        python.args.is_empty(),
        "arguments belonged to the replaced binary, got {:?}",
        python.args
    );
    assert_eq!(
        python.file_patterns,
        vec!["**/*.py".to_string()],
        "file patterns are the language's, not the binary's, so they are inherited"
    );
}

#[test]
#[allow(clippy::expect_used)]
fn test_an_explicit_empty_argument_list_is_not_unspecified() {
    let config: ServerConfig = toml::from_str(
        "[[lsp_servers]]\nlanguage_id = \"python\"\nargs = []\n",
    )
    .expect("config parses");

    let python = config
        .lsp_servers
        .iter()
        .find(|s| s.language_id == "python")
        .expect("python server survives");
    assert_eq!(python.command, "pyright-langserver", "command is inherited");
    assert!(python.args.is_empty(), "an explicit empty list wins");
}

#[test]
#[allow(clippy::expect_used)]
fn test_a_second_entry_for_one_id_adds_a_server_instead_of_overwriting() {
    // Two servers for one language, distinguished by heuristics, is a
    // supported configuration. Folding the second onto the first would
    // silently delete one of them.
    let config: ServerConfig = toml::from_str(
        "[[lsp_servers]]\nlanguage_id = \"python\"\ncommand = \"pyright-langserver\"\n\n\
         [[lsp_servers]]\nlanguage_id = \"python\"\ncommand = \"pylsp\"\n",
    )
    .expect("config parses");

    let commands: Vec<&str> = config
        .lsp_servers
        .iter()
        .filter(|s| s.language_id == "python")
        .map(|s| s.command.as_str())
        .collect();
    assert_eq!(commands, vec!["pyright-langserver", "pylsp"]);
}

#[test]
#[allow(clippy::expect_used, clippy::unwrap_used)]
fn test_an_entry_matching_no_builtin_needs_a_command() {
    let result: std::result::Result<ServerConfig, _> =
        toml::from_str("[[lsp_servers]]\nlanguage_id = \"elixir\"\n");
    let message = result
        .expect_err("an unmatched entry without a command is rejected")
        .to_string();
    assert!(
        message.contains("elixir"),
        "the error names the id it could not find, got: {message}"
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib config::tests 2>&1 | tail -30`

Expected: compile errors on `LspServerConfig::builtins`, plus assertion failures showing one server where the test wants several.

`test_overriding_the_command_drops_the_builtin_arguments` and `test_a_second_entry_for_one_id_adds_a_server_instead_of_overwriting` will fail only on their inheritance assertions (`file_patterns`, and the second server's presence), because today's replace semantics already give an entry the command it wrote and an empty `args`. That is the correct RED for them: they fail on the merge, not on the parse.

- [ ] **Step 3: Add the built-in list and the partial record**

In `crates/mcpls-core/src/config/server.rs`, add next to the existing built-in constructors:

```rust
impl LspServerConfig {
    /// The servers mcpls spawns when a configuration does not say otherwise.
    #[must_use]
    pub fn builtins() -> Vec<Self> {
        vec![
            Self::rust_analyzer(),
            Self::pyright(),
            Self::typescript(),
            Self::gopls(),
            Self::clangd(),
            Self::zls(),
        ]
    }
}
```

Then the partial record, in the same file:

```rust
/// One `[[lsp_servers]]` entry as written in a configuration file.
///
/// Every field is optional because an entry modifies a built-in rather than
/// replacing it: what it omits, it inherits. See [`resolve_lsp_servers`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialLspServerConfig {
    /// Language identifier. Required unless `name` identifies the entry.
    #[serde(default)]
    pub language_id: Option<String>,
    /// Command to start the LSP server.
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments to pass to the command. An empty list is empty arguments,
    /// not an absent key.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Environment variables for the server process.
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    /// File patterns this server handles.
    #[serde(default)]
    pub file_patterns: Option<Vec<String>>,
    /// Server-specific initialization options. Replaces the built-in's
    /// value rather than merging into it.
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,
    /// Handshake timeout in seconds.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    /// Per-request timeout in seconds.
    #[serde(default)]
    pub request_timeout_seconds: Option<u64>,
    /// Spawn heuristics.
    #[serde(default)]
    pub heuristics: Option<ServerHeuristics>,
    /// Routing identity, defaulting to `language_id`.
    #[serde(default)]
    pub name: Option<String>,
    /// Tools this server handles.
    #[serde(default)]
    pub handles: Option<Vec<ToolKind>>,
    /// Set to `false` to drop the server this entry names.
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl PartialLspServerConfig {
    /// The routing identity this entry names, or `None` when it gives
    /// neither a `name` nor a `language_id` to derive one from.
    #[must_use]
    pub fn id(&self) -> Option<ServerId> {
        self.name
            .clone()
            .or_else(|| self.language_id.clone())
            .map(ServerId::from)
    }
}
```

`deny_unknown_fields` means this struct must list every field of `LspServerConfig` or a valid config stops parsing. `from_partial` below is an exhaustive struct literal with no `..`, so the compiler catches a missing one; if it errors on a field not listed here, add it in both places.

Deriving `Deserialize` only, not `Serialize`: nothing serializes a partial entry, and `skip_serializing_if` on a field that is never serialized is noise.

- [ ] **Step 4: Write the fold**

Still in `crates/mcpls-core/src/config/server.rs`. Its imports today are `HashMap`, `Path`, `WalkBuilder`, serde and `super::routing::{ServerId, ToolKind}`, so this step also adds `std::collections::HashSet` and `crate::error::{Error, Result}`:

```rust
/// Fold configuration entries onto the built-in servers.
///
/// An entry modifies the built-in sharing its [`LspServerConfig::id`]: what
/// the entry omits, it inherits. An entry naming no built-in defines a new
/// server, where nothing can be inherited and `command` is required. An
/// entry with `enabled = false` removes every server it names.
///
/// Only the first entry claiming an id merges. A second entry with the same
/// id appends another server, because two servers for one language,
/// separated by their spawn heuristics, is a configuration mcpls supports
/// (see `ToolRouter::from_configs`, which adjudicates the pair against the
/// workspace). Folding the second onto the first would delete one of them.
///
/// Overriding `command` drops the built-in's `args`, `env`, and
/// `initialization_options`, because those belong to the binary being
/// replaced: pyright's `--stdio` means nothing to a different program, and
/// inheriting it fails at spawn time rather than at load time. The file
/// patterns describe the language rather than the binary, so they survive.
///
/// # Errors
///
/// Returns [`Error::InvalidConfig`] when an entry names no id at all, or
/// when an entry matching no built-in gives no `command`.
pub fn resolve_lsp_servers(
    partials: Vec<PartialLspServerConfig>,
) -> Result<Vec<LspServerConfig>> {
    let mut resolved = LspServerConfig::builtins();
    let mut claimed: HashSet<ServerId> = HashSet::new();

    for partial in partials {
        let id = partial.id().ok_or_else(|| {
            Error::InvalidConfig(
                "an [[lsp_servers]] entry needs a language_id or a name".to_string(),
            )
        })?;

        if partial.enabled == Some(false) {
            resolved.retain(|server| server.id() != id);
            continue;
        }

        let existing = if claimed.contains(&id) {
            None
        } else {
            resolved.iter().position(|server| server.id() == id)
        };
        claimed.insert(id.clone());

        match existing {
            Some(index) => resolved[index].merge(partial),
            None => resolved.push(LspServerConfig::from_partial(&id, partial)?),
        }
    }

    Ok(resolved)
}

impl LspServerConfig {
    /// Overlay one configuration entry onto this server.
    fn merge(&mut self, partial: PartialLspServerConfig) {
        // A new binary does not want the old one's invocation.
        if let Some(command) = partial.command {
            self.command = command;
            self.args = Vec::new();
            self.env = HashMap::new();
            self.initialization_options = None;
        }
        if let Some(language_id) = partial.language_id {
            self.language_id = language_id;
        }
        if let Some(args) = partial.args {
            self.args = args;
        }
        if let Some(env) = partial.env {
            self.env = env;
        }
        if let Some(file_patterns) = partial.file_patterns {
            self.file_patterns = file_patterns;
        }
        if let Some(options) = partial.initialization_options {
            self.initialization_options = Some(options);
        }
        if let Some(timeout) = partial.timeout_seconds {
            self.timeout_seconds = timeout;
        }
        if let Some(timeout) = partial.request_timeout_seconds {
            self.request_timeout_seconds = timeout;
        }
        if let Some(heuristics) = partial.heuristics {
            self.heuristics = Some(heuristics);
        }
        if let Some(name) = partial.name {
            self.name = Some(name);
        }
        if let Some(handles) = partial.handles {
            self.handles = Some(handles);
        }
    }

    /// Build a server from an entry that matched no built-in.
    fn from_partial(id: &ServerId, partial: PartialLspServerConfig) -> Result<Self> {
        let command = partial.command.ok_or_else(|| {
            Error::InvalidConfig(format!(
                "[[lsp_servers]] entry '{id}' inherits from no built-in server, so it needs a command"
            ))
        })?;
        let language_id = partial.language_id.unwrap_or_else(|| id.to_string());

        Ok(Self {
            language_id,
            command,
            args: partial.args.unwrap_or_default(),
            env: partial.env.unwrap_or_default(),
            file_patterns: partial.file_patterns.unwrap_or_default(),
            initialization_options: partial.initialization_options,
            timeout_seconds: partial.timeout_seconds.unwrap_or_else(default_timeout),
            request_timeout_seconds: partial
                .request_timeout_seconds
                .unwrap_or_else(default_request_timeout),
            heuristics: partial.heuristics,
            name: partial.name,
            handles: partial.handles,
        })
    }
}
```

`default_timeout` and `default_request_timeout` are `const fn` returning `u64`; `unwrap_or_else` takes them as function items. If the compiler objects, use `unwrap_or(default_timeout())`.

- [ ] **Step 5: Wire it into the load path**

In `crates/mcpls-core/src/config/mod.rs`, change the `lsp_servers` field of `ServerConfig` to resolve at deserialization:

```rust
    /// LSP server configurations, resolved against the built-ins.
    #[serde(default = "LspServerConfig::builtins", deserialize_with = "deserialize_lsp_servers")]
    pub lsp_servers: Vec<LspServerConfig>,
```

`default` and `deserialize_with` coexist on one field: `default` supplies the value when the key is absent, `deserialize_with` runs when it is present.

Add, at module level in the same file:

```rust
/// Deserialize `[[lsp_servers]]` entries and fold them onto the built-ins.
fn deserialize_lsp_servers<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<LspServerConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let partials = Vec::<PartialLspServerConfig>::deserialize(deserializer)?;
    resolve_lsp_servers(partials).map_err(serde::de::Error::custom)
}
```

Export `PartialLspServerConfig` and `resolve_lsp_servers` from `config/mod.rs`'s `pub use` block alongside `LspServerConfig`.

Leave `ServerConfig::default()`'s explicit list in place but have it call `LspServerConfig::builtins()` so the two cannot drift.

- [ ] **Step 6: Fix the tests and fixtures this breaks**

This is the widest step in the plan. Work through it in order and run the suite at the end rather than after each file.

**Unit tests in `crates/mcpls-core/src/config/mod.rs`:**

- `test_load_from_valid_toml` (around `:975`) asserts `lsp_servers.len() == 1`. Rewrite: find the rust server by `language_id` and assert its `timeout_seconds`, then assert the total is `LspServerConfig::builtins().len()`.
- `test_load_multiple_servers` (around `:1577`) asserts len 2 and indexes positionally. Rewrite to look each server up by `language_id`, then assert the total.
- `test_empty_config_file` (around `:1600`) asserts the list is empty. It now expects `LspServerConfig::builtins().len()`.
- `test_load_with_trust_loads_trusted_project_local_config` (around `:2015`) asserts len 1. Same rewrite as the first.

**Integration tests in `crates/mcpls-core/tests/integration/basic_tests.rs`:**

- `:26` asserts len 1 for `minimal.toml`. Rewrite to find the rust server and assert the total is `LspServerConfig::builtins().len()`.
- `:39` asserts len 3 for `multi_language.toml` and indexes `[0] [1] [2]`. Rewrite to look up `rust`, `python` and `typescript` by `language_id` and assert the total.
- `:87` and `:114` are fixture-driven and stay as they are; the fixtures change instead, below.

**Fixtures.** Three fixtures exist to pin an exact, closed set of servers, and a merge that adds five more breaks what each one is for. Each gets `enabled = false` entries for the built-ins it does not want. Place a disable entry before any `[[lsp_servers]]` that carries an `[lsp_servers.heuristics]` sub-table, since TOML attaches such a table to the most recent array element.

`crates/mcpls-core/tests/fixtures/empty_config.toml` is the one that matters most: `tests/e2e/mcp_client.rs:132` spawns the binary against it precisely so no language server starts. After the merge it would start all six, and since the e2e working directory has a `Cargo.toml` and rust-analyzer is on PATH, every protocol test would begin indexing this repository. Rewrite it as:

```toml
# Protocol-only configuration for E2E testing.
#
# Every built-in server is disabled: these tests exercise the MCP layer, and
# spawning a language server here would make each of them wait on an index.

[workspace]
roots = []

[[lsp_servers]]
language_id = "rust"
enabled = false

[[lsp_servers]]
language_id = "python"
enabled = false

[[lsp_servers]]
language_id = "typescript"
enabled = false

[[lsp_servers]]
language_id = "go"
enabled = false

[[lsp_servers]]
language_id = "cpp"
enabled = false

[[lsp_servers]]
language_id = "zig"
enabled = false
```

`crates/mcpls-core/tests/fixtures/configs/two_server_routing.toml`: its two entries carry explicit `name`s (`pyright`, `pylsp`), so neither matches the built-in whose id is `python`, and that built-in would survive as a third python server and a second catch-all, which `ToolRouter::from_configs` rejects. Add the same six disable entries before the existing two.

`crates/mcpls-core/tests/fixtures/configs/mutually_exclusive_heuristics.toml`: its two entries both have id `python`, so under the fold the first merges onto the built-in and the second appends, leaving seven servers where the test wants two, and the built-in's own `pyrightconfig.json` marker would make it applicable alongside them. Add the six disable entries at the top, before the first `[[lsp_servers]]` with a heuristics sub-table.

`minimal.toml` and `multi_language.toml` need no change: their tests are what change.

**Documentation that describes replace semantics:**

- `docs/user-guide/configuration.md:237` ("# Only Rust and Python") and the surrounding server section.
- `skills/mcpls/references/configuration.md:17-30`, which marks `command` as required.
- `examples/mcpls.toml:67-110`, whose "uncomment as needed" guidance only makes sense under replace.

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib config 2>&1 | tail -20`

Expected: every config test passes, including the seven added in step 1.

- [ ] **Step 7: Check the whole workspace and the lints**

Run:
```bash
cargo fmt --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --all
cargo clippy --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --workspace --all-targets -- -D warnings
cargo build --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --workspace
cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --workspace
cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --test integration_tests e2e:: -- --include-ignored
```

The e2e set is the reason for the last line: every test in `tests/e2e/protocol_tests.rs` is `#[ignore]`, so a plain `cargo test` runs none of them and the `empty_config.toml` regression would pass unnoticed. The staleness guard added in `92b7f4c` refuses a binary older than its sources, hence the build first.

Expected: all clean.

- [ ] **Step 8: Commit**

```bash
git -C /home/lev/Git/lev/mcpls-diagnostics add crates/mcpls-core/src/config/ crates/mcpls-core/tests/ docs/user-guide/configuration.md skills/mcpls/references/configuration.md examples/mcpls.toml CHANGELOG.md
git -C /home/lev/Git/lev/mcpls-diagnostics commit -m "feat(config)!: merge server entries onto the built-ins" -m "Writing any [[lsp_servers]] block replaced the built-in list wholesale,
so reaching one per-server key cost every default server, and a config
file with no server table spawned nothing at all.

Entries are now partial records folded onto the built-ins by routing
identity: what an entry omits, it inherits. enabled = false removes a
server, replacing the disable-by-omission that replace semantics gave
for free. Overriding command drops the built-in's args, env and
initialization options, which belong to the binary being replaced.

Only the first entry claiming an id merges; a second one appends,
so two servers for one language separated by their spawn heuristics
still load as two servers.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: The `[diagnostics]` table and the per-server severity floor

**Files:**
- Modify: `crates/mcpls-core/src/config/mod.rs`
- Modify: `crates/mcpls-core/src/config/server.rs`
- Test: both files' inline `mod tests`

**Interfaces:**
- Consumes: `PartialLspServerConfig` from task 1.
- Produces:
  - `pub enum SeverityFloor { Off, Error, Warning, Information, Hint }` with `pub fn admits(self, severity: Option<lsp_types::DiagnosticSeverity>) -> bool`
  - `pub struct DiagnosticsConfig` with `severity`, `max_per_file`, `max_total`, `settle_quiet_ms`, `settle_deadline_ms`
  - `ServerConfig::diagnostics: DiagnosticsConfig`
  - `LspServerConfig::diagnostics_severity: Option<SeverityFloor>`

Background: in the pinned `lsp-types` 0.97, `DiagnosticSeverity` is `pub struct DiagnosticSeverity(i32)` with a private field and associated constants (`lsp-types-0.97.0/src/lib.rs:445-459`). It is not a C-like enum, so `severity as i32` is a `non-primitive cast` error. It derives `Ord`, and its constants run `ERROR = 1` through `HINT = 4`, so comparison alone gives the ordering.

Adding a field to `LspServerConfig` breaks every struct literal that builds one, and none of them use a `..` spread. There are 38 across the crate, concentrated in `lsp/lifecycle.rs` (13), with the rest spread over `config/routing.rs`, `config/mod.rs`, `config/server.rs`, `bridge/translator/{routing,respawn,symbols}.rs`, `lib.rs`, `tests/ra_e2e.rs` and `tests/integration/rust_analyzer_tests.rs`. Find them with `rg -n 'LspServerConfig \{' /home/lev/Git/lev/mcpls-diagnostics/crates/mcpls-core` and expect the compiler to name each one.

- [ ] **Step 1: Write the failing tests**

In `crates/mcpls-core/src/config/mod.rs`'s test module:

```rust
#[test]
#[allow(clippy::expect_used)]
fn test_diagnostics_defaults_to_a_warning_floor() {
    let config: ServerConfig = toml::from_str("").expect("empty config parses");
    assert_eq!(config.diagnostics.severity, SeverityFloor::Warning);
    assert_eq!(config.diagnostics.max_per_file, 10);
    assert_eq!(config.diagnostics.max_total, 50);
}

#[test]
#[allow(clippy::expect_used)]
fn test_a_server_can_raise_its_own_floor() {
    let config: ServerConfig = toml::from_str(
        "[diagnostics]\nseverity = \"hint\"\n\n[[lsp_servers]]\nlanguage_id = \"rust\"\ndiagnostics_severity = \"error\"\n",
    )
    .expect("config parses");

    let rust = config
        .lsp_servers
        .iter()
        .find(|s| s.language_id == "rust")
        .expect("rust server survives");
    assert_eq!(rust.diagnostics_severity, Some(SeverityFloor::Error));
    assert_eq!(config.diagnostics.severity, SeverityFloor::Hint);
}

#[test]
fn test_off_admits_nothing_and_hint_admits_everything() {
    use lsp_types::DiagnosticSeverity;
    assert!(!SeverityFloor::Off.admits(Some(DiagnosticSeverity::ERROR)));
    assert!(SeverityFloor::Hint.admits(Some(DiagnosticSeverity::HINT)));
    assert!(SeverityFloor::Error.admits(Some(DiagnosticSeverity::ERROR)));
    assert!(!SeverityFloor::Error.admits(Some(DiagnosticSeverity::WARNING)));
    assert!(SeverityFloor::Warning.admits(Some(DiagnosticSeverity::ERROR)));
    assert!(!SeverityFloor::Warning.admits(Some(DiagnosticSeverity::INFORMATION)));
}

#[test]
fn test_a_diagnostic_without_a_severity_is_admitted_unless_muted() {
    assert!(SeverityFloor::Error.admits(None));
    assert!(!SeverityFloor::Off.admits(None));
}
```

The last test states a rule worth being explicit about: LSP makes `severity` optional, and a server that omits it is not thereby saying the diagnostic is unimportant. Anything but `off` lets it through.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib config::tests 2>&1 | tail -20`

Expected: compile errors on `SeverityFloor` and `config.diagnostics`.

- [ ] **Step 3: Add the types**

In `crates/mcpls-core/src/config/mod.rs`:

```rust
/// The least severe diagnostic worth delivering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeverityFloor {
    /// Deliver nothing from this server.
    Off,
    /// Errors only.
    Error,
    /// Errors and warnings.
    Warning,
    /// Everything but hints.
    Information,
    /// Everything.
    Hint,
}

impl SeverityFloor {
    /// Whether a diagnostic of this severity clears the floor.
    ///
    /// A diagnostic with no severity clears every floor but [`Self::Off`]:
    /// the LSP field is optional, and a server omitting it is not saying the
    /// diagnostic does not matter.
    #[must_use]
    pub fn admits(self, severity: Option<lsp_types::DiagnosticSeverity>) -> bool {
        use lsp_types::DiagnosticSeverity;

        // DiagnosticSeverity orders ERROR = 1 through HINT = 4 and derives
        // Ord, so "at least as severe as the floor" is `<=`.
        let deepest = match self {
            Self::Off => return false,
            Self::Error => DiagnosticSeverity::ERROR,
            Self::Warning => DiagnosticSeverity::WARNING,
            Self::Information => DiagnosticSeverity::INFORMATION,
            Self::Hint => DiagnosticSeverity::HINT,
        };
        severity.is_none_or(|severity| severity <= deepest)
    }
}

/// How much of what the language servers report reaches the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsConfig {
    /// The least severe diagnostic worth delivering, for any server that
    /// does not set its own.
    #[serde(default = "default_severity_floor")]
    pub severity: SeverityFloor,
    /// Most diagnostics delivered for one file in one flush.
    #[serde(default = "default_max_per_file")]
    pub max_per_file: usize,
    /// Most diagnostics delivered in one flush across every file. This is a
    /// context budget, which is why it is not per server.
    #[serde(default = "default_max_total")]
    pub max_total: usize,
    /// How long the language servers must report no work before their view
    /// of the workspace counts as complete.
    ///
    /// Raise it on a workspace whose servers pause mid-analysis for longer
    /// than this; the cost of raising it is a later baseline, and the cost
    /// of setting it too low is a baseline taken mid-index.
    #[serde(default = "default_settle_quiet_ms")]
    pub settle_quiet_ms: u64,
    /// How long to wait for that quiet before giving up and baselining
    /// anyway. Bounds the damage from a server that never finishes, or from
    /// a progress notification dropped before its pump existed.
    #[serde(default = "default_settle_deadline_ms")]
    pub settle_deadline_ms: u64,
}

const fn default_severity_floor() -> SeverityFloor {
    SeverityFloor::Warning
}

const fn default_max_per_file() -> usize {
    10
}

const fn default_max_total() -> usize {
    50
}

/// rust-analyzer's startup phases leave gaps of roughly 70 to 100
/// milliseconds between them. A second is an order of magnitude clear of
/// that and still well inside a session's first tool call.
const fn default_settle_quiet_ms() -> u64 {
    1_000
}

const fn default_settle_deadline_ms() -> u64 {
    300_000
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            severity: default_severity_floor(),
            max_per_file: default_max_per_file(),
            max_total: default_max_total(),
            settle_quiet_ms: default_settle_quiet_ms(),
            settle_deadline_ms: default_settle_deadline_ms(),
        }
    }
}
```

Add the field to `ServerConfig` next to `apply`:

```rust
    /// How much of what the language servers report reaches the agent.
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,
```

and to `ServerConfig::default()`.

Add `diagnostics_severity: Option<SeverityFloor>` to `LspServerConfig` (as `#[serde(default, skip_serializing_if = "Option::is_none")]`) and to `PartialLspServerConfig` (as `#[serde(default)]`), extend `LspServerConfig::merge` and `from_partial` to carry it, and set it to `None` in `LspServerConfig::builtin`. Then work through the 38 struct literals the compiler names.

- [ ] **Step 4: Run the tests**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib config 2>&1 | tail -20`

Expected: PASS. `test_apply_defaults_to_read_only` and the round-trip tests still pass, since every new field has a default.

- [ ] **Step 5: Verify and commit**

Run fmt, clippy and the full command set from task 1 step 7. Document the `[diagnostics]` table in `docs/user-guide/configuration.md` with a section per key, and add a `### Added` entry to the changelog.

```bash
git -C /home/lev/Git/lev/mcpls-diagnostics add crates/mcpls-core/src/ crates/mcpls-core/tests/ docs/user-guide/configuration.md CHANGELOG.md
git -C /home/lev/Git/lev/mcpls-diagnostics commit -m "feat(config): add the diagnostics table" -m "One severity floor per server, with a global default, plus two volume
caps that stay global because they bound one shared context budget
rather than one server's output. off mutes a server, which is how a
language is excluded.

The two settle durations decide when a workspace counts as analyzed.
They are configurable because the right quiet period depends on how
long the slowest server pauses mid-analysis.

Nothing reads these yet.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: The deduplication core

**Files:**
- Create: `crates/mcpls-core/src/bridge/delivery.rs`
- Modify: `crates/mcpls-core/src/bridge/mod.rs` (declare and re-export the module)
- Test: inline `mod tests` in `delivery.rs`

**Interfaces:**
- Consumes: `DiagnosticsConfig`, `SeverityFloor` from task 2.
- Produces:
  - `pub struct SessionId(String)` with `impl From<String>` and `pub fn process_default() -> Self`
  - `pub struct DiagnosticsDelivery` with `pub fn new(config: DiagnosticsConfig) -> Self`, `flush`, `set_baseline`, `has_baseline`, `visible_hash`
  - `pub struct FileEntry<'a> { pub key: &'a str, pub diagnostics: &'a [lsp_types::Diagnostic], pub floor: SeverityFloor }`
  - `pub struct FlushReport { pub changed: Vec<ChangedFile>, pub cleared: Vec<String>, pub omitted: usize }`
  - `pub struct ChangedFile { pub key: String, pub diagnostics: Vec<lsp_types::Diagnostic>, pub omitted: usize }`
  - `pub struct FloorTable` with `pub fn new(config: &DiagnosticsConfig, servers: &[LspServerConfig]) -> Self` and `pub fn for_server(&self, server: &ServerId) -> SeverityFloor`

This module knows nothing about URIs, paths, position encodings or locks. It speaks in opaque cache keys, and the caller maps those back to something an agent can read. That is what makes it testable without a language server, and it is why `ChangedFile` carries a key rather than a path.

- [ ] **Step 1: Create the module and write the failing tests**

Create `crates/mcpls-core/src/bridge/delivery.rs` containing only the test module below, and in the same step add `mod delivery;` to `crates/mcpls-core/src/bridge/mod.rs`. Without the module declaration the file is never compiled and step 2 passes for the wrong reason.

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

    use super::*;
    use crate::config::{DiagnosticsConfig, SeverityFloor};

    fn diagnostic(line: u32, severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position::new(line, 0),
                end: Position::new(line, 1),
            },
            severity: Some(severity),
            message: message.to_string(),
            ..Default::default()
        }
    }

    fn entry<'a>(
        key: &'a str,
        diagnostics: &'a [Diagnostic],
        floor: SeverityFloor,
    ) -> FileEntry<'a> {
        FileEntry {
            key,
            diagnostics,
            floor,
        }
    }

    #[test]
    fn test_a_changed_file_is_returned_once_and_not_twice() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];

        let first = delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Warning)]);
        assert_eq!(first.changed.len(), 1);

        let second = delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Warning)]);
        assert!(second.changed.is_empty(), "nothing changed since the last flush");
    }

    #[test]
    fn test_a_file_losing_its_diagnostics_is_reported_once() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Warning)]);

        let fixed = delivery.flush(&session, &[entry("a.rs", &[], SeverityFloor::Warning)]);
        assert_eq!(fixed.cleared, vec!["a.rs".to_string()]);

        let again = delivery.flush(&session, &[entry("a.rs", &[], SeverityFloor::Warning)]);
        assert!(again.cleared.is_empty(), "cleared is not re-reported");
    }

    #[test]
    fn test_sub_floor_churn_does_not_report_a_file_with_nothing_to_show() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let errors = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        delivery.flush(&session, &[entry("a.rs", &errors, SeverityFloor::Warning)]);

        let mut with_hint = errors.clone();
        with_hint.push(diagnostic(9, DiagnosticSeverity::HINT, "consider"));
        let after = delivery.flush(&session, &[entry("a.rs", &with_hint, SeverityFloor::Warning)]);
        assert!(
            after.changed.is_empty(),
            "the hint is below the floor, so nothing visible changed"
        );
    }

    #[test]
    fn test_the_hash_ignores_publish_order() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let a = diagnostic(1, DiagnosticSeverity::ERROR, "one");
        let b = diagnostic(2, DiagnosticSeverity::WARNING, "two");

        delivery.flush(
            &session,
            &[entry("a.rs", &[a.clone(), b.clone()], SeverityFloor::Warning)],
        );
        let reordered = delivery.flush(&session, &[entry("a.rs", &[b, a], SeverityFloor::Warning)]);
        assert!(reordered.changed.is_empty(), "order is not content");
    }

    #[test]
    fn test_a_capped_file_says_how_many_it_dropped() {
        let config = DiagnosticsConfig {
            max_per_file: 2,
            ..DiagnosticsConfig::default()
        };
        let mut delivery = DiagnosticsDelivery::new(config);
        let session = SessionId::from("s".to_string());
        let diags: Vec<_> = (0..5)
            .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, "boom"))
            .collect();

        let report = delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Warning)]);
        assert_eq!(report.changed[0].diagnostics.len(), 2);
        assert_eq!(report.changed[0].omitted, 3);
    }

    #[test]
    fn test_a_file_dropped_by_the_total_cap_is_still_pending() {
        let config = DiagnosticsConfig {
            max_total: 1,
            ..DiagnosticsConfig::default()
        };
        let mut delivery = DiagnosticsDelivery::new(config);
        let session = SessionId::from("s".to_string());
        let a = vec![diagnostic(1, DiagnosticSeverity::ERROR, "a")];
        let b = vec![diagnostic(1, DiagnosticSeverity::ERROR, "b")];

        let first = delivery.flush(
            &session,
            &[
                entry("a.rs", &a, SeverityFloor::Warning),
                entry("b.rs", &b, SeverityFloor::Warning),
            ],
        );
        assert_eq!(first.changed.len(), 1);
        assert_eq!(first.omitted, 1);

        let second = delivery.flush(
            &session,
            &[
                entry("a.rs", &a, SeverityFloor::Warning),
                entry("b.rs", &b, SeverityFloor::Warning),
            ],
        );
        assert_eq!(
            second.changed.len(),
            1,
            "the file the cap dropped is delivered next time, not swallowed"
        );
    }

    #[test]
    fn test_a_muted_server_delivers_nothing() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];

        let report = delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Off)]);
        assert!(report.changed.is_empty());
        assert!(report.cleared.is_empty(), "muted is not the same as fixed");
    }

    #[test]
    fn test_two_sessions_deduplicate_independently() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        let one = SessionId::from("one".to_string());
        let two = SessionId::from("two".to_string());

        delivery.flush(&one, &[entry("a.rs", &diags, SeverityFloor::Warning)]);
        let other = delivery.flush(&two, &[entry("a.rs", &diags, SeverityFloor::Warning)]);
        assert_eq!(other.changed.len(), 1, "a second session has its own record");
    }

    #[test]
    fn test_a_session_starting_after_the_baseline_ignores_what_it_recorded() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "pre-existing")];

        let baseline = std::collections::HashMap::from([(
            "a.rs".to_string(),
            DiagnosticsDelivery::visible_hash(&diags, SeverityFloor::Warning).unwrap(),
        )]);
        delivery.set_baseline(baseline);

        let report = delivery.flush(
            &SessionId::from("s".to_string()),
            &[entry("a.rs", &diags, SeverityFloor::Warning)],
        );
        assert!(
            report.changed.is_empty(),
            "the workspace already had this before the session started"
        );
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib bridge::delivery 2>&1 | tail -20`

Expected: compile failure, unresolved `DiagnosticsDelivery`, `SessionId`, `FileEntry`.

- [ ] **Step 3: Implement the module**

Above the test module in `crates/mcpls-core/src/bridge/delivery.rs`:

```rust
//! Per-session deduplication of language server diagnostics.
//!
//! A flush answers one question: what is different since this session last
//! asked? The record is a hash per file rather than a set of individual
//! diagnostics, because when a file breaks, its full current error list is
//! more useful than a delta against a list that has left the context window.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::config::{DiagnosticsConfig, LspServerConfig, ServerId, SeverityFloor};

/// Identity of one client session.
///
/// Claude Code exports `CLAUDE_CODE_SESSION_ID` into the environment of the
/// stdio MCP servers it spawns, so an in-process flush and a flush arriving
/// later over a socket name the same record.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl SessionId {
    /// The session this process serves when the host names none.
    ///
    /// Correct for stdio, where the host spawns one mcpls per client.
    #[must_use]
    pub fn process_default() -> Self {
        std::env::var("CLAUDE_CODE_SESSION_ID")
            .map_or_else(|_| Self("local".to_string()), Self)
    }
}

/// One file's current diagnostics, as the caller assembled them.
#[derive(Debug)]
pub struct FileEntry<'a> {
    /// Cache key identifying the file, stable across publishes.
    pub key: &'a str,
    /// Everything the owning server currently reports for this file.
    pub diagnostics: &'a [lsp_types::Diagnostic],
    /// The floor this file's server answers to.
    pub floor: SeverityFloor,
}

/// One file that changed since the last flush.
#[derive(Debug, Clone)]
pub struct ChangedFile {
    /// Cache key, matching the [`FileEntry`] this came from.
    pub key: String,
    /// Diagnostics at or above the floor, capped.
    pub diagnostics: Vec<lsp_types::Diagnostic>,
    /// How many this file's own cap dropped. Those are not offered again:
    /// the count is what tells the agent to look at the file itself.
    pub omitted: usize,
}

/// What one flush found.
#[derive(Debug, Clone, Default)]
pub struct FlushReport {
    /// Files whose visible diagnostics differ from the last flush.
    pub changed: Vec<ChangedFile>,
    /// Cache keys of files that had visible diagnostics and now have none.
    pub cleared: Vec<String>,
    /// Files the total cap held back entirely. They are not recorded as
    /// delivered, so the next flush offers them again.
    pub omitted: usize,
}

/// Per-session records of what has already been delivered.
#[derive(Debug)]
pub struct DiagnosticsDelivery {
    config: DiagnosticsConfig,
    sessions: HashMap<SessionId, HashMap<String, u64>>,
    baseline: Option<HashMap<String, u64>>,
}

impl DiagnosticsDelivery {
    /// Build a delivery core answering to `config`.
    #[must_use]
    pub fn new(config: DiagnosticsConfig) -> Self {
        Self {
            config,
            sessions: HashMap::new(),
            baseline: None,
        }
    }

    /// Adopt `baseline` as what every future session starts out believing.
    ///
    /// Taken once the workspace's servers have gone quiet. Without it the
    /// first flush of a session reports every warning the workspace already
    /// had, which is never what the agent asked for.
    pub fn set_baseline(&mut self, baseline: HashMap<String, u64>) {
        self.baseline = Some(baseline);
    }

    /// Whether a baseline has been adopted yet.
    #[must_use]
    pub const fn has_baseline(&self) -> bool {
        self.baseline.is_some()
    }

    /// Hash one file's visible diagnostics.
    ///
    /// Order-insensitive, because `cap_diagnostics_entry_size` re-sorts
    /// survivors by severity when it truncates and a resort is not a change.
    /// Computed over the set that clears the floor, so raising a hint on a
    /// file whose floor is `error` reports nothing.
    ///
    /// `None` means the file has nothing visible at all.
    #[must_use]
    pub fn visible_hash(
        diagnostics: &[lsp_types::Diagnostic],
        floor: SeverityFloor,
    ) -> Option<u64> {
        let mut parts: Vec<String> = diagnostics
            .iter()
            .filter(|d| floor.admits(d.severity))
            .map(|d| {
                format!(
                    "{}:{}:{}:{}:{:?}:{}",
                    d.range.start.line,
                    d.range.start.character,
                    d.range.end.line,
                    d.range.end.character,
                    d.severity,
                    d.message
                )
            })
            .collect();
        if parts.is_empty() {
            return None;
        }
        parts.sort_unstable();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        parts.hash(&mut hasher);
        Some(hasher.finish())
    }

    /// Report what changed for `session` since its last flush.
    ///
    /// A zero `max_per_file` or `max_total` means that cap is unlimited,
    /// matching `workspace.max_documents`/`max_file_size`'s convention:
    /// there is no other way to write "no limit", and a literal zero cap
    /// has no sensible reading (`severity = "off"` already covers "deliver
    /// nothing"). Running low on a finite total budget defers a whole file
    /// to the next flush rather than truncating it further: a partially
    /// delivered file reads as the complete picture, which is worse than
    /// waiting. The one exception is a file that does not fit even a
    /// fresh, untouched budget — no later flush would do better either, so
    /// that file is delivered truncated to the budget instead of withheld
    /// forever.
    pub fn flush(&mut self, session: &SessionId, entries: &[FileEntry<'_>]) -> FlushReport {
        let record = self
            .sessions
            .entry(session.clone())
            .or_insert_with(|| self.baseline.clone().unwrap_or_default());

        let mut report = FlushReport::default();
        let mut budget = (self.config.max_total > 0).then_some(self.config.max_total);
        let mut delivered_any = false;

        for entry in entries {
            let hash = Self::visible_hash(entry.diagnostics, entry.floor);
            let previous = record.get(entry.key).copied();

            match (hash, previous) {
                (None, Some(_)) => {
                    record.remove(entry.key);
                    report.cleared.push(entry.key.to_string());
                }
                (None, None) => {}
                (Some(current), Some(before)) if current == before => {}
                (Some(current), _) => {
                    let mut visible: Vec<_> = entry
                        .diagnostics
                        .iter()
                        .filter(|d| entry.floor.admits(d.severity))
                        .cloned()
                        .collect();
                    let per_file_omitted = if self.config.max_per_file == 0 {
                        0
                    } else {
                        let dropped = visible.len().saturating_sub(self.config.max_per_file);
                        visible.truncate(self.config.max_per_file);
                        dropped
                    };

                    let budget_omitted = match budget {
                        None => Some(0),
                        Some(remaining) if visible.len() <= remaining => {
                            budget = Some(remaining - visible.len());
                            Some(0)
                        }
                        Some(remaining) if !delivered_any => {
                            let shortfall = visible.len() - remaining;
                            visible.truncate(remaining);
                            budget = Some(0);
                            Some(shortfall)
                        }
                        Some(_) => None,
                    };

                    let Some(budget_omitted) = budget_omitted else {
                        report.omitted += 1;
                        continue;
                    };

                    delivered_any = true;
                    record.insert(entry.key.to_string(), current);
                    report.changed.push(ChangedFile {
                        key: entry.key.to_string(),
                        diagnostics: visible,
                        omitted: per_file_omitted + budget_omitted,
                    });
                }
            }
        }

        report
    }
}
```

Then the floor lookup, in the same file, because a floor is part of deciding what a file's visible diagnostics are:

```rust
/// The severity floor each server answers to.
///
/// Resolved once at startup, because a server's floor comes from
/// configuration and configuration does not change while the process runs.
#[derive(Debug)]
pub struct FloorTable {
    default: SeverityFloor,
    by_server: HashMap<ServerId, SeverityFloor>,
}

impl FloorTable {
    /// Build the table from a resolved configuration.
    #[must_use]
    pub fn new(config: &DiagnosticsConfig, servers: &[LspServerConfig]) -> Self {
        Self {
            default: config.severity,
            by_server: servers
                .iter()
                .filter_map(|s| s.diagnostics_severity.map(|floor| (s.id(), floor)))
                .collect(),
        }
    }

    /// The floor for `server`.
    #[must_use]
    pub fn for_server(&self, server: &ServerId) -> SeverityFloor {
        self.by_server.get(server).copied().unwrap_or(self.default)
    }
}
```

Add a `pub use delivery::{...};` to `crates/mcpls-core/src/bridge/mod.rs` matching how neighbouring modules are re-exported.

- [ ] **Step 4: Run the tests**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib bridge::delivery 2>&1 | tail -20`

Expected: all nine pass.

- [ ] **Step 5: Verify and commit**

Run fmt, clippy and the workspace suite.

```bash
git -C /home/lev/Git/lev/mcpls-diagnostics add crates/mcpls-core/src/bridge/
git -C /home/lev/Git/lev/mcpls-diagnostics commit -m "feat(bridge): add the diagnostics delivery core" -m "One record per session mapping each file to a hash of the diagnostics
that clear its floor, so a flush returns what changed rather than
everything known. The hash is order-insensitive because truncation
re-sorts survivors by severity, and a resort is not a change.

A file the total budget cannot fit keeps its old record, so the next
flush offers it again instead of swallowing it. A file truncated by its
own cap is recorded and reports the count it dropped.

Nothing calls this yet.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: The pump drops publishes describing text that is gone

**Files:**
- Modify: `crates/mcpls-core/src/lib.rs` (`PumpShared` at `:120`, `diagnostics_pump` at `:154`, `spawn_lsp_servers_background` at `:911`, `serve_with` at `:557`)
- Test: the existing `diagnostics_pump` test module in `crates/mcpls-core/src/lib.rs:2194`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `PumpShared::document_tracker: Arc<DocumentTracker>`, and a `PumpShared` built in `serve_with` and passed into `spawn_lsp_servers_background`, which task 5 extends.

Background:

- `DocumentTracker::get(&Path) -> Option<DocumentState>` (`bridge/state.rs:321`) and `DocumentState::version() -> i32` (`:162`) are both public and re-exported from `bridge`. `Translator::document_tracker()` is `pub(crate)` and reachable from `lib.rs`. The tracker's map is a `StdMutex` held only for short synchronous sections (`bridge/state.rs:280-286`), so a lookup from the pump cannot stall it. `DocumentTracker::open` touches no disk, so a test can open a path that does not exist.
- `PumpShared` is built at `lib.rs:980`, inside `spawn_lsp_servers_background`, not in `serve_with`. That function already takes exactly 7 parameters, which is where `too_many_arguments` fires, so task 5 cannot add its three handles to it. This task therefore also collapses the signature: build `PumpShared` in `serve_with` and pass it in, replacing `notification_cache`, `subscriptions`, `peer_cell` and `workspace_roots`. That leaves four parameters and gives task 5 somewhere to put its fields. Inside the function, reach the cache through `shared.notification_cache` for the `set_diagnostics_route_count` call.

- [ ] **Step 1: Write the failing tests**

In the `diagnostics_pump` test module in `crates/mcpls-core/src/lib.rs`, following the construction the neighbouring tests already use:

```rust
#[tokio::test]
async fn test_a_publish_below_the_tracked_version_is_dropped() {
    // A late publish describing pre-edit text must not overwrite the
    // current entry: the agent would be shown errors for text that no
    // longer exists, and the next publish would flip them back.
}

#[tokio::test]
async fn test_a_publish_without_a_version_is_kept() {
    // Servers publish for files they were never told to open, and those
    // fanout files are what this feature exists to deliver.
}

#[tokio::test]
async fn test_a_publish_for_an_untracked_path_is_kept() {
    // Same reason: no tracked entry means no evidence of staleness, not
    // evidence of it.
}
```

Fill each body following the pattern of the existing pump tests: build a `PumpShared` with a fresh `NotificationCache` and a `DocumentTracker`, open and bump a document to a known version in the tracked cases, send an `LspNotification::PublishDiagnostics` down the channel, let the pump run, then assert on `cache.get_diagnostics(uri)`. Read the neighbouring tests first and match their setup helpers rather than inventing new ones.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib diagnostics_pump 2>&1 | tail -20`

Expected: the first test fails because the stale publish is stored. The other two pass before the change and must still pass after it; that is why they are written now.

- [ ] **Step 3: Add the tracker handle and the check**

Add to `PumpShared`:

```rust
    /// Used to drop publishes describing text an edit has already replaced.
    pub(crate) document_tracker: Arc<DocumentTracker>,
```

destructure it alongside the other fields, and in the `PublishDiagnostics` arm, after the workspace check and before the cache write:

```rust
                        if let Some(version) = p.version
                            && let Some(path) = bridge::uri_to_path(&p.uri)
                            && let Some(state) = document_tracker.get(&path)
                            && state.version() > version
                        {
                            debug!(
                                "dropping diagnostics for {} at version {version}, tracker holds {}",
                                p.uri.as_str(),
                                state.version()
                            );
                            continue;
                        }
```

Let-chains are stable in edition 2024 and this crate already uses one (`config/server.rs:121`); if clippy objects to the shape, use nested `if let` rather than restructuring the condition.

- [ ] **Step 4: Collapse the background spawn's parameters**

Change `spawn_lsp_servers_background` to take `(applicable_configs, translator, cancel_rx, shared: PumpShared)` and delete its construction of `PumpShared`. Build the value in `serve_with` instead, where `translator.document_tracker()` is in scope, and clone it into the call. Update the five test call sites that build a `PumpShared` (`lib.rs:2242, 2314, 2387, 2417, 2467`).

- [ ] **Step 5: Run the tests**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib diagnostics_pump 2>&1 | tail -20`

Expected: all three pass, and no existing pump test regresses. Then run fmt, clippy and the full command set from task 1 step 7.

- [ ] **Step 6: Commit**

```bash
git -C /home/lev/Git/lev/mcpls-diagnostics add crates/mcpls-core/src/lib.rs
git -C /home/lev/Git/lev/mcpls-diagnostics commit -m "fix(bridge): drop diagnostics for text an edit replaced" -m "A publish arriving after an edit, describing the version before it,
overwrote the current cache entry. The agent was shown errors for text
that no longer existed, and the next publish flipped them back.

A publish naming a version below the tracked one is now dropped. A
publish with no version, or for a path with no tracked entry, is kept:
those are the files a server reports without being asked, and they are
the ones worth delivering.

The background spawn takes the shared pump state as one value rather
than four parameters, which is what leaves room to add to it.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: The baseline is taken once the servers have stayed quiet

**Files:**
- Create: `crates/mcpls-core/src/bridge/settle.rs`
- Modify: `crates/mcpls-core/src/bridge/mod.rs`, `crates/mcpls-core/src/bridge/notifications.rs`, `crates/mcpls-core/src/lib.rs`
- Test: inline `mod tests` in `settle.rs`, plus one pump test in `lib.rs`

**Interfaces:**
- Consumes: `DiagnosticsDelivery`, `FloorTable` from task 3; `PumpShared` from task 4.
- Produces:
  - `pub struct ServerSettle` with `new(quiet_for: Duration, deadline_after: Duration)`, `restart_deadline`, `begin`, `end`, `should_settle_at(&self, now: Instant) -> bool`, `should_settle(&self) -> bool`
  - `NotificationCache::diagnostics_snapshot(&self) -> Vec<(String, DiagnosticInfo, ServerId)>`
  - `PumpShared` gains `settle`, `delivery` and `floors`, which task 6 shares into `BridgeContext`

Why this exists, and why the obvious version is wrong. Language servers spawn in the background and rust-analyzer publishes for the whole workspace once its initial analysis finishes, well after the first session op. A baseline taken on first sight is therefore empty, and every publish after it looks new. But a baseline taken the first time the outstanding `$/progress` count reaches zero is just as wrong, and this is measured, not guessed: against rust-analyzer 1.98.1 the count crosses zero at roughly 3.21s, 3.34s, 3.40s and 4.11s while `Fetching`, `Building CrateGraph`, `Roots Scanned` and `Loading proc-macros` hand off to each other. Indexing does not finish until 6.52s and the first `cargo check` not until 6.73s. A first-crossing latch fires before a single diagnostic exists.

So quiet has to be sustained. The gaps between phases run about 70 to 100 milliseconds; a one-second debounce clears them by an order of magnitude. Two further hazards, both bounded by the deadline rather than solved: notification channels hold 64 items and drop on `try_send` (`lsp/lifecycle.rs:337`, `lsp/client.rs:673`), and the pumps do not exist until every server has registered, so a `begin` or an `end` from early startup can be lost; and a server that reports no progress at all never becomes quiet because it was never busy.

- [ ] **Step 1: Create the module and write the failing tests**

Create `crates/mcpls-core/src/bridge/settle.rs` with the test module below, and add `mod settle;` to `crates/mcpls-core/src/bridge/mod.rs` in the same step.

The tests take `now` as an argument rather than sleeping. That keeps them deterministic and keeps a one-second debounce from costing a second of test time.

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;

    fn settle() -> ServerSettle {
        ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600))
    }

    #[test]
    fn test_a_server_with_work_outstanding_is_not_quiet() {
        let settle = settle();
        let rust = ServerId::from("rust");
        settle.begin(&rust, &json!("rustAnalyzer/cachePriming"));
        assert!(!settle.should_settle_at(Instant::now() + Duration::from_secs(5)));
    }

    #[test]
    fn test_quiet_must_outlast_the_gap_between_two_startup_phases() {
        let settle = settle();
        let rust = ServerId::from("rust");
        settle.begin(&rust, &json!("rustAnalyzer/Fetching"));
        settle.end(&rust, &json!("rustAnalyzer/Fetching"));
        let quiet_began = Instant::now();

        assert!(
            !settle.should_settle_at(quiet_began + Duration::from_millis(100)),
            "rust-analyzer hands off between startup phases in about this long, \
             and indexing has not started yet"
        );
        assert!(settle.should_settle_at(quiet_began + Duration::from_secs(2)));
    }

    #[test]
    fn test_work_resuming_restarts_the_quiet_period() {
        let settle = settle();
        let rust = ServerId::from("rust");
        settle.begin(&rust, &json!("rustAnalyzer/Indexing"));
        settle.end(&rust, &json!("rustAnalyzer/Indexing"));
        settle.begin(&rust, &json!("rust-analyzer/flycheck/0"));
        assert!(!settle.should_settle_at(Instant::now() + Duration::from_secs(5)));
    }

    #[test]
    fn test_every_server_must_be_quiet_at_once() {
        let settle = settle();
        let rust = ServerId::from("rust");
        let python = ServerId::from("python");
        settle.begin(&rust, &json!("indexing"));
        settle.begin(&python, &json!("analyzing"));
        settle.end(&rust, &json!("indexing"));

        assert!(!settle.should_settle_at(Instant::now() + Duration::from_secs(5)));
        settle.end(&python, &json!("analyzing"));
        assert!(settle.should_settle_at(Instant::now() + Duration::from_secs(5)));
    }

    #[test]
    fn test_an_end_without_a_begin_does_not_start_a_quiet_period() {
        let settle = settle();
        settle.end(&ServerId::from("rust"), &json!("orphan"));
        assert!(
            !settle.should_settle_at(Instant::now() + Duration::from_secs(5)),
            "an unmatched end proves nothing about what is still running"
        );
    }

    #[test]
    fn test_a_server_that_never_reports_progress_settles_on_the_deadline() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(60));
        assert!(!settle.should_settle_at(Instant::now() + Duration::from_secs(30)));
        assert!(
            settle.should_settle_at(Instant::now() + Duration::from_secs(61)),
            "a baseline taken late beats never reporting anything as new"
        );
    }

    #[test]
    fn test_the_deadline_fires_even_with_work_still_outstanding() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(60));
        settle.begin(&ServerId::from("rust"), &json!("stuck"));
        assert!(settle.should_settle_at(Instant::now() + Duration::from_secs(61)));
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib bridge::settle 2>&1 | tail -20`

Expected: unresolved `ServerSettle`.

- [ ] **Step 3: Implement it**

Above the tests in `settle.rs`:

```rust
//! Tracks whether the language servers have finished their startup work.
//!
//! Every server reports long-running work through `$/progress`. Quiet is not
//! the first moment the outstanding count reaches zero: rust-analyzer's
//! startup phases hand off to each other through gaps of about 70 to 100
//! milliseconds, and the first of those gaps arrives before indexing has
//! begun. Quiet is a count of zero that has held for `quiet_for`.
//!
//! A deadline bounds two failure modes neither the count nor the debounce
//! can see: a notification dropped from a full channel before its pump
//! existed, and a server that reports no progress at all and so never stops
//! being busy for the first time.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::ServerId;

/// Outstanding `$/progress` operations across every server.
#[derive(Debug)]
pub struct ServerSettle {
    state: Mutex<SettleState>,
    quiet_for: Duration,
    deadline_after: Duration,
}

#[derive(Debug)]
struct SettleState {
    outstanding: HashSet<(ServerId, String)>,
    /// When the outstanding set last became empty. `None` until the first
    /// operation ends, so a process that has not yet heard from a server is
    /// not mistaken for one whose servers have finished.
    quiet_since: Option<Instant>,
    /// When the backstop fires regardless of what the servers have said.
    deadline: Instant,
}

impl ServerSettle {
    /// Track settling with a `quiet_for` debounce, giving up after
    /// `deadline_after`.
    ///
    /// The deadline runs from construction; [`Self::restart_deadline`] moves
    /// it to cover the work it is meant to bound.
    #[must_use]
    pub fn new(quiet_for: Duration, deadline_after: Duration) -> Self {
        Self {
            state: Mutex::new(SettleState {
                outstanding: HashSet::new(),
                quiet_since: None,
                deadline: Instant::now() + deadline_after,
            }),
            quiet_for,
            deadline_after,
        }
    }

    /// Measure the deadline from now instead of from construction.
    ///
    /// The backstop exists to bound indexing, but a `ServerSettle` is built
    /// before any server is spawned, so config load and every server's
    /// `initialize` handshake would otherwise be spent out of the same
    /// budget. Callers restart the clock once the servers exist, and must do
    /// so unconditionally rather than on the arrival of server traffic: a
    /// server that never reports progress produces no event to hang this on,
    /// and the backstop is precisely what covers that case.
    pub fn restart_deadline(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.deadline = Instant::now() + self.deadline_after;
    }

    /// Record that `server` started a long-running operation.
    pub fn begin(&self, server: &ServerId, token: &serde_json::Value) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state
            .outstanding
            .insert((server.clone(), token.to_string()));
        state.quiet_since = None;
    }

    /// Record that `server` finished one.
    pub fn end(&self, server: &ServerId, token: &serde_json::Value) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if !state
            .outstanding
            .remove(&(server.clone(), token.to_string()))
        {
            return;
        }
        if state.outstanding.is_empty() {
            state.quiet_since = Some(Instant::now());
        }
    }

    /// Whether the workspace counts as analyzed as of `now`.
    #[must_use]
    pub fn should_settle_at(&self, now: Instant) -> bool {
        let Ok(state) = self.state.lock() else {
            return false;
        };
        if now >= state.deadline {
            return true;
        }
        state.outstanding.is_empty()
            && state
                .quiet_since
                .is_some_and(|since| now.duration_since(since) >= self.quiet_for)
    }

    /// [`Self::should_settle_at`] as of now.
    #[must_use]
    pub fn should_settle(&self) -> bool {
        self.should_settle_at(Instant::now())
    }
}
```

A poisoned lock is treated as "no information" rather than propagated: a diagnostics baseline is not worth taking a process down for. This differs from `bridge::lock_std`, which recovers the guard. The cost is that the deadline lives in the guarded state, so a poisoned lock withholds the backstop as well as the debounce and the baseline never lands. That trade is taken knowingly: both critical sections are a `HashSet` operation and an `Instant` comparison, neither of which can panic, so there is nothing here to poison the lock.

- [ ] **Step 4: Feed it from the pump**

In `crates/mcpls-core/src/lib.rs`, add three fields to `PumpShared`:

```rust
    /// Outstanding server work, so the baseline waits for a quiet workspace.
    pub(crate) settle: Arc<ServerSettle>,
    /// Per-session record of what has already been delivered.
    pub(crate) delivery: Arc<Mutex<DiagnosticsDelivery>>,
    /// The severity floor each server answers to.
    pub(crate) floors: Arc<FloorTable>,
```

and replace the discarding match arm in `diagnostics_pump`:

```rust
                    LspNotification::Progress { token, value } => {
                        match value.get("kind").and_then(serde_json::Value::as_str) {
                            Some("begin") => settle.begin(&server_id, &token),
                            Some("end") => settle.end(&server_id, &token),
                            _ => {}
                        }
                    }
                    LspNotification::Other { .. } => {}
```

Construct all three in `serve_with` where task 4 put the `PumpShared` literal:

```rust
    let settle = Arc::new(ServerSettle::new(
        Duration::from_millis(config.diagnostics.settle_quiet_ms),
        Duration::from_millis(config.diagnostics.settle_deadline_ms),
    ));
    let delivery = Arc::new(Mutex::new(DiagnosticsDelivery::new(config.diagnostics)));
    let floors = Arc::new(FloorTable::new(&config.diagnostics, &config.lsp_servers));
```

Place these before `applicable_configs` is moved into `spawn_lsp_servers_background`; `config` is still owned at that point.

Add one pump test asserting that a `$/progress` begin followed by its end leaves the settle tracker quiet, matching the setup of the tests added in task 4.

- [ ] **Step 5: Give the cache an owner-aware snapshot**

`NotificationCache::diagnostics` and `diagnostics_owners` are private (`bridge/notifications.rs:432-435`) and nothing iterates them. Add next to `get_diagnostics` (`:719`):

```rust
    /// Every cached entry with the key it is stored under and the server
    /// that published it.
    ///
    /// Returns owned values so a caller can release the cache lock before
    /// doing anything with them: the diagnostics pump needs the same lock.
    #[must_use]
    pub fn diagnostics_snapshot(&self) -> Vec<(String, DiagnosticInfo, ServerId)> {
        self.diagnostics
            .iter()
            .filter_map(|(key, info)| {
                let owner = self.diagnostics_owners.get(key)?;
                Some((key.clone(), info.clone(), owner.clone()))
            })
            .collect()
    }
```

- [ ] **Step 6: Take the baseline from its own task**

`diagnostics_pump` is already near the 100-line limit and there is one pump per server, so a poll inside it would run N times over. Give the baseline its own task, spawned beside the pumps in `spawn_lsp_servers_background`:

```rust
/// How often to ask whether the servers have gone quiet. The task stops
/// once the baseline is taken, so this cost is bounded by the settle
/// deadline rather than by the process lifetime.
const BASELINE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Adopt the current cache as the baseline once the servers have stayed
/// quiet, so a session's first flush reports what happened since startup
/// rather than everything the workspace already knew.
async fn baseline_task(
    settle: Arc<ServerSettle>,
    cache: Arc<Mutex<NotificationCache>>,
    delivery: Arc<Mutex<DiagnosticsDelivery>>,
    floors: Arc<FloorTable>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            result = cancel_rx.changed() => {
                if result.is_err() || *cancel_rx.borrow() {
                    return;
                }
            }
            () = tokio::time::sleep(BASELINE_POLL_INTERVAL) => {
                if !settle.should_settle() {
                    continue;
                }
                let snapshot = {
                    let cache = cache.lock().await;
                    cache.diagnostics_snapshot()
                };
                let baseline = snapshot
                    .iter()
                    .filter_map(|(key, info, owner)| {
                        DiagnosticsDelivery::visible_hash(
                            &info.diagnostics,
                            floors.for_server(owner),
                        )
                        .map(|hash| (key.clone(), hash))
                    })
                    .collect();
                delivery.lock().await.set_baseline(baseline);
                debug!("diagnostics baseline taken over {} file(s)", snapshot.len());
                return;
            }
        }
    }
}
```

Spawn it into the same `JoinSet` as the pumps, from the fields task 4 put on `PumpShared`.

- [ ] **Step 7: Run the tests and commit**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib 2>&1 | tail -20`, then fmt, clippy and the full command set from task 1 step 7.

```bash
git -C /home/lev/Git/lev/mcpls-diagnostics add crates/mcpls-core/src/bridge/ crates/mcpls-core/src/lib.rs
git -C /home/lev/Git/lev/mcpls-diagnostics commit -m "feat(bridge): baseline diagnostics once servers stay quiet" -m "Servers spawn in the background and rust-analyzer publishes for the
whole workspace once its initial analysis finishes, so a baseline taken
the first time a session appears is empty and every later publish looks
new. That is the workspace dump the baseline exists to prevent.

Waiting for the first moment of quiet is no better: measured against
rust-analyzer 1.98.1, the outstanding progress count reaches zero four
times while startup phases hand off, the first of them seconds before
indexing finishes. The baseline now waits for quiet that has held for
settle_quiet_ms, and takes it anyway after settle_deadline_ms so a
dropped notification or a server that never reports progress cannot
wedge it.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: The `get_new_diagnostics` tool

**Files:**
- Modify: `crates/mcpls-core/src/mcp/handlers.rs` (`BridgeContext`), `crates/mcpls-core/src/mcp/server.rs`, `crates/mcpls-core/src/lib.rs`
- Test: `crates/mcpls-core/tests/e2e/protocol_tests.rs`, `crates/mcpls-core/tests/ra_e2e.rs`

**Interfaces:**
- Consumes: everything from tasks 2 through 5.
- Produces: the `get_new_diagnostics` MCP tool. No parameters: the agent asks what is new, and narrowing it by path is what `get_cached_diagnostics` already does.

Background:

- Declare the tool with no `Parameters` argument at all rather than an empty parameter struct. `rmcp-macros/src/tool.rs:200-215` falls back to `schema_for_empty_input()` when it finds no `Parameters` argument, which emits `{"type":"object","properties":{}}`. An empty `#[derive(JsonSchema)]` struct emits `{"type":"object"}` with no `properties` key, and `tests/e2e/protocol_tests.rs:151` asserts every tool's schema has one.
- `BridgeContext::new` is a positional `const fn` with five parameters (`mcp/handlers.rs:54-60`). Adding two keeps it under the limit but breaks its call sites in `mcp/server.rs` and the handlers tests.
- The tool must return the same coordinates `get_cached_diagnostics` does. That one resolves the owner's negotiated encoding and runs the entry through `Translator::diagnostics_from_cache_entry` (`mcp/server.rs:601-627`). Two diagnostics tools disagreeing about columns is a trap for the agent, so this one does the same.

- [ ] **Step 1: Write the failing tests**

In `crates/mcpls-core/tests/e2e/protocol_tests.rs`, extend the `expected_names` array in `test_e2e_list_tools` with `"get_new_diagnostics"`. That test derives its count from the array, so no number needs changing.

In `crates/mcpls-core/tests/ra_e2e.rs`, add a sub-case. That file is one `#[test] fn ra_e2e_suite()` driving `SubCase` values sequentially against a single rust-analyzer, so the new test is a `fn(&mut McpClient, &Path) -> Result<(), String>` registered in the sub-case list, not a `#[tokio::test]`.

```rust
/// Tool 17: `get_new_diagnostics` — drains once and stays drained.
///
/// Scoped to the deduplication property rather than to a diagnostic
/// appearing, because whether the workspace's known errors land before or
/// after the baseline depends on rust-analyzer's startup timing. The
/// baseline semantics are pinned by unit tests instead.
fn sc_get_new_diagnostics(client: &mut McpClient, _workspace: &Path) -> Result<(), String> {
    let first = client
        .call_tool("get_new_diagnostics", &json!({}))
        .map_err(|e| format!("call failed: {e}"))?;
    let text = assertions::assert_tool_ok(&first);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;
    inner["changed"]
        .as_array()
        .ok_or_else(|| format!("expected a changed array, got {inner}"))?;

    let second = client
        .call_tool("get_new_diagnostics", &json!({}))
        .map_err(|e| format!("second call failed: {e}"))?;
    let text = assertions::assert_tool_ok(&second);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let changed = inner["changed"].as_array().map_or(0, Vec::len);
    let cleared = inner["cleared"].as_array().map_or(0, Vec::len);
    if changed != 0 || cleared != 0 {
        return Err(format!(
            "a second drain with no edits between should be empty, got {inner}"
        ));
    }
    Ok(())
}
```

Register it in the sub-case list, and update the module doc comment's tool count at the top of the file.

- [ ] **Step 2: Run them and watch them fail**

Run:
```bash
cargo build --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --workspace
cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --test integration_tests e2e::protocol_tests::test_e2e_list_tools -- --include-ignored 2>&1 | tail -20
```

Every test in `protocol_tests.rs` is `#[ignore = "Requires mcpls binary built"]`, so without `--include-ignored` this command runs nothing and reports success. The build comes first because the staleness guard from `92b7f4c` refuses a binary older than its sources.

Expected: the tool list is one short, naming `get_new_diagnostics` as missing.

- [ ] **Step 3: Wire the delivery core into the context**

Add to `BridgeContext` in `crates/mcpls-core/src/mcp/handlers.rs`:

```rust
    /// Per-session record of which diagnostics have already been delivered.
    ///
    /// Locked independently of `notification_cache`; a flush takes the cache
    /// lock only long enough to copy the snapshot it works from.
    pub delivery: Arc<Mutex<DiagnosticsDelivery>>,
    /// The severity floor each server answers to, resolved once at startup.
    pub floors: Arc<FloorTable>,
```

Extend `BridgeContext::new` and `McplsServer::new` to take them, and pass the same `Arc`s task 5 built in `serve_with`, so the baseline the background task adopts is the one this tool reads. Update the handlers tests that construct a `BridgeContext`.

- [ ] **Step 4: Add the tool**

In `crates/mcpls-core/src/mcp/server.rs`, beside `get_cached_diagnostics`:

```rust
    /// Drain diagnostics that changed since the last call.
    #[tool(
        description = "Diagnostics that changed since you last asked, across every file the language servers report on. Returns nothing when nothing changed.",
        title = "New Diagnostics"
    )]
    async fn get_new_diagnostics(&self) -> Result<String, McpError> {
        let session = SessionId::process_default();
        let snapshot = {
            let cache = self.context.notification_cache.lock().await;
            cache.diagnostics_snapshot()
        };

        let entries: Vec<FileEntry<'_>> = snapshot
            .iter()
            .map(|(key, info, owner)| FileEntry {
                key,
                diagnostics: &info.diagnostics,
                floor: self.context.floors.for_server(owner),
            })
            .collect();

        let report = {
            let mut delivery = self.context.delivery.lock().await;
            delivery.flush(&session, &entries)
        };

        to_tool_result(Ok(self.new_diagnostics_payload(&report, &snapshot).await))
    }
```

`new_diagnostics_payload` is a private async helper on the same impl, kept separate so the tool body stays short. For each `ChangedFile` it finds the matching `(uri, owner)` in the snapshot by key, builds a `DiagnosticInfo` carrying the file's *filtered* diagnostics, resolves the owner's encoding with `self.context.translator.position_encoding_for(&owner)`, and runs it through `Translator::diagnostics_from_cache_entry(.., encoding, self.context.translator.document_tracker())`. That is the same conversion `get_cached_diagnostics` performs, so both tools report the same columns.

The serialized shape: `changed` as an array of `{ file_path, diagnostics, omitted }`, `cleared` as an array of file paths, and a top-level `omitted` count with a sentence saying the caps held those files back and the next call will offer them again. Derive `file_path` from the URI with `bridge::uri_to_path`; drop an entry whose URI does not map to a path rather than showing the agent a URI it cannot open.

- [ ] **Step 5: Run the tests**

Run:
```bash
cargo fmt --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --all
cargo clippy --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --workspace --all-targets -- -D warnings
cargo build --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --workspace
cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --workspace
cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --test integration_tests e2e:: -- --include-ignored
cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --test ra_e2e -- --include-ignored
```

Expected: PASS throughout. If rust-analyzer is not on PATH, `MCPLS_SKIP_RA=1` skips the last command; say so in the report rather than treating a skip as a pass.

- [ ] **Step 6: Document and commit**

Add `get_new_diagnostics` to `docs/user-guide/tools-reference.md` and to the README's Diagnostics table. Add a `### Added` changelog entry.

```bash
git -C /home/lev/Git/lev/mcpls-diagnostics add crates/mcpls-core/ docs/ README.md CHANGELOG.md
git -C /home/lev/Git/lev/mcpls-diagnostics commit -m "feat(mcp): add the get_new_diagnostics tool" -m "Drains what the language servers have reported since the caller last
asked, deduplicated per session and filtered by each server's severity
floor. Returns nothing when nothing changed, which is the common case
and the reason it is cheap to call often.

Positions run through the same encoding conversion get_cached_diagnostics
uses, so the two tools cannot disagree about a column. The cache lock is
taken only to copy the snapshot, never across the deduplication work.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## What this plan does not build

Stated so a reviewer does not look for them:

- **The footer on the tools that write.** Stage B. Before the resync exists, an apply ends by telling every server to forget the files it wrote, so a footer would report nothing.
- **Automatic delivery.** Nothing here injects anything. The agent must call `get_new_diagnostics`. Stage C is where a hook drains it without being asked.
- **Coverage of external writes.** Measured and recorded in the spec: rust-analyzer sees an external write and will not check it, so no language is covered until stage B opens and saves changed files.
- **A way to start from no built-in servers at all.** A config wanting only its own servers must disable each built-in by name. The obvious key for this cannot be read by `deserialize_lsp_servers`, which is a field-level `deserialize_with` and cannot see a sibling field; supporting it means giving `ServerConfig` a shadow struct for deserialization, where every future field has to be added twice. That is worth its own change, not a rider on this one.
- **An end-to-end proof that a newly broken file shows up as new.** The e2e sub-case in task 6 asserts the drain deduplicates, not that a specific diagnostic is new, because whether the workspace's known errors land before or after the baseline depends on rust-analyzer's startup timing, which the test cannot control. The baseline semantics are pinned by unit tests instead.

## Self-review

**Spec coverage.** A1 is task 1. The `[diagnostics]` table and per-server floor are task 2. A2's record, hash, floor and caps are task 3; its stale-publish rule is task 4; its baseline rule is task 5. A3's flush tool and session keying are tasks 3 and 6. The one spec element deliberately not implemented is the `footer` key, recorded under deliberate deviations, and the one reinterpreted is the baseline trigger, also recorded there with its measurement.

**Placeholders.** Two test bodies are described rather than written: the pump tests in task 4 step 1 and the settle pump test in task 5 step 4. Both are cases where the surrounding file already has a setup idiom that a fresh implementer should copy rather than reinvent, and both name the file and the helper to copy from. `new_diagnostics_payload` in task 6 is specified by its inputs, its conversion and its output shape rather than written out, because its body is assembly against three signatures the task quotes. Every other step carries the code it needs.

**Type consistency.** `SessionId`, `FileEntry`, `FlushReport`, `ChangedFile`, `DiagnosticsDelivery`, `ServerSettle`, `FloorTable`, `SeverityFloor` and `DiagnosticsConfig` keep the same names and shapes from the task that defines them through every later use. `visible_hash` is used by both task 3's flush and task 5's baseline. `FloorTable` is defined in task 3, constructed in task 5 and read in task 6. `diagnostics_snapshot` is added in task 5, where the baseline first needs it, and reused in task 6. `PumpShared` grows in task 4 and again in task 5; no task references a type a later task defines.

**Ordering.** Task 4 collapses `spawn_lsp_servers_background`'s parameter list because task 5 cannot add to it otherwise. Task 5 constructs the delivery core and floor table because its baseline task needs them; task 6 shares the same `Arc`s rather than building new ones, which is what makes the baseline the tool reads the one the background task adopted.
