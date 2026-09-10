//! Tracks whether the language servers have finished their startup work.
//!
//! Every server reports long-running work through `$/progress`. Quiet is not
//! the first moment the outstanding count reaches zero: rust-analyzer's
//! startup phases hand off to each other through gaps of about 70 to 100
//! milliseconds, and the first of those gaps arrives before indexing has
//! begun. Quiet is a count of zero that has held for `quiet_for`.
//!
//! A deadline bounds a failure mode neither the count nor the debounce can
//! see on their own: a notification dropped from a full channel before its
//! pump existed. A server that reports no progress at all is not this case:
//! restarting the deadline also stamps its first quiet moment, so it settles
//! once its own grace has passed rather than waiting out the backstop. That
//! grace is longer than `quiet_for`, deliberately: `quiet_for` bridges gaps
//! between phases a server that is already talking has started, not the
//! silence before a slower one has said anything at all.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::ServerId;

/// How long a server that has never reported any `$/progress` is given,
/// once [`ServerSettle::restart_deadline`] runs, before its silence counts
/// as having nothing to report.
///
/// Measured against this repository's own workspace (method and raw numbers
/// in `docs/superpowers/notes/2026-09-07-stage-b-measurements.md`):
/// rust-analyzer's first `$/progress` `begin` arrived 3.8-4.7 ms after
/// `initialized`, across six samples split evenly between a workspace with
/// no `target/` directory yet and one already built. Two seconds is roughly
/// 500 times that observed latency -- room for scheduling and channel
/// delays, a slower machine, and a larger workspace than this one -- while
/// staying two orders of magnitude under the five-minute deadline, so a
/// server that is merely slow to speak is not mistaken for one that never
/// will, and a server that truly never reports anything still settles in
/// about two seconds instead of waiting out the backstop.
const NO_PROGRESS_GRACE: Duration = Duration::from_secs(2);

/// Outstanding `$/progress` operations across every server.
#[derive(Debug)]
pub struct ServerSettle {
    state: Mutex<SettleState>,
    quiet_for: Duration,
    deadline_after: Duration,
}

#[derive(Debug)]
struct SettleState {
    outstanding: HashSet<(ServerId, String)>,
    server_progress: HashMap<ServerId, ServerProgress>,
    diagnostics_owners: HashSet<ServerId>,
    owners_installed: bool,
    pending_owners: HashSet<ServerId>,
    replacement_pending: HashSet<ServerId>,
    retired_servers: HashSet<ServerId>,
    /// When the outstanding set last became empty. `None` until the first
    /// operation ends, so a process that has not yet heard from a server is
    /// not mistaken for one whose servers have finished.
    quiet_since: Option<Instant>,
    /// When the backstop fires regardless of what the servers have said.
    deadline: Instant,
    /// How many long-running operations have ever begun.
    epoch: u64,
}

#[derive(Debug, Default)]
struct ServerProgress {
    outstanding: HashSet<String>,
    quiet_since: Option<Instant>,
    progress_seen: bool,
}

impl ServerSettle {
    /// Track settling with a `quiet_for` debounce, giving up after
    /// `deadline_after`.
    ///
    /// The deadline runs from construction; [`Self::restart_deadline`] moves
    /// it to cover the work it is meant to bound.
    #[must_use]
    pub fn new(quiet_for: Duration, deadline_after: Duration) -> Self {
        Self {
            state: Mutex::new(SettleState {
                outstanding: HashSet::new(),
                server_progress: HashMap::new(),
                diagnostics_owners: HashSet::new(),
                owners_installed: false,
                pending_owners: HashSet::new(),
                replacement_pending: HashSet::new(),
                retired_servers: HashSet::new(),
                quiet_since: None,
                deadline: Instant::now() + deadline_after,
                epoch: 0,
            }),
            quiet_for,
            deadline_after,
        }
    }

