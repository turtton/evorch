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
                run_id: Some(run.into()),
                tool_name: tool_name.into(),
                call_id: "call-x".into(),
                input: None,
                output: None,
                detail: None,
                is_error: false,
                status,
                reviews: Vec::new(),
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

fn started(run: &str) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: "write".into(),
        call_id: "call-x".into(),
        run_id: Some(run.into()),
        input: Some(serde_json::json!({"path": "a.rs"})),
    })
}

fn status(entry: &TranscriptEntry) -> &ToolStatus {
    match entry {
        TranscriptEntry::Tool { status, .. } => status,
        other => panic!("not a tool entry: {other:?}"),
    }
}

#[test]
fn scoped_child_approval_after_plain_tool_start_updates_that_card() {
    for (event, expected) in approvals(CHILD) {
        // Given: ToolStarted indexed the plain id, not the scoped approval id.
        let mut registry = registry();
        registry.apply(&started(CHILD));
        // When: the scoped approval arrives on the denial path.
        registry.apply(&event);
        // Then: the original card carries the approval status; no second card appears.
        assert!(registry.thread().entries().is_empty());
        let entries = registry.run(CHILD).expect("child").entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(status(&entries[0]), status(&expected));
    }
}

#[test]
fn approval_before_start_and_approval_while_running_keep_one_card() {
    let approval_id = format!("{ROOT}:call-x");
    let request = Event::new(ToolEvent::ApprovalRequested {
        tool_name: "write".into(),
        call_id: approval_id.clone(),
        input: Some(serde_json::json!({"path": "a.rs"})),
    });
    let approve = Event::new(ToolEvent::ApprovalResolved {
        call_id: approval_id,
        approved: true,
    });
    // Given: the approval opens the card before the call starts.
    let mut before = registry();
    before.apply(&request);
    before.apply(&approve);
    // When: the approved call starts.
    before.apply(&started(ROOT));
    // Then: execution continues in the card the approval opened.
    let entries = before.run(ROOT).expect("root").entries();
    assert!(matches!(
        entries,
        [TranscriptEntry::Tool {
            status: ToolStatus::Running,
            input: Some(_),
            ..
        }]
    ));
    assert_eq!(before.thread().entries(), entries);

    // Given: a running call asks for approval mid-execution (sandbox escalation).
    let mut during = registry();
    during.apply(&started(ROOT));
    during.apply(&request);
    let [entry] = during.run(ROOT).expect("root").entries() else {
        panic!("one card")
    };
    assert_eq!(status(entry), &ToolStatus::AwaitingApproval);
    // When: the user approves.
    during.apply(&approve);
    // Then: the same card resumes running.
    let [entry] = during.run(ROOT).expect("root").entries() else {
        panic!("one card")
    };
    assert_eq!(status(entry), &ToolStatus::Running);
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
