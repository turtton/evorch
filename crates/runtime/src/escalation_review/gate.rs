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

use super::{
    CONSECUTIVE_DENIAL_LIMIT, DENIAL_GUIDANCE, QuickModelReviewer, ReviewContext, ReviewError,
    ReviewVerdict, review_context,
};
use crate::{RunId, runtime::Shared};

const HUMAN_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Identity of the in-memory evidence snapshot used for an automatic review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceStamp {
    pub revision: u64,
    pub synced: bool,
}

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
    ) -> Option<(ReviewContext, EvidenceStamp)> {
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
        review_context(run, rules.as_deref()).ok()
    }

    fn current_stamp(&self, ctx: &ToolExecutionContext) -> Option<EvidenceStamp> {
        let shared = self.runtime.as_ref()?.upgrade()?;
        let run_id = ctx.run_id.parse::<crate::RunId>().ok()?;
        shared
            .review_runs
            .lock()
            .ok()?
            .get(&run_id)
            .map(|run| run.stamp())
    }

    /// The run's breaker state: the shared streaks, the run, and how many real
    /// user inputs its lineage sees now. None outside a runtime.
    fn denial_streak(&self, ctx: &ToolExecutionContext) -> Option<(Arc<Shared>, RunId, usize)> {
        let shared = self.runtime.as_ref()?.upgrade()?;
        let run_id = ctx.run_id.parse::<RunId>().ok()?;
        let user_inputs = shared
            .review_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run_id)?
            .visible_request_count();
        Some((shared, run_id, user_inputs))
    }

    /// Applies `sandbox.escalation_approval` identically for every escalation
    /// kind: off denies, user asks, auto reviews and may fall back to the user.
    /// Approval is void if the settings change while the decision is pending.
    /// After [`CONSECUTIVE_DENIAL_LIMIT`] consecutive reviewer denials, auto
    /// refuses without review until the user sends new input.
    async fn arbitrate<R, RF, H, HF>(
        &self,
        ctx: &ToolExecutionContext,
        label: &str,
        review: R,
        human: H,
    ) -> EscalationDecision
    where
        R: FnOnce(Arc<QuickModelReviewer>) -> RF,
        RF: Future<Output = Result<(ReviewVerdict, Option<EvidenceStamp>), ReviewError>>,
        H: FnOnce() -> HF,
        HF: Future<Output = EscalationDecision>,
    {
        let settings = self.settings.lock().ok().map(|settings| *settings);
        let mut reviewed_stamp = None;
        let mut streak = None;
        // The reviewer's own denial reason, when the outcome hinges on it.
        let mut counted = None;
        let decision = match settings {
            None => EscalationDecision::Deny {
                reason: format!("{label} settings unavailable"),
            },
            Some((EscalationApproval::Off, _)) => EscalationDecision::Deny {
                reason: format!("{label} is disabled"),
            },
            Some((EscalationApproval::User, _)) => human().await,
            Some((EscalationApproval::Auto, fallback)) => {
                streak = self.denial_streak(ctx);
                let suspended = streak.as_ref().and_then(|(shared, run_id, user_inputs)| {
                    shared.escalation_denials.suspension(*run_id, *user_inputs)
                });
                if let Some(last) = suspended {
                    // Neither the reviewer nor the user is asked, even with fallback.
                    EscalationDecision::Deny {
                        reason: format!(
                            "{label} refused without review: the automatic reviewer denied {CONSECUTIVE_DENIAL_LIMIT} consecutive requests in this run (last: {last}). Further requests are refused until the user sends new input to this conversation; do not reword and resubmit. Work inside the sandbox can continue."
                        ),
                    }
                } else {
                    self.review_automatically(
                        ctx,
                        review,
                        human,
                        fallback,
                        &mut reviewed_stamp,
                        &mut counted,
                    )
                    .await
                }
            }
        };
        let decision = match decision {
            EscalationDecision::Approve => {
                let current = self.settings.lock().ok().map(|settings| *settings);
                if reviewed_stamp
                    .is_some_and(|stamp| !stamp.synced || self.current_stamp(ctx) != Some(stamp))
                {
                    EscalationDecision::Deny {
                        reason: "review context changed during review".into(),
                    }
                } else if current.is_some() && current == settings {
                    EscalationDecision::Approve
                } else {
                    EscalationDecision::Deny {
                        reason: format!("{label} settings changed during review"),
                    }
                }
            }
            EscalationDecision::Deny { reason } => EscalationDecision::Deny { reason },
        };
        if let Some((shared, run_id, user_inputs)) = &streak {
            match (&decision, &counted) {
                // Any approval, automatic or by the user, ends the streak.
                (EscalationDecision::Approve, _) => shared.escalation_denials.clear(*run_id),
                (EscalationDecision::Deny { .. }, Some(reason)) => {
                    shared
                        .escalation_denials
                        .record_denial(*run_id, *user_inputs, reason);
                }
                // Environment failures, critical handoffs, and approvals voided
                // by a context change neither count nor reset.
                (EscalationDecision::Deny { .. }, None) => {}
            }
        }
        match &decision {
            EscalationDecision::Approve => self.diagnose(ctx, DiagnosticSeverity::Info, "approved"),
            EscalationDecision::Deny { reason } => self.diagnose(
                ctx,
                DiagnosticSeverity::Warning,
                &format!("denied: {reason}"),
            ),
        }
        match decision {
            EscalationDecision::Approve => EscalationDecision::Approve,
            EscalationDecision::Deny { reason } => EscalationDecision::Deny {
                reason: format!("{reason}\n{DENIAL_GUIDANCE}"),
            },
        }
    }

    /// One automatic review: the reviewer's verdict, then the user when the
    /// model approved a critical action or declined with fallback enabled.
    /// `counted` receives the reviewer's denial reason.
    async fn review_automatically<R, RF, H, HF>(
        &self,
        ctx: &ToolExecutionContext,
        review: R,
        human: H,
        fallback: bool,
        reviewed_stamp: &mut Option<EvidenceStamp>,
        counted: &mut Option<String>,
    ) -> EscalationDecision
    where
        R: FnOnce(Arc<QuickModelReviewer>) -> RF,
        RF: Future<Output = Result<(ReviewVerdict, Option<EvidenceStamp>), ReviewError>>,
        H: FnOnce() -> HF,
        HF: Future<Output = EscalationDecision>,
    {
        let verdict = match &self.reviewer {
            Some(reviewer) => review(Arc::clone(reviewer)).await,
            None => Err(ReviewError::Model),
        };
        let (reason, requires_user) = match verdict {
            Ok((ReviewVerdict::Approve, stamp)) => {
                *reviewed_stamp = stamp;
                (None, false)
            }
            Ok((ReviewVerdict::RequiresUser { reason }, _)) => (Some(reason), true),
            Ok((ReviewVerdict::Deny { reason }, _)) => {
                *counted = Some(reason.clone());
                (Some(reason), false)
            }
            Err(error) => (Some(error.to_string()), false),
        };
        match reason {
            None => EscalationDecision::Approve,
            Some(reason) if requires_user => {
                self.diagnose(
                    ctx,
                    DiagnosticSeverity::Info,
                    &format!("automatic review requires user approval: {reason}"),
                );
                human().await
            }
            Some(reason) if fallback => {
                self.diagnose(
                    ctx,
                    DiagnosticSeverity::Info,
                    &format!("automatic review declined; requesting user approval: {reason}"),
                );
                human().await
            }
            Some(reason) => EscalationDecision::Deny { reason },
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
        _cwd: Option<&Path>,
        access: ShellAccess,
    ) -> EscalationDecision {
        self.arbitrate(
            ctx,
            "shell escalation",
            |reviewer| async move {
                let snapshot = self.review_context_for(ctx);
                if self.runtime.is_some() && snapshot.is_none() {
                    return Err(ReviewError::Unsynced);
                }
                let (context, stamp) = snapshot.map_or((None, None), |(context, stamp)| {
                    (Some(context), Some(stamp))
                });
                reviewer
                    .review_scoped_with_context(
                        &ctx.run_id,
                        command,
                        justification,
                        context,
                        access,
                    )
                    .await
                    .map(|verdict| (verdict, stamp))
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
                let snapshot = self.review_context_for(ctx);
                if self.runtime.is_some() && snapshot.is_none() {
                    return Err(ReviewError::Unsynced);
                }
                let (context, stamp) = snapshot.map_or((None, None), |(context, stamp)| {
                    (Some(context), Some(stamp))
                });
                reviewer
                    .review_file_access(&ctx.run_id, request, context)
                    .await
                    .map(|verdict| (verdict, stamp))
            },
            || self.human_file_access(ctx, request),
        )
        .await
    }
}

