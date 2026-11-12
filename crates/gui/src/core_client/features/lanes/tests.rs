//! The lanes feature drawn headless: lanes are read from the ledger, every
//! operation names the revision and the lead's root run the screen showed, and
//! an answer for a lane or lead no longer shown is dropped.
use super::*;
use crate::core_client::features::execution::tests::{
    answer, asked, command_of, ok_command, query_of, query_reply, screen,
};
use crate::core_client::features::{FeatureAction, FeatureRequest};
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

fn summary(id: &str, policy: &str, revision: u64) -> Value {
    json!({"lane_id": id, "thread_id": format!("thread-{id}"), "lead_thread_id": "lead",
        "policy": policy, "management_revision": revision.to_string(), "brief_revision": "2",
        "objective_preview": format!("objective {id}"), "root_run": "run-lane", "generation": "1",
        "start": "started", "last_activity_at": "1700000000000", "revision": "1"})
}

fn lanes(items: Vec<Value>) -> FeatureReply {
    query_reply(
        json!({"type": "lanes", "payload": {"type": "lanes", "payload": {
        "items": items, "snapshot_revision": "1"}}}),
    )
}

fn detail(id: &str, held: bool, revision: u64) -> FeatureReply {
    query_reply(
        json!({"type": "lanes", "payload": {"type": "lane", "payload": {
        "summary": summary(id, "attached", revision),
        "latest_report": {"seq": "4", "status": "ready", "summary": "done", "needs_human": false,
            "management_revision": "3", "brief_revision": "1", "created_at": "1700000000001"},
        "ready": {"report_seq": "4", "brief_revision": "1", "brief_changed_since": true,
            "readiness": "unreviewed"},
        "delivery": {"held": held, "pending_count": if held { 2 } else { 0 }},
        "attention": []}}}),
    )
}

fn roots(items: Vec<&str>) -> FeatureReply {
    let items: Vec<Value> = items
        .into_iter()
        .map(|id| {
            json!({"summary": {"id": id, "thread": "lead", "role": "orchestrator",
                "phase": "running", "revision": "1"},
                "name": id, "lifecycle": "active", "source": "live", "child_count": 0,
                "revision": "1"})
        })
        .collect();
    query_reply(
        json!({"type": "execution", "payload": {"type": "run_details", "payload": {
        "items": items, "snapshot_revision": "1"}}}),
    )
}

fn tag_of(actions: &[FeatureAction], wanted: impl Fn(&Query) -> bool) -> u64 {
    actions
        .iter()
        .find(|a| matches!(&a.request, FeatureRequest::Query(q) if wanted(q)))
        .expect("the query was asked for")
        .tag
}

fn is_list(query: &Query) -> bool {
    matches!(query, Query::Lanes(LanesQuery::ListLanes(_)))
}

fn is_roots(query: &Query) -> bool {
    matches!(query, Query::Execution(ExecutionQuery::ListRunDetails(_)))
}

