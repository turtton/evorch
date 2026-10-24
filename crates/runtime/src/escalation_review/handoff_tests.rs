//! Ordered handoffs from reviews that started before another review suspended the run.
use super::*;
use tokio::sync::Notify;

async fn pending_handoff(critical: bool, new_input: bool) {
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
        }
        release.notify_one();
    };
    let (decision, ()) = tokio::join!(pending, reach_limit);
    if new_input {
        assert!(matches!(decision, EscalationDecision::Deny { reason }
            if reason == "shell escalation approval timed out"));
        assert_eq!(
            fixture.denials(&fixture.ctx),
            0,
            "old verdict cannot revive the streak"
        );
    } else {
        assert!(refused_without_review(&decision), "{}", describe(&decision));
        assert!(describe(&decision).contains("last: threshold denial"));
        assert_eq!(
            fixture.denials(&fixture.ctx),
            CONSECUTIVE_DENIAL_LIMIT + u32::from(!critical)
        );
        if !critical {
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
    assert_eq!(fixture.reviewed(), 4);
    assert_eq!(
        fixture.asked(),
        if critical { 0 } else { 3 } + usize::from(new_input)
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
        usize::from(new_input),
        "no new dialog after suspension"
    );
}

#[tokio::test]
async fn concurrent_deny_fallback_does_not_open_dialog_after_suspension() {
    pending_handoff(false, false).await;
}

#[tokio::test]
async fn concurrent_requires_user_does_not_open_dialog_after_suspension() {
    pending_handoff(true, false).await;
}

#[tokio::test]
async fn concurrent_new_user_input_lifts_suspension_before_pending_handoff() {
    for critical in [false, true] {
        pending_handoff(critical, true).await;
    }
}
