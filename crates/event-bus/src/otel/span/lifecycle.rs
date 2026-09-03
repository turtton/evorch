use std::time::SystemTime;

use crate::event::{AgentRunPhase, LifecycleEvent};

use super::state::{EndSpec, StartSpec, terminal_error};
use super::{SpanAction, SpanAttribute, SpanDropKind, SpanKey, SpanKind, SpanMapper, SpanStatus};

const MAX_DEPTH: u32 = 99;

impl SpanMapper {
    pub(super) fn map_lifecycle(
        &mut self,
        event: &LifecycleEvent,
        at: SystemTime,
    ) -> Vec<SpanAction> {
        match event {
            LifecycleEvent::Started { session_id } => self.start_span(StartSpec {
                key: SpanKey::Session {
                    session_id: session_id.clone(),
                },
                parent: None,
                name: "evorch.session".to_owned(),
                kind: SpanKind::Internal,
                at,
                attributes: vec![SpanAttribute::new("evorch.session.id", session_id.as_str())],
            }),
            LifecycleEvent::Completed { session_id } => self.end_span(EndSpec {
                key: SpanKey::Session {
                    session_id: session_id.clone(),
                },
                at,
                status: SpanStatus::Unset,
                terminal: Vec::new(),
            }),
            LifecycleEvent::Failed { session_id, .. } => self.end_span(EndSpec {
                key: SpanKey::Session {
                    session_id: session_id.clone(),
                },
                at,
                status: SpanStatus::Error,
                terminal: terminal_error(Some("session_failed")),
            }),
            LifecycleEvent::AgentRunStarted { .. } => self.start_run(event, at),
            LifecycleEvent::AgentRunStateChanged { run_id, to, .. } => match to {
                AgentRunPhase::Done => self.end_run(run_id, at, SpanStatus::Unset),
                AgentRunPhase::Error => self.end_run(run_id, at, SpanStatus::Error),
                AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => {
                    Vec::new()
                }
            },
            LifecycleEvent::BackgroundTaskStarted { task_id } => {
                let key = SpanKey::Run {
                    run_id: task_id.clone(),
                };
                self.push_in_flight_attribute(
                    &key,
                    SpanAttribute::new("evorch.task.id", task_id.clone()),
                    at,
                );
                Vec::new()
            }
            LifecycleEvent::Delegated { .. }
            | LifecycleEvent::BackgroundTaskCompleted { .. }
            | LifecycleEvent::BackgroundTaskCancelled { .. } => Vec::new(),
        }
    }

