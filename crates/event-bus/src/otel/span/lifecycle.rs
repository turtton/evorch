use std::time::SystemTime;

use crate::event::{AgentRunPhase, LifecycleEvent};

use super::state::{EndSpec, StartSpec, terminal_error};
use super::{SpanAction, SpanAttribute, SpanKey, SpanKind, SpanMapper, SpanStatus};

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
        self.sampling_decision(run_id, parent_run_id.as_deref());
        let depth = parent_run_id
            .as_deref()
            .and_then(|parent| self.agent_depth.get(parent).copied())
            .and_then(|parent_depth| parent_depth.checked_add(1))
            .map_or(0, |depth| depth.min(MAX_DEPTH));
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
        let run_key = SpanKey::Run {
            run_id: run_id.clone(),
        };
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
        if actions.is_empty() && self.sampling_decisions.get(run_id).copied().unwrap_or(true) {
            return actions;
        }
        actions.extend(self.start_span(StartSpec {
            key: SpanKey::Agent {
                run_id: run_id.clone(),
            },
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
        if self.is_tombstoned(&SpanKey::Run {
            run_id: run_id.to_owned(),
        }) {
            return Vec::new();
        }
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
    }
}
