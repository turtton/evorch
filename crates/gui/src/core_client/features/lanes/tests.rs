//! The lanes feature drawn headless: lanes are read from the ledger, every
//! operation names the revision and the lead's root run the screen showed, and
//! an answer for a lane or lead no longer shown is dropped.
use super::*;
use crate::core_client::features::execution::tests::{
    answer, asked, command_of, ok_command, query_of, query_reply, screen,
};
use crate::core_client::features::{FeatureAction, FeatureRequest};
use crate::panes::lane_widgets::STALE_NOTICE;
use egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
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

    screen
        .get_by_label("Report completion only (summary)")
        .click();
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
    screen.get_by_label("Lane policy: stale_management_revision");
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
    screen.get_by_label("Detach lane").click();
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

    screen.get_by_label("Reattach").click();
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
    screen.get_by_label("Detached: objective l1");

    // Another thread shows none of it, and the lead shows it again once it is selected.
    screen.state_mut().selection.thread = Some(ThreadId::new("other").unwrap());
    let other = asked(&mut screen);
    assert!(
        other
            .iter()
            .any(|a| matches!(&a.request, FeatureRequest::Query(q) if is_detached_list(q)))
    );
    assert!(screen.query_by_label("Detached: objective l1").is_none());
    screen.state_mut().selection.thread = Some(ThreadId::new("lead").unwrap());
    let back = asked(&mut screen);
    answer(&mut screen, tag_of(&back, is_list), lanes(vec![]));
    answer(
        &mut screen,
        tag_of(&back, is_detached_list),
        lanes(vec![summary("l1", "detached", 4)]),
    );
    screen.get_by_label("Detached: objective l1");

    // A new connection starts from nothing and asks the core again.
    screen.state_mut().feature.reset();
    let reconnected = asked(&mut screen);
    assert!(screen.query_by_label("Detached: objective l1").is_none());
    answer(&mut screen, tag_of(&reconnected, is_list), lanes(vec![]));
    answer(
        &mut screen,
        tag_of(&reconnected, is_detached_list),
        lanes(vec![summary("l1", "detached", 4)]),
    );
    screen.get_by_label("Detached: objective l1");
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
    screen.get_by_label("Detached: objective l1");
    assert!(screen.query_by_label("Detached: objective l51").is_none());

    // The next page is asked for with the cursor the core returned.
    screen.get_by_label("Load more detached lanes").click();
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
    screen.get_by_label("Detached: objective l1");
    screen.get_by_label("Detached: objective l51");
    assert!(screen.query_by_label("Load more detached lanes").is_none());
    screen.get_by_label_contains("objective l51");
    let reattach_l51 = screen
        .get_all_by_label("Reattach")
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
        screen.get_by_label("Detached: objective l1");

        screen.state_mut().feature.event(&moved_to_b(also_for));
        let again = asked(&mut screen);
        let tag = tag_of(&again, is_detached_list);
        answer(
            &mut screen,
            tag,
            lanes_then(vec![summary("l2", "detached", 5)], "page-9"),
        );
        assert!(
            screen.query_by_label("Detached: objective l1").is_none(),
            "the lane that moved to another lead is gone"
        );
        screen.get_by_label("Detached: objective l2");
        screen.get_by_label("Load more detached lanes").click();
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
    screen
        .get_by_label("The lead was stopped: 2 notice(s) are held, not lost, and it is not woken.");
    screen.get_by_label(
        "The Ready answers brief revision 1, and the brief was revised after it. It does not block a close.",
    );

    screen.get_by_label("Resume notifications").click();
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

fn preview(management: u64, request: u64, ready_brief: u64) -> FeatureReply {
    query_reply(
        json!({"type": "lanes", "payload": {"type": "lane_close_preview", "payload": {
        "lane_id": "l1", "thread_id": "thread-l1", "objective": "the whole objective",
        "acceptance_criteria": ["it builds"], "brief_revision": "2",
        "management_revision": management.to_string(), "request_revision": request.to_string(),
        "ready": {"report_seq": "4", "brief_revision": ready_brief.to_string(),
            "summary": "all done", "artifact_refs": ["https://example.test/pull/1"],
            "artifact_ref_count": 3},
        "open_question_count": 2, "running": true}}}),
    )
}

fn closed(kind: &str, already: bool) -> FeatureReply {
    FeatureReply::Command(Ok(serde_json::from_value(
        json!({"disposition": "accepted", "result": {"type": "lanes", "payload": {
            "type": "closed", "payload": {"lane_id": "l1", "close_id": "close-1", "kind": kind,
            "ready_seq": "4", "management_revision": "7", "request_revision": "9",
            "already_closed": already}}}}),
    )
    .expect("a close reply")))
}

fn close_of(action: &FeatureAction) -> &workbench_protocol::lane_management::CloseLane {
    match command_of(action) {
        Command::Lanes(LanesCommand::CloseLane(close)) => close,
        other => panic!("{other:?}"),
    }
}

fn is_preview(query: &Query) -> bool {
    matches!(query, Query::Lanes(LanesQuery::GetLaneClosePreview { .. }))
}

const DIALOG_WINDOW: egui::Vec2 = egui::vec2(1024.0, 768.0);

