//! Stopping a chat or one run of it.

use runtime::RunId;

use super::{CommandService, HostEvent, NoticeOutcome, RejectKind};

impl CommandService {
    pub(super) fn stop_run(&mut self, thread_id: String, run_id: String) -> Vec<HostEvent> {
        let target = run_id.parse::<RunId>().ok();
        let root = self
            .bindings
            .chat_runs
            .get(&thread_id)
            .or_else(|| self.bindings.goal_runs.get(&thread_id));
        let Some(target) = target.filter(|target| {
            root.is_some_and(|root| self.runtime.live_descendants(*root).contains(target))
                || (self.runtime.graceful_stop_requested(*target)
                    && self.runtime.thread_of_live_run(*target).as_deref()
                        == Some(thread_id.as_str()))
        }) else {
            return vec![HostEvent::ChatNotice {
                outcome: NoticeOutcome::NotApplied,
                thread_id,
                text: "The agent is no longer active in this conversation".into(),
            }];
        };
        match self.stop_for_host(target, runtime::StopScope::SelfOnly) {
            Ok(()) => Vec::new(),
            Err(error) => vec![HostEvent::ChatRejected {
                kind: RejectKind::Failed,
                thread_id,
                reason: format!(
                    "Stop could not be confirmed; any applied stop remains in effect. Retry after resolving: {error}"
                ),
            }],
        }
    }

    pub(super) fn stop_chat(&mut self, thread_id: String) -> Vec<HostEvent> {
        let Some(&run_id) = self
            .bindings
            .chat_runs
            .get(&thread_id)
            .or_else(|| self.bindings.goal_runs.get(&thread_id))
        else {
            return vec![HostEvent::ChatNotice {
                outcome: NoticeOutcome::NotApplied,
                thread_id,
                text: "No chat run to stop".into(),
            }];
        };
        let scope = if self.bindings.stop_marked.contains(&thread_id) {
            runtime::StopScope::Subtree
        } else {
            match self.runtime.inspect_agent(run_id) {
                Ok(run) => match run.phase {
                    event_bus::AgentRunPhase::Pending
                    | event_bus::AgentRunPhase::Running
                    | event_bus::AgentRunPhase::Waiting => runtime::StopScope::SelfOnly,
                    event_bus::AgentRunPhase::Stopped
                        if !self.runtime.live_descendants(run_id).is_empty() =>
                    {
                        runtime::StopScope::Subtree
                    }
                    _ if self.runtime.graceful_stop_requested(run_id) => {
                        runtime::StopScope::SelfOnly
                    }
                    _ => {
                        return vec![HostEvent::ChatNotice {
                            outcome: NoticeOutcome::NotApplied,
                            thread_id,
                            text: "Already stopped; use /continue or send a message to resume"
                                .into(),
                        }];
                    }
                },
                // Admission-pending runs have no inspection yet. stop validates
                // the ID against admissions before accepting this first press.
                Err(runtime::RuntimeError::UnknownRun { .. }) => runtime::StopScope::SelfOnly,
                Err(error) => {
                    return vec![HostEvent::ChatRejected {
                        kind: RejectKind::Failed,
                        thread_id,
                        reason: error.to_string(),
                    }];
                }
            }
        };
        if let Err(error) = self.stop_for_host(run_id, scope) {
            return vec![HostEvent::ChatRejected {
                kind: RejectKind::Failed,
                thread_id,
                reason: format!(
                    "Stop could not be confirmed; any applied stop remains in effect. Retry after resolving: {error}"
                ),
            }];
        }
        match scope {
            runtime::StopScope::SelfOnly => {
                self.bindings.stop_marked.insert(thread_id.clone());
            }
            runtime::StopScope::Subtree => {
                self.bindings.stop_marked.remove(&thread_id);
            }
        }
        self.bindings.stopped_by_us.insert(thread_id.clone());
        let running_children = self.running_children(&thread_id).unwrap_or(0);
        vec![HostEvent::ChatStopped {
            thread_id,
            run_id: run_id.to_string(),
            running_children,
        }]
    }
}
