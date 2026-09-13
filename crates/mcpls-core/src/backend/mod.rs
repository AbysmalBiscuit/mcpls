//! One mcpls backend per checkout root, shared by every session in it.
//!
//! The frontend a host launches relays MCP traffic to the backend over the
//! project's endpoint, and the backend owns the language servers and every
//! session's records.

pub mod endpoint;
pub mod handshake;
pub mod spawn;

pub use endpoint::serve_backend;
pub use handshake::{
    ConfigStamp, ConnectionKind, Handshake, HandshakeReply, PROTOCOL, Refusal, VERSION,
    compare_builds,
};
pub use spawn::{BackendLaunch, SpawnLock, request_start, spawn_detached, start_requested};
