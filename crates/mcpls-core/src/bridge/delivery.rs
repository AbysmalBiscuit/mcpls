//! Per-session deduplication of language server diagnostics.
//!
//! A flush answers one question: what is different since this session was
//! last confirmed to have been told? The record is a hash per file rather
//! than a set of individual diagnostics, because when a file breaks, its
//! full current error list is more useful than a delta against a list that
//! has left the context window.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::config::{DiagnosticsConfig, LspServerConfig, ServerId, SeverityFloor};

/// Identity of one client session.
///
/// A host that names its session names it for every connection it opens,
/// so a hook and the agent's own tool call read one record. A connection
/// whose host names nothing reads a record of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl SessionId {
    /// The session a host named, or `None` when the value is absent or
    /// empty.
    ///
    #[must_use]
    pub fn named(value: Option<String>) -> Option<Self> {
        value.filter(|id| !id.is_empty()).map(Self)
    }

    /// The record a connection whose host named no session reads.
    #[must_use]
    pub fn for_connection(connection: ConnectionId) -> Self {
        Self(connection.to_string())
    }

    /// The session Claude Code exported to this process.
    ///
    /// Only a process facing the host reads this. A backend serves every
    /// session and learns each one from its connection's handshake.
    #[must_use]
    pub fn from_host_env() -> Option<Self> {
        Self::named(std::env::var("CLAUDE_CODE_SESSION_ID").ok())
    }
}

/// One MCP connection to this process, numbered in the order this process
/// created them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// A number no other connection in this process has.
    #[must_use]
    pub fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "connection-{}", self.0)
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
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

/// The record changes one staged report implies, held back until the
/// reader confirms the answer carrying that report reached it.
#[derive(Debug)]
struct PendingFlush {
    token: u64,
    /// `Some(hash)` records the file as delivered at that hash; `None`
    /// forgets it, for a file that cleared or was muted.
    updates: Vec<(String, Option<u64>)>,
}

/// Per-session records of what has already been delivered.
#[derive(Debug)]
pub struct DiagnosticsDelivery {
    config: DiagnosticsConfig,
    sessions: HashMap<SessionId, HashMap<String, u64>>,
    baseline: Option<HashMap<String, u64>>,
    /// At most one staged report per session. Never names a session
    /// `sessions` lacks: `stage` seeds the record before it stages, and
    /// `end_session` drops both.
    pending: HashMap<SessionId, PendingFlush>,
    next_token: u64,
}