fn type_reason(
    screen: &mut egui_kittest::Harness<
        'static,
        crate::core_client::features::execution::tests::Screen,
    >,
    text: &str,
) {
    screen.get_by_role(Role::TextInput).focus();
    screen.run_steps(4);
    screen.get_by_role(Role::TextInput).type_text(text);
    screen.run_steps(4);
}

fn disabled(
    screen: &egui_kittest::Harness<'static, crate::core_client::features::execution::tests::Screen>,
    label: &str,
) -> bool {
    screen.get_by_label(label).accesskit_node().is_disabled()
}

/// A screen with lane `l1` listed and its close dialog open on the preview `reply`.
fn with_close_dialog(
    button: &str,
    reply: FeatureReply,
) -> egui_kittest::Harness<'static, crate::core_client::features::execution::tests::Screen> {
    let mut screen = screen(Box::<LanesFeature>::default(), Some("lead"));
    screen.set_size(DIALOG_WINDOW);
    let first = asked(&mut screen);
    answer(
        &mut screen,
        tag_of(&first, is_list),
        lanes(vec![summary("l1", "attached", 3)]),
    );
    screen.get_by_label(button).click();
    let read = asked(&mut screen);
    let tag = tag_of(&read, is_preview);
    answer(&mut screen, tag, reply);
    screen
}

#[test]
fn a_close_reads_the_preview_and_sends_the_revisions_it_named_once() {
    let mut screen = screen(Box::<LanesFeature>::default(), Some("lead"));
    screen.set_size(DIALOG_WINDOW);
    let first = asked(&mut screen);
    answer(
        &mut screen,
        tag_of(&first, is_list),
        lanes(vec![summary("l1", "attached", 3)]),
    );
    screen.get_by_label("Close by human decision…").click();
    let read = asked(&mut screen);
    let Query::Lanes(LanesQuery::GetLaneClosePreview { lane_id }) = query_of(&read[0]) else {
        panic!("the close preview was expected");
    };
    assert_eq!(lane_id.as_str(), "l1");
    // Until the preview is read nothing can be confirmed.
    assert!(disabled(&screen, "Close lane"));
    // The Ready was for brief revision 1; the lane is at 2.
    answer(&mut screen, read[0].tag, preview(7, 9, 1));

    screen.get_by_label(close::MEANING);
    screen.get_by_label(close::REMAINS);
    screen.get_by_label(close::RUNNING_WARNING);
    screen.get_by_label(close::OLDER_BRIEF_WARNING);
    screen.get_by_label("2 questions from this lane are still unanswered. They stay open.");
    screen.get_by_label("Latest Ready (#4), for an earlier brief");
    screen.get_by_label("Evidence: 3 reference(s), no fixed artifact revision.");
    // The references are folded, the listed one and how many more are behind the header.
    screen.get_by_label("Artifact references (3)").hover();
    screen.run_steps(2);
    screen.get_by_label("Artifact references (3)").click();
    screen.run_steps(2);
    screen.get_by_label("https://example.test/pull/1");
    screen.get_by_label("… and 2 more");
    // The core sends no review state or pull request facts: no line claims either.
    assert!(screen.query_by_label_contains("Review:").is_none());
    assert!(screen.query_by_label_contains("Pull request:").is_none());

    // A reason is required before anything can be sent.
    assert!(disabled(&screen, "Close lane"));
    type_reason(&mut screen, "  done by hand  ");
    assert!(!disabled(&screen, "Close lane"));
    screen.get_by_label("Close lane").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let close = close_of(&sent[0]);
    assert_eq!(close.lane_id.as_str(), "l1");
    assert_eq!(
        close.kind,
        workbench_protocol::lane_management::HumanLaneCloseKind::Complete
    );
    // The revisions are the preview's (7 and 9), not the listed row's (3).
    assert_eq!(close.expected_management_revision, Revision(7));
    assert_eq!(close.expected_request_revision, Revision(9));
    assert_eq!(close.ready_seq.map(|seq| seq.0), Some(4));
    assert_eq!(close.note, "done by hand");

    // While the request is out, confirming again sends nothing.
    assert!(disabled(&screen, "Close lane"));
    screen.get_by_label("Close lane").click();
    assert!(asked(&mut screen).is_empty());

    // The answer names the record's kind, and the lane is read again.
    answer(&mut screen, sent[0].tag, closed("human_closed", false));
    screen.get_by_label(
        "Lane ended: completed by human decision (unverified). No run was stopped and nothing was deleted.",
    );
    assert!(screen.query_by_label("Close lane (unverified)").is_none());
    assert!(
        asked(&mut screen)
            .iter()
            .any(|a| matches!(&a.request, FeatureRequest::Query(q) if is_list(q)))
    );
}

