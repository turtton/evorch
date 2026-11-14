//! The lanes feature drawn headless: lanes are read from the ledger, every
//! operation names the revision and the lead's root run the screen showed, and
//! an answer for a lane or lead no longer shown is dropped.
use super::*;
use crate::core_client::features::execution::tests::{
    answer, asked, command_of, ok_command, query_of, query_reply, screen,
};
use crate::core_client::features::{FeatureAction, FeatureRequest};
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

fn type_reason(
    screen: &mut egui_kittest::Harness<
        'static,
        crate::core_client::features::execution::tests::Screen,
    >,
    text: &str,
) {
    use egui::accesskit::Role;
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
    let first = asked(&mut screen);
    answer(
        &mut screen,
        tag_of(&first, is_list),
        lanes(vec![summary("l1", "attached", 3)]),
    );
    screen.get_by_label("人間の判断で閉じる…").click();
    let read = asked(&mut screen);
    let Query::Lanes(LanesQuery::GetLaneClosePreview { lane_id }) = query_of(&read[0]) else {
        panic!("the close preview was expected");
    };
    assert_eq!(lane_id.as_str(), "l1");
    // Until the preview is read nothing can be confirmed.
    assert!(disabled(&screen, "lane を閉じる"));
    // The Ready was for brief revision 1; the lane is at 2.
    answer(&mut screen, read[0].tag, preview(7, 9, 1));

    screen.get_by_label(close::MEANING);
    screen.get_by_label(close::REMAINS);
    screen.get_by_label(close::RUNNING_WARNING);
    screen.get_by_label(close::OLDER_BRIEF_WARNING);
    screen.get_by_label("この lane には未回答の質問が 2 件あります。close しても質問は残ります。");
    screen.get_by_label("最新の Ready（#4）— 参考（以前の brief）");
    screen.get_by_label("ほか 2 件の参照");
    assert!(screen.query_by_label_contains("未回収").is_none());

    // A reason is required before anything can be sent.
    assert!(disabled(&screen, "lane を閉じる"));
    type_reason(&mut screen, "  done by hand  ");
    assert!(!disabled(&screen, "lane を閉じる"));
    screen.get_by_label("lane を閉じる").click();
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
    assert!(disabled(&screen, "lane を閉じる"));
    screen.get_by_label("lane を閉じる").click();
    assert!(asked(&mut screen).is_empty());

    // The answer names the record's kind, and the lane is read again.
    answer(&mut screen, sent[0].tag, closed("human_closed", false));
    screen.get_by_label("lane の close: 人の判断で完了（未検証） として記録しました。run の停止や削除は行っていません");
    assert!(screen.query_by_label("lane を閉じる（未検証）").is_none());
    assert!(
        asked(&mut screen)
            .iter()
            .any(|a| matches!(&a.request, FeatureRequest::Query(q) if is_list(q)))
    );
}

#[test]
fn a_discard_is_sent_as_a_discard_and_an_already_closed_answer_says_so() {
    let mut screen = with_close_dialog("破棄する…", preview(3, 5, 2));
    screen.get_by_label("lane を破棄する");
    // No claim of completion: the Ready warning belongs to closing as done.
    assert!(screen.query_by_label(close::NO_READY_WARNING).is_none());
    assert!(screen.query_by_label(close::OLDER_BRIEF_WARNING).is_none());
    type_reason(&mut screen, "not needed");
    screen.get_by_label("lane を破棄する").click();
    let sent = asked(&mut screen);
    assert_eq!(
        close_of(&sent[0]).kind,
        workbench_protocol::lane_management::HumanLaneCloseKind::Discard
    );
    answer(&mut screen, sent[0].tag, closed("discarded", true));
    screen.get_by_label("lane の close: 既に終了済みでした（破棄）。新しい記録は作っていません");
}

