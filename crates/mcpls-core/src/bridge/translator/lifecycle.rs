//! What each applicable language server is doing.
//!
//! Membership in this map is what makes a server applicable to the
//! checkout. `lsp_servers` stays the live-server registry.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter};
use tokio::sync::watch;

use super::Translator;
use crate::bridge::lock_std;
use crate::config::ServerId;

/// What an applicable language server is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, EnumIter)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "lowercase")]
pub enum ServerLifecycle {
    /// Applicable to this checkout, never triggered.
    Idle,
    /// A spawn is in flight.
    Starting,
    /// Registered and alive.
    Running,
    /// The command is not on `PATH`. Remembered for the backend's life,
    /// because a backend's environment is fixed when it starts, so the
    /// same attempt fails identically every time after.
    #[strum(serialize = "not installed")]
    NotInstalled,
    /// Spawned and died. Inside the respawn backoff.
    Failed,
    /// Stopped by a user. Nothing starts it but an explicit start.
    Stopped,
}

impl Translator {
    /// Record `state` for `id` and tell every subscriber.
    ///
    /// The map write and broadcast share the `lifecycles` lock, so
    /// subscribers and snapshot readers observe the same transition.
    pub fn set_lifecycle(&self, id: &ServerId, state: ServerLifecycle) {
        let mut states = lock_std(&self.lifecycles);
        self.publish_locked(&mut states, id, state);
    }

    /// End `id`'s spawn with the server it installed, publishing `state`.
    ///
    /// Returns false, leaving the spawn claimed, when a user stopped `id`
    /// meanwhile: the caller retires what it installed, then calls
    /// [`Self::release_stopped_spawn`].
    #[expect(
        clippy::significant_drop_tightening,
        reason = "state and watch updates must remain atomic"
    )]
    pub(crate) fn finish_spawn(&self, id: &ServerId, state: ServerLifecycle) -> bool {
        let mut states = lock_std(&self.lifecycles);
        if states.get(id) == Some(&ServerLifecycle::Stopped) {
            return false;
        }
        lock_std(&self.spawning).remove(id);
        self.publish_locked(&mut states, id, state);
        true
    }

    /// End `id`'s spawn without an installed server, publishing `state`
    /// unless a user stopped it meanwhile.
    pub(crate) fn abandon_spawn(&self, id: &ServerId, state: ServerLifecycle) {
        let mut states = lock_std(&self.lifecycles);
        lock_std(&self.spawning).remove(id);
        if states.get(id) != Some(&ServerLifecycle::Stopped) {
            self.publish_locked(&mut states, id, state);
        }
    }

    /// Give up the claim a stopped spawn kept while it retired its server.
    /// Returns true, keeping the claim, when an explicit start adopted the
    /// spawn meanwhile and the caller must spawn again.
    pub(crate) fn release_stopped_spawn(&self, id: &ServerId) -> bool {
        let states = lock_std(&self.lifecycles);
        if states.get(id) == Some(&ServerLifecycle::Starting) {
            return true;
        }
        lock_std(&self.spawning).remove(id);
        drop(states);
        false
    }

    /// Mark an eagerly spawned `id` as starting, with the spawn claimed by
    /// the startup batch.
    pub(crate) fn seed_starting(&self, id: &ServerId) {
        let mut states = lock_std(&self.lifecycles);
        lock_std(&self.spawning).insert(id.clone());
        self.publish_locked(&mut states, id, ServerLifecycle::Starting);
    }

    /// Prepare `id` for an explicit start. A stopped server whose spawn is
    /// still in flight goes back to `Starting` and keeps that spawn, so two
    /// processes never race to install. Otherwise a stopped, missing, or
    /// failed server returns to `Idle` for a fresh claim.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "state and watch updates must remain atomic"
    )]
    pub(crate) fn reset_for_explicit_start(&self, id: &ServerId) {
        let mut states = lock_std(&self.lifecycles);
        let next = match states.get(id) {
            Some(ServerLifecycle::Stopped) if lock_std(&self.spawning).contains(id) => {
                ServerLifecycle::Starting
            }
            Some(
                ServerLifecycle::Stopped | ServerLifecycle::NotInstalled | ServerLifecycle::Failed,
            ) => ServerLifecycle::Idle,
            _ => return,
        };
        self.publish_locked(&mut states, id, next);
    }

    /// Claim the right to spawn `id`, moving it to `Starting`.
    ///
    /// `Running` remains claimable because process death is discovered by
    /// the caller before this method publishes a fresh attempt.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "state and watch updates must remain atomic"
    )]
    pub fn begin_starting(&self, id: &ServerId) -> bool {
        let mut states = lock_std(&self.lifecycles);
        match states.get(id) {
            Some(ServerLifecycle::Idle | ServerLifecycle::Failed | ServerLifecycle::Running) => {}
            _ => return false,
        }
        if !lock_std(&self.spawning).insert(id.clone()) {
            return false;
        }
        self.publish_locked(&mut states, id, ServerLifecycle::Starting);
        true
    }

    fn publish_locked(
        &self,
        states: &mut HashMap<ServerId, ServerLifecycle>,
        id: &ServerId,
        state: ServerLifecycle,
    ) {
        states.insert(id.clone(), state);
        lock_std(&self.lifecycle_senders)
            .entry(id.clone())
            .or_insert_with(|| watch::channel(state).0)
            .send_replace(state);
    }

    /// The state recorded for `id`, or `None` when `id` is not applicable
    /// to this checkout.
    #[must_use]
    pub fn lifecycle_of(&self, id: &ServerId) -> Option<ServerLifecycle> {
        lock_std(&self.lifecycles).get(id).copied()
    }

    /// Every applicable server and its state, ordered by identity so a
    /// reader sees the same line twice in a row.
    #[must_use]
    pub fn lifecycles(&self) -> Vec<(ServerId, ServerLifecycle)> {
        let mut states: Vec<(ServerId, ServerLifecycle)> = lock_std(&self.lifecycles)
            .iter()
            .map(|(id, state)| (id.clone(), *state))
            .collect();
        states.sort_by(|(left, _), (right, _)| left.cmp(right));
        states
    }

    /// Watch `id`'s state. The channel is created on first use, so a
    /// caller can subscribe before anything has been recorded.
    pub fn subscribe_lifecycle(&self, id: &ServerId) -> watch::Receiver<ServerLifecycle> {
        let mut senders = lock_std(&self.lifecycle_senders);
        senders
            .entry(id.clone())
            .or_insert_with(|| watch::channel(ServerLifecycle::Idle).0)
            .subscribe()
    }
}

