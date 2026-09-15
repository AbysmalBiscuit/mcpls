//! Explicit per-tool routing (#174).
//!
//! `language_id` alone is not a unique server identity: two servers can
//! share one language (e.g. pyright and pylsp both for `python`), each
//! handling a different subset of MCP tools. This module defines the typed
//! vocabulary for that routing — [`ServerId`], [`ToolKind`] — and
//! [`ToolRouter`], which resolves `(language, tool)` to the server that
//! should handle it.
//!
//! `ToolKind` lives here, in `config`, rather than in `mcp` (which is where
//! its variants are semantically drawn from) to keep `config` a leaf module:
//! `mcp` and `bridge` both depend on `config`, so putting `ToolKind` in `mcp`
//! would create a `config -> mcp -> bridge -> config` cycle. When a new
//! routable MCP tool is added, extend [`ToolKind::ALL`] here.

use std::collections::{HashMap, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::server::LspServerConfig;
use crate::error::{Error, Result};

/// Unique identity of a configured LSP server within a workspace.
///
/// Derived from [`LspServerConfig::id`]: a server's explicit `name` if set,
/// otherwise its `language_id`. This is the key used throughout the bridge
/// layer (`Translator::lsp_clients`, `lsp_servers`, notification receivers)
/// instead of a raw language string, so two servers sharing a language no
/// longer silently overwrite each other.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ServerId(String);

impl ServerId {
    /// Borrow the identity as a plain string, e.g. for log messages or map
    /// lookups against external APIs that expect `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ServerId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for ServerId {
    fn from(id: &str) -> Self {
        Self(id.to_string())
    }
}

/// A routable MCP tool: every MCP tool that dispatches a request to a
/// specific LSP server via [`ToolRouter`].
///
/// Cache-only tools (`get_cached_diagnostics`, `get_new_diagnostics`,
/// `get_server_logs`, `get_server_messages`) are deliberately excluded —
/// they never reach a client directly, so they have nothing to route.
///
/// `CallHierarchy` covers `prepare`, `incoming_calls`, and `outgoing_calls`
/// as a single route: the opaque item returned by `prepare` is only
/// meaningful to the server that produced it, and the incoming/outgoing
/// handlers never call `ensure_open` themselves — they rely on `prepare`
/// having already synced the document to the *same* server.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ToolKind;
///
/// assert_eq!(ToolKind::Hover.as_str(), "hover");
/// assert_eq!(ToolKind::ALL.len(), 15);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// `textDocument/hover`.
    Hover,
    /// `textDocument/definition`.
    Definition,
    /// `textDocument/typeDefinition`.
    TypeDefinition,
    /// `textDocument/implementation`.
    Implementation,
    /// `textDocument/references`.
    References,
    /// `textDocument/diagnostic` (pull) and the `publishDiagnostics` cache filter.
    Diagnostics,
    /// `textDocument/rename`.
    Rename,
    /// `textDocument/completion`.
    Completions,
    /// `textDocument/signatureHelp`.
    SignatureHelp,
    /// `textDocument/documentSymbol`.
    DocumentSymbols,
    /// `workspace/symbol`.
    WorkspaceSymbols,
    /// `textDocument/formatting`.
    FormatDocument,
    /// `textDocument/codeAction`.
    CodeActions,
    /// `textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`, `callHierarchy/outgoingCalls`.
    CallHierarchy,
    /// `textDocument/inlayHint`.
    InlayHints,
}

impl ToolKind {
    /// Every routable tool, in a fixed order. Used to compute the §5
    /// coverage warning and to build error messages that enumerate tools.
    pub const ALL: [Self; 15] = [
        Self::Hover,
        Self::Definition,
        Self::TypeDefinition,
        Self::Implementation,
        Self::References,
        Self::Diagnostics,
        Self::Rename,
        Self::Completions,
        Self::SignatureHelp,
        Self::DocumentSymbols,
        Self::WorkspaceSymbols,
        Self::FormatDocument,
        Self::CodeActions,
        Self::CallHierarchy,
        Self::InlayHints,
    ];

