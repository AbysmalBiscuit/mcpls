//! MCP server implementation using rmcp.
//!
//! This module provides the MCP server that exposes LSP capabilities
//! as MCP tools using the rmcp SDK.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lsp_types::Uri;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    Implementation, ListResourcesResult, ListToolsResult, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, Resource, ResourceContents,
    ResourceUpdatedNotificationParam, ServerCapabilities, ServerInfo, SubscribeRequestParams, Tool,
    ToolAnnotations, UnsubscribeRequestParams,
};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::Mutex;

use super::handlers::BridgeContext;
use super::tools::{
    ApplyCodeActionParams, CachedDiagnosticsParams, CallHierarchyCallsParams, CodeActionsParams,
    CompletionsParams, DiagnosticsParams, DocumentSymbolsParams, FormatDocumentParams,
    InlayHintsParams, PositionParams, RangeParams, ReferencesParams, RenameParams,
    ServerLogsParams, ServerMessagesParams, WorkspaceSymbolParams,
};
use crate::bridge::resources::{make_uri, parse_uri};
use crate::bridge::{
    Caller, ConnectionId, DefinitionResult, Diagnostic, DiagnosticInfo, DiagnosticSeverity,
    DiagnosticSnapshot, DiagnosticsDelivery, DiagnosticsResult, DocumentSymbolsResult, FileEntry,
    FloorTable, FlushReport, NotificationCache, PositionEncoding, RecordId, ReferencesResult,
    ResourceSubscriptions, ServerSettle, SessionId, Translator, uri_to_path,
    validate_path_against_roots,
};
use crate::config::{DiagnosticsConfig, ServerId, ToolKind};

#[cfg(test)]
#[path = "session_identity_tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod session_identity_tests;

/// What every mcpls tells an agent about itself at `initialize`.
#[allow(clippy::redundant_pub_crate)]
pub(crate) const INSTRUCTIONS: &str = concat!(
    "Universal MCP to LSP bridge. Exposes Language Server Protocol ",
    "capabilities as MCP tools for semantic code intelligence. ",
    "Supports hover, definition, references, diagnostics, rename, ",
    "completions, symbols, and formatting."
);

/// MCP server that exposes LSP capabilities as tools.
#[derive(Clone)]
pub struct McplsServer {
    context: Arc<BridgeContext>,
    connection: ConnectionId,
    session: SessionId,
    adopted_anonymous: Arc<Mutex<bool>>,
    _http_cleanup: Option<Arc<HttpConnectionCleanup>>,
    /// Sentences appended to this connection's instructions.
    notes: Arc<[String]>,
}

struct HttpConnectionCleanup {
    context: Arc<BridgeContext>,
    connection: ConnectionId,
}

impl Drop for HttpConnectionCleanup {
    fn drop(&mut self) {
        let context = Arc::clone(&self.context);
        let connection = self.connection;
        // The last server clone outlives rmcp's detached request handlers.
        tokio::spawn(async move {
            context
                .delivery
                .lock()
                .await
                .end_session(&SessionId::for_connection(connection));
            context.subscriptions.remove_connection(connection).await;
        });
    }
}

/// The apply toggle a tool's writes answer to, for the tools that write.
///
/// Every other tool reads, so its annotations never depend on the config.
fn write_toggle(tool_name: &str) -> Option<ToolKind> {
    match tool_name {
        "rename_symbol" => Some(ToolKind::Rename),
        "format_document" => Some(ToolKind::FormatDocument),
        "apply_code_action" => Some(ToolKind::CodeActions),
        _ => None,
    }
}

/// Map a bridge-layer result to the MCP tool response shape shared by every `#[tool]` handler.
fn to_tool_result<T: serde::Serialize>(
    result: crate::error::Result<T>,
) -> Result<String, McpError> {
    match result {
        Ok(value) => serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
        Err(e) => Err(McpError::internal_error(e.to_string(), None)),
    }
}

fn to_structured_tool_result<T: Serialize + JsonSchema>(
    result: crate::error::Result<T>,
) -> Result<Json<T>, McpError> {
    match result {
        Ok(value) => Ok(Json(value)),
        Err(e) => Err(McpError::internal_error(e.to_string(), None)),
    }
}

/// Fixed page size for `list_resources` pagination.
///
/// `DocumentTracker`'s configured `max_documents` (0 = unlimited) isn't
/// reachable from here -- it's private to the tracker, and `0` means the
/// document count itself is unbounded anyway -- so this is an independent
/// page-size ceiling, large enough to rarely trigger for typical workspaces
/// but small enough to stay well under stdio transport buffer limits.
const RESOURCE_PAGE_SIZE: usize = 100;

/// Slice `paths` into the page starting at the position `cursor` resumes
/// from, returning the page and the cursor for the next page (`None` once
/// the last page is reached).
///
/// `paths` must already be sorted: the caller's source
/// (`open_document_paths()`) is backed by a `HashMap` with no ordering
/// guarantee, and a stable order is required for a cursor to resume at a
/// reproducible position across calls. The cursor is an index into that
/// order, not a document identity: if a document closes at an index below
/// the cursor between two calls, every later entry shifts down one and the
/// next page silently skips the entry that moved into the cursor's old
/// slot. Low-impact for this use case (a stdio single-session server), but
/// callers pairing pagination with concurrent document open/close should be
/// aware a page can miss an entry rather than duplicate one.
///
/// # Errors
///
/// Returns an error only if `cursor` fails to parse as a `usize`. Any
/// parseable value is accepted as a page-start index, including one that
/// isn't page-aligned (not a value this function itself ever returns via
/// `next_cursor`) or is out of range (e.g. documents were closed between
/// calls) -- an out-of-range cursor is not an error, it yields an empty
/// final page.
fn paginate_resource_paths<'a>(
    paths: &'a [PathBuf],
    cursor: Option<&str>,
    page_size: usize,
) -> Result<(&'a [PathBuf], Option<String>), McpError> {
    debug_assert!(
        page_size > 0,
        "page_size must be non-zero, or next_cursor never advances"
    );

    let start = match cursor {
        Some(c) => c.parse::<usize>().map_err(|_| {
            McpError::invalid_params(format!("invalid pagination cursor: {c}"), None)
        })?,
        None => 0,
    };

    let rest = paths.get(start..).unwrap_or_default();
    let page = &rest[..rest.len().min(page_size)];
    // `start` is client-controlled (parsed straight from the cursor), so the
    // addition must not panic (debug) or silently wrap (release) for a
    // cursor near `usize::MAX`.
    let next_start = start.saturating_add(page_size);
    let next_cursor = (next_start < paths.len()).then(|| next_start.to_string());

    Ok((page, next_cursor))
}

/// `read_resource`'s diagnostics payload, distinguishing a file mcpls has no
/// information about (`tracked: false`, always paired with empty
/// `diagnostics`) from one it does -- whether because the file is currently
/// open via `DocumentTracker`, or an LSP server has published diagnostics
/// for it regardless of open state (`tracked: true`; `diagnostics: []` if
/// clean or not yet analyzed).
///
/// `version` is the document version the diagnostics were computed against
/// (the client's staleness signal, mirroring `DiagnosticInfo::version`) --
/// `None` both when untracked and when tracked but nothing has been
/// published yet. `uri` is deliberately omitted: the caller already knows it
/// (it's the resource they requested).
#[derive(serde::Serialize)]
struct ResourceDiagnosticsResponse {
    tracked: bool,
    version: Option<i32>,
    diagnostics: Vec<lsp_types::Diagnostic>,
}

impl ResourceDiagnosticsResponse {
    fn new(tracked: bool, entry: Option<&DiagnosticInfo>) -> Self {
        Self {
            tracked,
            version: entry.and_then(|e| e.version),
            diagnostics: entry.map(|e| e.diagnostics.clone()).unwrap_or_default(),
        }
    }
}

/// Build `read_resource`'s response for a file. `tracked` is true when the
/// file is currently open via `DocumentTracker` (`document_open`) *or* the
/// diagnostics cache already holds an entry for it (`entry.is_some()`) --
/// not `document_open` alone: an LSP server publishes
/// `textDocument/publishDiagnostics` for whatever it analyzes, including
/// files mcpls never explicitly opened (e.g. one rust-analyzer pulls in
/// transitively), so `document_open` alone could report `tracked: false`
/// while `diagnostics` is still non-empty, contradicting the documented
/// "untracked implies empty diagnostics" contract.
fn build_resource_diagnostics_response(
    document_open: bool,
    entry: Option<&DiagnosticInfo>,
) -> ResourceDiagnosticsResponse {
    ResourceDiagnosticsResponse::new(document_open || entry.is_some(), entry)
}

/// One file whose visible diagnostics changed since the caller's last
/// `get_new_diagnostics` call.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NewDiagnosticsFile {
    /// Path the caller can open, derived from the notification's URI.
    pub file_path: String,
    /// Diagnostics at or above the file's severity floor, capped.
    pub diagnostics: Vec<Diagnostic>,
    /// Admitted diagnostics the caps held back this call. The file is
    /// recorded as seen in full regardless, so these are not offered
    /// again: the count is what tells the agent to look at the file
    /// itself. The whole-flush `omitted` count on the response is the
    /// other thing -- those files a later call does offer again.
    pub omitted: usize,
}

/// A tool result with the diagnostics that call produced appended.
#[derive(Debug, serde::Serialize)]
struct WithDiagnostics<T> {
    #[serde(flatten)]
    result: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_diagnostics: Option<NewDiagnosticsResult>,
}

/// Response shape for `get_new_diagnostics`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NewDiagnosticsResult {
    /// Files whose visible diagnostics differ from the caller's last call.
    pub changed: Vec<NewDiagnosticsFile>,
    /// Paths that had diagnostics before and have none now.
    pub cleared: Vec<String>,
    /// Whole files the total budget could not fit this call. The caps held
    /// them back; the next call offers them again in full.
    pub omitted: usize,
    /// An explanation the payload's other fields can't carry on their own:
    /// some servers are still settling, or `omitted` is non-zero and a later
    /// call will offer those files again. A report may be partial for both
    /// reasons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl NewDiagnosticsResult {
    /// The response returned before a baseline exists.
    ///
    /// Calling `DiagnosticsDelivery::flush` this early would permanently
    /// seed the caller's session record as empty, and every diagnostic the
    /// workspace already had would then read as newly changed forever
    /// after. Returning this instead, without touching the delivery
    /// record at all, keeps that session's first real flush available for
    /// once a baseline lands.
    fn starting_up() -> Self {
        Self {
            changed: Vec::new(),
            cleared: Vec::new(),
            omitted: 0,
            note: Some("Language servers are still starting up; call again shortly.".to_string()),
        }
    }
}

/// When a flush's record changes take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Advance {
    /// In the flush itself. For a reader on this process's own transport,
    /// whose answer either arrives or ends the session: the tool and the
    /// footer.
    Now,
    /// When the reader acknowledges the report, or never. For the hook
    /// socket, whose client gives up on a timer and may be gone before
    /// the answer is written; a report it never acknowledges is offered
    /// again.
    OnAcknowledgement,
}

/// How long each phase of a footer's wait lasts.
#[derive(Debug, Clone, Copy)]
struct FooterTiming {
    grace: Duration,
    quiet: Duration,
    cap: Duration,
}

impl FooterTiming {
    const fn from_config(config: &DiagnosticsConfig) -> Self {
        Self {
            grace: Duration::from_millis(config.footer_grace_ms),
            quiet: Duration::from_millis(config.footer_quiet_ms),
            cap: Duration::from_millis(config.footer_wait_ms),
        }
    }
}

/// How long a footer waits before it reports what it has.
///
/// This is the wait `footer_for_write` runs: the loop ends the first time
/// `footer_should_stop` reads the tracker as quiet or as running work that
/// predates the edit, and otherwise samples again every 50 ms until
/// `timing.cap` is reached. The cap bounds the *whole* wait, grace
/// included, rather than sitting on top of it — a call that never goes
/// quiet costs at most `timing.cap`, not `timing.grace + timing.cap`, which
/// is what `footer_wait_ms`'s own "in total" documents.
///
/// `tick` performs the delay between samples and reports the `Instant`
/// reached. `footer_for_write` passes a closure that awaits
/// `tokio::time::sleep` and reads `Instant::now()`, so production pays real
/// wall-clock time. A test that has already fixed every `begin`/`end_at`
/// timestamp relative to a `start: Instant` can instead pass a closure that
/// advances a counter and returns `start + elapsed` without ever
/// suspending, so the branch tests below run instantly against the exact
/// loop that ships rather than a replica of it.
///
/// Sampling starts at `grace` rather than at zero, and that is what covers
/// the case where the check has not begun yet: flycheck starts about 90 ms
/// after a `didSave`, and before it does the workspace reads as quiet. A
/// `grace` larger than the cap is spent up to the cap and no further: the
/// first sleep is the smaller of the two, because a configuration asking
/// for a shorter total wait than the grace has asked for the total, and
/// paying the whole grace would put the excess outside every bound the
/// configuration names.
///
/// The 50 ms sampling step means a call that never goes quiet can overshoot
/// `timing.cap` by up to one step: the tick that pushes `elapsed` past the
/// cap has already been paid in real time by the time the loop notices, and
/// a completed sleep cannot be undone. That residual is bounded and
/// constant — at most 50 ms — not proportional to `timing.cap`.
async fn wait_for_footer_quiet_at<F, Fut>(
    settle: &ServerSettle,
    epoch_before: u64,
    timing: FooterTiming,
    tick: F,
) -> Duration
where
    F: Fn(Duration) -> Fut,
    Fut: Future<Output = Instant>,
{
    const STEP: Duration = Duration::from_millis(50);
    let mut elapsed = timing.grace.min(timing.cap);
    let mut now = tick(elapsed).await;
    loop {
        if footer_should_stop(settle, epoch_before, now, timing.quiet) {
            return elapsed;
        }
        if elapsed >= timing.cap {
            return timing.cap;
        }
        now = tick(STEP).await;
        elapsed += STEP;
    }
}

/// Whether a footer has waited long enough, as of `now`.
///
/// Two ways to be done. The workspace is quiet, which is the ordinary one
/// and the only one that fires before any work has begun. Or work is
/// outstanding and none of it began since the resync, which means that work
/// was already running when the edit landed: an index after a `Cargo.toml`
/// change can run for minutes, and it is not this call's check.
fn footer_should_stop(
    settle: &ServerSettle,
    epoch_before: u64,
    now: Instant,
    quiet: Duration,
) -> bool {
    if settle.is_quiet_at(now, quiet) {
        return true;
    }
    settle.progress_epoch() == epoch_before
}

/// Build the `FileEntry` list a delivery flush should consider.
///
/// Unmappable URIs and owners with pending startup baselines are excluded
/// before `flush`, which advances the record for every entry it receives.
fn routable_entries_borrowed<'a>(
    cache: &'a NotificationCache,
    floors: &FloorTable,
    pending_baselines: &HashSet<ServerId>,
) -> Vec<FileEntry<'a>> {
    cache
        .diagnostics_entries()
        .into_iter()
        .filter(|(_, info, owner)| {
            !pending_baselines.contains(*owner) && uri_to_path(&info.uri).is_some()
        })
        .map(|(key, info, owner)| FileEntry {
            key,
            diagnostics: &info.diagnostics,
            floor: floors.for_server(owner),
        })
        .collect()
}

/// One flush rendered as the lines a hook prints, or `None` when the flush
/// found nothing to say.
///
/// A hook's output is injected into the agent's context, so an empty report
/// must produce no text at all rather than an empty structure the agent
/// then has to interpret.
fn render_for_hook(report: &NewDiagnosticsResult) -> Option<String> {
    if report.changed.is_empty() && report.cleared.is_empty() && report.note.is_none() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    for file in &report.changed {
        lines.push(format!("{}:", file.file_path));
        for diagnostic in &file.diagnostics {
            lines.push(format!(
                "  {}:{} {} {}",
                diagnostic.range.start.line,
                diagnostic.range.start.character,
                severity_label(&diagnostic.severity),
                diagnostic.message
            ));
        }
        if file.omitted > 0 {
            lines.push(format!("  ({} more not shown)", file.omitted));
        }
    }
    for path in &report.cleared {
        lines.push(format!("{path}: no diagnostics"));
    }
    if let Some(note) = &report.note {
        lines.push(note.clone());
    }
    Some(lines.join("\n"))
}

/// How one severity reads in a hook's plain-text output.
const fn severity_label(severity: &DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "error",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Information => "info",
        DiagnosticSeverity::Hint => "hint",
    }
}

/// What the payload build needs about one cached entry, after the cache
/// guard is gone.
///
/// Carries `version` because `new_diagnostics_payload` rebuilds a
/// `DiagnosticInfo` from these three fields before handing it to
/// `Translator::diagnostics_from_cache_entry`, and `DiagnosticInfo` requires
/// one. The converter itself never reads it.
#[derive(Debug, Clone)]
struct DiagnosticSource {
    uri: Uri,
    version: Option<i32>,
    owner: ServerId,
}

/// The URI, version and owning server of every key a report names, cloned
/// so the payload can be built after both guards are released.
fn source_map(
    cache: &NotificationCache,
    report: &FlushReport,
) -> HashMap<String, DiagnosticSource> {
    report
        .changed
        .iter()
        .map(|file| file.key.as_str())
        .chain(report.cleared.iter().map(String::as_str))
        .filter_map(|key| {
            let info = cache.get_diagnostics(key)?;
            let owner = cache.diagnostics_owner(key)?;
            Some((
                key.to_string(),
                DiagnosticSource {
                    uri: info.uri.clone(),
                    version: info.version,
                    owner: owner.clone(),
                },
            ))
        })
        .collect()
}

