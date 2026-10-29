//! モデル呼び出しの境界トレイト。
//!
//! runtime はモデル名・プロバイダ選択のロジックを一切持たない (issue #7 の
//! ルーティング委譲要件)。

use agents::Role;
use async_trait::async_trait;
use providers::{ChatResponse, Message, ToolSpec};

use crate::error::RuntimeError;

/// Explicit per-run provider selection; `None` model uses the profile default.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelPreference {
    pub profile: String,
    pub model: Option<String>,
    /// Reasoning effort sent with this selection; `None` uses the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// The member a run pinned in one role chain, kept in run snapshots (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutePinRecord {
    /// Affinity key of the chain (role, or role and category).
    pub chain: String,
    /// Position in the expanded chain; re-located by profile and model if it moved.
    pub position: usize,
    pub profile: String,
    pub model: String,
}

/// 1 回のモデル呼び出し (agent-loop の complete) の相関文脈。
///
/// agent-loop は実行中 run の [`RunId`](crate::RunId) をここへ載せて
/// [`AgentModel::complete`] へ渡す。production 実装はこれを provider request
/// の観測相関 (`ChatRequest.observation`) へ写し、provider attempt 観測イベント
/// へ run 相関を stamp する。テスト・demo 実装は受け取って無視してよい
/// (`_invocation`)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentInvocationContext {
    /// モデル呼び出しを行う run の ID ([`crate::RunId`] の表示形式)。
    pub run_id: String,
    pub model_preference: Option<ModelPreference>,
    pub category: Option<String>,
    /// 呼び出しの目的。provider 観測イベントへ写し、usage 集計で付随コストを区別する。
    pub purpose: event_bus::RequestPurpose,
    /// Logical input identity; model retries must retain this context unchanged.
    pub turn: Option<providers::RequestTurnContext>,
    /// Where the provider attempts made for this call are noted. Observation
    /// only: it reaches no wire request.
    pub attempts: Option<providers::AttemptLog>,
}

/// ロール実行のためのモデル呼び出し境界。
///
/// role→model の解決・フォールバックは v01-routing-profiles がこの境界の実装として
/// 提供する。runtime は model 名を一切持たない。このトレイトは「ロールと会話と
/// ツール定義を渡して応答を受け取る」ことのみを規定し、`role` は実装側
/// (routing profiles) がモデル解決に使う引数である。
#[async_trait]
pub trait AgentModel: Send + Sync {
    /// Whether request assembly selects editing tools separately for each candidate.
    /// Legacy/fixed implementations keep edit/write via the runtime call boundary.
    fn narrows_edit_tools(&self) -> bool {
        false
    }

    /// Editing interface of the run's selected route, for context/UI inspection.
    /// Models without routing information use the legacy edit/write interface.
    fn edit_tool_for_run(
        &self,
        _run_id: &str,
        _role: Role,
        _category: Option<&str>,
    ) -> Option<config::EditToolConfig> {
        None
    }

    /// Transient editing interfaces used by agent requests, keyed by (run, chain).
    /// Reconfiguration carries these over; they are not part of persisted snapshots.
    fn edit_tools_by_run(
        &self,
    ) -> std::collections::BTreeMap<(String, String), config::EditToolConfig> {
        std::collections::BTreeMap::new()
    }

    /// Restores request history, not edit-tool choices inferred from new settings.
    fn restore_edit_tools(
        &self,
        _history: &std::collections::BTreeMap<(String, String), config::EditToolConfig>,
    ) {
    }

    /// Commits an editing-interface boundary only after context preparation succeeds.
    fn model_switch_prepared(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _selected: &str,
    ) {
    }

    /// Mutable composition wrappers expose the model to retain for a whole
    /// invocation, including tool capabilities used after its response arrives.
    fn invocation_snapshot(&self) -> Option<std::sync::Arc<dyn AgentModel>> {
        None
    }

    /// Fixed image-generator capability for a run; independent of its chat route.
    fn image_generation_enabled(&self) -> bool {
        false
    }

    async fn generate_image(
        &self,
        _invocation: &AgentInvocationContext,
        _request: providers::ImageGenerationRequest,
    ) -> Result<crate::image_generation::GeneratedImage, String> {
        Err("image generation is not configured".into())
    }

