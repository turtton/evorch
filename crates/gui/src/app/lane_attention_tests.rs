//! The lane attention list in the Workbench: when it is read, which answer is kept and what an
//! action on a stale item does, driven by lane events and a controllable reader.

use std::sync::{Arc, Mutex};

use event_bus::lane::QuestionResponder;
use event_bus::{Event, LedgerEvent, ToolEvent, UserQuestion};
use workbench_protocol::ids::ThreadId as WireThread;
use workbench_protocol::lane_attention::{
    LaneAttentionAction, LaneAttentionItem, LaneAttentionItemId, LaneAttentionKind,
    LaneAttentionSnapshot, LaneId,
};
use workbench_protocol::wire::{Revision, Sequence, UnixMillis};

use super::WorkbenchState;
use super::delivery_tests::state;
use super::lane_attention::LaneAttentionReader;
use super::lanes::Spawner;
use crate::fixture::DemoSource;
use crate::model::lane_attention::{AttentionAction, AttentionOp, LeadRead};

#[derive(Default)]
struct FakeReader {
    reads: Mutex<Vec<String>>,
    items: Mutex<Vec<LaneAttentionItem>>,
    failing: Mutex<bool>,
}

impl FakeReader {
    fn set(&self, items: Vec<LaneAttentionItem>) {
        *self.items.lock().unwrap() = items;
    }

    fn fail(&self, failing: bool) {
        *self.failing.lock().unwrap() = failing;
    }

    fn reads(&self) -> usize {
        self.reads.lock().unwrap().len()
    }
}

impl LaneAttentionReader for FakeReader {
    fn read(&self, lead_thread_id: &str) -> Result<LeadRead, String> {
        self.reads.lock().unwrap().push(lead_thread_id.to_owned());
        if *self.failing.lock().unwrap() {
            return Err("database busy".into());
        }
        Ok(LeadRead {
            items: self.items.lock().unwrap().clone(),
            snapshot: LaneAttentionSnapshot::new("snapshot").unwrap(),
            truncated: false,
            questions: Default::default(),
        })
    }
}

fn item(revision: u64) -> LaneAttentionItem {
    LaneAttentionItem {
        id: LaneAttentionItemId::new("item").unwrap(),
        lane: LaneId::new("lane-1").unwrap(),
        lead_thread: WireThread::new("thread").unwrap(),
        management_revision: Revision(1),
        revision: Revision(revision),
        kind: LaneAttentionKind::Blocked {
            report_seq: Sequence(1),
        },
        since: UnixMillis(1),
        summary: "blocked".into(),
        evidence_refs: vec![],
        available_actions: vec![LaneAttentionAction::Inspect, LaneAttentionAction::Continue],
    }
}

fn lane_created() -> Event {
    Event::new(LedgerEvent::LaneCreated {
        lane_id: "lane-1".into(),
        thread_id: "lane-thread-1".into(),
        origin_run_id: "lead-run".into(),
        origin_thread_id: "thread".into(),
        lead_thread_id: Some("thread".into()),
        policy: event_bus::lane::LanePolicy::Attached,
        management_revision: 1,
    })
}

fn question_event(responder: Option<QuestionResponder>) -> Event {
    Event::new(ToolEvent::UserQuestionUpdated {
        question: UserQuestion {
            responder,
            routing_revision: 1,
            management_revision: Some(1),
            id: "q".into(),
            run_id: "lane-root".into(),
            root_run_id: "lane-root".into(),
            root_name: "chat:Orchestrator:lane-thread-1".into(),
            recipient_run_ids: vec![],
            title: "scope".into(),
            options: vec![],
            blocking: false,
            answer: None,
            answer_origin: None,
        },
    })
}

fn workbench() -> (
    WorkbenchState<DemoSource>,
    Arc<FakeReader>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let inline: Spawner = Arc::new(|job| job());
    let reader = Arc::new(FakeReader::default());
    let mut workbench = state(&dir.path().join("sidebar.json"))
        .with_lane_attention(reader.clone())
        .with_lane_spawner(inline);
    workbench.apply_conversation_event(&lane_created());
    (workbench, reader, dir)
}

/// Lets the reads started so far finish and their answers land.
fn settle(workbench: &mut WorkbenchState<DemoSource>) {
    for _ in 0..3 {
        workbench.pump_attention();
    }
}

#[test]
fn a_lead_the_sidebar_shows_is_read_once_and_its_items_are_listed() {
    let (mut workbench, reader, _dir) = workbench();
    reader.set(vec![item(1)]);

    settle(&mut workbench);
    settle(&mut workbench);

    assert_eq!(reader.reads(), 1, "polling alone never reads again");
    assert_eq!(workbench.lane_attention().items().len(), 1);
}

