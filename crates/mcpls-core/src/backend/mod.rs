//! One mcpls backend per checkout root, shared by every session in it.
//!
//! The frontend a host launches relays MCP traffic to the backend over the
//! project's endpoint, and the backend owns the language servers and every
//! session's records.

pub mod control;
pub mod endpoint;
pub mod frontend;
pub mod handshake;
pub mod spawn;
mod stub;

pub use endpoint::serve_backend;
pub use frontend::{FrontendOptions, run_frontend};
pub use handshake::{
    ConfigStamp, ConnectionKind, Handshake, HandshakeReply, PROTOCOL, Refusal, VERSION,
    compare_builds,
};
pub use spawn::{BackendLaunch, SpawnLock, request_start, spawn_detached, start_requested};
