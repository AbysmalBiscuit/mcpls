//! Document symbols and workspace symbol search handlers.

use std::collections::HashSet;

use lsp_types::{
    DocumentSymbol, DocumentSymbolParams, PartialResultParams, TextDocumentIdentifier,
    WorkDoneProgressParams, WorkspaceSymbolParams as LspWorkspaceSymbolParams,
};

use super::dto::{DocumentSymbolsResult, Location, Symbol, WorkspaceSymbol, WorkspaceSymbolResult};
use super::encoding_ctx::EncodingCtx;
use super::routing::FIRST_SPAWN_BUDGET;
use super::{ServerLifecycle, Translator};
use crate::bridge::lock_std;
use crate::config::{NoServerReason, ToolKind};
use crate::error::{Error, Result};

/// Validate parameters for `handle_workspace_symbol`.
fn validate_workspace_symbol_params(query: &str, kind_filter: Option<&str>) -> Result<()> {
    const MAX_QUERY_LENGTH: usize = 1000;
    const VALID_SYMBOL_KINDS: &[&str] = &[
        "File",
        "Module",
        "Namespace",
        "Package",
        "Class",
        "Method",
        "Property",
        "Field",
        "Constructor",
        "Enum",
        "Interface",
        "Function",
        "Variable",
        "Constant",
        "String",
        "Number",
        "Boolean",
        "Array",
        "Object",
        "Key",
        "Null",
        "EnumMember",
        "Struct",
        "Event",
        "Operator",
        "TypeParameter",
    ];

    if query.len() > MAX_QUERY_LENGTH {
        return Err(Error::InvalidToolParams(format!(
            "Query too long: {} bytes (max {MAX_QUERY_LENGTH})",
            query.len()
        )));
    }

    if let Some(kind) = kind_filter
        && !VALID_SYMBOL_KINDS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(kind))
    {
        return Err(Error::InvalidToolParams(format!(
            "Invalid kind_filter: '{kind}'. Valid values: {VALID_SYMBOL_KINDS:?}"
        )));
    }

    Ok(())
}

/// Convert LSP document symbol to MCP symbol. `uri` is the queried
/// document's own URI: nested `DocumentSymbol` entries have no URI of their
/// own, since `textDocument/documentSymbol` is always scoped to one file.
///
/// Boxed because it recurses through `children` and an `async fn` cannot
/// call itself directly (its future would have unbounded size).
fn convert_document_symbol<'a>(
    symbol: DocumentSymbol,
    ctx: &'a EncodingCtx,
    uri: &'a lsp_types::Uri,
) -> futures::future::BoxFuture<'a, Symbol> {
    Box::pin(async move {
        let range = ctx.normalize_range(uri, symbol.range).await;
        let selection_range = ctx.normalize_range(uri, symbol.selection_range).await;
        let children = match symbol.children {
            Some(children) => {
                let mut result = Vec::with_capacity(children.len());
                for child in children {
                    result.push(convert_document_symbol(child, ctx, uri).await);
                }
                Some(result)
            }
            None => None,
        };

        Symbol {
            name: symbol.name,
            kind: format!("{:?}", symbol.kind),
            range,
            selection_range,
            children,
        }
    })
}

