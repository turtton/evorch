mod support;

// allow: SIZE_OK — issue #63 requires five independent end-to-end trigger scenarios in this
// single integration-test target; splitting would violate the requested auditable test surface.

use std::collections::BTreeSet;
use std::sync::Arc;

use agents::Role;
use config::{CompactionConfig, SummarizerKind};
use event_bus::{AgentRunPhase, CompactionEvent, CompactionReason, EventBus, EventKind};
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole};
use runtime::{AgentRuntime, RunConfig, RunId, RuntimeError, SystemPromptCatalog};
use sandbox::DirectSandbox;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

const CHECKPOINT_PREFIX: &str = "[COMPACTION CHECKPOINT ";
const SYSTEM_TEXT: &str = "stable-system-prefix";

fn runtime_with(
    model: Arc<ScriptedModel>,
    settings: CompactionConfig,
) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let mut catalog = SystemPromptCatalog::builder();
    for role in [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
    ] {
        catalog = catalog.role_baseline(role, SYSTEM_TEXT);
    }
    for family in [
        "family-claude",
        "family-openai-reasoning",
        "family-gpt5",
        "family-gemini",
        "family-kimi",
        "family-generic",
    ] {
        catalog = catalog.family_section(family, "family");
    }
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model)
        .with_system_prompts(Arc::new(catalog.build().expect("complete test catalog")))
        .with_compaction(settings);
    (runtime, bus)
}

fn settings(window: u64, keep_recent_tokens: u64, cooldown_turns: u32) -> CompactionConfig {
    CompactionConfig {
        context_window_tokens: window,
        keep_recent_tokens,
        cooldown_turns,
        max_summary_bytes: 128,
        summarizer: SummarizerKind::Structural,
        ..CompactionConfig::default()
    }
}

fn text_message(role: MessageRole, text: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

fn estimated_tokens(messages: &[Message]) -> u64 {
    let chars = serde_json::to_string(messages)
        .expect("test messages serialize")
        .chars()
        .count() as u64;
    chars.div_ceil(4)
}

fn request_texts(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect()
}

fn checkpoint_ids(messages: &[Message]) -> Vec<&str> {
    request_texts(messages)
        .into_iter()
        .filter_map(|text| {
            text.strip_prefix(CHECKPOINT_PREFIX)
                .and_then(|tail| tail.split_once(']'))
                .map(|(id, _)| id)
        })
        .collect()
}

async fn wait_for_phase(runtime: &AgentRuntime, run_id: RunId, phase: AgentRunPhase) {
    timeout(Duration::from_secs(5), async {
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

async fn assembled_system_message(settings: CompactionConfig) -> Message {
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "probe-done",
        FinishReason::Stop,
    ))]));
    let (runtime, _) = runtime_with(Arc::clone(&model), settings);
    let run_id =
        runtime.delegate_background(Role::Worker, "probe".to_string(), RunConfig::default());
    assert_eq!(
        timeout(Duration::from_secs(5), runtime.wait(run_id)).await,
        Ok(Ok(AgentRunPhase::Done))
    );
    model.observed().await[0][0].clone()
}

async fn finish_and_collect(
    runtime: &AgentRuntime,
    receiver: &mut event_bus::EventReceiver,
    run_id: RunId,
) -> Vec<CompactionEvent> {
    assert_eq!(
        timeout(Duration::from_secs(30), runtime.wait(run_id)).await,
        Ok(Ok(AgentRunPhase::Done))
    );
    let mut compacted = Vec::new();
    timeout(Duration::from_secs(5), async {
        loop {
            let event = receiver.recv().await.expect("event bus remains open");
            match event.kind {
                EventKind::Compaction(event) => compacted.push(event),
                EventKind::Lifecycle(event_bus::LifecycleEvent::BackgroundTaskCompleted {
                    task_id,
                }) if task_id == run_id.to_string() => return,
                _ => {}
            }
        }
    })
    .await
    .expect("completion event timeout");
    compacted
}

