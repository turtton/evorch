use event_bus::{Event, ToolEvent};
use gui::{
    app::WorkbenchState,
    fixture::DemoSource,
    model::pending_approvals::{PendingApproval, PendingApprovalsModel},
};
use serde_json::json;
use workspace_ui::UiSettings;

fn requested(call_id: &str) -> Event {
    Event::new(ToolEvent::ApprovalRequested {
        input: None,
        tool_name: "shell".into(),
        call_id: call_id.into(),
    })
}

fn resolved(call_id: &str) -> Event {
    Event::new(ToolEvent::ApprovalResolved {
        call_id: call_id.into(),
        approved: true,
    })
}

fn apply(model: &mut PendingApprovalsModel, event: &Event) {
    model.apply_event(event, |_| None, |_, _| None);
}

#[test]
fn pending_approvals_removes_only_exact_resolved_key() {
    // Given: 同じ run の別々の承認要求。
    let mut model = PendingApprovalsModel::default();
    apply(&mut model, &requested("run-2:call-1:17"));
    apply(&mut model, &requested("run-2:call-2:18"));
    let before = model.get("run-2:call-2:18").cloned();
    // When: A だけを承認する。
    apply(&mut model, &resolved("run-2:call-1:17"));
    // Then: B は全フィールド不変、A だけが消える。
    assert!(model.get("run-2:call-1:17").is_none());
    assert_eq!(model.get("run-2:call-2:18"), before.as_ref());
    assert_eq!(model.items().count(), 1);
}

#[test]
fn pending_approvals_ignores_unknown_and_other_scope_resolutions() {
    // Given: 同じ元 call ID を異なる run/attempt で承認待ちにする。
    let mut model = PendingApprovalsModel::default();
    for id in ["run-2:call-1:17", "run-2:call-1:18", "run-3:call-1:17"] {
        apply(&mut model, &requested(id));
    }
    let before: Vec<_> = model.items().cloned().collect();
    // When: 未知 ID、元 ID、attempt のない ID を解決する。
    for id in ["unknown", "call-1", "run-2:call-1", "run-2:call-1:19"] {
        apply(&mut model, &resolved(id));
    }
    // Then: 部分一致で削除しない。
    assert_eq!(model.items().cloned().collect::<Vec<_>>(), before);
}

#[test]
fn pending_approvals_accepts_raw_call_with_fallback_run() {
    // Given: scoped でない AskFirst 要求。
    let mut model = PendingApprovalsModel::default();
    // When: resolver から run を補完する。
    model.apply_event(
        &requested("raw-call"),
        |id| (id == "raw-call").then(|| "run-9".into()),
        |_, _| None,
    );
    // Then: raw ID を保持し、attempt は推測しない。
    assert_eq!(
        model.get("raw-call"),
        Some(&PendingApproval {
            call_id: "raw-call".into(),
            tool_name: "shell".into(),
            run_id: Some("run-9".into()),
            attempt: None,
            input: None,
        })
    );
}

#[test]
fn pending_approvals_upsert_preserves_insertion_order() {
    // Given: 辞書順と異なる挿入順。
    let mut model = PendingApprovalsModel::default();
    apply(&mut model, &requested("run-2:z:17"));
    apply(&mut model, &requested("run-2:a:18"));
    // When: 先頭の要求を再送する。
    apply(&mut model, &requested("run-2:z:17"));
    // Then: 重複せず順序も変わらない。
    assert_eq!(
        model
            .items()
            .map(|item| item.call_id.as_str())
            .collect::<Vec<_>>(),
        ["run-2:z:17", "run-2:a:18"]
    );
}

#[test]
fn pending_approvals_has_no_input_without_tool_started() {
    // Given: ToolStarted のない preflight 要求。
    let mut model = PendingApprovalsModel::default();
    // When: scoped ID を解釈する。
    model.apply_event(
        &requested("run-2:call-1:17"),
        |_| Some("run-9".into()),
        |_, _| None,
    );
    // Then: scoped run を優先し、引数を捏造しない。
    assert_eq!(
        model.get("run-2:call-1:17"),
        Some(&PendingApproval {
            call_id: "run-2:call-1:17".into(),
            tool_name: "shell".into(),
            run_id: Some("run-2".into()),
            attempt: Some(17),
            input: None,
        })
    );
}

#[test]
fn pending_approvals_resolver_receives_original_call_and_scoped_run() {
    // Given: 引数を解決可能な scoped 要求。
    let mut model = PendingApprovalsModel::default();
    // When: input resolver が呼ばれる。
    model.apply_event(
        &requested("run-2:call-1:17"),
        |_| None,
        |run, call| {
            assert_eq!((run, call), ("run-2", "call-1"));
            Some(json!({"command": "pwd"}))
        },
    );
    // Then: 解決した引数が保存される。
    assert_eq!(
        model
            .get("run-2:call-1:17")
            .and_then(|item| item.input.as_ref()),
        Some(&json!({"command": "pwd"}))
    );
}

#[test]
fn pending_approvals_workbench_joins_input_by_run_and_original_call() {
    // Given: 元 call ID を複数 run が共有している。
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).expect("state");
    state.apply_events([("run-2", "pwd"), ("run-9", "ls")].map(|(run, command)| {
        Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "call-1".into(),
            run_id: Some(run.into()),
            input: Some(json!({"command": command})),
        })
    }));
    // When: 明示された run の承認を要求する。
    state.apply_events([requested("run-2:call-1:17")]);
    // Then: 他 run の引数を混ぜない。
    assert_eq!(
        state
            .pending_approvals()
            .get("run-2:call-1:17")
            .and_then(|item| item.input.as_ref()),
        Some(&json!({"command": "pwd"}))
    );
}

#[test]
fn pending_approvals_workbench_accepts_raw_and_missing_started_requests() {
    // Given: raw call の ToolStarted と、それとは無関係な scoped 要求。
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).expect("state");
    state.apply_events([Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: "raw".into(),
        run_id: Some("run-9".into()),
        input: Some(json!({"command": "pwd"})),
    })]);
    // When: 要求を折り畳む。
    state.apply_events([requested("raw"), requested("run-2:missing:17")]);
    // Then: raw の run/input を補完し、ToolStarted のない要求も保持する。
    assert_eq!(
        state.pending_approvals().get("raw"),
        Some(&PendingApproval {
            call_id: "raw".into(),
            tool_name: "shell".into(),
            run_id: Some("run-9".into()),
            attempt: None,
            input: Some(json!({"command": "pwd"})),
        })
    );
    assert_eq!(
        state
            .pending_approvals()
            .get("run-2:missing:17")
            .map(|item| &item.input),
        Some(&None)
    );
}

#[test]
fn pending_approvals_workbench_removes_only_resolved_request() {
    // Given: 実際の fold 経路で受信した複数の要求。
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).expect("state");
    state.apply_events([requested("run-2:call-1:17"), requested("run-2:call-2:18")]);
    // When: 解決イベントを受信する。
    state.apply_events([resolved("run-2:call-1:17")]);
    // Then: 他の要求は残る。
    assert_eq!(
        state
            .pending_approvals()
            .items()
            .map(|item| item.call_id.as_str())
            .collect::<Vec<_>>(),
        ["run-2:call-2:18"]
    );
}
