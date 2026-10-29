//! Per-call network reviews stay outside the escalation-denial streak.
use super::*;
use crate::escalation_review::{
    CONSECUTIVE_DENIAL_LIMIT, DENIAL_GUIDANCE,
    support::{ScriptedModel, text_response},
};
use event_bus::{EventKind, ToolEvent};
use providers::FinishReason;
use tokio::sync::watch;

fn fixture() -> (
    crate::AgentRuntime,
    LoopState,
    watch::Sender<crate::RunInterrupt>,
) {
    let (cancel, rx) = watch::channel(crate::RunInterrupt::None);
    let (runtime, mut state) = super::super::delegate_cleanup_tests::fixture(rx);
    state.shared.executor = Arc::new(
        tools::ToolExecutor::with_standard_tools(
            runtime.shared.bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )
        .with_web_tools()
        .unwrap(),
    );
    let run = state.task.run_id;
    let review = runtime.shared.review_runs.lock().unwrap()[&run].clone();
    review.add_user_request(&run.to_string(), "Read the public documentation");
    for _ in 0..CONSECUTIVE_DENIAL_LIMIT {
        runtime
            .shared
            .escalation_denials
            .record_denial(run, 1, "shell policy denial");
    }
    (runtime, state, cancel)
}

#[tokio::test]
async fn every_network_review_outcome_preserves_streak_and_denial_guidance() {
    for outcome in [
        "approve",
        "deny",
        "model_error",
        "context_change",
        "tool_policy",
    ] {
        let (runtime, mut state, _cancel) = fixture();
        let run = state.task.run_id;
        let mut events = runtime.shared.bus.subscribe();
        let release = Arc::new(tokio::sync::Notify::new());
        let response = match outcome {
            "deny" => Ok(text_response(
                r#"{"approve":false,"reason":"policy denial","risk_level":"low","authorization_level":"high"}"#,
                FinishReason::Stop,
            )),
            "model_error" => Err(crate::RuntimeError::Model {
                reason: "unavailable".into(),
            }),
            _ => Ok(text_response(
                r#"{"approve":true,"reason":"approved","risk_level":"low","authorization_level":"high"}"#,
                FinishReason::Stop,
            )),
        };
        let model = Arc::new(ScriptedModel::gated([response], release.clone()));
        state.shared.model = model.clone();
        if outcome == "tool_policy" {
            runtime.set_web_tools_enabled(false);
        }
        let review = runtime.shared.review_runs.lock().unwrap()[&run].clone();
        let input = serde_json::json!({"url":"https://example.com"});
        let gate = state.gate_network_tool("web_fetch", "fetch", &input);
        let change = async {
            if outcome != "tool_policy" {
                model.wait_for_request(0).await;
                if outcome == "context_change" {
                    // Invalidate evidence without advancing the input generation.
                    review.mark_unsynced();
                }
                release.notify_one();
            }
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(gate, change) })
                .await
                .expect("ordered network review");
        if outcome == "approve" {
            assert!(matches!(result, NetworkGate::Proceed));
        } else {
            let NetworkGate::Reject(result) = result else {
                panic!("must reject {outcome}")
            };
            assert!(result.is_error);
            assert_eq!(
                result.content.matches(DENIAL_GUIDANCE).count(),
                1,
                "{outcome}: {}",
                result.content
            );
            if outcome == "context_change" {
                assert!(
                    result
                        .content
                        .starts_with("review context changed during review\n")
                );
            }
        }
        assert_eq!(
            model.observed().await.len(),
            usize::from(outcome != "tool_policy")
        );
        assert_eq!(
            runtime.shared.escalation_denials.denials(run),
            CONSECUTIVE_DENIAL_LIMIT,
            "{outcome}"
        );
        let mut audits = Vec::new();
        while let Ok(Ok(event)) =
            tokio::time::timeout(Duration::from_millis(10), events.recv()).await
        {
            if let EventKind::Diagnostic(diagnostic) = event.kind {
                assert!(!diagnostic.detail.contains(DENIAL_GUIDANCE));
                if diagnostic.code == diagnostic_codes::TOOL_CALL_ACCESS {
                    audits.push(serde_json::from_str::<Value>(&diagnostic.detail).unwrap());
                }
            }
        }
        assert_eq!(audits.len(), 1, "{outcome}: {audits:?}");
        assert_eq!(
            audits[0]["decision"],
            if outcome == "approve" {
                "allow"
            } else {
                "deny"
            }
        );
        if outcome == "context_change" {
            assert_eq!(audits[0]["reason"], "review context changed during review");
        }
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();
    }
}

