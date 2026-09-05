//! T7 (issue #75): エージェントツールループと停滞検出器の結合テスト。
//!
//! 検出器の提案は観測専用であり、`EscalationProposed` ライフサイクルイベントの
//! 発行以外に副作用を持たないこと (モデル履歴への注入・自動昇格なし) を
//! run レベルで検証する。

mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{EscalationTrigger, Event, EventBus, EventKind, LifecycleEvent};
use providers::FinishReason;
use runtime::{AgentRunPhase, AgentRuntime, EscalationSettings, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, drain_events, text_response, tool_response};

/// 検証を短くするための低いしきい値 (連続編集失敗 2 / 同一ファイル書き換え 2 / ツール呼び出し 3)。
fn settings() -> EscalationSettings {
    EscalationSettings {
        consecutive_edit_failures: 2,
        same_file_rewrites: 2,
        tool_call_threshold: 3,
    }
}

fn runtime_with(
    model: Arc<ScriptedModel>,
    escalation: Option<EscalationSettings>,
) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model);
    let runtime = match escalation {
        Some(settings) => runtime.with_escalation_settings(settings),
        None => runtime,
    };
    (runtime, bus)
}

fn proposals(events: &[Event]) -> Vec<(String, EscalationTrigger)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::EscalationProposed { run_id, trigger }) => {
                Some((run_id.clone(), trigger.clone()))
            }
            _ => None,
        })
        .collect()
}

async fn wait_until_done(runtime: &AgentRuntime, run_id: runtime::RunId) {
    assert_eq!(
        timeout(Duration::from_secs(5), runtime.wait(run_id))
            .await
            .expect("run 完了タイムアウト"),
        Ok(AgentRunPhase::Done)
    );
}

// Given: しきい値 2/2/3 と存在しないパスへの edit を 2 回要求する Worker run
// When: run を自然 Stop まで実行する
// Then: 2 回目の失敗で ConsecutiveEditFailures{count:2} の提案が 1 回だけ発行される
#[tokio::test]
async fn consecutive_edit_failures_propose_once_at_threshold() {
    let directory = tempfile::tempdir().expect("temp directory");
    let missing = directory.path().join("absent.txt");
    let (runtime, bus) = runtime_with(
        Arc::new(ScriptedModel::new([
            Ok(tool_response(
                "edit-1",
                "edit",
                json!({ "path": missing, "old_string": "x", "new_string": "y" }),
            )),
            Ok(tool_response(
                "edit-2",
                "edit",
                json!({ "path": missing, "old_string": "x", "new_string": "y" }),
            )),
            Ok(text_response("done", FinishReason::Stop)),
        ])),
        Some(settings()),
    );
    let mut receiver = bus.subscribe();

    let run_id =
        runtime.delegate_background(Role::Worker, "ESCALATE".to_string(), RunConfig::default());
    wait_until_done(&runtime, run_id).await;

    let events = drain_events(&mut receiver).await;
    let proposals = proposals(&events);
    assert_eq!(proposals.len(), 1, "提案は 1 回だけ: {events:?}");
    assert_eq!(proposals[0].0, run_id.to_string());
    assert_eq!(
        proposals[0].1,
        EscalationTrigger::ConsecutiveEditFailures { count: 2 }
    );
}

// Given: しきい値 2/2/3 と同一の実在ファイルへの成功 edit を 2 回要求する Worker run
// When: run を自然 Stop まで実行する
// Then: 2 回目の書き込みで RepeatedFileRewrite{path, count:2} の提案が発行される
#[tokio::test]
async fn repeated_same_file_rewrites_propose_with_path() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("worker.txt");
    let (runtime, bus) = runtime_with(
        Arc::new(ScriptedModel::new([
            Ok(tool_response(
                "edit-1",
                "edit",
                json!({ "path": path, "new_string": "one" }),
            )),
            Ok(tool_response(
                "edit-2",
                "edit",
                json!({ "path": path, "old_string": "one", "new_string": "two" }),
            )),
            Ok(text_response("done", FinishReason::Stop)),
        ])),
        Some(settings()),
    );
    let mut receiver = bus.subscribe();

    let run_id =
        runtime.delegate_background(Role::Worker, "REWRITE".to_string(), RunConfig::default());
    wait_until_done(&runtime, run_id).await;

    let events = drain_events(&mut receiver).await;
    let proposals = proposals(&events);
    assert_eq!(proposals.len(), 1, "提案は 1 回だけ: {events:?}");
    assert_eq!(
        proposals[0].1,
        EscalationTrigger::RepeatedFileRewrite {
            path: path.to_string_lossy().into_owned(),
            count: 2,
        }
    );
}

