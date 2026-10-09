//! 委譲直後の取り消しが繰り返されたときの診断を、実際の agent loop で検証する。

mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{
    AgentRunPhase, DiagnosticEvent, EventBus, EventKind, EventReceiver, LifecycleEvent, RunActivity,
};
use providers::FinishReason;
use runtime::{AgentRuntime, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, drain_events, text_response, tool_response};

/// Waits for the next lifecycle event `select` accepts and returns what it extracts.
async fn next_lifecycle<T>(
    receiver: &mut EventReceiver,
    mut select: impl FnMut(LifecycleEvent) -> Option<T>,
) -> T {
    loop {
        let event = timeout(Duration::from_secs(10), receiver.recv())
            .await
            .expect("lifecycle event")
            .expect("bus open");
        if let EventKind::Lifecycle(lifecycle) = event.kind
            && let Some(value) = select(lifecycle)
        {
            return value;
        }
    }
}

async fn retract(
    targets: &[serde_json::Value],
) -> (AgentRunPhase, Vec<DiagnosticEvent>, Vec<AgentRunPhase>) {
    // Given: a parent turn gate, so each cancel can name the child it just spawned,
    // and children that never answer until cancelled.
    let model = Arc::new(ScriptedModel::new([]));
    let parent_turn = Arc::new(Notify::new());
    model.gate_key("ORCH", Arc::clone(&parent_turn)).await;
    model.gate_key("CHILD", Arc::new(Notify::new())).await;
    let bus = Arc::new(EventBus::new(1024));
    let mut receiver = bus.subscribe();
    let mut diagnostics = bus.subscribe();
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus, executor, model.clone());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "ORCH".to_owned(), RunConfig::default());

    // When: every delegation is cancelled in the parent's very next turn.
    let mut children = Vec::new();
    for (turn, target) in targets.iter().enumerate() {
        let delegate = json!({"target": target, "prompt": "CHILD", "background": true});
        model
            .add_keyed(
                "ORCH",
                [Ok(tool_response(&format!("d{turn}"), "delegate", delegate))],
            )
            .await;
        parent_turn.notify_one();
        let child = next_lifecycle(&mut receiver, |event| match event {
            LifecycleEvent::AgentRunStarted {
                run_id,
                parent_run_id: Some(parent_run_id),
                ..
            } if parent_run_id == parent.to_string() => Some(run_id),
            _ => None,
        })
        .await;
        let cancel = json!({"run_id": child});
        model
            .add_keyed(
                "ORCH",
                [Ok(tool_response(&format!("c{turn}"), "cancel", cancel))],
            )
            .await;
        parent_turn.notify_one();
        // The cancel reply is consumed once the child ends; only then queue the next turn.
        next_lifecycle(&mut receiver, |event| match event {
            LifecycleEvent::AgentRunStateChanged { run_id, to, .. }
                if run_id == child && to == AgentRunPhase::Error =>
            {
                Some(())
            }
            _ => None,
        })
        .await;
        children.push(child.parse().unwrap());
    }
    // Child cancellation notices wake the parent again; answer each later turn.
    let notices = targets.len() + 1;
    model
        .add_keyed(
            "ORCH",
            (0..notices).map(|_| Ok(text_response("done", FinishReason::Stop))),
        )
        .await;
    parent_turn.notify_one();
    let phase = next_lifecycle(&mut receiver, |event| match event {
        LifecycleEvent::RunProgress {
            run_id,
            activity: RunActivity::Model,
            ..
        } if run_id == parent.to_string() => {
            parent_turn.notify_one();
            None
        }
        LifecycleEvent::AgentRunStateChanged { run_id, to, .. }
            if run_id == parent.to_string()
                && matches!(
                    to,
                    AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                ) =>
        {
            Some(to)
        }
        _ => None,
    })
    .await;
    let mut child_phases = Vec::new();
    for child in children {
        child_phases.push(runtime.wait(child).await.unwrap());
    }
    let diagnostics = drain_events(&mut diagnostics)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Diagnostic(diagnostic) if diagnostic.code == "DelegationRetracted" => {
                Some(diagnostic)
            }
            _ => None,
        })
        .collect();
    (phase, diagnostics, child_phases)
}

#[tokio::test]
async fn repeated_next_turn_cancels_report_the_retracted_target_once() {
    let plan_review = json!({"role":"reviewer","category":"plan-review"});
    let (phase, diagnostics, children) = retract(&[
        plan_review.clone(),
        plan_review.clone(),
        plan_review.clone(),
        plan_review,
    ])
    .await;

    // Then: the parent keeps running, and one warning names the retracted target.
    assert_eq!(phase, AgentRunPhase::Done);
    assert!(children.iter().all(|phase| *phase == AgentRunPhase::Error));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let diagnostic = &diagnostics[0];
    assert_eq!(diagnostic.severity, event_bus::DiagnosticSeverity::Warning);
    assert!(diagnostic.run_id.is_some());
    assert!(
        diagnostic
            .detail
            .ends_with("\nsite=role=reviewer category=plan-review"),
        "{}",
        diagnostic.detail
    );
}

#[tokio::test]
async fn retractions_spread_across_targets_stay_below_the_threshold() {
    let (phase, diagnostics, _) = retract(&[
        json!({"role":"reviewer","category":"plan-review"}),
        json!({"role":"worker","category":"deep"}),
        json!({"role":"reviewer","category":"plan-review"}),
        json!({"role":"worker"}),
    ])
    .await;

    assert_eq!(phase, AgentRunPhase::Done);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}