#[tokio::test]
async fn a_web_review_records_what_it_sent_beside_the_call() {
    let (runtime, mut state, _cancel) = fixture();
    let mut events = runtime.shared.bus.subscribe();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        r#"{"approve":true,"reason":"ok","risk_level":"low","authorization_level":"high"}"#,
        FinishReason::Stop,
    ))]));
    state.shared.model = model.clone();
    let input = serde_json::json!({"url":"https://example.com"});
    let result = state.gate_network_tool("web_fetch", "fetch", &input).await;
    assert!(matches!(result, NetworkGate::Proceed));

    let sent = model.observed().await;
    let [providers::ContentBlock::Text { text: payload }] = sent[0][1].content.as_slice() else {
        panic!("one user text block");
    };
    let records: Vec<_> = events
        .drain_pending_snapshot()
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Diagnostic(diagnostic)
                if diagnostic.code == diagnostic_codes::ESCALATION_REVIEW_MANIFEST =>
            {
                Some(diagnostic)
            }
            _ => None,
        })
        .collect();
    let [record] = records.as_slice() else {
        panic!("one record, got {records:?}");
    };
    assert_eq!(record.call_id.as_deref(), Some("fetch"));
    assert_eq!(
        record.run_id.as_deref(),
        Some(state.task.run_id.to_string().as_str())
    );
    let manifest: crate::escalation_review::ReviewManifest =
        serde_json::from_str(&record.detail).unwrap();
    assert_eq!(manifest.scope, "web");
    assert_eq!(manifest.tool.as_deref(), Some("web_fetch"));
    let digest = manifest.payload.unwrap();
    assert_eq!(
        digest.sha256,
        crate::escalation_review::sha256_hex(payload.as_bytes())
    );
    assert_eq!(digest.bytes, payload.len() as u64);
    runtime.cancel(state.task.run_id).unwrap();
    runtime.wait(state.task.run_id).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn critical_network_approval_wait_can_timeout_or_cancel() {
    for cancel_wait in [false, true] {
        let (runtime, mut state, cancel) = fixture();
        let run = state.task.run_id;
        state
            .transition(event_bus::AgentRunPhase::Running, None)
            .unwrap();
        state.shared.model = Arc::new(ScriptedModel::new([Ok(text_response(
            r#"{"approve":true,"reason":"critical effects","risk_level":"critical","authorization_level":"high"}"#,
            FinishReason::Stop,
        ))]));
        let mut events = runtime.shared.bus.subscribe();
        let input = serde_json::json!({"url":"https://example.com"});
        let mut pending = Box::pin(state.gate_network_tool("web_fetch", "critical", &input));
        loop {
            tokio::select! {
                _ = &mut pending => panic!("human approval must be pending"),
                event = events.recv() => if matches!(event.unwrap().kind,
                    EventKind::Tool(ToolEvent::ApprovalRequested { .. })) { break; }
            }
        }
        if cancel_wait {
            cancel.send_replace(crate::RunInterrupt::Cancel);
            assert!(matches!(pending.await, NetworkGate::Cancelled));
        } else {
            tokio::time::advance(WEB_APPROVAL_TIMEOUT).await;
            let NetworkGate::Reject(result) = pending.await else {
                panic!("must time out")
            };
            assert!(result.content.starts_with("approval timed out:"));
            assert_eq!(result.content.matches(DENIAL_GUIDANCE).count(), 1);
            assert_eq!(
                runtime.shared.escalation_denials.denials(run),
                CONSECUTIVE_DENIAL_LIMIT
            );
        }
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();
    }
}