#[test]
fn only_an_event_that_may_change_the_list_reads_it_again() {
    let (mut workbench, reader, _dir) = workbench();
    settle(&mut workbench);
    assert_eq!(reader.reads(), 1);

    // A question that no lane routed, and a run that merely progressed, say nothing about lanes.
    workbench.note_attention_event(&question_event(None));
    workbench.note_attention_event(&Event::new(event_bus::LifecycleEvent::RootWakeRequested {
        run_id: "run".into(),
    }));
    settle(&mut workbench);
    assert_eq!(reader.reads(), 1);

    workbench.note_attention_event(&question_event(Some(QuestionResponder::User)));
    settle(&mut workbench);
    assert_eq!(reader.reads(), 2);

    workbench.note_attention_event(&lane_created());
    settle(&mut workbench);
    assert_eq!(reader.reads(), 3);
}

#[test]
fn regaining_focus_and_unreadable_history_read_again_but_a_focused_frame_does_not() {
    let (mut workbench, reader, _dir) = workbench();
    settle(&mut workbench);

    workbench.observe_attention_focus(Some(true));
    workbench.observe_attention_focus(None);
    settle(&mut workbench);
    assert_eq!(reader.reads(), 1);

    workbench.observe_attention_focus(Some(false));
    workbench.observe_attention_focus(Some(true));
    settle(&mut workbench);
    assert_eq!(reader.reads(), 2, "the window came back");

    workbench.notifications_mut().push_history_gap(&[7]);
    settle(&mut workbench);
    assert_eq!(reader.reads(), 3, "events were missing");
}

fn continue_lead(revision: u64) -> AttentionAction {
    AttentionAction {
        lead: "thread".into(),
        item: Some(("item".into(), revision)),
        op: AttentionOp::ContinueThread("thread".into()),
    }
}

#[test]
fn an_action_waits_for_a_read_made_after_the_choice_and_runs_only_if_it_still_holds() {
    let (mut workbench, reader, _dir) = workbench();
    reader.set(vec![item(3)]);
    settle(&mut workbench);
    let reads = reader.reads();

    // The item is unchanged in the ledger: nothing runs until a fresh read confirms it.
    workbench.apply_attention_action(&egui::Context::default(), continue_lead(3));
    assert!(
        workbench.issued().is_empty(),
        "not on the window's own projection"
    );
    settle(&mut workbench);

    // One read confirmed the choice; the operation then asks for another to show its effect.
    assert_eq!(reader.reads(), reads + 2);
    assert!(matches!(
        workbench.issued(),
        [crate::model::commands::WorkbenchCommand::ContinueChat(continuation)]
            if continuation.thread_id == "thread"
    ));
}

#[test]
fn another_windows_change_the_events_did_not_announce_stops_the_old_screens_action() {
    let (mut workbench, reader, _dir) = workbench();
    reader.set(vec![item(3)]);
    settle(&mut workbench);

    // Another window moved the cause on; no event reached this one, so its board still says 3.
    reader.set(vec![item(4)]);
    workbench.apply_attention_action(&egui::Context::default(), continue_lead(3));
    settle(&mut workbench);

    assert!(
        workbench.issued().is_empty(),
        "the old screen's choice was not run"
    );
    assert!(workbench.lane_attention().holds("thread", "item", 4));

    workbench.apply_attention_action(&egui::Context::default(), continue_lead(4));
    settle(&mut workbench);
    assert_eq!(workbench.issued().len(), 1);
}

#[test]
fn an_action_the_ledger_cannot_confirm_runs_nothing() {
    let (mut workbench, reader, _dir) = workbench();
    reader.set(vec![item(3)]);
    settle(&mut workbench);

    // The held list still says 3, but the read after the choice fails: unconfirmed.
    reader.fail(true);
    workbench.apply_attention_action(&egui::Context::default(), continue_lead(3));
    settle(&mut workbench);

    assert!(workbench.issued().is_empty());
    assert_eq!(workbench.lane_attention().errors().len(), 1);

    // Once the ledger answers again, the same choice is made afresh and confirmed.
    reader.fail(false);
    workbench.apply_attention_action(&egui::Context::default(), continue_lead(3));
    settle(&mut workbench);
    assert_eq!(workbench.issued().len(), 1);
}

#[test]
fn opening_a_lane_has_no_side_effect_and_needs_no_confirmation() {
    let (mut workbench, reader, _dir) = workbench();
    reader.set(vec![item(3)]);
    settle(&mut workbench);
    let reads = reader.reads();

    workbench.apply_attention_action(
        &egui::Context::default(),
        AttentionAction {
            lead: "thread".into(),
            item: Some(("item".into(), 1)),
            op: AttentionOp::OpenLane(workspace_ui::ThreadId::new("lane-thread-1")),
        },
    );
    settle(&mut workbench);

    assert_eq!(reader.reads(), reads);
    assert!(workbench.issued().is_empty());
}

#[test]
fn a_refresh_needs_no_item_and_reads_every_lead() {
    let (mut workbench, reader, _dir) = workbench();
    settle(&mut workbench);

    workbench.apply_attention_action(
        &egui::Context::default(),
        AttentionAction {
            lead: String::new(),
            item: None,
            op: AttentionOp::Refresh,
        },
    );
    settle(&mut workbench);

    assert_eq!(reader.reads(), 2);
    assert!(workbench.issued().is_empty());
}
