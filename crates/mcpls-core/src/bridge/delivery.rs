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
    /// How many this file's own cap dropped. Those are not offered again:
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
    /// Files the total cap held back entirely. They are not recorded as
    /// delivered, so the next flush offers them again.
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
    /// The two caps behave differently on purpose. A file truncated by
    /// `max_per_file` is recorded as delivered and carries its own `omitted`
    /// count, because the agent has the file and can look. A file the
    /// `max_total` budget could not fit at all keeps its old record, so the
    /// next flush offers it again rather than losing it silently.
    pub fn flush(&mut self, session: &SessionId, entries: &[FileEntry<'_>]) -> FlushReport {
        let baseline = self.baseline.clone().unwrap_or_default();
        let record = self
            .sessions
            .entry(session.clone())
            .or_insert_with(|| baseline);

        let mut report = FlushReport::default();
        let mut budget = self.config.max_total;

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
                    if budget == 0 {
                        report.omitted += 1;
                        continue;
                    }
                    let mut visible: Vec<_> = entry
                        .diagnostics
                        .iter()
                        .filter(|d| entry.floor.admits(d.severity))
                        .cloned()
                        .collect();
                    let per_file = self.config.max_per_file.min(budget);
                    let omitted = visible.len().saturating_sub(per_file);
                    visible.truncate(per_file);
                    budget -= visible.len();
                    record.insert(entry.key.to_string(), current);
                    report.changed.push(ChangedFile {
                        key: entry.key.to_string(),
                        diagnostics: visible,
                        omitted,
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
}
