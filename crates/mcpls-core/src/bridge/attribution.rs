//! Which caller wrote which file, and what each one is still owed.
//!
//! These are `DiagnosticsDelivery` methods kept in their own file and
//! attached to `delivery` with `#[path]` rather than made a module of their
//! own, so `super` here is `delivery`, not `bridge`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use super::{
    Caller, DiagnosticSnapshot, DiagnosticsDelivery, FileEntry, OwnedEntry, Ownership, RecordId,
    RetainedFile, SessionId, SeverityFloor,
};

impl DiagnosticsDelivery {
    pub(crate) fn capture_sources(&mut self, sources: HashMap<String, Arc<DiagnosticSnapshot>>) {
        self.sources = sources;
    }

    pub(crate) fn caller(&self, record: RecordId) -> Caller {
        let root = self.roots.get(&record).cloned().flatten();
        Caller { record, root }
    }

    /// Associate a caller's record with its root session when known.
    ///
    /// Registering without a root still records the caller, with its root
    /// left unknown. Routing reads that as an identified caller owed no
    /// root fallback, which is what an unregistered record would get.
    ///
    /// A caller seen before any root owns its files under its own session,
    /// so the first root to arrive carries that ownership over. Only the
    /// first: a later association cannot move a record's history again.
    pub fn register_caller(&mut self, caller: &Caller) {
        let rooted = self
            .roots
            .entry(caller.record.clone())
            .or_default()
            .is_some();
        let Some(root) = caller.root.clone() else {
            return;
        };
        if let RecordId::Session(provisional) = &caller.record
            && !rooted
            && *provisional != root
        {
            self.adopt_ownership(&provisional.clone(), &root);
        }
        self.roots.insert(caller.record.clone(), Some(root));
    }

    /// Re-key everything `provisional` owns onto `root`.
    fn adopt_ownership(&mut self, provisional: &SessionId, root: &SessionId) {
        let claimed: Vec<_> = self
            .ownership
            .keys()
            .filter(|(session, _)| session == provisional)
            .cloned()
            .collect();
        for key in claimed {
            let Some(owner) = self.ownership.remove(&key) else {
                continue;
            };
            let destination = (root.clone(), key.1.clone());
            let merged = match self.ownership.remove(&destination) {
                Some(existing) => self.merge_ownership(&key.1, owner, existing),
                None => owner,
            };
            self.ownership.insert(destination, merged);
        }
    }

    /// Reconcile an adopted owner with one the root already had for `file`.
    ///
    /// An undelivered cycle outranks a delivered one, so a recipient still
    /// owed its report keeps it. Two cycles at the same delivery state are
    /// one cycle with two writers.
    fn merge_ownership(&mut self, file: &str, owner: Ownership, existing: Ownership) -> Ownership {
        if existing.delivered != owner.delivered {
            return if existing.delivered { owner } else { existing };
        }
        let mut merged = owner;
        merged.writers.extend(existing.writers);
        self.restamp_retained(file, existing.generation, merged.generation);
        merged
    }

    /// Point reports already frozen under `from` at `to`, so an
    /// acknowledgment naming the surviving generation still finds them.
    fn restamp_retained(&mut self, file: &str, from: u64, to: u64) {
        for files in self.retained.values_mut() {
            let Some(reports) = files.get_mut(file) else {
                continue;
            };
            for report in reports
                .iter_mut()
                .filter(|report| report.generation == from)
            {
                Arc::make_mut(report).generation = to;
            }
        }
    }

    /// Attribute changed URI keys to the issuing caller.
    pub fn record_write(&mut self, caller: &Caller, keys: &[String]) {
        self.register_caller(caller);
        let root = self
            .roots
            .get(&caller.record)
            .cloned()
            .flatten()
            .unwrap_or_else(|| match &caller.record {
                RecordId::Session(session) | RecordId::ClaudeAgent { root: session, .. } => {
                    session.clone()
                }
            });
        for key in keys {
            let owner = self
                .ownership
                .entry((root.clone(), key.clone()))
                .or_default();
            if owner.generation == 0 || owner.delivered {
                self.next_generation += 1;
                *owner = Ownership {
                    generation: self.next_generation,
                    ..Default::default()
                };
            }
            owner.writers.insert(caller.record.clone());
        }
    }

