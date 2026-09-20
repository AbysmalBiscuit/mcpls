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
fn adopted_history_does_not_block_newer_retained_reports() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let child = caller("root", "child");
    let anonymous = SessionId::from("anonymous".to_string());
    let old = vec![diagnostic(0, DiagnosticSeverity::ERROR, "old")];
    let new = vec![diagnostic(0, DiagnosticSeverity::ERROR, "new")];
    let entries = [entry("a.rs", &old, SeverityFloor::Warning)];
    delivery.flush(&anonymous, &entries);
    delivery.record_write(&child, &["a.rs".into()]);
    delivery.stage(&child.record, &entries);
    delivery.merge_session(&anonymous, &child.record);
    let report = delivery.flush(
        &child.record,
        &[entry("a.rs", &new, SeverityFloor::Warning)],
    );
    assert_eq!(report.changed.len(), 1);
    assert_eq!(report.changed[0].diagnostics[0].message, "new");
}

#[test]
fn expired_cycles_cannot_be_acknowledged_as_replacement_cycles() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let first = caller("root", "a");
    let pending = caller("root", "b");
    let expired = caller("root", "c");
    let current = caller("root", "d");
    let joining = caller("root", "e");
    let old = vec![diagnostic(0, DiagnosticSeverity::ERROR, "old")];
    let new = vec![diagnostic(0, DiagnosticSeverity::ERROR, "new")];
    let entries = [entry("first.rs", &old, SeverityFloor::Warning)];
    for writer in [&first, &pending] {
        delivery.record_write(writer, &["first.rs".into()]);
    }
    delivery.flush(&first.record, &entries);
    let (_, token) = delivery.stage(&pending.record, &entries);
    delivery.record_write(&expired, &["first.rs".into()]);
    delivery.end_session(&expired.record);
    delivery.record_write(&current, &["first.rs".into()]);
    assert!(delivery.commit(&pending.record, token.unwrap()));
    delivery.record_write(&joining, &["first.rs".into()]);
    assert_eq!(
        delivery
            .flush(
                &current.record,
                &[entry("first.rs", &new, SeverityFloor::Warning)]
            )
            .changed
            .len(),
        1
    );
}

#[test]
fn late_root_resolution_preserves_an_undelivered_write_cycle() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let a = caller("root", "a");
    let mut b = caller("root", "b");
    let c = caller("root", "c");
    let old = vec![diagnostic(0, DiagnosticSeverity::ERROR, "old")];
    let new = vec![diagnostic(0, DiagnosticSeverity::ERROR, "new")];
    delivery.record_write(&a, &["a.rs".into()]);
    delivery.flush(&a.record, &[entry("a.rs", &old, SeverityFloor::Warning)]);
    b.root = None;
    delivery.record_write(&b, &["a.rs".into()]);
    b.root = a.root.clone();
    delivery.register_caller(&b);
    delivery.record_write(&c, &["a.rs".into()]);
    assert_eq!(
        delivery
            .flush(&b.record, &[entry("a.rs", &new, SeverityFloor::Warning)])
            .changed
            .len(),
        1
    );
}

#[test]
fn root_fallback_survives_child_ownership_and_output_caps() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
        max_total: 1,
        ..Default::default()
    });
    let root = caller("root", "root");
    let child = caller("root", "child");
    delivery.register_caller(&root);
    let old = vec![diagnostic(0, DiagnosticSeverity::ERROR, "old")];
    let entries = [
        entry("a.rs", &old, SeverityFloor::Warning),
        entry("b.rs", &old, SeverityFloor::Warning),
    ];
    let (first, _) = delivery.stage(&root.record, &entries);
    assert_eq!(first.changed[0].key, "a.rs");
    delivery.record_write(&child, &["a.rs".into(), "b.rs".into()]);
    let retry = delivery.flush(&root.record, &[]);
    assert_eq!(retry.changed.len(), 1);
    assert_eq!(retry.changed[0].key, "a.rs");
    assert_eq!(delivery.flush(&root.record, &[]).changed[0].key, "b.rs");
}

#[tokio::test(start_paused = true)]
async fn expiry_worker_rechecks_renewed_hook_deadlines() {
    let delivery = Arc::new(tokio::sync::Mutex::new(DiagnosticsDelivery::new(
        DiagnosticsConfig::default(),
    )));
    let (cancel, rx) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(DiagnosticsDelivery::run_expiry(Arc::clone(&delivery), rx));
    let child = caller("root", "child");
    delivery
        .lock()
        .await
        .touch_hook(&child, tokio::time::Instant::now());
    delivery.lock().await.record_write(&child, &["a.rs".into()]);
    let errors = vec![diagnostic(0, DiagnosticSeverity::ERROR, "broken")];
    let (_, token) = delivery.lock().await.stage(
        &child.record,
        &[entry("a.rs", &errors, SeverityFloor::Warning)],
    );
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(59)).await;
    delivery
        .lock()
        .await
        .touch_hook(&child, tokio::time::Instant::now());
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert!(delivery.lock().await.pending.contains_key(&child.record));
    tokio::time::advance(std::time::Duration::from_secs(59)).await;
    tokio::task::yield_now().await;
    assert!(!delivery.lock().await.commit(&child.record, token.unwrap()));
    cancel.send(true).unwrap();
    worker.await.unwrap();
}

