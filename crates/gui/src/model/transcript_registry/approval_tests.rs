use super::*;
use crate::model::transcript::ToolStatus;

const ROOT: &str = "run-1";
const CHILD: &str = "run-2";

fn registry() -> TranscriptRegistry {
    let mut registry = TranscriptRegistry::new();
    registry.bind_thread_root("thread", ROOT);
    registry.bind_run(CHILD, "thread");
    registry.select_thread(Some("thread".into()));
    registry
}

fn approvals(run: &str) -> [(Event, TranscriptEntry); 3] {
    let call_id = format!("{run}:call-x:0");
    [
        (
            ToolEvent::ApprovalRequested {
                tool_name: "write".into(),
                call_id: call_id.clone(),
                input: None,
            },
            "write",
            ToolStatus::AwaitingApproval,
        ),
        (
            ToolEvent::ApprovalResolved {
                call_id: call_id.clone(),
                approved: false,
            },
            "",
            ToolStatus::Denied {
                reason: "approval denied".into(),
            },
        ),
        (
            ToolEvent::ExecutionDenied {
                tool_name: "write".into(),
                call_id: call_id.clone(),
                reason: "policy denied".into(),
            },
            "write",
            ToolStatus::Denied {
                reason: "policy denied".into(),
            },
        ),
    ]
    .map(|(event, tool_name, status)| {
        (
            Event::new(event),
            TranscriptEntry::Tool {
                tool_name: tool_name.into(),
                call_id: call_id.clone(),
                input: None,
                output: None,
                detail: None,
                is_error: false,
                status,
            },
        )
    })
}

#[test]
fn scoped_child_preflight_approval_stays_in_run() {
    for (event, expected) in approvals(CHILD) {
        // Given: a bound child with no preceding ToolStarted.
        let mut registry = registry();
        // When: a scoped approval arrives before execution.
        registry.apply(&event);
        // Then: the child alone receives the complete approval entry.
        assert!(registry.thread().entries().is_empty());
        assert_eq!(registry.run(CHILD).expect("child").entries(), &[expected]);
        assert_eq!(
            registry.route(&event),
            vec![TranscriptKey::Run(CHILD.into())]
        );
    }
}

#[test]
fn scoped_child_approval_after_plain_tool_start_stays_in_run() {
    for (event, expected) in approvals(CHILD) {
        // Given: ToolStarted indexed the plain id, not the scoped approval id.
        let mut registry = registry();
        registry.apply(&Event::new(ToolEvent::ToolStarted {
            tool_name: "write".into(),
            call_id: "call-x".into(),
            run_id: Some(CHILD.into()),
            input: None,
        }));
        let started = registry.run(CHILD).expect("child").entries()[0].clone();
        // When: the scoped approval arrives on the denial path.
        registry.apply(&event);
        // Then: the plain entry is preserved and the child receives the approval.
        assert!(registry.thread().entries().is_empty());
        assert_eq!(
            registry.run(CHILD).expect("child").entries(),
            &[started, expected]
        );
    }
}

#[test]
fn scoped_child_approval_reaches_neither_active_nor_owning_thread() {
    for (event, expected) in approvals(CHILD) {
        // Given: the child belongs to B while A is active.
        let mut registry = TranscriptRegistry::new();
        registry.bind_thread_root("thread-a", "run-3");
        registry.bind_thread_root("thread-b", ROOT);
        registry.bind_run(CHILD, "thread-b");
        registry.select_thread(Some("thread-b".into()));
        registry.select_thread(Some("thread-a".into()));
        // When: the background child's scoped approval arrives.
        registry.apply(&event);
        // Then: neither conversation receives it, only the child run does.
        assert!(registry.thread().entries().is_empty());
        assert!(registry.threads["thread-b"].entries().is_empty());
        assert!(registry.thread.entries().is_empty());
        assert_eq!(registry.run(CHILD).expect("child").entries(), &[expected]);
    }
}

#[test]
fn scoped_root_approval_reaches_thread_and_run() {
    for (event, expected) in approvals(ROOT) {
        // Given: the root is explicitly bound to its conversation.
        let mut registry = registry();
        // When: a scoped root approval arrives without ToolStarted.
        registry.apply(&event);
        // Then: both the owning thread and root receive the approval.
        assert_eq!(registry.thread().entries(), std::slice::from_ref(&expected));
        assert_eq!(registry.run(ROOT).expect("root").entries(), &[expected]);
        assert_eq!(
            registry.route(&event),
            vec![TranscriptKey::Thread, TranscriptKey::Run(ROOT.into())]
        );
    }
}
