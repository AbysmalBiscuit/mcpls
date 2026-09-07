//! The per-project socket that lets Claude Code's hooks push changed paths
//! into a running mcpls and pull new diagnostics back out.
//!
//! Inert without the plugin: with no hook process ever connecting, the
//! listener binds, waits, and does nothing.

pub mod filters;
pub mod identity;
pub mod listener;
pub mod protocol;
pub mod service;
pub mod sweep;

pub use filters::{PathFilter, watch_paths};
pub use identity::{SocketIdentity, identity_for};
pub use listener::{HookListener, LockLoss, ServeExit, send, send_many};
pub use protocol::{ChangeEvent, Request, Response};
pub use service::{HookRole, Role, build_handler};
pub(crate) use service::{hook_owner_task, hook_takeover_task};
pub use sweep::{SweepKind, Sweeper};
