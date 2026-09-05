use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, Event, EventBus,
    EventKind, OrchestratorEvent, ProviderEvent,
};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use tokio::sync::Notify;

const DEMO_GOAL_KEY: &str = "DEMO-GOAL";
const DEMO_IMPL_KEY: &str = "DEMO-IMPL";
const REVIEW_KEY: &str = "[evorch review";
const REPAIR_KEY: &str = "[evorch repair";
const CONTINUATION_KEY: &str = "[evorch continuation";
const WORKTREE_PLACEHOLDER: &str = "{worktree}";

/// goal 登録と worker の RunAttached 発行が root run の finish 評価へ確実に
/// 先行するよう、DEMO-GOAL root の各ターンへ入れる固定遅延。
const ROOT_TURN_DELAY: Duration = Duration::from_millis(50);
/// bus イベント待ちゲート (worker 初回応答・root 最終ターン) の上限。
const GATE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct DemoScriptModel {
    bus: Arc<EventBus>,
    scripts: Mutex<HashMap<String, VecDeque<ChatResponse>>>,
    workspace_root: Option<PathBuf>,
    gated_workers: Mutex<HashSet<String>>,
    worker_message_sent: Notify,
    reviewer_message_sent: Notify,
    worker_reply_sent: Notify,
    reviewer_reply_sent: Notify,
    children_joined: AtomicBool,
}

