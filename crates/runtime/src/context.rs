//! ロールごとの独立したエージェントコンテキスト。

use agents::Role;
use providers::{ContentBlock, Message, Role as MessageRole, ToolResultContent};
use serde::{Deserialize, Serialize};
use tools::ToolResult;

use crate::run::RunId;

/// 圧縮チェックポイント。raw messages は変更せず、表示窓だけを狭める。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionCheckpoint {
    /// チェックポイントの一意な識別子。
    pub id: String,
    /// 表示窓の切替位置へ挿入するユーザーロールの要約メッセージ。
    pub summary: Message,
    /// 表示窓で `summary` に置換する raw message index の半開区間 `[start, end)`。
    /// 範囲内の System メッセージは要約直前に原文・順序を保って残す。
    pub range: (usize, usize),
    /// A model-switch replacement must never be undone by a later compaction cut.
    #[serde(default, skip_serializing_if = "is_false")]
    pub protects_history: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl CompactionCheckpoint {
    /// Keep durable instructions even when they were appended after conversation
    /// history, such as a workspace note added while restoring a run.
    pub(crate) fn project(&self, messages: &[Message]) -> Vec<Message> {
        let (start, end) = self.range;
        let mut visible = Vec::with_capacity(messages.len() - (end - start) + 1);
        visible.extend_from_slice(&messages[..start]);
        visible.extend(
            messages[start..end]
                .iter()
                .filter(|message| message.role == MessageRole::System)
                .cloned(),
        );
        visible.push(self.summary.clone());
        visible.extend_from_slice(&messages[end..]);
        visible
    }
}

/// Durable raw-index replay boundary. Never reconstruct this from item origins.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesReplayState {
    pub floor: usize,
    pub binding: ResponsesReplayBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResponsesReplayBinding {
    #[default]
    Unbound,
    Bound {
        scope: providers::ResolvedResponsesScope,
    },
    Reset,
}

impl ResponsesReplayState {
    pub fn scope(&self) -> Option<&providers::ResolvedResponsesScope> {
        match &self.binding {
            ResponsesReplayBinding::Bound { scope } => Some(scope),
            ResponsesReplayBinding::Unbound | ResponsesReplayBinding::Reset => None,
        }
    }

    pub fn reset(end: usize) -> Self {
        Self {
            floor: end,
            binding: ResponsesReplayBinding::Reset,
        }
    }

    pub fn prepare(&mut self, scope: Option<providers::ResolvedResponsesScope>, end: usize) {
        if self.scope() == scope.as_ref() {
            return;
        }
        if self.scope().is_some() {
            self.floor = end;
        }
        self.binding = match scope {
            Some(scope) => ResponsesReplayBinding::Bound { scope },
            None => ResponsesReplayBinding::Reset,
        };
    }

    pub(crate) fn validate(
        &self,
        messages: &[Message],
        checkpoints: &[CompactionCheckpoint],
    ) -> Result<(), String> {
        let opaque = messages
            .iter()
            .chain(checkpoints.iter().map(|c| &c.summary))
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, ContentBlock::ResponsesReasoning { .. }));
        let reset_has_live_opaque = matches!(self.binding, ResponsesReplayBinding::Reset)
            && messages
                .iter()
                .enumerate()
                .chain(
                    checkpoints
                        .iter()
                        .map(|c| (c.range.1.saturating_sub(1), &c.summary)),
                )
                .any(|(i, m)| {
                    i >= self.floor
                        && m.content
                            .iter()
                            .any(|b| matches!(b, ContentBlock::ResponsesReasoning { .. }))
                });
        if reset_has_live_opaque
            || self.floor > messages.len()
            || (matches!(self.binding, ResponsesReplayBinding::Unbound)
                && (opaque || self.floor != 0))
        {
            return Err("invalid Responses replay state".into());
        }
        Ok(())
    }

    pub(crate) fn for_saved_end(&self, end: usize) -> Self {
        Self {
            floor: self.floor.min(end),
            binding: self.binding.clone(),
        }
    }

    pub(crate) fn filter(
        &self,
        indexed: Vec<(usize, Message)>,
        scope: Option<&providers::ResolvedResponsesScope>,
    ) -> Vec<(usize, Message)> {
        indexed
            .into_iter()
            .filter_map(|(index, mut message)| {
                message.content.retain(|block| {
                    !matches!(block, ContentBlock::ResponsesReasoning { .. })
                        || (index >= self.floor
                            && scope.is_some_and(|scope| {
                                providers::responses_replay::eligible(block, message.role, scope)
                            }))
                });
                (!message.content.is_empty()).then_some((index, message))
            })
            .collect()
    }
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
    pub responses_replay: ResponsesReplayState,
}

