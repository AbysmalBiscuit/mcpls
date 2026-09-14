//! Owns the notification task for each live language-server generation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;

use crate::bridge::{ServerSettle, lock_std};
use crate::config::ServerId;
use crate::lsp::LspNotification;
use crate::{PumpShared, diagnostics_pump};

pub struct NotificationPumps {
    shared: PumpShared,
    cancel: watch::Receiver<bool>,
    state: StdMutex<PumpState>,
}

#[derive(Default)]
struct PumpState {
    closed: bool,
    tasks: HashMap<ServerId, Arc<Mutex<PumpTask>>>,
}

#[derive(Default)]
struct PumpTask(Option<JoinHandle<()>>);

impl PumpTask {
    async fn retire(&mut self) {
        if let Some(task) = self.0.as_mut() {
            task.abort();
            // Keep ownership until termination, including when this await is cancelled.
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                tracing::error!(%error, "LSP notification pump failed");
            }
            self.0 = None;
        }
    }
}

impl Drop for PumpTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

impl std::fmt::Debug for NotificationPumps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotificationPumps").finish_non_exhaustive()
    }
}

pub struct DiagnosticsReplacement {
    settle: Arc<ServerSettle>,
    id: ServerId,
    completed: bool,
}

impl DiagnosticsReplacement {
    pub const fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for DiagnosticsReplacement {
    fn drop(&mut self) {
        if !self.completed {
            self.settle.abort_diagnostics_replacement(&self.id);
            #[cfg(all(test, unix))]
            crate::recovery_tests::mark_replacement_aborted();
        }
    }
}

impl NotificationPumps {
    pub(crate) fn new(shared: PumpShared, cancel: watch::Receiver<bool>) -> Self {
        Self {
            shared,
            cancel,
            state: StdMutex::new(PumpState::default()),
        }
    }

    /// Shared cache and delivery state used by every notification pump.
    pub(crate) const fn shared(&self) -> &PumpShared {
        &self.shared
    }

    /// Shared settle state for every registered diagnostics owner.
    pub(crate) fn settle(&self) -> &ServerSettle {
        &self.shared.settle
    }

    /// Cancellation signal shared by every pump and baseline task.
    pub(crate) fn cancel_rx(&self) -> watch::Receiver<bool> {
        self.cancel.clone()
    }

    pub(crate) fn install(
        &self,
        id: ServerId,
        rx: mpsc::Receiver<LspNotification>,
        caches_diagnostics: bool,
    ) {
        let mut state = lock_std(&self.state);
        if state.closed {
            return;
        }
        let task = tokio::spawn(diagnostics_pump(
            id.clone(),
            rx,
            self.cancel.clone(),
            caches_diagnostics,
            self.shared.clone(),
        ));
        state
            .tasks
            .insert(id, Arc::new(Mutex::new(PumpTask(Some(task)))));
    }

    pub(crate) fn register_diagnostics_owner(&self, id: &ServerId) {
        self.shared.settle.register_diagnostics_owner(id);
    }

    pub(crate) fn prepare_diagnostics_replacement(&self, id: &ServerId) -> DiagnosticsReplacement {
        self.shared.settle.begin_diagnostics_replacement(id);
        DiagnosticsReplacement {
            settle: Arc::clone(&self.shared.settle),
            id: id.clone(),
            completed: false,
        }
    }

    pub(crate) async fn retire(&self, id: &ServerId) {
        let task = lock_std(&self.state).tasks.get(id).cloned();
        if let Some(task) = task {
            task.lock().await.retire().await;
        }
        self.shared.settle.forget_server(id);
        #[cfg(all(test, unix))]
        crate::recovery_tests::pause_after_retirement();
        #[cfg(all(test, unix))]
        crate::recovery_tests::pause_after_retirement_async().await;
    }

