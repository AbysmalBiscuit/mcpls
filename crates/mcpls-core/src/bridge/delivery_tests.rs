#![allow(clippy::unwrap_used)]

use lsp_types::DiagnosticSeverity;

use super::tests::{diagnostic, entry};
use super::*;

fn caller(root: &str, agent: &str) -> Caller {
    Caller {
        record: RecordId::Session(SessionId::from(agent.to_string())),
        root: Some(SessionId::from(root.to_string())),
    }
}

#[test]
fn deferred_files_keep_their_snapshot_when_the_cache_disappears() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
        max_total: 1,
        ..Default::default()
    });
    let a = caller("root", "a");
    let b = caller("root", "b");
    for writer in [&a, &b] {
        delivery.record_write(writer, &["a.rs".into(), "b.rs".into()]);
    }
    let errors = vec![diagnostic(0, DiagnosticSeverity::ERROR, "old")];
    let entries = [
        entry("a.rs", &errors, SeverityFloor::Warning),
        entry("b.rs", &errors, SeverityFloor::Warning),
    ];
    let first = delivery.flush(&a.record, &entries);
    assert_eq!(first.changed[0].key, "a.rs");
    assert_eq!(first.omitted, 1);
    assert_eq!(delivery.flush(&a.record, &[]).changed[0].key, "b.rs");
    assert_eq!(delivery.flush(&b.record, &[]).changed[0].key, "a.rs");
    assert_eq!(delivery.flush(&b.record, &[]).changed[0].key, "b.rs");
}

#[test]
fn independent_roots_and_unchanged_transfer_have_separate_histories() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let a = caller("root", "a");
    let b = caller("root", "b");
    let other = caller("other", "other");
    delivery.register_caller(&other);
    delivery.record_write(&a, &["a.rs".into()]);
    let errors = vec![diagnostic(0, DiagnosticSeverity::ERROR, "broken")];
    let entries = [entry("a.rs", &errors, SeverityFloor::Warning)];
    assert_eq!(delivery.flush(&a.record, &entries).changed.len(), 1);
    delivery.record_write(&b, &["a.rs".into()]);
    assert_eq!(delivery.flush(&b.record, &entries).changed.len(), 1);
    assert_eq!(delivery.flush(&other.record, &entries).changed.len(), 1);
    assert!(delivery.flush(&a.record, &entries).changed.is_empty());
}

#[test]
fn a_muted_file_forgets_history_without_reporting_a_clear() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let a = caller("root", "a");
    delivery.record_write(&a, &["a.rs".into()]);
    let errors = vec![diagnostic(0, DiagnosticSeverity::ERROR, "broken")];
    delivery.flush(&a.record, &[entry("a.rs", &errors, SeverityFloor::Warning)]);
    let muted = delivery.flush(&a.record, &[entry("a.rs", &errors, SeverityFloor::Off)]);
    assert!(muted.changed.is_empty());
    assert!(muted.cleared.is_empty());
    assert_eq!(
        delivery
            .flush(&a.record, &[entry("a.rs", &errors, SeverityFloor::Warning)])
            .changed
            .len(),
        1
    );
}

#[test]
fn retained_ranges_use_the_original_document_text() {
    let source = DiagnosticSnapshot {
        uri: "file:///a.rs".parse().unwrap(),
        encoding: super::super::PositionEncoding::Utf8,
        text: Some(Arc::from("😀broken")),
    };
    let mut error = diagnostic(0, DiagnosticSeverity::ERROR, "broken");
    error.range.start.character = 4;
    error.range.end.character = 5;
    let converted = source.render(&error);
    assert_eq!(converted.range.start.character, 3);
    assert_eq!(converted.range.end.character, 4);
}

