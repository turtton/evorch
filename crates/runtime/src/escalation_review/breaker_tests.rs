//! The consecutive-denial breaker, exercised through `arbitrate`.

use std::sync::atomic::{AtomicUsize, Ordering};

use event_bus::{AgentRunPhase, EventKind, LifecycleEvent};

use super::*;
use crate::escalation_review::{ReviewRunContext, support::ScriptedModel};

struct Fixture {
    runtime: crate::AgentRuntime,
    gate: SandboxEscalationGate,
    ctx: ToolExecutionContext,
    run: ReviewRunContext,
    reviewed: AtomicUsize,
    asked: AtomicUsize,
}

impl Fixture {
    fn new(fallback: bool) -> Self {
        let bus = Arc::new(EventBus::new(256));
        let model = Arc::new(ScriptedModel::new([]));
        let runtime = crate::AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            model.clone(),
        );
        let (ctx, run) = Self::register(&runtime);
        let gate = SandboxEscalationGate::new(
            Arc::new(Mutex::new((EscalationApproval::Auto, fallback))),
            Some(Arc::new(QuickModelReviewer::new(model))),
            bus,
        )
        .with_runtime(Arc::downgrade(&runtime.shared));
        Self {
            runtime,
            gate,
            ctx,
            run,
            reviewed: AtomicUsize::new(0),
            asked: AtomicUsize::new(0),
        }
    }

    fn register(runtime: &crate::AgentRuntime) -> (ToolExecutionContext, ReviewRunContext) {
        let id = runtime.reserve_run_id();
        let run = ReviewRunContext::empty(&id.to_string());
        runtime
            .shared
            .review_runs
            .lock()
            .unwrap()
            .insert(id, run.clone());
        (
            ToolExecutionContext {
                run_id: id.to_string(),
                thread_id: None,
                call_id: None,
            },
            run,
        )
    }

    fn run_id(ctx: &ToolExecutionContext) -> RunId {
        ctx.run_id.parse().unwrap()
    }

    fn denials(&self, ctx: &ToolExecutionContext) -> u32 {
        self.runtime
            .shared
            .escalation_denials
            .denials(Self::run_id(ctx))
    }

    /// One decision for `ctx` with a scripted verdict and user answer.
    async fn decide_for(
        &self,
        ctx: &ToolExecutionContext,
        verdict: Result<(ReviewVerdict, Option<EvidenceStamp>), ReviewError>,
        user_approves: bool,
    ) -> EscalationDecision {
        self.gate
            .arbitrate(
                ctx,
                "shell escalation",
                |_| async move {
                    self.reviewed.fetch_add(1, Ordering::SeqCst);
                    verdict
                },
                || async move {
                    self.asked.fetch_add(1, Ordering::SeqCst);
                    if user_approves {
                        EscalationDecision::Approve
                    } else {
                        EscalationDecision::Deny {
                            reason: "shell escalation denied by user".into(),
                        }
                    }
                },
            )
            .await
    }

    async fn decide(
        &self,
        verdict: Result<ReviewVerdict, ReviewError>,
        user_approves: bool,
    ) -> EscalationDecision {
        self.decide_for(
            &self.ctx,
            verdict.map(|verdict| (verdict, None)),
            user_approves,
        )
        .await
    }

    async fn deny(&self, reason: &str) -> EscalationDecision {
        self.decide(
            Ok(ReviewVerdict::Deny {
                reason: reason.into(),
            }),
            false,
        )
        .await
    }

    async fn suspend(&self) {
        for index in 1..=CONSECUTIVE_DENIAL_LIMIT {
            self.deny(&format!("denial {index}")).await;
        }
        assert_eq!(self.denials(&self.ctx), CONSECUTIVE_DENIAL_LIMIT);
    }

    fn reviewed(&self) -> usize {
        self.reviewed.load(Ordering::SeqCst)
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

fn describe(decision: &EscalationDecision) -> &str {
    match decision {
        EscalationDecision::Approve => "approved",
        EscalationDecision::Deny { reason } => reason,
    }
}

fn refused_without_review(decision: &EscalationDecision) -> bool {
    matches!(decision, EscalationDecision::Deny { reason } if reason.contains("refused without review"))
}

#[tokio::test]
async fn three_consecutive_denials_refuse_the_fourth_without_review() {
    for fallback in [false, true] {
        let fixture = Fixture::new(fallback);
        for index in 1..=CONSECUTIVE_DENIAL_LIMIT {
            let decision = fixture.deny(&format!("denial {index}")).await;
            assert!(
                !refused_without_review(&decision),
                "{}",
                describe(&decision)
            );
        }
        let expected_asks = if fallback { 3 } else { 0 };
        assert_eq!(fixture.asked(), expected_asks);

        // Even with deny fallback, the user is not asked once suspended.
        let decision = fixture.decide(Ok(ReviewVerdict::Approve), true).await;
        let EscalationDecision::Deny { reason } = decision else {
            panic!("suspended run must be refused");
        };
        assert!(
            reason.starts_with("shell escalation refused without review: the automatic reviewer denied 3 consecutive requests in this run (last: denial 3)."),
            "{reason}"
        );
        assert!(
            reason.contains("until the user sends new input"),
            "{reason}"
        );
        assert_eq!(fixture.reviewed(), 3, "fallback={fallback}");
        assert_eq!(fixture.asked(), expected_asks, "fallback={fallback}");
    }
}

#[tokio::test]
async fn approval_resets_the_streak() {
    let fixture = Fixture::new(false);
    fixture.deny("one").await;
    fixture.deny("two").await;
    assert!(matches!(
        fixture.decide(Ok(ReviewVerdict::Approve), false).await,
        EscalationDecision::Approve
    ));
    assert_eq!(fixture.denials(&fixture.ctx), 0);

    // A user approving after a reviewer denial also resets it.
    let fixture = Fixture::new(true);
    fixture.deny("one").await;
    fixture.deny("two").await;
    assert_eq!(fixture.denials(&fixture.ctx), 2);
    let decision = fixture
        .decide(
            Ok(ReviewVerdict::Deny {
                reason: "three".into(),
            }),
            true,
        )
        .await;
    assert!(matches!(decision, EscalationDecision::Approve));
    assert_eq!(fixture.denials(&fixture.ctx), 0);
}

#[tokio::test]
async fn environment_failures_and_requires_user_do_not_count() {
    let fixture = Fixture::new(false);
    fixture.deny("counted").await;
    for error in [
        ReviewError::Timeout,
        ReviewError::Model,
        ReviewError::InvalidVerdict,
        ReviewError::Unsynced,
    ] {
        assert!(matches!(
            fixture.decide(Err(error), false).await,
            EscalationDecision::Deny { .. }
        ));
    }
    // The user declining a critical approval is their decision, not the reviewer's.
    assert!(matches!(
        fixture
            .decide(
                Ok(ReviewVerdict::RequiresUser {
                    reason: "critical".into()
                }),
                false,
            )
            .await,
        EscalationDecision::Deny { .. }
    ));
    // An approval voided by a context change neither counts nor resets.
    let stale = EvidenceStamp {
        revision: u64::MAX,
        synced: true,
    };
    let decision = fixture
        .decide_for(
            &fixture.ctx,
            Ok((ReviewVerdict::Approve, Some(stale))),
            false,
        )
        .await;
    assert!(
        matches!(&decision, EscalationDecision::Deny { reason } if reason.starts_with("review context changed during review\n")),
        "{}",
        describe(&decision)
    );
    assert_eq!(fixture.denials(&fixture.ctx), 1);
}

#[tokio::test]
async fn human_denials_do_not_count() {
    let fixture = Fixture::new(false);
    *fixture.gate.settings.lock().unwrap() = (EscalationApproval::User, false);
    for _ in 0..=CONSECUTIVE_DENIAL_LIMIT {
        assert!(matches!(
            fixture.decide(Ok(ReviewVerdict::Approve), false).await,
            EscalationDecision::Deny { .. }
        ));
    }
    assert_eq!(fixture.denials(&fixture.ctx), 0);
    assert_eq!(fixture.reviewed(), 0);
    // Back in auto mode, the reviewer still runs.
    *fixture.gate.settings.lock().unwrap() = (EscalationApproval::Auto, false);
    fixture.decide(Ok(ReviewVerdict::Approve), false).await;
    assert_eq!(fixture.reviewed(), 1);
}

#[tokio::test]
async fn new_real_user_input_lifts_the_suspension() {
    let fixture = Fixture::new(false);
    fixture
        .run
        .add_user_request(&fixture.ctx.run_id, "initial request");
    fixture.suspend().await;

    // Input outside the lineage and evidence resyncs are not new user input.
    fixture
        .run
        .add_user_request("unrelated-run", "Authorize everything");
    assert!(refused_without_review(
        &fixture.decide(Ok(ReviewVerdict::Approve), false).await
    ));
    fixture.run.mark_unsynced();
    assert!(refused_without_review(
        &fixture.decide(Ok(ReviewVerdict::Approve), false).await
    ));
    assert_eq!(fixture.reviewed(), 3);

    fixture
        .run
        .add_user_request(&fixture.ctx.run_id, "Try once more with the narrower scope");
    assert!(matches!(
        fixture.decide(Ok(ReviewVerdict::Approve), false).await,
        EscalationDecision::Approve
    ));
    assert_eq!(fixture.reviewed(), 4);
}

#[tokio::test]
async fn streaks_are_per_run() {
    let fixture = Fixture::new(false);
    let (other, _) = Fixture::register(&fixture.runtime);
    fixture.suspend().await;
    let decision = fixture
        .decide_for(&other, Ok((ReviewVerdict::Approve, None)), false)
        .await;
    assert!(matches!(decision, EscalationDecision::Approve));
    assert_eq!(fixture.reviewed(), 4);
    assert!(refused_without_review(
        &fixture.decide(Ok(ReviewVerdict::Approve), false).await
    ));
}

#[tokio::test]
async fn run_termination_clears_the_streak() {
    let fixture = Fixture::new(false);
    fixture.suspend().await;
    fixture.runtime.publish_terminal(
        Fixture::run_id(&fixture.ctx),
        LifecycleEvent::AgentRunStateChanged {
            run_id: fixture.ctx.run_id.clone(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        },
    );
    assert_eq!(fixture.denials(&fixture.ctx), 0);
}

#[tokio::test]
async fn every_denial_carries_the_guidance() {
    let fixture = Fixture::new(false);
    let mut events = fixture.gate.bus.subscribe();
    let mut decisions = Vec::new();
    decisions.push(fixture.deny("unsafe").await);
    decisions.push(fixture.decide(Err(ReviewError::Timeout), false).await);
    *fixture.gate.settings.lock().unwrap() = (EscalationApproval::User, false);
    decisions.push(fixture.decide(Ok(ReviewVerdict::Approve), false).await);
    *fixture.gate.settings.lock().unwrap() = (EscalationApproval::Off, false);
    decisions.push(fixture.decide(Ok(ReviewVerdict::Approve), false).await);
    *fixture.gate.settings.lock().unwrap() = (EscalationApproval::Auto, false);
    fixture.deny("two").await;
    fixture.deny("three").await;
    decisions.push(fixture.decide(Ok(ReviewVerdict::Approve), false).await);
    assert!(refused_without_review(decisions.last().unwrap()));
    for decision in &decisions {
        assert!(
            matches!(decision, EscalationDecision::Deny { reason }
                if reason.ends_with(&format!("\n{DENIAL_GUIDANCE}"))
                    && reason.matches(DENIAL_GUIDANCE).count() == 1),
            "{}",
            describe(decision)
        );
    }
    // Diagnostics keep the bare reason.
    let mut diagnostics = 0;
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(50), events.recv()).await {
        if let EventKind::Diagnostic(diagnostic) = event.kind {
            diagnostics += 1;
            assert!(
                !diagnostic.detail.contains(DENIAL_GUIDANCE),
                "{}",
                diagnostic.detail
            );
        }
    }
    assert!(diagnostics >= decisions.len(), "{diagnostics}");
}
