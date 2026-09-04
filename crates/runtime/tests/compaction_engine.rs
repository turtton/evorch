mod support;

use std::sync::Arc;

use config::{CompactionConfig, SummarizerKind};
use event_bus::{AgentRunPhase, CompactionEvent, CompactionReason, Event, EventBus, EventKind};
use providers::{ContentBlock, FinishReason, Message};
use runtime::{AgentRuntime, Role, RunConfig, RunId, RuntimeError};
use sandbox::DirectSandbox;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

fn runtime_with(
    model: Arc<ScriptedModel>,
    settings: CompactionConfig,
) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (
        AgentRuntime::new(Arc::clone(&bus), executor, model).with_compaction(settings),
        bus,
    )
}

fn structural_settings(window: u64) -> CompactionConfig {
    CompactionConfig {
        context_window_tokens: window,
        keep_recent_tokens: 1,
        max_summary_bytes: 64,
        summarizer: SummarizerKind::Structural,
        ..CompactionConfig::default()
    }
}

async fn wait_for_phase(runtime: &AgentRuntime, run_id: runtime::RunId, phase: AgentRunPhase) {
    timeout(Duration::from_secs(2), async {
        loop {
            if runtime
                .inspect_agent(run_id)
                .expect("run remains inspectable")
                .phase
                == phase
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("phase transition timeout");
}

async fn compaction_events_until_done(
    receiver: &mut event_bus::EventReceiver,
    run_id: &str,
) -> Vec<Event> {
    let mut compacted = Vec::new();
    timeout(Duration::from_secs(2), async {
        loop {
            let event = receiver.recv().await.expect("event receiver remains open");
            if matches!(&event.kind, EventKind::Compaction(_)) {
                compacted.push(event.clone());
            }
            if matches!(
                &event.kind,
                EventKind::Lifecycle(event_bus::LifecycleEvent::BackgroundTaskCompleted { task_id })
                    if task_id == run_id
            ) {
                return;
            }
        }
    })
    .await
    .expect("run completion event timeout");
    compacted
}

fn has_checkpoint(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text } if text.starts_with("[COMPACTION CHECKPOINT ckpt-"))
        })
    })
}

#[tokio::test]
async fn automatic_compaction_emits_complete_audit_event() {
    // Given: a long first turn that will exceed the configured window after resume
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(&"old answer ".repeat(40), FinishReason::Stop)),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model), structural_settings(160));
    let mut receiver = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        "old request ".repeat(40),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;

    // When: a user message resumes the run and opens the next turn boundary
    runtime
        .send_message(run_id, "continue".to_string())
        .expect("waiting run resumes");
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = compaction_events_until_done(&mut receiver, &run_id.to_string()).await;

    // Then: one complete automatic audit event is emitted and the next request is compacted
    assert_eq!(events.len(), 1);
    let EventKind::Compaction(CompactionEvent::Compacted {
        run_id: event_run_id,
        reason,
        threshold,
        context_window_tokens,
        estimated_tokens_before,
        estimated_tokens_after,
        compacted_range_start,
        compacted_range_end,
        checkpoint_id,
        summary,
    }) = &events[0].kind
    else {
        panic!("expected compacted event")
    };
    assert_eq!(event_run_id, &run_id.to_string());
    assert_eq!(*reason, CompactionReason::Automatic);
    assert_eq!(*threshold, 0.75);
    assert_eq!(*context_window_tokens, 160);
    assert!(estimated_tokens_before > estimated_tokens_after);
    assert!(compacted_range_start < compacted_range_end);
    assert!(checkpoint_id.starts_with("ckpt-"));
    assert!(!summary.is_empty());
    println!(
        "{}",
        serde_json::to_string_pretty(&events[0].kind).expect("compaction event serializes")
    );
    let observed = model.observed().await;
    assert!(has_checkpoint(
        observed.last().expect("resumed provider request")
    ));
}

#[tokio::test]
async fn manual_requests_coalesce_and_run_before_resumed_provider_call() {
    // Given: an interactive run waiting with compactable history
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(&"old answer ".repeat(20), FinishReason::Stop)),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model), structural_settings(1_000_000));
    let mut receiver = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        "old request ".repeat(20),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;

    // When: two manual generations arrive before a message resumes the run
    runtime.compact(run_id).expect("first request accepted");
    runtime.compact(run_id).expect("second request accepted");
    runtime
        .send_message(run_id, "resume".to_string())
        .expect("waiting run resumes");
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = compaction_events_until_done(&mut receiver, &run_id.to_string()).await;

    // Then: the generations coalesce into one manual checkpoint before provider completion
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].kind,
        EventKind::Compaction(CompactionEvent::Compacted {
            reason: CompactionReason::Manual,
            ..
        })
    ));
    let observed = model.observed().await;
    assert!(has_checkpoint(
        observed.last().expect("resumed provider request")
    ));
}