#[test]
fn each_writer_receives_only_its_file_and_root_gets_unowned_files() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    delivery.set_baseline(HashMap::new());
    let a = caller("root", "a");
    let b = caller("root", "b");
    let root = caller("root", "root");
    for person in [&a, &b, &root] {
        delivery.register_caller(person);
    }
    delivery.record_write(&a, &["a.rs".to_string()]);
    delivery.record_write(&b, &["b.rs".to_string()]);
    let errors = vec![diagnostic(0, DiagnosticSeverity::ERROR, "broken")];
    let entries = [
        entry("a.rs", &errors, SeverityFloor::Warning),
        entry("b.rs", &errors, SeverityFloor::Warning),
        entry("c.rs", &errors, SeverityFloor::Warning),
    ];
    for (person, file) in [(&a, "a.rs"), (&b, "b.rs"), (&root, "c.rs")] {
        let report = delivery.flush(&person.record, &entries);
        assert_eq!(
            report
                .changed
                .iter()
                .map(|f| f.key.as_str())
                .collect::<Vec<_>>(),
            vec![file]
        );
        assert!(delivery.flush(&person.record, &entries).changed.is_empty());
    }
}

#[test]
fn a_late_writer_reads_its_snapshot_after_ownership_and_cache_change() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    delivery.set_baseline(HashMap::new());
    let a = caller("root", "a");
    let b = caller("root", "b");
    let c = caller("root", "c");
    delivery.record_write(&a, &["a.rs".to_string()]);
    delivery.record_write(&b, &["a.rs".to_string()]);
    let old = vec![diagnostic(0, DiagnosticSeverity::ERROR, "old")];
    let entries = [entry("a.rs", &old, SeverityFloor::Warning)];
    let (_, token) = delivery.stage(&b.record, &entries);
    assert_eq!(delivery.flush(&a.record, &entries).changed.len(), 1);
    delivery.record_write(&c, &["a.rs".to_string()]);
    let new = vec![diagnostic(1, DiagnosticSeverity::ERROR, "new")];
    let latest = [entry("a.rs", &new, SeverityFloor::Warning)];
    let report = delivery.flush(&b.record, &latest);
    assert_eq!(report.changed[0].diagnostics[0].message, "old");
    assert!(!delivery.commit(&b.record, token.unwrap()));
    assert_eq!(
        delivery.flush(&c.record, &latest).changed[0].diagnostics[0].message,
        "new"
    );
    assert!(delivery.flush(&a.record, &latest).changed.is_empty());
}

#[test]
fn later_publications_and_clears_stay_with_the_writer() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let a = caller("root", "a");
    let root = caller("root", "root");
    delivery.register_caller(&root);
    delivery.record_write(&a, &["a.rs".to_string()]);
    for message in ["first", "later"] {
        let diagnostics = vec![diagnostic(0, DiagnosticSeverity::ERROR, message)];
        let entries = [entry("a.rs", &diagnostics, SeverityFloor::Warning)];
        assert_eq!(delivery.flush(&a.record, &entries).changed.len(), 1);
        assert!(delivery.flush(&root.record, &entries).changed.is_empty());
    }
    assert_eq!(
        delivery
            .flush(&a.record, &[entry("a.rs", &[], SeverityFloor::Warning)])
            .cleared,
        vec!["a.rs"]
    );
}

#[test]
fn a_new_cycle_survives_an_older_acknowledgment() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let a = caller("root", "a");
    let b = caller("root", "b");
    delivery.record_write(&a, &["a.rs".to_string()]);
    delivery.record_write(&b, &["a.rs".to_string()]);
    let old = vec![diagnostic(0, DiagnosticSeverity::ERROR, "old")];
    let entries = [entry("a.rs", &old, SeverityFloor::Warning)];
    let (_, old_token) = delivery.stage(&b.record, &entries);
    delivery.flush(&a.record, &entries);
    delivery.record_write(&b, &["a.rs".to_string()]);
    assert!(delivery.commit(&b.record, old_token.unwrap()));
    delivery.record_write(&a, &["a.rs".to_string()]);
    let new = vec![diagnostic(1, DiagnosticSeverity::ERROR, "new")];
    let latest = [entry("a.rs", &new, SeverityFloor::Warning)];
    assert_eq!(delivery.flush(&b.record, &latest).changed.len(), 1);
    assert_eq!(delivery.flush(&a.record, &latest).changed.len(), 1);
}
