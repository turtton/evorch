use std::{
    future::Future,
    path::Path,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use config::EscalationApproval;
use event_bus::event::diagnostic_codes;
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventBus};
use sandbox::approval::{ApprovalGate, ApprovalOutcome};
use tools::{
    ToolExecutionContext,
    tools::shell_escalation::{
        EscalationDecision, FileAccessRequest, ShellAccess, ShellEscalationGate,
    },
};

use super::{QuickModelReviewer, ReviewContext, ReviewError, ReviewVerdict, review_context};
use crate::runtime::Shared;

const HUMAN_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

pub struct SandboxEscalationGate {
    settings: Arc<Mutex<(EscalationApproval, bool)>>,
    reviewer: Option<Arc<QuickModelReviewer>>,
    pub(super) bus: Arc<EventBus>,
    pub(super) human_timeout: Duration,
    runtime: Option<Weak<Shared>>,
}

impl SandboxEscalationGate {
    pub const fn new(
        settings: Arc<Mutex<(EscalationApproval, bool)>>,
        reviewer: Option<Arc<QuickModelReviewer>>,
        bus: Arc<EventBus>,
    ) -> Self {
        Self {
            settings,
            reviewer,
            bus,
            human_timeout: HUMAN_APPROVAL_TIMEOUT,
            runtime: None,
        }
    }

    pub(crate) fn with_runtime(mut self, runtime: Weak<Shared>) -> Self {
        self.runtime = Some(runtime);
        self
    }

    fn diagnose(&self, ctx: &ToolExecutionContext, severity: DiagnosticSeverity, verdict: &str) {
        self.bus.emit(Event::new(DiagnosticEvent {
            source: "sandbox".into(),
            severity,
            code: diagnostic_codes::ESCALATION_REVIEW.into(),
            detail: verdict.to_owned(),
            run_id: Some(ctx.run_id.clone()),
            thread_id: ctx.thread_id.clone(),
            call_id: ctx.call_id.clone(),
        }));
    }

    async fn human(
        &self,
        ctx: &ToolExecutionContext,
        command: &str,
        justification: &str,
        access: ShellAccess,
    ) -> EscalationDecision {
        let kind = match access {
            ShellAccess::Host => "shell_escalation",
            ShellAccess::Network => "shell_network",
        };
        let call_id = format!(
            "{}:{}",
            ctx.run_id,
            ctx.call_id.as_deref().unwrap_or_default()
        );
        match ApprovalGate::new(self.bus.clone(), self.human_timeout).request_with_input(
            "shell", &call_id,
            Some(serde_json::json!({"command":command, "justification":justification, "kind":kind})),
        ).await {
            ApprovalOutcome::Approved => EscalationDecision::Approve,
            ApprovalOutcome::Denied => EscalationDecision::Deny { reason: "shell escalation denied by user".into() },
            ApprovalOutcome::TimedOut => EscalationDecision::Deny { reason: "shell escalation approval timed out".into() },
        }
    }

    async fn human_file_access(
        &self,
        ctx: &ToolExecutionContext,
        request: &FileAccessRequest,
    ) -> EscalationDecision {
        let call_id = format!(
            "{}:{}",
            ctx.run_id,
            ctx.call_id.as_deref().unwrap_or_default()
        );
        let path = request
            .paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut input = serde_json::json!({
            "kind": "file_access",
            "tool": request.tool,
            "path": path,
            "access": if request.write { "write" } else { "read" },
            "justification": request.justification,
        });
        if let Some(change) = &request.change {
            input["change"] = change.clone().into();
        }
        match ApprovalGate::new(self.bus.clone(), self.human_timeout)
            .request_with_input(&request.tool, &call_id, Some(input))
            .await
        {
            ApprovalOutcome::Approved => EscalationDecision::Approve,
            ApprovalOutcome::Denied => EscalationDecision::Deny {
                reason: "file access escalation denied by user".into(),
            },
            ApprovalOutcome::TimedOut => EscalationDecision::Deny {
                reason: "file access escalation approval timed out".into(),
            },
        }
    }

