//! モデル呼び出しの境界トレイト。
//!
//! runtime はモデル名・プロバイダ選択のロジックを一切持たない (issue #7 の
//! ルーティング委譲要件)。

use agents::Role;
use async_trait::async_trait;
use providers::{ChatResponse, Message, ToolSpec};

use crate::error::RuntimeError;

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
    /// # Errors
    /// 境界の実装側 (モデル呼び出し) の失敗は [`RuntimeError::Model`] に寄せられる。
    async fn complete(
        &self,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError>;
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

        let response = model
            .complete(Role::Worker, &history, &[])
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