    /// Set the diagnostics owners whose startup silence needs its own grace.
    pub fn set_diagnostics_owners(&self, owners: impl IntoIterator<Item = ServerId>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let owners: HashSet<_> = owners.into_iter().collect();
        state.pending_owners.retain(|owner| owners.contains(owner));
        let retired_owners = owners
            .iter()
            .filter(|owner| {
                state.retired_servers.contains(*owner) || state.replacement_pending.contains(*owner)
            })
            .cloned()
            .collect::<Vec<_>>();
        state.pending_owners.extend(retired_owners);
        state.diagnostics_owners = owners
            .into_iter()
            .filter(|owner| !state.retired_servers.contains(owner))
            .collect();
        state.owners_installed = true;
    }

    /// Re-install a diagnostics owner after a server replacement completed.
    pub(crate) fn register_diagnostics_owner(&self, server: &ServerId) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.retired_servers.remove(server);
        state.pending_owners.remove(server);
        state.replacement_pending.remove(server);
        state.diagnostics_owners.insert(server.clone());
        let progress = state.server_progress.entry(server.clone()).or_default();
        if progress.outstanding.is_empty() && progress.quiet_since.is_none() {
            progress.quiet_since = Some(Instant::now());
        }
    }

    /// Keep a retiring diagnostics owner in the startup wait until its replacement is installed.
    pub(crate) fn begin_diagnostics_replacement(&self, server: &ServerId) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.replacement_pending.insert(server.clone());
        if state.diagnostics_owners.contains(server) {
            state.pending_owners.insert(server.clone());
        }
    }

    /// Measure the deadline from now instead of from construction.
    ///
    /// The backstop exists to bound indexing, but a `ServerSettle` is built
    /// before any server is spawned, so config load and every server's
    /// `initialize` handshake would otherwise be spent out of the same
    /// budget. Callers restart the clock once the servers exist, and must do
    /// so unconditionally rather than on the arrival of server traffic: a
    /// server that never reports progress produces no event to hang this on.
    ///
    /// This is also where such a server's clock gets its start: nothing is
    /// outstanding yet at this point, so if nothing has been stamped
    /// either, this counts as its first quiet moment. Unlike a stamp from a
    /// real `end`, [`Self::should_settle_at`] only trusts this one once
    /// `NO_PROGRESS_GRACE` has passed rather than the shorter `quiet_for`,
    /// so a server that has simply not spoken yet is not mistaken for one
    /// that never will. A server that begins reporting progress before then
    /// clears the stamp the same way any other `begin` does, and its later
    /// `end` re-stamps under the ordinary debounce instead.
    pub fn restart_deadline(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let now = Instant::now();
        state.deadline = now + self.deadline_after;
        if state.outstanding.is_empty() && state.quiet_since.is_none() {
            state.quiet_since = Some(now);
        }
        let owners = state.diagnostics_owners.clone();
        for owner in owners {
            let progress = state.server_progress.entry(owner).or_default();
            if progress.outstanding.is_empty() && progress.quiet_since.is_none() {
                progress.quiet_since = Some(now);
            }
        }
    }

    /// Record that `server` started a long-running operation.
    ///
    /// Takes no instant: nothing here is time-dependent, since starting
    /// work only clears the quiet stamp and bumps the epoch.
    pub fn begin(&self, server: &ServerId, token: &serde_json::Value) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let token = token.to_string();
        state.outstanding.insert((server.clone(), token.clone()));
        let progress = state.server_progress.entry(server.clone()).or_default();
        progress.outstanding.insert(token);
        progress.progress_seen = true;
        progress.quiet_since = None;
        state.quiet_since = None;
        state.epoch += 1;
    }

    /// Record that `server` finished one, as of `now`.
    ///
    /// Takes the instant rather than reading the clock, so the footer's
    /// wait can be driven in a test without sleeping. `end` stamps the wall
    /// clock, which is why this module's older tests sleep; the footer
    /// cannot afford that, since its assertions are about which of three
    /// branches ended the wait.
    pub fn end_at(&self, server: &ServerId, token: &serde_json::Value, now: Instant) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let token = token.to_string();
        if !state.outstanding.remove(&(server.clone(), token.clone())) {
            return;
        }
        if let Some(progress) = state.server_progress.get_mut(server) {
            progress.outstanding.remove(&token);
            if progress.outstanding.is_empty() {
                progress.quiet_since = Some(now);
            }
        }
        if state.outstanding.is_empty() {
            state.quiet_since = Some(now);
        }
    }

    /// Record that `server` finished one.
    pub fn end(&self, server: &ServerId, token: &serde_json::Value) {
        self.end_at(server, token, Instant::now());
    }

    /// Retire outstanding work owned by an exited server without resetting the session.
    pub(crate) fn forget_server(&self, server: &ServerId) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let before = state.outstanding.len();
        state.outstanding.retain(|(id, _)| id != server);
        if state.outstanding.len() != before && state.outstanding.is_empty() {
            state.quiet_since = Some(Instant::now());
        }
        state.server_progress.remove(server);
        let was_owner = state.diagnostics_owners.remove(server);
        if was_owner && state.replacement_pending.contains(server) {
            state.pending_owners.insert(server.clone());
        }
        state.retired_servers.insert(server.clone());
    }

    /// How many long-running operations have ever begun.
    ///
    /// The footer captures this before its resync and compares afterwards,
    /// so an index already running when the rename landed does not read as
    /// the check that rename started.
    #[must_use]
    pub fn progress_epoch(&self) -> u64 {
        self.state.lock().map_or(0, |state| state.epoch)
    }

    /// Whether nothing has been outstanding for `quiet_for` as of `now`.
    ///
    /// Differs from [`Self::should_settle_at`] in two ways, both deliberate.
    /// There is no deadline: the footer carries its own, much shorter, cap.
    /// And a workspace that has never reported any progress counts as quiet
    /// immediately here, with no grace of its own -- unlike the baseline's
    /// judgment, which waits out its own `NO_PROGRESS_GRACE` before reaching
    /// the same conclusion. The baseline pays that grace once per session; a
    /// footer runs on every write, after its own grace period has already
    /// elapsed once, and cannot afford to wait again inside this check, or a
    /// configured server that reports no `$/progress` would make every
    /// footer burn its whole cap. Applying that grace before the first call
    /// here is the caller's job -- this method enforces none of its own for
    /// the never-reported case.
    #[must_use]
    pub fn is_quiet_at(&self, now: Instant, quiet_for: Duration) -> bool {
        let Ok(state) = self.state.lock() else {
            return false;
        };
        state.outstanding.is_empty()
            && state
                .quiet_since
                .is_none_or(|since| now.duration_since(since) >= quiet_for)
    }

    /// Whether the workspace counts as analyzed as of `now`.
    ///
    /// A quiet stamp from a real `end` needs only `quiet_for` to elapse. One
    /// from [`Self::restart_deadline`], on a server that has never begun any
    /// work, needs `NO_PROGRESS_GRACE` instead -- see that constant's doc
    /// for why.
    #[must_use]
    pub fn should_settle_at(&self, now: Instant) -> bool {
        let Ok(state) = self.state.lock() else {
            return false;
        };
        if now >= state.deadline {
            return true;
        }
        if !state.owners_installed {
            if !state.outstanding.is_empty() {
                return false;
            }
            let Some(since) = state.quiet_since else {
                return false;
            };
            let required = if state.epoch == 0 {
                NO_PROGRESS_GRACE
            } else {
                self.quiet_for
            };
            return now.duration_since(since) >= required;
        }
        if !state.pending_owners.is_empty() {
            return false;
        }
        for owner in &state.diagnostics_owners {
            let Some(progress) = state.server_progress.get(owner) else {
                return false;
            };
            if !progress.outstanding.is_empty() {
                return false;
            }
            let Some(owner_quiet_since) = progress.quiet_since else {
                return false;
            };
            let required = if progress.progress_seen {
                self.quiet_for
            } else {
                NO_PROGRESS_GRACE
            };
            if now.duration_since(owner_quiet_since) < required {
                return false;
            }
        }
        true
    }

    /// [`Self::should_settle_at`] as of now.
    #[must_use]
    pub fn should_settle(&self) -> bool {
        self.should_settle_at(Instant::now())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;

    fn settle() -> ServerSettle {
        ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600))
    }

    #[test]
    fn test_settle_retirement_preserves_other_server_and_epoch() {
        let settle = settle();
        let rust = ServerId::from("rust");
        let python = ServerId::from("python");
        settle.begin(&rust, &json!("check"));
        settle.begin(&python, &json!("check"));
        settle.forget_server(&rust);
        assert!(!settle.is_quiet_at(
            Instant::now() + Duration::from_secs(2),
            Duration::from_secs(1)
        ));
        assert_eq!(settle.progress_epoch(), 2);
        settle.end(&python, &json!("check"));
        assert!(settle.should_settle_at(Instant::now() + Duration::from_secs(2)));
        settle.begin(&rust, &json!("replacement"));
        settle.forget_server(&rust);
        assert!(settle.should_settle_at(Instant::now() + Duration::from_secs(2)));
        assert_eq!(settle.progress_epoch(), 3);
    }

    #[test]
    fn test_a_server_with_work_outstanding_is_not_quiet() {
        let settle = settle();
        let rust = ServerId::from("rust");
        settle.begin(&rust, &json!("rustAnalyzer/cachePriming"));
        assert!(!settle.should_settle_at(Instant::now() + Duration::from_secs(5)));
    }

    #[test]
    fn test_quiet_must_outlast_the_gap_between_two_startup_phases() {
        let settle = settle();
        let rust = ServerId::from("rust");
        settle.begin(&rust, &json!("rustAnalyzer/Fetching"));
        settle.end(&rust, &json!("rustAnalyzer/Fetching"));
        let quiet_began = Instant::now();

        assert!(
            !settle.should_settle_at(quiet_began + Duration::from_millis(100)),
            "rust-analyzer hands off between startup phases in about this long, \
             and indexing has not started yet"
        );
        assert!(settle.should_settle_at(quiet_began + Duration::from_secs(2)));
    }

    #[test]
    fn test_work_resuming_restarts_the_quiet_period() {
        let settle = settle();
        let rust = ServerId::from("rust");
        settle.begin(&rust, &json!("rustAnalyzer/Indexing"));
        settle.end(&rust, &json!("rustAnalyzer/Indexing"));
        settle.begin(&rust, &json!("rust-analyzer/flycheck/0"));
        assert!(!settle.should_settle_at(Instant::now() + Duration::from_secs(5)));
    }

    #[test]
    fn test_a_second_quiet_period_restarts_the_clock_from_its_own_end() {
        // A small `quiet_for` so the real sleep below (needed because `end`
        // stamps `quiet_since` from the wall clock, not from an injectable
        // `now`) stays short.
        let quiet_for = Duration::from_millis(50);
        let settle = ServerSettle::new(quiet_for, Duration::from_secs(600));
        let rust = ServerId::from("rust");

        settle.begin(&rust, &json!("rustAnalyzer/Indexing"));
        settle.end(&rust, &json!("rustAnalyzer/Indexing"));
        let first_quiet_began = Instant::now();

        std::thread::sleep(quiet_for * 3);

        settle.begin(&rust, &json!("rust-analyzer/flycheck/0"));
        settle.end(&rust, &json!("rust-analyzer/flycheck/0"));
        let second_quiet_began = Instant::now();

        // Sanity-check the test's own timing assumption before relying on
        // it: the sleep above must actually have separated the two quiet
        // periods by more than `quiet_for`, or the assertions below would
        // hold vacuously regardless of which quiet period the clock is
        // reading from.
        assert!(second_quiet_began.duration_since(first_quiet_began) > quiet_for);

        // More than `quiet_for` past the first quiet period, but not yet
        // `quiet_for` past the second. An implementation that stamps
        // `quiet_since` once and accumulates from there, instead of
        // re-stamping it on every later empty transition, would already
        // call this settled here (or would never settle again at all, if
        // it instead forgets to re-stamp altogether).
        assert!(
            !settle.should_settle_at(second_quiet_began),
            "the clock must restart from the second quiet period, not the first"
        );
        assert!(settle.should_settle_at(second_quiet_began + quiet_for * 2));
    }

    #[test]
    fn test_two_servers_sharing_a_token_string_are_tracked_independently() {
        let settle = settle();
        let rust = ServerId::from("rust");
        let python = ServerId::from("python");
        settle.begin(&rust, &json!("indexing"));
        settle.begin(&python, &json!("indexing"));
        settle.end(&rust, &json!("indexing"));
        assert!(
            !settle.should_settle_at(Instant::now() + Duration::from_secs(5)),
            "python's identically-named token is still outstanding"
        );
    }

    #[test]
    fn test_every_server_must_be_quiet_at_once() {
        let settle = settle();
        let rust = ServerId::from("rust");
        let python = ServerId::from("python");
        settle.begin(&rust, &json!("indexing"));
        settle.begin(&python, &json!("analyzing"));
        settle.end(&rust, &json!("indexing"));

        assert!(!settle.should_settle_at(Instant::now() + Duration::from_secs(5)));
        settle.end(&python, &json!("analyzing"));
        assert!(settle.should_settle_at(Instant::now() + Duration::from_secs(5)));
    }

    #[test]
    fn test_an_end_without_a_begin_does_not_start_a_quiet_period() {
        let settle = settle();
        settle.end(&ServerId::from("rust"), &json!("orphan"));
        assert!(
            !settle.should_settle_at(Instant::now() + Duration::from_secs(5)),
            "an unmatched end proves nothing about what is still running"
        );
    }

    #[test]
    fn test_a_tracker_whose_deadline_was_never_restarted_settles_only_at_the_deadline() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(60));
        assert!(!settle.should_settle_at(Instant::now() + Duration::from_secs(30)));
        assert!(
            settle.should_settle_at(Instant::now() + Duration::from_secs(61)),
            "a baseline taken late beats never reporting anything as new"
        );
    }

    #[test]
    fn test_the_deadline_fires_even_with_work_still_outstanding() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(60));
        settle.begin(&ServerId::from("rust"), &json!("stuck"));
        assert!(settle.should_settle_at(Instant::now() + Duration::from_secs(61)));
    }

    #[test]
    fn test_restarting_the_deadline_spends_none_of_it_on_startup() {
        let deadline_after = Duration::from_secs(60);
        let settle = ServerSettle::new(Duration::from_secs(1), deadline_after);
        // Whatever ran between construction and the servers existing: config
        // load, then every server's `initialize` handshake.
        let servers_ready = Instant::now() + deadline_after;
        settle.begin(&ServerId::from("rust"), &json!("rustAnalyzer/Indexing"));

        assert!(
            settle.should_settle_at(servers_ready),
            "the original deadline is measured from construction and is spent by now"
        );

        settle.restart_deadline();
        let restarted = Instant::now();

        assert!(
            !settle.should_settle_at(restarted + deadline_after / 2),
            "indexing must still get the full deadline after the restart"
        );
        assert!(settle.should_settle_at(restarted + deadline_after * 2));
    }

    #[test]
    fn test_restarting_the_deadline_keeps_a_reached_quiet_period() {
        let quiet_for = Duration::from_millis(10);
        let settle = ServerSettle::new(quiet_for, Duration::from_secs(60));
        let rust = ServerId::from("rust");
        settle.begin(&rust, &json!("rustAnalyzer/Indexing"));
        settle.end(&rust, &json!("rustAnalyzer/Indexing"));
        let quiet_began = Instant::now();

        settle.restart_deadline();

        assert!(
            settle.should_settle_at(quiet_began + quiet_for * 2),
            "the restart moves the backstop, not the debounce"
        );
    }

    #[test]
    fn test_restart_deadline_rescues_a_server_that_never_reports_progress() {
        // A short `quiet_for` here, so that if a regression makes this
        // settle via the ordinary debounce instead of `NO_PROGRESS_GRACE`,
        // the first assertion below (querying well past `quiet_for` but
        // still short of the grace) catches it rather than passing
        // vacuously.
        let settle = ServerSettle::new(Duration::from_millis(1), Duration::from_secs(600));
        settle.restart_deadline();
        let restarted = Instant::now();

        assert!(
            !settle.should_settle_at(restarted + NO_PROGRESS_GRACE / 2),
            "a server that has not spoken yet gets the full no-progress \
             grace before its silence counts as having nothing to report"
        );
        assert!(
            settle.should_settle_at(restarted + NO_PROGRESS_GRACE),
            "a server that never reports progress must settle on the \
             no-progress grace once the servers are spawned, not wait for \
             the five-minute backstop"
        );
    }

    #[test]
    fn test_a_begin_inside_the_grace_stops_the_no_progress_settlement() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
        settle.restart_deadline();
        let restarted = Instant::now();
        settle.begin(&ServerId::from("rust"), &json!("rustAnalyzer/Fetching"));

        assert!(
            !settle.should_settle_at(restarted + NO_PROGRESS_GRACE * 2),
            "a begin arriving inside the grace clears the provisional \
             stamp; a server that spoke late must not be baselined empty \
             just because the grace window would otherwise have elapsed"
        );
    }

    #[test]
    fn test_a_begin_before_the_grace_elapses_switches_to_the_normal_debounce() {
        let quiet_for = Duration::from_millis(10);
        let settle = ServerSettle::new(quiet_for, Duration::from_secs(600));
        settle.restart_deadline();
        let rust = ServerId::from("rust");

        // A server that speaks up before the no-progress grace elapses is
        // no longer judged by the grace at all from here on: it gets the
        // ordinary, much shorter quiet debounce once its own work ends.
        settle.begin(&rust, &json!("rustAnalyzer/Fetching"));
        let end_at = Instant::now();
        settle.end_at(&rust, &json!("rustAnalyzer/Fetching"), end_at);

        assert!(
            !settle.should_settle_at(end_at + quiet_for / 2),
            "a real end still needs its own quiet debounce to elapse"
        );
        assert!(
            settle.should_settle_at(end_at + quiet_for * 2),
            "once real progress has begun and ended, the ordinary quiet \
             debounce governs, not the no-progress grace -- this must \
             settle well before the grace would elapse, not wait for it"
        );
    }

    #[test]
    fn test_footer_quiet_holds_for_a_workspace_that_never_reported_progress() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
        let now = Instant::now();

        assert!(
            !settle.should_settle_at(now),
            "the baseline's judgment waits for a first end, because a workspace \
             that has said nothing yet may simply not have started"
        );
        assert!(
            settle.is_quiet_at(now, Duration::from_millis(200)),
            "the footer's does not: it runs after its own grace period, and a \
             server that reports no progress at all would otherwise burn the \
             whole cap on every write"
        );
    }

    #[test]
    fn test_footer_quiet_waits_while_work_is_outstanding() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
        let now = Instant::now();
        settle.begin(&ServerId::from("rust"), &json!("flycheck"));

        assert!(!settle.is_quiet_at(now + Duration::from_secs(30), Duration::from_millis(200)));
    }

    #[test]
    fn test_footer_quiet_needs_its_own_debounce_after_the_last_end() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
        let start = Instant::now();
        let rust = ServerId::from("rust");
        settle.begin(&rust, &json!("flycheck"));
        settle.end_at(&rust, &json!("flycheck"), start);

        assert!(!settle.is_quiet_at(
            start + Duration::from_millis(100),
            Duration::from_millis(200)
        ));
        assert!(settle.is_quiet_at(
            start + Duration::from_millis(300),
            Duration::from_millis(200)
        ));
    }

    #[test]
    fn test_the_progress_epoch_moves_only_when_work_begins() {
        let settle = ServerSettle::new(Duration::from_secs(1), Duration::from_secs(600));
        let rust = ServerId::from("rust");
        let before = settle.progress_epoch();

        settle.end_at(&rust, &json!("orphan"), Instant::now());
        assert_eq!(
            settle.progress_epoch(),
            before,
            "an unmatched end is not new work; the footer uses this to tell an \
             index already in flight from a check its own save started"
        );

        settle.begin(&rust, &json!("flycheck"));
        assert_eq!(settle.progress_epoch(), before + 1);
    }

    #[test]
    fn i1_t5_owner_progress_uses_the_exact_quiet_boundary() {
        let quiet_for = Duration::from_millis(10);
        let settle = ServerSettle::new(quiet_for, Duration::from_secs(60));
        let owner = ServerId::from("owner");
        settle.set_diagnostics_owners([owner.clone()]);
        settle.restart_deadline();
        settle.begin(&owner, &json!("indexing"));
        let quiet_started = Instant::now();
        settle.end_at(&owner, &json!("indexing"), quiet_started);

        assert!(
            !settle.should_settle_at(
                (quiet_started + quiet_for)
                    .checked_sub(Duration::from_nanos(1))
                    .unwrap()
            )
        );
        assert!(settle.should_settle_at(quiet_started + quiet_for));
    }

    #[test]
    fn i1_t5_pre_registration_progress_keeps_its_quiet_state() {
        let quiet_for = Duration::from_millis(10);
        let settle = ServerSettle::new(quiet_for, Duration::from_secs(60));
        let owner = ServerId::from("owner");
        settle.begin(&owner, &json!("indexing"));
        let quiet_started = Instant::now();
        settle.end_at(&owner, &json!("indexing"), quiet_started);

        settle.set_diagnostics_owners([owner]);
        settle.restart_deadline();

        assert!(!settle.should_settle_at(quiet_started + quiet_for / 2));
        assert!(settle.should_settle_at(quiet_started + quiet_for * 2));
    }

    #[test]
    fn i1_t5_excluded_progress_does_not_delay_empty_owner_set() {
        let settle = ServerSettle::new(Duration::from_millis(10), Duration::from_secs(60));
        let excluded = ServerId::from("excluded");
        settle.begin(&excluded, &json!("indexing"));
        settle.set_diagnostics_owners(std::iter::empty());
        settle.restart_deadline();

        assert!(settle.should_settle_at(Instant::now()));
    }

    #[test]
    fn i1_t5_excluded_progress_does_not_delay_nonempty_owner_set() {
        let quiet_for = Duration::from_millis(10);
        let settle = ServerSettle::new(quiet_for, Duration::from_secs(60));
        let owner = ServerId::from("owner");
        let excluded = ServerId::from("excluded");
        settle.begin(&owner, &json!("indexing"));
        settle.begin(&excluded, &json!("indexing"));
        let owner_quiet_started = Instant::now();
        settle.end_at(&owner, &json!("indexing"), owner_quiet_started);
        settle.set_diagnostics_owners([owner]);
        settle.restart_deadline();

        assert!(!settle.should_settle_at(owner_quiet_started + quiet_for / 2));
        assert!(settle.should_settle_at(owner_quiet_started + quiet_for));
    }

    #[test]
    fn i1_t5_retiring_one_owner_preserves_another_owner() {
        let quiet_for = Duration::from_millis(10);
        let settle = ServerSettle::new(quiet_for, Duration::from_secs(60));
        let rust = ServerId::from("rust");
        let python = ServerId::from("python");
        settle.set_diagnostics_owners([rust.clone(), python.clone()]);
        settle.restart_deadline();
        settle.begin(&rust, &json!("indexing"));
        settle.begin(&python, &json!("indexing"));
        let python_quiet_started = Instant::now();
        settle.end_at(&python, &json!("indexing"), python_quiet_started);
        settle.forget_server(&rust);

        assert!(settle.should_settle_at(python_quiet_started + quiet_for * 2));
    }
}
