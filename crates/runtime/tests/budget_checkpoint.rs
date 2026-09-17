mod support;

use event_bus::{AgentRunPhase, Event, EventBus, EventKind, OrchestratorEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, Role, RunConfig};
use std::sync::Arc;
use support::{ScriptedModel, text_response, tool_response};

#[path = "support/budget_fixture.rs"]
mod budget_fixture;
use budget_fixture::{run_calls, run_calls_in_batches};

#[tokio::test]
async fn five_call_budget_rejects_tail_of_shared_batch() {
    // Given: eight shared reads in one provider response and five permits.
    let config = RunConfig {
        budget: runtime::budget_tracker::BudgetSettings {
            max_tool_calls: 5,
            ..Default::default()
        },
        ..Default::default()
    };
    // When: the real tool executor processes the batch.
    let events = run_calls_in_batches(8, config, true).await;
    // Then: only the first five calls execute, with no tail preflight or execution.
    let mut completed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Tool(event_bus::ToolEvent::ToolCompleted { call_id, .. }) => {
                Some(call_id.as_str())
            }
            _ => None,
        })
        .collect();
    completed.sort_unstable();
    assert_eq!(completed, ["0", "1", "2", "3", "4"]);
    assert_eq!(
        diagnostics(
            &events,
            event_bus::event::diagnostic_codes::BUDGET_EXHAUSTED
        ),
        1
    );
}

#[tokio::test]
async fn budget_overrun_emits_budget_exhausted_once() {
    // Given: a five-call budget, independent of reread and progress limits.
    let config = RunConfig {
        budget: runtime::budget_tracker::BudgetSettings {
            max_tool_calls: 5,
            ..Default::default()
        },
        ..Default::default()
    };
    // When: eight calls are requested.
    let events = run_calls(8, config).await;
    let completed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Tool(event_bus::ToolEvent::ToolCompleted { call_id, .. }) => {
                Some(call_id.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(completed, ["0", "1", "2", "3", "4"]);
    assert!(events.iter().any(|event| matches!(&event.kind,
        EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {to: AgentRunPhase::Error, reason: Some(reason), ..})
        if reason.starts_with("BudgetExhausted:"))));
    let progress = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Orchestrator(OrchestratorEvent::TaskProgressed { progress, .. })
                if progress.get("status").and_then(serde_json::Value::as_str) == Some("failed") =>
            {
                Some(progress)
            }
            _ => None,
        })
        .expect("persisted exhaustion");
    let task: storage::entity::TaskContinuation =
        serde_json::from_value(progress.clone()).expect("typed continuation");
    let cursor: Vec<providers::Message> =
        serde_json::from_str(task.resume_cursor.as_deref().expect("cursor")).expect("messages");
    let results: Vec<_> = cursor
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            providers::ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results, ["0", "1", "2", "3", "4"]);
    let breach = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Diagnostic(diagnostic) if diagnostic.source == "budget_tracker" => {
                Some(diagnostic)
            }
            _ => None,
        })
        .expect("breach diagnostic");
    assert_eq!(
        task.failure_reason,
        Some(format!("{}: {}", breach.code, breach.detail))
    );
    // Then: the threshold is latched for this run.
    assert_eq!(
        diagnostics(
            &events,
            event_bus::event::diagnostic_codes::BUDGET_EXHAUSTED
        ),
        1
    );
}

#[tokio::test]
async fn no_progress_rounds_emits_no_progress_once() {
    // Given: three consecutive rounds without a file change are permitted.
    let config = RunConfig {
        budget: runtime::budget_tracker::BudgetSettings {
            max_no_progress_rounds: 3,
            ..Default::default()
        },
        ..Default::default()
    };
    // When: eight read-only rounds complete.
    let events = run_calls(8, config).await;
    // Then: only one no-progress diagnostic is emitted.
    assert_eq!(
        diagnostics(&events, event_bus::event::diagnostic_codes::NO_PROGRESS),
        1
    );
}