#[test]
fn a_discard_is_sent_as_a_discard_and_an_already_closed_answer_says_so() {
    let mut screen = with_close_dialog("Discard…", preview(3, 5, 2));
    screen.get_by_role_and_label(Role::Button, "Discard lane");
    // No claim of completion: the Ready warning belongs to closing as done.
    assert!(screen.query_by_label(close::NO_READY_WARNING).is_none());
    assert!(screen.query_by_label(close::OLDER_BRIEF_WARNING).is_none());
    type_reason(&mut screen, "not needed");
    screen
        .get_by_role_and_label(Role::Button, "Discard lane")
        .click();
    let sent = asked(&mut screen);
    assert_eq!(
        close_of(&sent[0]).kind,
        workbench_protocol::lane_management::HumanLaneCloseKind::Discard
    );
    answer(&mut screen, sent[0].tag, closed("discarded", true));
    screen.get_by_label("The lane was already ended (discarded); nothing was recorded.");
}

#[test]
fn a_stale_refusal_is_shown_and_only_a_reload_makes_a_new_screen_the_one_to_confirm() {
    let mut screen = with_close_dialog("Close by human decision…", preview(3, 5, 2));
    type_reason(&mut screen, "done");
    screen.get_by_label("Close lane").click();
    let sent = asked(&mut screen);
    let mut refusal = FeatureError::new("stale_request");
    refusal.code = Some(workbench_protocol::errors::ErrorCode::StaleRevision);
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(refusal)),
    );
    screen.get_by_label(STALE_NOTICE);
    // Nothing is resent by itself, and the old screen cannot be confirmed again.
    let after = asked(&mut screen);
    assert!(
        !after
            .iter()
            .any(|a| matches!(a.request, FeatureRequest::Command(_)))
    );
    assert!(disabled(&screen, "Close lane"));

    screen.get_by_label("Reload").click();
    let reread = asked(&mut screen);
    answer(&mut screen, tag_of(&reread, is_preview), preview(4, 6, 2));
    assert!(!disabled(&screen, "Close lane"));
    screen.run_steps(4);
    screen.get_by_label("Close lane").click();
    let again = asked(&mut screen);
    let close = close_of(&again[0]);
    assert_eq!(close.expected_management_revision, Revision(4));
    assert_eq!(close.expected_request_revision, Revision(6));
}

#[test]
fn a_lost_answer_is_not_a_success_and_the_ledger_is_read_again() {
    let mut screen = with_close_dialog("Close by human decision…", preview(3, 5, 2));
    type_reason(&mut screen, "done");
    screen.get_by_label("Close lane").click();
    let sent = asked(&mut screen);
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    screen.get_by_label(
        "The outcome is unknown. It is not taken as recorded; the ledger is read again.",
    );
    let reread = asked(&mut screen);
    assert!(
        reread
            .iter()
            .any(|a| matches!(&a.request, FeatureRequest::Query(q) if is_preview(q)))
    );
    assert!(
        !reread
            .iter()
            .any(|a| matches!(a.request, FeatureRequest::Command(_)))
    );
    answer(&mut screen, tag_of(&reread, is_preview), preview(3, 5, 2));
    // The first request may still resolve: a second one is not offered meanwhile.
    assert!(disabled(&screen, "Close lane"));
    // The same tag later answers with what the core resolved.
    answer(&mut screen, sent[0].tag, closed("human_closed", false));
    assert!(screen.query_by_label("Close lane (unverified)").is_none());
}

#[test]
fn cancel_sends_nothing_and_a_reason_over_two_kib_cannot_be_sent() {
    let mut screen = with_close_dialog("Close by human decision…", preview(3, 5, 2));
    type_reason(&mut screen, &"x".repeat(2049));
    screen.get_by_label("The reason is too long.");
    assert!(disabled(&screen, "Close lane"));
    screen.get_by_label("Cancel").click();
    let after = asked(&mut screen);
    assert!(
        !after
            .iter()
            .any(|a| matches!(a.request, FeatureRequest::Command(_)))
    );
    assert!(screen.query_by_label("Close lane (unverified)").is_none());
}