    /// The `snake_case` name used in config `handles` lists and error messages.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Hover => "hover",
            Self::Definition => "definition",
            Self::TypeDefinition => "type_definition",
            Self::Implementation => "implementation",
            Self::References => "references",
            Self::Diagnostics => "diagnostics",
            Self::Rename => "rename",
            Self::Completions => "completions",
            Self::SignatureHelp => "signature_help",
            Self::DocumentSymbols => "document_symbols",
            Self::WorkspaceSymbols => "workspace_symbols",
            Self::FormatDocument => "format_document",
            Self::CodeActions => "code_actions",
            Self::CallHierarchy => "call_hierarchy",
            Self::InlayHints => "inlay_hints",
        }
    }
}

impl std::fmt::Display for ToolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Describe a `[[lsp_servers]]` entry for use in error messages that must let
/// a user tell apart two entries sharing the same [`ServerId`] — the id
/// alone is useless there, since it's exactly what collided.
///
/// Deliberately does not include a positional index: [`ToolRouter::from_configs`]
/// only ever sees the post-heuristics *applicable* subset for a given
/// workspace, not the raw `[[lsp_servers]]` array, so a printed index would
/// usually name the wrong TOML entry (misleading, worse than omitting it).
/// `command`/`args` distinguish the entries instead; when two entries are
/// truly identical in every visible field, the description is the same for
/// both halves, which is an honest reflection of the ambiguity.
fn describe_entry(cfg: &LspServerConfig) -> String {
    if cfg.args.is_empty() {
        format!("language '{}', command '{}'", cfg.language_id, cfg.command)
    } else {
        format!(
            "language '{}', command '{}', args {:?}",
            cfg.language_id, cfg.command, cfg.args
        )
    }
}

/// Per-language routing table: which server handles which tool.
#[derive(Debug, Default)]
struct LanguageRoutes {
    /// Tools explicitly claimed via a server's `handles` list.
    explicit: HashMap<ToolKind, ServerId>,
    /// The single server (if any) that omitted `handles` — serves every
    /// tool not explicitly claimed by another server for this language.
    default: Option<ServerId>,
}

/// Why [`ToolRouter::resolve_any`] could not find a server for a
/// workspace-wide tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoServerReason {
    /// No applicable server is configured in this workspace. The router is
    /// built from the applicable configs, so this also means no server has
    /// registered yet.
    NothingRegistered,
    /// At least one server is configured, but none explicitly claims the
    /// requested tool and none is a catch-all.
    NoClaimant,
}

/// Resolves `(language, tool)` to the [`ServerId`] that should handle it.
///
/// Built once at startup from the applicable server configs. Server
/// lifecycles determine whether a configured route is available.
#[derive(Debug, Default)]
pub struct ToolRouter {
    by_language: HashMap<String, LanguageRoutes>,
    /// Config declaration order, used by `resolve_any` for a deterministic
    /// choice among candidates.
    order: Vec<ServerId>,
}