#[test]
fn a_stale_refusal_is_shown_and_only_a_reload_makes_a_new_screen_the_one_to_confirm() {
    let mut screen = with_close_dialog("人間の判断で閉じる…", preview(3, 5, 2));
    type_reason(&mut screen, "done");
    screen.get_by_label("lane を閉じる").click();
    let sent = asked(&mut screen);
    let mut refusal = FeatureError::new("stale_request");
    refusal.code = Some(workbench_protocol::errors::ErrorCode::StaleRevision);
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(refusal)),
    );
    screen.get_by_label("lane が変わっていたため記録していません（stale_request）。Reload すると今の状態が確認対象になります。");
    // Nothing is resent by itself, and the old screen cannot be confirmed again.
    let after = asked(&mut screen);
    assert!(
        !after
            .iter()
            .any(|a| matches!(a.request, FeatureRequest::Command(_)))
    );
    assert!(disabled(&screen, "lane を閉じる"));

    screen.get_by_label("Reload").click();
    let reread = asked(&mut screen);
    answer(&mut screen, tag_of(&reread, is_preview), preview(4, 6, 2));
    assert!(!disabled(&screen, "lane を閉じる"));
    screen.run_steps(4);
    screen.get_by_label("lane を閉じる").click();
    let again = asked(&mut screen);
    let close = close_of(&again[0]);
    assert_eq!(close.expected_management_revision, Revision(4));
    assert_eq!(close.expected_request_revision, Revision(6));
}

#[test]
fn a_lost_answer_is_not_a_success_and_the_ledger_is_read_again() {
    let mut screen = with_close_dialog("人間の判断で閉じる…", preview(3, 5, 2));
    type_reason(&mut screen, "done");
    screen.get_by_label("lane を閉じる").click();
    let sent = asked(&mut screen);
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    screen.get_by_label("結果が分かりません。成功とは見なさず、台帳を読み直しています。");
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
    assert!(disabled(&screen, "lane を閉じる"));
    // The same tag later answers with what the core resolved.
    answer(&mut screen, sent[0].tag, closed("human_closed", false));
    assert!(screen.query_by_label("lane を閉じる（未検証）").is_none());
}

#[test]
fn cancel_sends_nothing_and_a_reason_over_two_kib_cannot_be_sent() {
    let mut screen = with_close_dialog("人間の判断で閉じる…", preview(3, 5, 2));
    type_reason(&mut screen, &"x".repeat(2049));
    screen.get_by_label("理由が長すぎます（2KiB まで）。");
    assert!(disabled(&screen, "lane を閉じる"));
    screen.get_by_label("キャンセル").click();
    let after = asked(&mut screen);
    assert!(
        !after
            .iter()
            .any(|a| matches!(a.request, FeatureRequest::Command(_)))
    );
    assert!(screen.query_by_label("lane を閉じる（未検証）").is_none());
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
    screen.get_by_label("close 済み: 人の判断で完了（未検証）");
    // The one row with a button is the managed lane; the detached lane has none.
    assert_eq!(screen.get_all_by_label("人間の判断で閉じる…").count(), 1);
    assert_eq!(screen.get_all_by_label("破棄する…").count(), 1);

    screen.get_by_label("破棄する…").click();
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
    screen.get_by_label("この lane は既に終了が記録されています（人の判断で完了（未検証））。記録するものはありません。");
    type_reason(&mut screen, "again");
    assert!(disabled(&screen, "lane を破棄する"));
}

#[test]
fn a_lane_that_moved_while_the_dialog_is_open_must_be_reloaded_before_confirming() {
    let mut screen = with_close_dialog("人間の判断で閉じる…", preview(3, 5, 2));
    type_reason(&mut screen, "done");
    assert!(!disabled(&screen, "lane を閉じる"));
    let moved = UiEvent::Lanes(LanesEvent::LaneChanged {
        lane: serde_json::from_value(summary("l1", "attached", 4)).unwrap(),
        also_for: vec![],
    });
    screen.state_mut().feature.event(&moved);
    screen.run_steps(2);
    screen
        .get_by_label("この画面を読んだ後に lane が変わりました。Reload してから決めてください。");
    assert!(disabled(&screen, "lane を閉じる"));
}
