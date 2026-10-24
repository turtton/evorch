//! Ordered handoffs from reviews that started before another review suspended the run.
use super::*;
use tokio::sync::Notify;

async fn pending_handoff(critical: bool, new_input: bool, suspend_again: bool) {
    let mut fixture = Fixture::new(!critical);
    fixture.gate.human_timeout = Duration::ZERO;
    for _ in 1..CONSECUTIVE_DENIAL_LIMIT {
        fixture.deny("earlier").await;
    }
    let entered = Notify::new();
    let release = Notify::new();
    let mut events = fixture.gate.bus.subscribe();
    let pending = fixture.gate.arbitrate(
        &fixture.ctx,
        "shell escalation",
        |_| async {
            fixture.reviewed.fetch_add(1, Ordering::SeqCst);
            entered.notify_one();
            release.notified().await;
            Ok((
                if critical {
                    ReviewVerdict::RequiresUser {
                        reason: "critical".into(),
                    }
                } else {
                    ReviewVerdict::Deny {
                        reason: "late policy denial".into(),
                    }
                },
                None,
            ))
        },
        || async {
            fixture.asked.fetch_add(1, Ordering::SeqCst);
            fixture
                .gate
                .human(&fixture.ctx, "inspection", "inspect", ShellAccess::Host)
                .await
        },
    );
    let reach_limit = async {
        entered.notified().await;
        fixture.deny("threshold denial").await;
        assert_eq!(fixture.denials(&fixture.ctx), CONSECUTIVE_DENIAL_LIMIT);
        if new_input {
            fixture
                .run
                .add_user_request(&fixture.ctx.run_id, "new user request");
            if suspend_again {
                fixture.suspend().await;
            }
        }
        release.notify_one();
    };
    let (decision, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(pending, reach_limit)
    })
    .await
    .expect("ordered pending handoff");
    if new_input && !suspend_again {
        assert!(matches!(decision, EscalationDecision::Deny { reason }
            if reason == "shell escalation approval timed out"));
        assert_eq!(
            fixture.denials(&fixture.ctx),
            0,
            "old verdict cannot revive the streak"
        );
    } else {
        assert!(refused_without_review(&decision), "{}", describe(&decision));
        assert!(describe(&decision).contains(if suspend_again {
            "last: denial 3"
        } else {
            "last: threshold denial"
        }));
        assert_eq!(
            fixture.denials(&fixture.ctx),
            CONSECUTIVE_DENIAL_LIMIT + u32::from(!critical && !new_input)
        );
        if !critical && !new_input {
            assert_eq!(
                fixture
                    .runtime
                    .shared
                    .escalation_denials
                    .suspension(Fixture::run_id(&fixture.ctx), 0)
                    .as_deref(),
                Some("late policy denial")
            );
        }
    }
    assert_eq!(
        fixture.reviewed(),
        4 + usize::from(suspend_again) * CONSECUTIVE_DENIAL_LIMIT as usize
    );
    assert_eq!(
        fixture.asked(),
        (if critical {
            0
        } else {
            3 + usize::from(suspend_again) * CONSECUTIVE_DENIAL_LIMIT as usize
        }) + usize::from(new_input && !suspend_again)
    );
    let dialogs = events
        .drain_pending_snapshot()
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                EventKind::Tool(event_bus::ToolEvent::ApprovalRequested { .. })
            )
        })
        .count();
    assert_eq!(
        dialogs,
        usize::from(new_input && !suspend_again),
        "no new dialog after suspension"
    );
}

async fn handoff_admitted_before_threshold(critical: bool) {
    let fixture = Fixture::new(!critical);
    for _ in 1..CONSECUTIVE_DENIAL_LIMIT {
        fixture.deny("earlier").await;
    }
    let admitted = Notify::new();
    let release = Notify::new();
    let mut events = fixture.gate.bus.subscribe();
    let pending = fixture.gate.arbitrate(
        &fixture.ctx,
        "shell escalation",
        |_| async {
            Ok((
                if critical {
                    ReviewVerdict::RequiresUser {
                        reason: "critical".into(),
                    }
                } else {
                    ReviewVerdict::Deny {
                        reason: "policy denial".into(),
                    }
                },
                None,
            ))
        },
        || async {
            // The gate's suspension check has admitted this handoff, but no
            // ApprovalRequested event has been emitted yet.
            admitted.notify_one();
            release.notified().await;
            fixture
                .gate
                .human(&fixture.ctx, "inspection", "inspect", ShellAccess::Host)
                .await
        },
    );
    let threshold_then_answer = async {
        admitted.notified().await;
        assert!(
            !events.drain_pending_snapshot().iter().any(|event| matches!(
                event.kind,
                EventKind::Tool(event_bus::ToolEvent::ApprovalRequested { .. })
            ))
        );
        fixture.deny("threshold denial").await;
        assert_eq!(fixture.denials(&fixture.ctx), CONSECUTIVE_DENIAL_LIMIT);
        assert!(
            fixture
                .runtime
                .shared
                .escalation_denials
                .suspension(Fixture::run_id(&fixture.ctx), 0)
                .is_some()
        );
        // Only after the third denial is recorded may the admitted handoff
        // publish its dialog. This ordering is intentionally allowed.
        release.notify_one();
        loop {
            if let EventKind::Tool(event_bus::ToolEvent::ApprovalRequested { call_id, .. }) =
                events.recv().await.unwrap().kind
            {
                assert_eq!(fixture.denials(&fixture.ctx), CONSECUTIVE_DENIAL_LIMIT);
                fixture
                    .gate
                    .bus
                    .emit(Event::new(event_bus::ToolEvent::ApprovalResolved {
                        call_id,
                        approved: true,
                    }));
                break;
            }
        }
    };
    let (decision, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(pending, threshold_then_answer)
    })
    .await
    .expect("handoff admission before threshold, dialog after threshold");
    assert!(matches!(decision, EscalationDecision::Approve));
    assert_eq!(
        fixture.denials(&fixture.ctx),
        0,
        "valid human approval resets"
    );
}

#[tokio::test]
async fn concurrent_critical_handoff_admitted_before_threshold_can_open_dialog_afterward() {
    handoff_admitted_before_threshold(true).await;
}

#[tokio::test]
async fn concurrent_fallback_handoff_admitted_before_threshold_can_open_dialog_afterward() {
    handoff_admitted_before_threshold(false).await;
}

#[tokio::test]
async fn concurrent_old_generation_fallback_cannot_bypass_new_generation_suspension() {
    pending_handoff(false, true, true).await;
}

#[tokio::test]
async fn concurrent_old_generation_critical_cannot_bypass_new_generation_suspension() {
    pending_handoff(true, true, true).await;
}

#[tokio::test]
async fn concurrent_deny_fallback_does_not_open_dialog_after_suspension() {
    pending_handoff(false, false, false).await;
}

#[tokio::test]
async fn concurrent_requires_user_does_not_open_dialog_after_suspension() {
    pending_handoff(true, false, false).await;
}

#[tokio::test]
async fn concurrent_new_user_input_lifts_suspension_before_pending_handoff() {
    for critical in [false, true] {
        pending_handoff(critical, true, false).await;
    }
}
