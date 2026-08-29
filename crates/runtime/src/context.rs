//! ロールごとの独立したエージェントコンテキスト。

use agents::Role;
use providers::{ContentBlock, Message, Role as MessageRole, ToolResultContent};
use tools::ToolResult;

use crate::run::RunId;

/// 単一 AgentRun の会話コンテキスト。
///
/// # 独立性の保証 (independence by construction)
///
/// `AgentContext` は共有参照・global state を一切持たない plain data であり、
/// その run を実行するタスクが独占的に所有する (owned exclusively by its run's
/// task)。複数 AgentRun の文脈独立性は、ロックや ID 検証ではなく所有権
/// (ownership) によって構成上保証される。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentContext {
    /// 所有する run の ID。
    pub run_id: RunId,
    /// この run を実行するロール。
    pub role: Role,
    /// 会話履歴 (ユーザー・アシスタント・ツール結果)。
    pub messages: Vec<Message>,
}

impl AgentContext {
    /// 空の履歴でコンテキストを生成する。
    pub fn new(run_id: RunId, role: Role) -> Self {
        Self {
            run_id,
            role,
            messages: Vec::new(),
        }
    }

    /// ユーザー発話を履歴に追加する。
    pub fn push_user(&mut self, text: &str) {
        self.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        });
    }

    /// アシスタント応答 (provider の `ChatResponse::message`) をそのまま履歴に追加する。
    pub fn push_assistant(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// ツール実行結果をユーザーロールのメッセージとして履歴に追加する。
    ///
    /// ツール結果は provider の canonical 形式ではユーザーロールで送る
    /// (Anthropic Messages API 互換の慣習)。
    pub fn push_tool_result(&mut self, tool_call_id: impl Into<String>, result: ToolResult) {
        self.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: tool_call_id.into(),
                content: vec![ToolResultContent::Text {
                    text: result.content,
                }],
                is_error: result.is_error,
            }],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_text(text: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    // Given: run-1 の Worker ロール / When: new で生成 / Then: 履歴は空
    #[test]
    fn new_context_starts_with_empty_history() {
        let context = AgentContext::new(RunId::new(1), Role::Worker);

        assert_eq!(context.run_id, RunId::new(1));
        assert_eq!(context.role, Role::Worker);
        assert!(context.messages.is_empty());
    }

    // Given: 空のコンテキスト / When: push_user / Then: User ロール + Text ブロックのメッセージが 1 件追加される
    #[test]
    fn push_user_appends_user_text_message() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);

        context.push_user("ファイルを読んで");

        assert_eq!(context.messages.len(), 1);
        assert_eq!(context.messages[0].role, MessageRole::User);
        assert_eq!(
            context.messages[0].content,
            vec![ContentBlock::Text {
                text: "ファイルを読んで".to_string(),
            }]
        );
    }

    // Given: 空のコンテキスト / When: push_assistant / Then: 渡した Message がそのまま 1 件追加される
    #[test]
    fn push_assistant_appends_message_verbatim() {
        let mut context = AgentContext::new(RunId::new(1), Role::Reviewer);
        let response = assistant_text("レビュー完了");

        context.push_assistant(response.clone());

        assert_eq!(context.messages, vec![response]);
    }

    // Given: 空のコンテキスト / When: success と error のツール結果を push_tool_result
    // Then: User ロール + ToolResult ブロック (tool_call_id・本文・is_error) のメッセージが 2 件追加される
    #[test]
    fn push_tool_result_appends_user_tool_result_message() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);

        context.push_tool_result("call_1", ToolResult::success("42 行 match"));
        context.push_tool_result("call_2", ToolResult::error("timeout"));

        assert_eq!(context.messages.len(), 2);
        assert_eq!(context.messages[0].role, MessageRole::User);
        assert_eq!(
            context.messages[0].content,
            vec![ContentBlock::ToolResult {
                tool_call_id: "call_1".to_string(),
                content: vec![ToolResultContent::Text {
                    text: "42 行 match".to_string(),
                }],
                is_error: false,
            }]
        );
        assert_eq!(context.messages[1].role, MessageRole::User);
        assert_eq!(
            context.messages[1].content,
            vec![ContentBlock::ToolResult {
                tool_call_id: "call_2".to_string(),
                content: vec![ToolResultContent::Text {
                    text: "timeout".to_string(),
                }],
                is_error: true,
            }]
        );
    }

    // Given: 空のコンテキスト / When: user / assistant / tool_result を順に push
    // Then: 履歴が時系列どおり 3 件 (User / Assistant / User) で蓄積される
    #[test]
    fn history_accumulates_in_order() {
        let mut context = AgentContext::new(RunId::new(9), Role::Explorer);

        context.push_user("grep して");
        context.push_assistant(assistant_text("grep を実行します"));
        context.push_tool_result("call_1", ToolResult::success("3 件"));

        assert_eq!(context.messages.len(), 3);
        assert_eq!(context.messages[0].role, MessageRole::User);
        assert_eq!(context.messages[1].role, MessageRole::Assistant);
        assert_eq!(context.messages[2].role, MessageRole::User);
    }
}
