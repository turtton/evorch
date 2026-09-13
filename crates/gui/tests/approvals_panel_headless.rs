use std::sync::{Arc, Mutex};

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, ToolEvent};
use gui::{
    app::WorkbenchState,
    fixture::DemoSource,
    model::commands::{CommandSink, LoopEvent, RecordingSink, WorkbenchCommand},
};
use serde_json::json;
use workspace_ui::{PanelId, UiSettings};

const A: &str = "run-2:call-1:17";
const B: &str = "run-3:call-1:0018:opaque";

struct SharedSink(Arc<Mutex<RecordingSink>>);

impl CommandSink for SharedSink {
    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        self.0.lock().expect("recording sink").submit(command)
    }
}

fn requested(call_id: &str, tool_name: &str) -> Event {
    Event::new(ToolEvent::ApprovalRequested {
        input: None,
        call_id: call_id.into(),
        tool_name: tool_name.into(),
    })
}

fn harness() -> (
    Harness<'static, WorkbenchState<DemoSource>>,
    Arc<Mutex<RecordingSink>>,
) {
    let sink = Arc::new(Mutex::new(RecordingSink::default()));
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("workbench")
        .with_command_sink(Box::new(SharedSink(sink.clone())));
    state.apply_events([
        Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "call-1".into(),
            run_id: Some("run-2".into()),
            input: Some(json!({"command": "pwd"})),
        }),
        requested(A, "shell"),
        requested(B, "write"),
    ]);
    let path = state
        .dock()
        .find_tab(&PanelId::new("approvals-main"))
        .expect("approvals tab");
    state.dock_mut().set_active_tab(path).expect("activate");
    let harness = Harness::builder()
        .with_size(egui::vec2(1280.0, 900.0))
        .build_ui_state(
            |ui, state| state.ui(ui, &mut eframe::Frame::_new_kittest()),
            state,
        );
    (harness, sink)
}

#[test]
fn displays_arguments_when_approval_arrives_before_tool_started() {
    // Given: ToolStarted がまだ発行されていない承認要求。
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).expect("state");
    let request: ToolEvent = serde_json::from_value(json!({
        "kind": "ApprovalRequested",
        "payload": {"tool_name": "shell", "call_id": A, "input": {"command": "pwd"}}
    }))
    .expect("approval event");
    state.apply_events([Event::new(request)]);
    let mut harness = Harness::builder().build_ui_state(
        |ui, state| {
            gui::theme::install(ui.ctx());
            gui::panes::approvals::approvals_pane(ui, state.pending_approvals());
        },
        state,
    );
    // When: 承認待ちの pane を描画する。
    harness.run_steps(3);
    // Then: 実行開始を待たずにイベントの引数要約を表示する。
    assert!(harness.query_by_label(r#"{"command":"pwd"}"#).is_some());
    assert!(harness.query_by_label("引数情報なし").is_none());
}

#[test]
fn displays_correlations_and_arguments_when_requests_are_pending() {
    // Given: 同じ元 call ID を共有する別 run の要求と、A だけの引数。
    let (mut harness, _) = harness();
    // When: 実際の Workbench pane を描画する。
    harness.run_steps(3);
    // Then: 引数を別 run に流用せず、両行の相関を表示する。
    for label in [
        "shell",
        "write",
        "run-2 · call-1 · attempt 17",
        "run-3 · call-1 · attempt 18",
        r#"{"command":"pwd"}"#,
        "引数情報なし",
    ] {
        assert!(harness.query_by_label(label).is_some(), "missing {label}");
    }
    assert_eq!(harness.query_all_by_label("Approve").count(), 2);
    assert_eq!(harness.query_all_by_label("Reject").count(), 2);
    if let Ok(path) = std::env::var("APPROVALS_QA_CAPTURE") {
        harness
            .render()
            .expect("render approvals")
            .save(path)
            .expect("save capture");
    }
}

#[test]
fn displays_fallbacks_and_empty_state_when_information_is_missing() {
    // Given: run、call、attempt の情報がない要求。
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).expect("state");
    state.apply_events([requested("", "shell")]);
    let mut harness = Harness::builder().build_ui_state(
        |ui, state| {
            gui::theme::install(ui.ctx());
            gui::panes::approvals::approvals_pane(ui, state.pending_approvals());
        },
        state,
    );
    // When: 欠落情報を持つ行を描画する。
    harness.run_steps(3);
    // Then: 推測せずフォールバックを表示する。
    assert!(harness.query_by_label("— · — · attempt —").is_some());
    assert!(harness.query_by_label("引数情報なし").is_some());
    harness
        .state_mut()
        .apply_events([Event::new(ToolEvent::ApprovalResolved {
            call_id: "".into(),
            approved: false,
        })]);
    harness.run_steps(3);
    assert!(
        harness
            .query_by_label("保留中の承認要求はありません")
            .is_some()
    );
    assert_eq!(harness.query_all_by_label("Approve").count(), 0);
}

