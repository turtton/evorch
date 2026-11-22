//! CLI invocations (ADR 0045 §6, §10): one `evorch run` command line as the
//! core sees it.
//!
//! An invocation owns only what it started: the run incarnation it created and
//! that run's subtree. When it ends (Ctrl-C, a lost connection, the end of the
//! command line) the core stops exactly that. Input it sent to a run it does not
//! own is taken back while it is still queued and detached once the run has taken
//! it; the other run is never stopped.
use crate::{
    ids::{InvocationId, RunId, SubmissionId, ThreadId},
    limits::{MAX_TEXT_BYTES, MAX_TITLE_BYTES},
    validation::{ValidationError, check_bytes, check_line, check_text},
    views::InputReceiptState,
};
use serde::{Deserialize, Serialize};

/// Most bytes of a project root a client names.
pub const MAX_INVOCATION_ROOT_BYTES: usize = 4096;
/// Most bytes of a run's final text an [`InvocationView`] carries.
pub const MAX_INVOCATION_FINAL_BYTES: usize = 64 * 1024;

/// The role the invocation's run takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationRole {
    Orchestrator,
    Explorer,
    Worker,
    Reviewer,
    WebResearcher,
    Planner,
    Advisor,
}

/// Start one invocation. Only a local CLI connection may send it: the root is
/// a path of the machine that runs the core, which no remote surface names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartInvocation {
    /// Chosen by the client. Resending the same id with the same request does
    /// not start anything twice; another request under the id is a conflict.
    pub invocation: InvocationId,
    pub project_root: String,
    pub role: InvocationRole,
    pub prompt: String,
    /// Continue this thread instead of creating a new one. Input to a thread
    /// whose run is live goes to that run (not owned); otherwise a new
    /// incarnation of the thread's conversation starts and is owned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadId>,
}

impl StartInvocation {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line(
            "project_root",
            &self.project_root,
            MAX_INVOCATION_ROOT_BYTES,
            false,
        )?;
        check_text("prompt", &self.prompt, MAX_TEXT_BYTES, false)
    }
}

/// Why an invocation ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// The person interrupted the command (Ctrl-C).
    Interrupted,
    /// The command has what it came for and leaves.
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndInvocation {
    pub invocation: InvocationId,
    pub reason: EndReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationOwnership {
    /// The invocation started the run incarnation and owns it with its subtree.
    OwnsRun,
    /// The invocation only sent input to a run somebody else started.
    InputOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationStarted {
    pub invocation: InvocationId,
    pub thread: ThreadId,
    pub run: RunId,
    pub ownership: InvocationOwnership,
    /// The same request answered again from the table.
    pub already_started: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationEnded {
    pub invocation: InvocationId,
    /// Runs of the invocation's subtree the core asked to stop.
    pub stopped_runs: u32,
    /// Queued inputs taken back.
    pub withdrawn_inputs: u32,
    /// Inputs the other run had already taken; the run was left alone.
    pub detached_inputs: u32,
    /// The invocation had ended before (a repeat is not an error).
    pub already_ended: bool,
}

/// Where an invocation stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationState {
    Running,
    /// Waiting for an answer to a question or a decision on an approval.
    WaitingForInput,
    Succeeded,
    Failed,
    /// The run was stopped (by somebody, or by this invocation ending).
    Stopped,
    /// The invocation ended and left nothing running.
    Ended,
}

impl InvocationState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Stopped | Self::Ended
        )
    }
}

/// Input the invocation sent to a run it does not own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationInput {
    pub run: RunId,
    pub submission: SubmissionId,
    pub receipt: InputReceiptState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetInvocation {
    pub invocation: InvocationId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationView {
    pub invocation: InvocationId,
    pub thread: ThreadId,
    pub state: InvocationState,
    /// The run incarnation the invocation owns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<InvocationInput>,
    /// What the owned run answered with, once it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_text: Option<String>,
    /// Why it failed or stopped, when the core knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl InvocationView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(text) = &self.final_text {
            check_bytes("final_text", text, MAX_INVOCATION_FINAL_BYTES, true)?;
        }
        if let Some(detail) = &self.detail {
            check_bytes("detail", detail, MAX_TITLE_BYTES * 4, true)?;
        }
        Ok(())
    }
}