#[test]
fn a_standing_close_is_a_badge_not_something_to_record_again_and_detached_lanes_have_no_button() {
    let mut screen = screen(Box::<LanesFeature>::default(), Some("lead"));
    let first = asked(&mut screen);
    let mut closed_lane = summary("l1", "attached", 3);
    closed_lane["current_close"] = json!({"close_id": "close-1", "kind": "human_closed"});
    answer(
        &mut screen,
        tag_of(&first, is_list),
        lanes(vec![closed_lane]),
    );
    answer(
        &mut screen,
        tag_of(&first, is_detached_list),
        lanes(vec![summary("l2", "detached", 4)]),
    );
    screen.get_by_label("Lane close: closed (unverified)");
    // The one row with a button is the managed lane; the detached lane has none.
    assert_eq!(
        screen.get_all_by_label("Close by human decision…").count(),
        1
    );
    assert_eq!(screen.get_all_by_label("Discard…").count(), 1);

    screen.get_by_label("Discard…").click();
    let read = asked(&mut screen);
    let mut standing = json!({"close_id": "close-1", "kind": "human_closed"});
    standing["ready_seq"] = json!("4");
    let reply = query_reply(
        json!({"type": "lanes", "payload": {"type": "lane_close_preview",
        "payload": {"lane_id": "l1", "thread_id": "thread-l1", "objective": "o",
        "acceptance_criteria": [], "brief_revision": "2", "management_revision": "3",
        "request_revision": "5", "open_question_count": 0, "running": false,
        "current_close": standing}}}),
    );
    answer(&mut screen, tag_of(&read, is_preview), reply);
    screen.get_by_label(
        "This lane is already ended (completed by human decision (unverified)). Nothing more to record.",
    );
    type_reason(&mut screen, "again");
    assert!(
        screen
            .get_by_role_and_label(Role::Button, "Discard lane")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn a_lane_that_moved_while_the_dialog_is_open_must_be_reloaded_before_confirming() {
    let mut screen = with_close_dialog("Close by human decision…", preview(3, 5, 2));
    type_reason(&mut screen, "done");
    assert!(!disabled(&screen, "Close lane"));
    let moved = UiEvent::Lanes(LanesEvent::LaneChanged {
        lane: serde_json::from_value(summary("l1", "attached", 4)).unwrap(),
        also_for: vec![],
    });
    screen.state_mut().feature.event(&moved);
    screen.run_steps(2);
    screen.get_by_label(crate::panes::lane_widgets::STALE_BLOCKER);
    assert!(disabled(&screen, "Close lane"));
}

fn open_close_dialog(
    screen: &mut egui_kittest::Harness<
        'static,
        crate::core_client::features::execution::tests::Screen,
    >,
) -> u64 {
    screen.get_by_label("Close by human decision…").click();
    let read = asked(screen);
    tag_of(&read, is_preview)
}

#[test]
fn a_late_answer_to_a_close_that_was_sent_never_acts_on_a_later_confirmation() {
    let mut screen = with_close_dialog("Close by human decision…", preview(3, 5, 2));
    type_reason(&mut screen, "done");
    screen.get_by_label("Close lane").click();
    let sent = asked(&mut screen);
    let first = sent[0].tag;
    // The person gives up waiting, and opens the confirmation of the same lane again.
    screen.get_by_label("Cancel").click();
    let again = open_close_dialog(&mut screen);
    answer(&mut screen, again, preview(4, 6, 2));

    // The old close is refused late: the new confirmation shows none of it.
    let mut refusal = FeatureError::new("stale_request");
    refusal.code = Some(workbench_protocol::errors::ErrorCode::StaleRevision);
    answer(&mut screen, first, FeatureReply::Command(Err(refusal)));
    assert!(
        screen
            .query_by_label_contains("nothing was recorded")
            .is_none()
    );
    type_reason(&mut screen, "second");
    assert!(!disabled(&screen, "Close lane"));

    // Had it been recorded instead, the new confirmation is still the one on screen.
    screen.get_by_label("Close lane").click();
    let resent = asked(&mut screen);
    let second = resent
        .iter()
        .find(|a| matches!(a.request, FeatureRequest::Command(_)))
        .expect("the new confirmation is sent");
    assert_eq!(close_of(second).expected_request_revision, Revision(6));
    answer(&mut screen, second.tag, closed("human_closed", false));
    assert!(screen.query_by_label("Close lane (unverified)").is_none());
}

#[test]
fn a_late_close_answer_does_not_finish_the_confirmation_opened_after_it() {
    let mut screen = with_close_dialog("Close by human decision…", preview(3, 5, 2));
    type_reason(&mut screen, "done");
    screen.get_by_label("Close lane").click();
    let first = asked(&mut screen)[0].tag;
    screen.get_by_label("Cancel").click();
    let again = open_close_dialog(&mut screen);
    answer(&mut screen, again, preview(4, 6, 2));
    // The first close was recorded after all: its answer reports it, and the open
    // confirmation stays for the person to decide.
    answer(&mut screen, first, closed("human_closed", false));
    screen.get_by_label("Close lane (unverified)");
}

#[test]
fn a_late_preview_of_an_earlier_opening_is_not_what_a_reopened_dialog_confirms() {
    let mut screen = screen(Box::<LanesFeature>::default(), Some("lead"));
    let first = asked(&mut screen);
    answer(
        &mut screen,
        tag_of(&first, is_list),
        lanes(vec![summary("l1", "attached", 3)]),
    );
    let old_read = open_close_dialog(&mut screen);
    screen.get_by_label("Cancel").click();
    let new_read = open_close_dialog(&mut screen);
    answer(&mut screen, old_read, preview(3, 5, 2));
    // Still waiting for its own reading: nothing can be confirmed yet.
    assert!(
        screen
            .query_by_label_contains("Reading the lane from the ledger")
            .is_some()
    );
    answer(&mut screen, new_read, preview(4, 6, 2));
    type_reason(&mut screen, "done");
    screen.get_by_label("Close lane").click();
    let sent = asked(&mut screen);
    let close = close_of(&sent[0]);
    assert_eq!(close.expected_management_revision, Revision(4));
    assert_eq!(close.expected_request_revision, Revision(6));
}

/// The feature as the dock keeps it while its tab may be hidden: maintained every pass, and
/// drawn only while `visible`.
struct Docked {
    feature: LanesFeature,
    selection: crate::core_client::features::Selection,
    cache: crate::core_client::cache::Cache,
    frame: crate::core_client::features::FeatureFrame,
    visible: bool,
}

fn docked(
    thread: Option<&str>,
    lane_of: Option<(&str, &str)>,
) -> egui_kittest::Harness<'static, Docked> {
    let mut cache = crate::core_client::cache::Cache::default();
    if let Some((lane_thread, lead)) = lane_of {
        let summary: workbench_protocol::views::ThreadSummary = serde_json::from_value(json!({
            "id": lane_thread, "project": "p", "title": "the lane thread", "state": "idle",
            "lead_thread": lead, "revision": "1", "updated_at": "1"}))
        .unwrap();
        cache.threads.insert(lane_thread.to_owned(), summary);
    }
    let state = Docked {
        feature: LanesFeature::default(),
        selection: crate::core_client::features::Selection {
            project: Some(workbench_protocol::ids::ProjectId::new("p").unwrap()),
            thread: thread.map(|id| ThreadId::new(id).unwrap()),
        },
        cache,
        frame: crate::core_client::features::FeatureFrame::default(),
        visible: false,
    };
    egui_kittest::Harness::new_ui_state(
        |ui, screen: &mut Docked| {
            let capabilities = [Capability::Queries, Capability::Commands];
            let view = crate::core_client::features::FeatureView {
                connected: true,
                capabilities: &capabilities,
                cache: &screen.cache,
                selection: &screen.selection,
            };
            let mut ctx = FeatureContext {
                data: &mut crate::core_client::features::data::DataInbox::default(),
                feature: FeatureId::Lanes,
                view,
                frame: &mut screen.frame,
            };
            let visibility = Visibility {
                available: true,
                expanded: screen.visible,
                minimized: false,
            };
            screen.feature.maintain(&mut ctx, visibility);
            if screen.visible {
                screen.feature.render(ui, &mut ctx);
            }
        },
        state,
    )
}

fn docked_asked(harness: &mut egui_kittest::Harness<'_, Docked>) -> Vec<FeatureAction> {
    harness.run_steps(2);
    std::mem::take(&mut harness.state_mut().frame.actions)
}

fn docked_answer(harness: &mut egui_kittest::Harness<'_, Docked>, tag: u64, reply: FeatureReply) {
    harness.state_mut().feature.reply(tag, reply);
    harness.run_steps(2);
}

fn lane_on_thread(id: &str, thread: &str, revision: u64) -> Value {
    let mut lane = summary(id, "attached", revision);
    lane["thread_id"] = json!(thread);
    lane
}

fn lookups_of(actions: &[FeatureAction]) -> Vec<(String, Option<String>)> {
    actions
        .iter()
        .filter_map(|a| match &a.request {
            FeatureRequest::Query(Query::Lanes(LanesQuery::ListLanes(list))) if !list.detached => {
                Some((
                    list.lead_thread.as_str().to_owned(),
                    list.page.cursor.as_ref().map(|c| c.as_str().to_owned()),
                ))
            }
            _ => None,
        })
        .collect()
}

// Given: the selected thread is a lane of `lead`, and the Lanes tab is hidden
// When: the passes run
// Then: the card's lane is found among the lead's lanes and its detail read, the tab's own
//       lists are not, and a resume pressed on the card names the lane's revision and the
//       lead's current root run
#[test]
fn a_lane_threads_card_is_read_while_the_tab_is_hidden_and_resumes_for_the_lead_root() {
    let mut screen = docked(Some("thread-l1"), Some(("thread-l1", "lead")));
    let first = docked_asked(&mut screen);
    assert_eq!(lookups_of(&first), [("lead".to_owned(), None)]);
    assert!(
        !first
            .iter()
            .any(|a| matches!(&a.request, FeatureRequest::Query(q) if is_detached_list(q))),
        "the hidden tab reads none of its lists"
    );
    assert!(screen.state().feature.card().is_none());
    docked_answer(
        &mut screen,
        tag_of(&first, is_roots),
        roots(vec!["run-lead"]),
    );
    docked_answer(
        &mut screen,
        tag_of(&first, is_list),
        lanes(vec![lane_on_thread("l1", "thread-l1", 3)]),
    );
    let read = docked_asked(&mut screen);
    let Query::Lanes(LanesQuery::GetLane { lane_id }) = query_of(&read[0]) else {
        panic!("the lane's detail was expected");
    };
    assert_eq!(lane_id.as_str(), "l1");
    docked_answer(&mut screen, read[0].tag, detail("l1", true, 3));
    {
        let (thread, card) = screen.state().feature.card().expect("the card");
        assert_eq!(thread.as_str(), "thread-l1");
        assert_eq!(card.lead.as_str(), "lead");
        assert!(card.can_resume);
        assert!(card.detail.is_some_and(|detail| detail.delivery.held));
    }

    screen
        .state_mut()
        .feature
        .enqueue_card(LaneCardAction::ResumeNotifications {
            lane: LaneId::new("l1").unwrap(),
        });
    let sent = docked_asked(&mut screen);
    let Command::Lanes(LanesCommand::ResumeLaneDeliveries(resume)) = command_of(&sent[0]) else {
        panic!("a resume was expected");
    };
    assert_eq!(resume.lead_thread_id.as_str(), "lead");
    assert_eq!(resume.expected_lead_root_run_id.as_str(), "run-lead");
    assert_eq!(resume.expected_management_revision, Revision(3));
    docked_answer(&mut screen, sent[0].tag, ok_command());
    assert_eq!(
        screen.state().feature.card().unwrap().1.status,
        "Resume notifications: accepted"
    );
}

// Given: the selected thread is a lane of `lead`, with the Lanes tab hidden
// When: the core refuses the card's read of the lead's root runs
// Then: the card shows the refusal, the tab's status stays clear, and the card offers no
//       resume without a root run
#[test]
fn a_refused_card_lead_root_read_is_shown_on_the_card_not_the_tab() {
    let mut screen = docked(Some("thread-l1"), Some(("thread-l1", "lead")));
    let first = docked_asked(&mut screen);
    docked_answer(
        &mut screen,
        tag_of(&first, is_list),
        lanes(vec![lane_on_thread("l1", "thread-l1", 3)]),
    );
    docked_answer(
        &mut screen,
        tag_of(&first, is_roots),
        FeatureReply::Query(Err(FeatureError::new("busy"))),
    );
    let feature = &screen.state().feature;
    assert_eq!(feature.status, "");
    let (_, card) = feature.card().expect("the card");
    assert_eq!(card.status, "busy");
    assert!(!card.can_resume);
}

// Given: a thread the core names no lead for, with the Lanes tab hidden
// When: the passes run
// Then: nothing is read for it
#[test]
fn a_thread_that_is_no_lane_reads_nothing_while_the_tab_is_hidden() {
    let mut screen = docked(Some("lead"), None);
    assert!(docked_asked(&mut screen).is_empty());
    assert!(screen.state().feature.card().is_none());
}

// Given: a lane thread's menu asking for its lane twice, and the lane on the lead's second
//        page
// When: the pages are answered, a detach is chosen and another client then detaches it
// Then: one read per page is made, the detach names the listed revision, and the event
//       takes the lane's operations off the menu
#[test]
fn a_menu_lookup_is_asked_once_pages_to_the_lane_and_follows_its_events() {
    let mut screen = docked(None, None);
    let thread = ThreadId::new("thread-l9").unwrap();
    let lead = ThreadId::new("lead").unwrap();
    for _ in 0..2 {
        screen
            .state_mut()
            .feature
            .enqueue_sidebar(LaneIntent::Lookup {
                thread: thread.clone(),
                lead: lead.clone(),
            });
    }
    let first = docked_asked(&mut screen);
    assert_eq!(lookups_of(&first), [("lead".to_owned(), None)]);
    assert_eq!(screen.state().feature.menu().entry(&thread), None);
    screen
        .state_mut()
        .feature
        .enqueue_sidebar(LaneIntent::Lookup {
            thread: thread.clone(),
            lead: lead.clone(),
        });
    docked_answer(
        &mut screen,
        first[0].tag,
        lanes_then(vec![lane_on_thread("l1", "thread-l1", 3)], "page-2"),
    );
    let second = docked_asked(&mut screen);
    assert_eq!(
        lookups_of(&second),
        [("lead".to_owned(), Some("page-2".to_owned()))]
    );
    docked_answer(
        &mut screen,
        second[0].tag,
        lanes(vec![lane_on_thread("l9", "thread-l9", 7)]),
    );
    assert_eq!(
        screen.state().feature.menu().entry(&thread),
        Some(&LaneMenuEntry::Managed {
            lane: LaneId::new("l9").unwrap(),
            revision: Revision(7),
        })
    );

    screen
        .state_mut()
        .feature
        .enqueue_sidebar(LaneIntent::Manage {
            lane: LaneId::new("l9").unwrap(),
            revision: Revision(7),
            action: LaneManagementAction::Detach,
        });
    let sent = docked_asked(&mut screen);
    let manage = manage_of(&sent[0]);
    assert_eq!(manage.request.lane_id.as_str(), "l9");
    assert_eq!(manage.request.expected_management_revision, Revision(7));
    assert!(matches!(
        manage.request.action,
        LaneManagementAction::Detach
    ));

    let mut detached = lane_on_thread("l9", "thread-l9", 8);
    detached["policy"] = json!("detached");
    detached.as_object_mut().unwrap().remove("lead_thread_id");
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Lanes(LanesEvent::LaneChanged {
            lane: serde_json::from_value(detached).unwrap(),
            also_for: vec![],
        }));
    assert_eq!(
        screen.state().feature.menu().entry(&thread),
        Some(&LaneMenuEntry::NotManaged)
    );

    // A lane the lead's last page does not hold is not offered, and not asked for again.
    let gone = ThreadId::new("thread-gone").unwrap();
    screen
        .state_mut()
        .feature
        .enqueue_sidebar(LaneIntent::Lookup {
            thread: gone.clone(),
            lead: lead.clone(),
        });
    let asked_gone = docked_asked(&mut screen);
    docked_answer(&mut screen, asked_gone[0].tag, lanes(vec![]));
    assert_eq!(
        screen.state().feature.menu().entry(&gone),
        Some(&LaneMenuEntry::NotManaged)
    );
    screen
        .state_mut()
        .feature
        .enqueue_sidebar(LaneIntent::Lookup { thread: gone, lead });
    assert!(docked_asked(&mut screen).is_empty());
}