    pub(crate) fn observe_owned(&mut self, entries: &[FileEntry<'_>]) {
        self.observe_for(None, entries);
    }

    pub(super) fn observe_for(&mut self, recipient: Option<&RecordId>, entries: &[FileEntry<'_>]) {
        for entry in entries {
            let hash = Self::visible_hash(entry.diagnostics, entry.floor);
            let owners = self
                .ownership
                .iter()
                .filter(|((_, key), _)| key == entry.key)
                .map(|((root, _), owner)| (root.clone(), owner.generation, owner.writers.clone()));
            let fallback = self.roots.iter().filter_map(|(record, root)| {
                let root = root.as_ref()?;
                (recipient == Some(record)
                    && *record == RecordId::Session(root.clone())
                    && !self
                        .ownership
                        .contains_key(&(root.clone(), entry.key.to_string())))
                .then(|| {
                    (
                        root.clone(),
                        0,
                        std::collections::HashSet::from([record.clone()]),
                    )
                })
            });
            let recipients: Vec<_> = owners.chain(fallback).collect();
            for (root, generation, writers) in recipients {
                let key = entry.key.to_string();
                self.next_snapshot += 1;
                let snapshot = Arc::new(RetainedFile {
                    id: self.next_snapshot,
                    root: root.clone(),
                    generation,
                    file: OwnedEntry {
                        key: key.clone(),
                        diagnostics: entry.diagnostics.to_vec(),
                        floor: entry.floor,
                    },
                    hash,
                    source: self.sources.get(&key).cloned(),
                });
                for writer in &writers {
                    let committed = self
                        .sessions
                        .get(writer)
                        .or(self.baseline.as_ref())
                        .and_then(|record| record.get(&key))
                        .copied();
                    let queue = self
                        .retained
                        .entry(writer.clone())
                        .or_default()
                        .entry(key.clone())
                        .or_default();
                    let previous = queue.back().map_or(committed, |last| last.hash);
                    if previous != hash {
                        queue.push_back(Arc::clone(&snapshot));
                    }
                }
            }
        }
    }

    pub(super) fn discard_consumed_snapshots(&mut self, record: &RecordId) {
        loop {
            let consumed: Vec<_> = self
                .retained
                .get(record)
                .into_iter()
                .flat_map(|files| files.iter())
                .filter_map(|(key, queue)| {
                    let report = queue.front()?;
                    let committed = self.sessions.get(record)?.get(key).copied();
                    (committed == report.hash).then(|| (key.clone(), report.id))
                })
                .collect();
            if consumed.is_empty() {
                break;
            }
            self.commit_snapshots(record, &consumed);
        }
    }

    pub(super) fn routed_entries(
        &self,
        record: &RecordId,
        entries: &[FileEntry<'_>],
    ) -> Vec<OwnedEntry> {
        let root = self.roots.get(record);
        let mut routed = BTreeMap::new();
        for entry in entries {
            let admitted = match root {
                None => true,
                Some(None) => false,
                Some(Some(root)) => {
                    !self
                        .ownership
                        .contains_key(&(root.clone(), entry.key.to_string()))
                        && *record == RecordId::Session(root.clone())
                }
            };
            if admitted {
                routed.insert(
                    entry.key.to_string(),
                    OwnedEntry {
                        key: entry.key.to_string(),
                        diagnostics: entry.diagnostics.to_vec(),
                        floor: entry.floor,
                    },
                );
            }
        }
        if let Some(files) = self.retained.get(record) {
            for (key, reports) in files {
                if let Some(report) = reports.front() {
                    routed.insert(key.clone(), report.file.clone());
                }
            }
        }
        routed.into_values().collect()
    }

    pub(super) fn commit_snapshots(&mut self, record: &RecordId, snapshots: &[(String, u64)]) {
        let Some(files) = self.retained.get_mut(record) else {
            return;
        };
        for (key, id) in snapshots {
            let Some(queue) = files.get_mut(key) else {
                continue;
            };
            if queue.front().is_none_or(|report| report.id != *id) {
                continue;
            }
            if let Some(report) = queue.pop_front()
                && let Some(owner) = self.ownership.get_mut(&(
                    self.roots
                        .get(record)
                        .cloned()
                        .flatten()
                        .unwrap_or_else(|| report.root.clone()),
                    key.clone(),
                ))
                && owner.generation == report.generation
                && report.file.floor != SeverityFloor::Off
            {
                owner.delivered = true;
            }
        }
        files.retain(|_, queue| !queue.is_empty());
        if files.is_empty() {
            self.retained.remove(record);
        }
    }
}

impl DiagnosticSnapshot {
    pub(crate) fn render(&self, diagnostic: &lsp_types::Diagnostic) -> super::super::Diagnostic {
        use super::super::{
            Diagnostic, DiagnosticSeverity, Position2D, Range, lsp_to_mcp_position,
        };
        let position = |pos: lsp_types::Position| {
            let line = self
                .text
                .as_deref()
                .and_then(|text| text.lines().nth(pos.line as usize));
            let (line, character) = lsp_to_mcp_position(pos, line, self.encoding);
            Position2D { line, character }
        };
        Diagnostic {
            range: Range {
                start: position(diagnostic.range.start),
                end: position(diagnostic.range.end),
            },
            severity: match diagnostic.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
                Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
                Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
                _ => DiagnosticSeverity::Information,
            },
            message: diagnostic.message.clone(),
            code: diagnostic.code.as_ref().map(|code| match code {
                lsp_types::NumberOrString::Number(n) => n.to_string(),
                lsp_types::NumberOrString::String(s) => s.clone(),
            }),
            source: diagnostic.source.clone(),
        }
    }
}