#[tool_router(router = declared_tool_router)]
impl McplsServer {
    /// Create a new MCP server with the given translator, notification
    /// cache, workspace roots, subscriptions, and diagnostics delivery
    /// state.
    ///
    /// `project_config_ignored` reports whether a CWD-discovered
    /// `./mcpls.toml` was skipped as untrusted when the active config was
    /// loaded (see [`ServerConfig::project_config_ignored`](crate::config::ServerConfig::project_config_ignored));
    /// `get_info` surfaces it in [`ServerInfo::instructions`].
    ///
    /// `delivery` and `floors` should be the same `Arc`s the caller's
    /// diagnostics-baseline background task shares, so that `flush` and
    /// `set_baseline` observe each other's writes.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        translator: Arc<Translator>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        workspace_roots: Arc<[PathBuf]>,
        subscriptions: Arc<ResourceSubscriptions>,
        project_config_ignored: bool,
        delivery: Arc<Mutex<DiagnosticsDelivery>>,
        floors: Arc<FloorTable>,
        diagnostics: DiagnosticsConfig,
        settle: Arc<ServerSettle>,
    ) -> Self {
        let context = Arc::new(BridgeContext::new(
            translator,
            notification_cache,
            workspace_roots,
            subscriptions,
            project_config_ignored,
            delivery,
            floors,
            diagnostics,
            settle,
        ));
        Self::from_context(context)
    }

    /// A server over an already-built context.
    ///
    /// The socket handler needs a server sharing the MCP server's context,
    /// so both are built from one `Arc<BridgeContext>` rather than through
    /// [`Self::new`].
    #[allow(clippy::missing_const_for_fn)]
    pub(crate) fn from_context(context: Arc<BridgeContext>) -> Self {
        let connection = ConnectionId::next();
        Self {
            context,
            connection,
            session: SessionId::for_connection(connection),
            adopted_anonymous: Arc::new(Mutex::new(false)),
            _http_cleanup: None,
            notes: Arc::from(Vec::new()),
        }
    }

    /// This server answering a new connection: the same shared state, a
    /// fresh connection id, and `session` when the host named one or the
    /// connection's own record when it did not.
    #[must_use]
    pub(crate) fn for_connection(&self, session: Option<SessionId>) -> Self {
        let connection = ConnectionId::next();
        Self {
            context: Arc::clone(&self.context),
            connection,
            session: session.unwrap_or_else(|| SessionId::for_connection(connection)),
            adopted_anonymous: Arc::new(Mutex::new(false)),
            _http_cleanup: None,
            notes: Arc::clone(&self.notes),
        }
    }

    #[cfg(feature = "transport-http")]
    pub(crate) fn for_http_connection(&self) -> Self {
        let server = self.for_connection(None);
        Self {
            _http_cleanup: Some(Arc::new(HttpConnectionCleanup {
                context: Arc::clone(&server.context),
                connection: server.connection,
            })),
            ..server
        }
    }

    /// This server with `notes` appended to its instructions.
    #[must_use]
    pub(crate) fn with_notes(mut self, notes: Vec<String>) -> Self {
        self.notes = Arc::from(notes);
        self
    }

    #[allow(dead_code)]
    pub(crate) const fn session(&self) -> &SessionId {
        &self.session
    }

    pub(crate) const fn connection(&self) -> ConnectionId {
        self.connection
    }

    pub(crate) fn subscriptions(&self) -> &Arc<ResourceSubscriptions> {
        &self.context.subscriptions
    }

    /// Router for every MCP tool, with the read-only classification applied
    /// as a default.
    ///
    /// Most mcpls tools are read-only LSP queries, so applying that once
    /// here replaces an identical `annotations(...)` block on each `#[tool]`
    /// attribute. A tool that mutates anything -- files on disk, or
    /// server-side state such as a session's delivery record -- declares its
    /// own annotations and keeps them;
    /// `test_tool_annotation_classifications_match_intent` forces any such
    /// tool to write down an explicit classification rather than inherit
    /// this default silently.
    fn tool_router() -> ToolRouter<Self> {
        let mut router = Self::declared_tool_router();
        for route in router.map.values_mut() {
            let title = route.attr.title.clone();
            route.attr.annotations.get_or_insert_with(|| {
                ToolAnnotations::from_raw(title, Some(true), Some(false), Some(true), None)
            });
        }
        router
    }

    /// `tools` with every writing tool this deployment forbids re-advertised
    /// as read-only.
    ///
    /// A `#[tool]` attribute describes what its tool does with its apply key
    /// on. With the key off the tool refuses the write and returns the same
    /// edits a query would, so a client that puts destructive tools behind a
    /// prompt has nothing here to prompt about.
    fn annotate_for_config(&self, mut tools: Vec<Tool>) -> Vec<Tool> {
        let config = self.context.translator.apply_config();
        for tool in &mut tools {
            let Some(kind) = write_toggle(&tool.name) else {
                continue;
            };
            if config.permits(kind) {
                continue;
            }
            let title = tool
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.title.clone());
            tool.annotations = Some(ToolAnnotations::from_raw(
                title,
                Some(true),
                Some(false),
                Some(true),
                None,
            ));
        }
        tools
    }

    /// Get hover information at a position in a file.
    #[tool(
        description = "Type and documentation info at position. Returns signatures, docs, and inferred types for symbols.",
        title = "Hover"
    )]
    async fn get_hover(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_hover(file_path, line, character)
                .await,
        )
    }

    /// Get the definition location of a symbol.
    #[tool(
        description = "Definition location of symbol at position. Returns file path, line, and character where declared.",
        title = "Go to Definition"
    )]
    async fn get_definition(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<Json<DefinitionResult>, McpError> {
        to_structured_tool_result(
            self.context
                .translator
                .handle_definition(file_path, line, character)
                .await,
        )
    }

    /// Find all references to a symbol.
    #[tool(
        description = "All references to symbol at position. Returns locations across workspace where symbol is used.",
        title = "Find References"
    )]
    async fn get_references(
        &self,
        Parameters(ReferencesParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
            include_declaration,
        }): Parameters<ReferencesParams>,
    ) -> Result<Json<ReferencesResult>, McpError> {
        to_structured_tool_result(
            self.context
                .translator
                .handle_references(file_path, line, character, include_declaration)
                .await,
        )
    }

    /// Get diagnostics for a file.
    #[tool(
        description = "Diagnostics for a file. Returns errors, warnings, and hints with severity and location.",
        title = "Diagnostics"
    )]
    async fn get_diagnostics(
        &self,
        Parameters(DiagnosticsParams { file_path }): Parameters<DiagnosticsParams>,
    ) -> Result<Json<DiagnosticsResult>, McpError> {
        // Merging push-model (flycheck/clippy) diagnostics into the pull
        // result, including the pull-error-but-cache-has-data fallback, is
        // handled inside handle_diagnostics itself -- see its doc comment.
        to_structured_tool_result(
            self.context
                .translator
                .handle_diagnostics(file_path, &self.context.notification_cache)
                .await,
        )
    }

    /// Rename a symbol across the workspace, optionally writing the edits.
    #[tool(
        description = "Rename symbol across workspace. Returns text edits for all files \
                       where symbol is used. With apply=true, and apply.rename enabled in \
                       config, writes those edits to disk.",
        title = "Rename Symbol",
        annotations(
            title = "Rename Symbol",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn rename_symbol(
        &self,
        Parameters(RenameParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
            new_name,
            apply,
        }): Parameters<RenameParams>,
    ) -> Result<String, McpError> {
        let epoch_before = self.context.settle.progress_epoch();
        let result = match self
            .context
            .translator
            .handle_rename(file_path, line, character, new_name, apply)
            .await
        {
            Ok(result) => result,
            Err(err) => return Err(McpError::internal_error(err.to_string(), None)),
        };
        let footer = self.footer_if_written(result.applied, epoch_before).await;
        to_tool_result(Ok(WithDiagnostics {
            result,
            new_diagnostics: footer,
        }))
    }

    /// Get code completion suggestions.
    #[tool(
        description = "Completion suggestions at position. Returns methods, functions, variables, types, and snippets.",
        title = "Completions"
    )]
    async fn get_completions(
        &self,
        Parameters(CompletionsParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
            trigger,
        }): Parameters<CompletionsParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_completions(file_path, line, character, trigger)
                .await,
        )
    }

    /// Get all symbols in a document.
    #[tool(
        description = "Symbols in a file. Returns hierarchical outline with functions, classes, structs, and locations.",
        title = "Document Symbols"
    )]
    async fn get_document_symbols(
        &self,
        Parameters(DocumentSymbolsParams { file_path }): Parameters<DocumentSymbolsParams>,
    ) -> Result<Json<DocumentSymbolsResult>, McpError> {
        to_structured_tool_result(
            self.context
                .translator
                .handle_document_symbols(file_path)
                .await,
        )
    }

    /// Format a document according to language server rules, optionally
    /// writing the edits.
    #[tool(
        description = "Format document with language-specific rules. Returns text edits for \
                       indentation, spacing, and style. With apply=true, and \
                       apply.format_document enabled in config, writes those edits to disk.",
        title = "Format Document",
        annotations(
            title = "Format Document",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn format_document(
        &self,
        Parameters(FormatDocumentParams {
            file_path,
            tab_size,
            insert_spaces,
            apply,
        }): Parameters<FormatDocumentParams>,
    ) -> Result<String, McpError> {
        let epoch_before = self.context.settle.progress_epoch();
        let result = match self
            .context
            .translator
            .handle_format_document(file_path, tab_size, insert_spaces, apply)
            .await
        {
            Ok(result) => result,
            Err(err) => return Err(McpError::internal_error(err.to_string(), None)),
        };
        let footer = self.footer_if_written(result.applied, epoch_before).await;
        to_tool_result(Ok(WithDiagnostics {
            result,
            new_diagnostics: footer,
        }))
    }

    /// Search for symbols across the workspace.
    #[tool(
        description = "Search workspace symbols by name. Supports partial matching and fuzzy search.",
        title = "Workspace Symbol Search"
    )]
    async fn workspace_symbol_search(
        &self,
        params: Parameters<WorkspaceSymbolParams>,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<String, McpError> {
        tokio::select! {
            result = self.workspace_symbol_search_impl(params) => result,
            () = context.ct.cancelled() => {
                #[cfg(all(test, unix))]
                crate::recovery_tests::mark_workspace_request_cancelled();
                Err(McpError::internal_error("workspace symbol request cancelled", None))
            }
        }
    }

    async fn workspace_symbol_search_impl(
        &self,
        Parameters(WorkspaceSymbolParams {
            query,
            kind_filter,
            limit,
        }): Parameters<WorkspaceSymbolParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_workspace_symbol(query, kind_filter, limit)
                .await,
        )
    }

    /// Get code actions for a range.
    // read-only: returns proposed CodeAction edits, does not apply them.
    // `apply_code_action` re-issues the same request and applies one by
    // index or title, since a list has no defined meaning for an `apply`
    // flag of its own.
    #[tool(
        description = "Code actions for range. Returns quick fixes, refactorings, and source actions with edits.",
        title = "Code Actions"
    )]
    async fn get_code_actions(
        &self,
        Parameters(CodeActionsParams {
            file_path,
            range:
                RangeParams {
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                },
            kind_filter,
        }): Parameters<CodeActionsParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_code_actions(
                    file_path,
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                    kind_filter,
                )
                .await,
        )
    }

    /// Apply one of the code actions available for a range.
    #[tool(
        description = "Apply one code action from get_code_actions for the same range and \
                       kind_filter, by index, by exact title, or by index confirmed with the \
                       title you read at it. Requires apply.code_actions = true in config. \
                       Writes the action's edits to disk.",
        title = "Apply Code Action",
        annotations(
            title = "Apply Code Action",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn apply_code_action(
        &self,
        Parameters(ApplyCodeActionParams {
            file_path,
            range:
                RangeParams {
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                },
            kind_filter,
            action_index,
            action_title,
        }): Parameters<ApplyCodeActionParams>,
    ) -> Result<String, McpError> {
        let epoch_before = self.context.settle.progress_epoch();
        let result = match self
            .context
            .translator
            .handle_apply_code_action(
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
                action_index,
                action_title,
            )
            .await
        {
            Ok(result) => result,
            Err(err) => return Err(McpError::internal_error(err.to_string(), None)),
        };
        let footer = self.footer_if_written(result.applied, epoch_before).await;
        to_tool_result(Ok(WithDiagnostics {
            result,
            new_diagnostics: footer,
        }))
    }

    /// Prepare call hierarchy at a position.
    #[tool(
        description = "Prepare call hierarchy at position. Returns callable items for incoming/outgoing call analysis.",
        title = "Prepare Call Hierarchy"
    )]
    async fn prepare_call_hierarchy(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_call_hierarchy_prepare(file_path, line, character)
                .await,
        )
    }

    /// Get incoming calls (callers).
    #[tool(
        description = "Functions calling the specified item. Takes call hierarchy item, returns all callers.",
        title = "Incoming Calls"
    )]
    async fn get_incoming_calls(
        &self,
        Parameters(CallHierarchyCallsParams { item }): Parameters<CallHierarchyCallsParams>,
    ) -> Result<String, McpError> {
        to_tool_result(self.context.translator.handle_incoming_calls(item).await)
    }

    /// Get outgoing calls (callees).
    #[tool(
        description = "Functions called by the specified item. Takes call hierarchy item, returns all callees.",
        title = "Outgoing Calls"
    )]
    async fn get_outgoing_calls(
        &self,
        Parameters(CallHierarchyCallsParams { item }): Parameters<CallHierarchyCallsParams>,
    ) -> Result<String, McpError> {
        to_tool_result(self.context.translator.handle_outgoing_calls(item).await)
    }

    /// Get cached diagnostics for a file.
    #[tool(
        description = "Cached diagnostics from server notifications. Faster than get_diagnostics, no new analysis.",
        title = "Cached Diagnostics"
    )]
    async fn get_cached_diagnostics(
        &self,
        Parameters(CachedDiagnosticsParams { file_path }): Parameters<CachedDiagnosticsParams>,
    ) -> Result<String, McpError> {
        let result =
            match Translator::cached_diagnostics_uri(&self.context.workspace_roots, &file_path) {
                Ok(uri) => {
                    // Lock only long enough for the map lookup + clone: no
                    // canonicalize() or Vec mapping while `notification_cache`
                    // is held, since `diagnostics_pump` needs the same lock.
                    let (diag_info, owner) = {
                        let cache = self.context.notification_cache.lock().await;
                        (
                            cache.get_diagnostics(&uri).cloned(),
                            cache.diagnostics_owner(&uri).cloned(),
                        )
                    };
                    let encoding = owner.map_or(PositionEncoding::Utf16, |server_id| {
                        self.context.translator.position_encoding_for(&server_id)
                    });
                    Ok(Translator::diagnostics_from_cache_entry(
                        diag_info.as_ref(),
                        encoding,
                        self.context.translator.document_tracker(),
                    )
                    .await)
                }
                Err(e) => Err(e),
            };

        to_tool_result(result)
    }

    /// Drain diagnostics that changed since the last call.
    ///
    /// The annotations are spelled out rather than inherited: draining
    /// advances the session's delivery record, so a host that dedupes or
    /// retries a call it believes to be read-only and idempotent would
    /// discard a response whose contents are already gone.
    #[tool(
        description = "Diagnostics that changed since you last asked, across every file the language servers report on. Returns nothing when nothing changed.",
        title = "New Diagnostics",
        annotations(
            title = "New Diagnostics",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    pub(crate) async fn get_new_diagnostics(&self) -> Result<String, McpError> {
        let baselined = {
            let delivery = self.context.delivery.lock().await;
            delivery.has_baseline()
        };
        if !baselined {
            return to_tool_result(Ok(NewDiagnosticsResult::starting_up()));
        }

        let session = self.session.clone();

        // `delivery` first, then the cache. The flush borrows its entries
        // straight out of the cache guard, so both are held together; taking
        // them in this order everywhere is what keeps that from deadlocking.
        // Neither guard outlives this block: `new_diagnostics_payload` awaits
        // per changed file, and holding the cache lock across those awaits
        // would block the diagnostics pump, which loses publishes rather than
        // waiting for them.
        to_tool_result(Ok(self
            .flush_now(&RecordId::from(&session), Advance::Now)
            .await
            .0))
    }

    /// Flush `session`'s record and render it, advancing the record as
    /// `advance` says.
    ///
    /// The caller checks `has_baseline()` first. Both the tool and the
    /// footer must, because `stage` seeds a session's record from the
    /// baseline and `set_baseline` never rewrites one that already exists.
    ///
    /// The token is `Some` only under `Advance::OnAcknowledgement`, and
    /// only when the report implies a record change.
    // What enforces the delivery-before-cache order is where the two
    // acquires sit, not how long either guard lives afterward. Clippy's fix
    // for this lint moves the `delivery` acquire below the cache acquire,
    // which is the exact reversal that order forbids, so the acquires stay
    // where they are and the lint is silenced instead.
    #[allow(clippy::significant_drop_tightening)]
    async fn flush_now(
        &self,
        session: &RecordId,
        advance: Advance,
    ) -> (NewDiagnosticsResult, Option<u64>) {
        let (report, token, sources, baseline_pending) = {
            let mut delivery = self.context.delivery.lock().await;
            let cache = self.context.notification_cache.lock().await;
            // A diagnostic publish also needs the cache lock, so this read
            // covers owners that start while the call waits for either lock.
            let pending_baselines = self.context.settle.pending_diagnostics_baselines();
            let entries =
                routable_entries_borrowed(&cache, &self.context.floors, &pending_baselines);
            let snapshots = entries
                .iter()
                .filter_map(|entry| {
                    let info = cache.get_diagnostics(entry.key)?;
                    let owner = cache.diagnostics_owner(entry.key)?;
                    let text = uri_to_path(&info.uri).and_then(|path| {
                        self.context
                            .translator
                            .document_tracker()
                            .get(&path)
                            .map(|document| Arc::<str>::from(document.content()))
                    });
                    Some((
                        entry.key.to_string(),
                        Arc::new(DiagnosticSnapshot {
                            uri: info.uri.clone(),
                            encoding: self.context.translator.position_encoding_for(owner),
                            text,
                        }),
                    ))
                })
                .collect();
            delivery.capture_sources(snapshots);
            let (report, token) = match advance {
                Advance::Now => (delivery.flush(session, &entries), None),
                Advance::OnAcknowledgement => delivery.stage(session, &entries),
            };
            let sources = source_map(&cache, &report);
            let baseline_pending = !pending_baselines.is_empty();
            (report, token, sources, baseline_pending)
        };
        let mut payload = self.new_diagnostics_payload(&report, &sources).await;
        if baseline_pending {
            let pending_note = "Diagnostics from a recently started language server are still settling; call again shortly.";
            payload.note = Some(payload.note.map_or_else(
                || pending_note.to_string(),
                |note| format!("{pending_note} {note}"),
            ));
        }
        (payload, token)
    }

    /// `session`'s flush, rendered as the text a hook prints, with the
    /// token the hook acknowledges once it has that text.
    ///
    /// The same report the tool would run, against the same record, so a
    /// hook and an agent never see the same diagnostic twice once either
    /// has confirmed it. The record moves only in `commit_for_hook`: the
    /// hook gives up on a timer, and a report it gave up on is offered
    /// again rather than marked delivered. Silent before a baseline exists
    /// for the same reason the footer is: `stage` seeds a session's record
    /// from the baseline and `set_baseline` never rewrites one that
    /// already exists, so flushing early would leave that session
    /// permanently believing the workspace started clean.
    pub(crate) async fn flush_for_hook(
        &self,
        session: impl Into<RecordId>,
    ) -> (Option<String>, Option<u64>) {
        let session = &session.into();
        if !self.context.delivery.lock().await.has_baseline() {
            return (None, None);
        }
        let (report, token) = self.flush_now(session, Advance::OnAcknowledgement).await;
        (render_for_hook(&report), token)
    }

    /// Mark the report staged under `token` delivered to `session`.
    ///
    /// Takes `delivery` alone. `false` when `token` no longer names the
    /// session's staged report, in which case the record already reflects
    /// something sent more recently, or nothing.
    pub(crate) async fn commit_for_hook(&self, session: impl Into<RecordId>, token: u64) -> bool {
        self.context.delivery.lock().await.commit(session, token)
    }

    pub(crate) async fn register_caller(&self, caller: &Caller) {
        self.context.delivery.lock().await.register_caller(caller);
    }

    /// Drop `session`'s delivery record.
    pub(crate) async fn end_session(&self, session: &SessionId) {
        self.context.delivery.lock().await.end_session(session);
    }

    /// Build `get_new_diagnostics`'s payload from one flush's report.
    ///
    /// Runs each changed file's diagnostics through the same conversion
    /// `get_cached_diagnostics` uses -- resolving the owning server's
    /// negotiated position encoding via `Translator::diagnostics_from_cache_entry`
    /// -- so the two tools never disagree about a column.
    async fn new_diagnostics_payload(
        &self,
        report: &FlushReport,
        sources: &HashMap<String, DiagnosticSource>,
    ) -> NewDiagnosticsResult {
        let mut changed = Vec::with_capacity(report.changed.len());
        for file in &report.changed {
            if let Some(source) = report.sources.get(&file.key) {
                if let Some(path) = uri_to_path(&source.uri) {
                    changed.push(NewDiagnosticsFile {
                        file_path: path.display().to_string(),
                        diagnostics: file
                            .diagnostics
                            .iter()
                            .map(|diagnostic| source.render(diagnostic))
                            .collect(),
                        omitted: file.omitted,
                    });
                }
                continue;
            }
            let Some(source) = sources.get(&file.key) else {
                continue;
            };
            // Drop an entry whose URI does not map to a path rather than
            // showing the agent a URI it cannot open.
            let Some(path) = uri_to_path(&source.uri) else {
                continue;
            };
            let encoding = self.context.translator.position_encoding_for(&source.owner);
            let entry = DiagnosticInfo {
                uri: source.uri.clone(),
                version: source.version,
                diagnostics: file.diagnostics.clone(),
            };
            let converted = Translator::diagnostics_from_cache_entry(
                Some(&entry),
                encoding,
                self.context.translator.document_tracker(),
            )
            .await;
            changed.push(NewDiagnosticsFile {
                file_path: path.display().to_string(),
                diagnostics: converted.diagnostics,
                omitted: file.omitted,
            });
        }

        let cleared = report
            .cleared
            .iter()
            .filter_map(|key| {
                report
                    .sources
                    .get(key)
                    .map(|source| &source.uri)
                    .or_else(|| sources.get(key).map(|source| &source.uri))
            })
            .filter_map(uri_to_path)
            .map(|path| path.display().to_string())
            .collect();

        NewDiagnosticsResult {
            changed,
            cleared,
            omitted: report.omitted,
            note: (report.omitted > 0).then(|| {
                format!(
                    "{} file(s) were held back by the diagnostics caps this call; call again \
                     to see them.",
                    report.omitted
                )
            }),
        }
    }

    /// The diagnostics a write tool's own edit produced, or `None` when the
    /// call wrote nothing.
    ///
    /// One method rather than an `if` repeated at three call sites, so a
    /// fourth write tool cannot be added with the guard forgotten.
    pub(crate) async fn footer_if_written(
        &self,
        applied: bool,
        epoch_before: u64,
    ) -> Option<NewDiagnosticsResult> {
        if !applied {
            return None;
        }
        self.footer_for_write(epoch_before).await
    }

    /// The diagnostics a write tool's own edit produced, or `None`.
    ///
    /// Silent while no baseline exists. `flush` seeds a session's record
    /// from the baseline, and `set_baseline` does not rewrite a record that
    /// already exists, so a footer flushing early would leave that session
    /// permanently believing the workspace started clean.
    ///
    /// Waits via `wait_for_footer_quiet_at`, which bounds the whole wait —
    /// grace included — by `footer_wait_ms`; the real worst case for one
    /// call is that value plus at most one 50 ms sampling tick, never the
    /// grace and the cap stacked on top of each other.
    pub(crate) async fn footer_for_write(&self, epoch_before: u64) -> Option<NewDiagnosticsResult> {
        if !self.context.diagnostics.footer {
            return None;
        }
        if !self.context.delivery.lock().await.has_baseline() {
            return None;
        }
        let timing = FooterTiming::from_config(&self.context.diagnostics);
        wait_for_footer_quiet_at(
            &self.context.settle,
            epoch_before,
            timing,
            |step| async move {
                tokio::time::sleep(step).await;
                Instant::now()
            },
        )
        .await;

        let session = self.session.clone();
        let mut report = self
            .flush_now(&RecordId::from(&session), Advance::Now)
            .await
            .0;
        report.note = Some(report.note.take().map_or_else(
            || {
                "This footer is best effort; anything slower than the wait arrives in the \
                 next get_new_diagnostics."
                    .to_string()
            },
            |existing| {
                format!(
                    "{existing} This footer is best effort; anything slower than the wait \
                     arrives in the next get_new_diagnostics."
                )
            },
        ));
        Some(report)
    }

    /// Get recent LSP server log messages.
    #[tool(
        description = "Recent server log messages. Filter by level (error, warning, info, debug) for debugging.",
        title = "Server Logs"
    )]
    async fn get_server_logs(
        &self,
        Parameters(ServerLogsParams { limit, min_level }): Parameters<ServerLogsParams>,
    ) -> Result<String, McpError> {
        to_tool_result({
            let cache = self.context.notification_cache.lock().await;
            Translator::handle_server_logs(&cache, limit, min_level)
        })
    }

    /// Get recent LSP server messages.
    #[tool(
        description = "Recent server messages (showMessage notifications). User-facing prompts and status updates.",
        title = "Server Messages"
    )]
    async fn get_server_messages(
        &self,
        Parameters(ServerMessagesParams { limit }): Parameters<ServerMessagesParams>,
    ) -> Result<String, McpError> {
        to_tool_result({
            let cache = self.context.notification_cache.lock().await;
            Translator::handle_server_messages(&cache, limit)
        })
    }

    /// Get signature help at a position.
    #[tool(
        description = "Signature help at position. Returns parameter info, active signature/parameter, and documentation while typing a call.",
        title = "Signature Help"
    )]
    async fn get_signature_help(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_signature_help(file_path, line, character)
                .await,
        )
    }

    /// Go to implementation locations.
    #[tool(
        description = "Implementation locations of trait method or interface member at position.",
        title = "Go to Implementation"
    )]
    async fn go_to_implementation(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_implementation(file_path, line, character)
                .await,
        )
    }

    /// Go to type definition location.
    #[tool(
        description = "Type definition location of expression at position. Distinct from go-to-definition for variable bindings.",
        title = "Go to Type Definition"
    )]
    async fn go_to_type_definition(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_type_definition(file_path, line, character)
                .await,
        )
    }

    /// Get inlay hints for a range.
    #[tool(
        description = "Inlay hints in range. Returns inferred type/parameter annotations the editor would render inline.",
        title = "Inlay Hints"
    )]
    async fn get_inlay_hints(
        &self,
        Parameters(InlayHintsParams {
            file_path,
            range:
                RangeParams {
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                },
        }): Parameters<InlayHintsParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_inlay_hints(
                    file_path,
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                )
                .await,
        )
    }
}

