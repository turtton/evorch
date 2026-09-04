mod support;

// allow: SIZE_OK — Wave-9 の budget / ratchet / in-flight を同じ runtime policy
// integration target で監査可能にし、各シナリオ固有の同期 harness を局所化する。

use std::sync::Arc;

use agents::Role;
use async_trait::async_trait;
use config::{CompactionConfig, SummarizerKind};
use event_bus::{AgentRunPhase, CompactionEvent, CompactionReason, EventBus, EventKind};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, RunConfig, RunId, RuntimeError};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

const WAIT: Duration = Duration::from_secs(5);

fn runtime_with(
    model: Arc<dyn AgentModel>,
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

fn settings(window: u64, budget: u64, summarizer: SummarizerKind) -> CompactionConfig {
    CompactionConfig {
        context_window_tokens: window,
        keep_recent_tokens: 1,
        cooldown_turns: 1,
        max_compactions_per_run: budget,
        max_summary_bytes: 128,
        summarizer,
        ..CompactionConfig::default()
    }
}

async fn wait_for_phase(runtime: &AgentRuntime, run_id: RunId, phase: AgentRunPhase) {
    timeout(WAIT, async {
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

async fn wait_for_calls(model: &ScriptedModel, expected: usize) {
    timeout(WAIT, async {
        loop {
            if model.observed().await.len() == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("model call timeout");
}

async fn finish_and_collect(
    runtime: &AgentRuntime,
    receiver: &mut event_bus::EventReceiver,
    run_id: RunId,
) -> Vec<CompactionEvent> {
    assert_eq!(
        timeout(WAIT, runtime.wait(run_id)).await,
        Ok(Ok(AgentRunPhase::Done))
    );
    let mut events = Vec::new();
    timeout(WAIT, async {
        loop {
            match receiver.recv().await.expect("event bus remains open").kind {
                EventKind::Compaction(event) => events.push(event),
                EventKind::Lifecycle(event_bus::LifecycleEvent::BackgroundTaskCompleted {
                    task_id,
                }) if task_id == run_id.to_string() => return,
                _ => {}
            }
        }
    })
    .await
    .expect("completion event timeout");
    events
}

// Given: 1 回だけ許可された run が複数の高使用率境界を通過する
// When: 最初の自動圧縮後に手動要求と次の自動判定が続く
// Then: 予算超過後は新しいイベントを出さず Done で完了する
#[tokio::test]
async fn budget_exhaustion_blocks_further_compactions() {
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [
            Ok(text_response(&"old ".repeat(100), FinishReason::Stop)),
            Ok(text_response(&"high ".repeat(100), FinishReason::ToolUse)),
            Ok(text_response(&"higher ".repeat(100), FinishReason::ToolUse)),
            Ok(text_response("done", FinishReason::Stop)),
        ],
        Arc::clone(&gate),
    ));
    let (runtime, bus) = runtime_with(model.clone(), settings(80, 1, SummarizerKind::Structural));
    let mut receiver = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        "budget-goal".to_string(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_calls(&model, 1).await;
    gate.notify_one();
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;

    runtime
        .send_message(run_id, "resume".to_string())
        .expect("run resumes");
    wait_for_calls(&model, 2).await;
    runtime
        .compact(run_id)
        .expect("manual generation is accepted");
    gate.notify_one();
    wait_for_calls(&model, 3).await;
    gate.notify_one();
    wait_for_calls(&model, 4).await;
    gate.notify_one();
    let events = finish_and_collect(&runtime, &mut receiver, run_id).await;

    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0],
        CompactionEvent::Compacted {
            reason: CompactionReason::Automatic,
            ..
        }
    ));
}

// Given: 手動圧縮で自動ラチェットが立ち、次の境界は閾値未満である
// When: その後の provider 応答で使用量が再び閾値を超える
// Then: 低使用率境界では圧縮せず、次の高使用率境界で自動圧縮を再開する
#[tokio::test]
async fn automatic_ratchet_rearms_only_after_below_threshold_boundary() {
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [
            Ok(text_response(&"old ".repeat(200), FinishReason::Stop)),
            Ok(text_response("small", FinishReason::ToolUse)),
            Ok(text_response(&"large ".repeat(600), FinishReason::ToolUse)),
            Ok(text_response("done", FinishReason::Stop)),
        ],
        Arc::clone(&gate),
    ));
    let (runtime, bus) = runtime_with(model.clone(), settings(800, 4, SummarizerKind::Structural));
    let mut receiver = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        "ratchet-goal".to_string(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_calls(&model, 1).await;
    gate.notify_one();
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;
    runtime
        .compact(run_id)
        .expect("manual compaction requested");
    runtime
        .send_message(run_id, "resume".to_string())
        .expect("run resumes");
    wait_for_calls(&model, 2).await;
    gate.notify_one();
    wait_for_calls(&model, 3).await;
    gate.notify_one();
    wait_for_calls(&model, 4).await;
    gate.notify_one();
    let events = finish_and_collect(&runtime, &mut receiver, run_id).await;

    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0],
        CompactionEvent::Compacted {
            reason: CompactionReason::Manual,
            ..
        }
    ));
    assert!(matches!(
        events[1],
        CompactionEvent::Compacted {
            reason: CompactionReason::Automatic,
            ..
        }
    ));
}

struct BlockingSummaryModel {
    inner: ScriptedModel,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl AgentModel for BlockingSummaryModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let is_summary = messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(block, ContentBlock::Text { text } if text.starts_with("Summarize the compacted conversation for continuation."))
            })
        });
        if is_summary {
            self.started.notify_one();
            self.release.notified().await;
        }
        self.inner.complete(invocation, role, messages, tools).await
    }

    fn selected_model(&self, role: Role) -> String {
        self.inner.selected_model(role)
    }
}

// Given: model summarizer が Notify 待ちで実行中の run
// When: 同じ run に二つ目の runtime.compact を要求する
// Then: CompactionInFlight を返し、解放後は一つの圧縮だけで Done になる
#[tokio::test]
async fn compact_rejected_while_compaction_in_flight() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let inner = ScriptedModel::new([Ok(text_response("summary", FinishReason::Stop))]);
    inner
        .add_keyed(
            "blocking-goal",
            [
                Ok(text_response(&"old ".repeat(100), FinishReason::Stop)),
                Ok(text_response("done", FinishReason::Stop)),
            ],
        )
        .await;
    let model = Arc::new(BlockingSummaryModel {
        inner,
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    });
    let (runtime, bus) = runtime_with(model, settings(1_000_000, 4, SummarizerKind::Model));
    let mut receiver = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        "blocking-goal".to_string(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;

    runtime.compact(run_id).expect("first compaction accepted");
    runtime
        .send_message(run_id, "resume".to_string())
        .expect("run resumes");
    timeout(WAIT, started.notified())
        .await
        .expect("summarizer start timeout");
    assert_eq!(
        runtime.compact(run_id),
        Err(RuntimeError::CompactionInFlight {
            run_id: run_id.to_string()
        })
    );
    release.notify_one();
    let events = finish_and_collect(&runtime, &mut receiver, run_id).await;

    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0],
        CompactionEvent::Compacted {
            reason: CompactionReason::Manual,
            ..
        }
    ));
}
