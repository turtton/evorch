//! T7: システムプロンプトシームの結合テスト (issue #49)。
//!
//! agent_loop がカタログから組み立てた System メッセージを run 開始時に履歴へ
//! 挿入すること、カタログなしで v0.1 の履歴を保つこと、委譲時に category が
//! 子 run へ流れることを ScriptedModel の観測結果で検証する。

mod support;

use std::sync::Arc;

use agents::Role;
use config::CompactionConfig;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole, ToolResultContent};
use runtime::prompt::SystemPromptCatalogBuilder;
use runtime::{AgentRuntime, RunConfig, RunId, SystemPromptCatalog};
use sandbox::DirectSandbox;
use serde_json::json;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_response};

const GATE_MARK: &str = "## Intent Gate";
const QUICK_OVERLAY_MARK: &str = "QUICK-OVERLAY-MARK";

fn catalog_builder() -> SystemPromptCatalogBuilder {
    let mut builder = SystemPromptCatalog::builder();
    for role in [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
    ] {
        builder = builder.role_baseline(role, format!("{}-BASELINE", role.name().to_uppercase()));
    }
    for family in [
        "claude",
        "openai-reasoning",
        "gpt5",
        "gemini",
        "kimi",
        "generic",
    ] {
        builder = builder.family_section(format!("family-{family}"), family.to_uppercase());
    }
    builder
}

fn complete_catalog() -> SystemPromptCatalog {
    catalog_builder()
        .category_overlay(
            "quick",
            format!("{QUICK_OVERLAY_MARK} quick カテゴリの上書き指示"),
        )
        .build()
        .expect("テスト用カタログは必須部品をすべて満たす")
}

/// 対話モードと category だけを指定した RunConfig を作る。
fn cfg(interactive: bool, category: Option<&str>) -> RunConfig {
    RunConfig {
        interactive,
        category: category.map(str::to_string),
        ..RunConfig::default()
    }
}

fn runtime_with(
    model: Arc<ScriptedModel>,
    catalog: Option<SystemPromptCatalog>,
) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let mut runtime = AgentRuntime::new(Arc::clone(&bus), executor, model);
    if let Some(catalog) = catalog {
        runtime = runtime.with_system_prompts(Arc::new(catalog));
    }
    (runtime, bus)
}

