use event_bus::{Event, ToolEvent};
use gui::model::pending_approvals::PendingApprovalsModel;
use serde_json::json;

#[test]
fn event_input_wins_when_fallback_has_different_arguments() {
    // Given: 引数の異なるイベントと旧 ToolStarted の補完候補。
    let mut model = PendingApprovalsModel::default();
    let event = Event::new(ToolEvent::ApprovalRequested {
        tool_name: "shell".into(),
        call_id: "run-2:call-1:17".into(),
        input: Some(json!({"command": "pwd"})),
    });
    let fallback_calls = std::cell::Cell::new(0);
    // When: イベントを適用する。
    model.apply_event(
        &event,
        |_| None,
        |_, _| {
            fallback_calls.set(fallback_calls.get() + 1);
            Some(json!({"command": "ls"}))
        },
    );
    // Then: イベントを優先し、不要な補完は呼ばない。
    assert_eq!(
        model
            .get("run-2:call-1:17")
            .and_then(|item| item.input.as_ref()),
        Some(&json!({"command": "pwd"}))
    );
    assert_eq!(fallback_calls.get(), 0);
}
