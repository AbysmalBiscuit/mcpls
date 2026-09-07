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
//! on the debounce like any other quiet workspace rather than waiting out
//! the backstop.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::ServerId;

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
    /// When the outstanding set last became empty. `None` until the first
    /// operation ends, so a process that has not yet heard from a server is
    /// not mistaken for one whose servers have finished.
    quiet_since: Option<Instant>,
    /// When the backstop fires regardless of what the servers have said.
    deadline: Instant,
    /// How many long-running operations have ever begun.
    epoch: u64,
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
                quiet_since: None,
                deadline: Instant::now() + deadline_after,
                epoch: 0,
            }),
            quiet_for,
            deadline_after,
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
    /// This is also where such a server's quiet debounce gets its start:
    /// nothing is outstanding yet at this point, so if nothing has been
    /// stamped either, this counts as the first quiet moment. A server that
    /// begins reporting progress afterwards clears the stamp the same way
    /// any other `begin` does.
    pub fn restart_deadline(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.deadline = Instant::now() + self.deadline_after;
        if state.outstanding.is_empty() && state.quiet_since.is_none() {
            state.quiet_since = Some(Instant::now());
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
        state
            .outstanding
            .insert((server.clone(), token.to_string()));
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
        if !state
            .outstanding
            .remove(&(server.clone(), token.to_string()))
        {
            return;
        }
        if state.outstanding.is_empty() {
            state.quiet_since = Some(now);
        }
    }

    /// Record that `server` finished one.
    pub fn end(&self, server: &ServerId, token: &serde_json::Value) {
        self.end_at(server, token, Instant::now());
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
    /// And a workspace that has never reported any progress counts as
    /// quiet, where the baseline's judgment waits for a first `end`. The
    /// baseline can afford to wait because it has five minutes and one
    /// chance to get the workspace's real state; a footer runs after its own
    /// grace period on every write, and a configured server that reports no
    /// `$/progress` would otherwise make every footer burn its whole cap.
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
    #[must_use]
    pub fn should_settle_at(&self, now: Instant) -> bool {
        let Ok(state) = self.state.lock() else {
            return false;
        };
        if now >= state.deadline {
            return true;
        }
        state.outstanding.is_empty()
            && state
                .quiet_since
                .is_some_and(|since| now.duration_since(since) >= self.quiet_for)
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
    fn test_a_server_that_never_reports_progress_settles_on_the_deadline() {
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
        let quiet_for = Duration::from_millis(10);
        let settle = ServerSettle::new(quiet_for, Duration::from_secs(600));
        settle.restart_deadline();
        let restarted = Instant::now();

        assert!(
            settle.should_settle_at(restarted + quiet_for * 2),
            "a server that never reports progress must settle on the quiet \
             debounce once the servers are spawned, not wait for the \
             five-minute backstop"
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
}
