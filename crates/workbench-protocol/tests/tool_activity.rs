mod common;
use common::*;

use workbench_protocol::{
    events::SnapshotItem,
    limits::{MAX_INLINE_MESSAGE_BYTES, MAX_NAME_BYTES, MAX_TITLE_BYTES},
    namespaces::execution::{ExecutionEvent, ToolActivityStatus},
    views::MessageContent,
};

#[test]
fn activity_uses_the_existing_id_summary_and_inline_body_limits() {
    let mut tool = common::tool_activity();
    tool.call_id = "c".repeat(MAX_NAME_BYTES);
    tool.parent_call_id = Some("p".repeat(MAX_NAME_BYTES));
    tool.tool_name = "t".repeat(MAX_NAME_BYTES);
    tool.input_summary = Some("s".repeat(MAX_TITLE_BYTES));
    tool.input = Some(MessageContent::Inline {
        text: "i".repeat(MAX_INLINE_MESSAGE_BYTES),
    });
    SnapshotItem::ToolActivity(tool.clone()).validate().unwrap();
    let mut invalid = tool.clone();
    invalid.call_id.push('c');
    assert!(invalid.validate().is_err());
    let mut invalid = tool.clone();
    invalid.parent_call_id = Some(String::new());
    assert!(invalid.validate().is_err());
    let mut invalid = tool.clone();
    invalid.tool_name.clear();
    assert!(invalid.validate().is_err());
    let mut invalid = tool.clone();
    invalid.input_summary.as_mut().unwrap().push('s');
    assert!(invalid.validate().is_err());
    tool.input = Some(MessageContent::Inline {
        text: "i".repeat(MAX_INLINE_MESSAGE_BYTES + 1),
    });
    assert!(tool.validate().is_err());
}

#[test]
fn finishes_are_typed_and_validate_call_identity_without_inventing_a_duration() {
    for status in [
        ToolActivityStatus::Succeeded,
        ToolActivityStatus::Failed,
        ToolActivityStatus::Interrupted,
    ] {
        let mut event = ExecutionEvent::ToolFinished {
            thread: common::thread(),
            run: common::run(),
            call_id: "call".into(),
            status,
            duration_ms: None,
        };
        event.validate().unwrap();
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(
            serde_json::from_value::<ExecutionEvent>(wire).unwrap(),
            event
        );
        if let ExecutionEvent::ToolFinished { call_id, .. } = &mut event {
            call_id.clear();
        }
        assert!(event.validate().is_err());
    }
}