impl DemoScriptModel {
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self {
            bus,
            scripts: Mutex::new(HashMap::from([
                (
                    "DEMO-ORCH".to_string(),
                    VecDeque::from([
                        tool_response(
                            "demo-delegate-w1",
                            "delegate_background",
                            serde_json::json!({
                                "role": "worker",
                                "prompt": "DEMO-W1",
                                "name": "worker-w1"
                            }),
                        ),
                        tool_response(
                            "demo-delegate-r1",
                            "delegate_background",
                            serde_json::json!({
                                "role": "reviewer",
                                "prompt": "DEMO-R1",
                                "name": "reviewer-r1"
                            }),
                        ),
                        tool_response(
                            "demo-message-w1",
                            "send_message",
                            serde_json::json!({
                                "run_id": "run-2",
                                "message": "implement the goal"
                            }),
                        ),
                        tool_response(
                            "demo-message-r1",
                            "send_message",
                            serde_json::json!({
                                "run_id": "run-3",
                                "message": "review run-2"
                            }),
                        ),
                        text_response("demo complete"),
                    ]),
                ),
                (
                    "DEMO-W1".to_string(),
                    VecDeque::from([
                        tool_response(
                            "demo-worker-done",
                            "send",
                            serde_json::json!({
                                "run_id": "run-1",
                                "message": "worker done"
                            }),
                        ),
                        text_response("worker done"),
                    ]),
                ),
                (
                    "DEMO-R1".to_string(),
                    VecDeque::from([
                        tool_response(
                            "demo-review-lgtm",
                            "send",
                            serde_json::json!({
                                "run_id": "run-1",
                                "message": "LGTM"
                            }),
                        ),
                        text_response("review done"),
                    ]),
                ),
                // goal loop demo (issue #73 T3.2): supervisor の prompts.rs が
                // 生成する header 行へ部分一致で key 解決される script 群。
                (
                    DEMO_GOAL_KEY.to_string(),
                    VecDeque::from([
                        tool_response(
                            "demo-goal-delegate",
                            "delegate_background",
                            serde_json::json!({
                                "role": "worker",
                                "prompt": "DEMO-IMPL implement the fixture unit",
                                "workspace_mode": "isolated"
                            }),
                        ),
                        // 配信前の早期 finish: gate が no_pull_request で拒否する
                        // 様子を見せる。run は finish せず終端へ進む。
                        tool_response(
                            "demo-goal-early-finish",
                            "finish",
                            serde_json::json!({"result": "demo goal delivered"}),
                        ),
                        text_response("root run ends without finish; the pipeline continues"),
                    ]),
                ),
                (
                    DEMO_IMPL_KEY.to_string(),
                    VecDeque::from([
                        tool_response(
                            "demo-impl-commit",
                            "shell",
                            serde_json::json!({
                                "command": "git",
                                "args": ["-C", WORKTREE_PLACEHOLDER, "commit", "--allow-empty", "-m", "demo"]
                            }),
                        ),
                        text_response("implemented the fixture unit"),
                    ]),
                ),
                (
                    REVIEW_KEY.to_string(),
                    VecDeque::from([
                        text_response(
                            "```json\n{\"verdict\":\"request-update\",\"findings\":[\"demo finding: apply the fixture commit\"],\"criteria\":[{\"id\":\"ac-1\",\"status\":\"unmet\",\"note\":\"commit missing\"}]}\n```",
                        ),
                        text_response(
                            "```json\n{\"verdict\":\"approve\",\"findings\":[],\"criteria\":[{\"id\":\"ac-1\",\"status\":\"met\",\"note\":\"ok\"}]}\n```",
                        ),
                    ]),
                ),
                (
                    REPAIR_KEY.to_string(),
                    VecDeque::from([
                        tool_response(
                            "demo-repair-commit",
                            "shell",
                            serde_json::json!({
                                "command": "git",
                                "args": ["-C", WORKTREE_PLACEHOLDER, "commit", "--allow-empty", "-m", "demo repair"]
                            }),
                        ),
                        text_response("repaired the fixture unit"),
                    ]),
                ),
                (
                    CONTINUATION_KEY.to_string(),
                    VecDeque::from([
                        tool_response(
                            "demo-continuation-finish",
                            "finish",
                            serde_json::json!({"result": "demo goal delivered"}),
                        ),
                        // merge 却下後の再 continuation 用 (happy path では未使用)。
                        tool_response(
                            "demo-continuation-finish-again",
                            "finish",
                            serde_json::json!({"result": "demo goal delivered after rejection"}),
                        ),
                    ]),
                ),
            ])),
            workspace_root: None,
            gated_workers: Mutex::new(HashSet::new()),
            worker_message_sent: Notify::new(),
            reviewer_message_sent: Notify::new(),
            worker_reply_sent: Notify::new(),
            reviewer_reply_sent: Notify::new(),
            children_joined: AtomicBool::new(false),
        }
    }

    /// worker 系 script の `{worktree}` placeholder 展開に使う demo project の
    /// repo root を設定する (`<root>/.evorch/worktrees/<run_id>` を導出する)。
    pub fn with_workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(root.into());
        self
    }

    fn scripted_response(&self, marker: &str) -> Result<(u64, ChatResponse), RuntimeError> {
        let mut scripts = self.scripts.lock().map_err(|_| RuntimeError::Model {
            reason: "demo script lock was poisoned".to_string(),
        })?;
        let script = scripts.get_mut(marker).ok_or_else(|| RuntimeError::Model {
            reason: format!("unknown demo script marker {marker}"),
        })?;
        let response = script
            .pop_front()
            .or_else(|| (marker == "DEMO-ORCH").then(|| text_response("demo complete")));
        let response = response.ok_or_else(|| RuntimeError::Model {
            reason: format!("demo script exhausted for {marker}"),
        })?;
        Ok((script.len() as u64, response))
    }

    /// この run の初回呼び出しなら true を返し、以後の呼び出しでは false。
    fn mark_worker_gate(&self, run_id: &str) -> bool {
        self.gated_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run_id.to_string())
    }

    /// 指定 script の残り応答数を返す。
    fn remaining_turns(&self, marker: &str) -> usize {
        self.scripts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(marker)
            .map_or(0, VecDeque::len)
    }

    /// bus 上で predicate を満たすイベントを GATE_TIMEOUT まで待つ。
    ///
    /// timeout 超過や bus 終了時はゲートなしで続行する (決定的順序は失われるが
    /// demo が hang することはない)。
    async fn wait_for_event(&self, predicate: fn(&Event) -> bool) {
        let mut receiver = self.bus.subscribe();
        let _ = tokio::time::timeout(GATE_TIMEOUT, async {
            loop {
                match receiver.recv().await {
                    Ok(event) if predicate(&event) => return,
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        })
        .await;
    }

    /// worker 系 script の `{worktree}` placeholder を run の isolated worktree
    /// path (`<root>/.evorch/worktrees/<run_id>`) で置き換える。
    ///
    /// shell tool は cwd を注入しないため、placeholder 未設定のまま `git commit`
    /// を実行すると呼び出しプロセスの cwd 配下のリポジトリを汚染しうる。
    /// workspace root 未設定で worker script に到達した場合は fail-closed で
    /// エラーにする。
    fn rewrite_worktree(
        &self,
        marker: &str,
        run_id: &str,
        mut response: ChatResponse,
    ) -> Result<ChatResponse, RuntimeError> {
        if !matches!(marker, DEMO_IMPL_KEY | REPAIR_KEY) {
            return Ok(response);
        }
        let uses_placeholder = response.message.content.iter().any(|block| {
            matches!(block, ContentBlock::ToolUse { input, .. } if input.to_string().contains(WORKTREE_PLACEHOLDER))
        });
        if !uses_placeholder {
            return Ok(response);
        }
        let Some(root) = &self.workspace_root else {
            return Err(RuntimeError::Model {
                reason: "demo worker shell script requires with_workspace_root".to_string(),
            });
        };
        let worktree = root
            .join(".evorch")
            .join("worktrees")
            .join(run_id)
            .to_string_lossy()
            .into_owned();
        for block in &mut response.message.content {
            if let ContentBlock::ToolUse { input, .. } = block {
                *input = substitute_placeholder(std::mem::take(input), &worktree);
            }
        }
        Ok(response)
    }
}

#[async_trait]
impl AgentModel for DemoScriptModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let marker = script_key(initial_marker(messages)?);
        if marker == DEMO_GOAL_KEY {
            tokio::time::sleep(ROOT_TURN_DELAY).await;
            if self.remaining_turns(marker) == 1 {
                // 最終ターン (finish せず終わる応答) は review round 1 の開始まで
                // 遅らせる。supervisor の pipeline busy は Review/Repair run のみを
                // 数えるため、root が Implement worker のみの間に終端すると
                // continuation が即座に dispatch されてしまう。reviewer 稼働後に
                // 終端すれば dispatch は ReadyToFinish まで deferred される。
                self.wait_for_event(is_review_round_started).await;
            }
        }
        if marker == DEMO_IMPL_KEY && self.mark_worker_gate(&invocation.run_id) {
            // 初回応答は root の早期 finish 拒否 (FinishRejected) まで遅らせ、
            // branch 束縛が拒否イベントへ後追いする bus 順序を固定する。
            self.wait_for_event(is_finish_rejected).await;
        }
        if marker == "DEMO-ORCH"
            && messages.iter().any(is_demo_review_result)
            && !self.children_joined.swap(true, Ordering::AcqRel)
        {
            self.worker_reply_sent.notified().await;
            self.reviewer_reply_sent.notified().await;
        }
        let (remaining_turns, response) = self.scripted_response(marker)?;
        let response = self.rewrite_worktree(marker, &invocation.run_id, response)?;
        let turn = match marker {
            "DEMO-ORCH" => 5 - remaining_turns,
            "DEMO-W1" | "DEMO-R1" => 2 - remaining_turns,
            _ => 1,
        };
        let model = self.selected_model(role);
        let request_id = format!("demo-{}-{turn}", invocation.run_id);
        self.bus.emit(Event::new(ProviderEvent::RequestStarted {
            request_id: request_id.clone(),
            provider: "demo".to_string(),
            profile: None,
            protocol: "scripted".to_string(),
            model: model.clone(),
            streaming: false,
            run_id: Some(invocation.run_id.clone()),
        }));
        match (marker, remaining_turns) {
            ("DEMO-ORCH", 2) => self.worker_message_sent.notify_one(),
            ("DEMO-ORCH", 1) => {
                self.reviewer_message_sent.notify_one();
            }
            ("DEMO-W1", 1) => {
                self.worker_message_sent.notified().await;
            }
            ("DEMO-W1", 0) => self.worker_reply_sent.notify_one(),
            ("DEMO-R1", 1) => {
                self.reviewer_message_sent.notified().await;
            }
            ("DEMO-R1", 0) => {
                self.bus.emit(Event::new(AgentMessageEvent::Delivered {
                    message: AgentMessage {
                        message_id: "demo-review-message".to_string(),
                        sender_run_id: invocation.run_id.clone(),
                        recipient_run_id: "run-1".to_string(),
                        kind: AgentMessageKind::Send,
                        content: "LGTM".to_string(),
                        reply_to: None,
                    },
                    disposition: DeliveryDisposition::Aside,
                }));
                self.reviewer_reply_sent.notify_one();
            }
            _ => {}
        }
        self.bus.emit(Event::new(ProviderEvent::RequestCompleted {
            request_id,
            provider: "demo".to_string(),
            profile: None,
            protocol: "scripted".to_string(),
            model,
            streaming: false,
            duration_ms: 0,
            input_tokens: 120 * turn,
            output_tokens: 40 * turn,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            finish_reason: finish_reason_name(&response.finish_reason).to_string(),
            run_id: Some(invocation.run_id.clone()),
        }));
        Ok(response)
    }

    fn selected_model(&self, role: Role) -> String {
        format!("demo-{}", role.name().to_lowercase())
    }
}