impl ToolRouter {
    /// Build a router from the configs applicable in this workspace,
    /// enforcing the workspace-scoped validation rules:
    ///
    /// 1. No two applicable servers (in any language) may share a
    ///    [`ServerId`] — it is the key of every map keyed by server identity.
    /// 2. No two applicable servers for one language may both omit `handles`
    ///    (two catch-alls).
    /// 3. No tool may be claimed via `handles` by two applicable servers of
    ///    the same language.
    ///
    /// Also emits a `tracing::warn!` for any language whose union of
    /// `handles` claims is partial and has no catch-all server, naming the
    /// tools nobody will serve.
    ///
    /// # Errors
    ///
    /// Returns `Error::InvalidConfig` naming the conflicting entries if any
    /// of the three rules above is violated.
    pub fn from_configs<'a, I>(cfgs: I) -> Result<Self>
    where
        I: IntoIterator<Item = &'a LspServerConfig>,
    {
        let mut by_language: HashMap<String, LanguageRoutes> = HashMap::new();
        let mut order: Vec<ServerId> = Vec::new();
        let mut seen_ids: HashMap<ServerId, String> = HashMap::new();

        for cfg in cfgs {
            let id = cfg.id();

            if let Some(prev_description) = seen_ids.get(&id) {
                return Err(Error::InvalidConfig(format!(
                    "duplicate server id '{id}' in this workspace (used by both an entry with \
                     {prev_description} and one with {}); add a unique `name` to each \
                     `[[lsp_servers]]` entry",
                    describe_entry(cfg)
                )));
            }
            seen_ids.insert(id.clone(), describe_entry(cfg));
            order.push(id.clone());

            let routes = by_language.entry(cfg.language_id.clone()).or_default();

            match &cfg.handles {
                None => {
                    if let Some(existing) = &routes.default {
                        return Err(Error::InvalidConfig(format!(
                            "language '{}' has two catch-all servers ('{existing}' and '{id}'); \
                             at most one server per language may omit `handles`",
                            cfg.language_id
                        )));
                    }
                    routes.default = Some(id);
                }
                Some(tools) => {
                    for tool in tools {
                        if let Some(existing) = routes.explicit.get(tool) {
                            return Err(Error::InvalidConfig(format!(
                                "tool '{tool}' for language '{}' is claimed by both \
                                 '{existing}' and '{id}'",
                                cfg.language_id
                            )));
                        }
                        routes.explicit.insert(*tool, id.clone());
                    }
                }
            }
        }

        // Deliberately untested (M4): asserting on `tracing` output would
        // need a subscriber/capture dev-dependency this crate doesn't
        // otherwise pull in. Verified by inspection instead; the `uncovered`
        // computation itself is exercised indirectly by every `resolve`
        // test above that checks an unclaimed tool returns `None`.
        for (language, routes) in &by_language {
            if routes.default.is_none() {
                let uncovered: Vec<&str> = ToolKind::ALL
                    .iter()
                    .filter(|t| !routes.explicit.contains_key(t))
                    .map(ToolKind::as_str)
                    .collect();
                if !uncovered.is_empty() {
                    tracing::warn!(
                        "language '{language}' has no catch-all server and does not claim: {}",
                        uncovered.join(", ")
                    );
                }
            }
        }

        Ok(Self { by_language, order })
    }

    /// Build a router where every entry is a catch-all for its language.
    ///
    /// Test helper: takes `(id, language)` pairs rather than a single entry
    /// because some tests (e.g. the `typescript`/`typescriptreact` exact-match
    /// preference) need two catch-alls registered at once.
    #[must_use]
    pub fn catch_all<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (ServerId, String)>,
    {
        let mut by_language: HashMap<String, LanguageRoutes> = HashMap::new();
        let mut order = Vec::new();
        for (id, language) in entries {
            order.push(id.clone());
            by_language.entry(language).or_default().default = Some(id);
        }
        Self { by_language, order }
    }

    /// Resolve the server that should handle `tool` for `language_id`.
    ///
    /// Explicit claims win over the language's catch-all; if neither exists,
    /// returns `None`.
    #[must_use]
    pub fn resolve(&self, language_id: &str, tool: ToolKind) -> Option<&ServerId> {
        let routes = self.by_language.get(language_id)?;
        routes.explicit.get(&tool).or(routes.default.as_ref())
    }

    /// Return the configured catch-all server for `language_id`, if present.
    #[must_use]
    pub fn catch_all_for_language(&self, language_id: &str) -> Option<&ServerId> {
        self.by_language
            .get(language_id)
            .and_then(|routes| routes.default.as_ref())
    }

    /// Resolve the first configured server for a workspace-wide tool.
    /// Explicit claims precede catch-alls, in declaration order.
    ///
    /// # Errors
    /// Returns [`NoServerReason::NothingRegistered`] or [`NoServerReason::NoClaimant`].
    pub fn resolve_any(&self, tool: ToolKind) -> std::result::Result<&ServerId, NoServerReason> {
        self.resolve_any_excluding(tool, &HashSet::new())
    }

