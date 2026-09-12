use std::collections::{BTreeMap, VecDeque};

use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent, OrchestratorEvent, ToolEvent};
use workspace_ui::ThreadRunPhase;

use crate::app::{AttentionAck, DisplayRevision};

pub const MAX_NOTIFICATIONS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationKind {
    RunCompleted,
    RunFailed { reason: Option<String> },
    ApprovalPending { tool_name: String, call_id: String },
    MergeApprovalPending { goal_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub id: u64,
    pub kind: NotificationKind,
    pub run_id: Option<String>,
    pub summary: String,
}

#[derive(Debug, Default)]
pub struct NotificationsModel {
    items: VecDeque<Notification>,
    acks: BTreeMap<u64, AttentionAck>,
    next_id: u64,
}

impl NotificationsModel {
    pub fn apply_event(&mut self, event: &Event, phases: &BTreeMap<String, ThreadRunPhase>) {
        let (kind, run_id, summary) = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) => {
                let (kind, summary) = match to {
                    AgentRunPhase::Done => (
                        NotificationKind::RunCompleted,
                        format!("Run {run_id} completed"),
                    ),
                    AgentRunPhase::Error => (
                        NotificationKind::RunFailed {
                            reason: reason.clone(),
                        },
                        match reason.as_deref() {
                            Some("cancelled") => format!("Run {run_id} cancelled"),
                            Some(reason) => format!("Run {run_id} failed: {reason}"),
                            None => format!("Run {run_id} failed"),
                        },
                    ),
                    AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => {
                        return;
                    }
                };
                (kind, Some(run_id.clone()), summary)
            }
            EventKind::Tool(ToolEvent::ApprovalRequested { tool_name, call_id }) => {
                let mut waiting = phases
                    .iter()
                    .filter(|(_, phase)| **phase == ThreadRunPhase::Waiting);
                let run_id = match (waiting.next(), waiting.next()) {
                    (Some((run_id, _)), None) => Some(run_id.clone()),
                    _ => None,
                };
                (
                    NotificationKind::ApprovalPending {
                        tool_name: tool_name.clone(),
                        call_id: call_id.clone(),
                    },
                    run_id,
                    format!("Approval requested: {tool_name}"),
                )
            }
            EventKind::Orchestrator(OrchestratorEvent::MergeApprovalRequested {
                goal_id, ..
            }) => (
                NotificationKind::MergeApprovalPending {
                    goal_id: goal_id.clone(),
                },
                None,
                format!("Merge approval requested: {goal_id}"),
            ),
            EventKind::Lifecycle(_)
            | EventKind::Tool(_)
            | EventKind::Orchestrator(_)
            | EventKind::Ledger(_)
            | EventKind::Message(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Diagnostic(_)
            | EventKind::Ownership(_)
            | EventKind::Snapshot(_) => return,
        };
        let phase = match &kind {
            NotificationKind::RunCompleted => ThreadRunPhase::Done,
            NotificationKind::RunFailed { .. } => ThreadRunPhase::Error,
            NotificationKind::ApprovalPending { .. }
            | NotificationKind::MergeApprovalPending { .. } => ThreadRunPhase::Waiting,
        };
        // Never reuse an id while a renderer may still hold its revision.
        let Some(next_id) = self.next_id.checked_add(1) else {
            return;
        };
        let mut ack = AttentionAck::default();
        // A fresh ack starts at sequence zero, so its first observation cannot exhaust it.
        if ack.observe(phase).is_err() {
            return;
        }
        let id = self.next_id;
        self.next_id = next_id;
        self.items.push_back(Notification {
            id,
            kind,
            run_id,
            summary,
        });
        self.acks.insert(id, ack);
        if self.items.len() > MAX_NOTIFICATIONS
            && let Some(oldest) = self.items.pop_front()
        {
            self.acks.remove(&oldest.id);
        }
    }

    pub fn items(&self) -> impl Iterator<Item = &Notification> {
        self.items.iter()
    }
    pub fn is_unread(&self, id: u64) -> bool {
        self.acks.get(&id).is_some_and(AttentionAck::is_unread)
    }
    pub fn unread_count(&self) -> usize {
        self.acks.values().filter(|ack| ack.is_unread()).count()
    }
    pub fn revision(&self, id: u64) -> Option<DisplayRevision> {
        self.acks.get(&id).map(AttentionAck::revision)
    }
    pub fn acknowledge(
        &mut self,
        id: u64,
        displayed: Option<&DisplayRevision>,
        focused: Option<bool>,
    ) -> bool {
        self.acks
            .get_mut(&id)
            .is_some_and(|ack| ack.acknowledge_surface(displayed, focused))
    }
}