// `list_resources` is synchronous (no `.await`), but `ServerHandler::list_resources`
// requires `async fn`; `#[tool_handler]` also expands other trait methods without
// `.await`, so the lint is suppressed for the whole impl block.
#[allow(clippy::unused_async_trait_impl)]
#[tool_handler]
impl ServerHandler for McplsServer {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        let mut server = self.clone();
        if context
            .client_info()
            .is_some_and(|client| client.name == "codex-mcp-client")
        {
            let thread = context
                .meta
                .get("threadId")
                .and_then(serde_json::Value::as_str)
                .filter(|thread| !thread.is_empty())
                .or_else(|| {
                    context
                        .meta
                        .get("x-codex-turn-metadata")?
                        .get("thread_id")?
                        .as_str()
                        .filter(|thread| !thread.is_empty())
                });
            let anonymous = SessionId::for_connection(self.connection);
            server.session = if let Some(session) = SessionId::named(thread.map(str::to_owned)) {
                let mut adopted = self.adopted_anonymous.lock().await;
                if !*adopted {
                    self.context
                        .delivery
                        .lock()
                        .await
                        .merge_session(&anonymous, &session);
                    *adopted = true;
                }
                drop(adopted);
                let root = context
                    .meta
                    .get("x-codex-turn-metadata")
                    .and_then(|meta| meta.get("session_id"))
                    .and_then(serde_json::Value::as_str)
                    .and_then(|root| SessionId::named(Some(root.to_owned())));
                self.context.delivery.lock().await.register_caller(&Caller {
                    record: RecordId::from(&session),
                    root,
                });
                session
            } else {
                anonymous
            };
        }
        if server.session != SessionId::for_connection(server.connection)
            && context
                .client_info()
                .is_none_or(|client| client.name != "codex-mcp-client")
        {
            server
                .register_caller(&Caller {
                    record: RecordId::from(&server.session),
                    root: Some(server.session.clone()),
                })
                .await;
        }
        let call = rmcp::handler::server::tool::ToolCallContext::new(&server, request, context);
        Self::tool_router().call(call).await
    }

    /// What `#[tool_handler]` would generate, plus the pass that fits each
    /// tool's annotations to this deployment. Defining it here is what stops
    /// the macro generating its own.
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= rmcp::model::ProtocolVersion::V_2026_07_28);
        Ok(ListToolsResult {
            result_type: Some(rmcp::model::ResultType::COMPLETE),
            tools: self.annotate_for_config(Self::tool_router().list_all()),
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(rmcp::model::CacheScope::Public),
        })
    }

    /// Same, for the single-tool lookup.
    fn get_tool(&self, name: &str) -> Option<Tool> {
        let tool = Self::tool_router().get(name).cloned()?;
        self.annotate_for_config(vec![tool]).pop()
    }

    async fn list_resources(
        &self,
        request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let mut open_paths = self.context.translator.open_document_paths();
        // `open_document_paths()` is backed by a `HashMap`; sort so pagination
        // cursors resume at a stable, deterministic position across calls.
        open_paths.sort();

        let cursor = request.and_then(|r| r.cursor);
        let (page, next_cursor) =
            paginate_resource_paths(&open_paths, cursor.as_deref(), RESOURCE_PAGE_SIZE)?;

        let resources: Vec<_> = page
            .iter()
            .filter_map(|path| {
                let uri = make_uri(path)
                    .inspect_err(|e| {
                        tracing::warn!(
                            "Skipping path in list_resources (make_uri failed): {}: {e}",
                            path.display()
                        );
                    })
                    .ok()?;
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                Some(
                    Resource::new(uri, name)
                        .with_mime_type("application/json")
                        .with_description("LSP diagnostics for this file"),
                )
            })
            .collect();

        Ok(ListResourcesResult {
            next_cursor,
            ..ListResourcesResult::with_all_items(resources)
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let path =
            parse_uri(&request.uri).map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Enforce workspace-root containment — mirrors the guard in every LSP tool.
        // Validated against a lock-free snapshot of workspace_roots (fixed at
        // startup) so this cache-only read never needs to touch `translator` at all.
        let validated_path = validate_path_against_roots(&path, &self.context.workspace_roots)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Build the URI from the canonicalized path (not the raw input path):
        // it must match what `diagnostics_pump` stores from LSP notifications,
        // which are always keyed by the canonical form.
        let lsp_uri = crate::bridge::path_to_uri(&validated_path)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Built from a borrow of the cache entry rather than `.cloned()`-ing the
        // whole `DiagnosticInfo` first: `build_resource_diagnostics_response`
        // only ever needs `version` (Copy) and its own clone of `diagnostics`,
        // so cloning the entry up front would clone `diagnostics` twice.
        let response = {
            let cache = self.context.notification_cache.lock().await;
            build_resource_diagnostics_response(
                self.context.translator.is_document_open(&validated_path),
                cache.get_diagnostics(lsp_uri.as_str()),
            )
        };

        let json = serde_json::to_string(&response)
            .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None))?;

        Ok(ReadResourceResult::new(vec![ResourceContents::text(json, request.uri)]).into())
    }

    /// When cached diagnostics exist, the replay notification is flushed to the client
    /// before this call returns its own response; this is legal per JSON-RPC/MCP, which
    /// permits notifications to interleave with in-flight requests, so a conformant
    /// client must demultiplex by request `id` rather than assume response-before-notification ordering.
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let path =
            parse_uri(&request.uri).map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Enforce workspace-root containment (same invariant as every LSP tool).
        // Validated against a lock-free snapshot of workspace_roots so subscribing
        // never needs to touch `translator` at all (see `read_resource`).
        let validated_path = validate_path_against_roots(&path, &self.context.workspace_roots)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // The pump derives its resource URI from the canonical LSP path, so
        // subscriptions use the same canonical key.
        let canonical_uri =
            make_uri(&validated_path).map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Record before checking the cache so diagnostics arriving between
        // the two operations are either replayed or pushed by the pump.
        self.context
            .subscriptions
            .subscribe(self.connection, context.peer.clone(), canonical_uri.clone())
            .await
            .map_err(|e| McpError::invalid_params(e, None))?;

        // Build the URI from the canonicalized path, matching `read_resource` and
        // what `diagnostics_pump` stores from LSP notifications.
        let lsp_uri = crate::bridge::path_to_uri(&validated_path)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let has_cached_diagnostics = {
            let cache = self.context.notification_cache.lock().await;
            cache.get_diagnostics(lsp_uri.as_str()).is_some()
        };

        if has_cached_diagnostics
            && let Err(e) = context
                .peer
                .notify_resource_updated(ResourceUpdatedNotificationParam::new(
                    canonical_uri.clone(),
                ))
                .await
        {
            tracing::warn!("Failed to replay cached diagnostics for {canonical_uri}: {e}");
        }

        Ok(())
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        // Parse the URI for consistency with subscribe validation.
        let path =
            parse_uri(&request.uri).map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Remove under the same canonical URI `subscribe` recorded under. Best-effort
        // fall back to the raw URI if canonicalization fails (e.g. the file was
        // deleted since subscribing) so unsubscribing a stale entry never errors.
        let key = validate_path_against_roots(&path, &self.context.workspace_roots)
            .ok()
            .and_then(|validated_path| make_uri(&validated_path).ok())
            .unwrap_or_else(|| request.uri.clone());

        self.context
            .subscriptions
            .unsubscribe(self.connection, &key)
            .await;
        Ok(())
    }

    fn get_info(&self) -> ServerInfo {
        let mut implementation = Implementation::new("mcpls", env!("CARGO_PKG_VERSION"));
        implementation.title = Some("MCPLS - MCP to LSP Bridge".to_string());
        implementation.description = Some(env!("CARGO_PKG_DESCRIPTION").to_string());
        implementation.website_url = Some("https://github.com/bug-ops/mcpls".to_string());

        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_resources_subscribe()
            .build();
        let mut server_info = ServerInfo::new(capabilities);
        server_info.server_info = implementation;
        let mut instructions = INSTRUCTIONS.to_string();

        if self.context.project_config_ignored {
            instructions.push_str(
                " NOTE: a project-local mcpls.toml was found in the current directory but \
                 ignored as untrusted; the server is running on built-in defaults or a global \
                 config instead. If this repository is trusted, restart mcpls with \
                 --trust-project-config (or MCPLS_TRUST_PROJECT_CONFIG=true) to load it.",
            );
        }
        for note in self.notes.iter() {
            instructions.push(' ');
            instructions.push_str(note);
        }
        server_info.instructions = Some(instructions);

        server_info
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::bridge::apply::Applier;
    use crate::bridge::{
        FakeServer, RenameResult, read_framed_reply, translator_with_capabilities, write_response,
    };
    use crate::config::ApplyConfig;

    /// A `DiagnosticsDelivery`/`FloorTable` pair for tests that don't care
    /// about diagnostics config or per-server floors, just a working
    /// `McplsServer::new` call.
    fn default_delivery_and_floors() -> (Arc<Mutex<DiagnosticsDelivery>>, Arc<FloorTable>) {
        let config = crate::config::DiagnosticsConfig::default();
        (
            Arc::new(Mutex::new(DiagnosticsDelivery::new(config))),
            Arc::new(FloorTable::new(&config, &[])),
        )
    }

    /// A settle tracker for tests that need a working `McplsServer::new`
    /// call but never drive `$/progress` through it.
    fn test_settle() -> Arc<ServerSettle> {
        Arc::new(ServerSettle::new(
            Duration::from_secs(1),
            Duration::from_secs(300),
        ))
    }

    /// A server over one context, with an adopted empty baseline and one
    /// error cached, so a flush has something to report.
    pub(super) async fn server_with_one_error() -> McplsServer {
        let (delivery, floors) = default_delivery_and_floors();
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let uri: lsp_types::Uri = if cfg!(windows) {
            "file:///C:/workspace/broken.rs".parse().unwrap()
        } else {
            "file:///workspace/broken.rs".parse().unwrap()
        };
        cache.lock().await.store_diagnostics(
            &ServerId::from("rust"),
            &uri,
            Some(1),
            vec![lsp_types::Diagnostic {
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "broken".to_string(),
                ..lsp_types::Diagnostic::default()
            }],
        );
        delivery.lock().await.set_baseline(HashMap::new());
        McplsServer::new(
            Arc::new(Translator::new()),
            cache,
            Arc::from(Vec::new()),
            Arc::new(ResourceSubscriptions::new()),
            false,
            delivery,
            floors,
            crate::config::DiagnosticsConfig::default(),
            test_settle(),
        )
    }

    /// Two connections whose hosts named no session each read their own
    /// record. A backend serves every session from one process, so a
    /// process-wide fallback would let one session consume another's
    /// report.
    #[tokio::test]
    async fn test_anonymous_connections_read_their_own_records() {
        let server = server_with_one_error().await;
        let first = server.for_connection(None);
        let second = server.for_connection(None);

        assert!(
            first
                .get_new_diagnostics()
                .await
                .unwrap()
                .contains("broken.rs")
        );
        assert!(
            second
                .get_new_diagnostics()
                .await
                .unwrap()
                .contains("broken.rs"),
            "the first connection's flush consumed the second's report"
        );
        assert!(
            !first
                .get_new_diagnostics()
                .await
                .unwrap()
                .contains("broken.rs")
        );
    }

    /// Two connections naming one session share its record, which is how a
    /// hook and the agent's own tool call agree on what was delivered.
    #[tokio::test]
    async fn test_connections_naming_one_session_share_its_record() {
        let server = server_with_one_error().await;
        let session = || SessionId::named(Some("s1".to_string()));
        let first = server.for_connection(session());
        let second = server.for_connection(session());

        assert!(
            first
                .get_new_diagnostics()
                .await
                .unwrap()
                .contains("broken.rs")
        );
        assert!(
            !second
                .get_new_diagnostics()
                .await
                .unwrap()
                .contains("broken.rs")
        );
    }

    #[test]
    fn test_notes_reach_the_instructions() {
        let (delivery, floors) = default_delivery_and_floors();
        let server = McplsServer::new(
            Arc::new(Translator::new()),
            Arc::new(Mutex::new(NotificationCache::new())),
            Arc::from(Vec::new()),
            Arc::new(ResourceSubscriptions::new()),
            false,
            delivery,
            floors,
            crate::config::DiagnosticsConfig::default(),
            test_settle(),
        )
        .with_notes(vec![
            "NOTE: first.".to_string(),
            "NOTE: second.".to_string(),
        ]);

        let instructions = server.get_info().instructions.unwrap();
        assert!(
            instructions.ends_with(" NOTE: first. NOTE: second."),
            "{instructions}"
        );
    }

    /// An `McplsServer` together with the `Arc`s it shares, so a test can
    /// reach the same cache and the same delivery record the server sees.
    ///
    /// `McplsServer::new` moves its arguments into a private
    /// `BridgeContext`, so a test that needs both sides keeps its own
    /// clones from before the call.
    struct TestServer {
        server: McplsServer,
        notification_cache: Arc<Mutex<NotificationCache>>,
        delivery: Arc<Mutex<DiagnosticsDelivery>>,
    }

    fn test_server_parts_with(diagnostics: DiagnosticsConfig) -> TestServer {
        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let delivery = Arc::new(Mutex::new(DiagnosticsDelivery::new(diagnostics)));
        let floors = Arc::new(FloorTable::new(&diagnostics, &[]));
        let server = McplsServer::new(
            translator,
            Arc::clone(&notification_cache),
            workspace_roots,
            subscriptions,
            false,
            Arc::clone(&delivery),
            floors,
            diagnostics,
            test_settle(),
        );
        TestServer {
            server,
            notification_cache,
            delivery,
        }
    }

    fn test_server_parts() -> TestServer {
        test_server_parts_with(DiagnosticsConfig::default())
    }

    /// The same, with an empty baseline adopted so `has_baseline()` is true
    /// and the flush is not answered with `starting_up()`.
    async fn test_server_with_baseline() -> TestServer {
        let parts = test_server_parts();
        parts.delivery.lock().await.set_baseline(HashMap::new());
        parts
    }

    /// A workspace file URI, spelled the way the running platform spells
    /// one, for a cache fixture whose file need not exist.
    fn workspace_uri(name: &str) -> lsp_types::Uri {
        let uri = if cfg!(windows) {
            format!("file:///C:/workspace/{name}")
        } else {
            format!("file:///workspace/{name}")
        };
        uri.parse().expect("a valid uri")
    }

    /// The footer switched on with its wait out of the test's way: what
    /// these assert is the guard, the record and the note, not the timing,
    /// which `wait_for_footer_quiet_at` covers directly.
    fn footer_config() -> DiagnosticsConfig {
        DiagnosticsConfig {
            footer: true,
            footer_grace_ms: 0,
            footer_quiet_ms: 0,
            footer_wait_ms: 0,
            ..DiagnosticsConfig::default()
        }
    }

    /// A server whose config enables the footer, with a baseline adopted
    /// and one error in the cache, so a footer has something to report.
    async fn test_server_with_footer_and_one_error() -> TestServer {
        server_with_footer_and_one_error(footer_config()).await
    }

    /// The same over a caller-chosen config, for a test that needs a cap
    /// the default leaves far out of reach.
    async fn server_with_footer_and_one_error(diagnostics: DiagnosticsConfig) -> TestServer {
        let parts = test_server_parts_with(diagnostics);
        parts.notification_cache.lock().await.store_diagnostics(
            &ServerId::from("rust"),
            &workspace_uri("broken.rs"),
            Some(1),
            vec![diagnostic_at("broken")],
        );
        parts.delivery.lock().await.set_baseline(HashMap::new());
        parts
    }

    /// One diagnostic, already converted to the DTO the payload carries,
    /// so a render test asserts the same values a hook would print.
    fn rendered_diagnostic(line: u32, severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            range: crate::bridge::Range {
                start: crate::bridge::Position2D { line, character: 3 },
                end: crate::bridge::Position2D { line, character: 9 },
            },
            severity,
            message: message.to_string(),
            code: None,
            source: None,
        }
    }

    /// The exact block a hook prints, pinned whole.
    ///
    /// Task 20's hook side parses nothing but still injects this verbatim
    /// into an agent's context, so the header line, the two-space indent,
    /// the `line:char severity message` order, the truncation line, the
    /// cleared phrasing and the note's placement are the contract. A
    /// `contains` assertion would let any of them move.
    #[test]
    fn test_the_hook_render_pins_every_line_it_produces() {
        let report = NewDiagnosticsResult {
            changed: vec![NewDiagnosticsFile {
                file_path: "/work/a.rs".to_string(),
                diagnostics: vec![
                    rendered_diagnostic(12, DiagnosticSeverity::Error, "mismatched types"),
                    rendered_diagnostic(40, DiagnosticSeverity::Warning, "unused variable"),
                ],
                omitted: 3,
            }],
            cleared: vec!["/work/b.rs".to_string()],
            omitted: 2,
            note: Some("2 file(s) were held back.".to_string()),
        };

        assert_eq!(
            render_for_hook(&report).expect("a report with content renders"),
            "/work/a.rs:\n  \
             12:3 error mismatched types\n  \
             40:3 warning unused variable\n  \
             (3 more not shown)\n\
             /work/b.rs: no diagnostics\n\
             2 file(s) were held back."
        );
    }

    /// The tool door advances the record in the same lock block that
    /// computes its report, so a hook report staged for the same session
    /// is superseded rather than committed on top of it.
    #[tokio::test]
    async fn test_an_immediate_flush_supersedes_a_staged_hook_report() {
        let parts = test_server_with_footer_and_one_error().await;
        let session = SessionId::from("s1".to_string());

        let (staged, token) = parts
            .server
            .flush_now(&RecordId::from(&session), Advance::OnAcknowledgement)
            .await;
        assert_eq!(staged.changed.len(), 1);
        let token = token.expect("a staged report with content carries a token");

        let (now, none) = parts
            .server
            .flush_now(&RecordId::from(&session), Advance::Now)
            .await;
        assert_eq!(
            now.changed.len(),
            1,
            "the hook's report was never confirmed"
        );
        assert_eq!(
            none, None,
            "an immediate flush leaves nothing to acknowledge"
        );

        assert!(
            !parts.server.commit_for_hook(&session, token).await,
            "the record already holds what the tool result showed"
        );

        let (after, _) = parts
            .server
            .flush_now(&RecordId::from(&session), Advance::OnAcknowledgement)
            .await;
        assert!(after.changed.is_empty());
    }

    /// A flush with nothing to say prints nothing at all: a hook's output is
    /// injected into the agent's context, and an empty structure there is
    /// noise the agent has to interpret.
    #[test]
    fn test_an_empty_report_renders_as_no_text() {
        assert!(
            render_for_hook(&NewDiagnosticsResult {
                changed: Vec::new(),
                cleared: Vec::new(),
                omitted: 0,
                note: None,
            })
            .is_none()
        );
    }

    /// A rename result shaped the way `rename_symbol` returns one.
    fn sample_rename_result() -> RenameResult {
        RenameResult {
            changes: Vec::new(),
            resource_operations: Vec::new(),
            applied: true,
            files_written: vec!["/workspace/broken.rs".to_string()],
        }
    }

    #[tokio::test]
    async fn test_a_footer_is_silent_before_the_baseline_lands() {
        let parts = test_server_parts_with(DiagnosticsConfig {
            footer: true,
            ..DiagnosticsConfig::default()
        });

        let epoch_before = parts.server.context.settle.progress_epoch();
        let footer = parts.server.footer_for_write(epoch_before).await;

        assert!(
            footer.is_none(),
            "flush seeds a session record from the baseline, and a record made \
             before the baseline lands stays empty forever, so the next flush \
             would report the whole workspace. The settle deadline is 300 \
             seconds, which puts the first rename of a session squarely inside \
             this window"
        );
    }

    #[tokio::test]
    async fn test_a_footer_consumes_what_it_reports() {
        let parts = test_server_with_footer_and_one_error().await;

        let epoch_before = parts.server.context.settle.progress_epoch();
        let footer = parts
            .server
            .footer_for_write(epoch_before)
            .await
            .expect("a report");
        assert_eq!(footer.changed.len(), 1);

        let raw = parts
            .server
            .get_new_diagnostics()
            .await
            .expect("the flush tool");
        let report: serde_json::Value = serde_json::from_str(&raw).expect("json");
        assert!(
            report["changed"].as_array().expect("changed").is_empty(),
            "one report per problem: the footer and the flush share one record"
        );
    }

    /// The footer's own note, in both of the shapes it is built in.
    ///
    /// It is the only thing telling the agent that a footer is a floor
    /// rather than the whole answer. An agent that reads one as complete
    /// stops looking, and whatever landed after the wait is then never
    /// asked for, so the sentence is pinned whole, and pinned again where
    /// it follows a note the flush had already written.
    #[tokio::test]
    async fn test_a_footer_says_it_is_best_effort() {
        let parts = test_server_with_footer_and_one_error().await;

        let epoch_before = parts.server.context.settle.progress_epoch();
        let footer = parts
            .server
            .footer_for_write(epoch_before)
            .await
            .expect("a report");

        assert_eq!(
            footer.note.as_deref(),
            Some(
                "This footer is best effort; anything slower than the wait \
                 arrives in the next get_new_diagnostics."
            )
        );
    }

    #[tokio::test]
    async fn test_a_footer_that_held_a_file_back_says_both_things() {
        let parts = server_with_footer_and_one_error(DiagnosticsConfig {
            max_total: 1,
            ..footer_config()
        })
        .await;
        // A second file the total budget cannot reach, so the flush writes
        // a note of its own for the footer's to follow.
        parts.notification_cache.lock().await.store_diagnostics(
            &ServerId::from("rust"),
            &workspace_uri("other.rs"),
            Some(1),
            vec![diagnostic_at("also broken")],
        );

        let epoch_before = parts.server.context.settle.progress_epoch();
        let footer = parts
            .server
            .footer_for_write(epoch_before)
            .await
            .expect("a report");

        assert_eq!(
            footer.note.as_deref(),
            Some(
                "1 file(s) were held back by the diagnostics caps this call; \
                 call again to see them. This footer is best effort; anything \
                 slower than the wait arrives in the next get_new_diagnostics."
            )
        );
    }

    /// The guard the three write tools run the footer behind, both ways.
    ///
    /// This is what `if result.applied` buys, so it is asserted against the
    /// method the call sites use rather than against a serialized struct: a
    /// serde test proves `skip_serializing_if`, not the guard.
    #[tokio::test]
    async fn test_no_footer_when_the_tool_wrote_nothing() {
        let parts = test_server_with_footer_and_one_error().await;
        let epoch_before = parts.server.context.settle.progress_epoch();

        assert!(
            parts
                .server
                .footer_if_written(false, epoch_before)
                .await
                .is_none(),
            "a rename with apply false changed nothing and has nothing to report"
        );
        assert!(
            parts
                .server
                .footer_if_written(true, epoch_before)
                .await
                .is_some(),
            "and a call that did write must still get one, or the guard is just \
             a footer that never fires"
        );
    }

    /// The three tools that can write to the working tree.
    ///
    /// Each of them appends the diagnostics its own edit produced on two
    /// adjacent lines. Driving all three from one place is what keeps a fourth
    /// write tool, or a refactor of one of these, from quietly losing them.
    #[derive(Clone, Copy, Debug)]
    enum WriteTool {
        Rename,
        Format,
        CodeAction,
    }

    const WRITE_TOOLS: [WriteTool; 3] =
        [WriteTool::Rename, WriteTool::Format, WriteTool::CodeAction];

    impl WriteTool {
        fn mcp_name(self) -> &'static str {
            match self {
                Self::Rename => "rename_symbol",
                Self::Format => "format_document",
                Self::CodeAction => "apply_code_action",
            }
        }

        fn lsp_method(self) -> &'static str {
            match self {
                Self::Rename => "textDocument/rename",
                Self::Format => "textDocument/formatting",
                Self::CodeAction => "textDocument/codeAction",
            }
        }

        fn mcp_arguments(self, path: &str) -> serde_json::Value {
            match self {
                Self::Rename => json!({
                    "file_path": path,
                    "line": 1,
                    "character": 4,
                    "new_name": "new",
                    "apply": true,
                }),
                Self::Format => json!({
                    "file_path": path,
                    "tab_size": 4,
                    "insert_spaces": true,
                    "apply": true,
                }),
                Self::CodeAction => json!({
                    "file_path": path,
                    "start_line": 1,
                    "start_character": 1,
                    "end_line": 1,
                    "end_character": 5,
                    "action_index": 0,
                }),
            }
        }

        /// What its language server has to advertise for the call to get as
        /// far as a request.
        fn capabilities(self) -> lsp_types::ServerCapabilities {
            match self {
                Self::Rename => lsp_types::ServerCapabilities {
                    rename_provider: Some(lsp_types::OneOf::Left(true)),
                    ..Default::default()
                },
                Self::Format => lsp_types::ServerCapabilities {
                    document_formatting_provider: Some(lsp_types::OneOf::Left(true)),
                    ..Default::default()
                },
                Self::CodeAction => lsp_types::ServerCapabilities {
                    code_action_provider: Some(lsp_types::CodeActionProviderCapability::Simple(
                        true,
                    )),
                    ..Default::default()
                },
            }
        }

        /// The apply key the deployment has to permit for the write to
        /// happen rather than be described.
        fn apply_config(self) -> ApplyConfig {
            match self {
                Self::Rename => ApplyConfig {
                    rename: true,
                    ..ApplyConfig::default()
                },
                Self::Format => ApplyConfig {
                    format_document: true,
                    ..ApplyConfig::default()
                },
                Self::CodeAction => ApplyConfig {
                    code_actions: true,
                    ..ApplyConfig::default()
                },
            }
        }

        /// The reply to this tool's own request, carrying an edit that
        /// rewrites `old` to `new` on the fixture's first line.
        fn reply_rewriting(self, uri: &Uri) -> serde_json::Value {
            let edits = json!([{
                "range": {
                    "start": { "line": 0, "character": 3 },
                    "end": { "line": 0, "character": 6 },
                },
                "newText": "new",
            }]);
            match self {
                Self::Rename => json!({ "changes": { uri.as_str(): edits } }),
                Self::Format => edits,
                Self::CodeAction => json!([{
                    "title": "Rewrite it",
                    "edit": { "changes": { uri.as_str(): edits } },
                }]),
            }
        }

        /// Call the tool over `path` with `apply` on, and return its raw
        /// JSON result.
        async fn call_applying(self, server: &McplsServer, path: &str) -> String {
            let result = match self {
                Self::Rename => {
                    server
                        .rename_symbol(Parameters(RenameParams {
                            position: PositionParams {
                                file_path: path.to_string(),
                                line: 1,
                                character: 4,
                            },
                            new_name: "new".to_string(),
                            apply: true,
                        }))
                        .await
                }
                Self::Format => {
                    server
                        .format_document(Parameters(FormatDocumentParams {
                            file_path: path.to_string(),
                            tab_size: 4,
                            insert_spaces: true,
                            apply: true,
                        }))
                        .await
                }
                Self::CodeAction => {
                    server
                        .apply_code_action(Parameters(ApplyCodeActionParams {
                            file_path: path.to_string(),
                            range: RangeParams {
                                start_line: 1,
                                start_character: 1,
                                end_line: 1,
                                end_character: 5,
                            },
                            kind_filter: None,
                            action_index: Some(0),
                            action_title: None,
                        }))
                        .await
                }
            };
            result.unwrap_or_else(|error| panic!("{self:?} must apply its edit: {error}"))
        }
    }

    /// An `McplsServer` whose translator is routed to a fake language
    /// server that advertises what `tool` needs and is permitted to write
    /// what `tool` writes, over a workspace holding one `main.rs`.
    struct WriteFixture {
        server: McplsServer,
        fake: FakeServer,
        /// The fixture file, canonicalized, which is the spelling the
        /// applier reports and the URI is built from.
        path: PathBuf,
        uri: Uri,
        _dir: tempfile::TempDir,
    }

    impl WriteFixture {
        fn new(tool: WriteTool, diagnostics: DiagnosticsConfig) -> Self {
            let dir = tempfile::tempdir().expect("a temp dir");
            let (translator, fake) =
                translator_with_capabilities(&dir, &ServerId::from("rust"), tool.capabilities());
            let translator = Arc::new(translator.with_applier(Arc::new(Applier::new(
                vec![dir.path().to_path_buf()],
                tool.apply_config(),
            ))));

            let path = dir.path().join("main.rs");
            std::fs::write(&path, "fn old() {}\n").expect("write the fixture");
            let path = dunce::canonicalize(path).expect("the fixture exists");
            let uri = crate::bridge::path_to_uri(&path).expect("a uri for the fixture");

            let context = BridgeContext::new(
                translator,
                Arc::new(Mutex::new(NotificationCache::new())),
                Arc::from(vec![dir.path().to_path_buf()]),
                Arc::new(ResourceSubscriptions::new()),
                false,
                Arc::new(Mutex::new(DiagnosticsDelivery::new(diagnostics))),
                Arc::new(FloorTable::new(&diagnostics, &[])),
                diagnostics,
                test_settle(),
            );
            let context = Arc::new(context);

            Self {
                server: McplsServer::from_context(context),
                fake,
                path,
                uri,
                _dir: dir,
            }
        }

        /// Adopt an empty baseline and record one error against the fixture
        /// file, so a footer has both a record to diff against and
        /// something to report.
        async fn with_one_error(self) -> Self {
            self.server
                .context
                .notification_cache
                .lock()
                .await
                .store_diagnostics(
                    &ServerId::from("rust"),
                    &self.uri,
                    Some(1),
                    vec![diagnostic_at("broken")],
                );
            self.server
                .context
                .delivery
                .lock()
                .await
                .set_baseline(HashMap::new());
            self
        }

        /// Drive `tool` to completion against the fake server, answering the
        /// one request it sends, and return its raw JSON result.
        async fn apply(&mut self, tool: WriteTool) -> String {
            let server = self.server.clone();
            let path = self.path.display().to_string();
            let reply = tool.reply_rewriting(&self.uri);
            let fake = &mut self.fake;
            let (result, ()) = tokio::join!(tool.call_applying(&server, &path), async move {
                let mut wire = tokio::io::BufReader::new(&mut fake.write_stdout);
                let request = read_framed_reply(&mut wire).await;
                write_response(&mut fake.read_half_stdin, &request["id"], reply).await;
            });
            result
        }
    }

    /// Every write tool appends the diagnostics its own edit produced.
    ///
    /// Asserted through the tool's own JSON rather than through
    /// `footer_if_written`, which is separately covered: what is under test
    /// here is that each call site still calls it, and a call site that
    /// stopped would return a result with no `new_diagnostics` key at all.
    #[tokio::test]
    async fn test_every_write_tool_appends_its_own_diagnostics() {
        for tool in WRITE_TOOLS {
            let mut fixture = WriteFixture::new(
                tool,
                DiagnosticsConfig {
                    footer: true,
                    footer_grace_ms: 0,
                    footer_quiet_ms: 0,
                    footer_wait_ms: 0,
                    ..DiagnosticsConfig::default()
                },
            )
            .with_one_error()
            .await;
            let expected = fixture.path.display().to_string();

            let result: serde_json::Value =
                serde_json::from_str(&fixture.apply(tool).await).expect("json");

            assert_eq!(
                result["new_diagnostics"]["changed"][0]["file_path"],
                json!(expected),
                "{tool:?} answered without the diagnostics its own write \
                 produced, so the agent has to ask for them in a second call \
                 and pays a turn for it: {result}"
            );
        }
    }

    async fn write_lsp_notification<W>(writer: &mut W, method: &str, params: serde_json::Value)
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::AsyncWriteExt as _;

        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let body = serde_json::to_vec(&message).unwrap();
        writer
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        writer.write_all(&body).await.unwrap();
        writer.flush().await.unwrap();
    }

    struct CausalWriteFixture {
        server: McplsServer,
        translator: Arc<Translator>,
        fake: FakeServer,
        settle: Arc<ServerSettle>,
        pump_cancel: tokio::sync::watch::Sender<bool>,
        pump: tokio::task::JoinHandle<()>,
        path: PathBuf,
        uri: Uri,
        _dir: tempfile::TempDir,
    }

    impl CausalWriteFixture {
        fn new(tool: WriteTool) -> Self {
            let dir = tempfile::tempdir().expect("a temp dir");
            let server_id = ServerId::from("rust");
            let cache = Arc::new(Mutex::new(NotificationCache::new()));
            let subscriptions = Arc::new(ResourceSubscriptions::new());
            let workspace_root = dunce::canonicalize(dir.path()).expect("the workspace exists");
            let workspace_roots: Arc<[PathBuf]> = Arc::from(vec![workspace_root.clone()]);
            let diagnostics = DiagnosticsConfig {
                footer: true,
                footer_grace_ms: 100,
                footer_quiet_ms: 100,
                footer_wait_ms: 3_000,
                ..DiagnosticsConfig::default()
            };
            let delivery = Arc::new(Mutex::new(DiagnosticsDelivery::new(diagnostics)));
            let floors = Arc::new(FloorTable::new(&diagnostics, &[]));
            let settle = Arc::new(ServerSettle::new(
                Duration::from_millis(25),
                Duration::from_secs(5),
            ));
            settle.set_diagnostics_owners([server_id.clone()]);

            let (client, fake, notification_rx) = FakeServer::with_notifications(server_id.clone());
            let mut translator = Translator::new()
                .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]))
                .with_router(crate::config::ToolRouter::catch_all([(
                    server_id.clone(),
                    "rust".to_string(),
                )]))
                .with_notification_cache(Arc::clone(&cache))
                .with_applier(Arc::new(Applier::new(
                    vec![workspace_root.clone()],
                    tool.apply_config(),
                )));
            translator.set_workspace_roots(vec![workspace_root]);
            translator.register_client(server_id.clone(), client);
            translator.register_server(
                server_id.clone(),
                crate::lsp::LspServer::new_for_test(tool.capabilities()),
            );
            let translator = Arc::new(translator);

            let path = dir.path().join("main.rs");
            std::fs::write(&path, "fn old() {}\n").expect("write the fixture");
            let path = dunce::canonicalize(path).expect("the fixture exists");
            let uri = crate::bridge::path_to_uri(&path).expect("a uri for the fixture");
            let server = McplsServer::new(
                Arc::clone(&translator),
                Arc::clone(&cache),
                Arc::clone(&workspace_roots),
                Arc::clone(&subscriptions),
                false,
                Arc::clone(&delivery),
                Arc::clone(&floors),
                diagnostics,
                Arc::clone(&settle),
            );

            let (pump_cancel, cancel_rx) = tokio::sync::watch::channel(false);
            let pump = tokio::spawn(crate::diagnostics_pump(
                server_id,
                notification_rx,
                cancel_rx,
                crate::PumpShared {
                    notification_cache: cache,
                    subs: subscriptions,
                    workspace_roots,
                    document_tracker: Arc::clone(translator.document_tracker()),
                    settle: Arc::clone(&settle),
                    delivery,
                    floors,
                },
            ));

            Self {
                server,
                translator,
                fake,
                settle,
                pump_cancel,
                pump,
                path,
                uri,
                _dir: dir,
            }
        }

        #[allow(clippy::too_many_lines)]
        async fn run(mut self, tool: WriteTool) {
            use rmcp::ServiceExt as _;
            use tokio::io::{AsyncWriteExt as _, BufReader, BufStream};

            let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            self.translator.install_resync_pause(reached_tx, release_rx);
            let owner = ServerId::from("rust");
            let baseline_generation = self.settle.diagnostics_baseline_generation(&owner);
            self.server
                .context
                .delivery
                .lock()
                .await
                .set_baseline(HashMap::new());
            if let Some(generation) = baseline_generation {
                self.settle
                    .finish_diagnostics_baseline_merge(&owner, generation);
            }

            let (server_io, client_io) = tokio::io::duplex(65_536);
            let server = self.server.clone();
            let started = tokio::spawn(async move { server.serve(server_io).await.unwrap() });
            let mut wire = BufStream::new(client_io);
            let initialized = mcp_test_request(
                &mut wire,
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25", "capabilities": {},
                        "clientInfo": {"name": "causal-write-test", "version": "1"}
                    }
                }),
            )
            .await;
            assert!(initialized["result"].is_object(), "{initialized}");
            wire.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
                .await
                .unwrap();
            wire.flush().await.unwrap();
            let running = started.await.unwrap();

            let epoch_before = self.settle.progress_epoch();
            let path = self.path.display().to_string();
            let request = json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": tool.mcp_name(), "arguments": tool.mcp_arguments(&path)}
            });
            let call = tokio::spawn(async move {
                let response = mcp_test_request(&mut wire, request).await;
                (Instant::now(), response)
            });

            let mut lsp_wire = BufReader::new(&mut self.fake.write_stdout);
            let opened = crate::test_support::read_framed_message(&mut lsp_wire).await;
            assert_eq!(opened["method"], "textDocument/didOpen", "{opened}");
            let request = crate::test_support::read_framed_message(&mut lsp_wire).await;
            assert_eq!(request["method"], tool.lsp_method(), "{request}");
            write_response(
                &mut self.fake.read_half_stdin,
                &request["id"],
                tool.reply_rewriting(&self.uri),
            )
            .await;

            let mut saw_change = false;
            loop {
                let message = crate::test_support::read_framed_message(&mut lsp_wire).await;
                match message["method"].as_str().unwrap_or_default() {
                    "textDocument/didOpen" => {}
                    "textDocument/didChange" => {
                        assert_eq!(
                            message["params"]["contentChanges"][0]["text"], "fn new() {}\n",
                            "{tool:?} sent stale content: {message}"
                        );
                        saw_change = true;
                    }
                    "textDocument/didSave" => {
                        assert!(saw_change, "{tool:?} saved before its change notification");
                        write_lsp_notification(
                            &mut self.fake.read_half_stdin,
                            "$/progress",
                            json!({
                                "token": "causal-write",
                                "value": {"kind": "begin", "title": "checking"}
                            }),
                        )
                        .await;
                        break;
                    }
                    method => panic!("{tool:?} produced unexpected LSP notification: {method}"),
                }
            }

            tokio::time::timeout(Duration::from_secs(2), reached_rx)
                .await
                .expect("translator reached its post-save pause")
                .expect("translator pause receiver stayed connected");
            let epoch_after_begin = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let epoch = self.settle.progress_epoch();
                    if epoch > epoch_before {
                        break epoch;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("real diagnostics pump observed progress begin");
            assert!(epoch_after_begin > epoch_before);
            assert!(
                !call.is_finished(),
                "{tool:?} completed while translator resync was paused"
            );

            release_tx
                .send(())
                .expect("translator resync pause receiver stayed connected");
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert!(
                !call.is_finished(),
                "{tool:?} completed before progress end after footer grace"
            );

            let progress_end_sent_at = Instant::now();
            write_lsp_notification(
                &mut self.fake.read_half_stdin,
                "$/progress",
                json!({
                    "token": "causal-write",
                    "value": {"kind": "end"}
                }),
            )
            .await;
            write_lsp_notification(
                &mut self.fake.read_half_stdin,
                "textDocument/publishDiagnostics",
                json!({
                    "uri": self.uri.as_str(),
                    "diagnostics": [{
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 1}
                        },
                        "severity": 1,
                        "message": "after causal write"
                    }]
                }),
            )
            .await;

            let (response_completed_at, response) =
                tokio::time::timeout(Duration::from_secs(3), call)
                    .await
                    .expect("MCP write completed after progress end")
                    .expect("MCP write task stayed connected");
            assert!(
                response_completed_at.duration_since(progress_end_sent_at)
                    >= Duration::from_millis(self.server.context.diagnostics.footer_quiet_ms),
                "{tool:?} completed before the configured footer quiet interval"
            );
            assert!(response["error"].is_null(), "{response}");
            assert_eq!(response["result"]["isError"], false, "{response}");
            let payload: serde_json::Value = serde_json::from_str(
                response["result"]["content"][0]["text"]
                    .as_str()
                    .expect("write result text"),
            )
            .expect("write result JSON");
            assert_eq!(payload["applied"], true, "{tool:?}: {payload}");
            assert_eq!(
                payload["files_written"],
                json!([self.path.display().to_string()]),
                "{tool:?}: {payload}"
            );
            assert_eq!(
                std::fs::read_to_string(&self.path).unwrap(),
                "fn new() {}\n",
                "{tool:?} did not persist its edit"
            );
            let changed = payload["new_diagnostics"]["changed"]
                .as_array()
                .expect("footer changed diagnostics");
            assert!(
                changed.iter().any(|file| {
                    file["file_path"] == self.path.display().to_string()
                        && file["diagnostics"].as_array().is_some_and(|diagnostics| {
                            diagnostics
                                .iter()
                                .any(|diagnostic| diagnostic["message"] == "after causal write")
                        })
                }),
                "{tool:?} footer omitted the final diagnostic: {payload}"
            );

            let _ = self.pump_cancel.send(true);
            self.pump.abort();
            running.cancel().await.unwrap();
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn i1_t9_real_progress_during_resync_reaches_footer() {
        tokio::time::timeout(Duration::from_secs(15), async {
            for tool in WRITE_TOOLS {
                CausalWriteFixture::new(tool).run(tool).await;
            }
        })
        .await
        .expect("bounded causal write transport scenarios");
    }

    #[test]
    fn test_the_wrapper_omits_an_absent_footer_from_its_json() {
        let wrapped = WithDiagnostics {
            result: sample_rename_result(),
            new_diagnostics: None,
        };
        let json = serde_json::to_string(&wrapped).expect("serialize");

        assert!(!json.contains("new_diagnostics"));
    }

    #[test]
    fn test_the_wrapper_flattens_rather_than_nesting() {
        let wrapped = WithDiagnostics {
            result: sample_rename_result(),
            new_diagnostics: None,
        };
        let json: serde_json::Value = serde_json::to_value(&wrapped).expect("serialize");

        assert!(
            json.get("applied").is_some(),
            "the existing result's fields stay at the top level; a caller \
             parsing RenameResult must keep parsing it"
        );
    }

    /// A `wait_for_footer_quiet_at` tick that never suspends: it records
    /// every step it was asked to sleep into `steps`, advances an internal
    /// counter by it, and reports `start` plus the running total, so a test
    /// can drive the exact loop that ships without paying any of its real
    /// time.
    ///
    /// The steps are recorded rather than only accumulated because what the
    /// loop reports and what it actually spends are two facts: a caller
    /// reading only the return value cannot see a sleep the loop paid and
    /// then declined to count.
    fn instant_tick(
        start: Instant,
        steps: &std::cell::RefCell<Vec<Duration>>,
    ) -> impl Fn(Duration) -> std::future::Ready<Instant> {
        let accumulated = std::cell::Cell::new(Duration::ZERO);
        move |step| {
            steps.borrow_mut().push(step);
            let total = accumulated.get() + step;
            accumulated.set(total);
            std::future::ready(start + total)
        }
    }

    /// The steps an `instant_tick` records, for a test that only asserts on
    /// what the loop returned.
    fn unread_steps() -> std::cell::RefCell<Vec<Duration>> {
        std::cell::RefCell::new(Vec::new())
    }

    /// Branch one: the grace period elapses before quiet is consulted.
    #[tokio::test]
    async fn test_the_footer_wait_never_returns_before_its_grace_period() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
        let start = Instant::now();

        // Nothing has ever begun, so the workspace reads as quiet from the very
        // first sample.
        let ended = wait_for_footer_quiet_at(
            &settle,
            settle.progress_epoch(),
            FooterTiming {
                grace: Duration::from_millis(250),
                quiet: Duration::from_millis(200),
                cap: Duration::from_secs(15),
            },
            instant_tick(start, &unread_steps()),
        )
        .await;

        assert_eq!(
            ended,
            Duration::from_millis(250),
            "rust-analyzer's flycheck begins about 90ms after a didSave, and a \
             footer that sampled before then would see a quiet workspace and \
             report the state from before the edit"
        );
    }

    /// Branch two: quiet ends the wait early.
    #[tokio::test]
    async fn test_the_footer_wait_ends_on_quiet_rather_than_on_its_cap() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
        let rust = ServerId::from("rust");
        let start = Instant::now();
        let epoch_before = settle.progress_epoch();
        settle.begin(&rust, &json!("flycheck"));
        settle.end_at(
            &rust,
            &json!("flycheck"),
            start + Duration::from_millis(400),
        );

        let ended = wait_for_footer_quiet_at(
            &settle,
            epoch_before,
            FooterTiming {
                grace: Duration::from_millis(250),
                quiet: Duration::from_millis(200),
                cap: Duration::from_secs(15),
            },
            instant_tick(start, &unread_steps()),
        )
        .await;

        assert!(
            ended < Duration::from_secs(1),
            "quiet arrived at 600ms, well inside the cap; a test that could only \
             ever end on the cap would pass against a broken quiet check"
        );
        assert!(
            ended >= Duration::from_millis(600),
            "and not before the quiet debounce has actually run out"
        );
    }

    /// Branch three: the cap ends it when quiet never arrives.
    #[tokio::test]
    async fn test_the_footer_wait_ends_on_its_cap_when_quiet_never_arrives() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
        let rust = ServerId::from("rust");
        let start = Instant::now();
        let epoch_before = settle.progress_epoch();
        settle.begin(&rust, &json!("flycheck"));
        // No `end_at`: the check is still running when the cap expires.

        let ended = wait_for_footer_quiet_at(
            &settle,
            epoch_before,
            FooterTiming {
                grace: Duration::from_millis(250),
                quiet: Duration::from_millis(200),
                cap: Duration::from_secs(15),
            },
            instant_tick(start, &unread_steps()),
        )
        .await;

        assert_eq!(
            ended,
            Duration::from_secs(15),
            "the footer is best effort: it reports what has landed rather than \
             waiting on a build that has not finished"
        );
    }

    /// `footer_wait_ms` documents a total, so a larger `footer_grace_ms`
    /// must not be paid in full.
    ///
    /// The value returned and the time actually spent are two facts, and
    /// only the first was bounded: the loop slept the whole grace before it
    /// ever looked at the cap, then reported the cap. A user who lowered
    /// `footer_wait_ms` to make writes snappier still paid the default
    /// grace on every write, and the overshoot grew with the gap rather
    /// than staying inside the documented one sampling step.
    #[tokio::test]
    async fn test_a_grace_beyond_the_cap_is_not_paid_beyond_it() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
        let rust = ServerId::from("rust");
        let start = Instant::now();
        let epoch_before = settle.progress_epoch();
        // No `end_at`: nothing here ever reads as quiet, so only the cap can
        // end the wait.
        settle.begin(&rust, &json!("flycheck"));
        let steps = unread_steps();

        let ended = wait_for_footer_quiet_at(
            &settle,
            epoch_before,
            FooterTiming {
                grace: Duration::from_millis(250),
                quiet: Duration::from_millis(200),
                cap: Duration::from_millis(100),
            },
            instant_tick(start, &steps),
        )
        .await;

        assert_eq!(ended, Duration::from_millis(100));
        assert_eq!(
            steps.borrow().iter().sum::<Duration>(),
            Duration::from_millis(100),
            "the cap is documented as the whole wait, grace included, so a \
             wait that reported the cap while sleeping the grace was reporting \
             a number it had not honoured"
        );
    }

    /// The sampling step, and the overshoot past the cap it buys.
    ///
    /// Both are named in `footer_wait_ms`'s own documentation as 50 ms, and
    /// neither is readable from what the wait returns: the loop reports the
    /// cap however long its samples were. What it spent is the only place
    /// the step shows, so that is what this reads.
    #[tokio::test]
    async fn test_the_footer_samples_every_fifty_milliseconds() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
        let rust = ServerId::from("rust");
        let start = Instant::now();
        let epoch_before = settle.progress_epoch();
        // No `end_at`: nothing here ever reads as quiet, so the loop samples
        // until the cap.
        settle.begin(&rust, &json!("flycheck"));
        let steps = unread_steps();

        let ended = wait_for_footer_quiet_at(
            &settle,
            epoch_before,
            FooterTiming {
                grace: Duration::from_millis(100),
                quiet: Duration::from_millis(200),
                cap: Duration::from_millis(180),
            },
            instant_tick(start, &steps),
        )
        .await;

        assert_eq!(ended, Duration::from_millis(180));
        assert_eq!(
            steps.borrow()[1..],
            [Duration::from_millis(50), Duration::from_millis(50)],
            "everything after the grace is one sampling step, and a shorter \
             one turns a write's wait into a busier poll of the same tracker"
        );
        assert_eq!(
            steps.borrow().iter().sum::<Duration>(),
            Duration::from_millis(200),
            "a sleep already paid cannot be undone, so a wait that never goes \
             quiet runs to the first sample past the cap: at most one step \
             beyond it, which is what the user-facing wait is documented to \
             overshoot by"
        );
    }

    /// Work that was already running when the edit landed does not eat the cap.
    #[tokio::test]
    async fn test_an_index_already_in_flight_does_not_hold_the_footer() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
        let rust = ServerId::from("rust");
        let start = Instant::now();
        settle.begin(&rust, &json!("rustAnalyzer/Indexing"));
        // Captured after the begin to model work already in flight before the
        // write.
        let epoch_before = settle.progress_epoch();

        let ended = wait_for_footer_quiet_at(
            &settle,
            epoch_before,
            FooterTiming {
                grace: Duration::from_millis(250),
                quiet: Duration::from_millis(200),
                cap: Duration::from_secs(15),
            },
            instant_tick(start, &unread_steps()),
        )
        .await;

        assert_eq!(
            ended,
            Duration::from_millis(250),
            "an index after a Cargo.toml change can run for minutes and is not \
             this call's check; waiting on it would spend the whole cap on \
             something this tool call did not cause"
        );
    }

    /// A footer with a real, non-zero grace does not answer before it
    /// elapses. `is_quiet_at` reads a workspace that never reported
    /// `$/progress` as quiet from the very first sample, so with nothing
    /// gating it but the grace, this proves `footer_for_write` actually
    /// pays that delay in real time rather than skipping straight to the
    /// flush. Runs on a paused tokio clock so the assertion is exact and
    /// the test itself does not sleep.
    #[tokio::test(start_paused = true)]
    async fn test_the_footer_actually_waits_out_its_grace_period() {
        let parts = test_server_parts_with(DiagnosticsConfig {
            footer: true,
            footer_grace_ms: 250,
            footer_quiet_ms: 200,
            footer_wait_ms: 15_000,
            ..DiagnosticsConfig::default()
        });
        parts.delivery.lock().await.set_baseline(HashMap::new());
        let epoch_before = parts.server.context.settle.progress_epoch();
        let server = parts.server;

        let handle = tokio::spawn(async move { server.footer_for_write(epoch_before).await });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        tokio::time::advance(Duration::from_millis(249)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(
            !handle.is_finished(),
            "nothing ever began, so is_quiet_at would read quiet at any real \
             instant; only the grace sleep can be holding this open, and it \
             has not elapsed yet"
        );

        tokio::time::advance(Duration::from_millis(5)).await;
        let footer = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the footer finished once its grace elapsed")
            .expect("the footer task");
        assert!(footer.is_some());
    }

    /// The footer defaults to off, and a disabled footer must never touch
    /// its timers: with grace and cap both an hour, a regression that
    /// forgot the `footer` guard would hang this test until it timed out
    /// instead of merely running slower.
    #[tokio::test]
    async fn test_the_footer_is_off_by_default_and_never_waits() {
        let parts = test_server_parts_with(DiagnosticsConfig {
            footer_grace_ms: 3_600_000,
            footer_wait_ms: 3_600_000,
            ..DiagnosticsConfig::default()
        });
        parts.delivery.lock().await.set_baseline(HashMap::new());

        let start = Instant::now();
        let epoch_before = parts.server.context.settle.progress_epoch();
        let footer = parts.server.footer_for_write(epoch_before).await;

        assert!(footer.is_none(), "footer defaults to off");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "a disabled footer must return before ever consulting its timers"
        );
    }

    /// With the footer absent, `WithDiagnostics`'s JSON must be exactly what
    /// the tool returned before this task: `#[serde(flatten)]` inlines the
    /// result's fields in declaration order and `skip_serializing_if` drops
    /// `new_diagnostics` entirely, so nothing about the wrapper is visible.
    #[test]
    fn test_the_wrapper_matches_the_bare_result_when_the_footer_is_absent() {
        let bare = serde_json::to_string(&sample_rename_result()).expect("serialize");
        let wrapped = serde_json::to_string(&WithDiagnostics {
            result: sample_rename_result(),
            new_diagnostics: None,
        })
        .expect("serialize");

        assert_eq!(
            bare, wrapped,
            "a caller of a write tool with the footer off must see byte-identical \
             output to before this task"
        );
    }

    /// The flush acquires `delivery` before `notification_cache`. With the
    /// cache lock held from the outside, the flush stalls at a point where it
    /// must already own `delivery`; the opposite acquisition order would leave
    /// `delivery` free at that moment.
    #[tokio::test]
    async fn test_a_flush_takes_delivery_before_the_cache() {
        let parts = test_server_with_baseline().await;
        let cache = Arc::clone(&parts.notification_cache);
        let delivery = Arc::clone(&parts.delivery);
        let server = parts.server;

        let held = cache.lock().await;
        let flush = tokio::spawn(async move { server.get_new_diagnostics().await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            delivery.try_lock().is_err(),
            "a flush blocked on the cache lock must already hold delivery; \
             finding delivery free means the cache was taken first, and two \
             sites taking these locks in opposite orders deadlock"
        );

        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(5), flush)
            .await
            .expect("the flush finished once the cache lock was free")
            .expect("the flush task")
            .expect("the flush");
    }

    fn create_test_server() -> McplsServer {
        create_test_server_with_ignored_flag(false)
    }

    fn create_test_server_with_ignored_flag(project_config_ignored: bool) -> McplsServer {
        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let (delivery, floors) = default_delivery_and_floors();
        McplsServer::new(
            translator,
            notification_cache,
            workspace_roots,
            subscriptions,
            project_config_ignored,
            delivery,
            floors,
            DiagnosticsConfig::default(),
            test_settle(),
        )
    }

    #[tokio::test]
    async fn test_server_info() {
        let server = create_test_server();
        let info = server.get_info();

        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "mcpls");
        assert!(info.instructions.is_some());
    }

    #[tokio::test]
    async fn test_server_info_omits_ignore_notice_when_not_ignored() {
        let server = create_test_server_with_ignored_flag(false);
        let info = server.get_info();

        assert!(!info.instructions.unwrap().contains("ignored as untrusted"));
    }

    #[tokio::test]
    async fn test_server_info_surfaces_ignored_project_config() {
        let server = create_test_server_with_ignored_flag(true);
        let info = server.get_info();

        let instructions = info.instructions.unwrap();
        assert!(instructions.contains("ignored as untrusted"));
        assert!(instructions.contains("--trust-project-config"));
    }

    #[tokio::test]
    async fn test_hover_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(PositionParams {
            file_path: "/nonexistent/file.rs".to_string(),
            line: 1,
            character: 1,
        });

        // This should return an error (no LSP server configured)
        let result = server.get_hover(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_definition_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(PositionParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
        });

        let result = server.get_definition(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_references_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(ReferencesParams {
            position: PositionParams {
                file_path: "/test/file.rs".to_string(),
                line: 10,
                character: 5,
            },
            include_declaration: false,
        });

        let result = server.get_references(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_diagnostics_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(DiagnosticsParams {
            file_path: "/test/file.rs".to_string(),
        });

        let result = server.get_diagnostics(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn test_rename_resyncs_unopened_targets_before_any_save() {
        use rmcp::ServiceExt as _;
        use tokio::io::{AsyncWriteExt as _, BufReader, BufStream};

        use crate::test_support::read_framed_message;

        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::TempDir::new().unwrap();
            let paths = [dir.path().join("anchor.rs"), dir.path().join("unopened.rs")];
            for path in &paths {
                std::fs::write(path, "fn old() {}\n").unwrap();
            }
            let paths = paths.map(|path| dunce::canonicalize(path).expect("fixture exists"));
            let uris = paths.each_ref().map(|path| crate::bridge::path_to_uri(path).unwrap());
            let (translator, mut lsp) = translator_with_capabilities(
                &dir, &ServerId::from("rust"), WriteTool::Rename.capabilities(),
            );
            let registry = Arc::new(crate::lsp::WatchRegistry::new());
            registry.register(&ServerId::from("rust"), "sources", &json!([{"globPattern": "**/*.rs"}]));
            let translator = Arc::new(translator.with_watch_registry(registry).with_applier(Arc::new(
                Applier::new(vec![dir.path().to_path_buf()], WriteTool::Rename.apply_config()),
            )));
            let (delivery, floors) = default_delivery_and_floors();
            let server = McplsServer::new(
                Arc::clone(&translator), Arc::new(Mutex::new(NotificationCache::new())),
                Arc::from(vec![dir.path().to_path_buf()]), Arc::new(ResourceSubscriptions::new()),
                false, delivery, floors, DiagnosticsConfig::default(), test_settle(),
            );
            let (server_io, client_io) = tokio::io::duplex(65_536);
            let started = tokio::spawn(async move { server.serve(server_io).await.unwrap() });
            let mut wire = BufStream::new(client_io);
            let initialized = mcp_test_request(&mut wire, json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                    "clientInfo": {"name": "resync-test", "version": "1"}}
            })).await;
            assert!(initialized["result"].is_object(), "{initialized}");
            wire.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n").await.unwrap();
            wire.flush().await.unwrap();
            let running = started.await.unwrap();
            let call = mcp_test_request(&mut wire, json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "rename_symbol", "arguments": {
                    "file_path": paths[0], "line": 1, "character": 4,
                    "new_name": "new", "apply": true}}
            }));
            let respond = async {
                let mut lsp_wire = BufReader::new(&mut lsp.write_stdout);
                let opened = read_framed_message(&mut lsp_wire).await;
                assert_eq!(opened["method"], "textDocument/didOpen");
                assert_eq!(opened["params"]["textDocument"]["uri"], uris[0].as_str());
                let rename = read_framed_message(&mut lsp_wire).await;
                assert_eq!(rename["method"], "textDocument/rename");
                assert!(translator.document_tracker().get(&paths[1]).is_none(), "target must be unopened at rename");
                let changes: Vec<_> = uris.iter().map(|uri| json!({
                    "textDocument": {"uri": uri, "version": null},
                    "edits": [{"range": {"start": {"line": 0, "character": 3},
                        "end": {"line": 0, "character": 6}}, "newText": "new"}]
                })).collect();
                write_response(&mut lsp.read_half_stdin, &rename["id"], json!({"documentChanges": changes})).await;
                let mut contents = std::collections::HashMap::new();
                let mut saves = std::collections::HashSet::new();
                let mut watched = std::collections::HashSet::new();
                while saves.len() < 2 || watched.len() < 2 {
                    let message = read_framed_message(&mut lsp_wire).await;
                    let params = &message["params"];
                    let uri = params["textDocument"]["uri"].as_str().unwrap_or_default();
                    match message["method"].as_str().unwrap() {
                        "textDocument/didOpen" => { contents.insert(uri.to_owned(), params["textDocument"]["text"].clone()); }
                        "textDocument/didChange" => { contents.insert(uri.to_owned(), params["contentChanges"][0]["text"].clone()); }
                        "textDocument/didSave" => {
                            for target in &uris {
                                assert_eq!(contents.get(target.as_str()), Some(&json!("fn new() {}\n")), "all affected content must arrive before the first save: {message}");
                            }
                            saves.insert(uri.to_owned());
                        }
                        "workspace/didChangeWatchedFiles" => {
                            for change in params["changes"].as_array().unwrap() {
                                watched.insert(change["uri"].as_str().unwrap().to_owned());
                            }
                        }
                        other => panic!("unexpected notification: {other}"),
                    }
                }
            };
            let (response, ()) = tokio::join!(call, respond);
            assert!(response["error"].is_null(), "{response}");
            assert_eq!(response["result"]["isError"], false, "{response}");
            for path in &paths {
                assert_eq!(std::fs::read_to_string(path).unwrap(), "fn new() {}\n");
                assert!(translator.document_tracker().get(path).is_some());
            }
            running.cancel().await.unwrap();
        }).await.expect("bounded MCP rename and LSP notification exchange");
    }

    pub(super) async fn mcp_test_request(
        wire: &mut tokio::io::BufStream<tokio::io::DuplexStream>,
        request: serde_json::Value,
    ) -> serde_json::Value {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

        let id = request["id"].clone();
        wire.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        wire.flush().await.unwrap();
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(wire.read_line(&mut line).await.unwrap(), 0);
            let response: serde_json::Value = serde_json::from_str(&line).unwrap();
            if response["id"] == id {
                return response;
            }
        }
    }

    fn diagnostic_wire_record(
        source: Option<&str>,
        code: Option<&str>,
        message: &str,
        line: u32,
    ) -> serde_json::Value {
        let mut record = json!({
            "range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 1}},
            "severity": 1, "message": message,
        });
        if let Some(source) = source {
            record["source"] = json!(source);
        }
        if let Some(code) = code {
            record["code"] = json!(code);
        }
        record
    }

    fn diagnostic_expected_record(
        source: Option<&str>,
        code: Option<&str>,
        message: &str,
        line: u32,
    ) -> serde_json::Value {
        json!({
            "range": {"start": {"line": line, "character": 1}, "end": {"line": line, "character": 2}},
            "severity": "error", "code": code, "source": source, "message": message,
        })
    }

    #[tokio::test]
    async fn test_diagnostics_structured_output_preserves_source() {
        assert_diagnostics_wire_reports(vec![(
            diagnostic_wire_record(Some("rust-analyzer"), Some("E0046"), "not all trait items implemented", 0),
            vec![diagnostic_wire_record(Some("rustc"), Some("E0046"), "missing hello in implementation", 1)],
            json!({"diagnostics": [
                diagnostic_expected_record(Some("rust-analyzer"), Some("E0046"), "not all trait items implemented", 1),
                diagnostic_expected_record(Some("rustc"), Some("E0046"), "missing hello in implementation", 2),
            ]}),
        )]).await;
    }

    #[tokio::test]
    async fn test_diagnostics_wire_exact_equality_and_opaque_sources() {
        for code in [Some("E0046"), None] {
            let pull = diagnostic_wire_record(Some("producer-a"), code, "broken", 0);
            let expected_pull = diagnostic_expected_record(Some("producer-a"), code, "broken", 1);
            assert_diagnostics_wire_reports(vec![
                (pull.clone(), vec![pull.clone()], json!({"diagnostics": [expected_pull.clone()]})),
                (pull, vec![diagnostic_wire_record(Some("producer-b"), code, "broken", 0)],
                 json!({"diagnostics": [expected_pull, diagnostic_expected_record(Some("producer-b"), code, "broken", 1)]})),
            ]).await;
        }
        assert_diagnostics_wire_reports(vec![(
            diagnostic_wire_record(None, None, "broken", 0),
            vec![diagnostic_wire_record(Some("producer-b"), None, "broken", 0)],
            json!({"diagnostics": [diagnostic_expected_record(None, None, "broken", 1), diagnostic_expected_record(Some("producer-b"), None, "broken", 1)]}),
        )]).await;
    }

    #[tokio::test]
    async fn test_diagnostics_wire_nearby_errors_and_cache_replacement() {
        for code in [Some("E0046"), None] {
            let pull = diagnostic_wire_record(Some("producer"), code, "fresh pull", 1);
            let expected_pull = diagnostic_expected_record(Some("producer"), code, "fresh pull", 2);
            assert_diagnostics_wire_reports(vec![
            (pull.clone(), vec![diagnostic_wire_record(Some("producer"), code, "older cached report", 1)],
             json!({"diagnostics": [expected_pull.clone(), diagnostic_expected_record(Some("producer"), code, "older cached report", 2)]})),
            (pull.clone(), vec![diagnostic_wire_record(Some("producer"), code, "fresh pull", 0)],
             json!({"diagnostics": [diagnostic_expected_record(Some("producer"), code, "fresh pull", 1), expected_pull.clone()]})),
            (pull, vec![], json!({"diagnostics": [expected_pull]})),
        ]).await;
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn assert_diagnostics_wire_reports(
        cases: Vec<(serde_json::Value, Vec<serde_json::Value>, serde_json::Value)>,
    ) {
        use rmcp::ServiceExt as _;
        use tokio::io::{AsyncWriteExt as _, BufReader, BufStream};

        tokio::time::timeout(Duration::from_secs(5), async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("main.rs");
            std::fs::write(
                &path,
                "trait T { fn hello(); }\nstruct S;\nimpl T for S {}\n",
            )
            .unwrap();
            let (translator, mut lsp) = translator_with_capabilities(
                &dir,
                &ServerId::from("rust"),
                lsp_types::ServerCapabilities::default(),
            );
            let cache = Arc::new(Mutex::new(NotificationCache::new()));
            let uri = crate::bridge::path_to_uri(&dunce::canonicalize(&path).unwrap()).unwrap();
            let (delivery, floors) = default_delivery_and_floors();
            let server = McplsServer::new(
                Arc::new(translator),
                Arc::clone(&cache),
                Arc::from(vec![dir.path().to_path_buf()]),
                Arc::new(ResourceSubscriptions::new()),
                false,
                delivery,
                floors,
                DiagnosticsConfig::default(),
                test_settle(),
            );
            let (server_io, client_io) = tokio::io::duplex(65_536);
            let started = tokio::spawn(async move { server.serve(server_io).await.unwrap() });
            let mut wire = BufStream::new(client_io);
            let initialized = mcp_test_request(
                &mut wire,
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25", "capabilities": {},
                        "clientInfo": {"name": "structured-output-test", "version": "1"}
                    }
                }),
            )
            .await;
            assert!(initialized["result"].is_object(), "{initialized}");
            wire.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
                .await
                .unwrap();
            wire.flush().await.unwrap();
            let running = started.await.unwrap();

            for (pull, cached, expected) in cases {
                cache.lock().await.store_diagnostics(
                    &ServerId::from("rust"),
                    &uri,
                    Some(1),
                    cached
                        .into_iter()
                        .map(|value| serde_json::from_value(value).unwrap())
                        .collect(),
                );
                let call = mcp_test_request(
                    &mut wire,
                    json!({
                        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                        "params": {"name": "get_diagnostics", "arguments": {"file_path": path}}
                    }),
                );
                let respond = async {
                    let request =
                        read_framed_reply(&mut BufReader::new(&mut lsp.write_stdout)).await;
                    assert_eq!(request["method"], "textDocument/diagnostic");
                    write_response(
                        &mut lsp.read_half_stdin,
                        &request["id"],
                        json!({"kind": "full", "items": [pull]}),
                    )
                    .await;
                };
                let (response, ()) = tokio::join!(call, respond);
                assert!(response["error"].is_null(), "{response}");
                assert_eq!(response["result"]["isError"], false, "{response}");
                let text: serde_json::Value = serde_json::from_str(
                    response["result"]["content"][0]["text"].as_str().unwrap(),
                )
                .unwrap();
                assert_eq!(text, expected);
                assert_eq!(response["result"]["structuredContent"], expected);
            }

            let listed = mcp_test_request(
                &mut wire,
                json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
            )
            .await;
            let diagnostics = listed["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "get_diagnostics")
                .unwrap();
            assert_eq!(
                diagnostics["outputSchema"]["$defs"]["Diagnostic"]["properties"]["source"]["type"],
                json!(["string", "null"])
            );
            running.cancel().await.unwrap();
        })
        .await
        .expect("MCP diagnostic call should finish within five seconds");
    }

    #[tokio::test]
    async fn test_rename_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(RenameParams {
            position: PositionParams {
                file_path: "/test/file.rs".to_string(),
                line: 10,
                character: 5,
            },
            new_name: "new_name".to_string(),
            apply: false,
        });

        let result = server.rename_symbol(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_completions_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(CompletionsParams {
            position: PositionParams {
                file_path: "/test/file.rs".to_string(),
                line: 10,
                character: 5,
            },
            trigger: None,
        });

        let result = server.get_completions(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_document_symbols_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(DocumentSymbolsParams {
            file_path: "/test/file.rs".to_string(),
        });

        let result = server.get_document_symbols(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_format_document_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(FormatDocumentParams {
            file_path: "/test/file.rs".to_string(),
            tab_size: 4,
            insert_spaces: true,
            apply: false,
        });

        let result = server.format_document(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_workspace_symbol_search_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(WorkspaceSymbolParams {
            query: "User".to_string(),
            kind_filter: None,
            limit: 100,
        });
        let result = server.workspace_symbol_search_impl(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_code_actions_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(CodeActionsParams {
            file_path: "/test/file.rs".to_string(),
            range: RangeParams {
                start_line: 10,
                start_character: 5,
                end_line: 10,
                end_character: 15,
            },
            kind_filter: None,
        });
        let result = server.get_code_actions(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_prepare_call_hierarchy_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(PositionParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
        });
        let result = server.prepare_call_hierarchy(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_incoming_calls_tool_with_params() {
        let server = create_test_server();
        let item = serde_json::json!({
            "name": "test_function",
            "kind": 12,
            "uri": "file:///test/file.rs",
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            },
            "selectionRange": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            }
        });
        let params = Parameters(CallHierarchyCallsParams { item });
        let result = server.get_incoming_calls(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_outgoing_calls_tool_with_params() {
        let server = create_test_server();
        let item = serde_json::json!({
            "name": "test_function",
            "kind": 12,
            "uri": "file:///test/file.rs",
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            },
            "selectionRange": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            }
        });
        let params = Parameters(CallHierarchyCallsParams { item });
        let result = server.get_outgoing_calls(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_cached_diagnostics_tool_with_params() {
        use std::fs;

        use tempfile::TempDir;

        let server = create_test_server();

        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let params = Parameters(CachedDiagnosticsParams {
            file_path: test_file.to_str().unwrap().to_string(),
        });

        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.get("diagnostics").is_some());
    }

    /// `get_cached_diagnostics` end-to-end: a cache entry stored under the
    /// canonical URI (as `diagnostics_pump` would store it) must be found when
    /// requested via a textually non-canonical path -- proving `cached_diagnostics_uri`
    /// still canonicalizes correctly after the lock-scope split, and that
    /// `diagnostics_from_cache_entry` correctly maps a populated entry through
    /// the actual tool call (not just the unit-level helpers directly).
    #[tokio::test]
    async fn test_cached_diagnostics_tool_finds_entry_via_noncanonical_path() {
        use std::fs;

        use tempfile::TempDir;
        use url::Url;

        let server = create_test_server();

        let temp_dir = TempDir::new().unwrap();
        let subdir = temp_dir.path().join("sub");
        fs::create_dir(&subdir).unwrap();
        let test_file = subdir.join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: None,
            message: "cached error".to_string(),
            related_information: None,
            tags: None,
            data: None,
        };
        {
            let mut cache = server.context.notification_cache.lock().await;
            cache.store_diagnostics(
                &crate::config::ServerId::from("rust"),
                &uri,
                Some(1),
                vec![diagnostic],
            );
        }

        // Textually distinct from `test_file`, but canonicalizes to the same path.
        let noncanonical = subdir.join("..").join("sub").join("test.rs");
        let params = Parameters(CachedDiagnosticsParams {
            file_path: noncanonical.to_str().unwrap().to_string(),
        });

        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let diagnostics = parsed.get("diagnostics").unwrap().as_array().unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].get("message").unwrap(), "cached error");
    }

    /// #290 gap: a cache-only read must resolve the *owner* server's
    /// negotiated encoding, not silently assume UTF-16. Registers the
    /// publishing server as UTF-8 and stores a diagnostic over a real
    /// multibyte line ("héllo") so a UTF-16 assumption would produce a
    /// visibly different (wrong) column: LSP byte offset 3 is MCP column 3
    /// under the registered server's UTF-8 encoding, but would read as raw
    /// column 4 (unconverted passthrough) under the UTF-16 default tested in
    /// `test_cached_diagnostics_tool_no_owner_falls_back_to_utf16` below.
    #[tokio::test]
    async fn test_cached_diagnostics_tool_uses_registered_owner_encoding() {
        use std::fs;

        use tempfile::TempDir;
        use url::Url;

        let server = create_test_server();
        let owner = crate::config::ServerId::from("rust");
        server.context.translator.register_server(
            owner.clone(),
            crate::lsp::LspServer::new_for_test_with_encoding(
                lsp_types::ServerCapabilities::default(),
                lsp_types::PositionEncodingKind::UTF8,
            ),
        );

        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "héllo").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 3,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: None,
            message: "multibyte range".to_string(),
            related_information: None,
            tags: None,
            data: None,
        };
        {
            let mut cache = server.context.notification_cache.lock().await;
            cache.store_diagnostics(&owner, &uri, Some(1), vec![diagnostic]);
        }

        let params = Parameters(CachedDiagnosticsParams {
            file_path: test_file.to_str().unwrap().to_string(),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let diagnostics = parsed.get("diagnostics").unwrap().as_array().unwrap();
        assert_eq!(
            diagnostics[0]["range"]["end"]["character"], 3,
            "byte offset 3 on \"héllo\" is UTF-16 column 3 when converted against the \
             registered UTF-8 owner"
        );
    }

    /// Companion to the test above: when no server is registered under the
    /// cached entry's owner id (or no owner is tracked at all),
    /// `get_cached_diagnostics` must fall back to UTF-16 -- a raw,
    /// unconverted passthrough -- rather than panicking or guessing.
    #[tokio::test]
    async fn test_cached_diagnostics_tool_no_owner_falls_back_to_utf16() {
        use std::fs;

        use tempfile::TempDir;
        use url::Url;

        let server = create_test_server();
        // Deliberately not registered with `translator.register_server`.
        let owner = crate::config::ServerId::from("rust");

        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "héllo").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 3,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: None,
            message: "multibyte range".to_string(),
            related_information: None,
            tags: None,
            data: None,
        };
        {
            let mut cache = server.context.notification_cache.lock().await;
            cache.store_diagnostics(&owner, &uri, Some(1), vec![diagnostic]);
        }

        let params = Parameters(CachedDiagnosticsParams {
            file_path: test_file.to_str().unwrap().to_string(),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let diagnostics = parsed.get("diagnostics").unwrap().as_array().unwrap();
        assert_eq!(
            diagnostics[0]["range"]["end"]["character"], 4,
            "with no registered owner, must fall back to UTF-16 (raw passthrough: \
             character + 1), not the UTF-8-correct column"
        );
    }

    /// Build a server for `get_new_diagnostics` tests with direct access to
    /// the cache and delivery `Arc`s it shares, so a test can seed the cache
    /// and the baseline independently of the tool calls under test.
    struct NewDiagnosticsTestServer {
        server: McplsServer,
        notification_cache: Arc<Mutex<NotificationCache>>,
        delivery: Arc<Mutex<DiagnosticsDelivery>>,
        settle: Arc<ServerSettle>,
    }

    fn new_diagnostics_test_server() -> NewDiagnosticsTestServer {
        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let (delivery, floors) = default_delivery_and_floors();
        let settle = test_settle();
        let server = McplsServer::new(
            translator,
            Arc::clone(&notification_cache),
            workspace_roots,
            subscriptions,
            false,
            Arc::clone(&delivery),
            floors,
            DiagnosticsConfig::default(),
            Arc::clone(&settle),
        );
        NewDiagnosticsTestServer {
            server,
            notification_cache,
            delivery,
            settle,
        }
    }

    fn diagnostic_at(message: &str) -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: None,
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    /// A file whose URI does not resolve to a filesystem path (e.g. a
    /// virtual document under some other scheme) must never reach `flush`
    /// at all -- reaching it and being dropped from the payload afterward
    /// would still record it delivered, permanently hiding diagnostics
    /// that were in fact never shown.
    #[test]
    fn test_routable_entries_borrowed_excludes_unmappable_uri() {
        // `Url::to_file_path` needs a drive letter on Windows, so a bare
        // `file:///workspace/...` maps to no path there and would be
        // excluded for the wrong reason.
        #[cfg(windows)]
        let ok_uri: lsp_types::Uri = "file:///C:/workspace/a.rs".parse().unwrap();
        #[cfg(not(windows))]
        let ok_uri: lsp_types::Uri = "file:///workspace/a.rs".parse().unwrap();
        let bad_uri: lsp_types::Uri = "http://example.com/not-a-file.rs".parse().unwrap();
        let owner = crate::config::ServerId::from("rust");

        let mut cache = NotificationCache::new();
        cache.store_diagnostics(&owner, &ok_uri, Some(1), vec![diagnostic_at("ok")]);
        cache.store_diagnostics(
            &owner,
            &bad_uri,
            Some(1),
            vec![diagnostic_at("unreachable")],
        );
        let floors = FloorTable::new(&crate::config::DiagnosticsConfig::default(), &[]);

        let entries = routable_entries_borrowed(&cache, &floors, &HashSet::new());

        assert_eq!(
            entries.len(),
            1,
            "the unmappable-URI file must be excluded before flush ever sees it"
        );
        assert_eq!(entries[0].diagnostics[0].message, "ok");
    }

    /// A non-zero `omitted` count is meaningless to an agent unless the
    /// payload itself says a later call offers those files again -- a doc
    /// comment and a line in the user guide are invisible from inside a
    /// tool response.
    #[tokio::test]
    async fn test_new_diagnostics_payload_notes_a_nonzero_omitted_count() {
        let server = create_test_server();
        let report = FlushReport {
            omitted: 2,
            ..Default::default()
        };

        let payload = server
            .new_diagnostics_payload(&report, &HashMap::new())
            .await;

        assert_eq!(payload.omitted, 2);
        let note = payload
            .note
            .as_deref()
            .unwrap_or_else(|| panic!("expected a note explaining the omitted count"));
        assert!(
            note.contains('2'),
            "note should mention how many files were omitted, got {note:?}"
        );
    }

    /// The other side of the same behavior: no note when nothing was
    /// omitted, so a caller isn't left looking for files that don't exist.
    #[tokio::test]
    async fn test_new_diagnostics_payload_no_note_when_nothing_omitted() {
        let server = create_test_server();
        let report = FlushReport::default();

        let payload = server
            .new_diagnostics_payload(&report, &HashMap::new())
            .await;

        assert_eq!(payload.omitted, 0);
        assert!(payload.note.is_none());
    }

    /// Before a baseline exists, `get_new_diagnostics` must not call
    /// `DiagnosticsDelivery::flush` at all -- calling it and discarding the
    /// result still seeds the session's record as empty, permanently, and
    /// every diagnostic the workspace already had would then read as newly
    /// changed forever after (see `DiagnosticsDelivery::flush`'s doc
    /// comment on why the record is seeded from the baseline).
    ///
    /// Pinned end-to-end: call once with no baseline (must not touch the
    /// delivery record), adopt a baseline that already accounts for a
    /// pre-existing error, then call again with both that pre-existing
    /// error and a genuinely new one in the cache. Only the new one may be
    /// reported -- if the first call had seeded the session's record empty,
    /// the pre-existing error would incorrectly show up too.
    #[tokio::test]
    async fn test_new_diagnostics_does_not_seed_session_before_baseline_exists() {
        let NewDiagnosticsTestServer {
            server,
            notification_cache,
            delivery,
            ..
        } = new_diagnostics_test_server();
        let owner = crate::config::ServerId::from("rust");

        // No baseline yet: the tool must report "starting up", not a
        // (falsely empty) drain.
        let early = server.get_new_diagnostics().await.unwrap();
        let early: serde_json::Value = serde_json::from_str(&early).unwrap();
        assert!(
            early["note"].is_string(),
            "expected a starting-up note before any baseline exists, got {early}"
        );
        assert_eq!(early["changed"].as_array().unwrap().len(), 0);
        assert_eq!(early["cleared"].as_array().unwrap().len(), 0);

        // A pre-existing error, as if a language server had reported it
        // before mcpls ever started -- the baseline task would have folded
        // this into the baseline once the servers went quiet.
        // `Url::to_file_path` needs a drive letter on Windows, so a bare
        // `file:///workspace/...` maps to no path there and would be
        // dropped by `routable_entries_borrowed` before any of this is
        // exercised.
        #[cfg(windows)]
        let pre_existing_uri: lsp_types::Uri =
            "file:///C:/workspace/pre_existing.rs".parse().unwrap();
        #[cfg(not(windows))]
        let pre_existing_uri: lsp_types::Uri = "file:///workspace/pre_existing.rs".parse().unwrap();
        let pre_existing = diagnostic_at("pre-existing error");
        {
            let mut cache = notification_cache.lock().await;
            cache.store_diagnostics(
                &owner,
                &pre_existing_uri,
                Some(1),
                vec![pre_existing.clone()],
            );
        }
        let baseline_key = {
            let cache = notification_cache.lock().await;
            cache
                .diagnostics_entries()
                .into_iter()
                .find(|(_, info, _)| info.uri == pre_existing_uri)
                .map(|(key, _, _)| key.to_string())
                .unwrap()
        };
        let baseline_hash = DiagnosticsDelivery::visible_hash(
            &[pre_existing],
            crate::config::SeverityFloor::Warning,
        )
        .unwrap();
        {
            let mut delivery = delivery.lock().await;
            let mut baseline = std::collections::HashMap::new();
            baseline.insert(baseline_key, baseline_hash);
            delivery.set_baseline(baseline);
        }

        // A genuinely new error, arriving after the baseline was captured.
        #[cfg(windows)]
        let (new_uri, new_file_path): (lsp_types::Uri, &str) = (
            "file:///C:/workspace/new_error.rs".parse().unwrap(),
            r"C:\workspace\new_error.rs",
        );
        #[cfg(not(windows))]
        let (new_uri, new_file_path): (lsp_types::Uri, &str) = (
            "file:///workspace/new_error.rs".parse().unwrap(),
            "/workspace/new_error.rs",
        );
        {
            let mut cache = notification_cache.lock().await;
            cache.store_diagnostics(&owner, &new_uri, Some(1), vec![diagnostic_at("new error")]);
        }

        let report = server.get_new_diagnostics().await.unwrap();
        let report: serde_json::Value = serde_json::from_str(&report).unwrap();
        assert!(
            report["note"].is_null(),
            "a real report must not carry the starting-up note, got {report}"
        );
        let changed = report["changed"].as_array().unwrap();
        assert_eq!(
            changed.len(),
            1,
            "the pre-existing error must be suppressed by the baseline, only the new one \
             reported, got {report}"
        );
        assert_eq!(changed[0]["file_path"], new_file_path);
        assert_eq!(changed[0]["diagnostics"][0]["message"], "new error");
    }

    #[tokio::test]
    async fn test_new_owner_startup_diagnostics_wait_for_baseline_merge() {
        let NewDiagnosticsTestServer {
            server,
            notification_cache,
            delivery,
            settle,
        } = new_diagnostics_test_server();
        let owner = ServerId::from("rust");
        #[cfg(windows)]
        let uri: lsp_types::Uri = "file:///C:/workspace/startup.rs".parse().unwrap();
        #[cfg(not(windows))]
        let uri: lsp_types::Uri = "file:///workspace/startup.rs".parse().unwrap();

        delivery.lock().await.set_baseline(HashMap::new());
        let mut cache = notification_cache.lock().await;
        let flush = server.get_new_diagnostics();
        tokio::pin!(flush);
        assert!(
            futures::poll!(&mut flush).is_pending(),
            "the diagnostics call must reach the held cache lock"
        );

        settle.register_diagnostics_owner(&owner);
        let startup_diagnostics = vec![diagnostic_at("startup diagnostic")];
        cache.store_diagnostics(&owner, &uri, Some(1), startup_diagnostics.clone());
        let key = cache.diagnostics_entries()[0].0.to_owned();
        drop(cache);

        let report: serde_json::Value = serde_json::from_str(&flush.await.unwrap()).unwrap();

        assert_eq!(
            report["changed"].as_array().unwrap().len(),
            0,
            "diagnostics published during a new owner's startup are baseline state"
        );
        assert!(
            report["note"]
                .as_str()
                .is_some_and(|note| note.contains("still settling")),
            "withheld startup diagnostics must not look like a clean workspace"
        );

        let generation = settle.diagnostics_baseline_generation(&owner).unwrap();
        let hash = DiagnosticsDelivery::visible_hash(
            &startup_diagnostics,
            crate::config::SeverityFloor::Warning,
        )
        .unwrap();
        delivery
            .lock()
            .await
            .merge_baseline(HashMap::from([(key, hash)]));
        assert!(settle.finish_diagnostics_baseline_merge(&owner, generation));

        let report: serde_json::Value =
            serde_json::from_str(&server.get_new_diagnostics().await.unwrap()).unwrap();
        assert_eq!(
            report["changed"].as_array().unwrap().len(),
            0,
            "settled startup diagnostics stay out of the session's changes"
        );
    }

    #[tokio::test]
    async fn test_cached_diagnostics_tool_nonexistent_file() {
        let server = create_test_server();
        let params = Parameters(CachedDiagnosticsParams {
            file_path: "/nonexistent/file.rs".to_string(),
        });

        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_default_params() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 50,
            min_level: None,
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.get("logs").is_some());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_error_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 10,
            min_level: Some("error".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let logs = parsed.get("logs").unwrap().as_array().unwrap();
        assert_eq!(logs.len(), 0);
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_warning_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 100,
            min_level: Some("warning".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_info_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 50,
            min_level: Some("info".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_debug_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 20,
            min_level: Some("debug".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_invalid_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 10,
            min_level: Some("invalid_level".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_zero_limit() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 0,
            min_level: None,
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let logs = parsed.get("logs").unwrap().as_array().unwrap();
        assert_eq!(logs.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_default_params() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 20 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.get("messages").is_some());
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_custom_limit() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 5 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let messages = parsed.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_zero_limit() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 0 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let messages = parsed.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_large_limit() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 1000 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_signature_help_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(PositionParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
        });

        let result = server.get_signature_help(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_go_to_implementation_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(PositionParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
        });

        let result = server.go_to_implementation(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_go_to_type_definition_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(PositionParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
        });

        let result = server.go_to_type_definition(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_inlay_hints_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(InlayHintsParams {
            file_path: "/test/file.rs".to_string(),
            range: RangeParams {
                start_line: 1,
                start_character: 1,
                end_line: 10,
                end_character: 1,
            },
        });

        let result = server.get_inlay_hints(params).await;
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // Tool annotation tests
    // ------------------------------------------------------------------

    /// Every registered tool must carry `ToolAnnotations` (plus the current-spec
    /// `Tool.title`) so MCP clients can decide when to skip confirmation dialogs
    /// (read-only tools) or must prompt the user (destructive tools) without
    /// invoking the tool first. Sourced from `tool_router().list_all()` (not a
    /// hand-written list of tool names). This test alone does not catch a
    /// future *mutating* tool that omits `annotations(...)`: `tool_router()`'s
    /// central pass (see its doc comment) blanket-labels any such tool
    /// read-only rather than leaving it `None`, so the hint assertions above
    /// always pass. `test_tool_annotation_classifications_match_intent` below
    /// forces a new mutating tool to write down an explicit classification,
    /// though it does not verify that classification is truthful.
    #[test]
    fn test_all_tools_carry_annotations() {
        let tools = McplsServer::tool_router().list_all();
        assert!(!tools.is_empty(), "no tools registered");

        for tool in &tools {
            assert!(
                tool.title.is_some(),
                "tool `{}` is missing a top-level title",
                tool.name
            );
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("tool `{}` is missing annotations", tool.name));
            assert!(
                annotations.title.is_some(),
                "tool `{}` is missing an annotations title",
                tool.name
            );
            assert!(
                annotations.read_only_hint.is_some(),
                "tool `{}` is missing read_only_hint",
                tool.name
            );
            assert!(
                annotations.destructive_hint.is_some(),
                "tool `{}` is missing destructive_hint",
                tool.name
            );
            assert!(
                annotations.idempotent_hint.is_some(),
                "tool `{}` is missing idempotent_hint",
                tool.name
            );
        }
    }

    /// Value-level regression guard for every tool's `(read_only, destructive,
    /// idempotent)` classification, sourced from the live `tool_router` (not
    /// per-tool `*_tool_attr()` calls) so the expected-tool table itself is
    /// checked against the actual registered count.
    #[test]
    fn test_tool_annotation_classifications_match_intent() {
        let tools = McplsServer::tool_router().list_all();
        let by_name: std::collections::HashMap<&str, &rmcp::model::ToolAnnotations> = tools
            .iter()
            .map(|tool| {
                (
                    tool.name.as_ref(),
                    tool.annotations
                        .as_ref()
                        .unwrap_or_else(|| panic!("tool `{}` is missing annotations", tool.name)),
                )
            })
            .collect();

        // (tool name, read_only_hint, destructive_hint, idempotent_hint)
        let expected: &[(&str, bool, bool, bool)] = &[
            ("get_hover", true, false, true),
            ("get_definition", true, false, true),
            ("get_references", true, false, true),
            ("get_diagnostics", true, false, true),
            ("rename_symbol", false, true, false),
            ("get_completions", true, false, true),
            ("get_document_symbols", true, false, true),
            ("format_document", false, true, true),
            ("workspace_symbol_search", true, false, true),
            ("get_code_actions", true, false, true),
            ("apply_code_action", false, true, false),
            ("prepare_call_hierarchy", true, false, true),
            ("get_incoming_calls", true, false, true),
            ("get_outgoing_calls", true, false, true),
            ("get_cached_diagnostics", true, false, true),
            ("get_server_logs", true, false, true),
            ("get_server_messages", true, false, true),
            ("get_signature_help", true, false, true),
            ("go_to_implementation", true, false, true),
            ("go_to_type_definition", true, false, true),
            ("get_inlay_hints", true, false, true),
            ("get_new_diagnostics", false, false, false),
        ];

        assert_eq!(
            expected.len(),
            tools.len(),
            "expected-classification table is out of sync with the registered tool count"
        );

        for (name, read_only, destructive, idempotent) in expected {
            let annotations = by_name
                .get(name)
                .unwrap_or_else(|| panic!("tool `{name}` not found in tool_router"));
            assert_eq!(
                annotations.read_only_hint,
                Some(*read_only),
                "tool `{name}` read_only_hint mismatch"
            );
            assert_eq!(
                annotations.destructive_hint,
                Some(*destructive),
                "tool `{name}` destructive_hint mismatch"
            );
            assert_eq!(
                annotations.idempotent_hint,
                Some(*idempotent),
                "tool `{name}` idempotent_hint mismatch"
            );
        }
    }

    fn server_permitting_writes() -> McplsServer {
        use crate::bridge::apply::Applier;
        use crate::config::ApplyConfig;

        let applier = Arc::new(Applier::new(
            Vec::new(),
            ApplyConfig {
                rename: true,
                format_document: true,
                code_actions: true,
                allow_file_deletion: false,
            },
        ));
        let (delivery, floors) = default_delivery_and_floors();
        McplsServer::new(
            Arc::new(Translator::new().with_applier(applier)),
            Arc::new(Mutex::new(NotificationCache::new())),
            Arc::from(Vec::new()),
            Arc::new(ResourceSubscriptions::new()),
            false,
            delivery,
            floors,
            DiagnosticsConfig::default(),
            test_settle(),
        )
    }

    /// The writing tools describe themselves by what they can actually do
    /// here: with every apply key off they only ever return edits, and a
    /// client that gates destructive tools should not be gating them.
    #[test]
    fn test_writing_tools_are_read_only_when_the_config_forbids_writing() {
        let server = create_test_server();
        for name in ["rename_symbol", "format_document", "apply_code_action"] {
            let annotations = server
                .get_tool(name)
                .unwrap_or_else(|| panic!("tool `{name}` is registered"))
                .annotations
                .unwrap_or_else(|| panic!("tool `{name}` carries annotations"));
            assert_eq!(annotations.read_only_hint, Some(true), "{name}");
            assert_eq!(annotations.destructive_hint, Some(false), "{name}");
        }
    }

    #[test]
    fn test_writing_tools_keep_their_warning_when_the_config_permits_writing() {
        let server = server_permitting_writes();
        for name in ["rename_symbol", "format_document", "apply_code_action"] {
            let annotations = server
                .get_tool(name)
                .unwrap_or_else(|| panic!("tool `{name}` is registered"))
                .annotations
                .unwrap_or_else(|| panic!("tool `{name}` carries annotations"));
            assert_eq!(annotations.read_only_hint, Some(false), "{name}");
            assert_eq!(annotations.destructive_hint, Some(true), "{name}");
        }
    }

    /// A reading tool's classification is the same either way.
    #[test]
    fn test_reading_tools_are_untouched_by_the_apply_config() {
        for server in [create_test_server(), server_permitting_writes()] {
            let annotations = server
                .get_tool("get_hover")
                .unwrap_or_else(|| panic!("get_hover is registered"))
                .annotations
                .unwrap_or_else(|| panic!("get_hover carries annotations"));
            assert_eq!(annotations.read_only_hint, Some(true));
            assert_eq!(annotations.destructive_hint, Some(false));
        }
    }

    // ------------------------------------------------------------------
    // Resource handler tests (logic-level, avoiding rmcp::service::RequestContext
    // which requires a live Peer with private fields)
    // ------------------------------------------------------------------

    /// `list_resources` returns an empty vec for a fresh translator with no open documents.
    #[tokio::test]
    async fn test_list_resources_returns_empty_when_no_open_documents() {
        let server = create_test_server();
        let empty = server.context.translator.open_document_paths().is_empty();
        assert!(empty);
    }

    // ------------------------------------------------------------------
    // `paginate_resource_paths` (pagination logic behind `list_resources`)
    // ------------------------------------------------------------------

    fn paths(n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| PathBuf::from(format!("/f{i:04}.rs")))
            .collect()
    }

    #[test]
    fn test_paginate_first_page_under_page_size_has_no_next_cursor() {
        let p = paths(5);
        let (page, next_cursor) = paginate_resource_paths(&p, None, 100).unwrap();
        assert_eq!(page.len(), 5);
        assert!(next_cursor.is_none());
    }

    #[test]
    fn test_paginate_splits_across_pages_when_over_page_size() {
        let p = paths(250);

        let (page1, cursor1) = paginate_resource_paths(&p, None, 100).unwrap();
        assert_eq!(page1.len(), 100);
        assert_eq!(page1.first(), p.first());
        assert_eq!(cursor1.as_deref(), Some("100"));

        let (page2, cursor2) = paginate_resource_paths(&p, cursor1.as_deref(), 100).unwrap();
        assert_eq!(page2.len(), 100);
        assert_eq!(page2.first(), Some(&p[100]));
        assert_eq!(cursor2.as_deref(), Some("200"));

        let (page3, cursor3) = paginate_resource_paths(&p, cursor2.as_deref(), 100).unwrap();
        assert_eq!(page3.len(), 50);
        assert_eq!(page3.first(), Some(&p[200]));
        assert!(cursor3.is_none());
    }

    #[test]
    fn test_paginate_rejects_malformed_cursor() {
        let p = paths(5);
        let result = paginate_resource_paths(&p, Some("not-a-number"), 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_paginate_out_of_range_cursor_yields_empty_page_not_error() {
        let p = paths(5);
        let (page, next_cursor) = paginate_resource_paths(&p, Some("9999"), 100).unwrap();
        assert!(page.is_empty());
        assert!(next_cursor.is_none());
    }

    /// Regression for a client-controlled cursor near `usize::MAX`: `start + page_size`
    /// must not panic (debug) or silently wrap to a bogus cursor (release).
    #[test]
    fn test_paginate_cursor_near_usize_max_does_not_overflow() {
        let p = paths(5);
        let cursor = usize::MAX.to_string();
        let (page, next_cursor) = paginate_resource_paths(&p, Some(&cursor), 100).unwrap();
        assert!(page.is_empty());
        assert!(next_cursor.is_none());
    }

    /// `list_resources` overrides `next_cursor` via struct-update syntax on top of
    /// `ListResourcesResult::with_all_items` (which always sets it to `None`) --
    /// confirm the explicit field wins and survives serialization under its
    /// wire name (`nextCursor`, camelCase per `rmcp`'s `paginated_result!`).
    #[test]
    fn test_list_resources_result_next_cursor_survives_struct_update_override() {
        let result = ListResourcesResult {
            next_cursor: Some("100".to_string()),
            ..ListResourcesResult::with_all_items(Vec::new())
        };
        assert_eq!(result.next_cursor.as_deref(), Some("100"));

        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json.get("nextCursor").unwrap(), "100");
    }

    // ------------------------------------------------------------------
    // `ResourceDiagnosticsResponse` (tracked-vs-untracked shape behind
    // `read_resource`)
    // ------------------------------------------------------------------

    fn sample_diagnostic_info(diagnostics: Vec<lsp_types::Diagnostic>) -> DiagnosticInfo {
        use url::Url;

        let uri: lsp_types::Uri = Url::parse("file:///sample.rs")
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        DiagnosticInfo {
            uri,
            version: Some(1),
            diagnostics,
        }
    }

    #[test]
    fn test_resource_diagnostics_response_untracked_is_not_tracked_and_empty() {
        let response = ResourceDiagnosticsResponse::new(false, None);
        assert!(!response.tracked);
        assert!(response.version.is_none());
        assert!(response.diagnostics.is_empty());

        // #132's contract is the wire shape, not the Rust struct -- assert the JSON directly.
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["tracked"], false);
        assert!(json["version"].is_null());
        assert_eq!(json["diagnostics"], serde_json::json!([]));
    }

    #[test]
    fn test_resource_diagnostics_response_tracked_but_no_cache_entry_is_clean() {
        let response = ResourceDiagnosticsResponse::new(true, None);
        assert!(response.tracked);
        assert!(response.version.is_none());
        assert!(response.diagnostics.is_empty());

        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["tracked"], true);
        assert!(json["version"].is_null());
        assert_eq!(json["diagnostics"], serde_json::json!([]));
    }

    #[test]
    fn test_resource_diagnostics_response_tracked_with_diagnostics() {
        let entry = sample_diagnostic_info(vec![lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: None,
            message: "boom".to_string(),
            related_information: None,
            tags: None,
            data: None,
        }]);
        let response = ResourceDiagnosticsResponse::new(true, Some(&entry));
        assert!(response.tracked);
        assert_eq!(response.version, Some(1));
        assert_eq!(response.diagnostics.len(), 1);
        assert_eq!(response.diagnostics[0].message, "boom");

        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["tracked"], true);
        assert_eq!(json["version"], 1);
        assert_eq!(json["diagnostics"][0]["message"], "boom");
    }

    /// A path `read_resource` never opened reports `is_document_open() == false`
    /// -- one of the two inputs `build_resource_diagnostics_response` ORs together.
    #[tokio::test]
    async fn test_read_resource_untracked_path_is_not_open() {
        let server = create_test_server();
        let tracked = server
            .context
            .translator
            .is_document_open(std::path::Path::new("/never/opened.rs"));
        assert!(!tracked);
    }

    #[test]
    fn test_build_resource_diagnostics_response_neither_open_nor_cached_is_untracked() {
        let response = build_resource_diagnostics_response(false, None);
        assert!(!response.tracked);
        assert!(response.diagnostics.is_empty());
    }

    #[test]
    fn test_build_resource_diagnostics_response_open_but_uncached_is_tracked() {
        let response = build_resource_diagnostics_response(true, None);
        assert!(response.tracked);
        assert!(response.diagnostics.is_empty());
    }

    /// Regression: an LSP server can publish diagnostics for a file mcpls never
    /// explicitly opened via `DocumentTracker` (e.g. one rust-analyzer analyzes
    /// transitively). `tracked` must still be `true` here -- deriving it from
    /// `document_open` alone would report `tracked: false` while `diagnostics`
    /// is non-empty, contradicting the documented "untracked implies empty
    /// diagnostics" contract.
    #[test]
    fn test_build_resource_diagnostics_response_cached_but_unopened_is_tracked() {
        let entry = sample_diagnostic_info(vec![lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::WARNING),
            code: None,
            code_description: None,
            source: None,
            message: "transitively analyzed".to_string(),
            related_information: None,
            tags: None,
            data: None,
        }]);

        let response = build_resource_diagnostics_response(false, Some(&entry));
        assert!(
            response.tracked,
            "a cached diagnostics entry must make the response tracked, \
             even for a file that was never explicitly opened"
        );
        assert_eq!(response.diagnostics.len(), 1);
    }

    /// `parse_uri` rejects `file://` scheme — ensures `read_resource` would return an error.
    #[test]
    fn test_read_resource_rejects_file_scheme() {
        let result = parse_uri("file:///some/file.rs");
        assert!(result.is_err());
    }

    /// `parse_uri` rejects `https://` scheme.
    #[test]
    fn test_subscribe_rejects_https_scheme() {
        let result = parse_uri("https://evil.com/file.rs");
        assert!(result.is_err());
    }

    /// Regression test for `read_resource`'s canonical-path fix: a path reached
    /// through a symlink must resolve, via `validate_path_against_roots`, to the
    /// same URI as its canonical (symlink-resolved) form -- matching what
    /// `diagnostics_pump` stores from LSP notifications. Building `lsp_uri` from
    /// the raw (symlinked) path (the pre-fix behavior) would produce a
    /// mismatched cache key and always miss.
    ///
    /// Uses a real symlink rather than `..` segments: `path_to_uri` re-parses
    /// the URI string through `url::Url::parse` (for RFC 3986 char encoding),
    /// which normalizes away `..` segments regardless of platform -- so a path
    /// differing only by `..` produces the same URI as its canonical form with
    /// or without the fix. Only an actual symlink resolution (which happens in
    /// `canonicalize()`, not in URI string normalization) creates a real
    /// raw-vs-canonical difference. Unix-only: creating symlinks on Windows CI
    /// runners typically requires elevated privileges / Developer Mode.
    #[test]
    #[cfg(unix)]
    fn test_read_resource_canonical_path_matches_pump_cache_key() {
        use std::fs;
        use std::os::unix::fs::symlink;

        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        // Canonicalize the base up front so any symlink-iness already present
        // in the OS temp directory itself (e.g. macOS's `/tmp` -> `/private/tmp`)
        // doesn't leak into the comparison -- the only symlink under test is
        // `link_dir`.
        let base = temp_dir.path().canonicalize().unwrap();
        let real_dir = base.join("real");
        fs::create_dir(&real_dir).unwrap();
        let test_file = real_dir.join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let link_dir = base.join("link");
        symlink(&real_dir, &link_dir).unwrap();
        let noncanonical = link_dir.join("test.rs");
        assert_ne!(noncanonical, test_file);

        let validated = validate_path_against_roots(&noncanonical, &[]).unwrap();
        assert_eq!(validated, test_file.canonicalize().unwrap());

        let uri_from_raw_path = crate::bridge::path_to_uri(&noncanonical).unwrap();
        let uri_from_validated_path = crate::bridge::path_to_uri(&validated).unwrap();
        assert_ne!(
            uri_from_raw_path, uri_from_validated_path,
            "raw and canonical paths must differ here, otherwise this test can't \
             detect a regression back to keying off the raw path"
        );
    }

    /// `validate_path` rejects a non-existent path (canonicalize fails).
    #[tokio::test]
    async fn test_validate_path_rejects_nonexistent_path() {
        use std::path::Path;

        let translator = Translator::new();
        let result = translator.validate_path(Path::new("/this/path/does/not/exist/at/all.rs"));
        assert!(result.is_err());
    }

    /// Server capabilities advertise resources support.
    #[tokio::test]
    async fn test_server_capabilities_include_resources() {
        let server = create_test_server();
        let info = server.get_info();
        assert!(info.capabilities.resources.is_some());
    }

    /// Dump the current tool surface to stdout so it can be captured into
    /// `tool_surface.json`. Not part of the regular suite.
    #[test]
    #[ignore = "run manually to (re)generate tool_surface.json"]
    fn dump_tool_surface() {
        let tools = McplsServer::tool_router().list_all();
        println!("{}", serde_json::to_string_pretty(&tools).unwrap());
    }

    /// Pins the client-visible tool surface (name, description, title,
    /// annotations, input schema) exposed by `tool_router().list_all()`.
    /// `serde_json::Value` comparison, not string comparison, so key
    /// order/whitespace drift doesn't cause false failures -- only an actual
    /// change to what an MCP client sees does.
    #[test]
    fn test_tool_surface_matches_golden_snapshot() {
        let tools = McplsServer::tool_router().list_all();
        let actual = serde_json::to_value(&tools).unwrap();
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("tool_surface.json")).unwrap();
        assert_eq!(
            actual, expected,
            "client-visible tool surface changed -- update tool_surface.json only if the \
             change is intentional"
        );
    }
    #[test]
    fn test_output_schema_present_only_for_structured_tools() {
        const STRUCTURED_TOOLS: &[&str] = &[
            "get_diagnostics",
            "get_definition",
            "get_references",
            "get_document_symbols",
        ];

        let tools = McplsServer::tool_router().list_all();
        assert!(!tools.is_empty(), "no tools registered");
        for tool in &tools {
            let expects_schema = STRUCTURED_TOOLS.contains(&tool.name.as_ref());
            assert_eq!(
                tool.output_schema.is_some(),
                expects_schema,
                "tool `{}`: expected output_schema.is_some() == {expects_schema}",
                tool.name
            );
        }
    }
}
