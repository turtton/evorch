//! Memory-only admission registration. This is not a reservation or an intent queue.
//! Lock order: host -> attachment -> sidebar -> storage/run-map; this gate is a leaf.
use crate::{AgentRuntime, RunId};
use event_bus::{Event, EventBus, EventKind, PersistenceStage};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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
}
#[derive(Hash, PartialEq, Eq)]
struct EventKey {
    monotonic: std::time::Duration,
    wall: std::time::SystemTime,
    subkind: &'static str,
    scope: String,
}
#[derive(Default)]
pub struct RetentionGate(Mutex<State>);
pub(crate) struct Admission {
    gate: Arc<RetentionGate>,
    scope: Option<String>,
    retain: bool,
}
impl Drop for Admission {
    fn drop(&mut self) {
        let mut s = self.gate.0.lock().unwrap_or_else(|e| e.into_inner());
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
        gate
    }
    fn enter(self: &Arc<Self>, scope: Option<String>) -> Admission {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
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
    pub(crate) fn enter_scope(self: &Arc<Self>, scope: Option<String>) -> Admission {
        self.enter(scope)
    }
    #[cfg(test)]
    pub(crate) fn enter_storage(self: &Arc<Self>) -> Admission {
        self.enter(None)
    }
    pub fn stamp(&self, scope: &str) -> GateStamp {
        let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut v = s.scopes.get(scope).copied().unwrap_or_default();
        v.revision = v.revision.wrapping_add(s.global.revision);
        v.pending += s.global.pending;
        v.failed += s.global.failed;
        v
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
            let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
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
impl AgentRuntime {
    pub(crate) fn retention_admission(&self) -> Admission {
        self.shared.retention_gate.enter(None)
    }
    pub(crate) fn retention_run_admission(&self, run: RunId) -> Admission {
        self.shared.retention_gate.enter(Some(format!("r:{run}")))
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
        let mut state = self.gate.0.lock().unwrap_or_else(|e| e.into_inner());
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
        let bus = EventBus::new(8);
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
        let bus = EventBus::new(8);
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
