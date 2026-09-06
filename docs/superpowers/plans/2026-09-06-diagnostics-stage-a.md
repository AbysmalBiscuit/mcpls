# Diagnostics injection, stage A: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make configuration entries merge onto the built-in servers, then build the per-session diagnostics deduplication core and the tool that drains it.

**Architecture:** Config entries become partial records folded onto the built-in server list by routing identity, which is what makes a per-server severity floor reachable. A new `bridge::delivery` module holds one record per session mapping each file to a hash of its diagnostics, so a flush returns only what changed. The diagnostics pump gains a document-tracker handle to drop publishes describing text that is already gone, and a settle signal so the baseline is taken when servers go quiet rather than on first sight.

**Tech Stack:** Rust 2024 edition, MSRV 1.88, tokio, rmcp 3.1.4, serde/toml, clippy pedantic + nursery.

**Spec:** `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`

## Global Constraints

- Rust edition 2024, MSRV 1.88. `cargo fmt` clean and `cargo clippy --workspace --all-targets -- -D warnings` clean before every commit.
- `unwrap_used` and `expect_used` are warn-level in this crate. Production code uses neither. Test code that needs one carries `#[allow(clippy::unwrap_used)]` or `#[allow(clippy::expect_used)]` on the test module or the individual test, matching the surrounding file.
- `missing_docs` is warn-level. Every public item gets a doc comment. Every public function returning `Result` gets an `# Errors` section.
- `too_many_lines` triggers at 100. Split rather than allow.
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
| `crates/mcpls-core/src/bridge/delivery.rs` | New. The per-session record, the diagnostic-set hash, the severity floor and volume caps, and the flush. Pure: no locks, no IO, no LSP types beyond what a diagnostic is. |
| `crates/mcpls-core/src/bridge/settle.rs` | New. Counts outstanding `$/progress` operations per server so the delivery core knows when a workspace has gone quiet. |
| `crates/mcpls-core/src/lib.rs` | `PumpShared` gains the tracker and settle handles; the pump drops stale publishes and feeds the settle counter. |
| `crates/mcpls-core/src/mcp/handlers.rs` | `BridgeContext` gains the delivery core. |
| `crates/mcpls-core/src/mcp/server.rs` | The `get_new_diagnostics` tool. |
| `crates/mcpls-core/src/mcp/tools.rs` | Its parameter struct. |
| `crates/mcpls-core/tests/ra_e2e.rs` | End-to-end proof against a real rust-analyzer. |

## Deliberate deviations from the spec

- The spec's `[diagnostics]` block shows a `footer` key. This plan does not add it. Nothing in stage A reads it, and an inert configuration key is worse than an absent one. Stage B adds it in the commit that makes it do something.
- Task 1 also changes what a config file with no `[[lsp_servers]]` block produces. Today that is zero servers (`config/mod.rs:1600-1610`), so a file setting only `[workspace]` spawns nothing at all. After the merge it produces the built-ins. This is a strict improvement and falls out of the same change, so it is not called out separately in the spec.

---

### Task 1: Configuration entries merge onto the built-in servers

**Files:**
- Modify: `crates/mcpls-core/src/config/server.rs`
- Modify: `crates/mcpls-core/src/config/mod.rs`
- Test: `crates/mcpls-core/src/config/server.rs` (inline `mod tests`), `crates/mcpls-core/src/config/mod.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub struct PartialLspServerConfig` with every field optional plus `pub enabled: Option<bool>`
  - `LspServerConfig::builtins() -> Vec<LspServerConfig>`
  - `PartialLspServerConfig::id(&self) -> Option<ServerId>`
  - `pub fn resolve_lsp_servers(partials: Vec<PartialLspServerConfig>) -> Result<Vec<LspServerConfig>>`

