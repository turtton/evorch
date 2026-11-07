//! Conversions between the GUI's command types and the host's.

use workbench_host::commands::{
    ChatInput, ComposerRole, ContinueInput, GoalInput, HostCommand, HostEvent, PacketReference,
    ReferenceKind,
};

use crate::model::commands::{ChatSubmission, GoalSubmission, LoopEvent, WorkbenchCommand};
use crate::model::composer;

fn composer_role(role: composer::ComposerRole) -> ComposerRole {
    match role {
        composer::ComposerRole::Worker => ComposerRole::Worker,
        composer::ComposerRole::Orchestrator => ComposerRole::Orchestrator,
    }
}

pub(super) fn chat_input(chat: ChatSubmission) -> ChatInput {
    ChatInput {
        submission_id: chat.submission_id,
        project_root: chat.project_root,
        composer_role: composer_role(chat.composer_role),
        images: chat.images,
        thread_id: chat.thread_id,
        text: chat.text,
        skills: chat.skills,
        model_preference: chat.model_preference,
        fork_seed: chat.fork_seed,
    }
}

pub(super) fn goal_input(goal: GoalSubmission) -> GoalInput {
    GoalInput {
        project_root: goal.project_root,
        delegation_value: goal.delegation_value,
        project_id: goal.project_id,
        thread_id: goal.thread_id,
        goal: goal.goal,
        references: goal
            .references
            .into_iter()
            .map(|reference| PacketReference {
                kind: match reference.kind {
                    crate::model::commands::ReferenceKind::Packet => ReferenceKind::Packet,
                    crate::model::commands::ReferenceKind::Issue => ReferenceKind::Issue,
                },
                value: reference.value,
            })
            .collect(),
        constraints: goal.constraints,
    }
}

pub(super) fn host_command(command: WorkbenchCommand) -> HostCommand {
    match command {
        WorkbenchCommand::AnswerUserQuestion {
            thread_id,
            question_id,
            answer,
        } => HostCommand::AnswerUserQuestion {
            thread_id,
            question_id,
            answer,
        },
        WorkbenchCommand::SendChat(chat) => HostCommand::SendChat(chat_input(chat)),
        WorkbenchCommand::ContinueChat(continuation) => HostCommand::ContinueChat(ContinueInput {
            project_root: continuation.project_root,
            thread_id: continuation.thread_id,
            composer_role: composer_role(continuation.composer_role),
            model_preference: continuation.model_preference,
            automatic: continuation.automatic,
        }),
        WorkbenchCommand::CompactChat { thread_id } => HostCommand::CompactChat { thread_id },
        WorkbenchCommand::StopChat { thread_id } => HostCommand::StopChat { thread_id },
        WorkbenchCommand::StopRun { thread_id, run_id } => {
            HostCommand::StopRun { thread_id, run_id }
        }
        WorkbenchCommand::DeliverFollowUpsNextTurn { thread_id } => {
            HostCommand::DeliverFollowUpsNextTurn { thread_id }
        }
        WorkbenchCommand::SubmitGoal(goal) => HostCommand::SubmitGoal(goal_input(goal)),
        WorkbenchCommand::SetGoalReview {
            thread_id,
            goal_id,
            enabled,
        } => HostCommand::SetGoalReview {
            thread_id,
            goal_id,
            enabled,
        },
        WorkbenchCommand::SetGoalChecksPaused {
            thread_id,
            goal_id,
            paused,
        } => HostCommand::SetGoalChecksPaused {
            thread_id,
            goal_id,
            paused,
        },
        WorkbenchCommand::DecideToolApproval { call_id, approved } => {
            HostCommand::DecideToolApproval { call_id, approved }
        }
        WorkbenchCommand::SetWebToolsEnabled { enabled } => {
            HostCommand::SetWebToolsEnabled { enabled }
        }
        WorkbenchCommand::ResumeLaneDeliveries { thread_id } => {
            HostCommand::ResumeLaneDeliveries { thread_id }
        }
        WorkbenchCommand::LinkPullRequest { .. }
        | WorkbenchCommand::UnlinkPullRequest { .. }
        | WorkbenchCommand::RefreshPullRequest { .. }
        | WorkbenchCommand::SetPullRequestAutoFix { .. }
        | WorkbenchCommand::ResetPullRequestAutoFixBudget { .. }
        | WorkbenchCommand::ChangePullRequestAutoFixDestination { .. }
        | WorkbenchCommand::SendPullRequestNotice { .. }
        | WorkbenchCommand::ConfirmPullRequestCandidate { .. }
        | WorkbenchCommand::DismissPullRequestCandidate { .. }
        | WorkbenchCommand::InheritPullRequest { .. } => {
            unreachable!("pull request operations are routed by the sink, not the host")
        }
    }
}

/// The desktop loop shows only the reason text; the refusal kind is for the protocol.
pub(super) fn loop_event(event: HostEvent) -> LoopEvent {
    match event {
        HostEvent::UserAnswerSaved { question_id, .. } => {
            LoopEvent::UserAnswerSaved { question_id }
        }
        HostEvent::ChatAccepted { thread_id, run_id } => {
            LoopEvent::ChatAccepted { thread_id, run_id }
        }
        HostEvent::ChatStopped {
            thread_id,
            run_id,
            running_children,
        } => LoopEvent::ChatStopped {
            thread_id,
            run_id,
            running_children,
        },
        HostEvent::ChatNotice {
            thread_id, text, ..
        } => LoopEvent::ChatNotice { thread_id, text },
        HostEvent::ChatRejected {
            thread_id, reason, ..
        } => LoopEvent::ChatRejected { thread_id, reason },
        HostEvent::GoalAccepted { thread_id, goal_id } => {
            LoopEvent::GoalAccepted { thread_id, goal_id }
        }
        HostEvent::CommandRejected { reason, .. } => LoopEvent::CommandRejected { reason },
    }
}