#[tokio::test]
async fn automatic_fires_once_at_turn_boundary_and_provider_sees_compact_window() {
    // Given: the resumed boundary is exactly 75%, while the first request is below it.
    let window = 400;
    let compaction_settings = settings(window, 1, 1);
    let system = assembled_system_message(compaction_settings.clone()).await;
    let old_request = "old-request-".repeat(8);
    let mut old_reply = String::new();
    let resume = "resume";
    let before_messages = |old_reply: &str| {
        vec![
            system.clone(),
            text_message(MessageRole::User, &old_request),
            text_message(MessageRole::Assistant, old_reply),
            text_message(MessageRole::User, resume),
        ]
    };
    while estimated_tokens(&before_messages(&old_reply)) < window * 3 / 4 {
        old_reply.push('x');
    }
    let boundary_tokens = estimated_tokens(&before_messages(&old_reply));
    assert_eq!(boundary_tokens * 4, window * 3);
    assert!(estimated_tokens(&before_messages(&old_reply)[..2]) * 4 < window * 3);
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(&old_reply, FinishReason::Stop)),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model), compaction_settings);
    let mut receiver = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        old_request.clone(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;

    // When: the user resumes the run, opening the boundary at exactly 75%.
    runtime
        .send_message(run_id, resume.to_string())
        .expect("waiting run resumes");
    let events = finish_and_collect(&runtime, &mut receiver, run_id).await;

    // Then: one automatic event precedes a compacted provider request with stable System prefix.
    assert_eq!(events.len(), 1);
    let CompactionEvent::Compacted {
        reason,
        threshold,
        context_window_tokens,
        estimated_tokens_before,
        estimated_tokens_after,
        compacted_range_start,
        compacted_range_end,
        checkpoint_id,
        ..
    } = &events[0];
    assert_eq!(*reason, CompactionReason::Automatic);
    assert!((*threshold - 0.75).abs() < f64::EPSILON);
    assert_eq!(*context_window_tokens, window);
    assert_eq!(*estimated_tokens_before, boundary_tokens);
    assert!(*estimated_tokens_after < *estimated_tokens_before);
    assert!(compacted_range_start < compacted_range_end);
    assert!(checkpoint_id.starts_with("ckpt-"));
    let observed = model.observed().await;
    assert_eq!(observed.len(), 2);
    assert!(request_texts(&observed[0]).contains(&old_request.as_str()));
    assert!(checkpoint_ids(&observed[0]).is_empty());
    assert_eq!(checkpoint_ids(&observed[1]), vec![checkpoint_id.as_str()]);
    assert!(!request_texts(&observed[1]).contains(&old_request.as_str()));
    assert!(!request_texts(&observed[1]).contains(&old_reply.as_str()));
    assert!(request_texts(&observed[1]).contains(&resume));
    for request in &observed {
        assert_eq!(request[0], observed[0][0]);
    }
}

#[tokio::test]
async fn below_threshold_never_fires() {
    // Given: the complete resumed request is exactly 74% of its configured window.
    let window = 400;
    let compaction_settings = settings(window, 1, 1);
    let system = assembled_system_message(compaction_settings.clone()).await;
    let old_request = "below-request-".repeat(4);
    let mut old_reply = String::new();
    let resume = "below-resume";
    let raw_resumed = |old_reply: &str| {
        vec![
            system.clone(),
            text_message(MessageRole::User, &old_request),
            text_message(MessageRole::Assistant, old_reply),
            text_message(MessageRole::User, resume),
        ]
    };
    while estimated_tokens(&raw_resumed(&old_reply)) < window * 74 / 100 {
        old_reply.push('x');
    }
    let raw_resumed = raw_resumed(&old_reply);
    let raw_tokens = estimated_tokens(&raw_resumed);
    assert_eq!(raw_tokens * 100, window * 74);
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(&old_reply, FinishReason::Stop)),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model), compaction_settings);
    let mut receiver = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        old_request,
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;

    // When: the run resumes below the inclusive automatic threshold.
    runtime
        .send_message(run_id, resume.to_string())
        .expect("waiting run resumes");
    let events = finish_and_collect(&runtime, &mut receiver, run_id).await;

    // Then: no compaction occurs and the provider receives the exact raw history.
    assert!(events.is_empty());
    let observed = model.observed().await;
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[1], raw_resumed);
    assert!(
        observed
            .iter()
            .all(|request| checkpoint_ids(request).is_empty())
    );
}