fn is_demo_review_result(message: &Message) -> bool {
    message.content.iter().any(|block| {
        matches!(
            block,
            ContentBlock::ToolResult { tool_call_id, .. }
                if tool_call_id == "demo-message-r1"
        )
    })
}

fn initial_marker(messages: &[Message]) -> Result<&str, RuntimeError> {
    messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .and_then(|message| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Reasoning { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
        })
        .ok_or_else(|| RuntimeError::Model {
            reason: "demo run did not contain an initial prompt".to_string(),
        })
}

/// 最初の user プロンプトを script key へ解決する。
///
/// 従来の 3 script は全文一致、goal loop 用の script は先頭行の部分一致で
/// 解決する (supervisor 生成プロンプトは goal_id 等の動的要素を含むため)。
/// 未知のプロンプトは全文をそのまま返し、従来どおり unknown marker エラーに
/// なる。
fn script_key(prompt: &str) -> &str {
    if matches!(prompt, "DEMO-ORCH" | "DEMO-W1" | "DEMO-R1") {
        return prompt;
    }
    let first_line = prompt.lines().next().unwrap_or(prompt);
    for key in [
        CONTINUATION_KEY,
        REVIEW_KEY,
        REPAIR_KEY,
        DEMO_IMPL_KEY,
        DEMO_GOAL_KEY,
    ] {
        if first_line.contains(key) {
            return key;
        }
    }
    prompt
}