Background the implementer needs: `LspServerConfig::id()` (`config/server.rs:269`) returns `name` when set and `language_id` otherwise. It is already the routing key across `Translator`'s maps, and `ToolRouter::from_configs` rejects duplicates. That is the merge key; do not invent another.

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
    assert!(
        config.lsp_servers.len() > 1,
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
    // different binary.
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
fn test_an_entry_matching_no_builtin_needs_a_command() {
    let result: std::result::Result<ServerConfig, _> =
        toml::from_str("[[lsp_servers]]\nlanguage_id = \"elixir\"\n");
    let message = result.expect_err("an unmatched entry without a command is rejected").to_string();
    assert!(
        message.contains("elixir"),
        "the error names the id it could not find, got: {message}"
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib config::tests 2>&1 | tail -30`

Expected: compile errors on `LspServerConfig::builtins`, plus assertion failures showing one server where the test wants several. If instead a test passes, stop: the behaviour is not what this task assumes.

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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialLspServerConfig {
    /// Language identifier. Required unless `name` identifies the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_id: Option<String>,
    /// Command to start the LSP server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments to pass to the command. An empty list is empty arguments,
    /// not an absent key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// Environment variables for the server process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// File patterns this server handles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_patterns: Option<Vec<String>>,
    /// Server-specific initialization options. Replaces the built-in's
    /// value rather than merging into it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_options: Option<serde_json::Value>,
    /// Handshake timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// Per-request timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_seconds: Option<u64>,
    /// Spawn heuristics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heuristics: Option<ServerHeuristics>,
    /// Routing identity, defaulting to `language_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Tools this server handles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handles: Option<Vec<ToolKind>>,
    /// Set to `false` to drop the built-in this entry names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

- [ ] **Step 4: Write the fold**

Still in `crates/mcpls-core/src/config/server.rs`:

```rust
/// Fold configuration entries onto the built-in servers.
///
/// An entry modifies the built-in sharing its [`LspServerConfig::id`]: what
/// the entry omits, it inherits. An entry naming no built-in defines a new
/// server, where nothing can be inherited and `command` is required. An
/// entry with `enabled = false` removes the server it names.
///
/// Overriding `command` drops the built-in's `args`, `env`, and
/// `initialization_options`, because those belong to the binary being
/// replaced: pyright's `--stdio` means nothing to a different program, and
/// inheriting it fails at spawn time rather than at load time.
///
/// # Errors
///
/// Returns [`Error::InvalidConfig`] when an entry names no id at all, or
/// when an entry matching no built-in gives no `command`.
pub fn resolve_lsp_servers(
    partials: Vec<PartialLspServerConfig>,
) -> Result<Vec<LspServerConfig>> {
    let mut resolved = LspServerConfig::builtins();

    for partial in partials {
        let id = partial.id().ok_or_else(|| {
            Error::InvalidConfig(
                "an [[lsp_servers]] entry needs a language_id or a name".to_string(),
            )
        })?;

        let existing = resolved.iter().position(|server| server.id() == id);

        if partial.enabled == Some(false) {
            if let Some(index) = existing {
                resolved.remove(index);
            }
            continue;
        }

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
                "[[lsp_servers]] entry '{id}' matches no built-in server, so it needs a command"
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

and add, at module level in the same file:

```rust
/// Deserialize `[[lsp_servers]]` entries and fold them onto the built-ins.
fn deserialize_lsp_servers<'de, D>(deserializer: D) -> std::result::Result<Vec<LspServerConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let partials = Vec::<PartialLspServerConfig>::deserialize(deserializer)?;
    resolve_lsp_servers(partials).map_err(serde::de::Error::custom)
}
```

Export `PartialLspServerConfig` and `resolve_lsp_servers` from `config/mod.rs`'s `pub use` block alongside `LspServerConfig`.

Leave `ServerConfig::default()`'s explicit list in place but have it call `LspServerConfig::builtins()` so the two cannot drift.

- [ ] **Step 6: Fix the tests this breaks**

`test_load_from_valid_toml` (`config/mod.rs:975`) asserts `config.lsp_servers.len() == 1` and `test_empty_config_file` asserts the list is empty. Both pinned the replace semantics this task removes. Rewrite them to assert the merged shape: the first now expects the rust entry's `timeout_seconds` to be 30 with every other built-in still present, the second expects `LspServerConfig::builtins().len()`.

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib config 2>&1 | tail -20`

Expected: every config test passes, including the six added in step 1.

- [ ] **Step 7: Check the whole workspace and the lints**

Run:
```bash
cargo fmt --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --all
cargo clippy --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --workspace --all-targets -- -D warnings
cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --workspace
```

Expected: all clean. Integration tests that build a `ServerConfig` literal need the field unchanged, since `lsp_servers` keeps its type.

- [ ] **Step 8: Document and commit**

Update `docs/user-guide/configuration.md`'s server section to describe entries as modifying built-ins, with `enabled = false` to remove one and the `command` rule spelled out. Add a `## [Unreleased]` entry to `CHANGELOG.md` under `### Changed` naming the breaking change.

```bash
git -C /home/lev/Git/lev/mcpls-diagnostics add crates/mcpls-core/src/config/ docs/user-guide/configuration.md CHANGELOG.md
git -C /home/lev/Git/lev/mcpls-diagnostics commit -m "feat(config)!: merge server entries onto the built-ins" -m "Writing any [[lsp_servers]] block replaced the built-in list wholesale,
so reaching one per-server key cost every default server, and a config
file with no server table spawned nothing at all.

Entries are now partial records folded onto the built-ins by routing
identity: what an entry omits, it inherits. enabled = false removes a
built-in, replacing the disable-by-omission that replace semantics gave
for free. Overriding command drops the built-in's args, env and
initialization options, which belong to the binary being replaced.

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
  - `pub struct DiagnosticsConfig { pub severity: SeverityFloor, pub max_per_file: usize, pub max_total: usize }`
  - `ServerConfig::diagnostics: DiagnosticsConfig`
  - `LspServerConfig::diagnostics_severity: Option<SeverityFloor>`

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
        let Some(severity) = severity else {
            return self != Self::Off;
        };
        let rank = |floor: Self| match floor {
            Self::Off => 0_u8,
            Self::Error => 1,
            Self::Warning => 2,
            Self::Information => 3,
            Self::Hint => 4,
        };
        // lsp_types orders ERROR = 1 through HINT = 4, matching the ranks
        // above, so a diagnostic clears the floor when its own rank is no
        // deeper than the floor's.
        self != Self::Off && u8::try_from(severity as i32).is_ok_and(|s| s <= rank(self))
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

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            severity: default_severity_floor(),
            max_per_file: default_max_per_file(),
            max_total: default_max_total(),
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

and to `ServerConfig::default()`. Add `diagnostics_severity: Option<SeverityFloor>` to both `LspServerConfig` (as `#[serde(default, skip_serializing_if = "Option::is_none")]`) and `PartialLspServerConfig`, extend `LspServerConfig::merge` and `from_partial` to carry it, and set it to `None` in `LspServerConfig::builtin`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib config 2>&1 | tail -20`

Expected: PASS. `test_apply_defaults_to_read_only` and the round-trip tests still pass, since every new field has a default.

- [ ] **Step 5: Verify and commit**

Run fmt, clippy and the workspace suite as in task 1 step 7. Document the `[diagnostics]` table in `docs/user-guide/configuration.md` with a section per key, and add a `### Added` entry to the changelog.

```bash
git -C /home/lev/Git/lev/mcpls-diagnostics add crates/mcpls-core/src/config/ docs/user-guide/configuration.md CHANGELOG.md
git -C /home/lev/Git/lev/mcpls-diagnostics commit -m "feat(config): add the diagnostics table" -m "One severity floor per server, with a global default, plus two volume
caps that stay global because they bound one shared context budget
rather than one server's output. off mutes a server, which is how a
language is excluded.

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
  - `pub struct DiagnosticsDelivery` with `pub fn new(config: DiagnosticsConfig) -> Self`
  - `pub fn flush(&mut self, session: &SessionId, entries: &[FileEntry]) -> FlushReport`
  - `pub struct FileEntry<'a> { pub key: &'a str, pub path: String, pub diagnostics: &'a [lsp_types::Diagnostic], pub floor: SeverityFloor }`
  - `pub struct FlushReport { pub changed: Vec<ChangedFile>, pub cleared: Vec<String>, pub omitted: usize }`
  - `pub struct ChangedFile { pub path: String, pub diagnostics: Vec<lsp_types::Diagnostic>, pub omitted: usize }`
  - `pub struct FloorTable` with `pub fn new(config: &DiagnosticsConfig, servers: &[LspServerConfig]) -> Self` and `pub fn for_server(&self, server: &ServerId) -> SeverityFloor`

The caller assembles `FileEntry` values from `NotificationCache` and the config; this module never touches a lock, a file, or a server. That is what makes it testable without a language server.

- [ ] **Step 1: Write the failing tests**

Create `crates/mcpls-core/src/bridge/delivery.rs` with only the test module and the imports it needs, so the tests fail to compile against types that do not exist yet:

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

    fn entry<'a>(key: &'a str, diagnostics: &'a [Diagnostic], floor: SeverityFloor) -> FileEntry<'a> {
        FileEntry {
            key,
            path: format!("/w/{key}"),
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
        assert_eq!(fixed.cleared, vec!["/w/a.rs".to_string()]);

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

        delivery.flush(&session, &[entry("a.rs", &[a.clone(), b.clone()], SeverityFloor::Warning)]);
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

use crate::config::{DiagnosticsConfig, SeverityFloor};

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
    /// Path to show the agent.
    pub path: String,
    /// Everything the owning server currently reports for this file.
    pub diagnostics: &'a [lsp_types::Diagnostic],
    /// The floor this file's server answers to.
    pub floor: SeverityFloor,
}

/// One file that changed since the last flush.
#[derive(Debug, Clone)]
pub struct ChangedFile {
    /// Path to show the agent.
    pub path: String,
    /// Diagnostics at or above the floor, capped.
    pub diagnostics: Vec<lsp_types::Diagnostic>,
    /// How many this file's cap dropped.
    pub omitted: usize,
}

/// What one flush found.
#[derive(Debug, Clone, Default)]
pub struct FlushReport {
    /// Files whose visible diagnostics differ from the last flush.
    pub changed: Vec<ChangedFile>,
    /// Files that had visible diagnostics and now have none.
    pub cleared: Vec<String>,
    /// Files the total cap held back. They are not recorded as delivered,
    /// so the next flush offers them again.
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
    /// Taken when the workspace's servers first go quiet. Without it the
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
    #[must_use]
    pub fn visible_hash(diagnostics: &[lsp_types::Diagnostic], floor: SeverityFloor) -> Option<u64> {
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
    pub fn flush(&mut self, session: &SessionId, entries: &[FileEntry<'_>]) -> FlushReport {
        let baseline = self.baseline.clone().unwrap_or_default();
        let record = self
            .sessions
            .entry(session.clone())
            .or_insert_with(|| baseline);

        let mut report = FlushReport::default();
        let mut budget = self.config.max_total;

        for entry in entries {
            let hash = Self::visible_hash(entry.diagnostics, entry.floor);
            let previous = record.get(entry.key).copied();

            match (hash, previous) {
                (None, Some(_)) => {
                    record.remove(entry.key);
                    report.cleared.push(entry.path.clone());
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
                    if budget == 0 {
                        // Held back rather than delivered: leaving the record
                        // untouched is what makes the next flush offer it again.
                        report.omitted += 1;
                        continue;
                    }
                    let per_file = self.config.max_per_file.min(budget);
                    let omitted = visible.len().saturating_sub(per_file);
                    visible.truncate(per_file);
                    budget -= visible.len();
                    record.insert(entry.key.to_string(), current);
                    report.changed.push(ChangedFile {
                        path: entry.path.clone(),
                        diagnostics: visible,
                        omitted,
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
#[derive(Debug, Default)]
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

`SeverityFloor` needs a `Default` implementation returning `Warning` for `FloorTable`'s derive; add it in `config/mod.rs` beside the enum.

Declare the module in `crates/mcpls-core/src/bridge/mod.rs` (`mod delivery;` plus a `pub use delivery::{...};` matching how neighbouring modules are re-exported).

- [ ] **Step 4: Run the tests**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib bridge::delivery 2>&1 | tail -20`

Expected: all eight pass.

- [ ] **Step 5: Verify and commit**

Run fmt, clippy and the workspace suite.

```bash
git -C /home/lev/Git/lev/mcpls-diagnostics add crates/mcpls-core/src/bridge/
git -C /home/lev/Git/lev/mcpls-diagnostics commit -m "feat(bridge): add the diagnostics delivery core" -m "One record per session mapping each file to a hash of the diagnostics
that clear its floor, so a flush returns what changed rather than
everything known. The hash is order-insensitive because truncation
re-sorts survivors by severity, and a resort is not a change.

A file the total cap holds back keeps its old record, so the next
flush offers it again instead of swallowing it.

Nothing calls this yet.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: The pump drops publishes describing text that is gone

**Files:**
- Modify: `crates/mcpls-core/src/lib.rs:120-127` (`PumpShared`), `crates/mcpls-core/src/lib.rs:154` (`diagnostics_pump`), and the `serve_with` call site that builds `PumpShared`
- Test: the existing `diagnostics_pump` test module in `crates/mcpls-core/src/lib.rs:2194`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `PumpShared::document_tracker: Arc<DocumentTracker>`, consumed by task 5.

Background: `DocumentTracker::get(&Path) -> Option<DocumentState>` (`bridge/state.rs:321`) and `DocumentState::version() -> i32`. The tracker's map is a `StdMutex` held only for short synchronous sections (`bridge/state.rs:280-286`), so a lookup from the pump cannot stall it. `Translator::document_tracker()` already exposes the handle; `serve_with` has the translator in scope where it builds `PumpShared`.

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

Expected: the first test fails because the stale publish is stored. The other two should pass before the change and must still pass after it; that is the point of writing them now.

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

Let-chains are stable in edition 2024; if clippy objects to the shape, use nested `if let` rather than restructuring the condition.

Update the `PumpShared` construction in `serve_with` to pass `translator.document_tracker()`, and every test that builds a `PumpShared`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib diagnostics_pump 2>&1 | tail -20`

Expected: all three pass, and no existing pump test regresses.

- [ ] **Step 5: Verify and commit**

Run fmt, clippy and the workspace suite.

```bash
git -C /home/lev/Git/lev/mcpls-diagnostics add crates/mcpls-core/src/lib.rs
git -C /home/lev/Git/lev/mcpls-diagnostics commit -m "fix(bridge): drop diagnostics for text an edit replaced" -m "A publish arriving after an edit, describing the version before it,
overwrote the current cache entry. The agent was shown errors for text
that no longer existed, and the next publish flipped them back.

A publish naming a version below the tracked one is now dropped. A
publish with no version, or for a path with no tracked entry, is kept:
those are the files a server reports without being asked, and they are
the ones worth delivering.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: The baseline is taken when the servers go quiet

**Files:**
- Create: `crates/mcpls-core/src/bridge/settle.rs`
- Modify: `crates/mcpls-core/src/bridge/mod.rs`, `crates/mcpls-core/src/lib.rs`
- Test: inline `mod tests` in `settle.rs`, plus one pump test in `lib.rs`

**Interfaces:**
- Consumes: `DiagnosticsDelivery::set_baseline` and `visible_hash` from task 3; `PumpShared` from task 4.
- Produces: `pub struct ServerSettle` with `pub fn begin(&self, server: &ServerId, token: &serde_json::Value)`, `pub fn end(&self, server: &ServerId, token: &serde_json::Value)`, `pub fn has_settled(&self) -> bool`.

Why this exists: language servers spawn in the background (`lib.rs:652-690`) and rust-analyzer publishes for the whole workspace once its initial analysis finishes, well after the first session op. A baseline taken on first sight is therefore empty, and every publish after it looks new, which is exactly the workspace dump the baseline exists to prevent.

- [ ] **Step 1: Write the failing tests**

Create `crates/mcpls-core/src/bridge/settle.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_a_server_that_never_started_work_has_not_settled() {
        let settle = ServerSettle::default();
        assert!(!settle.has_settled(), "nothing has happened yet");
    }

    #[test]
    fn test_a_server_settles_when_its_last_operation_ends() {
        let settle = ServerSettle::default();
        let server = "rust".into();
        settle.begin(&server, &json!("rustAnalyzer/cachePriming"));
        assert!(!settle.has_settled());
        settle.end(&server, &json!("rustAnalyzer/cachePriming"));
        assert!(settle.has_settled());
    }

    #[test]
    fn test_nested_operations_settle_only_when_all_end() {
        let settle = ServerSettle::default();
        let server = "rust".into();
        settle.begin(&server, &json!("rustAnalyzer/Indexing"));
        settle.begin(&server, &json!("rust-analyzer/flycheck/0"));
        settle.end(&server, &json!("rustAnalyzer/Indexing"));
        assert!(!settle.has_settled(), "flycheck is still running");
        settle.end(&server, &json!("rust-analyzer/flycheck/0"));
        assert!(settle.has_settled());
    }

    #[test]
    fn test_settling_once_is_remembered_through_later_work() {
        let settle = ServerSettle::default();
        let server = "rust".into();
        settle.begin(&server, &json!("a"));
        settle.end(&server, &json!("a"));
        settle.begin(&server, &json!("b"));
        assert!(
            settle.has_settled(),
            "the baseline moment already happened; later work does not undo it"
        );
    }

    #[test]
    fn test_an_end_without_a_begin_is_ignored() {
        let settle = ServerSettle::default();
        settle.end(&"rust".into(), &json!("orphan"));
        assert!(!settle.has_settled(), "an unmatched end proves nothing");
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
//! Every server reports long-running work through `$/progress`. When a
//! server's outstanding operations reach zero, its view of the workspace is
//! as complete as it is going to get without new input, which is the moment
//! worth calling a baseline.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::lsp::ServerId;

/// Outstanding `$/progress` operations, per server.
#[derive(Debug, Default)]
pub struct ServerSettle {
    state: Mutex<SettleState>,
}

#[derive(Debug, Default)]
struct SettleState {
    outstanding: HashMap<ServerId, HashSet<String>>,
    settled: bool,
}

impl ServerSettle {
    /// Record that `server` started a long-running operation.
    pub fn begin(&self, server: &ServerId, token: &serde_json::Value) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state
            .outstanding
            .entry(server.clone())
            .or_default()
            .insert(token.to_string());
    }

    /// Record that `server` finished one, and note the settle if that was
    /// the last outstanding operation anywhere.
    pub fn end(&self, server: &ServerId, token: &serde_json::Value) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(tokens) = state.outstanding.get_mut(server) else {
            return;
        };
        if !tokens.remove(&token.to_string()) {
            return;
        }
        if state.outstanding.values().all(HashSet::is_empty) {
            state.settled = true;
        }
    }

    /// Whether the servers have gone quiet at least once.
    #[must_use]
    pub fn has_settled(&self) -> bool {
        self.state.lock().is_ok_and(|state| state.settled)
    }
}
```

A poisoned lock is treated as "no information" rather than propagated: a diagnostics baseline is not worth taking a process down for.

- [ ] **Step 4: Feed it from the pump**

In `crates/mcpls-core/src/lib.rs`, add `pub(crate) settle: Arc<ServerSettle>` to `PumpShared`, and replace the discarding arm:

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

Add one pump test asserting that a begin followed by an end marks the settle, matching the setup of the tests added in task 4.

- [ ] **Step 5: Take the baseline**

The delivery core is behind a lock in `BridgeContext` (task 6 puts it there). For this task, add the plumbing on the pump side only: after processing a publish, if `settle.has_settled()` and the delivery core has no baseline yet, snapshot the cache into it.

Put that in a small helper next to the pump rather than inline, so `diagnostics_pump` stays under the 100-line limit:

```rust
/// Adopt the current cache as the baseline the first time the servers go
/// quiet, so a session's first flush reports what happened since startup
/// rather than everything the workspace already knew.
async fn take_baseline_once(
    settle: &ServerSettle,
    cache: &Mutex<NotificationCache>,
    delivery: &Mutex<DiagnosticsDelivery>,
    floors: &FloorTable,
) {
    if !settle.has_settled() {
        return;
    }
    let mut delivery = delivery.lock().await;
    if delivery.has_baseline() {
        return;
    }
    let cache = cache.lock().await;
    delivery.set_baseline(baseline_from_cache(&cache, floors));
}
```

`FloorTable` comes from task 3. `baseline_from_cache` iterates the cache's diagnostics entries and records `DiagnosticsDelivery::visible_hash` for each.

- [ ] **Step 6: Run the tests and commit**

Run: `cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --lib 2>&1 | tail -20`, then fmt, clippy and the workspace suite.

```bash
git -C /home/lev/Git/lev/mcpls-diagnostics add crates/mcpls-core/src/bridge/ crates/mcpls-core/src/lib.rs
git -C /home/lev/Git/lev/mcpls-diagnostics commit -m "feat(bridge): baseline diagnostics when servers go quiet" -m "Servers spawn in the background and rust-analyzer publishes for the
whole workspace once its initial analysis finishes, so a baseline taken
the first time a session appears is empty and every later publish looks
new. That is the workspace dump the baseline exists to prevent.

The pump now counts outstanding $/progress operations per server, which
it previously discarded, and the baseline is taken the first time they
all reach zero.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: The `get_new_diagnostics` tool

**Files:**
- Modify: `crates/mcpls-core/src/mcp/handlers.rs` (`BridgeContext`), `crates/mcpls-core/src/mcp/server.rs`, `crates/mcpls-core/src/mcp/tools.rs`, `crates/mcpls-core/src/lib.rs` (construct the delivery core and the floor table)
- Test: `crates/mcpls-core/tests/e2e/protocol_tests.rs`, `crates/mcpls-core/tests/ra_e2e.rs`

**Interfaces:**
- Consumes: everything from tasks 2 through 5.
- Produces: the `get_new_diagnostics` MCP tool. No parameters: the agent asks what is new, and narrowing it by path is what `get_cached_diagnostics` already does.

- [ ] **Step 1: Write the failing tests**

In `crates/mcpls-core/tests/e2e/protocol_tests.rs`, extend the `expected_names` array in `test_e2e_list_tools` with `"get_new_diagnostics"`. That test derives its count from the array, so no number needs changing.

In `crates/mcpls-core/tests/ra_e2e.rs`, following the existing fixture-driven tests:

```rust
#[tokio::test]
#[ignore = "requires rust-analyzer"]
async fn test_new_diagnostics_reports_a_break_once() {
    // Warm the workspace, break a caller by changing the callee's return
    // type, then assert the caller's file appears in a flush and does not
    // appear in the flush after it.
}
```

Fill the body following the existing rust-analyzer e2e tests: they already drive a real server against `tests/fixtures/rust_workspace` and wait for diagnostics. Reuse their wait helper rather than writing a new sleep.

- [ ] **Step 2: Run them and watch them fail**

Run:
```bash
cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --test integration_tests e2e::protocol_tests::test_e2e_list_tools 2>&1 | tail -20
```
Expected: the tool count is one short, naming `get_new_diagnostics` as missing.

- [ ] **Step 3: Wire the delivery core into the context**

Add to `BridgeContext` in `crates/mcpls-core/src/mcp/handlers.rs`:

```rust
    /// Per-session record of which diagnostics have already been delivered.
    ///
    /// Locked independently of `notification_cache`; a flush takes the cache
    /// lock only long enough to copy what it needs.
    pub delivery: Arc<Mutex<DiagnosticsDelivery>>,
    /// The severity floor each server answers to, resolved once at startup.
    pub floors: Arc<FloorTable>,
```

`FloorTable` was defined in task 3. Construct it and the delivery core in `serve_with` where `BridgeContext` is built, and pass the same `Arc`s into `PumpShared` so task 5's `take_baseline_once` reaches them.

- [ ] **Step 4: Add the tool**

In `crates/mcpls-core/src/mcp/tools.rs`:

```rust
/// Parameters for the `get_new_diagnostics` tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for draining diagnostics not yet delivered.")]
pub struct NewDiagnosticsParams {}
```

In `crates/mcpls-core/src/mcp/server.rs`, beside `get_cached_diagnostics`:

```rust
    /// Drain diagnostics that changed since the last call.
    #[tool(
        description = "Diagnostics that changed since you last asked, across every file the language servers report on. Returns nothing when nothing changed.",
        title = "New Diagnostics"
    )]
    async fn get_new_diagnostics(
        &self,
        Parameters(NewDiagnosticsParams {}): Parameters<NewDiagnosticsParams>,
    ) -> Result<String, McpError> {
        let session = SessionId::process_default();
        let snapshot = {
            let cache = self.context.notification_cache.lock().await;
            cache.diagnostics_snapshot()
        };
        let entries: Vec<FileEntry<'_>> = snapshot
            .iter()
            .filter_map(|(key, info, owner)| {
                let path = bridge::uri_to_path(&info.uri)?;
                Some(FileEntry {
                    key,
                    path: path.display().to_string(),
                    diagnostics: &info.diagnostics,
                    floor: self.context.floors.for_server(owner),
                })
            })
            .collect();

        let report = {
            let mut delivery = self.context.delivery.lock().await;
            delivery.flush(&session, &entries)
        };
        to_tool_result(Ok(report_to_payload(&report)))
    }
```

`NotificationCache` needs a `diagnostics_snapshot()` returning owned `(String, DiagnosticInfo, ServerId)` triples, so the cache lock is released before the flush. Add it next to `get_diagnostics` (`bridge/notifications.rs:719`).

`report_to_payload` converts a `FlushReport` into the serializable shape the tool returns: changed files with their diagnostics and per-file omitted count, cleared paths, and the total omitted count with a sentence saying the caps held them back and the next call will offer them again.

- [ ] **Step 5: Run the tests**

Run:
```bash
cargo build --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --workspace
cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml --workspace
cargo test --manifest-path /home/lev/Git/lev/mcpls-diagnostics/Cargo.toml -p mcpls-core --test integration_tests e2e:: -- --include-ignored
```

The e2e binary staleness guard added in `92b7f4c` will refuse a binary older than the sources, so build before running the e2e set.

Expected: PASS throughout, including the new rust-analyzer test.

- [ ] **Step 6: Document and commit**

Add `get_new_diagnostics` to `docs/user-guide/tools-reference.md` and to the README's Diagnostics table. Add a `### Added` changelog entry.

```bash
git -C /home/lev/Git/lev/mcpls-diagnostics add crates/mcpls-core/ docs/ README.md CHANGELOG.md
git -C /home/lev/Git/lev/mcpls-diagnostics commit -m "feat(mcp): add the get_new_diagnostics tool" -m "Drains what the language servers have reported since the caller last
asked, deduplicated per session and filtered by each server's severity
floor. Returns nothing when nothing changed, which is the common case
and the reason it is cheap to call often.

The cache lock is taken only to copy the snapshot, so a flush never
holds it across the deduplication work.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## What this plan does not build

Stated so a reviewer does not look for them:

- **The footer on the tools that write.** Stage B. Before the resync exists, an apply ends by telling every server to forget the files it wrote, so a footer would report nothing.
- **Automatic delivery.** Nothing here injects anything. The agent must call `get_new_diagnostics`. Stage C is where a hook drains it without being asked.
- **Coverage of external writes.** Measured and recorded in the spec: rust-analyzer sees an external write and will not check it, so no language is covered until stage C opens and saves changed files.

## Self-review

**Spec coverage.** A1 is task 1. The `[diagnostics]` table and per-server floor are task 2. A2's record, hash, floor and caps are task 3; its stale-publish rule is task 4; its baseline rule is task 5. A3's flush tool and session keying are tasks 3 and 6. Stage A's testing section maps onto the tests in tasks 1, 3 and 6. The one spec element deliberately not implemented is the `footer` key, recorded above under deliberate deviations.

**Placeholders.** Two test bodies are described rather than written: the pump tests in task 4 step 1 and the rust-analyzer test in task 6 step 1. Both are cases where the surrounding file already has a setup idiom that a fresh implementer should copy rather than reinvent, and both name the file and the helper to copy from. Every other step carries the code it needs.

**Type consistency.** `SessionId`, `FileEntry`, `FlushReport`, `ChangedFile`, `DiagnosticsDelivery`, `ServerSettle`, `FloorTable`, `SeverityFloor` and `DiagnosticsConfig` keep the same names and shapes from the task that defines them through every later use. `visible_hash` is used by both task 3's flush and task 5's baseline. `FloorTable` is defined in task 3 beside the delivery core, used by task 5's baseline and constructed by task 6, so no task references a type a later task defines.
