//! Translation layer between MCP and LSP protocols.
//!
//! This module handles the bidirectional conversion between
//! MCP tool calls and LSP requests/responses.

use std::sync::{Mutex as StdMutex, MutexGuard, PoisonError};

pub mod apply;
mod delivery;
mod encoding;
mod identity;
mod notifications;
pub mod resources;
mod settle;
mod state;
mod translator;

pub(crate) use delivery::DiagnosticSnapshot;
pub use delivery::{
    ChangedFile, ConnectionId, DiagnosticsDelivery, FileEntry, FloorTable, FlushReport, SessionId,
};
pub use encoding::{PositionEncoding, lsp_to_mcp_position, mcp_to_lsp_position};
pub use identity::{Caller, HookAgent, HookHost, RecordId};
pub(crate) use notifications::uri_cache_key;
pub use notifications::{
    DiagnosticInfo, LogEntry, LogLevel, MessageType, NotificationCache, ServerMessage,
};
pub use resources::ResourceSubscriptions;
pub use settle::ServerSettle;
pub(crate) use state::try_path_to_uri;
pub use state::{
    DEFAULT_MAX_DOCUMENTS, DEFAULT_MAX_FILE_SIZE, DocumentState, DocumentTracker, ResourceLimits,
    path_to_uri, uri_to_path,
};
/// The fake servers the translator's own tests drive, and the framing
/// helpers that answer them, re-exported so a test in another module can
/// assert on what actually reached a server -- or drive a whole tool call
/// through one -- rather than building a second fake of its own.
#[cfg(test)]
pub(crate) use translator::testing::{
    FakeServer, TranslatorHarness, read_framed_reply, translator_with_capabilities, write_response,
};
pub use translator::{
    Completion, CompletionsResult, DefinitionResult, Diagnostic, DiagnosticSeverity,
    DiagnosticsResult, DocumentChanges, DocumentSymbolsResult, FormatDocumentResult, HoverResult,
    Location, Position2D, Range, ReferencesResult, RenameResult, ResourceOperation,
    ServerLifecycle, Symbol, TextEdit, Translator,
};
pub(crate) use translator::{OpenOutcome, validate_path_against_roots};

/// Lock a `std::sync::Mutex`, recovering the guard if a previous holder
/// panicked while holding it.
///
/// Every lock guarded this way protects a short, synchronous, panic-free
/// critical section (a `HashMap`/`HashSet` lookup or insert), so poisoning
/// can only happen if an unrelated bug already panicked; refusing to unwind
/// the whole process a second time over stale poisoning is preferable to
/// deadlocking future calls. Shared by `translator` and `state` so both
/// modules lock their interior `HashMap`/`HashSet` fields the same way.
pub(crate) fn lock_std<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
