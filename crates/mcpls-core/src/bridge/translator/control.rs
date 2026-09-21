//! Lifecycle changes a user asks for: stop, start, and restart.

use std::sync::Weak;

use serde::{Deserialize, Serialize};

use super::{ServerLifecycle, Translator, shut_down_server};
use crate::config::ServerId;

/// A lifecycle change requested through `mcpls lsp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspAction {
    /// Spawn a server that is not running.
    Start,
    /// Shut a server down and keep it down until an explicit start.
    Stop,
    /// Replace a running server's process, or start one that is not running.
    Restart,
}

impl Translator {
    /// Apply `action` to each server in `requested`, or to every applicable
    /// server when `requested` is empty, returning each one's state
    /// afterwards.
    ///
    /// # Errors
    ///
    /// Names the ids that do not apply to this checkout and applies
    /// nothing.
    pub async fn control(
        &self,
        action: LspAction,
        requested: &[String],
    ) -> std::result::Result<Vec<(ServerId, ServerLifecycle)>, String> {
        let applicable: Vec<ServerId> = self.lifecycles().into_iter().map(|(id, _)| id).collect();
        let targets: Vec<ServerId> = if requested.is_empty() {
            applicable.clone()
        } else {
            requested
                .iter()
                .map(|id| ServerId::from(id.as_str()))
                .collect()
        };
        let unknown: Vec<&str> = targets
            .iter()
            .filter(|id| !applicable.contains(id))
            .map(ServerId::as_str)
            .collect();
        if !unknown.is_empty() {
            let known: Vec<&str> = applicable.iter().map(ServerId::as_str).collect();
            let known = if known.is_empty() {
                "none do".to_string()
            } else {
                format!("these do: {}", known.join(", "))
            };
            return Err(format!(
                "no language server named {} applies here; {known}",
                unknown.join(", ")
            ));
        }
        for id in &targets {
            match action {
                LspAction::Start => self.start_server(id).await,
                LspAction::Stop => self.stop_server(id).await,
                LspAction::Restart => self.restart_server(id).await,
            }
        }
        Ok(targets
            .into_iter()
            .filter_map(|id| self.lifecycle_of(&id).map(|state| (id, state)))
            .collect())
    }

    /// Mark `id` stopped, then shut its process down.
    pub(crate) async fn stop_server(&self, id: &ServerId) {
        self.set_lifecycle(id, ServerLifecycle::Stopped);
        self.tear_down(id).await;
    }

    /// Spawn `id` unless it is already running or starting. Retries a
    /// server recorded as not installed, since an explicit start usually
    /// follows installing it.
    pub(crate) async fn start_server(&self, id: &ServerId) {
        self.reset_for_explicit_start(id);
        self.clear_respawn_backoff(id);
        // The lifecycle the caller reads afterwards carries the outcome.
        let _ = self.ensure_server(id, None).await;
    }

    /// Replace `id`'s live process with a fresh one, or start it when none
    /// is live.
    pub(crate) async fn restart_server(&self, id: &ServerId) {
        if !self.has_live_client(id) {
            self.start_server(id).await;
            return;
        }
        self.clear_respawn_backoff(id);
        if !self.begin_starting(id) {
            return;
        }
        match self.self_handle.get().and_then(Weak::upgrade) {
            Some(translator) => {
                tokio::spawn(translator.run_spawn(id.clone()));
            }
            None => self.set_lifecycle(id, ServerLifecycle::Failed),
        }
    }

    /// Detach `id` from routing, drop its watches and diagnostics
    /// ownership, and shut its process down in the background.
    pub(crate) async fn tear_down(&self, id: &ServerId) {
        self.forget_watch_registrations(id);
        if let Some(server) = self.retire_server(id).await {
            tokio::spawn(shut_down_server(id.clone(), server));
        }
        self.reconcile_diagnostics_owners(None).await;
    }
}