fn is_review_round_started(event: &Event) -> bool {
    matches!(
        &event.kind,
        EventKind::Orchestrator(OrchestratorEvent::ReviewRoundStarted { .. })
    )
}

fn is_finish_rejected(event: &Event) -> bool {
    matches!(
        &event.kind,
        EventKind::Orchestrator(OrchestratorEvent::FinishRejected { .. })
    )
}

fn substitute_placeholder(value: serde_json::Value, worktree: &str) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            serde_json::Value::String(text.replace(WORKTREE_PLACEHOLDER, worktree))
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .into_iter()
                .map(|item| substitute_placeholder(item, worktree))
                .collect(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, item)| (key, substitute_placeholder(item, worktree)))
                .collect(),
        ),
        other => other,
    }
}

fn text_response(text: &str) -> ChatResponse {
    response(
        vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        FinishReason::Stop,
    )
}

fn tool_response(id: &str, name: &str, input: serde_json::Value) -> ChatResponse {
    response(
        vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }],
        FinishReason::ToolUse,
    )
}

fn response(content: Vec<ContentBlock>, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: MessageRole::Assistant,
            content,
        },
        usage: Usage::default(),
        finish_reason,
    }
}

const fn finish_reason_name(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolUse => "tool_use",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::Other(_) => "other",
    }
}