impl DiagnosticsDelivery {
    /// Build a delivery core answering to `config`.
    #[must_use]
    pub fn new(config: DiagnosticsConfig) -> Self {
        Self {
            config,
            sessions: HashMap::new(),
            baseline: None,
            pending: HashMap::new(),
            next_token: 0,
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

    /// Drop `session`'s record, so a later flush for the same id starts
    /// from the baseline again.
    pub fn end_session(&mut self, session: &SessionId) {
        self.sessions.remove(session);
        self.pending.remove(session);
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

    /// Report what changed for `session` since its last committed flush,
    /// and stage the record changes that report implies.
    ///
    /// The record does not move here. It moves in [`Self::commit`], once
    /// the reader confirms the answer carrying this report reached it, so
    /// a reader that gives up before that answer is in hand, or dies
    /// holding it without confirming, leaves the record where it was and
    /// the next `stage` offers the same report again. What comes back is a confirmation of the
    /// answer and not of its files: a caller whose rendering of the report
    /// drops a file still commits that file's hash. The token is `None`
    /// when the report implies no record change, which is also when there
    /// is nothing for a reader to acknowledge. A stage replaces whatever
    /// the session had staged before: a report nobody has confirmed is
    /// superseded by the newer one, and an acknowledgement for the old one
    /// then commits nothing.
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
    pub fn stage(
        &mut self,
        session: &SessionId,
        entries: &[FileEntry<'_>],
    ) -> (FlushReport, Option<u64>) {
        let (report, updates) = self.diff(session, entries);
        if updates.is_empty() {
            self.pending.remove(session);
            return (report, None);
        }
        self.next_token += 1;
        let token = self.next_token;
        self.pending
            .insert(session.clone(), PendingFlush { token, updates });
        (report, Some(token))
    }

    /// Apply the record changes staged under `token`.
    ///
    /// `false`, and nothing written, when `token` is not the session's
    /// staged report: a later stage replaced it, an immediate flush
    /// superseded it, or the session ended. The record then already
    /// reflects something a reader was sent more recently, or nothing.
    pub fn commit(&mut self, session: &SessionId, token: u64) -> bool {
        let staged = match self.pending.get(session) {
            Some(pending) if pending.token == token => self.pending.remove(session),
            _ => None,
        };
        let Some(PendingFlush { updates, .. }) = staged else {
            return false;
        };
        let record = self.sessions.entry(session.clone()).or_default();
        for (key, hash) in updates {
            match hash {
                Some(hash) => {
                    record.insert(key, hash);
                }
                None => {
                    record.remove(&key);
                }
            }
        }
        true
    }

    /// [`Self::stage`] and [`Self::commit`] in one call, for a reader whose
    /// answer either arrives or ends the session: the MCP tool and the
    /// footer, whose transport is the session's own.
    pub fn flush(&mut self, session: &SessionId, entries: &[FileEntry<'_>]) -> FlushReport {
        let (report, token) = self.stage(session, entries);
        if let Some(token) = token {
            self.commit(session, token);
        }
        report
    }

    /// One pass over `entries` against `session`'s committed record: the
    /// report, and the record writes it implies, in the order `entries`
    /// gives them.
    fn diff(
        &mut self,
        session: &SessionId,
        entries: &[FileEntry<'_>],
    ) -> (FlushReport, Vec<(String, Option<u64>)>) {
        let record = &*self
            .sessions
            .entry(session.clone())
            .or_insert_with(|| self.baseline.clone().unwrap_or_default());

        let mut report = FlushReport::default();
        let mut updates = Vec::new();
        let mut budget = (self.config.max_total > 0).then_some(self.config.max_total);

        for entry in entries {
            let hash = Self::visible_hash(entry.diagnostics, entry.floor);
            let previous = record.get(entry.key).copied();

            match (hash, previous) {
                (None, Some(_)) if entry.floor == SeverityFloor::Off => {
                    // Muted, not fixed. Forgetting the entry without
                    // reporting means the file starts fresh if its floor
                    // ever rises again, and the agent is not told its
                    // problems are gone when they were only silenced.
                    updates.push((entry.key.to_string(), None));
                }
                (None, Some(_)) => {
                    if budget == Some(0) {
                        // Leave the record in place so the next flush
                        // offers this file again, the same deferral a
                        // changed file gets.
                        report.omitted += 1;
                    } else {
                        if let Some(remaining) = budget.as_mut() {
                            *remaining -= 1;
                        }
                        updates.push((entry.key.to_string(), None));
                        report.cleared.push(entry.key.to_string());
                    }
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
                        // The budget is untouched and this file still does
                        // not fit it, so no later flush offers more of it
                        // and withholding it withholds it forever.
                        // Truncated to the whole budget is the best any
                        // flush can do.
                        Some(remaining) if remaining == self.config.max_total => {
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

                    updates.push((entry.key.to_string(), Some(current)));
                    report.changed.push(ChangedFile {
                        key: entry.key.to_string(),
                        diagnostics: visible,
                        omitted: per_file_omitted + budget_omitted,
                    });
                }
            }
        }

        (report, updates)
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

    use super::*;
    use crate::config::{DiagnosticsConfig, SeverityFloor};

    #[test]
    fn test_a_named_session_needs_a_nonempty_value() {
        assert_eq!(
            SessionId::named(Some("abc-123".to_string())),
            Some(SessionId::from("abc-123".to_string()))
        );
        assert_eq!(SessionId::named(Some(String::new())), None);
        assert_eq!(SessionId::named(None), None);
    }

    #[test]
    fn test_each_connection_gets_its_own_fallback_session() {
        let first = ConnectionId::next();
        let second = ConnectionId::next();
        assert_ne!(first, second);
        assert_ne!(
            SessionId::for_connection(first),
            SessionId::for_connection(second)
        );
    }

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
    fn test_a_staged_report_is_offered_again_until_it_is_acknowledged() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        let entries = [entry("a.rs", &diags, SeverityFloor::Warning)];

        let (first, _) = delivery.stage(&session, &entries);
        assert_eq!(first.changed.len(), 1);

        let (second, token) = delivery.stage(&session, &entries);
        assert_eq!(
            second.changed.len(),
            1,
            "nothing confirmed the first report reached its reader, so it is \
             offered again rather than marked delivered"
        );

        assert!(delivery.commit(
            &session,
            token.expect("a report with content carries a token")
        ));

        let (third, token) = delivery.stage(&session, &entries);
        assert!(
            third.changed.is_empty(),
            "the acknowledged report is not offered again"
        );
        assert_eq!(
            token, None,
            "nothing to commit means nothing to acknowledge"
        );
    }

    #[test]
    fn test_an_acknowledgement_for_a_replaced_report_commits_nothing() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let one = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        let two = vec![
            diagnostic(1, DiagnosticSeverity::ERROR, "boom"),
            diagnostic(2, DiagnosticSeverity::ERROR, "bang"),
        ];

        let (_, stale) = delivery.stage(&session, &[entry("a.rs", &one, SeverityFloor::Warning)]);
        let (_, current) = delivery.stage(&session, &[entry("a.rs", &two, SeverityFloor::Warning)]);

        assert!(
            !delivery.commit(&session, stale.expect("token")),
            "a later report replaced this one; committing it would record a \
             hash its reader was never sent"
        );
        assert!(delivery.commit(&session, current.expect("token")));

        let (after_two, _) =
            delivery.stage(&session, &[entry("a.rs", &two, SeverityFloor::Warning)]);
        assert!(
            after_two.changed.is_empty(),
            "the record holds the acknowledged report's hash"
        );
        let (after_one, _) =
            delivery.stage(&session, &[entry("a.rs", &one, SeverityFloor::Warning)]);
        assert_eq!(
            after_one.changed.len(),
            1,
            "and not the replaced report's hash"
        );
    }

    #[test]
    fn test_a_stage_with_nothing_to_report_drops_the_older_staged_report() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let broken = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];

        let (first, token) =
            delivery.stage(&session, &[entry("a.rs", &broken, SeverityFloor::Warning)]);
        assert_eq!(first.changed.len(), 1);

        let (fixed, none) = delivery.stage(&session, &[entry("a.rs", &[], SeverityFloor::Warning)]);
        assert!(
            fixed.changed.is_empty() && fixed.cleared.is_empty(),
            "the file broke and was fixed before the record ever took its \
             hash, so there is nothing to report either way"
        );
        assert_eq!(none, None);

        assert!(
            !delivery.commit(&session, token.expect("token")),
            "the report that token named is no longer the session's staged one"
        );

        let (again, _) =
            delivery.stage(&session, &[entry("a.rs", &broken, SeverityFloor::Warning)]);
        assert_eq!(
            again.changed.len(),
            1,
            "committing the older report would have recorded a hash no reader \
             was ever confirmed to have, and the next flush would then say the \
             file is clean rather than broken"
        );
    }

    #[test]
    fn test_an_immediate_flush_supersedes_a_staged_report() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        let entries = [entry("a.rs", &diags, SeverityFloor::Warning)];

        let (_, staged) = delivery.stage(&session, &entries);
        let now = delivery.flush(&session, &entries);
        assert_eq!(
            now.changed.len(),
            1,
            "the tool door reports what the hook door has not yet confirmed"
        );

        assert!(
            !delivery.commit(&session, staged.expect("token")),
            "the flush already advanced the record, so a late acknowledgement \
             has nothing left to apply"
        );

        let (after, token) = delivery.stage(&session, &entries);
        assert!(after.changed.is_empty());
        assert_eq!(token, None);
    }

    #[test]
    fn test_ending_a_session_drops_its_staged_report() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        let entries = [entry("a.rs", &diags, SeverityFloor::Warning)];

        let (_, token) = delivery.stage(&session, &entries);
        delivery.end_session(&session);

        assert!(!delivery.commit(&session, token.expect("token")));
        let (again, _) = delivery.stage(&session, &entries);
        assert_eq!(
            again.changed.len(),
            1,
            "a session that starts over starts from the baseline, not from a \
             report its previous life never confirmed"
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
    fn test_an_unlimited_budget_still_applies_the_per_file_cap() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
            max_total: 0,
            max_per_file: 3,
            ..DiagnosticsConfig::default()
        });
        let session = SessionId::from("s".to_string());
        let files: Vec<Vec<Diagnostic>> = (0..3)
            .map(|f| {
                (0..5)
                    .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, &format!("f{f}-{i}")))
                    .collect()
            })
            .collect();
        let keys: Vec<String> = (0..3).map(|f| format!("f{f}.rs")).collect();
        let entries: Vec<FileEntry<'_>> = keys
            .iter()
            .zip(files.iter())
            .map(|(key, diags)| entry(key, diags, SeverityFloor::Warning))
            .collect();