    /// Resolve an optional hosted search capability for the invocation that
    /// produced the tool call. The callback accounts completed search usage even
    /// when response parsing or cancellation prevents delivery of its result.
    fn web_search_provider(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _tools: &[ToolSpec],
        _usage_sink: std::sync::Arc<dyn Fn(providers::Usage) + Send + Sync>,
    ) -> Result<Option<std::sync::Arc<dyn tools::search::SearchProvider>>, RuntimeError> {
        Ok(None)
    }

    /// Resolve exact model and effective generation for a trusted benchmark.
    async fn benchmark_settings(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _tools: &[ToolSpec],
    ) -> Result<crate::benchmark::BenchmarkModelSettings, RuntimeError> {
        Err(crate::benchmark::unsupported(
            "model cannot freeze request settings",
        ))
    }

    /// Return a model with an explicit route, stable settings and no fallback.
    fn freeze_for_benchmark(
        self: std::sync::Arc<Self>,
        _settings: crate::benchmark::BenchmarkModelSettings,
    ) -> Result<std::sync::Arc<dyn AgentModel>, RuntimeError> {
        Err(crate::benchmark::unsupported(
            "model cannot replay request settings",
        ))
    }
    fn requires_admission(&self) -> bool {
        false
    }

    async fn admit(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// provider が公式 compaction を提供する場合のみ利用する。
    /// 非対応は None、失敗は Err とし、呼び出し元が既存の要約へフォールバックする。
    async fn compact_context(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<Option<providers::CompactionResult>, RuntimeError> {
        Ok(None)
    }

    /// Validate D3 and replay boundaries before estimating the next window.
    async fn prepare_replay_context(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _replay: &crate::ResponsesReplayState,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Resolve the actual candidate scope before D3/window preparation.
    ///
    /// The default is for models without Responses replay. Implementations that
    /// return `ResponsesReasoning` must participate in replay preparation and
    /// check the prepared scope before sending (as `RoutedModel` does). Response
    /// origins alone must never bind durable state or reconstruct its floor.
    async fn resolve_replay_scope(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _tools: &[ToolSpec],
    ) -> Result<Option<providers::ResolvedResponsesScope>, RuntimeError> {
        Ok(None)
    }

    /// Replay-aware seams keep legacy models source compatible.
    /// The default does not authorize opaque replay or infer a scope from output.
    async fn complete_streaming_with_replay(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
        bus: &event_bus::EventBus,
        _replay: &crate::ResponsesReplayState,
    ) -> Result<ChatResponse, RuntimeError> {
        self.complete_streaming(invocation, role, messages, tools, bus)
            .await
    }

    /// ロールの会話履歴に対して補完を要求する。
    ///
    /// `invocation` は呼び出し元 run の相関文脈である。実装側は観測相関
    /// (provider attempt イベントの run_id stamp) へ写すことが期待されるが、
    /// 相関が不要な実装は無視してよい。
    ///
    /// # Errors
    /// 境界の実装側 (モデル呼び出し) の失敗は [`RuntimeError::Model`] に寄せられる。
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError>;

    /// Requests a schema-constrained final answer without tool access.
    /// Implementations without structured output retain the prompt-only path.
    /// Callers must still validate the response and handle incomplete/refused output.
    async fn complete_structured(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        _schema: &providers::JsonSchema,
    ) -> Result<ChatResponse, RuntimeError> {
        self.complete(invocation, role, messages, &[]).await
    }

    /// Completes with live deltas on `bus`; legacy models emit deferred deltas once.
    /// Overrides own delta emission; callers must never replay the canonical response.
    /// The canonical response alone supplies history and tools. Dropping cancels it,
    /// but already published display deltas cannot be retracted.
    /// Arguments mirror `complete` to preserve the existing model boundary.
    async fn complete_streaming(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
        bus: &event_bus::EventBus,
    ) -> Result<ChatResponse, RuntimeError> {
        let response = self.complete(invocation, role, messages, tools).await?;
        for block in &response.message.content {
            let event = match block {
                providers::ContentBlock::Text { text } if !text.is_empty() => {
                    event_bus::MessageEvent::MessageDelta {
                        delta: text.clone(),
                        run_id: Some(invocation.run_id.clone()),
                    }
                }
                block if block.reasoning_text().is_some_and(|text| !text.is_empty()) => {
                    event_bus::MessageEvent::ReasoningDelta {
                        delta: block.reasoning_text().unwrap().to_owned(),
                        run_id: Some(invocation.run_id.clone()),
                    }
                }
                providers::ContentBlock::Compaction { .. } => {
                    tracing::warn!("この処理では compaction block をスキップします");
                    continue;
                }
                providers::ContentBlock::Image { .. }
                | providers::ContentBlock::Text { .. }
                | providers::ContentBlock::Reasoning { .. }
                | providers::ContentBlock::Anthropic { .. }
                | providers::ContentBlock::ResponsesReasoning { .. }
                | providers::ContentBlock::ToolUse { .. }
                | providers::ContentBlock::ToolResult { .. } => continue,
            };
            bus.emit(event_bus::Event::new(event));
        }
        Ok(response)
    }

    /// ロールに選択されたモデル識別子を報告する。
    ///
    /// 実装側 (routing profile 層) がロールごとの選択済みモデル identity を報告し、
    /// runtime はそれをそのまま記録する。runtime は解決を行わない
    /// (lib.rs の「ルーティングの委譲」契約と一貫)。
    fn selected_model(&self, role: Role, category: Option<&str>) -> String;

    /// Identity the run's next request goes to, following its chain pin when it has one.
    fn selected_model_for_run(&self, _run_id: &str, role: Role, category: Option<&str>) -> String {
        self.selected_model(role, category)
    }

    /// Catalog window for an identity returned by `selected_model`, if known.
    fn catalog_context_window(&self, _selected_model: &str) -> Option<u64> {
        None
    }

    /// Configured picker entries; fixed models expose no provider profiles.
    fn available_profiles(&self) -> Vec<crate::compose::ProfileSummary> {
        Vec::new()
    }

    /// Reasoning effort the routed candidate for `selected_model` sends, for inspection.
    fn selected_reasoning_effort(&self, _role: Role, _category: Option<&str>) -> Option<String> {
        None
    }

    /// Logical model and generation overrides the role binding resolves to, for inspection.
    /// Fixed models have no binding and return `None`.
    fn binding_preview(
        &self,
        _role: Role,
        _category: Option<&str>,
    ) -> Option<config::types::agents::ResolvedAgentBinding> {
        None
    }

    /// Chain pins the run holds, for its snapshot. Fixed models do not route.
    fn route_pins(&self, _run_id: &str) -> Vec<RoutePinRecord> {
        Vec::new()
    }

    /// Seeds a restored run's chain pins; the next resolution checks them as usual.
    fn restore_route_pins(&self, _run_id: &str, _pins: &[RoutePinRecord]) {}

    /// Every run's chain pins, carried over when a reconfigured model replaces this one.
    fn route_pins_by_run(&self) -> std::collections::BTreeMap<String, Vec<RoutePinRecord>> {
        std::collections::BTreeMap::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::{ContentBlock, FinishReason, Role as MessageRole, Usage};

    #[derive(Default)]
    struct ToolObserver(std::sync::Mutex<Vec<Vec<ToolSpec>>>);

    #[async_trait]
    impl AgentModel for ToolObserver {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "fixed-model".into()
        }

        async fn complete(
            &self,
            _: &AgentInvocationContext,
            _: Role,
            _: &[Message],
            tools: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            self.0.lock().unwrap().push(tools.to_vec());
            Ok(ChatResponse {
                message: Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            })
        }

        async fn compact_context(
            &self,
            _: &AgentInvocationContext,
            _: Role,
            _: &[Message],
            tools: &[ToolSpec],
        ) -> Result<Option<providers::CompactionResult>, RuntimeError> {
            self.0.lock().unwrap().push(tools.to_vec());
            Ok(None)
        }
    }

    #[tokio::test]
    async fn legacy_model_boundary_narrows_complete_streaming_and_compaction() {
        let observer = std::sync::Arc::new(ToolObserver::default());
        assert!(!observer.narrows_edit_tools());
        assert_eq!(observer.edit_tool_for_run("run", Role::Worker, None), None);
        let model = crate::compose::SwitchableModel::new(observer.clone());
        let specs = ["edit", "apply_patch", "write"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: name.into(),
                input_schema: serde_json::json!({}),
            })
            .collect::<Vec<_>>();
        let invocation = AgentInvocationContext::default();
        model
            .complete(&invocation, Role::Worker, &[], &specs)
            .await
            .unwrap();
        model
            .invocation_snapshot()
            .unwrap()
            .complete_streaming(
                &invocation,
                Role::Worker,
                &[],
                &specs,
                &event_bus::EventBus::new(8),
            )
            .await
            .unwrap();
        model
            .compact_context(&invocation, Role::Worker, &[], &specs)
            .await
            .unwrap();
        let observed = observer.0.lock().unwrap();
        assert_eq!(observed.len(), 3);
        for tools in observed.iter() {
            assert_eq!(tools, &[specs[0].clone(), specs[2].clone()]);
        }
        assert_eq!(specs.len(), 3);
    }

    #[tokio::test]
    async fn fixed_runtime_keeps_apply_patch_out_of_stub_requests() {
        let observer = std::sync::Arc::new(ToolObserver::default());
        let bus = std::sync::Arc::new(event_bus::EventBus::new(128));
        let executor = std::sync::Arc::new(tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        let runtime = crate::AgentRuntime::new(bus, executor, observer.clone());
        let run =
            runtime.delegate_background(Role::Worker, "task".into(), crate::RunConfig::default());
        assert_eq!(runtime.wait(run).await.unwrap(), crate::AgentRunPhase::Done);
        let observed = observer.0.lock().unwrap();
        assert_eq!(observed.len(), 1);
        let names = observed[0]
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"write"));
        assert!(!names.contains(&"apply_patch"));
    }

    /// 履歴長を本文へエコーする stub 実装。
    struct EchoModel;

    #[tokio::test]
    async fn streaming_falls_back_when_model_only_implements_complete() {
        // Given: a legacy model behind the object-safe boundary.
        let model: &dyn AgentModel = &EchoModel;
        let bus = event_bus::EventBus::new(8);
        // When: the streaming API is requested.
        let response = model
            .complete_streaming(
                &AgentInvocationContext::default(),
                Role::Worker,
                &[],
                &[],
                &bus,
            )
            .await
            .expect("legacy completion");
        // Then: its canonical completion is preserved without an override.
        assert_eq!(
            response.message.content,
            vec![ContentBlock::Text {
                text: "0 messages".into()
            }]
        );
    }

    #[async_trait]
    impl AgentModel for EchoModel {
        async fn complete(
            &self,
            _invocation: &AgentInvocationContext,
            _role: Role,
            messages: &[Message],
            _tools: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            Ok(ChatResponse {
                message: Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::Text {
                        text: format!("{} messages", messages.len()),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            })
        }

        fn selected_model(&self, _role: Role, _category: Option<&str>) -> String {
            "echo".to_string()
        }
    }

    // Given: EchoModel を dyn AgentModel として用意し 2 件の履歴
    // When: 境界経由で complete を呼ぶ
    // Then: object-safe に呼び出せ、実装へ渡った履歴長が応答本文に現れる
    #[tokio::test]
    async fn agent_model_is_object_safe_and_passes_history() {
        let model: std::sync::Arc<dyn AgentModel> = std::sync::Arc::new(EchoModel);
        let history = vec![
            Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "a".to_string(),
                }],
            },
            Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "b".to_string(),
                }],
            },
        ];

        let invocation = AgentInvocationContext {
            attempts: None,
            turn: None,
            category: None,
            run_id: "run-1".to_string(),
            model_preference: None,
            purpose: Default::default(),
        };
        let response = model
            .complete(&invocation, Role::Worker, &history, &[])
            .await
            .expect("stub は常に成功する");

        assert_eq!(response.message.role, MessageRole::Assistant);
        assert_eq!(
            response.message.content,
            vec![ContentBlock::Text {
                text: "2 messages".to_string(),
            }]
        );
    }
}
