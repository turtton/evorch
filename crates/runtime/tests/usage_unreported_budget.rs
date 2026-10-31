//! A response without usage is not free: the run's cumulative token budget charges an
//! estimate for it, while an explicitly reported zero stays zero.
mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind};
use futures_util::FutureExt;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, Usage};
use runtime::budget_tracker::BudgetSettings;
use runtime::{AgentRuntime, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

/// Returns the terminal phase and how many `BudgetExhausted` diagnostics were emitted.
async fn run(usage_reported: bool) -> (Result<AgentRunPhase, runtime::RuntimeError>, usize) {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("x.rs");
    std::fs::write(&target, "fn x() {}\n").unwrap();
    let turn = ChatResponse {
        usage_reported,
        message: Message {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "x".repeat(8_000),
                },
                ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "read".into(),
                    input: json!({ "path": target }),
                },
            ],
        },
        usage: Usage::default(),
        finish_reason: FinishReason::ToolUse,
    };
    let model = Arc::new(ScriptedModel::new([
        Ok(turn),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let bus = Arc::new(EventBus::new(256));
    let mut receiver = bus.subscribe();
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model).with_sequential_run_ids();
    let run = runtime.delegate_background(
        Role::Worker,
        "read it".into(),
        RunConfig {
            budget: BudgetSettings {
                max_tokens: Some(1_000),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let phase = runtime.wait(run).await;
    let mut exhausted = 0;
    while let Some(event) = receiver.recv().now_or_never() {
        if let EventKind::Diagnostic(diagnostic) = event.unwrap().kind
            && diagnostic.code == event_bus::event::diagnostic_codes::BUDGET_EXHAUSTED
        {
            exhausted += 1;
        }
    }
    (phase, exhausted)
}

// Given: a ~2k-token turn against a 1k-token budget, with usage reported as zero
// When: the run proceeds / Then: the measured zero is honored and the budget is not exceeded
#[tokio::test]
async fn reported_zero_usage_charges_nothing() {
    assert_eq!(run(true).await, (Ok(AgentRunPhase::Done), 0));
}

// Given: the same turn, but the provider reported no usage
// When: the run proceeds / Then: the estimate is charged and the token budget is exhausted once
#[tokio::test]
async fn unreported_usage_is_charged_as_an_estimate() {
    let (phase, exhausted) = run(false).await;
    assert_eq!(phase, Ok(AgentRunPhase::Error));
    assert_eq!(exhausted, 1);
}
