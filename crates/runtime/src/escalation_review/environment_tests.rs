//! Environmental invalidation must preserve the streak for every final outcome.
use super::*;
use tokio::sync::Notify;

#[tokio::test(start_paused = true)]
async fn concurrent_settings_change_during_human_denial_or_timeout_preserves_streak() {
    for timeout in [false, true] {
        for change_mode in [false, true] {
            let mut fixture = Fixture::new(true);
            fixture
                .run
                .add_user_request(&fixture.ctx.run_id, "inspect the project");
            fixture.deny("existing").await;
            fixture.gate.human_timeout = Duration::from_millis(10);
            fixture.gate.reviewer = Some(Arc::new(QuickModelReviewer::new(Arc::new(
                ScriptedModel::new([Ok(super::super::super::support::text_response(
                    r#"{"approve":false,"reason":"pending denial","risk_level":"low","authorization_level":"high"}"#,
                    providers::FinishReason::Stop,
                ))]),
            ))));
            let mut events = fixture.gate.bus.subscribe();
            let decision = fixture.gate.decide(&fixture.ctx, "inspection", "inspect");
            let change = async {
                loop {
                    if let EventKind::Tool(event_bus::ToolEvent::ApprovalRequested {
                        call_id,
                        ..
                    }) = events.recv().await.unwrap().kind
                    {
                        *fixture.gate.settings.lock().unwrap() = (
                            if change_mode {
                                EscalationApproval::Off
                            } else {
                                EscalationApproval::Auto
                            },
                            false,
                        );
                        if !timeout {
                            fixture.gate.bus.emit(Event::new(
                                event_bus::ToolEvent::ApprovalResolved {
                                    call_id,
                                    approved: false,
                                },
                            ));
                        }
                        break;
                    }
                }
            };
            let (decision, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(decision, change)
            })
            .await
            .expect("settings change while the human dialog is pending");
            let expected = if timeout {
                "shell escalation approval timed out"
            } else {
                "shell escalation denied by user"
            };
            assert!(matches!(decision, EscalationDecision::Deny { reason } if reason == expected));
            assert_eq!(
                fixture.denials(&fixture.ctx),
                1,
                "timeout={timeout}, mode={change_mode}"
            );
        }
    }
}

#[tokio::test]
async fn concurrent_evidence_invalidation_during_model_denial_preserves_streak() {
    for unsynced in [false, true] {
        for current_denials in [0, 1, CONSECUTIVE_DENIAL_LIMIT - 1] {
            let fixture = Fixture::new(false);
            for _ in 0..current_denials {
                fixture.deny("existing").await;
            }
            let stamp = fixture.run.stamp();
            let entered = Notify::new();
            let changed = Notify::new();
            let decision = fixture.gate.arbitrate(
                &fixture.ctx,
                "shell escalation",
                |_| async {
                    entered.notify_one();
                    changed.notified().await;
                    Ok((
                        ReviewVerdict::Deny {
                            reason: "stale denial".into(),
                        },
                        Some(stamp),
                    ))
                },
                || async { panic!("no fallback") },
            );
            let invalidate = async {
                entered.notified().await;
                if unsynced {
                    fixture.run.mark_unsynced();
                } else {
                    fixture.run.user_requests.lock().unwrap().evidence_revision += 1;
                }
                changed.notify_one();
            };
            let (decision, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(decision, invalidate)
            })
            .await
            .expect("evidence invalidation while the review is pending");
            assert!(
                matches!(decision, EscalationDecision::Deny { reason } if reason == "stale denial")
            );
            assert_eq!(
                fixture.denials(&fixture.ctx),
                current_denials,
                "unsynced={unsynced}"
            );
        }
    }
}
