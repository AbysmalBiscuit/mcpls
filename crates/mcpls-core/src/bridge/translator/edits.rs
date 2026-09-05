//! Rename, format-document, and code-actions handlers.

use lsp_types::{
    DocumentChanges as LspDocumentChanges, DocumentFormattingParams, FormattingOptions, OneOf,
    OptionalVersionedTextDocumentIdentifier, PartialResultParams, RenameParams as LspRenameParams,
    TextDocumentEdit, TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
    WorkspaceEdit,
};
use tokio::sync::mpsc;
use tracing::warn;

use super::Translator;
use super::diagnostics::diagnostic_to_mcp;
use super::dto::{
    ApplyCodeActionResult, CodeAction, CodeActionsResult, CommandDescription, DocumentChanges,
    FormatDocumentResult, RenameResult, ResourceOperation, TextEdit, WorkspaceEditDescription,
    resource_operations_from_plan,
};
use super::encoding_ctx::EncodingCtx;
use super::routing::{MAX_POSITION_VALUE, MAX_RANGE_LINES};
use crate::bridge::apply::{Applier, ApplySummary, EditPlan, Operation};
use crate::config::{ServerId, ToolKind};
use crate::error::{Error, Result};
use crate::lsp::LspClient;

/// How many inbound `workspace/applyEdit` requests may queue while a
/// command's edits are applied one at a time. Deeper than any server is
/// known to need, and a server that overruns it simply waits.
const INBOUND_EDIT_QUEUE_DEPTH: usize = 4;

/// Convert LSP range to MCP range (0-based to 1-based).
/// Validate parameters for `handle_code_actions`.
fn validate_code_action_params(
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
    kind_filter: Option<&str>,
) -> Result<()> {
    const VALID_ACTION_KINDS: &[&str] = &[
        "quickfix",
        "refactor",
        "refactor.extract",
        "refactor.inline",
        "refactor.rewrite",
        "source",
        "source.organizeImports",
    ];

    if let Some(kind) = kind_filter
        && !VALID_ACTION_KINDS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(kind))
    {
        return Err(Error::InvalidToolParams(format!(
            "Invalid kind_filter: '{kind}'. Valid values: {VALID_ACTION_KINDS:?}"
        )));
    }

    if start_line < 1 || start_character < 1 || end_line < 1 || end_character < 1 {
        return Err(Error::InvalidToolParams(
            "Line and character positions must be >= 1".to_string(),
        ));
    }

    if start_line > MAX_POSITION_VALUE
        || start_character > MAX_POSITION_VALUE
        || end_line > MAX_POSITION_VALUE
        || end_character > MAX_POSITION_VALUE
    {
        return Err(Error::InvalidToolParams(format!(
            "Position values must be <= {MAX_POSITION_VALUE}"
        )));
    }

    if end_line.saturating_sub(start_line) > MAX_RANGE_LINES {
        return Err(Error::InvalidToolParams(format!(
            "Range size must be <= {MAX_RANGE_LINES} lines"
        )));
    }

    if start_line > end_line || (start_line == end_line && start_character > end_character) {
        return Err(Error::InvalidToolParams(
            "Start position must be before or equal to end position".to_string(),
        ));
    }

    Ok(())
}

/// Maximum length, in bytes, of a `rename_symbol` `new_name` parameter.
///
/// `new_name` is forwarded to the routed LSP server as-is with no inherent
/// bound of its own -- unlike `workspace_symbol_search`'s `query` (see
/// `validate_workspace_symbol_params`), it previously relied entirely on
/// outer transport limits (#309). No real identifier approaches this length
/// in any language mcpls targets.
pub(super) const MAX_NEW_NAME_LENGTH: usize = 1_000;

/// Validate parameters for `handle_rename`.
fn validate_rename_params(new_name: &str) -> Result<()> {
    if new_name.len() > MAX_NEW_NAME_LENGTH {
        return Err(Error::InvalidToolParams(format!(
            "new_name too long: {} bytes (max {MAX_NEW_NAME_LENGTH})",
            new_name.len()
        )));
    }
    Ok(())
}

/// Convert LSP code action to MCP code action. `uri` is the queried
/// document's own URI, used for the action's `diagnostics` (always scoped to
/// the requested document); `edit.changes` carries its own per-file URIs.
async fn convert_code_action(
    action: lsp_types::CodeAction,
    ctx: &EncodingCtx,
    uri: &lsp_types::Uri,
) -> CodeAction {
    let diagnostics = match action.diagnostics {
        Some(diags) => {
            let mut result = Vec::with_capacity(diags.len());
            for d in &diags {
                result.push(diagnostic_to_mcp(d, ctx, uri).await);
            }
            result
        }
        None => Vec::new(),
    };

    let edit = match action.edit {
        Some(workspace_edit) => {
            let plan = EditPlan::from_workspace_edit(workspace_edit).ok();
            match plan {
                Some(plan) => {
                    let mut changes = Vec::new();
                    for operation in plan.operations() {
                        if let Operation::Edit {
                            uri: edit_uri,
                            edits,
                            ..
                        } = operation
                        {
                            let mut text_edits = Vec::with_capacity(edits.len());
                            for e in edits {
                                text_edits.push(TextEdit {
                                    range: ctx.normalize_range(edit_uri, e.range).await,
                                    new_text: e.new_text.clone(),
                                });
                            }
                            changes.push(DocumentChanges {
                                uri: edit_uri.to_string(),
                                edits: text_edits,
                            });
                        }
                    }
                    Some(WorkspaceEditDescription {
                        resource_operations: resource_operations_from_plan(&plan),
                        changes,
                    })
                }
                // An edit this malformed cannot be applied either, so the
                // preview says the action carries no usable edit rather
                // than failing the whole listing.
                None => None,
            }
        }
        None => None,
    };

    let command = action.command.map(|cmd| {
        let arguments = cmd.arguments.unwrap_or_else(Vec::new);
        CommandDescription {
            title: cmd.title,
            command: cmd.command,
            arguments,
        }
    });

    CodeAction {
        title: action.title,
        kind: action.kind.map(|k| k.as_str().to_string()),
        diagnostics,
        edit,
        command,
        is_preferred: action.is_preferred.unwrap_or(false),
        index: 0,
    }
}

/// How a caller names the code action to apply.
///
/// Internal: the MCP surface takes two flat optional fields, which
/// [`selector_from_params`] narrows to this.
#[derive(Debug, Clone)]
enum CodeActionSelector {
    /// Position in the list `get_code_actions` returned.
    Index(usize),
    /// Exact title, which must match exactly one action.
    Title(String),
    /// Position, confirmed against the title the caller read at it.
    ///
    /// The tool is stateless: between the `get_code_actions` that produced
    /// an index and the `apply_code_action` that consumes it the file may
    /// have changed, so the index can name a different action and would
    /// otherwise apply it with no signal. Giving both fields turns that
    /// into a refusal, for one string comparison.
    ConfirmedIndex {
        /// Position to apply.
        index: usize,
        /// Title that position must still carry.
        title: String,
    },
}

