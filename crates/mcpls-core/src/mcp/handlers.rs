//! MCP handler context.
//!
//! This module provides the shared context for MCP tool handlers.
//! The actual tool implementations use the `#[tool]` macro from rmcp
//! and are defined in the `server` module.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::bridge::{
    DiagnosticsDelivery, FloorTable, NotificationCache, ResourceSubscriptions, ServerSettle,
    Translator,
};
use crate::config::DiagnosticsConfig;

/// Shared context for all tool handlers.
///
/// Holds the translator and subscription state. `Translator` uses interior
/// mutability, with each field locking independently for its short section, so
/// it is shared as a plain `Arc` with no outer lock. Concurrent tool calls can
/// run their LSP round trips without serializing behind a single mutex.
///
/// The MCP peer handle for resource notifications lives in
/// [`ResourceSubscriptions`], keyed by connection.
pub struct BridgeContext {
    /// Translator for converting MCP calls to LSP requests.
    pub translator: Arc<Translator>,
    /// Cache of pushed LSP notifications (diagnostics, logs, messages).
    ///
    /// Locked independently of `translator`, which itself holds no outer
    /// lock, so the `diagnostics_pump` task never contends with a tool call
    /// running an in-flight LSP round-trip.
    pub notification_cache: Arc<Mutex<NotificationCache>>,
    /// Workspace roots, fixed at startup and immutable thereafter.
    ///
    /// Shared as a lock-free snapshot so cache-only handlers (e.g.
    /// `get_cached_diagnostics`, `read_resource`) can validate a path without
    /// locking anything.
    pub workspace_roots: Arc<[PathBuf]>,
    /// Which connections subscribed to which resource URIs.
    pub subscriptions: Arc<ResourceSubscriptions>,
    /// Whether a CWD-discovered `./mcpls.toml` was ignored as untrusted when
    /// the active [`ServerConfig`](crate::config::ServerConfig) was loaded.
    ///
    /// Surfaced in-band via `McplsServer::get_info`'s `ServerInfo.instructions`
    /// (stderr's `tracing::warn!` at load time is typically invisible to an
    /// MCP client).
    pub project_config_ignored: bool,
    /// Per-session record of which diagnostics have already been delivered.
    ///
    /// A site that needs both locks takes `delivery` before
    /// `notification_cache`, never the reverse; a site that needs only one
    /// takes only that one.
    pub delivery: Arc<Mutex<DiagnosticsDelivery>>,
    /// The severity floor each server answers to, resolved once at startup.
    pub floors: Arc<FloorTable>,
    /// The diagnostics configuration, fixed at startup.
    ///
    /// Held by value: `DiagnosticsConfig` is `Copy` and never changes while
    /// the process runs, and the footer reads four scalars off it. Carrying
    /// the whole `ServerConfig` would drag `lsp_servers` and `apply` into a
    /// struct with no use for either.
    pub diagnostics: DiagnosticsConfig,
    /// The same settle tracker the diagnostics pump feeds.
    ///
    /// The footer waits on `$/progress` and the pump is what records it, so
    /// this must be the pump's own `Arc` rather than a fresh tracker.
    pub settle: Arc<ServerSettle>,
}

impl BridgeContext {
    /// Create a new bridge context.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
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
        Self {
            translator,
            notification_cache,
            workspace_roots,
            subscriptions,
            project_config_ignored,
            delivery,
            floors,
            diagnostics,
            settle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Translator;

    #[test]
    fn test_bridge_context_creation() {
        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let delivery = Arc::new(Mutex::new(DiagnosticsDelivery::new(
            DiagnosticsConfig::default(),
        )));
        let floors = Arc::new(FloorTable::new(&DiagnosticsConfig::default(), &[]));
        let settle = Arc::new(ServerSettle::new(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(300),
        ));
        let context = BridgeContext::new(
            translator,
            notification_cache,
            workspace_roots,
            subscriptions,
            false,
            delivery,
            floors,
            DiagnosticsConfig::default(),
            settle,
        );
        assert_eq!(Arc::strong_count(&context.translator), 1);
    }
}
