//! Memory-only admission registration. This is not a reservation or an intent queue.
//! Lock order: host -> attachment -> sidebar -> storage/run-map; this gate is a leaf.
use super::TombstoneGuard;
use crate::{AgentRuntime, RunId};
use event_bus::{Event, EventBus, EventKind, PersistenceStage};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GateStamp {
    pub revision: u64,
    pub pending: usize,
    pub failed: usize,
}
#[derive(Default)]
struct State {
    scopes: HashMap<String, GateStamp>,
    events: HashMap<EventKey, usize>,
    global: GateStamp,
    sealed: Option<HashSet<String>>,
    #[cfg(test)]
    waiters: usize,
}
#[derive(Hash, PartialEq, Eq)]
struct EventKey {
    monotonic: std::time::Duration,
    wall: std::time::SystemTime,
    subkind: &'static str,
    scope: String,
}
#[derive(Default)]
pub struct RetentionGate {
    state: Mutex<State>,
    released: Condvar,
    guard: OnceLock<TombstoneGuard>,
    max_seal_ns: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    blocked: Condvar,
}
pub(crate) struct Admission {
    gate: Arc<RetentionGate>,
    scope: Option<String>,
    retain: bool,
}
impl Drop for Admission {
    fn drop(&mut self) {
        let mut s = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        let stamp = match &self.scope {
            Some(k) => s.scopes.get_mut(k).expect("registered scope"),
            None => &mut s.global,
        };
        if !self.retain {
            stamp.pending -= 1;
        }
        stamp.revision = stamp.revision.wrapping_add(1);
    }
}
impl RetentionGate {
    pub(crate) fn install(bus: &EventBus) -> Arc<Self> {
        let gate = bus.shared_state(Self::default);
        let weak = Arc::downgrade(&gate);
        bus.persistence_notifications()
            .install(Arc::new(move |event, stage| {
                if let Some(gate) = weak.upgrade() {
                    gate.event(event, stage)
                }
            }));
        let weak = Arc::downgrade(&gate);
        bus.install_publish_admission(Arc::new(move |event| {
            let gate = weak.upgrade()?;
            let mut scopes = Vec::new();
            if let Some(run) = event.primary_run_id() {
                scopes.push(format!("r:{run}"));
            }
            if let EventKind::Orchestrator(event_bus::OrchestratorEvent::ThreadGoalUpdated {
                snapshot,
            }) = &event.kind
            {
                scopes.push(format!("t:{}", snapshot.thread_id));
            }
            if let EventKind::PullRequest(event) = &event.kind {
                scopes.push(format!("t:{}", event.thread_id()));
            }
            gate.enter_event(scopes)
                .map(|p| Box::new(p) as Box<dyn Send>)
        }));
        gate
    }
    fn enter(self: &Arc<Self>, scope: Option<String>) -> Admission {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while blocked(&s, scope.as_deref()) {
            #[cfg(test)]
            {
                s.waiters += 1;
                self.blocked.notify_all();
            }
            s = self.released.wait(s).unwrap_or_else(|e| e.into_inner());
            #[cfg(test)]
            {
                s.waiters -= 1;
            }
        }
        let stamp = match &scope {
            Some(k) => s.scopes.entry(k.clone()).or_default(),
            None => &mut s.global,
        };
        stamp.revision = stamp.revision.wrapping_add(1);
        stamp.pending += 1;
        drop(s);
        Admission {
            gate: self.clone(),
            scope,
            retain: false,
        }
    }
    #[cfg(test)]
    fn wait_blocked(&self, count: usize) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while s.waiters < count {
            s = self.blocked.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }
    pub(crate) fn set_guard(&self, guard: TombstoneGuard) {
        let _ = self.guard.set(guard);
    }
    fn enter_event(self: &Arc<Self>, scopes: Vec<String>) -> Option<Vec<Admission>> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while scopes.iter().any(|scope| blocked(&s, Some(scope))) {
            #[cfg(test)]
            {
                s.waiters += 1;
                self.blocked.notify_all();
            }
            s = self.released.wait(s).unwrap_or_else(|e| e.into_inner());
            #[cfg(test)]
            {
                s.waiters -= 1;
            }
        }
        if let Some(guard) = self.guard.get() {
            for scope in &scopes {
                let deleted = scope
                    .strip_prefix("r:")
                    .is_some_and(|id| guard.check_run(id).is_err())
                    || scope
                        .strip_prefix("t:")
                        .is_some_and(|id| guard.check_thread(id).is_err());
                if deleted {
                    return None;
                }
            }
        }
        Some(
            scopes
                .into_iter()
                .map(|scope| {
                    let stamp = s.scopes.entry(scope.clone()).or_default();
                    stamp.revision = stamp.revision.wrapping_add(1);
                    stamp.pending += 1;
                    Admission {
                        gate: self.clone(),
                        scope: Some(scope),
                        retain: false,
                    }
                })
                .collect(),
        )
    }
    fn try_seal(self: &Arc<Self>, before: &[(String, GateStamp)]) -> Option<Seal> {
        let guard = self.guard.get()?.clone();
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.sealed.is_some()
            || before.is_empty()
            || before.iter().any(|(scope, stamp)| {
                stamp.pending != 0 || stamp.failed != 0 || *stamp != stamp_for(&s, scope)
            })
        {
            return None;
        }
        s.sealed = Some(before.iter().map(|(scope, _)| scope.clone()).collect());
        Some(Seal {
            gate: self.clone(),
            guard,
            started: std::time::Instant::now(),
        })
    }
    pub(crate) fn enter_scope(self: &Arc<Self>, scope: Option<String>) -> Admission {
        self.enter(scope)
    }
    #[cfg(test)]
    pub(crate) fn enter_storage(self: &Arc<Self>) -> Admission {
        self.enter(None)
    }
    pub fn stamp(&self, scope: &str) -> GateStamp {
        stamp_for(&self.state.lock().unwrap_or_else(|e| e.into_inner()), scope)
    }
    fn event(&self, e: &Event, stage: PersistenceStage) {
        let relevant = matches!(
            e.kind,
            EventKind::Message(
                event_bus::MessageEvent::MessageCompleted { .. }
                    | event_bus::MessageEvent::MessagePart { .. }
                    | event_bus::MessageEvent::FinalResultPublished { .. }
            ) | EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged { .. }
                    | event_bus::LifecycleEvent::TurnCompleted { .. }
            ) | EventKind::AgentMessage(event_bus::AgentMessageEvent::Delivered { .. })
                | EventKind::Orchestrator(event_bus::OrchestratorEvent::ThreadGoalUpdated { .. })
        );
        if !relevant {
            return;
        }
        let Some(run) = e.primary_run_id() else {
            return;
        };
        let mut scopes = vec![format!("r:{run}")];
        if let EventKind::Orchestrator(event_bus::OrchestratorEvent::ThreadGoalUpdated {
            snapshot,
        }) = &e.kind
        {
            scopes.push(format!("t:{}", snapshot.thread_id));
        }
        for scope in scopes {
            let key = EventKey {
                monotonic: e.meta.monotonic,
                wall: e.meta.wall_clock,
                subkind: e.kind.subkind_name(),
                scope: scope.clone(),
            };
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match stage {
                PersistenceStage::Published => {
                    *s.events.entry(key).or_default() += 1;
                    let v = s.scopes.entry(scope).or_default();
                    v.pending += 1;
                    v.revision = v.revision.wrapping_add(1);
                }
                PersistenceStage::Committed => {
                    if let Some(count) = s.events.get_mut(&key) {
                        *count -= 1;
                        if *count == 0 {
                            s.events.remove(&key);
                        }
                        let v = s.scopes.get_mut(&scope).expect("pending scope");
                        v.pending -= 1;
                        v.revision = v.revision.wrapping_add(1);
                    }
                }
                PersistenceStage::Failed => {
                    if s.events.contains_key(&key) {
                        s.scopes.entry(scope).or_default().failed += 1;
                    }
                }
            }
        }
    }
}
fn blocked(state: &State, scope: Option<&str>) -> bool {
    state
        .sealed
        .as_ref()
        .is_some_and(|sealed| scope.is_none_or(|scope| sealed.contains(scope)))
}
fn stamp_for(state: &State, scope: &str) -> GateStamp {
    let mut stamp = state.scopes.get(scope).copied().unwrap_or_default();
    stamp.revision = stamp.revision.wrapping_add(state.global.revision);
    stamp.pending += state.global.pending;
    stamp.failed += state.global.failed;
    stamp
}
struct Fence {
    gate: Arc<RetentionGate>,
    before: Vec<(String, GateStamp)>,
}
struct Seal {
    gate: Arc<RetentionGate>,
    guard: TombstoneGuard,
    started: std::time::Instant,
}
impl storage::thread_retention::online::CommitFence for Fence {
    fn try_seal(&self) -> Option<Box<dyn storage::thread_retention::online::SealedCommit>> {
        self.gate
            .try_seal(&self.before)
            .map(|seal| Box::new(seal) as _)
    }
}
impl storage::thread_retention::online::SealedCommit for Seal {
    fn publish(&mut self, family: &storage::thread_retention::ConfirmedFamily) {
        self.guard.publish(family);
    }
}
impl Drop for Seal {
    fn drop(&mut self) {
        let mut s = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        s.sealed = None;
        self.gate.max_seal_ns.fetch_max(
            self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.gate.released.notify_all();
    }
}
impl AgentRuntime {
    pub(crate) fn retention_thread_admission(&self, thread: &str) -> Admission {
        self.shared
            .retention_gate
            .enter(Some(format!("t:{thread}")))
    }
    pub(crate) fn retention_admission(&self) -> Admission {
        self.shared.retention_gate.enter(None)
    }
    pub(crate) fn retention_run_admission(&self, run: RunId) -> Admission {
        self.shared.retention_gate.enter(Some(format!("r:{run}")))
    }
    /// Register before publishing sidebar or attachment state; keep through mirror ACK.
    pub fn retention_workspace_admission(&self) -> Box<dyn storage::MutationAdmission> {
        Box::new(self.retention_admission())
    }
    pub fn retention_commit_fence(
        &self,
        before: Vec<(String, GateStamp)>,
    ) -> Arc<dyn storage::thread_retention::online::CommitFence> {
        Arc::new(Fence {
            gate: self.shared.retention_gate.clone(),
            before,
        })
    }
    pub fn retention_max_seal_ns(&self) -> u64 {
        self.shared
            .retention_gate
            .max_seal_ns
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn retention_gate_stamp(&self, scope: &str) -> GateStamp {
        self.shared.retention_gate.stamp(scope)
    }
}

impl storage::MutationAdmission for Admission {
    fn persistence_failed(&mut self) {
        if self.retain {
            return;
        }
        self.retain = true;
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        let stamp = match &self.scope {
            Some(scope) => state.scopes.get_mut(scope).expect("registered scope"),
            None => &mut state.global,
        };
        stamp.failed += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage::MutationAdmission;

    fn message() -> Event {
        Event::new(event_bus::MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: "not logged".into(),
        })
    }