#[test]
fn truncates_unicode_arguments_when_summary_exceeds_limit() {
    // Given: 120 文字を超える日本語の JSON 引数。
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).expect("state");
    state.apply_events([
        Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "call-1".into(),
            run_id: Some("run-2".into()),
            input: Some(json!("界".repeat(130))),
        }),
        requested(A, "shell"),
    ]);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(375.0, 600.0))
        .build_ui_state(
            |ui, state| {
                gui::theme::install(ui.ctx());
                gui::panes::approvals::approvals_pane(ui, state.pending_approvals());
            },
            state,
        );
    // When: 狭い pane に描画する。
    harness.run_steps(3);
    // Then: UTF-8 を壊さず120文字と省略記号になり、ボタンは pane 内に収まる。
    let expected = format!("\"{}…", "界".repeat(119));
    assert!(harness.query_by_label(&expected).is_some());
    for label in ["Approve", "Reject"] {
        let rect = harness.get_by_label(label).rect();
        assert!(rect.min.x >= 0.0 && rect.max.x <= 375.0);
    }
}

#[test]
fn approves_exact_scope_when_second_row_is_clicked() {
    // Given: 複数行を表示した実際の pane。
    let (mut harness, sink) = harness();
    harness.run_steps(3);
    // When: B 行の Approve をクリックする。
    harness
        .query_all_by_label("Approve")
        .nth(1)
        .expect("B approve")
        .click();
    harness.run_steps(3);
    // Then: 正規化していない exact ID が一度だけ送られ、解決までは両行が残る。
    assert_eq!(
        sink.lock().expect("sink").issued,
        [WorkbenchCommand::DecideToolApproval {
            call_id: B.into(),
            approved: true,
        }]
    );
    assert_eq!(harness.query_all_by_label("Approve").count(), 2);
}

#[test]
fn rejects_exact_scope_when_second_row_is_clicked() {
    // Given: 複数行を表示した実際の pane。
    let (mut harness, sink) = harness();
    harness.run_steps(3);
    // When: B 行の Reject をクリックする。
    harness
        .query_all_by_label("Reject")
        .nth(1)
        .expect("B reject")
        .click();
    harness.run_steps(3);
    // Then: Reject も同じ exact ID を一度だけ送り、UI は削除しない。
    assert_eq!(
        sink.lock().expect("sink").issued,
        [WorkbenchCommand::DecideToolApproval {
            call_id: B.into(),
            approved: false,
        }]
    );
    assert_eq!(harness.query_all_by_label("Reject").count(), 2);
}

#[test]
fn removes_only_resolved_row_when_resolution_arrives() {
    for approved in [true, false] {
        // Given: 同じ元 call ID を持つ A/B の保留行。
        let (mut harness, _) = harness();
        harness.run_steps(3);
        // When: B の解決イベントが fold される。
        harness
            .state_mut()
            .apply_events([Event::new(ToolEvent::ApprovalResolved {
                call_id: B.into(),
                approved,
            })]);
        harness.run_steps(3);
        // Then: B だけが消え、A は判断可能なまま残る。
        assert!(
            harness
                .query_by_label("run-3 · call-1 · attempt 18")
                .is_none()
        );
        assert!(
            harness
                .query_by_label("run-2 · call-1 · attempt 17")
                .is_some()
        );
        assert_eq!(harness.query_all_by_label("Approve").count(), 1);
        assert_eq!(harness.query_all_by_label("Reject").count(), 1);
    }
}