#[tokio::test]
async fn model_summarizer_reply_becomes_checkpoint_summary() {
    // Given: a keyed model script with a distinct summary reply
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    model
        .add_keyed(
            "MODEL-SUMMARY",
            [
                Ok(text_response(&"old answer ".repeat(20), FinishReason::Stop)),
                Ok(text_response(
                    "model generated checkpoint",
                    FinishReason::Stop,
                )),
            ],
        )
        .await;
    let settings = CompactionConfig {
        context_window_tokens: 1_000_000,
        keep_recent_tokens: 1,
        max_summary_bytes: 1_024,
        summarizer: SummarizerKind::Model,
        ..CompactionConfig::default()
    };
    let (runtime, bus) = runtime_with(Arc::clone(&model), settings);
    let mut receiver = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        "MODEL-SUMMARY".to_string(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;

    // When: manual compaction runs at the resumed boundary
    runtime.compact(run_id).expect("manual request accepted");
    runtime
        .send_message(run_id, "resume".to_string())
        .expect("waiting run resumes");
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = compaction_events_until_done(&mut receiver, &run_id.to_string()).await;

    // Then: the model reply is retained as the event and checkpoint summary
    assert_eq!(events.len(), 1);
    let EventKind::Compaction(CompactionEvent::Compacted { summary, .. }) = &events[0].kind else {
        panic!("expected compacted event")
    };
    assert_eq!(summary, "model generated checkpoint");
    let observed = model.observed().await;
    assert_eq!(observed.len(), 3);
    assert!(has_checkpoint(observed.last().expect("resumed request")));
}

#[tokio::test]
async fn summary_failure_preserves_visible_history_and_emits_no_compaction_event() {
    // Given: model summarization fails between a successful waiting turn and its resume
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "ATOMIC-HISTORY",
            [
                Ok(text_response(&"old answer ".repeat(20), FinishReason::Stop)),
                Err(RuntimeError::Model {
                    reason: "summary unavailable".to_string(),
                }),
                Ok(text_response("done", FinishReason::Stop)),
            ],
        )
        .await;
    let settings = CompactionConfig {
        context_window_tokens: 1_000_000,
        keep_recent_tokens: 1,
        summarizer: SummarizerKind::Model,
        ..CompactionConfig::default()
    };
    let (runtime, bus) = runtime_with(Arc::clone(&model), settings);
    let mut receiver = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        "ATOMIC-HISTORY".to_string(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;

    // When: the failing manual compaction is followed by the normal provider turn
    runtime.compact(run_id).expect("manual request accepted");
    runtime
        .send_message(run_id, "resume-after-failure".to_string())
        .expect("waiting run resumes");
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = compaction_events_until_done(&mut receiver, &run_id.to_string()).await;

    // Then: no checkpoint event or partial visible-window mutation escapes
    assert!(events.is_empty());
    let observed = model.observed().await;
    let resumed = observed
        .last()
        .expect("provider continues after summary error");
    assert!(!has_checkpoint(resumed));
    assert!(resumed.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text == "ATOMIC-HISTORY"))
    }));
    assert!(resumed.iter().any(|message| {
        message.content.iter().any(
            |block| matches!(block, ContentBlock::Text { text } if text == "resume-after-failure"),
        )
    }));
}

#[tokio::test]
async fn nothing_to_compact_keeps_run_healthy_and_unknown_run_is_rejected() {
    // Given: a tiny waiting context whose keep budget covers every message
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response("short", FinishReason::Stop)),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let settings = CompactionConfig {
        context_window_tokens: 1_000_000,
        keep_recent_tokens: 1_000_000,
        summarizer: SummarizerKind::Structural,
        ..CompactionConfig::default()
    };
    let (runtime, bus) = runtime_with(Arc::clone(&model), settings);
    let mut receiver = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        "tiny".to_string(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;

    // When: manual compaction is requested for the tiny run and an unknown run
    runtime.compact(run_id).expect("known run accepts request");
    let unknown = runtime.compact(RunId::new(999));
    runtime
        .send_message(run_id, "resume".to_string())
        .expect("waiting run resumes");
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = compaction_events_until_done(&mut receiver, &run_id.to_string()).await;

    // Then: the known run completes without an event and unknown identity is typed
    assert!(events.is_empty());
    assert_eq!(
        unknown,
        Err(RuntimeError::UnknownRun {
            run_id: "run-999".to_string()
        })
    );
    assert!(!has_checkpoint(
        model
            .observed()
            .await
            .last()
            .expect("resumed provider request")
    ));
}

#[tokio::test]
async fn still_above_threshold_compacts_once_without_reentry() {
    // Given: three provider boundaries whose structural checkpoint remains over a tiny window
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(
            &"first response ".repeat(20),
            FinishReason::ToolUse,
        )),
        Ok(text_response(
            &"second response ".repeat(20),
            FinishReason::ToolUse,
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let settings = CompactionConfig {
        context_window_tokens: 40,
        keep_recent_tokens: 1,
        cooldown_turns: 10,
        max_summary_bytes: 1_024,
        summarizer: SummarizerKind::Structural,
        ..CompactionConfig::default()
    };
    let (runtime, bus) = runtime_with(model, settings);
    let mut receiver = bus.subscribe();

    // When: the run crosses all three boundaries under a timeout
    let run_id = runtime.delegate_background(
        Role::Worker,
        "large prompt ".repeat(20),
        RunConfig::default(),
    );
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(run_id)).await,
        Ok(Ok(AgentRunPhase::Done))
    );
    let events = compaction_events_until_done(&mut receiver, &run_id.to_string()).await;

    // Then: a still-large result is accepted once and never recursively compacted
    assert_eq!(events.len(), 1);
    let EventKind::Compaction(CompactionEvent::Compacted {
        estimated_tokens_after,
        context_window_tokens,
        threshold,
        ..
    }) = events[0].kind
    else {
        panic!("expected compacted event")
    };
    assert!(estimated_tokens_after as f64 / context_window_tokens as f64 >= threshold);
}
