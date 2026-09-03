//! モデル呼び出しの境界トレイト。
//!
//! runtime はモデル名・プロバイダ選択のロジックを一切持たない (issue #7 の
//! ルーティング委譲要件)。

use agents::Role;
use async_trait::async_trait;
use providers::{ChatResponse, Message, ToolSpec};

use crate::error::RuntimeError;

/// 1 回のモデル呼び出し (agent-loop の complete) の相関文脈。
///
/// agent-loop は実行中 run の [`RunId`](crate::RunId) をここへ載せて
/// [`AgentModel::complete`] へ渡す。production 実装はこれを provider request
/// の観測相関 (`ChatRequest.observation`) へ写し、provider attempt 観測イベント
/// へ run 相関を stamp する。テスト・demo 実装は受け取って無視してよい
/// (`_invocation`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInvocationContext {
    /// モデル呼び出しを行う run の ID (`run-{n}` 形式)。
    pub run_id: String,
}

/// ロール実行のためのモデル呼び出し境界。
///
/// role→model の解決・フォールバックは v01-routing-profiles がこの境界の実装として
/// 提供する。runtime は model 名を一切持たない。このトレイトは「ロールと会話と
/// ツール定義を渡して応答を受け取る」ことのみを規定し、`role` は実装側
/// (routing profiles) がモデル解決に使う引数である。
#[async_trait]
pub trait AgentModel: Send + Sync {
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

    /// ロールに選択されたモデル識別子を報告する。
    ///
    /// 実装側 (routing profile 層) がロールごとの選択済みモデル identity を報告し、
    /// runtime はそれをそのまま記録する。runtime は解決を行わない
    /// (lib.rs の「ルーティングの委譲」契約と一貫)。
    fn selected_model(&self, role: Role) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::{ContentBlock, FinishReason, Role as MessageRole, Usage};

    /// 履歴長を本文へエコーする stub 実装。
    struct EchoModel;

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

        fn selected_model(&self, _role: Role) -> String {
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
            run_id: "run-1".to_string(),
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
