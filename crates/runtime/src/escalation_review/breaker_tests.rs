//! The consecutive-denial breaker, exercised through `arbitrate`.

use std::sync::atomic::{AtomicUsize, Ordering};

use event_bus::{AgentRunPhase, EventKind, LifecycleEvent};

use super::*;
use crate::escalation_review::{DENIAL_GUIDANCE, ReviewRunContext, support::ScriptedModel};

#[path = "denial_result_tests.rs"]
mod denial_result_tests;
#[path = "environment_tests.rs"]
mod environment_tests;
#[path = "handoff_tests.rs"]
mod handoff_tests;

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
        EscalationDecision::Deny { reason } | EscalationDecision::Suspended { reason } => reason,
    }
}

fn refused_without_review(decision: &EscalationDecision) -> bool {
    matches!(decision, EscalationDecision::Suspended { reason } if reason.contains("refused without review"))
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
        let EscalationDecision::Suspended { reason } = decision else {
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
async fn user_mode_approval_resets_auto_streak() {
    let fixture = Fixture::new(false);
    fixture.deny("one").await;
    fixture.deny("two").await;
    *fixture.gate.settings.lock().unwrap() = (EscalationApproval::User, false);
    let mut events = fixture.gate.bus.subscribe();
    let approval = fixture.gate.decide(&fixture.ctx, "inspection", "inspect");
    let answer = async {
        loop {
            if let EventKind::Tool(event_bus::ToolEvent::ApprovalRequested { call_id, .. }) =
                events.recv().await.unwrap().kind
            {
                fixture.gate.bus.emit(event_bus::Event::new(
                    event_bus::ToolEvent::ApprovalResolved {
                        call_id,
                        approved: true,
                    },
                ));
                break;
            }
        }
    };
    let (decision, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(approval, answer)
    })
    .await
    .expect("user-mode approval deadline");
    assert!(matches!(decision, EscalationDecision::Approve));
    assert_eq!(fixture.denials(&fixture.ctx), 0);
    *fixture.gate.settings.lock().unwrap() = (EscalationApproval::Auto, false);
    assert!(!refused_without_review(&fixture.deny("new one").await));
    assert_eq!(fixture.denials(&fixture.ctx), 1);
    assert!(!refused_without_review(&fixture.deny("new two").await));
    assert_eq!(fixture.reviewed(), 4);
}

#[tokio::test]
async fn settings_change_during_fallback_approval_neither_counts_nor_resets() {
    let mut fixture = Fixture::new(true);
    fixture
        .run
        .add_user_request(&fixture.ctx.run_id, "inspect the project");
    fixture.deny("existing").await;
    let model = Arc::new(ScriptedModel::new([Ok(
        super::super::support::text_response(
            r#"{"approve":false,"reason":"pending","risk_level":"low","authorization_level":"high"}"#,
            providers::FinishReason::Stop,
        ),
    )]));
    fixture.gate.reviewer = Some(Arc::new(QuickModelReviewer::new(model)));
    let mut events = fixture.gate.bus.subscribe();
    let decision = fixture.gate.decide(&fixture.ctx, "inspection", "inspect");
    let change = async {
        loop {
            if let EventKind::Tool(event_bus::ToolEvent::ApprovalRequested { call_id, .. }) =
                events.recv().await.unwrap().kind
            {
                *fixture.gate.settings.lock().unwrap() = (EscalationApproval::Auto, false);
                fixture.gate.bus.emit(event_bus::Event::new(
                    event_bus::ToolEvent::ApprovalResolved {
                        call_id,
                        approved: true,
                    },
                ));
                break;
            }
        }
    };
    let (decision, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(decision, change)
    })
    .await
    .expect("ordered settings change");
    assert!(matches!(decision, EscalationDecision::Deny { reason }
        if reason == "shell escalation settings changed during review"));
    assert_eq!(fixture.denials(&fixture.ctx), 1);
}

#[tokio::test]
async fn parallel_old_generation_completions_cannot_change_current_streak() {
    // Cover a reset watermark, a live streak, and an already suspended generation.
    for current_denials in [0, 1, CONSECUTIVE_DENIAL_LIMIT] {
        for approve in [false, true] {
            let mut fixture = Fixture::new(false);
            fixture
                .run
                .add_user_request(&fixture.ctx.run_id, "old request");
            let release = Arc::new(tokio::sync::Notify::new());
            let response = if approve {
                r#"{"approve":true,"reason":"old critical","risk_level":"critical","authorization_level":"high"}"#
            } else {
                r#"{"approve":false,"reason":"old denial","risk_level":"low","authorization_level":"high"}"#
            };
            let model = Arc::new(ScriptedModel::gated(
                [Ok(super::super::support::text_response(
                    response,
                    providers::FinishReason::Stop,
                ))],
                release.clone(),
            ));
            fixture.gate.reviewer = Some(Arc::new(QuickModelReviewer::new(model.clone())));
            let request = FileAccessRequest {
                tool: "read".into(),
                paths: vec!["/outside/documentation".into()],
                write: false,
                justification: "requested inspection".into(),
                change: None,
            };
            let mut events = fixture.gate.bus.subscribe();
            let old = fixture.gate.decide_file_access(&fixture.ctx, &request);
            let newer = async {
                model.wait_for_request(0).await;
                fixture
                    .run
                    .add_user_request(&fixture.ctx.run_id, "new request");
                if current_denials == 0 {
                    fixture.decide(Ok(ReviewVerdict::Approve), false).await;
                } else {
                    for _ in 0..current_denials {
                        fixture.deny("current denial").await;
                    }
                }
                release.notify_one();
                if approve && current_denials != CONSECUTIVE_DENIAL_LIMIT {
                    loop {
                        if let EventKind::Tool(event_bus::ToolEvent::ApprovalRequested {
                            call_id,
                            ..
                        }) = events.recv().await.unwrap().kind
                        {
                            fixture.gate.bus.emit(event_bus::Event::new(
                                event_bus::ToolEvent::ApprovalResolved {
                                    call_id,
                                    approved: true,
                                },
                            ));
                            break;
                        }
                    }
                }
            };
            let (decision, ()) =
                tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(old, newer) })
                    .await
                    .expect("ordered old-generation completion");
            // New input invalidates the old evidence, even when its human
            // handoff is allowed because the newer generation is not suspended.
            assert!(!matches!(decision, EscalationDecision::Approve));
            if approve && current_denials == CONSECUTIVE_DENIAL_LIMIT {
                assert!(refused_without_review(&decision));
                assert!(
                    !events.drain_pending_snapshot().iter().any(|event| matches!(
                        event.kind,
                        EventKind::Tool(event_bus::ToolEvent::ApprovalRequested { .. })
                    ))
                );
            }
            assert_eq!(
                fixture.denials(&fixture.ctx),
                current_denials,
                "approve={approve}"
            );
            assert_eq!(
                fixture
                    .runtime
                    .shared
                    .escalation_denials
                    .suspension(Fixture::run_id(&fixture.ctx), 2),
                (current_denials == CONSECUTIVE_DENIAL_LIMIT).then(|| "current denial".into())
            );
        }
    }
}

