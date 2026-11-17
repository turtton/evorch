//! The plan line of the execution feature: read on show, re-read on a newer
//! notice, never moved back by a late notice or answer, and reset on a thread
//! switch. It offers no control: the plan is read-only for people.
use super::super::tests::{answer, asked, query_of, query_reply, screen};
use super::super::*;
use crate::core_client::features::FeatureAction;
use egui_kittest::kittest::Queryable;
use serde_json::json;

fn plan_tag(actions: &[FeatureAction]) -> Option<u64> {
    actions
        .iter()
        .find(|a| {
            matches!(
                query_of(a),
                Query::Execution(ExecutionQuery::GetConversationPlan(_))
            )
        })
        .map(|a| a.tag)
}

fn summary(revision: u64, completed_ppm: u32, done: u32, blocked: u32) -> FeatureReply {
    let leaves = json!({"pending": 4 - done - blocked, "in_progress": 0,
        "completed": done, "blocked": blocked, "interrupted": 0});
    let weights = json!({"pending": 1_000_000 - completed_ppm, "in_progress": 0,
        "completed": completed_ppm, "blocked": 0, "interrupted": 0});
    query_reply(
        json!({"type": "execution", "payload": {"type": "conversation_plan",
        "payload": {"thread": "t1", "plan_id": "plan-1", "revision": revision.to_string(),
            "progress": {"node_count": 5, "leaf_count": 4, "leaf_counts": leaves,
                "weighted_ppm": weights}}}}),
    )
}

fn changed(revision: u64) -> UiEvent {
    UiEvent::Execution(ExecutionEvent::ConversationPlanChanged {
        thread: ThreadId::new("t1").unwrap(),
        revision: workbench_protocol::wire::Revision(revision),
    })
}

#[test]
fn the_shown_threads_plan_is_read_and_its_server_progress_shown() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let first = asked(&mut screen);
    let tag = plan_tag(&first).expect("the plan summary is read once shown");
    answer(&mut screen, tag, summary(3, 500_000, 2, 1));
    screen.get_by_label("計画: 50% · 完了 2/4 · ブロック 1");
}

#[test]
fn a_newer_notice_rereads_and_a_late_one_or_answer_never_moves_back() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let tag = plan_tag(&asked(&mut screen)).unwrap();
    answer(&mut screen, tag, summary(5, 750_000, 3, 0));
    screen.get_by_label("計画: 75% · 完了 3/4");

    // A notice for the revision already shown, or an older one, asks nothing.
    screen.state_mut().feature.event(&changed(5));
    screen.state_mut().feature.event(&changed(2));
    assert_eq!(plan_tag(&asked(&mut screen)), None);

    screen.state_mut().feature.event(&changed(6));
    let reread = plan_tag(&asked(&mut screen)).expect("a newer revision is read");
    // An answer that is older than what is shown is dropped.
    answer(&mut screen, reread, summary(4, 250_000, 1, 0));
    screen.get_by_label("計画: 75% · 完了 3/4");
}

#[test]
fn a_thread_switch_drops_the_old_plan_and_its_late_answer() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let tag = plan_tag(&asked(&mut screen)).unwrap();
    screen.state_mut().selection.thread = Some(ThreadId::new("t2").unwrap());
    let switched = asked(&mut screen);
    answer(&mut screen, tag, summary(9, 1_000_000, 4, 0));
    assert!(screen.query_all_by_label_contains("計画:").next().is_none());
    assert!(
        plan_tag(&switched).is_some(),
        "the new thread's plan is read"
    );
}

#[test]
fn an_unplanned_thread_says_so_and_the_line_offers_no_control() {
    let mut screen = screen(Box::<ExecutionFeature>::default(), Some("t1"));
    let tag = plan_tag(&asked(&mut screen)).unwrap();
    let empty = json!({"pending": 0, "in_progress": 0, "completed": 0, "blocked": 0,
        "interrupted": 0});
    answer(
        &mut screen,
        tag,
        query_reply(json!({"type": "execution", "payload": {
        "type": "conversation_plan", "payload": {"thread": "t1", "plan_id": null,
            "revision": "0", "progress": {"node_count": 0, "leaf_count": 0,
                "leaf_counts": empty, "weighted_ppm": empty}}}})),
    );
    screen.get_by_label("計画: 未計画").click();
    let clicked = asked(&mut screen);
    assert!(
        clicked.iter().all(|a| matches!(
            a.request,
            crate::core_client::features::FeatureRequest::Query(_)
        )),
        "the plan line sends no command"
    );
}
