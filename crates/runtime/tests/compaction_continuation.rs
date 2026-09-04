//! AC9 (issue #63 / T12): 長セッション継続の統合テスト。
//!
//! [`load_compaction_fixture`] の長セッションフィクスチャ (31 messages) を
//! 実ランターンで再生する。assistant ターン (tool call 含む) は fixture の
//! `Message` をそのまま ScriptedModel スクリプトへ供給し、goal と 2 件の
//! agent-message は親子 run 間の実配送経路 (`send_agent_message` / 停止中の
//! `send_message`) で run 履歴へ種付けする。tool 結果は実 ToolExecutor 経路
//! (Explorer の capability 拒否・存在しないパスの read 失敗) で生成されるため、
//! 履歴は fixture と同一の形状 (roles / tool pair / agent-message text) を保つ。
//!
//! Provenance: the fixture shape is derived from opencode (sst/opencode)
//! compaction summary+tail layout, senpi/pi-mono `CompactionEntry` +
//! `firstKeptEntryId` cut rules (a tail cut must not sever an open tool pair),
//! and the omo compress section model (Goal / Tasks / Decisions / Files /
//! Verification / Open items) — the same lineage recorded on the fixture loader.
//!
//! Provider 中立性: compaction は `providers::Message` 配列に対する純粋な再投影
//! (protected prefix + checkpoint summary + kept tail) であり、provider 固有の
//! compaction API は一切関与しない。本テストは観測した全 provider 要求が
//! canonical な Message 構造を保つこと (role 集合・ContentBlock variant 集合、
//! checkpoint が素の User + Text メッセージ) を検証する。

// allow: SIZE_OK — AC9 の 1 シナリオ (fixture 再生 → 自動圧縮 1 回 → 継続完了 →
// 可視窓検証) を単一の監査可能な統合テスト対象に収めるため (実測 約500 pure LOC)。
// シナリオ分割は T12 の 1 テスト要件を崩し、support への helper 移設は共有モジュール
// に AC9 専用の replay 機構を混入させて他ターゲットへ死にコードを生むため、
// compaction_triggers.rs と同様に単一対象で保つ判断とした。

mod support;

use std::sync::Arc;

use config::{CompactionConfig, SummarizerKind};
use event_bus::{
    AgentMessageKind, AgentRunPhase, CompactionEvent, CompactionReason, EventBus, EventKind,
    EventReceiver, LifecycleEvent,
};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolResultContent,
    Usage,
};
use runtime::{AgentModel, AgentRuntime, ExecutionPolicy, Role, RunConfig, RuntimeError};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use tools::{ToolError, ToolExecutor};

use support::{ScriptedModel, load_compaction_fixture, text_response};

const CHECKPOINT_PREFIX: &str = "[COMPACTION CHECKPOINT ";
const PARENT_MARKER: &str = "PARENT-ORCH-MARKER";
/// System メッセージ (compaction policy 合成テキスト) の文字数上限の見積り。
/// 実テキストは約 500 文字であり、推定アンカーの上限側にのみ使う。
const SYSTEM_ALLOWANCE_CHARS: usize = 700;
/// fixture の assistant tool ターン (スクリプト供給順)。
const SUMMARY_TURN_INDEX: usize = 26;

fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_use_of(message: &Message) -> (&str, &str) {
    message
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .expect("fixture assistant turn carries a tool use")
}

fn request_texts(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect()
}

fn tool_use_ids(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect()
}

fn tool_result_ids(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. } => None,
        })
        .collect()
}

fn checkpoint_ids(messages: &[Message]) -> Vec<&str> {
    request_texts(messages)
        .into_iter()
        .filter_map(|text| {
            text.strip_prefix(CHECKPOINT_PREFIX)
                .and_then(|tail| tail.split_once(']'))
                .map(|(id, _)| id)
        })
        .collect()
}

/// 実行時に agent-message として履歴へ入るテキスト (messages.rs の書式と同一)。
fn agent_message_text(id: &str, from: &str, kind: &str, body: &str) -> String {
    format!("[agent-message id={id} from={from} kind={kind}]\n{body}")
}