/// Everything one `apply_code_action` call wrote, across its own edit and
/// however many `workspace/applyEdit` requests the action's command sent
/// back.
#[derive(Debug, Default)]
struct InboundEdits {
    /// Whether any of those applies changed the tree.
    changed_the_tree: bool,
    /// Absolute paths whose content was rewritten.
    files_written: Vec<String>,
    /// File-system operations performed.
    resource_operations: Vec<ResourceOperation>,
}

impl InboundEdits {
    /// Fold one apply's outcome in.
    fn absorb(&mut self, summary: &ApplySummary) {
        self.changed_the_tree |= changed_the_tree(summary);
        self.files_written.extend(
            summary
                .files_changed
                .iter()
                .map(|change| change.path.display().to_string()),
        );
        self.resource_operations
            .extend(summary.resource_operations.iter().cloned());
    }
}

/// Whether an apply changed the working tree.
///
/// The one definition every write path reports as `applied`: an edit that
/// only moves or deletes a file changed the tree as surely as one that
/// rewrote bytes, and an edit the applier skipped entirely changed nothing.
const fn changed_the_tree(summary: &ApplySummary) -> bool {
    !summary.files_changed.is_empty() || !summary.resource_operations.is_empty()
}

/// Narrow the tool's two optional selector fields to a selector.
///
/// # Errors
///
/// Returns [`Error::InvalidToolParams`] when neither is given.
fn selector_from_params(
    action_index: Option<usize>,
    action_title: Option<String>,
) -> Result<CodeActionSelector> {
    match (action_index, action_title) {
        (Some(index), None) => Ok(CodeActionSelector::Index(index)),
        (None, Some(title)) => Ok(CodeActionSelector::Title(title)),
        (Some(index), Some(title)) => Ok(CodeActionSelector::ConfirmedIndex { index, title }),
        (None, None) => Err(Error::InvalidToolParams(
            "give one of action_index or action_title to name the action to apply".to_string(),
        )),
    }
}

/// The action at `index`, or an error naming how many there are.
fn action_at(actions: &[CodeAction], index: usize) -> Result<&CodeAction> {
    actions.get(index).ok_or_else(|| {
        Error::InvalidToolParams(format!(
            "action index {index} is out of range: {} actions available",
            actions.len()
        ))
    })
}

/// Pick the action `selector` names.
///
/// # Errors
///
/// Returns [`Error::InvalidToolParams`] when the index is out of range, no
/// title matches, a title matches more than one action, or a confirmed
/// index no longer carries the title the caller gave.
fn select_action<'a>(
    actions: &'a [CodeAction],
    selector: &CodeActionSelector,
) -> Result<&'a CodeAction> {
    match selector {
        CodeActionSelector::Index(index) => action_at(actions, *index),
        CodeActionSelector::ConfirmedIndex { index, title } => {
            let chosen = action_at(actions, *index)?;
            if chosen.title == *title {
                return Ok(chosen);
            }
            Err(Error::InvalidToolParams(format!(
                "action index {index} is now {:?}, not {title:?}: the list changed since \
                 get_code_actions produced it, so read it again",
                chosen.title
            )))
        }
        CodeActionSelector::Title(title) => {
            let mut matches = actions.iter().filter(|a| a.title == *title);
            let first = matches.next().ok_or_else(|| {
                Error::InvalidToolParams(format!("no code action titled {title:?}"))
            })?;
            if matches.next().is_some() {
                return Err(Error::InvalidToolParams(format!(
                    "title {title:?} is ambiguous: more than one action shares it, \
                     select by index instead"
                )));
            }
            Ok(first)
        }
    }
}

impl Translator {
    /// Handle rename request. With `apply` true, writes the resulting edits
    /// to disk instead of only describing them.
    ///
    /// # Errors
    ///
    /// Returns an error if `new_name` exceeds the maximum allowed length,
    /// the LSP request fails, the file cannot be opened, or the routed
    /// server does not advertise `renameProvider` support. When `apply` is
    /// true, also returns [`Error::ApplyDisabled`] if config forbids
    /// `rename_symbol` from writing, [`Error::ApplyRefused`] if the edit is
    /// rejected before anything is written, or
    /// [`Error::ApplyPartiallyFailed`] if a write step fails partway
    /// through.
    pub async fn handle_rename(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
        apply: bool,
    ) -> Result<RenameResult> {
        validate_rename_params(&new_name)?;

        let write_permit = if apply {
            Some(self.applier_for(ToolKind::Rename, "rename_symbol", "apply.rename")?)
        } else {
            None
        };

        let (server_id, client, uri) = self
            .prepare_gated_document(&file_path, ToolKind::Rename, "renameProvider", |caps| {
                matches!(
                    caps.rename_provider,
                    Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                )
            })
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = LspRenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            new_name,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response: Option<WorkspaceEdit> = client
            .request("textDocument/rename", params, client.request_timeout())
            .await?;

        let (changes, resource_operations, applied, files_written) = if let Some(edit) = response {
            let plan = EditPlan::from_workspace_edit(edit)?;
            let resource_operations = resource_operations_from_plan(&plan);
            let mut result_changes = Vec::new();
            for operation in plan.operations() {
                if let Operation::Edit { uri, edits, .. } = operation {
                    let mut text_edits = Vec::with_capacity(edits.len());
                    for e in edits {
                        text_edits.push(TextEdit {
                            range: ctx.normalize_range(uri, e.range).await,
                            new_text: e.new_text.clone(),
                        });
                    }
                    result_changes.push(DocumentChanges {
                        uri: uri.to_string(),
                        edits: text_edits,
                    });
                }
            }

            let (applied, files_written) = if let Some(applier) = write_permit {
                let summary = self.apply_locked(&applier, plan, &server_id).await?;
                (
                    changed_the_tree(&summary),
                    summary
                        .files_changed
                        .iter()
                        .map(|change| change.path.display().to_string())
                        .collect(),
                )
            } else {
                (false, Vec::new())
            };

            (result_changes, resource_operations, applied, files_written)
        } else {
            (vec![], vec![], false, vec![])
        };

        Ok(RenameResult {
            changes,
            resource_operations,
            applied,
            files_written,
        })
    }

    /// Handle format document request. With `apply` true, writes the
    /// resulting edits to disk instead of only describing them.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `documentFormattingProvider`
    /// support. When `apply` is true, also returns [`Error::ApplyDisabled`]
    /// if config forbids `format_document` from writing,
    /// [`Error::ApplyRefused`] if the edit is rejected before anything is
    /// written, or [`Error::ApplyPartiallyFailed`] if a write step fails
    /// partway through.
    pub async fn handle_format_document(
        &self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
        apply: bool,
    ) -> Result<FormatDocumentResult> {
        let write_permit = if apply {
            Some(self.applier_for(
                ToolKind::FormatDocument,
                "format_document",
                "apply.format_document",
            )?)
        } else {
            None
        };

        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::FormatDocument,
                "documentFormattingProvider",
                |caps| {
                    matches!(
                        caps.document_formatting_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let response_uri = uri.clone();

        let params = DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            options: FormattingOptions {
                tab_size,
                insert_spaces,
                ..Default::default()
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response: Option<Vec<lsp_types::TextEdit>> = client
            .request("textDocument/formatting", params, client.request_timeout())
            .await?;

        let edits = response.unwrap_or_default();

        let mut result_edits = Vec::with_capacity(edits.len());
        for edit in &edits {
            result_edits.push(TextEdit {
                range: ctx.normalize_range(&response_uri, edit.range).await,
                new_text: edit.new_text.clone(),
            });
        }

        // An already-well-formatted file yields no edits, so there is
        // nothing to plan and nothing to write.
        let applied = if let (Some(applier), false) = (write_permit, edits.is_empty()) {
            // A `Uri`-keyed `changes` map would trip `clippy::mutable_key_type`
            // (`Uri` wraps a `Cell`), so the single document is wrapped as
            // `document_changes` instead; the LSP spec gives it precedence
            // over `changes` anyway.
            let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
                document_changes: Some(LspDocumentChanges::Edits(vec![TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: response_uri,
                        version: None,
                    },
                    edits: edits.into_iter().map(OneOf::Left).collect(),
                }])),
                ..WorkspaceEdit::default()
            })?;
            let summary = self.apply_locked(&applier, plan, &server_id).await?;
            changed_the_tree(&summary)
        } else {
            false
        };