    fn start_run(&mut self, event: &LifecycleEvent, at: SystemTime) -> Vec<SpanAction> {
        let LifecycleEvent::AgentRunStarted {
            run_id,
            parent_run_id,
            agent_name,
            role,
        } = event
        else {
            return Vec::new();
        };
        let run_key = SpanKey::Run {
            run_id: run_id.clone(),
        };
        let agent_key = SpanKey::Agent {
            run_id: run_id.clone(),
        };
        // 委譲先の親 agent span が open でないなら子 subtree を開始しない
        // (未見 / 終了済み / 未 admission の親への委譲を root に降格させない)。
        // 親が tombstone 済みなら元の拒否 kind を replay する。
        if let Some(parent_run_id) = parent_run_id
            && !self.open.contains_key(&SpanKey::Agent {
                run_id: parent_run_id.clone(),
            })
        {
            let parent_agent_key = SpanKey::Agent {
                run_id: parent_run_id.clone(),
            };
            let replayed = self.tombstone_kind(&parent_agent_key);
            let kind = replayed.unwrap_or(SpanDropKind::UnknownParent);
            self.add_tombstone(run_key.clone(), kind);
            self.add_tombstone(agent_key.clone(), kind);
            match replayed {
                Some(kind) => self.record_replayed_drop(kind, run_key),
                None => self.record_drop(SpanDropKind::UnknownParent, run_key, at),
            }
            return Vec::new();
        }
        // sampling 判定は消費なしで先取りし、帳簿は admission 確定後にのみ
        // 保持する。拒否 run は terminal 不着でも帳簿に永久残存しない。
        let decision = self.sampling_decision_for(run_id, parent_run_id.as_deref());
        if !decision {
            self.add_tombstone(run_key.clone(), SpanDropKind::SampledOut);
            self.add_tombstone(agent_key.clone(), SpanDropKind::SampledOut);
            self.record_drop(SpanDropKind::SampledOut, run_key, at);
            self.record_drop(SpanDropKind::SampledOut, agent_key, at);
            return Vec::new();
        }
        let depth = parent_run_id
            .as_deref()
            .and_then(|parent| self.agent_depth.get(parent).copied())
            .and_then(|parent_depth| parent_depth.checked_add(1))
            .map_or(0, |depth| depth.min(MAX_DEPTH));
        // run と agent の 2 開始は 1 回の admission として扱う。片方でも
        // 開始できないなら両方開始せず、partial tree を作らない。
        let run_open = self.open.contains_key(&run_key);
        if run_open || self.open.contains_key(&agent_key) {
            let key = if run_open { run_key } else { agent_key };
            self.record_drop(SpanDropKind::DuplicateSpan, key, at);
            return Vec::new();
        }
        if let Err(kind) = self.check_admission(2, at) {
            self.add_tombstone(run_key.clone(), kind);
            self.add_tombstone(agent_key.clone(), kind);
            self.record_drop(kind, run_key, at);
            return Vec::new();
        }
        self.sampling_decisions.insert(run_id.clone(), decision);
        self.agent_depth.insert(run_id.clone(), depth);
        let mut run_attributes = vec![SpanAttribute::new("evorch.agent_run.id", run_id.clone())];
        if let Some(parent_run_id) = parent_run_id {
            run_attributes.push(SpanAttribute::new(
                "evorch.parent_agent_run.id",
                parent_run_id.clone(),
            ));
        }
        run_attributes.extend([
            SpanAttribute::new("evorch.agent.name", agent_name.clone()),
            SpanAttribute::new("evorch.delegation.role", role.clone()),
            SpanAttribute::new("evorch.delegation.depth", i64::from(depth)),
        ]);
        // run と agent の 2 開始は 1 回の admission として扱う。片方でも
        // 開始できないなら両方開始せず、partial tree を作らない。
        let run_open = self.open.contains_key(&run_key);
        if run_open || self.open.contains_key(&agent_key) {
            let key = if run_open { run_key } else { agent_key };
            self.record_drop(SpanDropKind::DuplicateSpan, key, at);
            return Vec::new();
        }
        if let Err(kind) = self.check_admission(2, at) {
            self.add_tombstone(run_key.clone(), kind);
            self.add_tombstone(agent_key.clone(), kind);
            self.record_drop(kind, run_key, at);
            return Vec::new();
        }
        let mut actions = self.start_span(StartSpec {
            key: run_key.clone(),
            parent: parent_run_id.as_ref().map(|parent| SpanKey::Agent {
                run_id: parent.clone(),
            }),
            name: format!("evorch.run {agent_name}"),
            kind: SpanKind::Internal,
            at,
            attributes: run_attributes,
        });
        actions.extend(self.start_span(StartSpec {
            key: agent_key,
            parent: Some(run_key),
            name: format!("invoke_agent {agent_name}"),
            kind: SpanKind::Client,
            at,
            attributes: vec![
                SpanAttribute::new("gen_ai.operation.name", "invoke_agent"),
                SpanAttribute::new("gen_ai.provider.name", "evorch"),
                SpanAttribute::new("gen_ai.agent.name", agent_name.clone()),
                SpanAttribute::new("evorch.agent_run.id", run_id.clone()),
                SpanAttribute::new("evorch.delegation.role", role.clone()),
                SpanAttribute::new("evorch.delegation.depth", i64::from(depth)),
            ],
        }));
        actions
    }

    fn end_run(&mut self, run_id: &str, at: SystemTime, status: SpanStatus) -> Vec<SpanAction> {
        let actions = if self.is_tombstoned(&SpanKey::Run {
            run_id: run_id.to_owned(),
        }) {
            Vec::new()
        } else {
            let error_type = (status == SpanStatus::Error).then_some("agent_run_error");
            let mut actions = self.end_span(EndSpec {
                key: SpanKey::Agent {
                    run_id: run_id.to_owned(),
                },
                at,
                status,
                terminal: terminal_error(error_type),
            });
            actions.extend(self.end_span(EndSpec {
                key: SpanKey::Run {
                    run_id: run_id.to_owned(),
                },
                at,
                status,
                terminal: terminal_error(error_type),
            }));
            actions
        };
        // run 終端で per-run 相関帳簿を解放する。子 run は開始時点で自
        // depth / decision を保持しているため、親の解放で active な子 subtree
        // は壊れない。
        self.sampling_decisions.remove(run_id);
        self.agent_depth.remove(run_id);
        actions
    }
}