        let report = delivery.flush(&session, &entries);

        assert_eq!(
            report.changed.len(),
            3,
            "an unlimited budget defers nothing"
        );
        for changed in &report.changed {
            assert_eq!(changed.diagnostics.len(), 3, "the per-file cap still bites");
            assert_eq!(changed.omitted, 2, "and its drops are counted");
        }
        assert_eq!(report.omitted, 0, "no file was deferred for budget");
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

    #[test]
    fn test_cleared_files_spend_the_total_budget() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
            max_total: 2,
            ..DiagnosticsConfig::default()
        });
        let session = SessionId::from("s".to_string());
        let broken = vec![diagnostic(0, DiagnosticSeverity::ERROR, "boom")];

        // Seed all four records two at a time, so each seeding flush fits the
        // budget of two and every key has a recorded hash to clear against.
        delivery.flush(
            &session,
            &[
                entry("a", &broken, SeverityFloor::Warning),
                entry("b", &broken, SeverityFloor::Warning),
            ],
        );
        delivery.flush(
            &session,
            &[
                entry("c", &broken, SeverityFloor::Warning),
                entry("d", &broken, SeverityFloor::Warning),
            ],
        );

        // Now all four are fixed at once, under a budget of two.
        let report = delivery.flush(
            &session,
            &[
                entry("a", &[], SeverityFloor::Warning),
                entry("b", &[], SeverityFloor::Warning),
                entry("c", &[], SeverityFloor::Warning),
                entry("d", &[], SeverityFloor::Warning),
            ],
        );

        assert_eq!(
            report.cleared,
            vec!["a".to_string(), "b".to_string()],
            "max_total is one shared context budget and a cleared line spends \
             from it like any other; a workspace-wide fix could otherwise emit \
             up to a thousand of them. The pass is key-ordered, so which two \
             land is reproducible"
        );
        assert_eq!(report.omitted, 2);
    }

    #[test]
    fn test_a_deferred_cleared_file_is_offered_again() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
            max_total: 2,
            ..DiagnosticsConfig::default()
        });
        let session = SessionId::from("s".to_string());
        let broken = vec![diagnostic(0, DiagnosticSeverity::ERROR, "boom")];

        delivery.flush(
            &session,
            &[
                entry("a", &broken, SeverityFloor::Warning),
                entry("b", &broken, SeverityFloor::Warning),
            ],
        );
        delivery.flush(
            &session,
            &[
                entry("c", &broken, SeverityFloor::Warning),
                entry("d", &broken, SeverityFloor::Warning),
            ],
        );

        let all_clean = [
            entry("a", &[], SeverityFloor::Warning),
            entry("b", &[], SeverityFloor::Warning),
            entry("c", &[], SeverityFloor::Warning),
            entry("d", &[], SeverityFloor::Warning),
        ];
        let first = delivery.flush(&session, &all_clean);
        let second = delivery.flush(&session, &all_clean);

        assert_eq!(
            (first.cleared.len(), second.cleared.len()),
            (2, 2),
            "the budget splits the four across two flushes; clearing all four \
             in one would satisfy the union below while spending no budget"
        );

        let mut seen: Vec<String> = first.cleared;
        seen.extend(second.cleared);
        seen.sort();
        assert_eq!(
            seen,
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string()
            ],
            "a deferred cleared file keeps its record entry, which is the same \
             deferral rule a deferred changed file already follows"
        );
    }

    #[test]
    fn test_a_muted_file_is_dropped_from_the_record_rather_than_reported_fixed() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let broken = vec![diagnostic(0, DiagnosticSeverity::ERROR, "boom")];

        let _ = delivery.flush(&session, &[entry("a", &broken, SeverityFloor::Error)]);
        let report = delivery.flush(&session, &[entry("a", &broken, SeverityFloor::Off)]);

        assert!(
            report.cleared.is_empty(),
            "the file still has an error; only the floor changed, and telling the \
             agent its problems are gone is a lie"
        );
        assert!(report.changed.is_empty());
    }

    #[test]
    fn test_a_clear_that_empties_the_budget_defers_the_next_file_whole() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
            max_total: 1,
            ..DiagnosticsConfig::default()
        });
        let session = SessionId::from("s".to_string());
        let broken = vec![diagnostic(0, DiagnosticSeverity::ERROR, "boom")];

        delivery.flush(&session, &[entry("a.rs", &broken, SeverityFloor::Warning)]);

        let report = delivery.flush(
            &session,
            &[
                entry("a.rs", &[], SeverityFloor::Warning),
                entry("b.rs", &broken, SeverityFloor::Warning),
            ],
        );
        assert_eq!(report.cleared, vec!["a.rs".to_string()]);
        assert!(
            report.changed.is_empty(),
            "the clear spent the whole budget, so b.rs waits for a flush that \
             can carry it rather than being sent empty"
        );

        let next = delivery.flush(&session, &[entry("b.rs", &broken, SeverityFloor::Warning)]);
        assert_eq!(
            next.changed.len(),
            1,
            "a deferred file is offered again, and the record must not claim it \
             was already delivered"
        );
        assert_eq!(next.changed[0].diagnostics.len(), 1);
        assert_eq!(next.changed[0].omitted, 0);
    }

    #[test]
    fn test_a_partly_spent_budget_defers_rather_than_truncating() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
            max_total: 2,
            ..DiagnosticsConfig::default()
        });
        let session = SessionId::from("s".to_string());
        let one = vec![diagnostic(0, DiagnosticSeverity::ERROR, "boom")];
        let five: Vec<_> = (1..6)
            .map(|i| diagnostic(i, DiagnosticSeverity::ERROR, "boom"))
            .collect();

        let first = delivery.flush(
            &session,
            &[
                entry("a.rs", &one, SeverityFloor::Warning),
                entry("b.rs", &five, SeverityFloor::Warning),
            ],
        );
        assert_eq!(first.changed.len(), 1, "a.rs fits and is delivered");
        assert_eq!(first.changed[0].key, "a.rs");
        assert_eq!(
            first.omitted, 1,
            "a.rs spent one of the two, so what is left shows less of b.rs \
             than a fresh budget would; b.rs waits rather than being cut to a \
             fifth of itself and recorded as delivered"
        );

        let second = delivery.flush(&session, &[entry("b.rs", &five, SeverityFloor::Warning)]);
        assert_eq!(second.changed.len(), 1);
        assert_eq!(
            second.changed[0].diagnostics.len(),
            2,
            "the whole budget is the most any flush can offer this file"
        );
        assert_eq!(second.changed[0].omitted, 3);
    }

    #[test]
    fn test_a_genuinely_fixed_file_is_still_reported_cleared() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let broken = vec![diagnostic(0, DiagnosticSeverity::ERROR, "boom")];

        let _ = delivery.flush(&session, &[entry("a", &broken, SeverityFloor::Error)]);
        let report = delivery.flush(&session, &[entry("a", &[], SeverityFloor::Error)]);

        assert_eq!(report.cleared, vec!["a".to_string()]);
    }
}