fn manage_of(action: &FeatureAction) -> &ManageLane {
    match command_of(action) {
        Command::Lanes(LanesCommand::ManageLane(manage)) => manage,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_lane_is_listed_from_the_ledger_and_its_operations_name_the_revision_shown() {
    let mut screen = screen(Box::<LanesFeature>::default(), Some("lead"));
    let first = asked(&mut screen);
    answer(
        &mut screen,
        tag_of(&first, is_list),
        lanes(vec![
            summary("l1", "attached", 3),
            summary("l2", "summary", 5),
        ]),
    );
    screen.get_by_label("objective l1");
    screen.get_by_label("objective l2");

    screen.get_by_label("概要のみにする").click();
    let policy = asked(&mut screen);
    let manage = manage_of(&policy[0]);
    assert_eq!(manage.request.lane_id.as_str(), "l1");
    assert_eq!(manage.request.expected_management_revision, Revision(3));
    assert!(matches!(
        manage.request.action,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Summary
        }
    ));

    // A refusal for an older revision is shown as it is; the lane is read again.
    answer(
        &mut screen,
        policy[0].tag,
        FeatureReply::Command(Err(FeatureError::new("stale_management_revision"))),
    );
    screen.get_by_label("lane の policy 変更: stale_management_revision");
    assert!(
        asked(&mut screen)
            .iter()
            .any(|a| matches!(&a.request, FeatureRequest::Query(q) if is_list(q)))
    );
}

#[test]
fn a_detach_then_a_reattach_waits_for_the_family_and_the_lead_root_run() {
    let mut screen = screen(Box::<LanesFeature>::default(), Some("lead"));
    let first = asked(&mut screen);
    answer(
        &mut screen,
        tag_of(&first, is_roots),
        roots(vec!["run-old", "run-lead"]),
    );
    answer(
        &mut screen,
        tag_of(&first, is_list),
        lanes(vec![summary("l1", "attached", 3)]),
    );
    screen.get_by_label("切り離す").click();
    let detach = asked(&mut screen);
    let manage = manage_of(&detach[0]);
    assert!(matches!(
        manage.request.action,
        LaneManagementAction::Detach
    ));
    assert_eq!(manage.request.expected_management_revision, Revision(3));

    answer(&mut screen, detach[0].tag, ok_command());
    let refreshed = asked(&mut screen);
    answer(&mut screen, tag_of(&refreshed, is_list), lanes(vec![]));
    answer(
        &mut screen,
        tag_of(&refreshed, is_roots),
        roots(vec!["run-old", "run-lead"]),
    );

    screen.get_by_label("再接続").click();
    let asks = asked(&mut screen);
    let state_tag = tag_of(&asks, |q| {
        matches!(q, Query::Lanes(LanesQuery::GetLaneManagement { .. }))
    });
    // Nothing is sent until both answers are in.
    assert!(
        !asks
            .iter()
            .any(|a| matches!(a.request, FeatureRequest::Command(_)))
    );
    answer(
        &mut screen,
        tag_of(&asks, is_roots),
        roots(vec!["run-old", "run-lead"]),
    );
    assert!(asked(&mut screen).is_empty());
    answer(
        &mut screen,
        state_tag,
        query_reply(
            json!({"type": "lanes", "payload": {"type": "lane_management", "payload": {
            "lane_id": "l1", "thread_id": "thread-l1", "lead_thread_id": null,
            "policy": "detached", "management_revision": "4",
            "detached_since_management_revision": "4"}}}),
        ),
    );
    let reattach = asked(&mut screen);
    let manage = manage_of(&reattach[0]);
    assert_eq!(manage.request.expected_management_revision, Revision(4));
    let LaneManagementAction::Reattach {
        lead_thread_id,
        expected_lead_root_run_id,
    } = &manage.request.action
    else {
        panic!("a reattach was expected");
    };
    assert_eq!(lead_thread_id.as_str(), "lead");
    assert_eq!(expected_lead_root_run_id.as_str(), "run-lead");
}

#[test]
fn held_notices_are_resumed_for_the_current_lead_root_and_revision_only() {
    let mut screen = screen(Box::<LanesFeature>::default(), Some("lead"));
    let first = asked(&mut screen);
    answer(
        &mut screen,
        tag_of(&first, is_roots),
        roots(vec!["run-lead"]),
    );
    answer(
        &mut screen,
        tag_of(&first, is_list),
        lanes(vec![summary("l1", "attached", 3)]),
    );
    screen.get_by_label("objective l1").click();
    let detail_ask = asked(&mut screen);
    let Query::Lanes(LanesQuery::GetLane { lane_id }) = query_of(&detail_ask[0]) else {
        panic!("a lane detail was expected");
    };
    assert_eq!(lane_id.as_str(), "l1");
    answer(&mut screen, detail_ask[0].tag, detail("l1", true, 3));
    screen.get_by_label("lead への通知が保留されています（2 件）");
    screen.get_by_label("Ready は brief revision 1 に対する報告です。その後 brief が更新されています（close を妨げません）");

    screen.get_by_label("保留中の通知を再配信").click();
    let sent = asked(&mut screen);
    let Command::Lanes(LanesCommand::ResumeLaneDeliveries(resume)) = command_of(&sent[0]) else {
        panic!("a resume was expected");
    };
    assert_eq!(resume.lane_id.as_str(), "l1");
    assert_eq!(resume.lead_thread_id.as_str(), "lead");
    assert_eq!(resume.expected_lead_root_run_id.as_str(), "run-lead");
    assert_eq!(resume.expected_management_revision, Revision(3));
}

#[test]
fn an_answer_for_another_lead_or_lane_is_dropped_and_events_of_other_leads_are_ignored() {
    let mut screen = screen(Box::<LanesFeature>::default(), Some("lead"));
    let first = asked(&mut screen);
    let list_tag = tag_of(&first, is_list);
    screen.state_mut().selection.thread = Some(ThreadId::new("other").unwrap());
    let other = asked(&mut screen);
    assert!(!other.is_empty());
    answer(
        &mut screen,
        list_tag,
        lanes(vec![summary("l1", "attached", 3)]),
    );
    assert!(screen.query_by_label("objective l1").is_none());

    let event = |lead: &str, lane: &str| {
        UiEvent::Lanes(LanesEvent::LaneChanged {
            lane: serde_json::from_value(
                json!({"lane_id": lane, "thread_id": "x", "lead_thread_id": lead,
                    "policy": "attached", "management_revision": "9", "brief_revision": "1",
                    "objective_preview": "o", "generation": "0", "start": "not_started",
                    "last_activity_at": "1", "revision": "1"}),
            )
            .unwrap(),
        })
    };
    screen.state_mut().feature.event(&event("lead", "l9"));
    assert!(asked(&mut screen).is_empty());
    screen.state_mut().feature.event(&event("other", "l9"));
    assert!(
        asked(&mut screen)
            .iter()
            .any(|a| matches!(&a.request, FeatureRequest::Query(q) if is_list(q)))
    );
}
