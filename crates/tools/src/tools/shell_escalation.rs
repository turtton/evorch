//! 呼び出し単位の隔離解除を審査する境界。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use sandbox::Sandbox;

use crate::ToolExecutionContext;

/// Shell command access requested for one reviewed invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellAccess {
    Host,
    Network,
}

macro_rules! denial_guidance {
    () => {
        "Do not pursue the same goal through a workaround, a reworded request, or indirect execution. If no safer alternative exists, consult instead: ask_user (a root run asks the user; a subagent asks its parent), escalate when available, or report the blocker in your result."
    };
}

/// Appended only to agent-facing results, never audit or diagnostic reasons.
pub const DENIAL_GUIDANCE: &str = denial_guidance!();

/// Suspension adds no alternative retry route; ordinary sandbox work remains available.
pub const SUSPENSION_GUIDANCE: &str =
    concat!(denial_guidance!(), " Work inside the sandbox can continue.");

/// 明示的な隔離解除の審査結果。拒否理由にはエージェント向けガイダンスを含めない。
pub enum EscalationDecision {
    Approve,
    Deny {
        reason: String,
    },
    /// Automatic review is suspended for the current user-input generation.
    Suspended {
        reason: String,
    },
}

impl EscalationDecision {
    /// Keep agent-only instructions separate from the auditable reason.
    pub fn denial(&self) -> Option<(&str, &'static str)> {
        match self {
            Self::Approve => None,
            Self::Deny { reason } => Some((reason, DENIAL_GUIDANCE)),
            Self::Suspended { reason } => Some((reason, SUSPENSION_GUIDANCE)),
        }
    }
}

/// One direct file-tool call that needs paths outside the sandbox boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAccessRequest {
    pub tool: String,
    /// Canonicalized where the path exists, so links cannot hide the target.
    pub paths: Vec<PathBuf>,
    pub write: bool,
    pub justification: String,
    /// Bounded content (write) or replacement (edit) shown to the reviewer.
    pub change: Option<String>,
}

/// 内部エラーも `Deny` に変換する fail-closed な審査インターフェース。
///
/// Shared by shell sandbox escalation and file-tool boundary escalation, so
/// both follow the same `sandbox.escalation_approval` settings.
#[async_trait::async_trait]
pub trait ShellEscalationGate: Send + Sync {
    /// Gates without a file-access review deny every request.
    async fn decide_file_access(
        &self,
        _ctx: &ToolExecutionContext,
        _request: &FileAccessRequest,
    ) -> EscalationDecision {
        EscalationDecision::Deny {
            reason: "file access escalation is unavailable".into(),
        }
    }

    async fn decide(
        &self,
        ctx: &ToolExecutionContext,
        command: &str,
        justification: &str,
    ) -> EscalationDecision;

    /// Supplies the resolved working directory when the reviewer can use it.
    async fn decide_with_cwd(
        &self,
        ctx: &ToolExecutionContext,
        command: &str,
        justification: &str,
        _cwd: Option<&Path>,
    ) -> EscalationDecision {
        self.decide(ctx, command, justification).await
    }

    async fn decide_scoped_with_cwd(
        &self,
        ctx: &ToolExecutionContext,
        command: &str,
        justification: &str,
        cwd: Option<&Path>,
        _access: ShellAccess,
    ) -> EscalationDecision {
        self.decide_with_cwd(ctx, command, justification, cwd).await
    }
}

#[derive(Clone)]
pub(crate) struct ShellEscalation {
    pub gate: Arc<dyn ShellEscalationGate>,
    pub unsandboxed: Arc<dyn Sandbox>,
}