    /// The approver's view of this run: real user requests and approved rules.
    fn review_context_for(
        &self,
        ctx: &ToolExecutionContext,
        cwd: Option<&Path>,
        command: &str,
    ) -> Option<ReviewContext> {
        let shared = self.runtime.as_ref().and_then(Weak::upgrade)?;
        let run_id = ctx.run_id.parse::<crate::RunId>().ok()?;
        let run = shared
            .review_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run_id)
            .cloned()?;
        let rules = match shared.project_of(run_id) {
            Some(project) => project.rules.clone(),
            None => shared.rules(),
        };
        Some(review_context(run, rules.as_deref(), cwd, command))
    }

    /// Applies `sandbox.escalation_approval` identically for every escalation
    /// kind: off denies, user asks, auto reviews and may fall back to the user.
    /// Approval is void if the settings change while the decision is pending.
    async fn arbitrate<R, RF, H, HF>(
        &self,
        ctx: &ToolExecutionContext,
        label: &str,
        review: R,
        human: H,
    ) -> EscalationDecision
    where
        R: FnOnce(Arc<QuickModelReviewer>) -> RF,
        RF: Future<Output = Result<ReviewVerdict, ReviewError>>,
        H: FnOnce() -> HF,
        HF: Future<Output = EscalationDecision>,
    {
        let settings = self.settings.lock().ok().map(|settings| *settings);
        let decision = match settings {
            None => EscalationDecision::Deny {
                reason: format!("{label} settings unavailable"),
            },
            Some((EscalationApproval::Off, _)) => EscalationDecision::Deny {
                reason: format!("{label} is disabled"),
            },
            Some((EscalationApproval::User, _)) => human().await,
            Some((EscalationApproval::Auto, fallback)) => {
                let verdict = match &self.reviewer {
                    Some(reviewer) => review(Arc::clone(reviewer)).await,
                    None => Err(ReviewError::Model),
                };
                let reason = match verdict {
                    Ok(ReviewVerdict::Approve) => None,
                    Ok(ReviewVerdict::Deny { reason }) => Some(reason),
                    Err(error) => Some(error.to_string()),
                };
                match reason {
                    None => EscalationDecision::Approve,
                    Some(reason) if fallback => {
                        self.diagnose(
                            ctx,
                            DiagnosticSeverity::Info,
                            &format!(
                                "automatic review declined; requesting user approval: {reason}"
                            ),
                        );
                        human().await
                    }
                    Some(reason) => EscalationDecision::Deny { reason },
                }
            }
        };
        let decision = match decision {
            EscalationDecision::Approve => {
                let current = self.settings.lock().ok().map(|settings| *settings);
                if current.is_some() && current == settings {
                    EscalationDecision::Approve
                } else {
                    EscalationDecision::Deny {
                        reason: format!("{label} settings changed during review"),
                    }
                }
            }
            EscalationDecision::Deny { reason } => EscalationDecision::Deny { reason },
        };
        match &decision {
            EscalationDecision::Approve => self.diagnose(ctx, DiagnosticSeverity::Info, "approved"),
            EscalationDecision::Deny { reason } => self.diagnose(
                ctx,
                DiagnosticSeverity::Warning,
                &format!("denied: {reason}"),
            ),
        }
        decision
    }
}

#[async_trait::async_trait]
impl ShellEscalationGate for SandboxEscalationGate {
    async fn decide(
        &self,
        ctx: &ToolExecutionContext,
        command: &str,
        justification: &str,
    ) -> EscalationDecision {
        self.decide_scoped_with_cwd(ctx, command, justification, None, ShellAccess::Host)
            .await
    }

    async fn decide_with_cwd(
        &self,
        ctx: &ToolExecutionContext,
        command: &str,
        justification: &str,
        cwd: Option<&Path>,
    ) -> EscalationDecision {
        self.decide_scoped_with_cwd(ctx, command, justification, cwd, ShellAccess::Host)
            .await
    }

    async fn decide_scoped_with_cwd(
        &self,
        ctx: &ToolExecutionContext,
        command: &str,
        justification: &str,
        cwd: Option<&Path>,
        access: ShellAccess,
    ) -> EscalationDecision {
        self.arbitrate(
            ctx,
            "shell escalation",
            |reviewer| async move {
                let context = self.review_context_for(ctx, cwd, command);
                reviewer
                    .review_scoped_with_context(
                        &ctx.run_id,
                        command,
                        justification,
                        context,
                        access,
                    )
                    .await
            },
            || self.human(ctx, command, justification, access),
        )
        .await
    }

    async fn decide_file_access(
        &self,
        ctx: &ToolExecutionContext,
        request: &FileAccessRequest,
    ) -> EscalationDecision {
        self.arbitrate(
            ctx,
            "file access escalation",
            |reviewer| async move {
                let context = self.review_context_for(ctx, None, "");
                reviewer
                    .review_file_access(&ctx.run_id, request, context)
                    .await
            },
            || self.human_file_access(ctx, request),
        )
        .await
    }
}