// Given: a lane thread's menu looking up its lane, and the core refusing the read
// When: the menu asks again, then the user asks to read it again
// Then: the menu says the lane could not be read without asking on its own, and the retry
//       reads it and offers its operations
#[test]
fn a_refused_menu_lookup_is_shown_and_read_again_only_when_asked() {
    let mut screen = docked(None, None);
    let thread = ThreadId::new("thread-l9").unwrap();
    let lead = ThreadId::new("lead").unwrap();
    let lookup = LaneIntent::Lookup {
        thread: thread.clone(),
        lead: lead.clone(),
    };
    screen.state_mut().feature.enqueue_sidebar(lookup.clone());
    let first = docked_asked(&mut screen);
    docked_answer(
        &mut screen,
        first[0].tag,
        FeatureReply::Query(Err(FeatureError::new("busy"))),
    );
    assert_eq!(
        screen.state().feature.menu().entry(&thread),
        Some(&LaneMenuEntry::Unreadable)
    );
    screen.state_mut().feature.enqueue_sidebar(lookup);
    assert!(docked_asked(&mut screen).is_empty());

    screen
        .state_mut()
        .feature
        .enqueue_sidebar(LaneIntent::Retry {
            thread: thread.clone(),
            lead,
        });
    let retry = docked_asked(&mut screen);
    assert_eq!(lookups_of(&retry), [("lead".to_owned(), None)]);
    assert_eq!(screen.state().feature.menu().entry(&thread), None);
    docked_answer(
        &mut screen,
        retry[0].tag,
        lanes(vec![lane_on_thread("l9", "thread-l9", 7)]),
    );
    assert_eq!(
        screen.state().feature.menu().entry(&thread),
        Some(&LaneMenuEntry::Managed {
            lane: LaneId::new("l9").unwrap(),
            revision: Revision(7),
        })
    );
}