#[tokio::test]
async fn input_arriving_during_denial_discards_old_result_before_next_call() {
    let fixture = Fixture::new(false);
    let entered = tokio::sync::Notify::new();
    let release = tokio::sync::Notify::new();
    let pending = fixture.gate.arbitrate(
        &fixture.ctx,
        "file access escalation",
        |_| async {
            entered.notify_one();
            release.notified().await;
            Ok((
                ReviewVerdict::Deny {
                    reason: "old".into(),
                },
                None,
            ))
        },
        || async { panic!("no fallback") },
    );
    let input = async {
        entered.notified().await;
        fixture
            .run
            .add_user_request(&fixture.ctx.run_id, "new request");
        release.notify_one();
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(pending, input)
    })
    .await
    .expect("ordered new input");
    assert_eq!(fixture.denials(&fixture.ctx), 0);
}

#[tokio::test]
async fn authorization_and_missing_user_request_policy_denials_count() {
    use crate::escalation_review::support::text_response;
    use providers::FinishReason;
    for missing_request in [false, true] {
        let mut fixture = Fixture::new(false);
        let model = Arc::new(ScriptedModel::new((0..3).map(|_| Ok(text_response(
            r#"{"approve":true,"reason":"insufficient authority","risk_level":"high","authorization_level":"low"}"#,
            FinishReason::Stop,
        )))));
        fixture.gate.reviewer = Some(Arc::new(QuickModelReviewer::new(model.clone())));
        if !missing_request {
            fixture
                .run
                .add_user_request(&fixture.ctx.run_id, "inspect the project");
        }
        for count in 1..=CONSECUTIVE_DENIAL_LIMIT {
            assert!(matches!(
                fixture
                    .gate
                    .decide(&fixture.ctx, "inspection", "inspect")
                    .await,
                EscalationDecision::Deny { .. }
            ));
            assert_eq!(fixture.denials(&fixture.ctx), count);
        }
        assert!(refused_without_review(
            &fixture
                .gate
                .decide(&fixture.ctx, "inspection", "inspect")
                .await
        ));
        assert_eq!(
            model.observed().await.len(),
            if missing_request { 0 } else { 3 }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn critical_human_timeout_or_cancel_does_not_change_streak() {
    for cancel in [false, true] {
        let fixture = Fixture::new(false);
        fixture.deny("existing").await;
        let mut events = fixture.gate.bus.subscribe();
        let mut decision = Box::pin(fixture.gate.arbitrate(
            &fixture.ctx,
            "shell escalation",
            |_| async {
                Ok((
                    ReviewVerdict::RequiresUser {
                        reason: "critical".into(),
                    },
                    None,
                ))
            },
            || {
                fixture.gate.human(
                    &fixture.ctx,
                    "critical operation",
                    "requested",
                    ShellAccess::Host,
                )
            },
        ));
        loop {
            tokio::select! {
                result = &mut decision => panic!("approval must be pending: {}", describe(&result)),
                event = events.recv() => if matches!(event.unwrap().kind,
                    EventKind::Tool(event_bus::ToolEvent::ApprovalRequested { .. })) { break; }
            }
        }
        if cancel {
            // The caller cancels by dropping the in-flight gate future.
            drop(decision);
        } else {
            tokio::time::advance(HUMAN_APPROVAL_TIMEOUT).await;
            assert!(matches!(decision.await, EscalationDecision::Deny { reason }
                if reason.contains("approval timed out") && !reason.contains(DENIAL_GUIDANCE)));
        }
        assert_eq!(fixture.denials(&fixture.ctx), 1);
    }
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
        matches!(&decision, EscalationDecision::Deny { reason } if reason == "review context changed during review"),
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

    // A fresh input lifts suspension, but review evidence must also be synced.
    fixture.run.user_requests.lock().unwrap().synced = true;
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
async fn gate_denial_reasons_and_diagnostics_exclude_agent_guidance() {
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
            matches!(decision, EscalationDecision::Deny { reason } | EscalationDecision::Suspended { reason }
                if !reason.contains(DENIAL_GUIDANCE)),
            "{}",
            describe(decision)
        );
    }
    // Diagnostics keep the bare reason.
    let mut diagnostics = 0;
    for event in events.drain_pending_snapshot() {
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