fn diagnostics(events: &[Event], code: &str) -> usize {
    events.iter().filter(|event| matches!(&event.kind, EventKind::Diagnostic(diagnostic) if diagnostic.code == code && diagnostic.severity == event_bus::DiagnosticSeverity::Error && diagnostic.run_id.is_some())).count()
}

#[tokio::test]
async fn checkpoint_fires_every_50_tool_calls() {
    // Given: 101 real read calls with known per-response usage.
    // When: the scripted agent completes.
    let events = run_calls(101, unlimited_progress()).await;
    // Then: only the two exact boundaries emit cumulative checkpoints.
    let checkpoints: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Orchestrator(OrchestratorEvent::TaskCheckpoint {
                tool_call_count,
                cumulative_input_tokens,
                cumulative_output_tokens,
                ..
            }) => Some((
                *tool_call_count,
                *cumulative_input_tokens,
                *cumulative_output_tokens,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(checkpoints, [(50, 150, 100), (100, 300, 200)]);
}

#[tokio::test]
async fn thresholds_do_not_fire_at_the_exact_limit() {
    // Given: eight calls consume exactly forty tokens and seven rereads.
    let config = RunConfig {
        budget: runtime::budget_tracker::BudgetSettings {
            max_tool_calls: 9,
            max_tokens: 40,
            max_file_rereads: 7,
            max_no_progress_rounds: 8,
            ..Default::default()
        },
        ..Default::default()
    };
    // When: all eight rounds complete.
    let events = run_calls(8, config).await;
    // Then: equality does not exceed any budget.
    assert_eq!(
        diagnostics(
            &events,
            event_bus::event::diagnostic_codes::BUDGET_EXHAUSTED
        ),
        0
    );
    assert_eq!(
        diagnostics(&events, event_bus::event::diagnostic_codes::NO_PROGRESS),
        0
    );
}

#[tokio::test]
async fn token_and_reread_thresholds_each_emit_once() {
    // Given: independent limits that both cross during repeated reads.
    let config = RunConfig {
        budget: runtime::budget_tracker::BudgetSettings {
            max_tokens: 10,
            max_file_rereads: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    // When: the agent keeps running after both limits.
    let events = run_calls(8, config).await;
    // Then: the first breach stops further work, without a second diagnostic.
    assert_eq!(
        diagnostics(
            &events,
            event_bus::event::diagnostic_codes::BUDGET_EXHAUSTED
        ),
        1
    );
}

#[tokio::test]
async fn elapsed_threshold_emits_once() {
    // Given: a zero elapsed-time allowance.
    let config = RunConfig {
        budget: runtime::budget_tracker::BudgetSettings {
            max_elapsed: std::time::Duration::ZERO,
            ..Default::default()
        },
        ..Default::default()
    };
    // When: multiple boundaries observe elapsed time.
    let events = run_calls(8, config).await;
    // Then: elapsed exhaustion is emitted only once.
    assert_eq!(
        diagnostics(
            &events,
            event_bus::event::diagnostic_codes::BUDGET_EXHAUSTED
        ),
        1
    );
}

#[tokio::test]
async fn checkpoints_continue_after_escalation_latches() {
    // Given: the default escalation proposal latches at 200 calls.
    let config = RunConfig {
        task_id: Some("durable-task".into()),
        ..unlimited_progress()
    };
    // When: another fifty calls complete.
    let events = run_calls(251, config).await;
    // Then: accounting continues, bound to the durable task, not the run ID.
    let checkpoints: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Orchestrator(OrchestratorEvent::TaskCheckpoint {
                task_id,
                tool_call_count,
                ..
            }) => Some((task_id.as_str(), *tool_call_count)),
            _ => None,
        })
        .collect();
    assert_eq!(
        checkpoints,
        [
            ("durable-task", 50),
            ("durable-task", 100),
            ("durable-task", 150),
            ("durable-task", 200),
            ("durable-task", 250)
        ]
    );
}

fn unlimited_progress() -> RunConfig {
    RunConfig {
        budget: runtime::budget_tracker::BudgetSettings {
            max_file_rereads: u32::MAX,
            max_no_progress_rounds: u32::MAX,
            ..Default::default()
        },
        ..Default::default()
    }
}
