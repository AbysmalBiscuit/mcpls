//! LSP client implementation.
//!
//! This module provides the LSP client for communicating with language servers
//! over JSON-RPC 2.0.

mod client;
mod lifecycle;
mod transport;
pub(crate) mod types;
pub mod watched_files;

pub(crate) use client::CONTENT_MODIFIED_RETRY_METHODS;
pub use client::{ApplySink, LspClient};
#[cfg(test)]
pub(crate) use lifecycle::fake_lsp_server;
pub use lifecycle::{LspServer, ServerInitConfig, ServerInitResult, ServerState};
pub use transport::LspTransport;
pub use types::{
    InboundMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, LspNotification,
    RequestId,
};
pub use watched_files::WatchRegistry;
