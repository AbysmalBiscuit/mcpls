//! Per-session deduplication of language server diagnostics.
//!
//! A flush answers one question: what is different since this session last
//! asked? The record is a hash per file rather than a set of individual
//! diagnostics, because when a file breaks, its full current error list is
//! more useful than a delta against a list that has left the context window.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::config::{DiagnosticsConfig, LspServerConfig, ServerId, SeverityFloor};

/// Identity of one client session.
///
/// Claude Code exports `CLAUDE_CODE_SESSION_ID` into the environment of the
/// stdio MCP servers it spawns, so an in-process flush and a flush arriving
/// later over a socket name the same record.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl SessionId {
    /// The session this process serves when the host names none.
    ///
    /// Correct for stdio, where the host spawns one mcpls per client.
    #[must_use]
    pub fn process_default() -> Self {
        std::env::var("CLAUDE_CODE_SESSION_ID").map_or_else(|_| Self("local".to_string()), Self)
    }
}

/// One file's current diagnostics, as the caller assembled them.
#[derive(Debug)]
pub struct FileEntry<'a> {
    /// Cache key identifying the file, stable across publishes.
    pub key: &'a str,
    /// Everything the owning server currently reports for this file.
    pub diagnostics: &'a [lsp_types::Diagnostic],
    /// The floor this file's server answers to.
    pub floor: SeverityFloor,
}

/// One file that changed since the last flush.
#[derive(Debug, Clone)]
pub struct ChangedFile {
    /// Cache key, matching the [`FileEntry`] this came from.
    pub key: String,
    /// Diagnostics at or above the floor, capped.
    pub diagnostics: Vec<lsp_types::Diagnostic>,
    /// How many admitted diagnostics did not make it into `diagnostics`.
    /// Ordinarily this is exactly what `max_per_file` dropped; the
    /// exception is the one flush whose total budget is too small to fit
    /// even this single file, where the budget's own shortfall is folded
    /// in too rather than held back for a later flush. Not offered again:
    /// the count is what tells the agent to look at the file itself.
    pub omitted: usize,
}

/// What one flush found.
#[derive(Debug, Clone, Default)]
pub struct FlushReport {
    /// Files whose visible diagnostics differ from the last flush.
    pub changed: Vec<ChangedFile>,
    /// Cache keys of files that had visible diagnostics and now have none.
    pub cleared: Vec<String>,
    /// Whole files the total budget could not fit this flush. Their
    /// session record is left untouched, so the next flush offers them
    /// again in full.
    pub omitted: usize,
}

/// Per-session records of what has already been delivered.
#[derive(Debug)]
pub struct DiagnosticsDelivery {
    config: DiagnosticsConfig,
    sessions: HashMap<SessionId, HashMap<String, u64>>,
    baseline: Option<HashMap<String, u64>>,
}

impl DiagnosticsDelivery {
    /// Build a delivery core answering to `config`.
    #[must_use]
    pub fn new(config: DiagnosticsConfig) -> Self {
        Self {
            config,
            sessions: HashMap::new(),
            baseline: None,
        }
    }

    /// Adopt `baseline` as what every future session starts out believing.
    ///
    /// Taken once the workspace's servers have gone quiet. Without it the
    /// first flush of a session reports every warning the workspace already
    /// had, which is never what the agent asked for.
    pub fn set_baseline(&mut self, baseline: HashMap<String, u64>) {
        self.baseline = Some(baseline);
    }

    /// Whether a baseline has been adopted yet.
    #[must_use]
    pub const fn has_baseline(&self) -> bool {
        self.baseline.is_some()
    }

    /// Hash one file's visible diagnostics.
    ///
    /// Order-insensitive, because `cap_diagnostics_entry_size` re-sorts
    /// survivors by severity when it truncates and a resort is not a change.
    /// Computed over the set that clears the floor, so raising a hint on a
    /// file whose floor is `error` reports nothing.
    ///
    /// `None` means the file has nothing visible at all.
    #[must_use]
    pub fn visible_hash(
        diagnostics: &[lsp_types::Diagnostic],
        floor: SeverityFloor,
    ) -> Option<u64> {
        let mut parts: Vec<String> = diagnostics
            .iter()
            .filter(|d| floor.admits(d.severity))
            .map(|d| {
                format!(
                    "{}:{}:{}:{}:{:?}:{}",
                    d.range.start.line,
                    d.range.start.character,
                    d.range.end.line,
                    d.range.end.character,
                    d.severity,
                    d.message
                )
            })
            .collect();
        if parts.is_empty() {
            return None;
        }
        parts.sort_unstable();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        parts.hash(&mut hasher);
        Some(hasher.finish())
    }