#[tokio::test]
async fn manual_compact_on_waiting_run_runs_at_resume_boundary() {
    // Given: an interactive Waiting run with compactable history.
    let old_request = "manual-request-".repeat(12);
    let old_reply = "manual-reply-".repeat(12);
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(&old_reply, FinishReason::Stop)),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model), settings(1_000_000, 1, 1));
    let mut receiver = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        old_request.clone(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;

    // When: manual compaction is requested before the Waiting run resumes.
    assert_eq!(runtime.compact(run_id), Ok(()));
    assert_eq!(
        runtime.compact(RunId::new(999_999)),
        Err(RuntimeError::UnknownRun {
            run_id: "run-999999".to_string()
        })
    );
    runtime
        .send_message(run_id, "manual-resume".to_string())
        .expect("waiting run resumes");
    let events = finish_and_collect(&runtime, &mut receiver, run_id).await;

    // Then: the first post-resume completion sees the manual checkpoint, not the raw old turn.
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0],
        CompactionEvent::Compacted {
            reason: CompactionReason::Manual,
            ..
        }
    ));
    let observed = model.observed().await;
    assert_eq!(observed.len(), 2);
    assert_eq!(checkpoint_ids(&observed[1]).len(), 1);
    assert!(!request_texts(&observed[1]).contains(&old_request.as_str()));
    assert!(!request_texts(&observed[1]).contains(&old_reply.as_str()));
}

#[tokio::test]
async fn still_above_threshold_reports_once_and_never_loops() {
    // Given: a tiny window whose first checkpoint and retained tail remain above 75%.
    let huge_prompt = "huge-prompt-".repeat(40);
    let huge_turn = "huge-turn-".repeat(40);
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(&huge_turn, FinishReason::ToolUse)),
        Ok(text_response("turn-two", FinishReason::ToolUse)),
        Ok(text_response("turn-three", FinishReason::ToolUse)),
        Ok(text_response("turn-four", FinishReason::ToolUse)),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model), settings(80, 100, 10));
    let mut receiver = bus.subscribe();

    // When: the run crosses the compacting boundary and three later boundaries.
    let run_id = runtime.delegate_background(Role::Worker, huge_prompt, RunConfig::default());
    let events = finish_and_collect(&runtime, &mut receiver, run_id).await;

    // Then: the still-above diagnostic is emitted once and one checkpoint persists without stacking.
    assert_eq!(events.len(), 1);
    let CompactionEvent::Compacted {
        estimated_tokens_after,
        context_window_tokens,
        threshold,
        checkpoint_id,
        ..
    } = &events[0];
    assert!(*estimated_tokens_after as f64 / *context_window_tokens as f64 >= *threshold);
    let observed = model.observed().await;
    assert_eq!(observed.len(), 5);
    let ids: BTreeSet<&str> = observed
        .iter()
        .flat_map(|request| checkpoint_ids(request))
        .collect();
    assert_eq!(ids, BTreeSet::from([checkpoint_id.as_str()]));
    assert!(
        observed
            .iter()
            .all(|request| checkpoint_ids(request).len() <= 1)
    );
    assert_eq!(checkpoint_ids(&observed[0]).len(), 0);
    assert!(
        observed[1..]
            .iter()
            .all(|request| checkpoint_ids(request).len() == 1)
    );
}

#[tokio::test]
async fn duplicate_manual_requests_within_cooldown_boundary_coalesce() {
    // Given: a compactable run paused in Waiting.
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(
            &"coalesce-reply-".repeat(12),
            FinishReason::Stop,
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model), settings(1_000_000, 1, 10));
    let mut receiver = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        "coalesce-request-".repeat(12),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;

    // When: duplicate manual generations arrive before the same resume boundary.
    assert_eq!(runtime.compact(run_id), Ok(()));
    assert_eq!(runtime.compact(run_id), Ok(()));
    runtime
        .send_message(run_id, "coalesce-resume".to_string())
        .expect("waiting run resumes");
    let events = finish_and_collect(&runtime, &mut receiver, run_id).await;

    // Then: the boundary handles the latest generation once as a manual compaction.
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0],
        CompactionEvent::Compacted {
            reason: CompactionReason::Manual,
            ..
        }
    ));
    let observed = model.observed().await;
    assert_eq!(checkpoint_ids(&observed[1]).len(), 1);
}
