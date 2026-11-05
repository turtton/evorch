//! The host's own command vocabulary.
//!
//! These are the shapes the command services act on. Every ingress translates
//! into them: the protocol handler ([`crate::ingress`]) from
//! `workbench_protocol::commands::Command`, and the in-process desktop adapter
//! from its legacy command type. Keeping one vocabulary is what makes both
//! paths go through the same validation.

use std::path::PathBuf;

/// Which composer a message was typed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComposerRole {
    #[default]
    Worker,
    Orchestrator,
}

/// A user message for a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatInput {
    /// Names a composer message across its delivery states. Empty for chats the
    /// host builds itself, such as goal prompts, resumes and answers.
    pub submission_id: String,
    /// Root of the project the thread belongs to; its runs work there.
    pub project_root: Option<PathBuf>,
    pub composer_role: ComposerRole,
    pub images: Vec<runtime::DelegateImage>,
    pub thread_id: String,
    /// What the user typed. The runtime expands `skills` into the prompt the
    /// agent sees; the words stay as typed everywhere else.
    pub text: String,
    pub skills: Vec<String>,
    pub model_preference: Option<runtime::ModelPreference>,
    /// Completed-turn boundary a forked thread starts from.
    pub fork_seed: Option<runtime::ChatForkSeed>,
}

/// A request to continue a thread; not a new user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinueInput {
    pub project_root: Option<PathBuf>,
    pub thread_id: String,
    pub composer_role: ComposerRole,
    pub model_preference: Option<runtime::ModelPreference>,
    /// The runtime asked to resume the root for held subagent reports. This is
    /// not user input and does not renew the automatic-continuation budget.
    /// Only the in-process adapter can set it; the protocol cannot.
    pub automatic: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceKind {
    Packet,
    Issue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketReference {
    pub kind: ReferenceKind,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalInput {
    /// Root of the project the thread belongs to; its runs work there.
    pub project_root: Option<PathBuf>,
    /// Team mode delegation value, when the goal is handed to a dynamic team.
    pub delegation_value: Option<String>,
    pub project_id: String,
    pub thread_id: String,
    pub goal: String,
    pub references: Vec<PacketReference>,
    pub constraints: Vec<String>,
}

/// Every state-changing request the command services execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCommand {
    AnswerUserQuestion {
        thread_id: String,
        question_id: String,
        answer: String,
    },
    SendChat(ChatInput),
    ContinueChat(ContinueInput),
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
    SubmitGoal(GoalInput),
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
    /// Approval through the call id the tool executor announced. The protocol's
    /// approval registry (ADR 0045 P4) does not feed this yet.
    DecideToolApproval {
        call_id: String,
        approved: bool,
    },
    SetWebToolsEnabled {
        enabled: bool,
    },
    /// Make the runtime redeliver the lane notifications held for a thread. Desktop
    /// only; the protocol has no such command yet.
    ResumeLaneDeliveries {
        thread_id: String,
    },
}

impl HostCommand {
    /// The thread whose ownership authorises this command, if it targets one.
    pub fn thread(&self) -> Option<&str> {
        match self {
            Self::SendChat(value) => Some(&value.thread_id),
            Self::ContinueChat(value) => Some(&value.thread_id),
            Self::SubmitGoal(value) => Some(&value.thread_id),
            Self::SetGoalReview { thread_id, .. }
            | Self::SetGoalChecksPaused { thread_id, .. }
            | Self::CompactChat { thread_id }
            | Self::StopChat { thread_id }
            | Self::StopRun { thread_id, .. }
            | Self::DeliverFollowUpsNextTurn { thread_id }
            | Self::ResumeLaneDeliveries { thread_id }
            | Self::AnswerUserQuestion { thread_id, .. } => Some(thread_id),
            Self::DecideToolApproval { .. } | Self::SetWebToolsEnabled { .. } => None,
        }
    }
}

/// Why a command was refused. The in-process adapter shows only the reason
/// text; the protocol handler turns the kind into an error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectKind {
    /// The caller does not own the thread (any more).
    Fenced,
    NotFound,
    /// The request conflicts with the current state of the thread or entity.
    Conflict,
    /// Another command settled the question first.
    AlreadyResolved,
    /// A job that excludes this command is in progress.
    Busy,
    /// The host cannot do this with its current configuration.
    Unsupported,
    Invalid,
    /// Storage could not be read; the same request may succeed later.
    Unavailable,
    /// The runtime or storage failed.
    Failed,
}

/// Whether a notice reports that the command took effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeOutcome {
    Applied,
    NotApplied,
}

/// What a command produced. Mirrors the events the desktop loop consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// Emitted only after durable storage accepted the answer. `newly_saved` is false
    /// when the same answer was already stored, so a repeat is told from the first save.
    UserAnswerSaved {
        question_id: String,
        newly_saved: bool,
    },
    ChatAccepted {
        thread_id: String,
        run_id: String,
    },
    /// A successful operator stop.
    ChatStopped {
        thread_id: String,
        run_id: String,
        running_children: usize,
    },
    /// Informational feedback, not a chat failure.
    ChatNotice {
        thread_id: String,
        text: String,
        outcome: NoticeOutcome,
    },
    ChatRejected {
        thread_id: String,
        reason: String,
        kind: RejectKind,
    },
    GoalAccepted {
        thread_id: String,
        goal_id: String,
    },
    CommandRejected {
        reason: String,
        kind: RejectKind,
    },
}

impl HostEvent {
    pub(crate) fn notice(thread_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::ChatNotice {
            thread_id: thread_id.into(),
            text: text.into(),
            outcome: NoticeOutcome::NotApplied,
        }
    }

    pub(crate) fn applied_notice(thread_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::ChatNotice {
            thread_id: thread_id.into(),
            text: text.into(),
            outcome: NoticeOutcome::Applied,
        }
    }

    pub(crate) fn chat_rejected(
        thread_id: impl Into<String>,
        kind: RejectKind,
        reason: impl Into<String>,
    ) -> Self {
        Self::ChatRejected {
            thread_id: thread_id.into(),
            reason: reason.into(),
            kind,
        }
    }

    pub(crate) fn rejected(kind: RejectKind, reason: impl Into<String>) -> Self {
        Self::CommandRejected {
            reason: reason.into(),
            kind,
        }
    }
}