#[test]
fn a_root_arriving_after_a_hook_protects_it_and_zero_grace_expires_on_close() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
        record_grace_ms: 0,
        ..Default::default()
    });
    let root = caller("root", "root");
    let child = caller("root", "child");
    let t = tokio::time::Instant::now();
    delivery.touch_hook(&child, t);
    delivery.record_write(&child, &["a.rs".into()]);
    let connection = ConnectionId::next();
    delivery.attach(connection, &root);
    delivery.expire(t + std::time::Duration::from_secs(100));
    assert!(delivery.roots.contains_key(&child.record));
    delivery.close(connection, t + std::time::Duration::from_secs(100));
    assert!(!delivery.roots.contains_key(&child.record));
    assert!(delivery.ownership.is_empty());
}

#[test]
fn root_connections_and_the_child_connection_protect_pending_tokens() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let root = caller("root", "root");
    let child = caller("root", "child");
    let one = ConnectionId::next();
    let two = ConnectionId::next();
    let own = ConnectionId::next();
    let now = tokio::time::Instant::now();
    delivery.attach(one, &root);
    delivery.attach(two, &root);
    delivery.touch_hook(&child, now);
    delivery.record_write(&child, &["a.rs".into()]);
    let errors = vec![diagnostic(0, DiagnosticSeverity::ERROR, "broken")];
    let (_, token) = delivery.stage(
        &child.record,
        &[entry("a.rs", &errors, SeverityFloor::Warning)],
    );
    delivery.end_root(root.root.as_ref().unwrap(), now);
    delivery.close(one, now);
    assert!(delivery.pending.contains_key(&child.record));
    delivery.attach(own, &child);
    delivery.close(two, now);
    assert!(delivery.pending.contains_key(&child.record));
    delivery.close(own, now);
    assert!(!delivery.commit(&child.record, token.unwrap()));
    assert!(!delivery.roots.contains_key(&child.record));
    assert!(delivery.ownership.is_empty());
}

#[test]
fn hook_only_grace_renews_and_expiry_discards_pending_reports() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let child = caller("root", "child");
    let t = tokio::time::Instant::now();
    delivery.touch_hook(&child, t);
    delivery.record_write(&child, &["a.rs".into()]);
    let errors = vec![diagnostic(0, DiagnosticSeverity::ERROR, "broken")];
    let (_, token) = delivery.stage(
        &child.record,
        &[entry("a.rs", &errors, SeverityFloor::Warning)],
    );
    delivery.touch_hook(&child, t + std::time::Duration::from_secs(59));
    delivery.expire(t + std::time::Duration::from_secs(60));
    assert!(delivery.pending.contains_key(&child.record));
    delivery.expire(t + std::time::Duration::from_secs(119));
    assert!(!delivery.commit(&child.record, token.unwrap()));
    assert!(delivery.retained.is_empty());
    assert!(delivery.ownership.is_empty());
}

#[test]
fn reconnect_preserves_history_until_the_last_close_plus_grace() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let root = caller("root", "root");
    let first = ConnectionId::next();
    let second = ConnectionId::next();
    let t = tokio::time::Instant::now();
    delivery.attach(first, &root);
    let errors = vec![diagnostic(0, DiagnosticSeverity::ERROR, "broken")];
    let entries = [entry("a.rs", &errors, SeverityFloor::Warning)];
    delivery.flush(&root.record, &entries);
    delivery.close(first, t);
    delivery.attach(second, &root);
    delivery.expire(t + std::time::Duration::from_secs(61));
    assert!(delivery.flush(&root.record, &entries).changed.is_empty());
    delivery.close(second, t + std::time::Duration::from_secs(61));
    delivery.expire(t + std::time::Duration::from_secs(121));
    assert_eq!(delivery.flush(&root.record, &entries).changed.len(), 1);
}

#[test]
fn a_writer_without_root_metadata_keeps_its_claim_when_resolved() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
    let mut a = caller("root", "a");
    a.root = None;
    delivery.record_write(&a, &["a.rs".into()]);
    let errors = vec![diagnostic(0, DiagnosticSeverity::ERROR, "broken")];
    let entries = [
        entry("a.rs", &errors, SeverityFloor::Warning),
        entry("unowned.rs", &errors, SeverityFloor::Warning),
    ];
    let report = delivery.flush(&a.record, &entries);
    assert_eq!(report.changed.len(), 1);
    assert_eq!(report.changed[0].key, "a.rs");
    a.root = Some("root".to_string().into());
    delivery.register_caller(&a);
    let root = caller("root", "root");
    delivery.register_caller(&root);
    assert_eq!(
        delivery.flush(&root.record, &entries).changed[0].key,
        "unowned.rs"
    );
    assert!(delivery.flush(&a.record, &entries).changed.is_empty());
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