// Given: しきい値 2/2/3 と実在ファイルへの read を 3 回要求する Worker run
// When: run を自然 Stop まで実行する
// Then: 3 回目の呼び出しで ToolCallThreshold{count:3} の提案が発行される
#[tokio::test]
async fn tool_call_threshold_proposes_at_three_reads() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("notes.txt");
    std::fs::write(&path, "content").expect("fixture ファイルを書ける");
    let (runtime, bus) = runtime_with(
        Arc::new(ScriptedModel::new([
            Ok(tool_response("read-1", "read", json!({ "path": path }))),
            Ok(tool_response("read-2", "read", json!({ "path": path }))),
            Ok(tool_response("read-3", "read", json!({ "path": path }))),
            Ok(text_response("done", FinishReason::Stop)),
        ])),
        Some(settings()),
    );
    let mut receiver = bus.subscribe();

    let run_id =
        runtime.delegate_background(Role::Worker, "READS".to_string(), RunConfig::default());
    wait_until_done(&runtime, run_id).await;

    let events = drain_events(&mut receiver).await;
    let proposals = proposals(&events);
    assert_eq!(proposals.len(), 1, "提案は 1 回だけ: {events:?}");
    assert_eq!(
        proposals[0].1,
        EscalationTrigger::ToolCallThreshold { count: 3 }
    );
}

// Given: 既定しきい値 (3/5/200) と read 1 回 + 異なるファイルへの成功 edit 1 回
// When: run を自然 Stop まで実行する
// Then: EscalationProposed は 1 件も発行されない
#[tokio::test]
async fn healthy_sequence_with_default_settings_does_not_propose() {
    let directory = tempfile::tempdir().expect("temp directory");
    let source = directory.path().join("source.txt");
    let target = directory.path().join("target.txt");
    std::fs::write(&source, "content").expect("fixture ファイルを書ける");
    let (runtime, bus) = runtime_with(
        Arc::new(ScriptedModel::new([
            Ok(tool_response("read-1", "read", json!({ "path": source }))),
            Ok(tool_response(
                "edit-1",
                "edit",
                json!({ "path": target, "new_string": "written" }),
            )),
            Ok(text_response("done", FinishReason::Stop)),
        ])),
        None,
    );
    let mut receiver = bus.subscribe();

    let run_id =
        runtime.delegate_background(Role::Worker, "HEALTHY".to_string(), RunConfig::default());
    wait_until_done(&runtime, run_id).await;

    let events = drain_events(&mut receiver).await;
    assert!(
        proposals(&events).is_empty(),
        "既定設定では提案しない: {events:?}"
    );
}

// Given: 提案が発火する脚本 (存在しないパスへの edit 2 回) としきい値 2/2/3
// When: run を自然 Stop まで実行する
// Then: 提案は観測のみで、EscalationRequested は発行されず新規 run も作られない
#[tokio::test]
async fn proposal_is_observation_only_and_run_continues_to_done() {
    let directory = tempfile::tempdir().expect("temp directory");
    let missing = directory.path().join("absent.txt");
    let (runtime, bus) = runtime_with(
        Arc::new(ScriptedModel::new([
            Ok(tool_response(
                "edit-1",
                "edit",
                json!({ "path": missing, "old_string": "x", "new_string": "y" }),
            )),
            Ok(tool_response(
                "edit-2",
                "edit",
                json!({ "path": missing, "old_string": "x", "new_string": "y" }),
            )),
            Ok(text_response("done", FinishReason::Stop)),
        ])),
        Some(settings()),
    );
    let mut receiver = bus.subscribe();

    let run_id =
        runtime.delegate_background(Role::Worker, "OBSERVE".to_string(), RunConfig::default());
    wait_until_done(&runtime, run_id).await;

    let events = drain_events(&mut receiver).await;
    assert_eq!(
        proposals(&events).len(),
        1,
        "前提として提案が 1 回発火する: {events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Lifecycle(LifecycleEvent::EscalationRequested { .. })
        )),
        "観測専用の提案から自動昇格してはならない: {events:?}"
    );
    assert_eq!(runtime.list_agents().len(), 1, "新規 run は作られない");
    assert_eq!(runtime.escalation_memo(run_id), None);
}

// Given: 失敗 edit 2 回 (提案発火) の後に read 1 回 (ツール数しきい値到達) を含む脚本
// When: run を自然 Stop まで実行する
// Then: ラッチにより提案は最初の 1 件のみで、合計は 1 件のままである
#[tokio::test]
async fn latch_emits_only_the_first_proposal() {
    let directory = tempfile::tempdir().expect("temp directory");
    let missing = directory.path().join("absent.txt");
    let notes = directory.path().join("notes.txt");
    std::fs::write(&notes, "content").expect("fixture ファイルを書ける");
    let (runtime, bus) = runtime_with(
        Arc::new(ScriptedModel::new([
            Ok(tool_response(
                "edit-1",
                "edit",
                json!({ "path": missing, "old_string": "x", "new_string": "y" }),
            )),
            Ok(tool_response(
                "edit-2",
                "edit",
                json!({ "path": missing, "old_string": "x", "new_string": "y" }),
            )),
            Ok(tool_response("read-1", "read", json!({ "path": notes }))),
            Ok(text_response("done", FinishReason::Stop)),
        ])),
        Some(settings()),
    );
    let mut receiver = bus.subscribe();

    let run_id =
        runtime.delegate_background(Role::Worker, "LATCH".to_string(), RunConfig::default());
    wait_until_done(&runtime, run_id).await;

    let events = drain_events(&mut receiver).await;
    let proposals = proposals(&events);
    assert_eq!(proposals.len(), 1, "提案は最初の 1 件のみ: {events:?}");
    assert_eq!(
        proposals[0].1,
        EscalationTrigger::ConsecutiveEditFailures { count: 2 }
    );
}