fn tool_result_message(id: &str, content: &str, is_error: bool) -> Message {
    Message {
        role: MessageRole::User,
        content: vec![ContentBlock::ToolResult {
            tool_call_id: id.to_string(),
            content: vec![ToolResultContent::Text {
                text: content.to_string(),
            }],
            is_error,
        }],
    }
}

/// fixture の tool 呼び出しが実ランターンで受ける結果本文。
///
/// - `edit` / `shell`: Explorer の capability boundary で拒否 (実行されない)。
/// - `read`: fixture パスはこの crate に存在しない → `PathNotFound`。
/// - `grep`: `src/` を実走査し `schedule_retry` の一致なし → 空の成功。
fn expected_tool_result(assistant: &Message) -> Message {
    let (id, name) = tool_use_of(assistant);
    let (content, is_error) = match name {
        "edit" | "shell" => (
            ExecutionPolicy::for_role(Role::Explorer)
                .authorize(name)
                .expect_err("Explorer must not authorize mutation tools")
                .to_string(),
            true,
        ),
        "read" => (
            ToolError::PathNotFound {
                path: "fixture path is absent in this crate".to_string(),
            }
            .to_string(),
            true,
        ),
        "grep" => (String::new(), false),
        other => panic!("unexpected fixture tool: {other}"),
    };
    tool_result_message(id, &content, is_error)
}

fn scripted_response(message: Message, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        message,
        usage: Usage::default(),
        finish_reason,
    }
}

/// 実行で積まれる既知メッセージ列 (System を除く、推定アンカー用)。
///
/// 順序: goal, T1..T3, agent1, T4..T9, agent2, T10, T11, A26, U27, T13。
/// T14 (fixture[30]) は read 引数が schema 拡張のため結果本文が確定しないので
/// アンカー対象外 (アンカーは T13 までで十分)。
fn replay_known_messages(fixture: &[Message]) -> Vec<Message> {
    let mut known = vec![fixture[1].clone()];
    let push_turn = |known: &mut Vec<Message>, index: usize| {
        known.push(fixture[index].clone());
        known.push(expected_tool_result(&fixture[index]));
    };
    for index in [2, 4, 6] {
        push_turn(&mut known, index);
    }
    known.push(Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: agent_message_text("msg-1", "run-1", "send", agent_body(&fixture[8])),
        }],
    });
    for index in [9, 11, 13, 15, 17, 19] {
        push_turn(&mut known, index);
    }
    known.push(Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: agent_message_text("msg-2", "run-1", "send", agent_body(&fixture[21])),
        }],
    });
    for index in [22, 24] {
        push_turn(&mut known, index);
    }
    known.push(fixture[SUMMARY_TURN_INDEX].clone());
    known.push(fixture[27].clone());
    push_turn(&mut known, 28);
    known
}

/// 境界 b10 (agent2 注入直後) と b13 (pause 直後) の推定から window を較正する。
///
/// 履歴の既知分は fixture と実 ToolExecutor の決定論的結果から復元でき、未知分は
/// System メッセージのみ。上限アンカー (System 700 文字扱い) < 0.75·window ≤
/// 下限アンカー (System 0 扱い) となる中心を採ることで、圧縮発火境界が
/// agent2 が履歴に沈んだ後・pause 前に収まることを保証する。
fn calibrated_window(fixture: &[Message]) -> u64 {
    fn estimate_tokens(messages: &[Message]) -> u64 {
        let chars = serde_json::to_string(messages)
            .expect("test messages serialize")
            .chars()
            .count() as u64;
        chars.div_ceil(4)
    }

    let known = replay_known_messages(fixture);
    // 境界 b10 = System + goal + T1..T9 + agent1 (= known[..20])
    let mut upper = vec![Message {
        role: MessageRole::System,
        content: vec![ContentBlock::Text {
            text: "x".repeat(SYSTEM_ALLOWANCE_CHARS),
        }],
    }];
    upper.extend(known[..20].iter().cloned());
    let upper_bound = estimate_tokens(&upper);
    // 境界 b13 = System + goal .. U27 (= known[..27])、System を 0 扱いにした下限
    let lower_bound = estimate_tokens(&known[..27]);

    assert!(
        lower_bound.saturating_sub(upper_bound) >= 50,
        "compaction anchor window collapsed: upper={upper_bound} lower={lower_bound}"
    );
    let midpoint = (upper_bound + lower_bound) / 2;
    ((midpoint as f64) / 0.75).ceil() as u64
}

