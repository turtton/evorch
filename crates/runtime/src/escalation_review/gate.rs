use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use config::EscalationApproval;
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventBus};
use sandbox::approval::{ApprovalGate, ApprovalOutcome};
use tools::{
    ToolExecutionContext,
    tools::shell_escalation::{EscalationDecision, ShellEscalationGate},
};

use super::{QuickModelReviewer, ReviewVerdict};

const HUMAN_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

pub struct SandboxEscalationGate {
    settings: Arc<Mutex<(EscalationApproval, bool)>>,
    reviewer: Option<Arc<QuickModelReviewer>>,
    pub(super) bus: Arc<EventBus>,
    pub(super) human_timeout: Duration,
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
        }
    }

    fn diagnose(&self, ctx: &ToolExecutionContext, severity: DiagnosticSeverity, verdict: &str) {
        self.bus.emit(Event::new(DiagnosticEvent {
            source: "sandbox".into(),
            severity,
            code: "escalation_review".into(),
            detail: format!(
                "run_id={} call_id={}: {verdict}",
                ctx.run_id,
                ctx.call_id.as_deref().unwrap_or_default()
            ),
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
    ) -> EscalationDecision {
        let call_id = format!(
            "{}:{}",
            ctx.run_id,
            ctx.call_id.as_deref().unwrap_or_default()
        );
        match ApprovalGate::new(self.bus.clone(), self.human_timeout).request_with_input(
            "shell", &call_id,
            Some(serde_json::json!({"command":command, "justification":justification, "kind":"shell_escalation"})),
        ).await {
            ApprovalOutcome::Approved => EscalationDecision::Approve,
            ApprovalOutcome::Denied => EscalationDecision::Deny { reason: "shell escalation denied by user".into() },
            ApprovalOutcome::TimedOut => EscalationDecision::Deny { reason: "shell escalation approval timed out".into() },
        }
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
        let settings = self.settings.lock().ok().map(|settings| *settings);
        let decision = match settings {
            None => EscalationDecision::Deny {
                reason: "shell escalation settings unavailable".into(),
            },
            Some((EscalationApproval::Off, _)) => EscalationDecision::Deny {
                reason: "shell escalation is disabled".into(),
            },
            Some((EscalationApproval::User, _)) => self.human(ctx, command, justification).await,
            Some((EscalationApproval::Auto, fallback)) => {
                let verdict = match &self.reviewer {
                    Some(reviewer) => reviewer.review(&ctx.run_id, command, justification).await,
                    None => Err(super::ReviewError::Model),
                };
                let reason = match verdict {
                    Ok(ReviewVerdict::Approve) => None,
                    Ok(ReviewVerdict::Deny { reason }) => Some(reason),
                    Err(error) => Some(error.to_string()),
                };
                match reason {
                    None => EscalationDecision::Approve,
                    Some(reason) if fallback => {
                        self.diagnose(ctx, DiagnosticSeverity::Warning, &reason);
                        self.human(ctx, command, justification).await
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
                        reason: "shell escalation settings changed during review".into(),
                    }
                }
            }
            EscalationDecision::Deny { reason } => EscalationDecision::Deny { reason },
        };
        match &decision {
            EscalationDecision::Approve => self.diagnose(ctx, DiagnosticSeverity::Info, "approved"),
            EscalationDecision::Deny { reason } => {
                self.diagnose(ctx, DiagnosticSeverity::Warning, reason)
            }
        }
        decision
    }
}