    /// Resolve a workspace-wide tool while skipping unavailable servers.
    /// Explicit claims precede catch-alls, in declaration order.
    ///
    /// # Errors
    /// Returns [`NoServerReason::NothingRegistered`] or [`NoServerReason::NoClaimant`].
    pub fn resolve_any_excluding(
        &self,
        tool: ToolKind,
        excluded: &HashSet<ServerId>,
    ) -> std::result::Result<&ServerId, NoServerReason> {
        let claims_explicitly = |id: &ServerId| {
            self.by_language
                .values()
                .any(|r| r.explicit.get(&tool) == Some(id))
        };
        let is_catch_all = |id: &ServerId| {
            self.by_language
                .values()
                .any(|r| r.default.as_ref() == Some(id))
        };

        self.order
            .iter()
            .find(|id| !excluded.contains(*id) && claims_explicitly(id))
            .or_else(|| {
                self.order
                    .iter()
                    .find(|id| !excluded.contains(*id) && is_catch_all(id))
            })
            .ok_or(if self.order.is_empty() {
                NoServerReason::NothingRegistered
            } else {
                NoServerReason::NoClaimant
            })
    }

    /// Whether `language_id` has a configured catch-all or explicit claim.
    #[must_use]
    pub fn has_language(&self, language_id: &str) -> bool {
        self.by_language
            .get(language_id)
            .is_some_and(|r| r.default.is_some() || !r.explicit.is_empty())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cfg(
        language_id: &str,
        name: Option<&str>,
        handles: Option<Vec<ToolKind>>,
    ) -> LspServerConfig {
        LspServerConfig {
            language_id: language_id.to_string(),
            command: "cmd".to_string(),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec![],
            initialization_options: None,
            timeout_seconds: 30,
            spawn: None,
            request_timeout_seconds: 30,
            heuristics: None,
            name: name.map(str::to_string),
            handles,
            diagnostics_severity: None,
        }
    }

    #[test]
    fn test_resolve_explicit_wins_over_catch_all() {
        let configs = vec![
            cfg("python", Some("pyright"), Some(vec![ToolKind::Hover])),
            cfg("python", Some("pylsp"), None),
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();
        assert_eq!(
            router.resolve("python", ToolKind::Hover),
            Some(&ServerId::from("pyright"))
        );
        assert_eq!(
            router.resolve("python", ToolKind::Diagnostics),
            Some(&ServerId::from("pylsp"))
        );
    }

    #[test]
    fn test_resolve_no_catch_all_unclaimed_is_none() {
        let configs = vec![cfg("python", Some("pyright"), Some(vec![ToolKind::Hover]))];
        let router = ToolRouter::from_configs(&configs).unwrap();
        assert_eq!(router.resolve("python", ToolKind::Diagnostics), None);
    }

    #[test]
    fn test_resolve_any_explicit_claimer_beats_catch_all_declared_first() {
        let configs = vec![
            cfg("python", Some("python-narrow"), Some(vec![ToolKind::Hover])),
            cfg("rust", Some("rust-catch-all"), None),
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();
        // Neither server explicitly claims WorkspaceSymbols, so the rust
        // catch-all must win over the narrowly-scoped python server, even
        // though python was declared first.
        assert_eq!(
            router.resolve_any(ToolKind::WorkspaceSymbols),
            Ok(&ServerId::from("rust-catch-all"))
        );
    }

    #[test]
    fn test_resolve_any_prefers_explicit_claimer_over_catch_all() {
        let configs = vec![
            cfg("rust", Some("rust-catch-all"), None),
            cfg(
                "python",
                Some("python-explicit"),
                Some(vec![ToolKind::WorkspaceSymbols]),
            ),
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();
        assert_eq!(
            router.resolve_any(ToolKind::WorkspaceSymbols),
            Ok(&ServerId::from("python-explicit"))
        );
    }

    #[test]
    fn test_from_configs_rejects_duplicate_server_id_across_languages() {
        let configs = vec![
            cfg("python", None, None),
            cfg("typescript", Some("python"), None),
        ];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn test_from_configs_duplicate_server_id_error_distinguishes_entries() {
        // Two `[[lsp_servers]]` entries sharing `language_id = "rust"` with
        // neither setting `name`: both resolve to the same ServerId, which
        // used to make the error message name both conflicting halves
        // identically ("used by both the 'rust' and 'rust' language
        // entries"). The message must let a user tell the two entries apart.
        let configs = vec![
            LspServerConfig {
                language_id: "rust".to_string(),
                command: "rust-analyzer".to_string(),
                args: vec![],
                env: HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 30,
                spawn: None,
                request_timeout_seconds: 30,
                heuristics: None,
                name: None,
                handles: None,
                diagnostics_severity: None,
            },
            LspServerConfig {
                language_id: "rust".to_string(),
                command: "rust-analyzer".to_string(),
                args: vec!["--dummy-second-instance".to_string()],
                env: HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 30,
                spawn: None,
                request_timeout_seconds: 30,
                heuristics: None,
                name: None,
                handles: None,
                diagnostics_severity: None,
            },
        ];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        let Error::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err:?}");
        };
        // Must not print a positional index: `from_configs` only ever sees
        // the post-heuristics applicable subset, so any "entry #N" would
        // usually name the wrong `[[lsp_servers]]` array position.
        assert!(!msg.contains("entry #"), "message was: {msg}");
        assert!(msg.contains("rust-analyzer"), "message was: {msg}");
        assert!(
            msg.contains("--dummy-second-instance"),
            "message was: {msg}"
        );
    }

    #[test]
    fn test_from_configs_duplicate_server_id_error_identical_entries_still_reports() {
        // When two colliding entries are identical in every visible field,
        // there's nothing left to distinguish them by; the message should
        // still name the collision (both halves read the same) rather than
        // fabricate a misleading index.
        let configs = vec![cfg("rust", None, None), cfg("rust", None, None)];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        let Error::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err:?}");
        };
        assert!(!msg.contains("entry #"), "message was: {msg}");
        assert!(
            msg.contains("duplicate server id 'rust'"),
            "message was: {msg}"
        );
    }

    #[test]
    fn test_from_configs_rejects_two_catch_alls() {
        let configs = vec![
            cfg("python", Some("a"), None),
            cfg("python", Some("b"), None),
        ];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn test_from_configs_rejects_duplicate_tool_claim() {
        let configs = vec![
            cfg("python", Some("a"), Some(vec![ToolKind::Hover])),
            cfg("python", Some("b"), Some(vec![ToolKind::Hover])),
        ];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn test_catch_all_for_language_returns_default_route() {
        let configs = vec![cfg("python", Some("pylsp"), None)];
        let router = ToolRouter::from_configs(&configs).unwrap();

        assert_eq!(
            router.catch_all_for_language("python"),
            Some(&ServerId::from("pylsp"))
        );
        assert_eq!(router.catch_all_for_language("rust"), None);
    }

    #[test]
    fn test_resolve_any_no_claimant_does_not_fall_back_to_arbitrary_server() {
        // A single narrowly-scoped server that does not claim WorkspaceSymbols
        // and has no catch-all anywhere must not be silently conscripted for
        // it -- that would violate its explicit `handles` declaration.
        let configs = vec![cfg("python", Some("pyright"), Some(vec![ToolKind::Hover]))];
        let router = ToolRouter::from_configs(&configs).unwrap();
        assert_eq!(
            router.resolve_any(ToolKind::WorkspaceSymbols),
            Err(NoServerReason::NoClaimant)
        );
    }

    #[test]
    fn test_has_language() {
        let configs = vec![cfg("rust", None, None)];
        let router = ToolRouter::from_configs(&configs).unwrap();
        assert!(router.has_language("rust"));
        assert!(!router.has_language("python"));
    }

    #[test]
    fn test_catch_all_helper_registers_two_entries() {
        let router = ToolRouter::catch_all([
            (ServerId::from("ts"), "typescript".to_string()),
            (ServerId::from("tsx"), "typescriptreact".to_string()),
        ]);
        assert_eq!(
            router.resolve("typescript", ToolKind::Hover),
            Some(&ServerId::from("ts"))
        );
        assert_eq!(
            router.resolve("typescriptreact", ToolKind::Hover),
            Some(&ServerId::from("tsx"))
        );
    }

    #[test]
    fn test_tool_kind_as_str_and_all_len() {
        assert_eq!(ToolKind::Hover.as_str(), "hover");
        assert_eq!(ToolKind::CallHierarchy.as_str(), "call_hierarchy");
        assert_eq!(ToolKind::ALL.len(), 15);
    }

    #[test]
    fn test_server_id_display_and_as_str() {
        let id = ServerId::from("pyright");
        assert_eq!(id.as_str(), "pyright");
        assert_eq!(id.to_string(), "pyright");
    }
}