fn agent_body(message: &Message) -> &str {
    message
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text { text } if text.starts_with("[agent-message ") => {
                text.split_once('\n').map(|(_, body)| body)
            }
            _ => None,
        })
        .expect("fixture agent message has a header line and a body")
}

async fn wait_for_phase(runtime: &AgentRuntime, run_id: runtime::RunId, phase: AgentRunPhase) {
    timeout(Duration::from_secs(10), async {
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

async fn collect_compactions(receiver: &mut EventReceiver, run_id: &str) -> Vec<CompactionEvent> {
    let mut compacted = Vec::new();
    timeout(Duration::from_secs(10), async {
        loop {
            let event = receiver.recv().await.expect("event bus remains open");
            match event.kind {
                EventKind::Compaction(event) => compacted.push(event),
                EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCompleted { task_id })
                    if task_id == run_id =>
                {
                    return;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("run completion event timeout");
    compacted
}

// Given: fixture 再生の実ランターン (Explorer 子 run + 親 run、小さめ window)
// When: assistant ターンを script で供給し、agent-message を実配送経路で 2 回注入、
//       対話待機点で fixture[27] をそのまま送って圧縮後も走らせ切る
// Then: 自動圧縮はちょうど 1 回、要約入力に fixture の AGENT_MESSAGE 本文が含まれ、
//       run は圧縮後も Done まで継続し、圧縮後の provider 要求は checkpoint +
//       kept tail のみ (raw 文は消え、tool pair は対で閉じたまま) になる
#[tokio::test]
async fn long_session_compacts_once_preserves_agent_messages_and_continues() {
    // Given: fixture 素材と小さめ window の compaction 設定
    let fixture = load_compaction_fixture();
    let goal = text_of(&fixture[1]);
    let body_send = agent_body(&fixture[8]).to_string();
    let body_reply = agent_body(&fixture[21]).to_string();
    let closeout_text = text_of(&fixture[27]);
    let agent1 = agent_message_text("msg-1", "run-1", "send", &body_send);
    let agent2 = agent_message_text("msg-2", "run-1", "send", &body_reply);
    let window = calibrated_window(&fixture);
    let settings = CompactionConfig {
        context_window_tokens: window,
        keep_recent_tokens: 100,
        cooldown_turns: 1000,
        max_summary_bytes: 8192,
        summarizer: SummarizerKind::Structural,
        ..CompactionConfig::default()
    };

    let tool_turn = |index: usize| {
        Ok(scripted_response(
            fixture[index].clone(),
            FinishReason::ToolUse,
        ))
    };
    let script: Vec<Result<ChatResponse, RuntimeError>> = vec![
        tool_turn(2),
        tool_turn(4),
        tool_turn(6),
        tool_turn(9),
        tool_turn(11),
        tool_turn(13),
        tool_turn(15),
        tool_turn(17),
        tool_turn(19),
        tool_turn(22),
        tool_turn(24),
        Ok(scripted_response(
            fixture[SUMMARY_TURN_INDEX].clone(),
            FinishReason::Stop,
        )),
        tool_turn(28),
        tool_turn(30),
        Ok(text_response("done", FinishReason::Stop)),
    ];
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(script, Arc::clone(&gate)));
    model
        .add_keyed(
            PARENT_MARKER,
            [Ok(text_response("parent standing by", FinishReason::Stop))],
        )
        .await;

    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime_model: Arc<dyn AgentModel> = model.clone();
    let runtime =
        AgentRuntime::new(Arc::clone(&bus), executor, runtime_model).with_compaction(settings);
    let mut receiver = bus.subscribe();

    let parent = runtime.delegate_background(
        Role::Explorer,
        PARENT_MARKER.to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(
            parent,
            Role::Explorer,
            goal.clone(),
            RunConfig {
                interactive: true,
                ..RunConfig::default()
            },
        )
        .expect("child delegation succeeds");

    // When: 各 provider 呼び出しを観測 → 検証 → (所定点で agent-message 注入) → 解放
    for step in 0..16usize {
        timeout(Duration::from_secs(10), async {
            while model.observed().await.len() <= step {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("provider call timeout");
        let request = &model.observed().await[step];
        match step {
            0 => {
                assert!(request_texts(request).contains(&PARENT_MARKER));
            }
            1 => {
                assert_eq!(request.len(), 2);
                assert!(
                    request_texts(request)
                        .iter()
                        .any(|text| text.contains("Fix the flaky compaction retry in scheduler"))
                );
            }
            3 => {
                // fixture[8] 相当の agent-message はまだ履歴に無い
                assert!(
                    !request_texts(request)
                        .iter()
                        .any(|text| text.contains(&body_send))
                );
                runtime
                    .send_agent_message(
                        parent,
                        child,
                        AgentMessageKind::Send,
                        body_send.clone(),
                        None,
                    )
                    .expect("parent relays the quota watchdog message");
            }
            4 => {
                // fixture[8] と同一位置 (T3 の結果の直後) に生の User text として入る
                assert!(request.iter().any(|message| message.content.iter().any(
                    |block| matches!(
                        block,
                        ContentBlock::Text { text } if *text == agent1
                    )
                )));
            }
            9 => {
                assert!(
                    !request_texts(request)
                        .iter()
                        .any(|text| text.contains(&body_reply))
                );
                runtime
                    .send_agent_message(
                        parent,
                        child,
                        AgentMessageKind::Send,
                        body_reply.clone(),
                        None,
                    )
                    .expect("parent relays the replay reply");
            }
            10 => {
                // fixture[21] と同一位置 (T9 の結果の直後) に生の User text として入る
                assert!(request.iter().any(|message| message.content.iter().any(
                    |block| matches!(
                        block,
                        ContentBlock::Text { text } if *text == agent2
                    )
                )));
            }
            _ => {}
        }
        gate.notify_one();
        if step == 12 {
            // fixture[26] の text-only Stop で対話待機になる → fixture[27] をそのまま送る
            wait_for_phase(&runtime, child, AgentRunPhase::Waiting).await;
            runtime
                .send_message(child, closeout_text.clone())
                .expect("waiting run accepts the closeout message");
        }
    }

    // Then: run は圧縮後も継続し Done に至る (長セッション継続)
    assert_eq!(
        timeout(Duration::from_secs(10), runtime.wait(child)).await,
        Ok(Ok(AgentRunPhase::Done))
    );
    let events = collect_compactions(&mut receiver, &child.to_string()).await;

    // (a) 自動圧縮はちょうど 1 回
    assert_eq!(events.len(), 1);
    let CompactionEvent::Compacted {
        run_id: event_run_id,
        reason,
        threshold,
        context_window_tokens,
        estimated_tokens_before,
        estimated_tokens_after,
        compacted_range_start,
        compacted_range_end,
        checkpoint_id,
        summary,
    } = &events[0];
    assert_eq!(event_run_id, &child.to_string());
    assert_eq!(*reason, CompactionReason::Automatic);
    assert!((*threshold - 0.75).abs() < f64::EPSILON);
    assert_eq!(*context_window_tokens, window);
    assert!(*estimated_tokens_before > *estimated_tokens_after);
    assert!(compacted_range_start < compacted_range_end);
    assert!(checkpoint_id.starts_with("ckpt-"));

    // 要約入力への fixture AGENT_MESSAGE 取り込みは Structural 要約の出力で検証する
    assert!(summary.contains("Fix the flaky compaction retry in scheduler"));
    assert!(
        summary.contains(&agent1),
        "summary must carry the relayed send body"
    );

    // (c) 圧縮後の provider 要求 = checkpoint + kept tail のみ
    let observed = model.observed().await;
    assert_eq!(observed.len(), 16, "parent + 15 child provider calls");
    let child_requests = &observed[1..];
    let first_compacted = child_requests
        .iter()
        .position(|request| !checkpoint_ids(request).is_empty())
        .expect("a compacted provider request exists")
        + 1;

    // B4a の完全ターン境界ルールでは relayed reply が compacted 域外(tail)に verbatim で残りうる。
    // tail 生存は要約内保持より強い保存形態なので「summary 内 or 圧縮後全要求の tail 内」の OR を検証する。
    let agent2_in_summary = summary.contains(&agent2);
    let agent2_in_tail = child_requests[first_compacted - 1..]
        .iter()
        .all(|request| request.iter().any(|message| text_of(message).contains(&agent2)));
    assert!(
        agent2_in_summary || agent2_in_tail,
        "relayed reply body must survive compaction (summary or kept tail)"
    );
    assert!(summary.contains("Decision: use exponential backoff with jitter"));
    assert!(summary.to_ascii_lowercase().contains("unresolved"));
    println!(
        "{}",
        serde_json::to_string_pretty(&events[0]).expect("compaction event serializes")
    );

    assert!(
        (11..=13).contains(&first_compacted),
        "compaction must fire mid-session, after both relayed agent messages sank into history (got {first_compacted})"
    );
    for (position, request) in child_requests.iter().enumerate() {
        if position + 1 < first_compacted {
            assert!(checkpoint_ids(request).is_empty());
        } else {
            assert_eq!(checkpoint_ids(request), vec![checkpoint_id.as_str()]);
        }
    }
    for request in &child_requests[first_compacted - 1..] {
        // raw compacted 文はどこにも残らない
        assert!(!request_texts(request).iter().any(|text| {
            text.contains("reproduce the flake with a stress run before touching anything")
        }));
        assert!(
            !request_texts(request)
                .iter()
                .any(|text| text.contains("The scheduler rearms"))
        );
        // B4a: goal は保護済み raw User(verbatim) + checkpoint 要約の 2 メッセージで保持される
        let goal_bearers = request
            .iter()
            .filter(|message| text_of(message).contains(goal.as_str()))
            .count();
        assert_eq!(goal_bearers, 2);
        assert!(
            request
                .iter()
                .any(|message| text_of(message) == goal.as_str())
        );
    }
    // kept tail を含む全要求で ToolUse / ToolResult が対で閉じる (尻切れなし)
    for request in child_requests {
        let mut uses = tool_use_ids(request);
        let mut results = tool_result_ids(request);
        uses.sort_unstable();
        results.sort_unstable();
        assert_eq!(uses, results);
    }
    // fixture[27] (byte-identical) は最終要求に 1 メッセージとして残る
    let last = child_requests.last().expect("child made provider calls");
    assert_eq!(
        last.iter()
            .filter(|message| text_of(message) == closeout_text)
            .count(),
        1
    );

    // (e) provider 中立性: canonical Message 構造のままの再投影であること
    for request in child_requests {
        assert!(matches!(request[0].role, MessageRole::System));
        for message in request {
            assert!(matches!(
                message.role,
                MessageRole::System | MessageRole::User | MessageRole::Assistant
            ));
            for block in &message.content {
                match block {
                    ContentBlock::Text { .. }
                    | ContentBlock::Reasoning { .. }
                    | ContentBlock::ToolUse { .. }
                    | ContentBlock::ToolResult { .. } => {}
                }
            }
        }
    }
    let checkpoint_message = last
        .iter()
        .find(|message| text_of(message).starts_with(CHECKPOINT_PREFIX))
        .expect("final request carries the checkpoint");
    assert!(matches!(checkpoint_message.role, MessageRole::User));
    assert!(matches!(
        checkpoint_message.content.as_slice(),
        [ContentBlock::Text { .. }]
    ));
    assert_eq!(
        runtime
            .inspect_agent(child)
            .expect("run remains inspectable")
            .phase,
        AgentRunPhase::Done
    );
}