// Given: the Lanes tab hidden, and a detach chosen from a lane thread's menu
// When: the core refuses it as stale, then reports an unknown outcome for the next one,
//       and then accepts it
// Then: each refusal is shown with the menu, and the acceptance clears it
#[test]
fn a_menu_lane_operation_reports_its_refusal_with_the_menu() {
    let mut screen = docked(None, None);
    let detach = LaneIntent::Manage {
        lane: LaneId::new("l9").unwrap(),
        revision: Revision(7),
        action: LaneManagementAction::Detach,
    };
    screen.state_mut().feature.enqueue_sidebar(detach.clone());
    let sent = docked_asked(&mut screen);
    let mut refusal = FeatureError::new("stale_management_revision");
    refusal.code = Some(ErrorCode::StaleRevision);
    docked_answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(refusal)),
    );
    assert_eq!(
        screen.state().feature.menu().notice(),
        Some("Detach lane: stale_management_revision")
    );

    screen.state_mut().feature.enqueue_sidebar(detach);
    let sent = docked_asked(&mut screen);
    assert_eq!(screen.state().feature.menu().notice(), None);
    docked_answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    assert_eq!(
        screen.state().feature.menu().notice(),
        Some("Detach lane: outcome unknown, not resent — connection lost")
    );
    docked_answer(&mut screen, sent[0].tag, ok_command());
    assert_eq!(screen.state().feature.menu().notice(), None);
}

