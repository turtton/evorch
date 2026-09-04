//! ロールごとの独立したエージェントコンテキスト。

use agents::Role;
use providers::{ContentBlock, Message, Role as MessageRole, ToolResultContent};
use tools::ToolResult;

use crate::run::RunId;

/// 圧縮チェックポイント。raw messages は変更せず、表示窓だけを狭める。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionCheckpoint {
    /// チェックポイントの一意な識別子。
    pub id: String,
    /// 表示窓の切替位置へ挿入するユーザーロールの要約メッセージ。
    pub summary: Message,
    /// 表示窓で `summary` に置換する raw message index の半開区間 `[start, end)`。
    pub range: (usize, usize),
}

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
    /// 会話履歴 (システム・ユーザー・アシスタント・ツール結果)。
    pub messages: Vec<Message>,
    /// raw 履歴を保持したままモデル可視窓を投影する圧縮履歴。
    checkpoints: Vec<CompactionCheckpoint>,
}

impl AgentContext {
    /// 空の履歴でコンテキストを生成する。
    pub fn new(run_id: RunId, role: Role) -> Self {
        Self {
            run_id,
            role,
            messages: Vec::new(),
            checkpoints: Vec::new(),
        }
    }

    /// システムプロンプトを履歴に追加する (Role::System + 単一 Text ブロック)。
    pub fn push_system(&mut self, text: &str) {
        self.messages.push(Message {
            role: MessageRole::System,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        });
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

    /// 圧縮チェックポイントを追加する。
    ///
    /// # Panics
    ///
    /// チェックポイントの範囲が raw 履歴の範囲外、または空のとき panic する。
    pub fn apply_checkpoint(&mut self, checkpoint: CompactionCheckpoint) {
        let (start, end) = checkpoint.range;
        assert!(start < end, "checkpoint range must not be empty");
        assert!(
            end <= self.messages.len(),
            "checkpoint range exceeds raw history"
        );
        self.checkpoints.push(checkpoint);
    }

    /// 最後に適用された圧縮チェックポイントを返す。
    #[must_use]
    pub fn latest_checkpoint(&self) -> Option<&CompactionCheckpoint> {
        self.checkpoints.last()
    }

    /// モデルに送るメッセージ窓を raw 履歴から投影する。
    #[must_use]
    pub fn visible_messages(&self) -> Vec<Message> {
        let Some(checkpoint) = self.latest_checkpoint() else {
            return self.messages.clone();
        };
        let (start, end) = checkpoint.range;
        let mut visible = Vec::with_capacity(self.messages.len() - (end - start) + 1);
        visible.extend_from_slice(&self.messages[..start]);
        visible.push(checkpoint.summary.clone());
        visible.extend_from_slice(&self.messages[end..]);
        visible
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

    fn summary_checkpoint(id: &str, range: (usize, usize), text: &str) -> CompactionCheckpoint {
        CompactionCheckpoint {
            id: id.to_string(),
            summary: Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: text.to_string(),
                }],
            },
            range,
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

    // Given: 空のコンテキスト / When: push_system / Then: System ロール + 単一 Text ブロックのメッセージが 1 件追加される
    #[test]
    fn push_system_appends_system_role_message() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);

        context.push_system("あなたは Worker です");

        assert_eq!(context.messages.len(), 1);
        assert_eq!(context.messages[0].role, MessageRole::System);
        assert_eq!(
            context.messages[0].content,
            vec![ContentBlock::Text {
                text: "あなたは Worker です".to_string(),
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

    // Given: チェックポイントなしの履歴 / When: visible_messages を取得 / Then: raw 履歴と同じメッセージが返る
    #[test]
    fn visible_messages_returns_raw_history_without_checkpoint() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_system("システム指示");
        context.push_user("調査して");

        let visible = context.visible_messages();

        assert_eq!(visible, context.messages);
    }

    // Given: raw 履歴 / When: 先頭範囲を要約するチェックポイントを適用 / Then: raw 履歴は不変で可視窓だけが要約へ置換される
    #[test]
    fn visible_messages_replaces_checkpoint_range_without_mutating_raw_history() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_user("最初の依頼");
        context.push_assistant(assistant_text("最初の応答"));
        context.push_user("最新の依頼");
        let raw_messages = context.messages.clone();
        let checkpoint = summary_checkpoint("ckpt-run-1-1", (0, 2), "最初の会話の要約");

        context.apply_checkpoint(checkpoint.clone());
        let visible = context.visible_messages();

        assert_eq!(context.messages.len(), raw_messages.len());
        assert_eq!(context.messages, raw_messages);
        assert_eq!(visible, vec![checkpoint.summary, raw_messages[2].clone()]);
    }

    // Given: 2 個のチェックポイント / When: 後のチェックポイントを適用して可視窓を取得 / Then: 最新だけが投影に使われる
    #[test]
    fn latest_checkpoint_supersedes_prior_checkpoint_for_visible_messages() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_user("最初の依頼");
        context.push_assistant(assistant_text("最初の応答"));
        context.push_user("次の依頼");
        let first = summary_checkpoint("ckpt-run-1-1", (0, 2), "古い要約");
        let latest = summary_checkpoint("ckpt-run-1-2", (1, 3), "新しい要約");

        context.apply_checkpoint(first);
        context.apply_checkpoint(latest.clone());
        let visible = context.visible_messages();

        assert_eq!(context.latest_checkpoint(), Some(&latest));
        assert_eq!(visible, vec![context.messages[0].clone(), latest.summary]);
    }

    // Given: 保護された System prefix を持つ raw 履歴 / When: 内部範囲を要約 / Then: 先頭の System メッセージは byte-identical に残る
    #[test]
    fn visible_messages_preserves_protected_system_prefix_for_interior_checkpoint() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_system("この指示は保存する");
        context.push_user("古い依頼");
        context.push_assistant(assistant_text("古い応答"));
        context.push_user("最新の依頼");
        let protected_system = context.messages[0].clone();
        let checkpoint = summary_checkpoint("ckpt-run-1-1", (1, 3), "古い会話の要約");

        context.apply_checkpoint(checkpoint.clone());
        let visible = context.visible_messages();

        assert_eq!(visible[0], protected_system);
        assert_eq!(
            visible,
            vec![
                protected_system,
                checkpoint.summary,
                context.messages[3].clone()
            ]
        );
    }
}