    #[test]
    fn scoped_registration_and_global_registration_invalidate_only_their_scope() {
        let gate = Arc::new(RetentionGate::default());
        let initial = gate.stamp("r:run-1");
        let admission = gate.enter(Some("r:run-1".into()));
        assert_eq!(gate.stamp("r:run-1").pending, 1);
        assert_eq!(gate.stamp("r:run-2").pending, 0);
        drop(admission);
        assert_eq!(gate.stamp("r:run-1").pending, 0);
        assert_ne!(gate.stamp("r:run-1"), initial);
        let admission = gate.enter_storage();
        assert_eq!(gate.stamp("r:run-2").pending, 1);
        drop(admission);
        assert_eq!(gate.stamp("r:run-2").pending, 0);
    }

    #[test]
    fn published_event_remains_pending_after_admission_until_its_own_writer_ack() {
        let bus = Arc::new(EventBus::new(8));
        let gate = RetentionGate::install(&bus);
        let event = message();
        let admission = gate.enter(Some("r:run-1".into()));
        bus.emit(event.clone());
        drop(admission);
        assert_eq!(gate.stamp("r:run-1").pending, 1);
        let mut unrelated = event.clone();
        unrelated.meta.monotonic += std::time::Duration::from_nanos(1);
        bus.persistence_notifications()
            .notify(&unrelated, PersistenceStage::Committed);
        assert_eq!(gate.stamp("r:run-1").pending, 1);
        bus.persistence_notifications()
            .notify(&event, PersistenceStage::Committed);
        assert_eq!(gate.stamp("r:run-1").pending, 0);
    }