// Given: the Lanes tab shown for another thread
// When: a lane thread's menu chooses to discard its lane
// Then: the confirmation opens with the thread's title and reads the lane's preview
#[test]
fn a_close_chosen_from_a_menu_opens_the_confirmation_and_reads_the_preview() {
    let mut screen = docked(Some("other"), None);
    screen.state_mut().visible = true;
    docked_asked(&mut screen);
    screen
        .state_mut()
        .feature
        .enqueue_sidebar(LaneIntent::Close {
            lane: LaneId::new("l1").unwrap(),
            title: "the lane thread".into(),
            choice: CloseChoice::Discard,
        });
    let read = docked_asked(&mut screen);
    let tag = tag_of(&read, is_preview);
    screen.get_by_role_and_label(Role::Label, "Discard lane");
    screen.get_by_label("the lane thread");
    screen.get_by_label(crate::panes::lane_widgets::LOADING_BLOCKER);
    docked_answer(&mut screen, tag, preview(3, 5, 2));
    assert!(
        screen
            .query_by_label(crate::panes::lane_widgets::LOADING_BLOCKER)
            .is_none()
    );
    screen.get_by_label("Latest Ready (#4)");
}

/// A lane thread's lane as the core lists or announces it, with the close that stands.
fn lane_with_close(id: &str, thread: &str, close: Option<&str>) -> Value {
    let mut lane = lane_on_thread(id, thread, 3);
    if let Some(kind) = close {
        lane["current_close"] = json!({"close_id": "close-1", "kind": kind});
    }
    lane
}

fn lane_changed(lane: Value) -> UiEvent {
    UiEvent::Lanes(LanesEvent::LaneChanged {
        lane: serde_json::from_value(lane).unwrap(),
        also_for: vec![],
    })
}