// allow: SIZE_OK — the task requires all twelve contract tests inline with this model.
#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::{AgentRunPhase, Event, LifecycleEvent, ToolEvent};
    use std::collections::BTreeMap;
    use workspace_ui::ThreadRunPhase;

    fn transition(to: AgentRunPhase, reason: Option<&str>) -> Event {
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-3".into(),
            from: AgentRunPhase::Running,
            to,
            reason: reason.map(str::to_owned),
        })
    }
    fn approval() -> Event {
        Event::new(ToolEvent::ApprovalRequested {
            tool_name: "shell".into(),
            call_id: "call-1".into(),
        })
    }
    fn completed() -> NotificationsModel {
        let mut model = NotificationsModel::default();
        model.apply_event(&transition(AgentRunPhase::Done, None), &BTreeMap::new());
        model
    }

    #[test]
    fn done_event_pushes_unread_notification() {
        // Given: an empty model. When: a run completes.
        let model = completed();
        // Then: one targeted, unread completion is retained.
        let item = model.items().next().unwrap();
        assert_eq!(item.kind, NotificationKind::RunCompleted);
        assert_eq!(item.run_id.as_deref(), Some("run-3"));
        assert_eq!(item.summary, "Run run-3 completed");
        assert_eq!(model.items().count(), 1);
        assert!(model.is_unread(item.id));
    }
    #[test]
    fn error_event_pushes_unread_notification_with_reason() {
        for reason in [Some("connection lost"), None] {
            // Given: an empty model.
            let mut model = NotificationsModel::default();
            // When: the run fails, with or without a reason.
            model.apply_event(&transition(AgentRunPhase::Error, reason), &BTreeMap::new());
            // Then: the failure retains its reason and target.
            let item = model.items().next().unwrap();
            assert_eq!(
                item.kind,
                NotificationKind::RunFailed {
                    reason: reason.map(str::to_owned)
                }
            );
            assert_eq!(item.run_id.as_deref(), Some("run-3"));
            assert!(item.summary.contains(reason.unwrap_or("failed")));
            assert!(model.is_unread(item.id));
        }
    }
    #[test]
    fn cancelled_reason_surfaces_as_failed_notification() {
        // Given: an empty model.
        let mut model = NotificationsModel::default();
        // When: cancellation terminates the run.
        model.apply_event(
            &transition(AgentRunPhase::Error, Some("cancelled")),
            &BTreeMap::new(),
        );
        // Then: cancellation remains a failure with a visible cancellation summary.
        let item = model.items().next().unwrap();
        assert_eq!(
            item.kind,
            NotificationKind::RunFailed {
                reason: Some("cancelled".into())
            }
        );
        assert!(item.summary.contains("cancelled"));
        assert!(model.is_unread(item.id));
    }
    #[test]
    fn non_terminal_transition_pushes_nothing() {
        for phase in [
            AgentRunPhase::Pending,
            AgentRunPhase::Running,
            AgentRunPhase::Waiting,
        ] {
            // Given: an empty model.
            let mut model = NotificationsModel::default();
            // When: a non-terminal transition arrives.
            model.apply_event(&transition(phase, None), &BTreeMap::new());
            // Then: no notification is created.
            assert_eq!(model.items().count(), 0);
            assert_eq!(model.unread_count(), 0);
        }
    }
    #[test]
    fn provider_request_events_push_nothing() {
        use event_bus::{ProviderEvent, ProviderFailureKind};
        let events = [
            ProviderEvent::RequestCompleted {
                request_id: "request-1".into(),
                provider: "demo".into(),
                profile: None,
                protocol: "demo".into(),
                model: "demo".into(),
                streaming: true,
                duration_ms: 1,
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                finish_reason: "stop".into(),
                run_id: Some("run-3".into()),
            },
            ProviderEvent::RequestFailed {
                request_id: "request-2".into(),
                provider: "demo".into(),
                profile: None,
                protocol: "demo".into(),
                model: "demo".into(),
                streaming: true,
                duration_ms: 1,
                failure: ProviderFailureKind::Timeout,
                run_id: Some("run-3".into()),
            },
        ];
        for event in events {
            // Given: an empty model. When: a provider attempt terminates.
            let mut model = NotificationsModel::default();
            model.apply_event(&Event::new(event), &BTreeMap::new());
            // Then: attempts are not run notifications.
            assert_eq!(model.items().count(), 0);
        }
    }
    #[test]
    fn approval_requested_resolves_single_waiting_run() {
        // Given: exactly one waiting run, alongside a running run.
        let mut model = NotificationsModel::default();
        let phases = BTreeMap::from([
            ("run-3".into(), ThreadRunPhase::Waiting),
            ("run-4".into(), ThreadRunPhase::Running),
        ]);
        // When: tool approval is requested.
        model.apply_event(&approval(), &phases);
        // Then: the notification targets only the waiting run.
        let item = model.items().next().unwrap();
        assert_eq!(
            item.kind,
            NotificationKind::ApprovalPending {
                tool_name: "shell".into(),
                call_id: "call-1".into()
            }
        );
        assert_eq!(item.run_id.as_deref(), Some("run-3"));
        assert_eq!(item.summary, "Approval requested: shell");
        assert!(model.is_unread(item.id));
    }
    #[test]
    fn approval_requested_with_zero_or_multiple_waiting_runs_has_no_target() {
        for phases in [
            BTreeMap::new(),
            BTreeMap::from([
                ("run-3".into(), ThreadRunPhase::Waiting),
                ("run-4".into(), ThreadRunPhase::Waiting),
            ]),
        ] {
            // Given: no uniquely waiting run.
            let mut model = NotificationsModel::default();
            // When: tool approval is requested.
            model.apply_event(&approval(), &phases);
            // Then: the notification has no guessed target.
            let item = model.items().next().unwrap();
            assert_eq!(item.run_id, None);
            assert!(model.is_unread(item.id));
        }
    }
    #[test]
    fn merge_approval_requested_pushes_unread_without_run_target() {
        use event_bus::{CiState, GateSnapshot, MergeBinding, OrchestratorEvent};
        // Given: a merge approval binding.
        let binding = MergeBinding {
            token_id: "token-1".into(),
            repo: "owner/repo".into(),
            pr_number: 1,
            head_sha: "sha".into(),
            snapshot: GateSnapshot {
                repo: "owner/repo".into(),
                pr_number: 1,
                base_ref: "main".into(),
                head_sha: "sha".into(),
                ci: CiState::Green,
                criteria_round: 1,
                review_round: 1,
                reviewer_run_id: "run-3".into(),
            },
        };
        let mut model = NotificationsModel::default();
        // When: merge approval is requested while one run waits.
        model.apply_event(
            &Event::new(OrchestratorEvent::MergeApprovalRequested {
                goal_id: "goal-1".into(),
                binding,
            }),
            &BTreeMap::from([("run-3".into(), ThreadRunPhase::Waiting)]),
        );
        // Then: the goal notification is unread but never targets a run.
        let item = model.items().next().unwrap();
        assert_eq!(
            item.kind,
            NotificationKind::MergeApprovalPending {
                goal_id: "goal-1".into()
            }
        );
        assert_eq!(item.run_id, None);
        assert_eq!(item.summary, "Merge approval requested: goal-1");
        assert!(model.is_unread(item.id));
    }
    #[test]
    fn ack_requires_displayed_focused_and_matching_revision() {
        for (displayed, focused, accepted) in [
            (false, Some(true), false),
            (true, None, false),
            (true, Some(false), false),
            (true, Some(true), true),
        ] {
            // Given: an unread notification and its render revision.
            let mut model = completed();
            let id = model.items().next().unwrap().id;
            let revision = model.revision(id).unwrap();
            // When: the surface reports display and focus evidence.
            let result = model.acknowledge(id, displayed.then_some(&revision), focused);
            // Then: only displayed and focused evidence acknowledges it.
            assert_eq!(result, accepted);
            assert_eq!(model.is_unread(id), !accepted);
        }
    }
    #[test]
    fn stale_revision_ack_is_ignored() {
        // Given: a render token belonging to an earlier notification lifetime.
        let old = completed();
        let stale = old.revision(old.items().next().unwrap().id).unwrap();
        let mut model = completed();
        let id = model.items().next().unwrap().id;
        // When: the old token acknowledges the new notification with the same numeric id.
        assert!(!model.acknowledge(id, Some(&stale), Some(true)));
        // Then: the new notification remains unread.
        assert!(model.is_unread(id));
    }
    #[test]
    fn unread_count_tracks_acknowledgement() {
        // Given: two unread notifications.
        let mut model = completed();
        model.apply_event(&approval(), &BTreeMap::new());
        let id = model.items().next().unwrap().id;
        let revision = model.revision(id).unwrap();
        assert_eq!(model.unread_count(), 2);
        // When: one notification is acknowledged.
        assert!(model.acknowledge(id, Some(&revision), Some(true)));
        // Then: one unread remains and unknown ids cannot be acknowledged.
        assert_eq!(model.unread_count(), 1);
        assert!(!model.is_unread(u64::MAX));
        assert!(model.revision(u64::MAX).is_none());
        assert!(!model.acknowledge(u64::MAX, Some(&revision), Some(true)));
    }
    #[test]
    fn capacity_evicts_oldest_notification_and_ack() {
        // Given: a full notification queue and its oldest revision.
        let mut model = completed();
        let oldest = model.items().next().unwrap().id;
        let revision = model.revision(oldest).unwrap();
        for _ in 1..MAX_NOTIFICATIONS {
            model.apply_event(&approval(), &BTreeMap::new());
        }
        let before: Vec<_> = model.items().map(|item| item.id).collect();
        // When: a new notification exceeds capacity.
        model.apply_event(&approval(), &BTreeMap::new());
        // Then: FIFO eviction removes only the oldest notification and its ack.
        let after: Vec<_> = model.items().map(|item| item.id).collect();
        assert_eq!(after.len(), MAX_NOTIFICATIONS);
        assert_eq!(&after[..MAX_NOTIFICATIONS - 1], &before[1..]);
        assert!(after.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(model.revision(oldest).is_none());
        assert!(!model.is_unread(oldest));
        assert!(!model.acknowledge(oldest, Some(&revision), Some(true)));
        assert_eq!(model.unread_count(), MAX_NOTIFICATIONS);
    }
}