impl Translator {
    /// Handle document symbols request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `documentSymbolProvider` support.
    pub async fn handle_document_symbols(
        &self,
        file_path: String,
    ) -> Result<DocumentSymbolsResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::DocumentSymbols,
                "documentSymbolProvider",
                |caps| {
                    matches!(
                        caps.document_symbol_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let response_uri = uri.clone();

        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<lsp_types::DocumentSymbolResponse> = client
            .request(
                "textDocument/documentSymbol",
                params,
                client.request_timeout(),
            )
            .await?;

        let symbols = match response {
            Some(lsp_types::DocumentSymbolResponse::Flat(symbols)) => {
                let mut result = Vec::with_capacity(symbols.len());
                for sym in symbols {
                    let range = ctx
                        .normalize_range(&sym.location.uri, sym.location.range)
                        .await;
                    let selection_range = range.clone();
                    result.push(Symbol {
                        name: sym.name,
                        kind: format!("{:?}", sym.kind),
                        range,
                        selection_range,
                        children: None,
                    });
                }
                result
            }
            Some(lsp_types::DocumentSymbolResponse::Nested(symbols)) => {
                let mut result = Vec::with_capacity(symbols.len());
                for sym in symbols {
                    result.push(convert_document_symbol(sym, &ctx, &response_uri).await);
                }
                result
            }
            None => vec![],
        };

        Ok(DocumentSymbolsResult { symbols })
    }

    /// Handle workspace symbol search.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, no server is configured, or
    /// the routed server does not advertise `workspaceSymbolProvider` support.
    pub async fn handle_workspace_symbol(
        &self,
        query: String,
        kind_filter: Option<String>,
        limit: u32,
    ) -> Result<WorkspaceSymbolResult> {
        validate_workspace_symbol_params(&query, kind_filter.as_deref())?;

        let mut excluded = HashSet::new();
        let server_id = loop {
            let candidate = lock_std(&self.router)
                .resolve_any_excluding(ToolKind::WorkspaceSymbols, &excluded)
                .cloned()
                .map_err(|reason| match reason {
                    NoServerReason::NothingRegistered => {
                        if self.lifecycles().is_empty() {
                            Error::NoServerConfigured
                        } else {
                            Error::WorkspaceServersInitializing
                        }
                    }
                    NoServerReason::NoClaimant => Error::NoServerForWorkspaceTool {
                        tool: ToolKind::WorkspaceSymbols,
                    },
                })?;
            if self.lifecycle_of(&candidate) == Some(ServerLifecycle::NotInstalled) {
                excluded.insert(candidate);
            } else {
                break candidate;
            }
        };
        if let Err(err) = self
            .ensure_server(&server_id, Some(FIRST_SPAWN_BUDGET))
            .await
        {
            return Err(match err {
                Error::ServerInitializing { .. } => Error::WorkspaceServersInitializing,
                other => other,
            });
        }
        let client = lock_std(&self.lsp_clients).get(&server_id).cloned();
        let client = client.ok_or_else(|| match self.lifecycle_of(&server_id) {
            Some(ServerLifecycle::Idle | ServerLifecycle::Starting) => Error::ServerInitializing {
                server_id: server_id.clone(),
            },
            Some(ServerLifecycle::NotInstalled) => Error::ServerUnavailable {
                server_id: server_id.clone(),
                reason: "command not found".to_string(),
            },
            Some(ServerLifecycle::Failed) => Error::ServerUnavailable {
                server_id: server_id.clone(),
                reason: "failed to start".to_string(),
            },
            Some(ServerLifecycle::Running) | None => Error::NoServerConfigured,
        })?;
        self.require_capability(&server_id, "workspaceSymbolProvider", |caps| {
            matches!(
                caps.workspace_symbol_provider,
                Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
            )
        })?;

        let params = LspWorkspaceSymbolParams {
            query,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<Vec<lsp_types::SymbolInformation>> = client
            .request("workspace/symbol", params, client.request_timeout())
            .await?;

        let ctx = self.encoding_ctx(&server_id);
        let mut symbols: Vec<WorkspaceSymbol> = Vec::new();
        for sym in response.unwrap_or_default() {
            let range = ctx
                .normalize_range(&sym.location.uri, sym.location.range)
                .await;
            symbols.push(WorkspaceSymbol {
                name: sym.name,
                kind: format!("{:?}", sym.kind),
                location: Location {
                    uri: sym.location.uri.to_string(),
                    range,
                },
                container_name: sym.container_name,
            });
        }

        // Apply kind filter if specified
        if let Some(kind) = kind_filter {
            symbols.retain(|s| s.kind.eq_ignore_ascii_case(&kind));
        }

        // Limit results
        symbols.truncate(limit as usize);

        Ok(WorkspaceSymbolResult { symbols })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::time::timeout;

    use super::*;
    use crate::bridge::translator::testing::{
        read_framed_message, translator_with_capabilities, write_response,
    };
    use crate::config::{ServerId, ToolRouter};

    fn workspace_symbol_server(language_id: &str, name: &str) -> crate::config::LspServerConfig {
        crate::config::LspServerConfig {
            language_id: language_id.to_string(),
            command: "fake-lsp".to_string(),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec![],
            initialization_options: None,
            timeout_seconds: 30,
            spawn: None,
            request_timeout_seconds: 30,
            heuristics: None,
            name: Some(name.to_string()),
            handles: Some(vec![ToolKind::WorkspaceSymbols]),
            diagnostics_severity: None,
        }
    }

    #[tokio::test]
    async fn test_handle_workspace_symbol_no_server() {
        let translator = Translator::new();
        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;
        assert!(matches!(result, Err(Error::NoServerConfigured)));
    }

    /// Lifecycle membership distinguishes a configured server from an empty
    /// setup when no workspace-symbol route is available yet.
    #[tokio::test]
    async fn test_handle_workspace_symbol_reports_initializing_while_server_is_starting() {
        let translator = Translator::new();
        translator.set_lifecycle(&ServerId::from("pyright"), ServerLifecycle::Starting);

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;
        assert!(matches!(result, Err(Error::WorkspaceServersInitializing)));
    }

    /// #242 regression: a server *is* configured and running, it just
    /// doesn't claim `workspace_symbols` and there is no catch-all -- the
    /// error must name the tool rather than collapse into the generic
    /// "no LSP server configured" message a client would also see if
    /// nothing were running at all.
    #[tokio::test]
    async fn test_handle_workspace_symbol_no_claimant_names_tool() {
        let configs = vec![crate::config::LspServerConfig {
            language_id: "python".to_string(),
            command: "pyright-langserver".to_string(),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec![],
            initialization_options: None,
            timeout_seconds: 30,
            spawn: None,
            request_timeout_seconds: 30,
            heuristics: None,
            name: Some("pyright".to_string()),
            handles: Some(vec![ToolKind::Hover]),
            diagnostics_severity: None,
        }];
        let router = ToolRouter::from_configs(&configs).unwrap();
        let translator = Translator::new().with_router(router);

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;
        assert!(matches!(
            result,
            Err(Error::NoServerForWorkspaceTool {
                tool: ToolKind::WorkspaceSymbols
            })
        ));
    }

    #[tokio::test]
    async fn workspace_symbol_skips_missing_binary_and_starts_only_the_next_claimant() {
        let configs = [
            workspace_symbol_server("python", "python-missing"),
            workspace_symbol_server("typescript", "typescript-idle"),
            workspace_symbol_server("lua", "lua-unused"),
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();
        let translator = Arc::new(Translator::new().with_router(router));
        translator.set_self_handle(Arc::downgrade(&translator));

        let missing_id = ServerId::from("python-missing");
        let selected_id = ServerId::from("typescript-idle");
        let unused_id = ServerId::from("lua-unused");
        translator.set_lifecycle(&missing_id, ServerLifecycle::NotInstalled);
        translator.set_lifecycle(&selected_id, ServerLifecycle::Idle);
        translator.set_lifecycle(&unused_id, ServerLifecycle::Idle);

        let result = translator
            .handle_workspace_symbol("query".to_string(), None, 100)
            .await;

        assert!(matches!(
            result,
            Err(Error::ServerUnavailable { server_id, .. }) if server_id == selected_id
        ));
        assert_eq!(
            translator.lifecycle_of(&missing_id),
            Some(ServerLifecycle::NotInstalled)
        );
        assert_eq!(
            translator.lifecycle_of(&selected_id),
            Some(ServerLifecycle::Failed)
        );
        assert_eq!(
            translator.lifecycle_of(&unused_id),
            Some(ServerLifecycle::Idle)
        );
    }

    #[tokio::test]
    async fn test_handle_document_symbols_flat_response_selection_range_matches_range() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, mut server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities {
                document_symbol_provider: Some(lsp_types::OneOf::Left(true)),
                ..Default::default()
            },
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move { translator.handle_document_symbols(path_str).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let symbol_request = read_framed_message(&mut wire).await;
        assert_eq!(symbol_request["method"], "textDocument/documentSymbol");

        write_response(
            &mut server.read_half_stdin,
            &symbol_request["id"],
            serde_json::json!([{
                "name": "main",
                "kind": 12,
                "location": {
                    "uri": "file:///main.rs",
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 12},
                    },
                },
            }]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .expect("flat document symbol response should succeed");

        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].range, result.symbols[0].selection_range);
    }
}
