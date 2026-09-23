//! 呼び出し単位の隔離解除を審査する境界。

use std::{path::Path, sync::Arc};

use sandbox::Sandbox;

use crate::ToolExecutionContext;

/// 明示的な隔離解除の審査結果。
pub enum EscalationDecision {
    Approve,
    Deny { reason: String },
}

/// 内部エラーも `Deny` に変換する fail-closed な審査インターフェース。
#[async_trait::async_trait]
pub trait ShellEscalationGate: Send + Sync {
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
}

#[derive(Clone)]
pub(crate) struct ShellEscalation {
    pub gate: Arc<dyn ShellEscalationGate>,
    pub unsandboxed: Arc<dyn Sandbox>,
}
