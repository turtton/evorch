//! entry pre-routing: ユーザーメッセージ到着時に ExecutionShape を事前判定する (issue #71)。
//!
//! 2 段階方式: まずローカルキーワードルール ([`classify_local`]) を適用し、
//! 判定不能だった場合に限り起動予定の Orchestrator と同じモデルで再分類する。
//! 再分類も確定しない場合は fail-safe として Coordinated に倒す。判定結果は
//! [`LifecycleEvent::RoutingDecision`] として event bus へ発行した上で返す。

// allow: SIZE_OK - EntryRouter 本体 (約115純LOC) に、pinned された 12 件の振る舞い
// テスト (stub モデル込み・約290純LOC) が inline テスト慣習どおり同居するため
// 分割不可能。テストを別ファイルへ分離すると impl+test ペアリング規約に反する。

mod keyword;
mod reclassify;

use std::sync::Arc;

use agents::Role;
use event_bus::{Event, EventBus, LifecycleEvent, RoutingSource};

use crate::model::AgentModel;
use crate::prompt::ExecutionShape;
pub use keyword::{
    COORDINATION_KEYWORDS, DIRECT_KEYWORDS, LocalVerdict, UncertainReason, classify_local,
};
use reclassify::{ReclassifyOutcome, reclassify};

/// entry pre-routing の判定結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingDecision {
    /// 判定された ExecutionShape。
    pub shape: ExecutionShape,
    /// 判定理由 (ユーザーメッセージ本文は含めない)。
    pub reason: String,
    /// 判定の出所 (使用ルール or 再分類モデル)。
    pub source: RoutingSource,
}

impl RoutingDecision {
    /// 判定 shape に対応する起動 role を返す (Direct→Worker, Coordinated→Orchestrator)。
    pub const fn role(&self) -> Role {
        match self.shape {
            ExecutionShape::Direct => Role::Worker,
            ExecutionShape::Coordinated => Role::Orchestrator,
        }
    }
}

/// entry pre-routing 判定器。ローカルキーワードルールを先に適用し、
/// 判定不能な場合のみ Orchestrator と同じモデルで再分類する (2 段階方式)。
pub struct EntryRouter {
    model: Arc<dyn AgentModel>,
    bus: Arc<EventBus>,
}

impl EntryRouter {
    /// モデルと event bus から判定器を生成する。
    pub fn new(model: Arc<dyn AgentModel>, bus: Arc<EventBus>) -> Self {
        Self { model, bus }
    }

    /// ユーザーメッセージを分類し、RoutingDecision event を bus へ発行して判定を返す。
    pub async fn classify(&self, message: &str) -> RoutingDecision {
        let decision = match classify_local(message) {
            LocalVerdict::Direct { keyword } => RoutingDecision {
                shape: ExecutionShape::Direct,
                reason: format!("明示的な direct キーワード「{keyword}」を検出した"),
                source: RoutingSource::LocalRule {
                    rule: format!("direct-keyword:{keyword}"),
                },
            },
            LocalVerdict::Coordinated => RoutingDecision {
                shape: ExecutionShape::Coordinated,
                reason: "direct キーワードが検出されなかった".into(),
                source: RoutingSource::LocalRule {
                    rule: "no-direct-keyword".into(),
                },
            },
            LocalVerdict::Uncertain(reason) => {
                let prefix = match reason {
                    UncertainReason::Contradiction {
                        direct,
                        coordination,
                    } => format!(
                        "ローカルルールが矛盾した (direct: {}; coordination: {}) ため Orchestrator モデルで再分類した",
                        direct.join(", "),
                        coordination.join(", ")
                    ),
                    UncertainReason::NoClassifiableText => {
                        "分類対象テキストが無いため Orchestrator モデルで再分類した".to_string()
                    }
                };
                self.decide_by_model(message, &prefix).await
            }
        };

        self.bus.emit(Event::new(LifecycleEvent::RoutingDecision {
            shape: decision.shape.name().to_string(),
            reason: decision.reason.clone(),
            source: decision.source.clone(),
        }));

        decision
    }