/// A text-only view plus the trusted replacement body that structural fallback
/// must preserve. The index is in the raw log, not the projected message list.
pub(crate) struct TextCompactionView {
    pub(crate) indexed: Vec<(usize, Message)>,
    pub(crate) protected: Option<(usize, String)>,
}

impl AgentContext {
    /// 空の履歴でコンテキストを生成する。
    pub fn new(run_id: RunId, role: Role) -> Self {
        Self::from_restored(
            run_id,
            role,
            Vec::new(),
            Vec::new(),
            ResponsesReplayState::default(),
        )
    }

    pub(crate) fn from_restored(
        run_id: RunId,
        role: Role,
        messages: Vec<Message>,
        checkpoints: Vec<CompactionCheckpoint>,
        responses_replay: ResponsesReplayState,
    ) -> Self {
        Self {
            run_id,
            role,
            messages,
            checkpoints,
            responses_replay,
        }
    }

    pub(crate) fn checkpoints(&self) -> &[CompactionCheckpoint] {
        &self.checkpoints
    }

    pub(crate) fn compaction_view(&self) -> Vec<(usize, Message)> {
        self.responses_replay
            .filter(self.raw_compaction_view(), self.responses_replay.scope())
    }

    /// Native checkpoints remain useful on their provider, but cannot be a
    /// knowledge source for a text-only re-summary. Recover the last readable D3
    /// replacement and its raw tail, never the batch that D3 deliberately hid.
    pub(crate) fn text_compaction_view(&self) -> Result<TextCompactionView, &'static str> {
        let checkpoint = self.checkpoints.iter().rev().find(|checkpoint| {
            checkpoint.protects_history
                && !checkpoint
                    .summary
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Compaction { .. }))
        });
        if checkpoint.is_none()
            && self
                .checkpoints
                .iter()
                .any(|checkpoint| checkpoint.protects_history)
        {
            return Err("protected native checkpoint has no readable recovery summary");
        }
        let mut indexed = self.responses_replay.filter(
            self.project_indexed(checkpoint),
            self.responses_replay.scope(),
        );
        for (_, message) in &mut indexed {
            message
                .content
                .retain(|block| !matches!(block, ContentBlock::Compaction { .. }));
        }
        indexed.retain(|(_, message)| !message.content.is_empty());
        let protected = checkpoint.map(|checkpoint| {
            let body = checkpoint
                .summary
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (checkpoint.range.1 - 1, body)
        });
        Ok(TextCompactionView { indexed, protected })
    }

    fn raw_compaction_view(&self) -> Vec<(usize, Message)> {
        self.project_indexed(
            self.latest_checkpoint()
                .filter(|checkpoint| checkpoint.protects_history),
        )
    }

    fn project_indexed(&self, checkpoint: Option<&CompactionCheckpoint>) -> Vec<(usize, Message)> {
        let Some(checkpoint) = checkpoint else {
            return self.messages.iter().cloned().enumerate().collect();
        };
        let (start, end) = checkpoint.range;
        let mut view: Vec<_> = self.messages[..start].iter().cloned().enumerate().collect();
        view.extend(
            self.messages[start..end]
                .iter()
                .enumerate()
                .filter(|(_, message)| message.role == MessageRole::System)
                .map(|(index, message)| (start + index, message.clone())),
        );
        view.push((end - 1, checkpoint.summary.clone()));
        view.extend(
            self.messages[end..]
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, message)| (end + index, message)),
        );
        view
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
    /// サイズ制限・artifact 退避は返却時に適用済み。追加後の本文は書き換えず、
    /// モデル要求間の共通 prefix を保つ。履歴の縮小は圧縮チェックポイントで行う。
    pub fn push_tool_result(&mut self, tool_call_id: impl Into<String>, result: ToolResult) {
        self.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: tool_call_id.into(),
                content: std::iter::once(ToolResultContent::Text {
                    text: result.content,
                })
                .chain(
                    result
                        .images
                        .into_iter()
                        .map(|image| ToolResultContent::Image {
                            media_type: image.media_type,
                            data: image.data,
                            source: image.path.map(|path| path.display().to_string()),
                        }),
                )
                .collect(),
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
        let indexed = if let Some(checkpoint) = self.latest_checkpoint() {
            let (start, end) = checkpoint.range;
            let mut view: Vec<_> = self.messages[..start].iter().cloned().enumerate().collect();
            view.extend(
                self.messages[start..end]
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| m.role == MessageRole::System)
                    .map(|(i, m)| (start + i, m.clone())),
            );
            view.push((end - 1, checkpoint.summary.clone()));
            view.extend(
                self.messages[end..]
                    .iter()
                    .cloned()
                    .enumerate()
                    .map(|(i, m)| (end + i, m)),
            );
            view
        } else {
            self.messages.iter().cloned().enumerate().collect()
        };
        self.responses_replay
            .filter(indexed, self.responses_replay.scope())
            .into_iter()
            .map(|(_, m)| m)
            .collect()
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
            protects_history: false,
        }
    }

    #[test]
    fn ordinary_checkpoint_storage_bytes_stay_unchanged_and_protection_round_trips() {
        let original = r#"{"id":"checkpoint","summary":{"role":"user","content":[{"type":"text","text":"summary"}]},"range":[1,3]}"#;
        let mut checkpoint: CompactionCheckpoint = serde_json::from_str(original).unwrap();
        assert!(!checkpoint.protects_history);
        assert_eq!(serde_json::to_string(&checkpoint).unwrap(), original);
        checkpoint.protects_history = true;
        let saved = serde_json::to_string(&checkpoint).unwrap();
        assert!(saved.contains("\"protects_history\":true"));
        assert_eq!(
            serde_json::from_str::<CompactionCheckpoint>(&saved).unwrap(),
            checkpoint
        );
    }

    #[test]
    fn protected_compaction_view_survives_storage_and_preserves_system_instructions() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_system("system");
        context.push_user("goal");
        context.push_assistant(assistant_text("old answer"));
        context.push_system("injected instructions");
        context.push_user("latest user");
        let mut checkpoint = summary_checkpoint("boundary", (2, 4), "safe summary");
        checkpoint.protects_history = true;
        let checkpoint: CompactionCheckpoint =
            serde_json::from_str(&serde_json::to_string(&checkpoint).unwrap()).unwrap();
        context.apply_checkpoint(checkpoint);
        let view = context.compaction_view();
        assert_eq!(
            view.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            [0, 1, 3, 3, 4]
        );
        assert_eq!(
            view.into_iter()
                .map(|(_, message)| message)
                .collect::<Vec<_>>(),
            context.visible_messages()
        );
        let mut ordinary = context.latest_checkpoint().unwrap().clone();
        ordinary.protects_history = false;
        context.apply_checkpoint(ordinary);
        assert_eq!(
            context
                .compaction_view()
                .into_iter()
                .map(|(_, message)| message)
                .collect::<Vec<_>>(),
            context.messages
        );
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

    #[test]
    fn shell_gui_uuid_detail_never_enters_serialized_provider_messages() {
        let mut context = AgentContext::new(RunId::new(9), Role::Explorer);
        let uid = "3f9c2a1e-0000-4000-8000-000000000000";
        context.push_tool_result(
            "shell-start",
            ToolResult::success("shell job: job-42\n").with_detail(
                serde_json::json!({"shell_job": {"job_id": "job-42", "job_uid": uid}}),
            ),
        );
        let serialized = serde_json::to_string(&context.messages).unwrap();
        assert!(serialized.contains("job-42"));
        assert!(!serialized.contains(uid));
        assert!(!serialized.contains("job_uid"));
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

    // Instructions before, inside and after a restored checkpoint all remain
    // verbatim and ordered, including after a later compaction replaces it.
    #[test]
    fn restored_checkpoints_preserve_all_system_messages_without_resurrecting_history() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_system("最初の指示");
        context.push_user("目的");
        context.push_assistant(assistant_text("古い応答"));
        context.push_system("追加の workspace 指示");
        context.push_user("次の依頼");
        context.push_assistant(assistant_text("次の応答"));
        context.push_system("後続の指示");
        context.push_user("最新の依頼");
        let raw = context.messages.clone();
        let first = summary_checkpoint("ckpt-run-1-1", (2, 4), "最初の要約");
        let mut restored = AgentContext::from_restored(
            context.run_id,
            context.role,
            raw.clone(),
            vec![first.clone()],
            ResponsesReplayState::default(),
        );
        assert_eq!(
            restored.visible_messages(),
            vec![
                raw[0].clone(),
                raw[1].clone(),
                raw[3].clone(),
                first.summary,
                raw[4].clone(),
                raw[5].clone(),
                raw[6].clone(),
                raw[7].clone(),
            ],
        );
        let latest = summary_checkpoint("ckpt-run-1-2", (2, 7), "全体の要約");
        restored.apply_checkpoint(latest.clone());
        assert_eq!(
            restored.visible_messages(),
            vec![
                raw[0].clone(),
                raw[1].clone(),
                raw[3].clone(),
                raw[6].clone(),
                latest.summary,
                raw[7].clone()
            ],
        );
        assert_eq!(restored.messages, raw);
    }
}

