mod support;

use std::sync::Arc;

use async_trait::async_trait;
use config::{CompactionConfig, SummarizerKind};
use event_bus::{AgentRunPhase, CompactionEvent, EventBus, EventKind};
use providers::{
    ChatResponse, CompactionResult, ContentBlock, FinishReason, Message, ToolSpec, Usage,
};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};
use sandbox::DirectSandbox;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

const BLOB: &str = "opaque-provider-state-do-not-display+/=";

type CompactionCall = (AgentInvocationContext, Role, Vec<Message>, Vec<ToolSpec>);

struct OfficialModel {
    chat: ScriptedModel,
    result: Result<Option<CompactionResult>, RuntimeError>,
    compacted: Mutex<Vec<CompactionCall>>,
}

#[async_trait]
impl AgentModel for OfficialModel {
    async fn compact_context(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<Option<CompactionResult>, RuntimeError> {
        self.compacted.lock().await.push((
            invocation.clone(),
            role,
            messages.to_vec(),
            tools.to_vec(),
        ));
        self.result.clone()
    }

    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.chat.complete(invocation, role, messages, tools).await
    }

    fn selected_model(&self, _role: Role, _category: Option<&str>) -> String {
        "openai-codex/gpt-5-codex".into()
    }
}

async fn scenario(
    result: Result<Option<CompactionResult>, RuntimeError>,
    official: bool,
    summarizer: SummarizerKind,
) {
    let chat = ScriptedModel::new([Ok(text_response(
        "model fallback summary",
        FinishReason::Stop,
    ))]);
    chat.add_keyed(
        "goal",
        [
            Ok(text_response(
                &"old answer ".repeat(100),
                FinishReason::Stop,
            )),
            Ok(text_response("done", FinishReason::Stop)),
        ],
    )
    .await;
    let model = Arc::new(OfficialModel {
        chat,
        result,
        compacted: Mutex::new(Vec::new()),
    });
    let bus = Arc::new(EventBus::new(256));
    let mut receiver = bus.subscribe();
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    // SwitchableModel も公式 compaction を current model へ委譲する。
    let runtime = AgentRuntime::new(
        bus,
        executor,
        Arc::new(runtime::compose::SwitchableModel::new(model.clone())),
    )
    .with_compaction(CompactionConfig {
        context_window_tokens: 1_000_000,
        keep_recent_tokens: 1,
        max_summary_bytes: if official { 0 } else { 128 },
        summarizer,
        ..CompactionConfig::default()
    });
    let run = runtime.delegate_background(
        Role::Worker,
        "goal".into(),
        RunConfig {
            interactive: true,
            budget: runtime::budget_tracker::BudgetSettings {
                max_tokens: official.then_some(60),
                ..Default::default()
            },
            ..RunConfig::default()
        },
    );
    timeout(Duration::from_secs(5), async {
        while runtime.inspect_agent(run).unwrap().phase != AgentRunPhase::Waiting {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    runtime.compact(run).unwrap();
    runtime.send_message(run, "resume".into()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(5), runtime.wait(run))
            .await
            .unwrap(),
        Ok(AgentRunPhase::Done)
    );
    let mut saw_usage_budget_warning = false;
    let events = timeout(Duration::from_secs(5), async {
        let mut compacted = Vec::new();
        loop {
            let event = receiver.recv().await.unwrap();
            assert!(
                !serde_json::to_string(&event).unwrap().contains(BLOB),
                "暗号文は event/UI に公開しない"
            );
            match event.kind {
                EventKind::Compaction(event) => compacted.push(event),
                EventKind::Diagnostic(diagnostic) if diagnostic.code == "BudgetWarning" => {
                    saw_usage_budget_warning = diagnostic.detail.contains("remaining_tokens=11");
                }
                EventKind::Lifecycle(event_bus::LifecycleEvent::BackgroundTaskCompleted {
                    task_id,
                }) if task_id == run.to_string() => break compacted,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        saw_usage_budget_warning, official,
        "公式 compaction の 49 tokens を予算へ計上する"
    );
    assert_eq!(events.len(), 1);
    let CompactionEvent::Compacted { summary, .. } = &events[0];
    if official {
        assert_eq!(summary, "[provider-side API compaction]");
    } else if summarizer == SummarizerKind::Model {
        assert_eq!(summary, "model fallback summary");
    }
    let observed = model.chat.observed().await;
    let last = observed.last().unwrap();
    assert_eq!(
        observed.len(),
        if !official && summarizer == SummarizerKind::Model {
            3
        } else {
            2
        }
    );
    assert!(last.iter().any(|message| message.role == providers::Role::User && message.content.iter().any(|block| {
        if official {
            matches!(block, ContentBlock::Compaction { encrypted_content } if encrypted_content == BLOB)
        } else {
            matches!(block, ContentBlock::Text { text } if text.starts_with("[COMPACTION CHECKPOINT"))
        }
    })));
    let calls = model.compacted.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.run_id, run.to_string());
    assert_eq!(calls[0].1, Role::Worker);
    assert_eq!(
        calls[0].2.len(),
        1,
        "goal と最近の user message は保護される"
    );
    assert_eq!(calls[0].2[0].role, providers::Role::Assistant);
    assert!(!calls[0].3.is_empty());
}

#[tokio::test]
async fn official_compaction_checkpoint_preserves_blob_without_byte_limit() {
    scenario(
        Ok(Some(CompactionResult {
            encrypted_content: BLOB.into(),
            usage: Usage {
                input_tokens: 42,
                output_tokens: 7,
                ..Usage::default()
            },
        })),
        true,
        SummarizerKind::Model,
    )
    .await;
}

#[tokio::test]
async fn unsupported_compaction_falls_back_to_structural_summary() {
    scenario(Ok(None), false, SummarizerKind::Structural).await;
}

#[tokio::test]
async fn failed_compaction_falls_back_to_model_summary() {
    scenario(
        Err(RuntimeError::Model {
            reason: "official compaction failed".into(),
        }),
        false,
        SummarizerKind::Model,
    )
    .await;
}