#[cfg(test)]
#[path = "breaker_tests.rs"]
mod breaker_tests;

#[cfg(test)]
mod evidence_tests {
    use super::*;
    use crate::escalation_review::{ReviewRunContext, support::ScriptedModel};

    fn fixture() -> (
        crate::AgentRuntime,
        SandboxEscalationGate,
        ToolExecutionContext,
        ReviewRunContext,
    ) {
        let bus = Arc::new(EventBus::new(32));
        let model = Arc::new(ScriptedModel::new([]));
        let runtime = crate::AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            model.clone(),
        );
        let id = runtime.reserve_run_id();
        let run = ReviewRunContext::empty(&id.to_string());
        runtime
            .shared
            .review_runs
            .lock()
            .unwrap()
            .insert(id, run.clone());
        let gate = SandboxEscalationGate::new(
            Arc::new(Mutex::new((EscalationApproval::Auto, false))),
            Some(Arc::new(QuickModelReviewer::new(model))),
            bus,
        )
        .with_runtime(Arc::downgrade(&runtime.shared));
        (
            runtime,
            gate,
            ToolExecutionContext {
                run_id: id.to_string(),
                thread_id: None,
                call_id: None,
            },
            run,
        )
    }

    #[tokio::test]
    async fn requires_user_verdict_asks_user_even_without_deny_fallback() {
        for approved in [false, true] {
            let (_runtime, gate, ctx, _run) = fixture();
            let asked = std::sync::atomic::AtomicBool::new(false);
            let decision = gate
                .arbitrate(
                    &ctx,
                    "test",
                    |_| async {
                        Ok((
                            ReviewVerdict::RequiresUser {
                                reason: "forced push".into(),
                            },
                            None,
                        ))
                    },
                    || async {
                        asked.store(true, std::sync::atomic::Ordering::SeqCst);
                        if approved {
                            EscalationDecision::Approve
                        } else {
                            EscalationDecision::Deny {
                                reason: "shell escalation denied by user".into(),
                            }
                        }
                    },
                )
                .await;
            assert!(asked.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(matches!(decision, EscalationDecision::Approve), approved);
        }
    }

    #[tokio::test]
    async fn approve_is_rejected_when_host_input_changes_review_snapshot() {
        let (_runtime, gate, ctx, run) = fixture();
        run.add_user_request(&ctx.run_id, "initial request");
        let entered = tokio::sync::Notify::new();
        let changed = tokio::sync::Notify::new();
        let review = gate.arbitrate(
            &ctx,
            "test",
            |_| async {
                let (_, stamp) = gate.review_context_for(&ctx).unwrap();
                entered.notify_one();
                changed.notified().await;
                Ok((ReviewVerdict::Approve, Some(stamp)))
            },
            || async { panic!("no fallback") },
        );
        let input = async {
            entered.notified().await;
            run.add_user_request(&ctx.run_id, "new constraints");
            changed.notify_one();
        };
        let (decision, ()) = tokio::join!(review, input);
        assert!(
            matches!(decision, EscalationDecision::Deny { reason } if reason.starts_with("review context changed during review\n"))
        );
    }

    #[tokio::test]
    async fn unsynced_evidence_denies_shell_file_and_web_with_or_without_requests() {
        for populated in [false, true] {
            let (_runtime, gate, ctx, run) = fixture();
            if populated {
                run.add_user_request(&ctx.run_id, "read docs");
            }
            run.mark_unsynced();
            assert!(matches!(
                gate.decide(&ctx, "pwd", "inspect").await,
                EscalationDecision::Deny { .. }
            ));
            let request = FileAccessRequest {
                tool: "read".into(),
                paths: vec!["/outside".into()],
                write: false,
                justification: "docs".into(),
                change: None,
            };
            assert!(matches!(
                gate.decide_file_access(&ctx, &request).await,
                EscalationDecision::Deny { .. }
            ));
            let snapshot = review_context(run, None);
            assert!(matches!(snapshot, Err(ReviewError::Unsynced)));
            let context = snapshot.ok().map(|(context, _)| context);
            let verdict = gate
                .reviewer
                .as_ref()
                .unwrap()
                .review_web_fetch(
                    &ctx.run_id,
                    &serde_json::json!({"url":"https://example.com"}),
                    context,
                )
                .await
                .unwrap();
            assert!(matches!(verdict, ReviewVerdict::Deny { .. }));
        }
    }
}
