//! Wire-level bounds and lossless revisions of the read-only plan protocol.
use serde_json::json;
use workbench_protocol::{
    handshake::WireLimits,
    namespaces::execution::plan::*,
    namespaces::{ExecutionQuery, ExecutionReply},
    wire::{Revision, Sequence},
};
fn progress() -> PlanProgress {
    PlanProgress {
        node_count: 1,
        leaf_count: 1,
        leaf_counts: PlanStatusValues {
            pending: 1,
            ..Default::default()
        },
        weighted_ppm: PlanStatusValues {
            pending: 1_000_000,
            ..Default::default()
        },
    }
}
fn node() -> PlanNodeView {
    PlanNodeView {
        task_id: "task-1".into(),
        parent_id: None,
        position: 0,
        depth: 1,
        title: "task".into(),
        weight: 1,
        status: Some(PlanTaskStatus::Pending),
        child_count: 0,
        progress_ppm: progress().weighted_ppm,
    }
}
#[test]
fn queries_bound_pages_and_keep_revision_cursors_lossless() {
    for (kind, max) in [
        ("read_conversation_plan_tree", 100),
        ("read_conversation_plan_history", 20),
    ] {
        for limit in [0, max + 1, u32::MAX] {
            let q: ExecutionQuery = serde_json::from_value(
                json!({"type":kind,"payload":{"thread":"thread","limit":limit}}),
            )
            .unwrap();
            assert!(q.validate(&WireLimits::default()).is_err());
        }
        let q: ExecutionQuery =
            serde_json::from_value(json!({"type":kind,"payload":{"thread":"thread","limit":max}}))
                .unwrap();
        q.validate(&WireLimits::default()).unwrap();
    }
    let rev = Revision(u64::MAX - 1);
    let q = PlanHistoryQuery {
        thread: workbench_protocol::ids::ThreadId::new("thread").unwrap(),
        after_revision: Some(rev),
        limit: None,
    };
    let value = serde_json::to_value(&q).unwrap();
    assert_eq!(value["after_revision"], rev.0.to_string());
    assert_eq!(
        serde_json::from_value::<PlanHistoryQuery>(value).unwrap(),
        q
    );
    assert!(
        serde_json::from_value::<PlanHistoryQuery>(
            json!({"thread":"thread","after_revision":rev.0})
        )
        .is_err()
    );
}
#[test]
fn update_times_round_trip_as_optional_unix_milliseconds() {
    for time in [None, Some(0), Some(-1), Some(1_700_000_000_123)] {
        let summary = PlanSummary {
            thread: workbench_protocol::ids::ThreadId::new("thread").unwrap(),
            plan_id: Some("plan".into()),
            revision: Revision(1),
            progress: progress(),
            updated_at_unix_ms: time,
        };
        summary.validate().unwrap();
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["updated_at_unix_ms"], json!(time));
        assert_eq!(
            serde_json::from_value::<PlanSummary>(value).unwrap(),
            summary
        );
        let entry = PlanHistoryEntry {
            revision: Revision(1),
            event_id: Sequence(7),
            change: None,
            updated_at_unix_ms: time,
        };
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value["updated_at_unix_ms"], json!(time));
        assert_eq!(
            serde_json::from_value::<PlanHistoryEntry>(value).unwrap(),
            entry
        );
    }
}

#[test]
fn replies_enforce_plan_shapes_and_serialized_bounds() {
    let mut page = PlanTreePage {
        plan_id: Some("plan-1".into()),
        revision: Revision(1),
        progress: progress(),
        nodes: vec![node()],
        next_cursor: None,
    };
    ExecutionReply::ConversationPlanTree(Box::new(page.clone()))
        .validate(&WireLimits::default())
        .unwrap();
    page.nodes[0].weight = 0;
    assert!(page.validate().is_err());
    page.nodes[0] = node();
    page.nodes[0].status = None;
    assert!(page.validate().is_err());
    page.nodes = vec![node(); 101];
    assert!(page.validate().is_err());
    let mut p = progress();
    p.leaf_counts.pending = u32::MAX;
    assert!(p.validate().is_err());
    let mut detail:PlanTaskRead=serde_json::from_value(json!({"plan_id":"plan-1","revision":"1","node":{"task_id":"task-1","parent_id":null,"position":0,"depth":1,"title":"task","weight":1,"status":"pending","child_ids":[],"progress_ppm":progress().weighted_ppm,"description":"detail","result":"","blocker":""}})).unwrap();
    detail.validate().unwrap();
    detail.node.description = "\u{0001}".repeat(4096);
    assert!(detail.validate().is_err()); // Escaped JSON, not raw field bytes, sets the result bound.
    let entry = PlanHistoryEntry {
        revision: Revision(1),
        event_id: Sequence(7),
        change: None,
        updated_at_unix_ms: None,
    };
    let mut history = PlanHistoryPage {
        plan_id: Some("plan-1".into()),
        revision: Revision(1),
        entries: vec![entry.clone()],
        next_after_revision: None,
    };
    history.validate().unwrap();
    history.entries.push(entry);
    assert!(history.validate().is_err());
}