    /// Report what changed for `session` since its last flush.
    ///
    /// A zero `max_per_file` or `max_total` means that cap is unlimited,
    /// matching `workspace.max_documents`/`max_file_size`'s convention:
    /// there is no other way to write "no limit", and a literal zero cap
    /// has no sensible reading (`severity = "off"` already covers "deliver
    /// nothing"). Running low on a finite total budget defers a whole file
    /// to the next flush rather than truncating it further: a partially
    /// delivered file reads as the complete picture, which is worse than
    /// waiting. The one exception is a file that does not fit even a
    /// fresh, untouched budget — no later flush would do better either, so
    /// that file is delivered truncated to the budget instead of withheld
    /// forever.
    pub fn flush(&mut self, session: &SessionId, entries: &[FileEntry<'_>]) -> FlushReport {
        let record = self
            .sessions
            .entry(session.clone())
            .or_insert_with(|| self.baseline.clone().unwrap_or_default());

        let mut report = FlushReport::default();
        let mut budget = (self.config.max_total > 0).then_some(self.config.max_total);
        let mut delivered_any = false;

        for entry in entries {
            let hash = Self::visible_hash(entry.diagnostics, entry.floor);
            let previous = record.get(entry.key).copied();

            match (hash, previous) {
                (None, Some(_)) => {
                    record.remove(entry.key);
                    report.cleared.push(entry.key.to_string());
                }
                (None, None) => {}
                (Some(current), Some(before)) if current == before => {}
                (Some(current), _) => {
                    let mut visible: Vec<_> = entry
                        .diagnostics
                        .iter()
                        .filter(|d| entry.floor.admits(d.severity))
                        .cloned()
                        .collect();
                    let per_file_omitted = if self.config.max_per_file == 0 {
                        0
                    } else {
                        let dropped = visible.len().saturating_sub(self.config.max_per_file);
                        visible.truncate(self.config.max_per_file);
                        dropped
                    };

                    let budget_omitted = match budget {
                        None => Some(0),
                        Some(remaining) if visible.len() <= remaining => {
                            budget = Some(remaining - visible.len());
                            Some(0)
                        }
                        Some(remaining) if !delivered_any => {
                            let shortfall = visible.len() - remaining;
                            visible.truncate(remaining);
                            budget = Some(0);
                            Some(shortfall)
                        }
                        Some(_) => None,
                    };

                    let Some(budget_omitted) = budget_omitted else {
                        report.omitted += 1;
                        continue;
                    };

                    delivered_any = true;
                    record.insert(entry.key.to_string(), current);
                    report.changed.push(ChangedFile {
                        key: entry.key.to_string(),
                        diagnostics: visible,
                        omitted: per_file_omitted + budget_omitted,
                    });
                }
            }
        }

        report
    }
}

/// The severity floor each server answers to.
///
/// Resolved once at startup, because a server's floor comes from
/// configuration and configuration does not change while the process runs.
#[derive(Debug)]
pub struct FloorTable {
    default: SeverityFloor,
    by_server: HashMap<ServerId, SeverityFloor>,
}

impl FloorTable {
    /// Build the table from a resolved configuration.
    #[must_use]
    pub fn new(config: &DiagnosticsConfig, servers: &[LspServerConfig]) -> Self {
        Self {
            default: config.severity,
            by_server: servers
                .iter()
                .filter_map(|s| s.diagnostics_severity.map(|floor| (s.id(), floor)))
                .collect(),
        }
    }

    /// The floor for `server`.
    #[must_use]
    pub fn for_server(&self, server: &ServerId) -> SeverityFloor {
        self.by_server.get(server).copied().unwrap_or(self.default)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

    use super::*;
    use crate::config::{DiagnosticsConfig, SeverityFloor};

    fn diagnostic(line: u32, severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position::new(line, 0),
                end: Position::new(line, 1),
            },
            severity: Some(severity),
            message: message.to_string(),
            ..Default::default()
        }
    }