#[cfg(test)]
pub(crate) mod responses_replay_tests {
    use super::*;

    pub(crate) fn scope(model: &str) -> providers::ResolvedResponsesScope {
        providers::ResolvedResponsesScope {
            dialect: providers::ResponsesDialect::Codex,
            profile: "codex".into(),
            route_model: model.into(),
            wire_model: model.into(),
            speed: providers::ResponsesSpeed::Standard,
            service_tier: None,
            endpoint_fingerprint: "endpoint".into(),
            account: providers::ResponsesAccount::Codex {
                identity: "account-a".into(),
            },
        }
    }
    pub(crate) fn opaque(id: &str, scope: providers::ResolvedResponsesScope) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ResponsesReasoning {
                item: serde_json::json!({"type":"reasoning", "id":id, "summary":[], "encrypted_content":format!("cipher-{id}")}),
                origin: scope,
            }],
        }
    }
    #[test]
    fn b3_unbound_history_never_replays_opaque_in_either_projection() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_user("goal");
        context.push_assistant(opaque("R1", scope("A")));
        assert_eq!(
            context.visible_messages().len(),
            1,
            "unbound opaque must be excluded from generation"
        );
        assert_eq!(
            context.compaction_view().len(),
            1,
            "unbound opaque must be excluded from official compaction"
        );
        assert_eq!(context.messages.len(), 2, "raw history is immutable");
    }
    #[test]
    fn b3_a_b_a_and_account_boundaries_never_revive_old_items() {
        let a = scope("A");
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.responses_replay.prepare(Some(a.clone()), 0);
        context.push_assistant(opaque("R1", a.clone()));
        assert_eq!(context.visible_messages().len(), 1);
        context
            .responses_replay
            .prepare(Some(scope("B")), context.messages.len());
        context
            .responses_replay
            .prepare(Some(a.clone()), context.messages.len());
        context.push_assistant(opaque("R2", a.clone()));
        assert_eq!(
            context.visible_messages(),
            vec![context.messages[1].clone()]
        );
        assert_eq!(
            context.compaction_view(),
            vec![(1, context.messages[1].clone())]
        );
        let stable = context.responses_replay.clone();
        // Tokens are deliberately absent from scope: same-account refresh is stable.
        context
            .responses_replay
            .prepare(Some(a.clone()), context.messages.len());
        assert_eq!(context.responses_replay, stable);
        let mut account_b = a.clone();
        account_b.account = providers::ResponsesAccount::Codex {
            identity: "account-b".into(),
        };
        context
            .responses_replay
            .prepare(Some(account_b), context.messages.len());
        context
            .responses_replay
            .prepare(Some(a), context.messages.len());
        assert!(context.visible_messages().is_empty());
        assert!(context.compaction_view().is_empty());
        assert_eq!(context.messages.len(), 2);
    }

    #[test]
    fn b3_saved_end_clamps_floor_without_changing_eligibility() {
        let a = scope("A");
        let messages: Vec<_> = (0..6)
            .map(|i| opaque(&format!("R{i}"), a.clone()))
            .collect();
        for floor in [1, 3, 5] {
            let replay = ResponsesReplayState {
                floor,
                binding: ResponsesReplayBinding::Bound { scope: a.clone() },
            };
            replay.validate(&messages, &[]).unwrap();
            let saved = replay.for_saved_end(3);
            assert_eq!(saved.floor, floor.min(3));
            for i in 0..3 {
                assert_eq!(i >= saved.floor, i >= floor);
            }
            assert_eq!(replay.floor, floor);
        }
        let mut child = ResponsesReplayState::reset(3);
        child.prepare(Some(a), 3);
        assert_eq!(child.floor, 3);
        assert!(
            child
                .filter(
                    messages[..3].iter().cloned().enumerate().collect(),
                    child.scope()
                )
                .is_empty()
        );
    }

    #[test]
    fn b3_restore_validation_checks_checkpoint_opaque_and_explicit_reset() {
        let messages = vec![Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "goal".into(),
            }],
        }];
        let checkpoint = CompactionCheckpoint {
            id: "checkpoint".into(),
            summary: opaque("R1", scope("A")),
            range: (0, 1),
            protects_history: true,
        };
        assert!(
            ResponsesReplayState::default()
                .validate(&messages, std::slice::from_ref(&checkpoint))
                .is_err()
        );
        ResponsesReplayState::default()
            .validate(&messages, &[])
            .unwrap();
        ResponsesReplayState::reset(1)
            .validate(&messages, &[checkpoint])
            .unwrap();
        assert!(
            ResponsesReplayState::reset(2)
                .validate(&messages, &[])
                .is_err()
        );
    }

    #[test]
    fn b3_native_protection_without_readable_source_never_falls_back_to_hidden_raw() {
        let mut context = AgentContext::new(RunId::new(1), Role::Worker);
        context.push_user("goal");
        context.push_assistant(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "hidden".into(),
                name: "edit".into(),
                input: serde_json::json!({"hidden": "never recover this raw batch"}),
            }],
        });
        context.apply_checkpoint(CompactionCheckpoint {
            id: "native-without-readable-source".into(),
            summary: Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Compaction {
                    encrypted_content: "native".into(),
                }],
            },
            range: (1, 2),
            protects_history: true,
        });
        assert!(context.text_compaction_view().is_err());
        assert_eq!(
            context.compaction_view()[1].1,
            context.latest_checkpoint().unwrap().summary
        );
        assert_eq!(context.messages.len(), 2);
    }
}
