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
    /// UUID v4 naming a composer message across its delivery states. Empty for
    /// chats the host builds itself, such as goal prompts, resumes and answers.
    #[serde(default)]
    pub submission_id: String,
    /// Root of the project the thread belongs to; its runs work there.
    #[serde(default)]
    pub project_root: Option<std::path::PathBuf>,
    #[serde(default)]
    pub composer_role: super::composer::ComposerRole,
    #[serde(default)]
    pub attachments: Vec<workbench_protocol::commands::AttachmentRef>,
    pub thread_id: String,
    /// What the user typed. The runtime expands `skills` into the prompt the
    /// agent sees; the words stay as typed everywhere else.
    pub text: String,
    /// Names of the skills the user attached with `@name` mentions in `text`.
    #[serde(default)]
    pub skills: Vec<String>,
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
    /// The user's explicit resume of held lane notices for a lead thread: the runtime
    /// clears its durable Stop and the wake guards of the lanes it leads.
    ResumeLaneDeliveries {
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
    /// Link a thread to a pull request named by a URL or `owner/repo#N` (ADR 0043). The host
    /// acquires the thread's write mode first; `expected_revision` is the lifecycle revision
    /// the display read, so a decision made meanwhile is never overwritten.
    LinkPullRequest {
        thread_id: String,
        locator: String,
        expected_revision: u64,
    },
    /// End the thread's link. Derived threads that already took it keep theirs.
    UnlinkPullRequest {
        thread_id: String,
        expected_revision: u64,
    },
    /// Fetch the thread's pull request now. Also makes the process that holds the thread
    /// poll it again.
    RefreshPullRequest {
        thread_id: String,
    },
    /// Link the thread to a pull request a tool output suggested.
    ConfirmPullRequestCandidate {
        thread_id: String,
        candidate_id: u64,
        expected_revision: u64,
    },
    /// Ignore a suggested pull request.
    DismissPullRequestCandidate {
        thread_id: String,
        candidate_id: u64,
    },
    /// Turn the pull request's auto fix on for this thread, or off. Revisions are what the
    /// person saw: a refusal follows when either moved.
    SetPullRequestAutoFix {
        thread_id: String,
        link_revision: u64,
        policy_revision: u64,
        enabled: bool,
    },
    /// Start the automatic count again and turn auto fix on. From a thread other than the one
    /// that has auto fix this moves it, so `confirmed_previous` names the thread the person
    /// confirmed leaving; `None` is a reset that needs no confirmation.
    ResetPullRequestAutoFixBudget {
        thread_id: String,
        link_revision: u64,
        policy_revision: u64,
        confirmed_previous: Option<String>,
    },
    /// Turn auto fix on here after the person confirmed which thread had it.
    ChangePullRequestAutoFixDestination {
        thread_id: String,
        link_revision: u64,
        policy_revision: u64,
        confirmed_previous: Option<String>,
    },
    /// Send the pull request's failing head to the agent now. `expected_head` is the commit
    /// the person saw: another head is refused, never sent in its place.
    SendPullRequestNotice {
        thread_id: String,
        link_revision: u64,
        expected_head: String,
    },
    /// A thread was derived from another: it may take the parent's open pull request.
    /// Idempotent, and sent when the thread is created, before any run exists.
    InheritPullRequest {
        parent_thread_id: String,
        child_thread_id: String,
        kind: PullRequestDerivation,
    },
}

/// How a thread was derived, for [`WorkbenchCommand::InheritPullRequest`]. A lane never
/// inherits, so it has no variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullRequestDerivation {
    Fork,
    Rewind,
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