    #[test]
    fn failed_event_and_failed_synchronous_source_ack_remain_protected() {
        let bus = Arc::new(EventBus::new(8));
        let gate = RetentionGate::install(&bus);
        let event = message();
        bus.emit(event.clone());
        bus.persistence_notifications()
            .notify(&event, PersistenceStage::Failed);
        assert_eq!(gate.stamp("r:run-1").pending, 1);
        assert_eq!(gate.stamp("r:run-1").failed, 1);
        let mut admission = gate.enter_storage();
        admission.persistence_failed();
        drop(admission);
        assert_eq!(gate.stamp("r:run-2").pending, 1);
        assert_eq!(gate.stamp("r:run-2").failed, 1);
    }
}

#[cfg(test)]
mod seal_tests {
    use super::*;
    use storage::thread_retention::online::SealedCommit;
    fn ready(bus: &EventBus) -> (Arc<RetentionGate>, TombstoneGuard) {
        let gate = RetentionGate::install(bus);
        let guard = TombstoneGuard::default();
        gate.set_guard(guard.clone());
        (gate, guard)
    }
    fn before(gate: &RetentionGate) -> Vec<(String, GateStamp)> {
        ["", "r:run-1", "t:thread-1"]
            .into_iter()
            .map(|s| (s.into(), gate.stamp(s)))
            .collect()
    }
    fn confirmed() -> storage::thread_retention::ConfirmedFamily {
        storage::thread_retention::ConfirmedFamily {
            family_key: "thread-1".into(),
            threads: vec!["thread-1".into()],
            runs: vec!["run-1".into()],
            tasks: vec![],
            estimate_bytes: 0,
        }
    }
    #[test]
    fn stale_or_pending_scope_cannot_seal_but_unrelated_scope_can_enter() {
        let bus = Arc::new(EventBus::new(8));
        let (gate, _) = ready(&bus);
        let old = before(&gate);
        let token = gate.enter(Some("r:run-1".into()));
        assert!(gate.try_seal(&old).is_none());
        drop(token);
        assert!(gate.try_seal(&old).is_none());
        let seal = gate.try_seal(&before(&gate)).unwrap();
        let unrelated = gate.enter(Some("r:run-2".into()));
        drop(unrelated);
        drop(seal);
    }
    #[test]
    fn synchronous_global_writer_factory_waits_and_observes_published_guard() {
        let bus = Arc::new(EventBus::new(8));
        let (gate, guard) = ready(&bus);
        let mut seal = gate.try_seal(&before(&gate)).unwrap();
        let g = gate.clone();
        let observer = guard.clone();
        let waiter = std::thread::spawn(move || {
            let _request = g.enter_storage();
            (Ok::<_, ()>(42), observer.check_run("run-1").is_err())
        });
        gate.wait_blocked(1);
        seal.publish(&confirmed());
        drop(seal);
        assert_eq!(waiter.join().unwrap(), (Ok(42), true));
        assert_eq!(gate.stamp("r:run-1").pending, 0);
    }
    #[tokio::test]
    async fn rollback_releases_live_bus_publish_without_dropping_and_ack_drains() {
        let bus = Arc::new(EventBus::new(8));
        let mut rx = bus.subscribe();
        let (gate, _) = ready(&bus);
        let seal = gate.try_seal(&before(&gate)).unwrap();
        let event = Event::new(event_bus::MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: "body".into(),
        });
        let publisher = bus.clone();
        let sent = event.clone();
        let waiter = std::thread::spawn(move || publisher.emit(sent));
        gate.wait_blocked(1);
        drop(seal);
        assert_eq!(waiter.join().unwrap(), 1);
        assert!(rx.recv().await.is_ok());
        assert_eq!(gate.stamp("r:run-1").pending, 1);
        bus.persistence_notifications()
            .notify(&event, PersistenceStage::Committed);
        assert_eq!(gate.stamp("r:run-1").pending, 0);
    }
    #[test]
    fn committed_deleted_bus_target_is_fenced_only_after_publish() {
        let bus = Arc::new(EventBus::new(8));
        let _rx = bus.subscribe();
        let (gate, _) = ready(&bus);
        let mut seal = gate.try_seal(&before(&gate)).unwrap();
        let publisher = bus.clone();
        let waiter = std::thread::spawn(move || {
            publisher.emit(Event::new(event_bus::MessageEvent::MessageCompleted {
                run_id: "run-1".into(),
                text: "late".into(),
            }))
        });
        gate.wait_blocked(1);
        seal.publish(&confirmed());
        drop(seal);
        assert_eq!(waiter.join().unwrap(), 0);

        assert_eq!(gate.stamp("r:run-1").pending, 0);
    }
    #[test]
    fn panic_releases_seal_without_publishing_guard() {
        let bus = Arc::new(EventBus::new(8));
        let (gate, guard) = ready(&bus);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _seal = gate.try_seal(&before(&gate)).unwrap();
            panic!("injected writer panic");
        }));
        assert!(result.is_err());
        assert!(guard.check_run("run-1").is_ok());
        let _next = gate.try_seal(&before(&gate)).unwrap();
    }
}
