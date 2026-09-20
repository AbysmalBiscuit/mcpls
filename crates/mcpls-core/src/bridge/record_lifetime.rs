use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify, watch};
use tokio::time::Instant;

use super::{Caller, ConnectionId, DiagnosticsDelivery, RecordId, SessionId};

#[derive(Debug)]
struct Record {
    root: Option<SessionId>,
    last_unprotected_activity: Instant,
    ended: bool,
}

#[derive(Debug)]
pub(super) struct RecordLifetime {
    grace: Duration,
    records: HashMap<RecordId, Record>,
    connections: HashMap<ConnectionId, HashSet<RecordId>>,
    ended_roots: HashSet<SessionId>,
    pub(super) changed: Arc<Notify>,
}

impl RecordLifetime {
    pub(super) fn new(grace: Duration) -> Self {
        Self {
            grace,
            records: HashMap::new(),
            connections: HashMap::new(),
            ended_roots: HashSet::new(),
            changed: Arc::new(Notify::new()),
        }
    }

    fn protected(&self, record: &RecordId, state: &Record) -> bool {
        self.connections.values().any(|records| {
            records.contains(record)
                || state
                    .root
                    .as_ref()
                    .is_some_and(|root| records.contains(&RecordId::Session(root.clone())))
        })
    }

    fn touch_hook(&mut self, caller: &Caller, now: Instant) {
        let ended = caller
            .root
            .as_ref()
            .is_some_and(|root| self.ended_roots.contains(root));
        let state = self
            .records
            .entry(caller.record.clone())
            .or_insert_with(|| Record {
                root: caller.root.clone(),
                last_unprotected_activity: now,
                ended,
            });
        if caller.root.is_some() {
            state.root.clone_from(&caller.root);
        }
        state.ended |= ended;
        if !state.ended {
            state.last_unprotected_activity = now;
        }
        self.changed.notify_one();
    }

    fn attach(&mut self, connection: ConnectionId, caller: &Caller) {
        self.touch_hook(caller, Instant::now());
        self.connections
            .entry(connection)
            .or_default()
            .insert(caller.record.clone());
        self.changed.notify_one();
    }

    fn close(&mut self, connection: ConnectionId, now: Instant) {
        let protected: Vec<_> = self
            .records
            .iter()
            .filter(|(record, state)| self.protected(record, state))
            .map(|(record, _)| record.clone())
            .collect();
        self.connections.remove(&connection);
        for record in protected {
            let unprotected = self
                .records
                .get(&record)
                .is_some_and(|state| !self.protected(&record, state));
            if unprotected && let Some(state) = self.records.get_mut(&record) {
                state.last_unprotected_activity = now;
            }
        }
        self.changed.notify_one();
    }

    fn end_root(&mut self, root: &SessionId, now: Instant) {
        self.ended_roots.insert(root.clone());
        for (record, state) in &mut self.records {
            if state.root.as_ref() == Some(root) || *record == RecordId::Session(root.clone()) {
                state.ended = true;
                state.last_unprotected_activity = now;
            }
        }
        self.changed.notify_one();
    }

    fn deadline(&self, record: &RecordId, state: &Record) -> Option<Instant> {
        if self.protected(record, state) {
            return None;
        }
        Some(
            state.last_unprotected_activity
                + if state.ended {
                    Duration::ZERO
                } else {
                    self.grace
                },
        )
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.records
            .iter()
            .filter_map(|(record, state)| self.deadline(record, state))
            .min()
    }

    fn expire(&mut self, now: Instant) -> Vec<RecordId> {
        let expired: Vec<_> = self
            .records
            .iter()
            .filter(|(record, state)| {
                self.deadline(record, state)
                    .is_some_and(|deadline| deadline <= now)
            })
            .map(|(record, _)| record.clone())
            .collect();
        for record in &expired {
            self.records.remove(record);
        }
        self.ended_roots.retain(|root| {
            self.records
                .values()
                .any(|state| state.root.as_ref() == Some(root))
        });
        expired
    }

    pub(super) fn forget(&mut self, record: &RecordId) {
        self.records.remove(record);
        for records in self.connections.values_mut() {
            records.remove(record);
        }
    }
}

impl DiagnosticsDelivery {
    pub(crate) fn attach(&mut self, connection: ConnectionId, caller: &Caller) {
        let mut caller = caller.clone();
        let anonymous = SessionId::for_connection(connection);
        if caller.record == RecordId::Session(anonymous.clone()) {
            caller.root = Some(anonymous);
        }
        self.register_caller(&caller);
        let caller = self.caller(caller.record.clone());
        self.lifetime.attach(connection, &caller);
    }

    pub(crate) fn close(&mut self, connection: ConnectionId, now: Instant) {
        self.lifetime.close(connection, now);
        self.end_session(&SessionId::for_connection(connection));
        self.expire(now);
    }

    pub(crate) fn touch_hook(&mut self, caller: &Caller, now: Instant) {
        self.register_caller(caller);
        let caller = self.caller(caller.record.clone());
        self.lifetime.touch_hook(&caller, now);
    }

    pub(crate) fn end_root(&mut self, root: &SessionId, now: Instant) {
        self.lifetime.end_root(root, now);
        self.expire(now);
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        for record in self.lifetime.expire(now) {
            self.end_session(&record);
        }
    }

    pub(crate) async fn run_expiry(delivery: Arc<Mutex<Self>>, mut cancel: watch::Receiver<bool>) {
        let changed = Arc::clone(&delivery.lock().await.lifetime.changed);
        loop {
            if *cancel.borrow() {
                return;
            }
            let deadline = {
                let mut delivery = delivery.lock().await;
                delivery.expire(Instant::now());
                delivery.lifetime.next_deadline()
            };
            let wait = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                () = wait => {},
                () = changed.notified() => {},
                result = cancel.changed() => { if result.is_err() || *cancel.borrow() { return; } }
            }
        }
    }
}