        Ok(FormatDocumentResult {
            edits: result_edits,
            applied,
        })
    }

    /// Handle code actions request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `codeActionProvider` support.
    pub async fn handle_code_actions(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
    ) -> Result<CodeActionsResult> {
        validate_code_action_params(
            start_line,
            start_character,
            end_line,
            end_character,
            kind_filter.as_deref(),
        )?;

        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::CodeActions,
                "codeActionProvider",
                |caps| {
                    matches!(
                        caps.code_action_provider,
                        Some(
                            lsp_types::CodeActionProviderCapability::Simple(true)
                                | lsp_types::CodeActionProviderCapability::Options(_)
                        )
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let response_uri = uri.clone();

        let range = lsp_types::Range {
            start: ctx.to_lsp(&uri, start_line, start_character).await,
            end: ctx.to_lsp(&uri, end_line, end_character).await,
        };

        // Build context with optional kind filter
        let only = kind_filter.map(|k| vec![lsp_types::CodeActionKind::from(k)]);

        // Pass empty diagnostics context — rust-analyzer generates code actions
        // based on cursor position and its internal analysis state, not on the
        // passed diagnostics.  Passing stale cached diagnostics (which may lack
        // the internal `data` field ra uses for fix mapping) suppresses results.
        let context_diagnostics: Vec<lsp_types::Diagnostic> = vec![];

        let params = lsp_types::CodeActionParams {
            text_document: TextDocumentIdentifier { uri },
            range,
            context: lsp_types::CodeActionContext {
                diagnostics: context_diagnostics,
                only,
                trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<lsp_types::CodeActionResponse> = client
            .request("textDocument/codeAction", params, client.request_timeout())
            .await?;
        let response_vec = response.unwrap_or_default();
        let mut actions = Vec::with_capacity(response_vec.len());

        for (index, action_or_command) in response_vec.into_iter().enumerate() {
            let mut action = match action_or_command {
                lsp_types::CodeActionOrCommand::CodeAction(action) => {
                    convert_code_action(action, &ctx, &response_uri).await
                }
                lsp_types::CodeActionOrCommand::Command(cmd) => {
                    let arguments = cmd.arguments.unwrap_or_else(Vec::new);
                    CodeAction {
                        title: cmd.title.clone(),
                        kind: None,
                        diagnostics: Vec::new(),
                        edit: None,
                        command: Some(CommandDescription {
                            title: cmd.title,
                            command: cmd.command,
                            arguments,
                        }),
                        is_preferred: false,
                        index: 0,
                    }
                }
            };
            action.index = index;
            actions.push(action);
        }

        Ok(CodeActionsResult { actions })
    }

    /// Apply one of the code actions available for a range.
    ///
    /// Re-issues `textDocument/codeAction` for the same range
    /// `get_code_actions` used, selects one action by `action_index`,
    /// `action_title`, or both (the index applied and the title confirming
    /// it), resolves it if the server sent only `data`, and
    /// applies its edit and/or runs its command. An action carrying both an
    /// edit and a command applies the edit first, per the LSP specification.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyDisabled`] when `apply.code_actions` is false,
    /// [`Error::InvalidToolParams`] when the selector names no action or an
    /// ambiguous one, [`Error::ApplyRefused`] when the resolved action
    /// carries neither an edit nor a command or the edit is rejected before
    /// anything is written, [`Error::ApplyPartiallyFailed`] if a write step
    /// fails partway through, and whatever the LSP request itself returns
    /// (e.g. the file cannot be opened, or the routed server does not
    /// advertise `codeActionProvider` support).
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub async fn handle_apply_code_action(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
        action_index: Option<usize>,
        action_title: Option<String>,
    ) -> Result<ApplyCodeActionResult> {
        validate_code_action_params(
            start_line,
            start_character,
            end_line,
            end_character,
            kind_filter.as_deref(),
        )?;
        let selector = selector_from_params(action_index, action_title)?;
        let write_permit = self.applier_for(
            ToolKind::CodeActions,
            "apply_code_action",
            "apply.code_actions",
        )?;

        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::CodeActions,
                "codeActionProvider",
                |caps| {
                    matches!(
                        caps.code_action_provider,
                        Some(
                            lsp_types::CodeActionProviderCapability::Simple(true)
                                | lsp_types::CodeActionProviderCapability::Options(_)
                        )
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);

        let range = lsp_types::Range {
            start: ctx.to_lsp(&uri, start_line, start_character).await,
            end: ctx.to_lsp(&uri, end_line, end_character).await,
        };

        // Built identically to `handle_code_actions`'s `only`, so this
        // re-issued request returns the same numbered list the caller read
        // `action_index`/`action_title` from: a differing filter would
        // shift or drop entries and silently apply the wrong action.
        let only = kind_filter.map(|k| vec![lsp_types::CodeActionKind::from(k)]);

        let params = lsp_types::CodeActionParams {
            text_document: TextDocumentIdentifier { uri },
            range,
            context: lsp_types::CodeActionContext {
                diagnostics: vec![],
                only,
                trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<lsp_types::CodeActionResponse> = client
            .request("textDocument/codeAction", params, client.request_timeout())
            .await?;
        let entries = response.unwrap_or_default();

        // Numbered exactly as `handle_code_actions` numbers its output, so
        // an index the caller read from that listing names the same entry
        // here. Legacy `Command` entries occupy positions in that list, so
        // they are numbered too rather than filtered out.
        let selectable: Vec<CodeAction> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let title = match entry {
                    lsp_types::CodeActionOrCommand::CodeAction(action) => action.title.clone(),
                    lsp_types::CodeActionOrCommand::Command(cmd) => cmd.title.clone(),
                };
                CodeAction {
                    title,
                    kind: None,
                    diagnostics: Vec::new(),
                    edit: None,
                    command: None,
                    is_preferred: false,
                    index,
                }
            })
            .collect();
        let chosen_index = select_action(&selectable, &selector)?.index;

        let (title, edit, command) = match entries[chosen_index].clone() {
            lsp_types::CodeActionOrCommand::Command(cmd) => (cmd.title.clone(), None, Some(cmd)),
            lsp_types::CodeActionOrCommand::CodeAction(action) => {
                let resolved = if action.edit.is_none() && action.data.is_some() {
                    client
                        .request::<_, lsp_types::CodeAction>(
                            "codeAction/resolve",
                            action,
                            client.request_timeout(),
                        )
                        .await?
                } else {
                    action
                };
                (resolved.title, resolved.edit, resolved.command)
            }
        };

        let mut written = InboundEdits::default();
        if let Some(edit) = edit {
            let plan = EditPlan::from_workspace_edit(edit)?;
            let summary = self.apply_locked(&write_permit, plan, &server_id).await?;
            written.absorb(&summary);
        }

        // A command-only action still reaches `workspace/executeCommand`
        // even with no edit to write: an organize-imports-style action can
        // legitimately carry a command and nothing else.
        let executed_command = if let Some(command) = command {
            let name = command.command.clone();
            self.execute_command_applying_inbound_edits(
                &client,
                &write_permit,
                &server_id,
                command,
                &mut written,
            )
            .await?;
            Some(name)
        } else {
            None
        };

        if !written.changed_the_tree && executed_command.is_none() {
            return Err(Error::ApplyRefused(format!(
                "code action {title:?} resolved to neither an edit nor a command"
            )));
        }

        Ok(ApplyCodeActionResult {
            title,
            applied: written.changed_the_tree,
            files_written: written.files_written,
            resource_operations: written.resource_operations,
            executed_command,
        })
    }

    /// Run `command` on `client` while applying any `workspace/applyEdit`
    /// the server sends back, and report the files those edits wrote.
    ///
    /// A server that delivers an assist as a command asks the client to
    /// apply the edit and waits for that answer before replying to the
    /// command itself, so the command and the inbound edits are driven
    /// together rather than one after the other.
    ///
    /// The sink is installed for this call and no longer. Clearing it is
    /// not enough on its own to close that window: the message loop clones
    /// the sink when an inbound request arrives and hands the clone to a
    /// responder it spawns, so a responder already in flight outlives the
    /// clearing. Closing and then dropping the receiver is what closes the
    /// window -- the receiver is a local, so it goes on every path out of
    /// this call, including when the caller's own future is dropped
    /// mid-command -- and a late send then fails and is answered `applied:
    /// false`. That is what keeps a server from writing to the tree at a
    /// moment of its own choosing.
    ///
    /// Only one window is open at a time, per `Translator::apply_sink_lock`:
    /// a client's sink is a single slot shared by every clone of it, so
    /// overlapping windows would clobber each other's sender.
    ///
    /// # Errors
    ///
    /// Returns whatever `workspace/executeCommand` returns. An inbound edit
    /// that is refused does not fail the call: see [`Self::apply_inbound_edit`].
    async fn execute_command_applying_inbound_edits(
        &self,
        client: &LspClient,
        applier: &Applier,
        server_id: &ServerId,
        command: lsp_types::Command,
        written: &mut InboundEdits,
    ) -> Result<()> {
        let _guard = self.apply_sink_lock.lock().await;

        let (sink_tx, mut sink_rx) = mpsc::channel(INBOUND_EDIT_QUEUE_DEPTH);
        client.set_apply_sink(Some(sink_tx)).await;

        let request = client.request::<_, serde_json::Value>(
            "workspace/executeCommand",
            lsp_types::ExecuteCommandParams {
                command: command.command,
                arguments: command.arguments.unwrap_or_default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            client.request_timeout(),
        );
        tokio::pin!(request);

        let outcome = loop {
            tokio::select! {
                // Biased, edits first: a server may send its edit and its
                // command response without waiting in between, leaving both
                // arms ready at once. Random order would then break the loop
                // on the response half the time and refuse an edit that had
                // already arrived.
                biased;
                inbound = sink_rx.recv() => match inbound {
                    Some((edit, reply)) => {
                        let answer = self
                            .apply_inbound_edit(applier, server_id, edit, written)
                            .await;
                        let _ = reply.send(answer);
                    }
                    // Reached only if something else replaces or clears this
                    // client's sink slot mid-command. The window lock rules
                    // that out for this translator's own code actions, and a
                    // client shared with another translator could still do
                    // it. Wait the command out rather than selecting on a
                    // closed channel that is ready forever.
                    None => break (&mut request).await,
                },
                result = &mut request => break result,
            }
        };

        // Refuse anything sent from here on, before draining: the drain
        // awaits an apply per message, and a still-open channel would take
        // in edits the server sent after the command answered.
        sink_rx.close();

        // The command's response can overtake an edit that was queued
        // before it, so what is already in the channel still belongs to
        // this window.
        while let Ok((edit, reply)) = sink_rx.try_recv() {
            let answer = self
                .apply_inbound_edit(applier, server_id, edit, written)
                .await;
            let _ = reply.send(answer);
        }

        drop(sink_rx);
        client.set_apply_sink(None).await;

        match outcome {
            Ok(_) => Ok(()),
            Err(error) => {
                if !written.files_written.is_empty() {
                    warn!(
                        "workspace/executeCommand failed after inbound edits had already \
                         written {:?}: {error}",
                        written.files_written
                    );
                }
                Err(error)
            }
        }
    }

    /// Apply one inbound `workspace/applyEdit`, folding what it wrote into
    /// `written`, and report whether the server's edit was applied.
    ///
    /// The answer is whether the apply succeeded, which is the yes-or-no
    /// the server asked. That is not the same question the tool's own
    /// `applied` field answers: an edit planning no operations succeeds
    /// without writing, so it answers `true` here while adding nothing to
    /// `written`.
    ///
    /// A refused or unplannable edit is answered `false` instead of failing
    /// the tool call: the command around it may still succeed, and the
    /// caller reports only what actually landed.
    async fn apply_inbound_edit(
        &self,
        applier: &Applier,
        server_id: &ServerId,
        edit: WorkspaceEdit,
        written: &mut InboundEdits,
    ) -> bool {
        let outcome = match EditPlan::from_workspace_edit(edit) {
            Ok(plan) => self.apply_locked(applier, plan, server_id).await,
            Err(error) => Err(error),
        };
        match outcome {
            Ok(summary) => {
                written.absorb(&summary);
                true
            }
            Err(error) => {
                warn!("Refused an inbound workspace/applyEdit from {server_id}: {error}");
                false
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;

    use super::*;
    use crate::bridge::translator::dto::DiagnosticSeverity;
    use crate::bridge::translator::testing::*;

    /// #309: `new_name` has no inherent bound of its own and is forwarded to
    /// the LSP server as-is, so it must be rejected before that happens.
    #[test]
    fn test_validate_rename_params_rejects_oversized_new_name() {
        let new_name = "a".repeat(MAX_NEW_NAME_LENGTH + 1);
        let result = validate_rename_params(&new_name);
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[test]
    fn test_validate_rename_params_accepts_name_at_exact_limit() {
        let new_name = "a".repeat(MAX_NEW_NAME_LENGTH);
        assert!(validate_rename_params(&new_name).is_ok());
    }

    #[test]
    fn test_validate_rename_params_accepts_typical_identifier() {
        assert!(validate_rename_params("my_variable").is_ok());
    }

    /// #309: length checks have no lower bound -- an empty `new_name` is
    /// syntactically valid input for this validator (semantic rejection of
    /// an empty rename target, if desired, is a separate concern).
    #[test]
    fn test_validate_rename_params_accepts_empty_string() {
        assert!(validate_rename_params("").is_ok());
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_kind() {
        let translator = Translator::new();
        let result = translator
            .handle_code_actions(
                "/tmp/test.rs".to_string(),
                1,
                1,
                1,
                10,
                Some("invalid_kind".to_string()),
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_quickfix() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("quickfix".to_string()),
            )
            .await;
        // Will fail due to no LSP server, but validates kind is accepted
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_refactor() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("refactor".to_string()),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_refactor_extract() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("refactor.extract".to_string()),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_source() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("source.organizeImports".to_string()),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_range_zero() {
        let translator = Translator::new();
        let result = translator
            .handle_code_actions("/tmp/test.rs".to_string(), 0, 1, 1, 10, None)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_range_order() {
        let translator = Translator::new();
        let result = translator
            .handle_code_actions("/tmp/test.rs".to_string(), 10, 5, 5, 1, None)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_empty_range() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // Empty range (same position) should be valid
        let result = translator
            .handle_code_actions(test_file.to_str().unwrap().to_string(), 1, 5, 1, 5, None)
            .await;
        // Will fail due to no LSP server, but validates range is accepted
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_convert_code_action_minimal() {
        let lsp_action = lsp_types::CodeAction {
            title: "Fix issue".to_string(),
            kind: None,
            diagnostics: None,
            edit: None,
            command: None,
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, &test_ctx(), &test_uri()).await;
        assert_eq!(result.title, "Fix issue");
        assert!(result.kind.is_none());
        assert!(result.diagnostics.is_empty());
        assert!(result.edit.is_none());
        assert!(result.command.is_none());
        assert!(!result.is_preferred);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn test_convert_code_action_with_diagnostics_all_severities() {
        let lsp_diagnostics = vec![
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "Error message".to_string(),
                code: Some(lsp_types::NumberOrString::Number(1)),
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 1,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                message: "Warning message".to_string(),
                code: Some(lsp_types::NumberOrString::String("W001".to_string())),
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 2,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 2,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::INFORMATION),
                message: "Info message".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 3,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 3,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::HINT),
                message: "Hint message".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
        ];

        let lsp_action = lsp_types::CodeAction {
            title: "Fix all issues".to_string(),
            kind: Some(lsp_types::CodeActionKind::QUICKFIX),
            diagnostics: Some(lsp_diagnostics),
            edit: None,
            command: None,
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, &test_ctx(), &test_uri()).await;
        assert_eq!(result.diagnostics.len(), 4);
        assert!(matches!(
            result.diagnostics[0].severity,
            DiagnosticSeverity::Error
        ));
        assert!(matches!(
            result.diagnostics[1].severity,
            DiagnosticSeverity::Warning
        ));
        assert!(matches!(
            result.diagnostics[2].severity,
            DiagnosticSeverity::Information
        ));
        assert!(matches!(
            result.diagnostics[3].severity,
            DiagnosticSeverity::Hint
        ));
        assert_eq!(result.diagnostics[0].code, Some("1".to_string()));
        assert_eq!(result.diagnostics[1].code, Some("W001".to_string()));
    }

    #[tokio::test]
    #[allow(clippy::mutable_key_type)]
    async fn test_convert_code_action_with_workspace_edit() {
        use std::collections::HashMap;
        use std::str::FromStr;

        let uri = lsp_types::Uri::from_str("file:///test.rs").unwrap();
        let mut changes_map = HashMap::new();
        changes_map.insert(
            uri,
            vec![lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 5,
                    },
                },
                new_text: "fixed".to_string(),
            }],
        );

        let lsp_action = lsp_types::CodeAction {
            title: "Apply fix".to_string(),
            kind: Some(lsp_types::CodeActionKind::QUICKFIX),
            diagnostics: None,
            edit: Some(lsp_types::WorkspaceEdit {
                changes: Some(changes_map),
                document_changes: None,
                change_annotations: None,
            }),
            command: None,
            is_preferred: Some(true),
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, &test_ctx(), &test_uri()).await;
        assert!(result.edit.is_some());
        let edit = result.edit.unwrap();
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes[0].uri, "file:///test.rs");
        assert_eq!(edit.changes[0].edits.len(), 1);
        assert_eq!(edit.changes[0].edits[0].new_text, "fixed");
        assert!(result.is_preferred);
    }

    #[tokio::test]
    async fn test_convert_code_action_with_command() {
        let lsp_action = lsp_types::CodeAction {
            title: "Run command".to_string(),
            kind: Some(lsp_types::CodeActionKind::REFACTOR),
            diagnostics: None,
            edit: None,
            command: Some(lsp_types::Command {
                title: "Execute refactor".to_string(),
                command: "refactor.extract".to_string(),
                arguments: Some(vec![serde_json::json!("arg1"), serde_json::json!(42)]),
            }),
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, &test_ctx(), &test_uri()).await;
        assert!(result.command.is_some());
        let cmd = result.command.unwrap();
        assert_eq!(cmd.title, "Execute refactor");
        assert_eq!(cmd.command, "refactor.extract");
        assert_eq!(cmd.arguments.len(), 2);
    }

    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn test_rename_with_apply_is_refused_when_config_forbids_it() {
        let translator = Translator::new();
        let error = translator
            .handle_rename("/w/a.rs".to_string(), 1, 1, "new".to_string(), true)
            .await
            .expect_err("apply must be refused by a read-only translator");
        let message = error.to_string();
        assert!(
            message.contains("apply.rename"),
            "the error names the config key that would permit it: {message}"
        );
    }

    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn test_format_with_apply_is_refused_when_config_forbids_it() {
        let translator = Translator::new();
        let error = translator
            .handle_format_document("/w/a.rs".to_string(), 4, true, true)
            .await
            .expect_err("apply must be refused by a read-only translator");
        assert!(
            error.to_string().contains("apply.format_document"),
            "the error names the config key"
        );
    }

    /// A server that returns no edits (an already-well-formatted file)
    /// must not report `applied: true` even when config permits the write:
    /// `applied` means bytes were written, not that the pipeline ran.
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn test_format_with_apply_and_no_edits_reports_not_applied() {
        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::io::BufReader;
        use tokio::time::{Duration, timeout};

        use crate::bridge::apply::Applier;
        use crate::config::{ApplyConfig, ServerId};

        type JsonValue = serde_json::Value;

        let dir = TempDir::new().expect("temp dir");
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            document_formatting_provider: Some(lsp_types::OneOf::Left(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);
        let applier = Arc::new(Applier::new(
            vec![dir.path().to_path_buf()],
            ApplyConfig {
                format_document: true,
                ..ApplyConfig::default()
            },
        ));
        let translator = translator.with_applier(applier);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").expect("write fixture");
        let path_str = path.to_string_lossy().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_format_document(path_str, 4, true, true)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let format_request = read_framed_message(&mut wire).await;
        assert_eq!(format_request["method"], "textDocument/formatting");
        write_response(
            &mut server.read_half_stdin,
            &format_request["id"],
            JsonValue::Array(vec![]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .expect("task should not panic");

        let format_result = result.expect("apply is permitted, no edits should not error");
        assert!(
            !format_result.applied,
            "no edits were returned, nothing was written, applied must be false"
        );
        assert!(format_result.edits.is_empty());
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_preview_reports_file_rename_operations() {
        use std::str::FromStr;

        use lsp_types::{
            DocumentChangeOperation, DocumentChanges, RenameFile, ResourceOp, Uri, WorkspaceEdit,
        };

        use crate::bridge::apply::EditPlan;
        use crate::bridge::translator::dto::resource_operations_from_plan;

        let edit = WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
                    old_uri: Uri::from_str("file:///w/foo.rs").expect("valid uri"),
                    new_uri: Uri::from_str("file:///w/bar.rs").expect("valid uri"),
                    options: None,
                    annotation_id: None,
                })),
            ])),
            ..WorkspaceEdit::default()
        };
        let plan = EditPlan::from_workspace_edit(edit).expect("plan builds");
        let ops = resource_operations_from_plan(&plan);

        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "rename");
        assert_eq!(ops[0].uri, "file:///w/foo.rs");
        assert_eq!(ops[0].new_uri.as_deref(), Some("file:///w/bar.rs"));
    }

    #[tokio::test]
    async fn test_convert_code_action_with_document_changes_only() {
        use std::str::FromStr;

        use lsp_types::{
            DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, Position, Range,
            TextDocumentEdit, TextEdit as LspTextEdit, Uri,
        };

        let uri = Uri::from_str("file:///test.rs").expect("valid uri");

        let lsp_action = lsp_types::CodeAction {
            title: "Fix issue".to_string(),
            kind: Some(lsp_types::CodeActionKind::QUICKFIX),
            diagnostics: None,
            edit: Some(lsp_types::WorkspaceEdit {
                document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: uri.clone(),
                        version: None,
                    },
                    edits: vec![OneOf::Left(LspTextEdit {
                        range: Range::new(Position::new(0, 0), Position::new(0, 5)),
                        new_text: "fixed".to_string(),
                    })],
                }])),
                changes: None,
                change_annotations: None,
            }),
            command: None,
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, &test_ctx(), &uri).await;
        assert!(
            result.edit.is_some(),
            "code action with document_changes-only edit must not preview as empty"
        );
        let edit = result.edit.unwrap();
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes[0].uri, "file:///test.rs");
        assert_eq!(edit.changes[0].edits.len(), 1);
        assert_eq!(edit.changes[0].edits[0].new_text, "fixed");
    }

    fn stub_action(index: usize, title: &str) -> CodeAction {
        CodeAction {
            title: title.to_string(),
            kind: None,
            diagnostics: Vec::new(),
            edit: None,
            command: None,
            is_preferred: false,
            index,
        }
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_code_action_selector_matches_by_index() {
        let actions = vec![
            stub_action(0, "Add missing import"),
            stub_action(1, "Extract into function"),
        ];
        let chosen = select_action(&actions, &CodeActionSelector::Index(1))
            .expect("index 1 selects the second action");
        assert_eq!(chosen.title, "Extract into function");
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_code_action_selector_matches_by_exact_title() {
        let actions = vec![
            stub_action(0, "Add missing import"),
            stub_action(1, "Extract into function"),
        ];
        let chosen = select_action(
            &actions,
            &CodeActionSelector::Title("Add missing import".to_string()),
        )
        .expect("exact title selects");
        assert_eq!(chosen.index, 0);
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_code_action_selector_rejects_an_ambiguous_title() {
        let actions = vec![
            stub_action(0, "Fill match arms"),
            stub_action(1, "Fill match arms"),
        ];
        let error = select_action(
            &actions,
            &CodeActionSelector::Title("Fill match arms".to_string()),
        )
        .expect_err("two actions share the title");
        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn test_code_action_selector_rejects_an_out_of_range_index() {
        let actions = vec![stub_action(0, "Only one")];
        assert!(select_action(&actions, &CodeActionSelector::Index(7)).is_err());
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_selector_from_params_requires_at_least_one_field() {
        assert!(matches!(
            selector_from_params(Some(2), None).expect("index alone is valid"),
            CodeActionSelector::Index(2)
        ));
        assert!(matches!(
            selector_from_params(None, Some("Fill".to_string())).expect("title alone is valid"),
            CodeActionSelector::Title(_)
        ));
        assert!(matches!(
            selector_from_params(Some(0), Some("Fill".to_string()))
                .expect("both together confirm the index"),
            CodeActionSelector::ConfirmedIndex { index: 0, .. }
        ));
        assert!(selector_from_params(None, None).is_err());
    }

    /// The list `action_index` came from can have changed before the call
    /// that consumes it. A caller that also sends the title it read gets a
    /// refusal instead of a different action applied in silence.
    #[test]
    #[allow(clippy::expect_used)]
    fn test_a_confirmed_index_is_refused_when_the_title_no_longer_matches() {
        let actions = vec![
            stub_action(0, "Add missing import"),
            stub_action(1, "Remove unused import"),
        ];

        let chosen = select_action(
            &actions,
            &CodeActionSelector::ConfirmedIndex {
                index: 1,
                title: "Remove unused import".to_string(),
            },
        )
        .expect("the title still matches the position");
        assert_eq!(chosen.index, 1);

        let error = select_action(
            &actions,
            &CodeActionSelector::ConfirmedIndex {
                index: 1,
                title: "Extract into function".to_string(),
            },
        )
        .expect_err("index 1 is a different action now");
        assert!(
            error.to_string().contains("Remove unused import"),
            "the error says what is actually there: {error}"
        );
    }

    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn test_apply_code_action_is_refused_when_config_forbids_it() {
        let translator = Translator::new();
        let error = translator
            .handle_apply_code_action("/w/a.rs".to_string(), 1, 1, 1, 5, None, Some(0), None)
            .await
            .expect_err("apply must be refused by a read-only translator");
        assert!(
            error.to_string().contains("apply.code_actions"),
            "the error names the config key"
        );
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_code_action_selector_rejects_a_title_matching_nothing() {
        let actions = vec![stub_action(0, "Only one")];
        let error = select_action(
            &actions,
            &CodeActionSelector::Title("Not present".to_string()),
        )
        .expect_err("no action has this title");
        assert!(error.to_string().contains("Not present"));
    }

    /// A translator permitted to apply code actions, routed to a fake
    /// server advertising `codeActionProvider`, over a file holding
    /// `fn old() {}`.
    fn code_action_fixture(
        dir: &tempfile::TempDir,
    ) -> (std::sync::Arc<Translator>, FakeServer, std::path::PathBuf) {
        use crate::bridge::apply::Applier;
        use crate::config::{ApplyConfig, ServerId};

        let caps = lsp_types::ServerCapabilities {
            code_action_provider: Some(lsp_types::CodeActionProviderCapability::Simple(true)),
            ..Default::default()
        };
        let (translator, server) = translator_with_capabilities(dir, &ServerId::from("rust"), caps);
        let translator = translator.with_applier(std::sync::Arc::new(Applier::new(
            vec![dir.path().to_path_buf()],
            ApplyConfig {
                code_actions: true,
                ..ApplyConfig::default()
            },
        )));

        let path = dir.path().join("target.rs");
        fs::write(&path, "fn old() {}\n").expect("write fixture");

        (std::sync::Arc::new(translator), server, path)
    }

    /// A JSON `textDocument/codeAction` listing holding one command-only
    /// action, the shape a server uses for an assist it runs itself.
    fn command_only_listing() -> serde_json::Value {
        serde_json::json!([{
            "title": "Extract into function",
            "command": {
                "title": "Extract into function",
                "command": "test.extract",
                "arguments": [],
            },
        }])
    }

    /// `params` for a `workspace/applyEdit` rewriting `old` to `new` in
    /// `uri`'s first line.
    fn rename_old_to_new(uri: &lsp_types::Uri) -> serde_json::Value {
        serde_json::json!({
            "edit": {
                "changes": {
                    uri.as_str(): [{
                        "range": {
                            "start": { "line": 0, "character": 3 },
                            "end": { "line": 0, "character": 6 },
                        },
                        "newText": "new",
                    }],
                },
            },
        })
    }

    /// Several servers deliver an assist as a command whose only effect is
    /// an inbound `workspace/applyEdit`. That edit must reach the applier
    /// the direct path writes through, and must be answered while the
    /// command is still in flight: a server that waits for the answer
    /// before replying to the command would otherwise never reply.
    #[tokio::test]
    async fn test_command_driven_action_applies_the_edit_the_server_sends_back() {
        use std::sync::Arc;

        use tokio::io::BufReader;
        use tokio::time::{Duration, timeout};

        let dir = tempfile::TempDir::new().expect("temp dir");
        let (translator, mut server, path) = code_action_fixture(&dir);
        let uri = crate::bridge::path_to_uri(&path).expect("uri for the fixture file");
        let path_str = path.to_string_lossy().to_string();

        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_apply_code_action(path_str, 1, 1, 1, 5, None, Some(0), None)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");

        let listing = read_framed_message(&mut wire).await;
        assert_eq!(listing["method"], "textDocument/codeAction");
        write_response(
            &mut server.read_half_stdin,
            &listing["id"],
            command_only_listing(),
        )
        .await;

        let command = read_framed_message(&mut wire).await;
        assert_eq!(command["method"], "workspace/executeCommand");
        assert_eq!(command["params"]["command"], "test.extract");

        write_request(
            &mut server.read_half_stdin,
            &serde_json::json!(4242),
            "workspace/applyEdit",
            rename_old_to_new(&uri),
        )
        .await;

        let answer = read_framed_reply(&mut wire).await;
        assert_eq!(answer["id"], 4242);
        assert_eq!(
            answer["result"]["applied"], true,
            "the edit must be applied while the command is still in flight"
        );

        write_response(
            &mut server.read_half_stdin,
            &command["id"],
            serde_json::Value::Null,
        )
        .await;

        let result = timeout(Duration::from_secs(5), handle)
            .await
            .expect("the handler must not hang")
            .expect("the handler task must not panic")
            .expect("a command-driven action succeeds");

        assert!(
            result.applied,
            "the inbound edit wrote bytes, so the action applied"
        );
        let written = path.canonicalize().expect("the fixture file exists");
        assert_eq!(result.files_written, vec![written.display().to_string()]);
        assert_eq!(result.executed_command.as_deref(), Some("test.extract"));
        assert_eq!(
            fs::read_to_string(&path).expect("read the fixture back"),
            "fn new() {}\n"
        );
    }

    /// One command can answer with several edits, over several files. Every
    /// one of them must land and be reported, which is what the pump loops
    /// for.
    #[tokio::test]
    async fn test_command_driven_action_applies_every_edit_the_server_sends() {
        use std::sync::Arc;

        use tokio::io::BufReader;
        use tokio::time::{Duration, timeout};

        let dir = tempfile::TempDir::new().expect("temp dir");
        let (translator, mut server, first) = code_action_fixture(&dir);
        let second = dir.path().join("other.rs");
        fs::write(&second, "fn old() {}\n").expect("write the second fixture");
        let uris = [
            crate::bridge::path_to_uri(&first).expect("uri for the first file"),
            crate::bridge::path_to_uri(&second).expect("uri for the second file"),
        ];
        let path_str = first.to_string_lossy().to_string();

        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_apply_code_action(path_str, 1, 1, 1, 5, None, Some(0), None)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");

        let listing = read_framed_message(&mut wire).await;
        write_response(
            &mut server.read_half_stdin,
            &listing["id"],
            command_only_listing(),
        )
        .await;

        let command = read_framed_message(&mut wire).await;
        assert_eq!(command["method"], "workspace/executeCommand");

        for (id, uri) in [(4244, &uris[0]), (4245, &uris[1])] {
            write_request(
                &mut server.read_half_stdin,
                &serde_json::json!(id),
                "workspace/applyEdit",
                rename_old_to_new(uri),
            )
            .await;
            let answer = read_framed_reply(&mut wire).await;
            assert_eq!(answer["id"], id);
            assert_eq!(
                answer["result"]["applied"], true,
                "every edit of the command must be applied, not just the first"
            );
        }

        write_response(
            &mut server.read_half_stdin,
            &command["id"],
            serde_json::Value::Null,
        )
        .await;

        let result = timeout(Duration::from_secs(5), handle)
            .await
            .expect("the handler must not hang")
            .expect("the handler task must not panic")
            .expect("a command-driven action succeeds");

        assert!(result.applied);
        let written: Vec<String> = [&first, &second]
            .iter()
            .map(|path| {
                path.canonicalize()
                    .expect("the fixture file exists")
                    .display()
                    .to_string()
            })
            .collect();
        assert_eq!(
            result.files_written, written,
            "both files the command wrote must be reported"
        );
        for path in [&first, &second] {
            assert_eq!(
                fs::read_to_string(path).expect("read the fixture back"),
                "fn new() {}\n"
            );
        }
    }

    /// A `WorkspaceEdit` that only moves a file writes no bytes, but the
    /// tree changed: the file is somewhere else now, and a caller told
    /// `applied: false` would believe nothing happened.
    #[tokio::test]
    async fn test_a_rename_only_edit_reports_applied() {
        use std::sync::Arc;

        use tokio::io::BufReader;
        use tokio::time::{Duration, timeout};

        use crate::bridge::apply::Applier;
        use crate::config::{ApplyConfig, ServerId};

        let dir = tempfile::TempDir::new().expect("temp dir");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::OneOf::Left(true)),
            ..Default::default()
        };
        let (translator, mut server) =
            translator_with_capabilities(&dir, &ServerId::from("rust"), caps);
        let translator = Arc::new(translator.with_applier(Arc::new(Applier::new(
            vec![dir.path().to_path_buf()],
            ApplyConfig {
                rename: true,
                ..ApplyConfig::default()
            },
        ))));

        let anchor = dir.path().join("anchor.rs");
        let old = dir.path().join("moved.rs");
        let new = dir.path().join("moved_again.rs");
        fs::write(&anchor, "fn caller() {}\n").expect("write the anchor fixture");
        fs::write(&old, "fn moved() {}\n").expect("write the moved fixture");
        let old_uri = crate::bridge::path_to_uri(&old.canonicalize().expect("moved.rs exists"))
            .expect("uri for moved.rs");
        let new_uri = crate::bridge::path_to_uri(&new).expect("uri for the destination");

        let handle = {
            let translator = Arc::clone(&translator);
            let path = anchor.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_rename(path, 1, 4, "renamed".to_string(), true)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let rename = read_framed_reply(&mut wire).await;
        write_response(
            &mut server.read_half_stdin,
            &rename["id"],
            serde_json::json!({
                "documentChanges": [{
                    "kind": "rename",
                    "oldUri": old_uri.as_str(),
                    "newUri": new_uri.as_str(),
                }],
            }),
        )
        .await;

        let result = timeout(Duration::from_secs(5), handle)
            .await
            .expect("the handler must not hang")
            .expect("the handler task must not panic")
            .expect("a rename-only edit applies");

        assert!(
            result.applied,
            "a file moved, so the edit reached the working tree"
        );
        assert!(result.files_written.is_empty(), "no bytes were rewritten");
        assert!(!old.exists());
        assert_eq!(
            fs::read_to_string(&new).expect("read the moved file"),
            "fn moved() {}\n"
        );
    }

    /// An apply writes files the call never queried -- a rename anchored in
    /// one file rewrites every file referencing the symbol. Those files stay
    /// open in the routed server at their pre-apply content unless mcpls
    /// closes them, and the next edit against that content corrupts them.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn test_apply_closes_a_written_document_the_call_never_queried() {
        use std::sync::Arc;

        use tokio::io::BufReader;
        use tokio::time::{Duration, timeout};

        use crate::bridge::apply::Applier;
        use crate::config::{ApplyConfig, ServerId};

        let dir = tempfile::TempDir::new().expect("temp dir");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::OneOf::Left(true)),
            ..Default::default()
        };
        let (translator, mut server) =
            translator_with_capabilities(&dir, &ServerId::from("rust"), caps);
        let translator = Arc::new(translator.with_applier(Arc::new(Applier::new(
            vec![dir.path().to_path_buf()],
            ApplyConfig {
                rename: true,
                ..ApplyConfig::default()
            },
        ))));

        let anchor = dir.path().join("anchor.rs");
        let other = dir.path().join("other.rs");
        fs::write(&anchor, "fn caller() {}\n").expect("write the anchor fixture");
        fs::write(&other, "fn old() {}\n").expect("write the other fixture");
        let other_canonical = other.canonicalize().expect("the other fixture exists");
        let other_uri = crate::bridge::path_to_uri(&other_canonical).expect("uri for other.rs");

        let mut wire = BufReader::new(&mut server.write_stdout);

        // A read-only call on other.rs, so the server holds it open at its
        // pre-apply content -- exactly the state an earlier tool call leaves
        // behind.
        let opening = {
            let translator = Arc::clone(&translator);
            let path = other.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_rename(path, 1, 1, "x".to_string(), false)
                    .await
            })
        };
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let first_rename = read_framed_message(&mut wire).await;
        write_response(
            &mut server.read_half_stdin,
            &first_rename["id"],
            serde_json::Value::Null,
        )
        .await;
        timeout(Duration::from_secs(5), opening)
            .await
            .expect("the opening call must not hang")
            .expect("the opening task must not panic")
            .expect("a rename with no edits still succeeds");
        assert!(
            translator.is_document_open(&other_canonical),
            "the read-only call left other.rs open"
        );

        // A rename anchored in anchor.rs whose edit lands entirely in
        // other.rs.
        let handle = {
            let translator = Arc::clone(&translator);
            let path = anchor.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_rename(path, 1, 4, "renamed".to_string(), true)
                    .await
            })
        };
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let rename = read_framed_message(&mut wire).await;
        assert_eq!(rename["method"], "textDocument/rename");
        write_response(
            &mut server.read_half_stdin,
            &rename["id"],
            serde_json::json!({
                "changes": {
                    other_uri.as_str(): [{
                        "range": {
                            "start": { "line": 0, "character": 3 },
                            "end": { "line": 0, "character": 6 },
                        },
                        "newText": "renamed",
                    }],
                },
            }),
        )
        .await;

        let closed = read_framed_message(&mut wire).await;
        assert_eq!(
            closed["method"], "textDocument/didClose",
            "the server must be told the file it still holds open was rewritten"
        );
        assert_eq!(closed["params"]["textDocument"]["uri"], other_uri.as_str());

        let result = timeout(Duration::from_secs(5), handle)
            .await
            .expect("the handler must not hang")
            .expect("the handler task must not panic")
            .expect("the rename applies");
        assert!(result.applied);
        assert_eq!(
            fs::read_to_string(&other).expect("read other.rs back"),
            "fn renamed() {}\n"
        );
        assert!(
            !translator.is_document_open(&other_canonical),
            "a written document must not stay tracked at its pre-apply content"
        );
    }

    /// The sink lives for the `executeCommand` call and no longer: an edit
    /// the server sends once the command has answered is refused, so it
    /// cannot write to the tree at a moment of its own choosing.
    ///
    /// This covers the window closing, not the mechanism that closes it.
    /// With the sink cleared, `forward_apply_edit` refuses before it ever
    /// reaches the send, so this passes whether or not the receiver was
    /// also dropped. `lsp::client`'s
    /// `test_apply_edit_is_refused_when_the_sink_is_gone` covers the drop.
    #[tokio::test]
    async fn test_inbound_apply_edit_after_the_command_is_refused() {
        use std::sync::Arc;

        use tokio::io::BufReader;
        use tokio::time::{Duration, timeout};

        let dir = tempfile::TempDir::new().expect("temp dir");
        let (translator, mut server, path) = code_action_fixture(&dir);
        let uri = crate::bridge::path_to_uri(&path).expect("uri for the fixture file");
        let path_str = path.to_string_lossy().to_string();

        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_apply_code_action(path_str, 1, 1, 1, 5, None, Some(0), None)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");

        let listing = read_framed_message(&mut wire).await;
        write_response(
            &mut server.read_half_stdin,
            &listing["id"],
            command_only_listing(),
        )
        .await;

        let command = read_framed_message(&mut wire).await;
        write_response(
            &mut server.read_half_stdin,
            &command["id"],
            serde_json::Value::Null,
        )
        .await;

        let result = timeout(Duration::from_secs(5), handle)
            .await
            .expect("the handler must not hang")
            .expect("the handler task must not panic")
            .expect("a command that writes nothing still succeeds");
        assert!(!result.applied, "the command sent no edit back");

        write_request(
            &mut server.read_half_stdin,
            &serde_json::json!(4243),
            "workspace/applyEdit",
            rename_old_to_new(&uri),
        )
        .await;

        let answer = read_framed_message(&mut wire).await;
        assert_eq!(answer["id"], 4243);
        assert_eq!(
            answer["result"]["applied"], false,
            "the apply window closed with the command"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("read the fixture back"),
            "fn old() {}\n",
            "a refused edit writes nothing"
        );
    }
}
