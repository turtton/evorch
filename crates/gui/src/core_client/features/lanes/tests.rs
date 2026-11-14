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

/// A page of lanes with a following page behind `cursor`.
fn lanes_then(items: Vec<Value>, cursor: &str) -> FeatureReply {
    query_reply(
        json!({"type": "lanes", "payload": {"type": "lanes", "payload": {
        "items": items, "next_cursor": cursor, "snapshot_revision": "1"}}}),
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
    matches!(query, Query::Lanes(LanesQuery::ListLanes(list)) if !list.detached)
}

fn is_detached_list(query: &Query) -> bool {
    matches!(query, Query::Lanes(LanesQuery::ListLanes(list)) if list.detached)
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
    answer(&mut screen, tag_of(&first, is_detached_list), lanes(vec![]));
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
    // The detached lane is offered because the core lists it, not because this window
    // remembers detaching it.
    answer(
        &mut screen,
        tag_of(&refreshed, is_detached_list),
        lanes(vec![summary("l1", "detached", 4)]),
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
fn a_detached_lane_is_found_again_after_switching_threads_and_after_a_reconnect() {
    let mut screen = screen(Box::<LanesFeature>::default(), Some("lead"));
    let first = asked(&mut screen);
    answer(&mut screen, tag_of(&first, is_list), lanes(vec![]));
    answer(
        &mut screen,
        tag_of(&first, is_detached_list),
        lanes(vec![summary("l1", "detached", 4)]),
    );
    screen.get_by_label("切り離し済み: objective l1");

    // Another thread shows none of it, and the lead shows it again once it is selected.
    screen.state_mut().selection.thread = Some(ThreadId::new("other").unwrap());
    let other = asked(&mut screen);
    assert!(
        other
            .iter()
            .any(|a| matches!(&a.request, FeatureRequest::Query(q) if is_detached_list(q)))
    );
    assert!(
        screen
            .query_by_label("切り離し済み: objective l1")
            .is_none()
    );
    screen.state_mut().selection.thread = Some(ThreadId::new("lead").unwrap());
    let back = asked(&mut screen);
    answer(&mut screen, tag_of(&back, is_list), lanes(vec![]));
    answer(
        &mut screen,
        tag_of(&back, is_detached_list),
        lanes(vec![summary("l1", "detached", 4)]),
    );
    screen.get_by_label("切り離し済み: objective l1");

    // A new connection starts from nothing and asks the core again.
    screen.state_mut().feature.reset();
    let reconnected = asked(&mut screen);
    assert!(
        screen
            .query_by_label("切り離し済み: objective l1")
            .is_none()
    );
    answer(&mut screen, tag_of(&reconnected, is_list), lanes(vec![]));
    answer(
        &mut screen,
        tag_of(&reconnected, is_detached_list),
        lanes(vec![summary("l1", "detached", 4)]),
    );
    screen.get_by_label("切り離し済み: objective l1");
}

#[test]
fn detached_lanes_past_the_first_page_are_loaded_with_the_cursor_and_can_be_reattached() {
    let mut screen = screen(Box::<LanesFeature>::default(), Some("lead"));
    let first = asked(&mut screen);
    answer(
        &mut screen,
        tag_of(&first, is_roots),
        roots(vec!["run-lead"]),
    );
    answer(&mut screen, tag_of(&first, is_list), lanes(vec![]));
    answer(
        &mut screen,
        tag_of(&first, is_detached_list),
        lanes_then(vec![summary("l1", "detached", 4)], "page-2"),
    );
    screen.get_by_label("切り離し済み: objective l1");
    assert!(
        screen
            .query_by_label("切り離し済み: objective l51")
            .is_none()
    );

    // The next page is asked for with the cursor the core returned.
    screen.get_by_label("切り離し済みをさらに読み込む").click();
    let more = asked(&mut screen);
    let tag = tag_of(&more, is_detached_list);
    let cursor = more
        .iter()
        .find_map(|a| match &a.request {
            FeatureRequest::Query(Query::Lanes(LanesQuery::ListLanes(list))) if list.detached => {
                Some(list.page.cursor.clone())
            }
            _ => None,
        })
        .expect("a detached list was asked for");
    assert_eq!(
        cursor.map(|c| c.as_str().to_owned()).as_deref(),
        Some("page-2")
    );
    answer(&mut screen, tag, lanes(vec![summary("l51", "detached", 9)]));
    // The first page stays and the lane behind the page boundary is shown and reattachable.
    screen.get_by_label("切り離し済み: objective l1");
    screen.get_by_label("切り離し済み: objective l51");
    assert!(
        screen
            .query_by_label("切り離し済みをさらに読み込む")
            .is_none()
    );
    screen.get_by_label_contains("objective l51");
    let reattach_l51 = screen
        .get_all_by_label("再接続")
        .nth(1)
        .expect("a reattach button for the second lane");
    reattach_l51.click();
    let asked_for = asked(&mut screen);
    let manage_lane = asked_for
        .iter()
        .find_map(|a| match &a.request {
            FeatureRequest::Query(Query::Lanes(LanesQuery::GetLaneManagement { lane_id })) => {
                Some(lane_id.as_str().to_owned())
            }
            _ => None,
        })
        .expect("the management state of the chosen lane is asked for");
    assert_eq!(manage_lane, "l51");
}

#[test]
fn a_reattach_to_another_lead_from_another_client_refreshes_the_detached_rows_and_cursor() {
    let moved_to_b = |also_for: Vec<&str>| {
        UiEvent::Lanes(LanesEvent::LaneChanged {
            lane: serde_json::from_value(
                json!({"lane_id": "l1", "thread_id": "thread-l1", "lead_thread_id": "lead-b",
                    "policy": "attached", "management_revision": "9", "brief_revision": "2",
                    "objective_preview": "objective l1", "generation": "1",
                    "start": "started", "last_activity_at": "1", "revision": "1"}),
            )
            .unwrap(),
            also_for: also_for
                .into_iter()
                .map(|t| ThreadId::new(t).unwrap())
                .collect(),
        })
    };
    // Whether the event names this thread, or only the lane it already shows, the rows and
    // the page cursor are read again.
    for also_for in [vec!["lead"], vec![]] {
        let mut screen = screen(Box::<LanesFeature>::default(), Some("lead"));
        let first = asked(&mut screen);
        answer(&mut screen, tag_of(&first, is_list), lanes(vec![]));
        answer(
            &mut screen,
            tag_of(&first, is_detached_list),
            lanes_then(vec![summary("l1", "detached", 4)], "page-2"),
        );
        screen.get_by_label("切り離し済み: objective l1");

        screen.state_mut().feature.event(&moved_to_b(also_for));
        let again = asked(&mut screen);
        let tag = tag_of(&again, is_detached_list);
        answer(
            &mut screen,
            tag,
            lanes_then(vec![summary("l2", "detached", 5)], "page-9"),
        );
        assert!(
            screen
                .query_by_label("切り離し済み: objective l1")
                .is_none(),
            "the lane that moved to another lead is gone"
        );
        screen.get_by_label("切り離し済み: objective l2");
        screen.get_by_label("切り離し済みをさらに読み込む").click();
        let more = asked(&mut screen);
        let cursor = more
            .iter()
            .find_map(|a| match &a.request {
                FeatureRequest::Query(Query::Lanes(LanesQuery::ListLanes(list)))
                    if list.detached =>
                {
                    Some(list.page.cursor.clone())
                }
                _ => None,
            })
            .expect("a detached list was asked for");
        assert_eq!(
            cursor.map(|c| c.as_str().to_owned()).as_deref(),
            Some("page-9"),
            "the cursor is the one of the page read after the move"
        );
    }
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
            also_for: vec![],
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