    /// 再分類をモデルへ委ねて RoutingDecision を組み立てる。
    ///
    /// 判定できた場合のみ prefix を理由に接続する。fail-safe (一意マーカー無し・
    /// 呼び出し失敗) では Coordinated に倒す。モデルは常に相談済みのため、
    /// 出所はいずれの場合も Model になる。
    async fn decide_by_model(&self, message: &str, reason_prefix: &str) -> RoutingDecision {
        let source = RoutingSource::Model {
            model: self.model.selected_model(Role::Orchestrator),
        };
        let (shape, reason) = match reclassify(&self.model, message).await {
            ReclassifyOutcome::Classified(shape) => {
                (shape, format!("{reason_prefix}: {}", shape.name()))
            }
            ReclassifyOutcome::NoUniqueMarker => (
                ExecutionShape::Coordinated,
                "再分類の応答に一意な ExecutionShape マーカーが無いため Coordinated に倒した"
                    .into(),
            ),
            ReclassifyOutcome::Error(error) => (
                ExecutionShape::Coordinated,
                format!("再分類に失敗したため Coordinated に倒した: {error}"),
            ),
        };
        RoutingDecision {
            shape,
            reason,
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agents::Role;
    use async_trait::async_trait;
    use event_bus::{EventBus, EventKind, LifecycleEvent, RoutingSource};
    use providers::{
        ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
    };
    use tools::ToolExecutor;

    use super::EntryRouter;
    use crate::error::RuntimeError;
    use crate::model::{AgentInvocationContext, AgentModel};
    use crate::prompt::{ExecutionShape, render_routing_gate_body};
    use crate::runtime::AgentRuntime;

    /// 1 回の complete 呼び出しの記録。
    #[derive(Clone)]
    struct RecordedCall {
        invocation: AgentInvocationContext,
        role: Role,
        messages: Vec<Message>,
        tool_count: usize,
    }

    /// complete 呼び出しを記録し、応答を返すテスト用 stub。
    struct StubModel {
        calls: Mutex<Vec<RecordedCall>>,
        response: Mutex<Option<Result<ChatResponse, RuntimeError>>>,
    }

    impl StubModel {
        fn responding_with_text(text: &str) -> Self {
            Self::responding(Ok(ChatResponse {
                message: Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::Text {
                        text: text.to_string(),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            }))
        }

        fn responding_with_tool_use() -> Self {
            Self::responding(Ok(ChatResponse {
                message: Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "call-1".to_string(),
                        name: "shell".to_string(),
                        input: serde_json::json!({}),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::ToolUse,
            }))
        }

        fn failing_with(reason: &str) -> Self {
            Self::responding(Err(RuntimeError::Model {
                reason: reason.to_string(),
            }))
        }

        fn responding(response: Result<ChatResponse, RuntimeError>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                response: Mutex::new(Some(response)),
            }
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.lock().expect("call log lock").clone()
        }
    }

    #[async_trait]
    impl AgentModel for StubModel {
        async fn complete(
            &self,
            invocation: &AgentInvocationContext,
            role: Role,
            messages: &[Message],
            tools: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            self.calls
                .lock()
                .expect("call log lock")
                .push(RecordedCall {
                    invocation: invocation.clone(),
                    role,
                    messages: messages.to_vec(),
                    tool_count: tools.len(),
                });
            self.response
                .lock()
                .expect("response lock")
                .clone()
                .unwrap_or_else(|| {
                    Err(RuntimeError::Model {
                        reason: "no scripted response".to_string(),
                    })
                })
        }

        fn selected_model(&self, _role: Role) -> String {
            "stub-model".to_string()
        }
    }

    /// content ブロックから Text の本文だけを連結する。
    fn text_of(content: &[ContentBlock]) -> String {
        content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Given: direct キーワードで始まるメッセージと応答を埋めた stub モデル
    // When: classify する
    // Then: モデルは呼ばれず Direct 判定・LocalRule 出所・Worker 起動 role
    #[tokio::test]
    async fn local_direct_skips_model_and_reports_local_rule() {
        let stub = Arc::new(StubModel::responding_with_text("unused"));
        let bus = Arc::new(EventBus::new(8));
        let model: Arc<dyn AgentModel> = stub.clone();
        let router = EntryRouter::new(model, Arc::clone(&bus));

        let decision = router.classify("direct: fix the typo").await;

        assert!(stub.calls().is_empty());
        assert_eq!(decision.shape, ExecutionShape::Direct);
        assert_eq!(
            decision.reason,
            "明示的な direct キーワード「direct」を検出した"
        );
        assert_eq!(
            decision.source,
            RoutingSource::LocalRule {
                rule: "direct-keyword:direct".to_string()
            }
        );
        assert_eq!(decision.role(), Role::Worker);
    }

    // Given: キーワードを含まないメッセージと stub モデル
    // When: classify する
    // Then: モデルは呼ばれず Coordinated 判定・no-direct-keyword 出所・Orchestrator 起動 role
    #[tokio::test]
    async fn local_coordinated_skips_model() {
        let stub = Arc::new(StubModel::responding_with_text("unused"));
        let bus = Arc::new(EventBus::new(8));
        let model: Arc<dyn AgentModel> = stub.clone();
        let router = EntryRouter::new(model, Arc::clone(&bus));

        let decision = router.classify("implement issue #65").await;

        assert!(stub.calls().is_empty());
        assert_eq!(decision.shape, ExecutionShape::Coordinated);
        assert_eq!(decision.reason, "direct キーワードが検出されなかった");
        assert_eq!(
            decision.source,
            RoutingSource::LocalRule {
                rule: "no-direct-keyword".to_string()
            }
        );
        assert_eq!(decision.role(), Role::Orchestrator);
    }

    // Given: 矛盾キーワードを含むメッセージと Coordinated マーカーを返す stub モデル
    // When: classify する
    // Then: Orchestrator role・ツールなし・run_id "entry-routing" で 1 回呼ばれ、
    //       System は routing gate 本文で始まる報告形式指示つき、User は入力そのまま
    #[tokio::test]
    async fn uncertain_input_reclassifies_with_orchestrator_role_and_routing_gate_body() {
        let stub = Arc::new(StubModel::responding_with_text(
            "再分類します\nExecutionShape: Coordinated",
        ));
        let bus = Arc::new(EventBus::new(8));
        let model: Arc<dyn AgentModel> = stub.clone();
        let router = EntryRouter::new(model, Arc::clone(&bus));

        let decision = router.classify("direct fix, but delegate the tests").await;

        let calls = stub.calls();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.role, Role::Orchestrator);
        assert_eq!(call.tool_count, 0);
        assert_eq!(call.invocation.run_id, "entry-routing");
        assert_eq!(call.messages.len(), 2);
        assert_eq!(call.messages[0].role, MessageRole::System);
        assert_eq!(call.messages[1].role, MessageRole::User);
        let gate_body = render_routing_gate_body();
        let system_text = text_of(&call.messages[0].content);
        assert!(system_text.starts_with(gate_body.as_str()));
        assert!(system_text.contains("ExecutionShape: Direct"));
        assert!(system_text.contains("ExecutionShape: Coordinated"));
        assert_eq!(
            text_of(&call.messages[1].content),
            "direct fix, but delegate the tests"
        );
        assert_eq!(decision.shape, ExecutionShape::Coordinated);
    }

    // Given: 矛盾キーワードを含むメッセージと Direct マーカーを返す stub モデル
    // When: classify する
    // Then: Direct 判定・出所は再分類モデル識別子・起動 role は Worker
    #[tokio::test]
    async fn model_direct_answer_yields_worker_with_model_source() {
        let stub = Arc::new(StubModel::responding_with_text(
            "単一ロールで完結します\nExecutionShape: Direct",
        ));
        let bus = Arc::new(EventBus::new(8));
        let model: Arc<dyn AgentModel> = stub.clone();
        let router = EntryRouter::new(model, Arc::clone(&bus));

        let decision = router.classify("direct fix, but delegate the tests").await;

        assert_eq!(stub.calls().len(), 1);
        assert_eq!(decision.shape, ExecutionShape::Direct);
        assert_eq!(
            decision.source,
            RoutingSource::Model {
                model: stub.selected_model(Role::Orchestrator)
            }
        );
        assert_eq!(decision.role(), Role::Worker);
    }

    // Given: 矛盾キーワードを含むメッセージと Coordinated マーカーを返す stub モデル
    // When: classify する
    // Then: Coordinated 判定・出所は再分類モデル識別子・起動 role は Orchestrator
    #[tokio::test]
    async fn model_coordinated_answer_yields_orchestrator() {
        let stub = Arc::new(StubModel::responding_with_text(
            "委譲が必要です\nExecutionShape: Coordinated",
        ));
        let bus = Arc::new(EventBus::new(8));
        let model: Arc<dyn AgentModel> = stub.clone();
        let router = EntryRouter::new(model, Arc::clone(&bus));

        let decision = router.classify("direct fix, but delegate the tests").await;

        assert_eq!(stub.calls().len(), 1);
        assert_eq!(decision.shape, ExecutionShape::Coordinated);
        assert_eq!(
            decision.source,
            RoutingSource::Model {
                model: stub.selected_model(Role::Orchestrator)
            }
        );
        assert_eq!(decision.role(), Role::Orchestrator);
    }

    // Given: 矛盾キーワードを含むメッセージとマーカー無し応答を返す stub モデル
    // When: classify する
    // Then: fail-safe の Coordinated 判定・出所は再分類モデル識別子
    #[tokio::test]
    async fn model_answer_without_marker_falls_back_to_coordinated() {
        let stub = Arc::new(StubModel::responding_with_text("たぶん調整が必要でしょう"));
        let bus = Arc::new(EventBus::new(8));
        let model: Arc<dyn AgentModel> = stub.clone();
        let router = EntryRouter::new(model, Arc::clone(&bus));

        let decision = router.classify("direct fix, but delegate the tests").await;

        assert_eq!(stub.calls().len(), 1);
        assert_eq!(decision.shape, ExecutionShape::Coordinated);
        assert_eq!(
            decision.source,
            RoutingSource::Model {
                model: stub.selected_model(Role::Orchestrator)
            }
        );
        assert_eq!(decision.role(), Role::Orchestrator);
    }

    // Given: 矛盾キーワードを含むメッセージと両マーカーを含む応答を返す stub モデル
    // When: classify する
    // Then: 一意に定まらないため fail-safe の Coordinated 判定
    #[tokio::test]
    async fn model_answer_with_conflicting_markers_falls_back_to_coordinated() {
        let stub = Arc::new(StubModel::responding_with_text(
            "ExecutionShape: Direct\nやはり ExecutionShape: Coordinated が良い",
        ));
        let bus = Arc::new(EventBus::new(8));
        let model: Arc<dyn AgentModel> = stub.clone();
        let router = EntryRouter::new(model, Arc::clone(&bus));

        let decision = router.classify("direct fix, but delegate the tests").await;

        assert_eq!(stub.calls().len(), 1);
        assert_eq!(decision.shape, ExecutionShape::Coordinated);
        assert_eq!(
            decision.source,
            RoutingSource::Model {
                model: stub.selected_model(Role::Orchestrator)
            }
        );
    }

    // Given: 矛盾キーワードを含むメッセージと ToolUse ブロックのみの応答を返す stub モデル
    // When: classify する
    // Then: Text ブロックが無いため fail-safe の Coordinated 判定
    #[tokio::test]
    async fn model_tool_use_response_falls_back_to_coordinated() {
        let stub = Arc::new(StubModel::responding_with_tool_use());
        let bus = Arc::new(EventBus::new(8));
        let model: Arc<dyn AgentModel> = stub.clone();
        let router = EntryRouter::new(model, Arc::clone(&bus));

        let decision = router.classify("direct fix, but delegate the tests").await;

        assert_eq!(stub.calls().len(), 1);
        assert_eq!(decision.shape, ExecutionShape::Coordinated);
        assert_eq!(
            decision.source,
            RoutingSource::Model {
                model: stub.selected_model(Role::Orchestrator)
            }
        );
    }

    // Given: 矛盾キーワードを含むメッセージと失敗する stub モデル
    // When: classify する
    // Then: fail-safe の Coordinated 判定・出所は再分類モデル識別子・理由は「倒した」を含む
    #[tokio::test]
    async fn model_error_falls_back_to_coordinated() {
        let stub = Arc::new(StubModel::failing_with("boom"));
        let bus = Arc::new(EventBus::new(8));
        let model: Arc<dyn AgentModel> = stub.clone();
        let router = EntryRouter::new(model, Arc::clone(&bus));

        let decision = router.classify("direct fix, but delegate the tests").await;

        assert_eq!(stub.calls().len(), 1);
        assert_eq!(decision.shape, ExecutionShape::Coordinated);
        assert_eq!(
            decision.source,
            RoutingSource::Model {
                model: stub.selected_model(Role::Orchestrator)
            }
        );
        assert!(decision.reason.contains("Coordinated に倒した"));
    }

    // Given: classify 前に購読した受信者
    // When: direct キーワードつきメッセージを classify する
    // Then: 返り値と同じ shape/reason/source を持つ RoutingDecision イベントを受信する
    #[tokio::test]
    async fn routing_decision_event_is_emitted_with_shape_reason_and_source() {
        let stub = Arc::new(StubModel::responding_with_text("unused"));
        let bus = Arc::new(EventBus::new(8));
        let mut rx = bus.subscribe();
        let model: Arc<dyn AgentModel> = stub.clone();
        let router = EntryRouter::new(model, Arc::clone(&bus));

        let decision = router.classify("direct: x").await;

        let event = rx
            .recv()
            .await
            .expect("RoutingDecision イベントを受信できる");
        assert_eq!(
            event.kind,
            EventKind::Lifecycle(LifecycleEvent::RoutingDecision {
                shape: decision.shape.name().to_string(),
                reason: decision.reason.clone(),
                source: decision.source.clone(),
            })
        );
    }

    // Given: stub モデルを持つ AgentRuntime
    // When: runtime.entry_router().classify で矛盾キーワードを分類する
    // Then: 再分類には runtime のモデルが構造的に使われ、ちょうど 1 回呼ばれる
    #[tokio::test]
    async fn runtime_entry_router_uses_the_runtime_model() {
        let bus = Arc::new(EventBus::new(8));
        let executor = Arc::new(ToolExecutor::new(Arc::clone(&bus)));
        let stub = Arc::new(StubModel::responding_with_text(
            "ExecutionShape: Coordinated",
        ));
        let model: Arc<dyn AgentModel> = stub.clone();
        let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model);

        let decision = runtime
            .entry_router()
            .classify("direct x, delegate y")
            .await;

        assert_eq!(stub.calls().len(), 1);
        assert_eq!(decision.shape, ExecutionShape::Coordinated);
    }
}