// Given: a lane thread whose lookup was asked for by its row
// When: the row's ask is queued, sent, answered with a standing close, and then announced
//       with each kind of close and with none
// Then: the menu counts the thread as asked for from the moment it is queued, without a
//       close until the lane is read; afterwards it carries the close the ledger last
//       announced, and no more reads are made
#[test]
fn a_lane_threads_close_is_read_by_its_row_and_follows_the_lane_events() {
    use workbench_protocol::lane_management::LaneCloseRecordKind::{
        Discarded, HumanClosed, LeadClosed,
    };
    let mut screen = docked(None, None);
    let thread = ThreadId::new("thread-l9").unwrap();
    let lead = ThreadId::new("lead").unwrap();
    assert!(!screen.state().feature.menu().is_known(&thread));
    screen
        .state_mut()
        .feature
        .enqueue_sidebar(LaneIntent::Lookup {
            thread: thread.clone(),
            lead,
        });
    assert!(
        screen.state().feature.menu().is_known(&thread),
        "queued, not yet sent: the row must not ask again"
    );
    let first = docked_asked(&mut screen);
    assert_eq!(lookups_of(&first), [("lead".to_owned(), None)]);
    let menu = screen.state().feature.menu();
    assert!(menu.is_known(&thread), "sent, not yet answered");
    assert_eq!(menu.entry(&thread), None);
    assert_eq!(menu.current_close(&thread), None);

    docked_answer(
        &mut screen,
        first[0].tag,
        lanes(vec![lane_with_close(
            "l9",
            "thread-l9",
            Some("lead_closed"),
        )]),
    );
    assert_eq!(
        screen.state().feature.menu().current_close(&thread),
        Some(LeadClosed)
    );

    for (close, expected) in [
        (Some("human_closed"), Some(HumanClosed)),
        (Some("discarded"), Some(Discarded)),
        (None, None),
        (Some("lead_closed"), Some(LeadClosed)),
        (None, None),
    ] {
        screen
            .state_mut()
            .feature
            .event(&lane_changed(lane_with_close("l9", "thread-l9", close)));
        let menu = screen.state().feature.menu();
        assert_eq!(menu.current_close(&thread), expected);
        assert!(matches!(
            menu.entry(&thread),
            Some(LaneMenuEntry::Managed { .. })
        ));
    }
    assert!(docked_asked(&mut screen).is_empty());
}

// Given: a lane thread read with a standing close, its lane then detached
// When: the detach is announced, and later the connection is reset
// Then: a detached lane has no close badge, and a reset forgets the read so the row asks
//       for the lane again
#[test]
fn a_detach_and_a_reset_take_the_close_off_the_menu() {
    let mut screen = docked(None, None);
    let thread = ThreadId::new("thread-l9").unwrap();
    screen
        .state_mut()
        .feature
        .event(&lane_changed(lane_with_close(
            "l9",
            "thread-l9",
            Some("human_closed"),
        )));
    assert!(
        screen
            .state()
            .feature
            .menu()
            .current_close(&thread)
            .is_some()
    );

    let mut detached = lane_with_close("l9", "thread-l9", Some("human_closed"));
    detached["policy"] = json!("detached");
    detached.as_object_mut().unwrap().remove("lead_thread_id");
    screen.state_mut().feature.event(&lane_changed(detached));
    let menu = screen.state().feature.menu();
    assert_eq!(menu.current_close(&thread), None);
    assert_eq!(menu.entry(&thread), Some(&LaneMenuEntry::NotManaged));

    screen
        .state_mut()
        .feature
        .event(&lane_changed(lane_with_close(
            "l9",
            "thread-l9",
            Some("discarded"),
        )));
    assert!(
        screen
            .state()
            .feature
            .menu()
            .current_close(&thread)
            .is_some()
    );
    screen.state_mut().feature.reset();
    let menu = screen.state().feature.menu();
    assert_eq!(menu.current_close(&thread), None);
    assert!(!menu.is_known(&thread));
}

// Given: a lane thread whose lookup the core refuses, or whose lane the lead does not list
// When: the menu is read
// Then: both count as read, so a row asks no more, and neither shows a close
#[test]
fn a_refused_or_missing_lane_is_known_without_a_close() {
    let mut screen = docked(None, None);
    let lead = ThreadId::new("lead").unwrap();
    let refused = ThreadId::new("thread-refused").unwrap();
    screen
        .state_mut()
        .feature
        .enqueue_sidebar(LaneIntent::Lookup {
            thread: refused.clone(),
            lead: lead.clone(),
        });
    let first = docked_asked(&mut screen);
    docked_answer(
        &mut screen,
        first[0].tag,
        FeatureReply::Query(Err(FeatureError::new("busy"))),
    );
    let gone = ThreadId::new("thread-gone").unwrap();
    screen
        .state_mut()
        .feature
        .enqueue_sidebar(LaneIntent::Lookup {
            thread: gone.clone(),
            lead,
        });
    let second = docked_asked(&mut screen);
    docked_answer(&mut screen, second[0].tag, lanes(vec![]));
    let menu = screen.state().feature.menu();
    for thread in [&refused, &gone] {
        assert!(menu.is_known(thread));
        assert_eq!(menu.current_close(thread), None);
    }
    assert!(docked_asked(&mut screen).is_empty());
}
