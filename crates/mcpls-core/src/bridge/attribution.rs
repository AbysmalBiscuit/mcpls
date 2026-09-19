use super::{
    Arc, BTreeMap, Caller, DiagnosticSnapshot, DiagnosticsDelivery, FileEntry, HashMap, OwnedEntry,
    RecordId, RetainedFile, SeverityFloor,
};

impl DiagnosticsDelivery {
    pub(crate) fn capture_sources(&mut self, sources: HashMap<String, Arc<DiagnosticSnapshot>>) {
        self.sources = sources;
    }

    /// Associate a caller's record with its root session when known.
    pub fn register_caller(&mut self, caller: &Caller) {
        let root = self.roots.entry(caller.record.clone()).or_default();
        if caller.root.is_some() {
            root.clone_from(&caller.root);
        }
    }

    /// Attribute changed URI keys to the issuing caller.
    pub fn record_write(&mut self, caller: &Caller, keys: &[String]) {
        self.register_caller(caller);
        let Some(root) = self.roots.get(&caller.record).cloned().flatten() else {
            return;
        };
        for key in keys {
            let owner = self
                .ownership
                .entry((root.clone(), key.clone()))
                .or_default();
            if owner.delivered {
                owner.generation += 1;
                owner.writers.clear();
                owner.delivered = false;
            }
            owner.writers.insert(caller.record.clone());
        }
    }

    pub(super) fn observe_owned(&mut self, entries: &[FileEntry<'_>]) {
        for entry in entries {
            let hash = Self::visible_hash(entry.diagnostics, entry.floor);
            for ((root, key), owner) in &self.ownership {
                if key != entry.key {
                    continue;
                }
                self.next_snapshot += 1;
                let snapshot = Arc::new(RetainedFile {
                    id: self.next_snapshot,
                    root: root.clone(),
                    generation: owner.generation,
                    file: OwnedEntry {
                        key: key.clone(),
                        diagnostics: entry.diagnostics.to_vec(),
                        floor: entry.floor,
                    },
                    hash,
                    source: self.sources.get(key).cloned(),
                });
                for writer in &owner.writers {
                    let committed = self
                        .sessions
                        .get(writer)
                        .or(self.baseline.as_ref())
                        .and_then(|record| record.get(key))
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
                && let Some(owner) = self.ownership.get_mut(&(report.root.clone(), key.clone()))
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