    pub(crate) async fn shutdown(&self) {
        let tasks: Vec<_> = {
            let mut state = lock_std(&self.state);
            state.closed = true;
            state.tasks.values().cloned().collect()
        };
        for task in tasks {
            task.lock().await.retire().await;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use lsp_types::{Diagnostic, PublishDiagnosticsParams, Uri};

    use super::*;
    use crate::bridge::{
        DiagnosticsDelivery, DocumentTracker, FloorTable, NotificationCache, ResourceSubscriptions,
        ServerSettle,
    };
    use crate::config::DiagnosticsConfig;

    fn shared() -> PumpShared {
        PumpShared {
            notification_cache: Arc::new(Mutex::new(NotificationCache::new())),
            subs: Arc::new(ResourceSubscriptions::new()),
            workspace_roots: Arc::from([]),
            document_tracker: Arc::new(DocumentTracker::new(
                crate::bridge::ResourceLimits::default(),
                HashMap::new(),
            )),
            settle: Arc::new(ServerSettle::new(
                Duration::from_secs(1),
                Duration::from_secs(60),
            )),
            delivery: Arc::new(Mutex::new(DiagnosticsDelivery::new(
                DiagnosticsConfig::default(),
            ))),
            floors: Arc::new(FloorTable::new(&DiagnosticsConfig::default(), &[])),
        }
    }

    #[tokio::test]
    async fn pump_retirement_prevents_queued_publish_repopulating_cache() {
        let shared = shared();
        let cache = Arc::clone(&shared.notification_cache);
        let mut held = cache.lock().await;
        let (_cancel, cancel_rx) = watch::channel(false);
        let pumps = NotificationPumps::new(shared, cancel_rx);
        let id = ServerId::from("rust");
        let uri: Uri = "file:///workspace/main.rs".parse().unwrap();
        let (tx, rx) = mpsc::channel(1);
        pumps.install(id.clone(), rx, true);
        tx.send(LspNotification::PublishDiagnostics(
            PublishDiagnosticsParams {
                uri: uri.clone(),
                version: None,
                diagnostics: vec![Diagnostic {
                    message: "old-generation".into(),
                    ..Diagnostic::default()
                }],
            },
        ))
        .await
        .unwrap();
        // Capacity returns only after the pump takes the publish and waits on the cache.
        tx.send(LspNotification::Other {
            method: "barrier".into(),
            params: None,
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), pumps.retire(&id))
            .await
            .unwrap();
        held.clear_server_diagnostics(&id);
        drop(held);
        assert!(cache.lock().await.get_diagnostics(uri.as_str()).is_none());
        assert!(tx.is_closed());
    }

    struct TerminationBarrier {
        entered: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Drop for TerminationBarrier {
        fn drop(&mut self) {
            let _ = self.entered.take().unwrap().send(());
            let _ = self.release.recv_timeout(Duration::from_secs(5));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pump_cancelled_retirement_retry_waits_for_termination() {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            let _barrier = TerminationBarrier {
                entered: Some(entered_tx),
                release: release_rx,
            };
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready_rx.await.unwrap();
        let (_cancel, cancel_rx) = watch::channel(false);
        let pumps = Arc::new(NotificationPumps::new(shared(), cancel_rx));
        let id = ServerId::from("rust");
        let slot = Arc::new(Mutex::new(PumpTask(Some(task))));
        lock_std(&pumps.state)
            .tasks
            .insert(id.clone(), Arc::clone(&slot));
        let first_pumps = Arc::clone(&pumps);
        let first_id = id.clone();
        let first = tokio::spawn(async move { first_pumps.retire(&first_id).await });
        entered_rx.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let premature = tokio::time::timeout(Duration::from_millis(30), pumps.retire(&id)).await;
        assert!(
            premature.is_err(),
            "a cancelled retirement must retain the old task's termination barrier"
        );
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), pumps.retire(&id))
            .await
            .unwrap();
        assert!(slot.lock().await.0.is_none());
    }

    #[tokio::test]
    async fn pump_owner_drop_and_shutdown_close_receivers() {
        let (_cancel, cancel_rx) = watch::channel(false);
        let pumps = NotificationPumps::new(shared(), cancel_rx.clone());
        let (tx, rx) = mpsc::channel(1);
        pumps.install(ServerId::from("rust"), rx, true);
        drop(pumps);
        tokio::time::timeout(Duration::from_secs(2), tx.closed())
            .await
            .unwrap();

        let pumps = NotificationPumps::new(shared(), cancel_rx);
        let (tx, rx) = mpsc::channel(1);
        pumps.install(ServerId::from("rust"), rx, true);
        pumps.shutdown().await;
        assert!(tx.is_closed());
        let (late_tx, late_rx) = mpsc::channel(1);
        pumps.install(ServerId::from("python"), late_rx, true);
        assert!(
            late_tx.is_closed(),
            "startup must not install a pump after shutdown"
        );
    }
}