/// The senders' map type, named so the struct field reads clearly.
pub(super) type LifecycleSenders = HashMap<ServerId, watch::Sender<ServerLifecycle>>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use strum::IntoEnumIterator;

    use super::*;

    #[test]
    fn test_every_state_renders_for_a_reader() {
        let rendered: Vec<String> = ServerLifecycle::iter()
            .map(|state| state.to_string())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "idle",
                "starting",
                "running",
                "not installed",
                "failed",
                "stopped"
            ]
        );
    }

    #[test]
    fn test_every_state_survives_a_wire_round_trip() {
        for state in ServerLifecycle::iter() {
            let json = serde_json::to_string(&state).expect("serialize");
            let back: ServerLifecycle = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, state);
        }
    }

    #[test]
    fn test_a_missing_binary_is_not_the_same_state_as_a_crash() {
        assert_ne!(ServerLifecycle::NotInstalled, ServerLifecycle::Failed);
        assert_eq!(ServerLifecycle::NotInstalled.to_string(), "not installed");
        assert_eq!(ServerLifecycle::Failed.to_string(), "failed");
    }

    #[test]
    fn test_a_state_change_reaches_a_subscriber() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Idle);
        let mut states = translator.subscribe_lifecycle(&id);
        translator.set_lifecycle(&id, ServerLifecycle::Running);
        assert_eq!(*states.borrow_and_update(), ServerLifecycle::Running);
    }

    #[test]
    fn test_lifecycles_are_reported_in_a_stable_order() {
        let translator = Translator::new();
        translator.set_lifecycle(&ServerId::from("typescript"), ServerLifecycle::Idle);
        translator.set_lifecycle(&ServerId::from("rust"), ServerLifecycle::Running);
        let reported: Vec<String> = translator
            .lifecycles()
            .into_iter()
            .map(|(id, _)| id.to_string())
            .collect();
        assert_eq!(reported, vec!["rust", "typescript"]);
    }

    #[test]
    fn test_only_one_caller_claims_a_spawn() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Idle);
        assert!(translator.begin_starting(&id));
        assert!(!translator.begin_starting(&id));
        assert_eq!(
            translator.lifecycle_of(&id),
            Some(ServerLifecycle::Starting)
        );
    }

    #[test]
    fn test_a_settled_failure_is_not_claimed_as_a_spawn() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::NotInstalled);
        assert!(!translator.begin_starting(&id));
        assert_eq!(
            translator.lifecycle_of(&id),
            Some(ServerLifecycle::NotInstalled)
        );
    }

    #[test]
    fn test_a_server_absent_from_the_applicable_set_is_not_claimed() {
        let translator = Translator::new();
        assert!(!translator.begin_starting(&ServerId::from("rust")));
        assert_eq!(translator.lifecycle_of(&ServerId::from("rust")), None);
    }

    #[test]
    fn test_a_running_entry_is_claimable_because_death_is_not_published() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Running);
        assert!(translator.begin_starting(&id));
    }

    #[test]
    fn test_a_stopped_server_keeps_its_state_against_a_spawn_outcome() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Idle);
        assert!(translator.begin_starting(&id));
        translator.set_lifecycle(&id, ServerLifecycle::Stopped);
        assert!(!translator.finish_spawn(&id, ServerLifecycle::Running));
        assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Stopped));
        assert!(!translator.release_stopped_spawn(&id));

        translator.set_lifecycle(&id, ServerLifecycle::Idle);
        assert!(translator.begin_starting(&id));
        assert!(translator.finish_spawn(&id, ServerLifecycle::Running));
        assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Running));
    }

    #[test]
    fn test_a_start_during_a_stopped_spawn_adopts_it() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Idle);
        assert!(translator.begin_starting(&id));
        translator.set_lifecycle(&id, ServerLifecycle::Stopped);

        translator.reset_for_explicit_start(&id);
        assert_eq!(
            translator.lifecycle_of(&id),
            Some(ServerLifecycle::Starting)
        );
        assert!(
            !translator.begin_starting(&id),
            "the adopted spawn is the only one"
        );
        assert!(translator.finish_spawn(&id, ServerLifecycle::Running));
        assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Running));
    }

    #[test]
    fn test_an_abandoned_spawn_frees_the_claim_and_keeps_a_stop() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Idle);
        assert!(translator.begin_starting(&id));
        translator.set_lifecycle(&id, ServerLifecycle::Stopped);
        translator.abandon_spawn(&id, ServerLifecycle::Failed);
        assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Stopped));

        translator.reset_for_explicit_start(&id);
        assert_eq!(translator.lifecycle_of(&id), Some(ServerLifecycle::Idle));
        assert!(translator.begin_starting(&id));
    }

    #[test]
    fn test_an_explicit_start_resets_only_states_that_block_a_spawn() {
        let translator = Translator::new();
        for (state, expected) in [
            (ServerLifecycle::Stopped, ServerLifecycle::Idle),
            (ServerLifecycle::NotInstalled, ServerLifecycle::Idle),
            (ServerLifecycle::Failed, ServerLifecycle::Idle),
            (ServerLifecycle::Running, ServerLifecycle::Running),
            (ServerLifecycle::Starting, ServerLifecycle::Starting),
        ] {
            let id = ServerId::from(state.to_string());
            translator.set_lifecycle(&id, state);
            translator.reset_for_explicit_start(&id);
            assert_eq!(translator.lifecycle_of(&id), Some(expected), "{state}");
        }
    }

    #[test]
    fn test_a_stopped_server_does_not_claim_a_spawn() {
        let translator = Translator::new();
        let id = ServerId::from("rust");
        translator.set_lifecycle(&id, ServerLifecycle::Stopped);
        assert!(!translator.begin_starting(&id));
    }
}
