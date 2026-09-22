use std::{sync::Arc, time::Duration};

use agents::Role;
use async_trait::async_trait;
use event_bus::{Event, EventBus, MessageEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};

use super::{ModelSummarizer, SummarizeError, Summarizer, SummaryInput};
use crate::{AgentInvocationContext, AgentModel, RuntimeError};

struct SlowStream {
    interval: Duration,
    chunks: u32,
}

#[async_trait]
impl AgentModel for SlowStream {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        panic!("summary must use streaming")
    }

    async fn complete_streaming(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        tools: &[ToolSpec],
        bus: &EventBus,
    ) -> Result<ChatResponse, RuntimeError> {
        assert!(tools.is_empty());
        assert_eq!(invocation.category.as_deref(), Some("coding"));
        assert_eq!(
            invocation.model_preference.as_ref().unwrap().profile,
            "kimi"
        );
        for _ in 0..self.chunks {
            tokio::time::sleep(self.interval).await;
            bus.emit(Event::new(MessageEvent::ReasoningDelta {
                run_id: Some(invocation.run_id.clone()),
                delta: "summary reasoning".into(),
            }));
        }
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "continuation summary".into(),
                }],
            },
            usage: Usage {
                input_tokens: 1200,
                output_tokens: 40,
                cache_read_tokens: 1000,
                ..Usage::default()
            },
            finish_reason: FinishReason::Stop,
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "slow-stream".into()
    }
}

fn summarizer(interval: u64, chunks: u32) -> ModelSummarizer {
    ModelSummarizer {
        model: Arc::new(SlowStream {
            interval: Duration::from_secs(interval),
            chunks,
        }),
        role: Role::Worker,
        run_id: "run-summary".into(),
        category: Some("coding".into()),
        model_preference: Some(crate::ModelPreference {
            profile: "kimi".into(),
            model: None,
        }),
        idle_timeout: Duration::from_secs(90),
        timeout: Duration::from_secs(300),
    }
}

#[tokio::test(start_paused = true)]
async fn streaming_summary_survives_former_sixty_second_deadline() {
    let start = tokio::time::Instant::now();
    let summary = summarizer(30, 4)
        .summarize(&SummaryInput {
            goal: Some("goal"),
            compacted: &[],
        })
        .await
        .unwrap();
    assert_eq!(summary, "continuation summary");
    assert_eq!(start.elapsed(), Duration::from_secs(120));
}

#[tokio::test(start_paused = true)]
async fn silent_summary_is_cancelled_at_idle_deadline() {
    let result = summarizer(91, 1)
        .summarize(&SummaryInput {
            goal: None,
            compacted: &[],
        })
        .await;
    assert!(matches!(
        result,
        Err(SummarizeError::IdleTimeout { seconds: 90 })
    ));
}

#[tokio::test(start_paused = true)]
async fn active_summary_still_has_overall_deadline() {
    let result = summarizer(30, 20)
        .summarize(&SummaryInput {
            goal: None,
            compacted: &[],
        })
        .await;
    assert!(matches!(
        result,
        Err(SummarizeError::Deadline { seconds: 300 })
    ));
}

#[tokio::test(start_paused = true)]
async fn summary_exposes_reported_usage_for_run_budget_accounting() {
    let (summary, usage) = summarizer(1, 1)
        .summarize_with_usage(&SummaryInput {
            goal: None,
            compacted: &[],
        })
        .await;
    assert_eq!(summary.unwrap(), "continuation summary");
    let usage = usage.unwrap();
    assert_eq!(usage.input_tokens, 1200);
    assert_eq!(usage.output_tokens, 40);
    assert_eq!(usage.cache_read_tokens, 1000);
}
