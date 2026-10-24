use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferenceKind {
    Packet,
    Issue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacketReference {
    pub kind: ReferenceKind,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSubmission {
    /// Root of the project the thread belongs to; its runs work there.
    #[serde(default)]
    pub project_root: Option<std::path::PathBuf>,
    #[serde(default)]
    pub delegation_value: Option<String>,
    pub project_id: String,
    pub thread_id: String,
    pub goal: String,
    pub references: Vec<PacketReference>,
    pub constraints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSubmission {
    /// Root of the project the thread belongs to; its runs work there.
    #[serde(default)]
    pub project_root: Option<std::path::PathBuf>,
    #[serde(default)]
    pub composer_role: super::composer::ComposerRole,
    #[serde(default)]
    pub images: Vec<runtime::DelegateImage>,
    pub thread_id: String,
    pub text: String,
    #[serde(default)]
    pub model_preference: Option<runtime::ModelPreference>,
    /// Completed-turn boundary a forked thread starts from until it saves its own history.
    #[serde(default)]
    pub fork_seed: Option<runtime::ChatForkSeed>,
}

/// A host continuation request, not a new user message or a new conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatContinuation {
    /// Root of the project the thread belongs to; its runs work there.
    #[serde(default)]
    pub project_root: Option<std::path::PathBuf>,
    pub thread_id: String,
    pub composer_role: super::composer::ComposerRole,
    pub model_preference: Option<runtime::ModelPreference>,
    /// The runtime asked to resume the root for held subagent reports; this is
    /// not user input and does not renew the automatic-continuation budget.
    #[serde(default)]
    pub automatic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkbenchCommand {
    AnswerUserQuestion {
        thread_id: String,
        question_id: String,
        answer: String,
    },
    SendChat(ChatSubmission),
    ContinueChat(ChatContinuation),
    /// Request compaction of the current conversation root without sending a message.
    CompactChat {
        thread_id: String,
    },
    StopChat {
        thread_id: String,
    },
    StopRun {
        thread_id: String,
        run_id: String,
    },
    DeliverFollowUpsNextTurn {
        thread_id: String,
    },
    SubmitGoal(GoalSubmission),
    SetGoalReview {
        thread_id: String,
        goal_id: String,
        enabled: bool,
    },
    SetGoalChecksPaused {
        thread_id: String,
        goal_id: String,
        paused: bool,
    },
    DecideToolApproval {
        call_id: String,
        approved: bool,
    },
    SetWebToolsEnabled {
        enabled: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopEvent {
    /// Host command acknowledgement, emitted only after durable storage accepted the answer.
    UserAnswerSaved {
        question_id: String,
    },
    ChatAccepted {
        thread_id: String,
        run_id: String,
    },
    /// A successful operator stop; also seeds the live composer banner.
    ChatStopped {
        thread_id: String,
        run_id: String,
        running_children: usize,
    },
    /// Informational feedback, not a chat failure.
    ChatNotice {
        thread_id: String,
        text: String,
    },
    ChatRejected {
        thread_id: String,
        reason: String,
    },
    GoalAccepted {
        thread_id: String,
        goal_id: String,
    },
    CommandRejected {
        reason: String,
    },
}

/// Delivers one base context preview composed off the render thread.
pub type ContextPreviewReceiver =
    std::sync::mpsc::Receiver<Result<runtime::base_context::BaseContextReport, String>>;

pub trait CommandSink: Send {
    /// Runtime receipt observation, not an acknowledgement of command acceptance.
    fn follow_up_status(&self, _thread: &str) -> Option<runtime::FollowUpStatus> {
        None
    }
    fn bind_thread_goal(&mut self, _snapshot: &event_bus::ThreadGoalSnapshot, _project: &str) {}
    /// Restore procedure data only; this cannot authorize or wake a root run.
    fn bind_thread_todo(&mut self, _snapshot: &event_bus::ThreadTodoSnapshot) {}
    /// Refresh counts on lifecycle events, including descendants that just settled.
    fn observe_lifecycle(&mut self, _event: &event_bus::Event) {}
    /// Live count for the stopped banner; None leaves fixture/event state intact.
    fn running_children(&mut self, _thread: &str) -> Option<usize> {
        None
    }
    /// Rebind a thread to a root run (escalation handoff), without spawning work.
    fn bind_goal_context(&mut self, _thread: &str, _project: &str, _run: &str) {}
    fn restore_diagnostics(
        &self,
        _run: &str,
    ) -> Result<Option<runtime::restore::RunRestoreDiagnostics>, String> {
        Ok(None)
    }

    /// Compose the base context a new run would receive; `None` without a live runtime.
    /// The result arrives on the receiver once the runtime has resolved model windows.
    fn preview_base_context(
        &self,
        _request: runtime::base_context::BaseContextRequest,
    ) -> Option<ContextPreviewReceiver> {
        None
    }

    /// The last persisted context of a run; `Ok(None)` without a run store or snapshot.
    fn run_context_view(
        &self,
        _run: &str,
    ) -> Result<Option<runtime::base_context::RunContextView>, String> {
        Ok(None)
    }

    fn set_default_cwd(&mut self, _cwd: Option<std::path::PathBuf>) -> Result<(), String> {
        Ok(())
    }

    /// Declares whether a project's own instructions may reach its runs.
    fn set_project_trust(&mut self, _root: std::path::PathBuf, _trusted: bool) {}

    /// Declares a project's trusted allowed directories; the list replaces the previous one.
    fn set_project_allowed_directories(
        &mut self,
        _root: std::path::PathBuf,
        _directories: Vec<std::path::PathBuf>,
    ) {
    }

    /// Declares the directories a thread's runs may read, never write (ADR 0037).
    /// The list replaces the previous one; an empty list removes the declaration.
    fn set_thread_read_only_directories(
        &mut self,
        _thread_id: String,
        _directories: Vec<std::path::PathBuf>,
    ) {
    }

    fn start_background_run(&self, _text: String) -> Option<runtime::RunId> {
        None
    }

    fn submit(&mut self, cmd: WorkbenchCommand) -> Vec<LoopEvent>;
    fn submit_chat_with_permit(
        &mut self,
        chat: ChatSubmission,
        permit: runtime::ownership::OwnerPermit,
    ) -> Vec<LoopEvent> {
        if permit.thread_id != chat.thread_id {
            return vec![LoopEvent::ChatRejected {
                thread_id: chat.thread_id,
                reason: "write-mode permit belongs to another thread".into(),
            }];
        }
        if let Err(error) = permit.validate_generation() {
            return vec![LoopEvent::ChatRejected {
                thread_id: chat.thread_id,
                reason: error.to_string(),
            }];
        }
        self.submit(WorkbenchCommand::SendChat(chat))
    }
    fn poll(&mut self) -> Vec<LoopEvent> {
        Vec::new()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordingSink {
    pub issued: Vec<WorkbenchCommand>,
}

impl CommandSink for RecordingSink {
    fn submit(&mut self, cmd: WorkbenchCommand) -> Vec<LoopEvent> {
        self.issued.push(cmd);
        Vec::new()
    }
}

#[derive(Debug, Clone, Default)]
pub struct FixtureLoopAdapter {
    accepted_goals: u64,
    accepted_chats: u64,
}

impl CommandSink for FixtureLoopAdapter {
    fn submit(&mut self, cmd: WorkbenchCommand) -> Vec<LoopEvent> {
        match cmd {
            WorkbenchCommand::CompactChat { thread_id } => vec![LoopEvent::ChatNotice {
                thread_id,
                text: "Context compaction is unavailable in demo mode".into(),
            }],
            WorkbenchCommand::ContinueChat(_) => vec![LoopEvent::CommandRejected {
                reason: "No resumable conversation in demo mode".into(),
            }],
            WorkbenchCommand::DeliverFollowUpsNextTurn { .. }
            | WorkbenchCommand::StopRun { .. }
            | WorkbenchCommand::StopChat { .. } => Vec::new(),
            WorkbenchCommand::DecideToolApproval { .. }
            | WorkbenchCommand::SetWebToolsEnabled { .. }
            | WorkbenchCommand::AnswerUserQuestion { .. } => Vec::new(),
            WorkbenchCommand::SendChat(submission) => {
                self.accepted_chats = self.accepted_chats.saturating_add(1);
                vec![LoopEvent::ChatAccepted {
                    thread_id: submission.thread_id,
                    run_id: format!("chat-{}", self.accepted_chats),
                }]
            }
            WorkbenchCommand::SubmitGoal(submission) => {
                self.accepted_goals = self.accepted_goals.saturating_add(1);
                vec![LoopEvent::GoalAccepted {
                    thread_id: submission.thread_id,
                    goal_id: format!("goal-{}", self.accepted_goals),
                }]
            }
            // goal 設定の結果はバス上の OrchestratorEvent として届くため、
            // fixture は即応イベントを発行しない。
            WorkbenchCommand::SetGoalReview { .. }
            | WorkbenchCommand::SetGoalChecksPaused { .. } => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalFormModel {
    pub delegation_value: Option<String>,
    pub goal: String,
    pub references: Vec<PacketReference>,
    pub constraints: Vec<String>,
    pub last_accepted: Option<String>,
}

impl GoalFormModel {
    pub fn build_command(
        &self,
        project_id: &str,
        project_root: &std::path::Path,
        thread_id: &str,
    ) -> WorkbenchCommand {
        WorkbenchCommand::SubmitGoal(GoalSubmission {
            project_root: Some(project_root.to_path_buf()),
            delegation_value: self.delegation_value.clone(),
            project_id: project_id.into(),
            thread_id: thread_id.into(),
            goal: self.goal.clone(),
            references: self.references.clone(),
            constraints: self.constraints.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_adapter_accepts_chat_with_sequential_run_ids() {
        // Given: a fresh adapter and submissions on distinct threads.
        let mut adapter = FixtureLoopAdapter::default();
        // When: both chats are submitted in order.
        let events = ["t1", "t2"].map(|thread_id| {
            adapter.submit(WorkbenchCommand::SendChat(ChatSubmission {
                project_root: None,
                fork_seed: None,
                composer_role: crate::model::composer::ComposerRole::Worker,
                images: Vec::new(),
                thread_id: thread_id.into(),
                text: "hi".into(),
                model_preference: None,
            }))
        });
        // Then: each acceptance retains its thread and receives a sequential ID.
        assert_eq!(
            events,
            [
                vec![LoopEvent::ChatAccepted {
                    thread_id: "t1".into(),
                    run_id: "chat-1".into()
                }],
                vec![LoopEvent::ChatAccepted {
                    thread_id: "t2".into(),
                    run_id: "chat-2".into()
                }],
            ]
        );
    }

    #[test]
    fn goal_submission_serializes_references_and_constraints() {
        let command = WorkbenchCommand::SubmitGoal(GoalSubmission {
            project_root: None,
            delegation_value: None,
            project_id: "evorch".into(),
            thread_id: "thread-1".into(),
            goal: "implement issue".into(),
            references: vec![PacketReference {
                kind: ReferenceKind::Issue,
                value: "65".into(),
            }],
            constraints: vec!["model only".into()],
        });

        let json = serde_json::to_string(&command).expect("serialize command");
        let decoded: WorkbenchCommand = serde_json::from_str(&json).expect("deserialize command");

        assert_eq!(decoded, command);
    }

    #[test]
    fn fixture_adapter_accepts_goal_without_a_pr_flow() {
        let mut adapter = FixtureLoopAdapter::default();
        let events = adapter.submit(WorkbenchCommand::SubmitGoal(GoalSubmission {
            project_root: None,
            delegation_value: None,
            project_id: "evorch".into(),
            thread_id: "thread-1".into(),
            goal: "implement issue".into(),
            references: Vec::new(),
            constraints: Vec::new(),
        }));

        assert_eq!(
            events,
            vec![LoopEvent::GoalAccepted {
                thread_id: "thread-1".into(),
                goal_id: "goal-1".into(),
            },]
        );
    }

    #[test]
    fn recording_sink_records_in_order() {
        let mut sink = RecordingSink::default();
        let goal = WorkbenchCommand::SubmitGoal(GoalSubmission {
            project_root: None,
            delegation_value: None,
            project_id: "evorch".into(),
            thread_id: "thread-1".into(),
            goal: "goal".into(),
            references: Vec::new(),
            constraints: Vec::new(),
        });
        let stop = WorkbenchCommand::StopChat {
            thread_id: "thread-1".into(),
        };

        assert!(sink.submit(goal.clone()).is_empty());
        assert!(sink.submit(stop.clone()).is_empty());
        assert_eq!(sink.issued, vec![goal, stop]);
    }
}