/// observed の 1 complete 呼び出しから、指定ロールの最初の Text ブロック本文を返す。
fn text_of_role(messages: &[Message], role: MessageRole) -> Option<&str> {
    messages
        .iter()
        .find(|message| message.role == role)
        .and_then(|message| match &message.content[0] {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
}

/// Orchestrator が category 付きで Worker を委譲し closing で終わるスクリプトを登録する。
async fn orchestrator_delegate_script(model: &ScriptedModel, category: &str, closing: &str) {
    let script = [
        Ok(tool_response(
            "delegate-worker",
            "delegate_background",
            json!({ "role": "worker", "prompt": "W1", "category": category }),
        )),
        Ok(text_response(closing, FinishReason::Stop)),
    ];
    model.add_keyed("ORCH", script).await;
}

// Given: カタログを接続したランタイムと Stop 応答 1 件のスクリプト
// When: Worker run を終端まで実行する
// Then: 最初の complete 呼び出しは [System, User] で始まり、会話全体で System は 1 件のみ
#[tokio::test]
async fn run_starts_with_single_assembled_system_message() {
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let (runtime, _bus) = runtime_with(model.clone(), Some(complete_catalog()));

    let run_id = runtime.delegate_background(Role::Worker, "W".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let first_call = observed.first().expect("complete は 1 回以上観測される");
    assert_eq!(first_call.len(), 2);
    assert_eq!(first_call[0].role, MessageRole::System);
    assert_eq!(first_call[1].role, MessageRole::User);
    let last_call = observed.last().expect("complete は 1 回以上観測される");
    let system_count = last_call
        .iter()
        .filter(|m| m.role == MessageRole::System)
        .count();
    assert_eq!(system_count, 1);
}

// Given: compaction 設定を明示したランタイム
// When: Worker run の初回モデル呼び出しを観測する
// Then: compaction 方針が閾値・compact tool・cache prefix・cooldown を含む
#[tokio::test]
async fn configured_compaction_policy_is_added_to_initial_system_message() {
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let (runtime, _bus) = runtime_with(model.clone(), Some(complete_catalog()));
    let runtime = runtime.with_compaction(CompactionConfig::default());

    let run_id = runtime.delegate_background(Role::Worker, "W".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let system = text_of_role(&observed[0], MessageRole::System).expect("System がある");
    assert!(system.contains("75"));
    assert!(system.contains("call the `compact` tool"));
    assert!(system.contains("stable prompt prefix"));
    assert!(system.contains("cooldown 1 turn(s)"));
}

// Given: compaction 設定を接続していないゼロ設定ランタイム
// When: Worker run の初回モデル呼び出しを観測する
// Then: compaction 方針セクションは追加されない
#[tokio::test]
async fn zero_config_runtime_omits_compaction_policy() {
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let (runtime, _bus) = runtime_with(model.clone(), Some(complete_catalog()));

    let run_id = runtime.delegate_background(Role::Worker, "W".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let system = text_of_role(&observed[0], MessageRole::System).expect("System がある");
    assert!(!system.contains("call the `compact` tool"));
}

// Given: Claude と GPT-5 のモデルを選択した compaction 設定ランタイム
// When: それぞれの初回モデル呼び出しを観測する
// Then: cache 方針の family 条件分岐が保持される
#[tokio::test]
async fn compaction_policy_uses_selected_model_family() {
    for (model_id, expected, unexpected) in [
        (
            "claude-sonnet-4-5",
            "Preserve Claude prompt-cache prefix stability",
            "Preserve provider cache reuse",
        ),
        (
            "gpt-5.2",
            "Preserve provider cache reuse",
            "Preserve Claude prompt-cache prefix stability",
        ),
    ] {
        let model = Arc::new(
            ScriptedModel::new([Ok(text_response("done", FinishReason::Stop))])
                .with_selected_model(model_id),
        );
        let (runtime, _bus) = runtime_with(model.clone(), Some(complete_catalog()));
        let runtime = runtime.with_compaction(CompactionConfig::default());
        let run_id =
            runtime.delegate_background(Role::Worker, "W".to_string(), RunConfig::default());
        assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

        let observed = model.observed().await;
        let system = text_of_role(&observed[0], MessageRole::System).expect("System がある");
        assert!(system.contains(expected));
        assert!(!system.contains(unexpected));
    }
}

// Given: 対話モード run と 2 応答のスクリプト
// When: send_message で再開し終端まで実行する
// Then: すべての complete 呼び出しで先頭の System メッセージがバイト一致する (Stable Prefix)
#[tokio::test]
async fn system_message_is_assembled_once_and_byte_stable_across_turns() {
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response("waiting", FinishReason::Stop)),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _bus) = runtime_with(model.clone(), Some(complete_catalog()));
    let run_id = runtime.delegate_background(Role::Worker, "W".to_string(), cfg(true, None));
    runtime
        .send_message(run_id, "continue".to_string())
        .expect("inbox は受信可能");
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    assert_eq!(observed.len(), 2);
    let stable_prefix = observed[0][0].clone();
    assert_eq!(stable_prefix.role, MessageRole::System);
    for call in &observed {
        assert_eq!(
            call[0], stable_prefix,
            "System メッセージはターン間でバイト一致する"
        );
    }
}

// Given: カタログ未接続のランタイム (v0.1 構成)
// When: run を終端まで実行する
// Then: 最初の complete 呼び出しの先頭は User で System は挿入されない
#[tokio::test]
async fn runtime_without_catalog_keeps_user_first_history() {
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let (runtime, _bus) = runtime_with(model.clone(), None);

    let run_id = runtime.delegate_background(Role::Worker, "W".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let first_call = observed.first().expect("complete は 1 回以上観測される");
    assert_eq!(first_call[0].role, MessageRole::User);
    assert!(matches!(&first_call[0].content[0], ContentBlock::Text { text } if text == "W"));
}

// Given: category "quick" で Worker を委譲する Orchestrator スクリプト
// When: 親子両方の run を終端まで実行する
// Then: 子の System には Intent Gate がなく quick overlay があり、親は Intent Gate を持つ
#[tokio::test]
async fn delegated_child_run_gets_role_and_category_specific_system_prompt() {
    let model = Arc::new(ScriptedModel::new([]));
    orchestrator_delegate_script(&model, "quick", "all done").await;
    model
        .add_keyed("W1", [Ok(text_response("child done", FinishReason::Stop))])
        .await;
    let (runtime, _bus) = runtime_with(model.clone(), Some(complete_catalog()));

    let parent =
        runtime.delegate_background(Role::Orchestrator, "ORCH".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    let child = RunId::new(parent.get() + 1);
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let parent_call = observed
        .iter()
        .find(|call| text_of_role(call, MessageRole::User) == Some("ORCH"))
        .expect("親 run の complete 呼び出しが観測される");
    let child_call = observed
        .iter()
        .find(|call| text_of_role(call, MessageRole::User) == Some("W1"))
        .expect("子 run の complete 呼び出しが観測される");
    let parent_system =
        text_of_role(parent_call, MessageRole::System).expect("親の System メッセージ");
    let child_system =
        text_of_role(child_call, MessageRole::System).expect("子の System メッセージ");

    assert!(
        parent_system.contains(GATE_MARK)
            && parent_system.contains("ORCHESTRATOR-BASELINE")
            && !parent_system.contains(QUICK_OVERLAY_MARK),
        "Orchestrator の System は Intent Gate と role baseline を持ち overlay を持たない"
    );
    assert!(
        !child_system.contains(GATE_MARK)
            && child_system.contains("WORKER-BASELINE")
            && child_system.contains(QUICK_OVERLAY_MARK),
        "Worker の System は Intent Gate を持たず role baseline と quick overlay を持つ"
    );
    assert_ne!(parent_system, child_system);
}

// Given: 未知カテゴリ "typo" で委譲する Orchestrator スクリプト (子用スクリプトは未登録)
// When: 親 run を終端まで実行する
// Then: 委譲 op はエラー ToolResult で拒否され、子 run は登録もモデル呼び出しもされない
#[tokio::test]
async fn delegate_rejects_unknown_category_before_model_call() {
    let model = Arc::new(ScriptedModel::new([]));
    orchestrator_delegate_script(&model, "typo", "recover").await;
    let (runtime, _bus) = runtime_with(model.clone(), Some(complete_catalog()));

    let parent =
        runtime.delegate_background(Role::Orchestrator, "ORCH".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let second_turn = observed.get(1).expect("親の 2 回目の complete 呼び出し");
    let rejection = second_turn
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error: true,
            } if tool_call_id == "delegate-worker" => Some(content),
            _ => None,
        })
        .expect("委譲 op はエラー ToolResult で拒否される");
    assert!(rejection.iter().any(|item| matches!(
        item,
        ToolResultContent::Text { text } if text.contains("unknown category")
    )));
    assert_eq!(runtime.list_agents().len(), 1, "子 run は登録されない");
    let child_seen = observed
        .iter()
        .any(|call| text_of_role(call, MessageRole::User) == Some("W1"));
    assert!(!child_seen, "子 run のモデル呼び出しは発生しない");
}

// Given: "quick" overlay を欠くカタログ (このケースでは不完全) とカテゴリ付き run
// When: run を開始して終端を待つ
// Then: run は Error へ遷移し、モデルは 1 度も呼ばれず reason は識別子のみを運ぶ
#[tokio::test]
async fn missing_binding_fails_run_without_calling_complete() {
    let incomplete = catalog_builder()
        .build()
        .expect("overlay を除き必須部品を満たすカタログは構築できる");
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let (runtime, bus) = runtime_with(model.clone(), Some(incomplete));
    let mut receiver = bus.subscribe();

    let run_id =
        runtime.delegate_background(Role::Worker, "W".to_string(), cfg(false, Some("quick")));
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Error));
    let mut reason = None;
    while reason.is_none() {
        let event = timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("event timeout")
            .expect("event receiver remains open");
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id: id,
            to: AgentRunPhase::Error,
            reason: event_reason,
            ..
        }) = event.kind
            && id == run_id.to_string()
        {
            reason = event_reason;
        }
    }

    let reason = reason.expect("Error 遷移は reason を持つ");
    assert!(
        model.observed().await.is_empty() && reason.contains("quick"),
        "モデルは呼ばれず reason はカテゴリ名 (識別子) を運ぶ (fail-closed)"
    );
}
