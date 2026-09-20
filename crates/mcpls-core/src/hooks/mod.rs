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
pub mod watcher;

pub use filters::{PathFilter, WatchSet, watch_set};
#[cfg(windows)]
pub use identity::windows_pipe_prefix;
pub use identity::{SocketIdentity, identity_for, identity_hash, project_root};
pub use listener::{
    HookListener, ProbeOutcome, ServeExit, probe, send, send_and_acknowledge, send_many,
};
pub use protocol::{ChangeEvent, Request, Response, WatcherStatus};
pub use service::{HookLocation, HookStats, StatusExtras, StatusSource, build_handler};
pub use sweep::{Origin, SweepKind, Sweeper};
pub use watcher::{ProjectWatcher, WatchState};
