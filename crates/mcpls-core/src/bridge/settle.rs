//! Tracks whether the language servers have finished their startup work.
//!
//! Every server reports long-running work through `$/progress`. Quiet is not
//! the first moment the outstanding count reaches zero: rust-analyzer's
//! startup phases hand off to each other through gaps of about 70 to 100
//! milliseconds, and the first of those gaps arrives before indexing has
//! begun. Quiet is a count of zero that has held for `quiet_for`.
//!
//! A deadline bounds two failure modes neither the count nor the debounce
//! can see: a notification dropped from a full channel before its pump
//! existed, and a server that reports no progress at all and so never stops
//! being busy for the first time.

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
    /// server that never reports progress produces no event to hang this on,
    /// and the backstop is precisely what covers that case.
    pub fn restart_deadline(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.deadline = Instant::now() + self.deadline_after;
    }

    /// Record that `server` started a long-running operation.
    pub fn begin(&self, server: &ServerId, token: &serde_json::Value) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state
            .outstanding
            .insert((server.clone(), token.to_string()));
        state.quiet_since = None;
    }

    /// Record that `server` finished one.
    pub fn end(&self, server: &ServerId, token: &serde_json::Value) {
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
            state.quiet_since = Some(Instant::now());
        }
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
}