    fn entry<'a>(
        key: &'a str,
        diagnostics: &'a [Diagnostic],
        floor: SeverityFloor,
    ) -> FileEntry<'a> {
        FileEntry {
            key,
            diagnostics,
            floor,
        }
    }

    #[test]
    fn test_a_changed_file_is_returned_once_and_not_twice() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];

        let first = delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Warning)]);
        assert_eq!(first.changed.len(), 1);

        let second = delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Warning)]);
        assert!(
            second.changed.is_empty(),
            "nothing changed since the last flush"
        );
    }

    #[test]
    fn test_a_file_losing_its_diagnostics_is_reported_once() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Warning)]);

        let fixed = delivery.flush(&session, &[entry("a.rs", &[], SeverityFloor::Warning)]);
        assert_eq!(fixed.cleared, vec!["a.rs".to_string()]);

        let again = delivery.flush(&session, &[entry("a.rs", &[], SeverityFloor::Warning)]);
        assert!(again.cleared.is_empty(), "cleared is not re-reported");
    }

    #[test]
    fn test_sub_floor_churn_does_not_report_a_file_with_nothing_to_show() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let errors = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        delivery.flush(&session, &[entry("a.rs", &errors, SeverityFloor::Warning)]);

        let mut with_hint = errors.clone();
        with_hint.push(diagnostic(9, DiagnosticSeverity::HINT, "consider"));
        let after = delivery.flush(
            &session,
            &[entry("a.rs", &with_hint, SeverityFloor::Warning)],
        );
        assert!(
            after.changed.is_empty(),
            "the hint is below the floor, so nothing visible changed"
        );
    }

    #[test]
    fn test_the_hash_ignores_publish_order() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let a = diagnostic(1, DiagnosticSeverity::ERROR, "one");
        let b = diagnostic(2, DiagnosticSeverity::WARNING, "two");

        delivery.flush(
            &session,
            &[entry(
                "a.rs",
                &[a.clone(), b.clone()],
                SeverityFloor::Warning,
            )],
        );
        let reordered = delivery.flush(&session, &[entry("a.rs", &[b, a], SeverityFloor::Warning)]);
        assert!(reordered.changed.is_empty(), "order is not content");
    }

    #[test]
    fn test_a_capped_file_says_how_many_it_dropped() {
        let config = DiagnosticsConfig {
            max_per_file: 2,
            ..DiagnosticsConfig::default()
        };
        let mut delivery = DiagnosticsDelivery::new(config);
        let session = SessionId::from("s".to_string());
        let diags: Vec<_> = (0..5)
            .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, "boom"))
            .collect();

        let report = delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Warning)]);
        assert_eq!(report.changed[0].diagnostics.len(), 2);
        assert_eq!(report.changed[0].omitted, 3);
    }

    #[test]
    fn test_a_file_dropped_by_the_total_cap_is_still_pending() {
        let config = DiagnosticsConfig {
            max_total: 1,
            ..DiagnosticsConfig::default()
        };
        let mut delivery = DiagnosticsDelivery::new(config);
        let session = SessionId::from("s".to_string());
        let a = vec![diagnostic(1, DiagnosticSeverity::ERROR, "a")];
        let b = vec![diagnostic(1, DiagnosticSeverity::ERROR, "b")];

        let first = delivery.flush(
            &session,
            &[
                entry("a.rs", &a, SeverityFloor::Warning),
                entry("b.rs", &b, SeverityFloor::Warning),
            ],
        );
        assert_eq!(first.changed.len(), 1);
        assert_eq!(first.omitted, 1);

        let second = delivery.flush(
            &session,
            &[
                entry("a.rs", &a, SeverityFloor::Warning),
                entry("b.rs", &b, SeverityFloor::Warning),
            ],
        );
        assert_eq!(
            second.changed.len(),
            1,
            "the file the cap dropped is delivered next time, not swallowed"
        );
    }

    #[test]
    fn test_a_muted_server_delivers_nothing() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];

        let report = delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Off)]);
        assert!(report.changed.is_empty());
        assert!(report.cleared.is_empty(), "muted is not the same as fixed");
    }

    #[test]
    fn test_two_sessions_deduplicate_independently() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        let one = SessionId::from("one".to_string());
        let two = SessionId::from("two".to_string());

        delivery.flush(&one, &[entry("a.rs", &diags, SeverityFloor::Warning)]);
        let other = delivery.flush(&two, &[entry("a.rs", &diags, SeverityFloor::Warning)]);
        assert_eq!(
            other.changed.len(),
            1,
            "a second session has its own record"
        );
    }

    #[test]
    fn test_a_session_starting_after_the_baseline_ignores_what_it_recorded() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "pre-existing")];

        let baseline = std::collections::HashMap::from([(
            "a.rs".to_string(),
            DiagnosticsDelivery::visible_hash(&diags, SeverityFloor::Warning).unwrap(),
        )]);
        delivery.set_baseline(baseline);

        let report = delivery.flush(
            &SessionId::from("s".to_string()),
            &[entry("a.rs", &diags, SeverityFloor::Warning)],
        );
        assert!(
            report.changed.is_empty(),
            "the workspace already had this before the session started"
        );
    }

    #[test]
    fn test_max_per_file_and_max_total_both_bind_in_one_flush() {
        let config = DiagnosticsConfig {
            max_per_file: 2,
            max_total: 3,
            ..DiagnosticsConfig::default()
        };
        let mut delivery = DiagnosticsDelivery::new(config);
        let session = SessionId::from("s".to_string());
        let a: Vec<_> = (0..4)
            .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, "a"))
            .collect();
        let b: Vec<_> = (0..4)
            .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, "b"))
            .collect();

        let report = delivery.flush(
            &session,
            &[
                entry("a.rs", &a, SeverityFloor::Warning),
                entry("b.rs", &b, SeverityFloor::Warning),
            ],
        );

        assert_eq!(report.changed.len(), 1, "only a.rs fit the total budget");
        assert_eq!(report.changed[0].key, "a.rs");
        assert_eq!(
            report.changed[0].diagnostics.len(),
            2,
            "a.rs is still capped at max_per_file"
        );
        assert_eq!(
            report.changed[0].omitted, 2,
            "a.rs's own omitted count is max_per_file's drop, not the total cap's"
        );
        assert_eq!(report.omitted, 1, "b.rs was deferred whole, not truncated");
    }

    #[test]
    fn test_a_file_deferred_by_the_total_cap_is_offered_whole_next_flush() {
        let config = DiagnosticsConfig {
            max_total: 3,
            ..DiagnosticsConfig::default()
        };
        let mut delivery = DiagnosticsDelivery::new(config);
        let session = SessionId::from("s".to_string());
        let a: Vec<_> = (0..2)
            .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, "a"))
            .collect();
        let b: Vec<_> = (0..3)
            .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, "b"))
            .collect();

        let first = delivery.flush(
            &session,
            &[
                entry("a.rs", &a, SeverityFloor::Warning),
                entry("b.rs", &b, SeverityFloor::Warning),
            ],
        );
        assert_eq!(first.changed.len(), 1);
        assert_eq!(first.changed[0].key, "a.rs");
        assert_eq!(first.omitted, 1, "b.rs did not fit alongside a.rs");

        let second = delivery.flush(
            &session,
            &[
                entry("a.rs", &a, SeverityFloor::Warning),
                entry("b.rs", &b, SeverityFloor::Warning),
            ],
        );
        assert_eq!(second.changed.len(), 1);
        assert_eq!(second.changed[0].key, "b.rs");
        assert_eq!(
            second.changed[0].diagnostics.len(),
            3,
            "b.rs is offered whole once the budget is free again"
        );
        assert_eq!(second.changed[0].omitted, 0);
    }

    #[test]
    fn test_a_file_larger_than_the_total_budget_is_delivered_truncated_once() {
        let config = DiagnosticsConfig {
            max_total: 2,
            ..DiagnosticsConfig::default()
        };
        let mut delivery = DiagnosticsDelivery::new(config);
        let session = SessionId::from("s".to_string());
        let diags: Vec<_> = (0..5)
            .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, "boom"))
            .collect();

        let report = delivery.flush(&session, &[entry("c.rs", &diags, SeverityFloor::Warning)]);

        assert_eq!(
            report.changed.len(),
            1,
            "a file that can never fit the budget is delivered truncated, not deferred forever"
        );
        assert_eq!(report.changed[0].diagnostics.len(), 2);
        assert_eq!(report.changed[0].omitted, 3);
        assert_eq!(report.omitted, 0, "delivered, so not counted as deferred");
    }

    #[test]
    fn test_diagnostics_entirely_below_the_floor_are_reported_as_cleared() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let errors = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        delivery.flush(&session, &[entry("a.rs", &errors, SeverityFloor::Warning)]);

        let hints = vec![diagnostic(9, DiagnosticSeverity::HINT, "consider")];
        let report = delivery.flush(&session, &[entry("a.rs", &hints, SeverityFloor::Warning)]);

        assert_eq!(
            report.cleared,
            vec!["a.rs".to_string()],
            "a non-empty publish with nothing above the floor is still a clear"
        );
    }

    #[test]
    fn test_a_zero_total_cap_means_unlimited_not_zero() {
        let config = DiagnosticsConfig {
            max_total: 0,
            ..DiagnosticsConfig::default()
        };
        let mut delivery = DiagnosticsDelivery::new(config);
        let session = SessionId::from("s".to_string());
        let files: Vec<Vec<Diagnostic>> = (0..6)
            .map(|f| {
                (0..8)
                    .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, "boom"))
                    .map(|mut d| {
                        d.message = format!("f{f}-{}", d.message);
                        d
                    })
                    .collect()
            })
            .collect();
        let keys: Vec<String> = (0..6).map(|f| format!("f{f}.rs")).collect();
        let entries: Vec<FileEntry<'_>> = keys
            .iter()
            .zip(files.iter())
            .map(|(key, diags)| entry(key, diags, SeverityFloor::Warning))
            .collect();

        let report = delivery.flush(&session, &entries);

        assert_eq!(report.changed.len(), 6, "every file was delivered");
        for changed in &report.changed {
            assert_eq!(changed.diagnostics.len(), 8, "delivered whole");
            assert_eq!(changed.omitted, 0);
        }
        assert_eq!(
            report.omitted, 0,
            "nothing deferred under an unlimited budget"
        );
    }

    #[test]
    fn test_a_zero_per_file_cap_means_unlimited_not_zero() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
            max_per_file: 0,
            ..DiagnosticsConfig::default()
        });
        let session = SessionId::from("s".to_string());
        let diags: Vec<_> = (0..15)
            .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, "boom"))
            .collect();

        let report = delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Warning)]);

        assert_eq!(report.changed[0].diagnostics.len(), 15, "delivered whole");
        assert_eq!(report.changed[0].omitted, 0);
    }

    #[test]
    fn test_both_caps_zero_together_deliver_everything() {
        let config = DiagnosticsConfig {
            max_per_file: 0,
            max_total: 0,
            ..DiagnosticsConfig::default()
        };
        let mut delivery = DiagnosticsDelivery::new(config);
        let session = SessionId::from("s".to_string());
        let a: Vec<_> = (0..30)
            .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, "a"))
            .collect();
        let b: Vec<_> = (0..30)
            .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, "b"))
            .collect();

        let report = delivery.flush(
            &session,
            &[
                entry("a.rs", &a, SeverityFloor::Warning),
                entry("b.rs", &b, SeverityFloor::Warning),
            ],
        );

        assert_eq!(report.changed.len(), 2);
        assert_eq!(report.changed[0].diagnostics.len(), 30);
        assert_eq!(report.changed[0].omitted, 0);
        assert_eq!(report.changed[1].diagnostics.len(), 30);
        assert_eq!(report.changed[1].omitted, 0);
        assert_eq!(report.omitted, 0);
    }

    #[test]
    fn test_a_zero_total_cap_still_deduplicates_across_flushes() {
        let config = DiagnosticsConfig {
            max_total: 0,
            ..DiagnosticsConfig::default()
        };
        let mut delivery = DiagnosticsDelivery::new(config);
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];

        let first = delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Warning)]);
        assert_eq!(
            first.changed[0].diagnostics.len(),
            1,
            "an unlimited budget delivers the diagnostic, not an empty list"
        );

        let second = delivery.flush(&session, &[entry("a.rs", &diags, SeverityFloor::Warning)]);
        assert!(
            second.changed.is_empty(),
            "the second flush is unchanged because the first one actually delivered \
             the diagnostic, not because it was silently swallowed"
        );
    }
}
